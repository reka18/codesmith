//! Ports of OpenCode's `tool.use` policy tests (`config/policy.test.ts`):
//! policy statements deny permissions as `action:resource` and never grant.

use codesmith_core::config::Config;
use codesmith_core::permission::{
    Asker, Denied, Effect, Fixed, Gate, Policy, PolicyEffect, Reply, Request, Rule,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Counting(Arc<AtomicUsize>);
impl Asker for Counting {
    fn ask(&self, _: &Request) -> Reply {
        self.0.fetch_add(1, Ordering::SeqCst);
        Reply::Once
    }
}

fn tool(effect: &str, resource: &str) -> Policy {
    Policy::tool(
        if effect == "deny" {
            PolicyEffect::Deny
        } else {
            PolicyEffect::Allow
        },
        resource,
    )
}

/// What a request comes to when the rules alone give `effect`: OpenCode's
/// `permission.evaluate` hook result, and the deny's message.
fn evaluate(
    policies: &[Policy],
    action: &str,
    resources: &[&str],
    effect: Effect,
) -> (Effect, Option<String>) {
    let mut rules = vec![Rule::new("*", "*", effect)];
    rules.extend(policies.iter().filter_map(|p| p.rule()));
    let asked = Arc::new(AtomicUsize::new(0));
    let g = Gate::new(rules, Arc::new(Counting(asked.clone())));
    let req = Request {
        action: action.into(),
        resources: resources.iter().map(|r| r.to_string()).collect(),
        save: vec![],
        title: String::new(),
        access: None,
    };
    match g.assert(&req) {
        Err(Denied::Policy { message }) => (Effect::Deny, Some(message)),
        Err(_) => (Effect::Deny, None),
        Ok(()) if asked.load(Ordering::SeqCst) > 0 => (Effect::Ask, None),
        Ok(()) => (Effect::Allow, None),
    }
}

#[test]
fn oc_policy_preserves_last_match_precedence_for_tool_use_without_granting_approvals() {
    let p = [
        tool("deny", "shell:*"),
        tool("allow", "shell:git *"),
        tool("deny", "shell:git push *"),
    ];
    assert_eq!(
        evaluate(&p, "shell", &["git status"], Effect::Ask).0,
        Effect::Ask
    );
    assert_eq!(
        evaluate(&p, "shell", &["git push origin main"], Effect::Allow).0,
        Effect::Deny
    );
    assert_eq!(
        evaluate(&p, "shell", &["ls"], Effect::Allow).0,
        Effect::Deny
    );
    assert_eq!(
        evaluate(&p, "edit", &["notes.md"], Effect::Allow).0,
        Effect::Allow
    );
}

#[test]
fn oc_policy_matches_tool_use_file_policies_without_case_where_file_systems_ignore_it_by_default() {
    let p = [tool("deny", "read:*.env"), tool("deny", "shell:git push*")];
    let files = cfg!(target_os = "macos");
    let read = evaluate(&p, "read", &["secrets.ENV"], Effect::Allow).0;
    assert_eq!(read, if files { Effect::Deny } else { Effect::Allow });
    // Commands keep their case.
    assert_eq!(
        evaluate(&p, "shell", &["GIT PUSH origin dev"], Effect::Allow).0,
        Effect::Allow
    );
}

#[test]
fn oc_policy_denies_permissions_matched_as_action_resource() {
    let p = [tool("deny", "shell:git push *")];
    let blocked = (
        Effect::Deny,
        Some("Blocked by configuration policy".to_string()),
    );
    assert_eq!(evaluate(&p, "shell", &["git push"], Effect::Allow), blocked);
    assert_eq!(
        evaluate(&p, "shell", &["git push origin main"], Effect::Allow),
        blocked
    );
    // Compound commands check several resources; any denied one denies.
    assert_eq!(
        evaluate(&p, "shell", &["git status", "git push"], Effect::Allow).0,
        Effect::Deny
    );
    assert_eq!(
        evaluate(&p, "shell", &["git status"], Effect::Allow),
        (Effect::Allow, None)
    );
    assert_eq!(
        evaluate(&p, "edit", &["git push"], Effect::Allow),
        (Effect::Allow, None)
    );
}

#[test]
fn oc_policy_turns_an_ask_into_a_deny_but_never_grants() {
    let p = [tool("deny", "webfetch:*"), tool("allow", "shell:*")];
    assert_eq!(
        evaluate(&p, "webfetch", &["https://example.com"], Effect::Ask).0,
        Effect::Deny
    );
    assert_eq!(evaluate(&p, "shell", &["ls"], Effect::Ask).0, Effect::Ask);
}

#[test]
fn oc_policy_lets_a_later_allow_lift_an_earlier_broad_deny() {
    let p = [tool("deny", "shell:*"), tool("allow", "shell:git status *")];
    assert_eq!(
        evaluate(&p, "shell", &["git status --short"], Effect::Allow).0,
        Effect::Allow
    );
    assert_eq!(
        evaluate(&p, "shell", &["rm -rf /"], Effect::Allow).0,
        Effect::Deny
    );
}

#[test]
fn oc_policy_denies_every_permission_with_a_bare_wildcard() {
    let p = [tool("deny", "*")];
    for (action, resource) in [
        ("question", "*"),
        ("read", "/tmp/notes.txt"),
        ("github_delete_repository", "*"),
    ] {
        assert_eq!(
            evaluate(&p, action, &[resource], Effect::Allow).0,
            Effect::Deny,
            "{action}"
        );
    }
}

#[test]
fn a_saved_approval_never_lifts_a_policy_deny() {
    let mut rules = vec![Rule::new("*", "*", Effect::Ask)];
    rules.extend(tool("deny", "shell:rm *").rule());
    let g = Gate::new(rules, Arc::new(Fixed(Reply::Always)));
    let req = |cmd: &str| Request {
        action: "shell".into(),
        resources: vec![cmd.into()],
        save: vec!["*".into()],
        title: String::new(),
        access: None,
    };
    g.assert(&req("ls")).unwrap();
    assert!(!g.saved().is_empty());
    assert!(matches!(
        g.assert(&req("rm -rf x")),
        Err(Denied::Policy { .. })
    ));
    assert!(!g.allows("shell", &["rm -rf x".into()]));
    assert!(g.assert_not_denied("shell", &["rm x".into()]).is_err());
}

#[test]
fn config_policies_reach_every_agent_and_the_earlier_file_has_the_final_say() {
    let mut global = json!({"policies": [
        {"action": "tool.use", "resource": "shell:git push *", "effect": "deny", "message": "No pushing"}
    ]});
    // A project file cannot lift the user's deny.
    codesmith_core::config::merge(
        &mut global,
        json!({"policies": [{"action": "tool.use", "resource": "shell:*", "effect": "allow"}]}),
    );
    let c: Config = serde_json::from_value(global).unwrap();
    assert_eq!(c.policies.len(), 2);
    assert_eq!(c.policies[0].effect, PolicyEffect::Allow);
    for id in ["build", "plan", "explore"] {
        let a = codesmith_core::agent::resolve(&c, id).unwrap();
        let g = Gate::new(a.permissions, Arc::new(Fixed(Reply::Once)));
        let e = g
            .assert(&Request {
                action: "shell".into(),
                resources: vec!["git push origin".into()],
                save: vec![],
                title: String::new(),
                access: None,
            })
            .unwrap_err();
        assert_eq!(e.to_string(), "Permission denied: No pushing", "{id}");
    }
}
