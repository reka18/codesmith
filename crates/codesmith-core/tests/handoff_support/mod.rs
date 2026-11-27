//! Shared fixtures for the automation handoff tests: a scripted model that
//! can hold a turn, a counting MCP server, and helpers to stop a runner at
//! a phase and start another on the same database.
#![allow(dead_code)]

use codesmith_ai::{Part, Role};
use codesmith_core::automation::{self as au, Action, Runner, Target, Trigger};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::store::{Info, Store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long any wait in these tests may take, over the whole wait: far
/// more than any of them needs (well under a second alone), so a loaded
/// machine running the whole suite does not fail them. The one place to
/// change it.
pub const DEADLINE: Duration = Duration::from_secs(180);

/// A model on a local port. Its answer depends on the last message: `call
/// <tool> <json>` calls the tool, `use shell` calls the shell, `fail this
/// turn` fails, a tool result is answered with "tool said: …", anything
/// else with "answer to: …". A prompt with `hold` in it waits while the
/// gate is closed.
pub struct Model {
    pub base: String,
    seen: Arc<Mutex<Vec<String>>>,
    gate: tokio::sync::watch::Sender<bool>,
}

impl Model {
    pub async fn start() -> Model {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let (gate, open) = tokio::sync::watch::channel(true);
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let log = log.clone();
                let mut open = open.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    let body = loop {
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some(i) = text.find("\r\n\r\n") {
                            let len: usize = text[..i]
                                .lines()
                                .find_map(|l| {
                                    l.to_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            if buf.len() >= i + 4 + len {
                                break serde_json::from_slice::<Value>(&buf[i + 4..i + 4 + len])
                                    .unwrap();
                            }
                        }
                    };
                    // Title requests have no tools; they are not counted.
                    if body.get("tools").is_none() {
                        let chunk = json!({"choices": [{"delta": {"content": "T"}, "finish_reason": "stop"}]});
                        return respond(&mut s, &chunk).await;
                    }
                    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
                    let content = last["content"].as_str().unwrap_or("").to_string();
                    log.lock().unwrap().push(content.clone());
                    if content.contains("hold") && last["role"] == "user" {
                        while !*open.borrow_and_update() {
                            if open.changed().await.is_err() {
                                return;
                            }
                        }
                    }
                    if content.contains("fail this turn") {
                        let err = r#"{"error": {"message": "bad request", "type": "invalid_request_error"}}"#;
                        let _ = s.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{err}", err.len()).as_bytes()).await;
                        return;
                    }
                    let chunk = if let Some(rest) = content
                        .find("call ")
                        .map(|i| &content[i + 5..])
                        .filter(|_| last["role"] == "user")
                    {
                        let (name, args) = rest.split_once(' ').unwrap_or((rest, "{}"));
                        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": name, "arguments": args}}]}, "finish_reason": "tool_calls"}]})
                    } else if last["role"] == "user" && content.contains("use shell") {
                        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{\"command\": \"echo hi\", \"description\": \"x\"}"}}]}, "finish_reason": "tool_calls"}]})
                    } else if last["role"] == "tool" {
                        json!({"choices": [{"delta": {"content": format!("tool said: {content}")}, "finish_reason": "stop"}]})
                    } else {
                        json!({"choices": [{"delta": {"content": format!("answer to: {content}")}, "finish_reason": "stop"}]})
                    };
                    respond(&mut s, &chunk).await;
                });
            }
        });
        Model {
            base: format!("http://{addr}/v1"),
            seen,
            gate,
        }
    }

    /// Hold every `hold` prompt until [`Model::release`].
    pub fn hold(&self) {
        self.gate.send_replace(false);
    }

    pub fn release(&self) {
        self.gate.send_replace(true);
    }

    /// How many model requests ended with a message containing `needle`.
    pub fn requests(&self, needle: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.contains(needle))
            .count()
    }
}

async fn respond(s: &mut tokio::net::TcpStream, chunk: &Value) {
    let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes()).await;
}

