use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

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

    let out = Command::new("git")
        .arg("-C")
        .arg(&top)
        .args(["worktree", "add", "-b", &branch])
        .arg(&wt)
        .output()
        .context("spawning git worktree add")?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(wt)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
