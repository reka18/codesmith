//! One running `claude`, spoken to over its stream-json control protocol,
//! the protocol the Claude Agent SDK speaks to it.
//!
//! The process gets its own process group, so killing it also kills what
//! it started, and it is killed when dropped. Its output is read line by
//! line, each line bounded; its error output is drained and only its tail
//! kept. At most [`MAX_PROCESSES`] run at once in all, and a registry
//! caps its own further (see [`super::Pool`]): the slot a process holds is
//! given back only once its child has been reaped.

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};

/// The most `claude` processes CodeSmith runs at once.
pub const MAX_PROCESSES: usize = 32;

/// The longest output line read (a tool result with images can be large).
const LINE_LIMIT: usize = 64 * 1024 * 1024;

/// How much of the error output is kept for messages.
const STDERR_TAIL: usize = 4096;

static SLOTS: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(MAX_PROCESSES)));

/// Answers the control requests `claude` sends (hook callbacks, the tool
/// server's MCP messages).
pub(crate) trait Handler: Send + Sync {
    fn handle(
        &self,
        request: Value,
    ) -> futures_util::future::BoxFuture<'static, Option<Result<Value, String>>>;
}

pub(crate) struct Spawn {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
}

type Controls = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

/// How the process ended: its exit code (`None` when killed by a signal).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Exit {
    pub code: Option<i32>,
    pub stderr: String,
}

impl Exit {
    /// The error a turn that was still running ends with.
    pub fn error(&self) -> Option<String> {
        match self.code {
            Some(0) => None,
            Some(c) => {
                let tail = self.stderr.trim();
                Some(if tail.is_empty() {
                    format!("Claude Code process exited with code {c}")
                } else {
                    format!("Claude Code process exited with code {c}: {tail}")
                })
            }
            None => Some("Claude Code process was terminated by a signal".into()),
        }
    }
}

pub(crate) struct Process {
    stdin: Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>,
    controls: Controls,
    messages: tokio::sync::Mutex<mpsc::UnboundedReceiver<Value>>,
    exit: watch::Receiver<Option<Exit>>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    pid: Option<u32>,
}

/// Held while a process runs; dropped once its child was reaped.
pub(crate) type Guard = Box<dyn Send + Sync>;

fn random_id() -> String {
    super::uuid().replace('-', "")[..12].to_string()
}

async fn write_line(
    stdin: &tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    msg: &Value,
) -> Result<(), String> {
    let mut guard = stdin.lock().await;
    let Some(w) = guard.as_mut() else {
        return Err("Claude Code session has stopped".into());
    };
    let mut line = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    line.push(b'\n');
    w.write_all(&line).await.map_err(|e| e.to_string())?;
    w.flush().await.map_err(|e| e.to_string())
}

