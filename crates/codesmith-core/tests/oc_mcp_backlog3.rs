//! Ports of OpenCode's MCP tests (`mcp.test.ts`, `mcp-stdio.test.ts`,
//! `mcp-events.test.ts` and `mcp-instructions.test.ts`, MIT) from the
//! porting backlog. Remote servers are played by a scripted local HTTP
//! server answering JSON-RPC in its POST responses; stdio servers by the
//! Python fixtures in `tests/fixtures` (skipped without Python), each of
//! which exits when its stdin closes. Sessions run against the fake model
//! of `support`, with the user's home, config and data kept out.

mod support;

use codesmith_ai::Part;
use codesmith_core::mcp::{self, Mcp, McpConfig, NotFound, ServerConfig, Status, ToolCallError};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{Quiet, H};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

// --- A scripted remote MCP server ---

/// One JSON-RPC message the server got, with the URL it was posted to.
#[derive(Debug, Clone)]
struct Got {
    url: String,
    msg: Value,
}

impl Got {
    fn method(&self) -> &str {
        self.msg["method"].as_str().unwrap_or("")
    }
}

/// How the server answers a message.
enum Http {
    Result(Value),
    Error(i64, &'static str),
    Status(u16),
    Later(Duration, Box<Http>),
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

/// The answers of a plain server: tools, prompts and resources as given.
fn answer(g: &Got, tools: &Value) -> Http {
    match g.method() {
        "initialize" => Http::Result(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}, "prompts": {}, "resources": {}},
            "serverInfo": {"name": "resources", "title": "MCP Resources", "version": "1"},
            "instructions": "Use the resources tools."
        })),
        "tools/list" => Http::Result(json!({"tools": tools})),
        "prompts/list" => Http::Result(json!({"prompts": []})),
        "tools/call" => Http::Result(json!({"content": [{"type": "text", "text": "echoed"}]})),
        _ => Http::Error(-32601, "unknown"),
    }
}

fn echo_tools() -> Value {
    json!([{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}])
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
                let (target, body) = loop {
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
                            let target = text
                                .lines()
                                .next()
                                .and_then(|l| l.split_whitespace().nth(1))
                                .unwrap_or("/")
                                .to_string();
                            break (target, buf[i + 4..i + 4 + len].to_vec());
                        }
                    }
                };
                let msg: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let got = Got { url: target, msg };
                log.lock().unwrap().push(got.clone());
                // A notification is accepted with no body.
                let mut reply = if got.msg.get("id").is_none() {
                    Http::Status(202)
                } else {
                    handler(&got)
                };
                while let Http::Later(d, inner) = reply {
                    tokio::time::sleep(d).await;
                    reply = *inner;
                }
                let id = got.msg["id"].clone();
                let (status, body) = match reply {
                    Http::Result(r) => (
                        200,
                        json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string(),
                    ),
                    Http::Error(c, m) => (
                        200,
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": c, "message": m}})
                            .to_string(),
                    ),
                    Http::Status(c) => (c, String::new()),
                    Http::Later(..) => unreachable!(),
                };
                let out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
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

fn connected(m: &Mcp) -> bool {
    matches!(m.statuses()["srv"], Status::Connected { .. })
}

// mcp.test.ts: "expose useful messages"
#[test]
fn oc_mcp_exposes_useful_messages() {
    assert_eq!(
        NotFound("demo".into()).to_string(),
        "MCP server \"demo\" not found"
    );
    let e = ToolCallError {
        server: "demo".into(),
        tool: "search".into(),
        message: "failed".into(),
    };
    assert_eq!(e.to_string(), "failed");
}

// mcp.test.ts: "MCP tool names match V1 sanitization"
#[test]
fn oc_mcp_tool_names_match_v1_sanitization() {
    assert_eq!(
        mcp::tool_name("context 7", "resolve.library/id"),
        "context_7_resolve_library_id"
    );
}

// mcp.test.ts: "passes session IDs as MCP request metadata" (adapted: no
// progress token)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_passes_session_ids_as_request_metadata() {
    let tools = echo_tools();
    let server = remote(Arc::new(move |g| answer(g, &tools))).await;
    let m = connect(&server.url, json!({})).await;
    assert!(connected(&m), "{:?}", m.statuses());
    m.call_raw_for(
        "srv_echo",
        json!({"text": "hello"}),
        None,
        Some("ses_mcp_metadata"),
    )
    .await
    .unwrap();
    m.call_raw_for("srv_echo", json!({}), None, None)
        .await
        .unwrap();
    let calls = server.calls("tools/call");
    assert_eq!(
        calls[0].msg["params"]["arguments"],
        json!({"text": "hello"})
    );
    assert_eq!(
        calls[0].msg["params"]["_meta"][mcp::SESSION_META],
        "ses_mcp_metadata"
    );
    assert_eq!(calls[1].msg["params"]["arguments"], json!({}));
    assert!(calls[1].msg["params"]["_meta"][mcp::SESSION_META].is_null());
}

// mcp.test.ts: "accepts legacy MCP tools whose output schema is not a root
// object"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_accepts_tools_whose_output_schema_is_not_a_root_object() {
    let tools = json!([{"name": "union", "inputSchema": {"type": "object"},
        "outputSchema": {"anyOf": [{"type": "object"}, {"type": "string"}]}}]);
    let server = remote(Arc::new(move |g| match g.method() {
        "tools/call" => {
            Http::Result(json!({"content": [], "structuredContent": {"kind": "struct"}}))
        }
        _ => answer(g, &tools),
    }))
    .await;
    let m = connect(&server.url, json!({})).await;
    assert_eq!(
        m.tools()
            .iter()
            .map(|t| t.remote.clone())
            .collect::<Vec<_>>(),
        ["union"]
    );
    let r = m
        .call_raw_for("srv_union", json!({}), None, None)
        .await
        .unwrap();
    assert_eq!(r["structuredContent"], json!({"kind": "struct"}));
    assert_eq!(mcp::result_text(&r).0, "{\"kind\":\"struct\"}");
}

