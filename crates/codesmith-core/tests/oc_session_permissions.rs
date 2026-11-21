//! Ports of OpenCode's per-session permission rules test (MIT):
//! `session-create.test.ts`.
//!
//! Adapted: the rules are kept with the session's meta. They only
//! restrict, as a built-in agent's own rules do, so a session's `allow`
//! never loosens a configured deny (CodeSmith's permission invariant).

mod support;

use codesmith_core::permission::{Effect, Fixed, Reply, Rule};
use codesmith_core::session::{Options, Session};
use codesmith_core::store::NotFound;
use serde_json::json;
use std::sync::Arc;
use support::H;

fn child(h: &H, parent: &str) -> Session {
    Session::new(
        &h.loaded,
        h.loaded.root.clone(),
        Options {
            agent: None,
            model: None,
            resume: None,
            title: Some("child".into()),
            parent: Some(parent.into()),
            id: None,
            questioner: None,
            mcp: None,
        },
        Arc::new(Fixed(Reply::Once)),
        Some(h.store()),
    )
    .unwrap()
}

// session-create.test.ts: "stores permission rules, inherits them through children and forks, and replaces them"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_stores_permission_rules_inherits_them_through_children_and_forks_and_replaces_them(
) {
    let mut h = H::with(
        json!({"permissions": [{"action": "edit", "resource": "*", "effect": "allow"}]}),
        None,
    )
    .await;
    let rules = vec![Rule::new("edit", "/original/**", Effect::Deny)];
    h.s.set_permissions(rules.clone()).unwrap();
    assert_eq!(h.s.permissions(), rules);
    assert!(!h.s.allows("edit", "/original/a.ts"));
    assert!(h.s.allows("edit", "/elsewhere/a.ts"));
    h.prompt("Fork context").await;
    let parent = h.s.id.clone();
    assert_eq!(h.store().permissions(&parent).unwrap(), Some(rules.clone()));
    // Reopened, the session keeps them.
    assert_eq!(h.open(&parent).permissions(), rules);

    // A child starts with its parent's rules; its own replace them.
    let mut c = child(&h, &parent);
    assert_eq!(c.permissions(), rules);
    assert!(!c.allows("edit", "/original/a.ts"));
    c.set_permissions(vec![]).unwrap();
    assert!(c.permissions().is_empty());
    assert!(c.allows("edit", "/original/a.ts"));

    // A fork copies them.
    let mut st = h.store();
    st.fork(&parent, "fork").unwrap();
    assert_eq!(st.permissions("fork").unwrap(), Some(rules.clone()));

    let replaced = vec![Rule::new("shell", "*", Effect::Ask)];
    h.s.set_permissions(replaced.clone()).unwrap();
    assert_eq!(
        h.store().permissions(&parent).unwrap(),
        Some(replaced.clone())
    );
    assert_eq!(h.s.permissions(), replaced);
    // The edit deny is gone with them.
    assert!(h.s.allows("edit", "/original/a.ts"));

    let e = st.set_permissions("ses_missing", &replaced).unwrap_err();
    assert!(e.downcast_ref::<NotFound>().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn session_permission_rules_never_loosen_a_configured_deny() {
    let mut h = H::with(
        json!({"permissions": [{"action": "edit", "resource": "*", "effect": "deny"}]}),
        None,
    )
    .await;
    h.s.set_permissions(vec![Rule::new("edit", "*", Effect::Allow)])
        .unwrap();
    assert!(!h.s.allows("edit", "/any/file.ts"));
    // A store-level set takes effect when the session is next opened.
    h.prompt("x").await;
    let id = h.s.id.clone();
    h.store()
        .set_permissions(&id, &[Rule::new("edit", "*", Effect::Allow)])
        .unwrap();
    assert!(!h.open(&id).allows("edit", "/any/file.ts"));
}
