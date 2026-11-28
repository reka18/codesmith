//! The gateway (`codesmith __remoted --dir DIR`): one per user per machine.
//! It holds `remote.lock` in its 0700 directory for its whole life, so only
//! one runs, and makes way for a newer CodeSmith as the bus broker does.
//!
//! It binds one iroh endpoint, keyed by the machine key, that accepts the
//! ALPNs `codesmith/pair/1` and `codesmith/remote/1`, and opens no
//! listening TCP port. It runs while a device is paired (or a pairing
//! dialog is open) and `remote.enabled` is on; `codesmith remote stop`
//! ends it.
//!
//! # Pairing
//!
//! A dialog (`/mobile`, `codesmith remote pair`) asks for a code over the
//! control socket. The code lasts [`Options::code_ttl`] and is replaced
//! when it runs out, until the dialog closes; one dialog at a time. A
//! phone's `POST /v1/pair/claim` with the live code burns it and is held
//! while the dialog asks the terminal, for at most
//! [`Options::confirm_timeout`].
//!
//! # Connections
//!
//! On `codesmith/remote/1` only a paired, unrevoked device is accepted;
//! revoking closes its connections at once. The routes a device may use
//! come in the next phase ([`route`] is the seam); until then every
//! request answers 404.

use super::control::{
    self, Answer, Asking, Frame, Hello, PairRequest, Registration, Request, Status,
};
use super::http::{self, Request as HttpRequest, Response};
use super::link::{self, Link};
use super::settings::Relay;
use super::store::{self, Store};
use super::{close, PAIR_ALPN, REMOTE_ALPN};
use crate::daemon::fs;
use iroh::endpoint::{presets, Connection};
use iroh::{Endpoint, RelayMap, RelayMode, SecretKey, Watcher};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

/// How many bad claims a device key may make in [`Limits::window`].
#[derive(Debug, Clone)]
pub struct Limits {
    pub bad_claims: usize,
    pub window: Duration,
    pub lockout: Duration,
    /// Connections open at once, of every kind.
    pub connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            bad_claims: 5,
            window: Duration::from_secs(60),
            lockout: Duration::from_secs(300),
            connections: 256,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    pub key: PathBuf,
    pub db: PathBuf,
    /// The global config folder, re-read to see `remote.enabled` turned
    /// off; `None` to run regardless (tests).
    pub config_dir: Option<PathBuf>,
    pub relay: Relay,
    /// Bind loopback only (tests) instead of every interface.
    pub loopback: bool,
    pub code_ttl: Duration,
    pub confirm_timeout: Duration,
    /// How long a dialog waits for the home relay before showing a code.
    pub online_wait: Duration,
    /// How long it runs with nothing paired and no dialog open.
    pub idle: Duration,
    pub version: String,
    /// The machine's name, as the link shows it.
    pub machine: String,
    pub limits: Limits,
}

/// Overrides of [`Options`] for tests, in milliseconds.
pub const IDLE_VAR: &str = "CODESMITH_REMOTE_IDLE_MS";
pub const CODE_TTL_VAR: &str = "CODESMITH_REMOTE_CODE_TTL_MS";
pub const CONFIRM_VAR: &str = "CODESMITH_REMOTE_CONFIRM_MS";

fn ms_var(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

impl Options {
    /// The gateway's options for `dir`, from the environment and settings.
    pub fn new(dir: PathBuf, relay: Relay) -> Options {
        Options {
            dir,
            key: super::key_path(),
            db: super::db_path(),
            config_dir: Some(crate::paths::config_dir()),
            relay,
            loopback: false,
            code_ttl: ms_var(CODE_TTL_VAR, Duration::from_secs(180)),
            confirm_timeout: ms_var(CONFIRM_VAR, Duration::from_secs(90)),
            online_wait: Duration::from_secs(5),
            idle: ms_var(IDLE_VAR, Duration::from_secs(60)),
            version: env!("CARGO_PKG_VERSION").to_string(),
            machine: machine_name(),
            limits: Limits::default(),
        }
    }
}

/// This machine's name: its host name without `.local`.
pub fn machine_name() -> String {
    let mut buf = vec![0u8; 256];
    // SAFETY: `buf` is valid for writes of its length.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    let name = if r == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    let name = link::printable(name.trim_end_matches(".local").trim());
    if name.is_empty() {
        "this machine".into()
    } else {
        name.chars().take(64).collect()
    }
}

/// `codesmith __remoted --dir DIR`.
pub fn main(args: &[String]) -> ! {
    let mut dir = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--dir" {
            dir = it.next().cloned();
        }
    }
    let Some(dir) = dir.filter(|d| Path::new(d).is_absolute()) else {
        eprintln!("usage: codesmith __remoted --dir DIR");
        std::process::exit(2);
    };
    let _ = std::env::set_current_dir("/");
    let dir = PathBuf::from(dir);
    let code = match super::settings::load()
        .map_err(io::Error::other)
        .and_then(|s| run(Options::new(dir.clone(), s.relay)))
    {
        Ok(()) => 0,
        Err(e) => {
            let _ = crate::paths::write_private(
                &dir.join(control::ERROR_FILE),
                e.to_string().as_bytes(),
            );
            eprintln!("codesmith __remoted: {e}");
            1
        }
    };
    std::process::exit(code)
}

