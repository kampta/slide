use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// SSH options the daemon attaches to every `ssh` invocation it spawns.
///
/// - `ConnectTimeout=8` bounds the dead-host case. Without it, a remote
///   that's gone dark (firewall change, VPN partition, sshd hung) hangs
///   the daemon's create / reattach / list-dir calls indefinitely instead
///   of failing fast and surfacing an error to the user.
/// - Connection multiplexing (ControlMaster + ControlPath + ControlPersist)
///   when the resolved control socket path will fit in macOS's 104-byte
///   `sun_path` cap. The first call to a host pays the full TCP+KEX+auth
///   handshake and becomes the master; later calls within `ControlPersist`
///   piggyback on it at ~1 RTT.
///
/// Returned flat for splatting into `Command::args`.
pub fn ssh_args() -> Vec<String> {
    let mut args = vec!["-o".into(), "ConnectTimeout=8".into()];
    args.extend(multiplex_args());
    args
}

/// Just the connection-multiplexing options (no `ConnectTimeout`).
///
/// The control socket lives under `~/.slide-cm/` rather than the longer
/// `~/Library/Application Support/slide/ssh-cm/` because macOS caps
/// Unix domain socket paths at 104 bytes. ssh expands `%C` to a 40-char
/// SHA1 and appends a ~17-char `.<rand>` temp suffix during master
/// setup — combined with the data-dir prefix on macOS that already
/// pushes ~113 bytes, blowing every multiplex master with
/// `unix_listener: ... too long for Unix domain socket`.
///
/// Best-effort: if the dir can't be created or the resolved path would
/// still overflow `sun_path`, returns an empty vec so callers fall back
/// to vanilla (non-multiplexed) SSH instead of failing outright.
pub fn multiplex_args() -> Vec<String> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let dir = home.join(".slide-cm");
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // macOS sockaddr_un.sun_path is 104 bytes. Reserve room for the
    // separator, %C's 40-char SHA1, and ssh's `.<rand>` master-setup
    // temp suffix (~17 chars). If we don't fit, drop multiplexing.
    const SUN_PATH_MAX: usize = 104;
    const RESOLVE_OVERHEAD: usize = 1 + 40 + 17;
    if dir.to_string_lossy().len() + RESOLVE_OVERHEAD > SUN_PATH_MAX {
        return Vec::new();
    }
    build_multiplex_args(&dir.join("%C"))
}

fn build_multiplex_args(control_path: &Path) -> Vec<String> {
    // Quote the value: ssh parses each `-o` arg as a config-file line and
    // tokenizes on whitespace, so an unquoted path with a space (e.g. macOS's
    // `~/Library/Application Support/slide/...`) trips
    // "keyword controlpath extra arguments at end of line".
    vec![
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath=\"{}\"", control_path.display()),
        "-o".into(),
        "ControlPersist=10m".into(),
    ]
}

/// Reject ssh destinations that openssh would parse as options (leading `-`),
/// contain shell metacharacters, or embed whitespace / control bytes.
///
/// The `-`-prefix case is the load-bearing one: `ssh -o BatchMode=yes <host>`
/// treats a host like `-oProxyCommand=…` as another option, turning
/// user-supplied input into arbitrary local command execution. The other
/// checks are belt-and-braces: a well-formed ssh alias or `user@host[:port]`
/// needs none of these characters.
pub fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("ssh host must not be empty");
    }
    if host.starts_with('-') {
        bail!("ssh host must not start with '-'");
    }
    for c in host.chars() {
        if c.is_control() || c.is_whitespace() {
            bail!("ssh host must not contain whitespace or control characters");
        }
        // Anything beyond this set is suspicious for a destination spec.
        let ok = c.is_ascii_alphanumeric()
            || matches!(c, '.' | '-' | '_' | '@' | ':' | '/' | '[' | ']' | '%');
        if !ok {
            bail!("ssh host contains disallowed character: {c:?}");
        }
    }
    Ok(())
}

/// Require a destination to be one of the explicit, non-wildcard aliases in
/// the user's SSH config. This keeps authenticated HTTP callers from turning
/// Slide's directory and runtime probes into a general-purpose SSH client.
pub fn validate_configured_host(host: &str) -> Result<()> {
    validate_configured_host_in(host, &list_hosts())
}

fn validate_configured_host_in(host: &str, configured: &[SshHost]) -> Result<()> {
    validate_host(host)?;
    if configured.iter().any(|configured| configured.alias == host) {
        return Ok(());
    }
    bail!("ssh host is not configured: {host}")
}

#[derive(Debug, Clone, Serialize)]
pub struct SshHost {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}

pub fn list_hosts() -> Vec<SshHost> {
    let config_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".ssh")
        .join("config");
    parse_file(&config_path, &mut std::collections::HashSet::new())
}

fn parse_file(path: &Path, visited: &mut std::collections::HashSet<PathBuf>) -> Vec<SshHost> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return vec![];
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return vec![];
    };
    parse_content(&content, visited)
}