// mcp.test.ts: "lists paginated prompts and invokes them through the MCP
// client"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_lists_paginated_prompts_and_invokes_them() {
    let tools = echo_tools();
    let server = remote(Arc::new(move |g| match g.method() {
        "prompts/list" if g.msg["params"]["cursor"] == "p2" => Http::Result(json!({
            "prompts": [{"name": "second", "description": "Second prompt"}]
        })),
        "prompts/list" => Http::Result(json!({
            "prompts": [{"name": "first", "description": "First prompt",
                "arguments": [{"name": "topic", "description": "Topic to explain", "required": true}]}],
            "nextCursor": "p2"
        })),
        "prompts/get" => Http::Result(json!({"messages": [{"role": "user",
            "content": {"type": "text", "text": g.msg["params"]["arguments"]["topic"]}}]})),
        _ => answer(g, &tools),
    }))
    .await;
    let m = connect(&server.url, json!({})).await;
    let prompts = m.prompts();
    assert_eq!(
        prompts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(prompts[0].description.as_deref(), Some("First prompt"));
    assert_eq!(prompts[0].arguments[0].name, "topic");
    assert!(prompts[0].arguments[0].required);
    assert!(prompts[1].arguments.is_empty());
    assert_eq!(
        m.prompt_text("srv", "first", "Effect").await.unwrap(),
        "Effect"
    );
}

// mcp.test.ts: "applies the configured MCP catalog timeout", "applies the
// configured MCP execution timeout", "... to prompts" and "applies
// configured MCP timeouts to resource operations"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_applies_the_configured_timeouts() {
    let slow = |method: &'static str| -> Handler {
        let tools = echo_tools();
        Arc::new(move |g: &Got| {
            if g.method() == method {
                Http::Later(Duration::from_secs(30), Box::new(Http::Result(json!({}))))
            } else {
                match g.method() {
                    "resources/list" => Http::Result(json!({"resources": []})),
                    "prompts/list" => Http::Result(json!({"prompts": [{"name": "slow"}]})),
                    _ => answer(g, &tools),
                }
            }
        })
    };
    let timeout = |catalog: u64, execution: u64| json!({"timeout": {"catalog": catalog, "execution": execution}});
    let started = Instant::now();
    // The catalog: listing tools at connection.
    let server = remote(slow("tools/list")).await;
    let m = connect(&server.url, timeout(100, 60_000)).await;
    assert!(
        matches!(&m.statuses()["srv"], Status::Failed(e) if e.contains("timed out")),
        "{:?}",
        m.statuses()
    );
    // Execution: a tool call and a prompt.
    let server = remote(slow("tools/call")).await;
    let m = connect(&server.url, timeout(60_000, 100)).await;
    let e = m
        .call_raw_for("srv_echo", json!({}), None, None)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("timed out"), "{e:#}");
    let server = remote(slow("prompts/get")).await;
    let m = connect(&server.url, timeout(60_000, 100)).await;
    let e = m.prompt_text("srv", "slow", "").await.unwrap_err();
    assert!(format!("{e:#}").contains("timed out"), "{e:#}");
    // Resources: listing under the catalog timeout, reading under the
    // execution one.
    let server = remote(slow("resources/list")).await;
    let m = connect(&server.url, timeout(100, 60_000)).await;
    let listed = m.list_resources(Some("srv")).await;
    assert!(format!("{listed:?}").contains("timed out"), "{listed:?}");
    let server = remote(slow("resources/read")).await;
    let m = connect(&server.url, timeout(60_000, 100)).await;
    let e = m.read_resource("srv", "test://slow").await.unwrap_err();
    assert!(format!("{e:#}").contains("timed out"), "{e:#}");
    assert!(started.elapsed() < Duration::from_secs(25));
}

// mcp.test.ts: "remote MCP code mode preference: ${entry.name}" (4 cases;
// adapted: the configured URL is dialed as given, whatever its query)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_dials_a_remote_servers_url_as_given_for_its_code_mode_preference() {
    for (query, codemode) in [
        ("", Some(false)),
        ("?codemode=true", None),
        ("?codemode=false", None),
        ("?codemode=off", None),
    ] {
        let tools = echo_tools();
        let server = remote(Arc::new(move |g| answer(g, &tools))).await;
        let url = format!("{}{query}", server.url);
        let extra = match codemode {
            Some(c) => json!({"codemode": c}),
            None => json!({}),
        };
        let m = connect(&url, extra).await;
        assert!(connected(&m), "{query}");
        assert_eq!(server.calls("initialize").len(), 1, "{query}");
        assert_eq!(server.calls("tools/list").len(), 1, "{query}");
        let urls: std::collections::BTreeSet<String> =
            server.got().into_iter().map(|g| g.url).collect();
        assert_eq!(
            urls.into_iter().collect::<Vec<_>>(),
            [format!("/mcp{query}")],
            "{query}"
        );
    }
}

