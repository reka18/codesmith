//! Terminals: programs (usually a shell) running on pseudo-terminals that
//! outlive any one client, ported from OpenCode's `Pty` service and the
//! session terminals of its `PersistentPty`. A client creates a terminal,
//! writes to it, resizes and renames it, attaches to its output (a replay
//! of what it kept, then live output) and removes it. Terminals are not
//! offered to the model; `codesmith serve` exposes them to its clients.
//!
//! Terminals live in the process that made them (when it ends, the kernel
//! hangs them up), or, made with [`Ptys::remote`], in the server's shell
//! daemon (see [`crate::daemon`]), which keeps them running across a
//! server restart that hands it over, as OpenCode's persistent terminals.
//!
//! Limits, so terminals cannot take the machine down:
//!
//! - at most [`Limits::running`] terminals run at once; creating another
//!   fails with [`PtyError::Limit`];
//! - each keeps at most [`Limits::scrollback`] bytes of output (the oldest
//!   output goes first);
//! - an exited terminal is kept (status, exit code, output) until removed,
//!   but only the [`Limits::exited`] most recent are: older ones are removed;
//! - an attachment that falls more than [`Limits::pending`] bytes behind is
//!   dropped (its stream ends) rather than queueing without bound.
//!
//! So scrollback never takes more than `(running + exited) × scrollback`.
//!
//! A terminal's program leads its own session and process group. Removing
//! the terminal, removing the session it belongs to, dropping the manager
//! or the program exiting stops the whole group and the terminal's
//! foreground group (see [`sys`]).

pub mod protocol;
mod remote;
mod sys;
pub mod ticket;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// How much output one terminal keeps (OpenCode's `BUFFER_LIMIT`).
pub const SCROLLBACK: usize = 2 * 1024 * 1024;
/// How many exited terminals are kept (OpenCode's `EXITED_LIMIT`).
pub const EXITED: usize = 25;
/// How many terminals may run at once.
pub const RUNNING: usize = 16;
/// How far an attachment may fall behind before it is dropped.
pub const PENDING: usize = 4 * 1024 * 1024;

/// How long a removed terminal's program gets after `SIGHUP` before
/// `SIGKILL`.
const STOP_GRACE: Duration = Duration::from_millis(500);
/// How long output is still read after the program exits.
const DRAIN: Duration = Duration::from_millis(250);
/// How often the reader checks on the program.
const TICK: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub running: usize,
    pub exited: usize,
    pub scrollback: usize,
    pub pending: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            running: RUNNING,
            exited: EXITED,
            scrollback: SCROLLBACK,
            pending: PENDING,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Exited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Default for Size {
    fn default() -> Self {
        Size { cols: 80, rows: 24 }
    }
}

/// A terminal, as OpenCode's `Pty.Info` (with the session, the title the
/// user set and the size of its persistent terminals).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub id: String,
    /// The user's title when set, else the one it was created with.
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: Status,
    /// 0 when unknown.
    pub pid: u32,
    /// Once exited: the program's code, or 128 + the signal that ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub user_title: Option<String>,
    #[serde(default)]
    pub size: Size,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreateInput {
    /// The program; the configured shell when empty.
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    /// The default directory when empty; a relative one is taken from it.
    pub cwd: Option<String>,
    pub title: Option<String>,
    /// Added to this process's environment.
    pub env: Option<BTreeMap<String, String>>,
    pub size: Option<Size>,
    /// The session the terminal belongs to; it is removed with it.
    #[serde(skip)]
    pub session_id: Option<String>,
}

/// A change to a terminal. `title: Some(None)` clears the user's title.
#[derive(Clone, Debug, Default)]
pub struct UpdateInput {
    pub title: Option<Option<String>>,
    pub size: Option<Size>,
}

/// What [`Ptys::events`] publishes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "pty.created")]
    Created { info: Info },
    #[serde(rename = "pty.updated")]
    Updated { info: Info },
    #[serde(rename = "pty.exited")]
    Exited {
        id: String,
        #[serde(rename = "exitCode")]
        exit_code: u32,
    },
    #[serde(rename = "pty.deleted")]
    Deleted {
        id: String,
        #[serde(rename = "sessionID", skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
}

