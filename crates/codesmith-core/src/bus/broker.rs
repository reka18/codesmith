//! The broker (`codesmith __busd --dir DIR`): one per user, started by the
//! first member that finds none, gone [`super::idle`] after the last
//! leaves. It holds `bus.lock` for its whole life, so only one runs, and
//! writes every message to its [`Mailbox`] before it acknowledges it.

use super::mailbox::{Mailbox, State as MsgState};
use super::protocol::{
    self, new_id, now_ms, valid_name, Envelope, Frame, Hello, Kind, Member, Message, Peer,
    Registration, Relayed, Reply, Request, Role, MAX_BODY, MAX_FRAME, MAX_HOPS, PROTOCOL,
};
use crate::daemon::fs;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

/// What bounds a broker.
#[derive(Clone, Debug)]
pub struct Limits {
    pub connections: usize,
    /// Messages not yet handled, across every session.
    pub unhandled: i64,
    /// Requests one session may send in a minute.
    pub per_minute: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            connections: 128,
            unhandled: 10_000,
            per_minute: 30,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub dir: PathBuf,
    pub mailbox: PathBuf,
    pub idle: Duration,
    pub limits: Limits,
    pub version: String,
}

impl Options {
    pub fn new(dir: PathBuf) -> Options {
        Options {
            mailbox: crate::paths::state_dir().join("bus").join("mailbox.db"),
            dir,
            idle: super::idle(),
            limits: Limits::default(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// How long a connection may take to send its first line.
const FIRST_LINE: Duration = Duration::from_secs(5);

pub fn main(args: &[String]) -> ! {
    let mut dir = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--dir" {
            dir = it.next().cloned();
        }
    }
    let Some(dir) = dir.filter(|d| Path::new(d).is_absolute()) else {
        eprintln!("usage: codesmith __busd --dir DIR");
        std::process::exit(2);
    };
    let _ = std::env::set_current_dir("/");
    let code = match run(Options::new(PathBuf::from(dir))) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("codesmith __busd: {e}");
            1
        }
    };
    std::process::exit(code)
}

/// Run a broker until it is idle for `opts.idle` or told to shut down.
/// Returns at once if another broker holds the lock.
pub fn run(opts: Options) -> io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let r = rt.block_on(serve(opts));
    rt.shutdown_timeout(Duration::from_secs(1));
    r
}

struct Conn {
    member: Member,
    name: String,
    joined: i64,
    paused: bool,
    tx: mpsc::UnboundedSender<String>,
}

struct State {
    mailbox: Mailbox,
    conns: HashMap<u64, Conn>,
    next: u64,
    /// When the broker last had no members (for the idle exit).
    empty_since: Option<Instant>,
    opts: Options,
}

type Shared = Arc<Mutex<State>>;

/// Read one line of at most [`MAX_FRAME`] bytes; `None` at the end.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    String::from_utf8(buf)
        .map(|s| Some(s.trim_end_matches(['\n', '\r']).to_string()))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

fn line(f: &Frame) -> String {
    let mut s = serde_json::to_string(f).unwrap_or_default();
    s.push('\n');
    s
}

