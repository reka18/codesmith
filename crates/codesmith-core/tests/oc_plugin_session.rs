//! Ports of OpenCode's plugin session API tests (`packages/core/test/`
//! `plugin/session-*.test.ts` and `plugin.test.ts`, MIT): a plugin creates,
//! lists, gets and removes sessions, manages their instruction entries,
//! and admits compactions, through the plugin host. OpenCode runs each test
//! for its Effect and its Promise plugin APIs; CodeSmith has one Rust API,
//! so the two ports of a pair check the parts each covers. See
//! `crates/codesmith-core/PARITY.md`.

mod plugin_support;

use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::plugin::host::{Delivery, ListQuery, NewSession, Parent};
use codesmith_core::plugin::{self, Host, HostError};
use codesmith_core::session::Session;
use codesmith_core::store::Store;
use plugin_support::{loaded, options, serve, text, Collect};
use serde_json::json;
use std::sync::Arc;

struct H {
    dir: tempfile::TempDir,
    host: Arc<Host>,
}

impl H {
    fn new() -> H {
        let dir = tempfile::tempdir().unwrap();
        let host =
            Arc::new(Host::open(&dir.path().join("db.sqlite"), dir.path().to_path_buf()).unwrap());
        H { dir, host }
    }
    fn titled(&self, title: &str) -> codesmith_core::store::Info {
        self.host
            .create(NewSession {
                title: Some(title.into()),
                ..Default::default()
            })
            .unwrap()
    }
    fn child(&self, parent: &str, title: Option<&str>) -> codesmith_core::store::Info {
        self.host
            .create(NewSession {
                parent_id: Some(parent.into()),
                title: title.map(str::to_string),
                ..Default::default()
            })
            .unwrap()
    }
    /// A session object continuing stored session `id`.
    fn session(&self, base: &str, id: &str) -> Session {
        let l = loaded(self.dir.path(), base, json!({}));
        let mut o = options();
        o.resume = Some(id.to_string());
        let store = Store::open(&self.dir.path().join("db.sqlite")).unwrap();
        Session::new(
            &l,
            self.dir.path().to_path_buf(),
            o,
            Arc::new(Fixed(Reply::Once)),
            Some(store),
        )
        .unwrap()
    }
}

fn not_found(e: HostError, id: &str) {
    match e {
        HostError::NotFound(got) => assert_eq!(got, id),
        e => panic!("expected NotFound, got {e}"),
    }
}

// plugin.test.ts: "creates child sessions from a plugin at the parent's location"
#[test]
fn oc_creates_child_sessions_from_a_plugin_at_the_parents_location() {
    let h = H::new();
    let elsewhere = h.dir.path().join("elsewhere");
    let parent = h
        .host
        .create(NewSession {
            title: Some("Parent".into()),
            directory: Some(elsewhere.clone()),
            agent: Some("plan".into()),
            model: Some("fake/m".into()),
            ..Default::default()
        })
        .unwrap();
    let child = h.child(&parent.id, Some("Child"));
    let child = h.host.get(&child.id).unwrap();
    assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
    assert_eq!(child.directory, elsewhere.display().to_string());
    assert_eq!(
        (child.agent.as_str(), child.model.as_str()),
        ("plan", "fake/m")
    );
    not_found(
        h.host
            .create(NewSession {
                parent_id: Some("ses_missing".into()),
                ..Default::default()
            })
            .unwrap_err(),
        "ses_missing",
    );
}

// plugin/session-compact.test.ts: "Effect plugins can admit session compaction"
#[tokio::test(flavor = "multi_thread")]
async fn oc_plugins_can_admit_session_compaction() {
    let h = H::new();
    plugin::set_host(h.host.clone());
    let created = h.titled("Plugin compaction");
    let admitted = h
        .host
        .compact(
            &created.id,
            Some(Delivery::Queue),
            Some("msg_admitted".into()),
        )
        .unwrap();
    assert_eq!(admitted.id, "msg_admitted");
    assert_eq!(admitted.session_id, created.id);
    assert_eq!(admitted.delivery, Delivery::Queue);
    assert_eq!(
        h.host.pending_compaction(&created.id).unwrap(),
        Some(admitted)
    );
    not_found(
        h.host.compact("ses_missing", None, None).unwrap_err(),
        "ses_missing",
    );
}