/// What an attachment receives after its replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Data(String),
    /// No more output: the program exited (with its code), or the terminal
    /// was removed (none).
    End {
        exit_code: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyError {
    NotFound(String),
    /// Attaching to a terminal whose program has exited.
    Exited(String),
    /// Already [`Limits::running`] terminals running.
    Limit(usize),
    InvalidCwd(String),
    Invalid(String),
    Spawn(String),
}

impl std::fmt::Display for PtyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PtyError::NotFound(id) => write!(f, "PTY session not found: {id}"),
            PtyError::Exited(id) => write!(f, "PTY session exited: {id}"),
            PtyError::Limit(n) => write!(f, "too many terminals running (at most {n})"),
            PtyError::InvalidCwd(cwd) => write!(f, "cwd is not a directory: {cwd}"),
            PtyError::Invalid(m) => f.write_str(m),
            PtyError::Spawn(m) => write!(f, "could not start the terminal: {m}"),
        }
    }
}

impl std::error::Error for PtyError {}

/// What a session's current terminal shows, for
/// [`Ptys::read`]: its last lines with control sequences taken out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResult {
    #[serde(rename = "ptyID")]
    pub pty_id: String,
    pub title: String,
    pub cwd: String,
    pub screen: Screen,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    pub text: String,
    pub cols: u16,
    pub rows: u16,
}

/// The most lines [`Ptys::read`] returns (OpenCode's `ReadLines`).
pub const READ_LINES: u32 = 65_535;

struct Sub {
    tx: mpsc::UnboundedSender<Output>,
    pending: Arc<AtomicUsize>,
}

struct Data {
    info: Info,
    /// The title it was created with.
    base_title: String,
    /// Retained output, ending at `cursor`.
    buffer: String,
    /// Bytes of output ever produced.
    cursor: u64,
    subs: HashMap<u64, Sub>,
    next_sub: u64,
}

impl Data {
    fn notify_end(&mut self, exit_code: Option<u32>) {
        for (_, s) in self.subs.drain() {
            let _ = s.tx.send(Output::End { exit_code });
        }
    }
}

struct Term {
    id: String,
    master: OwnedFd,
    write: Mutex<()>,
    data: Mutex<Data>,
    /// Set when the terminal is removed: its reader stops the program.
    stop: AtomicBool,
}

struct State {
    terms: BTreeMap<String, Arc<Term>>,
    exit_order: VecDeque<String>,
    /// Each session's current terminal: the one last written to, resized
    /// or attached to.
    current: HashMap<String, String>,
}

struct Shared {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    limits: Limits,
    directory: PathBuf,
    shell: String,
}

impl Drop for Shared {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        for t in st.terms.values() {
            t.stop.store(true, Ordering::SeqCst);
            t.data
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .notify_end(None);
        }
    }
}

/// The terminals of one server (or test). Cloning shares them; when the
/// last clone is dropped every terminal is stopped.
#[derive(Clone)]
pub struct Ptys {
    shared: Arc<Shared>,
    /// Set for terminals that live in the shell daemon.
    remote: Option<Arc<remote::Remote>>,
    /// Alive while any clone is: the daemon event forwarder stops with it.
    alive: Arc<()>,
}

impl Ptys {
    /// Terminals that start in `directory` and run `shell` by default.
    pub fn new(directory: impl Into<PathBuf>, shell: impl Into<String>) -> Ptys {
        Ptys::with_limits(directory, shell, Limits::default())
    }

