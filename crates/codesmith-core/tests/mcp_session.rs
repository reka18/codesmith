//! MCP beyond tools inside a session, against a scripted model: server
//! instructions in the system prompt, the resource tools, prompts as
//! commands, and an event automation created through the `automation` tool.

use codesmith_core::automation as au;
use codesmith_core::config::{Config, Loaded};
use codesmith_core::mcp::{Mcp, McpConfig};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Collect(String);
impl Sink for Collect {
    fn text(&mut self, d: &str) {
        self.0.push_str(d);
    }
}

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// A model that calls the tool named in the user's message (`call NAME ARGS`)
/// and then repeats what the tool said; it records every request.
async fn model(log: Arc<Mutex<Vec<Value>>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let log = log.clone();
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
                log.lock().unwrap().push(body.clone());
                let last = body["messages"].as_array().unwrap().last().unwrap().clone();
                let content = last["content"].as_str().unwrap_or("").to_string();
                let chunk = if body.get("tools").is_none() {
                    json!({"choices": [{"delta": {"content": "T"}, "finish_reason": "stop"}]})
                } else if let Some(rest) = content
                    .strip_prefix("call ")
                    .filter(|_| last["role"] == "user")
                {
                    let (name, args) = rest.split_once(' ').unwrap_or((rest, "{}"));
                    json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": name, "arguments": args}}]}, "finish_reason": "tool_calls"}]})
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

#[tokio::test(flavor = "multi_thread")]
async fn mcp_in_a_session() {
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = model(log.clone()).await;
    let fixture = format!("{}/tests/fixtures/mcp_rich.py", env!("CARGO_MANIFEST_DIR"));
    let config: Config = serde_json::from_value(json!({
        "model": "fake/m", "snapshots": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
        "mcp": {"servers": {"rich": {"type": "local", "command": [py, fixture]}}},
        "permissions": [{"action": "read_mcp_resource", "resource": "rich:memo://secret*", "effect": "deny"}]
    }))
    .unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: dir.path().to_path_buf(),
    };
    let mcp: Arc<Mcp> = Mcp::connect(&loaded.config.mcp, &loaded.root).await;
    let db = dir.path().join("s.db");
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
            mcp: Some(mcp.clone()),
        },
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(&db).unwrap()),
    )
    .unwrap();

    let mut out = Collect(String::new());
    s.prompt("call list_mcp_resources {}", &mut out)
        .await
        .unwrap();
    assert!(out.0.contains("memo://one"), "{}", out.0);
    assert!(out.0.contains("memo://{id}"), "templates are listed too");

    let req = log.lock().unwrap()[0].clone();
    let system = req["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("<mcp_instructions>\n  <server name=\"rich\">\n    Use tools from this server through `execute` under `tools[\"rich\"]`.\n    Always cite ticket ids.\n    Prefer memo resources.\n  </server>\n</mcp_instructions>"),
        "{system}"
    );
    let tools: Vec<&str> = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(tools.contains(&"list_mcp_resources") && tools.contains(&"read_mcp_resource"));

    let mut out = Collect(String::new());
    s.prompt(
        "call read_mcp_resource {\"server\": \"rich\", \"uri\": \"memo://one\"}",
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.0, "tool said: the first memo");
    let mut out = Collect(String::new());
    s.prompt(
        "call read_mcp_resource {\"server\": \"rich\", \"uri\": \"memo://secret-plans\"}",
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.0.to_lowercase().contains("denied"), "{}", out.0);

    // `/rich:review` sends the server's prompt.
    let c = mcp.commands()["rich:review"].clone();
    let text = s.expand_command(&c, "README.md").await.unwrap();
    assert_eq!(text, "Review README.md focusing on everything.");

    // The automation tool creates an event watch after checking the event.
    let mut out = Collect(String::new());
    s.prompt(
        "call automation {\"action\": \"create\", \"name\": \"tickets\", \"prompt\": \"triage\", \"trigger\": {\"type\": \"event\", \"server\": \"rich\", \"event\": \"ticket.created\", \"args\": {\"queue\": \"ops\"}}}",
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.0.contains("runs once per event"), "{}", out.0);
    let store = Store::open(&db).unwrap();
    let list = au::list(&store, None).unwrap();
    assert!(matches!(&list[0].trigger, au::Trigger::Event { server, .. } if server == "rich"));
    let mut out = Collect(String::new());
    s.prompt(
        "call automation {\"action\": \"create\", \"prompt\": \"x\", \"trigger\": {\"type\": \"event\", \"server\": \"rich\", \"event\": \"polled\"}}",
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.0.contains("No push event polled on rich"), "{}", out.0);
    assert_eq!(au::list(&store, None).unwrap().len(), 1);

    // Without resource servers the resource tools are not offered.
    let plain = Mcp::connect(&McpConfig::default(), Path::new("/")).await;
    assert!(plain.resource_servers().is_empty());
}

