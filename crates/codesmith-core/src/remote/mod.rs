//! Remote access from a phone: pairing a device with this machine by QR
//! code, and the gateway (`codesmith __remoted`) the device connects to
//! over iroh. See `docs/remote.md`, `docs/remote-protocol.md` and
//! `docs/design/remote.md`.
//!
//! Nothing here touches the network until the user opts in (`/mobile`,
//! `codesmith remote pair` or `start`, or `remote.enabled: true`): only
//! [`gateway`] binds a socket, and only a gateway started by one of those
//! runs.
//!
//! - [`settings`]: the `remote.*` settings, read only from the global config.
//! - [`key`]: the machine key (`remote-key`), whose public key is the
//!   machine's node id.
//! - [`link`]: the `codesmith://pair?…` link and the check code.
//! - [`store`]: `remote.db`, the devices, their grants and the audit log.
//! - [`gateway`]: the gateway process and its endpoint.
//! - [`control`]: the gateway's local socket, which `codesmith remote …`
//!   and `/mobile` use.
//!
//! Without the `remote` cargo feature only [`settings`] and the paths are
//! built, so the secret-file guards and the config warnings still apply to
//! files a build with the feature left.
//!
//! # Security
//!
//! The local socket is this user's alone, as the bus's is: its directory is
//! `codesmith-remote-<uid>` under the user's runtime directory, mode 0700;
//! both ends check the other's uid; every connection's first line carries
//! the token from the gateway's registration (0600, `O_NOFOLLOW`). The
//! remote files are guarded from the agent ([`crate::permission`]).

pub mod settings;

#[cfg(feature = "remote")]
pub mod control;
#[cfg(feature = "remote")]
pub mod gateway;
#[cfg(feature = "remote")]
pub mod http;
#[cfg(feature = "remote")]
pub mod key;
#[cfg(feature = "remote")]
pub mod link;
#[cfg(feature = "remote")]
pub mod phone;
#[cfg(feature = "remote")]
pub mod store;

#[cfg(all(test, feature = "remote"))]
mod remote_tests;

use std::path::PathBuf;

/// The hidden subcommand that runs the gateway.
pub const CHILD_ARG: &str = "__remoted";
/// The gateway's directory itself, instead of the one picked (tests, or to
/// keep two gateways apart).
pub const DIR_VAR: &str = "CODESMITH_REMOTE_DIR";

/// The ALPN a phone pairs on.
pub const PAIR_ALPN: &[u8] = b"codesmith/pair/1";
/// The ALPN a paired device uses for everything else.
pub const REMOTE_ALPN: &[u8] = b"codesmith/remote/1";

/// Connection close codes (QUIC application error codes).
pub mod close {
    /// The gateway is stopping.
    pub const STOPPING: u32 = 0;
    /// The device key isn't paired, or was revoked.
    pub const UNPAIRED: u32 = 1;
    /// An ALPN the gateway doesn't serve.
    pub const ALPN: u32 = 2;
    /// A pairing connection when no pairing code is live.
    pub const NO_CODE: u32 = 3;
}

/// The control socket, registration and locks, in [`dir`].
pub const SOCKET: &str = "remote.sock";
pub const REGISTRATION: &str = "remote.json";
pub const LOCK: &str = "remote.lock";
pub const START_LOCK: &str = "start.lock";

/// The machine key's file name, in the data folder.
pub const KEY_FILE: &str = "remote-key";
/// The database's file name, in the data folder.
pub const DB_FILE: &str = "remote.db";

/// The machine key: `<data>/remote-key`.
pub fn key_path() -> PathBuf {
    crate::paths::data_dir().join(KEY_FILE)
}

/// The devices, grants and audit log: `<data>/remote.db`.
pub fn db_path() -> PathBuf {
    crate::paths::data_dir().join(DB_FILE)
}

/// Where the gateway's directory is, without creating it: `$CODESMITH_REMOTE_DIR`
/// when set (absolute, no `..`), else `codesmith-remote-<uid>` under the
/// user's runtime directory, else `/tmp`.
#[cfg(unix)]
pub fn dir_path() -> std::io::Result<PathBuf> {
    use crate::daemon::fs;
    use std::path::Component;
    match std::env::var_os(DIR_VAR).filter(|v| !v.is_empty()) {
        Some(v) => {
            let p = PathBuf::from(v);
            if !p.is_absolute() || p.components().any(|c| c == Component::ParentDir) {
                return Err(std::io::Error::other(format!(
                    "{DIR_VAR} must be an absolute path without .."
                )));
            }
            Ok(p)
        }
        None => Ok(
            fs::pick_base(&crate::bus::bases()).join(format!("codesmith-remote-{}", fs::euid()))
        ),
    }
}

/// [`dir_path`], created private ([`crate::daemon::fs::secure_dir`]).
#[cfg(unix)]
pub fn dir() -> std::io::Result<PathBuf> {
    let d = dir_path()?;
    crate::daemon::fs::secure_dir(&d)?;
    Ok(d)
}

/// The shell-command guards for `codesmith remote …` (X2), as
/// `(pattern, asks)`, later ones winning: every `codesmith remote`
/// command, and the gateway itself, asks; `id` and `status` (which only
/// read) are let through after.
pub fn shell_guard_patterns() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for at in ["", "*/"] {
        for p in [
            "codesmith remote*",
            "codesmith * remote*",
            "codesmith __remoted*",
        ] {
            out.push((format!("{at}{p}"), true));
        }
    }
    for at in ["", "*/"] {
        for sub in ["id", "status"] {
            out.push((format!("{at}codesmith remote {sub}"), false));
            out.push((format!("{at}codesmith remote {sub} --json"), false));
        }
    }
    out
}

/// For a binary's `main`: run as the gateway if started as one (this does
/// not return then).
#[cfg(all(unix, feature = "remote"))]
pub fn child_main_if_requested() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == CHILD_ARG) {
        gateway::main(&args[2..]);
    }
}
