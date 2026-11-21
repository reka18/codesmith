//! The daemon process (`codesmith __shelld`): it runs shell commands as
//! pipe processes and terminals for its owning server, and keeps them
//! running while the server hands them to the next one.

use super::fs;
use super::protocol::{
    self, now_ms, Envelope, Exited, Lifecycle, Mode, ProcessInfo, PtyCreate, Registration, Request,
    Response, AUTH_FAILED, MAX_FRAME, MAX_METADATA, PROTOCOL,
};
use crate::pty::{CreateInput, Output, PtyError, Ptys, UpdateInput};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;

/// What bounds a daemon.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Pipe processes running at once; creating another fails.
    pub running: usize,
    /// Pipe process records (running or finished, until terminated).
    pub records: usize,
    /// Client connections open at once; more are closed at once.
    pub connections: usize,
    /// Terminals running at once.
    pub terminals: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            running: 64,
            records: 1024,
            connections: 128,
            terminals: crate::pty::RUNNING,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// `DIR/NAME`: its registration and lock.
    pub runtime: PathBuf,
    /// How long it waits for its first owner.
    pub claim_within: Duration,
    /// How long a prepared handoff stays good.
    pub handoff_ttl: Duration,
    /// How long a stopped process gets after SIGTERM before SIGKILL.
    pub kill_grace: Duration,
    pub limits: Limits,
}

impl Options {
    pub fn new(runtime: impl Into<PathBuf>) -> Options {
        Options {
            runtime: runtime.into(),
            claim_within: Duration::from_secs(5),
            handoff_ttl: Duration::from_secs(120),
            kill_grace: Duration::from_secs(3),
            limits: Limits::default(),
        }
    }
}

/// How long a connection may take to send its first request.
const FIRST_REQUEST: Duration = Duration::from_secs(5);
/// How long output is still read after a pipe process exits.
const DRAIN: Duration = Duration::from_secs(1);
/// Abandoned sibling runtime directories older than this are removed.
const ABANDONED: Duration = Duration::from_secs(10 * 60);
/// Stopping takes at most this long before the daemon exits anyway.
const STOP_LIMIT: Duration = Duration::from_secs(10);

/// The `__shelld` entry point: `--runtime-dir DIR --name NAME`. Does not
/// return.
pub fn main(args: &[String]) -> ! {
    let mut dir = None;
    let mut name = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--runtime-dir" => dir = it.next().cloned(),
            "--name" => name = it.next().cloned(),
            _ => {}
        }
    }
    let (Some(dir), Some(name)) = (dir, name) else {
        eprintln!("usage: codesmith __shelld --runtime-dir DIR --name NAME");
        std::process::exit(2);
    };
    if !fs::valid_name(&name) || !Path::new(&dir).is_absolute() {
        eprintln!("codesmith __shelld: invalid runtime directory or name");
        std::process::exit(2);
    }
    // Hold no directory open; every command names its own.
    let _ = std::env::set_current_dir("/");
    let code = match run(Options::new(Path::new(&dir).join(name))) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("codesmith __shelld: {e}");
            1
        }
    };
    std::process::exit(code)
}

/// Run a daemon until it stops: its owner leaves without a handoff, no
/// owner claims it in time, a handoff expires unclaimed, or it is told to
/// shut down. Everything it ran is stopped and its files removed before
/// this returns.
pub fn run(opts: Options) -> io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let r = rt.block_on(serve(opts));
    rt.shutdown_timeout(Duration::from_secs(1));
    r
}

struct Owned {
    conn: u64,
}

struct HandoffState {
    ticket: String,
    expires_at: u64,
    expires: Instant,
}

struct State {
    owner: Option<Owned>,
    ever_owned: bool,
    handoff: Option<HandoffState>,
    procs: BTreeMap<u64, Arc<Proc>>,
    next_id: u64,
    next_conn: u64,
}

enum WaitState {
    Running,
    Exited(Exited),
    Terminated,
}

struct ProcData {
    lifecycle: Lifecycle,
    output_tail: u64,
    output_lines: u64,
    output_error: Option<String>,
    metadata: Value,
    exited_at: Option<u64>,
}

