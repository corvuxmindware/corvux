/**
 * AUDIT-051: contract tests for the `rebuild_database` IPC the
 * Settings → Diagnostics → "Repair database" button calls.
 *
 * The button always pre-confirms via `window.confirm` so a misclick
 * doesn't trigger a roost re-walk. These tests pin the wire contract
 * (the IPC name, the `{ roostPath }` arg shape, the GraphStatsResult
 * return shape) and verify that a backend error is propagated so the
 * Settings toast can surface it.
 */

import { describe, it, expect } from "vitest";
import { mockInvoke } from "./setup";
import { appInvoke } from "../../src/lib/invoke";

interface GraphStatsResult {
  notes: number;
  links: number;
  tags: number;
}

describe("rebuild_database IPC (AUDIT-051)", () => {
  it("passes the roost path through and returns the rebuilt-stats payload", async () => {
    const calls: Array<Record<string, unknown>> = [];
    mockInvoke.set("rebuild_database", (args) => {
      calls.push(args);
      return { notes: 42, links: 7, tags: 3 };
    });

    const stats = await appInvoke<GraphStatsResult>("rebuild_database", {
      roostPath: "/home/user/MyRoost",
    });

    expect(calls).toHaveLength(1);
    expect(calls[0]).toEqual({ roostPath: "/home/user/MyRoost" });
    expect(stats).toEqual({ notes: 42, links: 7, tags: 3 });
  });

  it("propagates backend errors so the Settings toast can surface them", async () => {
    mockInvoke.set("rebuild_database", () => {
      throw "roost backend not registered";
    });
    await expect(
      appInvoke<GraphStatsResult>("rebuild_database", { roostPath: "/v" }),
    ).rejects.toBe("roost backend not registered");
  });

  it("surfaces the __db_corrupt__ marker init_graph emits when graph.db is unreadable", async () => {
    // init_graph (which rebuild_database also calls under the hood)
    // tags corruption with this prefix so the frontend can branch on
    // it without inspecting the message. This test pins the contract
    // — if the prefix ever changes, the Settings UX needs to update.
    mockInvoke.set("init_graph", () => {
      throw "__db_corrupt__:database disk image is malformed";
    });
    await expect(
      appInvoke<GraphStatsResult>("init_graph", { roostPath: "/v" }),
    ).rejects.toMatch(/^__db_corrupt__:/);
  });
});
