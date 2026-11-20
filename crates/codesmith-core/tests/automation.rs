//! The automation scheduler end to end against a scripted model.

use codesmith_ai::{Part, Role};
use codesmith_core::automation::{self as au, Action, Runner, Target, Trigger};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn model() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let body = loop {
                    let n = s.read(&mut tmp).await.unwrap();
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
                let last = body["messages"].as_array().unwrap().last().unwrap().clone();
                let content = last["content"].as_str().unwrap_or("").to_string();
                let chunk = if body.get("tools").is_none() {
                    json!({"choices": [{"delta": {"content": "T"}, "finish_reason": "stop"}]})
                } else if last["role"] == "user" && content.contains("use shell") {
                    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "shell", "arguments": "{\"command\": \"echo hi\", \"description\": \"x\"}"}}]}, "finish_reason": "tool_calls"}]})
                } else if last["role"] == "tool" {
                    json!({"choices": [{"delta": {"content": format!("tool said: {content}")}, "finish_reason": "stop"}]})
                } else {
                    json!({"choices": [{"delta": {"content": format!("answer to: {content}")}, "finish_reason": "stop"}]})
                };
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

fn loader(base: String) -> au::Loader {
    Arc::new(move |dir: &Path| {
        let config: Config = serde_json::from_value(json!({
            "model": "fake/m", "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
            "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]
        }))?;
        Ok(Loaded {
            config,
            files: vec![],
            root: dir.to_path_buf(),
        })
    })
}

/// Make an automation due now.
fn due_now(db: &Store, id: &str) {
    db.with_conn(|c| {
        Ok(c.execute(
            "UPDATE automation SET next_due = ?2 WHERE id = ?1",
            rusqlite::params![id, codesmith_core::store::now() - 1],
        )?)
    })
    .unwrap();
}

fn texts(db: &Store, session: &str) -> Vec<String> {
    db.history(session)
        .unwrap()
        .iter()
        .flat_map(|m| {
            m.parts
                .iter()
                .filter_map(|p| {
                    if let Part::Text(t) = p {
                        Some(format!(
                            "{}:{t}",
                            if m.role == Role::User { "u" } else { "a" }
                        ))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduled_runs() {
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let base = model().await;
    let d = dir.path().display().to_string();
    let runner = Runner::new(dbp.clone(), loader(base.clone()));

    // A new session per run.
    let a = au::create(
        &db,
        au::New {
            name: "nightly".into(),
            directory: d.clone(),
            session_id: None,
            trigger: Trigger::Every {
                every: "1 day".into(),
                tz: None,
            },
            action: Action::Prompt {
                prompt: "check the build".into(),
                to: Target::Subagent {
                    subagent: "build".into(),
                },
            },
        },
    )
    .unwrap();
    assert_eq!(runner.tick().await.unwrap(), 0, "nothing is due yet");
    due_now(&db, &a.id);
    let other = Runner::new(dbp.clone(), loader(base.clone()));
    assert_eq!(runner.tick().await.unwrap(), 1);
    due_now(&db, &a.id);
    assert_eq!(
        other.tick().await.unwrap(),
        0,
        "the lease keeps a second scheduler from firing"
    );
    let run = &au::runs(&db, &a.id).unwrap()[0];
    assert_eq!(
        (run.outcome.as_str(), run.detail.as_str()),
        ("fired", "answer to: check the build")
    );
    let sid = run.session_id.clone().unwrap();
    assert_eq!(
        texts(&db, &sid),
        ["u:check the build", "a:answer to: check the build"]
    );
    assert_eq!(
        db.get(&sid).unwrap().unwrap().title.as_deref(),
        Some("nightly #1")
    );

    // Into an existing session, unattended: an approval-needing command is refused.
    let s = au::create(
        &db,
        au::New {
            name: "ping".into(),
            directory: d.clone(),
            session_id: None,
            trigger: Trigger::Every {
                every: "1 hour".into(),
                tz: None,
            },
            action: Action::Prompt {
                prompt: "use shell".into(),
                to: Target::Session {
                    session: sid.clone(),
                },
            },
        },
    )
    .unwrap();
    due_now(&db, &s.id);
    runner.tick().await.unwrap();
    let r = &au::runs(&db, &s.id).unwrap()[0];
    assert_eq!(r.session_id.as_deref(), Some(sid.as_str()));
    assert!(r.detail.contains("runs unattended"), "{}", r.detail);
    assert_eq!(
        texts(&db, &sid).len(),
        4,
        "the prompt and answer joined the existing session"
    );
    au::remove(&db, &s.id).unwrap();
    au::set_status(&db, &a.id, "paused").unwrap();

    // A Code Mode program with state that hands off a prompt and finishes itself.
    let code = "const count = ((input.state && input.state.count) || 0) + 1\nreturn { state: { count }, done: count >= 2, send: { prompt: 'summarize run ' + input.runs } }";
    let c = au::create(
        &db,
        au::New {
            name: "code".into(),
            directory: d.clone(),
            session_id: None,
            trigger: Trigger::Every {
                every: "1 hour".into(),
                tz: None,
            },
            action: Action::Code {
                code: code.into(),
                args: None,
            },
        },
    )
    .unwrap();
    due_now(&db, &c.id);
    runner.tick().await.unwrap();
    let c1 = au::get(&db, &c.id).unwrap().unwrap();
    assert_eq!(
        (c1.state.clone(), c1.status.as_str()),
        (json!({"count": 1}), "active")
    );
    let r1 = &au::runs(&db, &c.id).unwrap()[0];
    assert!(
        r1.detail.contains("answer to: summarize run 1"),
        "{}",
        r1.detail
    );
    due_now(&db, &c.id);
    runner.tick().await.unwrap();
    let c2 = au::get(&db, &c.id).unwrap().unwrap();
    assert_eq!(
        (c2.state.clone(), c2.status.as_str(), c2.next_due),
        (json!({"count": 2}), "done", None),
        "done: true finishes it"
    );

    // A failing program is recorded as failed.
    let f = au::create(
        &db,
        au::New {
            name: "bad".into(),
            directory: d,
            session_id: None,
            trigger: Trigger::Every {
                every: "1 hour".into(),
                tz: None,
            },
            action: Action::Code {
                code: "throw new Error('nope')".into(),
                args: None,
            },
        },
    )
    .unwrap();
    due_now(&db, &f.id);
    runner.tick().await.unwrap();
    let fr = au::get(&db, &f.id).unwrap().unwrap();
    assert_eq!(fr.last_outcome.as_deref(), Some("failed"));
    let le = fr.last_error.unwrap();
    assert!(le.contains("nope"), "{le}");
}
