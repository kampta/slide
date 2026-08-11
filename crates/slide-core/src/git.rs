use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::turn_diff::{self, RepoTarget};

const REMOTE_GIT_OUTPUT_LIMIT: usize = 64 * 1024;
const REMOTE_GIT_TIMEOUT: Duration = Duration::from_secs(30);

pub fn is_git_repo(path: &Path) -> bool {
    // `--is-inside-work-tree` exits 0 with stdout "false" when the cwd is
    // inside a .git/ directory — checking only the exit status would let
    // those paths through and surface a confusing "this operation must be
    // run in a work tree" error from the next git call. Require stdout
    // to actually say "true".
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success() && o.stdout.trim_ascii() == b"true")
        .unwrap_or(false)
}

pub fn toplevel(path: &Path) -> Result<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running git rev-parse")?;
    if !out.status.success() {
        bail!("git: {}", String::from_utf8_lossy(&out.stderr));
    }
    let s = String::from_utf8(out.stdout)?.trim().to_string();
    Ok(PathBuf::from(s))
}

/// Sanitize a session name into a filesystem-/branch-safe segment.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else if c.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("session");
    }
    out
}

/// Validate a user-supplied session name. Restricts to `[A-Za-z0-9_-]` with
/// a non-hyphen first char so the name is safe to reuse verbatim as a git
/// branch segment, worktree directory name, and (eventually) tmux session
/// name without further escaping.
pub fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if name.trim() != name {
        bail!("session name must not have leading or trailing whitespace");
    }
    for (i, c) in name.chars().enumerate() {
        let ok = c.is_ascii_alphanumeric() || c == '_' || (c == '-' && i > 0);
        if !ok {
            if c == '-' {
                bail!("session name must not start with a hyphen");
            }
            bail!(
                "session name may only contain letters, digits, underscore, and hyphen (got {:?})",
                c
            );
        }
    }
    Ok(())
}

/// Create a worktree rooted at `<base>/.slide-worktrees/<slug>` on branch
/// `slide/<slug>`. Returns the worktree path.
pub fn add_worktree(base: &Path, session_name: &str) -> Result<PathBuf> {
    add_worktree_at(base, session_name, None)
}

/// Create an isolated worktree at the source session's current commit, then
/// reproduce its complete Git-visible tracked/untracked file state without
/// reading or changing the source index. The fork starts with the same files
/// as its conversation while retaining ordinary uncommitted changes in the
/// target. Ignored files remain local to the source worktree.
pub fn add_worktree_from(base: &Path, session_name: &str, source: &Path) -> Result<PathBuf> {
    if !is_git_repo(source) {
        bail!("{} is not a git repo", source.display());
    }
    let source_head = revision(source, "HEAD^{commit}")?;
    let snapshot = turn_diff::capture_snapshot(&RepoTarget {
        path: source.to_path_buf(),
        ssh_host: None,
    })?
    .context("source session is not in a Git worktree")?;
    let worktree = add_worktree_at(base, session_name, Some(&source_head))?;

    let seed = (|| -> Result<()> {
        checked_git(
            Command::new("git").arg("-C").arg(&worktree).args([
                "read-tree",
                "--reset",
                "-u",
                snapshot.tree_id(),
            ]),
            "restore source session snapshot",
        )?;
        // Keep the source's file contents but do not imply that its staging
        // choices belong to the new agent. This turns snapshot-only paths
        // back into untracked files and leaves modifications unstaged.
        checked_git(
            Command::new("git")
                .arg("-C")
                .arg(&worktree)
                .args(["reset", "--mixed", "--quiet", "HEAD"]),
            "reset fork worktree index",
        )?;
        Ok(())
    })();
    if let Err(error) = seed {
        rollback_worktree(base, &worktree, session_name);
        return Err(error);
    }
    Ok(worktree)
}

fn add_worktree_at(base: &Path, session_name: &str, start_point: Option<&str>) -> Result<PathBuf> {
    if !is_git_repo(base) {
        bail!("{} is not a git repo", base.display());
    }
    let top = toplevel(base)?;
    let slug = slugify(session_name);
    let wt = top.join(".slide-worktrees").join(&slug);
    let branch = format!("slide/{slug}");

    if wt.exists() {
        bail!("worktree already exists: {}", wt.display());
    }

    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&top)
        .args(["worktree", "add", "-b", &branch])
        .arg(&wt);
    if let Some(start_point) = start_point {
        command.arg(start_point);
    }
    let out = command.output().context("spawning git worktree add")?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(wt)
}

