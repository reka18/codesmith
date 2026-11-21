//! API keys saved locally with `codesmith auth login`, in the data dir's
//! `auth.json` (owner-only permissions). Config and environment variables
//! take precedence over saved keys.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// An exclusive advisory lock on `<file>.lock`, held until dropped. Files
/// are replaced by renaming, so the lock lives beside the file rather than
/// on it. Concurrent CodeSmith processes take it around each
/// read-modify-write of `auth.json` and `mcp-auth.json`, so one cannot lose
/// another's update (such as a rotated refresh token). Advisory, and only
/// on Unix; elsewhere it does nothing.
pub(crate) struct FileLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

impl FileLock {
    /// Block until the lock on `file` is ours.
    pub(crate) fn acquire(file: &Path) -> Result<FileLock> {
        let mut name = file.as_os_str().to_owned();
        name.push(".lock");
        let lock = PathBuf::from(name);
        crate::paths::ensure_parent(&lock)
            .with_context(|| format!("creating {}", lock.parent().unwrap_or(&lock).display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::io::AsRawFd;
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&lock)
                .with_context(|| format!("opening {}", lock.display()))?;
            loop {
                // SAFETY: flock on a descriptor this function owns.
                if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    break;
                }
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e).with_context(|| format!("locking {}", lock.display()));
                }
            }
            Ok(FileLock { _file: f })
        }
        #[cfg(not(unix))]
        Ok(FileLock {})
    }
}

/// Read a JSON file for changing it: missing is empty, but a file that does
/// not parse is copied to `<file>.bak` and refused, so a write cannot
/// silently replace what it held.
pub(crate) fn read_for_update<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).or_else(|e| {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        let bak = PathBuf::from(bak);
        crate::paths::write_private(&bak, text.as_bytes())
            .with_context(|| format!("saving a copy of {} to {}", path.display(), bak.display()))?;
        Err(anyhow!(
            "{} is not valid JSON ({e}); a copy is in {}. Fix or remove the file, then try again",
            path.display(),
            bak.display()
        ))
    })
}

fn read(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write(path: &Path, map: &Map<String, Value>) -> Result<()> {
    crate::paths::write_private(path, serde_json::to_string_pretty(map)?.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Change the saved keys under the file lock.
fn update<T>(path: &Path, f: impl FnOnce(&mut Map<String, Value>) -> (T, bool)) -> Result<T> {
    let _lock = FileLock::acquire(path)?;
    let mut m: Map<String, Value> = read_for_update(path)?;
    let (out, changed) = f(&mut m);
    if changed {
        write(path, &m)?;
    }
    Ok(out)
}

pub fn key_in(path: &Path, provider: &str) -> Option<String> {
    read(path)
        .get(provider)?
        .get("key")?
        .as_str()
        .filter(|k| !k.is_empty())
        .map(str::to_string)
}

pub fn set_in(path: &Path, provider: &str, key: &str) -> Result<()> {
    update(path, |m| {
        m.insert(provider.to_string(), json!({"type": "api", "key": key}));
        ((), true)
    })
}

/// Remove a saved key; false when there was none.
pub fn remove_in(path: &Path, provider: &str) -> Result<bool> {
    update(path, |m| {
        let had = m.remove(provider).is_some();
        (had, had)
    })
}

/// A provider's saved entry as stored: `{"type": "api", "key"}`, a
/// provider login (`{"type": "oauth", "access", "refresh", "expires",
/// "metadata"}`) or an external one (`{"type": "external", "method",
/// "metadata"}`, such as an AWS profile).
pub fn entry_in(path: &Path, provider: &str) -> Option<Value> {
    read(path).get(provider).cloned()
}

/// Save a provider's entry (see [`entry_in`]).
pub fn set_entry_in(path: &Path, provider: &str, entry: Value) -> Result<()> {
    update(path, |m| {
        m.insert(provider.to_string(), entry);
        ((), true)
    })
}

/// The saved entry for `provider` in the default auth file.
pub fn entry(provider: &str) -> Option<Value> {
    entry_in(&crate::paths::auth_file(), provider)
}

/// The providers with a saved entry (not the bookkeeping kept beside them,
/// under keys starting with `$`).
pub fn providers_in(path: &Path) -> Vec<String> {
    read(path)
        .keys()
        .filter(|k| !k.starts_with('$'))
        .cloned()
        .collect()
}

pub fn key(provider: &str) -> Option<String> {
    key_in(&crate::paths::auth_file(), provider)
}

/// `sk-ab…wxyz` style, for listing without revealing the key.
pub fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 10 {
        return "…".into();
    }
    format!(
        "{}…{}",
        chars[..4].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_read_remove() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/auth.json");
        assert_eq!(key_in(&p, "openai"), None);
        set_in(&p, "openai", "sk-1234567890abcd").unwrap();
        set_in(&p, "groq", "gsk_x").unwrap();
        assert_eq!(key_in(&p, "openai").as_deref(), Some("sk-1234567890abcd"));
        assert_eq!(providers_in(&p), ["groq", "openai"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(remove_in(&p, "groq").unwrap());
        assert!(!remove_in(&p, "groq").unwrap());
        assert_eq!(providers_in(&p), ["openai"]);
    }

    #[test]
    fn concurrent_writers_keep_every_update() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        set_in(&p, &format!("p{i}-{j}"), "k").unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        #[cfg(unix)]
        assert_eq!(providers_in(&p).len(), 80);
    }

    #[test]
    fn a_corrupt_file_is_kept_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("auth.json");
        std::fs::write(&p, "{\"openai\": {\"key\": \"sk-").unwrap();
        let e = set_in(&p, "groq", "gsk_x").unwrap_err().to_string();
        assert!(e.contains("auth.json is not valid JSON"), "{e}");
        assert!(e.contains("auth.json.bak"), "{e}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "{\"openai\": {\"key\": \"sk-",
            "left as it was"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("auth.json.bak")).unwrap(),
            "{\"openai\": {\"key\": \"sk-"
        );
        assert!(remove_in(&p, "openai").is_err());
    }

    #[test]
    fn masking() {
        assert_eq!(mask("sk-1234567890abcd"), "sk-1…abcd");
        assert_eq!(mask("short"), "…");
    }
}
