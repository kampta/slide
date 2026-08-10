import { useEffect, useMemo, useState } from "react";
import { api, type TurnDiff, type TurnDiffSummary } from "../state/api";

const SUCCESS_POLL_MS = 5_000;
const ERROR_POLL_MS = 30_000;

export function sortTurnDiffs(turns: TurnDiffSummary[]): TurnDiffSummary[] {
  return [...turns].sort(
    (a, b) => b.turn - a.turn || b.completed_at - a.completed_at,
  );
}

export function turnStats(turn: TurnDiffSummary): string {
  const files = `${turn.files_changed} ${turn.files_changed === 1 ? "file" : "files"}`;
  return `${files} · +${turn.additions} −${turn.deletions}`;
}

export function availableTurn(
  current: number | null,
  turns: TurnDiffSummary[],
): number | null {
  if (current !== null && turns.some((turn) => turn.id === current)) {
    return current;
  }
  return turns[0]?.id ?? null;
}

function formatTime(timestamp: number): string {
  const date = new Date(timestamp);
  if (!Number.isFinite(timestamp) || Number.isNaN(date.getTime())) return "—";
  return date.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

function dateTimeValue(timestamp: number): string | undefined {
  const date = new Date(timestamp);
  return Number.isFinite(timestamp) && !Number.isNaN(date.getTime())
    ? date.toISOString()
    : undefined;
}

function duration(turn: TurnDiffSummary): string {
  const elapsed = Math.max(0, turn.completed_at - turn.started_at);
  if (elapsed < 1_000) return "<1s";
  const seconds = Math.round(elapsed / 1_000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${seconds % 60}s`;
}

export function TurnDiffDock({
  sessionId,
  live,
}: {
  sessionId: string;
  live: boolean;
}) {
  const [turns, setTurns] = useState<TurnDiffSummary[]>([]);
  const [expanded, setExpanded] = useState(false);
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [detail, setDetail] = useState<TurnDiff | null>(null);
  const [loadingDetail, setLoadingDetail] = useState(false);
  const [detailError, setDetailError] = useState(false);

  useEffect(() => {
    let cancelled = false;
    let timer: number | null = null;
    let stoppedRefreshes = live ? 0 : 2;
    setTurns([]);
    setExpanded(false);
    setSelectedId(null);
    setDetail(null);
    setDetailError(false);

    const tick = async () => {
      let nextDelay = SUCCESS_POLL_MS;
      try {
        const next = sortTurnDiffs(await api.listTurnDiffs(sessionId));
        if (cancelled) return;
        setTurns(next);
        setSelectedId((current) => availableTurn(current, next));
      } catch {
        nextDelay = ERROR_POLL_MS;
      }
      if (cancelled) return;
      if (live) {
        timer = window.setTimeout(tick, nextDelay);
      } else if (stoppedRefreshes > 0) {
        // The final Git/SSH capture is queued behind the Stopped event.
        // Two bounded follow-ups catch it without leaving stopped sessions
        // on a permanent polling loop.
        const delay = stoppedRefreshes === 2 ? 2_000 : 8_000;
        stoppedRefreshes -= 1;
        timer = window.setTimeout(tick, delay);
      }
    };

    void tick();
    return () => {
      cancelled = true;
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [sessionId, live]);

  useEffect(() => {
    if (!expanded || selectedId === null) {
      setDetail(null);
      setLoadingDetail(false);
      setDetailError(false);
      return;
    }
    let cancelled = false;
    setDetail(null);
    setLoadingDetail(true);
    setDetailError(false);
    api
      .getTurnDiff(sessionId, selectedId)
      .then((next) => {
        if (!cancelled) setDetail(next);
      })
      .catch(() => {
        if (!cancelled) {
          setDetail(null);
          setDetailError(true);
        }
      })
      .finally(() => {
        if (!cancelled) setLoadingDetail(false);
      });
    return () => {
      cancelled = true;
    };
  }, [expanded, selectedId, sessionId]);

  const selected = useMemo(
    () => turns.find((turn) => turn.id === selectedId) ?? null,
    [selectedId, turns],
  );

  if (turns.length === 0) return null;
  const latest = turns[0];

  return (
    <section className="turn-diff-dock" aria-label="Changes by turn">
      <button
        type="button"
        className="turn-diff-header"
        aria-expanded={expanded}
        onClick={() => setExpanded((value) => !value)}
      >
        <span aria-hidden="true">{expanded ? "▾" : "▸"}</span>
        <span>Changes</span>
        <span className="turn-diff-count">
          {turns.length} {turns.length === 1 ? "turn" : "turns"}
        </span>
        <span className="turn-diff-latest">{turnStats(latest)}</span>
      </button>
      {expanded && (
        <div className="turn-diff-body">
          <nav className="turn-diff-turns" aria-label="Changed turns">
            {turns.map((turn) => (
              <button
                type="button"
                key={turn.id}
                className={turn.id === selectedId ? "selected" : ""}
                aria-current={turn.id === selectedId ? "true" : undefined}
                onClick={() => setSelectedId(turn.id)}
                title={`Turn ${turn.turn} · ${turnStats(turn)} · ${duration(turn)}`}
              >
                <span>
                  <strong>Turn {turn.turn}</strong>
                  <time dateTime={dateTimeValue(turn.completed_at)}>
                    {formatTime(turn.completed_at)}
                  </time>
                </span>
                <small>{turnStats(turn)}</small>
              </button>
            ))}
          </nav>
          <div className="turn-diff-detail">
            {selected && (
              <div className="turn-diff-detail-header">
                <strong>Turn {selected.turn}</strong>
                <span>{turnStats(selected)}</span>
                <span>{duration(selected)}</span>
                {selected.truncated && <span className="turn-diff-truncated">truncated</span>}
              </div>
            )}
            {loadingDetail ? (
              <p className="turn-diff-empty">Loading diff…</p>
            ) : detailError ? (
              <p className="turn-diff-empty">Diff unavailable.</p>
            ) : detail?.patch ? (
              <pre className="turn-diff-patch"><code>{detail.patch}</code></pre>
            ) : (
              <p className="turn-diff-empty">No file changes in this turn.</p>
            )}
          </div>
        </div>
      )}
    </section>
  );
}