struct Proc {
    id: u64,
    pid: u32,
    command: Vec<String>,
    cwd: String,
    group_id: String,
    output_path: String,
    started_at: u64,
    data: Mutex<ProcData>,
    /// The leader was reaped: no more signals to its pid.
    reaped: AtomicBool,
    done: watch::Sender<WaitState>,
}

impl Proc {
    fn info(&self) -> ProcessInfo {
        let d = self.data.lock().unwrap();
        ProcessInfo {
            id: self.id,
            pid: Some(self.pid),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            group_id: self.group_id.clone(),
            lifecycle: d.lifecycle.clone(),
            output_tail: d.output_tail,
            output_lines: d.output_lines,
            output_path: self.output_path.clone(),
            output_error: d.output_error.clone(),
            metadata: d.metadata.clone(),
            started_at: self.started_at,
            exited_at: d.exited_at,
        }
    }

    fn running(&self) -> bool {
        matches!(self.data.lock().unwrap().lifecycle, Lifecycle::Running)
    }

    /// Signal the process group (and the leader), while it is unreaped.
    fn signal(&self, sig: i32) {
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        // SAFETY: plain signal delivery to this daemon's own child and its
        // group; no memory is involved.
        unsafe {
            libc::killpg(self.pid as i32, sig);
            libc::kill(self.pid as i32, sig);
        }
    }

    /// Signal what is left of the group after the leader was reaped.
    fn signal_group(&self, sig: i32) -> bool {
        // SAFETY: as above.
        unsafe { libc::killpg(self.pid as i32, sig) == 0 }
    }
}

struct Shared {
    reg: Registration,
    opts: Options,
    socket: PathBuf,
    st: Mutex<State>,
    stop: watch::Sender<bool>,
    ptys: Ptys,
    started: Instant,
}

impl Shared {
    fn stop(&self) {
        let _ = self.stop.send(true);
    }

    fn stopping(&self) -> bool {
        *self.stop.borrow()
    }

    fn owner_gone(&self, conn: u64) {
        let mut st = self.st.lock().unwrap();
        if st.owner.as_ref().is_some_and(|o| o.conn == conn) {
            st.owner = None;
            let waiting = st
                .handoff
                .as_ref()
                .is_some_and(|h| h.expires > Instant::now());
            if !waiting {
                drop(st);
                self.stop();
            }
        }
    }

    /// Stop on the ownership deadlines: no first owner in time, or a
    /// handoff that expired with no owner.
    fn check_ownership(&self) {
        let mut st = self.st.lock().unwrap();
        let now = Instant::now();
        if !st.ever_owned && now.duration_since(self.started) >= self.opts.claim_within {
            drop(st);
            self.stop();
            return;
        }
        if st.handoff.as_ref().is_some_and(|h| h.expires <= now) {
            st.handoff = None;
            if st.owner.is_none() {
                drop(st);
                self.stop();
            }
        }
    }
}

