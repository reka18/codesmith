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
        access: None,
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
fn projects_are_keyed_by_their_folder_alone() {
    let d = tempfile::tempdir().unwrap();
    let real = d.path().canonicalize().unwrap();
    let key = codesmith_core::approvals::project_key(d.path());
    assert_eq!(key, format!("dir:{}", real.display()));
    // Repository metadata doesn't change it.
    std::fs::create_dir(d.path().join(".hg")).unwrap();
    assert_eq!(codesmith_core::approvals::project_key(d.path()), key);
}

// --- Keyed by folder, never by what the repository says ---------------------

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = codesmith_core::git::fixture()
        .current_dir(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository with one commit, in `dir`.
fn repo(dir: &std::path::Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "first"]);
    git(dir, &["rev-list", "--max-parents=0", "HEAD"])
}

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

/// A gate for the project at `root`, keyed as sessions key it.
fn gate_at(book: &Arc<Book>, root: &std::path::Path, reply: Reply) -> (Gate, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let mut g = Gate::new(
        vec![Rule::new("read", "*", Effect::Ask)],
        Arc::new(Counting(asked.clone(), reply)),
    );
    g.set_durable(book.clone(), codesmith_core::approvals::project_key(root));
    (g, asked)
}

/// Save "always" for `src/*` in `root`.
fn approve(b: &Arc<Book>, root: &std::path::Path) {
    let (g, _) = gate_at(b, root, Reply::Always);
    g.assert(&read(&["src/a.ts"], &["src/*"])).unwrap();
}

/// Whether a later read in `root` is allowed without asking.
fn approved(b: &Arc<Book>, root: &std::path::Path) -> bool {
    let (g, asked) = gate_at(b, root, Reply::Reject(None));
    let ok = g.assert(&read(&["src/b.ts"], &[])).is_ok();
    ok && asked.load(Ordering::SeqCst) == 0
}

#[test]
fn a_fork_with_the_same_first_commit_in_another_folder_inherits_nothing() {
    if !have_git() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let b = book(&d);
    let original = base.join("original");
    let first = repo(&original);
    approve(&b, &original);
    assert!(approved(&b, &original));
    // A clone, as a fork would be: the same first commit, another folder.
    git(&base, &["clone", "-q", original.to_str().unwrap(), "fork"]);
    let fork = base.join("fork");
    assert_eq!(git(&fork, &["rev-list", "--max-parents=0", "HEAD"]), first);
    assert!(!approved(&b, &fork), "the fork inherited the approval");
    // A crafted copy: the original's whole `.git` (history, config,
    // remotes) in another folder.
    let crafted = base.join("crafted");
    let status = std::process::Command::new("cp")
        .args(["-R", original.to_str().unwrap(), crafted.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        git(&crafted, &["rev-list", "--max-parents=0", "HEAD"]),
        first
    );
    assert!(
        !approved(&b, &crafted),
        "the crafted copy inherited the approval"
    );
    assert!(approved(&b, &original));
}

#[test]
fn the_same_folder_keeps_its_approvals_across_sessions_and_through_a_symlink() {
    if !have_git() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let root = base.join("app");
    repo(&root);
    {
        let b = book(&d);
        approve(&b, &root);
    }
    // A new process: the database opened afresh.
    let b = book(&d);
    assert!(approved(&b, &root));
    #[cfg(unix)]
    {
        let link = base.join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(approved(&b, &link), "a symlink to the folder shares them");
        assert_eq!(
            codesmith_core::approvals::project_key(&link),
            codesmith_core::approvals::project_key(&root)
        );
    }
}

#[test]
fn a_worktree_starts_with_no_approvals() {
    if !have_git() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let b = book(&d);
    let root = base.join("app");
    repo(&root);
    approve(&b, &root);
    let wt = base.join("wt");
    git(
        &root,
        &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
    );
    assert!(!approved(&b, &wt), "the worktree inherited the approval");
    assert!(approved(&b, &root));
}

#[test]
fn approvals_saved_under_an_old_first_commit_key_are_not_applied() {
    if !have_git() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let b = book(&d);
    let root = base.join("app");
    let first = repo(&root);
    // How 0.6.1 keyed a Git project: its first commit, cut to 12.
    let old: String = first.chars().take(12).collect();
    b.add(&old, "read", &["*".into()]).unwrap();
    assert_ne!(codesmith_core::approvals::project_key(&root), old);
    assert!(!approved(&b, &root));
    // The old row is left in place, unread.
    assert_eq!(b.list(Some(&old)).unwrap().len(), 1);
}

/// Bounded property: keys are the resolved folder, so distinct folders
/// never share one and a folder always gets the same one.
#[test]
fn property_keys_follow_the_folder() {
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let names = ["a", "b", "a b", "a.b", "ä", "x-1", "deep/er", "deep/er2"];
    let mut keys = std::collections::BTreeSet::new();
    for n in names {
        let p = base.join(n);
        std::fs::create_dir_all(&p).unwrap();
        let k = codesmith_core::approvals::project_key(&p);
        assert_eq!(
            k,
            codesmith_core::approvals::project_key(&p.join(".").join(""))
        );
        assert!(keys.insert(k), "{n} shared a key");
    }
}
