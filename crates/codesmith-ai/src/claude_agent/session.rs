//! One chat's `claude` and its turns (OpenCode's `ClaudeSession`).
//!
//! A Claude turn can outlive a CodeSmith step: at a tool call the step
//! ends while the turn waits for the results, which the next step hands
//! over. One reader takes every message, so a cancelled turn still drains
//! to its result before another starts.

use super::probe::{version_error, Probe};
use super::process::{Handler, Process, Spawn};
use super::query::Launch;
use super::tools;
use crate::ToolSpec;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::{watch, Notify, OnceCell};

#[derive(Default)]
struct Turn {
    messages: VecDeque<Value>,
    finished: bool,
}

#[derive(Default)]
struct State {
    turn: Option<Turn>,
    ended: bool,
    failure: Option<String>,
    initialized: bool,
    persisted: bool,
    stopping: bool,
}

/// What a step ended with.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// The reply called CodeSmith's tools; the turn waits for their results.
    Tools,
    /// The turn's result.
    Result(Value),
}

pub(crate) struct Session {
    pub id: String,
    process: Arc<Process>,
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    ready: watch::Receiver<Option<Result<Probe, String>>>,
    stopped: OnceCell<()>,
}

impl Session {
    pub fn start(launch: &Launch, handler: Arc<dyn Handler>) -> Result<Session, String> {
        let id = launch
            .resume
            .clone()
            .or_else(|| launch.session_id.clone())
            .unwrap_or_default();
        let process = Arc::new(Process::start(
            Spawn {
                executable: launch.executable.clone(),
                args: launch.args(),
                env: launch.environment(),
                cwd: launch.directory.clone(),
            },
            handler,
        )?);
        let state = Arc::new(Mutex::new(State::default()));
        let wake = Arc::new(Notify::new());
        let (ready_tx, ready) = watch::channel(None);
        {
            let process = process.clone();
            let state = state.clone();
            let init = launch.initialize();
            tokio::spawn(async move {
                let r = process.request(init).await.and_then(|init| {
                    let probe = Probe::from_init(&init);
                    if let Some(e) = version_error(probe.version.as_deref()) {
                        return Err(e);
                    }
                    Ok(probe)
                });
                if r.is_ok() {
                    state.lock().unwrap().initialized = true;
                }
                let _ = ready_tx.send(Some(r));
            });
        }
        // The only reader, cancelled turns included.
        {
            let process = process.clone();
            let state = state.clone();
            let wake = wake.clone();
            tokio::spawn(async move {
                while let Some(m) = process.recv().await {
                    let mut s = state.lock().unwrap();
                    let kind = m["type"].as_str().unwrap_or_default().to_string();
                    if kind == "assistant" || kind == "result" {
                        s.persisted = true;
                    }
                    if let Some(t) = s.turn.as_mut() {
                        t.messages.push_back(m);
                        if kind == "result" {
                            t.finished = true;
                        }
                    }
                    drop(s);
                    wake.notify_waiters();
                }
                let exit = process.exited().await;
                let mut s = state.lock().unwrap();
                s.ended = true;
                s.failure = exit.error();
                if let Some(t) = s.turn.as_mut() {
                    t.finished = true;
                }
                drop(s);
                wake.notify_waiters();
            });
        }
        Ok(Session {
            id,
            process,
            state,
            wake,
            ready,
            stopped: OnceCell::new(),
        })
    }

    /// The report the process started with.
    pub async fn ready(&self) -> Result<Probe, String> {
        let mut rx = self.ready.clone();
        loop {
            if let Some(r) = rx.borrow().clone() {
                return r;
            }
            if rx.changed().await.is_err() {
                return Err("Claude Code session has stopped".into());
            }
        }
    }

    pub fn ended(&self) -> bool {
        self.state.lock().unwrap().ended
    }

    /// Claude saved something of this session.
    pub fn persisted(&self) -> bool {
        self.state.lock().unwrap().persisted
    }

    /// A turn is still going (a parked tool turn among them).
    pub fn running(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .turn
            .as_ref()
            .is_some_and(|t| !t.finished)
    }

    fn stopped_error(s: &State) -> String {
        s.failure
            .clone()
            .unwrap_or_else(|| "Claude Code session has stopped".into())
    }

    fn user(content: Vec<Value>, uuid: &str, next: bool) -> Value {
        let mut m = json!({
            "type": "user",
            "uuid": uuid,
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": null,
            // Sent exactly as given: CodeSmith already expanded commands
            // and file references, and adds its own context.
            "client_composed": true,
        });
        if next {
            m["priority"] = json!("next");
        }
        m
    }

