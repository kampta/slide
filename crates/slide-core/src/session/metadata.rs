use super::manager::valid_id;
use super::{Location, Session, SessionEvent};
use crate::backend::{self, ContextUsage};
use crate::store::Store;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

const DISCOVERY_DEADLINE: Duration = Duration::from_secs(120);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);

struct DiscoveryTask {
    generation: u64,
    abort: AbortHandle,
}

/// Provider-owned metadata that is independent from PTY lifecycle mechanics.
/// It centralizes query caching and guarantees at most one discovery worker
/// per Slide session.
pub(super) struct BackendMetadata {
    store: Arc<Store>,
    events: broadcast::Sender<Arc<SessionEvent>>,
    discovery_tasks: StdMutex<HashMap<String, DiscoveryTask>>,
    next_generation: AtomicU64,
}

impl BackendMetadata {
    pub(super) fn new(
        store: Arc<Store>,
        events: broadcast::Sender<Arc<SessionEvent>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            events,
            discovery_tasks: StdMutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
        })
    }

    pub(super) async fn context_usage(&self, id: &str) -> Option<ContextUsage> {
        if !valid_id(id) {
            return None;
        }
        let session = self.store.get(id).await.ok()??;
        if matches!(session.location, Location::Remote) || !session.backend.info().context_usage {
            return None;
        }
        let session_id = session.backend_session_id?;
        let cwd = PathBuf::from(session.project_path);
        tokio::task::spawn_blocking(move || {
            backend::for_kind(session.backend).read_context_usage(&cwd, &session_id)
        })
        .await
        .ok()
        .flatten()
    }

    pub(super) fn start_discovery(self: &Arc<Self>, session: &Session, since: SystemTime) {
        let provider = backend::for_kind(session.backend);
        if !matches!(session.location, Location::Local)
            || session.backend_session_id.is_some()
            || !provider.supports_session_discovery()
        {
            return;
        }

        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let metadata = self.clone();
        let id = session.id.clone();
        let task_id = id.clone();
        let cwd = PathBuf::from(&session.project_path);
        let backend_kind = session.backend;
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            let deadline = tokio::time::Instant::now() + DISCOVERY_DEADLINE;
            loop {
                let cwd = cwd.clone();
                let discovered = tokio::task::spawn_blocking(move || {
                    backend::for_kind(backend_kind).discover_session_id(&cwd, since)
                })
                .await
                .ok()
                .flatten();
                if let Some(provider_id) = discovered {
                    let updated = metadata
                        .store
                        .set_backend_session_id_if_current(&task_id, backend_kind, &provider_id)
                        .await
                        .unwrap_or(false);
                    if updated {
                        if let Ok(Some(session)) = metadata.store.get(&task_id).await {
                            let _ = metadata
                                .events
                                .send(Arc::new(SessionEvent::SessionUpdated { session }));
                        }
                    }
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(DISCOVERY_INTERVAL).await;
            }
            metadata.finish_discovery(&task_id, generation);
        });
        let replaced = self
            .discovery_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                id,
                DiscoveryTask {
                    generation,
                    abort: task.abort_handle(),
                },
            );
        if let Some(previous) = replaced {
            previous.abort.abort();
        }
        let _ = start_tx.send(());
    }

    pub(super) async fn clear_session(&self, id: &str) {
        self.cancel_discovery(id);
    }

    pub(super) fn cancel_discovery(&self, id: &str) {
        if let Some(task) = self
            .discovery_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id)
        {
            task.abort.abort();
        }
    }

    fn finish_discovery(&self, id: &str, generation: u64) {
        let mut tasks = self
            .discovery_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if tasks
            .get(id)
            .is_some_and(|task| task.generation == generation)
        {
            tasks.remove(id);
        }
    }
}
