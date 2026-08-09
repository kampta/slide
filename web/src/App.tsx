import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { SessionList } from "./components/SessionList";
import { SessionView } from "./components/SessionView";
import { NewSessionDialog } from "./components/NewSessionDialog";
import { useSessions } from "./state/sessionStore";
import { useHotkeys } from "./hooks/useHotkeys";
import { useIsMobile } from "./hooks/useMediaQuery";
import { api, setToken, type SessionState } from "./state/api";

const SIDEBAR_WIDTH_KEY = "slide.sidebarWidth";
const SIDEBAR_COLLAPSED_KEY = "slide.sidebarCollapsed";
const DEFAULT_SIDEBAR_WIDTH = 320;
const MIN_SIDEBAR_WIDTH = 240;
const MAX_SIDEBAR_WIDTH = 520;
// Drag the resizer below this width and it snaps to fully collapsed instead of
// fighting the min-width clamp.
const COLLAPSE_DRAG_THRESHOLD = 160;

function clampSidebarWidth(width: number): number {
  return Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, width));
}

function loadSidebarWidth(): number {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(SIDEBAR_WIDTH_KEY);
  } catch {}
  const raw = Number(stored);
  return Number.isFinite(raw) && raw > 0
    ? clampSidebarWidth(raw)
    : DEFAULT_SIDEBAR_WIDTH;
}

function loadSidebarCollapsed(): boolean {
  try {
    return localStorage.getItem(SIDEBAR_COLLAPSED_KEY) === "1";
  } catch {
    return false;
  }
}