/// Fixed: a tool renamed after a name collision (`my srv` and `my_srv`
/// both offer `my_srv_echo`) escaped a rule denying the shared name, which
/// the user wrote before the collision. A deny on either name now wins.
#[tokio::test(flavor = "multi_thread")]
async fn a_deny_on_a_shared_name_still_denies_the_renamed_tools() {
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = model(log.clone()).await;
    let fixture = format!("{}/tests/fixtures/mcp_stdio.py", env!("CARGO_MANIFEST_DIR"));
    let config: Config = serde_json::from_value(json!({
        "model": "fake/m", "snapshots": false, "codemode": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
        "mcp": {"servers": {
            "my srv": {"type": "local", "command": [py, fixture]},
            "my_srv": {"type": "local", "command": [py, fixture]}
        }},
        "permissions": [{"action": "my_srv_echo", "resource": "*", "effect": "deny"}]
    }))
    .unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: dir.path().to_path_buf(),
    };
    let mcp: Arc<Mcp> = Mcp::connect(&loaded.config.mcp, &loaded.root).await;
    let echoes: Vec<String> = mcp
        .tools()
        .into_iter()
        .filter(|t| t.remote == "echo")
        .map(|t| t.name)
        .collect();
    assert_eq!(echoes.len(), 2);
    assert!(echoes.iter().all(|n| n != "my_srv_echo"), "{echoes:?}");
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
            mcp: Some(mcp.clone()),
        },
        Arc::new(Fixed(Reply::Once)),
        None,
    )
    .unwrap();
    for name in &echoes {
        let mut out = Collect(String::new());
        s.prompt(&format!("call {name} {{\"text\": \"hi\"}}"), &mut out)
            .await
            .unwrap();
        assert!(!out.0.contains("echo: hi"), "{name} ran: {}", out.0);
    }
    let req = log.lock().unwrap()[0].clone();
    let offered: Vec<&str> = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(offered.iter().all(|n| !n.contains("echo")), "{offered:?}");
    assert!(
        offered.iter().any(|n| n.contains("fail_now")),
        "{offered:?}"
    );
}

/// A local MCP server's environment is CodeSmith's without the local
/// server's token (OpenCode's `cli/test/env`, "session environment omits
/// server credentials", for MCP servers). Every test in this file spawns
/// children that must not see it, so setting it here is safe.
#[tokio::test(flavor = "multi_thread")]
async fn local_mcp_servers_do_not_inherit_the_server_token() {
    let Some(py) = python() else { return };
    std::env::set_var(codesmith_core::SERVER_TOKEN_VAR, "secret-token");
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("env.txt");
    let fixture = format!("{}/tests/fixtures/mcp_stdio.py", env!("CARGO_MANIFEST_DIR"));
    let wrapper = format!(
        "import os, runpy, sys\n\
         open({seen:?}, 'w').write('[' + os.environ.get('CODESMITH_SERVER_TOKEN', '') + '][' + os.environ.get('HOME', '') + ']')\n\
         sys.argv = [{fixture:?}]\n\
         runpy.run_path({fixture:?}, run_name='__main__')\n",
        seen = seen.display().to_string(),
    );
    let cfg: McpConfig = serde_json::from_value(json!({"servers": {
        "fake": {"type": "local", "command": [py, "-c", wrapper]}
    }}))
    .unwrap();
    let mcp = Mcp::connect(&cfg, dir.path()).await;
    assert!(!mcp.tools().is_empty(), "the server started");
    let home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        format!("[][{home}]")
    );
}

/// Fixed: a tool whose `<server>_<tool>` name is over 64 characters is
/// offered cut and hashed, and escaped a rule denying its whole name. A deny
/// on the whole name now wins too.
#[tokio::test(flavor = "multi_thread")]
async fn a_deny_on_a_long_name_still_denies_the_cut_tool() {
    let Some(py) = python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = model(log.clone()).await;
    let fixture = format!("{}/tests/fixtures/mcp_stdio.py", env!("CARGO_MANIFEST_DIR"));
    let server = format!("long-{}", "s".repeat(60));
    let config: Config = serde_json::from_value(json!({
        "model": "fake/m", "snapshots": false, "codemode": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
        "mcp": {"servers": {server.clone(): {"type": "local", "command": [py, fixture]}}},
        "permissions": [{"action": format!("{server}_echo"), "resource": "*", "effect": "deny"}]
    }))
    .unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: dir.path().to_path_buf(),
    };
    let mcp: Arc<Mcp> = Mcp::connect(&loaded.config.mcp, &loaded.root).await;
    let echo = mcp
        .tools()
        .into_iter()
        .find(|t| t.remote == "echo")
        .unwrap()
        .name;
    assert!(
        echo.len() <= codesmith_core::mcp::MAX_TOOL_NAME && echo != format!("{server}_echo"),
        "{echo}"
    );
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
            mcp: Some(mcp.clone()),
        },
        Arc::new(Fixed(Reply::Once)),
        None,
    )
    .unwrap();
    let mut out = Collect(String::new());
    s.prompt(&format!("call {echo} {{\"text\": \"hi\"}}"), &mut out)
        .await
        .unwrap();
    assert!(!out.0.contains("echo: hi"), "{echo} ran: {}", out.0);
    let req = log.lock().unwrap()[0].clone();
    let offered: Vec<&str> = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(!offered.contains(&echo.as_str()), "{offered:?}");
    let others = offered.iter().filter(|n| n.starts_with("long-")).count();
    assert_eq!(others, 1, "the server's other tool stays: {offered:?}");
}