async fn serve(opts: Options) -> io::Result<()> {
    let runtime = opts.runtime.clone();
    let parent = runtime
        .parent()
        .ok_or_else(|| io::Error::other("runtime directory has no parent"))?
        .to_path_buf();
    fs::secure_dir(&parent)?;
    fs::secure_dir(&runtime)?;
    let lock = fs::open_lock(&runtime)?;
    if !fs::try_lock(&lock) {
        return Err(io::Error::other(
            "another daemon holds this runtime directory",
        ));
    }
    sweep(&parent, &runtime);
    let socket = fs::socket_path(&runtime)?;
    // Ours to replace: we hold the lock for this runtime directory.
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let reg = Registration {
        instance_id: crate::paths::random_token(16),
        pid: std::process::id(),
        protocol: PROTOCOL,
        socket: socket.display().to_string(),
        token: crate::paths::random_token(32),
        features: vec![],
    };
    fs::write_registration(&runtime, &reg)?;
    let (stop, mut stop_rx) = watch::channel(false);
    let limits = crate::pty::Limits {
        running: opts.limits.terminals,
        ..Default::default()
    };
    let shared = Arc::new(Shared {
        reg,
        socket: socket.clone(),
        st: Mutex::new(State {
            owner: None,
            ever_owned: false,
            handoff: None,
            procs: BTreeMap::new(),
            next_id: 1,
            next_conn: 1,
        }),
        stop,
        ptys: Ptys::with_limits("/", "/bin/sh", limits),
        started: Instant::now(),
        opts,
    });
    let permits = Arc::new(tokio::sync::Semaphore::new(shared.opts.limits.connections));

    let watcher = {
        let shared = shared.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                shared.check_ownership();
                if shared.stopping() {
                    return;
                }
            }
        })
    };
    let signals = {
        let shared = shared.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut term), Ok(mut hup), Ok(mut int)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::hangup()),
                signal(SignalKind::interrupt()),
            ) else {
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = hup.recv() => {}
                _ = int.recv() => {}
            }
            shared.stop();
        })
    };

    loop {
        tokio::select! {
            _ = stopped(&mut stop_rx) => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    connection(shared, stream).await;
                    drop(permit);
                });
            }
        }
    }
    drop(listener);
    watcher.abort();
    signals.abort();
    let cleanup = {
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || stop_everything(&shared))
    };
    let _ = tokio::time::timeout(STOP_LIMIT, cleanup).await;
    // Remove our files, and only ours.
    if fs::read_registration(&runtime).is_ok_and(|r| r.instance_id == shared.reg.instance_id) {
        let _ = std::fs::remove_file(runtime.join(fs::REGISTRATION));
    }
    let _ = std::fs::remove_file(&shared.socket);
    let _ = std::fs::remove_file(runtime.join(fs::LOCK));
    drop(lock);
    let _ = std::fs::remove_dir(&runtime);
    Ok(())
}