/// Run a gateway until it's told to stop or has nothing to do. Returns at
/// once if another gateway holds the lock.
pub fn run(opts: Options) -> io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let r = rt.block_on(serve(opts, None));
    rt.shutdown_timeout(Duration::from_secs(2));
    r
}

/// What a started gateway tells an in-process caller.
#[derive(Debug, Clone)]
pub struct Ready {
    pub node_id: String,
    pub addrs: Vec<std::net::SocketAddr>,
    pub stop: watch::Sender<bool>,
}

/// The relays `relay` names, as iroh takes them.
fn relay_mode(relay: &Relay) -> io::Result<Option<RelayMode>> {
    Ok(match relay {
        Relay::None => Some(RelayMode::Disabled),
        Relay::N0 => None,
        r => {
            let urls = r.urls();
            let map = RelayMap::try_from_iter(urls.iter().map(String::as_str))
                .map_err(|e| io::Error::other(format!("remote.relay: {e}")))?;
            Some(RelayMode::Custom(map))
        }
    })
}

async fn bind(opts: &Options, key: SecretKey) -> io::Result<Endpoint> {
    let mode = relay_mode(&opts.relay)?;
    // No address lookup: n0's DNS and pkarr servers are never contacted.
    // The phone dials the node id with the relay and addresses from the QR.
    let mut b = match mode {
        Some(_) => Endpoint::builder(presets::Minimal),
        None => Endpoint::builder(presets::N0),
    }
    .secret_key(key)
    .alpns(vec![PAIR_ALPN.to_vec(), REMOTE_ALPN.to_vec()]);
    if let Some(m) = mode {
        b = b.relay_mode(m);
    }
    if opts.loopback {
        b = b
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .map_err(|e| io::Error::other(e.to_string()))?;
    }
    b.bind()
        .await
        .map_err(|e| io::Error::other(format!("starting the endpoint: {e}")))
}

/// A device key's recent bad claims.
#[derive(Default)]
struct Attempts {
    bad: VecDeque<Instant>,
    locked_until: Option<Instant>,
}

struct Pending {
    id: u64,
    tx: oneshot::Sender<Answer>,
}

struct Pairing {
    dialog: u64,
    tx: mpsc::UnboundedSender<Frame>,
    request: PairRequest,
    /// The live code and when it runs out; `None` while a claim of it is
    /// being confirmed.
    code: Option<(String, Instant)>,
    pending: Option<Pending>,
}

struct State {
    opts: Options,
    store: Store,
    node_id: String,
    started: i64,
    pairing: Option<Pairing>,
    /// Each paired device's open connections.
    conns: HashMap<String, Vec<Connection>>,
    attempts: HashMap<String, Attempts>,
    open: usize,
    next: u64,
    /// When there was last something to do, for the idle exit.
    busy_at: Instant,
}

type Shared = Arc<Mutex<State>>;

