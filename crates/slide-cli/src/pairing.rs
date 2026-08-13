//! Persistent phone credentials and short-lived, single-use pairing tickets.
//!
//! Only SHA-256 digests are written to disk. The cleartext ticket is returned
//! once to the CLI, and device credentials exist only in the browser's
//! host-only HttpOnly cookie.

use anyhow::{bail, Context, Result};
use axum::http::header::COOKIE;
use axum::http::{HeaderMap, HeaderValue, Uri};
use fs2::FileExt;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const COOKIE_NAME: &str = "__Host-slide_device";
const TICKET_TTL_SECS: u64 = 5 * 60;
const DEVICE_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const MAX_DEVICES: usize = 32;

#[derive(Clone)]
pub struct PairingStore {
    path: PathBuf,
    state: Arc<Mutex<PersistedState>>,
}

#[derive(Default, Deserialize, Serialize)]
struct PersistedState {
    tickets: Vec<Ticket>,
    devices: Vec<Device>,
}

#[derive(Deserialize, Serialize)]
struct Ticket {
    hash: String,
    expires_at: u64,
}

#[derive(Deserialize, Serialize)]
struct Device {
    hash: String,
    created_at: u64,
    expires_at: u64,
}

impl PairingStore {
    pub fn open(path: PathBuf) -> Result<Self> {
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        Ok(Self {
            path,
            state: Arc::new(Mutex::new(state)),
        })
    }

    pub fn create_ticket(&self) -> Result<String> {
        self.create_ticket_at(now_secs())
    }

    fn create_ticket_at(&self, now: u64) -> Result<String> {
        let secret = random_secret();
        let _mutation_lock = self.mutation_lock()?;
        let mut state = self.lock_state()?;
        self.reload(&mut state)?;
        state.tickets.retain(|ticket| ticket.expires_at > now);
        state.tickets.push(Ticket {
            hash: secret_hash(&secret),
            expires_at: now.saturating_add(TICKET_TTL_SECS),
        });
        self.save(&state)?;
        Ok(secret)
    }

    pub fn redeem(&self, ticket: &str) -> Result<Option<String>> {
        self.redeem_at(ticket, now_secs())
    }

    fn redeem_at(&self, ticket: &str, now: u64) -> Result<Option<String>> {
        let hash = secret_hash(ticket);
        let _mutation_lock = self.mutation_lock()?;
        let mut state = self.lock_state()?;
        self.reload(&mut state)?;
        let Some(index) = state.tickets.iter().position(|candidate| {
            candidate.expires_at > now && constant_time_eq(&candidate.hash, &hash)
        }) else {
            let before = state.tickets.len();
            state.tickets.retain(|candidate| candidate.expires_at > now);
            if state.tickets.len() != before {
                self.save(&state)?;
            }
            return Ok(None);
        };

        state.tickets.remove(index);
        let credential = random_secret();
        state.devices.push(Device {
            hash: secret_hash(&credential),
            created_at: now,
            expires_at: now.saturating_add(DEVICE_TTL_SECS),
        });
        state.devices.retain(|device| device.expires_at > now);
        state.devices.sort_by_key(|device| device.created_at);
        let excess = state.devices.len().saturating_sub(MAX_DEVICES);
        state.devices.drain(..excess);
        self.save(&state)?;
        Ok(Some(credential))
    }

    pub fn authenticate(&self, credential: &str) -> bool {
        self.authenticate_at(credential, now_secs())
    }

    fn authenticate_at(&self, credential: &str, now: u64) -> bool {
        let hash = secret_hash(credential);
        self.state
            .lock()
            .map(|state| {
                state
                    .devices
                    .iter()
                    .any(|device| device.expires_at > now && constant_time_eq(&device.hash, &hash))
            })
            .unwrap_or(false)
    }

    fn save(&self, state: &PersistedState) -> Result<()> {
        let bytes = serde_json::to_vec(state)?;
        write_secret_file(&self.path, &bytes)
    }

    fn reload(&self, state: &mut PersistedState) -> Result<()> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                *state = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse {}", self.path.display()))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *state = PersistedState::default();
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.path.display()));
            }
        }
        Ok(())
    }

    fn mutation_lock(&self) -> Result<std::fs::File> {
        let path = self.path.with_extension("lock");
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
        };
        #[cfg(not(unix))]
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path);
        let file = file.with_context(|| format!("open {}", path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(file)
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, PersistedState>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state lock poisoned"))
    }
}

pub fn cookie_credential(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| (name == COOKIE_NAME).then_some(value))
}

pub fn device_cookie(credential: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={credential}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age={DEVICE_TTL_SECS}"
    ))
    .context("generated device credential is not a valid cookie")
}

