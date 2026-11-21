//! OpenCode's `mcp-attach.test.ts` (MIT): MCP servers a client attaches over
//! a connection it opened, stacked by name. Each client server here is an
//! in-process task on an in-memory pipe; it ends when its connection
//! closes, and with the test's runtime.

mod support;

use codesmith_core::mcp::{Mcp, NotFound, ServerConfig, Status, ToolCallError, SESSION_META};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Quiet, H};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// A client-hosted MCP server: its tools answer `label:tool` and record the
/// caller's session.
struct ClientServer {
    tools: Arc<Mutex<Vec<String>>>,
    sessions: Arc<Mutex<Vec<Value>>>,
    /// `tools/list` fails while set.
    fail_list: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    control: mpsc::UnboundedSender<Control>,
    /// The connection's other end, for [`ClientServer::attach`].
    ends: Mutex<
        Option<(
            tokio::io::ReadHalf<tokio::io::DuplexStream>,
            tokio::io::WriteHalf<tokio::io::DuplexStream>,
        )>,
    >,
}

enum Control {
    /// Add a tool and say so (`notifications/tools/list_changed`).
    Register(String),
    Close,
}

fn client_server(label: &str, tools: &[&str]) -> ClientServer {
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let (read, mut write) = tokio::io::split(theirs);
    let state = ClientServer {
        tools: Arc::new(Mutex::new(tools.iter().map(|t| t.to_string()).collect())),
        sessions: Arc::default(),
        fail_list: Arc::default(),
        connected: Arc::new(AtomicBool::new(true)),
        control: mpsc::unbounded_channel().0,
        ends: Mutex::new(Some(tokio::io::split(ours))),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let state = ClientServer {
        control: tx,
        ..state
    };
    let (label, tools, sessions, fail, connected) = (
        label.to_string(),
        state.tools.clone(),
        state.sessions.clone(),
        state.fail_list.clone(),
        state.connected.clone(),
    );
    tokio::spawn(async move {
        let mut lines = BufReader::new(read).lines();
        loop {
            let line = tokio::select! {
                l = lines.next_line() => match l {
                    Ok(Some(l)) => l,
                    _ => break,
                },
                c = rx.recv() => match c {
                    Some(Control::Register(t)) => {
                        tools.lock().unwrap().push(t);
                        let n = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
                        let _ = write.write_all(format!("{n}\n").as_bytes()).await;
                        continue;
                    }
                    _ => break,
                },
            };
            let m: Value = serde_json::from_str(&line).unwrap();
            let Some(id) = m.get("id").cloned() else {
                continue;
            };
            let result = match m["method"].as_str().unwrap_or("") {
                "initialize" => {
                    json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {"listChanged": true}}, "serverInfo": {"name": label, "version": "1.0.0"}})
                }
                "tools/list" if fail.load(Ordering::SeqCst) => {
                    let e = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "list failed"}});
                    let _ = write.write_all(format!("{e}\n").as_bytes()).await;
                    continue;
                }
                "tools/list" => {
                    let list: Vec<Value> = tools.lock().unwrap().iter().map(|t| json!({"name": t, "description": format!("{label} {t}"), "inputSchema": {"type": "object"}})).collect();
                    json!({"tools": list})
                }
                "tools/call" => {
                    let name = m["params"]["name"].as_str().unwrap_or("").to_string();
                    if !tools.lock().unwrap().contains(&name) {
                        let e = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": format!("Tool {name} not found")}});
                        let _ = write.write_all(format!("{e}\n").as_bytes()).await;
                        continue;
                    }
                    sessions
                        .lock()
                        .unwrap()
                        .push(m["params"]["_meta"][SESSION_META].clone());
                    json!({"content": [{"type": "text", "text": format!("{label}:{name}")}]})
                }
                _ => json!({}),
            };
            let r = json!({"jsonrpc": "2.0", "id": id, "result": result});
            if write.write_all(format!("{r}\n").as_bytes()).await.is_err() {
                break;
            }
        }
        connected.store(false, Ordering::SeqCst);
    });
    state
}

impl ClientServer {
    async fn attach(
        &self,
        mcp: &Arc<Mcp>,
        name: &str,
    ) -> anyhow::Result<codesmith_core::mcp::Attachment> {
        let (r, w) = self.ends.lock().unwrap().take().expect("attached once");
        mcp.attach(name, r, w).await
    }
    fn register(&self, tool: &str) {
        let _ = self.control.send(Control::Register(tool.into()));
    }
    fn close(&self) {
        let _ = self.control.send(Control::Close);
    }
    fn connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }
    fn sessions(&self) -> Vec<Value> {
        self.sessions.lock().unwrap().clone()
    }
}

