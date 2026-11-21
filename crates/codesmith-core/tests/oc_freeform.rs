//! Freeform tool formats, after OpenCode's `session-model-request-hooks.test.ts`
//! and `tool-execute.test.ts` (MIT): a tool whose input is one required string
//! can take it as raw text. The model request advertises a tool's format only
//! when the model accepts it, and Code Mode lets a program pass that string
//! directly. The client MCP server here is an in-process task on an in-memory
//! pipe; it ends when its connection closes, and with the test's runtime.

mod support;

use codesmith_core::codemode::{self, Entry};
use codesmith_core::mcp::Mcp;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{Reply_, H};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Attach an MCP server with one tool, `echo`, under `tui`, so the session
/// offers `execute`.
async fn attach(mcp: &Arc<Mcp>) -> codesmith_core::mcp::Attachment {
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    let (read, mut write) = tokio::io::split(theirs);
    tokio::spawn(async move {
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let m: Value = serde_json::from_str(&line).unwrap();
            let Some(id) = m.get("id").cloned() else {
                continue;
            };
            let result = match m["method"].as_str().unwrap_or("") {
                "initialize" => {
                    json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "tui", "version": "1"}})
                }
                "tools/list" => {
                    json!({"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]})
                }
                "tools/call" => {
                    let text = m["params"]["arguments"]["text"].as_str().unwrap_or("");
                    json!({"content": [{"type": "text", "text": format!("echo: {text}")}]})
                }
                _ => json!({}),
            };
            let r = json!({"jsonrpc": "2.0", "id": id, "result": result});
            if write.write_all(format!("{r}\n").as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let (r, w) = tokio::io::split(ours);
    mcp.attach("tui", r, w).await.unwrap()
}

/// A tool's name and how it goes out: `("function", None)` or
/// `("custom", Some(format))`.
fn declared(t: &Value) -> (String, (String, Option<Value>)) {
    match t["type"].as_str().unwrap() {
        "custom" => (
            t["custom"]["name"].as_str().unwrap().into(),
            ("custom".into(), Some(t["custom"]["format"].clone())),
        ),
        kind => (
            t["function"]["name"].as_str().unwrap().into(),
            (kind.into(), None),
        ),
    }
}

// session-model-request-hooks.test.ts: "advertises only supported freeform
// formats without removing tools or changing the shared definitions"
// (adapted) CodeSmith's freeform tools are `execute` (text) and `patch` (lark);
// the model's formats come from its `capabilities.freeform`.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_model_request_advertises_only_supported_freeform_formats() {
    let cases: [(Option<Value>, bool, bool); 4] = [
        (None, false, false),
        (Some(json!([])), false, false),
        (Some(json!(["text", "lark"])), true, true),
        (Some(json!(["regex"])), false, false),
    ];
    let mut names: Option<Vec<String>> = None;
    for (supported, text, lark) in cases {
        // A GPT model, which edits with `patch`.
        let model = match &supported {
            Some(f) => json!({"id": "gpt-5", "capabilities": {"freeform": f}}),
            None => json!({"id": "gpt-5"}),
        };
        let mut h = H::with(
            json!({"mcp": {"servers": {}}, "full_codemode": false, "fakeModel": model}),
            None,
        )
        .await;
        let _a = attach(h.mcp.as_ref().unwrap()).await;
        h.prompt("hi").await;
        let req = h
            .requests()
            .into_iter()
            .rfind(|r| r.get("tools").is_some())
            .unwrap();
        let tools: Vec<(String, (String, Option<Value>))> = req["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(declared)
            .collect();
        let these: Vec<String> = tools.iter().map(|t| t.0.clone()).collect();
        // The same tools every time, whatever the model accepts.
        assert_eq!(names.get_or_insert_with(|| these.clone()), &these);
        assert!(
            these.contains(&"execute".to_string()) && these.contains(&"patch".to_string()),
            "{these:?}"
        );
        for (name, how) in tools {
            let want = match name.as_str() {
                "execute" if text => ("custom".into(), Some(json!({"type": "text"}))),
                "patch" if lark => (
                    "custom".into(),
                    Some(
                        json!({"type": "grammar", "grammar": {"syntax": "lark", "definition": codesmith_core::tool::apply_patch::GRAMMAR}}),
                    ),
                ),
                _ => ("function".into(), None),
            };
            assert_eq!(how, want, "{name} with {supported:?}");
        }
    }
}

// The model writes `execute`'s program as raw text: it runs as `{code}`, and
// the call replays as a custom tool call.
#[tokio::test(flavor = "multi_thread")]
async fn a_freeform_execute_call_runs_its_raw_program_and_replays_as_custom() {
    let program = "return await tools.tui.echo({text: \"raw \\\"quoted\\\"\"})";
    let reply: Reply_ = Arc::new(move |body: &Value| {
        let last = body["messages"].as_array()?.last()?;
        (last["role"] == "user" && last["content"] == "run it").then(|| {
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "custom", "custom": {"name": "execute", "input": program}}]}, "finish_reason": "tool_calls"}]})
        })
    });
    let mut h = H::with(
        json!({"mcp": {"servers": {}}, "fakeModel": {"capabilities": {"freeform": ["text"]}}}),
        Some(reply),
    )
    .await;
    let _a = attach(h.mcp.as_ref().unwrap()).await;
    h.prompt("run it").await;
    let req = h
        .requests()
        .into_iter()
        .rfind(|r| r.get("tools").is_some())
        .unwrap();
    let messages = req["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .find_map(|m| m["tool_calls"].get(0))
        .unwrap();
    assert_eq!(
        call,
        &json!({"id": "c1", "type": "custom", "custom": {"name": "execute", "input": program}})
    );
    let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert!(
        result["content"]
            .as_str()
            .unwrap()
            .contains(r#"echo: raw \"quoted\""#),
        "{result}"
    );
}

// tool-execute.test.ts: "execute takes raw code as freeform text, and Code
// Mode calls freeform tools with a string"
// (adapted) CodeSmith's tools declare no output schema, so results are
// `Promise<unknown>`; `execute`'s own freeform format is pinned by the session
// tests above.
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_execute_takes_raw_code_as_freeform_text_and_code_mode_calls_freeform_tools_with_a_string(
) {
    let schema = json!({"type": "object", "properties": {"text": {"type": "string", "minLength": 1}}, "required": ["text"]});
    let entry = |name: &str, freeform| Entry {
        tool: name.into(),
        namespace: String::new(),
        name: name.into(),
        description: "Send text".into(),
        schema: schema.clone(),
        output: None,
        freeform,
    };
    let entries = vec![entry("echo", false), entry("send", true)];
    let signatures: Vec<String> = entries.iter().map(codemode::signature).collect();
    assert_eq!(
        signatures,
        [
            "tools.echo({\n  /** @minLength 1 */\n  text: string,\n}): Promise<unknown>",
            "tools.send(/** @minLength 1 */ text: string): Promise<unknown>",
        ]
    );
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let seen = inputs.clone();
    let out = codemode::execute_with(
        None,
        r#"return [await tools.send("first"), await tools.send({text: "second"})]"#,
        &entries,
        codemode::MAX_TOOL_CALLS,
        Duration::from_secs(30),
        move |_tool, input| {
            seen.lock().unwrap().push(input.clone());
            async move { Ok(input["text"].as_str().unwrap_or("").to_string()) }
        },
    )
    .await;
    assert_eq!(out.result, Ok(json!(["first", "second"])));
    assert_eq!(
        *inputs.lock().unwrap(),
        [json!({"text": "first"}), json!({"text": "second"})]
    );
}
