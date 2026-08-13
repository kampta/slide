use super::manager::{now_ms, SessionManager};
use super::pty::Pty;
use super::{Session, SessionState, SupervisorKind};
use crate::backend;
use crate::classifier;
use crate::supervisor::WritesLog;
use anyhow::Result;
use bytes::Bytes;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc, Mutex, Notify};
use tokio::task::JoinHandle;

const RING_CAP: usize = 2 * 1024 * 1024;
const TAIL_SNIFF: usize = 4 * 1024;
const BROADCAST_CAP: usize = 256;
const UNKNOWN_RECHECK_INITIAL: Duration = Duration::from_secs(5);
const UNKNOWN_RECHECK_MAX: Duration = Duration::from_secs(30);

/// Bounded terminal history without repeatedly shifting a full buffer when
/// old output expires.
struct ByteRing {
    bytes: VecDeque<u8>,
}

impl ByteRing {
    fn new() -> Self {
        Self {
            bytes: VecDeque::with_capacity(8192),
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let overflow = self.bytes.len().saturating_sub(RING_CAP);
        if overflow > 0 {
            self.bytes.drain(..overflow);
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }

    fn tail(&self, len: usize) -> Vec<u8> {
        self.bytes
            .iter()
            .skip(self.bytes.len().saturating_sub(len))
            .copied()
            .collect()
    }
}

/// All live resources for one attached session. Removing this value from the
/// manager stops classification immediately; the PTY reader exits when its
/// channel closes.
pub(super) struct RunningSession {
    pty: Pty,
    supervisor: SupervisorKind,
    output_tx: broadcast::Sender<Bytes>,
    ring: Arc<Mutex<ByteRing>>,
    classifier_handle: JoinHandle<()>,
    client_sizes: Mutex<HashMap<u64, (u16, u16)>>,
}

impl RunningSession {
    pub(super) async fn start(
        manager: Arc<SessionManager>,
        session: &Session,
        pty: Pty,
        output: mpsc::Receiver<Bytes>,
        writes_log: WritesLog,
        log_path: &Path,
    ) -> Arc<Self> {
        let (output_tx, _) = broadcast::channel(BROADCAST_CAP);
        let ring = Arc::new(Mutex::new(ByteRing::new()));
        let last_activity = Arc::new(AtomicI64::new(now_ms()));
        let activity_notify = Arc::new(Notify::new());
        let classifier_handle = tokio::spawn(classifier_task(ClassifierCtx {
            manager,
            id: session.id.clone(),
            backend: session.backend,
            supervisor: session.supervisor,
            ssh_host: session.ssh_host.clone(),
            initial_state: session.state,
            last_activity: last_activity.clone(),
            activity_notify: activity_notify.clone(),
            ring: ring.clone(),
        }));

        let running = Arc::new(Self {
            pty,
            supervisor: session.supervisor,
            output_tx: output_tx.clone(),
            ring: ring.clone(),
            classifier_handle,
            client_sizes: Mutex::new(HashMap::new()),
        });

        let log_file = if matches!(writes_log, WritesLog::Daemon) {
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
                .await
                .ok()
        } else {
            None
        };
        spawn_output_reader(
            output,
            log_file,
            ring,
            output_tx,
            last_activity,
            activity_notify,
        );
        running
    }

    pub(super) fn supervisor(&self) -> SupervisorKind {
        self.supervisor
    }

    pub(super) fn write(&self, bytes: &[u8]) -> Result<()> {
        self.pty.write(bytes)
    }

    pub(super) fn kill(&self) {
        self.classifier_handle.abort();
        self.pty.kill();
    }

    pub(super) async fn snapshot(&self) -> Vec<u8> {
        self.ring.lock().await.snapshot()
    }

    pub(super) async fn tail(&self, len: usize) -> Vec<u8> {
        self.ring.lock().await.tail(len)
    }

    pub(super) async fn subscribe_with_snapshot(&self) -> (Vec<u8>, broadcast::Receiver<Bytes>) {
        let ring = self.ring.lock().await;
        let snapshot = ring.snapshot();
        let receiver = self.output_tx.subscribe();
        (snapshot, receiver)
    }

    pub(super) async fn set_client_size(&self, client_id: u64, cols: u16, rows: u16) -> Result<()> {
        let mut sizes = self.client_sizes.lock().await;
        sizes.insert(client_id, (cols, rows));
        if let Some((cols, rows)) = effective_min_size(&sizes) {
            self.pty.resize(cols, rows)?;
        }
        Ok(())
    }

    pub(super) async fn forget_client(&self, client_id: u64) {
        let mut sizes = self.client_sizes.lock().await;
        if sizes.remove(&client_id).is_none() {
            return;
        }
        if let Some((cols, rows)) = effective_min_size(&sizes) {
            let _ = self.pty.resize(cols, rows);
        }
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        self.classifier_handle.abort();
    }
}

fn spawn_output_reader(
    mut output: mpsc::Receiver<Bytes>,
    mut log_file: Option<tokio::fs::File>,
    ring: Arc<Mutex<ByteRing>>,
    output_tx: broadcast::Sender<Bytes>,
    last_activity: Arc<AtomicI64>,
    activity_notify: Arc<Notify>,
) {
    tokio::spawn(async move {
        while let Some(chunk) = output.recv().await {
            last_activity.store(now_ms(), Ordering::SeqCst);
            {
                let mut ring = ring.lock().await;
                ring.push(&chunk);
                let _ = output_tx.send(chunk.clone());
            }
            if let Some(file) = log_file.as_mut() {
                let _ = file.write_all(&chunk).await;
            }
            activity_notify.notify_one();
        }
    });
}

struct ClassifierCtx {
    manager: Arc<SessionManager>,
    id: String,
    backend: crate::backend::BackendKind,
    supervisor: SupervisorKind,
    ssh_host: Option<String>,
    initial_state: SessionState,
    last_activity: Arc<AtomicI64>,
    activity_notify: Arc<Notify>,
    ring: Arc<Mutex<ByteRing>>,
}

async fn classifier_task(ctx: ClassifierCtx) {
    let signals = backend::for_kind(ctx.backend).signals();
    let mut last_state = ctx.initial_state;
    let mut unknown_recheck = UNKNOWN_RECHECK_INITIAL;

    loop {
        let activity = ctx.last_activity.load(Ordering::SeqCst);
        let elapsed = now_ms().saturating_sub(activity);
        // Raw byte arrival is sufficient to classify a session Active. Avoid
        // spawning tmux/SSH capture commands for every burst; wait until the
        // backend has settled before inspecting the rendered prompt.
        if elapsed < signals.settle_ms as i64 {
            if last_state != SessionState::Active {
                ctx.manager
                    .persist_classification(&ctx.id, SessionState::Active, activity)
                    .await;
                last_state = SessionState::Active;
            }
            unknown_recheck = UNKNOWN_RECHECK_INITIAL;
            let remaining = (signals.settle_ms as i64 - elapsed).max(1) as u64;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(remaining)) => {}
                _ = ctx.activity_notify.notified() => {}
            }
            continue;
        }
        let pane = match ctx.supervisor {
            SupervisorKind::Tmux => {
                let host = ctx.ssh_host.clone();
                let id = ctx.id.clone();
                tokio::task::spawn_blocking(move || crate::tmux::capture_pane(host.as_deref(), &id))
                    .await
                    .ok()
                    .and_then(Result::ok)
            }
            SupervisorKind::Direct => {
                let ring = ctx.ring.lock().await;
                let tail = ring.tail(TAIL_SNIFF);
                Some(crate::terminal_text::strip_ansi(&String::from_utf8_lossy(
                    &tail,
                )))
            }
        };
        let classification = pane.map_or(
            classifier::Classification {
                state: SessionState::Unknown,
                reason: classifier::ClassificationReason::CaptureFailed,
            },
            |pane| {
                classifier::classify(
                    &classifier::Snapshot {
                        pane: &pane,
                        idle_ms: elapsed,
                    },
                    signals,
                )
            },
        );
        let desired = classification.state;
        if desired != last_state {
            ctx.manager
                .persist_classification(&ctx.id, desired, activity)
                .await;
            tracing::debug!(
                session = %ctx.id,
                state = desired.as_str(),
                reason = ?classification.reason,
                "classified session state"
            );
            last_state = desired;
        }

        if desired == SessionState::Unknown {
            let delay = unknown_recheck;
            unknown_recheck = std::cmp::min(unknown_recheck * 2, UNKNOWN_RECHECK_MAX);
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = ctx.activity_notify.notified() => unknown_recheck = UNKNOWN_RECHECK_INITIAL,
            }
        } else {
            unknown_recheck = UNKNOWN_RECHECK_INITIAL;
            ctx.activity_notify.notified().await;
        }
    }
}

fn effective_min_size(sizes: &HashMap<u64, (u16, u16)>) -> Option<(u16, u16)> {
    let mut values = sizes.values().copied();
    let first = values.next()?;
    Some(values.fold(first, |(cols, rows), (next_cols, next_rows)| {
        (cols.min(next_cols), rows.min(next_rows))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_only_newest_bytes() {
        let mut ring = ByteRing::new();
        ring.push(&vec![b'a'; RING_CAP]);
        ring.push(b"bc");

        let snapshot = ring.snapshot();
        assert_eq!(snapshot.len(), RING_CAP);
        assert_eq!(&snapshot[RING_CAP - 2..], b"bc");
        assert_eq!(ring.tail(3), b"abc");
    }

    #[test]
    fn minimum_size_uses_each_axis() {
        let sizes = HashMap::from([(1, (200, 30)), (2, (60, 80)), (3, (120, 40))]);
        assert_eq!(effective_min_size(&sizes), Some((60, 30)));
        assert_eq!(effective_min_size(&HashMap::new()), None);
    }
}
