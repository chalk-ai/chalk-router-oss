import { describe, expect, it } from "vitest";
import { activeHeuristicKeywords, addHeuristicKeyword, removeHeuristicKeyword } from "./heuristicKeywords";
import { DEFAULT_HARD_KEYWORDS, heuristicKeywordError, type HeuristicBackend } from "./routeGrid";

const defaults = (): HeuristicBackend => ({ type: "heuristic", added_hard_keywords: [], removed_hard_keywords: [] });

describe("editing the active heuristic keyword list", () => {
  it("starts with defaults and removes/re-adds a default without overlapping overrides", () => {
    expect(activeHeuristicKeywords(defaults())).toEqual(DEFAULT_HARD_KEYWORDS);
    const removed = removeHeuristicKeyword(defaults(), "debug");
    expect(activeHeuristicKeywords(removed)).not.toContain("debug");
    expect(removed.removed_hard_keywords).toEqual(["debug"]);
    const restored = addHeuristicKeyword(removed, "DEBUG");
    expect(activeHeuristicKeywords(restored)).toEqual(DEFAULT_HARD_KEYWORDS);
    expect(restored.added_hard_keywords).toEqual([]);
    expect(heuristicKeywordError(restored.added_hard_keywords, restored.removed_hard_keywords)).toBeNull();
  });

  it("adds and removes a custom phrase without changing other settings", () => {
    const config = { ...defaults(), code_blocks_are_hard: false, max_easy_input_bytes: 0 };
    const added = addHeuristicKeyword(config, "complex analysis");
    expect(activeHeuristicKeywords(added)).toContain("complex analysis");
    expect(removeHeuristicKeyword(added, "COMPLEX ANALYSIS")).toEqual(config);
  });

  it("preserves trailing spaces because they change substring matching", () => {
    const removed = removeHeuristicKeyword(defaults(), "prove ");
    expect(removed.removed_hard_keywords).toEqual(["prove "]);
    const different = addHeuristicKeyword(removed, "prove");
    expect(activeHeuristicKeywords(different)).toContain("prove");
    expect(activeHeuristicKeywords(different)).not.toContain("prove ");
    const restored = addHeuristicKeyword(removed, "prove ");
    expect(restored).toEqual(defaults());
  });

  it("keeps defaults disabled for an existing custom-only configuration", () => {
    const config = { ...defaults(), use_default_keywords: false, added_hard_keywords: ["custom"] };
    expect(activeHeuristicKeywords(config)).toEqual(["custom"]);
    const added = addHeuristicKeyword(config, "debug");
    expect(activeHeuristicKeywords(added)).toEqual(["custom", "debug"]);
    expect(removeHeuristicKeyword(added, "debug")).toEqual(config);
  });

  it("allows removing every keyword", () => {
    const empty = DEFAULT_HARD_KEYWORDS.reduce(removeHeuristicKeyword, defaults());
    expect(activeHeuristicKeywords(empty)).toEqual([]);
    expect(heuristicKeywordError(empty.added_hard_keywords, empty.removed_hard_keywords)).toBeNull();
  });

  it("removes both copies when an addition duplicates a default", () => {
    const config = { ...defaults(), added_hard_keywords: ["DEBUG"] };
    expect(activeHeuristicKeywords(config).filter(word => word.toLowerCase() === "debug")).toHaveLength(1);
    const removed = removeHeuristicKeyword(config, "debug");
    expect(activeHeuristicKeywords(removed)).not.toContain("debug");
    expect(heuristicKeywordError(removed.added_hard_keywords, removed.removed_hard_keywords)).toBeNull();
  });

  it("shares keyword validation with the route form and BFF", () => {
    expect(heuristicKeywordError(["DEBUG"], ["debug"])).toMatch(/both added and removed/);
    expect(heuristicKeywordError(["é".repeat(129)], [])).toMatch(/256 bytes/);
    expect(heuristicKeywordError(["   "], [])).toMatch(/nonblank/);
    expect(heuristicKeywordError(Array(257).fill("a"), [])).toMatch(/256 keyword overrides/);
  });
});
