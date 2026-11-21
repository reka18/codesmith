//! Tool input validation in a session (OpenCode's `tool/runtime.ts`
//! decodes a call's input against its schema before the tool runs; MIT):
//! built-in, MCP (direct and from Code Mode) and plugin tools get input
//! that failed its schema back as the call's error, and never run with it.
//! The validator itself is ported in `src/tool/input_schema.rs`; see
//! `crates/codesmith-core/PARITY.md`.

mod plugin_support;
mod support;

use codesmith_core::plugin::{self, BoxFuture, Plugin, Tool, ToolContext};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// The results of the latest step's calls: (name, content, is_error).
fn results(h: &support::H) -> Vec<(String, String, bool)> {
    h.s.messages
        .iter()
        .rev()
        .find_map(|m| {
            let r: Vec<(String, String, bool)> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    codesmith_ai::Part::ToolResult {
                        name,
                        content,
                        is_error,
                        ..
                    } => Some((name.clone(), content.clone(), *is_error)),
                    _ => None,
                })
                .collect();
            (!r.is_empty()).then_some(r)
        })
        .unwrap()
}

/// A built-in tool's call that fails its schema is answered with the issues
/// and the arguments, and the tool does not run; the same call with valid
/// input then does.
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_runtime_rejects_invalid_built_in_input_before_running_the_tool() {
    let mut h = support::H::new().await;
    h.prompt(r#"call write {"path": "out.txt", "content": 5}"#)
        .await;
    let r = results(&h);
    assert_eq!(
        r[0].1,
        "Invalid arguments for tool \"write\":\n- content: Expected string\n\nArguments provided:\n{\n  \"content\": 5,\n  \"path\": \"out.txt\"\n}\n\nUpdate the arguments and call the tool again."
    );
    assert!(r[0].2);
    assert!(!h.project.path().join("out.txt").exists());
    // Repair still runs first: a stringified root object is valid input.
    h.prompt(r#"call write "{\"path\": \"out.txt\", \"content\": \"ok\"}""#)
        .await;
    assert!(!results(&h)[0].2, "{:?}", results(&h));
    assert_eq!(
        std::fs::read_to_string(h.project.path().join("out.txt")).unwrap(),
        "ok"
    );
    // A missing required key.
    h.prompt(r#"call write {"path": "other.txt"}"#).await;
    let r = results(&h);
    assert!(r[0].1.contains("\n- content: Missing key\n"), "{}", r[0].1);
    assert!(!h.project.path().join("other.txt").exists());
}

/// A session with the `fix` MCP server (its `count` tool takes an integer
/// `count`), Code Mode on or off.
async fn mcp(codemode: bool) -> Option<support::H> {
    let py = python()?;
    let fixture = format!(
        "{}/tests/fixtures/mcp_repair.py",
        env!("CARGO_MANIFEST_DIR")
    );
    Some(
        support::H::with(
            json!({
                "codemode": codemode,
                "mcp": {"servers": {"fix": {"type": "local", "command": [py, fixture]}}},
            }),
            None,
        )
        .await,
    )
}

/// An MCP tool's input is checked after repair against the server's
/// schema; what fails never reaches the server.
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_runtime_rejects_invalid_mcp_input_after_repair() {
    let Some(mut h) = mcp(false).await else {
        return;
    };
    h.prompt(r#"call fix_count {"count": "many"}"#).await;
    let r = results(&h);
    assert_eq!(
        r[0].1,
        "Invalid arguments for tool \"fix_count\":\n- count: Expected an integer\n\nArguments provided:\n{\n  \"count\": \"many\"\n}\n\nUpdate the arguments and call the tool again."
    );
    assert!(r[0].2);
    // Repaired input passes.
    h.prompt(r#"call fix_count {"count": "2"}"#).await;
    assert_eq!(results(&h)[0].1, r#"got: {"count": 2}"#);
}

/// A call a Code Mode program makes is checked against the schema it was
/// shown, after the same repair; the program gets the error.
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_runtime_rejects_invalid_input_from_code_mode() {
    let Some(mut h) = mcp(true).await else {
        return;
    };
    let code = json!({"code": "try { await tools.fix.count({ count: \"many\" }) } catch (e) { return 'caught: ' + e.message }"});
    h.prompt(&format!("call execute {code}")).await;
    let r = results(&h);
    assert!(
        r[0].1
            .contains("caught: fix_count: Invalid arguments for tool")
            && r[0].1.contains("count: Expected an integer"),
        "{}",
        r[0].1
    );
    assert!(!r[0].1.contains("got:"), "{}", r[0].1);
    let code = json!({"code": "return await tools.fix.count({ count: \"3\" })"});
    h.prompt(&format!("call execute {code}")).await;
    let r = results(&h);
    assert!(r[0].1.contains("got:"), "{}", r[0].1);
}

/// Records the input it runs with.
struct Strict(Arc<Mutex<Vec<Value>>>);
impl Tool for Strict {
    fn name(&self) -> &str {
        "strict"
    }
    fn description(&self) -> &str {
        "Takes a level from 1 to 3"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"level": {"type": "integer", "minimum": 1, "maximum": 3}},
               "required": ["level"], "additionalProperties": false})
    }
    fn execute<'a>(
        &'a self,
        _: ToolContext,
        input: Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.0.lock().unwrap().push(input);
        Box::pin(async { Ok("ran".to_string()) })
    }
}

struct WithTool(Arc<Mutex<Vec<Value>>>);
impl Plugin for WithTool {
    fn id(&self) -> &str {
        "with-tool"
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(Strict(self.0.clone()))]
    }
}

/// A plugin tool's input is checked against the schema it declares; valid
/// input runs it, with a closed object's extras dropped.
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_runtime_rejects_invalid_plugin_tool_input() {
    static RAN: std::sync::OnceLock<Arc<Mutex<Vec<Value>>>> = std::sync::OnceLock::new();
    let ran = RAN
        .get_or_init(|| {
            let ran: Arc<Mutex<Vec<Value>>> = Arc::default();
            plugin::install(vec![Arc::new(WithTool(ran.clone()))]);
            ran
        })
        .clone();
    let _lock = plugin_support::serial();
    let dir = tempfile::tempdir().unwrap();
    let args = Arc::new(Mutex::new(json!({"level": 5})));
    let next = args.clone();
    let m = plugin_support::serve(move |r| {
        if r.body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool" {
            plugin_support::text("Done")
        } else {
            plugin_support::tool("call-strict", "strict", next.lock().unwrap().clone())
        }
    })
    .await;
    let l = plugin_support::loaded(
        dir.path(),
        &m.base,
        json!({"permissions": [{"action": "strict", "resource": "*", "effect": "allow"}]}),
    );
    let mut s = codesmith_core::session::Session::new(
        &l,
        dir.path().to_path_buf(),
        plugin_support::options(),
        Arc::new(codesmith_core::permission::Fixed(
            codesmith_core::permission::Reply::Once,
        )),
        None,
    )
    .unwrap();
    s.prompt("Use it", &mut plugin_support::Collect::default())
        .await
        .unwrap();
    let tool_message = |s: &codesmith_core::session::Session| {
        s.messages
            .iter()
            .rev()
            .flat_map(|m| m.parts.iter())
            .find_map(|p| match p {
                codesmith_ai::Part::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .unwrap()
    };
    let out = tool_message(&s);
    assert!(
        out.starts_with("Invalid arguments for tool \"strict\":\n- level: Expected a value less than or equal to 3\n"),
        "{out}"
    );
    assert!(ran.lock().unwrap().is_empty());
    *args.lock().unwrap() = json!({"level": 2, "extra": true});
    s.prompt("Again", &mut plugin_support::Collect::default())
        .await
        .unwrap();
    assert_eq!(tool_message(&s), "ran");
    assert_eq!(*ran.lock().unwrap(), [json!({"level": 2})]);
}
