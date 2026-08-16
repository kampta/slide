import { create } from "zustand";
import {
  api,
  openEventsSocket,
  setToken,
  STALE_TOKEN_MESSAGE,
  WS_CLOSE_AUTH_FAILED,
  type Session,
  type SessionState,
} from "./api";

interface Store {
  sessions: Record<string, Session>;
  order: string[];
  activeId: string | null;
  connected: boolean;
  /// Set when the daemon closes a WebSocket with code 4401 (stale or
  /// missing bearer token). Surfaced by the shared status banner so the user
  /// sees the same instructions HTTP errors print, instead of an indefinite
  /// "disconnected" spinner that hides the real cause.
  authError: string | null;
  error: string | null;
  setActive: (id: string | null) => void;
  reportError: (error: unknown) => void;
  clearError: () => void;
  loadSnapshot: (list: Session[]) => void;
  upsert: (s: Session) => void;
  setSessionState: (
    id: string,
    state: SessionState,
    lastActivity?: number,
  ) => void;
  remove: (id: string) => void;
  createSession: (
    request: Parameters<typeof api.createSession>[0],
  ) => Promise<Session>;
  updateSession: (
    id: string,
    patch: Parameters<typeof api.updateSession>[1],
  ) => Promise<Session>;
  deleteSession: (id: string) => Promise<void>;
  forkSession: (
    id: string,
    request: Parameters<typeof api.forkSession>[1],
  ) => Promise<Session>;
  connect: () => () => void;
}

function sortOrder(a: Session, b: Session): number {
  // Keep running sessions above stopped ones, then use immutable creation
  // time so activity and classifier updates cannot shuffle either group.
  const stopped = Number(a.state === "stopped") - Number(b.state === "stopped");
  return stopped || b.created_at - a.created_at || a.id.localeCompare(b.id);
}

/// Field-by-field equality so `upsert` can skip rebuilding `sessions` and
/// re-sorting `order` when an event delivers no observable change.
/// Cheap primitive comparisons that dodge every memoized child
/// re-render that would have followed a no-op store write.
function sessionsEqual(a: Session, b: Session): boolean {
  return (
    a.id === b.id &&
    a.name === b.name &&
    a.backend === b.backend &&
    a.execution_policy === b.execution_policy &&
    a.location === b.location &&
    a.ssh_host === b.ssh_host &&
    a.base_dir === b.base_dir &&
    a.project_path === b.project_path &&
    a.worktree === b.worktree &&
    a.state === b.state &&
    a.created_at === b.created_at &&
    a.last_activity === b.last_activity &&
    a.supervisor === b.supervisor &&
    a.host_log_path === b.host_log_path &&
    a.backend_session_id === b.backend_session_id &&
    a.parent_session_id === b.parent_session_id
  );
}

function shouldReplaceSession(current: Session, incoming: Session): boolean {
  if (incoming.last_activity !== current.last_activity) {
    return incoming.last_activity > current.last_activity;
  }
  // Stopped is terminal for one lifecycle. Resume always advances the
  // timestamp, while live classifier states may legitimately share one.
  return current.state !== "stopped" || incoming.state === "stopped";
}

function upsertIfUnchanged(
  get: () => Store,
  previous: Session | undefined,
  response: Session,
): void {
  if (get().sessions[response.id] === previous) get().upsert(response);
}

function upsertLifecycleResponse(
  get: () => Store,
  previous: Session | undefined,
  response: Session,
): void {
  // Stop/resume timestamps are monotonic. They let the HTTP result beat an
  // earlier classifier event without overwriting a genuinely newer lifecycle.
  const current = get().sessions[response.id];
  if (
    current === previous ||
    (current && response.last_activity > current.last_activity)
  ) {
    get().upsert(response);
  }
}

