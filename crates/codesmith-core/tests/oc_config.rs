//! Ports of OpenCode's agent configuration tests (`packages/core/test/`
//! `agent.test.ts`, `config/agent.test.ts`, MIT) against `agent::resolve`
//! and `permission::evaluate`. See `crates/codesmith-core/PARITY.md`.

use codesmith_core::agent;
use codesmith_core::config::Config;
use codesmith_core::permission::{evaluate, Effect};
use serde_json::json;

fn config(v: serde_json::Value) -> Config {
    serde_json::from_value(v).unwrap()
}

/// "allows external directories without opting built-in agents into bash":
/// the explore agent's read-only rules.
#[test]
fn oc_agent_explore_is_read_only_and_asks_for_env_files() {
    let c = config(json!({}));
    let explore = agent::resolve(&c, "explore").unwrap();
    let p = &explore.permissions;
    assert_eq!(evaluate("shell", "git log -5", p), Effect::Allow);
    assert_eq!(evaluate("edit", "src/index.ts", p), Effect::Deny);
    assert_eq!(evaluate("read", ".env", p), Effect::Ask);
    assert_eq!(evaluate("read", ".env.local", p), Effect::Ask);
    assert_eq!(evaluate("read", ".env.example", p), Effect::Allow);
    assert_eq!(evaluate("read", "src/index.ts", p), Effect::Allow);
}

/// "denies the subagent tool for built-in subagents"
#[test]
fn oc_agent_built_in_subagents_deny_the_subagent_tool() {
    let c = config(json!({}));
    for id in ["general", "explore"] {
        let a = agent::resolve(&c, id).unwrap();
        assert_eq!(a.mode, "subagent", "{id}");
        assert_eq!(
            evaluate("subagent", "*", &a.permissions),
            Effect::Deny,
            "{id}"
        );
    }
}

/// "removes a built-in agent disabled by configuration"
#[test]
fn oc_agent_a_built_in_agent_disabled_by_config_is_removed() {
    let c = config(json!({"agents": {"build": {"disabled": true}}}));
    assert!(agent::resolve(&c, "build").is_none());
    assert!(!agent::list(&c).contains(&"build".to_string()));
}

/// "applies all global permissions before agent-specific permissions"
#[test]
fn oc_agent_global_permissions_come_before_agent_specific_ones() {
    let c = config(json!({
        "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}],
        "agents": {
            "build": {"permissions": [{"action": "shell", "resource": "git *", "effect": "allow"}]},
            "reviewer": {
                "description": "Review changes",
                "mode": "subagent",
                "permissions": [
                    {"action": "edit", "resource": "*", "effect": "deny"},
                    {"action": "read", "resource": "*", "effect": "deny"}
                ]
            }
        }
    }));
    let build = agent::resolve(&c, "build").unwrap();
    assert_eq!(
        evaluate("shell", "git status", &build.permissions),
        Effect::Allow
    );
    assert_eq!(
        evaluate("shell", "rm -rf x", &build.permissions),
        Effect::Ask
    );
    let reviewer = agent::resolve(&c, "reviewer").unwrap();
    assert_eq!(reviewer.mode, "subagent");
    assert_eq!(reviewer.description, "Review changes");
    assert_eq!(
        evaluate("edit", "a.rs", &reviewer.permissions),
        Effect::Deny
    );
    assert_eq!(
        evaluate("read", "a.rs", &reviewer.permissions),
        Effect::Deny
    );
    assert_eq!(evaluate("shell", "ls", &reviewer.permissions), Effect::Ask);
}
