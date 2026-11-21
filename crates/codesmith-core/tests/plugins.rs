//! The plugin hooks, end to end against a scripted model. Its own test
//! binary, because the plugin registry is process-wide.

use codesmith_ai::Usage;
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply, Request};
use codesmith_core::plugin::{
    self, BoxFuture, Event, Idle, Plugin, ShellCommand, Tool, ToolCall, ToolContext,
};
use codesmith_core::session::{Options, Session, Sink};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Seen {
    events: Mutex<Vec<String>>,
}

struct WordCount;
impl Tool for WordCount {
    fn name(&self) -> &str {
        "word_count"
    }
    fn description(&self) -> &str {
        "Count words in a file"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
    }
    fn execute<'a>(
        &'a self,
        ctx: ToolContext,
        input: Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let p = ctx
                .cwd
                .join(input["path"].as_str().ok_or("path is required")?);
            let text = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
            Ok(format!("{} words", text.split_whitespace().count()))
        })
    }
}

struct Test(Arc<Seen>);
impl Plugin for Test {
    fn id(&self) -> &str {
        "test"
    }
    fn config(&self, c: &mut Config, options: &Value) {
        let cmd = serde_json::from_value(
            json!({"template": format!("Say {}", options["word"].as_str().unwrap_or("hi"))}),
        )
        .unwrap();
        c.commands.insert("greet".into(), cmd);
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(WordCount)]
    }
    fn tool_after(&self, call: &ToolCall, output: &mut String, _is_error: &mut bool) {
        if call.name == "word_count" {
            output.push_str(" (counted by plugin)");
        }
    }
    fn tool_before(&self, call: &mut ToolCall) -> Result<(), String> {
        if call.input["command"]
            .as_str()
            .is_some_and(|c| c.contains("forbidden"))
        {
            return Err("blocked by the test plugin".into());
        }
        Ok(())
    }
    fn shell_before(&self, cmd: &mut ShellCommand) {
        cmd.env.insert("PLUGIN_VAR".into(), "from-plugin".into());
    }
    fn request(&self, _session: &str, req: &mut codesmith_ai::Request) {
        req.system.push_str("\nPLUGIN NOTE");
    }
    fn permission(&self, req: &Request) -> Option<Reply> {
        (req.action == "shell").then_some(Reply::Once)
    }
    fn event(&self, e: &Event) {
        let s = match e {
            Event::TurnStart { .. } => "turn_start".to_string(),
            Event::ToolStart { name, .. } => format!("tool_start:{name}"),
            Event::ToolEnd { name, is_error, .. } => format!("tool_end:{name}:{is_error}"),
            Event::StepEnd { .. } => "step".to_string(),
            Event::TurnEnd { error, .. } => format!("turn_end:{}", error.is_none()),
        };
        self.0.events.lock().unwrap().push(s);
    }
    fn idle(&self, idle: &Idle) -> Option<String> {
        (idle.answer == "finished" && idle.continued == 0).then(|| "continue please".to_string())
    }
}

/// A scripted OpenAI-compatible model on a raw TCP socket; records request bodies.
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
                let last = body["messages"].as_array().unwrap().last().unwrap().clone();
                let call = |name: &str, args: Value| json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": format!("c_{name}"), "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}]});
                let say = |t: &str| json!({"choices": [{"delta": {"content": t}, "finish_reason": "stop"}]});
                let chunk = if body.get("tools").is_none() {
                    say("T")
                } else if last["role"] == "user" && last["content"] == "go" {
                    call("word_count", json!({"path": "a.txt"}))
                } else if last["role"] == "tool" && last["tool_call_id"] == "c_word_count" {
                    call(
                        "shell",
                        json!({"command": "echo $PLUGIN_VAR", "description": "echo"}),
                    )
                } else if last["role"] == "tool" {
                    say("finished")
                } else if last["content"] == "continue please" {
                    say("really finished")
                } else {
                    say("?")
                };
                if body.get("tools").is_some() {
                    log.lock().unwrap().push(body);
                }
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

