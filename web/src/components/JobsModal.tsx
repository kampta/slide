import { FormEvent, useEffect, useRef, useState } from "react";
import {
  api,
  type ScheduledJob,
  type Session,
} from "../state/api";

type IntervalUnit = "minutes" | "hours" | "days";

export function toIntervalSeconds(value: number, unit: IntervalUnit): number {
  const multiplier = unit === "minutes" ? 60 : unit === "hours" ? 3_600 : 86_400;
  return value * multiplier;
}

export function localDateTimeValue(date: Date): string {
  const local = new Date(date.getTime() - date.getTimezoneOffset() * 60_000);
  return local.toISOString().slice(0, 16);
}

function formatTime(value: number): string {
  return new Date(value).toLocaleString([], {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

function formatInterval(seconds: number): string {
  if (seconds % 86_400 === 0) return `${seconds / 86_400}d`;
  if (seconds % 3_600 === 0) return `${seconds / 3_600}h`;
  return `${seconds / 60}m`;
}

export function JobsModal({
  open,
  session,
  onClose,
}: {
  open: boolean;
  session: Session;
  onClose: () => void;
}) {
  const [jobs, setJobs] = useState<ScheduledJob[]>([]);
  const [title, setTitle] = useState("");
  const [prompt, setPrompt] = useState("");
  const [kind, setKind] = useState<"once" | "interval">("once");
  const [runAt, setRunAt] = useState(() =>
    localDateTimeValue(new Date(Date.now() + 5 * 60_000)),
  );
  const [intervalValue, setIntervalValue] = useState("1");
  const [intervalUnit, setIntervalUnit] = useState<IntervalUnit>("hours");
  const [loading, setLoading] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const requestVersion = useRef(0);

  useEffect(() => {
    if (!open) return;
    requestVersion.current += 1;
    let cancelled = false;
    let inFlight = false;
    const refresh = async () => {
      if (inFlight) return;
      inFlight = true;
      const version = requestVersion.current;
      try {
        const next = await api.listScheduledJobs(session.id);
        if (!cancelled && version === requestVersion.current) setJobs(next);
      } catch (reason) {
        if (!cancelled && version === requestVersion.current) {
          setError(reason instanceof Error ? reason.message : String(reason));
        }
      } finally {
        inFlight = false;
      }
    };
    setError(null);
    void refresh();
    const timer = window.setInterval(refresh, 5_000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [open, session.id]);

  if (!open) return null;

  async function mutate<T>(
    label: string,
    action: () => Promise<T>,
    apply: (result: T) => void,
  ) {
    if (loading) return;
    requestVersion.current += 1;
    setLoading(label);
    setError(null);
    try {
      apply(await action());
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setLoading(null);
    }
  }

  function create(event: FormEvent) {
    event.preventDefault();
    const nextRunAt = new Date(runAt).getTime();
    const interval = Number(intervalValue);
    if (!Number.isFinite(nextRunAt)) {
      setError("Choose a valid first run time.");
      return;
    }
    if (kind === "interval" && (!Number.isInteger(interval) || interval < 1)) {
      setError("Choose a positive whole-number interval.");
      return;
    }
    void mutate(
      "create",
      () => api.createScheduledJob(session.id, {
        title,
        prompt,
        schedule_kind: kind,
        ...(kind === "interval"
          ? { interval_seconds: toIntervalSeconds(interval, intervalUnit) }
          : {}),
        next_run_at: nextRunAt,
        enabled: true,
      }),
      (job) => {
        setJobs((current) => [job, ...current.filter((item) => item.id !== job.id)]);
        setTitle("");
        setPrompt("");
        setRunAt(localDateTimeValue(new Date(Date.now() + 5 * 60_000)));
      },
    );
  }

  return (
    <div className="modal-backdrop" onMouseDown={() => !loading && onClose()}>
      <section
        className="modal jobs-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="jobs-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="diagnostics-heading">
          <div>
            <h2 id="jobs-title">Scheduled jobs</h2>
            <p>{session.name} · prompts submit only while the session is Waiting</p>
          </div>
          <button
            type="button"
            className="btn-icon"
            onClick={onClose}
            disabled={Boolean(loading)}
            aria-label="Close scheduled jobs"
          >
            ×
          </button>
        </div>

        <form className="jobs-create" onSubmit={create}>
          <label>
            <span>Title</span>
            <input value={title} onChange={(event) => setTitle(event.target.value)} maxLength={100} required />
          </label>
          <label>
            <span>Prompt</span>
            <textarea value={prompt} onChange={(event) => setPrompt(event.target.value)} maxLength={4000} required />
          </label>
          <div className="jobs-schedule-row">
            <label>
              <span>Schedule</span>
              <select value={kind} onChange={(event) => setKind(event.target.value as typeof kind)}>
                <option value="once">One time</option>
                <option value="interval">Fixed interval</option>
              </select>
            </label>
            <label>
              <span>First run</span>
              <input type="datetime-local" value={runAt} onChange={(event) => setRunAt(event.target.value)} required />
            </label>
          </div>
          {kind === "interval" && (
            <div className="jobs-interval-row">
              <label>
                <span>Every</span>
                <input
                  type="number"
                  min="1"
                  step="1"
                  value={intervalValue}
                  onChange={(event) => setIntervalValue(event.target.value)}
                  required
                />
              </label>
              <label>
                <span>Unit</span>
                <select value={intervalUnit} onChange={(event) => setIntervalUnit(event.target.value as IntervalUnit)}>
                  <option value="minutes">Minutes</option>
                  <option value="hours">Hours</option>
                  <option value="days">Days</option>
                </select>
              </label>
            </div>
          )}
          <button type="submit" className="btn-primary" disabled={Boolean(loading)}>
            {loading === "create" ? "Creating…" : "Create job"}
          </button>
        </form>

        <div className="jobs-list" aria-live="polite">
          {jobs.length === 0 && <p className="jobs-empty">No scheduled jobs for this session.</p>}
          {jobs.map((job) => (
            <article className={`job-card${job.enabled ? "" : " job-disabled"}`} key={job.id}>
              <div className="job-card-main">
                <strong>{job.title}</strong>
                <span>
                  {job.schedule_kind === "interval" && job.interval_seconds
                    ? `Every ${formatInterval(job.interval_seconds)} · `
                    : ""}
                  {job.enabled ? `Next ${formatTime(job.retry_at ?? job.next_run_at)}` : "Disabled"}
                </span>
                <p>{job.prompt}</p>
                {job.last_error && <em>{job.last_error}</em>}
                {job.last_run_at && <small>Last submitted {formatTime(job.last_run_at)} · {job.run_count} run{job.run_count === 1 ? "" : "s"}</small>}
              </div>
              <div className="job-card-actions">
                <button
                  type="button"
                  disabled={Boolean(loading) || session.state !== "waiting"}
                  title={session.state === "waiting" ? "Submit this prompt now" : "Session must be Waiting"}
                  onClick={() => void mutate(
                    job.id,
                    () => api.runScheduledJobNow(session.id, job.id),
                    (updated) => setJobs((current) => current.map((item) => item.id === updated.id ? updated : item)),
                  )}
                >
                  Run now
                </button>
                <button
                  type="button"
                  disabled={Boolean(loading)}
                  onClick={() => void mutate(
                    job.id,
                    () => api.updateScheduledJob(session.id, job.id, !job.enabled),
                    (updated) => setJobs((current) => current.map((item) => item.id === updated.id ? updated : item)),
                  )}
                >
                  {job.enabled ? "Pause" : "Enable"}
                </button>
                <button
                  type="button"
                  className="danger"
                  disabled={Boolean(loading)}
                  onClick={() => {
                    if (confirm(`Delete scheduled job "${job.title}"?`)) {
                      void mutate(
                        job.id,
                        () => api.deleteScheduledJob(session.id, job.id),
                        () => setJobs((current) => current.filter((item) => item.id !== job.id)),
                      );
                    }
                  }}
                >
                  Delete
                </button>
              </div>
            </article>
          ))}
        </div>
        {error && <p className="error jobs-error">{error}</p>}
      </section>
    </div>
  );
}
