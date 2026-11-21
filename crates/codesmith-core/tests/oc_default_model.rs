//! Ports of OpenCode's `catalog.test.ts` (MIT) default model cases: with
//! no model configured a session runs the newest available model that
//! takes and makes text, and a configured default on a disabled provider
//! (`disabled_providers`) is passed over. Adapted: OpenCode's provider and
//! model transforms are config and a fixed catalog here.

mod support;

use codesmith_core::catalog::{Json, ModelsDev, Snapshot};
use codesmith_core::config::{self, Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session};
use serde_json::{json, Value};
use std::sync::Arc;

fn catalog(models: &[(&str, &str, &str, &str)]) -> Vec<Snapshot> {
    let entries: Vec<String> = models
        .iter()
        .map(|(id, released, input, output)| {
            format!(
                r#""{id}": {{"id": "{id}", "type": "chat", "name": "{id}", "release_date": "{released}",
                "modalities": {{"input": {input}, "output": {output}}},
                "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}}},
                "limit": {{"context": 128000, "output": 32000}},
                "api": {{"chat-completions": {{"base_url": "https://api.test.invalid/v1",
                    "sdk": {{"opencodeai": "@opencode/ai/providers/openai-compatible"}}}}}}}}"#
            )
        })
        .collect();
    let text = format!(
        r#"{{"test": {{"id": "test", "name": "Test", "env": [], "models": {{{}}}}}}}"#,
        entries.join(",")
    );
    codesmith_core::catalog::normalize(&Json::parse(&text).unwrap())
}

fn config(v: Value, data: Vec<Snapshot>) -> Config {
    support::isolate();
    let mut c: Config = serde_json::from_value(v).unwrap();
    c.models_dev = Some(Arc::new(ModelsDev::fixed(data)));
    c
}

const TEXT: &str = r#"["text"]"#;

