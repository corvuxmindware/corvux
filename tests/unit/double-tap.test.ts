import { createDoubleTapDetector } from "@/lib/double-tap";

describe("createDoubleTapDetector", () => {
  it("treats a first tap as a single tap", () => {
    const dt = createDoubleTapDetector(300);
    expect(dt.isDouble("a", 1000)).toBe(false);
  });

  it("detects a double-tap on the same key within the window", () => {
    const dt = createDoubleTapDetector(300);
    expect(dt.isDouble("a", 1000)).toBe(false);
    expect(dt.isDouble("a", 1200)).toBe(true); // 200ms < 300ms
  });

  it("does not double when the second tap is outside the window", () => {
    const dt = createDoubleTapDetector(300);
    expect(dt.isDouble("a", 1000)).toBe(false);
    expect(dt.isDouble("a", 1400)).toBe(false); // 400ms > 300ms → new first tap
  });

  it("does not double across different keys", () => {
    const dt = createDoubleTapDetector(300);
    expect(dt.isDouble("a", 1000)).toBe(false);
    expect(dt.isDouble("b", 1100)).toBe(false); // different node
  });

  it("resets after a successful double so a third tap is a fresh single", () => {
    const dt = createDoubleTapDetector(300);
    dt.isDouble("a", 1000);
    expect(dt.isDouble("a", 1100)).toBe(true);
    expect(dt.isDouble("a", 1150)).toBe(false); // window restarts
  });

  it("reset() forgets a pending first tap", () => {
    const dt = createDoubleTapDetector(300);
    dt.isDouble("a", 1000);
    dt.reset();
    expect(dt.isDouble("a", 1100)).toBe(false); // no longer counts as double
  });
});
