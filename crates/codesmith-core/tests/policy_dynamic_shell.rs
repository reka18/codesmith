//! A `tool.use` policy deny refuses what the shell tool cannot see into (a
//! computed command name, an evaluated string, a write to a computed path,
//! a `cd` to a computed directory), as the same deny written as a rule does.

use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{
    base_rules, Effect, Fixed, Gate, Policy, PolicyEffect, Reply, Rule,
};
use codesmith_core::tool::{self, Ctx};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

async fn run(gate: &Gate, dir: &Path, cmd: &str) -> (String, bool) {
    let ctx = Ctx {
        cwd: dir.to_path_buf(),
        root: dir.to_path_buf(),
        gate,
        shell: "/bin/sh".into(),
        session_id: "policy-dynamic".into(),
        limits: ToolOutputConfig::default(),
        client: reqwest::Client::new(),
        jobs: None,
        websearch: None,
        format: None,
        lsp: None,
        progress: None,
    };
    tool::execute(&ctx, "shell", &json!({"command": cmd, "description": "x"})).await
}

fn gate(extra: Vec<Rule>) -> Gate {
    let mut rules = base_rules();
    rules.extend(extra);
    Gate::new(rules, Arc::new(Fixed(Reply::Once)))
}

fn policy(effect: PolicyEffect, resource: &str) -> Rule {
    Policy::tool(effect, resource).rule().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_deny_refuses_dynamic_commands_like_a_rule() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().canonicalize().unwrap();
    for (label, extra) in [
        ("policy", policy(PolicyEffect::Deny, "shell:touch *")),
        ("rule", Rule::new("shell", "touch *", Effect::Deny)),
    ] {
        let gate = gate(vec![extra]);
        let direct = run(&gate, &dir, &format!("touch direct_{label}")).await;
        assert!(direct.1, "[{label}] direct: {}", direct.0);
        assert!(!dir.join(format!("direct_{label}")).exists());

        let dynamic = run(&gate, &dir, &format!("c=touch; $c dyn_{label}")).await;
        assert!(dynamic.1, "[{label}] dynamic: {}", dynamic.0);
        assert!(!dir.join(format!("dyn_{label}")).exists());

        let ev = run(&gate, &dir, &format!("eval \"touch ev_{label}\"")).await;
        assert!(ev.1, "[{label}] eval: {}", ev.0);
        assert!(!dir.join(format!("ev_{label}")).exists());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_deny_on_git_push_refuses_a_computed_git() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().canonicalize().unwrap();
    let gate = gate(vec![policy(PolicyEffect::Deny, "shell:git push *")]);
    let (out, err) = run(&gate, &dir, "c=git; $c push").await;
    assert!(err, "{out}");
    assert!(out.contains("policy"), "{out}");
    let (out, err) = run(&gate, &dir, "eval 'git push'").await;
    assert!(err, "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_deny_on_edit_refuses_an_unknown_write_target() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().canonicalize().unwrap();
    // Everything else allowed, so only the policy can refuse.
    let gate = Gate::new(
        vec![
            Rule::new("*", "*", Effect::Allow),
            policy(PolicyEffect::Deny, "edit:*secret*"),
        ],
        Arc::new(Fixed(Reply::Once)),
    );
    let (out, err) = run(&gate, &dir, "echo hi > \"$(printf secret.txt)\"").await;
    assert!(err, "{out}");
    assert!(!dir.join("secret.txt").exists());
    // A visible write the policy does not cover still runs.
    let (out, err) = run(&gate, &dir, "echo hi > plain.txt").await;
    assert!(!err, "{out}");
    assert!(dir.join("plain.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_deny_on_external_directory_refuses_an_unknown_cwd() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().canonicalize().unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    let gate = gate(vec![policy(
        PolicyEffect::Deny,
        "external_directory:/etc/*",
    )]);
    let (out, err) = run(&gate, &dir, "d=sub; cd \"$d\" && touch moved").await;
    assert!(err, "{out}");
    assert!(!dir.join("sub/moved").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_commands_run_without_a_relevant_policy_deny() {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().canonicalize().unwrap();
    // A deny for another action, and a shell deny lifted for every command.
    let gate = gate(vec![
        policy(PolicyEffect::Deny, "webfetch:*"),
        policy(PolicyEffect::Deny, "shell:touch *"),
        policy(PolicyEffect::Allow, "shell:*"),
    ]);
    let (out, err) = run(&gate, &dir, "c=touch; $c made").await;
    assert!(!err, "{out}");
    assert!(dir.join("made").exists());
}
