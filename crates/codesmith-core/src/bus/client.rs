//! A member's side of the bus: [`Bus::join`] connects in the background
//! (starting the broker when none answers, and asking an older one to make
//! way), joins again whenever the connection drops, and hands each message
//! to whoever waits for it ([`Bus::wait`]) or else to [`Bus::incoming`].

use super::broker::read_registration;
use super::protocol::{
    newer, now_ms, Envelope, Frame, Hello, Kind, Member, Message, Peer, Relayed, Request,
    MAX_FRAME, PROTOCOL,
};
use crate::daemon::fs;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};

/// How long a call waits for its reply.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long starting a broker may take.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// Message ids remembered to drop repeats.
const SEEN: usize = 2048;

/// Where a member is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Joined {
    Connecting,
    /// On the bus, under this name.
    On(String),
    /// Off it for now, and why (it keeps trying).
    Off(String),
}

/// How to join.
#[derive(Clone, Debug)]
pub struct Options {
    /// Start a broker when none answers (an observer doesn't).
    pub start: bool,
    /// Keep trying after a connection fails or drops.
    pub reconnect: bool,
    /// `HOME` for a broker this member starts (a relay's is the user's own,
    /// not its bridge's).
    pub home: Option<PathBuf>,
    /// This program's version, which decides who makes way.
    pub version: String,
    /// The bus directory, instead of [`super::dir`] (tests).
    pub dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            start: true,
            reconnect: true,
            home: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            dir: None,
        }
    }
}

struct Waiter {
    reply_to: Option<String>,
    tx: oneshot::Sender<Message>,
}

struct Inner {
    member: Mutex<Member>,
    opts: Options,
    calls: mpsc::UnboundedSender<(Request, oneshot::Sender<Result<Value, String>>)>,
    status: watch::Sender<Joined>,
    waiters: Mutex<Vec<Waiter>>,
    incoming_tx: mpsc::UnboundedSender<Message>,
    incoming_rx: Mutex<Option<mpsc::UnboundedReceiver<Message>>>,
    seen: Mutex<VecDeque<String>>,
}

/// A member's handle; clones share one connection.
#[derive(Clone)]
pub struct Bus {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bus({:?})", self.status())
    }
}

impl Bus {
    /// Join as `member`, in the background. Needs a Tokio runtime.
    pub fn join(member: Member, opts: Options) -> Bus {
        let (calls, rx) = mpsc::unbounded_channel();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (status, _) = watch::channel(Joined::Connecting);
        let inner = Arc::new(Inner {
            member: Mutex::new(member),
            opts,
            calls,
            status,
            waiters: Mutex::new(Vec::new()),
            incoming_tx,
            incoming_rx: Mutex::new(Some(incoming_rx)),
            seen: Mutex::new(VecDeque::new()),
        });
        tokio::spawn(run(Arc::downgrade(&inner), rx));
        Bus { inner }
    }

    pub fn status(&self) -> Joined {
        self.inner.status.borrow().clone()
    }

    /// This member's name, once on the bus.
    pub fn name(&self) -> Option<String> {
        match self.status() {
            Joined::On(n) => Some(n),
            _ => None,
        }
    }

    pub fn session(&self) -> String {
        self.inner.member.lock().unwrap().session.clone()
    }

