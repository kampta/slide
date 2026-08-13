import { useEffect, useRef, useState } from "react";
import {
  api,
  type Backend,
  type BackendInfo,
  type ExecutionPolicy,
  type Location,
  type SshHost,
} from "../state/api";
import { useSessions } from "../state/sessionStore";
import { useModalDialog } from "../hooks/useModalDialog";
import { DirectoryBrowser } from "./DirectoryBrowser";

const RECENTS_KEY = "slide.recentBaseDirs";

type ClusterKey = string; // "local" or "ssh:<alias>"

function clusterKey(location: Location, sshHost: string): ClusterKey {
  return location === "remote" ? `ssh:${sshHost}` : "local";
}

function allRecents(): Record<ClusterKey, string[]> {
  try {
    const raw = localStorage.getItem(RECENTS_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw);
    // Legacy: a flat array under the same key — migrate to { local: [...] }.
    if (Array.isArray(parsed)) return { local: parsed };
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

function recents(cluster: ClusterKey): string[] {
  const all = allRecents();
  const list = all[cluster];
  if (Array.isArray(list)) return list;
  // First time we see a remote cluster: seed empty. The legacy flat list
  // already migrated into `local` via allRecents().
  return [];
}

function pushRecent(cluster: ClusterKey, dir: string) {
  const all = allRecents();
  const prev = Array.isArray(all[cluster]) ? all[cluster] : [];
  all[cluster] = [dir, ...prev.filter((d) => d !== dir)].slice(0, 8);
  try {
    localStorage.setItem(RECENTS_KEY, JSON.stringify(all));
  } catch {}
}

// Names feed directly into git branch names, worktree directory names, and
// (potentially) tmux session names. Restricting to [A-Za-z0-9_-] sidesteps
// tmux's ban on `.`/`:` and git refname rules in one shot.
const NAME_RE = /^[A-Za-z0-9_][A-Za-z0-9_-]*$/;

function nameError(name: string): string | null {
  if (!name) return null;
  if (/\s/.test(name)) return "No spaces allowed.";
  if (!NAME_RE.test(name)) {
    return "Only letters, digits, underscore, and hyphen; must not start with a hyphen.";
  }
  return null;
}

export function NewSessionDialog({
  open,
  onClose,
  onCreated,
}: {
  open: boolean;
  onClose: () => void;
  onCreated: (id: string) => void;
}) {
  const [name, setName] = useState("");
  const [backend, setBackend] = useState<Backend>("claude");
  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [executionPolicy, setExecutionPolicy] =
    useState<ExecutionPolicy>("unrestricted");
  const [location, setLocation] = useState<Location>("local");
  const [sshHost, setSshHost] = useState("");
  const [sshHosts, setSshHosts] = useState<SshHost[]>([]);
  const [baseDir, setBaseDir] = useState("");
  const [browseOpen, setBrowseOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const nameRef = useRef<HTMLInputElement | null>(null);
  const createSession = useSessions((state) => state.createSession);
  const dialogRef = useModalDialog<HTMLFormElement>(open, onClose, !submitting);

  useEffect(() => {
    api
      .listBackends()
      .then((available) => {
        setBackends(available);
        if (available.length > 0 && !available.some((item) => item.id === backend)) {
          setBackend(available[0].id);
        }
      })
      .catch(() => setBackends([]));
    api.listSshHosts().then(setSshHosts).catch(() => setSshHosts([]));
    // Initial metadata load only. The daemon's backend list is stable for
    // the process lifetime.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const cluster = clusterKey(location, sshHost);
  const selectedBackend = backends.find((item) => item.id === backend);

  useEffect(() => {
    if (!open) return;
    setError(null);
    setSubmitting(false);
    const timer = window.setTimeout(() => nameRef.current?.focus(), 10);
    return () => window.clearTimeout(timer);
  }, [open]);

  // Whenever the dialog opens or the cluster (Local / SSH host) changes,
  // reset the base dir to the last one used for that cluster.
  useEffect(() => {
    if (!open) return;
    setBrowseOpen(false);
    if (location === "remote" && !sshHost) {
      setBaseDir("");
      return;
    }
    setBaseDir(recents(cluster)[0] ?? "");
  }, [open, cluster, location, sshHost]);

  if (!open) return null;

  const canSubmit =
    !submitting &&
    name.trim().length > 0 &&
    baseDir.trim().length > 0 &&
    nameError(name.trim()) === null &&
    backends.some((item) => item.id === backend) &&
    (location === "local" || sshHost.trim().length > 0);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!name.trim() || !baseDir.trim()) {
      setError("Name and base directory are required.");
      return;
    }
    const nameErr = nameError(name.trim());
    if (nameErr) {
      setError(nameErr);
      return;
    }
    if (location === "remote" && !sshHost.trim()) {
      setError("SSH host is required for remote sessions.");
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      const s = await createSession({
        name: name.trim(),
        backend,
        execution_policy: executionPolicy,
        location,
        ssh_host: location === "remote" ? sshHost.trim() : undefined,
        base_dir: baseDir.trim(),
      });
      pushRecent(cluster, baseDir.trim());
      setName("");
      onCreated(s.id);
      onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSubmitting(false);
    }
  }

  return (
    <div
      className="modal-backdrop"
      onMouseDown={() => !submitting && onClose()}
    >
      <form
        ref={dialogRef}
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="new-session-title"
        tabIndex={-1}
        onMouseDown={(e) => e.stopPropagation()}
        onSubmit={submit}
      >
        <h2 id="new-session-title">New session</h2>
        <fieldset className="form-field">
          <legend>Location</legend>
          <div className="btn-group btn-group-wrap">
            <button
              type="button"
              className={location === "local" ? "active" : ""}
              aria-pressed={location === "local"}
              onClick={() => {
                setLocation("local");
                setSshHost("");
              }}
            >
              Local
            </button>
            {sshHosts.map((h) => {
              const active = location === "remote" && sshHost === h.alias;
              const detail = `${h.user ? `${h.user}@` : ""}${h.hostname}${h.port ? `:${h.port}` : ""}`;
              return (
                <button
                  key={h.alias}
                  type="button"
                  className={active ? "active" : ""}
                  aria-pressed={active}
                  onClick={() => {
                    setLocation("remote");
                    setSshHost(h.alias);
                  }}
                  title={detail}
                >
                  {h.alias}
                </button>
              );
            })}
          </div>
        </fieldset>
        <fieldset className="form-field">
          <legend>Backend</legend>
          <div className="btn-group btn-group-wrap">
            {backends.map((item) => (
              <button
                key={item.id}
                type="button"
                className={backend === item.id ? "active" : ""}
                aria-pressed={backend === item.id}
                onClick={() => {
                  setBackend(item.id);
                  if (!item.execution_policies.includes(executionPolicy)) {
                    setExecutionPolicy("unrestricted");
                  }
                }}
              >
                {item.label}
              </button>
            ))}
          </div>
        </fieldset>
        {selectedBackend && selectedBackend.execution_policies.length > 1 && (
          <fieldset className="form-field">
            <legend>Permissions</legend>
            <div className="btn-group btn-group-wrap">
              {selectedBackend.execution_policies.map((policy) => (
                <button
                  key={policy}
                  type="button"
                  className={executionPolicy === policy ? "active" : ""}
                  aria-pressed={executionPolicy === policy}
                  onClick={() => setExecutionPolicy(policy)}
                  title={
                    policy === "sandboxed_auto"
                      ? "Use Codex's workspace-write sandbox and never pause for approvals"
                      : "Bypass approvals and filesystem sandboxing"
                  }
                >
                  {policy === "sandboxed_auto" ? "Sandboxed auto" : "Unrestricted"}
                </button>
              ))}
            </div>
          </fieldset>
        )}
        <label>
          <span>{location === "remote" ? "Remote directory" : "Base directory"}</span>
          <div className="dir-input-row">
            <input
              list={`slide-recents-${cluster}`}
              value={baseDir}
              onChange={(e) => setBaseDir(e.target.value)}
              placeholder={location === "remote" ? "/path/on/remote/host" : "/path/to/git/repo"}
              disabled={location === "remote" && !sshHost}
            />
            <button
              type="button"
              className="dir-browse-toggle"
              onClick={() => setBrowseOpen((v) => !v)}
              disabled={location === "remote" && !sshHost}
              aria-expanded={browseOpen}
            >
              {browseOpen ? "Hide" : "Browse"}
            </button>
          </div>
          <datalist id={`slide-recents-${cluster}`}>
            {recents(cluster).map((d) => (
              <option key={d} value={d} />
            ))}
          </datalist>
          {browseOpen && (location === "local" || sshHost) && (
            <>
              <span className="hint">
                Browsing the {location === "remote" ? sshHost : "daemon host"}'s
                filesystem, not your phone's.
              </span>
              <DirectoryBrowser
                startPath={baseDir}
                host={location === "remote" ? sshHost : null}
                onSelect={setBaseDir}
              />
            </>
          )}
        </label>
        <label>
          <span>Session name</span>
          <input
            ref={nameRef}
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. auth-refactor"
            aria-invalid={nameError(name.trim()) !== null}
          />
          {nameError(name.trim()) ? (
            <span className="hint hint-error">{nameError(name.trim())}</span>
          ) : (
            location === "local" && name.trim() && baseDir.trim() ? (
              <span className="hint">
                worktree: <code>{baseDir.trim().replace(/\/+$/, "")}/.slide-worktrees/{name.trim()}</code>
              </span>
            ) : location === "remote" && name.trim() && sshHost.trim() ? (
              <span className="hint">an isolated worktree will be created on {sshHost.trim()}</span>
            ) : null
          )}
        </label>
        {error && <div className="error">{error}</div>}
        <div className="modal-actions">
          <button type="button" onClick={onClose} disabled={submitting}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={!canSubmit}
          >
            {submitting ? "Creating…" : "Create"}
          </button>
        </div>
      </form>
    </div>
  );
}
