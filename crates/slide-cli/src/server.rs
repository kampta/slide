use crate::assets;
use crate::http;
use crate::pair;
use crate::ws;
use anyhow::{Context, Result};
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Router};
use rand::RngCore;
use slide_core::config;
use slide_core::SessionManager;
use std::sync::Arc;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<SessionManager>,
    pub token: Arc<String>,
    /// Hostnames the daemon will accept in the Host (and, when present,
    /// Origin) header. Always includes loopback names; widened with the
    /// concrete LAN IPs we're listening on when `--lan` (or any
    /// non-loopback `--bind`) is used, so phones reaching us at e.g.
    /// `http://100.64.0.5:7777/` aren't rejected by the DNS-rebinding
    /// check while DNS-rebinding-as-loopback attacks from the public
    /// internet still 403.
    pub allowed_hosts: Arc<Vec<String>>,
}

pub async fn run(bind: &str, port: u16, open_browser: bool, dev: bool) -> Result<()> {
    config::ensure_dirs()?;
    // Claim the port before mutating the daemon lock or reconciling stored
    // sessions. A second `slide serve` must fail without corrupting the
    // first daemon's discovery file.
    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let manager = SessionManager::new()
        .await
        .context("init session manager")?;
    // Hold a separate handle for the post-serve shutdown drain. AppState
    // gets moved into `with_state` below, so we can't reach back through
    // it once axum owns the router.
    let manager_for_shutdown = manager.clone();

    // Limit a leaked pairing URL to this daemon process. `slide open` and
    // `slide pair` read the current value from the lock file.
    let token = generate_token();
    write_lock_file(&token, bind, port)?;
    let _lock_guard = DaemonLockGuard;

    let allowed_hosts = build_allowed_hosts(bind);
    let state = AppState {
        manager,
        token: Arc::new(token.clone()),
        allowed_hosts: Arc::new(allowed_hosts),
    };

    // /api/* is fully gated by `auth_layer` (Host/Origin + token). /ws/* only
    // receives the Host/Origin check at the middleware layer; the token check
    // moves into the WS handler so we can return an application-layer close
    // code (4401) the frontend can observe — browsers don't expose the HTTP
    // status from a failed WS handshake to JS, so a token-rejected handshake
    // would otherwise look identical to a transient network drop.
    let api = Router::new()
        .nest("/api", http::routes())
        .layer(middleware::from_fn_with_state(state.clone(), auth_layer));
    let ws_routes = Router::new()
        .route("/ws/events", get(ws::events))
        .route("/ws/session/:id", get(ws::session))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            host_origin_layer,
        ));
    let protected = api.merge(ws_routes);

    // Always-open routes: health check + static assets.
    let mut app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .merge(protected);

    if dev {
        app = app.fallback(get(|| async {
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain")],
                "slide dev daemon running — open http://localhost:5173 for the Vite UI",
            )
                .into_response()
        }));
    } else {
        app = app.fallback(get(assets::serve));
    }

    let app = app
        .layer(CatchPanicLayer::custom(
            |err: Box<dyn std::any::Any + Send + 'static>| {
                let msg = if let Some(s) = err.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = err.downcast_ref::<&str>() {
                    (*s).to_string()
                } else {
                    "handler panicked".to_string()
                };
                tracing::error!("panic: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, "application/json")],
                    axum::body::Body::from(
                        serde_json::json!({ "error": format!("internal error: {msg}") })
                            .to_string(),
                    ),
                )
                    .into_response()
            },
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // The token-bearing URL is only used internally by the auto-open path
    // and never written to stdout in production: doing so would leak the
    // token into shell scrollback, screen recordings, and `ps -ef`
    // snapshots. Users who disabled auto-open can re-launch the browser
    // with `slide open`, which reads the token from the mode-0600 lock
    // file directly.
    //
    // Dev mode is the exception: `dev.sh` runs `serve --no-open --dev` so
    // that Vite (port 5173) hosts the SPA and proxies /api+/ws to the
    // daemon. `slide open` would target the daemon's own port and serve
    // the fallback "open Vite at :5173" page — useless. So in dev mode we
    // print the full URL with token instead, on the assumption that a
    // developer running their own toolchain on their own laptop is fine
    // with the token in their terminal scrollback.
    let local_addr = local_browser_addr(bind, port);
    let bootstrap_url = if dev {
        format!("http://localhost:5173/?token={token}")
    } else {
        format!("http://{local_addr}/?token={token}")
    };
    let public_url = if dev {
        bootstrap_url.clone()
    } else {
        format!("http://{local_addr}/")
    };
    tracing::info!("slide listening on http://{addr}");
    println!();
    println!("  open slide in your browser:");
    println!("    {public_url}");
    if !dev && !open_browser {
        println!();
        println!("  (auto-open disabled — run `slide open` to launch the browser)");
    }
    println!();
    if !dev {
        // Startup never prints the token-bearing URL — same stance as #32
        // for the loopback case. The operator runs `slide pair` to get
        // the scannable QR explicitly. Touches stdout only when there are
        // actually LAN URLs to advertise.
        pair::print_lan_summary(bind, port);
    }

    if open_browser {
        let url = bootstrap_url.clone();
        tokio::task::spawn_blocking(move || {
            let _ = opener::open(&url);
        });
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    // After axum returns, the listener is closed and in-flight requests
    // have drained. Now stop direct-supervised backends so they don't
    // outlive us as orphans; tmux-supervised sessions stay alive on
    // purpose so the user can reattach later.
    manager_for_shutdown.shutdown().await;
    tracing::info!("slide daemon stopped");
    Ok(())
}

/// Resolve when the user asks for shutdown via SIGINT (Ctrl+C) or, on
/// Unix, SIGTERM. Both signals get the same drain treatment; the daemon
/// finishes in-flight HTTP work before exiting.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutdown signal received; draining");
}

