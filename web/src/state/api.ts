export type Backend = "claude" | "codex" | "grok" | "agy" | "opencode";
export type Location = "local" | "remote";
export type SessionState = "active" | "waiting" | "unknown" | "stopped";
export type Supervisor = "direct" | "tmux";

export interface BackendInfo {
  id: Backend;
  label: string;
  context_usage: boolean;
  subagents: boolean;
  fork: boolean;
}

export interface SshHost {
  alias: string;
  hostname: string;
  user: string | null;
  port: number | null;
}

export interface Session {
  id: string;
  name: string;
  backend: Backend;
  location: Location;
  ssh_host?: string | null;
  base_dir: string;
  project_path: string;
  worktree: boolean;
  state: SessionState;
  created_at: number;
  last_activity: number;
  supervisor: Supervisor;
  host_log_path?: string | null;
  log_offset: number;
  backend_session_id?: string | null;
  parent_session_id?: string | null;
}

export interface CreateSessionRequest {
  name: string;
  backend: Backend;
  base_dir: string;
  location?: Location;
  ssh_host?: string;
}

/// Last segment of `base_dir` — used as the "repo" label in the sidebar
/// and header so users can tell sessions in different repos apart at a
/// glance. Returns "" if base_dir is empty / just slashes; the caller
/// should fall back to showing only the session name in that case.
export function repoLabel(session: Session): string {
  const trimmed = (session.base_dir || "").replace(/\/+$/, "");
  if (!trimmed) return "";
  const idx = trimmed.lastIndexOf("/");
  return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
}

/// Cluster (host) label: the SSH alias for remote sessions, "local"
/// otherwise. Used as the leading segment of the title chain so that a
/// "slide" repo on `sp1` reads differently from one on the laptop.
export function clusterLabel(session: Session): string {
  if (session.location === "remote" && session.ssh_host) return session.ssh_host;
  return "local";
}

const TOKEN_KEY = "slide.token";

// One-time migration: the token used to live in sessionStorage. Drop the
// stale key so a future read with the same key from sessionStorage (e.g.
// future bug, browser extension) can't shadow the localStorage value.
try {
  sessionStorage.removeItem(TOKEN_KEY);
} catch {
  // sessionStorage can throw in private-mode tabs; ignore.
}

// Token is read on every request so a fresh `?token=…` (e.g. a re-pair via
// QR scan) takes effect immediately. URL token always wins, since it's the
// most recent explicit pairing intent; we cache it to localStorage so the
// phone keeps working after Safari closes/sleeps. The previous module-init
// `const token` capture meant in-flight calls used the stale value after
// daemon restart, which rotates the token on every boot.
export function getToken(): string {
  // Skip the URLSearchParams parse on the common case where the URL was
  // already stripped by App.tsx — `?token=…` only appears on first load.
  if (window.location.search.includes("token=")) {
    const fromUrl = new URLSearchParams(window.location.search).get("token");
    if (fromUrl) {
      try {
        localStorage.setItem(TOKEN_KEY, fromUrl);
      } catch {}
      return fromUrl;
    }
  }
  try {
    return localStorage.getItem(TOKEN_KEY) ?? "";
  } catch {
    return "";
  }
}

export function setToken(t: string): void {
  try {
    if (t) localStorage.setItem(TOKEN_KEY, t);
    else localStorage.removeItem(TOKEN_KEY);
  } catch {}
}

function authHeaders(): HeadersInit {
  const token = getToken();
  return token ? { Authorization: `Bearer ${token}` } : {};
}

// Surfaced both on HTTP 401 and on WebSocket close with code 4401 so the
// user sees one consistent message regardless of which channel discovered
// the stale token first. Mentions both `slide pair` (phone re-pair via QR)
// and `slide open` (host browser re-launch) so the message works from
// either device.
export const STALE_TOKEN_MESSAGE =
  "401 unauthorized — token rotated. On phone, re-scan the QR from `slide pair` (or run `slide open` on the host).";

// WebSocket application close code emitted by the daemon when the bearer
// token in the upgrade subprotocol is missing or doesn't match. Mirrors
// `WS_CLOSE_AUTH_FAILED` in crates/slide-cli/src/server.rs.
export const WS_CLOSE_AUTH_FAILED = 4401;

