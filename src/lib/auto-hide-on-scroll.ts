import { createSignal, onCleanup } from "solid-js";

/**
 * Auto-hide-on-scroll.
 *
 * Returns a `hidden` accessor that flips to `true` the moment the user
 * scrolls and back to `false` once scrolling has been idle for `idleMs`.
 * Wire the returned `onScroll` to a scroll container's scroll event (or a
 * SolidJS `onScroll={...}` handler).
 *
 * Used to slide the mobile bottom bar out of the way while the user reads /
 * scrolls a note, then bring it back when they stop — a standard mobile
 * chrome-on-scroll pattern. The debounce coalesces iOS momentum scrolling
 * (which fires many scroll events) into a single hide → reveal cycle.
 *
 * Must be called within a reactive owner (component setup) — it registers
 * an `onCleanup` to clear its pending timer.
 */
export function createAutoHideOnScroll(idleMs = 250) {
  const [hidden, setHidden] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;

  const onScroll = () => {
    setHidden(true);
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => setHidden(false), idleMs);
  };

  onCleanup(() => {
    if (timer) clearTimeout(timer);
  });

  return { hidden, onScroll };
}