fn revision(path: &Path, revision: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", revision])
        .output()
        .context("running git rev-parse")?;
    if !output.status.success() {
        bail!("source session has no commit to fork from");
    }
    let value = String::from_utf8(output.stdout)?.trim().to_string();
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("git returned an invalid revision");
    }
    Ok(value)
}

fn checked_git(command: &mut Command, action: &str) -> Result<()> {
    let output = command.output().with_context(|| action.to_string())?;
    if !output.status.success() {
        bail!(
            "{action} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Remove a worktree whose session never finished creating, including the
/// branch Slide just created so an immediate retry can reuse the same name.
pub fn rollback_worktree(base: &Path, worktree: &Path, session_name: &str) {
    let Ok(top) = toplevel(base) else { return };
    let _ = Command::new("git")
        .arg("-C")
        .arg(&top)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output();
    let branch = format!("slide/{}", slugify(session_name));
    let _ = Command::new("git")
        .arg("-C")
        .arg(top)
        .args(["branch", "-D", &branch])
        .output();
}

pub fn remove_worktree(base: &Path, worktree: &Path) -> Result<()> {
    let top = toplevel(base)?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&top)
        .args(["worktree", "remove", "--force"])
        .arg(worktree)
        .output()?;
    if !out.status.success() {
        // Not fatal — the worktree may have been removed manually.
        tracing::warn!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Create an isolated worktree on a remote host and return its absolute path.
/// The remote repository's top-level directory is used, matching the local
/// worktree layout even when `base` points at a subdirectory of the repo.
pub fn add_remote_worktree(host: &str, base: &Path, session_name: &str) -> Result<PathBuf> {
    let slug = slugify(session_name);
    let output = run_remote_script(
        host,
        ADD_REMOTE_WORKTREE_SCRIPT,
        &[base.to_string_lossy().into_owned(), slug],
    )?;
    let stdout = output.stdout.clone();
    ensure_remote_success(output, "adding remote Git worktree")?;
    let path = String::from_utf8(stdout)
        .context("remote Git worktree path was not valid UTF-8")?
        .lines()
        .next()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .context("remote Git worktree command returned no path")?;
    Ok(path)
}

/// Remove a Slide-owned worktree on a remote host. The branch is retained,
/// matching local deletion semantics; rollback removes the branch as well.
pub fn remove_remote_worktree(host: &str, base: &Path, worktree: &Path) -> Result<()> {
    let output = run_remote_script(
        host,
        REMOVE_REMOTE_WORKTREE_SCRIPT,
        &[
            base.to_string_lossy().into_owned(),
            worktree.to_string_lossy().into_owned(),
        ],
    )?;
    ensure_remote_success(output, "removing remote Git worktree")
}

/// Best-effort cleanup for a worktree created before session creation or
/// spawning failed. Unlike normal deletion, this also removes the branch so
/// an immediate retry with the same session name can succeed.
pub fn rollback_remote_worktree(
    host: &str,
    base: &Path,
    worktree: &Path,
    session_name: &str,
) -> Result<()> {
    let output = run_remote_script(
        host,
        ROLLBACK_REMOTE_WORKTREE_SCRIPT,
        &[
            base.to_string_lossy().into_owned(),
            worktree.to_string_lossy().into_owned(),
            slugify(session_name),
        ],
    )?;
    ensure_remote_success(output, "rolling back remote Git worktree")
}

const ADD_REMOTE_WORKTREE_SCRIPT: &str = r#"set -eu
base=$1
slug=$2
case "$base" in
  "~") base=$HOME ;;
  "~/"*) base=$HOME/${base#~/} ;;
esac
repo=$(git -C "$base" rev-parse --show-toplevel)
worktree="$repo/.slide-worktrees/$slug"
branch="slide/$slug"
if [ -e "$worktree" ]; then
  printf 'worktree already exists: %s\n' "$worktree" >&2
  exit 2
fi
git -C "$repo" worktree add -b "$branch" "$worktree" >&2
printf '%s\n' "$worktree"
"#;

const REMOVE_REMOTE_WORKTREE_SCRIPT: &str = r#"set -eu
base=$1
worktree=$2
case "$base" in
  "~") base=$HOME ;;
  "~/"*) base=$HOME/${base#~/} ;;
esac
repo=$(git -C "$base" rev-parse --show-toplevel)
if [ -e "$worktree" ]; then
  git -C "$repo" worktree remove --force "$worktree"
fi
"#;

const ROLLBACK_REMOTE_WORKTREE_SCRIPT: &str = r#"set -eu
base=$1
worktree=$2
slug=$3
case "$base" in
  "~") base=$HOME ;;
  "~/"*) base=$HOME/${base#~/} ;;
esac
repo=$(git -C "$base" rev-parse --show-toplevel)
if [ -e "$worktree" ]; then
  git -C "$repo" worktree remove --force "$worktree" || true
fi
git -C "$repo" branch -D "slide/$slug" 2>/dev/null || true
"#;

fn run_remote_script(
    host: &str,
    script: &str,
    args: &[String],
) -> Result<crate::process::BoundedOutput> {
    crate::ssh::validate_host(host)?;
    let mut parts = vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
    ];
    parts.extend(args.iter().cloned());
    let remote = parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = Command::new("ssh");
    command
        .args(["-o", "BatchMode=yes"])
        .args(crate::ssh::ssh_args())
        .arg(host)
        .arg(remote);
    crate::process::run_bounded(
        command,
        REMOTE_GIT_OUTPUT_LIMIT,
        REMOTE_GIT_OUTPUT_LIMIT,
        REMOTE_GIT_TIMEOUT,
    )
}