async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: {
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      ...authHeaders(),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 401) {
    setToken("");
    throw new Error(STALE_TOKEN_MESSAGE);
  }
  if (!res.ok) {
    const text = await res.text();
    let detail = text;
    try {
      const j = JSON.parse(text);
      if (j && typeof j.error === "string") detail = j.error;
    } catch {}
    throw new Error(`${res.status} ${method} ${path}: ${detail || "(empty body)"}`);
  }
  return (await res.json()) as T;
}

const sessionPath = (id: string, suffix = "") =>
  `/api/sessions/${encodeURIComponent(id)}${suffix}`;

export interface ContextUsage {
  used_tokens: number;
  window: number;
  model: string;
  input_tokens: number;
  cache_read_input_tokens: number;
  cache_creation_input_tokens: number;
  output_tokens: number;
}

export type SubagentState =
  | "starting"
  | "running"
  | "waiting"
  | "completed"
  | "failed";

export interface Subagent {
  id: string;
  parent_id: string;
  name: string | null;
  role: string | null;
  state: SubagentState;
  created_at: number;
  updated_at: number;
}

export interface SubagentList {
  supported: boolean;
  agents: Subagent[];
}

export interface TurnDiffSummary {
  id: number;
  turn: number;
  started_at: number;
  completed_at: number;
  files_changed: number;
  additions: number;
  deletions: number;
  truncated: boolean;
}

export interface TurnDiff extends TurnDiffSummary {
  patch: string;
}

export interface ScheduledJob {
  id: string;
  session_id: string;
  title: string;
  prompt: string;
  schedule_kind: "once" | "interval";
  interval_seconds: number | null;
  next_run_at: number;
  retry_at: number | null;
  enabled: boolean;
  last_run_at: number | null;
  last_error: string | null;
  run_count: number;
  created_at: number;
  updated_at: number;
}

export interface CreateScheduledJobRequest {
  title: string;
  prompt: string;
  schedule_kind: "once" | "interval";
  interval_seconds?: number;
  next_run_at: number;
  enabled: boolean;
}

export interface Artifact {
  id: number;
  filename: string;
  title: string | null;
  text: string | null;
  content_type: string;
  size: number;
}

export interface ArtifactList {
  manifest_present: boolean;
  artifacts: Artifact[];
  unavailable: number;
}

export type RuntimeStatus =
  | "ready"
  | "missing"
  | "unauthenticated"
  | "broken";

export interface RuntimeDiagnostic {
  backend: Backend;
  status: RuntimeStatus;
  available: boolean;
  installed: boolean;
  authenticated: boolean | null;
  version: string | null;
  message: string;
  action: string | null;
  last_error: string | null;
}

export interface RuntimeCapability {
  available: boolean;
  required: boolean;
  version: string | null;
  message: string;
  action: string | null;
}

export interface RuntimeDiagnosticsSnapshot {
  target: string;
  checked_at: number;
  backends: RuntimeDiagnostic[];
  tmux: RuntimeCapability;
}

export interface HistorySearchResult {
  session_id: string;
  session_name: string;
  backend: Backend;
  location: Location;
  state: SessionState;
  position: number;
  snippet: string;
}

export interface HistorySearchResponse {
  results: HistorySearchResult[];
  searched_sessions: number;
  unavailable_sessions: number;
  truncated: boolean;
}

