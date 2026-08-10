import { describe, expect, it } from "vitest";
import { highlightSegments } from "./HistorySearchModal";

describe("highlightSegments", () => {
  it("highlights every case-insensitive match without changing text", () => {
    expect(highlightSegments("Needle then NEEDLE", "needle")).toEqual([
      { text: "Needle", match: true },
      { text: " then ", match: false },
      { text: "NEEDLE", match: true },
    ]);
  });

  it("returns unmatched text intact", () => {
    expect(highlightSegments("plain output", "missing")).toEqual([
      { text: "plain output", match: false },
    ]);
  });
});
