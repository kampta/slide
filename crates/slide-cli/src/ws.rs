use crate::server::{request_is_authenticated, AppState, SAFE_PROTO, WS_CLOSE_AUTH_FAILED};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::stream::{SplitSink, SplitStream};
use futures::{sink::SinkExt, stream::StreamExt};
use serde_json::json;
use slide_core::session::manager::TerminalAttachment;
use tokio::sync::broadcast::error::RecvError;

const MAX_TERMINAL_DIMENSION: u16 = 1000;
// Keep large pastes practical without inheriting Tungstenite's 64 MiB default.
const MAX_CLIENT_MESSAGE_BYTES: usize = 1024 * 1024;
const TERMINAL_ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const TERMINAL_HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

type SocketSender = SplitSink<WebSocket, Message>;
type SocketReceiver = SplitStream<WebSocket>;

fn terminal_dimension(value: Option<&serde_json::Value>) -> Option<u16> {
    value
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .filter(|n| (1..=MAX_TERMINAL_DIMENSION).contains(n))
}

fn terminal_hello(text: &str) -> Option<(u16, u16)> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if !matches!(
        value.get("type").and_then(|kind| kind.as_str()),
        Some("hello" | "resize")
    ) {
        return None;
    }
    Some((
        terminal_dimension(value.get("cols"))?,
        terminal_dimension(value.get("rows"))?,
    ))
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

async fn send_terminal_error(tx: &mut SocketSender, error: &str) {
    let _ = tx
        .send(Message::Text(
            json!({ "type": "error", "error": error }).to_string(),
        ))
        .await;
}

async fn send_terminal_ready(tx: &mut SocketSender) -> bool {
    tx.send(Message::Text(json!({ "type": "ready" }).to_string()))
        .await
        .is_ok()
}

async fn receive_terminal_hello(
    tx: &mut SocketSender,
    rx: &mut SocketReceiver,
) -> Option<(u16, u16)> {
    let deadline = tokio::time::Instant::now() + TERMINAL_HELLO_TIMEOUT;
    loop {
        let message = match tokio::time::timeout_at(deadline, rx.next()).await {
            Ok(Some(Ok(message))) => message,
            _ => {
                send_terminal_error(tx, "terminal hello required").await;
                return None;
            }
        };
        match message {
            Message::Text(text) => match terminal_hello(&text) {
                Some(size) => return Some(size),
                None => {
                    send_terminal_error(tx, "invalid terminal hello").await;
                    return None;
                }
            },
            Message::Ping(payload) => {
                if tx.send(Message::Pong(payload)).await.is_err() {
                    return None;
                }
            }
            Message::Close(_) => return None,
            _ => {
                send_terminal_error(tx, "terminal hello must be the first message").await;
                return None;
            }
        }
    }
}

pub async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let auth_ok = request_is_authenticated(&headers, &state);
    ws.max_message_size(MAX_CLIENT_MESSAGE_BYTES)
        .max_frame_size(MAX_CLIENT_MESSAGE_BYTES)
        .protocols([SAFE_PROTO])
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
    let auth_ok = request_is_authenticated(&headers, &state);
    ws.max_message_size(MAX_CLIENT_MESSAGE_BYTES)
        .max_frame_size(MAX_CLIENT_MESSAGE_BYTES)
        .protocols([SAFE_PROTO])
        .on_upgrade(move |sock| async move {
            if !auth_ok {
                close_with_auth_failed(sock).await;
                return;
            }
            handle_session(sock, state, id).await;
        })
}