/// The servers whose tools changed, in order.
fn changes(mcp: &Mcp) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut rx = mcp.subscribe();
    let out = seen.clone();
    tokio::spawn(async move {
        while let Ok(s) = rx.recv().await {
            out.lock().unwrap().push(s);
        }
    });
    seen
}

async fn wait_until(check: impl Fn() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("timed out waiting");
}

fn names(mcp: &Mcp) -> Vec<String> {
    mcp.tools().into_iter().map(|t| t.remote).collect()
}

fn registered(mcp: &Mcp, id: &str) -> bool {
    mcp.tools().iter().any(|t| t.name == id)
}

/// Call a tool by the name the model sees, for session `session`.
async fn call(mcp: &Mcp, name: &str, session: &str) -> String {
    let r = mcp
        .call_raw_for(name, json!({}), None, Some(session))
        .await
        .unwrap();
    r["content"][0]["text"].as_str().unwrap().to_string()
}

fn mcp() -> Arc<Mcp> {
    Mcp::at(std::path::Path::new("/"))
}

// mcp-attach.test.ts: "attaches a client MCP server and routes its tools with the session"
// (adapted) through a session: the model calls the tool in Code Mode, under the
// same permission rules and plugin hooks as a configured server's tools.
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_attaches_a_client_server_and_routes_its_tools_with_the_session() {
    let mut h = H::with(json!({"mcp": {"servers": {}}}), None).await;
    let mcp = h.mcp.clone().unwrap();
    let seen = changes(&mcp);
    let client = client_server("tui", &["open_terminal"]);
    let _a = client.attach(&mcp, "tui").await.unwrap();
    wait_until(|| seen.lock().unwrap().contains(&"tui".to_string())).await;
    let tools: Vec<(String, String)> = mcp
        .tools()
        .into_iter()
        .map(|t| (t.server, t.remote))
        .collect();
    assert_eq!(tools, [("tui".to_string(), "open_terminal".to_string())]);
    assert!(registered(&mcp, "tui_open_terminal"));

    h.prompt("call execute {\"code\": \"return await tools.tui.open_terminal({})\"}")
        .await;
    let sent = h.requests().last().unwrap()["messages"].to_string();
    assert!(sent.contains("tui:open_terminal"), "{sent}");
    assert_eq!(client.sessions(), [json!(h.s.id.clone())]);
}

// The attached server's tools obey the same rules: denied, they are not offered
// or called.
#[tokio::test(flavor = "multi_thread")]
async fn attached_tools_follow_the_configured_permission_rules() {
    let mut h = H::with(
        json!({"mcp": {"servers": {}}, "permissions": [{"action": "tui_*", "resource": "*", "effect": "deny"}]}),
        None,
    )
    .await;
    let mcp = h.mcp.clone().unwrap();
    let client = client_server("tui", &["open_terminal"]);
    let _a = client.attach(&mcp, "tui").await.unwrap();
    h.prompt("call execute {\"code\": \"return await tools.tui.open_terminal({})\"}")
        .await;
    h.s.prompt("call tui_open_terminal {}", &mut Quiet)
        .await
        .unwrap();
    assert_eq!(client.sessions(), Vec::<Value>::new());
    let tools = h.requests().last().unwrap()["tools"].to_string();
    assert!(!tools.contains("open_terminal"), "{tools}");
}

// mcp-attach.test.ts: "removes an attached entry when its scope closes"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_removes_an_attached_entry_when_its_scope_closes() {
    let mcp = mcp();
    let seen = changes(&mcp);
    let client = client_server("tui", &["open_terminal"]);
    let scope = client.attach(&mcp, "tui").await.unwrap();
    assert!(registered(&mcp, "tui_open_terminal"));
    wait_until(|| seen.lock().unwrap().len() == 1).await;

    drop(scope);
    assert!(mcp.tools().is_empty());
    wait_until(|| seen.lock().unwrap().len() == 2).await;
    assert!(!registered(&mcp, "tui_open_terminal"));
    let e = mcp
        .call_tool("tui", "open_terminal", json!({}), None)
        .await
        .unwrap_err();
    assert!(e.downcast_ref::<NotFound>().is_some(), "{e:#}");
    wait_until(|| !client.connected()).await;
}

// mcp-attach.test.ts: "fails an attach whose initial tool list fails and closes its transport"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_fails_an_attach_whose_initial_tool_list_fails_and_closes_its_transport() {
    let mcp = mcp();
    let first = client_server("first", &["open_terminal"]);
    let second = client_server("second", &["broken"]);
    second.fail_list.store(true, Ordering::SeqCst);
    let _a = first.attach(&mcp, "tui").await.unwrap();
    let e = second.attach(&mcp, "tui").await.err().unwrap();
    assert!(format!("{e:#}").contains("list failed"), "{e:#}");
    wait_until(|| !second.connected()).await;
    assert_eq!(names(&mcp), ["open_terminal"]);
}

