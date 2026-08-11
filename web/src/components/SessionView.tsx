import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { useSessions } from "../state/sessionStore";
import {
  api,
  type Backend,
  type BackendInfo,
  type ContextUsage,
} from "../state/api";
import type { TerminalHandle } from "./Terminal";
import { MobileKeyBar } from "./MobileKeyBar";
import { useIsMobile } from "../hooks/useMediaQuery";
import { SessionPath, sessionDisplayPath } from "./SessionPath";
import { SubagentDock } from "./SubagentDock";
import { TurnDiffDock } from "./TurnDiffDock";
import { SessionTransferModal } from "./SessionTransferModal";
import { ArtifactDock } from "./ArtifactDock";

const TerminalView = lazy(() =>
  import("./Terminal").then((module) => ({ default: module.TerminalView })),
);

/** Primary Resume/Start button label when a stopped session may switch backend. */
export function resumeActionLabel(current: Backend, selected: Backend): string {
  return current === selected ? "Resume" : "Start";
}

/** Tooltip explaining same-backend resume vs fresh start after a backend switch. */
export function resumeActionTitle(
  current: Backend,
  selected: Backend,
  hasBackendSessionId: boolean,
): string {
  if (current !== selected) {
    return `Start a fresh ${selected} conversation in this workspace. The prior ${current} conversation is not continued.`;
  }
  if (hasBackendSessionId) {
    return "Resume the prior conversation.";
  }
  return "Resume the most recent conversation for this workspace when supported; otherwise start fresh.";
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 10_000 ? 0 : 1)}k`;
  return `${n}`;
}

function ContextChip({ usage }: { usage: ContextUsage }) {
  const pct = usage.window > 0 ? (usage.used_tokens / usage.window) * 100 : 0;
  const level =
    pct >= 85 ? "danger" : pct >= 65 ? "warn" : "ok";
  const title =
    `${formatTokens(usage.used_tokens)} / ${formatTokens(usage.window)} tokens` +
    ` (${pct.toFixed(1)}%)\n` +
    `model: ${usage.model || "unknown"}\n` +
    `input: ${formatTokens(usage.input_tokens)} · ` +
    `cache read: ${formatTokens(usage.cache_read_input_tokens)} · ` +
    `cache create: ${formatTokens(usage.cache_creation_input_tokens)} · ` +
    `output: ${formatTokens(usage.output_tokens)}`;
  return (
    <span className={`ctx-chip ctx-${level}`} title={title}>
      <span className="ctx-bar">
        <span
          className="ctx-fill"
          style={{ width: `${Math.min(100, pct).toFixed(1)}%` }}
        />
      </span>
      <span className="ctx-label">{pct.toFixed(0)}%</span>
    </span>
  );
}

export function SessionView() {
  const activeId = useSessions((s) => s.activeId);
  const session = useSessions((s) => (activeId ? s.sessions[activeId] : null));
  const setActive = useSessions((s) => s.setActive);
  const reportError = useSessions((s) => s.reportError);
  const [usage, setUsage] = useState<ContextUsage | null>(null);
  const [pendingAction, setPendingAction] = useState<string | null>(null);
  const [transferOpen, setTransferOpen] = useState(false);
  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [resumeBackend, setResumeBackend] = useState<Backend | null>(null);
  const termRef = useRef<TerminalHandle>(null);
  const isMobile = useIsMobile();

  // Poll only after a backend-native session id exists. Backends that do not
  // expose context usage return null, so adding one needs no frontend branch.
  useEffect(() => {
    setUsage(null);
    if (!session?.backend_session_id) return;
    let cancelled = false;
    const tick = async () => {
      try {
        const u = await api.getContext(session.id);
        if (!cancelled) setUsage(u);
      } catch {
        if (!cancelled) setUsage(null);
      }
    };
    tick();
    const iv = window.setInterval(tick, 5000);
    return () => {
      cancelled = true;
      window.clearInterval(iv);
    };
  }, [session?.id, session?.backend_session_id]);

  // Load backends once for the stopped-session resume picker.
  useEffect(() => {
    let cancelled = false;
    api
      .listBackends()
      .then((items) => {
        if (!cancelled) setBackends(items);
      })
      .catch(() => {
        if (!cancelled) setBackends([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // Default the picker to the session's stored backend when the active
  // session changes (or when the store reports a switched backend after start).
  useEffect(() => {
    if (session) setResumeBackend(session.backend);
  }, [session?.id, session?.backend]);

  if (!session) {
    return (
      <div className="session-view empty">
        <div>
          <h1>slide</h1>
          <p>Pick a session on the left, or create a new one.</p>
        </div>
      </div>
    );
  }

  // Unknown means classification is uncertain, not that the process stopped.
  const isRunning = session.state !== "stopped";
  const selectedBackend = resumeBackend ?? session.backend;
  const resumeLabel = resumeActionLabel(session.backend, selectedBackend);
  const resumeTitle = resumeActionTitle(
    session.backend,
    selectedBackend,
    Boolean(session.backend_session_id),
  );
  const switchingBackend = selectedBackend !== session.backend;

  async function runAction(label: string, action: () => Promise<unknown>) {
    setPendingAction(label);
    try {
      await action();
    } catch (error) {
      reportError(error);
    } finally {
      setPendingAction(null);
    }
  }

  return (
    <section className="session-view">
      <header className="session-view-header">
        {isMobile && (
          <button
            type="button"
            className="hdr-back"
            onClick={() => setActive(null)}
            aria-label="Back to session list"
            title="Back"
          >
            ←
          </button>
        )}
        <div className="hdr-main">
          <h2 title={sessionDisplayPath(session)}>
            <SessionPath session={session} />
          </h2>
          <div className="hdr-sub">
            <span className={`dot dot-${session.state}`} />
            <span>{session.state}</span>
            <span className="sep">·</span>
            <span>{session.backend}</span>
            {session.parent_session_id && (
              <>
                <span className="sep">·</span>
                <span>fork</span>
              </>
            )}
            <span className="sep">·</span>
            <code title={session.project_path}>{session.project_path}</code>
            {usage && (
              <>
                <span className="sep">·</span>
                <ContextChip usage={usage} />
              </>
            )}
          </div>
        </div>
        <div className="hdr-actions">
          <button
            type="button"
            disabled={pendingAction !== null}
            onClick={() => setTransferOpen(true)}
            title="Fork this session or hand context to another waiting session"
          >
            Branch
          </button>
          {isRunning ? (
            <button
              disabled={pendingAction !== null}
              onClick={() =>
                runAction("Stopping…", () =>
                  api.updateSession(session.id, { action: "stop" }),
                )
              }
            >
              {pendingAction === "Stopping…" ? pendingAction : "Stop"}
            </button>
          ) : (
            <>
              {backends.length > 0 && (
                <label className="resume-backend" title="Backend to launch">
                  <span className="sr-only">Backend</span>
                  <select
                    value={selectedBackend}
                    disabled={pendingAction !== null}
                    onChange={(event) =>
                      setResumeBackend(event.target.value as Backend)
                    }
                    aria-label="Backend to launch on resume"
                  >
                    {backends.map((item) => (
                      <option key={item.id} value={item.id}>
                        {item.label}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              <button
                title={resumeTitle}
                disabled={pendingAction !== null}
                onClick={() =>
                  runAction("Starting…", () =>
                    api.updateSession(session.id, {
                      action: "resume",
                      ...(switchingBackend
                        ? { backend: selectedBackend }
                        : {}),
                    }),
                  )
                }
              >
                {pendingAction === "Starting…" ? pendingAction : resumeLabel}
              </button>
            </>
          )}
          <button
            className="danger"
            disabled={pendingAction !== null}
            onClick={() => {
              if (confirm(`Delete session "${session.name}"? This removes the worktree if slide created it.`)) {
                void runAction("Deleting…", () => api.deleteSession(session.id));
              }
            }}
          >
            {pendingAction === "Deleting…" ? pendingAction : "Delete"}
          </button>
        </div>
      </header>
      <SessionTransferModal
        open={transferOpen}
        source={session}
        onClose={() => setTransferOpen(false)}
        onSelect={setActive}
      />
      {session.backend_session_id && (
        <SubagentDock
          sessionId={session.id}
          rootThreadId={session.backend_session_id}
          live={isRunning}
        />
      )}
      <ArtifactDock session={session} />
      <TurnDiffDock key={session.id} sessionId={session.id} live={isRunning} />
      <Suspense fallback={<div className="term-host terminal-loading">Loading terminal…</div>}>
        <TerminalView
          ref={termRef}
          sessionId={session.id}
          live={isRunning}
          supervisor={session.supervisor}
        />
      </Suspense>
      {isMobile && isRunning && (
        <MobileKeyBar
          onSend={(b) => termRef.current?.sendBytes(b)}
        />
      )}
    </section>
  );
}
