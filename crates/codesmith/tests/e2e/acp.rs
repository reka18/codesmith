//! `codesmith acp` over the binary's stdin and stdout, as an editor drives
//! it: initialize, a prompt with a permission round trip and an output-limit
//! continuation, a cancel while a permission is open, a model switch, and a
//! replay in a fresh process after the first one exits.

use crate::harness::Env;
use crate::model::{self, Model, Reply, Script, M};
use codesmith::core::runner;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub struct Acp {
    child: tokio::process::Child,
    w: tokio::process::ChildStdin,
    r: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next: u64,
}

impl Acp {
    pub fn start(e: &Env) -> Acp {
        let mut c = e.command(&["acp"]);
        c.stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        let w = child.stdin.take().unwrap();
        let r = BufReader::new(child.stdout.take().unwrap()).lines();
        Acp {
            child,
            w,
            r,
            next: 0,
        }
    }
    pub async fn send(&mut self, v: Value) {
        self.w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        self.w.flush().await.unwrap();
    }
    pub async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }
    pub async fn call(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }
    pub async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(20), self.r.next_line())
            .await
            .expect("timed out waiting for the agent")
            .unwrap()
            .expect("the agent closed its output");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }
    /// Read until the response to `id`; `on_request` answers the agent's
    /// own requests (permissions). Returns the response and what came before.
    pub async fn until(
        &mut self,
        id: u64,
        mut on_request: impl FnMut(&Value) -> Option<Value>,
    ) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        loop {
            let m = self.recv().await;
            if m["id"] == json!(id) && m.get("method").is_none() {
                return (m, seen);
            }
            if m.get("method").is_some() && m.get("id").is_some() {
                if let Some(result) = on_request(&m) {
                    self.send(json!({"jsonrpc": "2.0", "id": m["id"], "result": result}))
                        .await;
                }
            }
            seen.push(m);
        }
    }
    pub async fn wait(&mut self, pred: impl Fn(&Value) -> bool) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        loop {
            let m = self.recv().await;
            if pred(&m) {
                return (m, seen);
            }
            seen.push(m);
        }
    }
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.call(method, params).await;
        let r = self.until(id, |_| None).await.0;
        assert!(r.get("error").is_none(), "{method}: {r}");
        r
    }
    pub async fn prompt(&mut self, sid: &str, text: &str) -> u64 {
        self.call(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
        )
        .await
    }
    /// Close stdin and wait for a clean exit.
    pub async fn close(mut self) {
        drop(self.w);
        let status = tokio::time::timeout(Duration::from_secs(20), self.child.wait())
            .await
            .expect("acp did not exit after stdin closed")
            .unwrap();
        assert!(status.success(), "{status:?}");
    }
}

/// The session/update kinds in `seen`, other messages by method.
pub fn kinds(seen: &[Value]) -> Vec<String> {
    seen.iter()
        .map(|m| match m["method"].as_str() {
            Some("session/update") => m["params"]["update"]["sessionUpdate"]
                .as_str()
                .unwrap()
                .to_string(),
            Some(other) => other.to_string(),
            None => "response".into(),
        })
        .collect()
}

pub fn updates<'a>(seen: &'a [Value], kind: &str) -> Vec<&'a Value> {
    seen.iter()
        .filter(|m| {
            m["method"] == "session/update" && m["params"]["update"]["sessionUpdate"] == kind
        })
        .map(|m| &m["params"]["update"])
        .collect()
}

fn texts(seen: &[Value], kind: &str) -> String {
    updates(seen, kind)
        .iter()
        .filter_map(|u| u["content"]["text"].as_str())
        .collect()
}

