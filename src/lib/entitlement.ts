/**
 * Frontend mirror of the Rust entitlement gate (`src-tauri/src/entitlement.rs`).
 *
 * This is the UI's single answer to "can the user use a paid capability?" —
 * read off a reactive snapshot fetched from the backend. The backend is the
 * source of truth: it returns the *resolved* set of unlocked capabilities, so
 * the frontend never re-implements the tier→capability map (no second copy to
 * drift). UI gates on membership in that set.
 *
 * The check here is a *soft* gate (drives show/hide + upgrade nudges). The
 * surfaces that actually cost money — the relay queue and the hosted-AI proxy
 * — hard-gate server-side on the signed token; this signal is convenience, not
 * enforcement.
 *
 * Phase A: the backend always returns Free, so `isPro()` is false for everyone
 * until payments ship. `refresh()` should be called once at app startup (and
 * again after a Phase-B activation). See `docs/decisions/tier-feature-split.md`.
 */
import { createRoot, createSignal } from "solid-js";

import type { Capability, EntitlementSnapshot, Tier } from "./bindings";
import { commands, unwrap } from "./invoke";

/** Fail-closed default. Any doubt → Free; never break the app on a billing read. */
const FREE: EntitlementSnapshot = { tier: "Free", allowed: [] };

function createEntitlementStore() {
  const [snapshot, setSnapshot] = createSignal<EntitlementSnapshot>(FREE);

  /** Pull the current entitlement from the backend. Fails closed to Free. */
  async function refresh(): Promise<void> {
    try {
      setSnapshot(await unwrap(commands.getEntitlement()));
    } catch {
      // A failed entitlement read must never block the user — drop to Free.
      setSnapshot(FREE);
    }
  }

  return {
    /** Current tier (reactive). */
    tier: (): Tier => snapshot().tier,
    /** Convenience: is the device on the paid tier? */
    isPro: (): boolean => snapshot().tier === "Pro",
    /** The one question UI asks before showing/enabling a paid feature. */
    allows: (cap: Capability): boolean => snapshot().allowed.includes(cap),
    refresh,
  };
}

/**
 * App-lifetime singleton — `createRoot` keeps the signal alive for the life of
 * the app (same pattern as `editHistoryStore` / `tasksStore`).
 */
export const entitlement = createRoot(createEntitlementStore);
