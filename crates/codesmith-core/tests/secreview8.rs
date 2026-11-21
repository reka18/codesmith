//! The probes of security review 8, kept as tests with their assertions
//! turned to the safe outcome. (The 300 MB `list` probe is
//! `swap_bounded.rs`, which needs a test binary of its own.)

use codesmith_core::permission::{evaluate, Effect};
use codesmith_core::providers::swap;
use serde_json::json;

#[test]
fn probe_use_model_id_not_in_known_is_not_selected() {
    let (id, warning) = swap::outcome(
        true,
        Some(0),
        Some(&json!({"model_id": "other-model"})),
        &[],
        "big",
        &[],
    )
    .unwrap();
    assert_eq!(id, "big");
    assert!(warning.is_some());
}

#[test]
fn probe_valid_names() {
    for (n, ok) in [
        ("a:b", false),
        ("u@h:/etc/passwd", false),
        ("a/-rf", true),
        ("x+y", true),
        ("a..b", true),
        ("a.", true),
        ("A", true),
        ("qwen:7b", true),
    ] {
        assert_eq!(swap::valid_name(n), ok, "{n:?}");
    }
}

#[test]
fn probe_strip_controls_drops_bidi() {
    assert_eq!(
        swap::strip_controls("ok\u{202e}evil\u{200b}\u{9b}31m"),
        "okevil31m"
    );
}

#[test]
fn probe_plan_agent_asks_before_reading_home_secrets() {
    let cfg = codesmith_core::config::Config::default();
    let plan = codesmith_core::agent::resolve(&cfg, "plan").unwrap();
    let build = codesmith_core::agent::resolve(&cfg, "build").unwrap();
    let home = dirs::home_dir().unwrap().display().to_string();
    let data = codesmith_core::paths::data_dir().display().to_string();
    for (a, r, want_plan, want_build) in [
        // The plan agent keeps OpenCode's outside access...
        (
            "external_directory",
            format!("{home}/.ssh/*"),
            Effect::Allow,
            Effect::Ask,
        ),
        // ...but secret files ask, for it and for build.
        (
            "read",
            format!("{home}/.ssh/id_ed25519"),
            Effect::Ask,
            Effect::Ask,
        ),
        (
            "read",
            format!("{data}/auth.json"),
            Effect::Ask,
            Effect::Ask,
        ),
        (
            "read",
            format!("{home}/.aws/credentials"),
            Effect::Ask,
            Effect::Ask,
        ),
        (
            "read",
            format!("{home}/proj/.env"),
            Effect::Ask,
            Effect::Ask,
        ),
        // Ordinary outside files and edits are as before.
        (
            "read",
            format!("{home}/.zshrc"),
            Effect::Allow,
            Effect::Allow,
        ),
        (
            "edit",
            format!("{home}/.zshrc"),
            Effect::Deny,
            Effect::Allow,
        ),
    ] {
        assert_eq!(
            evaluate(a, &r, &plan.permissions),
            want_plan,
            "plan {a} {r}"
        );
        assert_eq!(
            evaluate(a, &r, &build.permissions),
            want_build,
            "build {a} {r}"
        );
    }
}
