//! Ports of OpenCode's `tool-registry.test.ts` (MIT) cases on a plugin's
//! `tool_before` hook sending a call to another tool, inside a Code Mode
//! program and as a direct call, with the guardrails around it: the
//! renamed call runs through the new tool's permission checks, a rename
//! never reaches a tool that is denied, hidden or not the program's own,
//! and odd names run nothing. Its own test binary, because the plugin
//! registry is process-wide.

mod support;

use codesmith_core::permission::{Reply, Request};
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

/// Renames `read` calls by their path: `to-write:<file>` writes `<file>`,
/// `to-srv_echo:<text>` echoes `<text>` through the MCP fixture, and
/// `to-<name>` sends the call to `<name>` unchanged. A `write` of
/// `rename-to-read.txt` reads it instead. Records the tool each after hook
/// sees and the action of each permission request.
#[derive(Default)]
struct Renamer {
    after: Mutex<Vec<String>>,
    asked: Mutex<Vec<String>>,
}

impl Plugin for Renamer {
    fn id(&self) -> &str {
        "rename-hooks"
    }
    fn tool_before(&self, call: &mut ToolCall) -> Result<(), String> {
        let path = call.input["path"].as_str().unwrap_or("").to_string();
        if call.name == "write" && path == "rename-to-read.txt" {
            call.name = "read".into();
        } else if call.name == "read" {
            if path == "vetoed.txt" {
                return Err("vetoed by the plugin".into());
            }
            if let Some(file) = path.strip_prefix("to-write:") {
                call.name = "write".into();
                call.input = json!({"path": file, "content": "written by a rename\n"});
            } else if let Some(text) = path.strip_prefix("to-srv_echo:") {
                call.name = "srv_echo".into();
                call.input = json!({ "text": text });
            } else if let Some(name) = path.strip_prefix("to-") {
                call.name = name.to_string();
            }
        }
        Ok(())
    }
    fn tool_after(&self, call: &ToolCall, _output: &mut String, _is_error: &mut bool) {
        self.after.lock().unwrap().push(call.name.clone());
    }
    fn permission(&self, req: &Request) -> Option<Reply> {
        self.asked.lock().unwrap().push(req.action.clone());
        None
    }
}

fn hooks() -> Arc<Renamer> {
    static HOOKS: std::sync::OnceLock<Arc<Renamer>> = std::sync::OnceLock::new();
    HOOKS
        .get_or_init(|| {
            let h = Arc::new(Renamer::default());
            plugin::install(vec![h.clone()]);
            h
        })
        .clone()
}

/// The tests share the recorder; each looks for its own entries only.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn reset(h: &Renamer) {
    h.after.lock().unwrap().clear();
    h.asked.lock().unwrap().clear();
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

/// A session whose strong model calls the built-ins from inside `execute`.
async fn code_mode(extra: Value) -> H {
    let mut v = json!({"model": "fake/claude-sonnet-4-5", "full_codemode": true});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    H::with(v, None).await
}

async fn run_program(h: &mut H, code: &str) -> String {
    h.s.prompt(
        &format!("call execute {}", json!({ "code": code })),
        &mut Quiet,
    )
    .await
    .unwrap();
    last_tool_result(h)
}

async fn call(h: &mut H, name: &str, input: Value) -> String {
    h.prompt(&format!("call {name} {input}")).await;
    last_tool_result(h)
}

