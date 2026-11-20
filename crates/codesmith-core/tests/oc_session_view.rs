//! Ports of OpenCode's session read-state tests (MIT):
//! `session-view.test.ts`.
//!
//! Adapted: CodeSmith keeps the idle time, the viewed watermark and the
//! outcome with the session's meta rather than as durable events; a turn
//! sets the idle time as it ends ([`Store::set_idle`]), and the local
//! server records a user's abort as `interrupted`. A shutdown records
//! nothing, as in OpenCode.

use codesmith_ai::Usage;
use codesmith_core::store::{Info, NotFound, Store};

fn create(st: &Store, id: &str) -> Info {
    let info = Info {
        id: id.into(),
        directory: "/project".into(),
        title: None,
        agent: "build".into(),
        model: "fake/m".into(),
        parent_id: None,
        created: 1,
        updated: 1,
        usage: Usage::default(),
    };
    st.create(&info).unwrap();
    st.get(id).unwrap().unwrap()
}

#[test]
fn oc_session_view_copies_the_latest_idle_time_without_changing_session_recency() {
    let st = Store::memory().unwrap();
    let created = create(&st, "s");
    let state = st.read_state("s").unwrap();
    assert_eq!(
        (state.idle, state.viewed, state.outcome),
        (None, None, None)
    );

    // Nothing to have seen yet.
    st.view("s", 0).unwrap();
    assert_eq!(st.read_state("s").unwrap().viewed, None);

    st.set_idle("s", "succeeded").unwrap();
    let idle = st.read_state("s").unwrap();
    assert!(idle.idle.is_some() && idle.viewed.is_none() && idle.unread());
    assert_eq!(idle.outcome.as_deref(), Some("succeeded"));
    assert_eq!(st.get("s").unwrap().unwrap().updated, created.updated);

    st.view("s", idle.idle.unwrap()).unwrap();
    let viewed = st.read_state("s").unwrap();
    assert_eq!(viewed.viewed, viewed.idle);
    assert!(!viewed.unread());
    assert_eq!(st.get("s").unwrap().unwrap().updated, created.updated);
    // The same watermark again changes nothing.
    st.view("s", viewed.idle.unwrap()).unwrap();
    assert_eq!(st.read_state("s").unwrap(), viewed);

    st.set_idle("s", "failed").unwrap();
    let unread = st.read_state("s").unwrap();
    assert!(unread.idle.unwrap() > unread.viewed.unwrap());
    assert_eq!(unread.outcome.as_deref(), Some("failed"));
    st.view("s", unread.idle.unwrap()).unwrap();
    assert_eq!(st.read_state("s").unwrap().viewed, unread.idle);

    // A user's interrupt is a newer completion.
    st.set_idle("s", "interrupted").unwrap();
    let interrupted = st.read_state("s").unwrap();
    assert!(interrupted.idle.unwrap() > interrupted.viewed.unwrap());
    assert_eq!(interrupted.outcome.as_deref(), Some("interrupted"));
}

#[test]
fn oc_session_view_keeps_a_newer_completion_unread_when_the_viewed_watermark_is_stale() {
    let st = Store::memory().unwrap();
    create(&st, "s");
    let observed = st.set_idle("s", "succeeded").unwrap();
    // A failure lands between the viewer's look and its report.
    st.set_idle("s", "failed").unwrap();
    st.view("s", observed).unwrap();
    let stale = st.read_state("s").unwrap();
    assert_eq!(stale.viewed, Some(observed));
    assert!(stale.idle.unwrap() > observed);

    // A watermark past the idle time is not taken.
    st.view("s", stale.idle.unwrap() + 1).unwrap();
    assert_eq!(st.read_state("s").unwrap().viewed, Some(observed));

    st.view("s", stale.idle.unwrap()).unwrap();
    let acked = st.read_state("s").unwrap();
    assert_eq!(acked.viewed, acked.idle);
    // A late stale watermark never takes it back.
    st.view("s", observed).unwrap();
    assert_eq!(st.read_state("s").unwrap().viewed, acked.viewed);
}

#[test]
fn oc_session_view_rejects_an_unknown_session() {
    let st = Store::memory().unwrap();
    let e = st.view("ses_missing_view", 0).unwrap_err();
    assert_eq!(
        e.downcast_ref::<NotFound>(),
        Some(&NotFound {
            session: "ses_missing_view".into()
        })
    );
}

#[test]
fn oc_session_view_keeps_viewed_state_when_the_store_is_reopened() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let expected = {
        let st = Store::open(&path).unwrap();
        create(&st, "ses_view_replay");
        let idle = st.set_idle("ses_view_replay", "succeeded").unwrap();
        st.view("ses_view_replay", idle).unwrap();
        st.set_idle("ses_view_replay", "failed").unwrap();
        st.read_state("ses_view_replay").unwrap()
    };
    let st = Store::open(&path).unwrap();
    let replayed = st.read_state("ses_view_replay").unwrap();
    assert_eq!(replayed, expected);
    assert_eq!(replayed.outcome.as_deref(), Some("failed"));
    assert!(replayed.unread());
}
