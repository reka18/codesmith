//! Ports of OpenCode's Code Mode tool placement tests
//! (`tool-model-policy.test.ts`): with `full_codemode`, glob, grep and
//! webfetch are hidden and strong models call the built-ins from inside
//! `execute`.

use codesmith_core::codemode::placement::{self, Model};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn policy(model: Option<&Model>, websearch: bool, full: bool) -> (Vec<String>, Vec<String>) {
    let r = placement::model_tools(model, websearch, full);
    (
        r.direct.into_iter().collect(),
        r.hidden.into_iter().collect(),
    )
}

fn m(provider: &str, id: &str) -> Model {
    Model::new(provider, id)
}

fn v(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn oc_model_tools_treats_anthropic_claude_and_claude_agent_models_as_strong_when_full_code_mode_is_on(
) {
    let models = [
        m("anthropic", "custom-sonnet"),
        m("claude-agent", "opus"),
        Model {
            canonical: Some("anthropic".into()),
            ..m("gateway", "sonnet")
        },
        Model {
            canonical: Some("claude-agent".into()),
            ..m("gateway", "opus")
        },
        m("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1"),
        Model {
            model_id: "claude-sonnet-4-5".into(),
            ..m("custom", "alias")
        },
        Model {
            family: Some("claude-sonnet".into()),
            ..m("custom", "sonnet-v4")
        },
    ];
    for model in &models {
        assert_eq!(
            policy(Some(model), true, true),
            (v(&[]), v(&["glob", "grep", "patch", "webfetch"])),
            "{model:?}"
        );
    }
}

#[test]
fn oc_model_tools_hides_glob_grep_and_webfetch_for_every_model_when_full_code_mode_is_on() {
    let models = [
        Some(m("openai", "gpt-5")),
        Some(m("anthropic", "claude-sonnet-4-5")),
        Some(m("deepseek", "deepseek-reasoner")),
        Some(m("zhipu", "glm-5.3")),
        Some(m("google", "gemini-4-neon")),
        None,
    ];
    for model in &models {
        let (_, hidden) = policy(model.as_ref(), true, true);
        for name in ["glob", "grep", "webfetch"] {
            assert!(hidden.contains(&name.to_string()), "{model:?}");
        }
    }
}

#[test]
fn oc_model_tools_keeps_non_strong_and_absent_models_direct_when_full_code_mode_is_on_hiding_websearch_when_unavailable(
) {
    assert_eq!(
        policy(Some(&m("deepseek", "deepseek-reasoner")), true, true),
        (
            v(&["edit", "read", "shell", "websearch", "write"]),
            v(&["glob", "grep", "patch", "webfetch"])
        )
    );
    assert_eq!(
        policy(None, true, true),
        (
            v(&["edit", "patch", "read", "shell", "websearch", "write"]),
            v(&["glob", "grep", "webfetch"])
        )
    );
    assert_eq!(
        policy(None, false, true),
        (
            v(&["edit", "patch", "read", "shell", "write"]),
            v(&["glob", "grep", "webfetch", "websearch"])
        )
    );
}

// --- Sessions: the tools each model is offered ---

type Log = Arc<Mutex<(VecDeque<String>, Vec<Value>)>>;

async fn model(log: Log) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let body: Value = loop {
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
                            break serde_json::from_slice(&buf[i + 4..i + 4 + len]).unwrap();
                        }
                    }
                };
                let reply = {
                    let mut l = log.lock().unwrap();
                    l.1.push(body);
                    l.0.pop_front()
                };
                let chunk = match reply {
                    Some(c) => c,
                    None => {
                        json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})
                            .to_string()
                    }
                };
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

struct Run {
    s: Session,
    log: Log,
    _dir: tempfile::TempDir,
}