// mcp-attach.test.ts: "routes an unknown tool on an attached-only name to the client"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_routes_an_unknown_tool_on_an_attached_only_name_to_the_client() {
    let mcp = mcp();
    let client = client_server("tui", &["open_terminal"]);
    let _a = client.attach(&mcp, "tui").await.unwrap();
    let e = mcp
        .call_tool("tui", "missing", json!({}), None)
        .await
        .unwrap_err();
    assert!(e.downcast_ref::<ToolCallError>().is_some(), "{e:#}");
    assert!(format!("{e:#}").contains("Tool missing not found"), "{e:#}");
}

// mcp-attach.test.ts: "removes an attached entry when the client closes the connection"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_removes_an_attached_entry_when_the_client_closes_the_connection() {
    let mcp = mcp();
    let seen = changes(&mcp);
    let client = client_server("tui", &["open_terminal"]);
    let _a = client.attach(&mcp, "tui").await.unwrap();
    wait_until(|| seen.lock().unwrap().len() == 1).await;

    client.close();
    wait_until(|| seen.lock().unwrap().len() == 2).await;
    assert!(mcp.tools().is_empty());
    assert!(!registered(&mcp, "tui_open_terminal"));
}

// mcp-attach.test.ts: "stacks a second attach under the same name and keeps the first connected"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_stacks_a_second_attach_under_the_same_name_and_keeps_the_first_connected() {
    let mcp = mcp();
    let first = client_server("first", &["old_tool", "shared"]);
    let second = client_server("second", &["shared"]);
    let _a = first.attach(&mcp, "tui").await.unwrap();
    assert!(registered(&mcp, "tui_old_tool"));
    let _b = second.attach(&mcp, "tui").await.unwrap();
    assert_eq!(names(&mcp), ["shared"]);
    assert!(!registered(&mcp, "tui_old_tool"));

    assert_eq!(
        call(&mcp, "tui_shared", "ses_client_mcp").await,
        "second:shared"
    );
    assert!(first.sessions().is_empty());
    assert!(first.connected());
}

// mcp-attach.test.ts: "falls back to the previous connection when the top one closes"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_falls_back_to_the_previous_connection_when_the_top_one_closes() {
    let mcp = mcp();
    let seen = changes(&mcp);
    let first = client_server("first", &["shared"]);
    let second = client_server("second", &["shared"]);
    let _a = first.attach(&mcp, "tui").await.unwrap();
    let _b = second.attach(&mcp, "tui").await.unwrap();
    // Changes on an idle connection stay hidden until it takes over.
    first.register("added");
    wait_until(|| seen.lock().unwrap().len() == 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(names(&mcp), ["shared"]);
    assert_eq!(seen.lock().unwrap().len(), 2);

    second.close();
    wait_until(|| seen.lock().unwrap().len() == 3).await;
    assert_eq!(names(&mcp), ["added", "shared"]);
    assert!(registered(&mcp, "tui_added"));
    assert_eq!(call(&mcp, "tui_shared", "s").await, "first:shared");
    assert_eq!(call(&mcp, "tui_added", "s").await, "first:added");
}

// mcp-attach.test.ts: "falls back to a connection with the same tools without publishing a change"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_falls_back_to_a_connection_with_the_same_tools_without_publishing_a_change() {
    let mcp = mcp();
    let seen = changes(&mcp);
    // Same label, so both list identical tools; the recorded sessions tell them apart.
    let first = client_server("same", &["shared"]);
    let second = client_server("same", &["shared"]);
    let marker = client_server("marker", &["done"]);
    let _a = first.attach(&mcp, "tui").await.unwrap();
    let scope = second.attach(&mcp, "tui").await.unwrap();
    wait_until(|| seen.lock().unwrap().len() == 2).await;

    drop(scope);
    call(&mcp, "tui_shared", "ses_fallback").await;
    assert_eq!(first.sessions(), [json!("ses_fallback")]);
    assert!(second.sessions().is_empty());
    // Give a fallback relist time to (wrongly) publish before the marker.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _m = marker.attach(&mcp, "marker").await.unwrap();
    wait_until(|| seen.lock().unwrap().contains(&"marker".to_string())).await;
    assert_eq!(*seen.lock().unwrap(), ["tui", "tui", "marker"]);
}

