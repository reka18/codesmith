//! A server's connection to its daemon, after OpenCode's
//! `makeDaemonTransport`: it finds the daemon through its registration,
//! starts it on demand, owns it, retries what is safe to retry, and hands
//! it to the next server.

use super::fs;
use super::protocol::{
    self, now_ms, Envelope, Exited, Handoff, Registration, Request, Response, AUTH_FAILED, PROTOCOL,
};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long one request (connect, send, reply) may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a started daemon may take to register.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a request failed. `Connect`, `Spawn`, `Registration` and
/// `Protocol` mean it never reached the daemon; `Response` means it was
/// sent and its reply was lost (it may have run); `Request` is the
/// daemon's own refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Connect,
    Response,
    Registration,
    Protocol,
    Spawn,
    Request,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonError {
    pub kind: Kind,
    pub message: String,
    /// The daemon's pid, for a protocol mismatch.
    pub pid: Option<u32>,
    /// The daemon's error code, for a refused terminal request.
    pub code: Option<String>,
}

impl DaemonError {
    pub fn new(kind: Kind, message: impl Into<String>) -> DaemonError {
        DaemonError {
            kind,
            message: message.into(),
            pid: None,
            code: None,
        }
    }

    /// Whether the request certainly did not run, so running it some other
    /// way is safe.
    pub fn unsent(&self) -> bool {
        !matches!(self.kind, Kind::Request | Kind::Response)
    }
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DaemonError {}

fn failure(kind: Kind, e: impl std::fmt::Display) -> DaemonError {
    DaemonError::new(kind, e.to_string())
}

/// How to start a daemon: `program args... --runtime-dir DIR --name NAME`.
#[derive(Clone, Debug, PartialEq)]
pub struct Launcher {
    pub program: PathBuf,
    pub args: Vec<String>,
}

struct OwnerConn {
    stream: Mutex<UnixStream>,
}

struct Inner {
    /// The daemon's runtime directory.
    dir: PathBuf,
    registration: Option<Registration>,
    owner: Option<Arc<OwnerConn>>,
    /// Set once a daemon this client started spoke another protocol.
    incompatible: Option<DaemonError>,
}

/// A server's daemon. Requests are blocking; call them off the async
/// runtime's workers where they may take a while (see [`Client::call`]).
pub struct Client {
    root: PathBuf,
    launcher: Option<Launcher>,
    inner: Mutex<Inner>,
    startup: Mutex<()>,
    closed: AtomicBool,
    handoff_expires: AtomicU64,
}

impl Client {
    /// A client for the daemon in `root/name`, started with `launcher`
    /// when a request needs one. With `inherited`, it first takes over the
    /// daemon a previous server handed over; if that fails, the daemon is
    /// told to shut down and a fresh one is used later.
    pub fn new(
        root: impl Into<PathBuf>,
        name: &str,
        launcher: Option<Launcher>,
        inherited: Option<Handoff>,
    ) -> Client {
        let root = root.into();
        let c = Client {
            inner: Mutex::new(Inner {
                dir: root.join(name),
                registration: None,
                owner: None,
                incompatible: None,
            }),
            root,
            launcher,
            startup: Mutex::new(()),
            closed: AtomicBool::new(false),
            handoff_expires: AtomicU64::new(0),
        };
        if let Some(h) = inherited {
            c.adopt(h);
        }
        c
    }

    fn adopt(&self, h: Handoff) {
        let mut found = None;
        let r = (|| {
            if h.expires_at <= now_ms() {
                return Err(DaemonError::new(
                    Kind::Registration,
                    "the shell daemon handoff expired",
                ));
            }
            let dir = PathBuf::from(&h.directory);
            self.inner.lock().unwrap().dir = dir.clone();
            let reg = discover(&dir)?;
            found = Some(reg.clone());
            if reg.instance_id != h.instance_id {
                return Err(DaemonError::new(
                    Kind::Registration,
                    "the handed-over shell daemon changed",
                ));
            }
            self.claim(&reg, Some(&h.ticket))
        })();
        if let Err(e) = r {
            // A daemon this server cannot take over is stopped, unless it
            // refused the claim: then someone else owns it (the ticket was
            // used already), and its commands are theirs.
            if let Some(reg) = found.filter(|_| e.kind != Kind::Request) {
                let _ = one_shot(&reg, &Request::Shutdown);
            }
            self.abandon();
        }
    }

