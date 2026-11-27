//! Ports of the session and session store tests in PARITY.md's porting
//! backlog (`packages/core/test/session-create.test.ts`,
//! `session-remove.test.ts`, `session-runner-retry.test.ts`,
//! `session-runner-tool-events.test.ts`, `session-system-prompt.test.ts`,
//! MIT). OpenCode's sessions are an event store behind a service; here they
//! are `store::Store` rows written by the `Session` that runs them, so
//! "adapted" notes say what stands in for an event or a service call.

mod scripted;

use codesmith_ai::{Message, Usage};
use codesmith_core::config::Loaded;
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session};
use codesmith_core::store::{self, Info, Record, Store};
use scripted::*;
use serde_json::json;
use std::sync::Arc;

fn info(id: &str, directory: &str, parent: Option<&str>, t: i64) -> Info {
    Info {
        id: id.into(),
        directory: directory.into(),
        title: None,
        agent: "build".into(),
        model: "fake/m".into(),
        parent_id: parent.map(String::from),
        created: t,
        updated: t,
        usage: Usage::default(),
    }
}

fn ids(list: &[Info]) -> Vec<&str> {
    list.iter().map(|i| i.id.as_str()).collect()
}

/// A second session on the same scripted model and store as `t`'s, with
/// `cwd` as its directory.
fn another(t: &T, cwd: std::path::PathBuf, parent: Option<String>) -> Session {
    let loaded: Loaded = t.loaded();
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("Another".into()),
        parent,
        id: None,
        questioner: None,
        mcp: None,
    };
    Session::new(
        &loaded,
        cwd,
        opts,
        Arc::new(Fixed(Reply::Once)),
        Some(t.store()),
    )
    .unwrap()
}

// --- Creating sessions ---

/// "persists a missing title until one is generated or supplied"
#[test]
fn oc_session_create_keeps_a_missing_title_unset_until_one_is_supplied() {
    let st = Store::memory().unwrap();
    st.create(&info("ses_untitled", "/project", None, 1))
        .unwrap();
    assert_eq!(st.get("ses_untitled").unwrap().unwrap().title, None);
    st.set_title("ses_untitled", "Explicit title").unwrap();
    assert_eq!(
        st.get("ses_untitled").unwrap().unwrap().title.as_deref(),
        Some("Explicit title")
    );
    let mut titled = info("ses_titled", "/project", None, 2);
    titled.title = Some("Given".into());
    st.create(&titled).unwrap();
    assert_eq!(
        st.get("ses_titled").unwrap().unwrap().title.as_deref(),
        Some("Given")
    );
}

/// "creates a fresh projected session when the ID is omitted"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_create_makes_a_fresh_session_each_time() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.prompt("First").await.unwrap();
    let mut second = another(&t, t.dir.path().canonicalize().unwrap(), None);
    assert_ne!(second.id, t.s.id);
    second
        .prompt("Second", &mut Rec(Default::default()))
        .await
        .unwrap();
    assert_eq!(t.store().list(None, false).unwrap().len(), 2);
}

/// "stores a trailing-slash directory in the same session list". Fixed: a
/// session started in `dir/` was stored under `dir/`, so the list for
/// `dir` did not show it.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_create_stores_a_trailing_slash_directory_in_the_same_list() {
    let t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    let root = t.dir.path().canonicalize().unwrap();
    let slashed = std::path::PathBuf::from(format!("{}/", root.display()));
    let mut s = another(&t, slashed, None);
    s.prompt("Hello", &mut Rec(Default::default()))
        .await
        .unwrap();
    let listed = t
        .store()
        .list(Some(&root.display().to_string()), false)
        .unwrap();
    assert_eq!(ids(&listed), [s.id.as_str()]);
    assert_eq!(listed[0].directory, root.display().to_string());
}

/// "inherits location from an existing parent when omitted". Adapted: a
/// subagent's child session.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_create_gives_a_child_its_parents_directory() {
    let mut t = start(Setup {
        extra: json!({"agents": {"reviewer": {"description": "Reviews", "mode": "subagent", "system": "You review code."}}}),
        store: true,
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "call-subagent",
            "subagent",
            json!({"agent": "reviewer", "description": "review", "prompt": "review this", "model": "", "sessionID": ""}),
        ),
        text("Looks fine."),
        text("Parent done"),
    ]);
    t.prompt("Delegate").await.unwrap();
    let st = t.store();
    let all = st.list(None, true).unwrap();
    let parent = all.iter().find(|i| i.id == t.s.id).unwrap();
    let child = all
        .iter()
        .find(|i| i.parent_id.as_deref() == Some(t.s.id.as_str()))
        .expect("a child session");
    assert_eq!(child.directory, parent.directory);
}

// --- Listing sessions ---

/// "filters root sessions before applying the page limit". Adapted: the
/// list has no page limit; the newest session it returns is still the
/// newest root, however many newer children there are.
#[test]
fn oc_session_list_filters_root_sessions_before_the_newest() {
    let st = Store::memory().unwrap();
    st.create(&info("ses_stale", "/project", None, 5_000))
        .unwrap();
    st.create(&info("ses_root", "/project", None, 10_000))
        .unwrap();
    for i in 0..60 {
        st.create(&info(
            &format!("ses_child_{i}"),
            "/project",
            Some("ses_root"),
            20_000 + i,
        ))
        .unwrap();
    }
    let roots = st.list(Some("/project"), false).unwrap();
    assert_eq!(ids(&roots), ["ses_root", "ses_stale"]);
    assert_eq!(st.list(Some("/project"), true).unwrap().len(), 62);
}

