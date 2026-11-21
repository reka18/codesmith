//! Ports of OpenCode's Markdown agent file tests
//! (`packages/core/test/config/agent.test.ts`, MIT) against
//! `agent::from_markdown`, `agent::documents`, config loading and
//! `agent::resolve`. Each test names the OpenCode test it ports; see
//! `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: OpenCode's `Model.Ref` is CodeSmith's `provider/model#variant`
//! spec, its `request.settings` has no counterpart, and its default rules
//! are `permission::base_rules()`.

use codesmith_core::agent::{self, Agent};
use codesmith_core::config::{self, Config, Sources};
use codesmith_core::permission::{base_rules, evaluate, Effect, Rule};
use serde_json::{json, Value};
use std::path::Path;

/// The agent one Markdown file defines, as `agents.test`.
fn markdown_agent(frontmatter: &str) -> Agent {
    let text = format!("---\n{frontmatter}\n---\nUse the test agent.");
    let value = agent::from_markdown(&text, false).expect("the file loads");
    let c: Config = serde_json::from_value(json!({"agents": {"test": value}})).unwrap();
    agent::resolve(&c, "test").unwrap()
}

fn rule(action: &str, resource: &str, effect: Effect) -> Rule {
    Rule::new(action, resource, effect)
}

/// The config `dir` loads as a global config folder (its `codesmith.json`
/// first, then its agent files), with no project layers.
fn load_dir(dir: &Path) -> Config {
    let sources = Sources {
        global_dir: dir.to_path_buf(),
        file: None,
        content: None,
        project: false,
    };
    config::load_from(dir, &sources).unwrap().config
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// "keeps schema fields and name out of legacy agent options": unknown
/// legacy fields and `options` become request body fields; known ones
/// (and the name) do not (adapted: `temperature` and `top_p` join them, as
/// OpenCode's migration puts them in the body too).
#[test]
fn oc_agent_keeps_schema_fields_and_name_out_of_legacy_agent_options() {
    let text = "---\nname: reviewer\nmodel: test/model\nvariant: high\ntemperature: 0.5\ntop_p: 0.9\nprompt: Review carefully.\ntools:\n  edit: false\ndisable: false\ndescription: Reviews changes\nmode: subagent\nhidden: true\noptions:\n  existing: true\ncolor: \"#112233\"\nsteps: 10\nmaxSteps: 20\npermission:\n  read: allow\ncustom: preserved\n---\n";
    let v = agent::from_markdown(text, false).unwrap();
    assert_eq!(
        v["request"]["body"],
        json!({"existing": true, "custom": "preserved", "temperature": 0.5, "top_p": 0.9})
    );
    assert_eq!(v["model"], json!("test/model#high"));
    assert_eq!(v["steps"], json!(10), "steps beats maxSteps");
    assert_eq!(v["color"], json!("#112233"));
    assert_eq!(v["hidden"], json!(true));
    assert_eq!(v["disabled"], json!(false));
    assert_eq!(
        v["system"],
        Value::Null,
        "the empty body is the prompt, so none"
    );
    assert_eq!(
        v["permissions"],
        json!([
            {"action": "edit", "resource": "*", "effect": "deny"},
            {"action": "read", "resource": "*", "effect": "allow"}
        ])
    );
}

/// "loads Markdown ${item.name}${native ? " with native request and
/// permissions" : ""}" (16 cases): the model selection in every form, with
/// and without the native `request` and `permissions` fields.
#[test]
fn oc_agent_loads_markdown_model_selections_with_and_without_native_fields() {
    let items: [(&str, &str, Option<&str>); 8] = [
        (
            "separate legacy variant",
            "model: example/chat\nvariant: high",
            Some("example/chat#high"),
        ),
        (
            "unqualified model",
            "model: example/chat",
            Some("example/chat"),
        ),
        (
            "embedded native variant",
            "model: example/chat#high",
            Some("example/chat#high"),
        ),
        (
            "structured native variant",
            "model:\n  providerID: example\n  model: chat\n  variant: high",
            Some("example/chat#high"),
        ),
        (
            "structured unqualified model",
            "model:\n  providerID: example\n  model: chat",
            Some("example/chat"),
        ),
        ("standalone variant", "variant: high", None),
        (
            "embedded native variant with an ignored separate variant",
            "model: example/chat#high\nvariant: low",
            Some("example/chat#high"),
        ),
        (
            "structured native variant with an ignored separate variant",
            "model:\n  providerID: example\n  model: chat\n  variant: high\nvariant: low",
            Some("example/chat#high"),
        ),
    ];
    for (name, frontmatter, model) in items {
        for native in [false, true] {
            let text = if native {
                format!("{frontmatter}\nrequest:\n  headers:\n    x-agent: native\n  body:\n    effort: high\npermissions:\n  - action: edit\n    resource: \"*\"\n    effect: deny")
            } else {
                frontmatter.to_string()
            };
            let a = markdown_agent(&text);
            assert_eq!(a.model.as_deref(), model, "{name} (native: {native})");
            let headers: Vec<(&str, &str)> = a
                .request
                .headers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let body = Value::Object(a.request.body.clone());
            if native {
                assert_eq!(headers, [("x-agent", "native")], "{name}");
                assert_eq!(body, json!({"effort": "high"}), "{name}");
                assert!(a.permissions.contains(&rule("edit", "*", Effect::Deny)));
                assert_eq!(
                    evaluate("edit", "example.txt", &a.permissions),
                    Effect::Deny
                );
            } else {
                assert!(headers.is_empty(), "{name}");
                assert_eq!(body, json!({}), "{name}");
            }
        }
    }
}

/// "keeps Markdown legacy permission precedence in source order"
#[test]
fn oc_agent_keeps_markdown_legacy_permission_precedence_in_source_order() {
    let a = markdown_agent("permission:\n  \"*\": allow\n  bash: ask\n  edit: deny");
    assert_eq!(evaluate("shell", "ls", &a.permissions), Effect::Ask);
    assert_eq!(
        evaluate("edit", "example.txt", &a.permissions),
        Effect::Deny
    );
    assert_eq!(evaluate("webfetch", "*", &a.permissions), Effect::Allow);
}

/// "loads Markdown legacy temperature ${variant ? "with" : "without"} a
/// separate variant" (2 cases)
#[test]
fn oc_agent_loads_markdown_legacy_temperature_with_and_without_a_variant() {
    for variant in [None, Some("high")] {
        let mut frontmatter = "model: example/chat\ntemperature: 0.5".to_string();
        if let Some(v) = variant {
            frontmatter.push_str(&format!("\nvariant: {v}"));
        }
        let a = markdown_agent(&frontmatter);
        let want = if variant.is_some() {
            "example/chat#high"
        } else {
            "example/chat"
        };
        assert_eq!(a.model.as_deref(), Some(want));
        assert!(a.request.headers.is_empty());
        assert_eq!(
            Value::Object(a.request.body.clone()),
            json!({"temperature": 0.5})
        );
    }
}

/// "loads legacy file-based agents from config directories": agent files
/// in `agents/` (nested ones named by their path) and `modes/`, over a
/// JSON definition loaded before the folder.
#[test]
fn oc_agent_loads_legacy_file_based_agents_from_config_directories() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path();
    write(
        &dir.join("codesmith.json"),
        r#"{"agents": {"reviewer": {"description": "JSON description"}}}"#,
    );
    write(
        &dir.join("agents/.codesmith/node_modules/probe/README.md"),
        "# Dependency README",
    );
    write(
        &dir.join("agents/reviewer.md"),
        "---\nmodel: openrouter/openai/gpt-5\ndescription: Markdown description\ntemperature: 0.5\ntools:\n  write: false\n---\nReview carefully.",
    );
    write(&dir.join("agents/team/helper.md"), "Help the team.");
    write(
        &dir.join("agents/native.md"),
        "---\nvariant: high\nrequest:\n  headers:\n    x-agent: native\n  body:\n    effort: high\npermissions:\n  - action: edit\n    resource: \"*\"\n    effect: deny\n---\nUse native v2 fields.",
    );
    write(
        &dir.join("agents/themed.md"),
        "---\ndescription: Themed\nmode: subagent\ncolor: accent\n---\nUse a theme color.",
    );
    write(
        &dir.join("agents/disabled.md"),
        "---\ndisabled: true\n---\nDisabled",
    );
    write(&dir.join("agents/empty.md"), "");
    write(&dir.join("modes/plan.md"), "Make a plan.");
    let c = load_dir(dir);
    let mut own = base_rules();
    // Every agent's built-in ask before `codesmith models swap`.
    own.extend(codesmith_core::providers::swap::shell_rules());
    own.push(rule("edit", "*", Effect::Deny));

    let reviewer = agent::resolve(&c, "reviewer").unwrap();
    assert_eq!(reviewer.model.as_deref(), Some("openrouter/openai/gpt-5"));
    assert_eq!(reviewer.system.as_deref(), Some("Review carefully."));
    assert_eq!(reviewer.description, "Markdown description");
    assert_eq!(
        Value::Object(reviewer.request.body.clone()),
        json!({"temperature": 0.5})
    );
    assert_eq!(reviewer.permissions, own);

    let helper = agent::resolve(&c, "team/helper").unwrap();
    assert_eq!(helper.system.as_deref(), Some("Help the team."));

    let native = agent::resolve(&c, "native").unwrap();
    assert_eq!(native.system.as_deref(), Some("Use native v2 fields."));
    assert_eq!(native.request.headers["x-agent"], "native");
    assert_eq!(
        Value::Object(native.request.body.clone()),
        json!({"effort": "high"})
    );
    assert_eq!(native.permissions, own);

    let themed = agent::resolve(&c, "themed").unwrap();
    assert_eq!(themed.system.as_deref(), Some("Use a theme color."));
    assert_eq!(themed.description, "Themed");
    assert_eq!(themed.color, None, "a theme token is not a color");

    assert!(agent::resolve(&c, "disabled").is_none());
    assert!(agent::resolve(&c, "empty").is_none());
    assert!(agent::resolve(&c, ".codesmith/node_modules/probe/README").is_none());
    let plan = agent::resolve(&c, "plan").unwrap();
    assert_eq!(plan.system.as_deref(), Some("Make a plan."));
    assert_eq!(plan.mode, "primary");
}

