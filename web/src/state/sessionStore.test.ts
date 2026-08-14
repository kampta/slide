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

    useSessions.getState().upsert({
      ...stopped,
      state: "active",
      last_activity: stopped.last_activity + 1,
    });
    expect(useSessions.getState().order).toEqual(["stopped", "older", "newer"]);
  });

  it("does not let an older snapshot resurrect a stopped session", () => {
    const stopped = session("one", {
      state: "stopped",
      last_activity: 20,
    });
    useSessions.getState().loadSnapshot([stopped]);
    useSessions.getState().loadSnapshot([
      { ...stopped, state: "active", last_activity: 10 },
    ]);

    expect(useSessions.getState().sessions.one).toEqual(stopped);
  });

  it("orders state events by lifecycle timestamp", () => {
    const stopped = session("one", {
      state: "stopped",
      last_activity: 20,
    });
    useSessions.getState().loadSnapshot([stopped]);

    useSessions.getState().setSessionState("one", "active", 19);
    useSessions.getState().setSessionState("one", "active", 20);
    useSessions.getState().setSessionState("one", "active");
    expect(useSessions.getState().sessions.one).toEqual(stopped);

    useSessions.getState().setSessionState("one", "active", 21);
    useSessions.getState().setSessionState("one", "waiting", 21);
    expect(useSessions.getState().sessions.one).toMatchObject({
      state: "waiting",
      last_activity: 21,
    });

    useSessions.getState().setSessionState("one", "stopped");
    expect(useSessions.getState().sessions.one.state).toBe("stopped");
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

  it("inserts fork responses without waiting for events", async () => {
    const fork = session("fork", { parent_session_id: "source" });
    vi.spyOn(api, "forkSession").mockResolvedValue(fork);
    await useSessions
      .getState()
      .forkSession("source", { name: "fork", focus: "alternate path" });
    expect(useSessions.getState().sessions.fork).toEqual(fork);
  });

  it("does not let a stale handoff response overwrite a newer state event", async () => {
    const waiting = session("target", { state: "waiting", last_activity: 20 });
    const active = { ...waiting, state: "active" as const, last_activity: 30 };
    useSessions.getState().loadSnapshot([waiting]);
    vi.spyOn(api, "handoffSession").mockImplementation(async () => {
      useSessions.getState().upsert(active);
      return waiting;
    });

    await expect(useSessions.getState().handoffSession("source", {
      target_session_id: "target",
      focus: "continue here",
    })).resolves.toEqual(waiting);
    expect(useSessions.getState().sessions.target).toEqual(active);
  });

  it("preserves newer events that beat create and update responses", async () => {
    const original = session("existing", { last_activity: 10 });
    const updated = { ...original, state: "waiting" as const, last_activity: 30 };
    const staleUpdate = { ...original, state: "stopped" as const, last_activity: 20 };
    useSessions.getState().loadSnapshot([original]);
    vi.spyOn(api, "updateSession").mockImplementation(async () => {
      useSessions.getState().upsert(updated);
      return staleUpdate;
    });

    await useSessions.getState().updateSession(original.id, { action: "stop" });
    expect(useSessions.getState().sessions.existing).toEqual(updated);

    const staleCreate = session("created", { last_activity: 10 });
    const createdEvent = { ...staleCreate, backend_session_id: "provider-id" };
    vi.spyOn(api, "createSession").mockImplementation(async () => {
      useSessions.getState().upsert(createdEvent);
      return staleCreate;
    });

    await useSessions.getState().createSession({
      name: "created",
      backend: "claude",
      base_dir: "/code/slide",
    });
    expect(useSessions.getState().sessions.created).toEqual(createdEvent);
  });

  it("applies a newer lifecycle response after an intervening event", async () => {
    const original = session("existing", { created_at: 20, last_activity: 10 });
    const other = session("other", { created_at: 10, last_activity: 10 });
    const waiting = { ...original, state: "waiting" as const };
    const stopped = {
      ...original,
      state: "stopped" as const,
      last_activity: 20,
    };
    useSessions.getState().loadSnapshot([original, other]);
    vi.spyOn(api, "updateSession").mockImplementation(async () => {
      useSessions.getState().upsert(waiting);
      return stopped;
    });

    await useSessions.getState().updateSession(original.id, { action: "stop" });

    expect(useSessions.getState().sessions.existing).toEqual(stopped);
    expect(useSessions.getState().order).toEqual(["other", "existing"]);
  });

  it("does not let an older lifecycle response overwrite a newer event", async () => {
    const stopped = session("existing", {
      state: "stopped",
      last_activity: 20,
    });
    const resumed = {
      ...stopped,
      state: "active" as const,
      last_activity: 30,
    };
    useSessions.getState().loadSnapshot([stopped]);
    vi.spyOn(api, "updateSession").mockImplementation(async () => {
      useSessions.getState().upsert(resumed);
      return stopped;
    });

    await useSessions.getState().updateSession(stopped.id, { action: "stop" });

    expect(useSessions.getState().sessions.existing).toEqual(resumed);
  });

  it("does not apply an older full-session event", () => {
    const stopped = session("existing", {
      state: "stopped",
      last_activity: 30,
    });
    useSessions.getState().loadSnapshot([stopped]);

    useSessions.getState().upsert({
      ...stopped,
      state: "waiting",
      last_activity: 20,
    });

    expect(useSessions.getState().sessions.existing).toEqual(stopped);
  });
});