/// Stop every process: SIGTERM to each running pipe process's group, then
/// SIGKILL to those left after the grace period; terminals get SIGHUP and
/// then SIGKILL.
fn stop_everything(shared: &Shared) {
    let procs: Vec<Arc<Proc>> = shared.st.lock().unwrap().procs.values().cloned().collect();
    let running: Vec<&Arc<Proc>> = procs.iter().filter(|p| p.running()).collect();
    for p in &running {
        p.signal(libc::SIGTERM);
    }
    let deadline = Instant::now() + shared.opts.kill_grace;
    while Instant::now() < deadline && running.iter().any(|p| !p.reaped.load(Ordering::SeqCst)) {
        std::thread::sleep(Duration::from_millis(20));
    }
    for p in &running {
        p.signal(libc::SIGKILL);
        p.signal_group(libc::SIGKILL);
    }
    shared.ptys.remove_all();
    // Give the waiters a moment to reap what was killed.
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline && running.iter().any(|p| !p.reaped.load(Ordering::SeqCst)) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Remove sibling runtime directories that a daemon left behind: older
/// than [`ABANDONED`], holding nothing but registration files, and either
/// with a lock no one holds or, without one, a registration whose process
/// is gone.
fn sweep(parent: &Path, own: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for e in entries.flatten() {
        let dir = e.path();
        if dir == own {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(&dir) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if !m.is_dir() || m.uid() != fs::euid() {
            continue;
        }
        let old = m
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= ABANDONED);
        if !old {
            continue;
        }
        let names: Option<HashSet<String>> = std::fs::read_dir(&dir).ok().map(|r| {
            r.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        });
        let Some(names) = names else { continue };
        if names.is_empty() || !names.iter().all(|n| n == fs::REGISTRATION || n == fs::LOCK) {
            continue;
        }
        let abandoned = if names.contains(fs::LOCK) {
            fs::open_lock(&dir).is_ok_and(|f| fs::try_lock(&f))
        } else {
            fs::read_registration(&dir).is_ok_and(|r| !fs::alive(r.pid))
        };
        if abandoned {
            // Its socket, if it is one of ours.
            if let (Ok(r), Ok(sockets)) = (fs::read_registration(&dir), fs::socket_dir()) {
                let socket = PathBuf::from(&r.socket);
                if socket.parent() == Some(sockets.as_path()) {
                    let _ = std::fs::remove_file(&socket);
                }
            }
            let _ = std::fs::remove_file(dir.join(fs::REGISTRATION));
            let _ = std::fs::remove_file(dir.join(fs::LOCK));
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

/// Until the daemon stops.
async fn stopped(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|s| *s).await;
}

async fn read_frame(s: &mut (impl AsyncReadExt + Unpin)) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match s.read(&mut len[..1]).await? {
        0 => return Ok(None),
        _ => s.read_exact(&mut len[1..]).await?,
    };
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await?;
    Ok(Some(body))
}

async fn send(s: &mut (impl AsyncWriteExt + Unpin), r: &Response) -> io::Result<()> {
    let frame = protocol::encode(r)
        .or_else(|_| protocol::encode(&Response::error("response too large")))?;
    s.write_all(&frame).await?;
    s.flush().await
}

/// What a request body that does not parse is answered with.
fn unparsed(request: &Value, e: serde_json::Error) -> Response {
    const KNOWN: &[&str] = &[
        "ping",
        "own",
        "prepare_handoff",
        "shutdown",
        "create_pipe",
        "list",
        "get",
        "wait",
        "signal",
        "set_metadata",
        "terminate",
        "create_pty",
        "list_pty",
        "get_pty",
        "update_pty",
        "write_pty",
        "remove_pty",
        "remove_pty_session",
        "read_pty",
        "attach",
        "events",
    ];
    let op = request.get("op").and_then(Value::as_str).unwrap_or("");
    if KNOWN.contains(&op) {
        Response::error(format!("invalid {op} request: {e}"))
    } else {
        Response::error(format!("unsupported request: {op}"))
    }
}

async fn connection(shared: Arc<Shared>, mut stream: UnixStream) {
    // Only this user may talk to the daemon, whatever the socket's mode.
    if stream
        .peer_cred()
        .map(|c| c.uid() != fs::euid())
        .unwrap_or(true)
    {
        return;
    }
    let conn = {
        let mut st = shared.st.lock().unwrap();
        st.next_conn += 1;
        st.next_conn
    };
    let mut owner = false;
    let mut stop = shared.stop.subscribe();
    loop {
        let frame = tokio::select! {
            _ = stopped(&mut stop) => break,
            f = async {
                if owner {
                    read_frame(&mut stream).await
                } else {
                    tokio::time::timeout(FIRST_REQUEST, read_frame(&mut stream))
                        .await
                        .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
                }
            } => f,
        };
        let Ok(Some(body)) = frame else { break };
        let Ok(env) = serde_json::from_slice::<Envelope>(&body) else {
            let _ = send(&mut stream, &Response::error("invalid envelope")).await;
            break;
        };
        if !fs::constant_eq(env.token.as_bytes(), shared.reg.token.as_bytes()) {
            let _ = send(&mut stream, &Response::error(AUTH_FAILED)).await;
            break;
        }
        let request = match serde_json::from_value::<Request>(env.request.clone()) {
            Ok(r) => r,
            Err(e) => {
                let _ = send(&mut stream, &unparsed(&env.request, e)).await;
                if owner {
                    continue;
                }
                break;
            }
        };
        match request {
            Request::Own {
                instance_id,
                ticket,
            } => {
                let r = claim(&shared, conn, &instance_id, ticket.as_deref());
                let ok = r == Response::Owned;
                if send(&mut stream, &r).await.is_err() {
                    break;
                }
                if ok {
                    owner = true;
                    continue;
                }
                if !owner {
                    break;
                }
            }
            Request::Wait { id } => {
                wait(&shared, &mut stream, id).await;
                break;
            }
            Request::Attach { id, cursor } => {
                attach(&shared, stream, id, cursor).await;
                return;
            }
            Request::Events => {
                events(&shared, stream).await;
                return;
            }
            other => {
                let r = handle(&shared, conn, owner, other).await;
                if send(&mut stream, &r).await.is_err() || !owner {
                    break;
                }
            }
        }
    }
    if owner {
        shared.owner_gone(conn);
    }
}

fn claim(shared: &Shared, conn: u64, instance_id: &str, ticket: Option<&str>) -> Response {
    if instance_id != shared.reg.instance_id {
        return Response::error("instance mismatch");
    }
    let mut st = shared.st.lock().unwrap();
    match ticket {
        None if !st.ever_owned && st.owner.is_none() => {
            st.ever_owned = true;
            st.owner = Some(Owned { conn });
            Response::Owned
        }
        None => Response::error("the daemon already has an owner"),
        Some(t) => {
            let good = st.handoff.as_ref().is_some_and(|h| {
                h.expires > Instant::now() && fs::constant_eq(h.ticket.as_bytes(), t.as_bytes())
            });
            if !good {
                return Response::error("invalid or expired handoff ticket");
            }
            st.handoff = None;
            st.ever_owned = true;
            st.owner = Some(Owned { conn });
            Response::Owned
        }
    }
}

async fn handle(shared: &Arc<Shared>, conn: u64, owner: bool, req: Request) -> Response {
    let current = || {
        shared
            .st
            .lock()
            .unwrap()
            .owner
            .as_ref()
            .is_some_and(|o| o.conn == conn)
    };
    match req {
        Request::Ping => Response::Pong {
            instance_id: shared.reg.instance_id.clone(),
            pid: shared.reg.pid,
            protocol: PROTOCOL,
            features: shared.reg.features.clone(),
        },
        Request::PrepareHandoff => {
            if !owner || !current() {
                return Response::error("only the owner can prepare a handoff");
            }
            let mut st = shared.st.lock().unwrap();
            let now = Instant::now();
            if let Some(h) = st.handoff.as_ref().filter(|h| h.expires > now) {
                return Response::Handoff {
                    ticket: h.ticket.clone(),
                    expires_at: h.expires_at,
                };
            }
            let ttl = shared.opts.handoff_ttl;
            let h = HandoffState {
                ticket: crate::paths::random_token(32),
                expires_at: now_ms() + ttl.as_millis() as u64,
                expires: now + ttl,
            };
            let r = Response::Handoff {
                ticket: h.ticket.clone(),
                expires_at: h.expires_at,
            };
            st.handoff = Some(h);
            r
        }
        Request::Shutdown => {
            if owner && !current() {
                return Response::error("this connection no longer owns the daemon");
            }
            shared.stop();
            Response::Ok
        }
        Request::CreatePipe {
            program,
            args,
            cwd,
            env,
            group_id,
            output_path,
            metadata,
        } => {
            let shared = shared.clone();
            let spec = PipeSpec {
                program,
                args,
                cwd,
                env,
                group_id,
                output_path,
                metadata,
            };
            tokio::task::spawn_blocking(move || create_pipe(&shared, spec))
                .await
                .unwrap_or_else(|_| Response::error("create failed"))
        }
        Request::List { mode: Mode::Pipe } => Response::Processes {
            processes: shared
                .st
                .lock()
                .unwrap()
                .procs
                .values()
                .map(|p| p.info())
                .collect(),
        },
        Request::List { mode: Mode::Pty } => Response::Ptys {
            terminals: shared.ptys.list(None),
        },
        Request::Get { id } => match find(shared, id) {
            Some(p) => Response::Process { process: p.info() },
            None => not_found(id),
        },
        Request::Signal { id, signal } => {
            let Some(sig) = protocol::signal_number(&signal) else {
                return Response::error(format!("unsupported signal: {signal}"));
            };
            match find(shared, id) {
                Some(p) => {
                    if p.running() {
                        p.signal(sig);
                    }
                    Response::Ok
                }
                None => not_found(id),
            }
        }
        Request::SetMetadata { id, metadata } => {
            if let Err(e) = check_metadata(&metadata) {
                return Response::error(e);
            }
            match find(shared, id) {
                Some(p) => {
                    p.data.lock().unwrap().metadata = metadata;
                    Response::Ok
                }
                None => not_found(id),
            }
        }
        Request::Terminate { id } => {
            let Some(p) = shared.st.lock().unwrap().procs.remove(&id) else {
                return not_found(id);
            };
            if p.running() {
                p.signal(libc::SIGKILL);
            }
            let _ = p.done.send(WaitState::Terminated);
            Response::Ok
        }
        Request::CreatePty { input } => {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || match shared.ptys.create(pty_input(input)) {
                Ok(info) => Response::Pty { info },
                Err(e) => pty_error(e),
            })
            .await
            .unwrap_or_else(|_| Response::error("create failed"))
        }
        Request::ListPty { session } => Response::Ptys {
            terminals: shared.ptys.list(session.as_deref()),
        },
        Request::GetPty { id } => match shared.ptys.get(&id) {
            Ok(info) => Response::Pty { info },
            Err(e) => pty_error(e),
        },
        Request::UpdatePty { id, title, size } => {
            match shared.ptys.update(&id, UpdateInput { title, size }) {
                Ok(info) => Response::Pty { info },
                Err(e) => pty_error(e),
            }
        }
        Request::WritePty { id, data } => {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || match shared.ptys.write(&id, &data) {
                Ok(()) => Response::Ok,
                Err(e) => pty_error(e),
            })
            .await
            .unwrap_or_else(|_| Response::error("write failed"))
        }
        Request::RemovePty { id } => match shared.ptys.remove(&id) {
            Ok(()) => Response::Ok,
            Err(e) => pty_error(e),
        },
        Request::RemovePtySession { session } => {
            shared.ptys.remove_session(&session);
            Response::Ok
        }
        Request::ReadPty { session, lines } => match shared.ptys.read(&session, lines) {
            Ok(result) => Response::Read { result },
            Err(e) => pty_error(e),
        },
        // Handled by `connection`.
        Request::Own { .. } | Request::Wait { .. } | Request::Attach { .. } | Request::Events => {
            Response::error("unexpected request")
        }
    }
}

