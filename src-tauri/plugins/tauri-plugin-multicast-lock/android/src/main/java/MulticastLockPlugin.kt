package app.tauri.multicastlock

import android.app.Activity
import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log
import android.webkit.WebView
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

/**
 * Acquires/releases a [WifiManager.MulticastLock] so mDNS multicast
 * traffic isn't dropped on the way to our app's socket. Without this,
 * `mdns-sd`'s browser sees zero peers on Android even when a desktop
 * is advertising on the same LAN.
 *
 * Reference-counted: calling acquire twice and release once still
 * leaves the lock held. We wrap that with our own held-flag so calls
 * stay symmetric — the Rust side acquires on sync start, releases on
 * sync stop, and the lock count stays at 0 or 1.
 *
 * The required `<uses-permission android:name="android.permission
 * .CHANGE_WIFI_MULTICAST_STATE" />` is declared in the main app's
 * manifest, not here, so the Gradle merger picks it up alongside the
 * other sync-related permissions.
 */
@TauriPlugin
class MulticastLockPlugin(private val activity: Activity) : Plugin(activity) {

    private val tag = "CorvuxMulticast"
    private var lock: WifiManager.MulticastLock? = null

    /**
     * Initialize the global `ndk_context` (JavaVM + Application context) as
     * soon as the plugin loads — BEFORE any sync pairing can run. Tauri V2's
     * Android flow never runs tao's `create()`, which would normally set it,
     * so iroh's network monitor would otherwise panic ("android context was
     * not initialized") on the first endpoint build and (under panic=abort)
     * crash the whole app. This is the only reliable startup hook we have
     * with the Activity in hand. See the Rust side in `src/android_context.rs`
     * + tauri-apps/tauri#13267.
     *
     * Wrapped so a failure here never blocks app startup — worst case we fall
     * back to the original (broken) behavior instead of crashing on launch.
     */
    override fun load(webView: WebView) {
        super.load(webView)
        try {
            nativeInitNdkContext(activity.applicationContext)
            Log.i(tag, "ndk_context initialized at plugin load")
        } catch (ex: Throwable) {
            Log.e(tag, "ndk_context init failed: ${ex.message}", ex)
        }
    }

    /** Implemented in Rust: `Java_app_tauri_multicastlock_MulticastLockPlugin_nativeInitNdkContext`. */
    private external fun nativeInitNdkContext(context: Context)

    @Command
    fun acquire(invoke: Invoke) {
        try {
            ensureLock()
            val l = lock!!
            if (!l.isHeld) {
                l.acquire()
                Log.i(tag, "MulticastLock acquired")
            } else {
                Log.d(tag, "MulticastLock already held; no-op")
            }
            invoke.resolve()
        } catch (ex: Exception) {
            Log.e(tag, "acquire failed: ${ex.message}", ex)
            invoke.reject(ex.message ?: "Failed to acquire MulticastLock")
        }
    }

    @Command
    fun release(invoke: Invoke) {
        try {
            val l = lock
            if (l != null && l.isHeld) {
                l.release()
                Log.i(tag, "MulticastLock released")
            } else {
                Log.d(tag, "MulticastLock not held; no-op")
            }
            invoke.resolve()
        } catch (ex: Exception) {
            Log.e(tag, "release failed: ${ex.message}", ex)
            invoke.reject(ex.message ?: "Failed to release MulticastLock")
        }
    }

    /**
     * Lazily create the lock the first time we need it. Reference-
     * counted mode is set explicitly to `false` so our own held-flag
     * tracking matches the OS view exactly — easier to reason about
     * than ref-counting from two layers.
     */
    private fun ensureLock() {
        if (lock != null) return
        val wifi = activity.applicationContext
            .getSystemService(Context.WIFI_SERVICE) as WifiManager
        val l = wifi.createMulticastLock("corvux-sync")
        l.setReferenceCounted(false)
        lock = l
    }
}
