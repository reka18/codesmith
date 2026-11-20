//! Ports of OpenCode's saved-approval tests (`permission.test.ts`): "always"
//! answers kept per project, listed, edited and removed.

use codesmith_core::approvals::{Book, EditError};
use codesmith_core::permission::{Asker, Denied, Effect, Gate, Reply, Request, Rule};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Counting(Arc<AtomicUsize>, Reply);
impl Asker for Counting {
    fn ask(&self, _: &Request) -> Reply {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.clone()
    }
}

fn read(resources: &[&str], save: &[&str]) -> Request {
    Request {
        action: "read".into(),
        resources: resources.iter().map(|r| r.to_string()).collect(),
        save: save.iter().map(|r| r.to_string()).collect(),
        title: String::new(),
    }
}

/// A gate asking for every read, keeping "always" answers in `book` for
/// project `p`.
fn gate(book: &Arc<Book>, rules: Vec<Rule>, reply: Reply) -> (Gate, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let mut all = vec![Rule::new("read", "*", Effect::Ask)];
    all.extend(rules);
    let mut g = Gate::new(all, Arc::new(Counting(asked.clone(), reply)));
    g.set_durable(book.clone(), "p".into());
    (g, asked)
}

fn book(d: &tempfile::TempDir) -> Arc<Book> {
    Arc::new(Book::open(&d.path().join("sessions.db")).unwrap())
}

#[test]
fn oc_permission_stores_and_removes_saved_resources_for_a_project() {
    let d = tempfile::tempdir().unwrap();
    let b = book(&d);
    let (g, asked) = gate(&b, vec![], Reply::Always);
    g.assert(&read(&["src/a.ts"], &["src/*"])).unwrap();
    let saved = b.list(None).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        (
            saved[0].project.as_str(),
            saved[0].action.as_str(),
            saved[0].resource.as_str()
        ),
        ("p", "read", "src/*")
    );
    assert!(saved[0].id.starts_with("psv_"));
    // A later session of the project is not asked again.
    let (later, later_asked) = gate(&b, vec![], Reply::Once);
    later.assert(&read(&["src/next.ts"], &[])).unwrap();
    assert_eq!(later_asked.load(Ordering::SeqCst), 0);
    assert!(b.remove(&saved[0].id).unwrap());
    assert!(b.list(None).unwrap().is_empty());
    // A removal applies to a running session at its next request.
    g.assert(&read(&["src/next.ts"], &[])).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    // Another project's approvals do not apply.
    b.add("other", "read", &["*".into()]).unwrap();
    later.assert(&read(&["src/x.ts"], &[])).unwrap();
    assert_eq!(later_asked.load(Ordering::SeqCst), 1);
}

#[test]
fn oc_permission_edits_saved_approvals_and_enforces_the_edited_rule() {
    let d = tempfile::tempdir().unwrap();
    let b = book(&d);
    b.add("p", "read", &["src/*".into(), "docs/*".into()])
        .unwrap();
    let src = b
        .list(Some("p"))
        .unwrap()
        .into_iter()
        .find(|s| s.resource == "src/*")
        .unwrap();
    let edited = b
        .update(&src.id, None, Some("src/allowed.ts"))
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            edited.id.as_str(),
            edited.project.as_str(),
            edited.action.as_str(),
            edited.resource.as_str()
        ),
        (src.id.as_str(), "p", "read", "src/allowed.ts")
    );
    let (g, asked) = gate(&b, vec![], Reply::Once);
    g.assert(&read(&["src/allowed.ts"], &[])).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 0, "the edited rule allows");
    g.assert(&read(&["src/other.ts"], &[])).unwrap();
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "the old pattern no longer does"
    );
    assert_eq!(
        b.update(&src.id, None, Some("docs/*")).unwrap(),
        Err(EditError::Conflict {
            id: src.id.clone(),
            resource: "docs/*".into()
        })
    );
    assert_eq!(
        b.update("psv_missing", Some("edit"), None).unwrap(),
        Err(EditError::NotFound {
            id: "psv_missing".into()
        })
    );
}

#[test]
fn saved_approvals_never_override_configured_or_built_in_denies() {
    let d = tempfile::tempdir().unwrap();
    let b = book(&d);
    b.add("p", "read", &["*".into()]).unwrap();
    let (g, _) = gate(
        &b,
        vec![Rule::new("read", "secret/*", Effect::Deny)],
        Reply::Once,
    );
    assert!(matches!(
        g.assert(&read(&["secret/key"], &[])),
        Err(Denied::Blocked { .. })
    ));
    assert!(!g.allows("read", &["secret/key".into()]));
    g.assert(&read(&["src/a"], &[])).unwrap();
    // A built-in agent rule (plan's edit deny) still restricts.
    b.add("p", "edit", &["*".into()]).unwrap();
    let (g, _) = gate(
        &b,
        vec![Rule::new("edit", "*", Effect::Deny).into_builtin()],
        Reply::Once,
    );
    let edit = Request {
        action: "edit".into(),
        ..read(&["a.rs"], &[])
    };
    assert!(g.assert(&edit).is_err());
    assert!(!g.allows("edit", &["a.rs".into()]));
}

#[test]
fn projects_are_keyed_by_their_repository_else_their_directory() {
    let d = tempfile::tempdir().unwrap();
    let key = codesmith_core::approvals::project_key(d.path());
    assert_eq!(key, format!("dir:{}", d.path().display()));
    std::fs::create_dir(d.path().join(".hg")).unwrap();
    // Unreadable Mercurial metadata has no id: the directory keys it.
    assert!(codesmith_core::approvals::project_key(d.path()).starts_with("dir:"));
}
