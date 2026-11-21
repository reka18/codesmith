//! provider-chatgpt.test.ts and provider-openai.test.ts (MIT): "omits the
//! default output limit from OpenAI steps and compaction". The `openai`
//! provider (a key or a ChatGPT sign-in) is sent no output limit on a
//! session's steps or its compaction summaries; another provider, such as
//! Azure, keeps it. Adapted: the provider speaks OpenAI Chat to a local
//! fake, so the limit is the request's `max_tokens`.

mod support;

use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use support::Quiet;

/// The bodies a session on `provider` sent for a step and a compaction.
async fn bodies(provider: &str) -> Vec<Value> {
    support::isolate();
    let log = Arc::new(Mutex::new(Vec::new()));
    let base = support::model(log.clone(), None).await;
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let config: Config = serde_json::from_value(json!({
        "model": format!("{provider}/gpt-5.5"),
        "snapshots": false,
        "title": false,
        "providers": {provider: {"protocol": "openai-chat",
            "settings": {"baseURL": base, "apiKey": "sk-test"},
            "models": {"gpt-5.5": {"limit": {"context": 400_000, "output": 128_000}}}}}
    }))
    .unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: root.clone(),
    };
    let db = project.path().join("s.db");
    let mut s = Session::new(
        &loaded,
        root,
        Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("t".into()),
            parent: None,
            questioner: None,
            mcp: None,
            id: None,
        },
        Arc::new(Fixed(Reply::Always)),
        Some(codesmith_core::store::Store::open(&db).unwrap()),
    )
    .unwrap();
    s.prompt("hello", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let sent = log.lock().unwrap().clone();
    sent
}

fn limit(b: &Value) -> Option<&Value> {
    b.get("max_tokens")
        .or_else(|| b.get("max_completion_tokens"))
        .filter(|v| !v.is_null())
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_omits_the_default_output_limit_from_openai_steps_and_compaction() {
    let sent = bodies("openai").await;
    assert!(sent.len() >= 2, "a step and a summary: {}", sent.len());
    for b in &sent {
        assert_eq!(limit(b), None, "{b}");
    }
    // Another provider (OpenCode's case is Azure) keeps the limit on both.
    let sent = bodies("elsewhere").await;
    assert!(sent.len() >= 2);
    for b in &sent {
        assert!(limit(b).is_some(), "{b}");
    }
}