    pub fn with_limits(
        directory: impl Into<PathBuf>,
        shell: impl Into<String>,
        limits: Limits,
    ) -> Ptys {
        let (events, _) = broadcast::channel(256);
        Ptys {
            remote: None,
            alive: Arc::new(()),
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    terms: BTreeMap::new(),
                    exit_order: VecDeque::new(),
                    current: HashMap::new(),
                }),
                events,
                limits,
                directory: directory.into(),
                shell: shell.into(),
            }),
        }
    }

    /// Terminals that live in `client`'s daemon, started in `directory`
    /// with `shell` by default. The daemon is started by the first
    /// [`Ptys::create`]; terminals it already runs (handed over by a
    /// previous server) are listed and attached to as they are.
    pub fn remote(
        client: Arc<crate::daemon::Client>,
        directory: impl Into<PathBuf>,
        shell: impl Into<String>,
    ) -> Ptys {
        let directory = directory.into();
        let shell = shell.into();
        let mut p = Ptys::new(directory.clone(), shell.clone());
        p.remote = Some(Arc::new(remote::Remote::new(client, directory, shell)));
        p.follow();
        p
    }

    /// Republish the daemon's events, when it runs.
    fn follow(&self) {
        if let Some(r) = &self.remote {
            r.follow(self.shared.events.clone(), Arc::downgrade(&self.alive));
        }
    }

    pub fn limits(&self) -> Limits {
        self.shared.limits
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.shared.events.subscribe()
    }

    fn publish(&self, e: Event) {
        let _ = self.shared.events.send(e);
    }

    fn term(&self, id: &str) -> Result<Arc<Term>, PtyError> {
        self.shared
            .state
            .lock()
            .unwrap()
            .terms
            .get(id)
            .cloned()
            .ok_or_else(|| PtyError::NotFound(id.into()))
    }

    /// Every terminal (of `session` when given), oldest first, exited ones
    /// included.
    pub fn list(&self, session: Option<&str>) -> Vec<Info> {
        if let Some(r) = &self.remote {
            return r.list(session);
        }
        let st = self.shared.state.lock().unwrap();
        st.terms
            .values()
            .map(|t| t.data.lock().unwrap().info.clone())
            .filter(|i| session.is_none_or(|s| i.session_id.as_deref() == Some(s)))
            .collect()
    }

    pub fn get(&self, id: &str) -> Result<Info, PtyError> {
        if let Some(r) = &self.remote {
            return r.get(id);
        }
        Ok(self.term(id)?.data.lock().unwrap().info.clone())
    }

    pub fn create(&self, input: CreateInput) -> Result<Info, PtyError> {
        if let Some(r) = &self.remote {
            let info = r.create(input)?;
            self.follow();
            return Ok(info);
        }
        let sh = &self.shared;
        let command = input
            .command
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| sh.shell.clone());
        let mut args = input.args.unwrap_or_default();
        if login(&command) {
            args.push("-l".into());
        }
        let cwd = match input.cwd.filter(|c| !c.is_empty()) {
            Some(c) => sh.directory.join(c),
            None => sh.directory.clone(),
        };
        if !cwd.is_dir() {
            return Err(PtyError::InvalidCwd(cwd.display().to_string()));
        }
        let size = input.size.unwrap_or_default();
        if size.cols == 0 || size.rows == 0 {
            return Err(PtyError::Invalid("size must be positive".into()));
        }
        let mut env: BTreeMap<String, String> = std::env::vars().collect();
        env.extend(input.env.unwrap_or_default());
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("CODESMITH_TERMINAL".into(), "1".into());
        // A terminal must not be able to drive the server.
        env.remove(crate::SERVER_TOKEN_VAR);

        // Hold the state lock across the spawn so two creates cannot both
        // pass the running limit.
        let mut st = sh.state.lock().unwrap();
        let running = st
            .terms
            .values()
            .filter(|t| t.data.lock().unwrap().info.status == Status::Running)
            .count();
        if running >= sh.limits.running {
            return Err(PtyError::Limit(sh.limits.running));
        }
        let program = program(&command, &env);
        let sys::Spawned { master, child } =
            sys::spawn(&program, &args, &cwd, &env, size.cols, size.rows)
                .map_err(|e| PtyError::Spawn(format!("{command}: {e}")))?;
        let id = new_id();
        let base_title = input
            .title
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("Terminal {}", &id[id.len() - 4..]));
        let info = Info {
            id: id.clone(),
            title: base_title.clone(),
            command,
            args,
            cwd: cwd.display().to_string(),
            status: Status::Running,
            pid: child.id(),
            exit_code: None,
            session_id: input.session_id,
            user_title: None,
            size,
        };
        let term = Arc::new(Term {
            id: id.clone(),
            master,
            write: Mutex::new(()),
            data: Mutex::new(Data {
                info: info.clone(),
                base_title,
                buffer: String::new(),
                cursor: 0,
                subs: HashMap::new(),
                next_sub: 0,
            }),
            stop: AtomicBool::new(false),
        });
        st.terms.insert(id.clone(), term.clone());
        drop(st);
        let weak = Arc::downgrade(&self.shared);
        let started = std::thread::Builder::new()
            .name(format!("codesmith-{id}"))
            .spawn(move || run(term, child, weak));
        if let Err(e) = started {
            // Without a reader nothing would stop it: undo.
            let _ = self.remove(&id);
            return Err(PtyError::Spawn(e.to_string()));
        }
        self.publish(Event::Created { info: info.clone() });
        Ok(info)
    }

    pub fn update(&self, id: &str, input: UpdateInput) -> Result<Info, PtyError> {
        if let Some(r) = &self.remote {
            return r.update(id, input.title, input.size);
        }
        let t = self.term(id)?;
        if let Some(size) = input.size {
            if size.cols == 0 || size.rows == 0 {
                return Err(PtyError::Invalid("size must be positive".into()));
            }
        }
        let info = {
            let mut d = t.data.lock().unwrap();
            if let Some(title) = input.title {
                let title = title.filter(|t| !t.is_empty());
                d.info.title = title.clone().unwrap_or_else(|| d.base_title.clone());
                d.info.user_title = title;
            }
            if let Some(size) = input.size {
                if d.info.status == Status::Running {
                    let _ = sys::resize(&t.master, size.cols, size.rows);
                }
                d.info.size = size;
            }
            d.info.clone()
        };
        if input.size.is_some() {
            self.select(&info);
        }
        self.publish(Event::Updated { info: info.clone() });
        Ok(info)
    }

    /// Set the user's title (`None` clears it), as `PersistentPty.rename`.
    pub fn rename(&self, id: &str, title: Option<&str>) -> Result<Info, PtyError> {
        self.update(
            id,
            UpdateInput {
                title: Some(title.map(str::to_string)),
                size: None,
            },
        )
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<Info, PtyError> {
        self.update(
            id,
            UpdateInput {
                title: None,
                size: Some(Size { cols, rows }),
            },
        )
    }

    /// Type `data` into the terminal; nothing once its program has exited.
    /// Blocks while the program is not reading, for a few seconds at most.
    pub fn write(&self, id: &str, data: &str) -> Result<(), PtyError> {
        if let Some(r) = &self.remote {
            return r.write(id, data);
        }
        let t = self.term(id)?;
        let info = t.data.lock().unwrap().info.clone();
        self.select(&info);
        write_term(&t, data)
    }

    /// Stop the terminal (if it runs) and forget it.
    pub fn remove(&self, id: &str) -> Result<(), PtyError> {
        if let Some(r) = &self.remote {
            return r.remove(id);
        }
        let t = {
            let mut st = self.shared.state.lock().unwrap();
            let t = st
                .terms
                .remove(id)
                .ok_or_else(|| PtyError::NotFound(id.into()))?;
            st.exit_order.retain(|x| x != id);
            st.current.retain(|_, v| v != id);
            t
        };
        let session_id = teardown(&t);
        self.publish(Event::Deleted {
            id: id.into(),
            session_id,
        });
        Ok(())
    }

    /// Remove every terminal of `session` (it was deleted).
    pub fn remove_session(&self, session: &str) {
        if let Some(r) = &self.remote {
            return r.remove_session(session);
        }
        for info in self.list(Some(session)) {
            let _ = self.remove(&info.id);
        }
        self.shared.state.lock().unwrap().current.remove(session);
    }

    /// Remove every terminal.
    pub fn remove_all(&self) {
        for info in self.list(None) {
            let _ = self.remove(&info.id);
        }
    }

    /// Follow the terminal's output: what it kept from `cursor` on (`None`:
    /// all of it; `Some(-1)`: none, only what comes next), then live output.
    pub fn attach(&self, id: &str, cursor: Option<i64>) -> Result<Attachment, PtyError> {
        if let Some(r) = &self.remote {
            return r.attach(id, cursor);
        }
        let t = self.term(id)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let mut d = t.data.lock().unwrap();
        if d.info.status != Status::Running {
            return Err(PtyError::Exited(id.into()));
        }
        let end = d.cursor;
        let start = end - d.buffer.len() as u64;
        let from = match cursor {
            Some(-1) => end,
            Some(c) => (c.max(0) as u64).clamp(start, end),
            None => start,
        };
        let mut at = (from - start) as usize;
        while !d.buffer.is_char_boundary(at) {
            at += 1;
        }
        let replay = d.buffer[at..].to_string();
        let sub = d.next_sub;
        d.next_sub += 1;
        d.subs.insert(
            sub,
            Sub {
                tx,
                pending: pending.clone(),
            },
        );
        let info = d.info.clone();
        drop(d);
        self.select(&info);
        Ok(Attachment {
            replay,
            cursor: end,
            sub,
            rx,
            pending,
            term: Arc::downgrade(&t),
            remote: None,
        })
    }

    /// The last `lines` lines (default: its height) of the session's
    /// current terminal, without control sequences; `None` when the session
    /// has none. Unlike OpenCode, which reads a terminal emulator's screen,
    /// this reads the kept output, so full-screen programs read roughly.
    pub fn read(&self, session: &str, lines: Option<u32>) -> Result<Option<ReadResult>, PtyError> {
        if let Some(r) = self
            .remote
            .as_ref()
            .filter(|_| lines.is_none_or(|n| n > 0 && n <= READ_LINES))
        {
            return r.read(session, lines);
        }
        if lines.is_some_and(|n| n == 0 || n > READ_LINES) {
            return Err(PtyError::Invalid(format!(
                "lines must be an integer between 1 and {READ_LINES}"
            )));
        }
        let id = {
            let st = self.shared.state.lock().unwrap();
            match st.current.get(session) {
                Some(id) => id.clone(),
                None => return Ok(None),
            }
        };
        let Ok(t) = self.term(&id) else {
            return Ok(None);
        };
        let d = t.data.lock().unwrap();
        if d.info.session_id.as_deref() != Some(session) {
            return Ok(None);
        }
        let n = lines.unwrap_or(d.info.size.rows as u32) as usize;
        let text = plain(&d.buffer);
        let all: Vec<&str> = text.split('\n').collect();
        let keep = all.len().saturating_sub(n);
        Ok(Some(ReadResult {
            pty_id: d.info.id.clone(),
            title: d.info.title.clone(),
            cwd: d.info.cwd.clone(),
            screen: Screen {
                text: all[keep..].join("\n"),
                cols: d.info.size.cols,
                rows: d.info.size.rows,
            },
        }))
    }

    fn select(&self, info: &Info) {
        if let Some(s) = &info.session_id {
            let mut st = self.shared.state.lock().unwrap();
            if st.terms.contains_key(&info.id) {
                st.current.insert(s.clone(), info.id.clone());
            }
        }
    }
}

