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
use std::process::Command;
use std::time::Duration;

use crate::process::{run_bounded, BoundedOutput};

/// tmux socket / server label used by slide. `-L slide` isolates us from
/// the user's own tmux sessions.
pub const SERVER_LABEL: &str = "slide";
const HISTORY_LIMIT_LINES: &str = "20000";
const SESSION_PREFIX: &str = "slide-";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const CONTROL_STDOUT_LIMIT: usize = 2 * 1024 * 1024;
const CONTROL_STDERR_LIMIT: usize = 64 * 1024;

/// Format a tmux session name for a given slide session id.
pub fn session_name(id: &str) -> String {
    format!("{SESSION_PREFIX}{id}")
}

/// `true` if tmux is on the local `PATH`. Remote availability is probed
/// lazily on first use (tmux command fails if it's missing there).
pub fn is_available() -> bool {
    let mut command = Command::new("tmux");
    command.arg("-V");
    run_bounded(
        command,
        CONTROL_STDOUT_LIMIT,
        CONTROL_STDERR_LIMIT,
        Duration::from_secs(2),
    )
    .map(|output| output.success && !output.timed_out)
    .unwrap_or(false)
}

/// Build and run `tmux -L slide <tmux_args...>`, either directly or via
/// `ssh <host> "tmux …"`. Returns the captured output so callers can
/// inspect stderr for benign conditions like "no server running".
fn exec_tmux_with_limit(
    host: Option<&str>,
    tmux_args: &[&str],
    ctx: &str,
    stdout_limit: usize,
) -> Result<BoundedOutput> {
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
    let output = run_bounded(cmd, stdout_limit, CONTROL_STDERR_LIMIT, CONTROL_TIMEOUT)
        .with_context(|| format!("exec tmux ({ctx})"))?;
    if output.timed_out {
        bail!("tmux {ctx} timed out after {}s", CONTROL_TIMEOUT.as_secs());
    }
    if output.stdout_truncated || output.stderr_truncated {
        bail!("tmux {ctx} exceeded its output limit");
    }
    Ok(output)
}

fn exec_tmux(host: Option<&str>, tmux_args: &[&str], ctx: &str) -> Result<BoundedOutput> {
    exec_tmux_with_limit(host, tmux_args, ctx, CONTROL_STDOUT_LIMIT)
}

fn run(host: Option<&str>, tmux_args: &[&str], ctx: &str) -> Result<()> {
    let out = exec_tmux(host, tmux_args, ctx)?;
    if !out.success {
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
fn build_pane_command(cwd: &Path, argv: &[String], env: &[(String, String)]) -> String {
    let quoted: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
    let assignments = env
        .iter()
        .map(|(key, value)| format!("{key}={}", shell_quote(value)))
        .collect::<Vec<_>>()
        .join(" ");
    let env_prefix = if assignments.is_empty() {
        String::new()
    } else {
        // `exec` is a shell builtin, not an executable for `/usr/bin/env`.
        // Keep the environment setup in the shell, then let the shell's
        // `exec` replace itself with the backend. The previous `env ...
        // exec ...` form made every backend exit with `env: exec: No such
        // file or directory` as soon as it received any environment vars.
        format!("export {assignments} && ")
    };
    let inner = format!(
        "cd {} && {}exec {}",
        shell_quote(&cwd.to_string_lossy()),
        env_prefix,
        quoted.join(" "),
    );
    format!("exec \"${{SHELL:-/bin/sh}}\" -lc {}", shell_quote(&inner))
}

/// Backend process definition used when creating a tmux pane.
pub struct PaneCommand<'a> {
    pub argv: &'a [String],
    pub env: &'a [(String, String)],
}

/// Create a new detached tmux session that runs `argv` in `cwd`.
pub fn new_session(
    host: Option<&str>,
    id: &str,
    cwd: &Path,
    command: PaneCommand<'_>,
    cols: u16,
    rows: u16,
) -> Result<()> {
    if command.argv.is_empty() {
        bail!("new_session: empty argv");
    }
    let cmd = build_pane_command(cwd, command.argv, command.env);
    let name = session_name(id);
    let cols_s = cols.to_string();
    let rows_s = rows.to_string();
    let mut chained = vec!["start-server", ";"];
    chained.extend(setup_server_argv());
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
    ]);
    run(host, &chained, "new-session")
}

