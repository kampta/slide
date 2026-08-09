//! Phone pairing helpers: enumerate non-loopback IPv4 addresses, render
//! pairing URLs, and emit ASCII QR codes that a phone can scan.
//!
//! The lockfile written by `server.rs` carries `bind` so `slide pair` can
//! re-derive the same set of URLs the daemon printed at startup, without
//! having to talk to the running daemon.

use anyhow::{anyhow, Context, Result};
use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;
use slide_core::config;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

/// One pairing URL plus a short label naming the interface (e.g. "100.x.y.z").
pub struct PairUrl {
    pub label: String,
    pub url: String,
}

/// Enumerate non-loopback IPv4 addresses, with Tailscale CGNAT addresses
/// (100.64.0.0/10) listed first so the most-likely-correct URL is the one
/// printed at the top.
pub fn lan_ipv4_addresses() -> Vec<Ipv4Addr> {
    let Ok(addrs) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut tailscale = Vec::new();
    let mut other = Vec::new();
    for iface in addrs {
        if iface.is_loopback() {
            continue;
        }
        if let IpAddr::V4(v4) = iface.ip() {
            if v4.is_link_local() {
                continue;
            }
            if is_tailscale_cgnat(v4) {
                tailscale.push(v4);
            } else {
                other.push(v4);
            }
        }
    }
    tailscale.sort();
    tailscale.dedup();
    other.sort();
    other.dedup();
    tailscale.into_iter().chain(other).collect()
}

/// Tailscale assigns from the 100.64.0.0/10 carrier-grade NAT range by
/// default. Detection is heuristic — anyone running their own CGNAT will
/// match — but it's good enough to pick a sensible "first" URL.
fn is_tailscale_cgnat(addr: Ipv4Addr) -> bool {
    let o = addr.octets();
    o[0] == 100 && (64..128).contains(&o[1])
}

/// Build the list of URLs the phone should be able to reach. If `bind` is
/// `0.0.0.0` / `::` (or unset / unparseable), enumerate non-loopback
/// interfaces. If `bind` is a specific non-loopback address, use only
/// that. Loopback binds yield no URLs (the phone can't reach 127.0.0.1).
//
// TODO: also enumerate IPv6 addresses for clients reachable only over
// IPv6 (e.g., Tailscale ULA-only setups). Today we walk `if-addrs` for
// IPv4 only, which is sufficient for typical home LAN + Tailscale4.
pub fn build_pair_urls(bind: &str, port: u16, token: &str) -> Vec<PairUrl> {
    let parsed: Option<IpAddr> = bind.parse().ok();
    match parsed {
        Some(ip) if ip.is_loopback() => Vec::new(),
        Some(IpAddr::V4(v4)) if v4.is_unspecified() => urls_for_all_interfaces(port, token),
        Some(IpAddr::V6(v6)) if v6.is_unspecified() => urls_for_all_interfaces(port, token),
        Some(ip) => vec![PairUrl {
            label: format!("{ip}"),
            url: format!("http://{ip}:{port}/?token={token}"),
        }],
        None => urls_for_all_interfaces(port, token),
    }
}

fn urls_for_all_interfaces(port: u16, token: &str) -> Vec<PairUrl> {
    lan_ipv4_addresses()
        .into_iter()
        .map(|ip| PairUrl {
            label: format!("{ip}"),
            url: format!("http://{ip}:{port}/?token={token}"),
        })
        .collect()
}

/// Render a URL as a QR code using unicode half-blocks. Returns the
/// multi-line string with no trailing newline.
pub fn render_qr(url: &str) -> Result<String> {
    let code = QrCode::new(url.as_bytes()).with_context(|| format!("encode QR for {url}"))?;
    Ok(code.render::<Dense1x2>().quiet_zone(true).build())
}

/// Print the LAN URLs (no token, no QR) plus a pointer to `slide pair`.
/// Used at startup when `--lan` exposes the daemon: matches the project's
/// "never write token-bearing URLs to stdout" stance from #32, while still
/// telling the operator how to pair a phone.
pub fn print_lan_summary(bind: &str, port: u16) {
    let urls = build_pair_urls(bind, port, "");
    if urls.is_empty() {
        return;
    }
    println!("  warning: daemon exposed to LAN — bearer token is the only");
    println!("  protection. Avoid running this on untrusted networks.");
    println!();
    println!("  reachable at:");
    for u in &urls {
        // `u.url` already contains `?token=` (with the empty placeholder);
        // strip the query so we only show the bare authority here.
        let bare = u
            .url
            .split_once('?')
            .map(|(prefix, _)| prefix)
            .unwrap_or(&u.url);
        println!("    {bare} ({})", u.label);
    }
    println!();
    println!("  to pair a phone, run `slide pair` (prints scannable QR codes).");
    println!();
}

