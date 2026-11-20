//! Built-ins called from inside `execute` (under `full_codemode`) get the
//! per-call plugin hooks and change tracking a direct call gets. Its own
//! test binary, because the plugin registry is process-wide.

mod support;

use codesmith_core::plugin::{self, Plugin, ToolCall};
use codesmith_core::store::{Record, Store};
use serde_json::json;
use std::sync::{Arc, Mutex};
use support::{Quiet, H};

#[derive(Default)]
struct Hooks {
    before: Mutex<Vec<String>>,
}

impl Plugin for Hooks {
    fn id(&self) -> &str {
        "hooks"
    }
    fn tool_before(&self, call: &mut ToolCall) -> Result<(), String> {
        self.before
            .lock()
            .unwrap()
            .push(format!("{} {}", call.name, call.input["path"]));
        if call.input["path"]
            .as_str()
            .is_some_and(|p| p.contains("vetoed"))
        {
            return Err("vetoed by the test plugin".into());
        }
        Ok(())
    }
    fn tool_after(&self, _call: &ToolCall, output: &mut String, _is_error: &mut bool) {
        *output = output.replace("SECRET-TOKEN", "[redacted]");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn built_ins_inside_execute_get_plugin_hooks_and_record_their_changes() {
    let hooks = Arc::new(Hooks::default());
    plugin::install(vec![hooks.clone()]);
    let mut h = H::with(
        json!({"model": "fake/claude-sonnet-4-5", "full_codemode": true}),
        None,
    )
    .await;
    let root = h.loaded.root.clone();
    std::fs::write(root.join("kept.txt"), "old text\n").unwrap();
    std::fs::write(root.join("secret.txt"), "the SECRET-TOKEN is here\n").unwrap();
    let code = "await tools.write({ path: 'kept.txt', content: 'new text\\n' });\n\
        let r;\n\
        try { await tools.write({ path: 'vetoed.txt', content: 'x' }); r = 'ran'; }\n\
        catch (e) { r = 'refused: ' + String(e && e.message || e); }\n\
        const t = await tools.read({ path: 'secret.txt' });\n\
        return r + ' | ' + t;";
    let prompt = format!("call execute {}", json!({ "code": code }));
    h.s.prompt(&prompt, &mut Quiet).await.unwrap();

    // The veto held, and the plugin saw each inner call.
    assert!(!root.join("vetoed.txt").exists());
    let before = hooks.before.lock().unwrap().clone();
    assert!(
        before
            .iter()
            .any(|b| b.starts_with("write") && b.contains("kept.txt")),
        "{before:?}"
    );
    assert!(
        before.iter().any(|b| b.contains("vetoed.txt")),
        "{before:?}"
    );
    assert!(before.iter().any(|b| b.starts_with("read")), "{before:?}");

    // What the model saw: the refusal, and the redacted read.
    let log = h.log.lock().unwrap().clone();
    let sent = log.last().unwrap()["messages"].to_string();
    assert!(sent.contains("vetoed by the test plugin"), "{sent}");
    assert!(sent.contains("[redacted]"), "{sent}");
    assert!(!sent.contains("SECRET-TOKEN"), "{sent}");

    // The inner write is recorded as a change of the execute call.
    assert_eq!(
        std::fs::read_to_string(root.join("kept.txt")).unwrap(),
        "new text\n"
    );
    let store = Store::open(&h.db).unwrap();
    let changes: Vec<_> = store
        .entries(&h.s.id)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.record {
            Record::Change { path, old, new, .. } => Some((path, old, new)),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes,
        vec![(
            root.join("kept.txt"),
            Some("old text\n".into()),
            "new text\n".into()
        )]
    );
}
