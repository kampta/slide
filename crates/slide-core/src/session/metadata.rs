use super::manager::{check_id, valid_id};
use super::{Location, Session, SessionEvent};
use crate::backend::{self, ContextUsage, SubagentList};
use crate::store::Store;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, RwLock};
use tokio::task::AbortHandle;

const SUBAGENT_CACHE_TTL: Duration = Duration::from_secs(3);
const DISCOVERY_DEADLINE: Duration = Duration::from_secs(120);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct CachedSubagents {
    fetched_at: Instant,
    value: SubagentList,
}

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
    subagent_cache: RwLock<HashMap<String, CachedSubagents>>,
    subagent_query_locks: StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
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
            subagent_cache: RwLock::new(HashMap::new()),
            subagent_query_locks: StdMutex::new(HashMap::new()),
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

    pub(super) async fn subagents(&self, id: &str) -> Result<SubagentList> {
        check_id(id)?;
        if let Some(cached) = self.cached_subagents(id).await {
            return Ok(cached);
        }
        let query_lock = self
            .subagent_query_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _query_guard = query_lock.lock().await;
        if let Some(cached) = self.cached_subagents(id).await {
            return Ok(cached);
        }

        let session = self
            .store
            .get(id)
            .await?
            .with_context(|| format!("unknown session {id}"))?;
        if !session.backend.info().subagents {
            return Ok(SubagentList {
                supported: false,
                agents: Vec::new(),
            });
        }
        let Some(session_id) = session.backend_session_id.clone() else {
            return Ok(SubagentList {
                supported: true,
                agents: Vec::new(),
            });
        };
        let cwd = PathBuf::from(session.project_path);
        let ssh_host = session.ssh_host.clone();
        let backend_kind = session.backend;
        let result = tokio::task::spawn_blocking(move || {
            backend::for_kind(backend_kind).read_subagents(&cwd, &session_id, ssh_host.as_deref())
        })
        .await
        .context("join subagent metadata query")??;
        let value = result.map_or(
            SubagentList {
                supported: false,
                agents: Vec::new(),
            },
            |agents| SubagentList {
                supported: true,
                agents,
            },
        );
        if self.store.get(id).await?.is_some() {
            self.subagent_cache.write().await.insert(
                id.to_string(),
                CachedSubagents {
                    fetched_at: Instant::now(),
                    value: value.clone(),
                },
            );
        }
        Ok(value)
    }

    pub(super) fn start_discovery(self: &Arc<Self>, session: &Session) {
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
        let since = std::time::UNIX_EPOCH + Duration::from_millis(session.created_at.max(0) as u64);
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
        self.subagent_cache.write().await.remove(id);
        self.subagent_query_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
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

    async fn cached_subagents(&self, id: &str) -> Option<SubagentList> {
        self.subagent_cache
            .read()
            .await
            .get(id)
            .filter(|cached| cached.fetched_at.elapsed() < SUBAGENT_CACHE_TTL)
            .map(|cached| cached.value.clone())
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