export function App() {
  const [newOpen, setNewOpen] = useState(false);
  const [sidebarWidth, setSidebarWidth] = useState(loadSidebarWidth);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(loadSidebarCollapsed);
  const connect = useSessions((s) => s.connect);
  const refresh = useSessions((s) => s.refresh);
  const setActive = useSessions((s) => s.setActive);
  const reportError = useSessions((s) => s.reportError);
  // Subscribe to activeId so the mobile single-pane render swaps when the
  // user picks (or backs out of) a session. Desktop layout doesn't need
  // this subscription but the cost is one selector.
  const activeId = useSessions((s) => s.activeId);
  const resizeCleanupRef = useRef<(() => void) | null>(null);
  const isMobile = useIsMobile();

  useEffect(() => {
    // Capture the token *before* stripping the URL: getToken() falls back to
    // localStorage, which would be empty on a fresh device if we stripped
    // first. Stripping after capture keeps the address bar clean across
    // refreshes without losing auth on a new device's first load.
    const url = new URL(window.location.href);
    const fromUrl = url.searchParams.get("token");
    if (fromUrl) {
      setToken(fromUrl);
      url.searchParams.delete("token");
      window.history.replaceState({}, "", url.toString());
    }
    refresh().catch(reportError);
    const stop = connect();
    return stop;
  }, [connect, refresh, reportError]);

  useEffect(() => {
    try {
      localStorage.setItem(SIDEBAR_WIDTH_KEY, String(sidebarWidth));
    } catch {}
  }, [sidebarWidth]);

  useEffect(() => {
    try {
      localStorage.setItem(SIDEBAR_COLLAPSED_KEY, sidebarCollapsed ? "1" : "0");
    } catch {}
  }, [sidebarCollapsed]);

  const collapseSidebar = useCallback(() => setSidebarCollapsed(true), []);
  const expandSidebar = useCallback(() => setSidebarCollapsed(false), []);

  useEffect(() => () => resizeCleanupRef.current?.(), []);

  const cycleState = useCallback(
    (state: SessionState | "any") => {
      const { sessions, order, activeId } = useSessions.getState();
      const filtered = order
        .map((id) => sessions[id])
        .filter((s) => s && (state === "any" || s.state === state));
      if (filtered.length === 0) return;
      const idx = filtered.findIndex((s) => s.id === activeId);
      const next = filtered[(idx + 1) % filtered.length];
      setActive(next.id);
    },
    [setActive],
  );

  const cyclePrev = useCallback(() => {
    const { sessions, order, activeId } = useSessions.getState();
    const list = order.map((id) => sessions[id]).filter(Boolean);
    if (list.length === 0) return;
    const idx = list.findIndex((s) => s!.id === activeId);
    const prev = list[(idx - 1 + list.length) % list.length]!;
    setActive(prev.id);
  }, [setActive]);

  const stopResizing = useCallback(() => {
    resizeCleanupRef.current?.();
  }, []);

  const startResizing = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      event.preventDefault();
      stopResizing();
      const startX = event.clientX;
      const startWidth = sidebarWidth;

      document.body.classList.add("is-resizing-panels");

      const handleMove = (moveEvent: PointerEvent) => {
        const raw = startWidth + moveEvent.clientX - startX;
        if (raw < COLLAPSE_DRAG_THRESHOLD) {
          setSidebarCollapsed(true);
        } else {
          setSidebarCollapsed(false);
          setSidebarWidth(clampSidebarWidth(raw));
        }
      };

      const cleanup = () => {
        window.removeEventListener("pointermove", handleMove);
        window.removeEventListener("pointerup", cleanup);
        window.removeEventListener("pointercancel", cleanup);
        document.body.classList.remove("is-resizing-panels");
        resizeCleanupRef.current = null;
      };

      resizeCleanupRef.current = cleanup;
      window.addEventListener("pointermove", handleMove);
      window.addEventListener("pointerup", cleanup);
      window.addEventListener("pointercancel", cleanup);
    },
    [sidebarWidth, stopResizing],
  );

  const handleResizeKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLDivElement>) => {
      if (event.key === "ArrowLeft") {
        event.preventDefault();
        setSidebarWidth((width) => clampSidebarWidth(width - 24));
      } else if (event.key === "ArrowRight") {
        event.preventDefault();
        setSidebarWidth((width) => clampSidebarWidth(width + 24));
      } else if (event.key === "Home") {
        event.preventDefault();
        setSidebarWidth(MIN_SIDEBAR_WIDTH);
      } else if (event.key === "End") {
        event.preventDefault();
        setSidebarWidth(MAX_SIDEBAR_WIDTH);
      }
    },
    [],
  );

  useHotkeys({
    "alt+n": () => setNewOpen(true),
    "alt+j": () => cycleState("any"),
    "alt+k": cyclePrev,
    "alt+shift+w": () => cycleState("waiting"),
    "alt+shift+a": () => cycleState("active"),
    "alt+shift+x": async () => {
      const { activeId, sessions } = useSessions.getState();
      if (!activeId) return;
      const s = sessions[activeId];
      if (!s) return;
      const running = s.state === "active" || s.state === "waiting";
      try {
        await api.updateSession(activeId, {
          action: running ? "stop" : "resume",
        });
      } catch (error) {
        reportError(error);
      }
    },
    escape: () => setNewOpen(false),
  });

  // Mobile: single pane. List when no session is focused; SessionView
  // (with its own back button) when one is. Skip the resizer entirely —
  // a thin draggable divider makes no sense on touch and would steal
  // 10px of horizontal space.
  if (isMobile) {
    return (
      <div className="app app-mobile">
        {activeId ? (
          <main className="main">
            <SessionView />
          </main>
        ) : (
          <SessionList onNew={() => setNewOpen(true)} />
        )}
        <NewSessionDialog
          open={newOpen}
          onClose={() => setNewOpen(false)}
          onCreated={(id) => setActive(id)}
        />
      </div>
    );
  }

  return (
    <div
      className={`app${sidebarCollapsed ? " sidebar-collapsed" : ""}`}
      style={{
        gridTemplateColumns: sidebarCollapsed
          ? "minmax(0, 1fr)"
          : `${sidebarWidth}px 10px minmax(0, 1fr)`,
      }}
    >
      {!sidebarCollapsed && (
        <SessionList
          onNew={() => setNewOpen(true)}
          onCollapse={collapseSidebar}
        />
      )}
      {!sidebarCollapsed && (
        <div
          className="panel-resizer"
          role="separator"
          tabIndex={0}
          aria-label="Resize session list"
          aria-orientation="vertical"
          aria-valuemin={MIN_SIDEBAR_WIDTH}
          aria-valuemax={MAX_SIDEBAR_WIDTH}
          aria-valuenow={sidebarWidth}
          onPointerDown={startResizing}
          onDoubleClick={() => setSidebarWidth(DEFAULT_SIDEBAR_WIDTH)}
          onKeyDown={handleResizeKeyDown}
        />
      )}
      <main className="main">
        {sidebarCollapsed && (
          <button
            className="sidebar-expand-btn"
            onClick={expandSidebar}
            aria-label="Show session list"
            title="Show session list"
          >
            ›
          </button>
        )}
        <SessionView />
      </main>
      <NewSessionDialog
        open={newOpen}
        onClose={() => setNewOpen(false)}
        onCreated={(id) => setActive(id)}
      />
    </div>
  );
}
