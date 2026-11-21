//! Ports of OpenCode's MCP tests (`mcp.test.ts`, `tool-mcp-events.test.ts`
//! and `plugin/mcp-codemode-defaults.test.ts`, MIT) for the MCP roadmap
//! groups (output schemas, session recovery, resource blobs, events over
//! HTTP, the events tool, Code Mode defaults), with the security and edge
//! cases around them. Remote servers are played by a scripted HTTP server
//! on loopback answering JSON-RPC in its POST responses (as JSON or as an
//! event stream); sessions run against the fake model of `support`, with
//! the user's home, config and data kept out. Waits are bounded by
//! deadlines; nothing sleeps a fixed time.

#![allow(dead_code)]

mod support;

use codesmith_core::mcp::{self, Mcp, McpConfig};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Quiet, H};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEADLINE: Duration = Duration::from_secs(30);

// --- A scripted remote MCP server ---

/// One JSON-RPC message the server got, with its request headers.
#[derive(Debug, Clone)]
struct Got {
    headers: BTreeMap<String, String>,
    msg: Value,
}

impl Got {
    fn method(&self) -> &str {
        self.msg["method"].as_str().unwrap_or("")
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// How the server answers a message.
enum Http {
    Result(Value),
    Error(i64, &'static str),
    Status(u16),
    /// A result with extra response headers.
    Headed(Vec<(String, String)>, Value),
    /// An event stream carrying these messages (the request's id filled in
    /// where a message has `"id": null`).
    Sse(Vec<Value>),
}

type Handler = Arc<dyn Fn(&Got) -> Http + Send + Sync>;

struct Remote {
    url: String,
    seen: Arc<Mutex<Vec<Got>>>,
}

impl Remote {
    fn got(&self) -> Vec<Got> {
        self.seen.lock().unwrap().clone()
    }
    fn calls(&self, method: &str) -> Vec<Got> {
        self.got()
            .into_iter()
            .filter(|g| g.method() == method)
            .collect()
    }
}

fn init_result(capabilities: Value) -> Value {
    json!({
        "protocolVersion": "2025-06-18", "capabilities": capabilities,
        "serverInfo": {"name": "resources", "title": "MCP Resources", "version": "1"}
    })
}

/// The answers of a plain server with `tools`.
fn answer(g: &Got, tools: &Value) -> Http {
    match g.method() {
        "initialize" => Http::Result(init_result(json!({"tools": {}, "resources": {}}))),
        "tools/list" => Http::Result(json!({"tools": tools})),
        "tools/call" => Http::Result(json!({"content": [{"type": "text", "text": "echoed"}]})),
        _ => Http::Error(-32601, "unknown"),
    }
}

async fn remote(handler: Handler) -> Remote {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", l.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Got>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let (log, handler) = (log.clone(), handler.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head, body) = loop {
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
                            break (text[..i].to_string(), buf[i + 4..i + 4 + len].to_vec());
                        }
                    }
                };
                let headers = head
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                    .collect();
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let got = Got { headers, msg };
                log.lock().unwrap().push(got.clone());
                let reply = if got.msg.get("id").is_none() {
                    Http::Status(202)
                } else {
                    handler(&got)
                };
                let id = got.msg["id"].clone();
                let rpc = |r: Value| json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string();
                let (status, extra, ctype, body) = match reply {
                    Http::Result(r) => (200, vec![], "application/json", rpc(r)),
                    Http::Headed(h, r) => (200, h, "application/json", rpc(r)),
                    Http::Error(c, m) => (
                        200,
                        vec![],
                        "application/json",
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": c, "message": m}})
                            .to_string(),
                    ),
                    Http::Status(c) => (c, vec![], "application/json", String::new()),
                    Http::Sse(msgs) => {
                        let mut out = String::new();
                        for mut m in msgs {
                            if m.get("id").is_some_and(Value::is_null) {
                                m["id"] = id.clone();
                            }
                            m["jsonrpc"] = json!("2.0");
                            out.push_str(&format!("data: {m}\n\n"));
                        }
                        (200, vec![], "text/event-stream", out)
                    }
                };
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in extra {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&body);
                let _ = s.write_all(out.as_bytes()).await;
            });
        }
    });
    Remote { url, seen }
}

fn remote_config(url: &str, extra: Value) -> McpConfig {
    let mut server = json!({"type": "remote", "url": url, "oauth": false});
    for (k, v) in extra.as_object().unwrap() {
        server[k] = v.clone();
    }
    serde_json::from_value(json!({"servers": {"srv": server}})).unwrap()
}

