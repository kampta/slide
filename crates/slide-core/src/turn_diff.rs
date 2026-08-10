use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MAX_PATCH_BYTES: usize = 256 * 1024;
const MAX_STAT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TurnDiffSummary {
    pub id: i64,
    pub turn: i64,
    pub started_at: i64,
    pub completed_at: i64,
    pub files_changed: u64,
    pub additions: u64,
    pub deletions: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TurnDiff {
    #[serde(flatten)]
    pub summary: TurnDiffSummary,
    pub patch: String,
}

#[derive(Debug, Clone)]
pub struct NewTurnDiff {
    pub started_at: i64,
    pub completed_at: i64,
    pub files_changed: u64,
    pub additions: u64,
    pub deletions: u64,
    pub truncated: bool,
    pub patch: String,
}

#[derive(Debug, Clone)]
pub struct RepoTarget {
    pub path: PathBuf,
    pub ssh_host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSnapshot {
    tree: String,
}

impl RepoSnapshot {
    pub(crate) fn tree_id(&self) -> &str {
        &self.tree
    }

    #[cfg(test)]
    fn new(tree: impl Into<String>) -> Self {
        Self { tree: tree.into() }
    }
}

type CommandOutput = crate::process::BoundedOutput;

/// Snapshot the complete Git worktree through a private temporary index.
/// The user's real index is never read or modified, while `git add -A`
/// ensures new, modified, and deleted files all participate in the tree.
/// Returns `None` for a non-Git directory so sessions rooted in arbitrary
/// folders simply omit the Changes dock.
pub fn capture_snapshot(target: &RepoTarget) -> Result<Option<RepoSnapshot>> {
    match target.ssh_host.as_deref() {
        Some(host) => capture_remote_snapshot(host, &target.path),
        None => capture_local_snapshot(&target.path),
    }
}

/// Diff two immutable worktree snapshots. Summary statistics and patch text
/// are gathered separately: list endpoints stay tiny, while the patch is
/// capped before it can consume unbounded daemon, SQLite, or browser memory.
pub fn diff_snapshots(
    target: &RepoTarget,
    base: &RepoSnapshot,
    head: &RepoSnapshot,
    started_at: i64,
    completed_at: i64,
) -> Result<NewTurnDiff> {
    validate_tree(&base.tree)?;
    validate_tree(&head.tree)?;

    let stat_args = [
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--numstat",
        &base.tree,
        &head.tree,
        "--",
    ];
    let stat_output = run_bounded(
        diff_command(target, &stat_args)?,
        MAX_STAT_BYTES,
        "git diff --numstat",
    )?;
    let (files_changed, additions, deletions) = parse_numstat(&stat_output.stdout);

    let patch_args = [
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
        "--unified=3",
        &base.tree,
        &head.tree,
        "--",
    ];
    let patch_output = run_bounded(
        diff_command(target, &patch_args)?,
        MAX_PATCH_BYTES,
        "git diff",
    )?;
    let mut patch = String::from_utf8_lossy(&patch_output.stdout).into_owned();
    let truncated = stat_output.stdout_truncated || patch_output.stdout_truncated;
    if truncated {
        if !patch.ends_with('\n') {
            patch.push('\n');
        }
        patch.push_str("… diff truncated by Slide …\n");
    }

    Ok(NewTurnDiff {
        started_at,
        completed_at,
        files_changed,
        additions,
        deletions,
        truncated,
        patch,
    })
}

fn capture_local_snapshot(path: &Path) -> Result<Option<RepoSnapshot>> {
    let mut top = Command::new("git");
    top.arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"]);
    let top = run_raw_bounded(top, 64 * 1024).context("locate Git worktree")?;
    if !top.success {
        return Ok(None);
    }
    let repo = PathBuf::from(String::from_utf8(top.stdout)?.trim());
    let temp_index = TempIndex::new()?;
    let index = temp_index.path();

    let mut verify = Command::new("git");
    verify
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--verify", "HEAD"]);
    let has_head = run_raw_bounded(verify, 64 * 1024)?.success;

    let mut read_tree = local_index_command(&repo, index);
    if has_head {
        read_tree.args(["read-tree", "HEAD"]);
    } else {
        read_tree.args(["read-tree", "--empty"]);
    }
    checked(read_tree, 64 * 1024, "initialize temporary Git index")?;

    let mut add = local_index_command(&repo, index);
    add.args(["add", "-A", "--", "."]);
    checked(add, 64 * 1024, "snapshot Git worktree")?;

    let mut write_tree = local_index_command(&repo, index);
    write_tree.arg("write-tree");
    let output = checked(write_tree, 128, "write Git snapshot tree")?;
    Ok(Some(RepoSnapshot {
        tree: parse_tree(&output.stdout)?,
    }))
}

fn capture_remote_snapshot(host: &str, path: &Path) -> Result<Option<RepoSnapshot>> {
    crate::ssh::validate_host(host)?;
    const SCRIPT: &str = r#"set -eu
repo=$(git -C "$1" rev-parse --show-toplevel 2>/dev/null) || exit 3
dir=$(mktemp -d /tmp/slide-turn-diff.XXXXXX)
index=$dir/index
trap 'rm -f "$index"; rmdir "$dir"' EXIT HUP INT TERM
if git -C "$repo" rev-parse --verify HEAD >/dev/null 2>&1; then
  GIT_INDEX_FILE="$index" git -C "$repo" read-tree HEAD
else
  GIT_INDEX_FILE="$index" git -C "$repo" read-tree --empty
fi
GIT_INDEX_FILE="$index" git -C "$repo" add -A -- .
GIT_INDEX_FILE="$index" git -C "$repo" write-tree
"#;
    let remote = format!(
        "sh -c {} sh {}",
        shell_quote(SCRIPT),
        shell_quote(&path.to_string_lossy()),
    );
    let mut command = Command::new("ssh");
    command.args(crate::ssh::ssh_args()).arg(host).arg(remote);
    let output = run_raw_bounded(command, 128).context("snapshot remote Git worktree")?;
    if output.code == Some(3) {
        return Ok(None);
    }
    ensure_success(&output, "snapshot remote Git worktree")?;
    Ok(Some(RepoSnapshot {
        tree: parse_tree(&output.stdout)?,
    }))
}

fn local_index_command(repo: &Path, index: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).env("GIT_INDEX_FILE", index);
    command
}

fn diff_command(target: &RepoTarget, args: &[&str]) -> Result<Command> {
    match target.ssh_host.as_deref() {
        None => {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(&target.path)
                .args(["-c", "core.quotePath=true"])
                .args(args);
            Ok(command)
        }
        Some(host) => {
            crate::ssh::validate_host(host)?;
            let mut parts = vec![
                "git".to_string(),
                "-C".to_string(),
                target.path.to_string_lossy().into_owned(),
                "-c".to_string(),
                "core.quotePath=true".to_string(),
            ];
            parts.extend(args.iter().map(|arg| (*arg).to_string()));
            let remote = parts
                .iter()
                .map(|part| shell_quote(part))
                .collect::<Vec<_>>()
                .join(" ");
            let mut command = Command::new("ssh");
            command.args(crate::ssh::ssh_args()).arg(host).arg(remote);
            Ok(command)
        }
    }
}

fn parse_numstat(bytes: &[u8]) -> (u64, u64, u64) {
    let mut files = 0u64;
    let mut additions = 0u64;
    let mut deletions = 0u64;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(3, |byte| *byte == b'\t');
        let Some(add) = fields.next() else { continue };
        let Some(del) = fields.next() else { continue };
        if fields.next().is_none() {
            continue;
        }
        files = files.saturating_add(1);
        additions = additions.saturating_add(parse_count(add));
        deletions = deletions.saturating_add(parse_count(del));
    }
    (files, additions, deletions)
}

