//! `codesmith serve`: sessions over HTTP, a turn streamed as Server-Sent
//! Events, a permission answered over HTTP, an abort, undo, list and delete,
//! and the bearer token.

use crate::harness::{user_texts, Env};
use crate::model::{Model, Reply, Script, M};
use crate::need_git;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

struct Server {
    _child: tokio::process::Child,
    url: String,
    http: reqwest::Client,
    token: Option<String>,
}

impl Server {
    async fn start(e: &Env, extra: &[&str]) -> Server {
        Self::start_with(e, extra, &[]).await
    }

    async fn start_with(e: &Env, extra: &[&str], env: &[(&str, &str)]) -> Server {
        let mut args = vec!["serve", "--port", "0"];
        args.extend_from_slice(extra);
        let mut child = e.command(&args).envs(env.iter().copied()).spawn().unwrap();
        let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
        let url = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(line) = err.next_line().await.unwrap() {
                if let Some(at) = line.find("http://") {
                    let rest = &line[at..];
                    return rest.split_whitespace().next().unwrap().to_string();
                }
            }
            panic!("serve exited before listening");
        })
        .await
        .expect("serve did not start");
        // Keep draining stderr so the server never blocks on it.
        tokio::spawn(async move { while let Ok(Some(_)) = err.next_line().await {} });
        // Without --token, serve generates one and saves it owner-only in
        // the data dir, which is how local clients find it.
        let token = extra
            .iter()
            .position(|a| *a == "--token")
            .map(|i| extra[i + 1].to_string())
            .or_else(|| {
                let file = e.data.join("codesmith").join("serve-token");
                std::fs::read_to_string(&file)
                    .ok()
                    .map(|t| t.trim().to_string())
            });
        Server {
            _child: child,
            url,
            http: reqwest::Client::new(),
            token,
        }
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, format!("{}{path}", self.url));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self.req(reqwest::Method::GET, path).send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .req(reqwest::Method::POST, path)
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn delete(&self, path: &str) -> (u16, Value) {
        let r = self
            .req(reqwest::Method::DELETE, path)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    /// Start a turn and return its event stream.
    async fn turn(&self, sid: &str, text: &str) -> Events {
        let r = self
            .req(reqwest::Method::POST, &format!("/sessions/{sid}/messages"))
            .json(&json!({"text": text}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        Events {
            body: Box::pin(r.bytes_stream()),
            buf: String::new(),
        }
    }
}

impl Server {
    /// Follow a session's events (`GET /sessions/{sid}/events`).
    async fn watch(&self, sid: &str) -> Events {
        let r = self
            .req(reqwest::Method::GET, &format!("/sessions/{sid}/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        Events {
            body: Box::pin(r.bytes_stream()),
            buf: String::new(),
        }
    }
}

type ByteStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<axum::body::Bytes>> + Send>>;

/// A Server-Sent Events reader.
struct Events {
    body: ByteStream,
    buf: String,
}

impl Events {
    /// The next event's (name, data), or `None` at the end of the stream.
    async fn next(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let frame: String = self.buf.drain(..end + 2).collect();
                let mut name = String::new();
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        name = v.trim().into();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim_start());
                    }
                }
                if name.is_empty() && data.is_empty() {
                    continue; // keep-alive
                }
                return Some((name, serde_json::from_str(&data).unwrap_or(Value::Null)));
            }
            let chunk = tokio::time::timeout(Duration::from_secs(20), self.body.next())
                .await
                .expect("timed out waiting for an event")?;
            self.buf.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    }

    /// Events until one named `name` (inclusive).
    async fn until(&mut self, name: &str) -> Vec<(String, Value)> {
        let mut out = vec![];
        while let Some(ev) = self.next().await {
            let stop = ev.0 == name;
            out.push(ev);
            if stop {
                return out;
            }
        }
        panic!("the stream ended before {name}: {out:?}");
    }
}

