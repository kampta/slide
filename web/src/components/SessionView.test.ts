import { describe, expect, it } from "vitest";
import {
  policyForBackend,
  resumeActionLabel,
  resumeActionTitle,
} from "./SessionView";
import type { BackendInfo } from "../state/api";

const codex: BackendInfo = {
  id: "codex",
  label: "Codex",
  context_usage: true,
  fork: true,
  execution_policies: ["unrestricted", "sandboxed_auto"],
};

const claude: BackendInfo = {
  ...codex,
  id: "claude",
  label: "Claude",
  execution_policies: ["unrestricted"],
};

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

describe("policyForBackend", () => {
  it("keeps a policy supported by the selected backend", () => {
    expect(policyForBackend(codex, "sandboxed_auto")).toBe("sandboxed_auto");
  });

  it("falls back safely when switching to an unsupported backend", () => {
    expect(policyForBackend(claude, "sandboxed_auto")).toBe("unrestricted");
  });

  it("preserves the stored policy while backend metadata is loading", () => {
    expect(policyForBackend(undefined, "sandboxed_auto")).toBe(
      "sandboxed_auto",
    );
  });
});
