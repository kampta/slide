use crate::server::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use slide_core::backend::BackendKind;
use slide_core::session::{CreateSessionRequest, ForkSessionRequest, HandoffRequest};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route(
            "/sessions/:id",
            patch(update_session).delete(delete_session),
        )
        .route("/sessions/:id/log", get(get_log))
        .route("/sessions/:id/fork", post(fork_session))
        .route("/sessions/:id/handoff", post(handoff_session))
        .route("/sessions/:id/context", get(get_context))
        .route("/sessions/:id/subagents", get(get_subagents))
        .route("/ls", get(list_dir))
        .route("/diagnostics", get(get_runtime_diagnostics))
        .route("/backends", get(list_backends))
        .route("/ssh-hosts", get(list_ssh_hosts))
}

async fn list_backends() -> Response {
    Json(slide_core::backend::available()).into_response()
}

async fn list_ssh_hosts() -> Response {
    Json(slide_core::ssh::list_hosts()).into_response()
}

async fn list_sessions(State(state): State<AppState>) -> Response {
    match state.manager.list().await {
        Ok(s) => Json(s).into_response(),
        Err(e) => server_error(&e),
    }
}

async fn create_session(
    State(state): State<AppState>,
    Json(mut req): Json<CreateSessionRequest>,
) -> Response {
    // Local paths from the UI may use `~` (the file picker accepts it via
    // the `/api/ls` endpoint, which already expands). Mirror that here so
    // the session record stores the canonical absolute path and downstream
    // git operations don't see a literal `~/...` they can't chdir into.
    if matches!(req.location, slide_core::session::Location::Local) {
        req.base_dir = expand_tilde(&req.base_dir).to_string_lossy().into_owned();
        if let Some(p) = req.project_path.as_deref() {
            if !p.trim().is_empty() {
                req.project_path = Some(expand_tilde(p).to_string_lossy().into_owned());
            }
        }
    }
    match state.manager.create(req).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => client_error(&e),
    }
}

#[derive(Deserialize)]
struct UpdateReq {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    action: Option<String>, // "stop" | "resume"
    /// When resuming, optionally switch the session's LLM backend. Ignored
    /// for other actions. A switch starts a fresh conversation in the same
    /// workspace (provider conversation ids are not portable).
    #[serde(default)]
    backend: Option<BackendKind>,
}

async fn update_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateReq>,
) -> Response {
    let mgr = &state.manager;
    let result = async {
        if req.backend.is_some() && req.action.as_deref() != Some("resume") {
            anyhow::bail!("backend can only be set when action is \"resume\"");
        }
        let mut session = None;
        if let Some(name) = req.name.as_deref() {
            session = Some(mgr.rename(&id, name).await?);
        }
        match req.action.as_deref() {
            Some("stop") => session = Some(mgr.stop(&id).await?),
            Some("resume") => session = Some(mgr.resume(&id, req.backend).await?),
            Some(other) => anyhow::bail!("unknown action: {other}"),
            None => {}
        }
        let s = match session {
            Some(s) => s,
            None => mgr
                .list()
                .await?
                .into_iter()
                .find(|s| s.id == id)
                .ok_or_else(|| anyhow::anyhow!("unknown session"))?,
        };
        anyhow::Ok(s)
    }
    .await;
    match result {
        Ok(s) => Json(s).into_response(),
        Err(e) => client_error(&e),
    }
}

async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.manager.delete(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => server_error(&e),
    }
}

async fn fork_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ForkSessionRequest>,
) -> Response {
    match state.manager.fork_session(&id, request).await {
        Ok(session) => Json(session).into_response(),
        Err(error) => client_error(&error),
    }
}

async fn handoff_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<HandoffRequest>,
) -> Response {
    match state.manager.handoff(&id, request).await {
        Ok(session) => Json(session).into_response(),
        Err(error) => client_error(&error),
    }
}

async fn get_log(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.manager.get_log(&id).await {
        Ok(bytes) => ([("content-type", "application/octet-stream")], bytes).into_response(),
        Err(e) => server_error(&e),
    }
}

async fn get_context(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    // Returns `null` when the session has no usable transcript yet — the
    // frontend treats that as "hide the chip" rather than an error.
    Json(state.manager.context_usage(&id).await).into_response()
}

async fn get_subagents(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.manager.subagents(&id).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => server_error(&error),
    }
}

#[derive(Deserialize)]
struct DiagnosticsQuery {
    host: Option<String>,
    #[serde(default)]
    refresh: bool,
}

async fn get_runtime_diagnostics(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<DiagnosticsQuery>,
) -> Response {
    let host = query.host.as_deref().filter(|host| !host.is_empty());
    match state.manager.runtime_diagnostics(host, query.refresh).await {
        Ok(diagnostics) => Json(diagnostics).into_response(),
        Err(error) => client_error(&error),
    }
}

#[derive(Deserialize)]
struct LsQuery {
    path: Option<String>,
    host: Option<String>,
}

async fn list_dir(axum::extract::Query(q): axum::extract::Query<LsQuery>) -> Response {
    match tokio::task::spawn_blocking(move || {
        if let Some(host) = q.host.as_deref().filter(|s| !s.is_empty()) {
            match list_dir_remote(host, q.path.as_deref()) {
                Ok(v) => Json(v).into_response(),
                Err(e) => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response(),
            }
        } else {
            list_dir_local(q.path.as_deref())
        }
    })
    .await
    {
        Ok(response) => response,
        Err(error) => server_error(&error.into()),
    }
}

