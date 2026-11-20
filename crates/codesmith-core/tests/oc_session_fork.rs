//! Ports of OpenCode's session forking tests (MIT): `session-create.test.ts`'s
//! fork cases. Each test names the OpenCode test it ports; see
//! `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: OpenCode identifies messages by ID and forks by replaying a
//! durable event; CodeSmith's messages are identified by their record
//! sequence numbers and a fork copies the records (see
//! `codesmith_core::fork`).

mod support;

use codesmith_ai::{Message, Part, Role, Usage};
use codesmith_core::fork::{self, Boundary, Origin};
use codesmith_core::store::{Info, Record, Store};
use serde_json::json;
use support::{convo, text, H};

fn create(st: &Store, id: &str, title: Option<&str>) {
    st.create(&Info {
        id: id.into(),
        directory: "/p".into(),
        title: title.map(String::from),
        agent: "build".into(),
        model: "fake/m".into(),
        parent_id: None,
        created: 0,
        updated: 0,
        usage: Usage {
            input: 6,
            output: 3,
            ..Usage::default()
        },
    })
    .unwrap();
}

fn message(st: &Store, id: &str, m: Message) -> i64 {
    st.append(id, &Record::Message { message: m }).unwrap()
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::text(text)],
    }
}

fn texts(st: &Store, id: &str) -> Vec<String> {
    st.history(id).unwrap().iter().map(text).collect()
}

// session-create.test.ts: "forks a session by replaying a durable fork event into copied projected rows"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_forks_a_session_into_copied_history() {
    let mut h = H::new().await;
    h.prompt("First").await;
    let parent = h.s.id.clone();
    let mut st = h.store();
    let forked = st.fork(&parent, "fork").unwrap();

    assert_eq!(forked.title.as_deref(), Some("Parent (fork #1)"));
    assert_eq!(forked.parent_id, None);
    let last = fork::messages(&st.entries(&parent).unwrap())
        .last()
        .unwrap()
        .0;
    assert_eq!(
        st.fork_origin("fork").unwrap(),
        Some(Origin {
            session: parent.clone(),
            boundary: Boundary::Through(last),
        })
    );
    assert_eq!(st.history("fork").unwrap(), st.history(&parent).unwrap());
    assert_eq!(
        convo(&st.history("fork").unwrap()),
        [("user", "First".into()), ("assistant", "ok".into())]
    );
    // A fork is a session of its own, top-level.
    assert!(st.list(None, false).unwrap().iter().any(|i| i.id == "fork"));

    // The two go their own ways.
    h.prompt("Parent changed").await;
    let mut child = h.open("fork");
    child
        .prompt("Child continues", &mut support::Quiet)
        .await
        .unwrap();
    assert_eq!(
        convo(&st.history(&parent).unwrap()),
        [
            ("user", "First".into()),
            ("assistant", "ok".into()),
            ("user", "Parent changed".into()),
            ("assistant", "ok".into()),
        ]
    );
    let child: Vec<_> = convo(&st.history("fork").unwrap())
        .into_iter()
        .filter(|(_, t)| t != fork::NOTICE)
        .collect();
    assert_eq!(
        child,
        [
            ("user", "First".into()),
            ("assistant", "ok".into()),
            ("user", "Child continues".into()),
            ("assistant", "ok".into()),
        ]
    );
    // Forking a fork counts up.
    let again = st.fork("fork", "fork2").unwrap();
    assert_eq!(again.title.as_deref(), Some("Parent (fork #2)"));
}

// session-create.test.ts: "queues a fork notice after copied history and before the first prompt"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_queues_a_fork_notice_before_the_first_prompt() {
    let mut h = H::new().await;
    h.prompt("Original").await;
    let parent = h.s.id.clone();
    let mut st = h.store();
    st.fork(&parent, "fork").unwrap();
    // Owed, not yet in the history.
    assert_eq!(texts(&st, "fork"), ["Original", "ok"]);

    let mut child = h.open("fork");
    child.prompt("Continue", &mut support::Quiet).await.unwrap();
    assert_eq!(
        texts(&st, "fork"),
        ["Original", "ok", fork::NOTICE, "Continue", "ok"]
    );
    // The model saw it, right before the prompt.
    let last = h.requests().last().unwrap().clone();
    let users: Vec<String> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        users[users.len() - 2].contains("<fork-notice>"),
        "{users:?}"
    );
    assert_eq!(users.last().unwrap(), "Continue");
    // Once only, and never in the parent.
    child.prompt("Again", &mut support::Quiet).await.unwrap();
    let notices = texts(&st, "fork")
        .iter()
        .filter(|t| t.contains("<fork-notice>"))
        .count();
    assert_eq!(notices, 1);
    assert_eq!(texts(&st, &parent), ["Original", "ok"]);
    // Views of the conversation leave it out, as engine text.
    assert!(codesmith_core::runner::is_engine(&Message::user(
        fork::NOTICE
    )));
}

// session-create.test.ts: "keeps a fork untitled when its parent is untitled"
#[test]
fn oc_session_keeps_a_fork_untitled_when_its_parent_is_untitled() {
    let mut st = Store::memory().unwrap();
    create(&st, "p", None);
    message(&st, "p", Message::user("First"));
    let forked = st.fork("p", "f").unwrap();
    assert_eq!(forked.title, None);
    assert_eq!(st.get("f").unwrap().unwrap().title, None);
}

