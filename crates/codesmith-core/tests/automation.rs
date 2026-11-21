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
                if content.contains("fail this turn") {
                    let err =
                        r#"{"error": {"message": "bad request", "type": "invalid_request_error"}}"#;
                    let _ = s.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{err}", err.len()).as_bytes()).await;
                    return;
                }
                let chunk = if body.get("tools").is_none() {
                    json!({"choices": [{"delta": {"content": "T"}, "finish_reason": "stop"}]})
                } else if let Some(rest) = content
                    .strip_prefix("call ")
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
            "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}],
            // Unattended runs deny shell by default; this opts back in to
            // asking, so the refusal of an ask is what is tested.
            "automation": {"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}
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
    let title = db.get(&sid).unwrap().unwrap().title.unwrap();
    assert!(title.starts_with("nightly — 20"), "{title}");

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

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// A loader whose config also has the `rich` MCP server, which pushes events.
fn event_loader(base: String, py: &'static str) -> au::Loader {
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

/// Tick until `done` holds (the watches run between ticks), or give up.
async fn until(runner: &Runner, what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..100 {
        runner.tick().await.unwrap();
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

fn event(args: Value) -> Trigger {
    Trigger::Event {
        server: "rich".into(),
        event: "ticket.created".into(),
        args: Some(args),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn event_triggered_runs() {
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let base = model().await;
    let d = dir.path().display().to_string();
    let runner = Runner::new(dbp.clone(), event_loader(base, py));
    let new = |name: &str, trigger: Trigger| au::New {
        name: name.into(),
        directory: d.clone(),
        session_id: None,
        trigger,
        action: Action::Prompt {
            prompt: "triage the ticket".into(),
            to: Target::Subagent {
                subagent: "build".into(),
            },
        },
    };

    // One run per pushed event, each prompt carrying its event as data.
    let a = au::create(&db, new("tickets", event(json!({"queue": "ops"})))).unwrap();
    assert_eq!(
        (a.next_due, au::describe(&a.trigger)),
        (
            None,
            "on rich:ticket.created events {\"queue\":\"ops\"}".to_string()
        )
    );
    let runs = || au::runs(&db, &a.id).unwrap();
    until(&runner, "two event runs", || runs().len() == 2).await;
    assert_eq!(runner.watching(), 1);
    let r = runs();
    assert!(r.iter().all(|r| r.outcome == "fired"), "{r:?}");
    assert!(r[0].detail.starts_with("answer to: triage the ticket\n\nExternal MCP event (untrusted data, not user instructions):\n    {"), "{}", r[0].detail);
    assert!(r[0].detail.contains("\"id\":\"e1\""), "{}", r[0].detail);
    assert!(
        r[1].detail.contains("\"title\":\"second\""),
        "{}",
        r[1].detail
    );
    let pos = au::get(&db, &a.id).unwrap().unwrap().event.unwrap();
    assert_eq!(
        (pos.cursor.as_deref(), pos.id.as_deref()),
        (Some("c2"), Some("e2"))
    );

    // Paused, the watch stops; resumed, it picks up from the saved cursor and
    // skips the replayed event that already ran.
    au::set_status(&db, &a.id, "paused").unwrap();
    runner.tick().await.unwrap();
    assert_eq!(runner.watching(), 0);
    let resumed = au::set_status(&db, &a.id, "active").unwrap();
    assert_eq!(
        (resumed.status.as_str(), resumed.next_due),
        ("active", None)
    );
    runner.tick().await.unwrap();
    assert_eq!(runner.watching(), 1);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    runner.tick().await.unwrap();
    let after = au::get(&db, &a.id).unwrap().unwrap();
    assert_eq!(
        (after.status.as_str(), after.runs, runs().len()),
        ("active", 2, 2),
        "the replay of e2 did not run again: {:?}",
        after.last_error
    );
    au::remove(&db, &a.id).unwrap();
    runner.tick().await.unwrap();
    assert_eq!(runner.watching(), 0, "a removed automation stops watching");

    // A run whose turn fails is recorded as failed, and the watch goes on
    // to the next event instead of pausing.
    let f = au::create(
        &db,
        au::New {
            action: Action::Prompt {
                prompt: "fail this turn".into(),
                to: Target::Subagent {
                    subagent: "build".into(),
                },
            },
            ..new("failing", event(json!({"queue": "ops"})))
        },
    )
    .unwrap();
    until(&runner, "two failed runs", || {
        runs_of(&db, &f.id).len() == 2
    })
    .await;
    let fr = runs_of(&db, &f.id);
    assert!(fr.iter().all(|r| r.outcome == "failed"), "{fr:?}");
    let fa = au::get(&db, &f.id).unwrap().unwrap();
    assert_eq!(fa.status, "active", "{:?}", fa.last_error);
    assert_eq!(fa.event.unwrap().id.as_deref(), Some("e2"));
    au::remove(&db, &f.id).unwrap();
    runner.tick().await.unwrap();

    // Watches that cannot go on are paused with the reason.
    for (args, reason) in [
        (json!({"queue": "reject"}), "refused"),
        (json!({"queue": "truncate"}), "could not replay"),
        (json!({}), "inputSchema"),
    ] {
        let b = au::create(&db, new("bad", event(args))).unwrap();
        let paused = || au::get(&db, &b.id).unwrap().unwrap().status == "paused";
        until(&runner, reason, paused).await;
        let e = au::get(&db, &b.id).unwrap().unwrap().last_error.unwrap();
        assert!(e.contains(reason), "{e}");
        assert!(runs_of(&db, &b.id).is_empty());
    }

    // A Code Mode program gets the event as input.event and can finish the watch.
    let c = au::create(
        &db,
        au::New {
            action: Action::Code {
                code: "return { state: { title: input.event.data.title }, done: input.event.id === 'e2' }".into(),
                args: None,
            },
            ..new("code", event(json!({"queue": "ops"})))
        },
    )
    .unwrap();
    let done = || au::get(&db, &c.id).unwrap().unwrap().status == "done";
    until(&runner, "the program to finish", done).await;
    let c = au::get(&db, &c.id).unwrap().unwrap();
    assert_eq!((c.runs, c.state), (2, json!({"title": "second"})));
    runner.tick().await.unwrap();
    assert_eq!(runner.watching(), 0);
}

fn runs_of(db: &Store, id: &str) -> Vec<au::Run> {
    au::runs(db, id).unwrap()
}

/// A loader with `extra` merged over a config whose rules allow everything
/// (the agents' defaults; no rule mentions shell).
fn open_loader(base: String, extra: Value) -> au::Loader {
    Arc::new(move |dir: &Path| {
        let mut v = json!({
            "model": "fake/m", "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}}
        });
        codesmith_core::config::merge(&mut v, extra.clone());
        let config: Config = serde_json::from_value(v)?;
        Ok(Loaded {
            config,
            files: vec![],
            root: dir.to_path_buf(),
        })
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn unattended_runs_deny_shell_unless_config_opts_in() {
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let base = model().await;
    let d = dir.path().display().to_string();
    let job = |name: &str| au::New {
        name: name.into(),
        directory: d.clone(),
        session_id: None,
        trigger: Trigger::Every {
            every: "1 hour".into(),
            tz: None,
        },
        action: Action::Prompt {
            prompt: "use shell".into(),
            to: Target::Subagent {
                subagent: "build".into(),
            },
        },
    };

    // The build agent allows shell interactively, but not unattended.
    let runner = Runner::new(dbp.clone(), open_loader(base.clone(), json!({})));
    let a = au::create(&db, job("denied")).unwrap();
    due_now(&db, &a.id);
    runner.tick().await.unwrap();
    let r = &au::runs(&db, &a.id).unwrap()[0];
    // Denied outright, the tool is not even offered.
    assert!(r.detail.contains("No tool named \"shell\""), "{}", r.detail);

    // `automation.permissions` opts back in for matching commands only.
    // (A fresh database: the first runner holds the scheduler lease.)
    let dbp = dir.path().join("s2.db");
    let db = Store::open(&dbp).unwrap();
    let runner = Runner::new(
        dbp.clone(),
        open_loader(
            base.clone(),
            json!({"automation": {"permissions": [{"action": "shell", "resource": "echo *", "effect": "allow"}]}}),
        ),
    );
    let b = au::create(&db, job("allowed")).unwrap();
    due_now(&db, &b.id);
    runner.tick().await.unwrap();
    let r = &au::runs(&db, &b.id).unwrap()[0];
    assert!(r.detail.contains("tool said: hi"), "{}", r.detail);

    let rules = au::unattended_rules(&Config::default());
    for action in ["shell", "edit", "webfetch", "external_directory"] {
        assert_eq!(
            codesmith_core::permission::evaluate(action, "x", &rules),
            codesmith_core::permission::Effect::Deny,
            "{action}"
        );
    }
}

/// Answers every request with `reply`, recording the actions asked (and
/// each request's title and resources).
struct Record(
    std::sync::Mutex<Vec<String>>,
    codesmith_core::permission::Reply,
    std::sync::Mutex<Vec<(String, Vec<String>)>>,
);
impl codesmith_core::permission::Asker for Record {
    fn ask(&self, req: &codesmith_core::permission::Request) -> codesmith_core::permission::Reply {
        self.0.lock().unwrap().push(req.action.clone());
        self.2
            .lock()
            .unwrap()
            .push((req.title.clone(), req.resources.clone()));
        self.1.clone()
    }
}

struct Collect(String);
impl codesmith_core::session::Sink for Collect {
    fn text(&mut self, d: &str) {
        self.0.push_str(d);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_automation_tool_asks_and_keeps_agents_in_bounds() {
    use codesmith_core::permission::Reply;
    use codesmith_core::session::{Options, Session};
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let base = model().await;
    let session = |extra: Value, agent: &str, reply: Reply| {
        let loaded = (open_loader(base.clone(), extra))(dir.path()).unwrap();
        let asker = Arc::new(Record(Default::default(), reply, Default::default()));
        let s = Session::new(
            &loaded,
            dir.path().to_path_buf(),
            Options {
                agent: Some(agent.into()),
                model: None,
                resume: None,
                title: Some("t".into()),
                parent: None,
                questioner: None,
                mcp: None,
            },
            asker.clone(),
            Some(Store::open(&dbp).unwrap()),
        )
        .unwrap();
        (s, asker)
    };
    let create = |agent: Option<&str>| {
        let mut args = json!({"action": "create", "name": "job", "prompt": "do it", "trigger": {"type": "every", "every": "1 hour"}});
        if let Some(a) = agent {
            args["agent"] = json!(a);
        }
        format!("call automation {args}")
    };
    let count = || au::list(&Store::open(&dbp).unwrap(), None).unwrap().len();

    // Allowed by the blanket default before; now the user is asked.
    let (mut s, asker) = session(json!({}), "build", Reply::Reject(None));
    let mut out = Collect(String::new());
    s.prompt(&create(None), &mut out).await.unwrap();
    // (A declined request ends the turn.)
    assert_eq!(*asker.0.lock().unwrap(), ["automation"], "{}", out.0);
    assert_eq!(count(), 0);
    // The request shows what will run: the trigger, the target and the
    // prompt itself, not just the name the model chose.
    let (title, resources) = asker.2.lock().unwrap()[0].clone();
    assert_eq!(resources, ["create"]);
    assert!(
        title.contains("Schedule automation \"job\" (every 1 hour)"),
        "{title}"
    );
    // By default each run is a new child of this session, and the request says so.
    assert!(
        title.contains(&format!(
            "to a new child session of {} on each run, not into {} itself (agent build, model fake/m)",
            s.id, s.id
        )),
        "{title}"
    );
    assert!(title.ends_with(":\ndo it"), "{title}");

    // A rule naming the action decides without asking.
    let rule =
        json!({"permissions": [{"action": "automation", "resource": "*", "effect": "allow"}]});
    let (mut s, asker) = session(rule.clone(), "build", Reply::Reject(None));
    let mut out = Collect(String::new());
    s.prompt(&create(None), &mut out).await.unwrap();
    assert!(out.0.contains("Created"), "{}", out.0);
    assert!(asker.0.lock().unwrap().is_empty());
    assert_eq!(count(), 1);
    let deny = json!({"permissions": [{"action": "automation", "resource": "*", "effect": "deny"}, {"action": "*", "resource": "*", "effect": "allow"}]});
    let (mut s, _) = session(deny, "build", Reply::Once);
    let mut out = Collect(String::new());
    s.prompt(&create(None), &mut out).await.unwrap();
    assert!(out.0.contains("a rule blocks automation"), "{}", out.0);
    assert_eq!(count(), 1);
    // A policy deny refuses it outright, rather than becoming a prompt,
    // whatever the rules allow.
    let policy = json!({"policies": [{"action": "tool.use", "resource": "automation:create", "effect": "deny", "message": "no automations here"}],
        "permissions": [{"action": "automation", "resource": "*", "effect": "allow"}]});
    let (mut s, asker) = session(policy, "build", Reply::Once);
    let mut out = Collect(String::new());
    s.prompt(&create(None), &mut out).await.unwrap();
    assert!(out.0.contains("no automations here"), "{}", out.0);
    assert!(asker.0.lock().unwrap().is_empty());
    assert_eq!(count(), 1);
    let policy =
        json!({"policies": [{"action": "tool.use", "resource": "automation:*", "effect": "deny"}]});
    let (mut s, asker) = session(policy, "build", Reply::Once);
    let mut out = Collect(String::new());
    s.prompt(&create(None), &mut out).await.unwrap();
    assert!(
        out.0.contains("Blocked by configuration policy"),
        "{}",
        out.0
    );
    assert!(asker.0.lock().unwrap().is_empty());
    assert_eq!(count(), 1);

    // The plan agent cannot schedule work for build; read-only targets are fine.
    let (mut s, _) = session(rule, "plan", Reply::Once);
    let mut out = Collect(String::new());
    s.prompt(&create(Some("build")), &mut out).await.unwrap();
    assert!(
        out.0.contains("only as this session's agent (plan)"),
        "{}",
        out.0
    );
    assert_eq!(count(), 1);
    for ok in ["plan", "explore", "review"] {
        let mut out = Collect(String::new());
        s.prompt(&create(Some(ok)), &mut out).await.unwrap();
        assert!(out.0.contains("Created"), "{ok}: {}", out.0);
    }
    let mut out = Collect(String::new());
    s.prompt(&create(Some("general")), &mut out).await.unwrap();
    assert!(out.0.contains("not general"), "{}", out.0);
    assert_eq!(count(), 4);
}

/// Regression: the scheduler opened its own session object on a session a
/// live process was using, so the two raced and the live one never saw the
/// automation's messages.
#[tokio::test(flavor = "multi_thread")]
async fn a_busy_session_defers_runs_and_a_live_session_catches_up() {
    use codesmith_core::permission::{Fixed, Reply};
    use codesmith_core::session::{Options, Session};
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let base = model().await;
    let loaded = (loader(base.clone()))(dir.path()).unwrap();
    let mut live = Session::new(
        &loaded,
        dir.path().to_path_buf(),
        Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("live".into()),
            parent: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(&dbp).unwrap()),
    )
    .unwrap();
    let mut out = Collect(String::new());
    live.prompt("hello", &mut out).await.unwrap();
    let sid = live.id.clone();
    assert_eq!(
        db.session_holder(&sid).unwrap(),
        None,
        "released after the turn"
    );

    let a = au::create(
        &db,
        au::New {
            name: "ping".into(),
            directory: dir.path().display().to_string(),
            session_id: Some(sid.clone()),
            trigger: Trigger::Every {
                every: "1 hour".into(),
                tz: None,
            },
            action: Action::Prompt {
                prompt: "ping".into(),
                to: Target::Session {
                    session: sid.clone(),
                },
            },
        },
    )
    .unwrap();
    // Another process is running a turn in the session: the run is deferred.
    assert!(db.lease_session(&sid, "other-process", 30_000).unwrap());
    due_now(&db, &a.id);
    let runner = Runner::new(dbp.clone(), loader(base.clone()));
    runner.tick().await.unwrap();
    let d = au::get(&db, &a.id).unwrap().unwrap();
    assert_eq!(d.last_outcome.as_deref(), Some("deferred"));
    assert_eq!(d.runs, 0, "the claim was undone");
    assert!(d.next_due.unwrap() <= codesmith_core::store::now());
    assert!(au::runs(&db, &a.id).unwrap().is_empty());
    assert_eq!(texts(&db, &sid).len(), 2, "nothing was sent");

    // Once it is free, the next tick runs it.
    db.release_session(&sid, "other-process").unwrap();
    runner.tick().await.unwrap();
    let f = au::get(&db, &a.id).unwrap().unwrap();
    assert_eq!((f.last_outcome.as_deref(), f.runs), (Some("fired"), 1));
    assert_eq!(texts(&db, &sid).len(), 4);

    // The live session picks the automation's exchange up before its next turn.
    live.prompt("again", &mut out).await.unwrap();
    let seen: Vec<String> = live
        .messages
        .iter()
        .filter_map(|m| match m.parts.first() {
            Some(Part::Text(t)) => Some(t.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        seen,
        [
            "hello",
            "answer to: hello",
            "ping",
            "answer to: ping",
            "again",
            "answer to: again"
        ]
    );
    assert_eq!(texts(&db, &sid).len(), 6);

    // A live turn waits for a turn running elsewhere instead of racing it.
    assert!(db.lease_session(&sid, "other-process", 30_000).unwrap());
    let path = dbp.clone();
    let id = sid.clone();
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        Store::open(&path)
            .unwrap()
            .release_session(&id, "other-process")
            .unwrap();
    });
    let started = std::time::Instant::now();
    live.prompt("third", &mut out).await.unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(500));
    assert!(out.0.contains("answer to: third"));
    release.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn managing_automations_asks_and_unattended_runs_cannot() {
    use codesmith_core::permission::Reply;
    use codesmith_core::session::{Options, Session};
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let base = model().await;
    let d = dir.path().display().to_string();
    let job = |name: &str, prompt: &str| au::New {
        name: name.into(),
        directory: d.clone(),
        session_id: None,
        trigger: Trigger::Every {
            every: "1 hour".into(),
            tz: None,
        },
        action: Action::Prompt {
            prompt: prompt.into(),
            to: Target::Subagent {
                subagent: "build".into(),
            },
        },
    };
    let target = au::create(&db, job("target", "summarize the log")).unwrap();
    // A rule allowing only creation leaves pause, resume and remove asking.
    let only_create =
        json!({"permissions": [{"action": "automation", "resource": "create", "effect": "allow"}]});
    let session = |reply: Reply| {
        let loaded = (open_loader(base.clone(), only_create.clone()))(dir.path()).unwrap();
        let asker = Arc::new(Record(Default::default(), reply, Default::default()));
        let s = Session::new(
            &loaded,
            dir.path().to_path_buf(),
            Options {
                agent: None,
                model: None,
                resume: None,
                title: Some("t".into()),
                parent: None,
                questioner: None,
                mcp: None,
            },
            asker.clone(),
            Some(Store::open(&dbp).unwrap()),
        )
        .unwrap();
        (s, asker)
    };
    let call = |action: &str| {
        let args = json!({"action": action, "id": target.id});
        format!("call automation {args}")
    };
    let status = || au::get(&db, &target.id).unwrap().unwrap().status;

    let (mut s, asker) = session(Reply::Reject(None));
    for action in ["pause", "remove"] {
        let mut out = Collect(String::new());
        s.prompt(&call(action), &mut out).await.unwrap();
        assert_eq!(status(), "active", "{action}: {}", out.0);
    }
    assert_eq!(*asker.0.lock().unwrap(), ["automation", "automation"]);
    let titles = asker.2.lock().unwrap().clone();
    assert_eq!(titles[0].1, ["pause"]);
    assert!(
        titles[0].0.starts_with(&format!(
            "Pause automation \"target\" ({}, every 1 hour)",
            target.id
        )),
        "{:?}",
        titles[0]
    );
    assert!(titles[0].0.contains("summarize the log"), "{:?}", titles[0]);
    assert_eq!(titles[1].1, ["remove"]);

    // Approved, each goes ahead; a paused automation's resume asks too.
    let (mut s, asker) = session(Reply::Once);
    let mut out = Collect(String::new());
    s.prompt(&call("pause"), &mut out).await.unwrap();
    assert_eq!(status(), "paused", "{}", out.0);
    let mut out = Collect(String::new());
    s.prompt(&call("resume"), &mut out).await.unwrap();
    assert_eq!(status(), "active", "{}", out.0);
    assert_eq!(asker.0.lock().unwrap().len(), 2);

    // An unattended run can't manage automations, even when the rules and
    // `automation.permissions` allow the tool.
    let allow = json!({
        "permissions": [{"action": "automation", "resource": "*", "effect": "allow"}],
        "automation": {"permissions": [{"action": "automation", "resource": "*", "effect": "allow"}]}
    });
    let runner = Runner::new(dbp.clone(), open_loader(base.clone(), allow));
    let rogue = au::create(&db, job("rogue", &call("remove"))).unwrap();
    due_now(&db, &rogue.id);
    runner.tick().await.unwrap();
    let r = &au::runs(&db, &rogue.id).unwrap()[0];
    assert!(
        r.detail.contains("No tool named \"automation\""),
        "{}",
        r.detail
    );
    assert_eq!(status(), "active");
}

/// A loader whose config has the `fake` stdio MCP server (tool `echo`).
fn mcp_loader(base: String, py: &'static str, extra: Value) -> au::Loader {
    let fixture = format!("{}/tests/fixtures/mcp_stdio.py", env!("CARGO_MANIFEST_DIR"));
    let mut extra = extra;
    codesmith_core::config::merge(
        &mut extra,
        json!({"mcp": {"servers": {"fake": {"type": "local", "command": [py, fixture]}}}}),
    );
    open_loader(base, extra)
}

#[tokio::test(flavor = "multi_thread")]
async fn unattended_runs_deny_mcp_tools_unless_config_opts_in() {
    use codesmith_core::permission::{evaluate, Effect};
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let base = model().await;
    let d = dir.path().display().to_string();
    // A program that calls the server's tool with data from the run.
    let code = "return { state: await tools.fake.echo({ text: 'leaked ' + input.runs }) }";
    // Each case gets its own database: a runner holds the scheduler lease.
    let mut n = 0;
    let mut run = |extra: Value| {
        n += 1;
        let dbp = dir.path().join(format!("s{n}.db"));
        let (base, d) = (base.clone(), d.clone());
        async move {
            let db = Store::open(&dbp).unwrap();
            let runner = Runner::new(dbp.clone(), mcp_loader(base, py, extra));
            let a = au::create(
                &db,
                au::New {
                    name: "mcp".into(),
                    directory: d,
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
            due_now(&db, &a.id);
            runner.tick().await.unwrap();
            au::get(&db, &a.id).unwrap().unwrap()
        }
    };
    // Denied by default: the program doesn't even see the tool.
    let a = run(json!({})).await;
    assert_eq!(a.last_outcome.as_deref(), Some("failed"));
    assert_eq!(a.state, Value::Null, "{:?}", a.last_error);

    // `automation.permissions` opts one tool back in.
    let opt_in = json!({"automation": {"permissions": [{"action": "fake_echo", "resource": "*", "effect": "allow"}]}});
    let a = run(opt_in).await;
    assert_eq!(
        a.last_outcome.as_deref(),
        Some("fired"),
        "{:?}",
        a.last_error
    );
    assert_eq!(a.state, json!("echo: leaked 1"));

    // The same rules apply to prompts: every tool of every configured
    // server and the resource tools are denied, so no `execute` either.
    let config: Config = serde_json::from_value(json!({
        "mcp": {"servers": {"fake": {"type": "local", "command": ["x"]}, "my server": {"type": "local", "command": ["x"]}}},
        "automation": {"permissions": [{"action": "fake_echo", "resource": "*", "effect": "allow"}]}
    }))
    .unwrap();
    let rules = au::unattended_rules(&config);
    for (tool, effect) in [
        ("fake_echo", Effect::Allow),
        ("fake_other", Effect::Deny),
        ("my_server_send_mail", Effect::Deny),
        ("list_mcp_resources", Effect::Deny),
        ("read_mcp_resource", Effect::Deny),
        ("automation", Effect::Deny),
        ("watch_mcp_event", Effect::Deny),
        ("read", Effect::Ask),
    ] {
        assert_eq!(evaluate(tool, "*", &rules), effect, "{tool}");
    }
}

/// A Code Mode program's MCP calls are held to the creating session's own
/// rules, like every session a run starts: they only restrict, so a deny
/// set in that session beats the `automation.permissions` opt-in.
#[tokio::test(flavor = "multi_thread")]
async fn code_runs_keep_the_creating_sessions_rules() {
    use codesmith_core::permission::{Effect, Rule};
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let base = model().await;
    let d = dir.path().display().to_string();
    let opt_in = json!({"automation": {"permissions": [{"action": "fake_echo", "resource": "*", "effect": "allow"}]}});
    let loaded = (mcp_loader(base.clone(), py, opt_in.clone()))(dir.path()).unwrap();
    let mut creator = codesmith_core::session::Session::new(
        &loaded,
        dir.path().to_path_buf(),
        codesmith_core::session::Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("creator".into()),
            parent: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(codesmith_core::permission::Fixed(
            codesmith_core::permission::Reply::Once,
        )),
        Some(Store::open(&dbp).unwrap()),
    )
    .unwrap();
    let mut out = Collect(String::new());
    creator.prompt("hello", &mut out).await.unwrap();
    let runner = Runner::new(dbp.clone(), mcp_loader(base, py, opt_in));
    let code = "return { state: await tools.fake.echo({ text: 'hi' }) }";
    let run = |name: &str| {
        let a = au::create(
            &db,
            au::New {
                name: name.into(),
                directory: d.clone(),
                session_id: Some(creator.id.clone()),
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
        due_now(&db, &a.id);
        a.id
    };

    // Rules of the session that don't touch the tool leave the opt-in be.
    db.set_permissions(&creator.id, &[Rule::new("webfetch", "*", Effect::Deny)])
        .unwrap();
    let id = run("other");
    runner.tick().await.unwrap();
    let a = au::get(&db, &id).unwrap().unwrap();
    assert_eq!(
        a.last_outcome.as_deref(),
        Some("fired"),
        "{:?}",
        a.last_error
    );
    assert_eq!(a.state, json!("echo: hi"));
    au::remove(&db, &id).unwrap();

    // A session deny for the tool holds in the run: the program does not
    // even see it, and nothing is called.
    db.set_permissions(&creator.id, &[Rule::new("fake_echo", "*", Effect::Deny)])
        .unwrap();
    let id = run("denied");
    runner.tick().await.unwrap();
    let a = au::get(&db, &id).unwrap().unwrap();
    assert_eq!(a.last_outcome.as_deref(), Some("failed"));
    assert_eq!(a.state, Value::Null, "{:?}", a.last_error);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_watch_request_shows_the_server_event_args_and_prompt() {
    use codesmith_core::permission::Reply;
    use codesmith_core::session::{Options, Session};
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let base = model().await;
    let loaded = (event_loader(base, py))(dir.path()).unwrap();
    let mcp = codesmith_core::mcp::Mcp::connect(&loaded.config.mcp, dir.path()).await;
    let asker = Arc::new(Record(Default::default(), Reply::Once, Default::default()));
    let mut s = Session::new(
        &loaded,
        dir.path().to_path_buf(),
        Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("t".into()),
            parent: None,
            questioner: None,
            mcp: Some(mcp),
        },
        asker.clone(),
        Some(Store::open(&dbp).unwrap()),
    )
    .unwrap();
    let args = json!({"action": "create", "name": "tickets", "prompt": "triage it\nthen close it", "trigger": {"type": "event", "server": "rich", "event": "ticket.created", "args": {"queue": "ops"}}});
    let mut out = Collect(String::new());
    s.prompt(&format!("call automation {args}"), &mut out)
        .await
        .unwrap();
    assert!(out.0.contains("Created"), "{}", out.0);
    let asked = asker.2.lock().unwrap().clone();
    assert_eq!(*asker.0.lock().unwrap(), ["automation", "watch_mcp_event"]);
    assert!(
        asked[0]
            .0
            .contains("(on rich:ticket.created events {\"queue\":\"ops\"})"),
        "{:?}",
        asked[0]
    );
    assert!(
        asked[0].0.ends_with(":\ntriage it\nthen close it"),
        "{:?}",
        asked[0]
    );
    assert_eq!(asked[1].1, ["rich"]);
    assert_eq!(
        asked[1].0,
        "Watch ticket.created events from MCP server rich with arguments {\"queue\":\"ops\"}, for automation \"tickets\": each event runs it unattended with the event as untrusted data"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_start_a_child_of_the_creating_session_unless_told_to_continue_it() {
    use codesmith_core::permission::Reply;
    use codesmith_core::session::{Options, Session};
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let base = model().await;
    let allow =
        json!({"permissions": [{"action": "automation", "resource": "*", "effect": "allow"}]});
    let loaded = (open_loader(base.clone(), allow))(dir.path()).unwrap();
    let asker = Arc::new(Record(Default::default(), Reply::Once, Default::default()));
    // A plan session on a model other than the configured default.
    let mut s = Session::new(
        &loaded,
        dir.path().to_path_buf(),
        Options {
            agent: Some("plan".into()),
            model: Some("fake/m2".into()),
            resume: None,
            title: Some("creator".into()),
            parent: None,
            questioner: None,
            mcp: None,
        },
        asker.clone(),
        Some(Store::open(&dbp).unwrap()),
    )
    .unwrap();
    let create = |name: &str, prompt: &str, target: Option<&str>| {
        let mut args = json!({"action": "create", "name": name, "prompt": prompt, "trigger": {"type": "every", "every": "1 hour"}});
        if let Some(t) = target {
            args["target"] = json!(t);
        }
        format!("call automation {args}")
    };
    let made = |name: &str| {
        au::list(&db, None)
            .unwrap()
            .into_iter()
            .find(|a| a.name == name)
            .unwrap()
    };
    let mut out = Collect(String::new());
    s.prompt(&create("job", "check the build", None), &mut out)
        .await
        .unwrap();
    assert!(out.0.contains("Created"), "{}", out.0);
    let job = made("job");
    assert_eq!(
        job.action,
        Action::Prompt {
            prompt: "check the build".into(),
            to: Target::Child {
                child: s.id.clone(),
                agent: "plan".into(),
                model: Some("fake/m2".into()),
            },
        }
    );
    let own = texts(&db, &s.id).len();

    // Each run is a new child session with the creator's agent and model.
    let runner = Runner::new(dbp.clone(), open_loader(base.clone(), json!({})));
    for _ in 0..2 {
        due_now(&db, &job.id);
        assert_eq!(runner.tick().await.unwrap(), 1);
    }
    let runs = runs_of(&db, &job.id);
    let ids: Vec<String> = runs.iter().map(|r| r.session_id.clone().unwrap()).collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "a fresh session per run");
    for (r, id) in runs.iter().zip(&ids) {
        assert_eq!(
            (r.outcome.as_str(), r.detail.as_str()),
            ("fired", "answer to: check the build")
        );
        let info = db.get(id).unwrap().unwrap();
        assert_eq!(info.parent_id.as_deref(), Some(s.id.as_str()));
        assert_eq!(
            (info.agent.as_str(), info.model.as_str()),
            ("plan", "fake/m2")
        );
        assert_eq!(info.directory, dir.path().display().to_string());
        let title = info.title.unwrap();
        assert!(title.starts_with("job — 20"), "{title}");
        assert_eq!(
            texts(&db, id),
            ["u:check the build", "a:answer to: check the build"]
        );
    }
    assert_eq!(
        texts(&db, &s.id).len(),
        own,
        "the creating session is untouched"
    );

    // `target: "session"` continues the creating session, and asks saying so.
    let mut out = Collect(String::new());
    s.prompt(&create("same", "ping", Some("session")), &mut out)
        .await
        .unwrap();
    assert!(out.0.contains("Created"), "{}", out.0);
    let same = made("same");
    assert!(
        matches!(&same.action, Action::Prompt { to: Target::Session { session }, .. } if *session == s.id),
        "{:?}",
        same.action
    );
    let own = texts(&db, &s.id).len();
    due_now(&db, &same.id);
    runner.tick().await.unwrap();
    let r = &runs_of(&db, &same.id)[0];
    assert_eq!(r.session_id.as_deref(), Some(s.id.as_str()));
    assert_eq!(texts(&db, &s.id).len(), own + 2);

    // `agent` can't go with target "session"; an unknown target fails the
    // tool's schema before the tool runs.
    for (bad, want) in [
        (
            json!({"action": "create", "prompt": "x", "agent": "plan", "target": "session", "trigger": {"type": "every", "every": "1 hour"}}),
            "can't go with target",
        ),
        (
            json!({"action": "create", "prompt": "x", "target": "elsewhere", "trigger": {"type": "every", "every": "1 hour"}}),
            "- target: Expected \"child\" | \"session\"",
        ),
    ] {
        let mut out = Collect(String::new());
        s.prompt(&format!("call automation {bad}"), &mut out)
            .await
            .unwrap();
        assert!(out.0.contains(want), "{}", out.0);
    }
    let titles = asker.2.lock().unwrap().clone();
    assert!(titles.is_empty(), "the rule approved without asking");
}

#[tokio::test(flavor = "multi_thread")]
async fn stored_targets_keep_their_meaning_and_children_run_unattended() {
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("s.db");
    let db = Store::open(&dbp).unwrap();
    let base = model().await;
    let d = dir.path().display().to_string();
    let job = |name: &str, prompt: &str, to: Target| au::New {
        name: name.into(),
        directory: d.clone(),
        session_id: None,
        trigger: Trigger::Every {
            every: "1 hour".into(),
            tz: None,
        },
        action: Action::Prompt {
            prompt: prompt.into(),
            to,
        },
    };

    // Rows written before child targets existed read back unchanged.
    let old = au::create(
        &db,
        job(
            "old",
            "x",
            Target::Subagent {
                subagent: "build".into(),
            },
        ),
    )
    .unwrap();
    for (raw, want) in [
        (
            r#"{"type":"prompt","prompt":"x","to":{"session":"ses_old"}}"#,
            Target::Session {
                session: "ses_old".into(),
            },
        ),
        (
            r#"{"type":"prompt","prompt":"x","to":{"subagent":"explore"}}"#,
            Target::Subagent {
                subagent: "explore".into(),
            },
        ),
    ] {
        db.with_conn(|c| {
            Ok(c.execute(
                "UPDATE automation SET action = ?2 WHERE id = ?1",
                rusqlite::params![old.id, raw],
            )?)
        })
        .unwrap();
        let a = au::get(&db, &old.id).unwrap().unwrap();
        assert_eq!(
            a.action,
            Action::Prompt {
                prompt: "x".into(),
                to: want
            }
        );
    }
    au::remove(&db, &old.id).unwrap();
    // The child target's stored form.
    let t = Target::child_of("ses_p", "build", "fake/m");
    assert_eq!(
        serde_json::to_value(&t).unwrap(),
        json!({"child": "ses_p", "agent": "build", "model": "fake/m"})
    );

    // A child run is unattended like any other: shell is denied though the
    // build agent allows it interactively.
    let loaded = (open_loader(base.clone(), json!({})))(dir.path()).unwrap();
    let mut parent = codesmith_core::session::Session::new(
        &loaded,
        dir.path().to_path_buf(),
        codesmith_core::session::Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("p".into()),
            parent: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(codesmith_core::permission::Fixed(
            codesmith_core::permission::Reply::Once,
        )),
        Some(Store::open(&dbp).unwrap()),
    )
    .unwrap();
    let mut out = Collect(String::new());
    parent.prompt("hello", &mut out).await.unwrap();
    let a = au::create(
        &db,
        job(
            "child",
            "use shell",
            Target::child_of(&parent.id, "build", "fake/m"),
        ),
    )
    .unwrap();
    let runner = Runner::new(dbp.clone(), open_loader(base.clone(), json!({})));
    due_now(&db, &a.id);
    runner.tick().await.unwrap();
    let r = &runs_of(&db, &a.id)[0];
    assert!(r.detail.contains("No tool named \"shell\""), "{}", r.detail);
    let info = db.get(r.session_id.as_deref().unwrap()).unwrap().unwrap();
    assert_eq!(info.parent_id.as_deref(), Some(parent.id.as_str()));

    // With the creating session gone, so are the session rules a run
    // would start with: the run is refused, saying why, and the
    // automation paused. (Security review 7: runs went ahead unparented,
    // without the creator's session-only rules.)
    let sessions = db.list(None, true).unwrap().len();
    db.delete(&parent.id).unwrap();
    let left = db.list(None, true).unwrap().len();
    assert!(left < sessions);
    due_now(&db, &a.id);
    runner.tick().await.unwrap();
    let r = &runs_of(&db, &a.id)[1];
    assert_eq!(r.outcome, "failed", "{}", r.detail);
    assert!(r.detail.contains("was deleted"), "{}", r.detail);
    assert!(r.detail.contains(&parent.id), "{}", r.detail);
    assert_eq!(r.session_id, None);
    assert_eq!(
        db.list(None, true).unwrap().len(),
        left,
        "no session was started"
    );
    assert_eq!(au::get(&db, &a.id).unwrap().unwrap().status, "paused");

    // The same for an automation whose creator is recorded on it.
    let mut j = job(
        "sub",
        "x",
        Target::Subagent {
            subagent: "build".into(),
        },
    );
    j.session_id = Some(parent.id.clone());
    let b = au::create(&db, j).unwrap();
    due_now(&db, &b.id);
    runner.tick().await.unwrap();
    let r = &runs_of(&db, &b.id)[0];
    assert_eq!(r.outcome, "failed", "{}", r.detail);
    assert!(r.detail.contains("was deleted"), "{}", r.detail);
    assert_eq!(au::get(&db, &b.id).unwrap().unwrap().status, "paused");
}
