//! The process daemon's wire protocol, after OpenCode's `opencode-pty`
//! protocol: every frame is a four-byte big-endian length and that many
//! bytes of UTF-8 JSON, at most [`MAX_FRAME`]. A client sends
//! `{"token": ..., "request": {"op": ...}}` envelopes and reads
//! `{"type": ...}` responses. Most requests get one response on a
//! connection of their own; `own` keeps its connection (the owner's) open
//! for later requests, and `wait`, `attach` and `events` stream.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};

/// Bumped only by a change older clients cannot read; a client refuses a
/// daemon that registers another version.
pub const PROTOCOL: u32 = 1;
/// The feature a daemon registers when it applies `create_pipe`'s
/// `sandbox`.
pub const FEATURE_SANDBOX: &str = "sandbox";
/// The largest frame either side sends or accepts.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
/// The largest metadata object a pipe process may carry, serialized.
pub const MAX_METADATA: usize = 1024 * 1024;
/// What an authenticated client is told when its token is wrong: the
/// registration it read is stale.
pub const AUTH_FAILED: &str = "authentication failed";

/// What a running daemon publishes in `service.json` (mode 0600) in its
/// runtime directory: how to reach it and the credential every request
/// must carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub instance_id: String,
    pub pid: u32,
    pub protocol: u32,
    pub socket: String,
    pub token: String,
    #[serde(default)]
    pub features: Vec<String>,
}

/// What a server passes to the next one so it can take over its daemon
/// (OpenCode's `PersistentPty.Handoff`), as JSON in
/// [`super::HANDOFF_VAR`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    /// The daemon's runtime directory.
    pub directory: String,
    #[serde(rename = "instanceID")]
    pub instance_id: String,
    pub ticket: String,
    /// Unix milliseconds.
    pub expires_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Pipe,
    Pty,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    /// Become the daemon's owner: the first claim after it starts, or a
    /// successor presenting the handoff ticket.
    Own {
        instance_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ticket: Option<String>,
    },
    /// Owner only: let a successor claim the daemon within two minutes.
    PrepareHandoff,
    /// Stop the daemon and everything it runs.
    Shutdown,
    CreatePipe {
        program: String,
        args: Vec<String>,
        /// Absolute.
        cwd: String,
        /// The child's whole environment.
        env: BTreeMap<String, String>,
        group_id: String,
        /// Absolute; truncated on open, a symlink refused.
        output_path: String,
        metadata: Value,
        /// On Linux, the sandbox to apply in the child before it runs
        /// `program` (see [`crate::sandbox::linux`]). A daemon that
        /// registers [`FEATURE_SANDBOX`] applies it; on macOS the program
        /// is `sandbox-exec` instead, and this is unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<crate::sandbox::Policy>,
    },
    List {
        mode: Mode,
    },
    Get {
        id: u64,
    },
    /// Held open until the pipe process exits, then answered with `exited`.
    Wait {
        id: u64,
    },
    Signal {
        id: u64,
        signal: String,
    },
    SetMetadata {
        id: u64,
        metadata: Value,
    },
    /// Kill a running process and forget its record.
    Terminate {
        id: u64,
    },
    CreatePty {
        input: PtyCreate,
    },
    ListPty {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    GetPty {
        id: String,
    },
    UpdatePty {
        id: String,
        /// Absent: unchanged; `null`: cleared.
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        title: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<crate::pty::Size>,
    },
    WritePty {
        id: String,
        data: String,
    },
    RemovePty {
        id: String,
    },
    RemovePtySession {
        session: String,
    },
    ReadPty {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines: Option<u32>,
    },
    /// `attached`, then `output` frames until `end`.
    Attach {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<i64>,
    },
    /// Every terminal event, as `event` frames, until the client leaves.
    Events,
}

impl Request {
    pub fn op(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::Own { .. } => "own",
            Request::PrepareHandoff => "prepare_handoff",
            Request::Shutdown => "shutdown",
            Request::CreatePipe { .. } => "create_pipe",
            Request::List { .. } => "list",
            Request::Get { .. } => "get",
            Request::Wait { .. } => "wait",
            Request::Signal { .. } => "signal",
            Request::SetMetadata { .. } => "set_metadata",
            Request::Terminate { .. } => "terminate",
            Request::CreatePty { .. } => "create_pty",
            Request::ListPty { .. } => "list_pty",
            Request::GetPty { .. } => "get_pty",
            Request::UpdatePty { .. } => "update_pty",
            Request::WritePty { .. } => "write_pty",
            Request::RemovePty { .. } => "remove_pty",
            Request::RemovePtySession { .. } => "remove_pty_session",
            Request::ReadPty { .. } => "read_pty",
            Request::Attach { .. } => "attach",
            Request::Events => "events",
        }
    }
}