    /// Start a turn with this input.
    pub async fn begin(&self, content: Vec<Value>, uuid: &str) -> Result<(), String> {
        {
            let mut s = self.state.lock().unwrap();
            if s.ended || s.stopping {
                return Err(Self::stopped_error(&s));
            }
            if s.turn.as_ref().is_some_and(|t| !t.finished) {
                return Err("Claude Code is still finishing the previous turn".into());
            }
            s.turn = Some(Turn::default());
        }
        self.process.send(&Self::user(content, uuid, false)).await
    }

    /// Steer the running turn with more input, ahead of anything queued.
    pub async fn add_user(&self, content: Vec<Value>, uuid: &str) -> Result<(), String> {
        self.process.send(&Self::user(content, uuid, true)).await
    }

    /// Read the turn up to its next step boundary: the end of a reply that
    /// called CodeSmith's tools, or the turn's result. `emit` sees each
    /// message first and may fail the step.
    pub async fn read_step(
        &self,
        emit: &mut (dyn FnMut(&Value) -> Result<(), String> + Send),
        tools: &[ToolSpec],
    ) -> Result<Step, String> {
        let mut called = false;
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let next = {
                let mut s = self.state.lock().unwrap();
                let Some(turn) = s.turn.as_mut() else {
                    return Err("No Claude turn to read".into());
                };
                match turn.messages.pop_front() {
                    Some(m) => Some(m),
                    None if s.ended => {
                        return Err(s.failure.clone().unwrap_or_else(|| {
                            "Claude Code stopped before finishing its reply".into()
                        }))
                    }
                    None => None,
                }
            };
            let Some(m) = next else {
                notified.await;
                continue;
            };
            emit(&m)?;
            let top = m["parent_tool_use_id"].is_null();
            if m["type"] == "assistant" && top {
                for part in m["message"]["content"].as_array().into_iter().flatten() {
                    // Calls to tools that were not offered get Claude Code's
                    // own error, and the turn goes on without CodeSmith.
                    if part["type"] == "tool_use"
                        && tools::original(part["name"].as_str().unwrap_or_default(), tools)
                            .is_some()
                    {
                        called = true;
                    }
                }
            }
            let reply_ended =
                m["type"] == "stream_event" && top && m["event"]["type"] == "message_stop";
            if called && reply_ended {
                return Ok(Step::Tools);
            }
            if m["type"] == "result" {
                return Ok(Step::Result(m));
            }
        }
    }

    async fn drained(&self) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let s = self.state.lock().unwrap();
                if s.turn.as_ref().is_none_or(|t| t.finished) {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Interrupt the running turn and wait until it drained to its result,
    /// which comes after the interrupt's own answer.
    pub async fn interrupt(&self) -> Result<(), String> {
        {
            let s = self.state.lock().unwrap();
            if s.ended || s.turn.as_ref().is_none_or(|t| t.finished) {
                return Ok(());
            }
        }
        self.process
            .request(json!({"subtype": "interrupt"}))
            .await?;
        self.drained().await;
        Ok(())
    }

    /// Stop the process: interrupt and drain a running turn, close the
    /// input, and wait for it to exit (killing it if it does not).
    pub async fn stop(&self) {
        self.stopped
            .get_or_init(|| async {
                let (initialized, ended) = {
                    let mut s = self.state.lock().unwrap();
                    s.stopping = true;
                    (s.initialized, s.ended)
                };
                if initialized && !ended {
                    let _ = tokio::time::timeout(STOP_WAIT, async {
                        let _ = self.process.request(json!({"subtype": "interrupt"})).await;
                        self.drained().await;
                    })
                    .await;
                }
                self.process.close_input().await;
                // A process that never started has nothing to flush.
                if !initialized {
                    self.process.kill();
                }
                if tokio::time::timeout(STOP_WAIT, self.process.exited())
                    .await
                    .is_err()
                {
                    self.process.kill();
                    let _ = self.process.exited().await;
                }
            })
            .await;
    }

    pub async fn set_model(&self, model: &str) -> Result<(), String> {
        self.process
            .request(json!({"subtype": "set_model", "model": model}))
            .await
            .map(|_| ())
    }

    pub async fn set_effort(&self, effort: Option<&str>) -> Result<(), String> {
        self.process
            .request(json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": effort}}))
            .await
            .map(|_| ())
    }
}

/// How long stopping waits for Claude Code to drain and exit before
/// killing it.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