/// "orders sessions by their latest prompt"
#[test]
fn oc_session_list_orders_sessions_by_their_latest_prompt() {
    let st = Store::memory().unwrap();
    st.create(&info("ses_active", "/project", None, 1)).unwrap();
    st.create(&info("ses_newer", "/project", None, 2)).unwrap();
    assert_eq!(
        ids(&st.list(None, false).unwrap()),
        ["ses_newer", "ses_active"]
    );
    st.append_at(
        "ses_active",
        &Record::Message {
            message: Message::user("continue"),
        },
        3,
    )
    .unwrap();
    assert_eq!(
        ids(&st.list(None, false).unwrap()),
        ["ses_active", "ses_newer"]
    );
}

// --- Switching agent and model ---

/// "switches the selected agent through the durable Session event" and
/// "switches the selected model through the durable Session event".
/// Adapted: the stored selection, which a reopened session keeps.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_switches_keep_the_selected_agent_and_model() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.prompt("First").await.unwrap();
    let st = t.store();
    st.set_selection(&t.s.id, "plan", "fake/m").unwrap();
    let got = st.get(&t.s.id).unwrap().unwrap();
    assert_eq!((got.agent.as_str(), got.model.as_str()), ("plan", "fake/m"));
    st.set_selection(&t.s.id, "plan", "fake/other").unwrap();
    assert_eq!(st.get(&t.s.id).unwrap().unwrap().model, "fake/other");
}

// --- Import ---

/// "rejects an existing session ID without changing its transcript"
#[test]
fn oc_session_import_rejects_an_existing_id_without_changing_it() {
    let mut st = Store::memory().unwrap();
    st.create(&info("ses_existing", "/project", None, 1))
        .unwrap();
    st.append(
        "ses_existing",
        &Record::Message {
            message: Message::user("Original"),
        },
    )
    .unwrap();
    let mut export = st.export("ses_existing").unwrap();
    export.entries = vec![store::Entry {
        seq: 1,
        created: 2,
        record: Record::Message {
            message: Message::user("Replacement"),
        },
    }];
    let err = st.import(&export).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(
        st.history("ses_existing").unwrap(),
        [Message::user("Original")]
    );
}

// --- Removing sessions ---

/// "removes a session and its children"
#[test]
fn oc_session_remove_removes_a_session_and_its_children() {
    let st = Store::memory().unwrap();
    st.create(&info("ses_parent", "/project", None, 1)).unwrap();
    st.create(&info("ses_child", "/project", Some("ses_parent"), 2))
        .unwrap();
    st.create(&info("ses_grandchild", "/project", Some("ses_child"), 3))
        .unwrap();
    st.set_meta("ses_child", "environment", Some("child"))
        .unwrap();
    assert!(st.delete("ses_parent").unwrap());
    assert!(st.list(None, true).unwrap().is_empty());
    for id in ["ses_parent", "ses_child", "ses_grandchild"] {
        assert!(st.get(id).unwrap().is_none(), "{id}");
    }
    assert_eq!(st.meta("ses_child", "environment").unwrap(), None);
}

/// "fails when the session does not exist". Adapted: `delete` says it
/// removed nothing.
#[test]
fn oc_session_remove_reports_a_missing_session() {
    let st = Store::memory().unwrap();
    assert!(!st.delete("ses_missing").unwrap());
}

// --- Turn failures ---

/// "content-filter finish preserves partial streamed text and never ends
/// the step successfully" (`session-runner-tool-events.test.ts`)
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_content_filter_keeps_partial_text_and_fails_the_step() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.push([finish("content_filter", "Partial")]);
    let err = t.prompt("Blocked").await.unwrap_err();
    assert_eq!(err.to_string(), "Provider blocked the response");
    assert_eq!(texts(&t.s), ["Partial"]);
    assert_eq!(texts(&t.reopen()), ["Partial"]);
}

/// "names the network when the provider's host can't be reached"
/// (`session-runner-retry.test.ts`), as the turn reports it. The unit
/// rules are pinned in `retry.rs`; here an unreachable host is named once
/// its retries are spent (a refused connection is not unreachable).
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_reports_a_refused_connection_in_its_own_words() {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    // Nothing listens on port 9 of the loopback address.
    let loaded = Loaded {
        config: config("http://127.0.0.1:9/v1", json!({})),
        files: vec![],
        root: root.clone(),
        ..Default::default()
    };
    let opts = Options {
        agent: None,
        model: None,
        resume: None,
        title: Some("t".into()),
        parent: None,
        id: None,
        questioner: None,
        mcp: None,
    };
    let mut s = Session::new(&loaded, root, opts, Arc::new(Fixed(Reply::Once)), None).unwrap();
    s.set_retry_scale(0.0);
    let err = s
        .prompt("Hello", &mut Rec(Default::default()))
        .await
        .unwrap_err()
        .to_string();
    assert!(!err.starts_with("Can't reach"), "{err}");
    assert!(err.starts_with("request failed: "), "{err}");
}

// --- The system prompt ---

/// "renders the default system prompt instructions"
/// (`session-system-prompt.test.ts`)
#[test]
fn oc_session_system_prompt_renders_the_default_instructions() {
    let prompt = codesmith_core::prompt::base("m", &["edit", "read", "shell"]);
    assert!(!prompt.contains("${TOOL_GUIDANCE}"));
    assert!(prompt.contains("Use the edit tool for targeted changes to existing text files"));
}
