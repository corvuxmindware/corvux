import { describe, it, expect } from "vitest";
import { sha256Hex } from "../../src/lib/hash";

// EDT-1: the webview-side base hash MUST byte-match the Rust save path's
// `content_sha256_hex` (same lowercase-hex SHA-256). These pin the exact
// vectors also asserted in the Rust test, so the two can't drift apart and
// silently turn every save into a false conflict (or miss real ones).
describe("sha256Hex (EDT-1 save-conflict base hash)", () => {
  it("matches the known SHA-256 vectors the backend also pins", async () => {
    expect(await sha256Hex("")).toBe(
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    );
    expect(await sha256Hex("abc")).toBe(
      "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    );
  });

  it("hashes unicode content stably (UTF-8 encoded, not code units)", async () => {
    const a = await sha256Hex("café — 日本語 🦉");
    const b = await sha256Hex("café — 日本語 🦉");
    expect(a).toBe(b);
    expect(a).toHaveLength(64); // 32 bytes hex
  });
});
