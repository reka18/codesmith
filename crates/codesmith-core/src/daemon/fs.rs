//! Where a daemon keeps its files, and the checks that keep them private
//! to the user who runs it.

use super::protocol::Registration;
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

pub const REGISTRATION: &str = "service.json";
pub const LOCK: &str = "service.lock";

pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Create `dir` (parents too) as a directory only its owner can use, and
/// check that it is one: a real directory (not a symlink), owned by this
/// user, mode 0700.
pub fn secure_dir(dir: &Path) -> io::Result<()> {
    if let Some(p) = dir.parent() {
        std::fs::create_dir_all(p)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let m = std::fs::symlink_metadata(dir)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "unsafe daemon directory (not a directory): {}",
            dir.display()
        )));
    }
    if m.uid() != euid() {
        return Err(io::Error::other(format!(
            "daemon directory is owned by another user: {}",
            dir.display()
        )));
    }
    if m.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Whether `name` is a single path component a runtime directory may have.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// The directory sockets go in: `codesmith-shelld-<uid>` under
/// `$CODESMITH_SHELLD_SOCKET_DIR` (absolute, no `..`) or `/tmp`, which is
/// short enough for the socket path limit and skipped by temp cleaners.
pub fn socket_dir() -> io::Result<PathBuf> {
    let base = match std::env::var_os(super::SOCKET_DIR_VAR).filter(|v| !v.is_empty()) {
        Some(v) => {
            let p = PathBuf::from(v);
            if !p.is_absolute() || p.components().any(|c| c == Component::ParentDir) {
                return Err(io::Error::other(format!(
                    "{} must be an absolute path without ..",
                    super::SOCKET_DIR_VAR
                )));
            }
            p
        }
        None => PathBuf::from("/tmp"),
    };
    let dir = base.join(format!("codesmith-shelld-{}", euid()));
    secure_dir(&dir)?;
    Ok(dir)
}

/// The socket of the daemon whose runtime directory is `runtime`: a fixed
/// length hash of its canonical path, so any runtime path fits.
pub fn socket_path(runtime: &Path) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(runtime)?;
    let hash = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let name: String = hash[..10].iter().map(|b| format!("{b:02x}")).collect();
    Ok(socket_dir()?.join(format!("{name}.sock")))
}

/// Read a daemon's registration, refusing one that is a symlink, is not
/// this user's, or that others could have written.
pub fn read_registration(dir: &Path) -> io::Result<Registration> {
    let path = dir.join(REGISTRATION);
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != euid() || m.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("untrusted daemon registration: {}", path.display()),
        ));
    }
    let mut body = String::new();
    f.by_ref().take(64 * 1024).read_to_string(&mut body)?;
    serde_json::from_str(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Publish `reg` atomically, mode 0600.
pub fn write_registration(dir: &Path, reg: &Registration) -> io::Result<()> {
    let body = serde_json::to_vec(reg).map_err(io::Error::other)?;
    crate::paths::write_private(&dir.join(REGISTRATION), &body)
}

/// The uid of the process at the other end of a Unix socket.
pub fn peer_uid(fd: std::os::fd::RawFd) -> io::Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are valid for writes of the sizes given.
        let r = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `uid` and `gid` are valid for writes.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Whether two secrets are equal, in time that depends only on their
/// lengths.
pub fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Take an exclusive `flock` on `file` without waiting.
pub fn try_lock(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a descriptor this process owns.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Open (creating) a lock file, refusing a symlink.
pub fn open_lock(dir: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join(LOCK))
}

/// Whether process `pid` exists.
pub fn alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only checks for the process.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
