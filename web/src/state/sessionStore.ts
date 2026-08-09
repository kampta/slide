import { create } from "zustand";
import {
  api,
  openEventsSocket,
  setToken,
  STALE_TOKEN_MESSAGE,
  WS_CLOSE_AUTH_FAILED,
  type Session,
} from "./api";

interface Store {
  sessions: Record<string, Session>;
  order: string[];
  activeId: string | null;
  connected: boolean;
  /// Set when the daemon closes a WebSocket with code 4401 (stale or
  /// missing bearer token). Surfaced in the sidebar so the user sees the
  /// same instructions HTTP errors print, instead of an indefinite
  /// "disconnected" spinner that hides the real cause.
  authError: string | null;
  error: string | null;
  setActive: (id: string | null) => void;
  reportError: (error: unknown) => void;
  clearError: () => void;
  loadSnapshot: (list: Session[]) => void;
  upsert: (s: Session) => void;
  remove: (id: string) => void;
  refresh: () => Promise<void>;
  connect: () => () => void;
}

function sortOrder(a: Session, b: Session): number {
  // Stopped always sinks to the bottom; live sessions (active+waiting) interleave
  // by recency so a session bouncing between the two states keeps its position.
  const aStopped = a.state === "stopped" ? 1 : 0;
  const bStopped = b.state === "stopped" ? 1 : 0;
  if (aStopped !== bStopped) return aStopped - bStopped;
  return b.last_activity - a.last_activity;
}

/// Field-by-field equality so `upsert` can skip rebuilding `sessions` and
/// re-sorting `order` when an event delivers no observable change.
/// Cheap (15 primitive comparisons) and dodges every memoized child
/// re-render that would have followed a no-op store write.
function sessionsEqual(a: Session, b: Session): boolean {
  return (
    a.id === b.id &&
    a.name === b.name &&
    a.backend === b.backend &&
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
    a.log_offset === b.log_offset &&
    a.backend_session_id === b.backend_session_id
  );
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
    const sessions: Record<string, Session> = {};
    for (const s of list) sessions[s.id] = s;
    const order = [...list].sort(sortOrder).map((s) => s.id);
    const activeId = get().activeId;
    set({
      sessions,
      order,
      activeId: activeId && sessions[activeId] ? activeId : null,
    });
  },
  upsert: (s) => {
    const existing = get().sessions[s.id];
    if (existing && sessionsEqual(existing, s)) return;
    const sessions = { ...get().sessions, [s.id]: s };
    const order = Object.values(sessions).sort(sortOrder).map((x) => x.id);
    set({ sessions, order });
  },
  remove: (id) => {
    const sessions = { ...get().sessions };
    delete sessions[id];
    const order = get().order.filter((x) => x !== id);
    const activeId = get().activeId === id ? null : get().activeId;
    set({ sessions, order, activeId });
  },
  refresh: async () => {
    const list = await api.listSessions();
    get().loadSnapshot(list);
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
            {
              const s = get().sessions[msg.id];
              if (s) get().upsert({ ...s, state: msg.state, last_activity: Date.now() });
            }
            break;
          case "session_exit":
            {
              const s = get().sessions[msg.id];
              if (s) get().upsert({ ...s, state: "stopped" });
            }
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
