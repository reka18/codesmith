//! Where a shell command runs: a child of this process, or a pipe process
//! of the server's shell daemon (see [`crate::daemon`]), which keeps it
//! running across a server restart. Both are watched the same way.

use super::shell::Ending;
#[cfg(unix)]
use crate::daemon::{protocol, Client, DaemonError, Kind};
use serde_json::{json, Map, Value};
#[cfg(unix)]
use std::sync::Arc;
use std::time::Duration;

/// What a command reports when the daemon running it went away.
pub const LOST: &str = "Command was killed: the process daemon stopped";

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Why a command a restart interrupted was being stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    Timeout,
    Killed,
}

/// A running command.
pub(crate) enum Proc {
    Local(tokio::process::Child),
    #[cfg(unix)]
    Daemon(Remote),
}

impl Proc {
    pub fn id(&self) -> Option<u32> {
        match self {
            Proc::Local(c) => c.id(),
            #[cfg(unix)]
            Proc::Daemon(r) => Some(r.pid),
        }
    }

    /// How it ends. Cancel-safe: a `select!` may poll it again.
    pub async fn wait(&mut self) -> std::io::Result<Ending> {
        match self {
            Proc::Local(c) => c.wait().await.map(super::shell::ending_of),
            #[cfg(unix)]
            Proc::Daemon(r) => Ok(r.wait().await),
        }
    }

    /// Stop it now; with `kill`, SIGKILL without a SIGTERM first.
    pub async fn stop_now(&mut self, grace: Duration, kill: bool) -> Option<Ending> {
        match self {
            #[cfg(unix)]
            Proc::Daemon(r) => r.stop(grace, kill).await,
            _ => self.stop(grace).await,
        }
    }

    /// Its stop timeout is now `ms` from now (0: none), for the next server.
    pub fn set_timeout(&mut self, ms: u64) {
        #[cfg(unix)]
        if let Proc::Daemon(r) = self {
            r.update(|m| {
                m.insert("timeoutMs".into(), json!(ms));
                match ms {
                    0 => m.remove("timeoutDeadline"),
                    _ => m.insert("timeoutDeadline".into(), json!(now_ms() + ms)),
                };
            });
        }
        let _ = ms;
    }

    /// A foreground command moved to the background: no call waits on it
    /// and it runs until it ends.
    pub fn backgrounded(&mut self) {
        #[cfg(unix)]
        if let Proc::Daemon(r) = self {
            r.update(|m| {
                m.remove("waited");
                m.remove("timeoutDeadline");
            });
        }
    }

    /// It is being stopped (`timeout` or `killed`), so a server that takes
    /// it back mid-stop finishes the job.
    pub fn stopping(&mut self, reason: &str) {
        #[cfg(unix)]
        if let Proc::Daemon(r) = self {
            r.update(|m| {
                m.remove("timeoutDeadline");
                m.insert("stopReason".into(), json!(reason));
            });
        }
        let _ = reason;
    }

    /// Stop it: SIGTERM to its process group, then SIGKILL if it is still
    /// running after `grace`. How it ended, when known.
    pub async fn stop(&mut self, grace: Duration) -> Option<Ending> {
        match self {
            Proc::Local(c) => super::shell::stop_child(c, grace).await,
            #[cfg(unix)]
            Proc::Daemon(r) => r.stop(grace, false).await,
        }
    }
}

/// A command the daemon runs.
#[cfg(unix)]
pub(crate) struct Remote {
    pub client: Arc<Client>,
    /// The daemon's id for it.
    pub id: u64,
    pub pid: u32,
    waiter: Option<tokio::task::JoinHandle<Result<protocol::Exited, DaemonError>>>,
    done: Option<Ending>,
    meta: Map<String, Value>,
}

#[cfg(unix)]
impl Remote {
    pub fn new(client: Arc<Client>, id: u64, pid: u32, meta: Map<String, Value>) -> Remote {
        Remote {
            client,
            id,
            pid,
            waiter: None,
            done: None,
            meta,
        }
    }