fn allow_once(m: &Value) -> Option<Value> {
    assert_eq!(m["method"], "session/request_permission", "{m}");
    Some(json!({"outcome": {"outcome": "selected", "optionId": "once"}}))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_editor_session_from_initialize_to_replay() {
    let e = Env::new();
    e.write("test.sh", "echo PASS\n");
    e.write("victim.txt", "keep me\n");
    let tests = M::tools().and(M::prompt("run the tests"));
    let nudged = M::last(runner::CONTINUE_AFTER_OUTPUT_LIMIT);
    let m = Model::start(
        Script::new()
            .titled("Run the tests")
            .then(
                "run",
                tests.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sh test.sh", "description": "Run tests"}),
                ),
            )
            .then(
                "cut",
                tests.clone().and(M::step(1)).and(nudged.clone().not()),
                Reply::text("The tests").finish("length"),
            )
            .then(
                "rest",
                tests.and(nudged),
                Reply::text(" pass.").usage(120, 5),
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
                "which",
                M::tools()
                    .and(M::prompt("which model"))
                    .and(M::f(|b| b["model"] == "other")),
                Reply::text("The other one."),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );

    let mut c = Acp::start(&e);
    let init = c
        .request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}}}),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], 1);
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true);
    let new = c
        .request("session/new", json!({"cwd": e.project, "mcpServers": []}))
        .await;
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

    // A prompt with a permission round trip and an output-limit continuation.
    let id = c.prompt(&sid, "run the tests").await;
    let mut asked = vec![];
    let (done, seen) = c
        .until(id, |r| {
            asked.push(r["params"]["toolCall"]["toolCallId"].clone());
            allow_once(r)
        })
        .await;
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    assert_eq!(asked.len(), 1);
    let calls = updates(&seen, "tool_call");
    assert_eq!(calls.len(), 1);
    assert_eq!(asked[0], calls[0]["toolCallId"]);
    let last = *updates(&seen, "tool_call_update").last().unwrap();
    assert_eq!(last["status"], "completed");
    assert!(last["content"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("PASS"));
    assert_eq!(texts(&seen, "agent_message_chunk"), "The tests pass.");
    let k: Vec<String> = kinds(&seen)
        .into_iter()
        .filter(|k| {
            k != "available_commands_update" && k != "usage_update" && k != "session_info_update"
        })
        .collect();
    assert_eq!(
        k,
        [
            "tool_call",
            "tool_call_update",
            "session/request_permission",
            "tool_call_update",
            "agent_message_chunk",
            "agent_message_chunk"
        ],
        "{seen:#?}"
    );
    let statuses: Vec<&Value> = updates(&seen, "tool_call_update")
        .iter()
        .map(|u| &u["status"])
        .collect();
    assert_eq!(statuses, [&json!("in_progress"), &json!("completed")]);
    // No engine text reaches the editor.
    assert!(!serde_json::to_string(&seen)
        .unwrap()
        .contains("output token limit"));

    // Cancel while the permission is open: nothing runs, the turn is cancelled.
    let id = c.prompt(&sid, "delete the victim").await;
    let (ask, _) = c
        .wait(|m| m["method"] == "session/request_permission")
        .await;
    c.notify("session/cancel", json!({"sessionId": sid})).await;
    let (done, seen) = c.until(id, |_| None).await;
    assert_eq!(done["result"]["stopReason"], "cancelled");
    assert!(seen.contains(
        &json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": ask["id"]}})
    ));
    assert_eq!(
        updates(&seen, "tool_call_update").last().unwrap()["status"],
        "failed"
    );
    assert_eq!(e.read("victim.txt"), "keep me\n");

    // Switch the model through the config option; the next turn uses it.
    let r = c
        .request(
            "session/set_config_option",
            json!({"sessionId": sid, "configId": "model", "value": "fake/other"}),
        )
        .await;
    let model_opt = r["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "model")
        .unwrap()
        .clone();
    assert_eq!(model_opt["currentValue"], "fake/other");
    let id = c.prompt(&sid, "which model are you?").await;
    let (done, seen) = c.until(id, |_| None).await;
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    assert_eq!(texts(&seen, "agent_message_chunk"), "The other one.");
    c.close().await;
    let info = e.store().get(&sid).unwrap().unwrap();
    assert_eq!(info.model, "fake/other");

    // A fresh process replays the session: only the real prompts as user
    // messages, every answer, and no engine nudges.
    let mut c = Acp::start(&e);
    c.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    )
    .await;
    let id = c
        .call(
            "session/load",
            json!({"cwd": e.project, "sessionId": sid, "mcpServers": []}),
        )
        .await;
    let (loaded, seen) = c.until(id, |_| None).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let users: Vec<&str> = updates(&seen, "user_message_chunk")
        .iter()
        .map(|u| u["content"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        users,
        ["run the tests", "delete the victim", "which model are you?"]
    );
    assert!(!serde_json::to_string(&seen)
        .unwrap()
        .contains("output token limit"));
    assert_eq!(
        texts(&seen, "agent_message_chunk"),
        "The tests pass.The other one."
    );
    let replayed: Vec<String> = kinds(&seen)
        .into_iter()
        .filter(|k| {
            k != "available_commands_update" && k != "usage_update" && k != "session_info_update"
        })
        .collect();
    assert_eq!(
        replayed,
        [
            "user_message_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk",
            "agent_message_chunk",
            "user_message_chunk",
            "tool_call",
            "tool_call_update",
            "user_message_chunk",
            "agent_message_chunk"
        ],
        "{seen:#?}"
    );
    let calls = updates(&seen, "tool_call_update");
    assert_eq!(calls[0]["status"], "completed");
    assert_eq!(calls[1]["status"], "failed");
    c.close().await;

    // What the model was sent: the cancelled call's result and the switch.
    let which = &m.by("which")[0];
    let r = model::results(which);
    assert!(r.is_empty());
    let msgs = model::messages(which);
    let tool_msgs: Vec<String> = msgs
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(model::content)
        .collect();
    assert_eq!(tool_msgs.len(), 2, "{tool_msgs:?}");
    assert!(tool_msgs[0].contains("PASS"));
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_reasons_for_turns_cut_short() {
    let e = Env::new();
    let nudged = M::last(runner::CONTINUE_AFTER_OUTPUT_LIMIT);
    let m = Model::start(
        Script::new()
            .titled("Limits")
            .always(
                "ramble",
                M::tools().and(M::prompt("ramble")),
                Reply::text("more").finish("length"),
            )
            .then(
                "filtered",
                M::tools().and(M::prompt("forbidden")),
                Reply::text("I won't").finish("content_filter"),
            )
            .then(
                "quota",
                M::tools().and(M::prompt("broke")),
                Reply::status(
                    429,
                    r#"{"error": {"message": "You exceeded your current quota", "code": "insufficient_quota"}}"#,
                ),
            )
            .then(
                "fine",
                M::tools().and(M::prompt("hello")),
                Reply::text("Hi."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let mut c = Acp::start(&e);
    c.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    )
    .await;
    let new = c
        .request("session/new", json!({"cwd": e.project, "mcpServers": []}))
        .await;
    let sid = new["result"]["sessionId"].as_str().unwrap().to_string();

    let id = c.prompt(&sid, "ramble").await;
    let (done, seen) = c.until(id, |_| None).await;
    assert_eq!(done["result"]["stopReason"], "max_tokens", "{done}");
    assert_eq!(texts(&seen, "agent_message_chunk"), "moremoremore");
    assert_eq!(m.by("ramble").len(), 3);
    assert_eq!(
        m.by("ramble")
            .iter()
            .filter(|b| nudged_last(b, &nudged))
            .count(),
        2
    );

    let id = c.prompt(&sid, "forbidden").await;
    let (done, _) = c.until(id, |_| None).await;
    assert_eq!(done["result"]["stopReason"], "refusal", "{done}");

    let id = c.prompt(&sid, "broke").await;
    let (done, _) = c.until(id, |_| None).await;
    assert!(
        done["error"]["message"].as_str().unwrap().contains("quota"),
        "{done}"
    );

    // The session still works, and the earlier turns' nudges and errors are
    // not replayed to the model as user prompts.
    let id = c.prompt(&sid, "hello").await;
    let (done, _) = c.until(id, |_| None).await;
    assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
    let users: Vec<String> = model::messages(&m.by("fine")[0])
        .iter()
        .filter(|m| m["role"] == "user" && !model::is_date(m))
        .map(model::content)
        .filter(|t| !codesmith::core::runner::is_engine_message(t))
        .collect();
    assert_eq!(users, ["ramble", "forbidden", "broke", "hello"]);
    c.close().await;
    m.assert_done();
}

fn nudged_last(b: &Value, nudged: &M) -> bool {
    (nudged.0)(b)
}
