//! Ports of the agent, instruction and config cases in PARITY.md's porting
//! backlog (`packages/core/test/agent.test.ts`, `config/agent.test.ts`,
//! `instruction-discovery.test.ts`, `config/config.test.ts`,
//! `config/model.test.ts`, MIT).

mod scripted;

use codesmith_core::config::{self, Config, Sources};
use codesmith_core::instruction_state::Observed;
use codesmith_core::permission::{evaluate, Effect};
use codesmith_core::{agent, instructions, skill};
use serde_json::json;
use std::path::PathBuf;

fn config(v: serde_json::Value) -> Config {
    serde_json::from_value(v).unwrap()
}

/// "creates agents with runtime defaults and supports direct removal": a
/// configured agent without a built-in definition is primary, starts from
/// the base rules (its tool output and temporary folders reachable), and a
/// disabled one is gone.
#[test]
fn oc_agent_creates_agents_with_runtime_defaults() {
    scripted::isolate();
    let c = config(json!({"agents": {"custom": {}}}));
    let a = agent::resolve(&c, "custom").unwrap();
    assert_eq!(a.mode, "primary");
    let base = codesmith_core::permission::base_rules();
    assert_eq!(a.permissions[..base.len()], base[..]);
    let output = codesmith_core::paths::tool_output_dir().join("x");
    assert_eq!(
        evaluate(
            "external_directory",
            &output.display().to_string(),
            &a.permissions
        ),
        Effect::Allow
    );
    assert!(agent::list(&c).contains(&"custom".to_string()));
    let removed = config(json!({"agents": {"custom": {"disabled": true}}}));
    assert!(agent::resolve(&removed, "custom").is_none());
    assert!(!agent::list(&removed).contains(&"custom".to_string()));
}

/// "lists the selected default agent first". Adapted: the configured
/// default is the one a session starts with (`agent::default_id`); the
/// list keeps built-ins first.
#[test]
fn oc_agent_selects_the_configured_default() {
    let c = config(json!({
        "default_agent": "reviewer",
        "agents": {"reviewer": {"mode": "primary"}, "explore": {"mode": "subagent"}},
    }));
    assert_eq!(agent::default_id(&c), "reviewer");
    // A subagent cannot be the default.
    let c = config(json!({"default_agent": "explore"}));
    assert_eq!(agent::default_id(&c), "build");
}

/// "matches POSIX paths against home-relative permissions". Fixed: a path
/// rule written from the home folder (`~/p/**`) never matched, as paths
/// are checked absolute, so a deny such as `read ~/.ssh/*` did nothing; it
/// matches the absolute paths now (a shell rule's `$HOME` stays literal).
#[test]
fn oc_agent_matches_paths_against_home_relative_permissions() {
    scripted::isolate();
    let home = dirs::home_dir().unwrap().display().to_string();
    let c = config(json!({"permissions": [
        {"action": "external_directory", "resource": "*", "effect": "deny"},
        {"action": "external_directory", "resource": "~/p/**", "effect": "allow"},
        {"action": "shell", "resource": "$HOME/private/**", "effect": "deny"},
    ]}));
    let rules = agent::resolve(&c, "build").unwrap().permissions;
    let ext = |r: &str| evaluate("external_directory", r, &rules);
    assert_eq!(ext(&format!("{home}/p/opencode/src/*")), Effect::Allow);
    assert_eq!(ext(&format!("{home}/cache/files/*")), Effect::Deny);
    assert_eq!(ext("/some/~/path"), Effect::Deny);
    assert_eq!(ext("$HOMELESS/private/*"), Effect::Deny);
    assert_eq!(evaluate("shell", "$HOME/private/key", &rules), Effect::Deny);
    // A deny from the home folder applies to the files it names.
    let c = config(json!({"permissions": [
        {"action": "read", "resource": "~/.ssh/*", "effect": "deny"},
    ]}));
    let rules = agent::resolve(&c, "build").unwrap().permissions;
    assert_eq!(
        evaluate("read", &format!("{home}/.ssh/id_rsa"), &rules),
        Effect::Deny
    );
    assert_eq!(evaluate("read", "src/main.rs", &rules), Effect::Allow);
    // In a shell rule, `~` is the command's text.
    let c = config(json!({"permissions": [
        {"action": "shell", "resource": "cat ~/*", "effect": "deny"},
    ]}));
    let rules = agent::resolve(&c, "build").unwrap().permissions;
    assert_eq!(evaluate("shell", "cat ~/.ssh/id_rsa", &rules), Effect::Deny);
}

/// "isolates source failure without failing activation". Adapted: a
/// discovered file that cannot be read leaves the source unavailable, so
/// its stored value stands, rather than failing the session.
#[test]
fn oc_instructions_isolate_a_source_failure() {
    let d = tempfile::tempdir().unwrap();
    let gone: PathBuf = d.path().join("AGENTS.md");
    assert!(matches!(
        instructions::read(std::slice::from_ref(&gone)),
        Observed::Unavailable
    ));
    let dir = d.path().join("dir");
    std::fs::create_dir(&dir).unwrap();
    assert!(matches!(instructions::read(&[dir]), Observed::Unavailable));
}

/// "treats unreadable discovery roots as absent": a project's
/// `.codesmith` folder that cannot be searched is skipped, and its
/// `codesmith.json` still loads.
#[cfg(unix)]
#[test]
fn oc_config_treats_unreadable_discovery_roots_as_absent() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc_geteuid() } == 0 {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (global, project, locked) = (
        tmp.path().join("global"),
        tmp.path().join("project"),
        tmp.path().join("locked"),
    );
    std::fs::create_dir_all(locked.join("agents")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(
        project.join("codesmith.json"),
        r#"{"small_model": "fake/project"}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(locked.join("agents"), project.join(".codesmith")).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let loaded = config::load_from(
        &project,
        &Sources {
            global_dir: global,
            file: None,
            content: None,
            project: true,
        },
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let loaded = loaded.unwrap();
    assert_eq!(loaded.config.small_model.as_deref(), Some("fake/project"));
}

#[cfg(unix)]
unsafe fn libc_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}

/// "deduplicates global ecosystem directories found during upward
/// discovery": a project inside the home folder lists the home folder's
/// skill folders once.
#[test]
fn oc_config_lists_the_home_ecosystem_folders_once() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let roots = skill::roots(&project, &project, &tmp.path().join("global"), &home, &[]);
    for dir in [home.join(".agents/skills"), home.join(".claude/skills")] {
        assert!(
            roots.iter().filter(|r| **r == dir).count() <= 1,
            "{dir:?} in {roots:?}"
        );
    }
    assert_eq!(
        roots
            .iter()
            .filter(|r| **r == home.join(".agents/skills"))
            .count(),
        1
    );
}

/// "rejects malformed selections and reserved fragments". Adapted: a model
/// named without its provider is not found; an empty `#` selects no
/// variant.
#[test]
fn oc_config_rejects_a_model_without_its_provider() {
    scripted::isolate();
    let c = config(json!({
        "model": "fake/m",
        "providers": {"fake": {"protocol": "openai-chat", "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "x"}, "models": {"m": {}}}},
    }));
    assert!(c.resolve_model("gpt-5").is_err());
    assert_eq!(config::split_variant("fake/m#"), ("fake/m", None));
    assert_eq!(
        config::split_variant("fake/m#high"),
        ("fake/m", Some("high"))
    );
}