fn generate_token() -> String {
    let mut buf = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut buf);
    hex(&buf)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn write_lock_file(token: &str, bind: &str, port: u16) -> Result<()> {
    let path = config::lock_path();
    let body = serde_json::json!({
        "pid": std::process::id(),
        "bind": bind,
        "port": port,
        "token": token,
    });
    write_secret_file(&path, body.to_string().as_bytes())?;
    Ok(())
}

/// Removes only this process's discovery file. If another daemon has
/// replaced it in the meantime, leave the newer lock untouched.
struct DaemonLockGuard;

impl Drop for DaemonLockGuard {
    fn drop(&mut self) {
        let path = config::lock_path();
        if lock_owned_by(&path, std::process::id()) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn lock_owned_by(path: &std::path::Path, pid: u32) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .and_then(|value| value.get("pid").and_then(|value| value.as_u64()))
        == Some(pid as u64)
}

/// Write a 0o600 file containing secrets (token, daemon.lock). On Unix the
/// file is created via `OpenOptions::mode(0o600).create_new(true)` so the
/// permission bits are set at `open(2)` time — there is no umask-derived
/// window between `write` and `set_permissions`. We unlink any pre-existing
/// file first so a stale token from a prior boot doesn't keep its old mode.
fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create secret file {}", path.display()))?;
        f.write_all(contents)?;
        f.sync_all().ok();
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::File::create(path)
            .with_context(|| format!("create secret file {}", path.display()))?;
        f.write_all(contents)?;
    }
    Ok(())
}

/// Authenticate requests.
///
/// HTTP uses `Authorization: Bearer <token>`. WebSocket upgrades can't set
/// arbitrary headers from the browser, so we ride the subprotocol list:
/// the client sends `Sec-WebSocket-Protocol: slide.bearer.<token>, slide`
/// and the server matches on the `slide.bearer.*` entry. The handler
/// separately echoes back `slide` so `WebSocket.protocol` is set and no
/// strict intermediary drops the handshake.
///
/// The token in a subprotocol header doesn't leak into browser history,
/// referers, or server access logs the way `?token=…` did.
async fn auth_layer(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    // DNS rebinding hardening. By default the daemon binds 127.0.0.1, so
    // any request whose Host (or Origin) claims a non-loopback authority
    // is either a rebinding attempt by a malicious page or a misrouted
    // reverse proxy. With `--lan` the allowed-hosts list is widened to
    // include the daemon's actual LAN IPs (computed at startup) so the
    // phone's Host header passes; rebinding-to-loopback from the public
    // internet still 403s. Reject before checking the token so a valid
    // token can't be smuggled through.
    if !request_authority_is_allowed(&req, &state.allowed_hosts) {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(axum::body::Body::from("forbidden authority"))
            .unwrap();
    }
    if let Some(supplied) = token_from_headers(req.headers()) {
        if constant_time_eq(supplied.as_bytes(), state.token.as_bytes()) {
            return next.run(req).await;
        }
    }
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(axum::body::Body::from("unauthorized"))
        .unwrap()
}