// mcp.test.ts: "does not retry MCP beyond the query fallback:
// ${entry.name}" (11 cases; adapted: CodeSmith has no query fallback, so
// every refused connection is tried once, at the configured URL; a 401 is a
// server needing a login)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_does_not_retry_a_refused_connection() {
    for (status, query, codemode) in [
        (401, "", None),
        (403, "", None),
        (501, "", None),
        (404, "?codemode=true", None),
        (404, "?codemode=false", None),
        (404, "?codemode=", None),
        (404, "", Some(false)),
        (400, "?codemode=true", None),
        (400, "?codemode=false", None),
        (400, "?codemode=", None),
        (400, "", Some(false)),
    ] {
        let server = remote(Arc::new(move |_| Http::Status(status))).await;
        let url = format!("{}{query}", server.url);
        let extra = match codemode {
            Some(c) => json!({"codemode": c}),
            None => json!({}),
        };
        let m = connect(&url, extra).await;
        let st = m.statuses()["srv"].clone();
        if status == 401 {
            assert_eq!(st, Status::NeedsAuth);
        } else {
            assert!(matches!(st, Status::Failed(_)), "{status}{query}: {st:?}");
        }
        let got = server.got();
        assert_eq!(got.len(), 1, "{status}{query}: {got:?}");
        assert_eq!(got[0].url, format!("/mcp{query}"));
        assert_eq!(got[0].method(), "initialize");
    }
}

/// A server with resources: `docs://readme` reads as a text and a blob,
/// listed over two pages, with templates over two pages.
fn resources_server(capability: bool) -> Handler {
    let tools = echo_tools();
    Arc::new(move |g: &Got| match g.method() {
        "initialize" if !capability => Http::Result(json!({
            "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
            "serverInfo": {"name": "resources", "version": "1"}
        })),
        "resources/list" if g.msg["params"]["cursor"] == "r2" => Http::Result(json!({
            "resources": [{"name": "Logo", "uri": "docs://logo", "mimeType": "image/png"}]
        })),
        "resources/list" => Http::Result(json!({
            "resources": [{"name": "Readme", "uri": "docs://readme", "description": "Project docs"}],
            "nextCursor": "r2"
        })),
        "resources/templates/list" if g.msg["params"]["cursor"] == "t2" => Http::Result(json!({
            "resourceTemplates": [{"name": "Issue", "uriTemplate": "issue://{id}", "description": "Issue"}]
        })),
        "resources/templates/list" => Http::Result(json!({
            "resourceTemplates": [{"name": "File", "uriTemplate": "docs://{path}"}], "nextCursor": "t2"
        })),
        "resources/read" => Http::Result(json!({"contents": [
            {"uri": "docs://readme", "text": "hello", "mimeType": "text/plain"},
            {"uri": "docs://logo", "blob": "aGVsbG8=", "mimeType": "image/png"}
        ]})),
        _ => answer(g, &tools),
    })
}

// mcp.test.ts: "lists, reads, and reports MCP resource changes" and "loads
// and reads MCP resources" (adapted: listed afresh on each call, so there
// is no change to report)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_lists_and_reads_resources() {
    let server = remote(resources_server(true)).await;
    let m = connect(&server.url, json!({})).await;
    let listed = m.list_resources(Some("srv")).await.unwrap().to_string();
    for want in [
        "docs://readme",
        "Project docs",
        "docs://logo",
        "image/png",
        "docs://{path}",
        "issue://{id}",
    ] {
        assert!(listed.contains(want), "{want}: {listed}");
    }
    let read = m.read_resource("srv", "docs://readme").await.unwrap();
    assert!(read.contains("hello"), "{read}");
    assert!(read.contains("image/png"), "{read}");
    assert_eq!(server.calls("resources/list").len(), 2, "both pages");
    assert_eq!(server.calls("resources/templates/list").len(), 2);
    m.list_resources(Some("srv")).await.unwrap();
    assert_eq!(server.calls("resources/list").len(), 4, "listed again");
}

// mcp.test.ts: "skips MCP resource requests when the capability is absent"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_skips_resource_requests_without_the_capability() {
    let server = remote(resources_server(false)).await;
    let m = connect(&server.url, json!({})).await;
    assert!(m.resource_servers().is_empty());
    let _ = m.list_resources(None).await;
    let _ = m.read_resource("srv", "docs://readme").await;
    assert!(server.calls("resources/list").is_empty());
    assert!(server.calls("resources/templates/list").is_empty());
    assert!(server.calls("resources/read").is_empty());
}

// mcp.test.ts: "exposes every connection operation" (adapted: the 2025
// protocol; no resource change notifications, as resources are listed
// afresh)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_exposes_every_connection_operation() {
    let base = resources_server(true);
    let server = remote(Arc::new(move |g: &Got| match g.method() {
        "prompts/list" => Http::Result(json!({"prompts": [{"name": "greet", "arguments": [{"name": "name"}]}]})),
        "prompts/get" => Http::Result(json!({"messages": [{"role": "user", "content": {"type": "text",
            "text": format!("hi {}", g.msg["params"]["arguments"]["name"].as_str().unwrap_or(""))}}]})),
        _ => base(g),
    }))
    .await;
    let m = connect(&server.url, json!({})).await;
    assert_eq!(
        m.instructions(),
        [("srv".to_string(), "Use the resources tools.".to_string())]
    );
    assert_eq!(
        m.tools()
            .iter()
            .map(|t| t.remote.clone())
            .collect::<Vec<_>>(),
        ["echo"]
    );
    assert_eq!(
        m.prompts()
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>(),
        ["greet"]
    );
    assert!(m
        .list_resources(Some("srv"))
        .await
        .unwrap()
        .to_string()
        .contains("docs://readme"));
    assert!(m
        .read_resource("srv", "docs://readme")
        .await
        .unwrap()
        .contains("hello"));
    assert_eq!(
        m.prompt_text("srv", "greet", "bob").await.unwrap(),
        "hi bob"
    );
    m.call_raw_for("srv_echo", json!({"text": "hi"}), None, Some("ses_mcp_era"))
        .await
        .unwrap();
    let call = server.calls("tools/call").pop().unwrap();
    assert_eq!(call.msg["params"]["name"], "echo");
    assert_eq!(
        call.msg["params"]["_meta"][mcp::SESSION_META],
        "ses_mcp_era"
    );
}

