//! Thin wrappers over `tmux -L slide <command>`, optionally over SSH.
//!
//! Slide uses a dedicated tmux server (socket name `slide`) so its sessions
//! never collide with whatever tmux the user runs themselves. Every
//! function takes an `Option<&str>` host: `None` runs locally, `Some(h)`
//! wraps the command in `ssh h "tmux -L slide …"`. Control commands use
//! `-o BatchMode=yes` so a misconfigured host fails fast instead of
//! hanging on a password prompt.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Output};

/// tmux socket / server label used by slide. `-L slide` isolates us from
/// the user's own tmux sessions.
pub const SERVER_LABEL: &str = "slide";
const SESSION_PREFIX: &str = "slide-";

/// Format a tmux session name for a given slide session id.
pub fn session_name(id: &str) -> String {
    format!("{SESSION_PREFIX}{id}")
}

/// `true` if tmux is on the local `PATH`. Remote availability is probed
/// lazily on first use (tmux command fails if it's missing there).
pub fn is_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build and run `tmux -L slide <tmux_args...>`, either directly or via
/// `ssh <host> "tmux …"`. Returns the captured output so callers can
/// inspect stderr for benign conditions like "no server running".
fn exec_tmux(host: Option<&str>, tmux_args: &[&str], ctx: &str) -> Result<Output> {
    let cmd = match host {
        None => {
            let mut c = Command::new("tmux");
            c.args(["-L", SERVER_LABEL]);
            c.args(tmux_args);
            c
        }
        Some(h) => {
            // Defense in depth. `create_session` already validates ssh_host,
            // but this path is also reached from cold-start reconciliation
            // that trusts rows in SQLite. Re-check so a bad row (or a future
            // caller that forgets) can never hand openssh a `-oProxyCommand`.
            crate::ssh::validate_host(h)
                .with_context(|| format!("invalid ssh host for tmux ({ctx})"))?;
            // ssh takes the remote command as one argv element. Build a
            // shell-quoted string so remote-side word-splitting preserves
            // argument boundaries.
            let mut remote = format!("tmux -L {SERVER_LABEL}");
            for a in tmux_args {
                remote.push(' ');
                remote.push_str(&shell_quote(a));
            }
            let mut c = Command::new("ssh");
            c.args(["-o", "BatchMode=yes"]);
            // ConnectTimeout + connection multiplexing. The first call to
            // a host pays the full handshake; subsequent calls (within
            // ControlPersist) reuse the master, so a remote create chain
            // that used to be 4 fresh ssh handshakes collapses to ~1 over
            // the wire. ConnectTimeout makes a dead host fail in seconds
            // instead of hanging the daemon for OpenSSH's default.
            for a in crate::ssh::ssh_args() {
                c.arg(a);
            }
            c.arg(h);
            c.arg(remote);
            c
        }
    };
    let mut cmd = cmd;
    cmd.output().with_context(|| format!("exec tmux ({ctx})"))
}

fn run(host: Option<&str>, tmux_args: &[&str], ctx: &str) -> Result<()> {
    let out = exec_tmux(host, tmux_args, ctx)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("tmux {ctx} failed: {}", stderr.trim());
    }
    Ok(())
}

/// Build the shell command string that tmux's default-shell will execute for
/// a new pane. We wrap in `"$SHELL" -lc …` so the login shell sources the
/// user's profile before `exec`-ing the backend — without this, tools like
/// `claude` installed under `~/.local/bin` or an nvm shim dir are absent
/// from PATH (tmux's default-shell runs non-interactively and non-login)
/// and the backend dies immediately on exec. Falls back to `/bin/sh` if
/// `$SHELL` is unset, e.g. when the daemon is launched from a service
/// manager that strips env.
fn build_pane_command(cwd: &Path, argv: &[String]) -> String {
    let quoted: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
    let inner = format!(
        "cd {} && exec {}",
        shell_quote(&cwd.to_string_lossy()),
        quoted.join(" "),
    );
    format!("exec \"${{SHELL:-/bin/sh}}\" -lc {}", shell_quote(&inner))
}