    /// Forget the daemon and use a fresh runtime directory from now on.
    fn abandon(&self) {
        let mut i = self.inner.lock().unwrap();
        if let Some(o) = i.owner.take() {
            let _ = o.stream.lock().unwrap().shutdown(std::net::Shutdown::Both);
        }
        i.registration = None;
        i.dir = self.root.join(uuid());
    }

    /// The daemon's runtime directory.
    pub fn directory(&self) -> PathBuf {
        self.inner.lock().unwrap().dir.clone()
    }

    fn claim(&self, reg: &Registration, ticket: Option<&str>) -> Result<(), DaemonError> {
        let stream = connect(reg, REQUEST_TIMEOUT).map_err(|e| failure(Kind::Connect, e))?;
        let mut s = stream;
        let req = Request::Own {
            instance_id: reg.instance_id.clone(),
            ticket: ticket.map(str::to_string),
        };
        match exchange(&mut s, reg, &req) {
            Ok(Response::Owned) => {}
            // Refused: it has an owner, or the ticket is not good.
            Ok(Response::Error { message, .. }) => {
                return Err(DaemonError::new(Kind::Request, message))
            }
            Ok(other) => {
                return Err(DaemonError::new(
                    Kind::Connect,
                    format!("unexpected daemon response: {}", other.kind()),
                ))
            }
            Err(e) => return Err(failure(Kind::Connect, e)),
        }
        if self.closed.load(Ordering::SeqCst) {
            let _ = s.shutdown(std::net::Shutdown::Both);
            return Err(DaemonError::new(
                Kind::Connect,
                "the daemon client is closed",
            ));
        }
        // The owner connection stays open with no timeout until this
        // server ends; only handoff preparation uses it again.
        let _ = s.set_read_timeout(None);
        let mut i = self.inner.lock().unwrap();
        i.owner = Some(Arc::new(OwnerConn {
            stream: Mutex::new(s),
        }));
        i.registration = Some(reg.clone());
        Ok(())
    }

