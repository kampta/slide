import { beforeEach, describe, expect, it } from "vitest";
import { sessionDisplayPath } from "../components/SessionPath";
import type { Session } from "./api";
import { useSessions } from "./sessionStore";

function session(id: string, overrides: Partial<Session> = {}): Session {
  return {
    id,
    name: id,
    backend: "claude",
    location: "local",
    base_dir: "/code/slide",
    project_path: `/code/slide/.slide-worktrees/${id}`,
    worktree: true,
    state: "active",
    created_at: 1,
    last_activity: 1,
    supervisor: "tmux",
    log_offset: 0,
    ...overrides,
  };
}

describe("session store snapshots", () => {
  beforeEach(() => {
    useSessions.setState({
      sessions: {},
      order: [],
      activeId: null,
      connected: false,
      authError: null,
      error: null,
    });
  });

  it("keeps a selected session when it still exists", () => {
    useSessions.setState({ activeId: "one" });
    useSessions.getState().loadSnapshot([session("one"), session("two")]);
    expect(useSessions.getState().activeId).toBe("one");
  });

  it("clears a stale selection so mobile returns to the list", () => {
    useSessions.setState({ activeId: "removed" });
    useSessions.getState().loadSnapshot([session("one")]);
    expect(useSessions.getState().activeId).toBeNull();
  });

  it("does not rewrite state for an identical event", () => {
    const original = session("one");
    useSessions.getState().loadSnapshot([original]);
    const sessionsBefore = useSessions.getState().sessions;
    useSessions.getState().upsert({ ...original });
    expect(useSessions.getState().sessions).toBe(sessionsBefore);
  });
});

describe("sessionDisplayPath", () => {
  it("uses the same host:repo/name shape for local and remote sessions", () => {
    expect(sessionDisplayPath(session("fix"))).toBe("local:slide/fix");
    expect(
      sessionDisplayPath(
        session("fix", { location: "remote", ssh_host: "buildbox" }),
      ),
    ).toBe("buildbox:slide/fix");
  });
});