// mcp.test.ts: "skips unusable tools and keeps the rest of the server's
// tools" (adapted: a tool without a name is the unusable one; a malformed
// schema field is kept as sent)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_skips_unusable_tools_and_keeps_the_rest() {
    let tools = json!([
        {"description": "no name", "inputSchema": {"type": "object"}},
        {"name": "bad-required", "inputSchema": {"type": "object", "required": "text"}},
        {"name": "bad-fields", "title": 7, "annotations": "x", "inputSchema": {"type": "object"}},
        {"name": "full", "title": "Full", "description": "Every field", "inputSchema": {"type": "object"},
         "annotations": {"readOnlyHint": true}, "_meta": {"example/key": 1}},
        {"name": "echo", "inputSchema": {"type": "object"}}
    ]);
    let server = remote(Arc::new(move |g| answer(g, &tools))).await;
    let m = connect(&server.url, json!({})).await;
    assert!(connected(&m), "{:?}", m.statuses());
    let names: Vec<String> = m.tools().iter().map(|t| t.remote.clone()).collect();
    assert_eq!(names, ["bad-required", "bad-fields", "full", "echo"]);
    for n in &names {
        m.call_raw_for(&mcp::tool_name("srv", n), json!({"text": "hi"}), None, None)
            .await
            .unwrap();
        let call = server.calls("tools/call").pop().unwrap();
        assert_eq!(
            (
                call.msg["params"]["name"].as_str(),
                &call.msg["params"]["arguments"]
            ),
            (Some(n.as_str()), &json!({"text": "hi"}))
        );
    }
}

// mcp.test.ts: "returns results that miss or break the called tool's
// output schema"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_returns_results_that_miss_or_break_the_output_schema() {
    let tools = json!([
        {"name": "delete", "inputSchema": {"type": "object"}, "outputSchema": {"type": "object", "properties": {}}},
        {"name": "collaborators", "inputSchema": {"type": "object"}, "outputSchema": {"type": "object",
            "properties": {"role": {"type": "string", "enum": ["owner", "editor"]}}, "required": ["role"]}}
    ]);
    let server = remote(Arc::new(move |g: &Got| match g.method() {
        "tools/call" => {
            let a = &g.msg["params"]["arguments"];
            if let Some(s) = a.get("structured") {
                Http::Result(json!({"content": [], "structuredContent": s}))
            } else if let Some(t) = a["text"].as_str() {
                Http::Result(json!({"content": [{"type": "text", "text": t}]}))
            } else {
                Http::Result(json!({"content": []}))
            }
        }
        _ => answer(g, &tools),
    }))
    .await;
    let m = connect(&server.url, json!({})).await;
    let call = |name: &str, args: Value| {
        let m = m.clone();
        let name = name.to_string();
        async move { m.call_raw_for(&name, args, None, None).await.unwrap() }
    };
    let deleted = call("srv_delete", json!({"text": "Meeting deleted."})).await;
    assert_eq!(
        mcp::result_text(&deleted),
        ("Meeting deleted.".to_string(), false)
    );
    let empty = call("srv_delete", json!({})).await;
    assert_eq!(mcp::result_text(&empty), (String::new(), false));
    let broken = call(
        "srv_collaborators",
        json!({"structured": {"role": "commenter"}}),
    )
    .await;
    assert_eq!(broken["structuredContent"], json!({"role": "commenter"}));
    assert!(!mcp::result_text(&broken).1);
    let names: Vec<String> = server
        .calls("tools/call")
        .iter()
        .map(|g| g.msg["params"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["delete", "delete", "collaborators"]);
}

// mcp.test.ts: "adds, disconnects, and reconnects MCP servers at runtime"
// (adapted: disconnecting is configuring it disabled, reconnecting a
// restart)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_adds_disconnects_and_reconnects_servers_at_runtime() {
    support::isolate();
    let tools = echo_tools();
    let server = remote(Arc::new(move |g| answer(g, &tools))).await;
    let m = Mcp::at(Path::new("/"));
    assert_eq!(m.restart("missing").await, None);
    let cfg = |disabled: bool| -> ServerConfig {
        serde_json::from_value(
            json!({"type": "remote", "url": server.url, "oauth": false, "disabled": disabled}),
        )
        .unwrap()
    };
    assert_eq!(
        m.add("dynamic", cfg(false)).await,
        Status::Connected { tools: 1 }
    );
    assert_eq!(m.tools().len(), 1);
    assert_eq!(m.add("dynamic", cfg(true)).await, Status::Disabled);
    assert!(m.tools().is_empty());
    assert_eq!(
        m.add("dynamic", cfg(false)).await,
        Status::Connected { tools: 1 }
    );
    assert_eq!(
        m.restart("dynamic").await,
        Some(Status::Connected { tools: 1 })
    );
    m.remove("dynamic");
    assert!(m.tools().is_empty());
    assert!(!m.statuses().contains_key("dynamic"));
}

