package app.tauri.saf

/**
 * Message prefix of every rejection that means "these rows are NOT the whole
 * directory" (null cursor, provider error, provider still loading).
 *
 * The Rust side (`roostfs/saf.rs`, `INCOMPLETE_LISTING_PREFIX`) matches this
 * prefix BEFORE its generic substring mapping, so a provider's own wording
 * (which may contain "not found") can never turn an incomplete listing into
 * `RoostError::NotFound`. `tests/saf_trash_guard.rs` keeps the two spellings
 * in lockstep.
 */
internal const val INCOMPLETE_LISTING_PREFIX = "Directory listing incomplete"

/**
 * Prefix of `moveDocument`'s own rejection when the in-between name (the item's
 * OLD name inside the destination folder, e.g. a stray `.corvux-trash/a.md`) is
 * taken. Rust (`roostfs/mod.rs`, `STAGING_NAME_TAKEN_PREFIX`) matches it to move
 * the stray aside instead of refusing; a matching marker, not a text pattern,
 * because a provider's own exception text or a note's name can contain any
 * phrase. `tests/saf_trash_guard.rs` keeps the two spellings in lockstep.
 */
internal const val STAGING_NAME_TAKEN_PREFIX = "Staging name taken"

/** Re-query attempts while a provider reports `EXTRA_LOADING`. */
internal const val LOADING_MAX_ATTEMPTS = 5

/** Linear back-off between loading re-queries: 300, 600, 900, 1200 ms. */
internal const val LOADING_BACKOFF_MS = 300L

/**
 * What a directory-listing cursor says about whether its rows are the whole
 * directory. Free of `android.*` types so it runs in a plain JVM unit test.
 */
internal sealed class DirCursorState {
    /** The rows are the complete listing. */
    object Complete : DirCursorState()

    /** Partial rows: the provider set `EXTRA_LOADING` and will notify when done. */
    object Loading : DirCursorState()

    /** No usable listing: no cursor at all, or the provider reported an error. */
    data class Failed(val reason: String) : DirCursorState()
}

/**
 * Decide whether a `ContentResolver.query` result may be read as a complete
 * directory.
 *
 * A null cursor is a FAILURE, never an empty directory: the platform returns
 * null when the provider returns null or its process dies, and
 * `query(...)?.use { ... }` used to turn that into a successful, empty
 * listing, which the tree cache then cached and the SAF trash fallback then
 * treated as "nothing else inside" before deleting the original folder.
 *
 * `error` wins over `loading`: a provider that reports both has failed.
 */
internal fun classifyDirCursor(
    cursorPresent: Boolean,
    loading: Boolean,
    error: String?,
): DirCursorState = when {
    !cursorPresent -> DirCursorState.Failed("provider returned no cursor")
    error != null -> DirCursorState.Failed(error.ifBlank { "provider reported an error" })
    loading -> DirCursorState.Loading
    else -> DirCursorState.Complete
}
