//! The machine key: `remote-key` in the data folder, 32 raw ed25519 secret
//! bytes, mode 0600, made on first use and written atomically. Its public
//! key, as iroh shows it (64 lower-case hex characters), is the machine's
//! node id.

use anyhow::{anyhow, Context, Result};
use std::path::Path;

pub use iroh::{PublicKey, SecretKey};

/// The key in `path`, made (from the OS RNG) when there is none. A file
/// that isn't exactly 32 bytes, is a symlink, or that others can read or
/// write is refused rather than used or replaced.
pub fn load_or_create(path: &Path) -> Result<SecretKey> {
    match read(path) {
        Ok(Some(k)) => return Ok(k),
        Ok(None) => {}
        Err(e) => return Err(e),
    }
    let mut bytes = [0u8; 32];
    {
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut bytes);
    }
    crate::paths::ensure_parent(path)?;
    // Made exclusively, so two processes starting at once agree on one key.
    let made = create_new(path, &bytes);
    bytes.fill(0);
    match made {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("writing {}", path.display())),
    }
    read(path)?.ok_or_else(|| anyhow!("{} vanished as it was made", path.display()))
}

/// The key in `path`, or `None` when there is no file.
pub fn read(path: &Path) -> Result<Option<SecretKey>> {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut f = match o.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let meta = f.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !meta.is_file() || meta.uid() != crate::daemon::fs::euid() || meta.mode() & 0o077 != 0 {
            return Err(anyhow!(
                "{} must be a file of yours that only you can read (mode 0600)",
                path.display()
            ));
        }
    }
    if meta.len() != 32 {
        return Err(anyhow!(
            "{} is not a machine key (32 bytes)",
            path.display()
        ));
    }
    let mut bytes = [0u8; 32];
    f.read_exact(&mut bytes)?;
    let k = SecretKey::from_bytes(&bytes);
    bytes.fill(0);
    Ok(Some(k))
}

/// Write `bytes` to a fresh temporary file (0600), then link it into place
/// only if nothing is there yet: atomic, and never replacing a key.
fn create_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".remote-key.{}.tmp", crate::paths::random_token(8)));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let r = (|| {
        let mut f = o.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::hard_link(&tmp, path)
    })();
    let _ = std::fs::remove_file(&tmp);
    r
}

/// The node id of `key`: iroh's `Display` of its public key.
pub fn node_id(key: &SecretKey) -> String {
    key.public().to_string()
}

/// The first 10 hex characters of a node id, as devices are shown.
pub fn short(id: &str) -> String {
    id.chars().take(10).collect()
}

/// Whether `s` is a node id as shown: 64 lower-case hex characters.
pub fn is_node_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_made_once_private_and_kept() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("data/remote-key");
        let a = load_or_create(&p).unwrap();
        let b = load_or_create(&p).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(std::fs::read(&p).unwrap(), a.to_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let id = node_id(&a);
        assert!(is_node_id(&id), "{id}");
        assert_eq!(short(&id).len(), 10);
        // No temporary files are left behind.
        let left: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("remote-key")]);
    }

    #[test]
    fn a_bad_key_file_is_refused_not_replaced() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("remote-key");
        crate::paths::write_private(&p, b"short").unwrap();
        assert!(load_or_create(&p).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"short");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&p, [7u8; 32]).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load_or_create(&p).is_err(), "readable by others");
            std::fs::remove_file(&p).unwrap();
            let real = d.path().join("real");
            crate::paths::write_private(&real, &[7u8; 32]).unwrap();
            std::os::unix::fs::symlink(&real, &p).unwrap();
            assert!(load_or_create(&p).is_err(), "a symlink");
        }
    }

    #[test]
    fn two_makers_at_once_agree() {
        let d = tempfile::tempdir().unwrap();
        let p = std::sync::Arc::new(d.path().join("remote-key"));
        let ids: Vec<String> = (0..8)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || node_id(&load_or_create(&p).unwrap()))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    }

    #[test]
    fn node_ids_are_lower_case_hex() {
        assert!(is_node_id(&"a".repeat(64)));
        assert!(!is_node_id(&"A".repeat(64)));
        assert!(!is_node_id(&"a".repeat(63)));
        assert!(!is_node_id(&"g".repeat(64)));
    }
}