fn write_term(t: &Term, data: &str) -> Result<(), PtyError> {
    if t.data.lock().unwrap().info.status != Status::Running {
        return Ok(());
    }
    let _g = t.write.lock().unwrap();
    sys::write_all(&t.master, data.as_bytes()).map_err(|e| PtyError::Invalid(e.to_string()))
}

/// Stop a terminal taken out of the state: its attachments end and its
/// reader stops the program. Its session.
fn teardown(t: &Term) -> Option<String> {
    t.stop.store(true, Ordering::SeqCst);
    let mut d = t.data.lock().unwrap();
    d.notify_end(None);
    d.info.session_id.clone()
}

/// A client following one terminal's output. Dropping it detaches.
pub struct Attachment {
    /// Kept output from the requested cursor to `cursor`.
    pub replay: String,
    /// The absolute output cursor after the replay.
    pub cursor: u64,
    sub: u64,
    rx: mpsc::UnboundedReceiver<Output>,
    pending: Arc<AtomicUsize>,
    term: Weak<Term>,
    remote: Option<RemoteAttachment>,
}

/// An attachment to a terminal in the daemon: its stream, closed on drop.
struct RemoteAttachment {
    stream: std::os::unix::net::UnixStream,
    client: Arc<crate::daemon::Client>,
    id: String,
}