impl Process {
    pub fn start(
        spawn: Spawn,
        handler: Arc<dyn Handler>,
        guard: Option<Guard>,
    ) -> Result<Process, String> {
        let slot: OwnedSemaphorePermit = SLOTS.clone().try_acquire_owned().map_err(|_| {
            format!("CodeSmith is already running {MAX_PROCESSES} Claude Code processes")
        })?;
        let mut cmd = tokio::process::Command::new(&spawn.executable);
        cmd.args(&spawn.args)
            .env_clear()
            .envs(&spawn.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spawn.cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "could not start Claude Code ({}): {e}",
                spawn.executable.display()
            )
        })?;
        let pid = child.id();
        #[cfg(unix)]
        let pgid = pid.map(|p| p as i32);
        let stdin = Arc::new(tokio::sync::Mutex::new(child.stdin.take()));
        let stdout = child.stdout.take().ok_or("Claude Code has no output")?;
        let stderr = child.stderr.take();
        let controls: Controls = Arc::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = watch::channel(None);
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let tail = Arc::new(Mutex::new(String::new()));

        if let Some(mut err) = stderr {
            let tail = tail.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap();
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if t.len() > STDERR_TAIL {
                        let cut = t.len() - STDERR_TAIL;
                        let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
                        t.drain(..cut);
                    }
                }
            });
        }

        // The child: waits for its exit, or kills its group when asked.
        let (status_tx, status_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut owned = Owned {
                child,
                #[cfg(unix)]
                pgid,
            };
            let status = tokio::select! {
                s = owned.child.wait() => s.ok(),
                _ = kill_rx => {
                    owned.kill_group();
                    owned.child.wait().await.ok()
                }
            };
            // Reaped: the slots go back before anyone hears of the exit.
            drop(guard);
            drop(slot);
            let _ = status_tx.send(status.and_then(|s| s.code()));
        });

        // The reader: dispatches every output line.
        {
            let controls = controls.clone();
            let stdin = stdin.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    let read = (&mut reader)
                        .take(LINE_LIMIT as u64 + 1)
                        .read_until(b'\n', &mut line)
                        .await;
                    match read {
                        Ok(0) | Err(_) => break,
                        Ok(_) if line.len() > LINE_LIMIT && !line.ends_with(b"\n") => break,
                        Ok(_) => {}
                    }
                    let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
                        continue;
                    };
                    match msg["type"].as_str() {
                        Some("control_response") => {
                            let r = &msg["response"];
                            let id = r["request_id"].as_str().unwrap_or_default();
                            let waiting = controls.lock().unwrap().remove(id);
                            if let Some(w) = waiting {
                                let _ = w.send(if r["subtype"] == "success" {
                                    Ok(r["response"].clone())
                                } else {
                                    Err(r["error"]
                                        .as_str()
                                        .unwrap_or("control request failed")
                                        .into())
                                });
                            }
                        }
                        Some("control_request") => {
                            let id = msg["request_id"].clone();
                            let answer = handler.handle(msg["request"].clone());
                            let stdin = stdin.clone();
                            tokio::spawn(async move {
                                let Some(result) = answer.await else {
                                    return;
                                };
                                let response = match result {
                                    Ok(r) => {
                                        json!({"subtype": "success", "request_id": id, "response": r})
                                    }
                                    Err(e) => {
                                        json!({"subtype": "error", "request_id": id, "error": e})
                                    }
                                };
                                let _ = write_line(
                                    &stdin,
                                    &json!({"type": "control_response", "response": response}),
                                )
                                .await;
                            });
                        }
                        Some("control_cancel_request") | Some("keep_alive") => {}
                        _ => {
                            let _ = tx.send(msg);
                        }
                    }
                }
                drop(tx);
                let code = status_rx.await.ok().flatten();
                let stderr = tail.lock().unwrap().clone();
                let exit = Exit { code, stderr };
                let error = exit
                    .error()
                    .unwrap_or_else(|| "Claude Code session has stopped".into());
                for (_, w) in controls.lock().unwrap().drain() {
                    let _ = w.send(Err(error.clone()));
                }
                let _ = exit_tx.send(Some(exit));
            });
        }

        Ok(Process {
            stdin,
            controls,
            messages: tokio::sync::Mutex::new(rx),
            exit: exit_rx,
            kill: Mutex::new(Some(kill_tx)),
            pid,
        })
    }

    /// The child's process id.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Send a message (a user message).
    pub async fn send(&self, msg: &Value) -> Result<(), String> {
        write_line(&self.stdin, msg).await
    }

    /// Send a control request and wait for its answer.
    pub async fn request(&self, request: Value) -> Result<Value, String> {
        let id = random_id();
        let (tx, rx) = oneshot::channel();
        if let Some(e) = self.exit.borrow().as_ref() {
            return Err(e
                .error()
                .unwrap_or_else(|| "Claude Code session has stopped".into()));
        }
        self.controls.lock().unwrap().insert(id.clone(), tx);
        let msg = json!({"request_id": id, "type": "control_request", "request": request});
        if let Err(e) = self.send(&msg).await {
            self.controls.lock().unwrap().remove(&id);
            return Err(e);
        }
        rx.await
            .unwrap_or_else(|_| Err("Claude Code session has stopped".into()))
    }

    /// The next message, or `None` once the output ended.
    pub async fn recv(&self) -> Option<Value> {
        self.messages.lock().await.recv().await
    }

    /// Close the input, which asks `claude` to finish and exit.
    pub async fn close_input(&self) {
        self.stdin.lock().await.take();
    }

    /// Kill the process group.
    pub fn kill(&self) {
        if let Some(k) = self.kill.lock().unwrap().take() {
            let _ = k.send(());
        }
    }

    /// Wait until the process exited and its output was read.
    pub async fn exited(&self) -> Exit {
        let mut rx = self.exit.clone();
        loop {
            if let Some(e) = rx.borrow().clone() {
                return e;
            }
            if rx.changed().await.is_err() {
                return Exit {
                    code: None,
                    stderr: String::new(),
                };
            }
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The child, whose process group is killed if it is dropped still
/// running (the runtime shutting down).
struct Owned {
    child: tokio::process::Child,
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl Owned {
    /// Kill the group, while the child has not been reaped (so its group
    /// id cannot have been reused).
    fn kill_group(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        #[cfg(unix)]
        if let Some(p) = self.pgid {
            // SAFETY: signalling the group this unreaped child leads.
            unsafe { libc::killpg(p, libc::SIGKILL) };
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        self.kill_group();
    }
}