// plugin/session-compact.test.ts: "Promise plugins can admit session compaction"
#[tokio::test(flavor = "multi_thread")]
async fn oc_an_admitted_compaction_runs_at_the_sessions_next_step() {
    let h = H::new();
    plugin::set_host(h.host.clone());
    let m = serve(|r| {
        if r.last_text().contains("summar") {
            text("## Goal\n- the summary")
        } else {
            text("Done")
        }
    })
    .await;
    let created = h.titled("Promise plugin compaction");
    let mut s = h.session(&m.base, &created.id);
    let mut out = Collect::default();
    s.prompt("first", &mut out).await.unwrap();
    s.prompt("second", &mut out).await.unwrap();
    let admitted = h.host.compact(&created.id, None, None).unwrap();
    assert_eq!(admitted.delivery, Delivery::Steer, "steer by default");
    assert!(admitted.id.starts_with("msg_"));
    s.prompt("third", &mut out).await.unwrap();
    assert_eq!(h.host.pending_compaction(&created.id).unwrap(), None);
    let last = m.requests().last().unwrap().body.to_string();
    assert!(
        last.contains("the summary"),
        "the next step sees the checkpoint: {last}"
    );
}

fn context_block(key: &str, value: &str) -> String {
    format!("<context key=\"{key}\">\n{value}\n</context>")
}

