use crate::session::{Location, Session, SupervisorKind};
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub(crate) const DEFAULT_TAIL_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const RENDERED_HISTORY_BYTES: usize = 16 * 1024 * 1024;
const REMOTE_STDERR_LIMIT: usize = 16 * 1024;
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

fn rendered_path(id: &str) -> PathBuf {
    crate::config::logs_dir().join(format!("{id}.rendered"))
}

/// Read terminal history for a stopped session. Tmux pipe-pane logs contain
/// stateful terminal commands, so an arbitrary suffix cannot be replayed in a
/// fresh terminal. Explicit Stop writes a bounded rendered pane snapshot for
/// tmux sessions; if that snapshot is unavailable, surface an error instead
/// of displaying a distorted raw suffix. Direct sessions retain the legacy
/// raw-log fallback.
pub(crate) fn read_stopped(session: &Session) -> Result<Vec<u8>> {
    read_stopped_from(session, &rendered_path(&session.id))
}

fn read_stopped_from(session: &Session, rendered: &Path) -> Result<Vec<u8>> {
    match session.supervisor {
        SupervisorKind::Tmux => read_bounded(rendered, RENDERED_HISTORY_BYTES)
            .context("rendered terminal history is unavailable"),
        SupervisorKind::Direct => read_tail(session, DEFAULT_TAIL_BYTES),
    }
}

/// Atomically replace a rendered snapshot with a mode-0600 file. The parent
/// directory is already private, but the file mode is explicit as defense in
/// depth because terminal history can contain credentials and source code.
pub(crate) fn write_rendered(id: &str, bytes: &[u8]) -> Result<()> {
    crate::config::ensure_dirs().context("create terminal history directory")?;
    write_rendered_at(&rendered_path(id), bytes)
}

fn write_rendered_at(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > RENDERED_HISTORY_BYTES {
        bail!("rendered terminal history exceeded its output limit");
    }
    let parent = path
        .parent()
        .context("rendered history path has no parent")?;
    std::fs::create_dir_all(parent).context("create rendered history directory")?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("terminal-history"),
        uuid::Uuid::new_v4()
    ));

    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).context("create rendered history")?;
        file.write_all(bytes).context("write rendered history")?;
        file.sync_all().context("sync rendered history")?;
        std::fs::rename(&temp, path).context("publish rendered history")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

pub(crate) fn remove_rendered(id: &str) -> Result<()> {
    match std::fs::remove_file(rendered_path(id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove rendered terminal history"),
    }
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > limit as u64 {
        bail!("rendered terminal history exceeded its output limit");
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("rendered terminal history exceeded its output limit");
    }
    Ok(bytes)
}

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

pub(crate) fn remove_remote_log(session: &Session) -> Result<()> {
    let host = session
        .ssh_host
        .as_deref()
        .context("remote session missing SSH host")?;
    crate::ssh::validate_host(host)?;
    let path = session
        .host_log_path
        .as_deref()
        .context("remote session missing log path")?;
    let remote = format!("rm -f -- {}", shell_quote(path));
    let mut command = Command::new("ssh");
    command
        .args(["-o", "BatchMode=yes"])
        .args(crate::ssh::ssh_args())
        .arg(host)
        .arg(remote);
    let output = crate::process::run_bounded(command, 0, REMOTE_STDERR_LIMIT, REMOTE_TIMEOUT)?;
    if output.timed_out || output.stderr_truncated || !output.success {
        bail!("remote session log cleanup failed");
    }
    Ok(())
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
    use crate::session::{ExecutionPolicy, SessionState, SupervisorKind};

    fn local_session(id: &str, supervisor: SupervisorKind, log: &Path) -> Session {
        Session {
            id: id.to_string(),
            name: id.to_string(),
            backend: BackendKind::Codex,
            execution_policy: ExecutionPolicy::Unrestricted,
            location: Location::Local,
            ssh_host: None,
            base_dir: "/tmp".to_string(),
            project_path: "/tmp/project".to_string(),
            worktree: false,
            state: SessionState::Stopped,
            created_at: 1,
            last_activity: 2,
            supervisor,
            host_log_path: Some(log.to_string_lossy().into_owned()),
            backend_session_id: None,
            parent_session_id: None,
        }
    }

    #[test]
    fn local_tail_reads_only_the_requested_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.log");
        std::fs::write(&path, b"0123456789").unwrap();
        let session = local_session("tail", SupervisorKind::Direct, &path);

        assert_eq!(read_tail(&session, 4).unwrap(), b"6789");
        assert_eq!(read_tail(&session, 20).unwrap(), b"0123456789");
    }

    #[test]
    fn remote_paths_are_shell_quoted() {
        assert_eq!(shell_quote("/tmp/it's here"), "'/tmp/it'\\''s here'");
    }

    #[test]
    fn local_tail_bounds_large_logs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.log");
        let file = File::create(&path).unwrap();
        file.set_len((DEFAULT_TAIL_BYTES * 4) as u64).unwrap();
        let session = local_session("large", SupervisorKind::Direct, &path);

        assert_eq!(
            read_tail(&session, DEFAULT_TAIL_BYTES).unwrap().len(),
            DEFAULT_TAIL_BYTES
        );
    }

    #[test]
    fn stopped_tmux_never_replays_the_raw_log() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("raw.log");
        let rendered = dir.path().join("session.rendered");
        std::fs::write(&raw, b"arbitrary-stateful-suffix").unwrap();
        let session = local_session("tmux", SupervisorKind::Tmux, &raw);

        let error = read_stopped_from(&session, &rendered).unwrap_err();
        assert!(error
            .to_string()
            .contains("rendered terminal history is unavailable"));

        std::fs::write(&rendered, b"self-contained\r\n").unwrap();
        assert_eq!(
            read_stopped_from(&session, &rendered).unwrap(),
            b"self-contained\r\n"
        );
    }

    #[test]
    fn rendered_snapshot_is_atomic_private_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.rendered");
        write_rendered_at(&path, b"first\r\n").unwrap();
        write_rendered_at(&path, b"second\r\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second\r\n");
        assert!(write_rendered_at(&path, &vec![0; RENDERED_HISTORY_BYTES + 1]).is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn rendered_snapshot_read_is_hard_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.rendered");
        let file = File::create(&path).unwrap();
        file.set_len(RENDERED_HISTORY_BYTES as u64 + 1).unwrap();
        assert!(read_bounded(&path, RENDERED_HISTORY_BYTES).is_err());
    }
}