/// "reads agent files that use V2 fields as V2": a file with any field
/// only the current format has is read in it, legacy fields ignored.
#[test]
fn oc_agent_reads_agent_files_that_use_v2_fields_as_v2() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path();
    write(
        &dir.join("agents/named.md"),
        "---\nname: named\ndescription: Named reviewer\npermissions:\n  - action: edit\n    resource: \"*\"\n    effect: deny\n---\nReview only.",
    );
    write(
        &dir.join("agents/mixed.md"),
        "---\ntools:\n  shell: false\npermissions:\n  - action: edit\n    resource: \"*\"\n    effect: deny\n---\nMixed fields.",
    );
    let c = load_dir(dir);
    let mut own = base_rules();
    // Every agent's built-in ask before `codesmith models swap`.
    own.extend(codesmith_core::providers::swap::shell_rules());
    own.push(rule("edit", "*", Effect::Deny));
    let named = agent::resolve(&c, "named").unwrap();
    assert_eq!(named.description, "Named reviewer");
    assert_eq!(named.permissions, own);
    assert!(named.request.body.is_empty());
    let mixed = agent::resolve(&c, "mixed").unwrap();
    assert_eq!(mixed.permissions, own, "the legacy tools map is ignored");
    assert!(mixed.request.body.is_empty());
}