impl Attachment {
    fn took(&self, o: &Output) {
        if let Output::Data(s) = o {
            self.pending.fetch_sub(s.len(), Ordering::SeqCst);
        }
    }

    /// The next output; `None` once the stream is over (after an
    /// [`Output::End`], or when this attachment fell too far behind).
    pub async fn recv(&mut self) -> Option<Output> {
        let o = self.rx.recv().await?;
        self.took(&o);
        Some(o)
    }

    /// The next output if there is one now.
    pub fn try_recv(&mut self) -> Option<Output> {
        let o = self.rx.try_recv().ok()?;
        self.took(&o);
        Some(o)
    }

    /// Type into the terminal.
    pub fn write(&self, data: &str) -> Result<(), PtyError> {
        if let Some(r) = &self.remote {
            let req = crate::daemon::protocol::Request::WritePty {
                id: r.id.clone(),
                data: data.into(),
            };
            return r
                .client
                .request(&req, false)
                .map(|_| ())
                .map_err(|e| PtyError::Spawn(e.message));
        }
        match self.term.upgrade() {
            Some(t) => write_term(&t, data),
            None => Ok(()),
        }
    }

    /// Stop receiving output.
    pub fn detach(self) {}
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if let Some(r) = &self.remote {
            let _ = r.stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.term.upgrade() {
            t.data.lock().unwrap().subs.remove(&self.sub);
        }
    }
}