// tool-registry.test.ts: "a hook can rename a call inside a Code Mode
// program to another of its tools" (adapted: the program's tools are the
// built-ins a strong model calls from inside `execute`; there is no
// automation `check` here)
#[tokio::test(flavor = "multi_thread")]
async fn oc_a_hook_can_rename_a_call_inside_a_code_mode_program_to_another_of_its_tools() {
    let _g = SERIAL.lock().await;
    let hooks = hooks();
    reset(&hooks);
    let mut h = code_mode(json!({})).await;
    let root = h.root();
    std::fs::write(root.join("rename-to-read.txt"), "kept\n").unwrap();
    let out = run_program(
        &mut h,
        "return await tools.write({ path: 'rename-to-read.txt', content: 'clobbered' })",
    )
    .await;
    // It ran as read: the file is unchanged and its content came back.
    assert_eq!(
        std::fs::read_to_string(root.join("rename-to-read.txt")).unwrap(),
        "kept\n"
    );
    assert!(out.contains("kept"), "{out}");
    assert!(!out.contains("clobbered"), "{out}");
    assert!(hooks.after.lock().unwrap().contains(&"read".to_string()));

    // A program reaches only its own tools, however a call is renamed: not
    // a direct tool, a hidden one, `execute` itself, `fetch` or a missing
    // one.
    for name in ["todowrite", "glob", "execute", "\u{1}fetch", "missing"] {
        let path = json!(format!("to-{name}"));
        let out = run_program(
            &mut h,
            &format!(
                "try {{ await tools.read({{ path: {path} }}) }} catch (e) {{ return e.message }}"
            ),
        )
        .await;
        // The message comes back as a JSON string.
        let message = json!(format!("Tool is not available for this request: {name}"));
        let message = message.to_string();
        assert!(out.contains(message.trim_matches('"')), "{name}: {out}");
    }
}

// tool-registry.test.ts: "a call renamed inside a Code Mode program
// returns the renamed tool's structured output" (adapted: results are
// text; the call is renamed from a built-in to an MCP tool, with its input
// rewritten for it)
#[tokio::test(flavor = "multi_thread")]
async fn oc_a_call_renamed_inside_a_code_mode_program_returns_the_renamed_tools_output() {
    let _g = SERIAL.lock().await;
    let Some(py) = python() else {
        return;
    };
    let hooks = hooks();
    reset(&hooks);
    let fixture = format!("{}/tests/fixtures/mcp_stdio.py", env!("CARGO_MANIFEST_DIR"));
    let mut h = code_mode(
        json!({"mcp": {"servers": {"srv": {"type": "local", "command": [py, fixture]}}}}),
    )
    .await;
    let out = run_program(
        &mut h,
        "return await tools.read({ path: 'to-srv_echo:structured' })",
    )
    .await;
    assert!(out.contains("echo: structured"), "{out}");
    assert!(hooks
        .after
        .lock()
        .unwrap()
        .contains(&"srv_echo".to_string()));
}

// A rename inside a program to a tool the agent's rules deny outright: it
// is not one of the program's tools, so nothing runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_rename_to_a_denied_tool_is_refused() {
    let _g = SERIAL.lock().await;
    let hooks = hooks();
    reset(&hooks);
    let deny = json!({"action": "edit", "resource": "*", "effect": "deny"});
    let mut h = code_mode(json!({"permissions": [deny]})).await;
    let out = run_program(
        &mut h,
        "try { await tools.read({ path: 'to-write:made.txt' }) } catch (e) { return e.message }",
    )
    .await;
    assert!(
        out.contains("Tool is not available for this request: write"),
        "{out}"
    );
    assert!(!h.root().join("made.txt").exists());
}

// A rename inside a program to a tool denied for that file: the new tool's
// own check denies it.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_rename_is_checked_as_the_new_tool() {
    let _g = SERIAL.lock().await;
    let hooks = hooks();
    reset(&hooks);
    let deny = json!({"action": "edit", "resource": "*secret*", "effect": "deny"});
    let mut h = code_mode(json!({"permissions": [deny]})).await;
    let out = run_program(
        &mut h,
        "try { await tools.read({ path: 'to-write:secret.txt' }) } catch (e) { return e.message }",
    )
    .await;
    assert!(!h.root().join("secret.txt").exists(), "{out}");
    assert!(out.to_lowercase().contains("denied"), "{out}");
    // Allowed elsewhere, the same rename writes.
    run_program(
        &mut h,
        "return await tools.read({ path: 'to-write:open.txt' })",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(h.root().join("open.txt")).unwrap(),
        "written by a rename\n"
    );
}