// session-create.test.ts: "inherits instruction entries when forking"
#[test]
fn oc_session_inherits_instruction_entries_when_forking() {
    let mut st = Store::memory().unwrap();
    create(&st, "p", None);
    message(&st, "p", Message::user("Fork context"));
    st.put_context("p", "deploy-target", &json!("production"))
        .unwrap();
    st.put_context("p", "retired", &json!(true)).unwrap();
    st.remove_context("p", "retired").unwrap();
    for i in 0..20 {
        st.put_context("p", &format!("entry-{i:02}"), &json!(i))
            .unwrap();
    }
    st.fork("p", "f").unwrap();
    let list = st.context("f", false).unwrap();
    assert_eq!(list.len(), 21);
    assert!(list
        .iter()
        .any(|c| c.key == "deploy-target" && c.value == json!("production")));
    let all = st.context("f", true).unwrap();
    assert!(all.iter().any(|c| c.key == "retired" && c.removed));
    // The parent's later changes are its own.
    st.put_context("p", "deploy-target", &json!("staging"))
        .unwrap();
    st.put_context("p", "new-parent-entry", &json!(true))
        .unwrap();
    assert_eq!(st.context("f", false).unwrap(), list);
}

// session-create.test.ts: "does not copy a running assistant into a fork"
#[test]
fn oc_session_does_not_copy_a_running_assistant_into_a_fork() {
    let mut st = Store::memory().unwrap();
    create(&st, "p", None);
    message(&st, "p", Message::user("Run both tools"));
    message(
        &st,
        "p",
        Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall {
                id: "call_running".into(),
                name: "shell".into(),
                input: json!({"command": "sleep 10"}),
                meta: None,
            }],
        },
    );
    st.fork("p", "f").unwrap();
    assert_eq!(st.history("p").unwrap().len(), 2);
    assert_eq!(texts(&st, "f"), ["Run both tools"]);
}

// session-create.test.ts: "rejects forking an empty session"
#[test]
fn oc_session_rejects_forking_an_empty_session() {
    let mut st = Store::memory().unwrap();
    create(&st, "p", None);
    let e = st.fork("p", "f").unwrap_err();
    assert_eq!(
        e.downcast_ref::<fork::Empty>(),
        Some(&fork::Empty {
            session: "p".into()
        })
    );
    assert!(st.get("f").unwrap().is_none());
}

// session-create.test.ts: "forks before the selected boundary message"
#[test]
fn oc_session_forks_before_the_selected_boundary_message() {
    let mut st = Store::memory().unwrap();
    create(&st, "p", Some("Parent"));
    let first = message(&st, "p", Message::user("First"));
    let second = message(&st, "p", Message::user("Second"));
    message(&st, "p", assistant("Answer"));
    st.append(
        "p",
        &Record::Step {
            model: "fake/m".into(),
            usage: Usage {
                input: 6,
                output: 3,
                ..Usage::default()
            },
        },
    )
    .unwrap();

    let forked = st.fork_at("p", "before-second", Some(second)).unwrap();
    let before_first = st.fork_at("p", "before-first", Some(first)).unwrap();
    let complete = st.fork("p", "complete").unwrap();

    assert_eq!(
        st.fork_origin("before-second").unwrap(),
        Some(Origin {
            session: "p".into(),
            boundary: Boundary::Before(second),
        })
    );
    assert_eq!(texts(&st, "before-second"), ["First"]);
    assert_eq!(forked.usage, Usage::default());
    assert_eq!(texts(&st, "before-first"), Vec::<String>::new());
    assert_eq!(before_first.usage, Usage::default());
    assert_eq!(texts(&st, "complete"), ["First", "Second", "Answer"]);
    assert_eq!(complete.usage, Usage::default());
    assert_eq!(st.get("complete").unwrap().unwrap().usage, Usage::default());
    // The parent's steps stay its own, so stats count them once.
    assert!(!st
        .entries("complete")
        .unwrap()
        .iter()
        .any(|e| matches!(e.record, Record::Step { .. })));
    // A boundary that is not a message.
    let e = st.fork_at("p", "nowhere", Some(99)).unwrap_err();
    assert_eq!(
        e.downcast_ref::<fork::MessageNotFound>(),
        Some(&fork::MessageNotFound {
            session: "p".into(),
            seq: 99
        })
    );
}

/// Not in OpenCode: what an undo hid stays out of a fork.
#[test]
fn fork_leaves_out_undone_messages() {
    let mut st = Store::memory().unwrap();
    create(&st, "p", None);
    message(&st, "p", Message::user("Kept"));
    let from = message(&st, "p", Message::user("Undone"));
    st.append(
        "p",
        &Record::Revert {
            from_seq: from,
            after: "t".into(),
        },
    )
    .unwrap();
    st.fork("p", "f").unwrap();
    assert_eq!(texts(&st, "f"), ["Kept"]);
    assert!(!st
        .entries("f")
        .unwrap()
        .iter()
        .any(|e| matches!(e.record, Record::Revert { .. })));
}