/// Run a gateway: see [`run`]. `ready` gets its node id and addresses once
/// it serves.
pub async fn serve(opts: Options, ready: Option<oneshot::Sender<Ready>>) -> io::Result<()> {
    fs::secure_dir(&opts.dir)?;
    let lock = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(opts.dir.join(super::LOCK))?
    };
    if !fs::try_lock(&lock) {
        // Another gateway runs; it serves.
        return Ok(());
    }
    let _ = std::fs::remove_file(opts.dir.join(control::ERROR_FILE));
    let key =
        super::key::load_or_create(&opts.key).map_err(|e| io::Error::other(format!("{e:#}")))?;
    let node_id = super::key::node_id(&key);
    let store = Store::open(&opts.db).map_err(|e| io::Error::other(format!("{e:#}")))?;
    let endpoint = bind(&opts, key).await?;
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
        protocol: control::PROTOCOL,
        version: opts.version.clone(),
        token: token.clone(),
        node_id: node_id.clone(),
        started: store::now_ms(),
    };
    crate::paths::write_private(
        &opts.dir.join(super::REGISTRATION),
        &serde_json::to_vec(&reg).map_err(io::Error::other)?,
    )?;
    let (stop_tx, mut stop) = watch::channel(false);
    let state: Shared = Arc::new(Mutex::new(State {
        opts: opts.clone(),
        store,
        node_id: node_id.clone(),
        started: reg.started,
        pairing: None,
        conns: HashMap::new(),
        attempts: HashMap::new(),
        open: 0,
        next: 1,
        busy_at: Instant::now(),
    }));
    let _ = state
        .lock()
        .unwrap()
        .store
        .audit(None, "gateway_started", None, None);
    if let Some(r) = ready {
        let _ = r.send(Ready {
            node_id: node_id.clone(),
            addrs: endpoint.bound_sockets(),
            stop: stop_tx.clone(),
        });
    }
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut last_settings = Instant::now();
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tick.tick() => {
                if idle(&state) {
                    break;
                }
                if let Some(dir) = &opts.config_dir {
                    if last_settings.elapsed() >= Duration::from_secs(2) {
                        last_settings = Instant::now();
                        // Turned off (or made unreadable): stop.
                        if !super::settings::load_from(dir).is_ok_and(|s| s.enabled) {
                            break;
                        }
                    }
                }
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let state = state.clone();
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    accept(state, endpoint, incoming).await;
                });
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let (state, token, stop_tx, endpoint) =
                    (state.clone(), token.clone(), stop_tx.clone(), endpoint.clone());
                tokio::spawn(async move {
                    let _ = control_conn(stream, state, endpoint, token, stop_tx).await;
                });
            }
        }
    }
    // Leave: our files only, so a gateway that replaced us keeps its own.
    if control::read_registration(&opts.dir).is_ok_and(|r| r.token == token) {
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_file(opts.dir.join(super::REGISTRATION));
    }
    {
        let mut st = state.lock().unwrap();
        if let Some(p) = st.pairing.take() {
            let _ = p.tx.send(Frame::Refused {
                reason: "the gateway stopped".into(),
            });
        }
        for c in st.conns.values().flatten() {
            c.close(close::STOPPING.into(), b"the gateway is stopping");
        }
        let _ = st.store.audit(None, "gateway_stopped", None, None);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), endpoint.close()).await;
    drop(lock);
    Ok(())
}

/// Whether the gateway has had nothing to do for its idle time: no paired
/// device, no dialog, no connection.
fn idle(state: &Shared) -> bool {
    let mut st = state.lock().unwrap();
    let paired = st
        .store
        .devices(false)
        .map(|d| !d.is_empty())
        .unwrap_or(true);
    if paired || st.pairing.is_some() || st.open > 0 {
        st.busy_at = Instant::now();
        return false;
    }
    st.busy_at.elapsed() >= st.opts.idle
}

/// The link for `code`, from the endpoint's current addresses.
fn link_for(endpoint: &Endpoint, st: &State, code: &str) -> (Link, Option<String>, bool) {
    let addr = endpoint.addr();
    let home: Vec<(String, bool)> = endpoint
        .home_relay_status()
        .get()
        .into_iter()
        .map(|s| (s.url().to_string(), s.is_connected()))
        .collect();
    let connected = home.iter().find(|(_, c)| *c).map(|(u, _)| u.clone());
    let relay = connected
        .clone()
        .or_else(|| addr.relay_urls().next().map(|u| u.to_string()))
        .or_else(|| st.opts.relay.urls().into_iter().next());
    let mut addrs: Vec<String> = addr.ip_addrs().map(|a| a.to_string()).collect();
    addrs.sort();
    addrs.dedup();
    let l = Link {
        node_id: st.node_id.clone(),
        relay: relay.clone(),
        addrs,
        machine: st.opts.machine.clone(),
        session: st.pairing.as_ref().and_then(|p| p.request.session.clone()),
        code: code.to_string(),
    };
    (l, relay, connected.is_some())
}

