//! Ports of OpenCode's model-lab prompt tests
//! (`packages/core/test/plugin/optimize.test.ts`, MIT) against
//! `prompt::base_named`, `prompt::model_name` and the session's system
//! prompt. Each test names the OpenCode test it ports; see
//! `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: OpenCode's prompts are plugins rewriting the first system
//! part; CodeSmith picks the base prompt itself (project instructions
//! follow it in the system prompt), and the prompts are rebranded.

use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::prompt;
use codesmith_core::session::{Options, Session};
use serde_json::json;
use std::sync::Arc;

const TOOLS: [&str; 7] = ["shell", "read", "grep", "glob", "edit", "write", "patch"];

const SYSTEM: &str = include_str!("../src/prompt/system.txt");
const GPT: &str = include_str!("../src/prompt/gpt.txt");
const ASTRA: &str = include_str!("../src/prompt/gpt-astra.txt");
const KIMI: &str = include_str!("../src/prompt/kimi.txt");
const TRINITY: &str = include_str!("../src/prompt/trinity.txt");
const META: &str = include_str!("../src/prompt/meta.txt");
const ANTHROPIC: &str = include_str!("../src/prompt/anthropic.txt");

fn render(t: &str) -> String {
    prompt::render(t, &TOOLS)
}

fn appended() -> String {
    format!("{}\n\n{}", render(SYSTEM), render(ANTHROPIC).trim_end())
}

/// "selects model-lab prompts through session context hooks": the prompt
/// each model id gets.
#[test]
fn oc_prompt_selects_model_lab_prompts_by_model_id() {
    let meta = render(&META.replace("{{MODEL_NAME}}", "Muse Spark"));
    let cases: [(&str, String); 13] = [
        ("gpt-5", render(GPT)),
        ("gpt-4.1", render(GPT)),
        ("o3", render(SYSTEM)),
        ("gpt-5-codex", render(GPT)),
        ("gpt-6", render(GPT)),
        ("gpt-6-astra", render(ASTRA)),
        ("gpt-5-astra", render(ASTRA)),
        ("gemini-2.5-pro", render(SYSTEM)),
        ("claude-sonnet-4", appended()),
        ("kimi-k2", render(KIMI)),
        ("trinity", render(TRINITY)),
        ("meta/muse-spark-1.1", meta),
        ("llama-3.3", render(SYSTEM)),
    ];
    for (id, want) in cases {
        let name = if id.contains("muse") {
            "Muse Spark"
        } else {
            id
        };
        assert_eq!(prompt::base_named(id, name, &TOOLS), want, "{id}");
    }
}

/// "renders the OpenAI prompt without changing tools or project
/// instructions": the tool guidance is filled in for the tools offered.
#[test]
fn oc_prompt_renders_the_openai_prompt_with_tool_guidance() {
    let p = prompt::base_named("gpt-5", "gpt-5", &TOOLS);
    assert_eq!(p, render(GPT));
    assert!(!p.contains("${"), "no placeholder left");
    assert!(p.contains("Use the edit tool for targeted changes"));
    assert!(!prompt::base_named("gpt-5", "gpt-5", &["read"]).contains("edit tool"));
}

/// "appends the Anthropic prompt to the baseline without changing tools or
/// project instructions"
#[test]
fn oc_prompt_appends_the_anthropic_prompt_to_the_baseline() {
    let p = prompt::base_named("claude-sonnet-4", "Claude Sonnet 4", &TOOLS);
    assert!(p.starts_with(&render(SYSTEM)));
    assert_eq!(p, appended());
}