    fn spawn_daemon(&self) -> Result<Registration, DaemonError> {
        let Some(l) = &self.launcher else {
            return Err(DaemonError::new(Kind::Spawn, "no shell daemon program"));
        };
        let dir = self.directory();
        let (root, name) = (
            dir.parent().unwrap_or(&self.root).to_path_buf(),
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        fs::secure_dir(&root).map_err(|e| failure(Kind::Spawn, e))?;
        let mut cmd = std::process::Command::new(&l.program);
        cmd.args(&l.args)
            .arg("--runtime-dir")
            .arg(&root)
            .arg("--name")
            .arg(&name)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env_remove(crate::SERVER_TOKEN_VAR)
            .env_remove(super::HANDOFF_VAR);
        {
            use std::os::unix::process::CommandExt;
            // Its own session: no terminal's signals reach it.
            // SAFETY: setsid is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().map_err(|e| failure(Kind::Spawn, e))?;
        let pid = child.id();
        let deadline = Instant::now() + START_TIMEOUT;
        let mut last = None;
        let r = loop {
            if Instant::now() >= deadline {
                break Err(last.unwrap_or_else(|| {
                    DaemonError::new(Kind::Connect, "the shell daemon did not become ready")
                }));
            }
            if let Ok(Some(_)) = child.try_wait() {
                break Err(last.unwrap_or_else(|| {
                    DaemonError::new(Kind::Connect, "the shell daemon exited at startup")
                }));
            }
            match discover(&dir) {
                Ok(reg) => match self.claim(&reg, None) {
                    Ok(()) => break Ok(reg),
                    Err(e) => break Err(e),
                },
                Err(e) if e.kind == Kind::Protocol && e.pid == Some(pid) => {
                    self.inner.lock().unwrap().incompatible = Some(e.clone());
                    break Err(e);
                }
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if r.is_err() {
            // SAFETY: signalling the child this function started.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        // Reap it whenever it exits.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        r
    }

    /// Start a daemon; when one cannot be reached in this directory (an
    /// unreachable daemon may still hold its lock), once more in a fresh
    /// one. A program that cannot start, or that speaks another protocol,
    /// is not tried again.
    fn start(&self) -> Result<Registration, DaemonError> {
        match self.spawn_daemon() {
            Ok(r) => Ok(r),
            Err(e) if e.kind == Kind::Spawn || self.is_incompatible(&e) => Err(e),
            Err(_) => {
                self.abandon();
                self.spawn_daemon()
            }
        }
    }

    fn is_incompatible(&self, e: &DaemonError) -> bool {
        self.inner.lock().unwrap().incompatible.as_ref() == Some(e)
    }

    fn connect_daemon(&self, start: bool) -> Result<Registration, DaemonError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(DaemonError::new(
                Kind::Connect,
                "the daemon client is closed",
            ));
        }
        if let Some(r) = self.inner.lock().unwrap().registration.clone() {
            return Ok(r);
        }
        let _one = self.startup.lock().unwrap();
        if let Some(r) = self.inner.lock().unwrap().registration.clone() {
            return Ok(r);
        }
        let found = match discover(&self.directory()) {
            Ok(r) => r,
            Err(e) if start && e.kind == Kind::Connect => {
                if let Some(i) = self.inner.lock().unwrap().incompatible.clone() {
                    return Err(i);
                }
                return self.start();
            }
            Err(e) => return Err(e),
        };
        if self.inner.lock().unwrap().owner.is_none() {
            self.claim(&found, None)?;
        }
        self.inner.lock().unwrap().registration = Some(found.clone());
        Ok(found)
    }

    /// Run `f` against the live registration, forgetting it when the
    /// daemon says it is stale.
    fn with_registration<T>(
        &self,
        start: bool,
        f: impl Fn(&Registration) -> Result<T, DaemonError>,
    ) -> Result<T, DaemonError> {
        let reg = self.connect_daemon(start)?;
        f(&reg).inspect_err(|e| {
            if e.kind == Kind::Registration {
                let mut i = self.inner.lock().unwrap();
                if i.registration.as_ref() == Some(&reg) {
                    i.registration = None;
                }
            }
        })
    }

    fn forget(&self) {
        let mut i = self.inner.lock().unwrap();
        i.registration = None;
        i.owner = None;
    }

    /// Send `req`; with `start`, start the daemon when none runs. A
    /// request that failed before it was sent is retried once (after
    /// rediscovering the daemon); one whose reply was lost is not.
    pub fn request(&self, req: &Request, start: bool) -> Result<Response, DaemonError> {
        let attempt = |start| self.with_registration(start, |reg| one_shot(reg, req));
        match attempt(start) {
            Err(e) if e.kind == Kind::Registration => attempt(start),
            Err(e) if e.kind == Kind::Connect => {
                self.forget();
                if !start {
                    return Err(e);
                }
                attempt(true)
            }
            r => r,
        }
    }

    /// [`Client::request`] without starting a daemon: `None` when none can
    /// be reached.
    pub fn request_if_running(&self, req: &Request) -> Result<Option<Response>, DaemonError> {
        match self.request(req, false) {
            Ok(r) => Ok(Some(r)),
            Err(e) if e.kind == Kind::Connect => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Open a connection for `req` and read its first response, leaving
    /// the connection to stream (`wait`, `attach`, `events`). Never starts
    /// the daemon.
    pub fn stream(&self, req: &Request) -> Result<(Response, UnixStream), DaemonError> {
        let attempt = || {
            self.with_registration(false, |reg| {
                let mut s = connect(reg, REQUEST_TIMEOUT).map_err(|e| failure(Kind::Connect, e))?;
                let first = match exchange(&mut s, reg, req) {
                    Ok(r) => r,
                    // The connection was open, so the daemon went away.
                    Err(e) => return Err(failure(Kind::Connect, e)),
                };
                if let Response::Error { message, code } = first {
                    return Err(refusal(message, code));
                }
                let _ = s.set_read_timeout(None);
                Ok((first, s))
            })
        };
        match attempt() {
            Err(e) if e.kind == Kind::Registration => attempt(),
            r => r,
        }
    }

    /// Wait on a connection of its own, with no timeout, until pipe process
    /// `id` exits. Never starts the daemon. `cancel` closes the connection
    /// (the wait then fails), so a caller that stops waiting leaves nothing
    /// behind in the daemon.
    pub fn wait(&self, id: u64, cancel: &WaitHandle) -> Result<Exited, DaemonError> {
        let attempt = || {
            self.with_registration(false, |reg| {
                let s = connect(reg, REQUEST_TIMEOUT).map_err(|e| failure(Kind::Connect, e))?;
                let _ = s.set_read_timeout(None);
                if !cancel.set(&s) {
                    return Err(DaemonError::new(Kind::Connect, "the wait was cancelled"));
                }
                let mut s = s;
                let env = Envelope {
                    token: reg.token.clone(),
                    request: serde_json::to_value(Request::Wait { id }).unwrap(),
                };
                protocol::write_frame(&mut s, &env).map_err(|e| failure(Kind::Connect, e))?;
                match protocol::read_frame(&mut s) {
                    Ok(Some(body)) => match protocol::decode(&body) {
                        Ok(Response::Exited(e)) => Ok(e),
                        Ok(Response::Error { message, code }) => Err(refusal(message, code)),
                        Ok(other) => Err(DaemonError::new(
                            Kind::Response,
                            format!("unexpected daemon response: {}", other.kind()),
                        )),
                        Err(e) => Err(failure(Kind::Response, e)),
                    },
                    // The connection was open, so a missing reply means the
                    // daemon went away while the process ran.
                    Ok(None) | Err(_) => Err(DaemonError::new(
                        Kind::Connect,
                        "the shell daemon closed before the process exited",
                    )),
                }
            })
        };
        match attempt() {
            Err(e) if e.kind == Kind::Registration => attempt(),
            r => r,
        }
    }

    /// Stop the daemon (and everything it runs), if one runs, and wait for
    /// it to go. A prepared handoff is abandoned.
    pub fn shutdown(&self) -> Result<Option<Response>, DaemonError> {
        self.handoff_expires.store(0, Ordering::SeqCst);
        let r = self.request_if_running(&Request::Shutdown)?;
        self.forget();
        let Some(r) = r else { return Ok(None) };
        let deadline = Instant::now() + START_TIMEOUT;
        let dir = self.directory();
        while Instant::now() < deadline {
            if discover(&dir).is_err() {
                return Ok(Some(r));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(DaemonError::new(
            Kind::Connect,
            "the shell daemon did not stop",
        ))
    }

    /// Prepare a handoff on the owner connection: what the next server
    /// needs to take the daemon over. `None` when this client owns no
    /// daemon (nothing to hand over).
    pub fn handoff(&self) -> Result<Option<Handoff>, DaemonError> {
        let _one = self.startup.lock().unwrap();
        let (owner, reg, dir) = {
            let i = self.inner.lock().unwrap();
            (i.owner.clone(), i.registration.clone(), i.dir.clone())
        };
        let (Some(owner), Some(reg)) = (owner, reg) else {
            return Ok(None);
        };
        let mut s = owner.stream.lock().unwrap();
        let _ = s.set_read_timeout(Some(REQUEST_TIMEOUT));
        let r = exchange(&mut s, &reg, &Request::PrepareHandoff);
        let _ = s.set_read_timeout(None);
        match r.map_err(|e| failure(Kind::Response, e))? {
            Response::Handoff { ticket, expires_at } => {
                self.handoff_expires.store(expires_at, Ordering::SeqCst);
                Ok(Some(Handoff {
                    directory: dir.display().to_string(),
                    instance_id: reg.instance_id,
                    ticket,
                    expires_at,
                }))
            }
            Response::Error { message, .. } => Err(DaemonError::new(Kind::Request, message)),
            other => Err(DaemonError::new(
                Kind::Protocol,
                format!("expected a handoff ticket, got {}", other.kind()),
            )),
        }
    }

    /// Whether a handoff is prepared and still good: the next server takes
    /// the daemon's processes over, so this one must leave them running.
    pub fn handoff_prepared(&self) -> bool {
        now_ms() < self.handoff_expires.load(Ordering::SeqCst)
    }

    /// Whether this client owns a daemon now.
    pub fn owns(&self) -> bool {
        self.inner.lock().unwrap().owner.is_some()
    }

    /// Drop the owner connection: without a handoff the daemon then stops
    /// everything it runs.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let mut i = self.inner.lock().unwrap();
        if let Some(o) = i.owner.take() {
            let _ = o.stream.lock().unwrap().shutdown(std::net::Shutdown::Both);
        }
        i.registration = None;
    }

    /// Run a blocking request on the blocking pool.
    pub async fn call(
        self: &Arc<Self>,
        req: Request,
        start: bool,
    ) -> Result<Response, DaemonError> {
        let c = self.clone();
        tokio::task::spawn_blocking(move || c.request(&req, start))
            .await
            .unwrap_or_else(|e| Err(failure(Kind::Connect, e)))
    }

    /// [`Client::wait`] as a future that closes its connection when
    /// dropped.
    pub async fn wait_async(self: &Arc<Self>, id: u64) -> Result<Exited, DaemonError> {
        let handle = WaitHandle::default();
        let guard = CancelOnDrop(handle.clone());
        let c = self.clone();
        let r = tokio::task::spawn_blocking(move || c.wait(id, &handle))
            .await
            .unwrap_or_else(|e| Err(failure(Kind::Connect, e)));
        drop(guard);
        r
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

/// Closes a wait's connection from elsewhere.
#[derive(Clone, Default)]
pub struct WaitHandle(Arc<Mutex<(bool, Option<UnixStream>)>>);

impl WaitHandle {
    fn set(&self, s: &UnixStream) -> bool {
        let mut g = self.0.lock().unwrap();
        if g.0 {
            return false;
        }
        g.1 = s.try_clone().ok();
        true
    }

    /// Close the wait's connection; a wait not yet started fails at once.
    pub fn cancel(&self) {
        let mut g = self.0.lock().unwrap();
        g.0 = true;
        if let Some(s) = g.1.take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

struct CancelOnDrop(WaitHandle);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn refusal(message: String, code: Option<String>) -> DaemonError {
    DaemonError {
        kind: if message == AUTH_FAILED {
            Kind::Registration
        } else {
            Kind::Request
        },
        message,
        pid: None,
        code,
    }
}

/// A fresh runtime directory name.
fn uuid() -> String {
    crate::paths::random_token(16)
}

/// Connect to a registered daemon, refusing one run by another user.
fn connect(reg: &Registration, timeout: Duration) -> io::Result<UnixStream> {
    let s = UnixStream::connect(&reg.socket)?;
    if fs::peer_uid(s.as_raw_fd())? != fs::euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the daemon socket belongs to another user",
        ));
    }
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    Ok(s)
}

/// Send one request on `s` and read one response.
fn exchange(s: &mut UnixStream, reg: &Registration, req: &Request) -> io::Result<Response> {
    let env = Envelope {
        token: reg.token.clone(),
        request: serde_json::to_value(req).map_err(io::Error::other)?,
    };
    let frame = protocol::encode(&env)?;
    s.write_all(&frame)?;
    match protocol::read_frame(s)? {
        Some(body) => protocol::decode(&body),
        None => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the shell daemon closed without a response",
        )),
    }
}

/// One request on a connection of its own. Failing to connect is
/// `Connect`; anything after the request was sent is `Response`.
fn one_shot(reg: &Registration, req: &Request) -> Result<Response, DaemonError> {
    let mut s = connect(reg, REQUEST_TIMEOUT).map_err(|e| failure(Kind::Connect, e))?;
    let r = exchange(&mut s, reg, req).map_err(|e| failure(Kind::Response, e))?;
    let _ = s.shutdown(std::net::Shutdown::Both);
    match r {
        Response::Error { message, code } => Err(refusal(message, code)),
        r => Ok(r),
    }
}

/// Read and check the registration in `dir`: our protocol, and a daemon
/// that answers as the one registered.
pub fn discover(dir: &Path) -> Result<Registration, DaemonError> {
    let reg = fs::read_registration(dir).map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
            failure(Kind::Protocol, e)
        } else {
            failure(Kind::Connect, e)
        }
    })?;
    if reg.protocol != PROTOCOL {
        return Err(DaemonError {
            kind: Kind::Protocol,
            message: format!(
                "shell daemon protocol mismatch: daemon={}, client={PROTOCOL}",
                reg.protocol
            ),
            pid: Some(reg.pid),
            code: None,
        });
    }
    match one_shot(&reg, &Request::Ping) {
        Ok(Response::Pong {
            instance_id,
            pid,
            protocol,
            ..
        }) if instance_id == reg.instance_id && pid == reg.pid && protocol == PROTOCOL => Ok(reg),
        Ok(_) => Err(DaemonError::new(
            Kind::Protocol,
            "shell daemon registration mismatch",
        )),
        Err(e) if e.kind == Kind::Registration => Err(e),
        Err(e) if e.kind == Kind::Request => Err(DaemonError::new(Kind::Protocol, e.message)),
        Err(e) => Err(DaemonError::new(Kind::Connect, e.message)),
    }
}