fn ensure_remote_success(output: crate::process::BoundedOutput, action: &str) -> Result<()> {
    if output.success {
        return Ok(());
    }
    if output.timed_out {
        bail!("{action} timed out");
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.stderr_truncated {
        bail!("{action} failed: {}…", stderr.trim());
    }
    bail!("{action} failed: {}", stderr.trim());
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

    fn run_git(path: &Path, args: &[&str]) -> std::process::Output {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }

    #[test]
    fn validate_accepts_clean_names() {
        for ok in [
            "auth-refactor",
            "foo",
            "F",
            "a_b",
            "_leading_underscore",
            "x1-y_2",
        ] {
            assert!(validate_session_name(ok).is_ok(), "rejected {ok:?}");
        }
    }

    #[test]
    fn validate_rejects_bad_names() {
        for bad in [
            "",
            " leading",
            "trailing ",
            "has space",
            "has.dot",
            "has:colon",
            "has/slash",
            "-starts-hyphen",
            "weird!",
            "emoji-🙂",
        ] {
            assert!(validate_session_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn remote_worktree_script_creates_isolated_worktree() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q"]);
        run_git(repo.path(), &["config", "user.email", "slide@test.invalid"]);
        run_git(repo.path(), &["config", "user.name", "Slide Test"]);
        std::fs::write(repo.path().join("README.md"), "remote worktree\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-qm", "initial"]);

        let output = Command::new("sh")
            .args(["-c", ADD_REMOTE_WORKTREE_SCRIPT, "sh"])
            .arg(repo.path())
            .arg("remote-session")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "remote worktree script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let worktree = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        assert_eq!(
            worktree,
            repo.path()
                .canonicalize()
                .unwrap()
                .join(".slide-worktrees/remote-session")
        );
        assert!(worktree.join("README.md").is_file());
        assert!(is_git_repo(&worktree));
    }

    #[test]
    fn fork_worktree_copies_file_state_without_touching_source_index() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q"]);
        std::fs::write(repo.path().join("tracked.txt"), "original\n").unwrap();
        run_git(repo.path(), &["add", "tracked.txt"]);
        run_git(
            repo.path(),
            &[
                "-c",
                "user.name=Slide Test",
                "-c",
                "user.email=slide@example.invalid",
                "commit",
                "-qm",
                "initial",
            ],
        );

        let source = add_worktree(repo.path(), "source").unwrap();
        std::fs::write(source.join("tracked.txt"), "source change\n").unwrap();
        std::fs::write(source.join("untracked.txt"), "source only\n").unwrap();
        run_git(&source, &["add", "tracked.txt"]);
        let source_status = run_git(&source, &["status", "--short"]).stdout;

        let fork = add_worktree_from(repo.path(), "fork", &source).unwrap();
        assert_eq!(
            std::fs::read_to_string(fork.join("tracked.txt")).unwrap(),
            "source change\n",
        );
        assert_eq!(
            std::fs::read_to_string(fork.join("untracked.txt")).unwrap(),
            "source only\n",
        );
        assert_eq!(
            run_git(&source, &["status", "--short"]).stdout,
            source_status,
        );
        assert_eq!(
            String::from_utf8(run_git(&fork, &["status", "--short"]).stdout).unwrap(),
            " M tracked.txt\n?? untracked.txt\n",
        );
    }
}
