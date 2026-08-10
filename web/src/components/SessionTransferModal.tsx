import { FormEvent, useEffect, useMemo, useState } from "react";
import { api, type BackendInfo, type Session } from "../state/api";
import { useSessions } from "../state/sessionStore";

export function suggestedForkName(source: string, existing: Set<string>): string {
  const stem = `${source}-fork`;
  if (!existing.has(stem)) return stem;
  let suffix = 2;
  while (existing.has(`${stem}-${suffix}`)) suffix += 1;
  return `${stem}-${suffix}`;
}

export function SessionTransferModal({
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
  const sessions = useSessions((state) => state.sessions);
  const [mode, setMode] = useState<"fork" | "handoff">("fork");
  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [name, setName] = useState("");
  const [targetId, setTargetId] = useState("");
  const [focus, setFocus] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const targets = useMemo(
    () =>
      Object.values(sessions)
        .filter((session) => session.id !== source.id && session.state === "waiting")
        .sort((left, right) => right.last_activity - left.last_activity),
    [sessions, source.id],
  );
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
    const existing = new Set(Object.values(sessions).map((session) => session.name));
    setMode("fork");
    setName(suggestedForkName(source.name, existing));
    setTargetId(targets[0]?.id ?? "");
    setFocus("");
    setError(null);
  }, [open, source.id]);

  useEffect(() => {
    if (!open || targets.some((target) => target.id === targetId)) return;
    setTargetId(targets[0]?.id ?? "");
  }, [open, targetId, targets]);

  useEffect(() => {
    if (open && !canFork && backends.length > 0) setMode("handoff");
  }, [backends.length, canFork, open]);

  if (!open) return null;

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (loading) return;
    setLoading(true);
    setError(null);
    try {
      const target = mode === "fork"
        ? await api.forkSession(source.id, { name, focus: focus.trim() || undefined })
        : await api.handoffSession(source.id, {
            target_session_id: targetId,
            focus: focus.trim(),
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
        className="modal session-transfer-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="session-transfer-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="diagnostics-heading">
          <div>
            <h2 id="session-transfer-title">Branch or hand off</h2>
            <p>Source: {source.name}</p>
          </div>
          <button
            type="button"
            className="btn-icon"
            onClick={onClose}
            aria-label="Close transfer"
            disabled={loading}
          >
            ×
          </button>
        </div>
        <div className="btn-group session-transfer-modes" role="tablist" aria-label="Transfer type">
          <button
            type="button"
            role="tab"
            aria-selected={mode === "fork"}
            className={mode === "fork" ? "active" : ""}
            onClick={() => setMode("fork")}
          >
            Fork session
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === "handoff"}
            className={mode === "handoff" ? "active" : ""}
            onClick={() => setMode("handoff")}
          >
            Hand off context
          </button>
        </div>
        <form className="session-transfer-form" onSubmit={submit}>
          {mode === "fork" ? (
            <>
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
                  Native forks require a local Claude or Codex session with a discovered conversation ID.
                </p>
              )}
              <p className="hint">
                Creates a new provider conversation and an isolated Slide worktree from the source's current Git-visible files. The source stays unchanged.
              </p>
            </>
          ) : (
            <>
              <label>
                <span>Waiting target</span>
                <select value={targetId} onChange={(event) => setTargetId(event.target.value)} required>
                  {targets.length === 0 && <option value="">No waiting sessions</option>}
                  {targets.map((target) => (
                    <option key={target.id} value={target.id}>
                      {target.name} · {target.backend}
                    </option>
                  ))}
                </select>
              </label>
              <p className="hint">
                Sends a bounded, control-sequence-free tail of this session to the target as one focused turn.
              </p>
            </>
          )}
          <label>
            <span>{mode === "fork" ? "New direction (optional)" : "Focus for target"}</span>
            <textarea
              value={focus}
              onChange={(event) => setFocus(event.target.value)}
              maxLength={2000}
              required={mode === "handoff"}
              placeholder={mode === "fork" ? "Try a different implementation…" : "What should the target carry forward?"}
            />
          </label>
          {error && <p className="error session-transfer-error">{error}</p>}
          <div className="modal-actions">
            <button type="button" onClick={onClose} disabled={loading}>Cancel</button>
            <button
              type="submit"
              className="btn-primary"
              disabled={
                loading ||
                (mode === "fork" ? !canFork || !name : !targetId || !focus.trim())
              }
            >
              {loading ? "Working…" : mode === "fork" ? "Create fork" : "Send handoff"}
            </button>
          </div>
        </form>
      </section>
    </div>
  );
}
