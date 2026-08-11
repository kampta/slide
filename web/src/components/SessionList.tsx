import { useMemo, useState } from "react";
import { useSessions } from "../state/sessionStore";
import { SessionItem } from "./SessionItem";
import type { Session } from "../state/api";

const STOPPED_COLLAPSED_KEY = "slide.stoppedCollapsed";

function loadStoppedCollapsed(): boolean {
  const raw = localStorage.getItem(STOPPED_COLLAPSED_KEY);
  return raw === null ? true : raw === "1";
}

export function SessionList({
  onNew,
  onSearch,
  onDiagnostics,
  onCollapse,
}: {
  onNew: () => void;
  onSearch: () => void;
  onDiagnostics: () => void;
  // Optional: when present, header shows a collapse chevron. Omitted on
  // mobile where the sidebar IS the whole screen and there's nothing to
  // collapse to.
  onCollapse?: () => void;
}) {
  const sessions = useSessions((s) => s.sessions);
  const order = useSessions((s) => s.order);
  const activeId = useSessions((s) => s.activeId);
  const setActive = useSessions((s) => s.setActive);
  const connected = useSessions((s) => s.connected);
  const authError = useSessions((s) => s.authError);
  const error = useSessions((s) => s.error);
  const clearError = useSessions((s) => s.clearError);
  const [stoppedCollapsed, setStoppedCollapsed] = useState(loadStoppedCollapsed);

  const { live, stopped } = useMemo(() => {
    const live: Session[] = [];
    const stopped: Session[] = [];
    for (const id of order) {
      const s = sessions[id];
      if (!s) continue;
      if (s.state === "stopped") stopped.push(s);
      else live.push(s);
    }
    return { live, stopped };
  }, [sessions, order]);

  function toggleStopped() {
    setStoppedCollapsed((prev) => {
      const next = !prev;
      localStorage.setItem(STOPPED_COLLAPSED_KEY, next ? "1" : "0");
      return next;
    });
  }

  return (
    <aside className="session-list">
      {!connected && (
        <div
          className="disconnect-banner"
          role="status"
          aria-live="polite"
        >
          Disconnected — retrying…
        </div>
      )}
      <header className="session-list-header">
        <div className="brand">
          <svg
            className={`brand-mark ${connected ? "live" : "idle"}`}
            viewBox="0 0 24 24"
            width="22"
            height="22"
            fill="none"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden="true"
          >
            <path className="brand-mark-stroke" d="M3 19 C 8 19, 10 14, 12 11 S 17 5, 21 5" />
            <circle className="brand-mark-node" cx="20" cy="5" r="2.2" />
          </svg>
          <span className="brand-name">slide</span>
        </div>
        <div className="session-list-actions">
          <div className="session-action-group">
            <button
              type="button"
              className="session-search-btn"
              onClick={onSearch}
              aria-label="Search session history"
              title="Search session history"
            >
              <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <circle cx="11" cy="11" r="6" />
                <path d="m16 16 4 4" />
              </svg>
            </button>
            <button className="session-new-btn" onClick={onNew}>
              <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <path d="M12 5v14M5 12h14" />
              </svg>
              <span>New</span>
            </button>
          </div>
          {onCollapse && (
            <button
              className="btn-icon"
              onClick={onCollapse}
              aria-label="Hide session list"
              title="Hide session list"
            >
              <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
                <path d="M15 18l-6-6 6-6" />
              </svg>
            </button>
          )}
        </div>
      </header>
      {(authError || error) && (
        <div className="error-banner" role="alert">
          <span>{authError || error}</span>
          {error && !authError && (
            <button type="button" onClick={clearError} aria-label="Dismiss error">
              ×
            </button>
          )}
        </div>
      )}
      <div className="session-list-scroll">
        {live.length > 0 && (
          <section className="group group-live">
            {live.map((s) => (
              <SessionItem
                key={s.id}
                session={s}
                active={s.id === activeId}
                onClick={() => setActive(s.id)}
              />
            ))}
          </section>
        )}
        {stopped.length > 0 && (
          <section className="group group-stopped group-bottom">
            <button
              type="button"
              className="group-header"
              aria-expanded={!stoppedCollapsed}
              onClick={toggleStopped}
            >
              <span className={`chevron ${stoppedCollapsed ? "collapsed" : ""}`}>▸</span>
              Stopped <span className="count">{stopped.length}</span>
            </button>
            {!stoppedCollapsed &&
              stopped.map((s) => (
                <SessionItem
                  key={s.id}
                  session={s}
                  active={s.id === activeId}
                  onClick={() => setActive(s.id)}
                />
              ))}
          </section>
        )}
        {order.length === 0 && (
          <div className="empty">
            No sessions yet.<br />
            <span className="desktop-only">
              Select New session to create one.
            </span>
            <span className="mobile-only">Tap + to create one.</span>
          </div>
        )}
      </div>
      <footer className="session-list-footer">
        <button type="button" onClick={onDiagnostics}>
          <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
            <path d="M3 12h4l2.5-6 5 12 2.5-6h4" />
          </svg>
          <span>Runtime diagnostics</span>
        </button>
      </footer>
      {/* FAB is always rendered but only visible on mobile (via CSS); keeps
          markup simple and avoids subscribing this component to a media
          query just to flip a single button. */}
      <button
        type="button"
        className="session-list-fab"
        onClick={onNew}
        aria-label="New session"
        title="New session"
      >
        +
      </button>
    </aside>
  );
}