fn find(shared: &Shared, id: u64) -> Option<Arc<Proc>> {
    shared.st.lock().unwrap().procs.get(&id).cloned()
}

fn not_found(id: u64) -> Response {
    Response::error(format!("process {id} not found"))
}

fn pty_input(i: PtyCreate) -> CreateInput {
    CreateInput {
        command: Some(i.command),
        args: Some(i.args),
        cwd: Some(i.cwd),
        title: i.title,
        env: Some(i.env),
        size: i.size,
        session_id: i.session_id,
    }
}

fn pty_error(e: PtyError) -> Response {
    let code = match &e {
        PtyError::NotFound(_) => "not_found",
        PtyError::Exited(_) => "exited",
        PtyError::Limit(_) => "limit",
        PtyError::InvalidCwd(_) => "invalid_cwd",
        PtyError::Invalid(_) => "invalid",
        PtyError::Spawn(_) => "spawn",
    };
    Response::Error {
        message: e.to_string(),
        code: Some(code.into()),
    }
}

fn check_metadata(m: &Value) -> Result<(), String> {
    if !m.is_object() {
        return Err("metadata must be an object".into());
    }
    if serde_json::to_vec(m).map(|v| v.len()).unwrap_or(usize::MAX) > MAX_METADATA {
        return Err("metadata is too large".into());
    }
    Ok(())
}

