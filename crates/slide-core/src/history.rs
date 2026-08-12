use crate::session::{Location, Session};
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const REMOTE_STDERR_LIMIT: usize = 16 * 1024;
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

/// Read a bounded suffix of persisted terminal output without loading the
/// whole session log. Handoffs use this when the source is not currently
/// attached to the daemon.
pub(crate) fn read_tail(session: &Session, limit: usize) -> Result<Vec<u8>> {
    match session.location {
        Location::Local => {
            let path = session
                .host_log_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| crate::config::logs_dir().join(format!("{}.log", session.id)));
            let mut file = File::open(path)?;
            let len = file.metadata()?.len();
            file.seek(SeekFrom::Start(len.saturating_sub(limit as u64)))?;
            let mut bytes = Vec::with_capacity(limit.min(len as usize));
            file.take(limit as u64).read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        Location::Remote => read_remote_tail(session, limit),
    }
}

fn read_remote_tail(session: &Session, limit: usize) -> Result<Vec<u8>> {
    let host = session
        .ssh_host
        .as_deref()
        .context("remote session missing SSH host")?;
    crate::ssh::validate_host(host)?;
    let path = session
        .host_log_path
        .clone()
        .unwrap_or_else(|| format!("/tmp/slide-{}.log", session.id));
    let remote = [
        "tail".to_string(),
        "-c".to_string(),
        limit.to_string(),
        path,
    ]
    .iter()
    .map(|part| shell_quote(part))
    .collect::<Vec<_>>()
    .join(" ");
    let mut command = Command::new("ssh");
    command
        .args(["-o", "BatchMode=yes"])
        .args(crate::ssh::ssh_args())
        .arg(host)
        .arg(remote);
    let output = crate::process::run_bounded(command, limit, REMOTE_STDERR_LIMIT, REMOTE_TIMEOUT)?;
    if output.timed_out || output.stdout_truncated || output.stderr_truncated || !output.success {
        bail!("remote session history is unavailable");
    }
    Ok(output.stdout)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendKind;
    use crate::session::{SessionState, SupervisorKind};

    #[test]
    fn local_tail_reads_only_the_requested_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.log");
        std::fs::write(&path, b"0123456789").unwrap();
        let session = Session {
            id: "tail".to_string(),
            name: "tail".to_string(),
            backend: BackendKind::Codex,
            location: Location::Local,
            ssh_host: None,
            base_dir: "/tmp".to_string(),
            project_path: "/tmp/project".to_string(),
            worktree: false,
            state: SessionState::Stopped,
            created_at: 1,
            last_activity: 2,
            supervisor: SupervisorKind::Direct,
            host_log_path: Some(path.to_string_lossy().into_owned()),
            log_offset: 0,
            backend_session_id: None,
            parent_session_id: None,
        };

        assert_eq!(read_tail(&session, 4).unwrap(), b"6789");
        assert_eq!(read_tail(&session, 20).unwrap(), b"0123456789");
    }

    #[test]
    fn remote_paths_are_shell_quoted() {
        assert_eq!(shell_quote("/tmp/it's here"), "'/tmp/it'\\''s here'");
    }
}