async fn connect(url: &str, extra: Value) -> Arc<Mcp> {
    support::isolate();
    Mcp::connect(&remote_config(url, extra), Path::new("/")).await
}

/// A session with the remote server `srv` (in Code Mode unless `codemode`
/// is false), and `rules`.
async fn session_with(url: &str, codemode: bool, rules: Value) -> H {
    H::with(
        json!({"mcp": {"servers": {"srv": {"type": "remote", "url": url, "oauth": false, "codemode": codemode}}},
            "permissions": rules}),
        None,
    )
    .await
}

fn last_tool_result(h: &H) -> String {
    let req = h.requests().pop().unwrap();
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .unwrap()
}

async fn run_code(h: &mut H, code: &str) -> String {
    h.s.prompt(
        &format!("call execute {}", json!({ "code": code })),
        &mut Quiet,
    )
    .await
    .unwrap();
    last_tool_result(h)
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect()
}

// === MCP Output Schemas ===

fn schema(t: &str) -> Value {
    json!({"type": "object", "properties": {"value": {"type": t}}, "required": ["value"]})
}

// mcp.test.ts: "retains output schemas across paginated MCP discovery"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_retains_output_schemas_across_paginated_discovery() {
    let server = remote(Arc::new(|g: &Got| match g.method() {
        "tools/list" if g.msg["params"]["cursor"] == "page-2" => Http::Result(json!({"tools": [
            {"name": "second", "inputSchema": {"type": "object"}, "outputSchema": schema("number")}
        ]})),
        "tools/list" => Http::Result(json!({"tools": [
            {"name": "first", "inputSchema": {"type": "object"}, "outputSchema": schema("string")}
        ], "nextCursor": "page-2"})),
        _ => answer(g, &json!([])),
    }))
    .await;
    let m = connect(&server.url, json!({})).await;
    let tools: Vec<(String, Option<Value>)> = m
        .tools()
        .into_iter()
        .map(|t| (t.remote, t.output_schema))
        .collect();
    assert_eq!(
        tools,
        [
            ("first".to_string(), Some(schema("string"))),
            ("second".to_string(), Some(schema("number")))
        ]
    );
}

// Output schemas that are not objects, or too large to render, are left
// out; the tool stays. A union schema is kept.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_output_schemas_keep_objects_only_and_within_the_size_limit() {
    let huge = json!({"type": "object", "description": "x".repeat(mcp::MAX_INPUT_SCHEMA + 1)});
    let tools = json!([
        {"name": "none", "inputSchema": {"type": "object"}, "outputSchema": "none"},
        {"name": "null", "inputSchema": {"type": "object"}, "outputSchema": null},
        {"name": "huge", "inputSchema": {"type": "object"}, "outputSchema": huge},
        {"name": "union", "inputSchema": {"type": "object"}, "outputSchema": {"anyOf": [{"type": "object"}, {"type": "string"}]}},
        {"name": "plain", "inputSchema": {"type": "object"}}
    ]);
    let server = remote(Arc::new(move |g: &Got| answer(g, &tools))).await;
    let m = connect(&server.url, json!({})).await;
    let kept: BTreeMap<String, bool> = m
        .tools()
        .into_iter()
        .map(|t| (t.remote, t.output_schema.is_some()))
        .collect();
    assert_eq!(
        kept,
        BTreeMap::from([
            ("huge".into(), false),
            ("none".into(), false),
            ("null".into(), false),
            ("plain".into(), false),
            ("union".into(), true),
        ])
    );
}

fn schema_tools() -> Handler {
    let tools = json!([
        {"name": "search", "description": "Search", "inputSchema": {"type": "object"},
         "outputSchema": {"type": "object", "properties": {"ok": {"type": "boolean"}, "ünïcode": {"type": "string"}}, "required": ["ok"]}},
        {"name": "status", "description": "Status", "inputSchema": {"type": "object"}}
    ]);
    Arc::new(
        move |g: &Got| match (g.method(), g.msg["params"]["name"].as_str()) {
            ("tools/call", Some("search")) => match g.msg["params"]["arguments"]["mode"].as_str() {
                Some("error") => {
                    Http::Result(json!({"isError": true, "structuredContent": {"ok": false},
                    "content": [{"type": "text", "text": "index down"}]}))
                }
                Some("text-only") => Http::Result(
                    json!({"content": [{"type": "text", "text": "{\"ok\":\"text\"}"}]}),
                ),
                _ => Http::Result(json!({"structuredContent": {"ok": true, "ünïcode": "✓"},
                    "content": [{"type": "text", "text": "found it"}]})),
            },
            ("tools/call", Some("status")) => {
                Http::Result(json!({"structuredContent": {"ok": true},
                "content": [{"type": "text", "text": "hello"}]}))
            }
            _ => answer(g, &tools),
        },
    )
}

