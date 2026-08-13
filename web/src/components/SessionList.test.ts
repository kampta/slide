import { describe, expect, it } from "vitest";
import type { Session } from "../state/api";
import { matchesSession } from "./SessionList";

const session: Session = {
  id: "session-1",
  name: "auth-refactor",
  backend: "codex",
  execution_policy: "unrestricted",
  location: "remote",
  ssh_host: "buildbox",
  base_dir: "/code/slide",
  project_path: "/code/slide/.slide-worktrees/auth-refactor",
  worktree: true,
  state: "waiting",
  created_at: 1,
  last_activity: 2,
  supervisor: "tmux",
};

describe("session filtering", () => {
  it("matches identity, host, backend, state, and path without case sensitivity", () => {
    for (const query of ["AUTH", "buildbox", "CODEX", "waiting", "slide-worktrees"]) {
      expect(matchesSession(session, query)).toBe(true);
    }
    expect(matchesSession(session, "unrelated")).toBe(false);
    expect(matchesSession(session, "  ")).toBe(true);
  });
});