/// Create a new detached tmux session that runs `argv` in `cwd`.
pub fn new_session(
    host: Option<&str>,
    id: &str,
    cwd: &Path,
    argv: &[String],
    cols: u16,
    rows: u16,
) -> Result<()> {
    if argv.is_empty() {
        bail!("new_session: empty argv");
    }
    let cmd = build_pane_command(cwd, argv);
    let name = session_name(id);
    let cols_s = cols.to_string();
    let rows_s = rows.to_string();
    run(
        host,
        &[
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            &cols_s,
            "-y",
            &rows_s,
            &cmd,
        ],
        "new-session",
    )
}

/// Argv chunk that turns mouse on AND removes tmux's default drag bindings
/// from every key table that could process drag events.
///
/// Why both halves matter:
///
/// - `set -g mouse on` is required for tmux to react to wheel events
///   (its default `WheelUpPane` binding enters `copy-mode -e` and pages
///   through pane history — the only scroll mechanism that works inside
///   an alt-screen TUI like Claude / Codex, since xterm.js's own
///   scrollback is empty in that buffer).
///
/// - The `unbind-key` calls keep tmux from consuming drag events for its
///   own selection. Without these, plain drag would highlight inside tmux
///   but never reach the browser, defeating Cmd+C. With them, drag falls
///   through to xterm.js's selection layer (driven from a custom DOM
///   handler in `web/src/components/Terminal.tsx`).
///
/// We must unbind in the `root` key table AND the `copy-mode` /
/// `copy-mode-vi` tables. Once a wheel-up promotes the user into
/// copy-mode, tmux switches active tables and would otherwise re-eat
/// drag events for its own copy-mode selection — which redraws the
/// pane and clears the user's browser-side highlight.
///
/// Returned as a Vec so callers can splice it into a longer chained
/// tmux command (e.g. create_session_with_log inserts it between
/// start-server and new-session).
fn setup_mouse_argv() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = vec!["set-option", "-g", "mouse", "on"];
    for table in ["root", "copy-mode", "copy-mode-vi"] {
        for key in ["MouseDrag1Pane", "MouseDragEnd1Pane"] {
            v.push(";");
            v.push("unbind-key");
            v.push("-T");
            v.push(table);
            v.push(key);
        }
    }
    v
}

/// Create a fresh detached session and start teeing output to `log_path`
/// — all in a single tmux invocation. Over SSH this collapses what used
/// to be three sequential ssh handshakes (start-server / new-session /
/// pipe-pane) into one round trip, which is the bulk of the latency on
/// a VPN'd remote create.
///
/// Splices the `setup_mouse_argv` chunk inline so a brand-new tmux
/// server is configured before the first session lands.
pub fn create_session_with_log(
    host: Option<&str>,
    id: &str,
    cwd: &Path,
    argv: &[String],
    cols: u16,
    rows: u16,
    log_path: &Path,
) -> Result<()> {
    if argv.is_empty() {
        bail!("create_session_with_log: empty argv");
    }
    let cmd = build_pane_command(cwd, argv);
    let name = session_name(id);
    let cols_s = cols.to_string();
    let rows_s = rows.to_string();
    let pipe = format!("cat >> {}", shell_quote(&log_path.to_string_lossy()));
    // tmux's command separator `;` must be its own argv element. The remote
    // exec_tmux path shell-quotes each element, so the `;` survives the
    // round-trip as a literal arg to tmux (not a shell separator).
    let mut chained: Vec<&str> = vec!["start-server", ";"];
    chained.extend(setup_mouse_argv());
    chained.extend([
        ";",
        "new-session",
        "-d",
        "-s",
        &name,
        "-x",
        &cols_s,
        "-y",
        &rows_s,
        &cmd,
        ";",
        "pipe-pane",
        "-t",
        &name,
        "-O",
        &pipe,
    ]);
    run(host, &chained, "create-session+log")
}