/// Give the dialog `dialog` a new code, if it's still the one open.
fn new_code(state: &Shared, endpoint: &Endpoint, dialog: u64) {
    let mut st = state.lock().unwrap();
    let ttl = st.opts.code_ttl;
    if !st.pairing.as_ref().is_some_and(|p| p.dialog == dialog) {
        return;
    }
    let code = link::new_code();
    let (l, relay, connected) = link_for(endpoint, &st, &code);
    let machine = st.opts.machine.clone();
    let p = st.pairing.as_mut().unwrap();
    p.code = Some((code.clone(), Instant::now() + ttl));
    let _ = p.tx.send(Frame::Code {
        link: l.url(),
        code,
        expires_in_ms: ttl.as_millis() as u64,
        machine,
        relay,
        relay_connected: connected,
    });
}

async fn control_conn(
    stream: UnixStream,
    state: Shared,
    endpoint: Endpoint,
    token: String,
    stop: watch::Sender<bool>,
) -> io::Result<()> {
    if fs::peer_uid(stream.as_raw_fd())? != fs::euid() {
        return Ok(());
    }
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let first = match tokio::time::timeout(Duration::from_secs(5), control::read_line(&mut r)).await
    {
        Ok(Ok(Some(l))) => l,
        _ => return Ok(()),
    };
    let err = |m: &str| {
        control::line(&Frame::Error {
            message: m.to_string(),
        })
    };
    let v: Value = match serde_json::from_str(&first) {
        Ok(v) => v,
        Err(_) => return w.write_all(err("bad request").as_bytes()).await,
    };
    let theirs = v.get("token").and_then(Value::as_str).unwrap_or("");
    if !fs::constant_eq(theirs.as_bytes(), token.as_bytes()) {
        return w.write_all(err("authentication failed").as_bytes()).await;
    }
    let hello: Hello = match serde_json::from_value(v) {
        Ok(h) => h,
        Err(_) => return w.write_all(err("bad request").as_bytes()).await,
    };
    if hello.protocol != control::PROTOCOL {
        return w
            .write_all(
                err(&format!(
                    "this gateway speaks protocol {}, not {}",
                    control::PROTOCOL,
                    hello.protocol
                ))
                .as_bytes(),
            )
            .await;
    }
    let reply = match hello.request {
        Request::Status => Frame::Status(status(&state, &endpoint)),
        Request::Stop { version } => {
            let ours = state.lock().unwrap().opts.version.clone();
            match version {
                Some(v) if !crate::bus::protocol::newer(&v, &ours) => Frame::Error {
                    message: "only a newer CodeSmith may replace the gateway".into(),
                },
                _ => {
                    let _ = stop.send(true);
                    Frame::Ok
                }
            }
        }
        Request::Revoke { device } => match revoke(&state, &device) {
            Ok(()) => Frame::Ok,
            Err(e) => Frame::Error { message: e },
        },
        Request::Pair(req) => return dialog(r, w, state, endpoint, req).await,
    };
    w.write_all(control::line(&reply).as_bytes()).await
}

fn status(state: &Shared, endpoint: &Endpoint) -> Status {
    let st = state.lock().unwrap();
    let home = endpoint
        .home_relay_status()
        .get()
        .into_iter()
        .find(|s| s.is_connected())
        .map(|s| s.url().to_string());
    Status {
        pid: std::process::id(),
        version: st.opts.version.clone(),
        node_id: st.node_id.clone(),
        relay: match &st.opts.relay {
            Relay::Default => "default".into(),
            Relay::None => "none".into(),
            Relay::N0 => "n0".into(),
            Relay::Url(u) => u.clone(),
        },
        home_relay: home,
        addrs: endpoint.addr().ip_addrs().map(|a| a.to_string()).collect(),
        devices: st.store.devices(false).map(|d| d.len()).unwrap_or(0),
        connections: st.open,
        pairing: st.pairing.is_some(),
        started: st.started,
    }
}

/// Revoke the device `query` names and close its connections.
fn revoke(state: &Shared, query: &str) -> Result<(), String> {
    let mut st = state.lock().unwrap();
    let d = st.store.find(query).map_err(|e| e.to_string())?;
    st.store.revoke(&d.id).map_err(|e| e.to_string())?;
    let _ = st.store.audit(Some(&d.id), "revoked", None, None);
    for c in st.conns.remove(&d.id).unwrap_or_default() {
        c.close(close::UNPAIRED.into(), b"revoked");
    }
    Ok(())
}

