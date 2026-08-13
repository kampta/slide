import { useEffect, useState } from "react";
import {
  api,
  type RuntimeDiagnostic,
  type RuntimeDiagnosticsSnapshot,
  type RuntimeStatus,
  type SshHost,
} from "../state/api";
import { useModalDialog } from "../hooks/useModalDialog";

export function runtimeTone(status: RuntimeStatus): "ok" | "warn" | "danger" {
  if (status === "ready") return "ok";
  if (status === "broken") return "danger";
  return "warn";
}

export function runtimeLabel(status: RuntimeStatus): string {
  switch (status) {
    case "ready":
      return "Ready";
    case "missing":
      return "Missing";
    case "unauthenticated":
      return "Sign-in required";
    case "broken":
      return "Probe failed";
  }
}

function backendLabel(backend: RuntimeDiagnostic["backend"]): string {
  switch (backend) {
    case "claude":
      return "Claude Code";
    case "codex":
      return "Codex";
    case "grok":
      return "Grok";
    case "agy":
      return "Antigravity";
    case "opencode":
      return "OpenCode";
  }
}

function checkedTime(timestamp: number): string {
  const date = new Date(timestamp);
  if (!Number.isFinite(timestamp) || Number.isNaN(date.getTime())) return "unknown";
  return date.toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" });
}

export function DiagnosticsModal({
  open,
  onClose,
}: {
  open: boolean;
  onClose: () => void;
}) {
  const [hosts, setHosts] = useState<SshHost[]>([]);
  const [target, setTarget] = useState("");
  const [snapshot, setSnapshot] = useState<RuntimeDiagnosticsSnapshot | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshNonce, setRefreshNonce] = useState(0);
  const dialogRef = useModalDialog<HTMLElement>(open, onClose);

  useEffect(() => {
    if (!open) return;
    api.listSshHosts().then(setHosts).catch(() => setHosts([]));
  }, [open]);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    setSnapshot(null);
    api
      .getRuntimeDiagnostics({
        host: target || undefined,
        refresh: refreshNonce > 0,
      })
      .then((next) => {
        if (!cancelled) setSnapshot(next);
      })
      .catch((reason) => {
        if (!cancelled) {
          setSnapshot(null);
          setError(reason instanceof Error ? reason.message : String(reason));
        }
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [open, refreshNonce, target]);

  useEffect(() => {
    if (!open) {
      setTarget("");
      setSnapshot(null);
      setError(null);
      setRefreshNonce(0);
    }
  }, [open]);

  if (!open) return null;

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <section
        ref={dialogRef}
        className="modal diagnostics-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="diagnostics-title"
        tabIndex={-1}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="diagnostics-heading">
          <div>
            <h2 id="diagnostics-title">Runtime diagnostics</h2>
            <p>CLI, authentication, and tmux readiness. Probe output and account identity stay private.</p>
          </div>
          <button type="button" className="btn-icon" onClick={onClose} aria-label="Close diagnostics">
            ×
          </button>
        </div>
        <label>
          <span>Target</span>
          <select
            value={target}
            onChange={(event) => {
              setRefreshNonce(0);
              setTarget(event.target.value);
            }}
          >
            <option value="">Local</option>
            {hosts.map((host) => (
              <option key={host.alias} value={host.alias}>
                {host.alias}
              </option>
            ))}
          </select>
        </label>
        {error && <p className="error diagnostics-error">{error}</p>}
        {loading && !snapshot ? (
          <p className="diagnostics-loading" role="status">Checking runtimes…</p>
        ) : snapshot ? (
          <>
            <div className={`diagnostic-card diagnostic-${snapshot.tmux.available ? "ok" : snapshot.tmux.required ? "danger" : "warn"}`}>
              <div className="diagnostic-card-title">
                <strong>tmux</strong>
                <span>{snapshot.tmux.available ? "Ready" : snapshot.tmux.required ? "Required" : "Optional"}</span>
                {snapshot.tmux.version && <code>{snapshot.tmux.version}</code>}
              </div>
              <p>{snapshot.tmux.message}</p>
              {snapshot.tmux.action && <p className="diagnostic-action">{snapshot.tmux.action}</p>}
            </div>
            <div className="diagnostic-grid">
              {snapshot.backends.map((diagnostic) => (
                <article
                  key={diagnostic.backend}
                  className={`diagnostic-card diagnostic-${runtimeTone(diagnostic.status)}`}
                >
                  <div className="diagnostic-card-title">
                    <strong>{backendLabel(diagnostic.backend)}</strong>
                    <span>{runtimeLabel(diagnostic.status)}</span>
                    {diagnostic.version && <code>{diagnostic.version}</code>}
                  </div>
                  <p>{diagnostic.message}</p>
                  {diagnostic.action && <p className="diagnostic-action">{diagnostic.action}</p>}
                  {diagnostic.last_error && <p className="diagnostic-last-error">{diagnostic.last_error}</p>}
                </article>
              ))}
            </div>
            <p className="diagnostics-checked">
              Checked {snapshot.target} at {checkedTime(snapshot.checked_at)}
            </p>
          </>
        ) : null}
        <div className="modal-actions">
          <button
            type="button"
            onClick={() => setRefreshNonce((value) => value + 1)}
            disabled={loading}
          >
            {loading ? "Checking…" : "Refresh"}
          </button>
          <button type="button" className="btn-primary" onClick={onClose}>Done</button>
        </div>
      </section>
    </div>
  );
}