/// "uses catalog names in Meta prompts for Muse model IDs": the model's
/// name (from config here, as from the catalog) fills `{{MODEL_NAME}}`.
#[test]
fn oc_prompt_uses_model_names_in_meta_prompts_for_muse_models() {
    let cases = [
        ("meta/muse-spark-preview", "Muse Spark Preview"),
        ("muse-spark-1.2", "Muse Spark 1.2"),
        ("meta/muse-glimmer-30b", "Muse Glimmer 30B"),
        ("muse-glimmer-30b", "Muse Glimmer"),
    ];
    let models: serde_json::Map<String, serde_json::Value> = cases
        .iter()
        .map(|(id, name)| (id.to_string(), json!({ "name": name })))
        .collect();
    let c: Config = serde_json::from_value(json!({
        "providers": {"test": {"protocol": "openai-chat", "settings": {"baseURL": "http://127.0.0.1:9/v1"}, "models": models}}
    }))
    .unwrap();
    for (id, name) in cases {
        let named = prompt::model_name(&c, &format!("test/{id}"));
        assert_eq!(named, name);
        let p = prompt::base_named(id, &named, &TOOLS);
        assert_eq!(p, render(&META.replace("{{MODEL_NAME}}", name)), "{id}");
        assert!(!p.contains("{{MODEL_NAME}}"));
    }
    assert_eq!(
        prompt::model_name(&c, "test/unknown-muse"),
        "unknown-muse",
        "a model without a name is named by its id"
    );
}

fn session(config: serde_json::Value, agent: Option<&str>) -> Session {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut v = json!({
        "snapshots": false,
        "providers": {"test": {"protocol": "openai-chat", "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "x"}}},
    });
    for (k, val) in config.as_object().unwrap() {
        v[k] = val.clone();
    }
    let loaded = Loaded {
        config: serde_json::from_value(v).unwrap(),
        files: vec![],
        root: root.clone(),
    };
    let opts = Options {
        agent: agent.map(String::from),
        model: None,
        resume: None,
        title: Some("t".into()),
        parent: None,
        questioner: None,
        mcp: None,
    };
    Session::new(&loaded, root, opts, Arc::new(Fixed(Reply::Once)), None).unwrap()
}

/// "preserves tools and an explicit agent system prompt by default": an
/// agent's own prompt is used whatever the model.
#[test]
fn oc_prompt_preserves_an_explicit_agent_system_prompt() {
    let s = session(
        json!({"model": "test/gpt-5", "agents": {"build": {"system": "Custom agent prompt"}}}),
        None,
    );
    assert!(s.system.starts_with("Custom agent prompt"), "{}", s.system);
    assert!(!s
        .system
        .contains("running in CodeSmith, a coding agent harness"));
    let names: Vec<&str> = s.tools.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"shell") && names.contains(&"read"));
}

/// "preserves tools for model aliases and catalog-ID prompt selection by
/// default": the prompt follows the model's id in config, not the API
/// model it stands for.
#[test]
fn oc_prompt_selects_by_the_configured_model_id_not_the_api_model() {
    // The prompt each id gets, rendered for the session's own tools.
    let gpt = |t: &[&str]| prompt::render(GPT, t);
    let astra = |t: &[&str]| prompt::render(ASTRA, t);
    let default = |t: &[&str]| prompt::render(SYSTEM, t);
    let claude = |t: &[&str]| {
        format!(
            "{}\n\n{}",
            prompt::render(SYSTEM, t),
            prompt::render(ANTHROPIC, t).trim_end()
        )
    };
    type Pick<'a> = &'a dyn Fn(&[&str]) -> String;
    // OpenCode's family cases set a catalog family CodeSmith does not read;
    // the id alone picks the default prompt, as there.
    let cases: [(&str, &str, Pick); 10] = [
        ("gpt-5-alias", "custom-model", &gpt),
        ("gpt-6-alias", "custom-model", &gpt),
        ("gpt-5-astra-alias", "custom-model", &astra),
        ("openai-alias", "GPT-5", &default),
        ("codex-family-alias", "custom-deployment", &default),
        ("astra-api-alias", "gpt-6-astra", &default),
        ("astra-family-alias", "custom-deployment", &default),
        ("claude-catalog-alias", "custom-model", &claude),
        ("anthropic-api-alias", "Claude-Opus-4-8", &default),
        ("anthropic-family-alias", "custom-deployment", &default),
    ];
    for (id, api, want) in cases {
        let s = session(
            json!({
                "model": format!("test/{id}"),
                "providers": {"test": {"protocol": "openai-chat", "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "x"}, "models": {id: {"id": api}}}}
            }),
            None,
        );
        let names: Vec<&str> = s.tools.iter().map(|t| t.name()).collect();
        assert!(s.system.starts_with(&want(&names)), "{id}");
        assert_eq!(
            s.model.model_id, api,
            "{id}: requests still name the API model"
        );
    }
}