fn names(ev: &[(String, Value)]) -> Vec<&str> {
    ev.iter().map(|e| e.0.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_over_http_with_a_permission_an_abort_and_undo() {
    need_git!();
    let e = Env::new();
    e.write("test.sh", "echo PASS\n");
    e.write("victim.txt", "keep me\n");
    e.commit("files");
    let m = Model::start(
        Script::new()
            .titled("Served")
            .then(
                "run",
                M::tools().and(M::prompt("run the tests")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sh test.sh", "description": "Run tests"}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("run the tests")).and(M::step(1)),
                Reply::text("All tests pass."),
            )
            .then(
                "delete",
                M::tools().and(M::prompt("delete the victim")),
                Reply::call(
                    "shell",
                    json!({"command": "rm victim.txt", "description": "Delete"}),
                ),
            )
            .then(
                "write",
                M::tools().and(M::prompt("write a note")).and(M::step(0)),
                Reply::call("write", json!({"path": "note.txt", "content": "hello\n"})),
            )
            .then(
                "written",
                M::tools().and(M::prompt("write a note")).and(M::step(1)),
                Reply::text("Wrote note.txt."),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );
    let s = Server::start(&e, &[]).await;
    let (code, health) = s.get("/health").await;
    assert_eq!(code, 200);
    assert_eq!(health["ok"], true);
    assert_eq!(health["model"], "fake/m");
    assert_eq!(health["directory"], e.project.display().to_string());

    let (code, created) = s.post("/sessions", json!({})).await;
    assert_eq!(code, 200, "{created}");
    let sid = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["agent"], "Build");

    // A turn with a permission answered over HTTP.
    let mut ev = s.turn(&sid, "run the tests").await;
    let before = ev.until("permission").await;
    assert_eq!(names(&before).last(), Some(&"permission"));
    let perm = &before.last().unwrap().1;
    assert_eq!(perm["action"], "shell");
    assert_eq!(perm["resources"], json!(["sh test.sh"]));
    // A second message while the turn runs is refused.
    let (code, _) = s
        .post(
            &format!("/sessions/{sid}/messages"),
            json!({"text": "again"}),
        )
        .await;
    assert_eq!(code, 409);
    let rid = perm["id"].as_str().unwrap();
    let (code, _) = s
        .post(
            &format!("/sessions/{sid}/permissions/{rid}"),
            json!({"reply": "once"}),
        )
        .await;
    assert_eq!(code, 200);
    let rest = ev.until("done").await;
    let all: Vec<&str> = names(&before).into_iter().chain(names(&rest)).collect();
    // The output streams in as many pieces as it takes.
    let all: Vec<&str> = all
        .into_iter()
        .filter(|n| *n != "todos" && *n != "tool_output")
        .collect();
    assert_eq!(
        all,
        [
            "step_end",
            "tool_start",
            "permission",
            "tool_progress",
            "tool_end",
            "text",
            "step_end",
            "done"
        ]
    );
    let streamed: String = rest
        .iter()
        .filter(|e| e.0 == "tool_output")
        .map(|e| e.1["delta"].as_str().unwrap())
        .collect();
    assert!(streamed.contains("PASS"), "{streamed}");
    let tool_end = &rest.iter().find(|e| e.0 == "tool_end").unwrap().1;
    assert!(tool_end["output"].as_str().unwrap().contains("PASS"));
    assert_eq!(tool_end["error"], false);
    let done = &rest.last().unwrap().1;
    assert_eq!(done["error"], Value::Null);
    let text: String = rest
        .iter()
        .filter(|e| e.0 == "text")
        .map(|e| e.1["delta"].as_str().unwrap())
        .collect();
    assert_eq!(text, "All tests pass.");
    // Answering it again finds nothing open.
    let (code, _) = s
        .post(
            &format!("/sessions/{sid}/permissions/{rid}"),
            json!({"reply": "once"}),
        )
        .await;
    assert_eq!(code, 404);

    // Abort a turn waiting for permission: nothing runs.
    let mut ev = s.turn(&sid, "delete the victim").await;
    ev.until("permission").await;
    let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
    assert_eq!((code, &r["aborted"]), (200, &json!(true)));
    let end = ev.until("done").await;
    assert_eq!(end.last().unwrap().1["error"], "aborted");
    assert_eq!(e.read("victim.txt"), "keep me\n");
    let (code, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(code, 200);
    assert_eq!(shown["busy"], false);
    let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
    assert_eq!((code, &r["aborted"]), (200, &json!(false)));

    // A write, then undo over HTTP.
    let mut ev = s.turn(&sid, "write a note").await;
    let done = ev.until("done").await;
    assert_eq!(done.last().unwrap().1["error"], Value::Null, "{done:?}");
    assert_eq!(e.read("note.txt"), "hello\n");
    let (code, r) = s.post(&format!("/sessions/{sid}/undo"), json!({})).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["files"], json!(["note.txt"]));
    assert!(!e.path("note.txt").exists());

    // The store has the turns; list and delete.
    let h = e.history(&sid);
    let users = user_texts(&h);
    assert_eq!(users[0], "run the tests");
    assert_eq!(users[1], "delete the victim");
    assert!(
        !users.contains(&"write a note".to_string()),
        "undone: {users:?}"
    );
    let (code, list) = s.get("/sessions").await;
    assert_eq!(code, 200);
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["id"] == sid.as_str()));
    let (code, r) = s.delete(&format!("/sessions/{sid}")).await;
    assert_eq!((code, &r["deleted"]), (200, &json!(true)));
    let (code, _) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(code, 404);
    assert!(e.store().get(&sid).unwrap().is_none());
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bearer_token_is_required_beyond_loopback() {
    let e = Env::new();
    let m = Model::start(Script::new()).await;
    e.config(&m.url, json!({}));
    let out = e.run(&["serve", "--host", "0.0.0.0", "--port", "0"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(out.stderr.contains("set --token"), "{}", out.both());

    let s = Server::start(&e, &["--token", "s3cret"]).await;
    let bare = reqwest::get(format!("{}/health", s.url)).await.unwrap();
    assert_eq!(bare.status(), 401);
    let wrong = s
        .http
        .get(format!("{}/sessions", s.url))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let (code, _) = s.get("/health").await;
    assert_eq!(code, 200);
    let (code, list) = s.get("/sessions").await;
    assert_eq!((code, list), (200, json!([])));
}

/// `$CODESMITH_SERVER_TOKEN` still sets the token though `codesmith` takes
/// it out of its environment at startup (so no child inherits it).
#[tokio::test(flavor = "multi_thread")]
async fn the_token_can_come_from_the_environment() {
    let e = Env::new();
    let m = Model::start(Script::new()).await;
    e.config(&m.url, json!({}));
    let mut s = Server::start_with(
        &e,
        &["--host", "0.0.0.0"],
        &[("CODESMITH_SERVER_TOKEN", "from-env")],
    )
    .await;
    s.url = s.url.replace("0.0.0.0", "127.0.0.1");
    s.token = Some("from-env".into());
    let (code, _) = s.get("/health").await;
    assert_eq!(code, 200);
    let bare = reqwest::get(format!("{}/health", s.url)).await.unwrap();
    assert_eq!(bare.status(), 401);
    assert!(!e.data.join("codesmith").join("serve-token").exists());
}

/// Regression: the session view listed the engine's output-limit nudge as
/// a user message, unlike the TUI and ACP replay.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_view_shows_only_real_prompts() {
    let e = Env::new();
    let nudged = M::last(codesmith::core::runner::CONTINUE_AFTER_OUTPUT_LIMIT);
    let m = Model::start(
        Script::new()
            .titled("Essay")
            .then(
                "cut",
                M::tools().and(nudged.clone().not()),
                Reply::text("Part one,").finish("length"),
            )
            .then("rest", M::tools().and(nudged), Reply::text(" part two.")),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &[]).await;
    let (code, created) = s.post("/sessions", json!({})).await;
    assert_eq!(code, 200, "{created}");
    let sid = created["id"].as_str().unwrap().to_string();
    let mut ev = s.turn(&sid, "write the essay").await;
    let done = ev.until("done").await;
    assert_eq!(done.last().unwrap().1["error"], Value::Null);
    let (code, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(code, 200);
    let msgs = shown["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("{shown:#}"))
        .clone();
    assert_eq!(user_texts(&msgs), ["write the essay"], "{shown:#}");
    let answers: Vec<&Value> = msgs
        .iter()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| crate::harness::parts(m, "text"))
        .collect();
    assert_eq!(answers, ["Part one,", " part two."]);
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn aborting_a_streaming_turn_keeps_the_session_usable() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Stall")
            .then(
                "stall",
                M::tools().and(M::prompt("think forever")),
                Reply::text("Thinking").then_stall(),
            )
            .then(
                "after",
                M::tools().and(M::prompt("hello")),
                Reply::text("Hi."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &[]).await;
    let (code, created) = s.post("/sessions", json!({})).await;
    assert_eq!(code, 200, "{created}");
    let sid = created["id"].as_str().unwrap().to_string();
    let mut ev = s.turn(&sid, "think forever").await;
    let first = ev.until("text").await;
    assert_eq!(first.last().unwrap().1["delta"], "Thinking");
    let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
    assert_eq!((code, &r["aborted"]), (200, &json!(true)));
    let end = ev.until("done").await;
    assert_eq!(end.last().unwrap().1["error"], "aborted");

    let mut ev = s.turn(&sid, "hello").await;
    let done = ev.until("done").await;
    assert_eq!(done.last().unwrap().1["error"], Value::Null, "{done:?}");
    // The aborted prompt is kept, and so is the answer it had streamed.
    let after = &m.by("after")[0];
    let roles: Vec<(String, String)> = crate::model::messages(after)
        .iter()
        .skip(1)
        .filter(|m| !crate::model::is_date(m))
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_string(),
                crate::model::content(m),
            )
        })
        .collect();
    assert_eq!(
        roles,
        [
            ("user".to_string(), "think forever".to_string()),
            ("assistant".to_string(), "Thinking".to_string()),
            ("user".to_string(), "hello".to_string())
        ]
    );
    m.assert_done();
}

/// Tool calls and results of an OpenAI-style request body: every result
/// answers a call made before it, and no call is answered twice.
fn assert_tool_pairs_valid(body: &Value) {
    let mut calls = std::collections::HashSet::new();
    let mut answered = std::collections::HashSet::new();
    for m in body["messages"].as_array().unwrap() {
        for c in m["tool_calls"].as_array().into_iter().flatten() {
            calls.insert(c["id"].as_str().unwrap().to_string());
        }
        if m["role"] == "tool" {
            let id = m["tool_call_id"].as_str().unwrap().to_string();
            assert!(calls.contains(&id), "a result for no call: {body:#}");
            assert!(answered.insert(id), "a call answered twice: {body:#}");
        }
    }
}

/// Tool-result ids of a stored history that occur more than once.
fn repeated_result_ids(history: &[Value]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    crate::harness::tool_results(history)
        .iter()
        .filter_map(|p| p["id"].as_str().map(str::to_string))
        .filter(|id| !seen.insert(id.clone()))
        .collect()
}

// An abort while the turn waits for a permission answer: the declined
// result is recorded before the session reloads, so the next prompt does
// not answer the call again (which providers reject for good).
#[tokio::test(flavor = "multi_thread")]
async fn aborting_during_a_permission_prompt_answers_the_call_once() {
    need_git!();
    let e = Env::new();
    e.write("victim.txt", "keep me\n");
    e.commit("files");
    let m = Model::start(
        Script::new()
            .titled("T")
            .always(
                "delete",
                M::tools().and(M::prompt("delete the victim")),
                Reply::call(
                    "shell",
                    json!({"command": "rm victim.txt", "description": "Delete"}),
                ),
            )
            .always("next", M::tools().and(M::prompt("next")), Reply::text("ok")),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );
    let s = Server::start(&e, &[]).await;
    for _ in 0..6 {
        let (_, created) = s.post("/sessions", json!({})).await;
        let sid = created["id"].as_str().unwrap().to_string();
        let mut ev = s.turn(&sid, "delete the victim").await;
        ev.until("permission").await;
        let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
        assert_eq!((code, &r["aborted"]), (200, &json!(true)));
        let end = ev.until("done").await;
        assert_eq!(end.last().unwrap().1["error"], "aborted");
        let mut ev = s.turn(&sid, "next").await;
        let end = ev.until("done").await;
        assert_eq!(end.last().unwrap().1["error"], Value::Null, "{end:?}");
        let h = e.history(&sid);
        assert_eq!(crate::harness::tool_results(&h).len(), 1, "{h:#?}");
        assert!(repeated_result_ids(&h).is_empty(), "{h:#?}");
    }
    assert_eq!(e.read("victim.txt"), "keep me\n");
    let next = m.by("next");
    assert_eq!(next.len(), 6);
    for body in &next {
        assert_tool_pairs_valid(body);
    }
}

// An abort before anything was stored (the attachment's permission prompt
// comes first) keeps the session's id: it is not swapped for a new one.
#[tokio::test(flavor = "multi_thread")]
async fn aborting_before_anything_is_stored_keeps_the_session_id() {
    let e = Env::new();
    let outside = e.root.join("outside.png");
    // A 1x1 PNG.
    std::fs::write(
        &outside,
        [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ],
    )
    .unwrap();
    let m = Model::start(Script::new().titled("T").always(
        "hi",
        M::tools().and(M::prompt("hello")),
        Reply::text("hi"),
    ))
    .await;
    // The plan agent reaches outside folders without asking; config makes
    // it ask here, so the attachment's prompt comes first.
    e.config(
        &m.url,
        json!({"permissions": [{"action": "external_directory", "resource": "*", "effect": "ask"}]}),
    );
    let s = Server::start(&e, &[]).await;
    let (_, created) = s.post("/sessions", json!({"agent": "plan"})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let r = s
        .req(reqwest::Method::POST, &format!("/sessions/{sid}/messages"))
        .json(&json!({"text": "look", "attachments": [{"path": outside.display().to_string()}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut ev = Events {
        body: Box::pin(r.bytes_stream()),
        buf: String::new(),
    };
    let asked = ev.until("permission").await;
    assert_eq!(asked.last().unwrap().1["action"], "external_directory");
    let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
    assert_eq!((code, &r["aborted"]), (200, &json!(true)));
    ev.until("done").await;
    let (code, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(code, 200);
    assert_eq!(shown["id"], json!(sid));
    assert_eq!(shown["agent"], "Plan", "the selection is kept too");
    assert_eq!(shown["busy"], false);
    let mut ev = s.turn(&sid, "hello").await;
    let end = ev.until("done").await;
    assert_eq!(end.last().unwrap().1["error"], Value::Null, "{end:?}");
    let stored: Vec<String> = e
        .store()
        .list(None, false)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(stored, [sid]);
}

// "Always" approvals survive the reload an abort makes.
#[tokio::test(flavor = "multi_thread")]
async fn always_approvals_survive_an_abort() {
    let e = Env::new();
    let echo = || {
        Reply::call(
            "shell",
            json!({"command": "echo hi", "description": "Echo"}),
        )
    };
    let m = Model::start(
        Script::new()
            .titled("T")
            .then(
                "first",
                M::tools().and(M::prompt("first")).and(M::step(0)),
                echo(),
            )
            .then(
                "stall",
                M::tools().and(M::prompt("first")).and(M::step(1)),
                Reply::text("thinking").then_stall(),
            )
            .then(
                "second",
                M::tools().and(M::prompt("second")).and(M::step(0)),
                echo(),
            )
            .then(
                "done",
                M::tools().and(M::prompt("second")).and(M::step(1)),
                Reply::text("done"),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );
    let s = Server::start(&e, &[]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut ev = s.turn(&sid, "first").await;
    let asked = ev.until("permission").await;
    let rid = asked.last().unwrap().1["id"].as_str().unwrap().to_string();
    let (code, _) = s
        .post(
            &format!("/sessions/{sid}/permissions/{rid}"),
            json!({"reply": "always"}),
        )
        .await;
    assert_eq!(code, 200);
    ev.until("text").await;
    let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
    assert_eq!((code, &r["aborted"]), (200, &json!(true)));
    ev.until("done").await;
    let mut ev = s.turn(&sid, "second").await;
    let mut all = vec![];
    while let Some(e) = ev.next().await {
        assert_ne!(e.0, "permission", "asked again: {all:?}");
        let done = e.0 == "done";
        all.push(e);
        if done {
            break;
        }
    }
    assert_eq!(all.last().unwrap().1["error"], Value::Null, "{all:?}");
    assert_eq!(m.by("done").len(), 1);
}

/// The model starts a background command and answers; the turn ends. When
/// the command finishes, the idle session is resumed by itself (as in
/// OpenCode) and the resumed turn streams to the session's watchers.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_background_command_resumes_the_idle_session() {
    let e = Env::new();
    let ask = M::tools().and(M::prompt("build it"));
    let notified = M::last("<shell id=\"shell_1\"");
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                ask.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 2; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                ask.clone().and(M::step(1)).and(notified.clone().not()),
                Reply::text("The build is running."),
            )
            .then(
                "resumed",
                M::tools().and(notified),
                Reply::text("It finished."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &["--auto"]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut watch = s.watch(&sid).await;
    let mut ev = s.turn(&sid, "build it").await;
    let turn = ev.until("done").await;
    assert_eq!(turn.last().unwrap().1["error"], Value::Null, "{turn:?}");
    assert!(ev.next().await.is_none(), "the request's stream ends");
    let (_, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(shown["busy"], false);
    assert_eq!(shown["background"], 1, "{shown}");

    // The watcher sees the request's turn, then the resumed one.
    let first = watch.until("done").await;
    assert_eq!(first.last().unwrap().1["resumed"], Value::Null);
    let resumed = watch.until("done").await;
    let done = &resumed.last().unwrap().1;
    assert_eq!(done["resumed"], true, "{resumed:?}");
    assert_eq!(done["error"], Value::Null, "{resumed:?}");
    let notice = resumed
        .iter()
        .find(|(n, _)| n == "notice")
        .map(|(_, v)| v["message"].as_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        notice.starts_with("background command shell_1 finished"),
        "{resumed:?}"
    );
    let text: String = resumed
        .iter()
        .filter(|(n, _)| n == "text")
        .filter_map(|(_, v)| v["delta"].as_str())
        .collect();
    assert_eq!(text, "It finished.");
    let last = crate::model::messages(&m.by("resumed")[0])
        .last()
        .cloned()
        .unwrap();
    assert!(crate::model::content(&last).contains("built-ok"), "{last}");
    let (_, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(shown["background"], 0, "{shown}");
    m.assert_done();
}

/// A prompt sent while the session waits for its background command takes
/// over: the command is reported in that turn, once, and the session is
/// not resumed for it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_during_the_wait_reports_the_command_once() {
    let e = Env::new();
    let shell = "<shell id=\"shell_1\"";
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 3; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("build it")).and(M::step(1)),
                Reply::text("The build is running."),
            )
            .then(
                "check",
                M::tools().and(M::prompt("check on it")).and(M::step(0)),
                Reply::call("shell", json!({"command": "sleep 4", "description": "wait"})),
            )
            .then(
                "notified",
                M::tools().and(M::last(shell)),
                Reply::text("It built."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &["--auto"]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut watch = s.watch(&sid).await;
    s.turn(&sid, "build it").await.until("done").await;
    let mut ev = s.turn(&sid, "check on it").await;
    let turn = ev.until("done").await;
    assert_eq!(turn.last().unwrap().1["error"], Value::Null, "{turn:?}");
    // Nothing is resumed afterwards.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (_, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(shown["busy"], false);
    watch.until("done").await;
    watch.until("done").await;
    let dones = m.by("notified");
    assert_eq!(dones.len(), 1);
    let reported = crate::model::messages(&dones[0])
        .iter()
        .filter(|m| crate::model::content(m).contains(shell))
        .count();
    assert_eq!(reported, 1, "{:#}", dones[0]);
    assert!(m.unmatched().is_empty(), "{:#?}", m.unmatched());
    m.assert_done();
}

/// Aborting an idle session stops its background commands and the wait to
/// resume for them.
#[tokio::test(flavor = "multi_thread")]
async fn aborting_an_idle_session_stops_its_background_commands() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Server")
            .then(
                "start",
                M::tools().and(M::prompt("serve it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 60", "description": "server", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("serve it")).and(M::step(1)),
                Reply::text("Serving."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &["--auto"]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    s.turn(&sid, "serve it").await.until("done").await;
    let (_, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(shown["background"], 1, "{shown}");
    let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
    assert_eq!(code, 200);
    assert_eq!(r, json!({"aborted": false, "background_stopped": 1}));
    let (_, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(shown["background"], 0, "{shown}");
    assert_eq!(shown["busy"], false);
    m.assert_done();
}

/// Fixed: an abort of a later turn, and an undo, reloaded the session and
/// stopped its background commands with the old one, so they were never
/// reported. As in OpenCode they keep running; the idle session is
/// resumed when the command finishes.
#[tokio::test(flavor = "multi_thread")]
async fn background_commands_survive_an_abort_and_undo() {
    let e = Env::new();
    let notified = M::last("<shell id=\"shell_1\"");
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "resumed",
                M::tools().and(notified),
                Reply::text("It finished."),
            )
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 4; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("build it")).and(M::step(1)),
                Reply::text("The build is running."),
            )
            .then(
                "wait",
                M::tools().and(M::prompt("wait a bit")).and(M::step(0)),
                Reply::call("shell", json!({"command": "sleep 30", "description": "wait"})),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &["--auto"]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut watch = s.watch(&sid).await;
    s.turn(&sid, "build it").await.until("done").await;
    let mut ev = s.turn(&sid, "wait a bit").await;
    ev.until("tool_start").await;
    let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
    assert_eq!((code, &r), (200, &json!({"aborted": true})));
    let (code, r) = s.post(&format!("/sessions/{sid}/undo"), json!({})).await;
    assert_eq!(code, 200, "{r}");
    let (_, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(shown["background"], 1, "{shown}");
    let resumed = loop {
        let turn = watch.until("done").await;
        if turn.last().unwrap().1["resumed"] == true {
            break turn;
        }
    };
    assert_eq!(
        resumed.last().unwrap().1["error"],
        Value::Null,
        "{resumed:?}"
    );
    let last = crate::model::messages(&m.by("resumed")[0])
        .last()
        .cloned()
        .unwrap();
    assert!(crate::model::content(&last).contains("built-ok"), "{last}");
    m.assert_done();
}

/// Fixed: an abort reloaded the session and woke it at once for the
/// background commands that had already finished, so a turn the user
/// stopped (an aborted resume too) was followed by one they did not ask
/// for. Those commands now wait for the next prompt.
#[tokio::test(flavor = "multi_thread")]
async fn an_abort_does_not_resume_for_commands_already_finished() {
    let e = Env::new();
    let shell = "<shell id=\"shell_1\"";
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 1; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "wait",
                M::tools().and(M::prompt("build it")).and(M::step(1)),
                Reply::call("shell", json!({"command": "sleep 30", "description": "wait"})),
            )
            .then(
                "next",
                M::tools().and(M::prompt("what happened")),
                Reply::text("It built."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &["--auto"]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut ev = s.turn(&sid, "build it").await;
    ev.until("tool_start").await;
    ev.until("tool_start").await;
    // The background command ends while the foreground one still runs.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let (code, r) = s.post(&format!("/sessions/{sid}/abort"), json!({})).await;
    assert_eq!((code, &r), (200, &json!({"aborted": true})));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (_, shown) = s.get(&format!("/sessions/{sid}")).await;
    assert_eq!(shown["busy"], false, "{shown}");
    assert!(m.unmatched().is_empty(), "resumed: {:#?}", m.unmatched());
    // The next prompt reports it.
    let turn = s.turn(&sid, "what happened").await.until("done").await;
    assert_eq!(turn.last().unwrap().1["error"], Value::Null, "{turn:?}");
    let next = &m.by("next")[0];
    let reported = crate::model::messages(next)
        .iter()
        .filter(|m| crate::model::content(m).contains(shell))
        .count();
    assert_eq!(reported, 1, "{next:#}");
    m.assert_done();
}

/// Deleting a session stops its background commands: the command never
/// gets to its end, and nothing resumes the session.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_stops_its_background_commands() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 2; touch ended", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("build it")).and(M::step(1)),
                Reply::text("The build is running."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &["--auto"]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    s.turn(&sid, "build it").await.until("done").await;
    let (code, r) = s.delete(&format!("/sessions/{sid}")).await;
    assert_eq!((code, &r), (200, &json!({"deleted": true})));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!e.project.join("ended").exists(), "the command ran on");
    assert!(m.unmatched().is_empty(), "{:#?}", m.unmatched());
    m.assert_done();
}

// A subagent's run streams on the turn and on the session's events:
// `child_start` names the call that started it, its calls and permission
// requests carry its session, its text comes as `child_text`, and
// `child_end` says how it ended.
#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_activity_streams_with_its_session() {
    use codesmith::core::tool::subagent::PREAMBLE;
    let e = Env::new();
    let parent = M::tools()
        .and(M::prompt("delegate it"))
        .and(M::prompt(PREAMBLE).not());
    let child = M::tools().and(M::prompt(PREAMBLE));
    let m = Model::start(
        Script::new()
            .titled("Delegated")
            .then(
                "delegate",
                parent.clone().and(M::step(0)),
                Reply::call(
                    "subagent",
                    json!({"agent": "general", "description": "Run the check", "prompt": "Run the check."}),
                ),
            )
            .then(
                "child-run",
                child.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "echo child-ran", "description": "Check"}),
                ),
            )
            .then(
                "child-answer",
                child.and(M::step(1)),
                Reply::text("The check ran.").and_reasoning("It printed."),
            )
            .then(
                "answer",
                parent.and(M::step(1)),
                Reply::text("Delegated fine."),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );
    let s = Server::start(&e, &[]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut follow = s.watch(&sid).await;
    let mut ev = s.turn(&sid, "delegate it").await;
    let before = ev.until("permission").await;
    let start = before
        .iter()
        .find(|e| e.0 == "child_start")
        .map(|e| e.1.clone())
        .expect("child_start");
    let call = before
        .iter()
        .find(|e| e.0 == "tool_start" && e.1["name"] == "subagent")
        .map(|e| e.1["id"].clone())
        .unwrap();
    let kid = start["session"].as_str().unwrap().to_string();
    assert_ne!(kid, sid);
    assert_eq!(
        (&start["parent"], &start["depth"], &start["title"]),
        (&json!(sid), &json!(1), &json!("Run the check")),
        "{start}"
    );
    assert_eq!(
        (&start["agent"], &start["tool"]),
        (&json!("general"), &call)
    );
    let perm = &before.last().unwrap().1;
    assert_eq!(perm["session"], json!(kid), "{perm}");
    let rid = perm["id"].as_str().unwrap();
    let (code, _) = s
        .post(
            &format!("/sessions/{sid}/permissions/{rid}"),
            json!({"reply": "once"}),
        )
        .await;
    assert_eq!(code, 200);
    let rest = ev.until("done").await;
    let all: Vec<(String, Value)> = before.into_iter().chain(rest).collect();
    // The subagent's call names its session; the parent's calls do not.
    for (name, v) in &all {
        if name.starts_with("tool_") {
            let theirs = v["id"] != call;
            assert_eq!(v.get("session").is_some(), theirs, "{name} {v}");
            if theirs {
                assert_eq!(v["session"], json!(kid));
            }
        }
    }
    let shell = all
        .iter()
        .find(|e| e.0 == "tool_end" && e.1["name"] == "general › shell")
        .expect("the child's call");
    assert!(shell.1["output"].as_str().unwrap().contains("child-ran"));
    let of = |t: &str| -> String {
        all.iter()
            .filter(|e| e.0 == t)
            .map(|e| {
                assert_eq!(e.1["session"], json!(kid), "{e:?}");
                e.1["delta"].as_str().unwrap().to_string()
            })
            .collect()
    };
    assert_eq!(of("child_text"), "The check ran.");
    assert_eq!(of("child_reasoning"), "It printed.");
    let end = all.iter().find(|e| e.0 == "child_end").unwrap();
    assert_eq!(
        end.1,
        json!({"type": "child_end", "session": kid, "status": "completed", "error": null})
    );
    // In order: started, ran, ended, then the parent's result and answer.
    let at = |n: &str| all.iter().position(|e| e.0 == n).unwrap();
    assert!(at("child_start") < at("child_text") && at("child_text") < at("child_end"));
    let parent_end = all
        .iter()
        .position(|e| e.0 == "tool_end" && e.1["id"] == call)
        .unwrap();
    assert!(at("child_end") < parent_end);
    let text: String = all
        .iter()
        .filter(|e| e.0 == "text")
        .map(|e| e.1["delta"].as_str().unwrap())
        .collect();
    assert_eq!(text, "Delegated fine.");
    // The follower saw the same subagent events.
    let followed = follow.until("done").await;
    let kinds = |v: &[(String, Value)]| -> Vec<String> {
        v.iter()
            .map(|e| e.0.clone())
            .filter(|n| n.starts_with("child_") || n == "permission")
            .collect()
    };
    assert_eq!(kinds(&followed), kinds(&all));
    // The child session's messages are there for clients to read.
    let (code, info) = s.get(&format!("/sessions/{kid}")).await;
    assert_eq!(code, 200, "{info}");
    assert!(
        info["messages"].to_string().contains("The check ran."),
        "{info}"
    );
    m.assert_done();
}

impl Server {
    /// `GET /events`: the server's `filesystem.changed` stream.
    async fn fs_events(&self) -> Events {
        let r = self
            .req(reqwest::Method::GET, "/events")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        Events {
            body: Box::pin(r.bytes_stream()),
            buf: String::new(),
        }
    }
}

/// Wait for a `filesystem.changed` for `file`, rewriting it meanwhile (a
/// native watch arms asynchronously).
async fn fs_change(ev: &mut Events, file: &std::path::Path, write: impl Fn()) -> Value {
    let want = file.display().to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        write();
        if let Ok(Some((name, v))) =
            tokio::time::timeout(Duration::from_millis(300), ev.next()).await
        {
            if name == "filesystem.changed" && v["file"] == want {
                return v;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no filesystem.changed for {want}"
        );
    }
}

/// OpenCode's `fs.watch`: a watched file's changes, and the branch
/// metadata's, arrive on `GET /events` as `filesystem.changed`; only
/// existing files inside the directory can be watched.
#[tokio::test(flavor = "multi_thread")]
async fn watched_files_and_the_branch_publish_filesystem_changed() {
    need_git!();
    let e = Env::new();
    let m = Model::start(Script::new()).await;
    e.config(&m.url, json!({}));
    e.write("src/a.ts", "start\n");
    let s = Server::start(&e, &[]).await;
    let mut ev = s.fs_events().await;

    let (code, body) = s.post("/fs/watch", json!({"path": "missing.ts"})).await;
    assert_eq!(
        (code, body["error"].as_str()),
        (404, Some("File not found: missing.ts"))
    );
    let (code, _) = s.post("/fs/watch", json!({"path": "../outside.ts"})).await;
    assert_eq!(code, 404);
    let (code, _) = s.post("/fs/watch", json!({"path": "src/a.ts"})).await;
    assert_eq!(code, 200);

    let a = e.project.join("src/a.ts");
    let v = fs_change(&mut ev, &a, || {
        std::fs::write(&a, format!("{:?}\n", std::time::Instant::now())).unwrap()
    })
    .await;
    assert_eq!(v["event"], "change");
    assert_eq!(v["location"], e.project.display().to_string());

    let head = e.project.join(".git/HEAD");
    let v = fs_change(&mut ev, &head, || {
        std::fs::write(&head, "ref: refs/heads/main\n").unwrap()
    })
    .await;
    assert_eq!(v["type"], "filesystem.changed");

    let (code, _) = s.post("/fs/unwatch", json!({"path": "src/a.ts"})).await;
    assert_eq!(code, 200);
}

/// `CODESMITH_DISABLE_FILEWATCHER` refuses watches with 503.
#[tokio::test(flavor = "multi_thread")]
async fn file_watching_can_be_turned_off() {
    let e = Env::new();
    let m = Model::start(Script::new()).await;
    e.config(&m.url, json!({}));
    e.write("a.ts", "x\n");
    let s = Server::start_with(&e, &[], &[("CODESMITH_DISABLE_FILEWATCHER", "1")]).await;
    let (code, body) = s.post("/fs/watch", json!({"path": "a.ts"})).await;
    assert_eq!(
        (code, body["error"].as_str()),
        (503, Some("File watching is disabled"))
    );
}

/// The shell daemon's runtime directories (`$XDG_STATE_HOME/codesmith/shelld`).
fn daemons(e: &Env) -> Vec<std::path::PathBuf> {
    let root = e.home.join(".state").join("codesmith").join("shelld");
    std::fs::read_dir(root)
        .map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{what} never happened");
}

fn alive(pid: i64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The shell daemon across a graceful restart (OpenCode's
/// `shell-restart.test.ts` and "starts the daemon on the first command,
/// never on boot"): the server starts its daemon only when a command
/// runs; `POST /shell-daemon/handoff` hands it over and stops the server;
/// the next server, given the handoff, takes the session's command back,
/// and when it finishes the session is resumed with its output.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_command_survives_a_graceful_restart() {
    let e = Env::new();
    let notified = M::last("<shell id=\"shell_1\"");
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 3; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::step(1)).and(notified.clone().not()),
                Reply::text("The build is running."),
            )
            .then("resumed", M::tools().and(notified), Reply::text("It finished.")),
    )
    .await;
    e.config(&m.url, json!({}));
    let mut one = Server::start(&e, &["--auto"]).await;
    assert!(daemons(&e).is_empty(), "no daemon at boot");
    let (_, created) = one.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let turn = one.turn(&sid, "build it").await.until("done").await;
    assert_eq!(turn.last().unwrap().1["error"], Value::Null, "{turn:?}");
    assert_eq!(daemons(&e).len(), 1, "started by the first command");
    let (_, jobs) = one.get(&format!("/sessions/{sid}/jobs")).await;
    let pid = jobs["data"][0]["pid"].as_i64().unwrap();

    let (status, body) = one.post("/shell-daemon/handoff", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let handoff = body["handoff"].clone();
    assert!(handoff["ticket"].is_string(), "{body}");
    let exited = tokio::time::timeout(Duration::from_secs(10), one._child.wait())
        .await
        .expect("the server stops after handing over");
    assert!(exited.unwrap().success());
    assert!(alive(pid), "the command outlives the server");

    let two = Server::start_with(
        &e,
        &["--auto"],
        &[("CODESMITH_SHELLD_HANDOFF", &handoff.to_string())],
    )
    .await;
    until("the resumed turn", || !m.by("resumed").is_empty()).await;
    let last = crate::model::messages(&m.by("resumed")[0])
        .last()
        .cloned()
        .unwrap();
    assert!(crate::model::content(&last).contains("built-ok"), "{last}");
    assert_eq!(daemons(&e).len(), 1, "the same daemon");
    drop(two);
    // Its owner gone without a handoff, the daemon stops and cleans up.
    until("the daemon to stop", || daemons(&e).is_empty()).await;
    m.assert_done();
}

/// A server that ends without a handoff takes its commands with it: the
/// daemon stops everything once its owner is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_stopped_without_a_handoff_stops_its_commands() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Serve")
            .then(
                "start",
                M::tools().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 30", "description": "server", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::step(1)),
                Reply::text("Started."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &["--auto"]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    s.turn(&sid, "start it").await.until("done").await;
    let (_, jobs) = s.get(&format!("/sessions/{sid}/jobs")).await;
    let pid = jobs["data"][0]["pid"].as_i64().unwrap();
    assert!(alive(pid));
    drop(s);
    until("the command to stop", || !alive(pid)).await;
    until("the daemon to stop", || daemons(&e).is_empty()).await;
}

/// With `CODESMITH_SHELL_DAEMON=0` the server runs commands itself.
#[tokio::test(flavor = "multi_thread")]
async fn the_shell_daemon_can_be_turned_off() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Echo")
            .then(
                "run",
                M::tools().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "echo hi", "description": "echo"}),
                ),
            )
            .then("answer", M::tools().and(M::step(1)), Reply::text("Done.")),
    )
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start_with(&e, &["--auto"], &[("CODESMITH_SHELL_DAEMON", "0")]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    s.turn(&sid, "echo").await.until("done").await;
    assert!(daemons(&e).is_empty());
    let (_, body) = s.post("/shell-daemon/handoff", json!({})).await;
    assert_eq!(body, json!({"handoff": null}));
}

/// A foreground call whose command still runs when the server hands over
/// (`shell-restart.test.ts`, adapted): the call is interrupted with the
/// server, and the next server takes the command back as a background job
/// of the session, which is resumed with its output once it finishes.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreground_command_survives_a_graceful_restart() {
    let e = Env::new();
    let notified = M::last("<shell id=\"shell_1\"");
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                M::tools().and(M::step(0)).and(notified.clone().not()),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 3; echo fg-ok", "description": "build", "timeout": 60000}),
                ),
            )
            .then("resumed", M::tools().and(notified), Reply::text("It finished.")),
    )
    .await;
    e.config(&m.url, json!({}));
    let mut one = Server::start(&e, &["--auto"]).await;
    let (_, created) = one.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut ev = one.turn(&sid, "build it").await;
    ev.until("tool_start").await;
    let mut waited = Value::Null;
    for _ in 0..200 {
        let (_, jobs) = one.get(&format!("/sessions/{sid}/jobs")).await;
        if jobs["data"][0]["background"] == false {
            waited = jobs;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = waited["data"][0]["pid"]
        .as_i64()
        .expect("the call waits on it");
    let (_, body) = one.post("/shell-daemon/handoff", json!({})).await;
    let handoff = body["handoff"].clone();
    tokio::time::timeout(Duration::from_secs(10), one._child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(alive(pid), "nothing was stopped");

    let _two = Server::start_with(
        &e,
        &["--auto"],
        &[("CODESMITH_SHELLD_HANDOFF", &handoff.to_string())],
    )
    .await;
    until("the resumed turn", || !m.by("resumed").is_empty()).await;
    let last = crate::model::messages(&m.by("resumed")[0])
        .last()
        .cloned()
        .unwrap();
    assert!(crate::model::content(&last).contains("fg-ok"), "{last}");
}

/// `POST /sessions/{id}/fork` copies a session through its latest message,
/// or up to the one numbered `before` (from `seqs` in `GET /sessions/{id}`).
#[tokio::test(flavor = "multi_thread")]
async fn sessions_fork_over_http() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Served").always(
        "answer",
        M::tools(),
        Reply::text("Hi."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &[]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    // Nothing to fork yet.
    let (code, r) = s.post(&format!("/sessions/{sid}/fork"), json!({})).await;
    assert!(code == 400 || code == 404, "{code} {r}");

    let mut ev = s.turn(&sid, "hello").await;
    ev.until("done").await;
    let (_, shown) = s.get(&format!("/sessions/{sid}")).await;
    let seqs = shown["seqs"].as_array().unwrap();
    assert_eq!(seqs.len(), shown["messages"].as_array().unwrap().len());
    let first = seqs[0].as_i64().unwrap();

    let (code, forked) = s.post(&format!("/sessions/{sid}/fork"), json!({})).await;
    assert_eq!(code, 200, "{forked}");
    let fid = forked["id"].as_str().unwrap().to_string();
    assert_ne!(fid, sid);
    assert_eq!(forked["fork"]["session"], json!(sid));
    let (code, f) = s.get(&format!("/sessions/{fid}")).await;
    assert_eq!(code, 200);
    assert_eq!(f["messages"], shown["messages"]);

    let (code, before) = s
        .post(&format!("/sessions/{sid}/fork"), json!({"before": first}))
        .await;
    assert_eq!(code, 200, "{before}");
    assert_eq!(
        before["fork"]["boundary"],
        json!({"type": "before", "seq": first})
    );
    let bid = before["id"].as_str().unwrap();
    let (_, b) = s.get(&format!("/sessions/{bid}")).await;
    assert_eq!(b["messages"], json!([]));

    let (code, _) = s
        .post(&format!("/sessions/{sid}/fork"), json!({"before": 9999}))
        .await;
    assert_eq!(code, 404);
    let (code, _) = s.post("/sessions/ses_missing/fork", json!({})).await;
    assert_eq!(code, 404);
}

/// `POST /sessions/{id}/shell` runs a command as the user: answered when it
/// ends, recorded for the model's next step, no model call.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_run_user_shell_commands_over_http() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Served").always(
        "answer",
        M::tools().and(M::prompt("next")),
        Reply::text("Seen."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &[]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let (code, r) = s
        .post(
            &format!("/sessions/{sid}/shell"),
            json!({"command": "echo hi; exit 3"}),
        )
        .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(
        (r["state"].as_str(), r["exit"].as_i64()),
        (Some("completed"), Some(3))
    );
    assert!(r["output"].as_str().unwrap().contains("hi"), "{r}");
    assert!(m.steps().is_empty(), "no model call");
    let (code, _) = s.post(&format!("/sessions/{sid}/shell"), json!({})).await;
    assert_eq!(code, 400);

    let mut ev = s.turn(&sid, "next").await;
    ev.until("done").await;
    let step = m.by("answer").pop().unwrap();
    assert!(step.to_string().contains("Exit code: 3"), "{step}");
}

/// An MCP server's elicitation during a tool call reaches the turn's
/// stream as a question; the answer goes back to the server.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_elicitations_are_asked_as_questions_over_http() {
    let Some(py) = crate::harness::python() else {
        return;
    };
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../codesmith-core/tests/fixtures/mcp_elicit.py"
    );
    let e = Env::new();
    let ask = M::tools().and(M::prompt("ask me"));
    let m = Model::start(
        Script::new()
            .titled("Elicit")
            .then(
                "call",
                ask.clone().and(M::step(0)),
                Reply::call("elicit_form", json!({})),
            )
            .then("thanks", ask.and(M::step(1)), Reply::text("Thanks.")),
    )
    .await;
    e.config(
        &m.url,
        json!({"codemode": false, "mcp": {"servers": {"elicit": {"type": "local", "command": [py, fixture]}}}}),
    );
    let s = Server::start(&e, &[]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut ev = s.turn(&sid, "ask me").await;
    let seen = ev.until("question").await;
    let q = &seen.last().unwrap().1;
    assert_eq!(
        q["prompts"][0]["question"], "Who are you?\n\nGo ahead",
        "{q}"
    );
    let qid = q["id"].as_str().unwrap();
    let (code, _) = s
        .post(
            &format!("/sessions/{sid}/questions/{qid}"),
            json!({"answers": [["Yes"], ["Ava"]]}),
        )
        .await;
    assert_eq!(code, 200);
    let done = ev.until("done").await;
    assert_eq!(done.last().unwrap().1["error"], Value::Null, "{done:?}");
    let step = m.by("thanks").pop().unwrap().to_string();
    assert!(step.contains("Ava") && step.contains("accept"), "{step}");
}

/// `PATCH /sessions/{id} {archived}` archives a session or brings it back;
/// `GET /sessions?archived=` filters by it.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_archive_over_http() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Served").always(
        "answer",
        M::tools(),
        Reply::text("Hi."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let s = Server::start(&e, &[]).await;
    let (_, created) = s.post("/sessions", json!({})).await;
    let sid = created["id"].as_str().unwrap().to_string();
    let mut ev = s.turn(&sid, "hello").await;
    ev.until("done").await;
    let r = s
        .req(reqwest::Method::PATCH, &format!("/sessions/{sid}"))
        .json(&json!({"archived": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["archived"].is_i64(), "{body}");
    let ids = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    };
    let (_, all) = s.get("/sessions").await;
    assert_eq!(ids(&all), std::slice::from_ref(&sid));
    assert!(all[0]["archived"].is_i64());
    let (_, active) = s.get("/sessions?archived=false").await;
    assert!(ids(&active).is_empty());
    let (_, archived) = s.get("/sessions?archived=true").await;
    assert_eq!(ids(&archived), std::slice::from_ref(&sid));
    let r = s
        .req(reqwest::Method::PATCH, &format!("/sessions/{sid}"))
        .json(&json!({"archived": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let (_, active) = s.get("/sessions?archived=false").await;
    assert_eq!(ids(&active), std::slice::from_ref(&sid));
    let r = s
        .req(reqwest::Method::PATCH, "/sessions/ses_missing")
        .json(&json!({"archived": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}