/// The pairing dialog: codes out, answers in, until it closes.
async fn dialog(
    mut r: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut w: tokio::net::unix::OwnedWriteHalf,
    state: Shared,
    endpoint: Endpoint,
    req: PairRequest,
) -> io::Result<()> {
    if let Some(s) = &req.session {
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || s.len() > 128
        {
            return w
                .write_all(
                    control::line(&Frame::Error {
                        message: "a bad session id".into(),
                    })
                    .as_bytes(),
                )
                .await;
        }
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    let (me, online_wait, relay) = {
        let mut st = state.lock().unwrap();
        let me = st.next;
        st.next += 1;
        if let Some(old) = st.pairing.take() {
            let _ = old.tx.send(Frame::Replaced);
        }
        st.pairing = Some(Pairing {
            dialog: me,
            tx: tx.clone(),
            request: req,
            code: None,
            pending: None,
        });
        (me, st.opts.online_wait, st.opts.relay.clone())
    };
    // The home relay first, briefly, so the link names it.
    if relay != Relay::None {
        let _ = tokio::time::timeout(online_wait, endpoint.online()).await;
    }
    new_code(&state, &endpoint, me);
    let result: io::Result<()> = async {
        loop {
            let expires = {
                let st = state.lock().unwrap();
                match st.pairing.as_ref().filter(|p| p.dialog == me) {
                    None => None,
                    Some(p) => Some(p.code.as_ref().map(|(_, at)| *at)),
                }
            };
            let Some(expires) = expires else {
                // Replaced or ended: say what's queued, then close.
                while let Ok(f) = rx.try_recv() {
                    w.write_all(control::line(&f).as_bytes()).await?;
                }
                return Ok(());
            };
            let far = Instant::now() + Duration::from_secs(3600);
            tokio::select! {
                l = control::read_line(&mut r) => {
                    let Some(l) = l? else { return Ok(()) };
                    if let Ok(a) = serde_json::from_str::<Answer>(&l) {
                        let mut st = state.lock().unwrap();
                        if let Some(p) = st.pairing.as_mut().filter(|p| p.dialog == me) {
                            if p.pending.as_ref().is_some_and(|q| q.id == a.id) {
                                let q = p.pending.take().unwrap();
                                let _ = q.tx.send(a);
                            }
                        }
                    }
                }
                f = rx.recv() => {
                    let Some(f) = f else { return Ok(()) };
                    let last = matches!(f, Frame::Replaced | Frame::Paired { .. });
                    w.write_all(control::line(&f).as_bytes()).await?;
                    if last {
                        return Ok(());
                    }
                }
                _ = tokio::time::sleep_until(expires.unwrap_or(far)) => {
                    let still = {
                        let st = state.lock().unwrap();
                        st.pairing.as_ref().is_some_and(|p| {
                            p.dialog == me
                                && p.code.as_ref().is_some_and(|(_, at)| *at <= Instant::now())
                        })
                    };
                    if still {
                        new_code(&state, &endpoint, me);
                    }
                }
            }
        }
    }
    .await;
    let mut st = state.lock().unwrap();
    if st.pairing.as_ref().is_some_and(|p| p.dialog == me) {
        st.pairing = None;
    }
    result
}

/// Accept one connection and serve it by its ALPN.
async fn accept(state: Shared, endpoint: Endpoint, incoming: iroh::endpoint::Incoming) {
    let Ok(accepting) = incoming.accept() else {
        return;
    };
    let Ok(Ok(conn)) = tokio::time::timeout(Duration::from_secs(30), accepting).await else {
        return;
    };
    {
        let mut st = state.lock().unwrap();
        if st.open >= st.opts.limits.connections {
            conn.close(close::STOPPING.into(), b"too many connections");
            return;
        }
        st.open += 1;
    }
    let alpn = conn.alpn().to_vec();
    if alpn == PAIR_ALPN {
        pair_conn(&state, &endpoint, conn).await;
    } else if alpn == REMOTE_ALPN {
        remote_conn(&state, conn).await;
    } else {
        conn.close(close::ALPN.into(), b"unknown protocol");
    }
    state.lock().unwrap().open -= 1;
}

/// A connection on `codesmith/pair/1`: claims only, and only while a code
/// is live.
async fn pair_conn(state: &Shared, endpoint: &Endpoint, conn: Connection) {
    let live = {
        let st = state.lock().unwrap();
        st.pairing.as_ref().is_some_and(|p| p.code.is_some())
    };
    if !live {
        conn.close(close::NO_CODE.into(), b"no pairing code is live");
        return;
    }
    let device = conn.remote_id().to_string();
    // A pairing connection lasts long enough for a claim and its
    // confirmation, no longer.
    let limit = state.lock().unwrap().opts.confirm_timeout + Duration::from_secs(60);
    let serve = async {
        let mut tasks = tokio::task::JoinSet::new();
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            if tasks.len() >= 4 {
                let _ = http::write_response(
                    &mut send,
                    &Response::error(429, "rate_limited", "too many requests at once"),
                )
                .await;
                let _ = send.finish();
                continue;
            }
            let (state, endpoint, device) = (state.clone(), endpoint.clone(), device.clone());
            tasks.spawn(async move {
                let resp = match tokio::time::timeout(
                    Duration::from_secs(10),
                    http::read_request(&mut recv),
                )
                .await
                {
                    Err(_) => Response::error(400, "bad_request", "the request took too long"),
                    Ok(Err(http::ReadError::Closed)) => return,
                    Ok(Err(e)) => e.into(),
                    Ok(Ok(req)) => pair_route(&state, &endpoint, &device, req).await,
                };
                let _ = http::write_response(&mut send, &resp).await;
                let _ = send.finish();
                let _ = send.stopped().await;
            });
            while tasks.try_join_next().is_some() {}
        }
        while tasks.join_next().await.is_some() {}
    };
    let _ = tokio::time::timeout(limit, serve).await;
    conn.close(close::STOPPING.into(), b"done");
}