fn list_dir_local(path: Option<&str>) -> Response {
    let path = path.map(|s| s.to_string()).unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let expanded = expand_tilde(&path);
    let canonical = match expanded.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    if !path_within_allowed_roots(&canonical) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "path is outside the allowed roots (home, /Users, /home, /tmp, /Volumes)",
            })),
        )
            .into_response();
    }
    match std::fs::read_dir(&canonical) {
        Ok(rd) => {
            let mut entries: Vec<serde_json::Value> = rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        && !e.file_name().to_string_lossy().starts_with('.')
                })
                .map(|e| {
                    json!({
                        "name": e.file_name().to_string_lossy(),
                        "path": e.path().to_string_lossy(),
                    })
                })
                .collect();
            entries.sort_by(|a, b| {
                a["name"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
            });
            Json(json!({
                "path": canonical.to_string_lossy(),
                "entries": entries,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// List directories on a remote SSH host. Re-uses `validate_host` defense
/// against `-oProxyCommand=` injection, then runs a small POSIX shell script
/// remotely that resolves `~`/empty to `$HOME`, prints the canonical path,
/// and emits one directory name per line.
fn list_dir_remote(host: &str, path: Option<&str>) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context;
    use std::process::Command;

    slide_core::ssh::validate_host(host).context("invalid ssh host")?;

    // Passed as `$1` to the remote sh -c script — no further escaping needed
    // inside the script, since sh handles the argv split for us.
    let remote_path = path.unwrap_or("");

    // Portable across BSD (macOS) and GNU coreutils:
    //   - `ls -A1p` lists hidden-excluding-dotdot entries, one per line, with
    //     a trailing `/` on directories.
    //   - `grep '/$'` keeps directories.
    //   - `sed` strips the trailing slash so the UI can just append `/name`.
    // We additionally drop entries beginning with `.` to match local behavior.
    let script = r#"P="$1"
case "$P" in
  '') P="$HOME" ;;
  '~') P="$HOME" ;;
  '~/'*) P="$HOME/${P#~/}" ;;
esac
cd -- "$P" 2>/dev/null || { echo "cannot read $P" >&2; exit 1; }
pwd -P
ls -A1p 2>/dev/null | while IFS= read -r f; do
  case "$f" in
    .*) continue ;;
    */) printf '%s\n' "${f%/}" ;;
  esac
done"#;

    let remote = format!("sh -c {} _ {}", sh_quote(script), sh_quote(remote_path),);
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes"]);
    // ConnectTimeout + multiplex onto any existing master so the directory
    // browser doesn't pay a fresh handshake per click on a slow VPN, and
    // a dead remote fails fast instead of hanging the request.
    for a in slide_core::ssh::ssh_args() {
        cmd.arg(a);
    }
    cmd.arg(host).arg(&remote);
    let out = cmd.output().context("spawn ssh")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ssh ls failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    let resolved = lines.next().unwrap_or("").to_string();
    if resolved.is_empty() {
        anyhow::bail!("remote returned empty path");
    }
    let mut entries: Vec<serde_json::Value> = lines
        .filter(|l| !l.is_empty())
        .map(|name| {
            let path = if resolved.ends_with('/') {
                format!("{}{}", resolved, name)
            } else {
                format!("{}/{}", resolved, name)
            };
            json!({ "name": name, "path": path })
        })
        .collect();
    entries.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    Ok(json!({
        "path": resolved,
        "entries": entries,
    }))
}

/// POSIX single-quote a string for embedding into a shell command. Mirrors
/// the helper in `slide-core::tmux` but kept local to avoid widening that
/// crate's public surface for one helper.
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '/' | '.' | ':' | '=' | '@' | '+' | ',')
        })
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Roots the directory picker is allowed to enumerate. The token already
/// gates `/api/ls` against unauthenticated callers; this is defense in
/// depth so a token leak via, e.g., a future logging bug doesn't hand
/// an attacker `read_dir` on `/etc` or `/var`. The list intentionally
/// covers where users keep code and projects on macOS + Linux.
fn allowed_roots() -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home);
    }
    for fixed in ["/Users", "/home", "/tmp", "/private/tmp", "/Volumes"] {
        roots.push(std::path::PathBuf::from(fixed));
    }
    roots
        .into_iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect()
}

fn path_within_allowed_roots(canonical: &std::path::Path) -> bool {
    allowed_roots()
        .iter()
        .any(|root| canonical.starts_with(root))
}

fn expand_tilde(p: &str) -> std::path::PathBuf {
    if let Some(stripped) = p.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            let rest = stripped.trim_start_matches('/');
            return if rest.is_empty() {
                home
            } else {
                home.join(rest)
            };
        }
    }
    std::path::PathBuf::from(p)
}

fn client_error(e: &anyhow::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

fn server_error(e: &anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_roots_includes_home() {
        let roots = allowed_roots();
        let home = dirs::home_dir().expect("home").canonicalize().unwrap();
        assert!(roots.iter().any(|r| r == &home));
    }

    #[test]
    fn home_subdirectory_is_allowed() {
        let home = dirs::home_dir().expect("home").canonicalize().unwrap();
        assert!(path_within_allowed_roots(&home));
        assert!(path_within_allowed_roots(&home.join("any/nested/dir")));
    }

    #[test]
    fn system_dirs_are_rejected() {
        // /etc, /var, /usr are intentionally not in the allow-list. The
        // directory picker has no business there even with a valid token.
        for p in ["/etc", "/var", "/usr/local/bin", "/root"] {
            let path = std::path::PathBuf::from(p);
            assert!(!path_within_allowed_roots(&path), "{p} should be rejected");
        }
    }

    #[test]
    fn tmp_is_allowed_after_canonicalization() {
        // macOS canonicalizes /tmp to /private/tmp. Both are listed in
        // allowed_roots(); the test path here is the canonicalized form.
        let tmp = std::path::PathBuf::from("/tmp")
            .canonicalize()
            .expect("/tmp should canonicalize");
        assert!(path_within_allowed_roots(&tmp));
    }
}