// mcp.test.ts: "marks a remote MCP server with an invalid URL as failed"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_marks_a_remote_server_with_an_invalid_url_as_failed() {
    let tools = echo_tools();
    let server = remote(Arc::new(move |g| answer(g, &tools))).await;
    let m = connect(&server.url, json!({})).await;
    assert!(connected(&m));
    let broken: ServerConfig =
        serde_json::from_value(json!({"type": "remote", "url": "not a url", "oauth": false}))
            .unwrap();
    assert!(matches!(m.add("broken", broken).await, Status::Failed(_)));
    assert!(connected(&m), "the others stay connected");
    let fixed: ServerConfig =
        serde_json::from_value(json!({"type": "remote", "url": server.url, "oauth": false}))
            .unwrap();
    assert!(matches!(
        m.add("broken", fixed).await,
        Status::Connected { .. }
    ));
}

// mcp.test.ts: "flush waits for a server that connects within its startup
// window" (adapted: connecting waits for each server's startup)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_waits_for_a_server_that_connects_within_its_startup_window() {
    let tools = echo_tools();
    let server = remote(Arc::new(move |g: &Got| match g.method() {
        "initialize" => Http::Later(Duration::from_millis(300), Box::new(answer(g, &tools))),
        _ => answer(g, &tools),
    }))
    .await;
    let m = connect(&server.url, json!({"timeout": {"startup": 20_000}})).await;
    assert_eq!(m.statuses()["srv"], Status::Connected { tools: 1 });
}

// --- Through a session ---

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

fn content_tools() -> Handler {
    let tools = json!([
        {"name": "status", "inputSchema": {"type": "object"}},
        {"name": "issues", "inputSchema": {"type": "object"}},
        {"name": "count", "inputSchema": {"type": "object"}},
        {"name": "fail", "inputSchema": {"type": "object"}},
        {"name": "media", "inputSchema": {"type": "object"}}
    ]);
    Arc::new(
        move |g: &Got| match (g.method(), g.msg["params"]["name"].as_str()) {
            ("tools/call", Some("status")) => {
                Http::Result(json!({"content": [{"type": "text", "text": "hello"}]}))
            }
            ("tools/call", Some("issues")) => Http::Result(
                json!({"content": [{"type": "text", "text": "{\"issues\":[{\"id\":1}]}"}]}),
            ),
            ("tools/call", Some("count")) => {
                Http::Result(json!({"content": [{"type": "text", "text": "5"}]}))
            }
            ("tools/call", Some("fail")) => Http::Result(json!({"isError": true,
            "content": [{"type": "text", "text": "search index unavailable"}]})),
            ("tools/call", Some("media")) => Http::Result(json!({"content": [
                {"type": "text", "text": "rendered chart"},
                {"type": "image", "mimeType": "image/png",
                 "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="}
            ]})),
            _ => answer(g, &tools),
        },
    )
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

// mcp.test.ts: "returns content-only MCP results through Code Mode" and
// "parses JSON text results from MCP tools without an output schema"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_returns_content_and_parses_json_text_through_code_mode() {
    let server = remote(content_tools()).await;
    let mut h = session_with(&server.url, true, json!([])).await;
    assert!(run_code(&mut h, "return await tools.srv.status({})")
        .await
        .contains("\"hello\""));
    let id = run_code(&mut h, "return (await tools.srv.issues({})).issues[0].id").await;
    assert!(id.starts_with("Result:\n1\n"), "{id}");
    let kind = run_code(&mut h, "return typeof (await tools.srv.count({}))").await;
    assert!(kind.contains("\"string\""), "{kind}");
}

// mcp.test.ts: "advertises MCP tools directly when Code Mode is disabled
// for the server", with "parses JSON text results ..."'s outside-Code-Mode
// half: the model reads the original text
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_advertises_tools_directly_when_code_mode_is_disabled() {
    let server = remote(content_tools()).await;
    let mut h = session_with(&server.url, false, json!([])).await;
    h.prompt("call srv_issues {}").await;
    let req = h.requests()[0].clone();
    let names: Vec<String> = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"srv_issues".to_string()), "{names:?}");
    let execute = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "execute")
        .map(|t| t.to_string())
        .unwrap_or_default();
    assert!(!execute.contains("tools.srv"), "{execute}");
    assert_eq!(last_tool_result(&h), "{\"issues\":[{\"id\":1}]}");
}

/// The tool results in the session's stored history.
fn stored_results(h: &H) -> Vec<(String, bool, usize)> {
    h.store()
        .history(&h.s.id)
        .unwrap()
        .iter()
        .flat_map(|m| m.parts.clone())
        .filter_map(|p| match p {
            Part::ToolResult {
                content,
                is_error,
                media,
                ..
            } => Some((content, is_error, media.len())),
            _ => None,
        })
        .collect()
}

// mcp.test.ts: "fails the call when MCP reports isError" and "preserves MCP
// text and media content for the model"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_fails_on_is_error_and_keeps_text_and_media() {
    let server = remote(content_tools()).await;
    let mut h = session_with(&server.url, false, json!([])).await;
    h.prompt("call srv_fail {}").await;
    h.prompt("call srv_media {}").await;
    let results = stored_results(&h);
    assert_eq!(
        results,
        [
            ("search index unavailable".to_string(), true, 0),
            ("rendered chart".to_string(), false, 1),
        ]
    );
}

/// Records each question and answers with `reply`.
struct Asking {
    reply: codesmith_core::permission::Reply,
    asked: Mutex<Vec<String>>,
    calls_when_asked: Mutex<Vec<usize>>,
    server: Arc<Mutex<Vec<Got>>>,
}

