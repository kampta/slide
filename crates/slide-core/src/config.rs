use std::path::{Path, PathBuf};

pub fn data_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("SLIDE_DATA_DIR") {
        return PathBuf::from(path);
    }
    dirs::data_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap().join(".local/share"))
        .join("slide")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn db_path() -> PathBuf {
    data_dir().join("slide.db")
}

pub fn lock_path() -> PathBuf {
    data_dir().join("daemon.lock")
}

/// Create the data and logs directories with mode 0o700 on Unix. Without
/// this, `~/.local/share/slide/` inherits the process umask (typically
/// 0o022), leaving the dir world-executable — and `daemon.lock` inside
/// embeds the bearer token. Tightening the parent prevents another local
/// user from `stat`-ing or traversing into the directory even though the
/// individual files are 0o600.
pub fn ensure_dirs() -> std::io::Result<()> {
    create_secure_dir(&data_dir())?;
    create_secure_dir(&logs_dir())?;
    Ok(())
}

#[cfg(unix)]
fn create_secure_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_secure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn create_secure_dir_sets_0o700() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b/c");
        create_secure_dir(&nested).unwrap();
        let mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "nested dir should be 0o700, was {mode:o}");
    }

    #[test]
    fn create_secure_dir_tightens_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("loose");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_secure_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
