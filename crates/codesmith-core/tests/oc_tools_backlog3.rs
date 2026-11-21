//! Ports of OpenCode's `tool-registry.test.ts` and `tool-schema.test.ts`
//! (MIT) from the porting backlog: tool placement with `execute` denied,
//! plugin hooks around calls, and MCP tools without an input schema. Its
//! own test binary, because the plugin registry is process-wide.

mod support;

use codesmith_core::plugin::{self, Plugin, ToolCall};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use support::{Quiet, H};

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Records each `read` call it sees by phase and ID, and replaces the
/// output of reads of `hooked-*` files.
#[derive(Default)]
struct Hooks {
    seen: Mutex<Vec<(String, String, String)>>,
}

impl Plugin for Hooks {
    fn id(&self) -> &str {
        "backlog3-hooks"
    }
    fn tool_before(&self, call: &mut ToolCall) -> Result<(), String> {
        if call.name == "read" {
            self.seen.lock().unwrap().push((
                "before".into(),
                call.input["path"].as_str().unwrap_or("").into(),
                call.id.clone(),
            ));
        }
        Ok(())
    }
    fn tool_after(&self, call: &ToolCall, output: &mut String, _is_error: &mut bool) {
        if call.name != "read" {
            return;
        }
        let path = call.input["path"].as_str().unwrap_or("").to_string();
        self.seen
            .lock()
            .unwrap()
            .push(("after".into(), path.clone(), call.id.clone()));
        if path.contains("hooked-string") {
            *output = "hooked".into();
        } else if path.contains("hooked-empty") {
            *output = String::new();
        }
    }
}

fn hooks() -> Arc<Hooks> {
    static HOOKS: std::sync::OnceLock<Arc<Hooks>> = std::sync::OnceLock::new();
    HOOKS
        .get_or_init(|| {
            let h = Arc::new(Hooks::default());
            plugin::install(vec![h.clone()]);
            h
        })
        .clone()
}

fn offered(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect()
}

/// The last tool result the model was sent.
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

fn mcp(extra: Value) -> Option<Value> {
    let py = python()?;
    let fixture = format!("{}/tests/fixtures/mcp_stdio.py", env!("CARGO_MANIFEST_DIR"));
    let mut v = json!({"mcp": {"servers": {"srv": {"type": "local", "command": [py, fixture]}}}});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    Some(v)
}

// tool-registry.test.ts: "exposes Code Mode tools directly when execute is
// denied"
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_registry_exposes_code_mode_tools_directly_when_execute_is_denied() {
    hooks();
    let deny_execute = json!({"action": "execute", "resource": "*", "effect": "deny"});
    let Some(extra) = mcp(json!({"permissions": [deny_execute.clone()]})) else {
        return;
    };
    let mut h = H::with(extra, None).await;
    h.prompt("call srv_echo {\"text\":\"hi\"}").await;
    let first = h.requests()[0].clone();
    let names = offered(&first);
    assert!(names.contains(&"srv_echo".to_string()), "{names:?}");
    assert!(!names.contains(&"execute".to_string()), "{names:?}");
    assert!(last_tool_result(&h).contains("echo: hi"));

    // Denied by name too: not offered, and a call is refused.
    let deny_tool = json!({"action": "srv_echo", "resource": "*", "effect": "deny"});
    let extra = mcp(json!({"permissions": [deny_execute, deny_tool]})).unwrap();
    let mut hidden = H::with(extra, None).await;
    hidden.prompt("call srv_echo {\"text\":\"hi\"}").await;
    let names = offered(&hidden.requests()[0]);
    assert!(!names.contains(&"srv_echo".to_string()), "{names:?}");
    assert!(!names.contains(&"execute".to_string()), "{names:?}");
    assert!(!last_tool_result(&hidden).contains("echo: hi"));
}

// tool-schema.test.ts: "missing external input schemas fall back to an
// empty schema" (adapted: the empty schema is an object with no
// properties, which the providers accept, offered for an MCP tool that
// lists none)
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_schema_missing_external_input_schemas_fall_back_to_an_empty_schema() {
    hooks();
    let Some(extra) = mcp(json!({"codemode": false})) else {
        return;
    };
    let mut h = H::with(extra, None).await;
    h.prompt("hello").await;
    let req = h.requests()[0].clone();
    let tool = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "srv_fail_now")
        .cloned()
        .expect("offered");
    assert_eq!(tool["function"]["description"], "Always fails");
    let params = &tool["function"]["parameters"];
    assert_eq!(params["type"], "object", "{params}");
    assert!(
        params["properties"]
            .as_object()
            .is_none_or(|p| p.is_empty()),
        "{params}"
    );
    assert!(params.get("required").is_none_or(|r| r == &json!([])));
}

// tool-registry.test.ts: "normalizes ${input.name} after the final tool
// hook" (the string row; adapted: a result is text, so the hook replaces
// the text the model is sent). By design an empty result is sent as
// "(no tool output)", which every provider accepts.
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_registry_normalizes_output_after_the_final_tool_hook() {
    let _ = hooks();
    for (file, sent) in [
        ("hooked-string.txt", "hooked"),
        ("hooked-empty.txt", "(no tool output)"),
    ] {
        let mut h = H::new().await;
        std::fs::write(h.root().join(file), "original\n").unwrap();
        h.prompt(&format!("call read {}", json!({ "path": file })))
            .await;
        assert_eq!(last_tool_result(&h), sent, "{file}");
    }
}

// tool-registry.test.ts: "hooks see one ID per call inside a Code Mode
// program"
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_registry_hooks_see_one_id_per_call_inside_a_code_mode_program() {
    let hooks = hooks();
    let mut h = H::with(
        json!({"model": "fake/claude-sonnet-4-5", "full_codemode": true}),
        None,
    )
    .await;
    let root = h.root();
    std::fs::write(root.join("id-a.txt"), "a\n").unwrap();
    std::fs::write(root.join("id-b.txt"), "b\n").unwrap();
    let code =
        "await Promise.all([tools.read({ path: 'id-a.txt' }), tools.read({ path: 'id-b.txt' })])";
    h.s.prompt(
        &format!("call execute {}", json!({ "code": code })),
        &mut Quiet,
    )
    .await
    .unwrap();
    let seen = hooks.seen.lock().unwrap().clone();
    let ids = |phase: &str| -> Vec<String> {
        let mut v: Vec<String> = seen
            .iter()
            .filter(|(p, path, _)| p == phase && path.starts_with("id-"))
            .map(|(_, _, id)| id.clone())
            .collect();
        v.sort();
        v
    };
    let before = ids("before");
    assert_eq!(before.len(), 2, "{seen:?}");
    assert_ne!(before[0], before[1]);
    assert!(
        !before.contains(&"c1".to_string()),
        "not the execute call's"
    );
    assert_eq!(ids("after"), before);
}