/// Apply slide's mouse policy (mouse on, drag bindings unbound) to an
/// already-running tmux server. Idempotent. Used on reattach so a
/// daemon-restart against an existing server (which may still have a
/// stale `mouse off` from a previous slide build, or default drag
/// bindings from a server we didn't start) gets the right config without
/// requiring `tmux kill-server`.
pub fn setup_mouse(host: Option<&str>) -> Result<()> {
    let mut chained: Vec<&str> = vec!["start-server", ";"];
    chained.extend(setup_mouse_argv());
    run(host, &chained, "setup mouse")
}

/// Tee all output of the session's pane to `log_path` on the host.
///
/// `-O` opens the pipe in overwrite mode so we don't append to a stale log
/// from a prior tmux session with the same id.
pub fn pipe_pane(host: Option<&str>, id: &str, log_path: &Path) -> Result<()> {
    let cmd = format!("cat >> {}", shell_quote(&log_path.to_string_lossy()));
    let name = session_name(id);
    run(host, &["pipe-pane", "-t", &name, "-O", &cmd], "pipe-pane")
}

/// Plain-text snapshot of the session's visible pane. tmux renders the
/// virtual terminal internally, so this is exactly what the user sees —
/// we can't reconstruct that from the raw byte ring because a TUI's
/// cursor-positioning commands leave the prompt character nowhere near
/// the tail after ANSI stripping. A missing session is returned as an
/// empty string so the caller can just treat it as "nothing to match";
/// real errors (auth, network) still surface as `Err`.
pub fn capture_pane(host: Option<&str>, id: &str) -> Result<String> {
    let name = session_name(id);
    let out = exec_tmux(host, &["capture-pane", "-p", "-t", &name], "capture-pane")?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr_means_session_gone(&stderr) {
        return Ok(String::new());
    }
    bail!("tmux capture-pane failed: {}", stderr.trim());
}

/// True when tmux's stderr indicates the target session no longer exists.
/// Treated as an empty capture by the ticker — semantically equivalent to
/// "no prompt visible." `no current target` is what tmux 3.x on Ubuntu
/// emits when the named target was just removed (race between kill_session
/// and a follow-up capture-pane); macOS tmux says `can't find session` for
/// the same condition. `no server running` / `error connecting` cover the
/// case where the entire tmux server has exited.
fn stderr_means_session_gone(stderr: &str) -> bool {
    stderr.contains("can't find session")
        || stderr.contains("can't find pane")
        || stderr.contains("no server running")
        || stderr.contains("error connecting")
        || stderr.contains("no current target")
}

/// Tri-state result of probing whether tmux owns a given session. The
/// distinction between "tmux says no" and "we couldn't reach the host"
/// is load-bearing on cold start: a transient SSH outage must not flip a
/// healthy remote session to Stopped — the caller needs to keep the
/// prior state and retry once the host comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionProbe {
    /// tmux confirmed the session exists.
    Present,
    /// tmux confirmed the session does not exist (or the tmux server
    /// itself isn't running on the host).
    Absent,
    /// We couldn't reach the host (SSH timeout, auth failure, network).
    /// Only ever returned when `host.is_some()`.
    Unreachable,
}

