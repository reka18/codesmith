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
/// `$CODESMITH_SHELLD_SOCKET_DIR` (absolute, no `..`), else under the
/// user's own runtime directory ([`user_bases`]: one no other user can
/// write in, so no one can take the name first), else `/tmp`, which is
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
        None => pick_base(&user_bases()),
    };
    let dir = base.join(format!("codesmith-shelld-{}", euid()));
    secure_dir(&dir)?;
    Ok(dir)
}

/// The longest socket path used: under the limit of every platform
/// (`sun_path` is 104 bytes on macOS, 108 on Linux).
const MAX_SOCKET_PATH: usize = 100;

/// The user's own runtime directories, best first: `$XDG_RUNTIME_DIR`,
/// and on macOS the per-user temporary directory
/// (`confstr(_CS_DARWIN_USER_TEMP_DIR)`, else `$TMPDIR`).
fn user_bases() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(x));
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(t) = darwin_user_temp_dir() {
            out.push(t);
        }
        if let Some(t) = std::env::var_os("TMPDIR").filter(|v| !v.is_empty()) {
            out.push(PathBuf::from(t));
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn darwin_user_temp_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: `buf` is valid for writes of its length.
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
        )
    };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

/// The first of `candidates` that is this user's alone (an absolute, real
/// directory, on its resolved path owned by this user and writable by no
/// one else) and short enough for a socket path in it; else `/tmp`.
fn pick_base(candidates: &[PathBuf]) -> PathBuf {
    let private = |p: &Path| {
        p.is_absolute()
            && std::fs::canonicalize(p)
                .and_then(std::fs::metadata)
                .is_ok_and(|m| m.is_dir() && m.uid() == euid() && m.mode() & 0o022 == 0)
    };
    // `<base>/codesmith-shelld-<uid>/<20 hex>.sock`
    let fits = |p: &Path| {
        let tail = format!("/codesmith-shelld-{}/{}.sock", euid(), "0".repeat(20));
        p.as_os_str().len() + tail.len() <= MAX_SOCKET_PATH
    };
    candidates
        .iter()
        .map(|p| PathBuf::from(p.to_string_lossy().trim_end_matches('/')))
        .find(|p| private(p) && fits(p))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_base_is_a_private_directory_of_the_users() {
        // Under /tmp, short enough for a socket path.
        let mine = tempfile::tempdir_in("/tmp").unwrap();
        let mine = mine.path().canonicalize().unwrap();
        let private = mine.join("p");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&private)
            .unwrap();
        let shared = mine.join("s");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let long = private.join("x".repeat(MAX_SOCKET_PATH));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&long)
            .unwrap();
        let missing = mine.join("missing");
        // Shared, missing, relative or too long: skipped.
        assert_eq!(
            pick_base(&[shared.clone(), missing, "rel".into(), long, private.clone()]),
            private
        );
        assert_eq!(pick_base(&[shared]), PathBuf::from("/tmp"));
        assert_eq!(pick_base(&[]), PathBuf::from("/tmp"));
        // Someone else's (root's) directory is not ours.
        if euid() != 0 {
            assert_eq!(pick_base(&["/".into()]), PathBuf::from("/tmp"));
        }
        // A trailing slash makes no difference.
        let slashed = PathBuf::from(format!("{}/", private.display()));
        assert_eq!(pick_base(&[slashed]), private);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_has_a_per_user_temp_dir() {
        let t = darwin_user_temp_dir().expect("confstr");
        assert!(t.is_absolute(), "{t:?}");
    }
}
