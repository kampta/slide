use anyhow::{Context, Result};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub struct BoundedOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

/// Run a child process with hard time and output bounds. Both pipes are
/// drained concurrently so a noisy stderr cannot deadlock a stdout reader;
/// crossing either output limit closes the pipe and terminates the child.
pub fn run_bounded(
    mut command: Command,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: Duration,
) -> Result<BoundedOutput> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().context("spawn command")?;
    let mut stdout = child.stdout.take().context("capture command stdout")?;
    let mut stderr = child.stderr.take().context("capture command stderr")?;
    let (truncated_tx, truncated_rx) = mpsc::sync_channel(2);
    let stdout_tx = truncated_tx.clone();
    let stdout_reader =
        std::thread::spawn(move || read_limited(&mut stdout, stdout_limit, stdout_tx));
    let stderr_reader =
        std::thread::spawn(move || read_limited(&mut stderr, stderr_limit, truncated_tx));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if truncated_rx.try_recv().is_ok() {
            let _ = child.kill();
            break child.wait().context("wait for bounded command")?;
        }
        if let Some(status) = child.try_wait().context("poll command")? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().context("wait for timed out command")?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stdout_truncated) = stdout_reader.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_reader.join().unwrap_or_default();
    Ok(BoundedOutput {
        success: status.success(),
        code: status.code(),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        timed_out,
    })
}

fn read_limited(
    reader: &mut impl Read,
    limit: usize,
    truncated_tx: mpsc::SyncSender<()>,
) -> (Vec<u8>, bool) {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let _ = reader.take((limit + 1) as u64).read_to_end(&mut bytes);
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    if truncated {
        let _ = truncated_tx.send(());
    }
    (bytes, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_output_and_terminates_the_child() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 123456789"]);
        let output = run_bounded(command, 4, 100, Duration::from_secs(1)).unwrap();
        assert_eq!(output.stdout, b"1234");
        assert!(output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }

    #[test]
    fn terminates_a_timed_out_child() {
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do :; done"]);
        let started = Instant::now();
        let output = run_bounded(command, 10, 10, Duration::from_millis(50)).unwrap();
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn timed_out_child_cannot_perform_a_late_side_effect() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("late");
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 0.15; printf late > \"$1\"", "_"])
            .arg(&marker);

        let output = run_bounded(command, 10, 10, Duration::from_millis(20)).unwrap();
        assert!(output.timed_out);
        std::thread::sleep(Duration::from_millis(250));
        assert!(!marker.exists());
    }
}