impl codesmith_core::permission::Asker for Asking {
    fn ask(&self, req: &codesmith_core::permission::Request) -> codesmith_core::permission::Reply {
        self.asked.lock().unwrap().push(req.action.clone());
        let calls = self
            .server
            .lock()
            .unwrap()
            .iter()
            .filter(|g| g.method() == "tools/call")
            .count();
        self.calls_when_asked.lock().unwrap().push(calls);
        self.reply.clone()
    }
}

// mcp.test.ts: "waits for permission before calling an MCP tool", "does
// not call MCP when permission is blocked" and "returns permission
// feedback instead of calling MCP"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_asks_before_calling_and_calls_nothing_when_refused() {
    use codesmith_core::permission::Reply;
    for (reply, rule, called) in [
        (Reply::Once, "ask", true),
        (Reply::Once, "deny", false),
        (
            Reply::Reject(Some("use the cache instead".into())),
            "ask",
            false,
        ),
    ] {
        let server = remote(content_tools()).await;
        let mut h = session_with(
            &server.url,
            false,
            json!([{"action": "srv_status", "resource": "*", "effect": rule}]),
        )
        .await;
        let asking = Arc::new(Asking {
            reply: reply.clone(),
            asked: Mutex::default(),
            calls_when_asked: Mutex::default(),
            server: server.seen.clone(),
        });
        h.s = codesmith_core::session::Session::new(
            &h.loaded,
            h.root(),
            codesmith_core::session::Options {
                agent: None,
                model: None,
                resume: None,
                title: Some("t".into()),
                parent: None,
                id: None,
                questioner: None,
                mcp: h.mcp.clone(),
            },
            asking.clone(),
            Some(h.store()),
        )
        .unwrap();
        h.prompt("call srv_status {}").await;
        let calls = server.calls("tools/call").len();
        assert_eq!(calls, usize::from(called), "{rule} {reply:?}");
        if rule == "ask" {
            assert_eq!(*asking.asked.lock().unwrap(), ["srv_status"]);
            assert_eq!(*asking.calls_when_asked.lock().unwrap(), [0], "asked first");
        } else {
            assert!(asking.asked.lock().unwrap().is_empty());
        }
        let result = last_tool_result(&h);
        if let Reply::Reject(Some(why)) = &reply {
            assert!(result.contains(why.as_str()), "{result}");
        }
        if called {
            assert_eq!(result, "hello");
        }
    }
}

// mcp-instructions.test.ts: "renders instructions for servers with at
// least one reachable snapshot tool", "lists Code Mode servers as direct
// tools when the snapshot has no Code Mode catalog" and "keeps MCP
// instructions when Code Mode is disabled for the server and execute is
// denied"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_instructions_follow_the_reachable_tools() {
    let Some(py) = python() else { return };
    let rich = fixture("mcp_rich.py");
    let servers = |alpha_codemode: bool| {
        json!({"servers": {
            "alpha": {"type": "local", "command": [py, rich], "codemode": alpha_codemode},
            "beta": {"type": "local", "command": [py, rich]}
        }})
    };
    let system = |h: &H| {
        h.requests()[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // Beta's tools are denied: only alpha's instructions.
    let mut h = H::with(
        json!({"mcp": servers(true), "permissions": [{"action": "beta_*", "resource": "*", "effect": "deny"}]}),
        None,
    )
    .await;
    h.prompt("hello").await;
    let s = system(&h);
    assert!(s.contains("<server name=\"alpha\">\n    Use tools from this server through `execute` under `tools[\"alpha\"]`.\n    Always cite ticket ids."), "{s}");
    assert!(!s.contains("<server name=\"beta\">"), "{s}");
    // With execute denied, the servers' tools are direct, and so are their
    // instructions; one with Code Mode off keeps them too.
    for codemode in [true, false] {
        let deny = json!([{"action": "execute", "resource": "*", "effect": "deny"}]);
        let mut h = H::with(json!({"mcp": servers(codemode), "permissions": deny}), None).await;
        h.prompt("hello").await;
        let s = system(&h);
        assert!(
            s.contains("<server name=\"alpha\">\n    Always cite ticket ids."),
            "{codemode}: {s}"
        );
        assert!(!s.contains("through `execute`"), "{codemode}: {s}");
    }
}

// --- stdio ---

// mcp-stdio.test.ts: "resolves cwd from the workspace and expands ~ to the
// home directory"
#[test]
fn oc_mcp_stdio_resolves_cwd_from_the_workspace_and_expands_home() {
    let home = support::isolate().join("home");
    let home = dirs::home_dir().unwrap_or(home);
    let project = Path::new("/work/project");
    assert_eq!(mcp::resolve_cwd(project, None), project);
    assert_eq!(mcp::resolve_cwd(project, Some("~")), home);
    assert_eq!(
        mcp::resolve_cwd(project, Some("~/tools")),
        home.join("tools")
    );
    assert_eq!(
        mcp::resolve_cwd(project, Some("tools/server")),
        project.join("tools/server")
    );
    assert_eq!(
        mcp::resolve_cwd(project, Some("/elsewhere")),
        Path::new("/elsewhere")
    );
}

async fn frames() -> Option<Arc<Mcp>> {
    let py = python()?;
    support::isolate();
    let cfg: McpConfig = serde_json::from_value(json!({"servers": {"frames": {"type": "local",
        "command": [py, fixture("mcp_frames.py")]}}}))
    .unwrap();
    Some(Mcp::connect(&cfg, Path::new("/")).await)
}

const MIB: usize = 1 << 20;

// mcp-stdio.test.ts: "frames CRLF, blank lines, several messages per chunk
// and split UTF-8" (adapted: the server's initialize reply comes after a
// blank line, a whitespace line and a non-JSON line, in CRLF, sharing its
// write with a notification; a reply arrives a byte at a time)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_stdio_frames_crlf_blank_lines_several_messages_and_split_utf8() {
    let Some(m) = frames().await else { return };
    assert_eq!(m.statuses()["frames"], Status::Connected { tools: 1 });
    let r = m
        .call_raw_for("frames_frame", json!({"text": "héllo 🙂"}), None, None)
        .await
        .unwrap();
    assert_eq!(mcp::result_text(&r).0, "héllo 🙂");
}

