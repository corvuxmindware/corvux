/**
 * `isScopeExpiredError` — recognizes the backend's `SCOPE_EXPIRED` token so
 * openRoost can show a re-grant prompt instead of a dead-end EPERM. Must
 * stay in sync with the `SCOPE_EXPIRED` constant in commands/roost.rs.
 */

import { describe, it, expect } from "vitest";
import { isScopeExpiredError } from "../../src/stores/roosts";

describe("isScopeExpiredError", () => {
  it("matches the bare SCOPE_EXPIRED token (string error)", () => {
    expect(isScopeExpiredError("SCOPE_EXPIRED")).toBe(true);
  });

  it("matches when the token is embedded in a larger message", () => {
    expect(isScopeExpiredError("open_roost failed: SCOPE_EXPIRED")).toBe(true);
  });

  it("matches an Error instance carrying the token", () => {
    expect(isScopeExpiredError(new Error("SCOPE_EXPIRED"))).toBe(true);
  });

  it("does NOT match an ordinary open failure (so it still throws normally)", () => {
    expect(isScopeExpiredError("stat failed: Operation not permitted")).toBe(false);
    expect(isScopeExpiredError("does not exist")).toBe(false);
  });

  it("is safe on null/undefined", () => {
    expect(isScopeExpiredError(null)).toBe(false);
    expect(isScopeExpiredError(undefined)).toBe(false);
  });
});