/// Probe tmux for a session. See [`SessionProbe`] for the meaning of each
/// variant — in particular, callers MUST distinguish Absent from
/// Unreachable, since marking a row Stopped on Unreachable would lose
/// healthy remote sessions whenever SSH blips.
pub fn has_session(host: Option<&str>, id: &str) -> Result<SessionProbe> {
    let name = session_name(id);
    let out = exec_tmux(host, &["has-session", "-t", &name], "has-session")?;
    // has-session exits 0 if found, 1 if not, other codes on real errors.
    if out.status.success() {
        return Ok(SessionProbe::Present);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Benign: session isn't there, or the tmux server itself isn't running.
    // `error connecting` is tmux-client-to-its-own-socket, not SSH — keep
    // it as Absent.
    if stderr.contains("can't find session")
        || stderr.contains("no server running")
        || stderr.contains("error connecting")
        || stderr.contains("no current target")
    {
        return Ok(SessionProbe::Absent);
    }
    // Local invocation reaching this branch is a real error worth surfacing.
    if host.is_none() {
        bail!("tmux has-session failed: {}", stderr.trim());
    }
    // Remote: ssh-layer failure (timeout, auth, host down). We can't tell
    // whether the session is alive — let the caller retry instead of
    // force-Stopping it.
    Ok(SessionProbe::Unreachable)
}

pub fn kill_session(host: Option<&str>, id: &str) -> Result<()> {
    let name = session_name(id);
    let out = exec_tmux(host, &["kill-session", "-t", &name], "kill-session")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // "no server running" means tmux had no sessions at all — same
        // outcome as "can't find session" for idempotent kill. Some tmux
        // builds also report "server exited unexpectedly" after killing
        // the last session; 3.x ubuntu builds surface the same state as
        // "no current target". All mean "already gone" for our purposes.
        let benign = stderr.contains("can't find session")
            || stderr.contains("no server running")
            || stderr.contains("error connecting")
            || stderr.contains("server exited unexpectedly")
            || stderr.contains("no current target");
        if !benign {
            bail!("tmux kill-session failed: {}", stderr.trim());
        }
    }
    Ok(())
}

/// Return slide-owned session ids (the part after `slide-`), one per line.
pub fn list_session_ids(host: Option<&str>) -> Result<Vec<String>> {
    let out = exec_tmux(
        host,
        &["list-sessions", "-F", "#{session_name}"],
        "list-sessions",
    )?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no server running") || stderr.contains("error connecting") {
            return Ok(Vec::new());
        }
        bail!("tmux list-sessions failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix(SESSION_PREFIX).map(|s| s.to_string()))
        .collect())
}

/// Resize the session's default window.
pub fn resize_window(host: Option<&str>, id: &str, cols: u16, rows: u16) -> Result<()> {
    let name = session_name(id);
    let cols_s = cols.to_string();
    let rows_s = rows.to_string();
    run(
        host,
        &["resize-window", "-t", &name, "-x", &cols_s, "-y", &rows_s],
        "resize-window",
    )
}

/// argv to attach to a session from a local PTY. The daemon runs this
/// inside its own `Pty::spawn` to get bidi I/O with the backend. For a
/// remote host the result is `ssh -t <host> tmux …`.
pub fn attach_argv(host: Option<&str>, id: &str) -> Vec<String> {
    match host {
        None => vec![
            "tmux".to_string(),
            "-L".to_string(),
            SERVER_LABEL.to_string(),
            "attach-session".to_string(),
            "-t".to_string(),
            session_name(id),
        ],
        Some(h) => {
            // Pass the tmux invocation as a single string so the remote
            // shell parses it. `-t` forces ssh to allocate a TTY.
            let remote = format!(
                "tmux -L {SERVER_LABEL} attach-session -t {}",
                shell_quote(&session_name(id)),
            );
            // Reuse the multiplexed master set up by the create-time control
            // commands (no BatchMode here — attach is interactive).
            let mut argv = vec!["ssh".to_string(), "-t".to_string()];
            argv.extend(crate::ssh::ssh_args());
            argv.push(h.to_string());
            argv.push(remote);
            argv
        }
    }
}