pub fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// One test's world: a project folder, a session database in another
/// folder, the model, and the counting MCP server's log.
pub struct World {
    pub project: tempfile::TempDir,
    pub data: tempfile::TempDir,
    pub dbp: PathBuf,
    pub db: Store,
    pub model: Model,
    pub py: &'static str,
    pub posts: PathBuf,
    /// Config merged over the base for every load.
    pub extra: Value,
}

impl World {
    /// `None` without Python (the MCP fixtures need it).
    pub async fn new(extra: Value) -> Option<World> {
        let py = python()?;
        let project = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let dbp = data.path().join("s.db");
        let db = Store::open(&dbp).unwrap();
        let posts = data.path().join("posts.log");
        Some(World {
            project,
            data,
            dbp,
            db,
            model: Model::start().await,
            py,
            posts,
            extra,
        })
    }

    pub fn dir(&self) -> String {
        self.project.path().display().to_string()
    }

    pub fn loader(&self) -> au::Loader {
        let counter = format!(
            "{}/tests/fixtures/mcp_counter.py",
            env!("CARGO_MANIFEST_DIR")
        );
        let rich = format!("{}/tests/fixtures/mcp_rich.py", env!("CARGO_MANIFEST_DIR"));
        let mut config = json!({
            "model": "fake/m", "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": self.model.base, "apiKey": "x"}}},
            "mcp": {"servers": {
                "counter": {"type": "local", "command": [self.py, counter, self.posts.display().to_string()]},
                "rich": {"type": "local", "command": [self.py, rich]}
            }},
            "agents": {
                "helper": {"mode": "subagent", "description": "Helps", "permissions": [{"action": "edit", "resource": "*", "effect": "deny"}]},
                "internal": {"mode": "subagent", "hidden": true, "description": "Hidden", "permissions": [{"action": "edit", "resource": "*", "effect": "deny"}]}
            },
            "automation": {"permissions": [
                {"action": "counter_post", "resource": "*", "effect": "allow"},
                {"action": "counter_wait", "resource": "*", "effect": "allow"}
            ]}
        });
        codesmith_core::config::merge(&mut config, self.extra.clone());
        Arc::new(move |dir: &Path| {
            let config: Config = serde_json::from_value(config.clone())?;
            Ok(Loaded {
                config,
                files: vec![],
                root: dir.to_path_buf(),
                ..Default::default()
            })
        })
    }

    pub fn runner(&self) -> Runner {
        Runner::new(self.dbp.clone(), self.loader())
    }

    /// A stored session to create automations from, with these own rules.
    pub fn creator(&self, rules: Value) -> String {
        self.session(None, rules)
    }

    pub fn session(&self, parent: Option<&str>, rules: Value) -> String {
        self.session_with(&format!("ses_creator{}", rand_suffix()), parent, rules)
    }

    pub fn session_with(&self, id: &str, parent: Option<&str>, rules: Value) -> String {
        let id = id.to_string();
        let t = codesmith_core::store::now();
        self.db
            .create(&Info {
                id: id.clone(),
                directory: self.dir(),
                title: Some("creator".into()),
                agent: "build".into(),
                model: "fake/m".into(),
                parent_id: parent.map(str::to_string),
                created: t,
                updated: t,
                usage: Default::default(),
            })
            .unwrap();
        let rules: Vec<codesmith_core::permission::Rule> = serde_json::from_value(rules).unwrap();
        self.db.set_permissions(&id, &rules).unwrap();
        id
    }

    pub fn create(&self, session: Option<&str>, trigger: Trigger, action: Action) -> String {
        au::create(
            &self.db,
            au::New {
                name: "handoff".into(),
                directory: self.dir(),
                session_id: session.map(str::to_string),
                trigger,
                action,
            },
        )
        .unwrap()
        .id
    }

    pub fn code(&self, session: Option<&str>, code: &str, args: Option<Value>) -> String {
        self.create(
            session,
            hourly(),
            Action::Code {
                code: code.into(),
                args,
            },
        )
    }

    pub fn prompt(&self, session: Option<&str>, prompt: &str, to: Target) -> String {
        self.create(
            session,
            hourly(),
            Action::Prompt {
                prompt: prompt.into(),
                to,
            },
        )
    }

    pub fn get(&self, id: &str) -> au::Automation {
        au::get(&self.db, id).unwrap().unwrap()
    }

    pub fn due_now(&self, id: &str) {
        self.sql(
            "UPDATE automation SET next_due = ?2 WHERE id = ?1",
            rusqlite::params![id, codesmith_core::store::now() - 1],
        );
    }

    /// Make every unsettled run look abandoned (as after its process
    /// stopped more than a turn lease ago).
    pub fn stale(&self) {
        self.sql(
            "UPDATE automation_run SET updated = 0 WHERE phase IN ('claimed', 'checked', 'admitted')",
            [],
        );
    }

    pub fn sql<P: rusqlite::Params>(&self, sql: &str, p: P) {
        self.db.with_conn(|c| Ok(c.execute(sql, p)?)).unwrap();
    }

    /// Lines the counter server's `post` wrote.
    pub fn posts(&self) -> Vec<String> {
        std::fs::read_to_string(&self.posts)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The phase of each run of `id`, finished ones included.
    pub fn phases(&self, id: &str) -> Vec<(u64, String)> {
        self.db
            .with_conn(|c| {
                let mut st = c.prepare(
                    "SELECT run, phase FROM automation_run WHERE automation_id = ?1 ORDER BY run",
                )?;
                let rows = st.query_map([id], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)))?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap()
    }

    pub fn phase(&self, id: &str, run: u64) -> Option<String> {
        self.phases(id)
            .into_iter()
            .find(|(r, _)| *r == run)
            .map(|(_, p)| p)
    }

    /// How many messages tagged `key` (an automation message id) there are
    /// across all sessions.
    pub fn tagged(&self, key: &str) -> i64 {
        self.db
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM record WHERE json_extract(body, '$.kind') = 'metadata'
                     AND json_extract(body, '$.metadata.automation.message') = ?1",
                    [key],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }

    /// The texts of session `id`, as `u:` and `a:` lines.
    pub fn texts(&self, id: &str) -> Vec<String> {
        texts(&self.db, id)
    }

    /// Sessions whose parent is `id`.
    pub fn children(&self, id: &str) -> Vec<String> {
        self.db
            .list(None, true)
            .unwrap()
            .into_iter()
            .filter(|i| i.parent_id.as_deref() == Some(id))
            .map(|i| i.id)
            .collect()
    }
}