struct PipeSpec {
    program: String,
    args: Vec<String>,
    cwd: String,
    env: BTreeMap<String, String>,
    group_id: String,
    output_path: String,
    metadata: Value,
}

fn create_pipe(shared: &Arc<Shared>, spec: PipeSpec) -> Response {
    if spec.program.is_empty() {
        return Response::error("program is required");
    }
    if !Path::new(&spec.cwd).is_absolute() {
        return Response::error("cwd must be absolute");
    }
    if !Path::new(&spec.output_path).is_absolute() {
        return Response::error("output path must be absolute");
    }
    if let Err(e) = check_metadata(&spec.metadata) {
        return Response::error(e);
    }
    // Hold the state lock across the spawn, so two creates cannot both
    // pass the limits.
    let mut st = shared.st.lock().unwrap();
    if shared.stopping() {
        return Response::error("the daemon is stopping");
    }
    let limits = &shared.opts.limits;
    let running = st.procs.values().filter(|p| p.running()).count();
    if running >= limits.running {
        return Response::error(format!(
            "too many commands running (at most {})",
            limits.running
        ));
    }
    if st.procs.len() >= limits.records {
        return Response::error(format!(
            "too many command records (at most {}); terminate finished ones",
            limits.records
        ));
    }
    let file = match open_output(Path::new(&spec.output_path)) {
        Ok(f) => f,
        Err(e) => return Response::error(format!("cannot open output file: {e}")),
    };
    let (read, write) = match pipe() {
        Ok(p) => p,
        Err(e) => return Response::error(format!("cannot create a pipe: {e}")),
    };
    let child = {
        use std::os::unix::process::CommandExt;
        let err = match write.try_clone() {
            Ok(w) => w,
            Err(e) => return Response::error(e.to_string()),
        };
        let mut cmd = std::process::Command::new(&spec.program);
        cmd.args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(write))
            .stderr(std::process::Stdio::from(err));
        // Its own session (and so its own process group, which `signal`
        // reaches), with no controlling terminal: the daemon has none
        // either, but a command must not depend on how it was started.
        // SAFETY: `own_session` only calls setsid, which is
        // async-signal-safe.
        unsafe {
            cmd.pre_exec(crate::tool::shell::own_session);
        }
        cmd.spawn()
    };
    let child = match child {
        Ok(c) => c,
        Err(e) => return Response::error(format!("cannot start {}: {e}", spec.program)),
    };
    let id = st.next_id;
    st.next_id += 1;
    let mut command = vec![spec.program];
    command.extend(spec.args);
    let (done, _) = watch::channel(WaitState::Running);
    let p = Arc::new(Proc {
        id,
        pid: child.id(),
        command,
        cwd: spec.cwd,
        group_id: spec.group_id,
        output_path: spec.output_path,
        started_at: now_ms(),
        data: Mutex::new(ProcData {
            lifecycle: Lifecycle::Running,
            output_tail: 0,
            output_lines: 0,
            output_error: None,
            metadata: spec.metadata,
            exited_at: None,
        }),
        reaped: AtomicBool::new(false),
        done,
    });
    st.procs.insert(id, p.clone());
    drop(st);
    let info = p.info();
    let grace = shared.opts.kill_grace;
    std::thread::spawn(move || supervise(p, child, read, file, grace));
    Response::Created { process: info }
}

