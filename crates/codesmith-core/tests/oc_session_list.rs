//! Ports of OpenCode's session listing tests (MIT): the subpath filter
//! (`session-create.test.ts`) and title search (`session-store.test.ts`).
//!
//! Adapted: a CodeSmith session keeps the folder it started in, so a
//! project's subpath is that folder under the project root.

use codesmith_ai::Usage;
use codesmith_core::store::{self, Info, Store};

fn seed(st: &Store, id: &str, directory: &str, title: &str, updated: i64) {
    st.create(&Info {
        id: id.into(),
        directory: directory.into(),
        title: Some(title.into()),
        agent: "build".into(),
        model: "fake/m".into(),
        parent_id: None,
        created: updated,
        updated,
        usage: Usage::default(),
    })
    .unwrap();
}

// session-create.test.ts: "filters project sessions by subpath"
#[test]
fn oc_session_filters_project_sessions_by_subpath() {
    let st = Store::memory().unwrap();
    let root = std::path::Path::new("/project");
    seed(&st, "root", "/project", "root", 1);
    seed(&st, "nested", "/project/packages/tui", "nested", 2);
    let ids = |l: Vec<Info>| l.into_iter().map(|i| i.id).collect::<Vec<_>>();
    assert_eq!(
        ids(st.list_subpath(root, "packages/tui", false).unwrap()),
        ["nested"]
    );
    // Only folders inside the project.
    assert!(st.list_subpath(root, "../elsewhere", false).is_err());
    assert!(st.list_subpath(root, "/etc", false).is_err());
}

// session-store.test.ts: "searches titles case-insensitively beyond ASCII and keeps LIKE wildcard semantics"
#[test]
fn oc_session_searches_titles_case_insensitively_beyond_ascii_and_keeps_like_wildcard_semantics() {
    let st = Store::memory().unwrap();
    seed(&st, "ses_cyrillic", "/p", "Тестова сесия", 10);
    seed(&st, "ses_ascii", "/p", "OpenCode notes", 20);
    seed(&st, "ses_percent", "/p", "100% done", 30);
    seed(&st, "ses_underscore", "/p", "file_name [draft]", 40);
    let search = |text: &str| -> Vec<String> {
        st.search(None, false, text)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect()
    };
    assert_eq!(search("тестова"), ["ses_cyrillic"]);
    assert_eq!(search("ТЕСТОВА СЕСИЯ"), ["ses_cyrillic"]);
    assert_eq!(search("opencode"), ["ses_ascii"]);
    assert_eq!(search("OPENCODE"), ["ses_ascii"]);
    assert_eq!(search("тест%сия"), ["ses_cyrillic"]);
    assert_eq!(search("OP_NCODE"), ["ses_ascii"]);
    assert_eq!(search("100%"), ["ses_percent"]);
    assert_eq!(search("file_name"), ["ses_underscore"]);
    assert_eq!(search("[DRAFT]"), ["ses_underscore"]);
    assert!(search("*").is_empty());
    assert!(search("?").is_empty());
    assert!(store::title_matches("anything", ""));
}