// mcp.test.ts: "advertises MCP output schemas to Code Mode" (adapted: the
// catalog is in the session's request; a program gets the structured
// content as the declared value)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_advertises_output_schemas_to_code_mode() {
    let server = remote(schema_tools()).await;
    let mut h = session_with(&server.url, true, json!([])).await;
    let ok = run_code(&mut h, "return (await tools.srv.search({})).ok").await;
    assert!(ok.starts_with("Result:\ntrue"), "{ok}");
    let req = h.requests()[0].to_string();
    assert!(
        req.contains("ok: boolean"),
        "the catalog shows the result type: {req}"
    );
    assert!(!tool_names(&h.requests()[0]).contains(&"srv_search".to_string()));
    let uni = run_code(&mut h, "return (await tools.srv.search({}))['ünïcode']").await;
    assert!(uni.contains('✓'), "{uni}");
    // A tool without a schema gets its text, as before.
    let status = run_code(&mut h, "return await tools.srv.status({})").await;
    assert!(status.contains("\"hello\""), "{status}");
}

// A declared schema does not hide a failure, and a result without
// structured content falls back to its text.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_output_schemas_fall_back_to_text_and_keep_failures() {
    let server = remote(schema_tools()).await;
    let mut h = session_with(&server.url, true, json!([])).await;
    let failed = run_code(
        &mut h,
        "try { await tools.srv.search({mode: 'error'}); return 'no' } catch (e) { return 'caught ' + e.message }",
    )
    .await;
    assert!(
        failed.contains("caught") && failed.contains("index down"),
        "{failed}"
    );
    let text = run_code(
        &mut h,
        "return (await tools.srv.search({mode: 'text-only'})).ok",
    )
    .await;
    assert!(text.contains("\"text\""), "{text}");
}

// Called directly (Code Mode off), the model reads the text as before.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_output_schemas_leave_direct_calls_as_text() {
    let server = remote(schema_tools()).await;
    let mut h = session_with(&server.url, false, json!([])).await;
    h.prompt("call srv_search {}").await;
    assert_eq!(last_tool_result(&h), "found it");
}

#[test]
fn mcp_structured_result() {
    let r = json!({"structuredContent": {"a": 1}, "content": []});
    assert_eq!(mcp::structured_result(&r, true), Some(json!({"a": 1})));
    assert_eq!(mcp::structured_result(&r, false), None);
    let failed = json!({"isError": true, "structuredContent": {"a": 1}});
    assert_eq!(mcp::structured_result(&failed, true), None);
    assert_eq!(mcp::structured_result(&json!({"content": []}), true), None);
    // Any value the server sent, unchecked.
    let odd = json!({"structuredContent": [1, "two"]});
    assert_eq!(mcp::structured_result(&odd, true), Some(json!([1, "two"])));
}

// === MCP Session Recovery ===

/// A server handing out sessions (`Mcp-Session-Id`) that it forgets on
/// `restart`, answering 404 to a request in a forgotten one. `fail_init`
/// makes initializing fail after the first; `forgetful` forgets every
/// session as soon as it is made after the first.
#[derive(Default)]
struct Sessions {
    live: Vec<String>,
    made: usize,
    processed: Vec<Value>,
    fail_init: bool,
    forgetful: bool,
}

fn session_server(state: Arc<Mutex<Sessions>>) -> Handler {
    Arc::new(move |g: &Got| {
        let mut st = state.lock().unwrap();
        if g.method() == "initialize" {
            if st.made > 0 && st.fail_init {
                return Http::Error(-32603, "cannot start");
            }
            st.made += 1;
            let id = format!("session-{}", st.made);
            if !(st.forgetful && st.made > 1) {
                st.live.push(id.clone());
            }
            return Http::Headed(
                vec![("Mcp-Session-Id".into(), id)],
                init_result(json!({"tools": {}, "resources": {}})),
            );
        }
        match g.header("mcp-session-id") {
            Some(s) if !st.live.iter().any(|l| l == s) => return Http::Status(404),
            _ => {}
        }
        match g.method() {
            "tools/list" => Http::Result(
                json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}),
            ),
            "tools/call" => {
                st.processed.push(g.msg["params"]["arguments"].clone());
                Http::Result(json!({"content": [{"type": "text", "text": "echoed"}]}))
            }
            "resources/read" => {
                Http::Result(json!({"contents": [{"uri": "docs://readme", "text": "# Readme"}]}))
            }
            _ => Http::Error(-32601, "unknown"),
        }
    })
}