/// DNS-rebinding check only — applied to WS routes that need to perform
/// the upgrade themselves so the frontend can observe an application-layer
/// close code on auth failure (4401) rather than the opaque 1006 a browser
/// receives when the upgrade is rejected at HTTP level. Token verification
/// happens inside the WS handler.
pub async fn host_origin_layer(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if !request_authority_is_allowed(&req, &state.allowed_hosts) {
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(axum::body::Body::from("forbidden authority"))
            .unwrap();
    }
    next.run(req).await
}

fn request_authority_is_allowed(req: &Request, allowed: &[String]) -> bool {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| host_in(h, allowed))
        .unwrap_or(false);
    if !host_ok {
        return false;
    }
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if !host_in(origin, allowed) {
            return false;
        }
    }
    true
}

/// Return true when `value` (a Host header like `localhost:7777` or an Origin
/// like `http://127.0.0.1:5173`) parses to a host in `allowed`. We only
/// inspect the host portion; the port is irrelevant because the listener
/// already picks it.
fn host_in(value: &str, allowed: &[String]) -> bool {
    let authority = value
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    let authority = authority.split('/').next().unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            Some((host, _)) => host,
            None => return false,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, _)) => host,
            None => authority,
        }
    };
    allowed.iter().any(|a| a == host)
}

/// Pick the address to use for the local browser tab — what `opener::open`
/// receives and what we print to stdout. When `bind` is unspecified
/// (`0.0.0.0` / `::`, set by `--lan`) we MUST substitute loopback:
/// `http://0.0.0.0:7777/` reaches the daemon at the TCP layer, but the
/// request's `Host: 0.0.0.0:7777` isn't in `build_allowed_hosts`'s
/// allow-list, so the DNS-rebinding check 403s every /api and /ws
/// request. The phone-facing URLs in `print_lan_summary` are unaffected
/// because they use real LAN IPs, which the allow-list contains by
/// construction. A specific non-loopback `--bind 192.168.1.5` is kept
/// verbatim; the user picked it and the allow-list includes it.
fn local_browser_addr(bind: &str, port: u16) -> String {
    match bind.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_unspecified() => format!("127.0.0.1:{port}"),
        _ => format!("{bind}:{port}"),
    }
}

/// Build the host allow-list from the bind address. Always includes
/// loopback names. When `bind` is `0.0.0.0` / `::`, also adds every
/// non-loopback IPv4 currently exposed by the host (so the phone's QR
/// URL passes the rebinding check). When `bind` is a specific
/// non-loopback address, only that address is added.
//
// TODO(stage-b): when the daemon gains `--tls-cert`, the phone will
// reach us via the cert's CN (e.g. a Tailscale MagicDNS name like
// `mybox.tail-scale.ts.net`), not an IP. Add a `--allow-host=<name>`
// flag (or derive from the cert) and append the names here, otherwise
// the rebinding check will 403 the hostname-based requests.
fn build_allowed_hosts(bind: &str) -> Vec<String> {
    let mut hosts = vec![
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        "::1".to_string(),
    ];
    let parsed: Option<std::net::IpAddr> = bind.parse().ok();
    match parsed {
        Some(ip) if ip.is_loopback() => {}
        Some(std::net::IpAddr::V4(v4)) if v4.is_unspecified() => {
            for ip in pair::lan_ipv4_addresses() {
                hosts.push(format!("{ip}"));
            }
        }
        Some(std::net::IpAddr::V6(v6)) if v6.is_unspecified() => {
            for ip in pair::lan_ipv4_addresses() {
                hosts.push(format!("{ip}"));
            }
        }
        Some(ip) => {
            hosts.push(format!("{ip}"));
        }
        None => {}
    }
    hosts
}

