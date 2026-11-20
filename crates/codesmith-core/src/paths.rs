//! Where codesmith keeps things. XDG-style on every platform, overridable by
//! environment variables, so a fresh install needs nothing configured.

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

/// `$CODESMITH_CONFIG_DIR`, else `$XDG_CONFIG_HOME/codesmith`, else `~/.config/codesmith`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("CODESMITH_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("codesmith"))
}

/// `$XDG_DATA_HOME/codesmith`, else `~/.local/share/codesmith`.
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("codesmith")
}

/// `$XDG_STATE_HOME/codesmith`, else `~/.local/state/codesmith`: state
/// that outlives a process but is not worth backing up (the shell
/// daemon's runtime directories).
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("codesmith")
}

/// `$XDG_CACHE_HOME/codesmith`, else `~/.cache/codesmith`: things that can
/// be fetched again, such as the models.dev catalog.
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("codesmith")
}

/// API keys saved with `codesmith auth login`.
pub fn auth_file() -> PathBuf {
    data_dir().join("auth.json")
}

/// Full tool outputs that were too large to show inline.
pub fn tool_output_dir() -> PathBuf {
    data_dir().join("tool-output")
}

/// One session's saved tool output: sessions (and projects) cannot read
/// each other's, which `tool_output_dir` keeps for a week.
pub fn session_output_dir(session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    tool_output_dir().join(if safe.is_empty() { "_".into() } else { safe })
}

/// A private scratch directory the agent may use without asking.
pub fn tmp_dir() -> PathBuf {
    data_dir().join("tmp")
}

/// Create `dir` (and its parents) and, on Unix, make sure no one but the
/// owner can read it: an existing directory loses its group and other bits;
/// nothing is ever loosened.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        if !dir.exists() {
            if let Some(p) = dir.parent() {
                std::fs::create_dir_all(p)?;
            }
            match std::fs::DirBuilder::new().mode(0o700).create(dir) {
                Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
                _ => {}
            }
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    tighten(dir)
}

/// Remove group and other permission bits from an existing file or
/// directory (Unix; a no-op elsewhere or when it does not exist).
pub fn tighten(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return Ok(());
        };
        if meta.file_type().is_symlink() {
            return Ok(());
        }
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Create the directory a codesmith-owned file lives in: the data and config
/// directories are made owner-only, anything else is just created.
pub fn ensure_parent(path: &Path) -> std::io::Result<()> {
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Ok(());
    };
    let data = data_dir();
    if dir == data || dir == config_dir() {
        private_dir(dir)
    } else if dir.starts_with(&data) {
        private_dir(&data)?;
        std::fs::create_dir_all(dir)
    } else {
        std::fs::create_dir_all(dir)
    }
}

/// Open `path` for appending, creating it owner-only (0600) if missing and
/// tightening it if it exists.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    private_mode(&mut o);
    let f = o.open(path)?;
    tighten(path)?;
    Ok(f)
}

/// Create (or truncate) `path` owner-only (0600) for writing.
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true).truncate(true);
    private_mode(&mut o);
    let f = o.open(path)?;
    tighten(path)?;
    Ok(f)
}

fn private_mode(o: &mut std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = o;
}

/// Replace `path` with `bytes` atomically, owner-only: the data goes to a
/// fresh, uniquely named temporary file (created exclusively with 0600, so
/// nothing else can have it open) that is then renamed over `path`.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    ensure_parent(path)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    for _ in 0..16 {
        let tmp = dir.join(format!(".{name}.{}.tmp", random_token(8)));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        private_mode(&mut o);
        let mut f = match o.open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let r = f
            .write_all(bytes)
            .and_then(|_| f.sync_all())
            .and_then(|_| std::fs::rename(&tmp, path));
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return r;
    }
    Err(std::io::Error::other("could not create a temporary file"))
}

/// `bytes` random bytes from the OS-seeded CSPRNG, as lowercase hex.
pub fn random_token(bytes: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn private_files_and_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("a/b");
        private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        let loose = d.path().join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&loose).unwrap();
        assert_eq!(mode(&loose), 0o700);
        // Tightening never adds bits.
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o500)).unwrap();
        tighten(&loose).unwrap();
        assert_eq!(mode(&loose), 0o500);
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o700)).unwrap();

        let f = dir.join("x.json");
        write_private(&f, b"one").unwrap();
        assert_eq!(mode(&f), 0o600);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&f, b"two").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"two");
        assert_eq!(mode(&f), 0o600, "the replacement is a fresh 0600 file");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary files are left behind");

        let log = dir.join("x.log");
        drop(open_private_append(&log).unwrap());
        assert_eq!(mode(&log), 0o600);
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(open_private_append(&log).unwrap());
        assert_eq!(mode(&log), 0o600);
        let c = dir.join("c.log");
        drop(create_private(&c).unwrap());
        assert_eq!(mode(&c), 0o600);
    }

    #[test]
    fn tokens_are_random() {
        let (a, b) = (random_token(16), random_token(16));
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
}
