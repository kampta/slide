import { describe, expect, it } from "vitest";
import {
  availableTurn,
  sortTurnDiffs,
  turnStats,
} from "./TurnDiffDock";
import type { TurnDiffSummary } from "../state/api";

function turn(
  id: number,
  ordinal: number,
  filesChanged = 1,
): TurnDiffSummary {
  return {
    id,
    turn: ordinal,
    started_at: 100,
    completed_at: 200,
    files_changed: filesChanged,
    additions: 4,
    deletions: 2,
    truncated: false,
  };
}

describe("TurnDiffDock helpers", () => {
  it("sorts newest first without mutating API data", () => {
    const input = [turn(1, 1), turn(3, 3), turn(2, 2)];
    expect(sortTurnDiffs(input).map((item) => item.id)).toEqual([3, 2, 1]);
    expect(input.map((item) => item.id)).toEqual([1, 3, 2]);
  });

  it("preserves a selected turn and falls back to the newest", () => {
    const turns = [turn(3, 3), turn(2, 2)];
    expect(availableTurn(2, turns)).toBe(2);
    expect(availableTurn(1, turns)).toBe(3);
    expect(availableTurn(null, [])).toBeNull();
  });

  it("formats singular and plural file statistics", () => {
    expect(turnStats(turn(1, 1))).toBe("1 file · +4 −2");
    expect(turnStats(turn(2, 2, 3))).toBe("3 files · +4 −2");
  });
});
