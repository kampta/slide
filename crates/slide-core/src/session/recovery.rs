use super::manager::{now_ms, SessionManager, SpawnIntent};
use super::running::RunningSession;
use super::{Location, Session, SessionEvent, SessionState, SupervisorKind};
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const COLD_START_PROBE_CONCURRENCY: usize = 8;
const ATTACH_RETRY_INITIAL: Duration = Duration::from_secs(1);
const ATTACH_RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TmuxExitAction {
    Reattach,
    Stop,
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColdStartStatus {
    MissingWorktree,
    Probe(crate::tmux::SessionProbe),
}

/// Owns daemon-start reconciliation and recovery of tmux attach processes.
/// The session manager remains the lifecycle API; this type contains the
/// retry policy and the distinction between transport loss and backend exit.
pub(super) struct RecoveryCoordinator;

impl RecoveryCoordinator {
    pub(super) async fn reconcile(manager: &Arc<SessionManager>) -> Result<()> {
        let mut survivors = Vec::new();
        let mut deferred = Vec::new();
        let mut sessions = manager
            .store
            .list()
            .await?
            .into_iter()
            .filter(|session| !matches!(session.state, SessionState::Stopped));
        let mut probes = tokio::task::JoinSet::new();
        for session in sessions.by_ref().take(COLD_START_PROBE_CONCURRENCY) {
            probes.spawn(inspect_cold_session(session));
        }
        while let Some(result) = probes.join_next().await {
            if let Some(session) = sessions.next() {
                probes.spawn(inspect_cold_session(session));
            }
            let Ok((session, status)) = result else {
                tracing::warn!("cold-start session probe task failed");
                continue;
            };
            match status {
                ColdStartStatus::MissingWorktree => {
                    tracing::info!(
                        session = %session.id,
                        path = %session.project_path,
                        "project path missing on cold start; marking stopped",
                    );
                    let _ = manager
                        .store
                        .update_state(&session.id, SessionState::Stopped, now_ms())
                        .await;
                }
                ColdStartStatus::Probe(crate::tmux::SessionProbe::Present) => {
                    survivors.push(session);
                }
                ColdStartStatus::Probe(crate::tmux::SessionProbe::Absent) => {
                    let _ = manager
                        .store
                        .update_state(&session.id, SessionState::Stopped, now_ms())
                        .await;
                }
                ColdStartStatus::Probe(crate::tmux::SessionProbe::Unreachable) => {
                    tracing::info!(
                        session = %session.id,
                        host = ?session.ssh_host,
                        "host unreachable at cold start; deferring reattach",
                    );
                    manager
                        .persist_unattached_state(
                            &session,
                            state_after_failed_reattach(crate::tmux::SessionProbe::Unreachable),
                        )
                        .await;
                    deferred.push(session);
                }
            }
        }

        if !survivors.is_empty() {
            let manager = manager.clone();
            tokio::spawn(async move { Self::reattach_survivors(manager, survivors).await });
        }
        if !deferred.is_empty() {
            let manager = manager.clone();
            tokio::spawn(async move { Self::retry_deferred(manager, deferred).await });
        }
        Ok(())
    }

    pub(super) async fn handle_exit(
        manager: Arc<SessionManager>,
        id: String,
        running: Arc<RunningSession>,
        code: Option<i32>,
    ) {
        let operation = manager.operation_lock(&id);
        let guard = operation.lock().await;
        let removed_current = {
            let mut running_map = manager.running.write().await;
            remove_if_current(&mut running_map, &id, &running)
        };
        if !removed_current {
            return;
        }
        drop(running);

        let session = match manager.find(&id).await {
            Ok(session) => session,
            Err(_) => return,
        };
        if !matches!(session.supervisor, SupervisorKind::Tmux) {
            Self::mark_exited(&manager, &id, code).await;
            return;
        }

        drop(guard);
        Self::recover_tmux_attachment(manager, id, code).await;
    }

    async fn reattach_survivors(manager: Arc<SessionManager>, survivors: Vec<Session>) {
        for session in survivors {
            let operation = manager.operation_lock(&session.id);
            let _guard = operation.lock().await;
            let session = match manager.find(&session.id).await {
                Ok(current)
                    if !matches!(current.state, SessionState::Stopped)
                        && !manager.running.read().await.contains_key(&current.id) =>
                {
                    current
                }
                _ => continue,
            };
            if let Err(error) = manager.spawn_process(&session, SpawnIntent::Existing).await {
                tracing::warn!(
                    session = %session.id,
                    error = %format!("{error:#}"),
                    "reattach failed; retrying"
                );
                manager
                    .persist_unattached_state(
                        &session,
                        state_after_failed_reattach(crate::tmux::SessionProbe::Present),
                    )
                    .await;
                let manager = manager.clone();
                let id = session.id.clone();
                tokio::spawn(async move {
                    Self::recover_tmux_attachment(manager, id, None).await;
                });
            }
        }
    }

    async fn retry_deferred(manager: Arc<SessionManager>, mut pending: Vec<Session>) {
        let mut delay = Duration::from_secs(30);
        let max_delay = Duration::from_secs(300);
        while !pending.is_empty() {
            tokio::time::sleep(delay).await;
            let mut still_pending = Vec::new();
            let mut resolved_any = false;
            for prior in pending.drain(..) {
                let operation = manager.operation_lock(&prior.id);
                let _guard = operation.lock().await;
                let session = match manager.find(&prior.id).await {
                    Ok(session)
                        if !matches!(session.state, SessionState::Stopped)
                            && !manager.running.read().await.contains_key(&session.id) =>
                    {
                        session
                    }
                    _ => {
                        resolved_any = true;
                        continue;
                    }
                };
                let probe = probe_tmux(&session).await;
                match probe {
                    crate::tmux::SessionProbe::Present => {
                        resolved_any = true;
                        if let Err(error) =
                            manager.spawn_process(&session, SpawnIntent::Existing).await
                        {
                            tracing::warn!(
                                session = %session.id,
                                error = %format!("{error:#}"),
                                "deferred reattach failed; retrying",
                            );
                            manager
                                .persist_unattached_state(
                                    &session,
                                    state_after_failed_reattach(crate::tmux::SessionProbe::Present),
                                )
                                .await;
                            still_pending.push(session);
                        } else {
                            manager.emit(SessionEvent::SessionUpdated {
                                session: session.clone(),
                            });
                        }
                    }
                    crate::tmux::SessionProbe::Absent => {
                        resolved_any = true;
                        let _ = manager
                            .store
                            .update_state(&session.id, SessionState::Stopped, now_ms())
                            .await;
                        manager.emit(SessionEvent::SessionState {
                            id: session.id,
                            state: SessionState::Stopped,
                        });
                    }
                    crate::tmux::SessionProbe::Unreachable => {
                        manager
                            .persist_unattached_state(
                                &session,
                                state_after_failed_reattach(crate::tmux::SessionProbe::Unreachable),
                            )
                            .await;
                        still_pending.push(session);
                    }
                }
            }
            pending = still_pending;
            delay = if resolved_any {
                Duration::from_secs(30)
            } else {
                std::cmp::min(delay * 2, max_delay)
            };
        }
    }

    async fn recover_tmux_attachment(manager: Arc<SessionManager>, id: String, code: Option<i32>) {
        let mut delay = ATTACH_RETRY_INITIAL;
        tokio::time::sleep(delay).await;
        loop {
            let operation = manager.operation_lock(&id);
            let guard = operation.lock().await;
            if manager.running.read().await.contains_key(&id) {
                return;
            }
            let session = match manager.find(&id).await {
                Ok(session) if !matches!(session.state, SessionState::Stopped) => session,
                _ => return,
            };

            match tmux_exit_action(probe_tmux(&session).await) {
                TmuxExitAction::Reattach => {
                    match manager.spawn_process(&session, SpawnIntent::Existing).await {
                        Ok(()) => {
                            tracing::info!(
                                session = %id,
                                host = ?session.ssh_host,
                                "reattached after tmux client exited",
                            );
                            return;
                        }
                        Err(error) => tracing::warn!(
                            session = %id,
                            host = ?session.ssh_host,
                            error = %format!("{error:#}"),
                            "tmux session is alive but reattach failed; retrying",
                        ),
                    }
                }
                TmuxExitAction::Stop => {
                    Self::mark_exited(&manager, &id, code).await;
                    return;
                }
                TmuxExitAction::Retry => tracing::warn!(
                    session = %id,
                    host = ?session.ssh_host,
                    retry_seconds = delay.as_secs(),
                    "host unreachable after tmux client exited; retrying attachment",
                ),
            }
            drop(guard);
            tokio::time::sleep(delay).await;
            delay = std::cmp::min(delay * 2, ATTACH_RETRY_MAX);
        }
    }

    async fn mark_exited(manager: &SessionManager, id: &str, code: Option<i32>) {
        manager.backend_metadata.cancel_discovery(id);
        let _ = manager
            .store
            .update_state(id, SessionState::Stopped, now_ms())
            .await;
        manager.emit(SessionEvent::SessionExit {
            id: id.to_string(),
            code,
        });
        manager.emit(SessionEvent::SessionState {
            id: id.to_string(),
            state: SessionState::Stopped,
        });
    }
}

async fn inspect_cold_session(session: Session) -> (Session, ColdStartStatus) {
    let location = session.location;
    let project_path = session.project_path.clone();
    let supervisor = session.supervisor;
    let host = session.ssh_host.clone();
    let id = session.id.clone();
    let fallback = ColdStartStatus::Probe(match supervisor {
        SupervisorKind::Direct => crate::tmux::SessionProbe::Absent,
        SupervisorKind::Tmux => crate::tmux::SessionProbe::Unreachable,
    });
    let status = tokio::task::spawn_blocking(move || {
        if matches!(location, Location::Local) && !Path::new(&project_path).exists() {
            return ColdStartStatus::MissingWorktree;
        }
        ColdStartStatus::Probe(match supervisor {
            SupervisorKind::Direct => crate::tmux::SessionProbe::Absent,
            SupervisorKind::Tmux => crate::tmux::has_session(host.as_deref(), &id)
                .unwrap_or(crate::tmux::SessionProbe::Unreachable),
        })
    })
    .await
    .unwrap_or(fallback);
    (session, status)
}

async fn probe_tmux(session: &Session) -> crate::tmux::SessionProbe {
    let host = session.ssh_host.clone();
    let id = session.id.clone();
    tokio::task::spawn_blocking(move || crate::tmux::has_session(host.as_deref(), &id))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(crate::tmux::SessionProbe::Unreachable)
}

fn tmux_exit_action(probe: crate::tmux::SessionProbe) -> TmuxExitAction {
    match probe {
        crate::tmux::SessionProbe::Present => TmuxExitAction::Reattach,
        crate::tmux::SessionProbe::Absent => TmuxExitAction::Stop,
        crate::tmux::SessionProbe::Unreachable => TmuxExitAction::Retry,
    }
}

fn state_after_failed_reattach(probe: crate::tmux::SessionProbe) -> SessionState {
    match probe {
        crate::tmux::SessionProbe::Absent => SessionState::Stopped,
        crate::tmux::SessionProbe::Present | crate::tmux::SessionProbe::Unreachable => {
            SessionState::Unknown
        }
    }
}

fn remove_if_current<T>(map: &mut HashMap<String, Arc<T>>, id: &str, expected: &Arc<T>) -> bool {
    if map
        .get(id)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
    {
        map.remove(id);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::SessionProbe;

    #[test]
    fn stale_exit_cannot_remove_replacement_process() {
        let old = Arc::new(1);
        let replacement = Arc::new(2);
        let mut running = HashMap::from([("session".to_string(), replacement.clone())]);

        assert!(!remove_if_current(&mut running, "session", &old));
        assert!(Arc::ptr_eq(running.get("session").unwrap(), &replacement));
        assert!(remove_if_current(&mut running, "session", &replacement));
        assert!(!running.contains_key("session"));
    }

    #[test]
    fn tmux_probe_drives_recovery_action() {
        assert_eq!(
            tmux_exit_action(SessionProbe::Present),
            TmuxExitAction::Reattach
        );
        assert_eq!(tmux_exit_action(SessionProbe::Absent), TmuxExitAction::Stop);
        assert_eq!(
            tmux_exit_action(SessionProbe::Unreachable),
            TmuxExitAction::Retry
        );
    }

    #[test]
    fn only_authoritative_absence_stops_reattach() {
        assert_eq!(
            state_after_failed_reattach(SessionProbe::Absent),
            SessionState::Stopped
        );
        assert_eq!(
            state_after_failed_reattach(SessionProbe::Present),
            SessionState::Unknown
        );
        assert_eq!(
            state_after_failed_reattach(SessionProbe::Unreachable),
            SessionState::Unknown
        );
    }
}
