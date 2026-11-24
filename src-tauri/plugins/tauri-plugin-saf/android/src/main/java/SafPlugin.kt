package app.tauri.saf

import android.app.Activity
import android.content.Intent
import android.database.Cursor
import android.net.Uri
import android.provider.DocumentsContract
import android.util.Base64
import android.util.Log
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
import java.io.IOException
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

private const val TAG = "CorvuxSaf"

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
                // Name the exception when it has no message (e.g. an
                // UnsupportedOperationException) — a bare "saf op failed"
                // hid the cause of the 2026-11-05 rename/move failures.
                invoke.reject(ex.message ?: "saf op failed: ${ex.javaClass.simpleName}")
            }
        }
    }

    /**
     * THE only place a directory's children are queried. Returns a cursor
     * whose rows are the COMPLETE listing, or throws an [IOException] that
     * starts with [INCOMPLETE_LISTING_PREFIX] (surfaced to Rust as an error,
     * never as an empty/partial success). Callers `.use { }` the result.
     *
     * Before this existed `query(...)?.use { ... }` read a null cursor
     * (provider returned null or died), a provider error, and a provider
     * still loading as "this directory is empty/partial" — and the result
     * was cached as the tree and trusted by the SAF trash copy-then-delete
     * fallback. Per the DocumentsContract docs a loading provider "must send
     * a content changed notification when loading is finished"; we re-query
     * a bounded number of times instead of registering an observer, then
     * give up with an explicit error.
     *
     * Runs on the IO executor (see [runIO]), so the back-off sleep never
     * blocks the UI thread.
     */
    private fun queryChildrenChecked(
        childrenUri: Uri,
        projection: Array<String>,
        rel: String,
    ): Cursor {
        val where = rel.ifEmpty { "/" }
        var attempt = 1
        while (true) {
            // The query ITSELF can throw (provider process died, lost grant ->
            // SecurityException, ...). Keep that an incomplete-listing error so
            // Rust never maps the exception's own wording (which may contain
            // "not found") to NotFound, i.e. "it isn't there".
            val cursor = try {
                activity.contentResolver.query(childrenUri, projection, null, null, null)
            } catch (e: Exception) {
                Log.w(TAG, "listing '$where': query threw ${e.javaClass.simpleName}")
                throw IOException("$INCOMPLETE_LISTING_PREFIX: provider failed while listing '$where'")
            }
            // `extras` is a remote call on a cross-process cursor and can throw
            // when the provider dies mid-call. Never leave the cursor open on
            // that path, and keep the rejection an incomplete-listing error.
            val state = try {
                val extras = cursor?.extras
                classifyDirCursor(
                    cursorPresent = cursor != null,
                    loading = extras?.getBoolean(DocumentsContract.EXTRA_LOADING, false) ?: false,
                    error = extras?.getString(DocumentsContract.EXTRA_ERROR),
                )
            } catch (e: Exception) {
                cursor?.close()
                Log.w(TAG, "listing '$where': reading cursor extras failed: ${e.javaClass.simpleName}")
                throw IOException("$INCOMPLETE_LISTING_PREFIX: provider failed while listing '$where'")
            }
            when (state) {
                DirCursorState.Complete -> return requireNotNull(cursor)
                DirCursorState.Loading -> {
                    cursor?.close()
                    if (attempt >= LOADING_MAX_ATTEMPTS) {
                        throw IOException("$INCOMPLETE_LISTING_PREFIX: provider still loading '$where'")
                    }
                    try {
                        Thread.sleep(LOADING_BACKOFF_MS * attempt)
                    } catch (e: InterruptedException) {
                        Thread.currentThread().interrupt()
                        throw IOException("$INCOMPLETE_LISTING_PREFIX: interrupted while loading '$where'")
                    }
                    attempt += 1
                }
                is DirCursorState.Failed -> {
                    cursor?.close()
                    // The provider's own text can carry an absolute path
                    // (`/storage/...`): logcat only, never the user-facing error.
                    Log.w(TAG, "listing '$where' failed: ${state.reason}")
                    throw IOException("$INCOMPLETE_LISTING_PREFIX: provider could not list '$where'")
                }
            }
        }
    }

    /**
     * THE only child-by-name lookup. `DocumentFile.findFile` returns null both
     * when the child does not exist AND when the provider failed (androidx
     * `DocumentsContractApi21.listFiles` catches every exception and returns
     * an empty array), and every caller read null as "absent": `statDocument`
     * answered `exists: false`, and Rust's startup `recover_missing_files` then
     * overwrote a note that was there. So a null from `findFile` is re-checked
     * with the checked children query: an incomplete listing THROWS (rejected
     * to Rust as an error, never as "not found"); a complete listing without
     * the name is a genuine absence (null); a name that IS listed although
     * `findFile` missed it is retried once and otherwise fails closed.
     *
     * The common case (found) costs nothing extra; the extra query happens only
     * for a name that is not there (creating a note, probing a free trash name).
     */
    private fun findChildVerified(parent: DocumentFile, name: String): DocumentFile? {
        parent.findFile(name)?.let { return it }
        if (!childListedChecked(parent, name)) return null
        return parent.findFile(name)
            ?: throw IOException("$INCOMPLETE_LISTING_PREFIX: '$name' is listed but could not be resolved")
    }

    /** True iff a COMPLETE listing of `parent` contains a child called `name`. */
    private fun childListedChecked(parent: DocumentFile, name: String): Boolean {
        val parentDocId = DocumentsContract.getDocumentId(parent.uri)
        val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(parent.uri, parentDocId)
        val projection = arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
        queryChildrenChecked(childrenUri, projection, name).use { cursor ->
            val nameIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
            while (cursor.moveToNext()) {
                if (cursor.getString(nameIdx) == name) return true
            }
        }
        return false
    }

    /**
     * Providers do not FAIL a create on a name clash: they quietly store the new
     * document as "name (1).ext". That happens on case-insensitive storage (a
     * note `ideas.md` on disk, `Ideas.md` requested) and when another writer
     * wins the race between our lookup and the create. Unchecked, startup
     * recovery made `Ideas (1).md`, `Ideas (2).md`, ... on every launch and sync
     * spread each copy to every peer.
     *
     * So BEFORE creating, take a checked listing of the parent: refuse (as
     * "Already exists", which Rust maps to AlreadyExists, so recovery leaves
     * things alone) if any child's name equals the wanted name ignoring case —
     * nothing is created and nothing is deleted on that path — and keep the set
     * of existing document ids for [settleCreated]. The cost is one extra
     * checked listing per create. It also refuses a case-variant on a
     * case-SENSITIVE provider, which is the safe direction.
     */
    private fun snapshotBeforeCreate(parent: DocumentFile, name: String): Set<String> {
        val parentDocId = DocumentsContract.getDocumentId(parent.uri)
        val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(parent.uri, parentDocId)
        val projection = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
        )
        val ids = HashSet<String>()
        queryChildrenChecked(childrenUri, projection, name).use { cursor ->
            val idIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
            val nameIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
            while (cursor.moveToNext()) {
                val id = cursor.getString(idIdx)
                    ?: throw IOException("$INCOMPLETE_LISTING_PREFIX: row without an id while preparing a create")
                ids.add(id)
                val childName = cursor.getString(nameIdx)
                if (childName != null && childName.equals(name, ignoreCase = true)) {
                    throw IOException(
                        "Already exists: a document whose name differs at most by letter case is already there",
                    )
                }
            }
        }
        return ids
    }

    /**
     * After a create: if the provider stored the document under a different name
     * than requested (a racing writer, a normalised name), reject with
     * "Already exists". Remove the document ONLY if it is provably the one this
     * call just made: its id was NOT in [snapshotBeforeCreate]'s listing (a
     * provider that breaks the contract can return an EXISTING document, e.g. the
     * existing folder on a case clash, and deleting that would destroy user
     * data) AND it is empty (a file of length 0, a folder with no children).
     * Anything else is left untouched and logged. A name that cannot be read
     * (`null`) means "cannot tell": proceed.
     */
    private fun settleCreated(created: DocumentFile, wanted: String, before: Set<String>, isDir: Boolean) {
        val got = created.name ?: return
        if (got == wanted) return
        val id = DocumentsContract.getDocumentId(created.uri)
        val ours = id !in before && (if (isDir) hasNoChildrenChecked(created) else created.length() == 0L)
        if (ours) {
            if (!created.delete()) Log.w(TAG, "could not remove the renamed stray document")
        } else {
            Log.w(TAG, "provider returned an existing or non-empty document for a create; left untouched")
        }
        throw IOException(
            "Already exists: the storage provider keeps a differently-named document, " +
                "so '$wanted' was not created",
        )
    }

    /** True iff a COMPLETE listing of `dir` has no children. */
    private fun hasNoChildrenChecked(dir: DocumentFile): Boolean {
        val docId = DocumentsContract.getDocumentId(dir.uri)
        val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(dir.uri, docId)
        val projection = arrayOf(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
        queryChildrenChecked(childrenUri, projection, "new folder").use { cursor ->
            return !cursor.moveToNext()
        }
    }

    private fun createFileChecked(parent: DocumentFile, mime: String, name: String): DocumentFile? {
        val before = snapshotBeforeCreate(parent, name)
        val created = parent.createFile(mime, name) ?: return null
        settleCreated(created, name, before, isDir = false)
        return created
    }

    private fun createDirectoryChecked(parent: DocumentFile, name: String): DocumentFile? {
        val before = snapshotBeforeCreate(parent, name)
        val created = parent.createDirectory(name) ?: return null
        settleCreated(created, name, before, isDir = true)
        return created
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
            queryChildrenChecked(childrenUri, projection, args.relDir).use { cursor ->
                val idIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
                val nameIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
                val mimeIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_MIME_TYPE)
                while (cursor.moveToNext()) {
                    // A row we cannot name or address is a child we cannot list:
                    // skipping it would make this listing silently incomplete.
                    val name = cursor.getString(nameIdx)
                        ?: throw IOException("$INCOMPLETE_LISTING_PREFIX: unnamed row in '${args.relDir.ifEmpty { "/" }}'")
                    val childDocId = cursor.getString(idIdx)
                        ?: throw IOException("$INCOMPLETE_LISTING_PREFIX: row without an id in '${args.relDir.ifEmpty { "/" }}'")
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
                parent = findChildVerified(parent, seg)
                    ?: createDirectoryChecked(parent, seg)
                    ?: return@runIO invoke.reject("Cannot create directory: $seg")
            }

            val existing = findChildVerified(parent, name)
            if (args.createNew && existing != null) {
                return@runIO invoke.reject("Already exists: ${args.rel}")
            }
            val target = existing ?: (createFileChecked(parent, mimeFor(name), name)
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
     * Delete `rel` permanently (a folder with everything inside it).
     * Note and folder deletes from the app do NOT come here directly: the
     * Rust trash moves them into `.corvux-trash/` (see `moveDocument`). The
     * copy-then-delete fallback that used to call this to remove an original
     * is DISABLED (`COPY_THEN_DELETE_ENABLED` in `roostfs/trash.rs`): a write
     * landing after its final check would be lost. Nothing in Rust should
     * reach this command for a user delete until that is re-validated.
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
     * isn't supported here — SAF `DocumentFile.renameTo` can only rename.
     * (`moveDocument` below can move between folders; it is used by the
     * trash only so far.)
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
            // The provider would otherwise pick "name (1)" on a clash.
            if (from != to && resolveRelative(root, to) != null) {
                return@runIO invoke.reject("Already exists: ${args.toRel}")
            }
            // Rename through the document URI, not `doc.renameTo`: entries
            // warmed by `walkTreeFast` are `SingleDocumentFile`s, whose
            // `renameTo` throws UnsupportedOperationException — every note
            // rename on an Android folder roost failed with "saf op failed"
            // once the tree had been listed (found on-device 2026-11-05).
            DocumentsContract.renameDocument(activity.contentResolver, doc.uri, newName)
                ?: return@runIO invoke.reject("rename failed")
            // Both source and destination are mutated. A rename of a
            // directory needs to invalidate descendants too, which the
            // helper handles.
            invalidateCache(args.treeUri, from)
            invalidateCache(args.treeUri, to)
            invoke.resolve(JSObject())
        }
    }

    /**
     * Move `fromRel` (file or folder) into the folder of `toRel`, then give
     * it the name at the end of `toRel`. Uses the provider's own move
     * (`DocumentsContract.moveDocument`, API 24+), so nothing is copied
     * through the app and a folder moves as one unit, hidden files included.
     *
     * Used by the Rust trash to put deleted notes into `.corvux-trash/`.
     * The destination folder must already exist and the name must be free.
     * Rejects with "Move not supported" when the provider can't move. The
     * Rust copy-then-delete fallback for that case is currently DISABLED, so
     * the delete fails and the original stays. If the move works but the
     * follow-up rename fails, the document is already safe inside the
     * destination folder under its old name, so this still resolves.
     */
    @Command
    fun moveDocument(invoke: Invoke) {
        val args = invoke.parseArgs(RenameArgs::class.java)
        runIO(invoke) {
            val root = DocumentFile.fromTreeUri(activity, Uri.parse(args.treeUri))
                ?: return@runIO invoke.reject("Cannot resolve tree URI")
            val from = normalize(args.fromRel) ?: return@runIO invoke.reject("Invalid fromRel")
            val to = normalize(args.toRel) ?: return@runIO invoke.reject("Invalid toRel")
            if (from.isEmpty() || to.isEmpty()) {
                return@runIO invoke.reject("Invalid path: cannot move the roost root")
            }
            val fromParentRel = from.substringBeforeLast('/', "")
            val oldName = from.substringAfterLast('/')
            val toParentRel = to.substringBeforeLast('/', "")
            val newName = to.substringAfterLast('/')

            val doc = resolveCached(args.treeUri, root, from)
                ?: return@runIO invoke.reject("Not found: ${args.fromRel}")
            val srcParent = resolveCached(args.treeUri, root, fromParentRel)
                ?: return@runIO invoke.reject("Not found: $fromParentRel")
            val dstParent = resolveCached(args.treeUri, root, toParentRel)
                ?: return@runIO invoke.reject("Not found: $toParentRel")
            if (!dstParent.isDirectory) {
                return@runIO invoke.reject("Not a directory: $toParentRel")
            }
            // Walk from the root rather than `dstParent.findFile`: cached
            // entries are `SingleDocumentFile`s, which can't list children.
            if (resolveRelative(root, to) != null) {
                return@runIO invoke.reject("Already exists: ${args.toRel}")
            }
            // The provider moves under the OLD name, then we rename. If that
            // in-between name is already taken (e.g. a stray
            // `.corvux-trash/a.md` from a failed rename), the provider would
            // refuse. Report it under its OWN marker (not "Move not supported":
            // that now means the provider cannot move, which is a permanent
            // refusal) so Rust can move the stray aside and retry once.
            if (oldName != newName && resolveRelative(root, joinRel(toParentRel, oldName)) != null) {
                return@runIO invoke.reject("$STAGING_NAME_TAKEN_PREFIX: '$oldName' is already in '$toParentRel'")
            }

            val resolver = activity.contentResolver
            var moveError: String? = null
            val moved: Uri? = try {
                DocumentsContract.moveDocument(resolver, doc.uri, srcParent.uri, dstParent.uri)
            } catch (e: Exception) {
                moveError = e.message ?: e.javaClass.simpleName
                null
            }
            if (moved == null) {
                return@runIO invoke.reject("Move not supported: ${moveError ?: "provider returned no document"}")
            }
            // The source path (and everything under it) is gone now.
            invalidateCache(args.treeUri, from)
            invalidateCache(args.treeUri, joinRel(toParentRel, oldName))

            if (newName != oldName) {
                try {
                    DocumentsContract.renameDocument(resolver, moved, newName)
                        ?: Log.w(TAG, "moveDocument: moved to $toParentRel but rename to $newName returned null")
                } catch (e: Exception) {
                    Log.w(TAG, "moveDocument: moved to $toParentRel but rename to $newName failed: ${e.message}")
                }
            }
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
                val existing = findChildVerified(parent, seg)
                if (existing != null) {
                    if (i == parts.lastIndex) {
                        return@runIO invoke.reject("Already exists: ${args.rel}")
                    }
                    if (!existing.isDirectory) {
                        return@runIO invoke.reject("Not a directory: $seg")
                    }
                    parent = existing
                } else {
                    parent = createDirectoryChecked(parent, seg)
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
        // SIZE and LAST_MODIFIED ride along so the SAF trash fallback can tell
        // that a file was edited IN PLACE between two walks (same name, same
        // kind): the Rust side compares them before it removes the original.
        // Both are optional columns; a provider that omits one just yields no
        // field, which Rust reads as "unknown" on both walks.
        val projection = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
            DocumentsContract.Document.COLUMN_SIZE,
            DocumentsContract.Document.COLUMN_LAST_MODIFIED,
        )
        // Sub-paths and their doc-ids collected during the cursor pass —
        // we recurse only after the cursor is closed because some content
        // providers misbehave when nested queries are open against the
        // same authority.
        val subdirs = mutableListOf<Triple<String, String, String>>() // (childDocId, childRel, displayName)
        // A nested failure aborts the WHOLE walk (the exception propagates out
        // of every recursion level to `runIO`, which rejects): `out` is never
        // returned half-filled, so Rust sees an error, not a smaller tree.
        queryChildrenChecked(childrenUri, projection, rel).use { cursor ->
            val idIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
            val nameIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
            val mimeIdx = cursor.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_MIME_TYPE)
            val sizeIdx = cursor.getColumnIndex(DocumentsContract.Document.COLUMN_SIZE)
            val modIdx = cursor.getColumnIndex(DocumentsContract.Document.COLUMN_LAST_MODIFIED)
            while (cursor.moveToNext()) {
                // A row we cannot name or address is a child we cannot list:
                // skipping it would make the walk silently incomplete (and
                // let the trash fallback delete it with its parent).
                val name = cursor.getString(nameIdx)
                    ?: throw IOException("$INCOMPLETE_LISTING_PREFIX: unnamed row in '${rel.ifEmpty { "/" }}'")
                val childDocId = cursor.getString(idIdx)
                    ?: throw IOException("$INCOMPLETE_LISTING_PREFIX: row without an id in '${rel.ifEmpty { "/" }}'")
                val mime = cursor.getString(mimeIdx)
                val isDir = mime == DocumentsContract.Document.MIME_TYPE_DIR
                val childRel = joinRel(rel, name)
                val obj = JSObject()
                obj.put("name", name)
                obj.put("relPath", childRel)
                obj.put("isDir", isDir)
                if (!isDir) {
                    if (sizeIdx >= 0 && !cursor.isNull(sizeIdx)) obj.put("size", cursor.getLong(sizeIdx))
                    if (modIdx >= 0 && !cursor.isNull(modIdx)) obj.put("modified", cursor.getLong(modIdx))
                }
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
            cur = findChildVerified(cur, segment) ?: return null
        }
        return cur
    }

    private fun joinRel(parent: String, name: String): String {
        val p = parent.trim().trim('/').trim('\\')
        return if (p.isEmpty() || p == ".") name else "$p/$name"
    }
}
