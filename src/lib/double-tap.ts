/**
 * Stateful double-tap detector. Created once and fed each tap; `isDouble`
 * returns true when the same key is tapped twice within `windowMs`.
 *
 * Extracted from GraphView's mobile node-tap handler so the "single tap shows
 * a card, double tap opens" branch is unit-testable without rendering the
 * whole graph. Pure bookkeeping — no DOM, no timers.
 */
export function createDoubleTapDetector(windowMs = 300) {
  let lastKey: string | null = null;
  let lastAt = 0;

  return {
    /**
     * Record a tap on `key` at time `now` (ms). Returns true if it completes a
     * double-tap on the same key inside the window (and resets so a third tap
     * starts fresh); false for a first/standalone tap.
     */
    isDouble(key: string, now: number): boolean {
      if (lastKey === key && now - lastAt < windowMs) {
        lastKey = null;
        lastAt = 0;
        return true;
      }
      lastKey = key;
      lastAt = now;
      return false;
    },
    /** Forget any pending first-tap (e.g. when the card is dismissed). */
    reset(): void {
      lastKey = null;
      lastAt = 0;
    },
  };
}