fn open_output(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o666)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is valid for two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: fcntl on descriptors just created.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    // SAFETY: both descriptors are new and owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Read the process's output into its file until the end of output (or
/// [`DRAIN`] after it exits), wait for it, and publish the exit.
fn supervise(
    p: Arc<Proc>,
    mut child: std::process::Child,
    read: OwnedFd,
    file: std::fs::File,
    grace: Duration,
) {
    let exited_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let reader = {
        let (p, exited_at) = (p.clone(), exited_at.clone());
        std::thread::spawn(move || copy_output(&p, read, file, &exited_at))
    };
    let status = child.wait();
    p.reaped.store(true, Ordering::SeqCst);
    *exited_at.lock().unwrap() = Some(Instant::now());
    let (code, signal) = match status {
        Ok(s) => {
            use std::os::unix::process::ExitStatusExt;
            (s.code(), s.signal())
        }
        Err(_) => (None, None),
    };
    // A failed command may leave background processes running: stop its
    // group, as OpenCode's daemon does (a signal death does not).
    if code.is_some_and(|c| c != 0) && p.signal_group(libc::SIGTERM) {
        let p = p.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
                if !p.signal_group(0) {
                    return;
                }
            }
            p.signal_group(libc::SIGKILL);
        });
    }
    let _ = reader.join();
    let ended = now_ms();
    let exit = {
        let mut d = p.data.lock().unwrap();
        d.lifecycle = Lifecycle::Exited {
            exit_code: code,
            signal: signal.map(protocol::signal_name),
        };
        d.exited_at = Some(ended.max(p.started_at));
        Exited {
            exit_code: code,
            signal: signal.map(protocol::signal_name),
            final_offset: d.output_tail,
            output_lines: d.output_lines,
            output_error: d.output_error.clone(),
            exited_at: ended.max(p.started_at),
        }
    };
    p.done.send_if_modified(|s| {
        if matches!(s, WaitState::Running) {
            *s = WaitState::Exited(exit);
            true
        } else {
            false
        }
    });
}