// catalog.test.ts: "falls back to newest available model when no default
// is configured" (adapted: the video and speech models are dropped by the
// catalog, and a configured model that takes audio only stands in for
// them as the newest non-text model)
#[test]
fn oc_falls_back_to_the_newest_available_model_when_no_default_is_configured() {
    let data = catalog(&[
        ("old", "2024-01-01", TEXT, TEXT),
        ("new", "2025-01-01", TEXT, TEXT),
        (
            "newest-video",
            "2026-01-01",
            r#"["text", "image"]"#,
            r#"["video"]"#,
        ),
        ("newest-stt", "2026-02-01", r#"["audio"]"#, TEXT),
        ("newest-audio-in", "2026-03-01", TEXT, TEXT),
    ]);
    let c = config(
        json!({"providers": {"test": {
            "settings": {"apiKey": "x"},
            "models": {"newest-audio-in": {"modalities": {"input": ["audio"], "output": ["text"]}}}
        }}}),
        data,
    );
    assert_eq!(c.model, None);
    assert_eq!(c.default_model().as_deref(), Some("test/new"));
}

// catalog.test.ts: "ignores a configured default on a disabled provider"
#[test]
fn oc_ignores_a_configured_default_on_a_disabled_provider() {
    let c = config(
        json!({
            "model": "disabled/configured",
            "disabled_providers": ["disabled"],
            "providers": {
                "disabled": {"settings": {"baseURL": "https://a.test.invalid/v1", "apiKey": "x"},
                             "models": {"configured": {}}},
                "enabled": {"settings": {"baseURL": "https://b.test.invalid/v1", "apiKey": "x"},
                            "models": {"fallback": {}}}
            }
        }),
        vec![],
    );
    assert_eq!(c.default_model().as_deref(), Some("enabled/fallback"));
    // A disabled provider is never used: not listed, and naming it fails.
    let listed = codesmith_core::plugin::codesmith::available(&c, "chat");
    assert!(listed.iter().all(|r| r.provider_id != "disabled"));
    let err = c
        .resolve_model("disabled/configured")
        .unwrap_err()
        .to_string();
    assert!(err.contains("disabled"), "{err}");
    // With a variant too.
    let mut v = c.clone();
    v.model = Some("disabled/configured#high".into());
    assert_eq!(v.default_model().as_deref(), Some("enabled/fallback"));
    // Not disabled, the configured default stands.
    let mut on = c.clone();
    on.disabled_providers.clear();
    assert_eq!(on.default_model().as_deref(), Some("disabled/configured"));
}

// A configured default is kept even when its provider has no key: only a
// disabled provider is passed over, so a prompt never goes to a provider
// the user did not pick.
#[test]
fn a_configured_default_without_a_key_is_kept() {
    let c = config(
        json!({"model": "keyless/m", "providers": {
            "keyless": {"settings": {"baseURL": "https://k.test.invalid/v1"}, "models": {"m": {}}},
            "other": {"settings": {"baseURL": "https://o.test.invalid/v1", "apiKey": "x"},
                      "models": {"n": {}}}
        }}),
        vec![],
    );
    assert_eq!(c.default_model().as_deref(), Some("keyless/m"));
}

// Equally new models: the first listed (by provider, then id) wins, every
// time; models without a release date come after dated ones.
#[test]
fn ties_and_undated_models_are_ordered_deterministically() {
    let data = catalog(&[
        ("b", "2025-01-01", TEXT, TEXT),
        ("a", "2025-01-01", TEXT, TEXT),
    ]);
    let c = config(
        json!({"providers": {
            "test": {"settings": {"apiKey": "x"}},
            "aaa": {"settings": {"baseURL": "https://a.test.invalid/v1", "apiKey": "x"},
                    "models": {"undated": {}}}
        }}),
        data,
    );
    for _ in 0..5 {
        assert_eq!(c.default_model().as_deref(), Some("test/a"));
    }
    // With only undated models, the first listed.
    let c = config(
        json!({"providers": {
            "zed": {"settings": {"baseURL": "https://z.test.invalid/v1", "apiKey": "x"}, "models": {"m": {}}},
            "ábc": {"settings": {"baseURL": "https://u.test.invalid/v1", "apiKey": "x"}, "models": {"模型": {}}}
        }}),
        vec![],
    );
    assert_eq!(c.default_model().as_deref(), Some("zed/m"));
}

// Nothing to run: no default, and every provider disabled.
#[test]
fn no_model_when_nothing_is_available() {
    let c = config(
        json!({"disabled_providers": ["p", ""], "providers": {
            "p": {"settings": {"baseURL": "https://p.test.invalid/v1", "apiKey": "x"}, "models": {"m": {}}}
        }}),
        vec![],
    );
    assert_eq!(c.newest_model(), None);
    // An empty or odd entry disables nothing else.
    let c = config(
        json!({"disabled_providers": ["", "\u{0}", "p/m", "P"], "providers": {
            "p": {"settings": {"baseURL": "https://p.test.invalid/v1", "apiKey": "x"}, "models": {"m": {}}}
        }}),
        vec![],
    );
    assert_eq!(c.newest_model().as_deref(), Some("p/m"));
}

// Every config file's `disabled_providers` counts: a later file cannot
// turn a provider an earlier one disabled back on.
#[test]
fn disabled_providers_accumulate_across_config_files() {
    let mut base = json!({"disabled_providers": ["a"]});
    config::merge(&mut base, json!({"disabled_providers": ["b"]}));
    assert_eq!(base["disabled_providers"], json!(["a", "b"]));
    config::merge(&mut base, json!({"disabled_providers": []}));
    assert_eq!(base["disabled_providers"], json!(["a", "b"]));
}

fn session(v: Value) -> anyhow::Result<Session> {
    support::isolate();
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let config: Config = serde_json::from_value(v).unwrap();
    let loaded = Loaded {
        config,
        files: vec![],
        root: root.clone(),
    };
    Session::new(
        &loaded,
        root,
        Options {
            agent: None,
            model: None,
            resume: None,
            title: None,
            parent: None,
            questioner: None,
            mcp: None,
            id: None,
        },
        Arc::new(Fixed(Reply::Always)),
        None,
    )
}

// A session started with no model configured runs the available one
// (before, it refused to start).
#[test]
fn a_session_without_a_model_runs_the_available_one() {
    let s = session(json!({"snapshots": false, "providers": {
        "solo": {"settings": {"baseURL": "https://s.test.invalid/v1", "apiKey": "x"}, "models": {"m": {}}}
    }}))
    .unwrap();
    assert_eq!(s.model_spec, "solo/m");
    // A disabled provider's default falls back the same way.
    let s = session(json!({"snapshots": false, "model": "gone/x", "disabled_providers": ["gone"],
        "providers": {
            "gone": {"settings": {"baseURL": "https://g.test.invalid/v1", "apiKey": "x"}, "models": {"x": {}}},
            "solo": {"settings": {"baseURL": "https://s.test.invalid/v1", "apiKey": "x"}, "models": {"m": {}}}
        }}))
    .unwrap();
    assert_eq!(s.model_spec, "solo/m");
    // With nothing available it still refuses, saying what to do.
    let err = session(json!({"snapshots": false, "disabled_providers": ["solo"], "providers": {
        "solo": {"settings": {"baseURL": "https://s.test.invalid/v1", "apiKey": "x"}, "models": {"m": {}}}
    }}))
    .err()
    .unwrap()
    .to_string();
    assert!(err.contains("no model configured or available"), "{err}");
}