/// Print the pairing block (URL + QR code per interface) to stdout.
/// Used by the explicit `slide pair` subcommand only — never on startup,
/// to keep the token-bearing URL out of every operator's scrollback.
/// Silent when there are no reachable URLs.
pub fn print_pair_section(bind: &str, port: u16, token: &str) {
    let urls = build_pair_urls(bind, port, token);
    if urls.is_empty() {
        return;
    }
    println!("  warning: bearer token is the only protection on the LAN.");
    println!("  Anyone who sees this QR (shoulder, screen recording) gets");
    println!("  full access until the daemon restarts.");
    println!();
    println!("  scan from your phone:");
    for u in &urls {
        println!();
        println!("    {} ({})", u.url, u.label);
        if let Ok(qr) = render_qr(&u.url) {
            for line in qr.lines() {
                println!("      {line}");
            }
        }
    }
    println!();
}

/// Subset of the daemon lockfile we care about for pairing.
pub struct LockInfo {
    pub bind: String,
    pub port: u16,
    pub token: String,
}

pub fn read_lock() -> Result<LockInfo> {
    read_lock_at(&config::lock_path())
}

fn read_lock_at(path: &Path) -> Result<LockInfo> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
    let token = v
        .get("token")
        .and_then(|s| s.as_str())
        .context("token missing in lockfile")?
        .to_string();
    let port = v
        .get("port")
        .and_then(|s| s.as_u64())
        .context("port missing in lockfile")? as u16;
    // `bind` was added later; lockfiles written by older daemons don't
    // carry it. Default to 0.0.0.0 so we still enumerate something useful;
    // the loopback warning in run_pair_cmd only fires when bind explicitly
    // resolves to a loopback address.
    let bind = v
        .get("bind")
        .and_then(|s| s.as_str())
        .unwrap_or("0.0.0.0")
        .to_string();
    Ok(LockInfo { bind, port, token })
}

/// Implementation of the `slide pair` subcommand. Errors (and exits with
/// non-zero) when the daemon is loopback-only or the lockfile is missing,
/// so scripts can detect the misconfiguration.
pub fn run_pair_cmd() -> Result<()> {
    let lock = read_lock().context("no running daemon (lockfile not found or unreadable)")?;
    let parsed: Option<IpAddr> = lock.bind.parse().ok();
    if matches!(parsed, Some(ip) if ip.is_loopback()) {
        return Err(anyhow!(
            "daemon is bound to {} — phone can't reach loopback. \
             Restart with `slide serve --lan` to expose to your LAN/Tailscale.",
            lock.bind
        ));
    }
    print_pair_section(&lock.bind, lock.port, &lock.token);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_pair_urls_loopback_yields_none() {
        let urls = build_pair_urls("127.0.0.1", 7777, "tok");
        assert!(urls.is_empty());
    }

    #[test]
    fn build_pair_urls_specific_address() {
        let urls = build_pair_urls("100.64.0.1", 7777, "tok");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].url, "http://100.64.0.1:7777/?token=tok");
    }

    #[test]
    fn render_qr_produces_nonempty_block() {
        let qr = render_qr("http://example.com/?token=abc").unwrap();
        assert!(qr.contains('\n'));
        assert!(qr.lines().count() > 5);
    }

    #[test]
    fn build_pair_urls_with_empty_token_still_produces_url() {
        // print_lan_summary passes "" as token; we want the function to
        // still return URLs (caller strips the query for display).
        let urls = build_pair_urls("100.64.0.1", 7777, "");
        assert_eq!(urls.len(), 1);
        assert!(urls[0].url.starts_with("http://100.64.0.1:7777/"));
    }

    #[test]
    fn tailscale_cgnat_detection() {
        assert!(is_tailscale_cgnat("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale_cgnat("100.127.255.1".parse().unwrap()));
        assert!(!is_tailscale_cgnat("100.63.0.1".parse().unwrap()));
        assert!(!is_tailscale_cgnat("100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale_cgnat("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn read_lock_at_parses_full_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        std::fs::write(
            &path,
            r#"{"pid":123,"bind":"100.64.0.5","port":7777,"token":"abc"}"#,
        )
        .unwrap();
        let lock = read_lock_at(&path).unwrap();
        assert_eq!(lock.bind, "100.64.0.5");
        assert_eq!(lock.port, 7777);
        assert_eq!(lock.token, "abc");
    }

    #[test]
    fn read_lock_at_defaults_bind_when_field_missing() {
        // Lockfiles written by older daemons don't carry `bind`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, r#"{"pid":1,"port":7777,"token":"abc"}"#).unwrap();
        let lock = read_lock_at(&path).unwrap();
        assert_eq!(lock.bind, "0.0.0.0");
    }

    #[test]
    fn read_lock_at_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, "not json").unwrap();
        assert!(read_lock_at(&path).is_err());
    }

    #[test]
    fn read_lock_at_rejects_missing_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, r#"{"pid":1,"port":7777}"#).unwrap();
        assert!(read_lock_at(&path).is_err());
    }
}
