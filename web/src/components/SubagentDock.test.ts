import { describe, expect, it } from "vitest";
import type { Subagent, SubagentState } from "../state/api";
import { sortSubagents, subagentLabel } from "./SubagentDock";

function agent(
  id: string,
  state: SubagentState,
  updatedAt: number,
  overrides: Partial<Subagent> = {},
): Subagent {
  return {
    id,
    parent_id: "root",
    name: null,
    role: null,
    state,
    created_at: 1,
    updated_at: updatedAt,
    ...overrides,
  };
}

describe("subagent dock helpers", () => {
  it("keeps live work first and newest peers first", () => {
    const sorted = sortSubagents([
      agent("done", "completed", 50),
      agent("older", "running", 10),
      agent("newer", "running", 20),
      agent("blocked", "waiting", 100),
      agent("failed", "failed", 200),
    ]);
    expect(sorted.map((item) => item.id)).toEqual([
      "newer",
      "older",
      "blocked",
      "failed",
      "done",
    ]);
  });

  it("uses nickname, role, then a bounded id fallback", () => {
    expect(
      subagentLabel(agent("123456789", "running", 1, { name: "worker", role: "tests" })),
    ).toBe("worker");
    expect(
      subagentLabel(agent("123456789", "running", 1, { role: "tests" })),
    ).toBe("tests");
    expect(subagentLabel(agent("123456789", "running", 1))).toBe(
      "agent 12345678",
    );
  });

  it("does not mutate the provider snapshot order", () => {
    const input = [agent("done", "completed", 1), agent("live", "running", 2)];
    sortSubagents(input);
    expect(input.map((item) => item.id)).toEqual(["done", "live"]);
  });
});