export const api = {
  listSessions: () => req<Session[]>("GET", "/api/sessions"),
  listBackends: () => req<BackendInfo[]>("GET", "/api/backends"),
  createSession: (r: CreateSessionRequest) =>
    req<Session>("POST", "/api/sessions", r),
  updateSession: (
    id: string,
    patch: { name?: string; action?: "stop" | "resume" },
  ) => req<Session>("PATCH", sessionPath(id), patch),
  deleteSession: (id: string) =>
    req<{ ok: boolean }>("DELETE", sessionPath(id)),
  listDir: (opts: { path?: string; host?: string } = {}) => {
    const params = new URLSearchParams();
    if (opts.path) params.set("path", opts.path);
    if (opts.host) params.set("host", opts.host);
    const qs = params.toString();
    return req<{ path: string; entries: { name: string; path: string }[] }>(
      "GET",
      `/api/ls${qs ? `?${qs}` : ""}`,
    );
  },
  listSshHosts: () => req<SshHost[]>("GET", "/api/ssh-hosts"),
  getContext: (id: string) =>
    req<ContextUsage | null>("GET", sessionPath(id, "/context")),
  getSubagents: (id: string) =>
    req<SubagentList>("GET", sessionPath(id, "/subagents")),
  listTurnDiffs: (id: string) =>
    req<TurnDiffSummary[]>("GET", sessionPath(id, "/turn-diffs")),
  getTurnDiff: (id: string, turnDiffId: number) =>
    req<TurnDiff>(
      "GET",
      sessionPath(id, `/turn-diffs/${encodeURIComponent(turnDiffId)}`),
    ),
  getRuntimeDiagnostics: (opts: { host?: string; refresh?: boolean } = {}) => {
    const params = new URLSearchParams();
    if (opts.host) params.set("host", opts.host);
    if (opts.refresh) params.set("refresh", "true");
    const query = params.toString();
    return req<RuntimeDiagnosticsSnapshot>(
      "GET",
      `/api/diagnostics${query ? `?${query}` : ""}`,
    );
  },
  searchHistory: (query: string) =>
    req<HistorySearchResponse>("POST", "/api/history/search", { query }),
  forkSession: (id: string, request: { name: string; focus?: string }) =>
    req<Session>("POST", sessionPath(id, "/fork"), request),
  handoffSession: (
    sourceId: string,
    request: { target_session_id: string; focus: string },
  ) => req<Session>("POST", sessionPath(sourceId, "/handoff"), request),
  listScheduledJobs: (sessionId: string) =>
    req<ScheduledJob[]>("GET", sessionPath(sessionId, "/jobs")),
  createScheduledJob: (sessionId: string, request: CreateScheduledJobRequest) =>
    req<ScheduledJob>("POST", sessionPath(sessionId, "/jobs"), request),
  updateScheduledJob: (sessionId: string, jobId: string, enabled: boolean) =>
    req<ScheduledJob>(
      "PATCH",
      sessionPath(sessionId, `/jobs/${encodeURIComponent(jobId)}`),
      { enabled },
    ),
  deleteScheduledJob: (sessionId: string, jobId: string) =>
    req<{ ok: boolean }>(
      "DELETE",
      sessionPath(sessionId, `/jobs/${encodeURIComponent(jobId)}`),
    ),
  runScheduledJobNow: (sessionId: string, jobId: string) =>
    req<ScheduledJob>(
      "POST",
      sessionPath(sessionId, `/jobs/${encodeURIComponent(jobId)}/run`),
    ),
  listArtifacts: (sessionId: string) =>
    req<ArtifactList>("GET", sessionPath(sessionId, "/artifacts")),
  getArtifactBlob: async (sessionId: string, artifactId: number) => {
    const response = await fetch(
      sessionPath(sessionId, `/artifacts/${encodeURIComponent(artifactId)}`),
      { headers: authHeaders() },
    );
    if (response.status === 401) {
      setToken("");
      throw new Error(STALE_TOKEN_MESSAGE);
    }
    if (!response.ok) {
      throw new Error(
        `artifact fetch failed: ${response.status} ${await response.text()}`.trim(),
      );
    }
    return response.blob();
  },
  getLog: async (id: string) => {
    const res = await fetch(sessionPath(id, "/log"), {
      headers: authHeaders(),
    });
    if (res.status === 401) {
      setToken("");
      throw new Error(STALE_TOKEN_MESSAGE);
    }
    if (!res.ok) {
      throw new Error(`log fetch failed: ${res.status} ${await res.text()}`.trim());
    }
    return new Uint8Array(await res.arrayBuffer());
  },
};

function wsUrl(path: string): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}${path}`;
}

// The bearer token rides on the subprotocol list rather than `?token=…`
// so it doesn't end up in browser history, Referer headers, or daemon
// access logs. The server matches `slide.bearer.<token>` and echoes back
// the plain `slide` protocol.
function wsProtocols(): string[] {
  const token = getToken();
  return token ? [`slide.bearer.${token}`, "slide"] : ["slide"];
}

export function openEventsSocket(): WebSocket {
  return new WebSocket(wsUrl("/ws/events"), wsProtocols());
}

export function openSessionSocket(id: string): WebSocket {
  const ws = new WebSocket(
    wsUrl(`/ws/session/${encodeURIComponent(id)}`),
    wsProtocols(),
  );
  ws.binaryType = "arraybuffer";
  return ws;
}