fn parse_count(bytes: &[u8]) -> u64 {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn parse_tree(bytes: &[u8]) -> Result<String> {
    let tree = std::str::from_utf8(bytes)?.trim();
    validate_tree(tree)?;
    Ok(tree.to_string())
}

fn validate_tree(tree: &str) -> Result<()> {
    if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid Git tree id");
    }
    Ok(())
}

fn checked(command: Command, limit: usize, label: &str) -> Result<CommandOutput> {
    let output = run_raw_bounded(command, limit).with_context(|| label.to_string())?;
    if output.stdout_truncated || output.stderr_truncated {
        bail!("{label} produced too much output");
    }
    ensure_success(&output, label)?;
    Ok(output)
}

fn run_bounded(command: Command, limit: usize, label: &str) -> Result<CommandOutput> {
    let output = run_raw_bounded(command, limit).with_context(|| label.to_string())?;
    // Reaching the output bound intentionally kills Git. The prefix is still
    // valid data and the caller records `truncated`, so that is a success.
    if output.timed_out || output.stderr_truncated || (!output.success && !output.stdout_truncated)
    {
        ensure_success(&output, label)?;
    }
    Ok(output)
}

fn ensure_success(output: &CommandOutput, label: &str) -> Result<()> {
    if output.timed_out {
        bail!("{label} timed out after {}s", COMMAND_TIMEOUT.as_secs());
    }
    if output.success {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let status = output
        .code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    bail!("{label} failed ({status}): {}", detail.trim());
}

fn run_raw_bounded(command: Command, limit: usize) -> Result<CommandOutput> {
    crate::process::run_bounded(command, limit, MAX_STDERR_BYTES, COMMAND_TIMEOUT)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct TempIndex {
    dir: PathBuf,
    index: PathBuf,
}

impl TempIndex {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("slide-turn-diff-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).context("create private Git snapshot directory")?;
        let temp = Self {
            index: dir.join("index"),
            dir,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp.dir, std::fs::Permissions::from_mode(0o700))
                .context("secure Git snapshot directory")?;
        }
        Ok(temp)
    }

    fn path(&self) -> &Path {
        &self.index
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.index);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn git_stdout(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn snapshots_incremental_modified_and_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        fs::write(dir.path().join("existing.txt"), "before\n").unwrap();
        fs::write(dir.path().join("deleted.txt"), "remove me\n").unwrap();
        git(dir.path(), &["add", "existing.txt"]);
        let real_index_before = git_stdout(dir.path(), &["write-tree"]);
        let target = RepoTarget {
            path: dir.path().to_path_buf(),
            ssh_host: None,
        };
        let base = capture_snapshot(&target).unwrap().unwrap();

        fs::write(dir.path().join("existing.txt"), "after\n").unwrap();
        fs::remove_file(dir.path().join("deleted.txt")).unwrap();
        fs::write(dir.path().join("new.txt"), "one\ntwo\n").unwrap();
        let head = capture_snapshot(&target).unwrap().unwrap();
        let diff = diff_snapshots(&target, &base, &head, 10, 20).unwrap();

        assert_eq!(diff.files_changed, 3);
        assert_eq!(diff.additions, 3);
        assert_eq!(diff.deletions, 2);
        assert!(diff.patch.contains("existing.txt"));
        assert!(diff.patch.contains("deleted.txt"));
        assert!(diff.patch.contains("new.txt"));
        assert!(!diff.truncated);
        assert_eq!(git_stdout(dir.path(), &["write-tree"]), real_index_before);
    }

    #[test]
    fn non_git_directory_is_not_supported() {
        let dir = tempfile::tempdir().unwrap();
        let target = RepoTarget {
            path: dir.path().to_path_buf(),
            ssh_host: None,
        };
        assert!(capture_snapshot(&target).unwrap().is_none());
    }

    #[test]
    fn parses_numstat_and_treats_binary_counts_as_zero() {
        assert_eq!(
            parse_numstat(b"12\t3\tsrc/main.rs\n-\t-\timage.png\n"),
            (2, 12, 3),
        );
    }

    #[test]
    fn shell_quote_handles_spaces_quotes_and_empty_values() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn rejects_invalid_tree_ids_before_spawning_git() {
        let target = RepoTarget {
            path: PathBuf::from("/missing"),
            ssh_host: None,
        };
        let bad = RepoSnapshot::new("--output=/tmp/oops");
        assert!(diff_snapshots(&target, &bad, &bad, 0, 0).is_err());
    }

    #[test]
    fn patch_output_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        let target = RepoTarget {
            path: dir.path().to_path_buf(),
            ssh_host: None,
        };
        let base = capture_snapshot(&target).unwrap().unwrap();
        fs::write(
            dir.path().join("large.txt"),
            vec![b'x'; MAX_PATCH_BYTES * 2],
        )
        .unwrap();
        let head = capture_snapshot(&target).unwrap().unwrap();
        let diff = diff_snapshots(&target, &base, &head, 10, 20).unwrap();

        assert!(diff.truncated);
        assert!(diff.patch.ends_with("… diff truncated by Slide …\n"));
        assert!(diff.patch.len() <= MAX_PATCH_BYTES + 64);
    }
}
