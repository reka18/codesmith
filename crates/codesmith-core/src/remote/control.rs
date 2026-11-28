//! The gateway's local control socket: what `codesmith remote …` and
//! `/mobile` say to `codesmith __remoted`, and the client side of it.
//!
//! Each connection sends one line, a [`Hello`] carrying the registration's
//! token and one [`Request`], and reads JSON lines of [`Frame`]s back. A
//! `pair` connection stays open as the pairing dialog: it gets the live
//! code (again each time it's replaced), a `confirm` for each claim, and
//! answers with [`Answer`] lines. Closing it ends the pairing.
//!
//! An `owner` connection (I9) stays open too: the TUI, `codesmith serve`
//! or ACP registers its sessions and sends their events on it, and the
//! gateway asks it for snapshots and resumes.

use super::store::Scope;
use crate::daemon::fs;
use serde::{Deserialize, Serialize};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub const PROTOCOL: u32 = 1;
/// The longest line either side sends.
pub const MAX_LINE: usize = 64 * 1024;
/// How long a starting gateway may take to answer.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);

/// What the gateway publishes in its directory (0600).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub pid: u32,
    pub protocol: u32,
    pub version: String,
    pub token: String,
    pub node_id: String,
    pub started: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub token: String,
    pub protocol: u32,
    pub version: String,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// What the gateway is doing.
    Status,
    /// Stop. With `version`, only for a newer CodeSmith taking over.
    Stop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    /// Revoke a device and close its connections.
    Revoke { device: String },
    /// Open the pairing dialog.
    Pair(PairRequest),
    /// An owning process (the TUI, `codesmith serve`, ACP) registering
    /// the sessions it holds (I9). After the gateway's `ok`, the
    /// connection carries the owner protocol's JSON lines both ways (see
    /// [`super::tap`] and the gateway).
    Owner { kind: String, pid: u32 },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    /// The session the dialog offers; none to pair the device alone.
    #[serde(default)]
    pub session: Option<String>,
    /// The session's title, shown in the prompt.
    #[serde(default)]
    pub title: Option<String>,
}

/// The dialog's answer to a `confirm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The `confirm` it answers; any other is ignored.
    pub id: u64,
    /// The grant the terminal allowed, or none for a refusal. A device
    /// can be allowed with no grant (`allow` with `grant: null`) when the
    /// dialog offers no session.
    pub allow: bool,
    #[serde(default)]
    pub grant: Option<GrantAnswer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantAnswer {
    /// The dialog's session, or `*` for every session.
    pub session: String,
    pub scope: Scope,
}

/// The device asking, as the confirmation shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asking {
    /// Its node id.
    pub id: String,
    pub name: String,
    pub platform: String,
    /// Whether it's already paired (it then gets only the new grant).
    pub paired: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Error {
        message: String,
    },
    Ok,
    Status(Status),
    /// The live code, its link and how long it lasts.
    Code {
        link: String,
        code: String,
        expires_in_ms: u64,
        machine: String,
        relay: Option<String>,
        relay_connected: bool,
    },
    /// A phone claimed the code: ask the terminal.
    Confirm {
        id: u64,
        device: Asking,
        check: String,
        expires_in_ms: u64,
    },
    /// The terminal allowed it.
    Paired {
        device: Asking,
        grant: Option<GrantAnswer>,
    },
    /// The claim was refused (by the terminal, or as its time ran out).
    Refused {
        reason: String,
    },
    /// Another dialog opened elsewhere and took over the pairing.
    Replaced,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    pub node_id: String,
    /// What `remote.relay` names, as the gateway uses it.
    pub relay: String,
    /// The home relay, when one is connected.
    pub home_relay: Option<String>,
    pub addrs: Vec<String>,
    pub devices: usize,
    pub connections: usize,
    pub pairing: bool,
    pub started: i64,
    /// Owner processes (TUI, serve, ACP) registered.
    #[serde(default)]
    pub owners: usize,
}

/// The longest line an owner sends: an event is at most 1 MiB, but a
/// resume's replay (up to 12 MiB of events) or a large turn's partial
/// transcript is one line.
pub const MAX_OWNER_LINE: usize = 64 << 20;

/// Read one line of at most [`MAX_LINE`] bytes; `None` at the end.
pub async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R) -> io::Result<Option<String>> {
    read_line_max(r, MAX_LINE).await
}

/// Read one line of at most `max` bytes; `None` at the end.
pub async fn read_line_max<R: AsyncBufRead + Unpin>(
    r: &mut R,
    max: usize,
) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(max as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    String::from_utf8(buf)
        .map(|s| Some(s.trim_end_matches(['\n', '\r']).to_string()))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

pub fn line<T: Serialize>(v: &T) -> String {
    let mut s = serde_json::to_string(v).unwrap_or_default();
    s.push('\n');
    s
}

/// Read a registration, refusing one that is a symlink, isn't this
/// user's, or that others could read or have written.
pub fn read_registration(dir: &Path) -> io::Result<Registration> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = dir.join(super::REGISTRATION);
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != fs::euid() || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("untrusted gateway registration: {}", path.display()),
        ));
    }
    let mut body = String::new();
    f.by_ref().take(64 * 1024).read_to_string(&mut body)?;
    serde_json::from_str(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Where a gateway that couldn't start says why, in its directory.
pub const ERROR_FILE: &str = "error.txt";

/// What the last gateway in `dir` that failed to start said.
pub fn last_error(dir: &Path) -> Option<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(ERROR_FILE))
        .ok()?;
    let m = f.metadata().ok()?;
    if !m.is_file() || m.uid() != fs::euid() {
        return None;
    }
    let mut s = String::new();
    f.by_ref().take(4096).read_to_string(&mut s).ok()?;
    let s = super::link::printable(s.trim());
    (!s.is_empty()).then_some(s)
}

