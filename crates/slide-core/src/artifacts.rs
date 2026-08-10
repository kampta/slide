use crate::session::{Location, Session};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 50;
pub const MAX_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;
const REMOTE_STDERR_BYTES: usize = 16 * 1024;
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Artifact {
    pub id: usize,
    pub filename: String,
    pub title: Option<String>,
    pub text: Option<String>,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ArtifactList {
    pub manifest_present: bool,
    pub artifacts: Vec<Artifact>,
    pub unavailable: usize,
}

pub struct ArtifactPayload {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    files: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ManifestEntry {
    Path(String),
    Detail {
        path: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        text: Option<String>,
    },
}

impl ManifestEntry {
    fn parts(&self) -> (&str, Option<&str>, Option<&str>) {
        match self {
            Self::Path(path) => (path, None, None),
            Self::Detail { path, title, text } => (path, title.as_deref(), text.as_deref()),
        }
    }
}

pub fn manifest_path(session: &Session) -> String {
    match session.location {
        Location::Local => crate::config::artifact_manifest_path(&session.id)
            .to_string_lossy()
            .into_owned(),
        Location::Remote => format!("/tmp/slide-artifacts-{}.json", session.id),
    }
}

pub fn list(session: &Session) -> Result<ArtifactList> {
    let Some(manifest) = read_manifest(session)? else {
        return Ok(ArtifactList {
            manifest_present: false,
            artifacts: Vec::new(),
            unavailable: 0,
        });
    };
    let total = manifest.files.len();
    let entries = manifest
        .files
        .into_iter()
        .take(MAX_MANIFEST_ENTRIES)
        .enumerate()
        .filter_map(|(id, entry)| candidate(id, entry))
        .collect::<Vec<_>>();
    let mut unavailable = total.saturating_sub(entries.len());
    let sizes = match session.location {
        Location::Local => local_sizes(session, &entries),
        Location::Remote => remote_sizes(session, &entries)?,
    };
    let mut artifacts = Vec::with_capacity(entries.len());
    let mut seen = HashSet::new();
    for candidate in entries {
        let Some(size) = sizes.get(&candidate.id).copied() else {
            unavailable += 1;
            continue;
        };
        if size > MAX_ARTIFACT_BYTES as u64 || !seen.insert(candidate.path.clone()) {
            unavailable += 1;
            continue;
        }
        artifacts.push(Artifact {
            id: candidate.id,
            filename: candidate.filename,
            title: candidate.title,
            text: candidate.text,
            content_type: candidate.content_type.to_string(),
            size,
        });
    }
    Ok(ArtifactList {
        manifest_present: true,
        artifacts,
        unavailable,
    })
}

pub fn load(session: &Session, artifact_id: usize) -> Result<ArtifactPayload> {
    if artifact_id >= MAX_MANIFEST_ENTRIES {
        bail!("artifact not found");
    }
    let manifest = read_manifest(session)?.context("artifact manifest not found")?;
    let entry = manifest
        .files
        .into_iter()
        .nth(artifact_id)
        .context("artifact not found")?;
    let candidate = candidate(artifact_id, entry).context("artifact is unavailable")?;
    let bytes = match session.location {
        Location::Local => read_local(session, &candidate.path)?,
        Location::Remote => read_remote(session, &candidate.path)?,
    };
    Ok(ArtifactPayload {
        content_type: candidate.content_type.to_string(),
        bytes,
    })
}

#[derive(Debug)]
struct Candidate {
    id: usize,
    path: String,
    filename: String,
    title: Option<String>,
    text: Option<String>,
    content_type: &'static str,
}

fn candidate(id: usize, entry: ManifestEntry) -> Option<Candidate> {
    let (path, title, text) = entry.parts();
    let path = safe_relative_path(path)?;
    let filename = Path::new(&path).file_name()?.to_str()?.to_string();
    let content_type = content_type(&filename)?;
    Some(Candidate {
        id,
        path,
        filename,
        title: bounded_text(title, 120),
        text: bounded_text(text, 500),
        content_type,
    })
}

fn safe_relative_path(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > 1_024 || value.chars().any(char::is_control) {
        return None;
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(value.to_string())
}

fn bounded_text(value: Option<&str>, limit: usize) -> Option<String> {
    let compact = crate::terminal_text::compact(value?);
    if compact.is_empty() {
        return None;
    }
    Some(compact.chars().take(limit).collect())
}

fn read_manifest(session: &Session) -> Result<Option<Manifest>> {
    let bytes = match session.location {
        Location::Local => {
            let path = PathBuf::from(manifest_path(session));
            let mut file = match File::open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            read_bounded(&mut file, MAX_MANIFEST_BYTES, "artifact manifest")?
        }
        Location::Remote => {
            let host = session
                .ssh_host
                .as_deref()
                .context("remote session missing SSH host")?;
            crate::ssh::validate_host(host)?;
            let script = "test -f \"$1\" || exit 3; cat \"$1\"";
            let remote = format!(
                "sh -c {} sh {}",
                shell_quote(script),
                shell_quote(&manifest_path(session)),
            );
            let output = run_remote(host, remote, MAX_MANIFEST_BYTES)?;
            if output.code == Some(3) {
                return Ok(None);
            }
            ensure_remote_success(&output, "artifact manifest")?;
            output.stdout
        }
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).context("parse artifact manifest")?;
    Ok(Some(manifest))
}

fn local_sizes(session: &Session, entries: &[Candidate]) -> HashMap<usize, u64> {
    let Ok(root) = Path::new(&session.project_path).canonicalize() else {
        return HashMap::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            resolve_local_from_root(&root, &entry.path)
                .ok()
                .and_then(|path| path.metadata().ok())
                .filter(|metadata| metadata.is_file())
                .map(|metadata| (entry.id, metadata.len()))
        })
        .collect()
}

fn resolve_local(session: &Session, relative: &str) -> Result<PathBuf> {
    let root = Path::new(&session.project_path)
        .canonicalize()
        .context("resolve artifact root")?;
    resolve_local_from_root(&root, relative)
}

fn resolve_local_from_root(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = root
        .join(relative)
        .canonicalize()
        .context("resolve artifact")?;
    if path == root || !path.starts_with(root) {
        bail!("artifact escapes the session worktree");
    }
    Ok(path)
}

fn read_local(session: &Session, relative: &str) -> Result<Vec<u8>> {
    let path = resolve_local(session, relative)?;
    let mut file = File::open(path).context("open artifact")?;
    read_bounded(&mut file, MAX_ARTIFACT_BYTES, "artifact")
}

fn read_bounded(reader: &mut impl Read, limit: usize, label: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    reader.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("{label} exceeds the {} MiB limit", limit / (1024 * 1024));
    }
    Ok(bytes)
}

fn remote_sizes(session: &Session, entries: &[Candidate]) -> Result<HashMap<usize, u64>> {
    if entries.is_empty() {
        return Ok(HashMap::new());
    }
    let host = session
        .ssh_host
        .as_deref()
        .context("remote session missing SSH host")?;
    crate::ssh::validate_host(host)?;
    const SCRIPT: &str = r#"set -eu
root=$(cd -- "$1" 2>/dev/null && pwd -P) || exit 4
shift
while [ "$#" -ge 2 ]; do
  index=$1
  path=$2
  shift 2
  target=$(realpath "$root/$path" 2>/dev/null) || continue
  case "$target" in "$root"/*) ;; *) continue ;; esac
  [ -f "$target" ] || continue
  size=$(wc -c < "$target" 2>/dev/null) || continue
  printf '%s\t%s\n' "$index" "$size"
done"#;
    let mut parts = vec![
        "sh".to_string(),
        "-c".to_string(),
        SCRIPT.to_string(),
        "sh".to_string(),
        session.project_path.clone(),
    ];
    for entry in entries {
        parts.push(entry.id.to_string());
        parts.push(entry.path.clone());
    }
    let remote = parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    let output = run_remote(host, remote, MAX_MANIFEST_ENTRIES * 64)?;
    ensure_remote_success(&output, "artifact metadata")?;
    let mut sizes = HashMap::new();
    for line in output.stdout.split(|byte| *byte == b'\n') {
        let Some(separator) = line.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let (id, size) = (&line[..separator], &line[separator + 1..]);
        let id = std::str::from_utf8(id)
            .ok()
            .and_then(|value| value.parse().ok());
        let size = std::str::from_utf8(size)
            .ok()
            .and_then(|value| value.trim().parse().ok());
        if let (Some(id), Some(size)) = (id, size) {
            sizes.insert(id, size);
        }
    }
    Ok(sizes)
}

fn read_remote(session: &Session, relative: &str) -> Result<Vec<u8>> {
    let host = session
        .ssh_host
        .as_deref()
        .context("remote session missing SSH host")?;
    crate::ssh::validate_host(host)?;
    const SCRIPT: &str = r#"set -eu
root=$(cd -- "$1" 2>/dev/null && pwd -P) || exit 4
target=$(realpath "$root/$2" 2>/dev/null) || exit 5
case "$target" in "$root"/*) ;; *) exit 5 ;; esac
[ -f "$target" ] || exit 5
size=$(wc -c < "$target")
[ "$size" -le "$3" ] || exit 6
cat "$target""#;
    let limit = MAX_ARTIFACT_BYTES.to_string();
    let parts = [
        "sh",
        "-c",
        SCRIPT,
        "sh",
        &session.project_path,
        relative,
        &limit,
    ];
    let remote = parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    let output = run_remote(host, remote, MAX_ARTIFACT_BYTES)?;
    if output.code == Some(6) || output.stdout_truncated {
        bail!(
            "artifact exceeds the {} MiB limit",
            MAX_ARTIFACT_BYTES / (1024 * 1024)
        );
    }
    ensure_remote_success(&output, "artifact")?;
    Ok(output.stdout)
}

fn run_remote(
    host: &str,
    remote: String,
    stdout_limit: usize,
) -> Result<crate::process::BoundedOutput> {
    let mut command = Command::new("ssh");
    command
        .args(["-o", "BatchMode=yes"])
        .args(crate::ssh::ssh_args())
        .arg(host)
        .arg(remote);
    crate::process::run_bounded(command, stdout_limit, REMOTE_STDERR_BYTES, REMOTE_TIMEOUT)
}

fn ensure_remote_success(output: &crate::process::BoundedOutput, label: &str) -> Result<()> {
    if output.timed_out || output.stderr_truncated || output.stdout_truncated || !output.success {
        bail!("remote {label} is unavailable");
    }
    Ok(())
}

fn content_type(filename: &str) -> Option<&'static str> {
    let extension = Path::new(filename)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        _ => return None,
    })
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

    fn session(root: &Path) -> Session {
        Session {
            id: "artifact-test".to_string(),
            name: "artifact-test".to_string(),
            backend: BackendKind::Codex,
            location: Location::Local,
            ssh_host: None,
            base_dir: root.to_string_lossy().into_owned(),
            project_path: root.to_string_lossy().into_owned(),
            worktree: false,
            state: SessionState::Waiting,
            created_at: 1,
            last_activity: 1,
            supervisor: SupervisorKind::Direct,
            host_log_path: None,
            log_offset: 0,
            backend_session_id: None,
            parent_session_id: None,
        }
    }

    #[test]
    fn manifest_entries_resolve_and_load_bounded_media_without_paths() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plot.png"), b"png-bytes").unwrap();
        let source = session(root.path());
        let manifest: Manifest = serde_json::from_str(
            r#"{"files":[{"path":"plot.png","title":"Results","text":"Latest plot"}]}"#,
        )
        .unwrap();
        let entry = candidate(0, manifest.files.into_iter().next().unwrap()).unwrap();
        let sizes = local_sizes(&source, std::slice::from_ref(&entry));

        assert_eq!(entry.filename, "plot.png");
        assert_eq!(entry.content_type, "image/png");
        assert_eq!(sizes[&0], 9);
        assert_eq!(read_local(&source, &entry.path).unwrap(), b"png-bytes");
    }

    #[test]
    fn paths_cannot_escape_or_publish_unrecognized_files() {
        assert!(safe_relative_path("../secret.png").is_none());
        assert!(safe_relative_path("/tmp/secret.png").is_none());
        assert!(candidate(0, ManifestEntry::Path("notes.rs".to_string())).is_none());
        assert!(candidate(0, ManifestEntry::Path("nested/plot.png".to_string())).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_the_session_worktree() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.png"), b"secret").unwrap();
        symlink(
            outside.path().join("secret.png"),
            root.path().join("plot.png"),
        )
        .unwrap();

        assert!(read_local(&session(root.path()), "plot.png").is_err());
    }
}
