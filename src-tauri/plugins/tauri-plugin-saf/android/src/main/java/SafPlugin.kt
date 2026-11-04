package app.tauri.saf

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.provider.DocumentsContract
import android.util.Base64
import androidx.activity.result.ActivityResult
import androidx.documentfile.provider.DocumentFile
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.util.concurrent.Executors

@InvokeArg
class ListArgs {
    lateinit var treeUri: String
    var relDir: String = ""
}

@InvokeArg
class ReadArgs {
    lateinit var treeUri: String
    lateinit var rel: String
}

@InvokeArg
class WriteArgs {
    lateinit var treeUri: String
    lateinit var rel: String
    lateinit var base64: String
    /** When true, fail if `rel` already exists; used for `create` semantics. */
    var createNew: Boolean = false
}

@InvokeArg
class RelArgs {
    lateinit var treeUri: String
    lateinit var rel: String
}

@InvokeArg
class RenameArgs {
    lateinit var treeUri: String
    lateinit var fromRel: String
    lateinit var toRel: String
}

@TauriPlugin
class SafPlugin(private val activity: Activity) : Plugin(activity) {

    /**
     * Background executor for every SAF I/O command. Tauri V2 dispatches
     * `@Command` invocations on the Activity main thread by default; if
     * the body blocks for more than 5 seconds, Android's input
     * dispatcher fires an ANR (observed in production: a 213-entry SAF
     * tree walk took 5.1–7.6s on a Vivo I2126, killing the app on the
     * next user tap).
     *
     * Cached pool — typical concurrent demand is 1–4 commands at a
     * time, threads are reused across calls. `invoke.resolve` /
     * `invoke.reject` are safe from any thread; the Tauri runtime
     * marshals back to the JS side on its own.
     */
    private val ioExecutor = Executors.newCachedThreadPool { r ->
        Thread(r, "saf-plugin-io").apply { isDaemon = true }
    }

    /**
     * Run `block` on the IO executor. Catches anything `block` throws
     * and translates to `invoke.reject`, so individual @Command bodies
     * stay focused on the happy path.
     */
    private fun runIO(invoke: Invoke, block: () -> Unit) {
        ioExecutor.execute {
            try {
                block()
            } catch (ex: Exception) {
                invoke.reject(ex.message ?: "saf op failed")
            }
        }
    }

    /**
     * LRU cache mapping `(treeUri, relPath)` → resolved `DocumentFile`.
     * Walking a path from the tree root costs one `findFile()` per
     * segment, and `findFile()` is itself a `ContentResolver.query()` —
     * the dominant cost on Android SAF reads. Without caching, every
     * read of the same file re-resolves from the root.
     *
     * Invariants:
     * - Entries are invalidated whenever the path is mutated (write,
     *   delete, rename, mkdir). Stale entries would point at a doc the
     *   provider has already removed.
     * - Caller of `resolveRelative` is unchanged; this layer is internal.
     * - Tree URI is part of the key so multiple vaults can be mounted
     *   in the same process without collisions.
     */
    private val resolveCache: MutableMap<Pair<String, String>, DocumentFile> =
        object : LinkedHashMap<Pair<String, String>, DocumentFile>(128, 0.75f, true) {
            override fun removeEldestEntry(
                eldest: MutableMap.MutableEntry<Pair<String, String>, DocumentFile>?
            ): Boolean = size > 256
        }

    private val cacheLock = Any()

    /**
     * Cached variant of `resolveRelative`. Hits return the previously-
     * resolved `DocumentFile`; misses fall through to a fresh walk and
     * insert the result. Returns `null` exactly when the path doesn't
     * exist — negative results are NOT cached (they'd be invalidated by
     * a subsequent createFile and we'd serve a stale `null`).
     */
    private fun resolveCached(treeUri: String, root: DocumentFile, rel: String): DocumentFile? {
        val cleaned = rel.trim().trim('/').trim('\\')
        if (cleaned.isEmpty() || cleaned == ".") return root
        val key = treeUri to cleaned
        synchronized(cacheLock) {
            resolveCache[key]?.let { return it }
        }
        val resolved = resolveRelative(root, cleaned) ?: return null
        synchronized(cacheLock) {
            resolveCache[key] = resolved
        }
        return resolved
    }