fn restart(state: &Arc<Mutex<Sessions>>) {
    state.lock().unwrap().live.clear();
}

// mcp.test.ts: "reconnects and retries a tool call after the MCP session
// expires"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_reconnects_and_retries_a_tool_call_after_the_session_expires() {
    let state = Arc::new(Mutex::new(Sessions::default()));
    let server = remote(session_server(state.clone())).await;
    let m = connect(&server.url, json!({})).await;
    m.call_raw_for("srv_echo", json!({"n": 1}), None, None)
        .await
        .unwrap();
    assert_eq!(state.lock().unwrap().made, 1);
    // The restarted server does not know the client's session, so the next
    // request is answered 404.
    restart(&state);
    let r = m
        .call_raw_for("srv_echo", json!({"n": 2}), None, None)
        .await
        .unwrap();
    assert_eq!(mcp::result_text(&r), ("echoed".into(), false));
    let st = state.lock().unwrap();
    assert_eq!(st.processed, [json!({"n": 1}), json!({"n": 2})]);
    assert_eq!(st.made, 2);
    assert_eq!(m.statuses()["srv"], mcp::Status::Connected { tools: 1 });
    // The new session's requests carry its id.
    let last = server.calls("tools/call").pop().unwrap();
    assert_eq!(last.header("mcp-session-id"), Some("session-2"));
}

// Reading a resource recovers the same way.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_session_recovery_covers_resource_reads() {
    let state = Arc::new(Mutex::new(Sessions::default()));
    let server = remote(session_server(state.clone())).await;
    let m = connect(&server.url, json!({})).await;
    restart(&state);
    let text = m.read_resource("srv", "docs://readme").await.unwrap();
    assert_eq!(text, "# Readme");
    assert_eq!(state.lock().unwrap().made, 2);
}

// A server that forgets the new session too is tried once more, not again
// and again; one that cannot start a new session fails the call and is
// marked failed.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_session_recovery_retries_once() {
    let state = Arc::new(Mutex::new(Sessions {
        forgetful: true,
        ..Default::default()
    }));
    let server = remote(session_server(state.clone())).await;
    let m = connect(&server.url, json!({})).await;
    restart(&state);
    let e = m
        .call_raw_for("srv_echo", json!({}), None, None)
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("no longer knows this session"),
        "{e:#}"
    );
    assert!(state.lock().unwrap().processed.is_empty());
    // Initialized twice: the first session, and the one that recovery made
    // (whose tool list the forgetful server already refused).
    assert_eq!(state.lock().unwrap().made, 2);

    let state = Arc::new(Mutex::new(Sessions {
        fail_init: true,
        ..Default::default()
    }));
    let server = remote(session_server(state.clone())).await;
    let m = connect(&server.url, json!({})).await;
    restart(&state);
    let e = m
        .call_raw_for("srv_echo", json!({}), None, None)
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("starting a new one failed"),
        "{e:#}"
    );
    assert!(matches!(m.statuses()["srv"], mcp::Status::Failed(_)));
    assert_eq!(server.calls("initialize").len(), 2);
}

// Calls that find the session gone at the same time start one new session
// between them.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_session_recovery_starts_one_session_for_concurrent_calls() {
    let state = Arc::new(Mutex::new(Sessions::default()));
    let server = remote(session_server(state.clone())).await;
    let m = connect(&server.url, json!({})).await;
    restart(&state);
    let calls = (0..4).map(|n| {
        let m = m.clone();
        tokio::spawn(async move {
            m.call_raw_for("srv_echo", json!({"n": n}), None, None)
                .await
        })
    });
    for c in calls {
        tokio::time::timeout(DEADLINE, c)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    let st = state.lock().unwrap();
    assert_eq!(st.made, 2);
    assert_eq!(st.processed.len(), 4);
    drop(st);
    let _ = server;
}

// A 404 outside a session (a server that never gave one) is not taken for
// an expired session: nothing is initialized again.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_session_recovery_needs_a_session() {
    let server = remote(Arc::new(|g: &Got| match g.method() {
        "tools/call" => Http::Status(404),
        _ => answer(
            g,
            &json!([{"name": "echo", "inputSchema": {"type": "object"}}]),
        ),
    }))
    .await;
    let m = connect(&server.url, json!({})).await;
    let e = m
        .call_raw_for("srv_echo", json!({}), None, None)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("HTTP 404"), "{e:#}");
    assert_eq!(server.calls("initialize").len(), 1);
    assert_eq!(server.calls("tools/call").len(), 1);
}