// A direct call renamed to another offered tool runs as that tool, and
// the result still answers the call by the name it was made by.
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_call_can_be_renamed_to_another_offered_tool() {
    let _g = SERIAL.lock().await;
    let hooks = hooks();
    reset(&hooks);
    let mut h = H::new().await;
    let out = call(&mut h, "read", json!({"path": "to-write:direct.txt"})).await;
    assert_eq!(
        std::fs::read_to_string(h.root().join("direct.txt")).unwrap(),
        "written by a rename\n",
        "{out}"
    );
    assert!(hooks.after.lock().unwrap().contains(&"write".to_string()));
    let req = h.requests().pop().unwrap();
    let answered = req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()
        .clone();
    assert_eq!(answered["tool_call_id"], "c1");
}

// The permission asked for a renamed call is the new tool's.
#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_call_asks_the_new_tools_permission() {
    let _g = SERIAL.lock().await;
    let hooks = hooks();
    reset(&hooks);
    let ask = json!({"action": "edit", "resource": "*", "effect": "ask"});
    let mut h = H::with(json!({"permissions": [ask], "codemode": false}), None).await;
    call(&mut h, "read", json!({"path": "to-write:asked.txt"})).await;
    let asked = hooks.asked.lock().unwrap().clone();
    assert!(asked.contains(&"edit".to_string()), "{asked:?}");
    assert!(h.root().join("asked.txt").exists());
}

// A direct call renamed to a tool denied outright, or denied for the file,
// changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_rename_never_bypasses_a_deny() {
    let _g = SERIAL.lock().await;
    let hooks = hooks();
    reset(&hooks);
    let deny = json!({"action": "edit", "resource": "*", "effect": "deny"});
    let mut h = H::with(json!({"permissions": [deny], "codemode": false}), None).await;
    let out = call(&mut h, "read", json!({"path": "to-write:made.txt"})).await;
    assert!(
        out.contains("Tool is not available for this request: write"),
        "{out}"
    );
    assert!(!h.root().join("made.txt").exists());

    let deny = json!({"action": "edit", "resource": "*secret*", "effect": "deny"});
    let mut h = H::with(json!({"permissions": [deny], "codemode": false}), None).await;
    let out = call(&mut h, "read", json!({"path": "to-write:secret.txt"})).await;
    assert!(!h.root().join("secret.txt").exists(), "{out}");
    assert!(out.to_lowercase().contains("denied"), "{out}");
}

// Names that are no tool: empty, unknown, huge, unicode and control
// characters all run nothing, and a veto still refuses.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_to_no_tool_runs_nothing() {
    let _g = SERIAL.lock().await;
    let hooks = hooks();
    reset(&hooks);
    let mut h = H::with(json!({"codemode": false}), None).await;
    let huge = "x".repeat(20_000);
    for name in [
        "",
        "missing",
        "wr\u{0}ite",
        "write\n",
        "ëdit",
        "Write",
        huge.as_str(),
    ] {
        let out = call(&mut h, "read", json!({ "path": format!("to-{name}") })).await;
        assert!(
            out.starts_with("Tool is not available for this request: "),
            "{:?}: {}",
            &name[..name.len().min(20)],
            &out[..out.len().min(200)]
        );
    }
    let out = call(&mut h, "read", json!({"path": "vetoed.txt"})).await;
    assert_eq!(out, "vetoed by the plugin");
    // Renaming a call to the tool it already is changes nothing.
    std::fs::write(h.root().join("same.txt"), "same\n").unwrap();
    let out = call(&mut h, "read", json!({"path": "to-read"})).await;
    assert!(!out.contains("not available"), "{out}");
}