pub fn texts(db: &Store, session: &str) -> Vec<String> {
    db.history(session)
        .unwrap_or_default()
        .iter()
        .flat_map(|m| {
            m.parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(format!(
                        "{}:{t}",
                        if m.role == Role::User { "u" } else { "a" }
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn hourly() -> Trigger {
    Trigger::Every {
        every: "1 hour".into(),
        tz: None,
    }
}

fn rand_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:x}{}",
        codesmith_core::store::now(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// Poll `done` until it holds, failing after [`DEADLINE`] over the whole wait.
pub async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + DEADLINE;
    while !done() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Tick `runner` until `done` holds, within [`DEADLINE`] over the whole wait.
pub async fn tick_until(runner: &Runner, what: &str, mut done: impl FnMut() -> bool) {
    let end = Instant::now() + DEADLINE;
    loop {
        runner.tick().await.unwrap();
        if done() {
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A runner ticking once in the background, to be stopped mid-run.
pub struct Running {
    pub holder: String,
    task: tokio::task::JoinHandle<()>,
}

pub fn spawn_tick(runner: Runner) -> Running {
    let holder = runner.holder.clone();
    Running {
        holder,
        task: tokio::spawn(async move {
            let _ = runner.tick().await;
        }),
    }
}

impl Running {
    /// Stop it as a killed process stops: its future (and the runner,
    /// with its sessions, leases and MCP servers) is dropped where it is.
    /// The scheduler lease it held is let go, as its expiry would.
    pub async fn kill(self, db: &Store) {
        self.task.abort();
        let _ = self.task.await;
        au::release(db, &self.holder).unwrap();
    }

    /// Let it finish, then exit as a process does, letting the lease go.
    pub async fn join(self, db: &Store) {
        self.task.await.unwrap();
        au::release(db, &self.holder).unwrap();
    }
}
