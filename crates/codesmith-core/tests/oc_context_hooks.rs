//! Ports of OpenCode's session context hook tests
//! (`packages/core/test/session-runner.test.ts` and
//! `session-model-request-hooks.test.ts`, MIT): a plugin's `context` hook
//! rewrites the model context of a request (messages, system prompt and
//! the tools offered, under new names too), reading metadata a `prompt`
//! hook kept with the stored message, which never reaches the model. Its
//! own test binary, because the plugin registry is process-wide. See
//! `crates/codesmith-core/PARITY.md`.

mod plugin_support;

use codesmith_ai::{Part, Role};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::plugin::{self, BoxFuture, Context, Plugin, Prompt, Tool, ToolContext};
use codesmith_core::session::Session;
use codesmith_core::store::Store;
use plugin_support::{loaded, options, serve, text, tool, Collect};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

type ContextHook = Box<dyn Fn(&mut Context) + Send + Sync>;
type PromptHook = Box<dyn Fn(&mut Prompt) + Send + Sync>;

#[derive(Default)]
struct State {
    context: Mutex<Option<ContextHook>>,
    prompt: Mutex<Option<PromptHook>>,
    executions: Mutex<Vec<String>>,
}

struct Echo(Arc<State>);
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo the text"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
    }
    fn execute<'a>(
        &'a self,
        _: ToolContext,
        input: Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        let text = input["text"].as_str().unwrap_or_default().to_string();
        self.0.executions.lock().unwrap().push(text.clone());
        Box::pin(async move { Ok(text) })
    }
}

struct Lookup;
impl Tool for Lookup {
    fn name(&self) -> &str {
        "lookup"
    }
    fn description(&self) -> &str {
        "Lookup"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]})
    }
    fn execute<'a>(&'a self, _: ToolContext, _: Value) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Ok(String::new()) })
    }
}

struct Hooks(Arc<State>);
impl Plugin for Hooks {
    fn id(&self) -> &str {
        "hooks"
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(Echo(self.0.clone())), Arc::new(Lookup)]
    }
    fn prompt(&self, p: &mut Prompt) {
        if let Some(h) = &*self.0.prompt.lock().unwrap() {
            h(p);
        }
    }
    fn context(&self, c: &mut Context) {
        if let Some(h) = &*self.0.context.lock().unwrap() {
            h(c);
        }
    }
}

/// The plugin, installed once for this binary, and a lock the tests take
/// in turn, since they share it.
fn state() -> (Arc<State>, tokio::sync::MutexGuard<'static, ()>) {
    static STATE: std::sync::OnceLock<Arc<State>> = std::sync::OnceLock::new();
    let s = STATE
        .get_or_init(|| {
            let s = Arc::new(State::default());
            plugin::install(vec![Arc::new(Hooks(s.clone()))]);
            s
        })
        .clone();
    let guard = plugin_support::serial();
    *s.context.lock().unwrap() = None;
    *s.prompt.lock().unwrap() = None;
    s.executions.lock().unwrap().clear();
    (s, guard)
}

