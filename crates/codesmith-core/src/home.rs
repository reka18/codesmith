//! Another CodeSmith home: the `HOME` a tailbus bridge gives the
//! `codesmith run` it starts, apart from yours. `codesmith auth login
//! --home DIR` saves a login there, and `--to-file PATH` (or `codesmith
//! bridge init`) writes a credential to a file of its own, for a bridge's
//! `secrets`.
//!
//! A run under a bridge sees only `HOME`, `PATH` and `TMPDIR`, so its
//! folders are the defaults under that `HOME`: `XDG_*` and
//! `CODESMITH_CONFIG_DIR`, which are yours, never apply to it, and are not
//! read here either.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

/// The config folder a run with `HOME=home` reads.
pub fn config_dir(home: &Path) -> PathBuf {
    home.join(".config/codesmith")
}

/// The data folder a run with `HOME=home` keeps its keys and sessions in.
pub fn data_dir(home: &Path) -> PathBuf {
    home.join(".local/share/codesmith")
}

/// The auth store of a run with `HOME=home`.
pub fn auth_file(home: &Path) -> PathBuf {
    data_dir(home).join("auth.json")
}

/// Your own home folders: `$HOME` and your account's.
fn own_homes() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .into_iter()
        .collect();
    out.extend(crate::paths::account_home());
    out.iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect()
}

/// `dir` as another home: resolved against `cwd`, an existing folder, and
/// not your own home (signing in there needs no `--home`).
pub fn resolve(dir: &Path, cwd: &Path) -> Result<PathBuf> {
    resolve_with(dir, cwd, &own_homes())
}

fn resolve_with(dir: &Path, cwd: &Path, own: &[PathBuf]) -> Result<PathBuf> {
    let d = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        cwd.join(dir)
    };
    let real = d
        .canonicalize()
        .map_err(|e| anyhow!("{}: {e}; create the folder first", d.display()))?;
    if !real.is_dir() {
        bail!("{} is not a folder", d.display());
    }
    if own.contains(&real) {
        bail!(
            "{} is your own home folder: leave out --home to sign in there",
            d.display()
        );
    }
    Ok(real)
}

/// Whether a credential file is already at `path`. A symlink, or anything
/// but a plain file, is an error: a secret is never written through one.
pub fn secret_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => bail!(
            "{} is a symlink; CodeSmith doesn't write a secret through one",
            path.display()
        ),
        Ok(m) if !m.file_type().is_file() => bail!("{} is not a file", path.display()),
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("checking {}", path.display())),
    }
}

/// Write `value` (a key or token, trimmed) to `path`, readable only by you
/// (0600), the way a tailbus bridge's `secrets` reads it. A missing folder
/// is created owner-only. An existing file is replaced only when `replace`
/// (atomically, by a fresh file renamed over it); a symlink at `path` is
/// always refused, and the file is opened with `O_NOFOLLOW`.
pub fn write_secret(path: &Path, value: &str, replace: bool) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        bail!("nothing to write: the key is empty");
    }
    if value.chars().any(char::is_control) {
        bail!("the key has a line break or control character in it");
    }
    let exists = secret_exists(path)?;
    if exists && !replace {
        bail!("{} already exists", path.display());
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !dir.exists() {
        crate::paths::private_dir(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut text = value.to_string();
    text.push('\n');
    if !exists {
        return create_new(path, text.as_bytes())
            .with_context(|| format!("writing {}", path.display()));
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for _ in 0..16 {
        let tmp = dir.join(format!(".{name}.{}.tmp", crate::paths::random_token(8)));
        match create_new(&tmp, text.as_bytes()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("writing {}", tmp.display())),
        }
        // Renaming replaces whatever is at `path` itself, never what a
        // link there points to.
        return std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            anyhow!("replacing {}: {e}", path.display())
        });
    }
    bail!(
        "could not create a temporary file beside {}",
        path.display()
    )
}