/// The shells OpenCode starts as login shells (`ShellSelect.login`).
fn login(command: &str) -> bool {
    crate::shell_select::login(command)
}

/// `command` as a path: a bare name is looked up on the terminal's `PATH`.
fn program(command: &str, env: &BTreeMap<String, String>) -> String {
    if command.contains('/') {
        return command.into();
    }
    env.get("PATH")
        .and_then(|paths| {
            std::env::split_paths(paths)
                .map(|d| d.join(command))
                .find(|p| p.is_file())
        })
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| command.into())
}

/// An ascending id, as OpenCode's `Pty.ID.ascending()`.
fn new_id() -> String {
    use rand::Rng;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst) & 0xffff;
    format!(
        "pty_{ms:012x}{seq:04x}{:06x}",
        rand::thread_rng().gen::<u32>() & 0xff_ffff
    )
}

/// Decodes output as UTF-8 across reads: a character cut off by one read
/// is finished by the next; bytes that are not UTF-8 become U+FFFD.
#[derive(Default)]
struct Utf8 {
    carry: Vec<u8>,
}

impl Utf8 {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest: &[u8] = &self.carry;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (ok, bad) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(ok).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{fffd}');
                            rest = &bad[n..];
                        }
                        None => {
                            rest = bad;
                            break;
                        }
                    }
                }
            }
        }
        self.carry = rest.to_vec();
        out
    }

    fn finish(&mut self) -> String {
        let s = if self.carry.is_empty() {
            String::new()
        } else {
            "\u{fffd}".into()
        };
        self.carry.clear();
        s
    }
}

/// Add output to a terminal: to its buffer (trimmed to `limit`) and to
/// every attachment, dropping those too far behind.
fn deliver(t: &Term, chunk: String, limits: &Limits) {
    if chunk.is_empty() {
        return;
    }
    let mut d = t.data.lock().unwrap();
    d.cursor += chunk.len() as u64;
    d.subs.retain(|_, s| {
        let before = s.pending.fetch_add(chunk.len(), Ordering::SeqCst);
        before + chunk.len() <= limits.pending && s.tx.send(Output::Data(chunk.clone())).is_ok()
    });
    d.buffer.push_str(&chunk);
    if d.buffer.len() > limits.scrollback {
        let mut cut = d.buffer.len() - limits.scrollback;
        while !d.buffer.is_char_boundary(cut) {
            cut += 1;
        }
        d.buffer.drain(..cut);
    }
}

