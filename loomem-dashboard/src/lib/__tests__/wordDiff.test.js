import { describe, it, expect } from "vitest";
import { wordDiff } from "../wordDiff";

// Cycle/173 S3 — word-level LCS diff used by the memory History tab.

describe("wordDiff", () => {
  const join = (segs, type) =>
    segs
      .filter((s) => s.type === type)
      .map((s) => s.text)
      .join("");

  it("marks a substituted word as del + add, keeps the rest equal", () => {
    const segs = wordDiff("the cat sat", "the dog sat");
    expect(join(segs, "del")).toContain("cat");
    expect(join(segs, "add")).toContain("dog");
    expect(join(segs, "eq")).toContain("the");
    expect(join(segs, "eq")).toContain("sat");
  });

  it("identical text → all equal, no add/del", () => {
    const segs = wordDiff("same words here", "same words here");
    expect(segs.every((s) => s.type === "eq")).toBe(true);
  });

  it("pure insertion is all add; joining segments reproduces the new text", () => {
    const segs = wordDiff("", "brand new content");
    expect(segs.every((s) => s.type === "add")).toBe(true);
    expect(segs.map((s) => s.text).join("")).toBe("brand new content");
  });

  it("pure deletion is all del", () => {
    const segs = wordDiff("gone forever", "");
    expect(segs.every((s) => s.type === "del")).toBe(true);
  });

  it("segments rejoin to the original texts (lossless tokens)", () => {
    const oldT = "one two three four";
    const newT = "one three four five";
    const segs = wordDiff(oldT, newT);
    const oldReconstructed = segs
      .filter((s) => s.type !== "add")
      .map((s) => s.text)
      .join("");
    const newReconstructed = segs
      .filter((s) => s.type !== "del")
      .map((s) => s.text)
      .join("");
    expect(oldReconstructed).toBe(oldT);
    expect(newReconstructed).toBe(newT);
  });

  it("handles null/undefined input without throwing", () => {
    expect(() => wordDiff(undefined, null)).not.toThrow();
    expect(wordDiff(undefined, "x").map((s) => s.text).join("")).toBe("x");
  });
});