/// POSIX shell single-quote a string for tmux's `-c` / inline command use.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '/' | '.' | ':' | '=' | '@' | '+' | ',')
        })
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_prefix() {
        assert_eq!(session_name("abc"), "slide-abc");
    }

    #[test]
    fn setup_mouse_argv_turns_mouse_on_and_unbinds_drag() {
        let argv = setup_mouse_argv();
        // Order matters: tmux processes the chained command left-to-right,
        // so `set-option mouse on` (which reactivates default bindings if
        // they were unbound by a prior config) must come BEFORE the
        // unbinds — otherwise the unbinds wouldn't have anything to drop.
        let on_pos = argv
            .windows(4)
            .position(|w| w == ["set-option", "-g", "mouse", "on"])
            .expect("set-option -g mouse on chunk missing");
        // Every table that can process drag events must have both bindings
        // dropped, otherwise tmux re-eats drag once the user is in that
        // table (e.g. wheel-up promotes them into copy-mode, where the
        // copy-mode-vi MouseDrag1Pane binding would start a tmux-side
        // selection that redraws the pane and wipes our browser highlight).
        for table in ["root", "copy-mode", "copy-mode-vi"] {
            for key in ["MouseDrag1Pane", "MouseDragEnd1Pane"] {
                let pos = argv
                    .windows(4)
                    .position(|w| w == ["unbind-key", "-T", table, key])
                    .unwrap_or_else(|| panic!("unbind {table}:{key} missing"));
                assert!(on_pos < pos, "mouse on must come before {table}:{key}");
            }
        }
    }

    #[test]
    fn stderr_means_session_gone_recognizes_each_variant() {
        for s in [
            "can't find session: slide-abc",
            "can't find pane: slide-abc",
            "no server running on /tmp/tmux-1000/default",
            "error connecting to /tmp/tmux-1000/default (No such file or directory)",
            "no current target",
            // Real-world line from Ubuntu tmux 3.x after kill_session:
            "tmux: can't find session: slide-test-uuid",
        ] {
            assert!(stderr_means_session_gone(s), "should match: {s}");
        }
    }

    #[test]
    fn stderr_means_session_gone_rejects_real_errors() {
        for s in [
            "",
            "tmux: invalid option",
            "operation not permitted",
            "permission denied",
        ] {
            assert!(!stderr_means_session_gone(s), "should not match: {s}");
        }
    }

    #[test]
    fn shell_quote_plain() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("/tmp/foo-bar"), "/tmp/foo-bar");
    }

    #[test]
    fn shell_quote_with_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn shell_quote_with_quote() {
        assert_eq!(shell_quote("it's fine"), "'it'\\''s fine'");
    }

    #[test]
    fn shell_quote_empty() {
        // Empty string must be quoted, otherwise the shell skips it.
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn build_pane_command_wraps_in_login_shell() {
        // The wrapper must invoke the user's login shell so ~/.profile etc.
        // populate PATH before the backend execs. Without -l, tools like
        // claude installed under ~/.local/bin aren't visible.
        let cmd = build_pane_command(
            Path::new("/home/u/proj"),
            &[
                "claude".to_string(),
                "--resume".to_string(),
                "id".to_string(),
            ],
        );
        assert!(cmd.contains("-lc"), "missing login-shell flag: {cmd}");
        assert!(
            cmd.contains("${SHELL:-/bin/sh}"),
            "missing SHELL expansion: {cmd}"
        );
        assert!(cmd.contains("cd /home/u/proj"), "missing cd: {cmd}");
        assert!(cmd.contains("claude"), "missing backend: {cmd}");
        assert!(cmd.contains("--resume"), "missing backend args: {cmd}");
    }

    #[test]
    fn build_pane_command_quotes_paths_with_spaces() {
        // Paths with spaces must survive both the inner cd and the outer
        // -lc wrapping; otherwise tmux would cd into a truncated path.
        let cmd = build_pane_command(
            Path::new("/home/u/my projects/app"),
            &["claude".to_string()],
        );
        // The inner string is single-quoted once for the outer shell, so
        // embedded single quotes inside the cd path get the '\'' dance.
        assert!(cmd.contains("my projects/app"), "path mangled: {cmd}");
    }

    #[test]
    fn attach_argv_local_is_tmux_direct() {
        let argv = attach_argv(None, "abc");
        assert_eq!(argv[0], "tmux");
        assert_eq!(argv.last().unwrap(), "slide-abc");
    }

    #[test]
    fn attach_argv_remote_wraps_in_ssh() {
        let argv = attach_argv(Some("host.example"), "abc");
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "-t");
        // Locate the host position dynamically — multiplex options sit
        // between `-t` and the host, and we don't want this test pinned to
        // the exact option count.
        let host_idx = argv.iter().position(|a| a == "host.example").unwrap();
        assert!(argv[host_idx + 1].contains("tmux -L slide attach-session -t slide-abc"));
    }

    #[test]
    fn attach_argv_remote_threads_ssh_common_args() {
        // attach_argv must splat in `ssh_args()` between `-t` and the host,
        // not just bare ssh — otherwise the long-lived attach would (a)
        // re-handshake instead of using the warm multiplex master and (b)
        // hang indefinitely if the remote goes dark, since the default
        // OpenSSH connect timeout is effectively unbounded. ConnectTimeout
        // is always present; multiplex options are env-conditional (skipped
        // on macOS when $HOME would overflow `sun_path`) and covered by
        // ssh.rs unit tests.
        let argv = attach_argv(Some("host.example"), "abc");
        let joined = argv.join(" ");
        assert!(
            joined.contains("ConnectTimeout="),
            "missing ConnectTimeout: {joined}"
        );
    }

    /// Full tmux roundtrip: spawn a session running `sleep 60`, confirm
    /// it's listed, pipe output to a log, then kill it. Skipped on CI
    /// runners without tmux.
    #[test]
    fn tmux_lifecycle() {
        if !is_available() {
            eprintln!("tmux not on PATH; skipping");
            return;
        }
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("out.log");
        let cwd = tmp.path();

        new_session(
            None,
            &id,
            cwd,
            &["sleep".to_string(), "60".to_string()],
            80,
            24,
        )
        .unwrap();
        pipe_pane(None, &id, &log).unwrap();

        assert_eq!(has_session(None, &id).unwrap(), SessionProbe::Present);
        let ids = list_session_ids(None).unwrap();
        assert!(ids.contains(&id));

        kill_session(None, &id).unwrap();
        assert_eq!(has_session(None, &id).unwrap(), SessionProbe::Absent);

        // kill_session on a gone session is a no-op.
        kill_session(None, &id).unwrap();
    }

    /// capture-pane reads whatever tmux currently has rendered for the
    /// session. We use `sleep 60` as the body — mirrors `tmux_lifecycle`
    /// above and avoids any shell-escaping quirks in `build_pane_command`.
    /// The important contract for the ticker: capture returns Ok on a
    /// live pane (even if visually blank), and an empty string (not Err)
    /// once the session is gone.
    #[test]
    fn capture_pane_live_and_post_teardown() {
        if !is_available() {
            eprintln!("tmux not on PATH; skipping");
            return;
        }
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();

        new_session(
            None,
            &id,
            tmp.path(),
            &["sleep".to_string(), "60".to_string()],
            80,
            24,
        )
        .unwrap();
        // tmux paints asynchronously; let the first frame settle.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let captured = capture_pane(None, &id).unwrap();
        // A live pane renders its rows; capture must be non-empty even if
        // the command itself writes nothing.
        assert!(
            !captured.is_empty(),
            "expected non-empty pane capture, got {captured:?}"
        );

        kill_session(None, &id).unwrap();
        // After teardown, capture is empty rather than an error — lets the
        // ticker treat "session vanished mid-probe" the same as "no prompt".
        let gone = capture_pane(None, &id).unwrap();
        assert!(gone.is_empty(), "expected empty capture, got {gone:?}");
    }

    /// `create_session_with_log` is the chained equivalent of
    /// new_session + pipe_pane (with set-mouse and start-server folded in).
    /// Asserts the same observable end-state as `tmux_lifecycle`.
    #[test]
    fn create_session_with_log_matches_lifecycle() {
        if !is_available() {
            eprintln!("tmux not on PATH; skipping");
            return;
        }
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("out.log");

        create_session_with_log(
            None,
            &id,
            tmp.path(),
            &["sleep".to_string(), "60".to_string()],
            80,
            24,
            &log,
        )
        .unwrap();

        assert_eq!(has_session(None, &id).unwrap(), SessionProbe::Present);
        kill_session(None, &id).unwrap();
        assert_eq!(has_session(None, &id).unwrap(), SessionProbe::Absent);
    }
}
