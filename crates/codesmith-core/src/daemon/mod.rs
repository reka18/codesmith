//! The shell daemon: a background `codesmith __shelld` process that runs a
//! server's shell commands and terminals so they survive a server restart,
//! after OpenCode's process daemon (`opencode-pty`) and its client
//! (`PersistentPty`, MIT).
//!
//! # Lifecycle
//!
//! - A server (`codesmith serve`) starts its daemon on demand, when the
//!   first command or terminal is created; never at boot, never for a
//!   listing or a cleanup.
//! - Each server picks a fresh runtime directory `<state>/shelld/<random>`
//!   (`<state>` is `$XDG_STATE_HOME/codesmith`, else
//!   `~/.local/state/codesmith`) holding the daemon's registration
//!   (`service.json`) and lock (`service.lock`). The lock makes the daemon
//!   the only one for that directory.
//! - The daemon must be claimed by an owner connection within five
//!   seconds. That connection stays open for the server's life. When it
//!   closes (the server exits, crashes or is killed) the daemon stops and
//!   kills everything it runs, unless the owner first prepared a handoff.
//! - A handoff ticket is good for two minutes. The next server, started
//!   with the handoff in [`HANDOFF_VAR`], claims the daemon with it (even
//!   while the old server is still connected) and takes its commands
//!   back. A ticket is consumed by its first use; a superseded owner can no
//!   longer hand over or stop the daemon. A handoff that expires with no
//!   owner stops the daemon. No ownership state is written to disk.
//! - `shutdown` from any authenticated client stops it at once.
//! - Stopping sends SIGTERM to every running command's process group,
//!   then SIGKILL after three seconds, hangs up every terminal, and removes
//!   the daemon's registration (only if still its own), socket, lock and
//!   runtime directory. A starting daemon removes sibling runtime
//!   directories older than ten minutes that a dead daemon left behind.
//!
//! # Security
//!
//! - Same user only. The socket lives in `/tmp/codesmith-shelld-<uid>/`
//!   (or under `$CODESMITH_SHELLD_SOCKET_DIR`), a directory that must be
//!   a real directory owned by the user, mode 0700; the socket itself is
//!   0600. Both sides also check the peer's uid on every connection
//!   (`SO_PEERCRED` / `getpeereid`) and refuse anyone else.
//! - A per-daemon secret. The registration (0600, written atomically, read
//!   with `O_NOFOLLOW` and refused when not the user's or writable by
//!   others) carries a random 256-bit token; every request must carry it
//!   and is compared in constant time. A wrong token is answered with
//!   "authentication failed" and the connection closed.
//! - Pipe processes get exactly the environment the server sends; the
//!   server's own token (`CODESMITH_SERVER_TOKEN`) is never passed to the
//!   daemon or its commands. Output files are opened with `O_NOFOLLOW`.
//! - A protocol version mismatch is refused, and the other daemon is left
//!   alone (never killed or replaced).
//!
//! # Limits
//!
//! See [`server::Limits`]: at most 64 pipe processes running and 1024
//! records kept, 16 terminals running, 128 connections open, frames of at
//! most 8 MiB, 1 MiB of metadata per command; a connection must send its
//! first request within five seconds.
//!
//! # Opting out
//!
//! The server uses the daemon unless `CODESMITH_SHELL_DAEMON=0` is set;
//! without it, commands and terminals live in the server process and end
//! with it, as before. The TUI and `codesmith run` always run commands
//! in-process.

pub mod client;
pub mod fs;
pub mod protocol;
pub mod server;

pub use client::{Client, DaemonError, Kind, Launcher};
pub use protocol::Handoff;

use std::path::PathBuf;
use std::sync::RwLock;

/// The hidden subcommand that runs a daemon.
pub const CHILD_ARG: &str = "__shelld";
/// The handoff a server inherits from the one before it (JSON).
pub const HANDOFF_VAR: &str = "CODESMITH_SHELLD_HANDOFF";
/// Set to `0` to keep a server's commands in-process.
pub const ENABLE_VAR: &str = "CODESMITH_SHELL_DAEMON";
/// Where sockets go instead of `/tmp`.
pub const SOCKET_DIR_VAR: &str = "CODESMITH_SHELLD_SOCKET_DIR";

static LAUNCHER: RwLock<Option<Launcher>> = RwLock::new(None);

/// For a binary's `main`: run as a daemon if started as one (this does not
/// return then), otherwise register this program as the daemon to start.
pub fn child_main_if_requested() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a == CHILD_ARG) {
        server::main(&args[2..]);
    }
    if let Ok(program) = std::env::current_exe() {
        set_launcher(Some(Launcher {
            program,
            args: vec![CHILD_ARG.into()],
        }));
    }
}

pub fn set_launcher(l: Option<Launcher>) {
    *LAUNCHER.write().unwrap() = l;
}

pub fn launcher() -> Option<Launcher> {
    LAUNCHER.read().unwrap().clone()
}

/// Whether a server should run its commands in a daemon.
pub fn enabled() -> bool {
    std::env::var(ENABLE_VAR).map_or(true, |v| v != "0") && launcher().is_some()
}

/// The parent of every server's runtime directory.
pub fn root() -> PathBuf {
    crate::paths::state_dir().join("shelld")
}

/// Take the inherited handoff out of the environment (call before any
/// thread starts), so no command inherits it. `Err` holds the raw value
/// when it is not a handoff.
pub fn take_handoff() -> Result<Option<Handoff>, String> {
    let Some(v) = std::env::var_os(HANDOFF_VAR) else {
        return Ok(None);
    };
    std::env::remove_var(HANDOFF_VAR);
    let v = v.to_string_lossy().into_owned();
    serde_json::from_str(&v).map(Some).map_err(|_| v)
}

/// A server's daemon client: a fresh runtime directory, or the inherited
/// one when a handoff is given.
pub fn for_server(inherited: Option<Handoff>) -> Client {
    Client::new(
        root(),
        &crate::paths::random_token(16),
        launcher(),
        inherited,
    )
}