/// "ignores named agent color tokens"
#[test]
fn oc_agent_ignores_named_agent_color_tokens() {
    let c: Config = serde_json::from_value(
        json!({"agents": {"reviewer": {"description": "Reviews", "color": "warning"}}}),
    )
    .unwrap();
    let r = &c.agents["reviewer"];
    assert_eq!(r.description.as_deref(), Some("Reviews"));
    assert_eq!(r.color, None);
}

/// "maps configured agent fields and preserves an unspecified model
/// variant": every field, with `request` maps merged across two layers.
#[test]
fn oc_agent_maps_configured_agent_fields_and_merges_request_maps() {
    let d = tempfile::tempdir().unwrap();
    let global = d.path().join("global");
    let project = d.path().join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    write(
        &global.join("codesmith.json"),
        &json!({"agents": {"reviewer": {
            "model": "anthropic/claude-sonnet",
            "system": "Review carefully.",
            "description": "Reviews changes",
            "mode": "subagent",
            "hidden": true,
            "color": "#ff6b6b",
            "steps": 12,
            "request": {
                "headers": {"first": "one", "shared": "first"},
                "body": {"enabled": true, "profile": "review", "effort": "medium"}
            }
        }}})
        .to_string(),
    );
    write(
        &project.join("codesmith.json"),
        &json!({"agents": {"reviewer": {"request": {
            "headers": {"shared": "last", "second": "two"},
            "body": {"retries": 2, "effort": "high"}
        }}}})
        .to_string(),
    );
    let sources = Sources {
        global_dir: global,
        file: None,
        content: None,
        project: true,
    };
    let c = config::load_from(&project, &sources).unwrap().config;
    let r = agent::resolve(&c, "reviewer").unwrap();
    assert_eq!(r.system.as_deref(), Some("Review carefully."));
    assert_eq!(r.description, "Reviews changes");
    assert_eq!(r.mode, "subagent");
    assert!(r.hidden);
    assert_eq!(r.color.as_deref(), Some("#ff6b6b"));
    assert_eq!(r.steps, Some(12));
    assert_eq!(r.model.as_deref(), Some("anthropic/claude-sonnet"));
    let headers: Vec<(&str, &str)> = r
        .request
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        headers,
        [("first", "one"), ("second", "two"), ("shared", "last")]
    );
    assert_eq!(
        Value::Object(r.request.body.clone()),
        json!({"enabled": true, "profile": "review", "retries": 2, "effort": "high"})
    );
    // Hidden: not offered for delegation, and never the default agent.
    assert!(!agent::subagents(&c).iter().any(|(id, _)| id == "reviewer"));
    assert!(agent::delegable(&c, "reviewer"), "but it can be named");
}

/// The default agent skips a hidden or subagent `default_agent`, as
/// OpenCode's `selectedDefault` does.
#[test]
fn oc_agent_the_default_agent_is_a_selectable_primary_agent() {
    let c = |v: Value| -> Config { serde_json::from_value(v).unwrap() };
    assert_eq!(agent::default_id(&c(json!({}))), "build");
    assert_eq!(
        agent::default_id(&c(json!({"default_agent": "plan"}))),
        "plan"
    );
    assert_eq!(
        agent::default_id(&c(json!({"default_agent": "explore"}))),
        "build",
        "a subagent is not a default"
    );
    assert_eq!(
        agent::default_id(&c(
            json!({"default_agent": "secret", "agents": {"secret": {"hidden": true}}})
        )),
        "build"
    );
    assert_eq!(
        agent::default_id(&c(json!({"agents": {"build": {"disabled": true}}}))),
        "plan",
        "the first selectable agent when build is gone"
    );
}