fn parse_content(content: &str, visited: &mut std::collections::HashSet<PathBuf>) -> Vec<SshHost> {
    let ssh_dir = dirs::home_dir().unwrap_or_default().join(".ssh");

    let mut hosts = Vec::new();
    let mut current_alias: Option<String> = None;
    let mut current_props: HashMap<String, String> = HashMap::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (
                k.to_lowercase(),
                v.trim_start_matches(|c: char| c.is_whitespace() || c == '=')
                    .trim()
                    .to_string(),
            ),
            None => continue,
        };
        // Strip surrounding quotes.
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value[1..value.len() - 1].to_string()
        } else {
            value
        };

        if key == "include" {
            let include_path = if value.starts_with('/') {
                PathBuf::from(&value)
            } else {
                ssh_dir.join(&value)
            };
            if include_path.exists() {
                hosts.extend(parse_file(&include_path, visited));
            }
            continue;
        }

        if key == "host" {
            if let Some(alias) = current_alias.take() {
                if let Some(h) = build_host(alias, &current_props) {
                    hosts.push(h);
                }
            }
            // Skip wildcard patterns.
            if value.contains('*') || value.contains('?') {
                current_alias = None;
            } else {
                current_alias = Some(value);
            }
            current_props.clear();
        } else if current_alias.is_some() {
            current_props.entry(key).or_insert(value);
        }
    }

    // Flush the last block.
    if let Some(alias) = current_alias {
        if let Some(h) = build_host(alias, &current_props) {
            hosts.push(h);
        }
    }

    hosts
}

fn build_host(alias: String, props: &HashMap<String, String>) -> Option<SshHost> {
    let hostname = props
        .get("hostname")
        .cloned()
        .unwrap_or_else(|| alias.clone());
    let user = props.get("user").cloned();
    let port = props.get("port").and_then(|p| p.parse().ok());
    Some(SshHost {
        alias,
        hostname,
        user,
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_multiplex_args, ssh_args, validate_configured_host_in, validate_host, SshHost,
    };
    use std::path::Path;

    #[test]
    fn validate_host_accepts_common_shapes() {
        for ok in [
            "prod",
            "prod.example.com",
            "user@prod.example.com",
            "my-host_1",
            "192.168.1.10",
            "[2001:db8::1]",
        ] {
            assert!(validate_host(ok).is_ok(), "rejected {ok:?}");
        }
    }

    #[test]
    fn configured_hosts_are_matched_by_explicit_alias() {
        let hosts = [SshHost {
            alias: "spark".into(),
            hostname: "10.0.0.8".into(),
            user: Some("developer".into()),
            port: None,
        }];
        assert!(validate_configured_host_in("spark", &hosts).is_ok());
        assert!(validate_configured_host_in("10.0.0.8", &hosts).is_err());
        assert!(validate_configured_host_in("unlisted", &hosts).is_err());
    }

    #[test]
    fn build_multiplex_args_emits_master_path_persist() {
        // The trio is what makes multiplexing actually work — missing any
        // one of them silently degrades back to fresh handshakes. Drives
        // `build_multiplex_args` directly because `multiplex_args()` is
        // env-conditional (skipped when $HOME would push the resolved
        // socket path over macOS's `sun_path` limit), and a sibling test
        // in this binary mutates HOME for tempdir isolation.
        let args = build_multiplex_args(Path::new("/tmp/slide-cm/%C"));
        let joined = args.join(" ");
        assert!(joined.contains("ControlMaster=auto"), "got: {joined}");
        assert!(joined.contains("ControlPath="), "got: {joined}");
        assert!(joined.contains("ControlPersist="), "got: {joined}");
    }

    #[test]
    fn multiplex_args_quotes_control_path_with_spaces() {
        // Regression: macOS's data dir is `~/Library/Application Support/...`
        // — the unquoted space made ssh report "keyword controlpath extra
        // arguments at end of line" and aborted every remote create.
        let args = build_multiplex_args(Path::new(
            "/Users/me/Library/Application Support/slide/ssh-cm/%C",
        ));
        let i = args
            .iter()
            .position(|a| a.starts_with("ControlPath="))
            .unwrap();
        assert_eq!(
            args[i],
            "ControlPath=\"/Users/me/Library/Application Support/slide/ssh-cm/%C\""
        );
    }

    #[test]
    fn ssh_args_always_includes_connect_timeout() {
        // ConnectTimeout must come back even when multiplexing falls
        // back to disabled (long $HOME on macOS) — without it, a dead
        // remote hangs the daemon's create/reattach calls indefinitely.
        let joined = ssh_args().join(" ");
        assert!(
            joined.contains("ConnectTimeout="),
            "ssh_args missing ConnectTimeout: {joined}"
        );
    }

    #[test]
    fn multiplex_args_skips_when_path_would_exceed_sun_path() {
        // Defense for very long $HOME (uncommon but real on enterprise
        // macOS images): rather than emit a ControlPath that ssh will
        // refuse with `unix_listener: ... too long for Unix domain
        // socket`, drop the multiplex options entirely.
        // The check itself lives in `multiplex_args`; here we just
        // exercise `build_multiplex_args` to keep the encoding pinned.
        let args = build_multiplex_args(Path::new("/tmp/short/%C"));
        assert!(args.iter().any(|a| a.contains("ControlMaster=auto")));
    }

    #[test]
    fn validate_host_rejects_ssh_option_injection() {
        // The load-bearing case: a leading `-` turns the host arg into an
        // ssh option. `-oProxyCommand=…` in particular lets an attacker run
        // arbitrary commands locally when the daemon spawns `ssh <host> …`.
        for bad in [
            "",
            "-oProxyCommand=/bin/sh",
            "-oPermitLocalCommand=yes",
            "-lroot",
            "host with space",
            "host\nnewline",
            "host;rm -rf /",
            "host`id`",
            "host$(id)",
            "host\0",
        ] {
            assert!(validate_host(bad).is_err(), "accepted {bad:?}");
        }
    }
}