/// A new file (never an existing one or a link) holding `bytes`, 0600.
fn create_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = o.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let r = f.write_all(bytes).and_then(|_| f.sync_all());
    if r.is_err() {
        let _ = std::fs::remove_file(path);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_homes_folders_are_the_defaults_under_it() {
        let h = Path::new("/b/home");
        assert_eq!(config_dir(h), Path::new("/b/home/.config/codesmith"));
        assert_eq!(
            auth_file(h),
            Path::new("/b/home/.local/share/codesmith/auth.json")
        );
    }

    #[test]
    fn another_home_must_exist_and_not_be_yours() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let other = root.join("bridge");
        std::fs::create_dir(&other).unwrap();
        let mine = root.join("me");
        std::fs::create_dir(&mine).unwrap();
        let own = [mine.clone()];
        assert_eq!(
            resolve_with(Path::new("bridge"), &root, &own).unwrap(),
            other
        );
        assert_eq!(resolve_with(&other, Path::new("/"), &own).unwrap(), other);
        // Your own home, also through `..` or a link.
        let e = resolve_with(Path::new("bridge/../me"), &root, &own).unwrap_err();
        assert!(format!("{e}").contains("your own home"), "{e}");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&mine, root.join("link")).unwrap();
            assert!(resolve_with(Path::new("link"), &root, &own).is_err());
        }
        // Missing, or a file.
        let e = resolve_with(Path::new("nope"), &root, &own).unwrap_err();
        assert!(format!("{e}").contains("create the folder"), "{e}");
        std::fs::write(root.join("f"), "").unwrap();
        assert!(resolve_with(Path::new("f"), &root, &own).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_secret_is_written_owner_only_in_an_owner_only_folder() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("secrets/FIREWORKS_API_KEY");
        write_secret(&f, "  fw-secret-1 \n", false).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "fw-secret-1\n");
        assert_eq!(mode(&f), 0o600);
        assert_eq!(mode(f.parent().unwrap()), 0o700);
        // Not replaced without asking.
        let e = write_secret(&f, "fw-secret-2", false).unwrap_err();
        assert!(format!("{e}").contains("already exists"), "{e}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "fw-secret-1\n");
        // Replaced when asked, by a fresh 0600 file, leaving no temp file.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_secret(&f, "fw-secret-2", true).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "fw-secret-2\n");
        assert_eq!(mode(&f), 0o600);
        assert_eq!(std::fs::read_dir(f.parent().unwrap()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_secret_is_never_written_through_a_symlink() {
        let d = tempfile::tempdir().unwrap();
        let victim = d.path().join("victim");
        std::fs::write(&victim, "keep\n").unwrap();
        let link = d.path().join("key");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        for replace in [false, true] {
            let e = write_secret(&link, "fw-secret", replace).unwrap_err();
            assert!(format!("{e}").contains("symlink"), "{e}");
        }
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
        // A dangling link too: nothing is created where it points.
        let dangling = d.path().join("dangling");
        std::os::unix::fs::symlink(d.path().join("made"), &dangling).unwrap();
        assert!(write_secret(&dangling, "fw-secret", false).is_err());
        assert!(!d.path().join("made").exists());
        // A folder isn't a file.
        assert!(write_secret(d.path(), "fw-secret", true).is_err());
    }

    #[test]
    fn an_empty_or_multiline_secret_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("k");
        assert!(write_secret(&f, " \n", false).is_err());
        assert!(write_secret(&f, "a\nb", false).is_err());
        assert!(write_secret(&f, "a\x1b[2Jb", false).is_err());
        assert!(!f.exists());
    }

    /// Bounded property: whatever printable value goes in comes back
    /// trimmed, one line, 0600, whether new or replacing.
    #[cfg(unix)]
    #[test]
    fn any_printable_secret_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("k");
        let alphabet: Vec<char> = "aZ09-_.~+/=:é鍵 ".chars().collect();
        let mut seed = 7u64;
        for i in 0..200 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let len = 1 + (seed >> 33) as usize % 80;
            let v: String = (0..len)
                .map(|j| alphabet[(seed as usize >> (j % 32)).wrapping_add(j) % alphabet.len()])
                .collect();
            if v.trim().is_empty() {
                continue;
            }
            write_secret(&f, &v, i > 0).unwrap();
            assert_eq!(
                std::fs::read_to_string(&f).unwrap(),
                format!("{}\n", v.trim())
            );
            assert_eq!(mode(&f), 0o600);
        }
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }
}