    /// Wait until on the bus (or `timeout`).
    pub async fn ready(&self, timeout: Duration) -> Joined {
        let mut rx = self.inner.status.subscribe();
        let _ = tokio::time::timeout(timeout, async {
            loop {
                if matches!(*rx.borrow_and_update(), Joined::On(_)) {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        self.status()
    }

    /// Status changes, for a UI.
    pub fn watch(&self) -> watch::Receiver<Joined> {
        self.inner.status.subscribe()
    }

    /// Messages nobody waited for (requests, and replies outside a wait),
    /// once: the first caller gets the receiver.
    pub fn incoming(&self) -> Option<mpsc::UnboundedReceiver<Message>> {
        self.inner.incoming_rx.lock().unwrap().take()
    }

    async fn call(&self, req: Request) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .calls
            .send((req, tx))
            .map_err(|_| "the bus connection has stopped".to_string())?;
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the bus connection was lost".into()),
            Err(_) => Err("the bus didn't answer in time".into()),
        }
    }

    pub async fn peers(&self) -> Result<Vec<Peer>, String> {
        let v = self.call(Request::Peers).await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// Send; returns each message's `{id, to, delivered}`.
    pub async fn send(
        &self,
        to: &str,
        kind: Kind,
        body: &str,
        reply_to: Option<&str>,
        hop: u32,
    ) -> Result<Vec<Value>, String> {
        let v = self
            .call(Request::Send {
                to: to.into(),
                kind,
                body: body.into(),
                reply_to: reply_to.map(str::to_string),
                hop,
            })
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub async fn inbox(&self, session: Option<&str>) -> Result<Vec<Message>, String> {
        let v = self
            .call(Request::Inbox {
                session: session.map(str::to_string),
            })
            .await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// Mark a message handled.
    pub async fn ack(&self, id: &str) -> Result<(), String> {
        self.call(Request::Ack { id: id.into() }).await.map(|_| ())
    }

    pub async fn pause(&self, on: bool) -> Result<(), String> {
        self.call(Request::Pause { on }).await.map(|_| ())
    }

    /// Share this session as `name`, recording where this process keeps
    /// its folders for a relay's headless run.
    pub async fn share(&self, name: &str) -> Result<(), String> {
        let env = super::protocol::SHARE_ENV
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
            .collect();
        self.call(Request::Share {
            name: name.into(),
            env: super::protocol::share_env(&env),
        })
        .await
        .map(|_| ())
    }

    /// Whether the session was shared.
    pub async fn unshare(&self) -> Result<bool, String> {
        let v = self.call(Request::Unshare).await?;
        Ok(v["unshared"].as_bool().unwrap_or(false))
    }

    /// Carry one message to the session shared as `share` (a relay's).
    pub async fn relay(
        &self,
        share: &str,
        body: &str,
        via: &str,
        key: Option<&str>,
    ) -> Result<Relayed, String> {
        let v = self
            .call(Request::Relay {
                share: share.into(),
                body: body.into(),
                via: via.into(),
                key: key.map(str::to_string),
            })
            .await?;
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// Change what peers see; kept for the next join too. A name asked for
    /// is answered with the name given.
    #[allow(clippy::too_many_arguments)]
    pub async fn update(&self, change: Change) -> Result<String, String> {
        {
            let mut m = self.inner.member.lock().unwrap();
            if let Some(s) = &change.session {
                m.session = s.clone();
                // A new session starts from its own name.
                m.name = None;
            }
            if let Some(t) = &change.title {
                m.title = Some(t.clone());
            }
            if let Some(p) = &change.project {
                m.project = p.clone();
            }
            if let Some(a) = &change.agent {
                m.agent = a.clone();
            }
            if let Some(x) = &change.model {
                m.model = x.clone();
            }
            if let Some(b) = change.busy {
                m.busy = b;
            }
        }
        let v = self
            .call(Request::Update {
                session: change.session,
                name: change.name.clone(),
                title: change.title,
                project: change.project,
                agent: change.agent,
                model: change.model,
                busy: change.busy,
            })
            .await?;
        let name = v["name"].as_str().unwrap_or_default().to_string();
        if !name.is_empty() {
            if change.name.is_some() {
                self.inner.member.lock().unwrap().name = Some(name.clone());
            }
            self.inner.status.send_replace(Joined::On(name.clone()));
        }
        Ok(name)
    }

    /// Wait up to `timeout` for a response or progress note (to `reply_to`
    /// when given). It's handled once taken: acknowledged, and not also
    /// passed to [`Bus::incoming`].
    pub async fn wait(&self, timeout: Duration, reply_to: Option<&str>) -> Option<Message> {
        let (tx, rx) = oneshot::channel();
        self.inner.waiters.lock().unwrap().push(Waiter {
            reply_to: reply_to.map(str::to_string),
            tx,
        });
        // One that came before the wait began, and is still unhandled.
        if let Ok(pending) = self.inbox(None).await {
            if let Some(m) = pending.into_iter().find(|m| {
                m.kind != Kind::Request && reply_to.is_none_or(|r| m.reply_to.as_deref() == Some(r))
            }) {
                self.inner
                    .waiters
                    .lock()
                    .unwrap()
                    .retain(|w| !w.tx.is_closed());
                let _ = self.ack(&m.id).await;
                remember(&self.inner, &m.id);
                return Some(m);
            }
        }
        let r = tokio::time::timeout(timeout, rx)
            .await
            .ok()
            .and_then(Result::ok);
        self.inner
            .waiters
            .lock()
            .unwrap()
            .retain(|w| !w.tx.is_closed());
        if let Some(m) = &r {
            let _ = self.ack(&m.id).await;
        }
        r
    }
}

/// What [`Bus::update`] changes.
#[derive(Clone, Debug, Default)]
pub struct Change {
    pub session: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub busy: Option<bool>,
}

/// Remember `id` as seen; false if it already was.
fn remember(inner: &Inner, id: &str) -> bool {
    let mut seen = inner.seen.lock().unwrap();
    if seen.iter().any(|s| s == id) {
        return false;
    }
    seen.push_back(id.to_string());
    while seen.len() > SEEN {
        seen.pop_front();
    }
    true
}

/// Give a delivered message to a waiter for it, else to `incoming`.
fn dispatch(inner: &Inner, m: Message) {
    if !remember(inner, &m.id) {
        return;
    }
    if m.kind != Kind::Request {
        let mut waiters = inner.waiters.lock().unwrap();
        let i = waiters.iter().position(|w| {
            !w.tx.is_closed()
                && w.reply_to
                    .as_deref()
                    .is_none_or(|r| m.reply_to.as_deref() == Some(r))
        });
        if let Some(i) = i {
            let w = waiters.remove(i);
            if let Err(m) = w.tx.send(m) {
                drop(waiters);
                let _ = inner.incoming_tx.send(m);
            }
            return;
        }
    }
    let _ = inner.incoming_tx.send(m);
}

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
    Ok(Some(String::from_utf8_lossy(&buf).trim_end().to_string()))
}

/// Connect to the broker in `dir`, after checking it is this user's.
async fn connect(dir: &std::path::Path) -> io::Result<(UnixStream, super::protocol::Registration)> {
    let reg = read_registration(dir)?;
    let stream = UnixStream::connect(dir.join(super::SOCKET)).await?;
    if fs::peer_uid(stream.as_raw_fd())? != fs::euid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the bus socket belongs to another user",
        ));
    }
    Ok((stream, reg))
}

/// Ask the broker in `dir` to stop for `version`, and wait until it has.
async fn replace(dir: &std::path::Path, reg: &super::protocol::Registration, version: &str) {
    if let Ok(mut s) = UnixStream::connect(dir.join(super::SOCKET)).await {
        let mut l = serde_json::to_string(&serde_json::json!({
            "op": "shutdown", "token": reg.token, "version": version
        }))
        .unwrap_or_default();
        l.push('\n');
        let _ = s.write_all(l.as_bytes()).await;
        let mut r = BufReader::new(s);
        let _ = tokio::time::timeout(Duration::from_secs(2), read_line(&mut r)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if read_registration(dir).map_or(true, |r| r.token != reg.token) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Start a broker in `dir` unless one starts meanwhile, and connect.
async fn start(
    dir: &std::path::Path,
    opts: &Options,
) -> io::Result<(UnixStream, super::protocol::Registration)> {
    let Some(program) = super::program() else {
        return Err(io::Error::other(
            "no CodeSmith program to start the bus with",
        ));
    };
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
                return Err(io::Error::other("another process is starting the bus"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(f)
    })
    .await
    .map_err(io::Error::other)??;
    // Someone else may have started it while we waited.
    if let Ok(c) = connect(dir).await {
        drop(lock);
        return Ok(c);
    }
    let mut cmd = std::process::Command::new(program);
    cmd.arg(super::CHILD_ARG)
        .arg("--dir")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env_remove(crate::SERVER_TOKEN_VAR);
    if let Some(h) = &opts.home {
        cmd.env("HOME", h)
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_CACHE_HOME");
    }
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
    let mut child = cmd.spawn()?;
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let r = loop {
        if let Ok(c) = connect(dir).await {
            break Ok(c);
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Exited at once: another broker took the lock, or it failed.
            if let Ok(c) = connect(dir).await {
                break Ok(c);
            }
            break Err(io::Error::other(format!(
                "the bus broker exited ({status})"
            )));
        }
        if tokio::time::Instant::now() >= deadline {
            break Err(io::Error::other("the bus broker didn't start in time"));
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

/// One connection, from finding the broker to a welcome.
async fn open(
    inner: &Inner,
) -> Result<
    (
        BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
        String,
    ),
    String,
> {
    let opts = &inner.opts;
    let dir = match &opts.dir {
        Some(d) => d.clone(),
        None => super::dir().map_err(|e| format!("the bus directory: {e}"))?,
    };
    let (stream, reg) = match connect(&dir).await {
        Ok((s, reg)) if newer(&opts.version, &reg.version) => {
            // An older broker makes way for this newer CodeSmith.
            drop(s);
            replace(&dir, &reg, &opts.version).await;
            if !opts.start {
                return Err("no session is on the bus".into());
            }
            start(&dir, opts).await.map_err(|e| e.to_string())?
        }
        Ok(c) => c,
        Err(_) if opts.start => start(&dir, opts).await.map_err(|e| e.to_string())?,
        Err(_) => return Err("no session is on the bus".into()),
    };
    if reg.protocol != PROTOCOL {
        return Err(format!(
            "the bus is run by CodeSmith {} (protocol {}), which this one ({}) can't talk to; restart this session after upgrading",
            reg.version, reg.protocol, opts.version
        ));
    }
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let member = inner.member.lock().unwrap().clone();
    let hello = Hello {
        op: "hello".into(),
        protocol: PROTOCOL,
        version: opts.version.clone(),
        token: reg.token.clone(),
        member,
    };
    let mut l = serde_json::to_string(&hello).map_err(|e| e.to_string())?;
    l.push('\n');
    w.write_all(l.as_bytes()).await.map_err(|e| e.to_string())?;
    let first = tokio::time::timeout(Duration::from_secs(5), read_line(&mut r))
        .await
        .map_err(|_| "the bus didn't answer".to_string())?
        .map_err(|e| e.to_string())?
        .ok_or("the bus closed the connection")?;
    match serde_json::from_str::<Frame>(&first) {
        Ok(Frame::Welcome { name, .. }) => Ok((r, w, name)),
        Ok(Frame::Bye { reason }) => Err(reason),
        _ => Err("the bus sent something unexpected".into()),
    }
}

async fn run(
    inner: std::sync::Weak<Inner>,
    mut calls: mpsc::UnboundedReceiver<(Request, oneshot::Sender<Result<Value, String>>)>,
) {
    let mut backoff = Duration::from_millis(100);
    loop {
        let Some(me) = inner.upgrade() else { return };
        let opened = open(&me).await;
        let (mut r, mut w, name) = match opened {
            Ok(x) => x,
            Err(why) => {
                me.status.send_replace(Joined::Off(why.clone()));
                let reconnect = me.opts.reconnect;
                drop(me);
                // Calls made while off fail at once.
                let until = tokio::time::Instant::now() + backoff;
                loop {
                    tokio::select! {
                        c = calls.recv() => match c {
                            Some((_, tx)) => { let _ = tx.send(Err(format!("not on the bus: {why}"))); }
                            None => return,
                        },
                        _ = tokio::time::sleep_until(until) => break,
                    }
                }
                if !reconnect {
                    // One try only: keep failing calls until dropped.
                    while let Some((_, tx)) = calls.recv().await {
                        let _ = tx.send(Err(format!("not on the bus: {why}")));
                    }
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        backoff = Duration::from_millis(100);
        me.status.send_replace(Joined::On(name));
        drop(me);
        let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
        let mut next: u64 = 1;
        let lost = loop {
            tokio::select! {
                c = calls.recv() => {
                    let Some((req, tx)) = c else { return };
                    let rid = next;
                    next += 1;
                    let mut l = match serde_json::to_string(&Envelope { rid, request: req }) {
                        Ok(l) => l,
                        Err(e) => { let _ = tx.send(Err(e.to_string())); continue; }
                    };
                    l.push('\n');
                    if w.write_all(l.as_bytes()).await.is_err() {
                        let _ = tx.send(Err("the bus connection was lost".into()));
                        break "the bus connection was lost".to_string();
                    }
                    pending.insert(rid, tx);
                }
                l = read_line(&mut r) => {
                    let l = match l {
                        Ok(Some(l)) => l,
                        _ => break "the bus connection was lost".to_string(),
                    };
                    match serde_json::from_str::<Frame>(&l) {
                        Ok(Frame::Reply(rep)) => {
                            if let Some(tx) = pending.remove(&rep.rid) {
                                let _ = tx.send(if rep.ok { Ok(rep.data) } else { Err(rep.error.unwrap_or_default()) });
                            }
                        }
                        Ok(Frame::Deliver { message }) => {
                            let Some(me) = inner.upgrade() else { return };
                            dispatch(&me, message);
                        }
                        Ok(Frame::Bye { reason }) => break reason,
                        _ => {}
                    }
                }
            }
        };
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err("the bus connection was lost".into()));
        }
        let Some(me) = inner.upgrade() else { return };
        me.status.send_replace(Joined::Off(lost));
        if !me.opts.reconnect {
            drop(me);
            while let Some((_, tx)) = calls.recv().await {
                let _ = tx.send(Err("not on the bus".into()));
            }
            return;
        }
        drop(me);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A member for a session.
pub fn session_member(
    session: &str,
    title: Option<&str>,
    project: &str,
    agent: &str,
    model: &str,
    accepts: bool,
) -> Member {
    Member {
        role: super::Role::Session,
        session: session.to_string(),
        name: None,
        title: title.map(str::to_string),
        project: project.to_string(),
        agent: agent.to_string(),
        model: model.to_string(),
        pid: std::process::id(),
        busy: false,
        accepts,
    }
}

/// A made-up id for a relay or observer.
pub fn transient_id(prefix: &str) -> String {
    format!("{prefix}-{}-{}", now_ms(), crate::paths::random_token(6))
}
