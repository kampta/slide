//! CLI presentation for secure phone pairing.

use crate::pairing::{validate_public_url, PairingStore};
use anyhow::{bail, Context, Result};
use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;
use serde::Deserialize;
use slide_core::config;
use std::path::Path;

#[derive(Deserialize)]
pub struct LockInfo {
    pub public_url: Option<String>,
}

fn read_lock_at(path: &Path) -> Result<LockInfo> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

fn pairing_store() -> Result<PairingStore> {
    PairingStore::open(config::data_dir().join("pairing.json"))
}

pub fn render_qr(url: &str) -> Result<String> {
    let code = QrCode::new(url.as_bytes()).with_context(|| "encode pairing QR")?;
    Ok(code.render::<Dense1x2>().quiet_zone(true).build())
}

pub fn run_pair_cmd() -> Result<()> {
    let lock = read_lock_at(&config::lock_path())
        .context("no running daemon (lockfile not found or unreadable)")?;
    let public_url = lock.public_url.context(
        "phone pairing is disabled; restart with `slide serve --public-url https://...`",
    )?;
    let public_url = validate_public_url(&public_url)?;
    let secret = pairing_store()?.create_ticket()?;
    let url = format!("{public_url}/#pair={secret}");

    println!("\n  scan this single-use QR within 5 minutes:\n");
    println!("    {url}\n");
    for line in render_qr(&url)?.lines() {
        println!("      {line}");
    }
    println!();
    Ok(())
}

pub fn ensure_phone_pairing_is_safe(
    bind: &str,
    public_url: Option<&str>,
) -> Result<Option<String>> {
    let public_url = public_url.map(validate_public_url).transpose()?;
    let loopback = bind
        .parse::<std::net::IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(bind == "localhost");
    if !loopback {
        bail!(
            "refusing insecure non-loopback HTTP; keep Slide on loopback and use --public-url with an HTTPS reverse proxy"
        );
    }
    Ok(public_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_public_url_is_optional_for_local_only_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.lock");
        std::fs::write(&path, r#"{"pid":1,"port":7777,"token":"abc"}"#).unwrap();
        assert!(read_lock_at(&path).unwrap().public_url.is_none());
    }

    #[test]
    fn pairing_requires_loopback_and_https() {
        assert_eq!(
            ensure_phone_pairing_is_safe("127.0.0.1", Some("https://slide.example"))
                .unwrap()
                .as_deref(),
            Some("https://slide.example")
        );
        assert!(ensure_phone_pairing_is_safe("0.0.0.0", Some("https://slide.example")).is_err());
        assert!(ensure_phone_pairing_is_safe("127.0.0.1", Some("http://slide.example")).is_err());
    }

    #[test]
    fn pairing_url_uses_a_fragment() {
        let secret = "secret";
        let url = format!("https://slide.example/#pair={secret}");
        assert!(!url.contains("?"));
        assert!(url.ends_with("#pair=secret"));
    }
}