    async fn wait(&mut self) -> Ending {
        if let Some(e) = &self.done {
            return e.clone();
        }
        let waiter = self.waiter.get_or_insert_with(|| {
            let (c, id) = (self.client.clone(), self.id);
            tokio::spawn(async move { c.wait_async(id).await })
        });
        let r = match waiter.await {
            Ok(r) => r,
            Err(_) => Err(DaemonError::new(Kind::Connect, "wait failed")),
        };
        self.waiter = None;
        // The daemon went away (and the command with it), or forgot it.
        let e = r.map(|x| ending_of_exit(&x)).unwrap_or(Ending::Lost);
        self.done = Some(e.clone());
        e
    }

    async fn signal(&self, sig: &str) -> bool {
        let req = protocol::Request::Signal {
            id: self.id,
            signal: sig.into(),
        };
        self.client.call(req, false).await.is_ok()
    }

    /// SIGTERM (unless `kill_now`), then SIGKILL after `grace`.
    pub async fn stop(&mut self, grace: Duration, kill_now: bool) -> Option<Ending> {
        if let Some(e) = &self.done {
            return Some(e.clone());
        }
        if !kill_now {
            self.signal("SIGTERM").await;
            if let Ok(e) = tokio::time::timeout(grace, self.wait()).await {
                return Some(e);
            }
        }
        self.signal("SIGKILL").await;
        tokio::time::timeout(Duration::from_secs(5), self.wait())
            .await
            .ok()
    }

    /// Change its metadata in the daemon (what the next server reads),
    /// without waiting.
    pub fn update(&mut self, f: impl FnOnce(&mut Map<String, Value>)) {
        f(&mut self.meta);
        let req = protocol::Request::SetMetadata {
            id: self.id,
            metadata: Value::Object(self.meta.clone()),
        };
        let c = self.client.clone();
        tokio::task::spawn_blocking(move || {
            let _ = c.request(&req, false);
        });
    }
}

#[cfg(unix)]
impl Drop for Remote {
    fn drop(&mut self) {
        if let Some(w) = self.waiter.take() {
            w.abort();
        }
    }
}

/// Forget record `id` in the daemon, without waiting.
#[cfg(unix)]
pub fn forget(client: &Arc<Client>, id: u64) {
    let c = client.clone();
    let run = move || {
        let _ = c.request(&protocol::Request::Terminate { id }, false);
    };
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn_blocking(run);
        }
        Err(_) => run(),
    }
}

#[cfg(unix)]
pub fn ending_of_exit(x: &protocol::Exited) -> Ending {
    match (x.exit_code, x.signal.as_deref()) {
        (Some(c), _) => Ending::Exited(c),
        (None, Some(s)) => protocol::signal_from_name(s)
            .map(Ending::Signaled)
            .unwrap_or(Ending::Exited(-1)),
        (None, None) => Ending::Exited(-1),
    }
}

/// What the next server needs to take a command back, kept as the
/// command's metadata in the daemon.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub id: String,
    pub command: String,
    pub shell: String,
    pub file: String,
    pub session_id: String,
    pub directory: String,
    pub timeout_ms: u64,
    /// Unix milliseconds; none when it has no stop timeout.
    pub timeout_deadline: Option<u64>,
    /// A tool call waits on it (a foreground command).
    pub waited: bool,
    /// Being stopped: `timeout` or `killed`.
    pub stop_reason: Option<String>,
}

impl Meta {
    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("command".into(), json!(self.command));
        m.insert("shell".into(), json!(self.shell));
        m.insert("file".into(), json!(self.file));
        m.insert("sessionID".into(), json!(self.session_id));
        m.insert("directory".into(), json!(self.directory));
        m.insert("timeoutMs".into(), json!(self.timeout_ms));
        if let Some(d) = self.timeout_deadline {
            m.insert("timeoutDeadline".into(), json!(d));
        }
        if self.waited {
            m.insert("waited".into(), json!(true));
        }
        if let Some(r) = &self.stop_reason {
            m.insert("stopReason".into(), json!(r));
        }
        m
    }

    pub fn from_value(v: &Value) -> Option<Meta> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Meta {
            id: s("id")?,
            command: s("command").unwrap_or_default(),
            shell: s("shell").unwrap_or_default(),
            file: s("file")?,
            session_id: s("sessionID")?,
            directory: s("directory")?,
            timeout_ms: v.get("timeoutMs").and_then(Value::as_u64).unwrap_or(0),
            timeout_deadline: v.get("timeoutDeadline").and_then(Value::as_u64),
            waited: v.get("waited").and_then(Value::as_bool).unwrap_or(false),
            stop_reason: s("stopReason"),
        })
    }
}