// mcp-stdio.test.ts: "receives a frame between 10 and 16 MiB split into
// pipe-sized chunks" and "accepts a frame of exactly 16 MiB"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_stdio_receives_frames_up_to_16_mib() {
    let Some(m) = frames().await else { return };
    for size in [12 * MIB, 16 * MIB] {
        let r = m
            .call_raw_for(
                "frames_frame",
                json!({"size": size, "chunk": 65536}),
                None,
                None,
            )
            .await
            .unwrap();
        let text = mcp::result_text(&r).0;
        assert!(
            text.len() > size - 200 && text.bytes().all(|b| b == b'x'),
            "{size}"
        );
    }
}

// mcp-stdio.test.ts: "closes the connection on a line one byte over 16
// MiB" (adapted: the call fails and the server's process is still running)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_stdio_closes_the_connection_on_a_line_over_16_mib() {
    let Some(m) = frames().await else { return };
    let e = m
        .call_raw_for("frames_frame", json!({"size": 16 * MIB + 1}), None, None)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("larger than 16 MiB"), "{e:#}");
}

// mcp.test.ts: "terminates MCP descendants after the wrapper exits
// successfully" (adapted: the wrapper starts a descendant and runs the
// server; closing the connection kills its whole process group)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_terminates_descendants_of_a_stdio_server() {
    let Some(py) = python() else { return };
    support::isolate();
    let d = tempfile::tempdir().unwrap();
    let pid_file = d.path().join("child.pid");
    let script = format!(
        "trap '' TERM; sleep 300 & echo $! > '{}'; exec {py} '{}'",
        pid_file.display(),
        fixture("mcp_stdio.py")
    );
    let cfg: McpConfig = serde_json::from_value(json!({"servers": {"held": {"type": "local",
        "command": ["sh", "-c", script]}}}))
    .unwrap();
    let m = Mcp::connect(&cfg, Path::new("/")).await;
    assert!(
        matches!(m.statuses()["held"], Status::Connected { .. }),
        "{:?}",
        m.statuses()
    );
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks that the process exists.
    let alive = || unsafe { libc::kill(pid, 0) } == 0;
    assert!(alive());
    drop(m);
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let stopped = !alive();
    if !stopped {
        // SAFETY: as above; the test cleans up its own descendant.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(stopped, "the descendant was killed with its server");
}

// --- Events, from the rich fixture ---

struct Events {
    m: Arc<Mcp>,
    log: std::path::PathBuf,
    _d: tempfile::TempDir,
}

