import { describe, expect, it } from "vitest";
import { resumeActionLabel, resumeActionTitle } from "./SessionView";

describe("resumeActionLabel", () => {
  it("says Resume when the backend is unchanged", () => {
    expect(resumeActionLabel("claude", "claude")).toBe("Resume");
  });

  it("says Start when switching backends", () => {
    expect(resumeActionLabel("claude", "codex")).toBe("Start");
  });
});

describe("resumeActionTitle", () => {
  it("explains a fresh conversation after a backend switch", () => {
    expect(resumeActionTitle("claude", "codex", true)).toContain("fresh codex");
    expect(resumeActionTitle("claude", "codex", true)).toContain("claude");
  });

  it("mentions prior conversation when resuming the same backend with an id", () => {
    expect(resumeActionTitle("claude", "claude", true)).toBe(
      "Resume the prior conversation.",
    );
  });

  it("mentions latest-or-fresh when resuming without a provider id", () => {
    expect(resumeActionTitle("grok", "grok", false)).toMatch(/fresh/i);
  });
});