async fn session(model_id: &str, extra: Value) -> Run {
    let log: Log = Arc::default();
    let base = model(log.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut v = json!({
        "model": format!("fake/{model_id}"), "snapshots": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    let config: Config = serde_json::from_value(v).unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: root.clone(),
        ..Default::default()
    };
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("Test".into()),
        parent: None,
        id: None,
        questioner: None,
        mcp: None,
    };
    let s = Session::new(&loaded, root, opts, Arc::new(Fixed(Reply::Once)), None).unwrap();
    Run { s, log, _dir: dir }
}

impl Run {
    async fn offered(&mut self) -> (Vec<String>, String) {
        self.s.prompt("hi", &mut Quiet).await.unwrap();
        let l = self.log.lock().unwrap();
        let body = l.1.last().unwrap();
        let names = body["tools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let system = body["messages"][0]["content"].to_string();
        (names, system)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_select_keeps_strong_model_tools_direct_when_full_codemode_is_off() {
    let (names, _) = session("gpt-5", json!({})).await.offered().await;
    for name in ["glob", "grep", "patch", "read", "shell", "webfetch"] {
        assert!(names.contains(&name.to_string()), "{name}: {names:?}");
    }
    assert!(
        !names.contains(&"execute".to_string()),
        "no Code Mode tools: {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_select_places_tools_per_model_under_full_codemode() {
    let full = json!({"full_codemode": true});
    // A strong model calls the built-ins from inside `execute`.
    let mut gpt = session("gpt-5", full.clone()).await;
    let (names, system) = gpt.offered().await;
    for name in [
        "glob", "grep", "webfetch", "read", "shell", "patch", "edit", "write",
    ] {
        assert!(!names.contains(&name.to_string()), "{name}: {names:?}");
    }
    assert!(names.contains(&"execute".to_string()), "{names:?}");
    for listed in ["tools.patch(", "tools.read(", "tools.shell("] {
        assert!(system.contains(listed), "{listed} is listed: {system}");
    }
    let (names, system) = session("claude-sonnet-4-5", full.clone())
        .await
        .offered()
        .await;
    assert!(names.contains(&"execute".to_string()) && !names.contains(&"read".to_string()));
    assert!(
        system.contains("tools.edit(") && system.contains("tools.write("),
        "{system}"
    );
    assert!(!system.contains("tools.patch("), "{system}");
    // Any other model keeps them direct; only glob and grep go.
    // Adapted: OpenCode hides webfetch too and offers an `execute` with an
    // empty catalog (whose instructions say not to call it). CodeSmith
    // offers no `execute` then, so webfetch stays: with no `execute` there
    // is no `fetch` global to replace it.
    let (names, _) = session("deepseek-reasoner", full.clone())
        .await
        .offered()
        .await;
    for name in ["edit", "read", "shell", "write", "webfetch"] {
        assert!(names.contains(&name.to_string()), "{name}: {names:?}");
    }
    for name in ["glob", "grep", "execute"] {
        assert!(!names.contains(&name.to_string()), "{name}: {names:?}");
    }
    // Without `execute`, the search and fetch tools stay direct.
    let denied = json!({"full_codemode": true, "permissions": [{"action": "execute", "resource": "*", "effect": "deny"}]});
    let (names, _) = session("gpt-5", denied).await.offered().await;
    for name in ["glob", "grep", "webfetch", "read", "shell"] {
        assert!(names.contains(&name.to_string()), "{name}: {names:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_strong_model_runs_a_built_in_from_inside_execute() {
    let mut r = session("gpt-5", json!({"full_codemode": true})).await;
    std::fs::write(r.s.cwd.join("notes.txt"), "hello from a file\n").unwrap();
    let code = "const t = await tools.read({ path: 'notes.txt' }); return t;";
    let call = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function",
        "function": {"name": "execute", "arguments": json!({"code": code}).to_string()}}]}, "finish_reason": "tool_calls"}]});
    r.log.lock().unwrap().0.push_back(call.to_string());
    r.s.prompt("read it", &mut Quiet).await.unwrap();
    let l = r.log.lock().unwrap();
    let sent = l.1.last().unwrap()["messages"].to_string();
    assert!(sent.contains("hello from a file"), "{sent}");
}

/// A local server for the program to fetch from: its base URL and the
/// paths it was asked for.
async fn page(body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let line = head.lines().next().unwrap_or("").to_string();
                log.lock().unwrap().push(line);
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

fn execute_call(code: &str) -> String {
    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function",
        "function": {"name": "execute", "arguments": json!({"code": code}).to_string()}}]}, "finish_reason": "tool_calls"}]})
    .to_string()
}

/// Under `full_codemode` webfetch is hidden, and a program fetches pages
/// with the `fetch` global instead, through the webfetch permission.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_fetches_with_the_fetch_global_under_full_codemode() {
    let (base, seen) = page(r#"{"title": "from the page"}"#).await;
    let rules = json!([
        {"action": "webfetch", "resource": format!("{base}/denied"), "effect": "deny"}
    ]);
    let mut r = session(
        "gpt-5",
        json!({"full_codemode": true, "permissions": rules}),
    )
    .await;
    let (names, _) = r.offered().await;
    assert!(!names.contains(&"webfetch".to_string()), "{names:?}");
    let code = format!(
        "const r = await fetch('{base}/page'); const j = await r.json(); \
         const d = await fetch('{base}/denied').then(() => 'fetched', e => e.message); \
         return [r.status, j.title, d]"
    );
    r.log.lock().unwrap().0.push_back(execute_call(&code));
    r.s.prompt("fetch it", &mut Quiet).await.unwrap();
    let l = r.log.lock().unwrap();
    let sent = l.1.last().unwrap()["messages"].to_string();
    assert!(sent.contains("from the page"), "{sent}");
    assert!(sent.contains("Permission denied"), "{sent}");
    assert_eq!(*seen.lock().unwrap(), ["GET /page HTTP/1.1"]);
}

/// An OpenAPI spec in config becomes `tools.<name>.<operation>` in Code
/// Mode; a call is an HTTP request with the configured credential.
#[tokio::test(flavor = "multi_thread")]
async fn configured_openapi_operations_are_code_mode_tools() {
    let (base, seen) = page(r#"{"id": "42", "name": "Ada"}"#).await;
    let spec = json!({
        "openapi": "3.1.0",
        "paths": {"/users/{id}": {"get": {
            "operationId": "users.get",
            "summary": "Get a user",
            "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}],
            "responses": {"200": {"description": "The user", "content": {"application/json": {"schema": {
                "type": "object", "properties": {"id": {"type": "string"}, "name": {"type": "string"}}
            }}}}}
        }}}
    });
    let config = json!({
        "openapi": {"api": {"spec": spec, "base_url": base}},
        "permissions": [{"action": "openapi", "resource": "*", "effect": "allow"}]
    });
    let mut r = session("deepseek-reasoner", config).await;
    let (names, system) = r.offered().await;
    assert!(names.contains(&"execute".to_string()), "{names:?}");
    assert!(system.contains("tools.api.users.get("), "{system}");
    r.log.lock().unwrap().0.push_back(execute_call(
        "const u = await tools.api.users.get({ id: '42' }); return u.name",
    ));
    r.s.prompt("who is 42", &mut Quiet).await.unwrap();
    let l = r.log.lock().unwrap();
    let sent = l.1.last().unwrap()["messages"].to_string();
    assert!(sent.contains("Ada"), "{sent}");
    assert_eq!(*seen.lock().unwrap(), ["GET /users/42 HTTP/1.1"]);
}