async fn handle_session(socket: WebSocket, state: AppState, id: String) {
    let Ok(_permit) = state.terminal_slots.clone().try_acquire_owned() else {
        let (mut tx, _) = socket.split();
        send_terminal_error(&mut tx, "too many terminal connections; close another tab").await;
        return;
    };
    let (mut tx, mut rx) = socket.split();
    let Some((cols, rows)) = receive_terminal_hello(&mut tx, &mut rx).await else {
        return;
    };

    match state.manager.attach_terminal(&id, cols, rows).await {
        Ok(Some(attachment)) => {
            handle_dedicated_terminal(tx, rx, attachment).await;
        }
        Ok(None) => {
            handle_shared_terminal(tx, rx, state, id, cols, rows).await;
        }
        Err(error) => {
            send_terminal_error(&mut tx, &error.to_string()).await;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ClientEvent {
    Input(Vec<u8>),
    Resize(u16, u16),
    NativePing(Vec<u8>),
    AppPing,
    Close,
    Ignore,
}

fn client_event(message: Option<Result<Message, axum::Error>>) -> ClientEvent {
    match message {
        Some(Ok(Message::Binary(bytes))) => ClientEvent::Input(bytes),
        Some(Ok(Message::Text(text))) => {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                return ClientEvent::Ignore;
            };
            match value.get("type").and_then(|kind| kind.as_str()) {
                Some("resize") => match (
                    terminal_dimension(value.get("cols")),
                    terminal_dimension(value.get("rows")),
                ) {
                    (Some(cols), Some(rows)) => ClientEvent::Resize(cols, rows),
                    _ => ClientEvent::Ignore,
                },
                Some("ping") => ClientEvent::AppPing,
                _ => ClientEvent::Ignore,
            }
        }
        Some(Ok(Message::Ping(payload))) => ClientEvent::NativePing(payload),
        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => ClientEvent::Close,
        _ => ClientEvent::Ignore,
    }
}

async fn send_pong(tx: &mut SocketSender, event: &ClientEvent) -> bool {
    match event {
        ClientEvent::NativePing(payload) => tx.send(Message::Pong(payload.clone())).await.is_ok(),
        ClientEvent::AppPing => tx
            .send(Message::Text(json!({ "type": "pong" }).to_string()))
            .await
            .is_ok(),
        _ => true,
    }
}

async fn handle_dedicated_terminal(
    mut tx: SocketSender,
    mut rx: SocketReceiver,
    mut terminal: TerminalAttachment,
) {
    // A successful tmux attachment immediately emits its initial redraw.
    // Keep that first chunk buffered until `ready` so the frontend resets
    // only after an output stream exists, without losing any pane bytes.
    let first = match tokio::time::timeout(TERMINAL_ATTACH_TIMEOUT, terminal.recv()).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            send_terminal_error(&mut tx, "terminal attachment closed before it was ready").await;
            return;
        }
        Err(_) => {
            send_terminal_error(&mut tx, "terminal attachment timed out").await;
            return;
        }
    };
    if !send_terminal_ready(&mut tx).await
        || tx.send(Message::Binary(first.to_vec())).await.is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            output = terminal.recv() => match output {
                Some(bytes) if tx.send(Message::Binary(bytes.to_vec())).await.is_ok() => {}
                _ => break,
            },
            message = rx.next() => {
                let event = client_event(message);
                match event {
                    ClientEvent::Input(ref bytes) if terminal.write(bytes).is_err() => break,
                    ClientEvent::Resize(cols, rows) if terminal.resize(cols, rows).is_err() => break,
                    event @ (ClientEvent::NativePing(_) | ClientEvent::AppPing)
                        if !send_pong(&mut tx, &event).await => break,
                    ClientEvent::NativePing(_) | ClientEvent::AppPing => {}
                    ClientEvent::Close => break,
                    _ => {}
                }
            }
        }
    }
}

async fn handle_shared_terminal(
    mut tx: SocketSender,
    mut rx: SocketReceiver,
    state: AppState,
    id: String,
    cols: u16,
    rows: u16,
) {
    // Direct supervision owns one PTY, so all attached browser sizes are
    // reconciled by SessionManager. Tmux sessions never take this path.
    let client_id = state.manager.next_client_id();
    let _ = state
        .manager
        .set_client_size(&id, client_id, cols, rows)
        .await;
    handle_shared_terminal_inner(&mut tx, &mut rx, &state, &id, client_id).await;
    // This is the only post-registration exit path. `forget_client` safely
    // no-ops if registration never happened and also covers resize failures
    // that inserted the client before the underlying PTY resize failed.
    state.manager.forget_client(&id, client_id).await;
}