fn system_of(r: &plugin_support::Seen) -> String {
    r.body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The system updates of a request, as OpenAI Chat carries them (user
/// messages wrapped in `<system-update>`, the text escaped).
fn system_updates(r: &plugin_support::Seen) -> Vec<String> {
    r.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .filter_map(|c| {
            c.strip_prefix("<system-update>\n")?
                .strip_suffix("\n</system-update>")
        })
        .map(|t| {
            t.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
        })
        .collect()
}

// plugin/session-instructions.test.ts: "Effect plugins can put, list, and
// remove session instruction entries seen by the next model request"
#[tokio::test(flavor = "multi_thread")]
async fn oc_plugins_put_list_and_remove_session_instruction_entries_seen_by_the_next_model_request()
{
    let h = H::new();
    let m = serve(|_| text("Done")).await;
    let created = h.titled("Plugin instructions");
    h.host
        .put_instruction(&created.id, "deploy-target", &json!("production"))
        .unwrap();
    assert_eq!(
        h.host.list_instructions(&created.id).unwrap(),
        [("deploy-target".to_string(), json!("production"))]
    );
    let mut s = h.session(&m.base, &created.id);
    let mut out = Collect::default();
    s.prompt("First", &mut out).await.unwrap();
    let first = m.requests()[0].clone();
    assert!(system_of(&first).ends_with(&context_block("deploy-target", "production")));
    assert!(system_updates(&first).is_empty());

    h.host
        .put_instruction(
            &created.id,
            "deploy-target",
            &json!({"region": "us-east-1"}),
        )
        .unwrap();
    s.prompt("Second", &mut out).await.unwrap();
    assert_eq!(
        system_updates(&m.requests()[1]),
        [[
            "The context under \"deploy-target\" changed and supersedes the previous value:",
            "<context key=\"deploy-target\">",
            "{",
            "  \"region\": \"us-east-1\"",
            "}",
            "</context>",
        ]
        .join("\n")]
    );

    h.host
        .remove_instruction(&created.id, "deploy-target")
        .unwrap();
    assert!(h.host.list_instructions(&created.id).unwrap().is_empty());
    s.prompt("Third", &mut out).await.unwrap();
    assert_eq!(
        system_updates(&m.requests()[2]).last().unwrap(),
        "The context under \"deploy-target\" no longer applies. Disregard it."
    );

    let missing = "ses_missing";
    not_found(h.host.list_instructions(missing).unwrap_err(), missing);
    not_found(
        h.host
            .put_instruction(missing, "deploy-target", &json!("production"))
            .unwrap_err(),
        missing,
    );
    not_found(
        h.host
            .remove_instruction(missing, "deploy-target")
            .unwrap_err(),
        missing,
    );
    assert!(matches!(
        h.host
            .put_instruction(&created.id, "oversized", &json!("x".repeat(256 * 1024)))
            .unwrap_err(),
        HostError::TooLarge { .. }
    ));
    for key in ["Bad Key", "invalid/key"] {
        assert!(matches!(
            h.host
                .put_instruction(&created.id, key, &json!("rejected"))
                .unwrap_err(),
            HostError::InvalidKey(_)
        ));
        assert!(matches!(
            h.host.remove_instruction(&created.id, key).unwrap_err(),
            HostError::InvalidKey(_)
        ));
    }
    h.host
        .put_instruction(&created.id, "recovered", &json!("still works"))
        .unwrap();
    s.prompt("Fourth", &mut out).await.unwrap();
    assert_eq!(
        system_updates(&m.requests()[3]).last().unwrap(),
        &context_block("recovered", "still works")
    );
}

// plugin/session-instructions.test.ts: "Promise plugins can put, list, and
// remove session instruction entries seen by the next model request"
#[tokio::test(flavor = "multi_thread")]
async fn oc_plugins_remove_an_instruction_entry_before_the_session_runs_again() {
    let h = H::new();
    let m = serve(|_| text("Done")).await;
    let created = h.titled("Promise plugin instructions");
    h.host
        .put_instruction(
            &created.id,
            "team.guidelines",
            &json!("Prefer small, focused changes."),
        )
        .unwrap();
    let mut s = h.session(&m.base, &created.id);
    let mut out = Collect::default();
    s.prompt("First", &mut out).await.unwrap();
    assert!(system_of(&m.requests()[0]).ends_with(&context_block(
        "team.guidelines",
        "Prefer small, focused changes."
    )));
    h.host
        .remove_instruction(&created.id, "team.guidelines")
        .unwrap();
    assert!(h.host.list_instructions(&created.id).unwrap().is_empty());
    s.prompt("Second", &mut out).await.unwrap();
    assert_eq!(
        system_updates(&m.requests()[1]).last().unwrap(),
        "The context under \"team.guidelines\" no longer applies. Disregard it."
    );
}

// plugin/session-list.test.ts: "Effect plugins list sessions with the same pages as Core"
#[test]
fn oc_plugins_list_sessions_in_pages() {
    let h = H::new();
    let parent = h.titled("match parent");
    std::thread::sleep(std::time::Duration::from_millis(3));
    h.child(&parent.id, Some("match child"));
    std::thread::sleep(std::time::Duration::from_millis(3));
    let root = h.titled("match root");
    std::thread::sleep(std::time::Duration::from_millis(3));
    h.titled("different");
    let q = ListQuery {
        directory: Some(h.dir.path().display().to_string()),
        parent: Parent::Roots,
        search: Some("MATCH".into()),
        limit: Some(1),
        cursor: None,
    };
    let first = h.host.list(&q).unwrap();
    assert_eq!(
        first.data.iter().map(|s| &s.id).collect::<Vec<_>>(),
        [&root.id]
    );
    let next = h
        .host
        .list(&ListQuery {
            cursor: first.next.clone(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        next.data.iter().map(|s| &s.id).collect::<Vec<_>>(),
        [&parent.id]
    );
    assert_eq!(next.next, None);
    let children = h
        .host
        .list(&ListQuery {
            parent: Parent::Of(parent.id.clone()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(children.data.len(), 1);
    assert_eq!(children.data[0].title.as_deref(), Some("match child"));
}

// plugin/session-list.test.ts: "Promise plugins list sessions and follow cursors"
#[test]
fn oc_plugins_list_sessions_and_follow_cursors() {
    let h = H::new();
    let parent = h.titled("match parent");
    std::thread::sleep(std::time::Duration::from_millis(3));
    h.child(&parent.id, Some("match child"));
    std::thread::sleep(std::time::Duration::from_millis(3));
    h.titled("match root");
    let all = h
        .host
        .list(&ListQuery {
            parent: Parent::Roots,
            search: Some("match".into()),
            ..Default::default()
        })
        .unwrap();
    let roots: Vec<String> = all.data.iter().map(|s| s.id.clone()).collect();
    assert_eq!(roots.len(), 2);
    let first = h
        .host
        .list(&ListQuery {
            parent: Parent::Roots,
            search: Some("match".into()),
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert!(first.next.is_some());
    let second = h
        .host
        .list(&ListQuery {
            cursor: first.next,
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    let paged: Vec<String> = first
        .data
        .iter()
        .chain(&second.data)
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(paged, roots);
    assert!(matches!(
        h.host
            .list(&ListQuery {
                cursor: Some("invalid".into()),
                ..Default::default()
            })
            .unwrap_err(),
        HostError::InvalidCursor
    ));
}

// plugin/session-remove.test.ts: "Effect plugins can remove sessions and their children"
#[test]
fn oc_plugins_can_remove_sessions_and_their_children() {
    let h = H::new();
    let parent = h.titled("Temporary worker");
    let child = h.child(&parent.id, None);
    let unrelated = h.titled("Unrelated session");
    h.host.remove(&parent.id).unwrap();
    not_found(h.host.get(&parent.id).unwrap_err(), &parent.id);
    not_found(h.host.get(&child.id).unwrap_err(), &child.id);
    assert_eq!(h.host.get(&unrelated.id).unwrap(), unrelated);
    not_found(h.host.remove(&parent.id).unwrap_err(), &parent.id);
}

// plugin/session-remove.test.ts: "Promise plugins can remove sessions and their children"
#[test]
fn oc_removing_a_session_leaves_only_the_unrelated_ones_listed() {
    let h = H::new();
    let parent = h.titled("Temporary worker");
    h.child(&parent.id, None);
    let unrelated = h.titled("Unrelated session");
    h.host.remove(&parent.id).unwrap();
    let listed = h.host.list(&ListQuery::default()).unwrap();
    assert_eq!(
        listed.data.iter().map(|s| &s.id).collect::<Vec<_>>(),
        [&unrelated.id]
    );
}