struct Collect(String);
impl Sink for Collect {
    fn text(&mut self, d: &str) {
        self.0.push_str(d);
    }
}

fn loaded(dir: &std::path::Path, base: &str, extra: Value) -> Loaded {
    let mut v = json!({
        "model": "fake/m", "snapshots": false,
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": base, "apiKey": "x"}}},
        "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}],
        "plugins": {"test": {"word": "hello"}}
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    let mut config: Config = serde_json::from_value(v).unwrap();
    plugin::active().transform(&mut config);
    Loaded {
        config,
        files: vec![],
        root: dir.to_path_buf(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_hook_runs() {
    let seen = Arc::new(Seen::default());
    plugin::install(vec![Arc::new(Test(seen.clone()))]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one two three").unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = model(log.clone()).await;

    let l = loaded(dir.path(), &base, json!({}));
    assert_eq!(
        l.config.commands["greet"].template, "Say hello",
        "config transform with options"
    );
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("t".into()),
        parent: None,
        id: None,
        questioner: None,
        mcp: None,
    };
    let mut s = Session::new(
        &l,
        dir.path().to_path_buf(),
        opts,
        Arc::new(Fixed(Reply::Reject(None))),
        None,
    )
    .unwrap();
    let mut out = Collect(String::new());
    s.prompt("go", &mut out).await.unwrap();
    assert_eq!(
        out.0, "finishedreally finished",
        "the idle hook continued the turn once"
    );

    let reqs = log.lock().unwrap().clone();
    assert!(
        reqs[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .ends_with("PLUGIN NOTE"),
        "request hook edits the system prompt"
    );
    assert!(
        reqs[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "word_count"),
        "plugin tools are offered"
    );
    let tool_msgs: Vec<String> = reqs.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        tool_msgs[0], "3 words (counted by plugin)",
        "plugin tool ran and tool_after edited it"
    );
    assert!(
        tool_msgs[1].contains("from-plugin"),
        "shell ran: the permission hook approved it and shell_before set the env: {}",
        tool_msgs[1]
    );

    let ev = seen.events.lock().unwrap().clone();
    assert_eq!(ev.first().map(String::as_str), Some("turn_start"));
    assert!(
        ev.contains(&"tool_end:word_count:false".to_string())
            && ev.contains(&"tool_end:shell:false".to_string())
    );
    assert_eq!(ev.last().map(String::as_str), Some("turn_end:true"));
    assert_eq!(ev.iter().filter(|e| *e == "step").count(), 4);

    // Turned off in config: no tool, no note, no transform.
    log.lock().unwrap().clear();
    let l = loaded(dir.path(), &base, json!({"plugins": {"test": false}}));
    assert!(!l.config.commands.contains_key("greet"));
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("t".into()),
        parent: None,
        id: None,
        questioner: None,
        mcp: None,
    };
    let mut s = Session::new(
        &l,
        dir.path().to_path_buf(),
        opts,
        Arc::new(Fixed(Reply::Reject(None))),
        None,
    )
    .unwrap();
    let _ = s.prompt("hello there", &mut Collect(String::new())).await;
    let reqs = log.lock().unwrap().clone();
    assert!(!reqs[0]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("PLUGIN NOTE"));
    assert!(!reqs[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["name"] == "word_count"));
    assert!(
        plugin::active()
            .permission(&Request {
                action: "shell".into(),
                resources: vec![],
                save: vec![],
                title: String::new()
            })
            .is_none(),
        "a disabled plugin does not answer permission prompts"
    );
    let mut sc = ShellCommand {
        session_id: String::new(),
        cwd: dir.path().to_path_buf(),
        command: "x".into(),
        env: Default::default(),
    };
    plugin::active().shell_before(&mut sc);
    assert!(sc.env.is_empty(), "nor change shell commands");
    let _ = Usage::default();
}