/// Byte-equality check that doesn't short-circuit on the first differing
/// byte. `==` leaks the shared prefix length through wall time; the
/// daemon's bearer token is a fixed-width 48-char hex string, so a
/// length mismatch is already obvious from the wire and we can return
/// early on length without leaking anything useful.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn token_from_headers(headers: &header::HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = v.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return Some(rest.to_string());
            }
        }
    }
    // Sec-WebSocket-Protocol is a comma-separated list; each entry may have
    // surrounding whitespace. Multiple header values are allowed too.
    for v in headers.get_all(SEC_WEBSOCKET_PROTOCOL).iter() {
        if let Ok(s) = v.to_str() {
            for part in s.split(',') {
                if let Some(rest) = part.trim().strip_prefix(BEARER_PROTO_PREFIX) {
                    return Some(rest.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
fn token_from_request(req: &Request) -> Option<String> {
    token_from_headers(req.headers())
}

/// WebSocket close code returned when the bearer token is missing or stale.
/// In the application range (4000-4999) so it doesn't collide with RFC 6455
/// protocol codes. Frontend special-cases this to clear the stored token
/// and stop the reconnect loop instead of spinning forever.
pub(crate) const WS_CLOSE_AUTH_FAILED: u16 = 4401;

const SEC_WEBSOCKET_PROTOCOL: &str = "sec-websocket-protocol";
pub(crate) const BEARER_PROTO_PREFIX: &str = "slide.bearer.";
pub(crate) const SAFE_PROTO: &str = "slide";

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;

    fn req_with<F: FnOnce(&mut HttpRequest<axum::body::Body>)>(f: F) -> Request {
        let mut r = HttpRequest::builder()
            .uri("/api/sessions")
            .body(axum::body::Body::empty())
            .unwrap();
        f(&mut r);
        r
    }

    #[test]
    fn bearer_header_is_accepted() {
        let r = req_with(|r| {
            r.headers_mut()
                .insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        });
        assert_eq!(token_from_request(&r).as_deref(), Some("abc123"));
    }

    #[test]
    fn subprotocol_bearer_is_accepted() {
        let r = req_with(|r| {
            r.headers_mut().insert(
                SEC_WEBSOCKET_PROTOCOL,
                "slide.bearer.abc123, slide".parse().unwrap(),
            );
        });
        assert_eq!(token_from_request(&r).as_deref(), Some("abc123"));
    }

    #[test]
    fn subprotocol_bearer_tolerates_whitespace_and_ordering() {
        let r = req_with(|r| {
            r.headers_mut().insert(
                SEC_WEBSOCKET_PROTOCOL,
                "slide , slide.bearer.xyz".parse().unwrap(),
            );
        });
        assert_eq!(token_from_request(&r).as_deref(), Some("xyz"));
    }

    #[test]
    fn query_token_is_no_longer_accepted() {
        // The old ?token=… path used to auth WS here; we dropped it.
        let r = HttpRequest::builder()
            .uri("/ws/events?token=abc")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(token_from_request(&r).is_none());
    }

    #[test]
    fn constant_time_eq_matches_equal_tokens() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn constant_time_eq_rejects_differing_bytes_and_lengths() {
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    fn loopback_only() -> Vec<String> {
        vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ]
    }

    #[test]
    fn host_in_accepts_loopback_authorities() {
        let allowed = loopback_only();
        for v in [
            "127.0.0.1",
            "127.0.0.1:7777",
            "localhost",
            "localhost:5173",
            "[::1]",
            "[::1]:7777",
            "http://localhost:5173",
            "http://127.0.0.1:7777/some/path",
            "http://[::1]:7777",
        ] {
            assert!(host_in(v, &allowed), "expected loopback: {v}");
        }
    }

    #[test]
    fn host_in_rejects_non_loopback() {
        let allowed = loopback_only();
        for v in [
            "evil.com",
            "evil.com:7777",
            "http://evil.com",
            "http://evil.com:7777",
            "192.168.1.5",
            "192.168.1.5:7777",
            "10.0.0.1:7777",
            "null",
            "",
        ] {
            assert!(!host_in(v, &allowed), "expected non-loopback: {v}");
        }
    }

    #[test]
    fn host_in_accepts_lan_ip_when_allowed() {
        let allowed = vec!["100.64.0.5".to_string()];
        assert!(host_in("100.64.0.5:7777", &allowed));
        assert!(host_in("http://100.64.0.5:7777/x", &allowed));
        assert!(!host_in("192.168.1.5:7777", &allowed));
    }

    #[test]
    fn request_authority_requires_loopback_host_by_default() {
        let allowed = loopback_only();
        let r = req_with(|r| {
            r.headers_mut()
                .insert(header::HOST, "evil.com:7777".parse().unwrap());
        });
        assert!(!request_authority_is_allowed(&r, &allowed));
    }

    #[test]
    fn request_authority_rejects_missing_host() {
        let allowed = loopback_only();
        let r = req_with(|_| {});
        assert!(!request_authority_is_allowed(&r, &allowed));
    }

    #[test]
    fn request_authority_accepts_loopback_host_without_origin() {
        let allowed = loopback_only();
        let r = req_with(|r| {
            r.headers_mut()
                .insert(header::HOST, "127.0.0.1:7777".parse().unwrap());
        });
        assert!(request_authority_is_allowed(&r, &allowed));
    }

    #[test]
    fn request_authority_accepts_loopback_host_and_origin() {
        let allowed = loopback_only();
        let r = req_with(|r| {
            r.headers_mut()
                .insert(header::HOST, "127.0.0.1:7777".parse().unwrap());
            r.headers_mut()
                .insert(header::ORIGIN, "http://localhost:5173".parse().unwrap());
        });
        assert!(request_authority_is_allowed(&r, &allowed));
    }

    #[test]
    fn request_authority_rejects_non_loopback_origin() {
        let allowed = loopback_only();
        let r = req_with(|r| {
            r.headers_mut()
                .insert(header::HOST, "127.0.0.1:7777".parse().unwrap());
            r.headers_mut()
                .insert(header::ORIGIN, "https://evil.com".parse().unwrap());
        });
        assert!(!request_authority_is_allowed(&r, &allowed));
    }

    #[test]
    fn request_authority_rejects_null_origin() {
        let allowed = loopback_only();
        let r = req_with(|r| {
            r.headers_mut()
                .insert(header::HOST, "127.0.0.1:7777".parse().unwrap());
            r.headers_mut()
                .insert(header::ORIGIN, "null".parse().unwrap());
        });
        assert!(!request_authority_is_allowed(&r, &allowed));
    }

    #[test]
    fn request_authority_accepts_lan_host_when_in_allow_list() {
        let allowed = vec!["127.0.0.1".to_string(), "100.64.0.5".to_string()];
        let r = req_with(|r| {
            r.headers_mut()
                .insert(header::HOST, "100.64.0.5:7777".parse().unwrap());
        });
        assert!(request_authority_is_allowed(&r, &allowed));
    }

    #[test]
    fn build_allowed_hosts_loopback_bind_only_loopback() {
        let hosts = build_allowed_hosts("127.0.0.1");
        assert_eq!(hosts, vec!["127.0.0.1", "localhost", "::1"]);
    }

    #[test]
    fn build_allowed_hosts_specific_lan_ip() {
        let hosts = build_allowed_hosts("100.64.0.5");
        assert!(hosts.contains(&"100.64.0.5".to_string()));
        assert!(hosts.contains(&"127.0.0.1".to_string()));
    }

    #[test]
    fn local_browser_addr_unspecified_uses_loopback() {
        // --lan / --bind 0.0.0.0: browser must hit loopback so the Host
        // header passes the rebinding allow-list.
        assert_eq!(local_browser_addr("0.0.0.0", 7777), "127.0.0.1:7777");
        assert_eq!(local_browser_addr("::", 7777), "127.0.0.1:7777");
    }

    #[test]
    fn local_browser_addr_loopback_passes_through() {
        assert_eq!(local_browser_addr("127.0.0.1", 7777), "127.0.0.1:7777");
        assert_eq!(local_browser_addr("localhost", 7777), "localhost:7777");
    }

    #[test]
    fn local_browser_addr_specific_bind_passes_through() {
        // User picked the address explicitly; allow-list contains it.
        assert_eq!(local_browser_addr("192.168.1.5", 7777), "192.168.1.5:7777");
        assert_eq!(local_browser_addr("100.64.0.5", 7777), "100.64.0.5:7777");
    }

    #[test]
    fn process_tokens_are_fresh_fixed_width_hex() {
        let first = generate_token();
        let second = generate_token();
        assert_eq!(first.len(), 48);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn lock_ownership_is_pid_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon.lock");
        std::fs::write(&path, r#"{"pid":42}"#).unwrap();
        assert!(lock_owned_by(&path, 42));
        assert!(!lock_owned_by(&path, 43));
        std::fs::write(&path, "not json").unwrap();
        assert!(!lock_owned_by(&path, 42));
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_is_created_with_0o600_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("secret");
        write_secret_file(&path, b"hello").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_overwrites_loose_predecessor() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("secret");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_secret_file(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "rewritten file must inherit 0o600, was {mode:o}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}