async fn pair_route(
    state: &Shared,
    endpoint: &Endpoint,
    device: &str,
    req: HttpRequest,
) -> Response {
    if req.header("origin").is_some() {
        return Response::error(
            400,
            "bad_request",
            "requests with an Origin header are refused",
        );
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/v1/pair/claim") => claim(state, endpoint, device, &req.body).await,
        _ => Response::error(404, "not_found", "no such route"),
    }
}

#[derive(Deserialize)]
struct ClaimBody {
    code: String,
    device: ClaimDevice,
    #[serde(default)]
    push: Option<ClaimPush>,
    #[serde(default)]
    unlock_key: Option<ClaimUnlockKey>,
}

#[derive(Deserialize)]
struct ClaimDevice {
    name: String,
    platform: String,
}

#[derive(Deserialize)]
struct ClaimPush {
    kind: String,
    token: String,
}

#[derive(Deserialize)]
struct ClaimUnlockKey {
    alg: String,
    spki: String,
}

/// A P-256 SubjectPublicKeyInfo's DER prefix (id-ecPublicKey, prime256v1,
/// an uncompressed point follows).
const P256_SPKI_PREFIX: [u8; 27] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
];

/// Whether `spki` (standard base64 with padding) is a P-256 public key's
/// DER SubjectPublicKeyInfo.
pub fn valid_spki(spki: &str) -> bool {
    use base64::Engine;
    let Ok(der) = base64::engine::general_purpose::STANDARD.decode(spki) else {
        return false;
    };
    der.len() == 91 && der.starts_with(&P256_SPKI_PREFIX)
}

