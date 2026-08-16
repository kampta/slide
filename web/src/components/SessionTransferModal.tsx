import { FormEvent, useEffect, useState } from "react";
import { api, type BackendInfo, type Session } from "../state/api";
import { useSessions } from "../state/sessionStore";
import { useModalDialog } from "../hooks/useModalDialog";

export function suggestedForkName(source: string, existing: Set<string>): string {
  const stem = `${source}-fork`;
  if (!existing.has(stem)) return stem;
  let suffix = 2;
  while (existing.has(`${stem}-${suffix}`)) suffix += 1;
  return `${stem}-${suffix}`;
}

export function SessionForkModal({
  open,
  source,
  onClose,
  onSelect,
}: {
  open: boolean;
  source: Session;
  onClose: () => void;
  onSelect: (sessionId: string) => void;
}) {
  const forkSession = useSessions((state) => state.forkSession);
  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [name, setName] = useState("");
  const [focus, setFocus] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const dialogRef = useModalDialog<HTMLElement>(open, onClose, !loading);

  const backendCanFork = backends.find((backend) => backend.id === source.backend)?.fork ?? false;
  const canFork =
    source.location === "local" && Boolean(source.backend_session_id) && backendCanFork;

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    api.listBackends().then((items) => {
      if (!cancelled) setBackends(items);
    }).catch(() => {
      if (!cancelled) setBackends([]);
    });
    return () => {
      cancelled = true;
    };
  }, [open]);

  useEffect(() => {
    if (!open) {
      setLoading(false);
      setError(null);
      return;
    }
    const existing = new Set(
      Object.values(useSessions.getState().sessions).map((session) => session.name),
    );
    setName(suggestedForkName(source.name, existing));
    setFocus("");
    setError(null);
  }, [open, source.id, source.name]);

  if (!open) return null;

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (loading) return;
    setLoading(true);
    setError(null);
    try {
      const target = await forkSession(source.id, {
        name,
        focus: focus.trim() || undefined,
      });
      onSelect(target.id);
      onClose();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="modal-backdrop" onMouseDown={() => !loading && onClose()}>
      <section
        ref={dialogRef}
        className="modal session-fork-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="session-fork-title"
        tabIndex={-1}
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="diagnostics-heading">
          <div>
          <h2 id="session-fork-title">Fork session</h2>
            <p>Source: {source.name}</p>
          </div>
          <button
            type="button"
            className="btn-icon"
            onClick={onClose}
            aria-label="Close fork dialog"
            disabled={loading}
          >
            ×
          </button>
        </div>
        <form className="session-fork-form" onSubmit={submit}>
          <label>
            <span>New session name</span>
            <input
              autoFocus
              value={name}
              onChange={(event) => setName(event.target.value)}
              pattern="[A-Za-z0-9_][A-Za-z0-9_-]*"
              required
            />
          </label>
          {!canFork && (
            <p className="hint hint-error">
              Forks require a local Claude, Codex, Grok, or Agy session with a discovered conversation ID.
            </p>
          )}
          <p className="hint">
            Creates a new isolated worktree from the source's current Git-visible files and continues its provider conversation. The source stays unchanged.
          </p>
          {source.backend === "agy" ? (
            <p className="hint">
              Agy forks the existing conversation through its CLI. Its command does not accept an initial direction here; enter it in the new session.
            </p>
          ) : (
            <label>
              <span>New direction (optional)</span>
              <textarea
                value={focus}
                onChange={(event) => setFocus(event.target.value)}
                maxLength={2000}
                placeholder="Try a different implementation…"
              />
            </label>
          )}
          {error && <p className="error session-fork-error">{error}</p>}
          <div className="modal-actions">
            <button type="button" onClick={onClose} disabled={loading}>Cancel</button>
            <button
              type="submit"
              className="btn-primary"
              disabled={
                loading ||
                !canFork || !name
              }
            >
              {loading ? "Working…" : "Create fork"}
            </button>
          </div>
        </form>
      </section>
    </div>
  );
}