fn copy_output(p: &Proc, read: OwnedFd, mut file: std::fs::File, exited: &Mutex<Option<Instant>>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut src = std::fs::File::from(read);
    let mut failed = false;
    loop {
        if exited.lock().unwrap().is_some_and(|t| t.elapsed() >= DRAIN) {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: src.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let r = unsafe { libc::poll(&mut pfd, 1, 100) };
        if r == 0 {
            continue;
        }
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let mut lines = 0u64;
        if !failed {
            let mut written = 0;
            while written < n {
                match file.write(&buf[written..n]) {
                    Ok(0) => {
                        failed = true;
                        p.data.lock().unwrap().output_error = Some("write returned zero".into());
                        break;
                    }
                    Ok(w) => written += w,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        failed = true;
                        p.data.lock().unwrap().output_error = Some(e.to_string());
                        break;
                    }
                }
            }
            lines = buf[..written].iter().filter(|&&b| b == b'\n').count() as u64;
        }
        let mut d = p.data.lock().unwrap();
        d.output_tail += n as u64;
        d.output_lines += lines;
    }
    let _ = file.flush();
}

async fn wait(shared: &Shared, stream: &mut UnixStream, id: u64) {
    let Some(p) = find(shared, id) else {
        let _ = send(stream, &not_found(id)).await;
        return;
    };
    let mut rx = p.done.subscribe();
    drop(p);
    let mut stop = shared.stop.subscribe();
    let (r, mut w) = stream.split();
    let mut probe = [0u8; 1];
    let mut r = r;
    let reply = tokio::select! {
        s = async {
            match rx.wait_for(|s| !matches!(s, WaitState::Running)).await.as_deref() {
                Ok(WaitState::Exited(e)) => Some(Response::Exited(e.clone())),
                Ok(WaitState::Terminated) => Some(Response::error("terminated")),
                _ => None,
            }
        } => s,
        // The client left, or the daemon is stopping: close without a reply.
        _ = r.read(&mut probe) => None,
        _ = stopped(&mut stop) => None,
    };
    if let Some(reply) = reply {
        let _ = send(&mut w, &reply).await;
    }
}

async fn attach(shared: &Shared, stream: UnixStream, id: String, cursor: Option<i64>) {
    let (mut r, mut w) = stream.into_split();
    let mut att = match shared.ptys.attach(&id, cursor) {
        Ok(a) => a,
        Err(e) => {
            let _ = send(&mut w, &pty_error(e)).await;
            return;
        }
    };
    let first = Response::Attached {
        replay: std::mem::take(&mut att.replay),
        cursor: att.cursor,
    };
    if send(&mut w, &first).await.is_err() {
        return;
    }
    let mut stop = shared.stop.subscribe();
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            out = att.recv() => match out {
                Some(Output::Data(data)) => {
                    if send(&mut w, &Response::Output { data }).await.is_err() {
                        return;
                    }
                }
                Some(Output::End { exit_code }) => {
                    let _ = send(&mut w, &Response::End { exit_code }).await;
                    return;
                }
                // Fell too far behind.
                None => return,
            },
            _ = r.read(&mut probe) => return,
            _ = stopped(&mut stop) => {
                let _ = send(&mut w, &Response::End { exit_code: None }).await;
                return;
            }
        }
    }
}

async fn events(shared: &Shared, stream: UnixStream) {
    let (mut r, mut w) = stream.into_split();
    let mut rx = shared.ptys.events();
    if send(&mut w, &Response::Ok).await.is_err() {
        return;
    }
    let mut stop = shared.stop.subscribe();
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            e = rx.recv() => match e {
                Ok(event) => {
                    if send(&mut w, &Response::Event { event }).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            },
            _ = r.read(&mut probe) => return,
            _ = stopped(&mut stop) => return,
        }
    }
}