/// `s` cleaned for showing: no control or bidi characters, trimmed, cut
/// to `max` characters.
fn clean(s: &str, max: usize) -> String {
    link::printable(s)
        .trim()
        .chars()
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Record a bad claim by `device`; whether it's now locked out.
fn bad_claim(st: &mut State, device: &str) -> bool {
    let limits = st.opts.limits.clone();
    let now = Instant::now();
    if st.attempts.len() > 4096 {
        st.attempts.retain(|_, a| {
            a.locked_until.is_some_and(|t| t > now)
                || a.bad
                    .back()
                    .is_some_and(|t| now.duration_since(*t) < limits.window)
        });
    }
    let a = st.attempts.entry(device.to_string()).or_default();
    a.bad.push_back(now);
    while a
        .bad
        .front()
        .is_some_and(|t| now.duration_since(*t) >= limits.window)
    {
        a.bad.pop_front();
    }
    if a.bad.len() > limits.bad_claims {
        a.locked_until = Some(now + limits.lockout);
        a.bad.clear();
        true
    } else {
        false
    }
}

fn locked(st: &State, device: &str) -> bool {
    st.attempts
        .get(device)
        .and_then(|a| a.locked_until)
        .is_some_and(|t| t > Instant::now())
}

fn rate_limited() -> Response {
    Response::error(
        429,
        "rate_limited",
        "too many bad claims from this device; try again in a few minutes",
    )
}

fn expired() -> Response {
    Response::error(
        410,
        "code_expired",
        "that pairing code is wrong, used or expired; scan the QR code again",
    )
}

fn refused() -> Response {
    Response::error(
        403,
        "pairing_refused",
        "the pairing was refused at the terminal",
    )
}

/// `POST /v1/pair/claim`.
async fn claim(state: &Shared, endpoint: &Endpoint, device: &str, body: &[u8]) -> Response {
    // A locked key's claims are refused before the code is looked at, so
    // they can't burn it.
    if locked(&state.lock().unwrap(), device) {
        return rate_limited();
    }
    let parsed: ClaimBody = match serde_json::from_slice(body) {
        Ok(b) => b,
        Err(e) => return Response::error(400, "bad_request", &format!("a bad claim: {e}")),
    };
    let name = clean(&parsed.device.name, 64);
    let platform = clean(&parsed.device.platform, 32);
    if name.is_empty() || platform.is_empty() {
        return Response::error(
            400,
            "bad_request",
            "device.name and device.platform must not be empty",
        );
    }
    let unlock_key =
        match parsed.unlock_key {
            None => None,
            Some(k) if k.alg == "ES256" && valid_spki(&k.spki) => Some(store::UnlockKey {
                alg: k.alg,
                spki: k.spki,
            }),
            Some(_) => return Response::error(
                400,
                "bad_request",
                "unlock_key must be {alg: \"ES256\", spki: base64 of a P-256 SubjectPublicKeyInfo}",
            ),
        };
    let push = match parsed.push {
        None => None,
        Some(p)
            if !p.kind.is_empty()
                && p.kind.len() <= 32
                && p.kind
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                && !p.token.is_empty()
                && p.token.len() <= 4096 =>
        {
            Some(store::Push {
                kind: p.kind,
                token: p.token,
            })
        }
        Some(_) => return Response::error(400, "bad_request", "a bad push token"),
    };
    // Check and burn the code, and ask the dialog.
    let (rx, request, check, dialog, timeout) = {
        let mut guard = state.lock().unwrap();
        let st = &mut *guard;
        if locked(st, device) {
            return rate_limited();
        }
        let live = st
            .pairing
            .as_ref()
            .and_then(|p| p.code.as_ref())
            .filter(|(_, at)| *at > Instant::now())
            .map(|(c, _)| c.clone());
        let Some(code) = live.filter(|c| fs::constant_eq(c.as_bytes(), parsed.code.as_bytes()))
        else {
            let _ = st.store.audit(Some(device), "claim_failed", None, None);
            return if bad_claim(st, device) {
                let _ = st.store.audit(Some(device), "claim_locked", None, None);
                rate_limited()
            } else {
                expired()
            };
        };
        let check = link::check_code(&code, &st.node_id, device);
        let paired = st.store.is_paired(device).unwrap_or(false);
        let id = st.next;
        st.next += 1;
        let timeout = st.opts.confirm_timeout;
        let p = st.pairing.as_mut().unwrap();
        p.code = None;
        let (tx, rx) = oneshot::channel();
        p.pending = Some(Pending { id, tx });
        let _ = p.tx.send(Frame::Confirm {
            id,
            device: Asking {
                id: device.to_string(),
                name: name.clone(),
                platform: platform.clone(),
                paired,
            },
            check: check.clone(),
            expires_in_ms: timeout.as_millis() as u64,
        });
        (rx, p.request.clone(), check, p.dialog, timeout)
    };
    let _ = check;
    let answer = tokio::time::timeout(timeout, rx).await;
    let asking = |paired| Asking {
        id: device.to_string(),
        name: name.clone(),
        platform: platform.clone(),
        paired,
    };
    let refuse = |reason: &str| {
        let st = state.lock().unwrap();
        let _ = st.store.audit(
            Some(device),
            "pairing_refused",
            request.session.as_deref(),
            Some(reason),
        );
        if let Some(p) = st.pairing.as_ref().filter(|p| p.dialog == dialog) {
            let _ = p.tx.send(Frame::Refused {
                reason: reason.to_string(),
            });
        }
    };
    let a = match answer {
        Ok(Ok(a)) => a,
        Ok(Err(_)) => {
            refuse("the dialog closed");
            return refused();
        }
        Err(_) => {
            {
                let mut st = state.lock().unwrap();
                if let Some(p) = st.pairing.as_mut().filter(|p| p.dialog == dialog) {
                    p.pending = None;
                }
            }
            refuse("no answer in time");
            new_code(state, endpoint, dialog);
            return refused();
        }
    };
    // The grant must be one the dialog offered: its session or all.
    let grant_ok = a.grant.as_ref().is_none_or(|g| {
        g.session == store::ALL_SESSIONS || Some(&g.session) == request.session.as_ref()
    });
    if !a.allow || !grant_ok {
        refuse("refused at the terminal");
        new_code(state, endpoint, dialog);
        return refused();
    }
    let st = state.lock().unwrap();
    let paired_before = st.store.is_paired(device).unwrap_or(false);
    if let Err(e) = st.store.pair(
        device,
        &name,
        &platform,
        unlock_key.as_ref(),
        push.as_ref(),
        a.grant.as_ref().map(|g| (g.session.as_str(), g.scope)),
    ) {
        return Response::error(
            500,
            "internal",
            &format!("couldn't save the pairing: {e:#}"),
        );
    }
    let detail = a
        .grant
        .as_ref()
        .map(|g| format!("{} {}", g.scope.as_str(), g.session));
    let _ = st.store.audit(
        Some(device),
        if paired_before {
            "grant_added"
        } else {
            "paired"
        },
        a.grant.as_ref().map(|g| g.session.as_str()),
        detail.as_deref(),
    );
    if let Some(p) = st.pairing.as_ref().filter(|p| p.dialog == dialog) {
        let _ = p.tx.send(Frame::Paired {
            device: asking(paired_before),
            grant: a.grant.clone(),
        });
    }
    let grants: Vec<Value> = st
        .store
        .device(device)
        .ok()
        .flatten()
        .map(|d| d.grants)
        .unwrap_or_default()
        .into_iter()
        .map(|g| json!({"session": g.session, "scope": g.scope}))
        .collect();
    Response::ok(json!({
        "machine": {"id": st.node_id, "name": st.opts.machine},
        "device": {"id": device, "name": name, "platform": platform},
        "grants": grants,
    }))
}

/// A connection on `codesmith/remote/1`: paired, unrevoked devices only.
async fn remote_conn(state: &Shared, conn: Connection) {
    let device = conn.remote_id().to_string();
    {
        // Checked and registered under one lock, so a revoke can't fall
        // between the two.
        let mut st = state.lock().unwrap();
        if !st.store.is_paired(&device).unwrap_or(false) {
            drop(st);
            conn.close(close::UNPAIRED.into(), b"this device isn't paired");
            return;
        }
        st.conns
            .entry(device.clone())
            .or_default()
            .push(conn.clone());
        let _ = st.store.audit(Some(&device), "connected", None, None);
    }
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let (state, device) = (state.clone(), device.clone());
        tokio::spawn(async move {
            let resp =
                match tokio::time::timeout(Duration::from_secs(30), http::read_request(&mut recv))
                    .await
                {
                    Err(_) => Response::error(400, "bad_request", "the request took too long"),
                    Ok(Err(http::ReadError::Closed)) => return,
                    Ok(Err(e)) => e.into(),
                    Ok(Ok(req)) => route(&state, &device, req),
                };
            let _ = http::write_response(&mut send, &resp).await;
            let _ = send.finish();
            let _ = send.stopped().await;
        });
    }
    let mut guard = state.lock().unwrap();
    let st = &mut *guard;
    let id = conn.stable_id();
    if let Some(v) = st.conns.get_mut(&device) {
        v.retain(|c| c.stable_id() != id);
        if v.is_empty() {
            st.conns.remove(&device);
        }
    }
}

/// A paired device's request. The phase A routes (`/v1/machine`,
/// `/v1/sessions`, events) come here next; each checks the device's grant
/// and answers 404 for a session it has none for.
fn route(state: &Shared, device: &str, req: HttpRequest) -> Response {
    if req.header("origin").is_some() {
        return Response::error(
            400,
            "bad_request",
            "requests with an Origin header are refused",
        );
    }
    // Revoked while the connection was opening: nothing more.
    if !state
        .lock()
        .unwrap()
        .store
        .is_paired(device)
        .unwrap_or(false)
    {
        return Response::error(404, "not_found", "no such route");
    }
    Response::error(404, "not_found", "no such route")
}
