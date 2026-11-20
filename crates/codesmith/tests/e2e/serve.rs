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
        let mut args = vec!["serve", "--port", "0"];
        args.extend_from_slice(extra);
        let mut child = e.command(&args).spawn().unwrap();
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
    let all: Vec<&str> = all.into_iter().filter(|n| *n != "todos").collect();
    assert_eq!(
        all,
        [
            "step_end",
            "tool_start",
            "permission",
            "tool_end",
            "text",
            "step_end",
            "done"
        ]
    );
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
    // The aborted prompt is kept; its unfinished answer is not.
    let after = &m.by("after")[0];
    let roles: Vec<(String, String)> = crate::model::messages(after)
        .iter()
        .skip(1)
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
    e.config(&m.url, json!({}));
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
