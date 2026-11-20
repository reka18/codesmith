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
