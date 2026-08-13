import { beforeEach, describe, expect, it, vi } from "vitest";
import { sessionDisplayPath } from "../components/SessionPath";
import { api, type Session } from "./api";
import { useSessions } from "./sessionStore";

function session(id: string, overrides: Partial<Session> = {}): Session {
  return {
    id,
    name: id,
    backend: "claude",
    execution_policy: "unrestricted",
    location: "local",
    base_dir: "/code/slide",
    project_path: `/code/slide/.slide-worktrees/${id}`,
    worktree: true,
    state: "active",
    created_at: 1,
    last_activity: 1,
    supervisor: "tmux",
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

  it("keeps live sessions stable and stopped sessions at the bottom", () => {
    const older = session("older", { created_at: 10, last_activity: 100 });
    const newer = session("newer", { created_at: 20, last_activity: 20 });
    const stopped = session("stopped", {
      state: "stopped",
      created_at: 30,
    });
    useSessions.getState().loadSnapshot([stopped, older, newer]);
    expect(useSessions.getState().order).toEqual(["newer", "older", "stopped"]);

    useSessions.getState().upsert({
      ...older,
      state: "waiting",
      last_activity: 10_000,
    });
    expect(useSessions.getState().order).toEqual(["newer", "older", "stopped"]);

    useSessions.getState().upsert({ ...newer, state: "stopped" });
    expect(useSessions.getState().order).toEqual(["older", "stopped", "newer"]);

    useSessions.getState().upsert({ ...stopped, state: "active" });
    expect(useSessions.getState().order).toEqual(["stopped", "older", "newer"]);
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

describe("session store mutations", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useSessions.setState({
      sessions: {},
      order: [],
      activeId: null,
      connected: false,
      authError: null,
      error: null,
    });
  });

  it("applies create, update, and delete responses without waiting for events", async () => {
    const created = session("created", { created_at: 10 });
    vi.spyOn(api, "createSession").mockResolvedValue(created);

    await expect(
      useSessions.getState().createSession({
        name: "created",
        backend: "claude",
        base_dir: "/code/slide",
      }),
    ).resolves.toEqual(created);
    expect(useSessions.getState().sessions.created).toEqual(created);

    const stopped = { ...created, state: "stopped" as const };
    vi.spyOn(api, "updateSession").mockResolvedValue(stopped);
    await useSessions.getState().updateSession(created.id, { action: "stop" });
    expect(useSessions.getState().sessions.created).toEqual(stopped);

    useSessions.getState().setActive(created.id);
    vi.spyOn(api, "deleteSession").mockResolvedValue({ ok: true });
    await useSessions.getState().deleteSession(created.id);
    expect(useSessions.getState().sessions.created).toBeUndefined();
    expect(useSessions.getState().activeId).toBeNull();
  });

  it("inserts fork and handoff responses before selecting them", async () => {
    const fork = session("fork", { parent_session_id: "source" });
    vi.spyOn(api, "forkSession").mockResolvedValue(fork);
    await useSessions
      .getState()
      .forkSession("source", { name: "fork", focus: "alternate path" });
    expect(useSessions.getState().sessions.fork).toEqual(fork);

    const target = session("target", { state: "waiting", last_activity: 20 });
    vi.spyOn(api, "handoffSession").mockResolvedValue(target);
    await useSessions.getState().handoffSession("source", {
      target_session_id: "target",
      focus: "continue here",
    });
    expect(useSessions.getState().sessions.target).toEqual(target);
  });
});