// mcp-attach.test.ts: "keeps the stored tools when the fallback re-list fails"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_keeps_the_stored_tools_when_the_fallback_re_list_fails() {
    let mcp = mcp();
    let seen = changes(&mcp);
    let first = client_server("first", &["kept"]);
    let second = client_server("second", &["newer"]);
    let _a = first.attach(&mcp, "tui").await.unwrap();
    let scope = second.attach(&mcp, "tui").await.unwrap();
    first.fail_list.store(true, Ordering::SeqCst);
    first.register("hidden");
    wait_until(|| seen.lock().unwrap().len() == 2).await;

    drop(scope);
    assert_eq!(names(&mcp), ["kept"]);
    wait_until(|| seen.lock().unwrap().len() == 3).await;
    assert_eq!(names(&mcp), ["kept"]);
    assert_eq!(call(&mcp, "tui_kept", "s").await, "first:kept");
}

// mcp-attach.test.ts: "refreshes the top connection's tools when it reports a change"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_refreshes_the_top_connections_tools_when_it_reports_a_change() {
    let mcp = mcp();
    let seen = changes(&mcp);
    let first = client_server("first", &["below"]);
    let second = client_server("second", &["top"]);
    let _a = first.attach(&mcp, "tui").await.unwrap();
    let _b = second.attach(&mcp, "tui").await.unwrap();
    wait_until(|| seen.lock().unwrap().len() == 2).await;

    second.register("added");
    wait_until(|| seen.lock().unwrap().len() == 3).await;
    assert_eq!(names(&mcp), ["added", "top"]);
    assert!(registered(&mcp, "tui_added"));
    assert_eq!(call(&mcp, "tui_added", "s").await, "second:added");
}

// mcp-attach.test.ts: "closing a connection below the top changes nothing visible"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_closing_a_connection_below_the_top_changes_nothing_visible() {
    let mcp = mcp();
    let seen = changes(&mcp);
    let first = client_server("first", &["old_tool"]);
    let second = client_server("second", &["shared"]);
    let scope = first.attach(&mcp, "tui").await.unwrap();
    let _b = second.attach(&mcp, "tui").await.unwrap();
    wait_until(|| seen.lock().unwrap().len() == 2).await;

    drop(scope);
    wait_until(|| !first.connected()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(names(&mcp), ["shared"]);
    assert_eq!(call(&mcp, "tui_shared", "s").await, "second:shared");
}

// mcp-attach.test.ts: "removes the entry once every stacked connection closes"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_removes_the_entry_once_every_stacked_connection_closes() {
    let mcp = mcp();
    let first = client_server("first", &["shared"]);
    let second = client_server("second", &["shared"]);
    let _a = first.attach(&mcp, "tui").await.unwrap();
    let _b = second.attach(&mcp, "tui").await.unwrap();
    assert!(registered(&mcp, "tui_shared"));

    second.close();
    first.close();
    wait_until(|| !first.connected() && !second.connected()).await;
    wait_until(|| !registered(&mcp, "tui_shared")).await;
    assert!(mcp.tools().is_empty());
    let e = mcp
        .call_tool("tui", "shared", json!({}), None)
        .await
        .unwrap_err();
    assert!(e.downcast_ref::<NotFound>().is_some(), "{e:#}");
}

// mcp-attach.test.ts: "hides attached entries from server status and keeps them across config reconciles"
// (adapted) adding and removing configured servers stands for a reconcile.
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_hides_attached_entries_from_server_status_and_keeps_them_across_config_reconciles()
{
    let mcp = mcp();
    let client = client_server("tui", &["open_terminal"]);
    let _a = client.attach(&mcp, "tui").await.unwrap();
    assert!(mcp.statuses().is_empty());

    // A configured server sharing the client's name neither shows the client nor captures its tools.
    let off: ServerConfig =
        serde_json::from_value(json!({"type": "local", "command": ["unused"], "disabled": true}))
            .unwrap();
    assert_eq!(mcp.add("tui", off.clone()).await, Status::Disabled);
    mcp.add("other", off).await;
    let statuses: Vec<(String, Status)> = mcp.statuses().into_iter().collect();
    assert_eq!(
        statuses,
        [
            ("other".to_string(), Status::Disabled),
            ("tui".to_string(), Status::Disabled)
        ]
    );
    mcp.remove("tui");
    assert_eq!(mcp.statuses().into_keys().collect::<Vec<_>>(), ["other"]);

    let tools: Vec<(String, String)> = mcp
        .tools()
        .into_iter()
        .map(|t| (t.server, t.remote))
        .collect();
    assert_eq!(tools, [("tui".to_string(), "open_terminal".to_string())]);
    let r = mcp
        .call_tool("tui", "open_terminal", json!({}), None)
        .await
        .unwrap();
    assert_eq!(
        r["content"],
        json!([{"type": "text", "text": "tui:open_terminal"}])
    );
}
