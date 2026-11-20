//! Ports of OpenCode's session archiving tests (MIT):
//! `session-archive.test.ts`. Each test names the OpenCode test it ports;
//! see `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: CodeSmith keeps the archive time with the session's meta, not
//! as a durable event; archiving twice keeps the first time.

use codesmith_ai::{Message, Usage};
use codesmith_core::store::{Info, NotFound, Record, Store};

fn create(st: &Store, id: &str, updated: i64) {
    st.create(&Info {
        id: id.into(),
        directory: "/project".into(),
        title: None,
        agent: "build".into(),
        model: "fake/m".into(),
        parent_id: None,
        created: updated,
        updated,
        usage: Usage::default(),
    })
    .unwrap();
}

// session-archive.test.ts: "archives and unarchives through one durable event each"
#[test]
fn oc_session_archives_and_unarchives() {
    let st = Store::memory().unwrap();
    create(&st, "s", 1);
    assert_eq!(st.archived("s").unwrap(), None);
    st.archive("s").unwrap();
    let first = st.archived("s").unwrap().expect("archived");
    std::thread::sleep(std::time::Duration::from_millis(5));
    st.archive("s").unwrap();
    assert_eq!(
        st.archived("s").unwrap(),
        Some(first),
        "archiving again changes nothing"
    );
    st.unarchive("s").unwrap();
    st.unarchive("s").unwrap();
    assert_eq!(st.archived("s").unwrap(), None);
    // The history is untouched throughout.
    st.append(
        "s",
        &Record::Message {
            message: Message::user("kept"),
        },
    )
    .unwrap();
    st.archive("s").unwrap();
    assert_eq!(st.history("s").unwrap().len(), 1);
}

// session-archive.test.ts: "filters listed sessions by archive state"
#[test]
fn oc_session_lists_filter_by_archive_state() {
    let st = Store::memory().unwrap();
    create(&st, "active", 2);
    create(&st, "archived", 1);
    st.archive("archived").unwrap();
    let ids = |archived: Option<bool>| -> Vec<String> {
        let mut v: Vec<String> = st
            .list_archived(None, false, archived)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        v.sort();
        v
    };
    assert_eq!(ids(None), ["active", "archived"]);
    assert_eq!(ids(Some(false)), ["active"]);
    assert_eq!(ids(Some(true)), ["archived"]);
    // Continuing the latest session skips archived ones.
    st.archive("active").unwrap();
    assert_eq!(st.latest("/project").unwrap(), None);
}

// session-archive.test.ts: "rejects an unknown session"
#[test]
fn oc_session_archiving_rejects_an_unknown_session() {
    let st = Store::memory().unwrap();
    let missing = NotFound {
        session: "ses_missing_archive".into(),
    };
    let e = st.archive("ses_missing_archive").unwrap_err();
    assert_eq!(e.downcast_ref::<NotFound>(), Some(&missing));
    let e = st.unarchive("ses_missing_archive").unwrap_err();
    assert_eq!(e.downcast_ref::<NotFound>(), Some(&missing));
}

/// Not in OpenCode's tests: a fork of an archived session is not archived.
#[test]
fn a_fork_of_an_archived_session_is_not_archived() {
    let mut st = Store::memory().unwrap();
    create(&st, "s", 1);
    st.append(
        "s",
        &Record::Message {
            message: Message::user("hi"),
        },
    )
    .unwrap();
    st.archive("s").unwrap();
    st.fork("s", "f").unwrap();
    assert_eq!(st.archived("f").unwrap(), None);
}
