import { createEffect, onCleanup } from "solid-js";
import { isAndroid } from "../lib/platform";

/// Make the Android hardware back button close a transient overlay (the
/// mobile nav drawer, the chat panel, …) instead of exiting the app.
///
/// Mirrors the BottomSheet / SettingsPanel pattern: while `isOpen` is
/// true we keep exactly one sentinel entry on the history stack. A
/// hardware back pops it → `popstate` fires → we call `close()`. If the
/// overlay is closed some other way (scrim tap, nav action) we consume
/// our own entry with `history.back()` so the stack stays balanced and a
/// later back press doesn't pop a stale entry.
///
/// No-op off Android — desktop/iOS have no hardware back button and use
/// Esc / scrim taps. Compose freely: each call owns an independent entry,
/// so two open overlays close most-recent-first (LIFO), which is the
/// expected behaviour. (Audit 2026-06-08, MED-5.)
export function createAndroidBackClose(
  isOpen: () => boolean,
  close: () => void,
): void {
  if (!isAndroid()) return;

  let armed = false; // do we currently own a history entry?
  let poppedByBack = false; // did the pending close originate from a back press?

  const onPop = () => {
    // The OS already removed our entry; just react to it.
    poppedByBack = true;
    armed = false;
    window.removeEventListener("popstate", onPop);
    close();
  };

  createEffect(() => {
    const open = isOpen();
    if (open && !armed) {
      // Overlay just opened — claim a history entry to absorb one back press.
      poppedByBack = false;
      armed = true;
      history.pushState({ corvuxBackClose: true }, "");
      window.addEventListener("popstate", onPop);
    } else if (!open && armed) {
      // Overlay closed by something other than back — reclaim our entry.
      window.removeEventListener("popstate", onPop);
      armed = false;
      if (!poppedByBack) history.back();
    }
  });

  onCleanup(() => {
    if (!armed) return;
    window.removeEventListener("popstate", onPop);
    armed = false;
    if (!poppedByBack) history.back();
  });
}