    /**
     * Drop every cache entry whose path equals `rel` or sits under it.
     * Used on mutations: a rename of `a/b` invalidates `a/b` itself plus
     * everything under it, so descendants don't keep pointing at the old
     * `DocumentFile`. Tree URI scopes the invalidation.
     */
    private fun invalidateCache(treeUri: String, rel: String) {
        val cleaned = rel.trim().trim('/').trim('\\')
        synchronized(cacheLock) {
            if (cleaned.isEmpty()) {
                // Mutation on the vault root: nuke every cached entry for
                // this tree to be safe. Cheap relative to a SAF round-trip.
                val it = resolveCache.entries.iterator()
                while (it.hasNext()) {
                    if (it.next().key.first == treeUri) it.remove()
                }
                return
            }
            val prefix = "$cleaned/"
            val it = resolveCache.entries.iterator()
            while (it.hasNext()) {
                val (entryTree, entryRel) = it.next().key
                if (entryTree != treeUri) continue
                if (entryRel == cleaned || entryRel.startsWith(prefix)) {
                    it.remove()
                }
            }
        }
    }

    /**
     * Opens the system folder picker (`ACTION_OPEN_DOCUMENT_TREE`) and
     * persists the returned permission so the app can use the folder
     * across launches. Resolves to `{ uri, displayName }` on success.
     */
    @Command
    fun pickFolder(invoke: Invoke) {
        try {
            val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
                addFlags(
                    Intent.FLAG_GRANT_READ_URI_PERMISSION
                            or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                            or Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION
                )
            }
            startActivityForResult(invoke, intent, "onFolderPicked")
        } catch (ex: Exception) {
            invoke.reject(ex.message ?: "Failed to open folder picker")
        }
    }

    @ActivityCallback
    fun onFolderPicked(invoke: Invoke, result: ActivityResult) {
        when (result.resultCode) {
            Activity.RESULT_OK -> {
                val uri: Uri? = result.data?.data
                if (uri == null) {
                    invoke.reject("No folder selected")
                    return
                }
                try {
                    // Persist across process restarts — without this we'd
                    // have to re-prompt the user every launch.
                    activity.contentResolver.takePersistableUriPermission(
                        uri,
                        Intent.FLAG_GRANT_READ_URI_PERMISSION
                                or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                    )
                } catch (ex: SecurityException) {
                    invoke.reject("Failed to persist folder permission: ${ex.message}")
                    return
                }

                // Display name: prefer DocumentFile.name (human-readable);
                // many providers return null for primary storage, so fall
                // back to parsing the tree URI. A tree URI looks like
                //   content://.../tree/primary%3ADocuments
                // or, for an SD card,
                //   content://.../tree/12A3-456B%3ASub%2FFolder
                // lastPathSegment URL-decodes to `primary:Documents` or
                // `12A3-456B:Sub/Folder`. Drop the `<volume>:` prefix, then
                // take the last path segment so nested picks show just the
                // leaf folder name. Empty segment (= volume root) falls
                // back to the volume label itself.
                val displayName = DocumentFile.fromTreeUri(activity, uri)?.name
                    ?: run {
                        // Some ROMs (observed on Vivo/iQOO) leave the
                        // segment percent-encoded even though Android docs
                        // say it should be decoded. Decode explicitly.
                        val raw = Uri.decode(uri.lastPathSegment ?: "")
                        val afterVolume = raw.substringAfter(':', raw)
                        val leaf = afterVolume.trimEnd('/').substringAfterLast('/')
                        if (leaf.isNotEmpty()) leaf
                        else raw.substringBefore(':', raw).ifEmpty { "" }
                    }

                val ret = JSObject()
                ret.put("uri", uri.toString())
                ret.put("displayName", displayName)
                invoke.resolve(ret)
            }
            Activity.RESULT_CANCELED -> invoke.reject("User cancelled")
            else -> invoke.reject("Folder picker failed")
        }
    }

    /**
     * List immediate children of `relDir` under `treeUri`. Returns
     * `{ entries: [{ name, relPath, isDir }] }`. Hidden entries (name
     * starts with `.`) are filtered at the Rust layer — we return
     * everything here and let the caller decide.
     */
    @Command
    fun listFolder(invoke: Invoke) {
        val args = invoke.parseArgs(ListArgs::class.java)
        runIO(invoke) {
            val rootUri = Uri.parse(args.treeUri)
            val docId = if (args.relDir.trim().trim('/').isEmpty()) {
                DocumentsContract.getTreeDocumentId(rootUri)
            } else {
                val root = DocumentFile.fromTreeUri(activity, rootUri)
                    ?: return@runIO invoke.reject("Cannot resolve tree URI")
                val target = resolveCached(args.treeUri, root, args.relDir)
                    ?: return@runIO invoke.reject("Folder not found: ${args.relDir}")
                if (!target.isDirectory) {
                    return@runIO invoke.reject("Not a directory: ${args.relDir}")
                }
                DocumentsContract.getDocumentId(target.uri)
            }

            // Direct ContentResolver query — same fast path as listTree.
            // `DocumentFile.listFiles()` would run this exact query then
            // wrap each row, paying an extra metadata round-trip per
            // wrapper. Cursor-based reads pay zero per-row overhead.
            val entries = JSArray()
            val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(rootUri, docId)
            val projection = arrayOf(
                DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                DocumentsContract.Document.COLUMN_MIME_TYPE,
            )
            activity.contentResolver.query(childrenUri, projection, null, null, null)?.use { cursor ->
                val idIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
                val nameIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
                val mimeIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_MIME_TYPE)
                while (cursor.moveToNext()) {
                    val name = cursor.getString(nameIdx) ?: continue
                    val childDocId = cursor.getString(idIdx) ?: continue
                    val mime = cursor.getString(mimeIdx)
                    val isDir = mime == DocumentsContract.Document.MIME_TYPE_DIR
                    val childRel = joinRel(args.relDir, name)
                    val obj = JSObject()
                    obj.put("name", name)
                    obj.put("relPath", childRel)
                    obj.put("isDir", isDir)
                    entries.put(obj)

                    // Warm-load cache with a SingleDocumentFile — see the
                    // walkTreeFast comment for the rationale.
                    val childUri = DocumentsContract.buildDocumentUriUsingTree(rootUri, childDocId)
                    val childDoc = DocumentFile.fromSingleUri(activity, childUri)
                    if (childDoc != null) {
                        synchronized(cacheLock) {
                            resolveCache[args.treeUri to childRel.trim('/')] = childDoc
                        }
                    }
                }
            }
            val ret = JSObject()
            ret.put("entries", entries)
            invoke.resolve(ret)
        }
    }

    /**
     * Read `rel` under `treeUri` and return its bytes as base64. The
     * Rust side decodes. Base64 avoids Tauri's JSON binary-unfriendly
     * transport — same pattern Corvux uses for desktop attachments.
     */
    @Command
    fun readDocument(invoke: Invoke) {
        val args = invoke.parseArgs(ReadArgs::class.java)
        runIO(invoke) {
            val rootUri = Uri.parse(args.treeUri)
            val root = DocumentFile.fromTreeUri(activity, rootUri)
                ?: return@runIO invoke.reject("Cannot resolve tree URI")

            val doc = resolveCached(args.treeUri, root, args.rel)
                ?: return@runIO invoke.reject("Not found: ${args.rel}")
            if (!doc.isFile) {
                return@runIO invoke.reject("Not a file: ${args.rel}")
            }

            val bytes = activity.contentResolver.openInputStream(doc.uri)?.use { it.readBytes() }
                ?: return@runIO invoke.reject("Cannot open stream: ${args.rel}")

            val ret = JSObject()
            ret.put("base64", Base64.encodeToString(bytes, Base64.NO_WRAP))
            invoke.resolve(ret)
        }
    }

    /**
     * Write bytes (base64-encoded) to `rel`. Auto-creates parent dirs.
     * If `createNew` is true, fails when the document already exists.
     * Otherwise truncates and overwrites the existing document.
     *
     * Not strictly atomic — if the app is killed mid-write the file is
     * left partially written. Matches what `fs::File::create + write`
     * does on desktop filesystems without the tmp+rename dance; full
     * atomicity over SAF requires the `renameTo`+`delete` dance which
     * some storage providers reject. Trade-off accepted for Slice 5.
     */
    @Command
    fun writeDocument(invoke: Invoke) {
        val args = invoke.parseArgs(WriteArgs::class.java)
        runIO(invoke) {
            val root = DocumentFile.fromTreeUri(activity, Uri.parse(args.treeUri))
                ?: return@runIO invoke.reject("Cannot resolve tree URI")

            val cleaned = args.rel.trim().trim('/').trim('\\')
            if (cleaned.isEmpty() || cleaned.split('/', '\\').any { it == ".." }) {
                return@runIO invoke.reject("Invalid path: ${args.rel}")
            }
            val parts = cleaned.split('/', '\\').filter { it.isNotEmpty() }
            val name = parts.last()
            val dirSegments = parts.dropLast(1)

            // Auto-create any missing parent directories along the way.
            var parent: DocumentFile = root
            for (seg in dirSegments) {
                if (seg == ".") continue
                if (seg == "..") return@runIO invoke.reject("Path traversal blocked")
                parent = parent.findFile(seg)
                    ?: parent.createDirectory(seg)
                    ?: return@runIO invoke.reject("Cannot create directory: $seg")
            }

            val existing = parent.findFile(name)
            if (args.createNew && existing != null) {
                return@runIO invoke.reject("Already exists: ${args.rel}")
            }
            val target = existing ?: (parent.createFile(mimeFor(name), name)
                ?: return@runIO invoke.reject("createFile failed: $name"))

            val bytes = Base64.decode(args.base64, Base64.DEFAULT)
            // `wt` = write+truncate. Some providers reject `wt`; fall
            // back to `w` which also truncates on most ContentResolvers.
            val stream = activity.contentResolver.openOutputStream(target.uri, "wt")
                ?: activity.contentResolver.openOutputStream(target.uri, "w")
                ?: return@runIO invoke.reject("Cannot open output stream: ${args.rel}")
            stream.use { it.write(bytes) }

            // A new file changes the parent's listing; an overwrite leaves
            // the same `DocumentFile`. In both cases drop the cached
            // resolution to be safe — the cost of one re-walk is tiny
            // compared to the risk of serving a stale handle.
            invalidateCache(args.treeUri, args.rel)

            invoke.resolve(JSObject())
        }
    }

    /**
     * Delete `rel` permanently. SAF has no built-in trash; matching the
     * NativeFs soft-trash semantics would require a cross-dir move to
     * `.corvux-trash/`, which SAF's `renameTo` can't do (same-parent
     * only). Callers that need undo should snapshot via the graph DB
     * before calling — accepted Slice 5 trade-off.
     */
    @Command
    fun deleteDocument(invoke: Invoke) {
        val args = invoke.parseArgs(RelArgs::class.java)
        runIO(invoke) {
            val root = DocumentFile.fromTreeUri(activity, Uri.parse(args.treeUri))
                ?: return@runIO invoke.reject("Cannot resolve tree URI")
            val doc = resolveCached(args.treeUri, root, args.rel)
                ?: return@runIO invoke.reject("Not found: ${args.rel}")
            if (!doc.delete()) return@runIO invoke.reject("delete failed: ${args.rel}")
            invalidateCache(args.treeUri, args.rel)
            invoke.resolve(JSObject())
        }
    }

    /**
     * Rename within the same parent directory. Cross-directory rename
     * isn't supported — SAF `DocumentFile.renameTo` can only rename;
     * moving requires a copy+delete dance that we haven't built yet.
     */
    @Command
    fun renameDocument(invoke: Invoke) {
        val args = invoke.parseArgs(RenameArgs::class.java)
        runIO(invoke) {
            val root = DocumentFile.fromTreeUri(activity, Uri.parse(args.treeUri))
                ?: return@runIO invoke.reject("Cannot resolve tree URI")

            val from = normalize(args.fromRel) ?: return@runIO invoke.reject("Invalid fromRel")
            val to = normalize(args.toRel) ?: return@runIO invoke.reject("Invalid toRel")
            val fromParent = from.substringBeforeLast('/', "")
            val toParent = to.substringBeforeLast('/', "")
            if (fromParent != toParent) {
                return@runIO invoke.reject("SAF rename across directories not supported yet")
            }
            val newName = to.substringAfterLast('/')
            val doc = resolveCached(args.treeUri, root, from) ?: return@runIO invoke.reject("Not found: ${args.fromRel}")
            if (!doc.renameTo(newName)) return@runIO invoke.reject("rename failed")
            // Both source and destination are mutated. A rename of a
            // directory needs to invalidate descendants too, which the
            // helper handles.
            invalidateCache(args.treeUri, from)
            invalidateCache(args.treeUri, to)
            invoke.resolve(JSObject())
        }
    }

    @Command
    fun mkdir(invoke: Invoke) {
        val args = invoke.parseArgs(RelArgs::class.java)
        runIO(invoke) {
            val root = DocumentFile.fromTreeUri(activity, Uri.parse(args.treeUri))
                ?: return@runIO invoke.reject("Cannot resolve tree URI")

            val cleaned = normalize(args.rel) ?: return@runIO invoke.reject("Invalid rel")
            if (cleaned.isEmpty()) {
                return@runIO invoke.reject("Cannot mkdir the vault root")
            }
            val parts = cleaned.split('/').filter { it.isNotEmpty() }
            var parent: DocumentFile = root
            for ((i, seg) in parts.withIndex()) {
                val existing = parent.findFile(seg)
                if (existing != null) {
                    if (i == parts.lastIndex) {
                        return@runIO invoke.reject("Already exists: ${args.rel}")
                    }
                    if (!existing.isDirectory) {
                        return@runIO invoke.reject("Not a directory: $seg")
                    }
                    parent = existing
                } else {
                    parent = parent.createDirectory(seg)
                        ?: return@runIO invoke.reject("Cannot create directory: $seg")
                }
            }
            invalidateCache(args.treeUri, args.rel)
            invoke.resolve(JSObject())
        }
    }

    /**
     * Recursive list — walk every descendant under `relDir` in one JNI
     * round-trip. The previous design called `listFolder` per-directory
     * sequentially; on a vault with D directories that's D round-trips
     * before the parallel reads can even start. On a flat 15-entry vault
     * it's one round-trip either way, but on anything with structure
     * the gain is linear in tree depth.
     *
     * Returns `{ entries: [{ name, relPath, isDir }] }` — same shape as
     * `listFolder` so the Rust walker can consume it without conditional
     * code paths. Hidden entries (`.`-prefixed) are filtered at the Rust
     * layer like the non-recursive variant. Children are populated into
     * the resolve cache as we go, so a follow-up `readDocument` for any
     * descendant skips the per-segment `findFile` walk.
     */
    @Command
    fun listTree(invoke: Invoke) {
        val args = invoke.parseArgs(ListArgs::class.java)
        runIO(invoke) {
            val rootUri = Uri.parse(args.treeUri)
            val rootDocId = if (args.relDir.trim().trim('/').isEmpty()) {
                DocumentsContract.getTreeDocumentId(rootUri)
            } else {
                // Resolve subtree-relative starts via the cache / findFile
                // walk. For our SAF callers `relDir` is almost always "" so
                // this path is rarely exercised on cold start.
                val root = DocumentFile.fromTreeUri(activity, rootUri)
                    ?: return@runIO invoke.reject("Cannot resolve tree URI")
                val target = resolveCached(args.treeUri, root, args.relDir)
                    ?: return@runIO invoke.reject("Folder not found: ${args.relDir}")
                if (!target.isDirectory) {
                    return@runIO invoke.reject("Not a directory: ${args.relDir}")
                }
                DocumentsContract.getDocumentId(target.uri)
            }

            val entries = JSArray()
            walkTreeFast(args.treeUri, rootUri, rootDocId, args.relDir, entries)
            val ret = JSObject()
            ret.put("entries", entries)
            invoke.resolve(ret)
        }
    }

    /**
     * Recursive walk via direct ContentResolver queries.
     *
     * Skips the `DocumentFile` wrapper layer entirely. `DocumentFile.listFiles()`
     * runs the same `ContentResolver.query()` we run here, but then constructs
     * a wrapper per row — and every later access to the wrapper's `name` /
     * `isDirectory` fires a fresh metadata query. By reading the cursor
     * columns directly we get all the data we need in a single per-directory
     * query.
     *
     * Per-row work: build a child Document URI (cheap, just string formatting)
     * and warm-load the resolve cache with a `SingleDocumentFile`. That's
     * the minimal wrapper shape needed by `readDocument` / `writeDocument` /
     * `statDocument` — none of those call `listFiles()` on a cached entry,
     * and `listFolder` itself goes through this same fast path on demand.
     */
    private fun walkTreeFast(
        treeUri: String,
        treeUriParsed: Uri,
        docId: String,
        rel: String,
        out: JSArray,
    ) {
        val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(treeUriParsed, docId)
        val projection = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
        )
        // Sub-paths and their doc-ids collected during the cursor pass —
        // we recurse only after the cursor is closed because some content
        // providers misbehave when nested queries are open against the
        // same authority.
        val subdirs = mutableListOf<Triple<String, String, String>>() // (childDocId, childRel, displayName)
        activity.contentResolver.query(childrenUri, projection, null, null, null)?.use { cursor ->
            val idIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
            val nameIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
            val mimeIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_MIME_TYPE)
            while (cursor.moveToNext()) {
                val name = cursor.getString(nameIdx) ?: continue
                val childDocId = cursor.getString(idIdx) ?: continue
                val mime = cursor.getString(mimeIdx)
                val isDir = mime == DocumentsContract.Document.MIME_TYPE_DIR
                val childRel = joinRel(rel, name)
                val obj = JSObject()
                obj.put("name", name)
                obj.put("relPath", childRel)
                obj.put("isDir", isDir)
                out.put(obj)

                // Warm-load the resolve cache with a SingleDocumentFile for
                // each child URI. read/write/delete/rename/stat all work
                // with this — they just need the URI. listFolder uses the
                // same ContentResolver path so it doesn't need the tree
                // wrapper either.
                val childUri = DocumentsContract.buildDocumentUriUsingTree(treeUriParsed, childDocId)
                val childDoc = DocumentFile.fromSingleUri(activity, childUri)
                if (childDoc != null) {
                    synchronized(cacheLock) {
                        resolveCache[treeUri to childRel.trim('/')] = childDoc
                    }
                }

                if (isDir) {
                    subdirs.add(Triple(childDocId, childRel, name))
                }
            }
        }
        for ((childDocId, childRel, _) in subdirs) {
            walkTreeFast(treeUri, treeUriParsed, childDocId, childRel, out)
        }
    }

    /**
     * Stat a document — return whether it exists, and if so, whether it's
     * a directory and how big the file is. Replaces the previous
     * `exists()` strategy of "read the whole file and discard the bytes",
     * which was the dominant cost on every `index_note` save path that
     * stat-checked an attachment before opening it.
     *
     * Returns `{ exists: false }` when the path can't be resolved. When
     * `exists` is true, `isDir` and `size` are populated; `size` is `-1`
     * for directories (matching the convention in [SafEntry]).
     */
    @Command
    fun statDocument(invoke: Invoke) {
        val args = invoke.parseArgs(RelArgs::class.java)
        runIO(invoke) {
            val root = DocumentFile.fromTreeUri(activity, Uri.parse(args.treeUri))
                ?: return@runIO invoke.reject("Cannot resolve tree URI")
            val doc = resolveCached(args.treeUri, root, args.rel)
            val ret = JSObject()
            if (doc == null) {
                ret.put("exists", false)
                ret.put("isDir", false)
                ret.put("size", -1L)
            } else {
                ret.put("exists", true)
                ret.put("isDir", doc.isDirectory)
                ret.put("size", if (doc.isDirectory) -1L else doc.length())
            }
            invoke.resolve(ret)
        }
    }

    /** Infer a MIME type from filename. `.md` → `text/markdown`, else octet-stream. */
    private fun mimeFor(name: String): String = when {
        name.endsWith(".md", ignoreCase = true) -> "text/markdown"
        name.endsWith(".txt", ignoreCase = true) -> "text/plain"
        name.endsWith(".json", ignoreCase = true) -> "application/json"
        name.endsWith(".png", ignoreCase = true) -> "image/png"
        name.endsWith(".jpg", ignoreCase = true) || name.endsWith(".jpeg", ignoreCase = true) -> "image/jpeg"
        else -> "application/octet-stream"
    }

    private fun normalize(rel: String): String? {
        val cleaned = rel.replace('\\', '/').trim().trim('/')
        if (cleaned.split('/').any { it == ".." }) return null
        return cleaned
    }

    /**
     * Resolve a slash-separated relative path against a tree root.
     * Empty / "." returns the root. Returns null on any missing segment.
     * Case-sensitive by design — SAF is case-sensitive, and matching the
     * Rust-side contract (validated_rel) avoids ambiguity across devices.
     */
    private fun resolveRelative(root: DocumentFile, rel: String): DocumentFile? {
        val cleaned = rel.trim().trim('/').trim('\\')
        if (cleaned.isEmpty() || cleaned == ".") return root
        var cur: DocumentFile = root
        for (segment in cleaned.split('/', '\\')) {
            if (segment.isEmpty() || segment == ".") continue
            if (segment == "..") return null
            cur = cur.findFile(segment) ?: return null
        }
        return cur
    }

    private fun joinRel(parent: String, name: String): String {
        val p = parent.trim().trim('/').trim('\\')
        return if (p.isEmpty() || p == ".") name else "$p/$name"
    }
}