impl Events {
    fn client(&self) -> Arc<mcp::Client> {
        self.m.client("rich").unwrap()
    }
    fn sent(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    /// The plan file for one stream: its connections' steps.
    fn plan(&self, name: &str, steps: Value) -> String {
        let p = self._d.path().join(name);
        std::fs::write(&p, steps.to_string()).unwrap();
        p.display().to_string()
    }
    async fn until(&self, what: &str, mut done: impl FnMut(&[Value]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(&self.sent()) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn events() -> Option<Events> {
    let py = python()?;
    support::isolate();
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("sent.jsonl");
    let cfg: McpConfig = serde_json::from_value(json!({"servers": {"rich": {"type": "local",
        "command": [py, fixture("mcp_rich.py")], "environment": {"MCP_RICH_LOG": log.display().to_string()}}}}))
    .unwrap();
    let m = Mcp::connect(&cfg, Path::new("/")).await;
    Some(Events { m, log, _d: d })
}

fn cancelled(sent: &[Value]) -> usize {
    sent.iter()
        .filter(|m| m["method"] == "notifications/cancelled")
        .count()
}

// mcp-events.test.ts: "declares the extension and lists every page of event
// types"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_events_declare_the_extension_and_list_every_page() {
    let Some(e) = events().await else { return };
    let init = e
        .sent()
        .into_iter()
        .find(|m| m["method"] == "initialize")
        .unwrap();
    assert_eq!(
        init["params"]["capabilities"]["extensions"]["io.modelcontextprotocol/events"],
        json!({})
    );
    let types = e.client().list_events().await.unwrap();
    assert_eq!(
        types.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["ticket.created", "polled"]
    );
    assert_eq!(types[0].delivery, ["push"]);
    let lists: Vec<Value> = e
        .sent()
        .into_iter()
        .filter(|m| m["method"] == "events/list")
        .map(|m| m["params"]["cursor"].clone())
        .collect();
    assert_eq!(lists, [Value::Null, json!("page-2")]);
}

// mcp-events.test.ts: "delivers every message of a stream in order and ends
// when the server closes it"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_events_deliver_every_message_in_order_until_the_server_closes() {
    use codesmith_core::mcp::{EventMessage, StreamError};
    let Some(e) = events().await else { return };
    let plan = e.plan(
        "p.json",
        json!([{"events": [["e1", "c1", "one"]], "notes": [
            ["notifications/events/heartbeat", {"cursor": "c2"}],
            ["notifications/events/error", {"error": {"code": -32603, "message": "UpstreamError"}}],
            ["notifications/events/event", {"eventId": "e2", "name": "ticket.created", "timestamp": "2026-11-10T00:00:00Z", "data": {"title": "two"}}]
        ], "exit": true}]),
    );
    let mut s = e.client().stream_events(
        "ticket.created",
        Some(&json!({"queue": "plan", "plan": plan})),
        Some("c0"),
    );
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let next = tokio::time::timeout_at(deadline, s.next())
            .await
            .expect("in time");
        match next {
            Some(Ok(m)) => got.push(m),
            Some(Err(StreamError::Disconnected(_))) | None => break,
            Some(Err(other)) => panic!("{other}"),
        }
    }
    assert_eq!(
        got,
        [
            EventMessage::Active {
                cursor: Some("c0".into()),
                truncated: false
            },
            EventMessage::Event {
                id: "e1".into(),
                name: "ticket.created".into(),
                timestamp: "2026-11-19T10:00:00Z".into(),
                data: json!({"title": "one"}),
                cursor: Some("c1".into())
            },
            EventMessage::Heartbeat {
                cursor: Some("c2".into())
            },
            EventMessage::Error {
                code: -32603,
                message: "UpstreamError".into()
            },
            EventMessage::Event {
                id: "e2".into(),
                name: "ticket.created".into(),
                timestamp: "2026-11-10T00:00:00Z".into(),
                data: json!({"title": "two"}),
                cursor: None
            },
        ]
    );
    let opened = e
        .sent()
        .into_iter()
        .find(|m| m["method"] == "events/stream")
        .unwrap();
    assert_eq!(opened["params"]["cursor"], "c0");
    assert_eq!(opened["params"]["name"], "ticket.created");
}

// mcp-events.test.ts: "fails a stream the server terminates, and cancels
// its request"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_events_fail_a_stream_the_server_terminates_and_cancel_it() {
    use codesmith_core::mcp::StreamError;
    let Some(e) = events().await else { return };
    let plan = e.plan(
        "p.json",
        json!([{"notes": [["notifications/events/terminated", {"error": {"code": -32024, "message": "Forbidden"}}]]}]),
    );
    let mut s = e.client().stream_events(
        "ticket.created",
        Some(&json!({"queue": "plan", "plan": plan})),
        None,
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let ended = loop {
        match tokio::time::timeout_at(deadline, s.next())
            .await
            .expect("in time")
        {
            Some(Ok(_)) => continue,
            other => break other,
        }
    };
    match ended {
        Some(Err(StreamError::Terminated(m))) => assert!(m.contains("Forbidden"), "{m}"),
        other => panic!("{other:?}"),
    }
    drop(s);
    e.until("the cancellation", |sent| cancelled(sent) == 1)
        .await;
}

/// The id of the first event on `s`, within 30 seconds.
async fn first_event(s: &mut mcp::EventStream) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout_at(deadline, s.next())
            .await
            .expect("in time")
        {
            Some(Ok(mcp::EventMessage::Event { id, .. })) => return id,
            Some(Ok(_)) => continue,
            other => panic!("{other:?}"),
        }
    }
}

// mcp-events.test.ts: "routes concurrent streams by subscription id" and
// "cancels the request when the consumer stops reading"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_events_route_concurrent_streams_and_cancel_when_dropped() {
    let Some(e) = events().await else { return };
    let a = e.plan("a.json", json!([{"events": [["for-a", "c1", "a"]]}]));
    let b = e.plan("b.json", json!([{"events": [["for-b", "c1", "b"]]}]));
    let c = e.client();
    let mut sa = c.stream_events(
        "ticket.created",
        Some(&json!({"queue": "plan", "plan": a})),
        None,
    );
    let mut sb = c.stream_events(
        "ticket.created",
        Some(&json!({"queue": "plan", "plan": b})),
        None,
    );
    assert_eq!(first_event(&mut sb).await, "for-b");
    assert_eq!(first_event(&mut sa).await, "for-a");
    drop(sa);
    drop(sb);
    e.until("both cancellations", |sent| cancelled(sent) == 2)
        .await;
}

// mcp-events.test.ts: "offers no events when the server does not, or when
// the connection does not declare them"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_events_offers_none_when_the_server_does_not() {
    let Some(py) = python() else { return };
    support::isolate();
    let cfg: McpConfig = serde_json::from_value(json!({"servers": {"plain": {"type": "local",
        "command": [py, fixture("mcp_stdio.py")]}}}))
    .unwrap();
    let m = Mcp::connect(&cfg, Path::new("/")).await;
    let c = m.client("plain").unwrap();
    assert!(!c.has_events());
    let e = c.list_events().await.unwrap_err().to_string();
    assert!(e.contains("offers no events"), "{e}");
    let e = m
        .check_event("plain", "anything", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("offers no events"), "{e}");
}

// Edge cases of the cwd fix: only `~` and `~/` name the home folder;
// `~user`, `~~` and a `~` later in the path are folder names in the
// project, and `..` is kept as written.
#[test]
fn oc_mcp_stdio_expands_only_a_leading_home_tilde() {
    let home = dirs::home_dir().unwrap();
    let project = Path::new("/work/project");
    assert_eq!(mcp::resolve_cwd(project, Some("~/")), home.join(""));
    for c in ["~user/tools", "~~", "tools/~", "../sibling"] {
        assert_eq!(mcp::resolve_cwd(project, Some(c)), project.join(c), "{c}");
    }
}
