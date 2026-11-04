// @vitest-environment happy-dom
/**
 * TopBar sync `↑N` outbound indicator.
 *
 * Regression for the bug where the indicator painted `↑1` (up-arrow + count)
 * even with sync DISABLED: every local save optimistically bumped an
 * "outgoing" counter (`effects.ts` → `syncStatusStore.recordOutgoing`), which
 * conflated "I saved a file locally" with "I have N changes outbound to a
 * peer". With no peer — or sync off entirely — nothing is actually outbound,
 * so the arrow was misleading.
 *
 * The fix makes `outgoing` the backend's authoritative durable-outbox depth:
 * it is driven ONLY by `outbox_changed` events (and the startup seed), never
 * by local saves. The backend enqueues to the outbox only when the queue
 * client is running AND the peer is deliverable, so the depth is 0 whenever
 * sync is off or no peer exists. These tests lock that contract in.
 */
import { describe, it, expect } from "vitest";
import { createRoot } from "solid-js";
import { createSyncStatusStore } from "../../src/stores/sync";
import { shouldShowTransferCounts } from "../../src/components/TopBar/SyncIndicator";

type Store = ReturnType<typeof createSyncStatusStore>;

function withStore(fn: (store: Store) => void) {
  createRoot((dispose) => {
    fn(createSyncStatusStore());
    dispose();
  });
}

describe("sync outbound (↑N) indicator", () => {
  it("starts with no outbound backlog", () => {
    withStore((s) => {
      expect(s.outgoing()).toBe(0);
    });
  });

  it("reflects the durable outbox depth from outbox_changed events", () => {
    withStore((s) => {
      s.handleSyncEvent({ kind: "outbox_changed", total_pending: 3 });
      expect(s.outgoing()).toBe(3);

      // Delivery drains the outbox -> the count follows it down to 0.
      s.handleSyncEvent({ kind: "outbox_changed", total_pending: 0 });
      expect(s.outgoing()).toBe(0);
    });
  });

  it("does NOT expose an optimistic per-save bump (the old bug's source)", () => {
    withStore((s) => {
      // The removed `recordOutgoing` is what painted `↑1` on every local save
      // regardless of sync state. It must no longer exist on the store.
      expect("recordOutgoing" in s).toBe(false);
    });
  });

  it("leaves the outbound backlog untouched on inbound (note_written) activity", () => {
    withStore((s) => {
      s.handleSyncEvent({ kind: "outbox_changed", total_pending: 2 });
      // An inbound sync write bumps the ↓ counter but must not move ↑.
      s.handleSyncEvent({ kind: "note_written", path: "notes/from-peer.md" });
      expect(s.incoming()).toBe(1);
      expect(s.outgoing()).toBe(2);
    });
  });

  it("spins the icon on outbox DRAIN (delivery) but not on enqueue", () => {
    withStore((s) => {
      expect(s.lastActivityAt()).toBe(0);
      // Enqueue (count goes up because a peer is offline) is backlog growth,
      // not live traffic — it must NOT pulse the "syncing now" icon.
      s.handleSyncEvent({ kind: "outbox_changed", total_pending: 2 });
      expect(s.outgoing()).toBe(2);
      expect(s.lastActivityAt()).toBe(0);
      // Delivery (count drops) is real outbound traffic — pulse the icon.
      s.handleSyncEvent({ kind: "outbox_changed", total_pending: 1 });
      expect(s.lastActivityAt()).toBeGreaterThan(0);
    });
  });

  it("clears the outbound backlog when the engine stops", () => {
    withStore((s) => {
      s.handleSyncEvent({ kind: "outbox_changed", total_pending: 5 });
      expect(s.outgoing()).toBe(5);
      s.handleSyncEvent({ kind: "stopped" });
      expect(s.outgoing()).toBe(0);
    });
  });
});

describe("transfer-count display gate (bug #3)", () => {
  it("hides ↑N/↓N when no peer is connected, even with a backlog", () => {
    // The exact 0.2.1 report: outbox depth 2 on a roost with no peer. A
    // backlog with nobody to send to is not "uploading" — don't paint it.
    expect(shouldShowTransferCounts(0, 0, 2)).toBe(false);
    expect(shouldShowTransferCounts(0, 3, 0)).toBe(false);
    expect(shouldShowTransferCounts(0, 0, 0)).toBe(false);
  });

  it("shows the readout only when a peer is connected AND something moved", () => {
    expect(shouldShowTransferCounts(1, 0, 2)).toBe(true);
    expect(shouldShowTransferCounts(2, 4, 0)).toBe(true);
    // Peer connected but nothing in flight -> still no number.
    expect(shouldShowTransferCounts(1, 0, 0)).toBe(false);
  });
});

describe("resetForRoostSwitch (bug #3)", () => {
  it("zeroes indicator state so it can't bleed across roosts", () => {
    withStore((s) => {
      s.handleSyncEvent({ kind: "outbox_changed", total_pending: 2 });
      s.handleSyncEvent({ kind: "note_written", path: "notes/x.md" });
      expect(s.outgoing()).toBe(2);
      expect(s.incoming()).toBe(1);

      s.resetForRoostSwitch();
      expect(s.outgoing()).toBe(0);
      expect(s.incoming()).toBe(0);
      expect(s.connectedPeers()).toBe(0);
      expect(s.listening()).toBe(false);
      expect(s.connecting()).toBe(false);
    });
  });
});