/// A connection to the gateway in `dir`, after one request.
pub struct Conn {
    pub reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    pub writer: tokio::net::unix::OwnedWriteHalf,
    pub registration: Registration,
}

impl Conn {
    /// The next frame; `None` when the gateway closed the connection.
    pub async fn next(&mut self) -> io::Result<Option<Frame>> {
        match read_line(&mut self.reader).await? {
            None => Ok(None),
            Some(l) => serde_json::from_str(&l)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }

    pub async fn send<T: Serialize>(&mut self, v: &T) -> io::Result<()> {
        self.writer.write_all(line(v).as_bytes()).await
    }
}

/// Connect to the gateway in `dir` (checking it's this user's) and send
/// `request`. Errors when no gateway runs there.
pub async fn connect(dir: &Path, request: Request) -> io::Result<Conn> {
    let reg = read_registration(dir)?;
    let stream = UnixStream::connect(dir.join(super::SOCKET)).await?;
    if fs::peer_uid(stream.as_raw_fd())? != fs::euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the gateway's socket belongs to another user",
        ));
    }
    let (r, mut w) = stream.into_split();
    let hello = Hello {
        token: reg.token.clone(),
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").to_string(),
        request,
    };
    w.write_all(line(&hello).as_bytes()).await?;
    Ok(Conn {
        reader: BufReader::new(r),
        writer: w,
        registration: reg,
    })
}

/// Send `request` and read the one frame that answers it.
pub async fn call(dir: &Path, request: Request) -> io::Result<Frame> {
    let mut c = connect(dir, request).await?;
    tokio::time::timeout(Duration::from_secs(10), c.next())
        .await
        .map_err(|_| io::Error::other("the gateway didn't answer"))??
        .ok_or_else(|| io::Error::other("the gateway closed the connection"))
}

/// The running gateway's status, or `None` when none runs (never starts
/// one).
pub async fn status(dir: &Path) -> Option<Status> {
    match call(dir, Request::Status).await {
        Ok(Frame::Status(s)) => Some(s),
        _ => None,
    }
}

/// How to start a gateway.
#[derive(Debug, Clone)]
pub struct Launch {
    /// This program (`codesmith`), run as `codesmith __remoted --dir DIR`.
    pub program: PathBuf,
    /// This program's version: an older gateway makes way for it.
    pub version: String,
}

/// Make sure a gateway runs in `dir`, starting one when none answers, and
/// replacing one older than `launch.version`. Only an opt-in calls this.
pub async fn ensure(dir: &Path, launch: &Launch) -> io::Result<Registration> {
    if let Ok(reg) = read_registration(dir) {
        if status(dir).await.is_some() {
            if !crate::bus::protocol::newer(&launch.version, &reg.version) {
                return Ok(reg);
            }
            // An older gateway makes way for this newer CodeSmith.
            let _ = call(
                dir,
                Request::Stop {
                    version: Some(launch.version.clone()),
                },
            )
            .await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while read_registration(dir).is_ok_and(|r| r.token == reg.token)
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    start(dir, launch).await
}

/// Start a gateway in `dir` unless one starts meanwhile, and wait until it
/// answers.
async fn start(dir: &Path, launch: &Launch) -> io::Result<Registration> {
    let lock_path = dir.join(super::START_LOCK);
    let lock = tokio::task::spawn_blocking(move || -> io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)?;
        let deadline = std::time::Instant::now() + START_TIMEOUT;
        while !fs::try_lock(&f) {
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::other("another process is starting the gateway"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(f)
    })
    .await
    .map_err(io::Error::other)??;
    if status(dir).await.is_some() {
        drop(lock);
        return read_registration(dir);
    }
    let mut cmd = std::process::Command::new(&launch.program);
    cmd.arg(super::CHILD_ARG)
        .arg("--dir")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env_remove(crate::SERVER_TOKEN_VAR);
    {
        use std::os::unix::process::CommandExt;
        // Its own session: no terminal's signals reach it, and it has no
        // controlling terminal.
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn()?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let r = loop {
        if status(dir).await.is_some() {
            break read_registration(dir);
        }
        if let Ok(Some(st)) = child.try_wait() {
            if status(dir).await.is_some() {
                break read_registration(dir);
            }
            let why = last_error(dir).unwrap_or_else(|| st.to_string());
            break Err(io::Error::other(format!("the gateway stopped: {why}")));
        }
        if tokio::time::Instant::now() >= deadline {
            break Err(io::Error::other("the gateway didn't start in time"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // Reap it when it exits, so it never lingers as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    drop(lock);
    r
}
