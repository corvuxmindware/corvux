//! Android `ndk_context` initialization shim.
//!
//! ## Why this exists
//!
//! Crates that reach Android system APIs from native code — here, iroh's
//! network monitor, invoked the first time a sync **iroh endpoint** is built
//! (i.e. when the user taps "Show pairing code") — read the process-global
//! Android context via [`ndk_context::android_context`]. That global is
//! normally set by `tao`'s `create()` JNI binding, **but Tauri V2's Android
//! flow never runs that path**, so the global stays empty. iroh then reads it,
//! `ndk-context` panics with *"android context was not initialized"*, and
//! because the release profile sets `panic = "abort"` the panic becomes a
//! `SIGABRT` that kills the whole process — a sudden, message-less shutdown.
//!
//! Tauri V2 exposes no public API for the Android context
//! (`RuntimeHandle::run_on_android_context` is private — see
//! tauri-apps/tauri#13267), so we supply the JNI glue ourselves: the
//! `MulticastLockPlugin` (which Tauri instantiates with the `Activity` and
//! whose `load()` runs at startup, well before any pairing) calls the native
//! method below with the application `Context`. We grab the `JavaVM` from the
//! env and hand both raw pointers to [`ndk_context::initialize_android_context`].
//!
//! ## Why it lives in the main crate (not the plugin)
//!
//! The symbol must be **exported from the final `libcorvux_lib.so`** for the
//! JVM to resolve the plugin's `external fun`. `#[no_mangle]` symbols in a
//! dependency rlib can be stripped; the cdylib **root** crate reliably exports
//! them. The Kotlin side stays in the plugin (it owns the `Activity`); only the
//! symbol name ties the two together, and JNI resolves by name across the `.so`.

use std::ffi::c_void;

use jni::objects::JObject;
use jni::JNIEnv;

/// JNI entry: `MulticastLockPlugin.nativeInitNdkContext(Context)`.
///
/// Called once from the plugin's `load()` at app startup. Initializes the
/// global `ndk_context` with the real `JavaVM` + a global ref to the supplied
/// application `Context`. The global ref is intentionally **leaked** so the
/// `jobject` stays valid for the process lifetime (matching `ndk_context`'s
/// expectation that the stored pointer outlives every reader). Failures are
/// logged and swallowed — a failed init must not itself crash startup; the
/// worst case degrades to the original (already-broken) behavior.
#[no_mangle]
pub extern "system" fn Java_app_tauri_multicastlock_MulticastLockPlugin_nativeInitNdkContext(
    env: JNIEnv,
    _this: JObject,
    context: JObject,
) {
    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            tracing::error!("ndk_context init: get_java_vm failed: {e}");
            return;
        }
    };
    let ctx_global = match env.new_global_ref(&context) {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("ndk_context init: new_global_ref failed: {e}");
            return;
        }
    };

    // SAFETY: `get_java_vm_pointer()` returns the process-global JavaVM, valid
    // for the process lifetime. `ctx_global` is a JNI global reference we leak
    // immediately below, so the underlying `jobject` remains valid as long as
    // `ndk_context` holds the raw pointer (i.e. forever, in practice).
    unsafe {
        ndk_context::initialize_android_context(
            vm.get_java_vm_pointer() as *mut c_void,
            ctx_global.as_raw() as *mut c_void,
        );
    }
    std::mem::forget(ctx_global);

    tracing::info!("ndk_context initialized via MulticastLockPlugin.load()");
}
