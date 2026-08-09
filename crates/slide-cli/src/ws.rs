use crate::server::{
    constant_time_eq, token_from_headers, AppState, SAFE_PROTO, WS_CLOSE_AUTH_FAILED,
};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::{sink::SinkExt, stream::StreamExt};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

const MAX_TERMINAL_DIMENSION: u16 = 1000;

fn terminal_dimension(value: Option<&serde_json::Value>, fallback: u16) -> u16 {
    value
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .filter(|n| (1..=MAX_TERMINAL_DIMENSION).contains(n))
        .unwrap_or(fallback)
}

fn ws_token_ok(headers: &HeaderMap, expected: &str) -> bool {
    match token_from_headers(headers) {
        Some(supplied) => constant_time_eq(supplied.as_bytes(), expected.as_bytes()),
        None => false,
    }
}

async fn close_with_auth_failed(mut sock: WebSocket) {
    // Best-effort: if the peer already closed we just drop the socket.
    let _ = sock
        .send(Message::Close(Some(CloseFrame {
            code: WS_CLOSE_AUTH_FAILED,
            reason: "stale or missing bearer token".into(),
        })))
        .await;
}

pub async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let auth_ok = ws_token_ok(&headers, &state.token);
    ws.protocols([SAFE_PROTO])
        .on_upgrade(move |sock| async move {
            if !auth_ok {
                close_with_auth_failed(sock).await;
                return;
            }
            handle_events(sock, state).await;
        })
}

async fn handle_events(socket: WebSocket, state: AppState) {
    let (mut tx, mut rx) = socket.split();
    let mut sub = state.manager.subscribe_events();

    // Initial snapshot so new clients aren't empty until an event fires.
    if let Ok(sessions) = state.manager.list().await {
        let msg = json!({ "type": "snapshot", "sessions": sessions });
        if tx.send(Message::Text(msg.to_string())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            msg = sub.recv() => match msg {
                Ok(ev) => {
                    // /ws/events only carries lifecycle metadata. High-volume
                    // PTY output goes on the per-session output_tx channel
                    // and never enters this broadcast.
                    let payload = serde_json::to_string(&*ev).unwrap_or_default();
                    if tx.send(Message::Text(payload)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // Lifecycle events are snapshots of current state, so a
                    // client that falls behind can recover with one full list.
                    let Ok(sessions) = state.manager.list().await else { continue };
                    let snapshot = json!({ "type": "snapshot", "sessions": sessions });
                    if tx.send(Message::Text(snapshot.to_string())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            client = rx.next() => match client {
                Some(Ok(Message::Ping(p))) => {
                    let _ = tx.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            }
        }
    }
}

pub async fn session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let auth_ok = ws_token_ok(&headers, &state.token);
    ws.protocols([SAFE_PROTO])
        .on_upgrade(move |sock| async move {
            if !auth_ok {
                close_with_auth_failed(sock).await;
                return;
            }
            handle_session(sock, state, id).await;
        })
}

#[allow(clippy::collapsible_match)]
async fn handle_session(socket: WebSocket, state: AppState, id: String) {
    let (mut tx, mut rx) = socket.split();
    // Per-WS id so SessionManager can track each attached client's
    // viewport size independently. Without this, two clients on the
    // same session each set the PTY to their own dimensions and
    // trample each other's render.
    let client_id = state.manager.next_client_id();

    // Atomic snapshot + subscribe: the manager holds the ring lock across
    // both, so the snapshot we send below contains every byte broadcast up
    // to that instant, and the subscriber sees only bytes after — no gap,
    // no duplicate. Falling back to the on-disk log when the session isn't
    // running is safe because the log file is immutable in that state.
    let (snapshot, mut output) = match state.manager.subscribe_output_with_snapshot(&id).await {
        Some(pair) => pair,
        None => {
            if let Ok(bytes) = state.manager.get_log(&id).await {
                if !bytes.is_empty() && tx.send(Message::Binary(bytes)).await.is_err() {
                    return;
                }
            }
            let _ = tx
                .send(Message::Text(
                    json!({ "type": "error", "error": "session not running" }).to_string(),
                ))
                .await;
            return;
        }
    };

    if !snapshot.is_empty() && tx.send(Message::Binary(snapshot)).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            msg = output.recv() => match msg {
                Ok(bytes) => {
                    // axum 0.7's `Message::Binary` is `Vec<u8>`; the to_vec
                    // here is the only per-subscriber copy in the path.
                    // Until we move to axum 0.8 (which takes `Bytes`
                    // directly) this is an unavoidable boundary.
                    if tx.send(Message::Binary(bytes.to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // A slow client missed output chunks. Reset it from an
                    // atomic ring snapshot instead of leaving a permanent
                    // hole in the terminal stream.
                    let Some((fresh_snapshot, fresh_output)) = state
                        .manager
                        .subscribe_output_with_snapshot(&id)
                        .await
                    else {
                        break;
                    };
                    if tx
                        .send(Message::Text(json!({ "type": "terminal_reset" }).to_string()))
                        .await
                        .is_err()
                        || tx.send(Message::Binary(fresh_snapshot)).await.is_err()
                    {
                        break;
                    }
                    output = fresh_output;
                }
                Err(RecvError::Closed) => break,
            },
            client = rx.next() => match client {
                Some(Ok(Message::Binary(data))) => {
                    if state.manager.write_input(&id, &data).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    // JSON control messages: {"type":"resize","cols":120,"rows":40}
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        match v.get("type").and_then(|t| t.as_str()) {
                            Some("resize") => {
                                let cols = terminal_dimension(v.get("cols"), 120);
                                let rows = terminal_dimension(v.get("rows"), 40);
                                let _ = state.manager.set_client_size(&id, client_id, cols, rows).await;
                            }
                            Some("input") => {
                                if let Some(s) = v.get("bytes").and_then(|x| x.as_str()) {
                                    let _ = state.manager.write_input(&id, s.as_bytes()).await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some(Ok(Message::Ping(p))) => { let _ = tx.send(Message::Pong(p)).await; }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                _ => {}
            }
        }
    }
    // Drop this client's recorded viewport so the PTY can grow back to
    // whatever the remaining attached clients can accommodate.
    state.manager.forget_client(&id, client_id).await;
}

#[cfg(test)]
mod tests {
    use super::terminal_dimension;
    use serde_json::json;

    #[test]
    fn terminal_dimensions_reject_zero_and_overflow() {
        assert_eq!(terminal_dimension(Some(&json!(80)), 120), 80);
        assert_eq!(terminal_dimension(Some(&json!(0)), 120), 120);
        assert_eq!(terminal_dimension(Some(&json!(1001)), 120), 120);
        assert_eq!(terminal_dimension(Some(&json!(70_000)), 120), 120);
        assert_eq!(terminal_dimension(Some(&json!("80")), 120), 120);
    }
}
