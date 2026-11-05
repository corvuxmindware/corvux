package app.tauri.multicastlock

import android.app.Activity
import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log
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