/// Read a registration, refusing one that is a symlink, isn't this
/// user's, or that others could have written.
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
            format!("untrusted bus registration: {}", path.display()),
        ));
    }
    let mut body = String::new();
    f.by_ref().take(64 * 1024).read_to_string(&mut body)?;
    serde_json::from_str(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub async fn serve(opts: Options) -> io::Result<()> {
    fs::secure_dir(&opts.dir)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags_private()
        .open(opts.dir.join(super::LOCK))?;
    if !fs::try_lock(&lock) {
        // Another broker runs; it serves.
        return Ok(());
    }
    let mailbox = Mailbox::open(&opts.mailbox).map_err(|e| io::Error::other(format!("{e:#}")))?;
    let _ = mailbox.prune(now_ms());
    let socket = opts.dir.join(super::SOCKET);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let token = crate::paths::random_token(32);
    let reg = Registration {
        pid: std::process::id(),
        protocol: PROTOCOL,
        version: opts.version.clone(),
        socket: socket.display().to_string(),
        token: token.clone(),
        started: now_ms(),
    };
    crate::paths::write_private(
        &opts.dir.join(super::REGISTRATION),
        &serde_json::to_vec(&reg).map_err(io::Error::other)?,
    )?;
    let (stop_tx, mut stop) = watch::channel(false);
    let state: Shared = Arc::new(Mutex::new(State {
        mailbox,
        conns: HashMap::new(),
        next: 1,
        empty_since: Some(Instant::now()),
        opts: opts.clone(),
    }));
    let idle = opts.idle;
    let mut tick =
        tokio::time::interval(Duration::from_millis(250).min(idle.max(Duration::from_millis(10))));
    let mut last_prune = Instant::now();
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tick.tick() => {
                let idle_now = {
                    let st = state.lock().unwrap();
                    if last_prune.elapsed() >= Duration::from_secs(3600) {
                        last_prune = Instant::now();
                        let _ = st.mailbox.prune(now_ms());
                    }
                    st.conns.is_empty() && st.empty_since.is_some_and(|t| t.elapsed() >= idle)
                };
                if idle_now {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let (state, token, stop_tx) = (state.clone(), token.clone(), stop_tx.clone());
                tokio::spawn(async move {
                    let _ = connection(stream, state, token, stop_tx).await;
                });
            }
        }
    }
    // Leave: our files only, so a broker that replaced us keeps its own.
    if read_registration(&opts.dir).is_ok_and(|r| r.token == token) {
        let _ = std::fs::remove_file(opts.dir.join(super::REGISTRATION));
        let _ = std::fs::remove_file(&socket);
    }
    {
        let st = state.lock().unwrap();
        for c in st.conns.values() {
            let _ = c.tx.send(line(&Frame::Bye {
                reason: "the bus is stopping".into(),
            }));
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(lock);
    Ok(())
}

trait PrivateOpen {
    fn custom_flags_private(&mut self) -> &mut Self;
}

impl PrivateOpen for std::fs::OpenOptions {
    fn custom_flags_private(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
    }
}

async fn connection(
    stream: UnixStream,
    state: Shared,
    token: String,
    stop: watch::Sender<bool>,
) -> io::Result<()> {
    if fs::peer_uid(stream.as_raw_fd())? != fs::euid() {
        return Ok(());
    }
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let first = match tokio::time::timeout(FIRST_LINE, read_line(&mut r)).await {
        Ok(Ok(Some(l))) => l,
        _ => return Ok(()),
    };
    let bye = |reason: &str| {
        line(&Frame::Bye {
            reason: reason.into(),
        })
    };
    let v: Value = match serde_json::from_str(&first) {
        Ok(v) => v,
        Err(_) => {
            w.write_all(bye("bad request").as_bytes()).await?;
            return Ok(());
        }
    };
    let theirs = v.get("token").and_then(Value::as_str).unwrap_or("");
    if !fs::constant_eq(theirs.as_bytes(), token.as_bytes()) {
        w.write_all(bye("authentication failed").as_bytes()).await?;
        return Ok(());
    }
    if v.get("op").and_then(Value::as_str) == Some("shutdown") {
        let version = v.get("version").and_then(Value::as_str).unwrap_or("");
        let ours = state.lock().unwrap().opts.version.clone();
        if protocol::newer(version, &ours) {
            w.write_all(bye("stopping for a newer CodeSmith").as_bytes())
                .await?;
            let _ = stop.send(true);
        } else {
            w.write_all(bye("only a newer CodeSmith may stop the bus").as_bytes())
                .await?;
        }
        return Ok(());
    }
    let hello: Hello = match serde_json::from_value(v) {
        Ok(h) => h,
        Err(_) => {
            w.write_all(bye("bad hello").as_bytes()).await?;
            return Ok(());
        }
    };
    if hello.protocol != PROTOCOL {
        w.write_all(bye(&format!("protocol {} is not {PROTOCOL}", hello.protocol)).as_bytes())
            .await?;
        return Ok(());
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let id = match join(&state, hello.member, tx.clone()) {
        Ok(id) => id,
        Err(why) => {
            w.write_all(bye(&why).as_bytes()).await?;
            return Ok(());
        }
    };
    let writer = tokio::spawn(async move {
        while let Some(l) = rx.recv().await {
            if w.write_all(l.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let result = async {
        while let Some(l) = read_line(&mut r).await? {
            let Ok(env) = serde_json::from_str::<Envelope>(&l) else {
                let _ = tx.send(line(&Frame::Bye {
                    reason: "bad request".into(),
                }));
                break;
            };
            if let Request::Shutdown { version, .. } = &env.request {
                let ours = state.lock().unwrap().opts.version.clone();
                if protocol::newer(version, &ours) {
                    let _ = stop.send(true);
                }
            }
            let reply = handle(&state, id, env.rid, env.request);
            if tx.send(line(&Frame::Reply(reply))).is_err() {
                break;
            }
        }
        io::Result::Ok(())
    }
    .await;
    leave(&state, id);
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    result
}

/// Take a member in: its name, its welcome and what was waiting for it.
fn join(state: &Shared, member: Member, tx: mpsc::UnboundedSender<String>) -> Result<u64, String> {
    let mut st = state.lock().unwrap();
    if st.conns.len() >= st.opts.limits.connections {
        return Err("the bus has too many connections".into());
    }
    if member.session.is_empty() || member.session.len() > 128 {
        return Err("a member needs a session id".into());
    }
    if member.role == Role::Session {
        if let Some((&old, c)) = st
            .conns
            .iter()
            .find(|(_, c)| c.member.role == Role::Session && c.member.session == member.session)
        {
            if c.member.pid != member.pid && fs::alive(c.member.pid) {
                return Err(format!(
                    "session {} is already on the bus (process {})",
                    member.session, c.member.pid
                ));
            }
            st.conns.remove(&old);
        }
    }
    let name = match member.role {
        Role::Session => assign_name(
            &st,
            &member.session,
            member.name.as_deref(),
            member.title.as_deref(),
            true,
        )?,
        Role::Relay => "relay".into(),
        Role::Observer => "observer".into(),
    };
    if member.role == Role::Session {
        let now = now_ms();
        st.mailbox
            .set_name(&member.session, &name, &member.project, now)
            .map_err(|e| format!("{e:#}"))?;
    }
    let id = st.next;
    st.next += 1;
    let _ = tx.send(line(&Frame::Welcome {
        name: name.clone(),
        version: st.opts.version.clone(),
        protocol: PROTOCOL,
    }));
    let session = member.session.clone();
    let deliver_requests = member.role == Role::Session && member.accepts;
    st.conns.insert(
        id,
        Conn {
            member,
            name,
            joined: now_ms(),
            paused: false,
            tx,
        },
    );
    st.empty_since = None;
    flush(&st, id, &session, deliver_requests);
    Ok(id)
}

fn leave(state: &Shared, id: u64) {
    let mut st = state.lock().unwrap();
    if let Some(c) = st.conns.remove(&id) {
        if c.member.role == Role::Session {
            let _ = st.mailbox.touch_name(&c.member.session, now_ms());
        }
    }
    if st.conns.is_empty() {
        st.empty_since = Some(Instant::now());
    }
}

/// Send `id` (a member of `session`) every message waiting for it.
fn flush(st: &State, id: u64, session: &str, requests: bool) {
    let Some(c) = st.conns.get(&id) else { return };
    let Ok(pending) = st.mailbox.pending(session, requests) else {
        return;
    };
    for m in pending {
        if c.tx
            .send(line(&Frame::Deliver { message: m.clone() }))
            .is_ok()
        {
            let _ = st.mailbox.set_state(&m.id, MsgState::Delivered);
        }
    }
}

/// The live connection of session `session`.
fn live(st: &State, session: &str) -> Option<u64> {
    st.conns
        .iter()
        .find(|(_, c)| c.member.session == session && c.member.role != Role::Observer)
        .map(|(id, _)| *id)
}

/// The name a session gets: the one asked for when it's free (or held by a
/// session that isn't running), else the one it had, else one made from
/// its title with a number added until it's free.
fn assign_name(
    st: &State,
    session: &str,
    wanted: Option<&str>,
    title: Option<&str>,
    fallback: bool,
) -> Result<String, String> {
    let free = |n: &str, take_offline: bool| -> bool {
        match st.mailbox.name_owner(n).ok().flatten() {
            None => true,
            Some(s) if s == session => true,
            Some(s) => take_offline && live(st, &s).is_none(),
        }
    };
    if let Some(w) = wanted {
        if !valid_name(w) {
            return Err(format!(
                "{w:?} isn't a session name: use lowercase letters, digits and single dashes, starting with a letter"
            ));
        }
        if free(w, true) {
            return Ok(w.to_string());
        }
        if !fallback {
            return Err(format!("another running session is called {w}"));
        }
    }
    if let Some(n) = st.mailbox.name_of(session).ok().flatten() {
        return Ok(n);
    }
    Ok(derived_name(st, session, title))
}

/// A free name from `title` (or `session`), numbered until it's free.
fn derived_name(st: &State, session: &str, title: Option<&str>) -> String {
    let free = |n: &str| match st.mailbox.name_owner(n).ok().flatten() {
        None => true,
        Some(s) => s == session,
    };
    let base = protocol::slug(title.unwrap_or("session"));
    for i in 1..1000 {
        let n = if i == 1 {
            base.clone()
        } else {
            let suffix = format!("-{i}");
            let keep = base.len().min(protocol::MAX_NAME - suffix.len());
            format!("{}{suffix}", base[..keep].trim_end_matches('-'))
        };
        if free(&n) {
            return n;
        }
    }
    format!("session-{}", new_id()[16..].to_ascii_lowercase())
}

fn ok(rid: u64, data: Value) -> Reply {
    Reply {
        rid,
        ok: true,
        data,
        error: None,
    }
}

fn err(rid: u64, e: impl Into<String>) -> Reply {
    Reply {
        rid,
        ok: false,
        data: Value::Null,
        error: Some(e.into()),
    }
}

fn handle(state: &Shared, id: u64, rid: u64, req: Request) -> Reply {
    let mut st = state.lock().unwrap();
    let Some(me) = st
        .conns
        .get(&id)
        .map(|c| (c.member.clone(), c.name.clone()))
    else {
        return err(rid, "not on the bus");
    };
    let (member, my_name) = me;
    let r = match req {
        Request::Peers => Ok(json!(peers(&st))),
        Request::Inbox { session } => {
            let s = match (member.role, session) {
                (Role::Observer, Some(s)) => s,
                (_, _) => member.session.clone(),
            };
            st.mailbox
                .pending(&s, true)
                .map(|p| json!(p))
                .map_err(|e| format!("{e:#}"))
        }
        Request::Ack { id: mid } => match st.mailbox.get(&mid) {
            Ok(Some(m)) if m.to == member.session => st
                .mailbox
                .set_state(&mid, MsgState::Acked)
                .map(|_| Value::Null)
                .map_err(|e| format!("{e:#}")),
            Ok(_) => Err(format!("no message {mid} to this session")),
            Err(e) => Err(format!("{e:#}")),
        },
        Request::Pause { on } => {
            if let Some(c) = st.conns.get_mut(&id) {
                c.paused = on;
            }
            if !on && member.accepts {
                flush(&st, id, &member.session, true);
            }
            Ok(Value::Null)
        }
        Request::Update {
            session,
            name,
            title,
            project,
            agent,
            model,
            busy,
        } => update(
            &mut st, id, &member, session, name, title, project, agent, model, busy,
        ),
        Request::Share { name } => {
            if member.role != Role::Session {
                Err("only a session can be shared".into())
            } else if !valid_name(&name) {
                Err(format!("{name:?} isn't a session name: use lowercase letters, digits and single dashes, starting with a letter"))
            } else {
                match st
                    .mailbox
                    .share(&member.session, &name, &member.project, now_ms())
                {
                    Ok(true) => Ok(json!({"shared": name})),
                    Ok(false) => Err(format!("another session is shared as {name}")),
                    Err(e) => Err(format!("{e:#}")),
                }
            }
        }
        Request::Unshare => st
            .mailbox
            .unshare(&member.session)
            .map(|was| json!({"unshared": was}))
            .map_err(|e| format!("{e:#}")),
        Request::Send {
            to,
            kind,
            body,
            reply_to,
            hop,
        } => send(&st, &member, &my_name, &to, kind, body, reply_to, hop),
        Request::Relay {
            share,
            body,
            via,
            key,
        } => relay(&st, &member, &share, body, &via, key.as_deref()),
        Request::Shutdown { .. } => Ok(Value::Null),
    };
    match r {
        Ok(v) => ok(rid, v),
        Err(e) => err(rid, e),
    }
}

fn peers(st: &State) -> Vec<Peer> {
    let mut out: Vec<Peer> = st
        .conns
        .values()
        .filter(|c| c.member.role == Role::Session)
        .map(|c| Peer {
            name: c.name.clone(),
            session: c.member.session.clone(),
            project: c.member.project.clone(),
            agent: c.member.agent.clone(),
            model: c.member.model.clone(),
            busy: c.member.busy,
            accepts: c.member.accepts,
            paused: c.paused,
            joined: c.joined,
            shared: st.mailbox.share_of(&c.member.session).ok().flatten(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[allow(clippy::too_many_arguments)]
fn update(
    st: &mut State,
    id: u64,
    member: &Member,
    session: Option<String>,
    name: Option<String>,
    title: Option<String>,
    project: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    busy: Option<bool>,
) -> Result<Value, String> {
    if member.role != Role::Session {
        return Err("only a session can change".into());
    }
    let new_session = session.clone().unwrap_or_else(|| member.session.clone());
    if new_session.is_empty() || new_session.len() > 128 {
        return Err("a member needs a session id".into());
    }
    if new_session != member.session {
        if let Some(other) = live(st, &new_session).filter(|o| *o != id) {
            let pid = st.conns.get(&other).map(|c| c.member.pid).unwrap_or(0);
            if fs::alive(pid) {
                return Err(format!("session {new_session} is already on the bus"));
            }
            st.conns.remove(&other);
        }
        let _ = st.mailbox.touch_name(&member.session, now_ms());
    }
    // The session's name: the one asked for, else its own, else from its
    // (new) title. A session first named before it had a title (`session`,
    // `session-2`) takes one from the title it gets.
    let titled = title.is_some() && title != member.title;
    let title = title.or_else(|| member.title.clone());
    let changed_session = new_session != member.session;
    let current = st
        .conns
        .get(&id)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let untitled_name =
        member.name.is_none() && (current == "session" || current.starts_with("session-"));
    let explicit = name.is_some();
    let new_name = if name.is_some() || changed_session {
        let fallback = name.is_none();
        Some(assign_name(
            st,
            &new_session,
            name.as_deref(),
            title.as_deref(),
            fallback,
        )?)
    } else if titled && untitled_name {
        Some(derived_name(st, &new_session, title.as_deref()))
    } else {
        None
    };
    let project = project.unwrap_or_else(|| member.project.clone());
    if let Some(n) = &new_name {
        st.mailbox
            .set_name(&new_session, n, &project, now_ms())
            .map_err(|e| format!("{e:#}"))?;
    }
    let accepts;
    let current_name;
    {
        let c = st.conns.get_mut(&id).ok_or("not on the bus")?;
        c.member.session = new_session.clone();
        c.member.title = title;
        c.member.project = project;
        if let Some(a) = agent {
            c.member.agent = a;
        }
        if let Some(m) = model {
            c.member.model = m;
        }
        if let Some(b) = busy {
            c.member.busy = b;
        }
        if let Some(n) = new_name {
            c.name = n;
        }
        if explicit {
            c.member.name = Some(c.name.clone());
        } else if changed_session {
            c.member.name = None;
        }
        accepts = c.member.accepts && !c.paused;
        current_name = c.name.clone();
    }
    if changed_session {
        flush(st, id, &new_session, accepts);
    }
    Ok(json!({"name": current_name}))
}

#[allow(clippy::too_many_arguments)]
fn send(
    st: &State,
    member: &Member,
    my_name: &str,
    to: &str,
    kind: Kind,
    body: String,
    reply_to: Option<String>,
    hop: u32,
) -> Result<Value, String> {
    if member.role == Role::Observer {
        return Err("an observer can't send".into());
    }
    if member.role == Role::Relay && kind == Kind::Request {
        return Err("a relay carries one request with relay".into());
    }
    if body.len() > MAX_BODY {
        return Err(format!(
            "the message is {} bytes; the most is {MAX_BODY}",
            body.len()
        ));
    }
    let now = now_ms();
    let unhandled = st.mailbox.unhandled().map_err(|e| format!("{e:#}"))?;
    if unhandled >= st.opts.limits.unhandled {
        return Err("the bus is full: too many messages are waiting to be handled".into());
    }
    match kind {
        Kind::Request => {
            if hop >= MAX_HOPS {
                return Err(format!(
                    "refused: this request would be the {}th in a chain of requests between sessions (the most is {MAX_HOPS}), which looks like a loop",
                    hop + 1
                ));
            }
            let sent = st
                .mailbox
                .requests_since(&member.session, now - 60_000)
                .map_err(|e| format!("{e:#}"))?;
            if sent >= st.opts.limits.per_minute {
                return Err(format!(
                    "refused: this session sent {sent} requests in the last minute (the most is {})",
                    st.opts.limits.per_minute
                ));
            }
            let targets: Vec<(String, String)> = if to == "*" {
                let t: Vec<(String, String)> = st
                    .conns
                    .values()
                    .filter(|c| {
                        c.member.role == Role::Session
                            && c.member.accepts
                            && c.member.session != member.session
                            && c.member.project == member.project
                    })
                    .map(|c| (c.member.session.clone(), c.name.clone()))
                    .collect();
                if t.is_empty() {
                    return Err("no other session in this project is on the bus".into());
                }
                t
            } else {
                let (session, name) = resolve(st, to)?;
                if session == member.session {
                    return Err("a session can't send a request to itself".into());
                }
                vec![(session, name)]
            };
            let mut out = Vec::new();
            for (session, name) in targets {
                let m = Message {
                    id: new_id(),
                    kind,
                    from: member.session.clone(),
                    from_name: my_name.to_string(),
                    to: session.clone(),
                    to_name: name.clone(),
                    reply_to: None,
                    body: body.clone(),
                    sent: now,
                    hop,
                    project: member.project.clone(),
                    via: None,
                };
                let delivered = store_and_deliver(st, &m, None)?;
                out.push(json!({"id": m.id, "to": name, "delivered": delivered}));
            }
            Ok(json!(out))
        }
        Kind::Progress | Kind::Response => {
            let Some(rid) = reply_to else {
                return Err(format!(
                    "a {} needs reply_to: the request it answers",
                    kind.as_str()
                ));
            };
            let req = st
                .mailbox
                .get(&rid)
                .map_err(|e| format!("{e:#}"))?
                .filter(|r| r.kind == Kind::Request && r.to == member.session)
                .ok_or_else(|| format!("no request {rid} was sent to this session"))?;
            if kind == Kind::Response {
                if !st
                    .mailbox
                    .mark_answered(&rid)
                    .map_err(|e| format!("{e:#}"))?
                {
                    return Err(format!("request {rid} is already answered"));
                }
            } else if st.mailbox.answered(&rid).unwrap_or(false) {
                return Err(format!("request {rid} is already answered"));
            }
            let m = Message {
                id: new_id(),
                kind,
                from: member.session.clone(),
                from_name: my_name.to_string(),
                to: req.from.clone(),
                to_name: req.from_name.clone(),
                reply_to: Some(rid),
                body,
                sent: now,
                hop: req.hop,
                project: member.project.clone(),
                via: None,
            };
            let delivered = store_and_deliver(st, &m, None)?;
            Ok(json!([{"id": m.id, "to": m.to_name, "delivered": delivered}]))
        }
    }
}

/// The session `to` names: a name, else a session id that has one.
fn resolve(st: &State, to: &str) -> Result<(String, String), String> {
    if let Some(s) = st.mailbox.name_owner(to).ok().flatten() {
        return Ok((s, to.to_string()));
    }
    if let Some(n) = st.mailbox.name_of(to).ok().flatten() {
        return Ok((to.to_string(), n));
    }
    Err(format!("no session is called {to}"))
}

/// Keep `m`, and send it at once if its recipient is running and takes it.
fn store_and_deliver(st: &State, m: &Message, key: Option<&str>) -> Result<bool, String> {
    st.mailbox
        .insert(m, MsgState::Queued, key)
        .map_err(|e| format!("{e:#}"))?;
    let target = st.conns.values().find(|c| {
        c.member.session == m.to
            && c.member.role != Role::Observer
            && (m.kind != Kind::Request || (c.member.accepts && !c.paused))
    });
    if let Some(c) = target {
        if c.tx
            .send(line(&Frame::Deliver { message: m.clone() }))
            .is_ok()
        {
            let _ = st.mailbox.set_state(&m.id, MsgState::Delivered);
            return Ok(true);
        }
    }
    Ok(false)
}

/// A relay's request for the session shared as `share`.
fn relay(
    st: &State,
    member: &Member,
    share: &str,
    body: String,
    via: &str,
    key: Option<&str>,
) -> Result<Value, String> {
    if member.role != Role::Relay {
        return Err("only a relay can relay".into());
    }
    if body.len() > MAX_BODY {
        return Err(format!(
            "the message is {} bytes; the most is {MAX_BODY}",
            body.len()
        ));
    }
    let (session, project) = st
        .mailbox
        .shared(share)
        .map_err(|e| format!("{e:#}"))?
        .ok_or_else(|| format!("no session is shared as {share}"))?;
    let name = st
        .mailbox
        .name_of(&session)
        .ok()
        .flatten()
        .unwrap_or_else(|| share.to_string());
    let running = st
        .conns
        .values()
        .any(|c| c.member.session == session && c.member.role == Role::Session && c.member.accepts);
    // A repeat of a message already carried (tailbus delivers at least
    // once): its answer if it has one, else its answer when it comes.
    if let Some(k) = key {
        if let Some(first) = st.mailbox.by_key(k).map_err(|e| format!("{e:#}"))? {
            st.mailbox
                .set_from(&first.id, &member.session)
                .map_err(|e| format!("{e:#}"))?;
            if let Some(answer) = st
                .mailbox
                .response_to(&first.id)
                .map_err(|e| format!("{e:#}"))?
            {
                st.mailbox
                    .set_to(&answer.id, &member.session)
                    .map_err(|e| format!("{e:#}"))?;
                if let Some((&cid, _)) = st
                    .conns
                    .iter()
                    .find(|(_, c)| c.member.session == member.session)
                {
                    flush(st, cid, &member.session, false);
                }
            }
            return Ok(json!(Relayed {
                id: Some(first.id),
                live: true,
                session,
                project,
                name,
            }));
        }
    }
    if !running {
        return Ok(json!(Relayed {
            id: None,
            live: false,
            session,
            project,
            name,
        }));
    }
    let unhandled = st.mailbox.unhandled().map_err(|e| format!("{e:#}"))?;
    if unhandled >= st.opts.limits.unhandled {
        return Err("the bus is full: too many messages are waiting to be handled".into());
    }
    let label: String = via.chars().filter(|c| !c.is_control()).take(120).collect();
    let m = Message {
        id: new_id(),
        kind: Kind::Request,
        from: member.session.clone(),
        from_name: if label.is_empty() {
            "relay".into()
        } else {
            label.clone()
        },
        to: session.clone(),
        to_name: name.clone(),
        reply_to: None,
        body,
        sent: now_ms(),
        hop: 0,
        project: String::new(),
        via: Some("tailbus".into()),
    };
    store_and_deliver(st, &m, key)?;
    Ok(json!(Relayed {
        id: Some(m.id),
        live: true,
        session,
        project,
        name,
    }))
}