/// Configure options that must be in place before a new window is created.
/// `history-limit` is copied into each pane at creation time, so changing the
/// global default after `new-session` would not enlarge that pane's history.
/// The remaining commands turn mouse on and remove tmux's default drag
/// bindings from every key table that could process drag events.
///
/// Why both halves matter:
///
/// - `set -g mouse on` is required for tmux to react to wheel events
///   (its default `WheelUpPane` binding enters `copy-mode -e` and pages
///   through pane history — the only scroll mechanism that works inside
///   an alt-screen agent TUI, since xterm.js's own
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
fn setup_server_argv() -> Vec<&'static str> {
    let mut v = vec![
        "set-option",
        "-g",
        "history-limit",
        HISTORY_LIMIT_LINES,
        ";",
        "set-option",
        "-g",
        "mouse",
        "on",
    ];
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
/// Splices the server setup chunk inline so a brand-new tmux
/// server is configured before the first session lands.
pub fn create_session_with_log(
    host: Option<&str>,
    id: &str,
    cwd: &Path,
    command: PaneCommand<'_>,
    cols: u16,
    rows: u16,
    log_path: &Path,
) -> Result<()> {
    if command.argv.is_empty() {
        bail!("create_session_with_log: empty argv");
    }
    let cmd = build_pane_command(cwd, command.argv, command.env);
    let name = session_name(id);
    let cols_s = cols.to_string();
    let rows_s = rows.to_string();
    let pipe = log_pipe_command(log_path);
    // tmux's command separator `;` must be its own argv element. The remote
    // exec_tmux path shell-quotes each element, so the `;` survives the
    // round-trip as a literal arg to tmux (not a shell separator).
    let mut chained: Vec<&str> = vec!["start-server", ";"];
    chained.extend(setup_server_argv());
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
pub fn setup_server(host: Option<&str>) -> Result<()> {
    let mut chained: Vec<&str> = vec!["start-server", ";"];
    chained.extend(setup_server_argv());
    run(host, &chained, "setup server")
}

/// Tee all output of the session's pane to `log_path` on the host.
///
/// `-O` opens the pipe in overwrite mode so we don't append to a stale log
/// from a prior tmux session with the same id.
pub fn pipe_pane(host: Option<&str>, id: &str, log_path: &Path) -> Result<()> {
    let cmd = log_pipe_command(log_path);
    let name = session_name(id);
    run(host, &["pipe-pane", "-t", &name, "-O", &cmd], "pipe-pane")
}

fn log_pipe_command(log_path: &Path) -> String {
    let path = shell_quote(&log_path.to_string_lossy());
    format!(
        "umask 077; mkdir -p -- \"$(dirname -- {path})\" && touch -- {path} && chmod 600 -- {path} && exec cat >> {path}"
    )
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
    if out.success {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr_means_session_gone(&stderr) {
        return Ok(String::new());
    }
    bail!("tmux capture-pane failed: {}", stderr.trim());
}

/// Capture the complete rendered pane history for replay in a fresh browser
/// terminal. `-e` retains SGR styling, while CRLF line endings keep each row
/// anchored at column zero when xterm.js replays the snapshot. This path has
/// a larger, independent bound than ordinary tmux control commands because a
/// 20k-line pane history legitimately exceeds their 2 MiB budget.
pub fn capture_history(host: Option<&str>, id: &str) -> Result<Vec<u8>> {
    let name = session_name(id);
    let out = exec_tmux_with_limit(
        host,
        &["capture-pane", "-p", "-e", "-S", "-", "-t", &name],
        "capture history",
        crate::history::RENDERED_HISTORY_BYTES,
    )?;
    if !out.success {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr_means_session_gone(&stderr) {
            return Ok(Vec::new());
        }
        bail!("tmux capture history failed: {}", stderr.trim());
    }

    render_history(out.stdout)
}

fn render_history(bytes: Vec<u8>) -> Result<Vec<u8>> {
    let newline_count = bytes
        .iter()
        .enumerate()
        .filter(|(index, byte)| **byte == b'\n' && (*index == 0 || bytes[*index - 1] != b'\r'))
        .count();
    if bytes.len() + newline_count > crate::history::RENDERED_HISTORY_BYTES {
        bail!("tmux capture history exceeded its output limit");
    }
    let mut rendered = Vec::with_capacity(bytes.len() + newline_count);
    for byte in bytes {
        if byte == b'\n' && rendered.last() != Some(&b'\r') {
            rendered.push(b'\r');
        }
        rendered.push(byte);
    }
    Ok(rendered)
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
    if out.success {
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
    if !out.success {
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
    if !out.success {
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
fn attach_argv_with_flags(host: Option<&str>, id: &str, flags: Option<&str>) -> Vec<String> {
    let mut tmux_args = vec![
        "tmux".to_string(),
        "-L".to_string(),
        SERVER_LABEL.to_string(),
        "attach-session".to_string(),
    ];
    if let Some(flags) = flags {
        tmux_args.extend(["-f".to_string(), flags.to_string()]);
    }
    tmux_args.extend(["-t".to_string(), session_name(id)]);

    match host {
        None => tmux_args,
        Some(h) => {
            // Pass the tmux invocation as a single string so the remote
            // shell parses it. `-t` forces ssh to allocate a TTY.
            let remote = tmux_args
                .iter()
                .map(|part| shell_quote(part))
                .collect::<Vec<_>>()
                .join(" ");
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

pub fn attach_argv(host: Option<&str>, id: &str) -> Vec<String> {
    attach_argv_with_flags(host, id, None)
}

/// The daemon's lifecycle monitor must not constrain the window dimensions;
/// interactive browser attachments supply their own correctly sized PTYs.
pub fn monitor_argv(host: Option<&str>, id: &str) -> Vec<String> {
    attach_argv_with_flags(host, id, Some("ignore-size"))
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
    fn setup_server_argv_sets_history_before_mouse_and_unbinds_drag() {
        let argv = setup_server_argv();
        let history_pos = argv
            .windows(4)
            .position(|w| w == ["set-option", "-g", "history-limit", HISTORY_LIMIT_LINES])
            .expect("set-option -g history-limit chunk missing");
        // Order matters: tmux processes the chained command left-to-right,
        // so `set-option mouse on` (which reactivates default bindings if
        // they were unbound by a prior config) must come BEFORE the
        // unbinds — otherwise the unbinds wouldn't have anything to drop.
        let on_pos = argv
            .windows(4)
            .position(|w| w == ["set-option", "-g", "mouse", "on"])
            .expect("set-option -g mouse on chunk missing");
        assert!(history_pos < on_pos);
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
    fn rendered_history_uses_crlf_without_changing_escapes() {
        let rendered = render_history(b"\x1b[31mred\x1b[0m\nplain\r\n".to_vec()).unwrap();
        assert_eq!(rendered, b"\x1b[31mred\x1b[0m\r\nplain\r\n");
    }

    #[test]
    fn log_pipe_command_creates_private_files() {
        let command = log_pipe_command(Path::new("/tmp/slide logs/out.log"));
        assert!(command.contains("umask 077"));
        assert!(command.contains("mkdir -p"));
        assert!(command.contains("chmod 600"));
        assert!(command.contains("exec cat >>"));
        assert!(command.contains("slide logs/out.log"));
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
            &[],
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
            &[],
        );
        // The inner string is single-quoted once for the outer shell, so
        // embedded single quotes inside the cd path get the '\'' dance.
        assert!(cmd.contains("my projects/app"), "path mangled: {cmd}");
    }

    #[test]
    fn build_pane_command_applies_backend_environment() {
        let cmd = build_pane_command(
            Path::new("/home/u/proj"),
            &["opencode".to_string()],
            &[(
                "OPENCODE_PERMISSION".to_string(),
                r#"{"read":"allow"}"#.to_string(),
            )],
        );
        assert!(
            cmd.contains("export OPENCODE_PERMISSION="),
            "missing env: {cmd}"
        );
        assert!(
            cmd.contains("&& exec opencode"),
            "missing backend exec: {cmd}"
        );
        assert!(
            !cmd.contains("env OPENCODE_PERMISSION"),
            "env must not run shell builtin: {cmd}"
        );
        assert!(cmd.contains("opencode"), "missing backend: {cmd}");
    }

    #[test]
    fn build_pane_command_environment_reaches_backend() {
        let cmd = build_pane_command(
            Path::new("/tmp"),
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf '%s' \"$SLIDE_TMUX_ENV_TEST\"".to_string(),
            ],
            &[(
                "SLIDE_TMUX_ENV_TEST".to_string(),
                "value with spaces".to_string(),
            )],
        );
        let output = Command::new("/bin/sh")
            .args(["-c", &cmd])
            .env("SHELL", "/bin/sh")
            .output()
            .expect("run generated pane command");
        assert!(
            output.status.success(),
            "generated command failed: {output:?}"
        );
        assert_eq!(output.stdout, b"value with spaces");
    }

    #[test]
    fn attach_argv_local_is_tmux_direct() {
        let argv = attach_argv(None, "abc");
        assert_eq!(argv[0], "tmux");
        assert_eq!(argv.last().unwrap(), "slide-abc");
    }

    #[test]
    fn monitor_argv_does_not_constrain_interactive_clients() {
        let local = monitor_argv(None, "abc");
        assert!(local.windows(2).any(|args| args == ["-f", "ignore-size"]));

        let remote = monitor_argv(Some("host.example"), "abc");
        assert!(remote
            .last()
            .expect("remote command")
            .contains("attach-session -f ignore-size -t slide-abc"));
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
            PaneCommand {
                argv: &["sleep".to_string(), "60".to_string()],
                env: &[],
            },
            80,
            24,
        )
        .unwrap();
        pipe_pane(None, &id, &log).unwrap();

        assert_eq!(has_session(None, &id).unwrap(), SessionProbe::Present);
        let name = session_name(&id);
        let history = exec_tmux(
            None,
            &["display-message", "-p", "-t", &name, "#{history_limit}"],
            "display history-limit",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&history.stdout).trim(),
            HISTORY_LIMIT_LINES
        );
        let ids = list_session_ids(None).unwrap();
        assert!(ids.contains(&id));

        kill_session(None, &id).unwrap();
        assert_eq!(has_session(None, &id).unwrap(), SessionProbe::Absent);

        // kill_session on a gone session is a no-op.
        kill_session(None, &id).unwrap();
    }

    #[tokio::test]
    async fn monitor_client_does_not_constrain_interactive_attachment_size() {
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
            PaneCommand {
                argv: &["sleep".to_string(), "60".to_string()],
                env: &[],
            },
            80,
            24,
        )
        .unwrap();

        let monitor =
            crate::session::pty::spawn_sized(&monitor_argv(None, &id), tmp.path(), &[], 40, 10)
                .unwrap();
        let mut interactive =
            crate::session::pty::spawn_sized(&attach_argv(None, &id), tmp.path(), &[], 93, 27)
                .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), interactive.output.recv())
            .await
            .expect("tmux attachment produced no initial redraw")
            .expect("tmux attachment closed before its initial redraw");
        assert!(!first.is_empty());
        tokio::time::sleep(Duration::from_millis(150)).await;

        let name = session_name(&id);
        let clients = exec_tmux(
            None,
            &[
                "list-clients",
                "-t",
                &name,
                "-F",
                "#{client_width}x#{client_height} #{client_flags}",
            ],
            "list attached clients",
        )
        .unwrap();
        let clients = String::from_utf8_lossy(&clients.stdout);
        assert!(
            clients
                .lines()
                .any(|line| line.contains("40x10") && line.contains("ignore-size")),
            "{clients}"
        );
        assert!(
            clients.lines().any(|line| line.starts_with("93x27")),
            "{clients}"
        );

        let window = exec_tmux(
            None,
            &["display-message", "-p", "-t", &name, "#{window_width}"],
            "display window width",
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&window.stdout).trim(), "93");

        interactive.pty.kill();
        monitor.pty.kill();
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
            PaneCommand {
                argv: &["sleep".to_string(), "60".to_string()],
                env: &[],
            },
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

    #[test]
    fn capture_history_includes_scrollback_styles_and_crlf() {
        if !is_available() {
            eprintln!("tmux not on PATH; skipping");
            return;
        }
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let tmp = tempfile::tempdir().unwrap();
        let script = "i=1; while [ $i -le 40 ]; do printf '\\033[31mline-%02d\\033[0m\\n' \"$i\"; i=$((i + 1)); done; sleep 60";

        new_session(
            None,
            &id,
            tmp.path(),
            PaneCommand {
                argv: &["sh".to_string(), "-c".to_string(), script.to_string()],
                env: &[],
            },
            80,
            5,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let captured = capture_history(None, &id).unwrap();
        assert!(
            captured
                .windows(b"line-01".len())
                .any(|window| window == b"line-01"),
            "oldest scrollback row missing"
        );
        assert!(
            captured
                .windows(b"line-40".len())
                .any(|window| window == b"line-40"),
            "visible row missing"
        );
        assert!(captured.windows(5).any(|window| window == b"\x1b[31m"));
        assert!(captured
            .iter()
            .enumerate()
            .all(|(index, byte)| *byte != b'\n' || (index > 0 && captured[index - 1] == b'\r')));

        kill_session(None, &id).unwrap();
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
            PaneCommand {
                argv: &["sleep".to_string(), "60".to_string()],
                env: &[],
            },
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