export const useSessions = create<Store>((set, get) => ({
  sessions: {},
  order: [],
  activeId: null,
  connected: false,
  authError: null,
  error: null,
  setActive: (id) => set({ activeId: id }),
  reportError: (error) =>
    set({ error: error instanceof Error ? error.message : String(error) }),
  clearError: () => set({ error: null }),
  loadSnapshot: (list) => {
    const current = get().sessions;
    const sessions: Record<string, Session> = {};
    for (const session of list) {
      const existing = current[session.id];
      sessions[session.id] =
        existing && !shouldReplaceSession(existing, session) ? existing : session;
    }
    const order = Object.values(sessions).sort(sortOrder).map((s) => s.id);
    const activeId = get().activeId;
    set({
      sessions,
      order,
      activeId: activeId && sessions[activeId] ? activeId : null,
    });
  },
  upsert: (s) => {
    const existing = get().sessions[s.id];
    if (
      existing &&
      (sessionsEqual(existing, s) || !shouldReplaceSession(existing, s))
    ) {
      return;
    }
    const sessions = { ...get().sessions, [s.id]: s };
    const order = Object.values(sessions).sort(sortOrder).map((x) => x.id);
    set({ sessions, order });
  },
  setSessionState: (id, state, lastActivity) => {
    const session = get().sessions[id];
    if (!session) return;
    get().upsert({
      ...session,
      state,
      last_activity: lastActivity ?? session.last_activity,
    });
  },
  remove: (id) => {
    const sessions = { ...get().sessions };
    delete sessions[id];
    const order = get().order.filter((x) => x !== id);
    const activeId = get().activeId === id ? null : get().activeId;
    set({ sessions, order, activeId });
  },
  createSession: async (request) => {
    const session = await api.createSession(request);
    upsertIfUnchanged(get, undefined, session);
    return session;
  },
  updateSession: async (id, patch) => {
    const previous = get().sessions[id];
    const session = await api.updateSession(id, patch);
    if (patch.action) {
      upsertLifecycleResponse(get, previous, session);
    } else {
      upsertIfUnchanged(get, previous, session);
    }
    return session;
  },
  deleteSession: async (id) => {
    await api.deleteSession(id);
    get().remove(id);
  },
  forkSession: async (id, request) => {
    const session = await api.forkSession(id, request);
    upsertIfUnchanged(get, undefined, session);
    return session;
  },
  connect: () => {
    let ws: WebSocket | null = null;
    let retryTimer: number | null = null;
    let stopped = false;
    let retry = 500;

    const open = () => {
      if (stopped) return;
      ws = openEventsSocket();
      ws.onopen = () => {
        retry = 500;
        set({ connected: true, authError: null });
      };
      ws.onclose = (event) => {
        set({ connected: false });
        ws = null;
        // 4401 = daemon rejected the bearer token (rotated after a daemon
        // restart, or the cached token is stale). Stop the reconnect loop
        // and surface the same instruction HTTP 401s show, otherwise the
        // UI just spins forever with the same dead token.
        if (event.code === WS_CLOSE_AUTH_FAILED) {
          setToken("");
          stopped = true;
          set({ authError: STALE_TOKEN_MESSAGE });
          return;
        }
        if (!stopped) retryTimer = window.setTimeout(open, retry);
        retry = Math.min(retry * 2, 5000);
      };
      ws.onerror = () => ws?.close();
      ws.onmessage = (e) => {
        if (typeof e.data !== "string") return;
        let msg: Record<string, any>;
        try {
          msg = JSON.parse(e.data);
        } catch {
          return;
        }
        switch (msg.type) {
          case "snapshot":
            get().loadSnapshot(msg.sessions);
            break;
          case "session_added":
          case "session_updated":
            get().upsert(msg.session);
            break;
          case "session_removed":
            get().remove(msg.id);
            break;
          case "session_state":
            get().setSessionState(msg.id, msg.state, msg.last_activity);
            break;
        }
      };
    };

    // iOS aggressively suspends backgrounded WebSockets and the close
    // event isn't always fired when the tab returns. Force a reconnect on
    // visibilitychange → visible: closing here triggers our existing
    // ws.onclose ladder, which cleanly re-opens. If the socket really was
    // alive, the next snapshot is just a (cheap) re-sync.
    const onVisibility = () => {
      if (document.visibilityState !== "visible" || stopped) return;
      ws?.close();
    };
    document.addEventListener("visibilitychange", onVisibility);

    open();
    return () => {
      stopped = true;
      if (retryTimer !== null) window.clearTimeout(retryTimer);
      document.removeEventListener("visibilitychange", onVisibility);
      ws?.close();
    };
  },
}));
