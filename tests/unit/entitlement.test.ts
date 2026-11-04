/**
 * Tests for `src/lib/entitlement.ts` — the frontend mirror of the Rust
 * entitlement gate. Verifies it reads the backend snapshot, exposes
 * `isPro`/`allows`, and — critically — fails closed to Free when the read
 * errors (the prime invariant: a billing read must never block the user).
 *
 * `./invoke` is fully mocked so the store can be exercised without the Tauri
 * runtime; the mock owns a controllable backend response.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";

// Controllable stand-in for the backend `get_entitlement` response.
let nextSnapshot: { tier: string; allowed: string[] };
let shouldThrow = false;

vi.mock("../../src/lib/invoke", () => ({
  commands: {
    getEntitlement: () =>
      Promise.resolve(
        shouldThrow
          ? { status: "error", error: "backend unavailable" }
          : { status: "ok", data: nextSnapshot },
      ),
  },
  // Mirrors the real `unwrap`: unbox `ok`, throw on `error`.
  unwrap: async (p: Promise<{ status: string; data?: unknown; error?: unknown }>) => {
    const r = await p;
    if (r.status === "ok") return r.data;
    throw r.error;
  },
}));

import { entitlement } from "../../src/lib/entitlement";

describe("entitlement store", () => {
  beforeEach(() => {
    shouldThrow = false;
    nextSnapshot = { tier: "Free", allowed: [] };
  });

  it("defaults to Free (fail-closed) before any refresh", () => {
    expect(entitlement.tier()).toBe("Free");
    expect(entitlement.isPro()).toBe(false);
    expect(entitlement.allows("RelaySync")).toBe(false);
    expect(entitlement.allows("HostedAi")).toBe(false);
  });

  it("reflects a Pro snapshot after refresh", async () => {
    nextSnapshot = { tier: "Pro", allowed: ["RelaySync", "HostedAi"] };
    await entitlement.refresh();
    expect(entitlement.isPro()).toBe(true);
    expect(entitlement.allows("RelaySync")).toBe(true);
    expect(entitlement.allows("HostedAi")).toBe(true);
  });

  it("fails closed to Free when the backend read throws", async () => {
    // Go Pro first...
    nextSnapshot = { tier: "Pro", allowed: ["RelaySync", "HostedAi"] };
    await entitlement.refresh();
    expect(entitlement.isPro()).toBe(true);
    // ...then a failing refresh must drop back to Free, not stay Pro.
    shouldThrow = true;
    await entitlement.refresh();
    expect(entitlement.isPro()).toBe(false);
    expect(entitlement.allows("RelaySync")).toBe(false);
  });
});