fn session(
    l: &codesmith_core::config::Loaded,
    dir: &std::path::Path,
    store: Option<Store>,
) -> Session {
    Session::new(
        l,
        dir.to_path_buf(),
        options(),
        Arc::new(Fixed(Reply::Once)),
        store,
    )
    .unwrap()
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_executes_a_tool_renamed_by_a_session_context_hook() {
    let (state, _lock) = state();
    *state.context.lock().unwrap() = Some(Box::new(|c: &mut Context| {
        for t in &mut c.tools {
            if t.name == "echo" {
                t.name = "renamed_echo".into();
            }
        }
    }));
    let dir = tempfile::tempdir().unwrap();
    let m = serve(|r| {
        if r.body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool" {
            text("Done")
        } else {
            tool("call-renamed", "renamed_echo", json!({"text": "renamed"}))
        }
    })
    .await;
    let l = loaded(dir.path(), &m.base, json!({}));
    let mut s = session(&l, dir.path(), None);
    s.prompt("Use the renamed tool", &mut Collect::default())
        .await
        .unwrap();
    let names = tool_names(&m.requests()[0].body);
    assert!(names.contains(&"renamed_echo".to_string()), "{names:?}");
    assert!(!names.contains(&"echo".to_string()), "{names:?}");
    assert_eq!(*state.executions.lock().unwrap(), ["renamed"]);
    let result = &m.requests()[1].body["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(result["content"], "renamed");
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_passes_stored_message_metadata_to_the_context_hook_but_not_to_the_model_request() {
    let (state, _lock) = state();
    *state.prompt.lock().unwrap() = Some(Box::new(|p: &mut Prompt| {
        p.metadata = Some(json!({"canary": {"context": ["CANARY-TOKEN"]}}));
    }));
    *state.context.lock().unwrap() = Some(Box::new(|c: &mut Context| {
        for m in &mut c.messages {
            let canary = m.metadata.as_ref().map(|v| v["canary"]["context"].clone());
            if m.message.role != Role::User {
                continue;
            }
            if let Some(Value::Array(lines)) = canary {
                let t: Vec<&str> = lines.iter().filter_map(Value::as_str).collect();
                m.message.parts.insert(0, Part::text(t.join("\n")));
            }
        }
    }));
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
    let m = serve(|_| text("Done")).await;
    let l = loaded(dir.path(), &m.base, json!({}));
    let mut s = session(&l, dir.path(), Some(store));
    s.prompt("Start", &mut Collect::default()).await.unwrap();
    let body = m.requests()[0].body.clone();
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    let raw = last["content"].to_string();
    assert!(
        raw.contains("CANARY-TOKEN") && raw.contains("Start"),
        "{last}"
    );
    assert!(
        raw.find("CANARY-TOKEN") < raw.find("Start"),
        "the hook's text comes first: {last}"
    );
    let all = body.to_string();
    assert!(
        !all.contains("\"metadata\"") && !all.contains("canary"),
        "{all}"
    );

    // The metadata is kept with the stored message, for the next session
    // object that continues it.
    let id = s.id.clone();
    drop(s);
    *state.prompt.lock().unwrap() = None;
    let mut o = options();
    o.resume = Some(id);
    let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
    let mut s = Session::new(
        &l,
        dir.path().to_path_buf(),
        o,
        Arc::new(Fixed(Reply::Once)),
        Some(store),
    )
    .unwrap();
    s.prompt("Next", &mut Collect::default()).await.unwrap();
    let body = m.requests()[1].body.clone();
    let first_user = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()
        .clone();
    assert!(
        first_user["content"].to_string().contains("CANARY-TOKEN"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_keeps_context_hook_edits_to_messages_without_stored_metadata() {
    let (state, _lock) = state();
    *state.context.lock().unwrap() = Some(Box::new(|c: &mut Context| {
        for m in &mut c.messages {
            if m.message.role == Role::User && m.metadata.is_none() {
                m.message.parts.insert(0, Part::text("EDITED"));
            }
        }
    }));
    let dir = tempfile::tempdir().unwrap();
    let m = serve(|_| text("Done")).await;
    let l = loaded(dir.path(), &m.base, json!({}));
    let mut s = session(&l, dir.path(), None);
    s.prompt("Start", &mut Collect::default()).await.unwrap();
    let last = m.requests()[0].body["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let raw = last["content"].to_string();
    assert!(
        raw.find("EDITED").is_some() && raw.find("EDITED") < raw.find("Start"),
        "{last}"
    );
    assert!(
        !s.messages.iter().any(|m| m
            .parts
            .iter()
            .any(|p| matches!(p, Part::Text(t) if t.as_str() == "EDITED"))),
        "the history is not changed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_keeps_hook_edits_to_a_tool_schema_out_of_later_requests() {
    let (state, _lock) = state();
    *state.context.lock().unwrap() = Some(Box::new(|c: &mut Context| {
        for t in &mut c.tools {
            if t.name == "lookup" {
                t.parameters["required"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("extra"));
            }
        }
    }));
    let dir = tempfile::tempdir().unwrap();
    let m = serve(|_| text("Done")).await;
    let l = loaded(dir.path(), &m.base, json!({}));
    let mut s = session(&l, dir.path(), None);
    s.prompt("one", &mut Collect::default()).await.unwrap();
    s.prompt("two", &mut Collect::default()).await.unwrap();
    let required: Vec<Value> = m
        .requests()
        .iter()
        .map(|r| {
            r.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["function"]["name"] == "lookup")
                .unwrap()["function"]["parameters"]["required"]
                .clone()
        })
        .collect();
    assert_eq!(required, [json!(["id", "extra"]), json!(["id", "extra"])]);
    assert_eq!(Lookup.parameters()["required"], json!(["id"]));
}
