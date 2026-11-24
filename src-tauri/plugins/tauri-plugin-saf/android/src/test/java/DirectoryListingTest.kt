package app.tauri.saf

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Plain-JVM tests for [classifyDirCursor] — the decision that stops a null /
 * erroring / still-loading provider cursor from being read as a (smaller or
 * empty) directory. `./gradlew :tauri-plugin-saf:testDebugUnitTest`.
 *
 * These pin the decision table only. How `SafPlugin.queryChildrenChecked`
 * wires real `Cursor.extras` into it still needs the emulator / physical
 * provider run described in the data-safety plan (Batch 1 tests).
 */
class DirectoryListingTest {
    @Test
    fun nullCursorIsAFailureNotAnEmptyListing() {
        val s = classifyDirCursor(cursorPresent = false, loading = false, error = null)
        assertTrue("a null cursor must not read as an empty directory", s is DirCursorState.Failed)
    }

    @Test
    fun providerErrorIsAFailure() {
        val s = classifyDirCursor(cursorPresent = true, loading = false, error = "quota exceeded")
        assertEquals(DirCursorState.Failed("quota exceeded"), s)
    }

    @Test
    fun blankProviderErrorStillFails() {
        val s = classifyDirCursor(cursorPresent = true, loading = false, error = "  ")
        assertTrue(s is DirCursorState.Failed)
    }

    @Test
    fun loadingProviderIsNotComplete() {
        val s = classifyDirCursor(cursorPresent = true, loading = true, error = null)
        assertEquals(DirCursorState.Loading, s)
    }

    @Test
    fun errorWinsOverLoading() {
        val s = classifyDirCursor(cursorPresent = true, loading = true, error = "boom")
        assertEquals(DirCursorState.Failed("boom"), s)
    }

    @Test
    fun plainCursorIsComplete() {
        val s = classifyDirCursor(cursorPresent = true, loading = false, error = null)
        assertEquals(DirCursorState.Complete, s)
    }
}