/// A terminal's reader: copies output until the program exits (then kills
/// what is left of its groups) or the terminal is removed (then stops the
/// program), and reaps the program.
fn run(t: Arc<Term>, mut child: std::process::Child, shared: Weak<Shared>) {
    let limits = shared.upgrade().map(|s| s.limits).unwrap_or_default();
    let mut utf8 = Utf8::default();
    let mut buf = vec![0u8; 64 * 1024];
    let mut closed = false;
    let mut exited: Option<(std::process::ExitStatus, Instant)> = None;
    loop {
        if t.stop.load(Ordering::SeqCst) {
            if exited.is_none() {
                sys::stop(&t.master, &mut child, STOP_GRACE);
            } else {
                sys::kill_groups(&t.master, child.id());
            }
            return;
        }
        if closed {
            std::thread::sleep(TICK);
        } else {
            match sys::read(&t.master, &mut buf, TICK) {
                sys::Read::Data(n) => {
                    let s = utf8.push(&buf[..n]);
                    deliver(&t, s, &limits);
                }
                sys::Read::Empty => {}
                sys::Read::Closed => closed = true,
            }
        }
        match exited {
            None => {
                if let Ok(Some(status)) = child.try_wait() {
                    // The program is gone: so is anything it left behind.
                    sys::kill_groups(&t.master, child.id());
                    exited = Some((status, Instant::now()));
                }
            }
            Some((status, at)) => {
                if closed || at.elapsed() >= DRAIN {
                    let tail = utf8.finish();
                    deliver(&t, tail, &limits);
                    finish(&t, status, &shared);
                    return;
                }
            }
        }
    }
}

/// The program exited on its own: record it, end the attachments, publish
/// it, and remove the oldest exited terminals beyond the limit.
fn finish(t: &Arc<Term>, status: std::process::ExitStatus, shared: &Weak<Shared>) {
    let code = sys::exit_code(status);
    {
        let mut d = t.data.lock().unwrap();
        if d.info.status == Status::Exited {
            return;
        }
        d.info.status = Status::Exited;
        d.info.exit_code = Some(code);
        d.notify_end(Some(code));
    }
    let Some(shared) = shared.upgrade() else {
        return;
    };
    let ptys = Ptys {
        shared,
        remote: None,
        alive: Arc::new(()),
    };
    let evict: Vec<String> = {
        let mut st = ptys.shared.state.lock().unwrap();
        if !st.terms.contains_key(&t.id) {
            return;
        }
        st.exit_order.push_back(t.id.clone());
        let over = st
            .exit_order
            .len()
            .saturating_sub(ptys.shared.limits.exited);
        st.exit_order.iter().take(over).cloned().collect()
    };
    ptys.publish(Event::Exited {
        id: t.id.clone(),
        exit_code: code,
    });
    for id in evict {
        let _ = ptys.remove(&id);
    }
}

/// `text` without terminal control sequences: CSI and OSC sequences and
/// other escapes go, a carriage return starts its line over, a backspace
/// takes back a character.
pub fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut line = String::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    for c in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('_') | Some('^') => {
                    while let Some(c) = it.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && it.peek() == Some(&'\\') {
                            it.next();
                            break;
                        }
                    }
                }
                Some('(') | Some(')') | Some('#') => {
                    it.next();
                }
                _ => {}
            },
            '\r' => {
                if it.peek() != Some(&'\n') {
                    line.clear();
                }
            }
            '\n' => {
                out.push_str(&line);
                out.push('\n');
                line.clear();
            }
            '\x08' => {
                line.pop();
            }
            '\t' => line.push('\t'),
            c if c.is_control() => {}
            c => line.push(c),
        }
    }
    out.push_str(&line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_reads_is_kept_whole() {
        let mut u = Utf8::default();
        let e = "é".as_bytes();
        assert_eq!(u.push(&e[..1]), "");
        assert_eq!(u.push(&e[1..]), "é");
        assert_eq!(u.push(&[0xff, b'a']), "\u{fffd}a");
        assert_eq!(u.push(&e[..1]), "");
        assert_eq!(u.finish(), "\u{fffd}");
    }

    #[test]
    fn plain_text_drops_control_sequences() {
        assert_eq!(
            plain("\x1b[1;32mok\x1b[0m\r\n\x1b]0;title\x07next\rover\nab\x08c"),
            "ok\nover\nac"
        );
    }

    #[test]
    fn login_shells() {
        assert!(login("/bin/bash") && login("zsh") && login("sh"));
        assert!(!login("cat") && !login("/usr/bin/env"));
    }
}
