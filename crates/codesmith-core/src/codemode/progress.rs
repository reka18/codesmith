//! The rows an `execute` call reports while its program runs (OpenCode's
//! `codemode/tool.ts` progress): one row per tool call the program makes,
//! in start order, `running` until it settles as `completed` or `error`,
//! sent as `{"toolCalls": [...]}`. Each update replaces the last, so at
//! most one goes out per 100 ms and a trailing one carries the latest rows.
//! A program that ends well drops a pending update (its result carries the
//! final rows); one that fails, or is stopped, sends it first.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// How often updates may go out.
pub const INTERVAL: Duration = Duration::from_millis(100);

type Send = Arc<dyn Fn(Value) + std::marker::Send + Sync>;

struct State {
    rows: Vec<Value>,
    published: Option<Instant>,
    trailing: Option<tokio::task::JoinHandle<()>>,
    finished: bool,
}

/// One `execute` call's rows (see the module docs).
pub struct Progress {
    state: Arc<Mutex<State>>,
    send: Send,
}

fn publish(state: &mut State, send: &Send) {
    state.trailing = None;
    state.published = Some(Instant::now());
    send(json!({"toolCalls": state.rows}));
}

impl Progress {
    /// Rows sent with `send`.
    pub fn new(send: impl Fn(Value) + std::marker::Send + Sync + 'static) -> Progress {
        Progress {
            state: Arc::new(Mutex::new(State {
                rows: vec![],
                published: None,
                trailing: None,
                finished: false,
            })),
            send: Arc::new(send),
        }
    }

    fn record(&self, update: impl FnOnce(&mut Vec<Value>)) {
        let mut s = self.state.lock().unwrap();
        update(&mut s.rows);
        if s.trailing.is_some() || s.finished {
            return;
        }
        let now = Instant::now();
        let due = s.published.map(|p| p + INTERVAL).filter(|d| *d > now);
        let Some(due) = due else {
            return publish(&mut s, &self.send);
        };
        let (state, send) = (self.state.clone(), self.send.clone());
        s.trailing = Some(tokio::spawn(async move {
            tokio::time::sleep_until(due).await;
            let mut s = state.lock().unwrap();
            if !s.finished {
                publish(&mut s, &send);
            }
        }));
    }

    /// A call starts: its row, `running`, with the input shown.
    pub fn start(&self, tool: &str, input: Option<&Value>) -> usize {
        let mut index = 0;
        self.record(|rows| {
            index = rows.len();
            let mut row = json!({"tool": tool, "status": "running"});
            if let Some(i) = input.filter(|i| i.as_object().is_some_and(|o| !o.is_empty())) {
                row["input"] = i.clone();
            }
            rows.push(row);
        });
        index
    }

    /// A call settles.
    pub fn settle(&self, index: usize, ok: bool) {
        self.record(|rows| {
            if let Some(r) = rows.get_mut(index) {
                r["status"] = json!(if ok { "completed" } else { "error" });
            }
        });
    }

    /// The rows so far, for the result's metadata.
    pub fn rows(&self) -> Vec<Value> {
        self.state.lock().unwrap().rows.clone()
    }

    /// The program ended: a pending update is dropped when it succeeded,
    /// sent when it failed.
    pub fn finish(&self, ok: bool) {
        let mut s = self.state.lock().unwrap();
        if s.finished {
            return;
        }
        let pending = s.trailing.take();
        if let Some(t) = &pending {
            t.abort();
        }
        if !ok && pending.is_some() {
            publish(&mut s, &self.send);
        }
        s.finished = true;
    }
}

impl Drop for Progress {
    /// Stopped before it ended: calls still running settle as errors, and
    /// the latest rows go out.
    fn drop(&mut self) {
        let mut s = self.state.lock().unwrap();
        if s.finished {
            return;
        }
        let pending = s.trailing.take();
        if let Some(t) = &pending {
            t.abort();
        }
        let mut changed = false;
        for r in s.rows.iter_mut().filter(|r| r["status"] == "running") {
            r["status"] = json!("error");
            changed = true;
        }
        if changed || pending.is_some() {
            publish(&mut s, &self.send);
        }
        s.finished = true;
    }
}