fn secret_hash(secret: &str) -> String {
    hex(&Sha256::digest(secret.as_bytes()))
}

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex(&bytes)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn write_secret_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;

    let parent = path.parent().context("pairing state path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&temporary);

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;

    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn validate_public_url(value: &str) -> Result<String> {
    let value = value.trim();
    let uri: Uri = value.parse().context("--public-url is not a valid URL")?;
    if uri.scheme_str() != Some("https") || uri.authority().is_none() {
        bail!("--public-url must be an https:// URL served by a trusted reverse proxy")
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        bail!("--public-url must contain only an HTTPS origin (for example https://slide.example.ts.net)")
    }
    Ok(value.trim_end_matches('/').to_string())
}

pub fn public_url_host(value: &str) -> Result<String> {
    let value = validate_public_url(value)?;
    let uri: Uri = value.parse().context("--public-url is not a valid URL")?;
    uri.authority()
        .map(|authority| authority.host().to_string())
        .context("--public-url has no host")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PairingStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = PairingStore::open(dir.path().join("pairing.json")).unwrap();
        (dir, store)
    }

    #[test]
    fn ticket_is_single_use_and_creates_a_device() {
        let (_dir, store) = store();
        let ticket = store.create_ticket_at(100).unwrap();
        let credential = store.redeem_at(&ticket, 101).unwrap().unwrap();
        assert!(store.authenticate_at(&credential, 101));
        assert!(store.redeem_at(&ticket, 102).unwrap().is_none());
    }

    #[test]
    fn expired_ticket_cannot_be_redeemed() {
        let (_dir, store) = store();
        let ticket = store.create_ticket_at(100).unwrap();
        assert!(store
            .redeem_at(&ticket, 100 + TICKET_TTL_SECS)
            .unwrap()
            .is_none());
    }

    #[test]
    fn state_contains_hashes_not_secrets() {
        let (dir, store) = store();
        let ticket = store.create_ticket_at(100).unwrap();
        let credential = store.redeem_at(&ticket, 101).unwrap().unwrap();
        let persisted = std::fs::read_to_string(dir.path().join("pairing.json")).unwrap();
        assert!(!persisted.contains(&ticket));
        assert!(!persisted.contains(&credential));
    }

    #[test]
    fn device_credentials_are_bounded() {
        let (dir, store) = store();
        let mut credentials = Vec::new();
        for now in 0..(MAX_DEVICES as u64 + 2) {
            let ticket = store.create_ticket_at(now).unwrap();
            credentials.push(store.redeem_at(&ticket, now).unwrap().unwrap());
        }
        let now = MAX_DEVICES as u64 + 2;
        assert!(!store.authenticate_at(&credentials[0], now));
        assert!(!store.authenticate_at(&credentials[1], now));
        assert!(store.authenticate_at(credentials.last().unwrap(), now));
        let persisted: PersistedState =
            serde_json::from_slice(&std::fs::read(dir.path().join("pairing.json")).unwrap())
                .unwrap();
        assert_eq!(persisted.devices.len(), MAX_DEVICES);
    }

    #[test]
    fn device_credential_expires() {
        let (_dir, store) = store();
        let ticket = store.create_ticket_at(100).unwrap();
        let credential = store.redeem_at(&ticket, 100).unwrap().unwrap();
        assert!(store.authenticate_at(&credential, 100 + DEVICE_TTL_SECS - 1));
        assert!(!store.authenticate_at(&credential, 100 + DEVICE_TTL_SECS));
    }

    #[cfg(unix)]
    #[test]
    fn state_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, store) = store();
        store.create_ticket_at(100).unwrap();
        let mode = std::fs::metadata(dir.path().join("pairing.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn cookie_has_browser_security_attributes() {
        let cookie = device_cookie("secret")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            cookie,
            "__Host-slide_device=secret; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=2592000"
        );
    }

    #[test]
    fn cookie_parser_finds_device_credential() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            "other=x; __Host-slide_device=secret".parse().unwrap(),
        );
        assert_eq!(cookie_credential(&headers), Some("secret"));
    }

    #[test]
    fn public_url_requires_a_bare_https_origin() {
        assert_eq!(
            validate_public_url("https://slide.example.ts.net/").unwrap(),
            "https://slide.example.ts.net"
        );
        assert!(validate_public_url("http://slide.example.ts.net").is_err());
        assert!(validate_public_url("https://slide.example.ts.net/path").is_err());
        assert_eq!(
            public_url_host("https://slide.example.ts.net:8443").unwrap(),
            "slide.example.ts.net"
        );
    }
}
