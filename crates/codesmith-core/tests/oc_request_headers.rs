//! Ports of OpenCode's session correlation header and prompt cache key
//! tests (`packages/core/test/session-runner.test.ts`, MIT): every model
//! request of a session carries headers naming it, a subagent's requests
//! share their parent's cache affinity, and long session ids make bounded
//! cache keys. CodeSmith names its own headers `x-codesmith-*` where
//! OpenCode has `x-opencode-*`, and has no project header (it has no
//! project ids). See `crates/codesmith-core/PARITY.md`.

mod plugin_support;

use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::Session;
use plugin_support::{loaded, options, serve, text, Collect};
use serde_json::json;
use std::sync::Arc;

/// A compatible server takes `prompt_cache_key` only when asked to.
fn keyed(dir: &std::path::Path, base: &str) -> codesmith_core::config::Loaded {
    loaded(
        dir,
        base,
        json!({"providers": {"fake": {"protocol": "openai-chat", "options": {"setCacheKey": true}, "settings": {"baseURL": base, "apiKey": "x"}}}}),
    )
}

fn session(
    l: &codesmith_core::config::Loaded,
    dir: &std::path::Path,
    parent: Option<&str>,
) -> Session {
    let mut o = options();
    o.parent = parent.map(str::to_string);
    Session::new(l, dir.to_path_buf(), o, Arc::new(Fixed(Reply::Once)), None).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_adds_session_correlation_headers_to_model_requests() {
    let dir = tempfile::tempdir().unwrap();
    let m = serve(|_| text("Done")).await;
    let l = loaded(dir.path(), &m.base, json!({}));
    let mut s = session(&l, dir.path(), None);
    s.prompt("Run correlated request", &mut Collect::default())
        .await
        .unwrap();
    let id = s.id.clone();
    let h = &m.requests()[0].headers;
    let get = |k: &str| h.get(k).map(String::as_str);
    assert_eq!(get("x-codesmith-session-id"), Some(id.as_str()));
    assert_eq!(get("x-session-affinity"), Some(id.as_str()));
    assert_eq!(get("x-session-id"), Some(id.as_str()));
    assert_eq!(get("x-codesmith-session"), Some(id.as_str()));
    assert_eq!(get("x-codesmith-client"), Some("codesmith"));
    assert!(get("user-agent").is_some_and(|ua| ua.starts_with("codesmith/")));
    assert_eq!(get("x-parent-session-id"), None);
    assert_eq!(get("x-codesmith-parent-session-id"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_uses_parent_cache_affinity_for_child_model_requests() {
    let dir = tempfile::tempdir().unwrap();
    let m = serve(|_| text("Done")).await;
    let l = keyed(dir.path(), &m.base);
    let parent = "ses_runner_parent";
    let mut s = session(&l, dir.path(), Some(parent));
    s.prompt("Run child request", &mut Collect::default())
        .await
        .unwrap();
    let id = s.id.clone();
    let r = &m.requests()[0];
    let get = |k: &str| r.headers.get(k).map(String::as_str);
    assert_eq!(get("x-codesmith-session-id"), Some(id.as_str()));
    assert_eq!(get("x-codesmith-parent-session-id"), Some(parent));
    assert_eq!(get("x-session-affinity"), Some(parent));
    assert_eq!(get("x-session-id"), Some(parent));
    assert_eq!(get("x-parent-session-id"), Some(parent));
    assert_eq!(get("x-codesmith-session"), Some(parent));
    assert_eq!(r.body["prompt_cache_key"], parent);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_bounds_64_character_session_prompt_cache_keys() {
    let dir = tempfile::tempdir().unwrap();
    let m = serve(|_| text("Done")).await;
    let l = keyed(dir.path(), &m.base);
    let long = format!("ses_{}", "a".repeat(64));
    let other = format!("ses_{}", "b".repeat(64));
    for id in [&long, &other] {
        let mut s = session(&l, dir.path(), None);
        s.set_id(id.as_str());
        s.prompt("Run long session", &mut Collect::default())
            .await
            .unwrap();
    }
    let keys: Vec<String> = m
        .requests()
        .iter()
        .map(|r| r.body["prompt_cache_key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys, vec![long[4..].to_string(), other[4..].to_string()]);
    assert!(keys.iter().all(|k| k.len() == 64));
    assert_ne!(keys[0], keys[1]);
    assert_eq!(
        codesmith_core::session::prompt_cache_key("ses_short"),
        "ses_short"
    );
}