async fn handle_shared_terminal_inner(
    tx: &mut SocketSender,
    rx: &mut SocketReceiver,
    state: &AppState,
    id: &str,
    client_id: u64,
) {
    // Atomic snapshot + subscribe: the manager holds the ring lock across
    // both, so the snapshot we send below contains every byte broadcast up
    // to that instant, and the subscriber sees only bytes after — no gap,
    // no duplicate. Falling back to the on-disk log when the session isn't
    // running is safe because the log file is immutable in that state.
    let (snapshot, mut output) = match state.manager.subscribe_output_with_snapshot(id).await {
        Some(pair) => pair,
        None => {
            match state.manager.get_log(id).await {
                Ok(bytes) => {
                    if !send_terminal_ready(tx).await {
                        return;
                    }
                    if !bytes.is_empty() && tx.send(Message::Binary(bytes)).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    send_terminal_error(tx, &error.to_string()).await;
                    return;
                }
            }
            send_terminal_error(tx, "session not running").await;
            return;
        }
    };

    if !send_terminal_ready(tx).await {
        return;
    }
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
                        .subscribe_output_with_snapshot(id)
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
            message = rx.next() => {
                let event = client_event(message);
                match event {
                    ClientEvent::Input(ref bytes) => {
                        if state.manager.write_input(id, bytes).await.is_err() { break; }
                    }
                    ClientEvent::Resize(cols, rows) => {
                        if state.manager.set_client_size(id, client_id, cols, rows).await.is_err() {
                            break;
                        }
                    }
                    event @ (ClientEvent::NativePing(_) | ClientEvent::AppPing)
                        if !send_pong(tx, &event).await => break,
                    ClientEvent::NativePing(_) | ClientEvent::AppPing => {}
                    ClientEvent::Close => break,
                    ClientEvent::Ignore => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{client_event, terminal_dimension, terminal_hello, ClientEvent};
    use axum::extract::ws::Message;
    use serde_json::json;

    #[test]
    fn terminal_dimensions_reject_zero_and_overflow() {
        assert_eq!(terminal_dimension(Some(&json!(80))), Some(80));
        assert_eq!(terminal_dimension(Some(&json!(0))), None);
        assert_eq!(terminal_dimension(Some(&json!(1001))), None);
        assert_eq!(terminal_dimension(Some(&json!(70_000))), None);
        assert_eq!(terminal_dimension(Some(&json!("80"))), None);
    }

    #[test]
    fn initial_size_accepts_hello_and_legacy_resize() {
        assert_eq!(
            terminal_hello(r#"{"type":"hello","cols":120,"rows":40}"#),
            Some((120, 40))
        );
        assert_eq!(
            terminal_hello(r#"{"type":"resize","cols":120,"rows":40}"#),
            Some((120, 40))
        );
        assert_eq!(
            terminal_hello(r#"{"type":"hello","cols":0,"rows":40}"#),
            None
        );
        assert_eq!(terminal_hello(r#"{"type":"hello","cols":120}"#), None);
    }

    #[test]
    fn client_controls_cover_input_resize_and_ping() {
        assert_eq!(
            client_event(Some(Ok(Message::Binary(vec![1, 2, 3])))),
            ClientEvent::Input(vec![1, 2, 3])
        );
        assert_eq!(
            client_event(Some(Ok(Message::Text(
                r#"{"type":"resize","cols":93,"rows":27}"#.to_string()
            )))),
            ClientEvent::Resize(93, 27)
        );
        assert_eq!(
            client_event(Some(Ok(Message::Text(r#"{"type":"ping"}"#.to_string())))),
            ClientEvent::AppPing
        );
        assert_eq!(
            client_event(Some(Ok(Message::Ping(vec![4, 5])))),
            ClientEvent::NativePing(vec![4, 5])
        );
    }
}
