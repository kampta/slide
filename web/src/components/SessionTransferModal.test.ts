import { describe, expect, it } from "vitest";
import { suggestedForkName } from "./SessionTransferModal";

describe("suggestedForkName", () => {
  it("uses a stable fork suffix when available", () => {
    expect(suggestedForkName("research", new Set())).toBe("research-fork");
  });

  it("increments without colliding with existing sessions", () => {
    expect(
      suggestedForkName(
        "research",
        new Set(["research-fork", "research-fork-2", "other"]),
      ),
    ).toBe("research-fork-3");
  });
});
