//! Ports of OpenCode's event stream recovery tests
//! (`automation-event.test.ts`): a broken MCP event stream reopens on a new
//! connection from the last committed cursor, and a watch pauses after three
//! reconnects in a row that bring nothing.
//!
//! The `rich` fixture follows a plan file per connection (see
//! `fixtures/mcp_rich.py`); each reconnect waits 1, 2, then 4 seconds, as the
//! scheduler does.

use codesmith_core::automation::{self as au, Action, Runner, Target, Trigger};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A model that answers every prompt with text.
async fn model() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
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
                            break;
                        }
                    }
                }
                let chunk = json!({"choices": [{"delta": {"content": "handled"}, "finish_reason": "stop"}]});
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn loader(base: String, py: &'static str) -> au::Loader {
    let fixture = format!("{}/tests/fixtures/mcp_rich.py", env!("CARGO_MANIFEST_DIR"));
    Arc::new(move |dir: &Path| {
        let config: Config = serde_json::from_value(json!({
            "model": "fake/m", "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
            "mcp": {"servers": {"rich": {"type": "local", "command": [py, fixture]}}}
        }))?;
        Ok(Loaded {
            config,
            files: vec![],
            root: dir.to_path_buf(),
        })
    })
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: Store,
    runner: Runner,
    plan: PathBuf,
    id: String,
}

/// An event watch on the `rich` server following `plan`.
async fn watch(plan: Value) -> Option<Fixture> {
    let py = python()?;
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let plan_file = dir.path().join("plan.json");
    std::fs::write(&plan_file, plan.to_string()).unwrap();
    let runner = Runner::new(dbp, loader(model().await, py));
    let a = au::create(
        &db,
        au::New {
            name: "tickets".into(),
            directory: dir.path().display().to_string(),
            session_id: None,
            trigger: Trigger::Event {
                server: "rich".into(),
                event: "ticket.created".into(),
                args: Some(json!({"queue": "plan", "plan": plan_file.display().to_string()})),
            },
            action: Action::Prompt {
                prompt: "triage the ticket".into(),
                to: Target::Subagent {
                    subagent: "build".into(),
                },
            },
        },
    )
    .unwrap();
    Some(Fixture {
        _dir: dir,
        db,
        runner,
        plan: plan_file,
        id: a.id,
    })
}

impl Fixture {
    fn automation(&self) -> au::Automation {
        au::get(&self.db, &self.id).unwrap().unwrap()
    }
    /// The resume cursor of each stream opened so far.
    fn cursors(&self) -> Vec<Value> {
        let log = self.plan.with_extension("json.log");
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    async fn until(&self, what: &str, mut done: impl FnMut(&Fixture) -> bool) {
        for _ in 0..300 {
            self.runner.tick().await.unwrap();
            if done(self) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!(
            "timed out waiting for {what}: {:?} {:?}",
            self.automation(),
            self.cursors()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_a_broken_stream_opens_on_the_replacement_connection_replaying_from_the_last_committed_event(
) {
    let Some(f) = watch(json!([
        {"events": [["p1", "c1", "first"]], "exit": true},
        {"events": [["p1", "c1", "first"], ["t2", "c2", "second"]]}
    ]))
    .await
    else {
        return;
    };
    f.until("two runs", |f| f.automation().runs == 2).await;
    assert_eq!(f.cursors(), [Value::Null, json!("c1")]);
    let a = f.automation();
    assert_eq!(a.status, "active");
    let pos = a.event.unwrap();
    assert_eq!(
        (pos.cursor.as_deref(), pos.id.as_deref()),
        (Some("c2"), Some("t2"))
    );
    // The replayed p1 did not run again.
    let runs = au::runs(&f.db, &f.id).unwrap();
    assert_eq!(runs.len(), 2, "{runs:?}");
    au::remove(&f.db, &f.id).unwrap();
    f.runner.tick().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_reopens_after_four_healthy_streams_disconnect_each_from_its_last_committed_cursor(
) {
    let Some(f) = watch(json!([
        {"events": [["p1", "c1", "one"]], "exit": true},
        {"events": [["p2", "c2", "two"]], "exit": true},
        {"events": [["p3", "c3", "three"]], "exit": true},
        {"events": [["p4", "c4", "four"]], "exit": true},
        {"events": [["final", "c5", "five"]]}
    ]))
    .await
    else {
        return;
    };
    f.until("five runs", |f| f.automation().runs == 5).await;
    assert_eq!(
        f.cursors(),
        [
            Value::Null,
            json!("c1"),
            json!("c2"),
            json!("c3"),
            json!("c4")
        ]
    );
    assert_eq!(f.automation().status, "active");
    au::remove(&f.db, &f.id).unwrap();
    f.runner.tick().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_pauses_after_three_consecutive_reconnects_without_confirmed_stream_output(
) {
    let Some(f) = watch(json!([
        {"events": [["saved", "c1", "kept"]], "exit": true},
        {"exit": true},
        {"exit": true},
        {"exit": true},
        {"events": [["late", "c2", "never"]]}
    ]))
    .await
    else {
        return;
    };
    f.until("the pause", |f| f.automation().status == "paused")
        .await;
    let a = f.automation();
    assert_eq!(a.runs, 1);
    assert!(
        a.last_error
            .as_deref()
            .is_some_and(|e| e.starts_with("Paused: ")),
        "{a:?}"
    );
    assert_eq!(
        f.cursors(),
        [Value::Null, json!("c1"), json!("c1"), json!("c1")],
        "the first stream and three reconnects"
    );
}