/// A terminal to start, as the server resolved it: an absolute directory
/// and a program (the daemon adds nothing but its own environment).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PtyCreate {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<crate::pty::Size>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Lifecycle {
    Running,
    Exited {
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
    Failed {
        message: String,
    },
}

/// A pipe process: a command with no terminal whose output the daemon
/// writes to `output_path`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub id: u64,
    pub pid: Option<u32>,
    pub command: Vec<String>,
    pub cwd: String,
    pub group_id: String,
    pub lifecycle: Lifecycle,
    /// Bytes of output so far (the file may hold a little more).
    pub output_tail: u64,
    /// Newlines written to the file so far.
    pub output_lines: u64,
    pub output_path: String,
    /// Why the daemon stopped writing the file early.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_error: Option<String>,
    pub metadata: Value,
    /// Unix milliseconds.
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exited {
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub final_offset: u64,
    #[serde(default)]
    pub output_lines: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_error: Option<String>,
    pub exited_at: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Pong {
        instance_id: String,
        pid: u32,
        protocol: u32,
        #[serde(default)]
        features: Vec<String>,
    },
    Owned,
    Handoff {
        ticket: String,
        expires_at: u64,
    },
    Ok,
    Created {
        process: ProcessInfo,
    },
    Processes {
        processes: Vec<ProcessInfo>,
    },
    Process {
        process: ProcessInfo,
    },
    Exited(Exited),
    Pty {
        info: crate::pty::Info,
    },
    Ptys {
        terminals: Vec<crate::pty::Info>,
    },
    Read {
        result: Option<crate::pty::ReadResult>,
    },
    Attached {
        replay: String,
        cursor: u64,
    },
    Output {
        data: String,
    },
    End {
        exit_code: Option<u32>,
    },
    Event {
        event: crate::pty::Event,
    },
    Error {
        message: String,
        /// For terminal requests, which [`crate::pty::PtyError`] it was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}

impl Response {
    pub fn error(message: impl Into<String>) -> Response {
        Response::Error {
            message: message.into(),
            code: None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Response::Pong { .. } => "pong",
            Response::Owned => "owned",
            Response::Handoff { .. } => "handoff",
            Response::Ok => "ok",
            Response::Created { .. } => "created",
            Response::Processes { .. } => "processes",
            Response::Process { .. } => "process",
            Response::Exited(_) => "exited",
            Response::Pty { .. } => "pty",
            Response::Ptys { .. } => "ptys",
            Response::Read { .. } => "read",
            Response::Attached { .. } => "attached",
            Response::Output { .. } => "output",
            Response::End { .. } => "end",
            Response::Event { .. } => "event",
            Response::Error { .. } => "error",
        }
    }
}

/// A field that is there (even as `null`) as `Some`.
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Serialize, Deserialize)]
pub struct Envelope {
    pub token: String,
    pub request: Value,
}

/// `value` as one frame.
pub fn encode(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon frame too large",
        ));
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn write_frame(w: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    w.write_all(&encode(value)?)?;
    w.flush()
}

/// The next frame's body; `None` when the stream ended cleanly between
/// frames. A frame cut off or over [`MAX_FRAME`] is an error.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// A response frame's body.
pub fn decode(body: &[u8]) -> io::Result<Response> {
    serde_json::from_slice(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The name of a signal the daemon delivers, and its number.
pub fn signal_number(name: &str) -> Option<i32> {
    Some(match name {
        "SIGTERM" => libc::SIGTERM,
        "SIGINT" => libc::SIGINT,
        "SIGKILL" => libc::SIGKILL,
        _ => return None,
    })
}

/// A signal's name, for an exit.
pub fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGHUP => "SIGHUP".into(),
        libc::SIGINT => "SIGINT".into(),
        libc::SIGQUIT => "SIGQUIT".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGPIPE => "SIGPIPE".into(),
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGABRT => "SIGABRT".into(),
        libc::SIGSEGV => "SIGSEGV".into(),
        n => format!("SIG{n}"),
    }
}

/// A signal's number from its name, for any name [`signal_name`] gives.
pub fn signal_from_name(name: &str) -> Option<i32> {
    Some(match name {
        "SIGHUP" => libc::SIGHUP,
        "SIGINT" => libc::SIGINT,
        "SIGQUIT" => libc::SIGQUIT,
        "SIGKILL" => libc::SIGKILL,
        "SIGPIPE" => libc::SIGPIPE,
        "SIGTERM" => libc::SIGTERM,
        "SIGABRT" => libc::SIGABRT,
        "SIGSEGV" => libc::SIGSEGV,
        other => return other.strip_prefix("SIG")?.parse().ok(),
    })
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
