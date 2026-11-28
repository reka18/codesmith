//! Linux: Landlock for files, a seccomp filter for network `none`, and
//! `no_new_privs`, applied in the child between `fork` and `exec`.
//!
//! Everything that allocates or opens files is done before the fork
//! ([`Prepared::new`]): the Landlock ruleset is built as a file
//! descriptor and the seccomp filter as a BPF program. The child only
//! makes three system calls ([`Prepared::apply`]), which is all that is
//! safe between `fork` and `exec`.
//!
//! Landlock only allows, and can't refuse something inside a tree it
//! allows. So reads are granted on the fewest paths that cover the whole
//! filesystem but the protected ones ([`super::cover`]), and writes on the
//! write folders the same way. Rights are handled up to ABI 5 (Linux
//! 6.10): `Refer`, so files can move between folders a command may write,
//! `Truncate` and device `ioctl`s. ABI 9's Unix-socket rule isn't handled,
//! so agent sockets stay reachable.

use super::{cover, Network, Policy, Support};
use landlock::{
    Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, ABI,
};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// The Landlock ABI the kernel offers (0 when it has none, or it is
/// switched off, or a container's seccomp profile hides it).
pub fn abi_version() -> i64 {
    // SAFETY: LANDLOCK_CREATE_RULESET_VERSION with a null attribute and
    // size 0 only returns the version.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32,
        )
    };
    v.max(0)
}

fn abi() -> ABI {
    match abi_version() {
        0 => ABI::Unsupported,
        1 => ABI::V1,
        2 => ABI::V2,
        3 => ABI::V3,
        4 => ABI::V4,
        _ => ABI::V5,
    }
}

/// Whether Landlock and seccomp are there.
pub fn probe() -> Support {
    let v = abi_version();
    if v < 1 {
        let why = std::io::Error::last_os_error();
        return Support::No(format!(
            "the kernel has no Landlock (needs Linux 5.13 or later with Landlock enabled; {why})"
        ));
    }
    // SAFETY: PR_GET_SECCOMP takes no pointers.
    if unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) } < 0 {
        return Support::No("the kernel has no seccomp".into());
    }
    Support::Yes
}

/// The devices a command may write (and `ioctl`): the null and random
/// devices, the terminal, and ptys. Not `/dev/shm`.
const DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
    "/dev/ptmx",
    "/dev/pts",
];

/// A policy made ready before the fork.
pub struct Prepared {
    ruleset: OwnedFd,
    filter: Option<seccompiler::BpfProgram>,
}

impl Prepared {
    pub fn new(policy: &Policy) -> Result<Prepared, String> {
        let abi = abi();
        if abi == ABI::Unsupported {
            return Err("the kernel has no Landlock".into());
        }
        let all = AccessFs::from_all(abi);
        let read = AccessFs::from_read(abi);
        let err = |e: &dyn std::fmt::Display| format!("can't build the sandbox: {e}");
        let mut rs = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(all)
            .map_err(|e| err(&e))?
            .create()
            .map_err(|e| err(&e))?;
        let add = |rs: landlock::RulesetCreated, p: &Path, access| {
            // A path that is gone or can't be opened is just not granted.
            match PathFd::new(p) {
                Ok(fd) => rs
                    .set_compatibility(CompatLevel::BestEffort)
                    .add_rule(PathBeneath::new(fd, access))
                    .map_err(|e| err(&e)),
                Err(_) => Ok(rs),
            }
        };
        for p in cover(Path::new("/"), &policy.protected, &policy.readable) {
            rs = add(rs, &p, read)?;
        }
        let writes: Vec<PathBuf> = policy
            .write
            .iter()
            .flat_map(|w| cover(w, &policy.protected, &[]))
            .collect();
        for p in &writes {
            rs = add(rs, p, all)?;
        }
        for d in DEVICES {
            rs = add(rs, Path::new(d), all)?;
        }
        let fd: Option<OwnedFd> = rs.into();
        let ruleset = fd.ok_or_else(|| "the kernel has no Landlock".to_string())?;
        let filter = match policy.network {
            Network::None => Some(no_network()?),
            _ => None,
        };
        Ok(Prepared { ruleset, filter })
    }

    /// Restrict the calling process: `no_new_privs`, the Landlock ruleset,
    /// then the seccomp filter. Only system calls, so it may run between
    /// `fork` and `exec`.
    pub fn apply(&self) -> std::io::Result<()> {
        // SAFETY: plain system calls on values that live as long as `self`.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0u32,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        if let Some(f) = &self.filter {
            let prog = libc::sock_fprog {
                len: f.len() as libc::c_ushort,
                filter: f.as_ptr() as *mut libc::sock_filter,
            };
            // SAFETY: `prog` points into `f`, alive for the call.
            let r = unsafe {
                libc::syscall(
                    libc::SYS_seccomp,
                    1u32, // SECCOMP_SET_MODE_FILTER
                    0u32,
                    &prog as *const libc::sock_fprog,
                )
            };
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// The filter for network `none`: `socket()` for any family but Unix
/// fails with `EACCES`, and so does `io_uring_setup` (io_uring can make
/// sockets without `socket()`).
fn no_network() -> Result<seccompiler::BpfProgram, String> {
    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    };
    use std::collections::BTreeMap;
    let err = |e: &dyn std::fmt::Display| format!("can't build the network filter: {e}");
    let not_unix = SeccompRule::new(vec![SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        libc::AF_UNIX as u64,
    )
    .map_err(|e| err(&e))?])
    .map_err(|e| err(&e))?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules.insert(libc::SYS_socket, vec![not_unix]);
    rules.insert(libc::SYS_io_uring_setup, vec![]);
    let arch = std::env::consts::ARCH
        .try_into()
        .map_err(|e: seccompiler::BackendError| err(&e))?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EACCES as u32),
        arch,
    )
    .map_err(|e| err(&e))?;
    filter
        .try_into()
        .map_err(|e: seccompiler::BackendError| err(&e))
}
