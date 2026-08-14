use anyhow::{Context, Result};
use bytes::Bytes;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub struct Pty {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

pub struct Spawned {
    pub pty: Pty,
    /// Receiver for bytes read from the PTY. `Bytes` lets the daemon's
    /// fanout (per-session broadcast subscribers) share one allocation
    /// instead of cloning a fresh `Vec<u8>` per receiver.
    pub output: mpsc::Receiver<Bytes>,
    /// Fires when the child exits.
    pub exit: tokio::sync::oneshot::Receiver<()>,
}

/// Capacity of the PTY → daemon channel. A misbehaving consumer (slow disk
/// on the log file, contended ring-buffer lock) used to be able to balloon
/// memory because the channel was unbounded. With a bounded channel and
/// `blocking_send`, the OS PTY's own buffer absorbs short stalls and the
/// reader thread parks; we never queue more than CAP × 8 KiB ≈ 2 MiB per
/// session in the channel itself.
const PTY_CHANNEL_CAP: usize = 256;
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

impl Pty {
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let m = self.master.lock().unwrap();
        m.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("resize pty")
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes).context("write to pty")?;
        w.flush().ok();
        Ok(())
    }

    pub fn kill(&self) {
        let mut killer = self.killer.lock().unwrap();
        let _ = killer.kill();
    }
}

pub fn spawn(argv: &[String], cwd: &Path, env: &[(String, String)]) -> Result<Spawned> {
    spawn_sized(argv, cwd, env, DEFAULT_COLS, DEFAULT_ROWS)
}

pub fn spawn_sized(
    argv: &[String],
    cwd: &Path,
    env: &[(String, String)],
    cols: u16,
    rows: u16,
) -> Result<Spawned> {
    if cols == 0 || rows == 0 {
        anyhow::bail!("pty dimensions must be non-zero");
    }
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let program = argv.first().cloned().context("empty argv")?;
    let mut cmd = CommandBuilder::new(&program);
    for arg in &argv[1..] {
        cmd.arg(arg);
    }
    cmd.cwd(cwd);
    // Make sure the child sees a sensible TERM and forwards env.
    cmd.env("TERM", "xterm-256color");
    for (key, value) in env {
        cmd.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawning {}", program))?;
    let killer = child.clone_killer();

    // Release the slave; only the child process needs it after spawn.
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone reader")?;
    let writer = pair.master.take_writer().context("take writer")?;

    let (tx, rx) = mpsc::channel::<Bytes>(PTY_CHANNEL_CAP);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // `blocking_send` parks the reader thread while the
                    // async consumer drains. That backpressure is exactly
                    // what we want: the OS PTY keeps a kernel buffer, and
                    // the child slows on its next write rather than us
                    // queueing memory unboundedly.
                    let chunk = Bytes::copy_from_slice(&buf[..n]);
                    if tx.blocking_send(chunk).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
        }
    });

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
        let _ = exit_tx.send(());
    });

    Ok(Spawned {
        pty: Pty {
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            killer: Mutex::new(killer),
        },
        output: rx,
        exit: exit_rx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(unix)]
    fn long_running_command() -> Vec<String> {
        vec!["sh".into(), "-c".into(), "sleep 5".into()]
    }

    #[cfg(windows)]
    fn long_running_command() -> Vec<String> {
        vec![
            "powershell.exe".into(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 5".into(),
        ]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_does_not_wait_for_child_exit() {
        let cwd = tempfile::tempdir().unwrap();
        let spawned = spawn(&long_running_command(), cwd.path(), &[]).unwrap();

        // Give the waiter time to enter its blocking wait. The killer must
        // remain usable independently while that wait is in progress.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            spawned.pty.kill();
            let _ = kill_tx.send(());
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            kill_rx.await.expect("killer thread stopped unexpectedly");
            spawned.exit.await.expect("waiter stopped unexpectedly")
        })
        .await
        .expect("kill contended with the blocking child wait");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sized_spawn_sets_the_initial_terminal_dimensions() {
        let cwd = tempfile::tempdir().unwrap();
        let command = vec!["sh".into(), "-c".into(), "stty size".into()];
        let mut spawned = spawn_sized(&command, cwd.path(), &[], 93, 27).unwrap();
        let output = tokio::time::timeout(Duration::from_secs(2), spawned.output.recv())
            .await
            .expect("command produced no output")
            .expect("pty closed without output");

        assert_eq!(String::from_utf8_lossy(&output).trim(), "27 93");
    }

    #[test]
    fn sized_spawn_rejects_zero_dimensions() {
        let cwd = tempfile::tempdir().unwrap();
        assert!(spawn_sized(&long_running_command(), cwd.path(), &[], 0, 40).is_err());
        assert!(spawn_sized(&long_running_command(), cwd.path(), &[], 120, 0).is_err());
    }
}
