//! Ports of the permission cases in PARITY.md's porting backlog
//! (`packages/core/test/file-access-path.test.ts`, `file-access.test.ts`,
//! `permission.test.ts`, MIT). OpenCode resolves a path to a target with
//! an optional external directory; CodeSmith's `Ctx::resolve` gives the
//! target and `external`, and the tools ask `external_directory` (naming
//! the file, saving its folder or repository) before their own action.

mod scripted;

use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{Asker, Gate, Reply, Request};
use codesmith_core::tool::{self, Ctx};
use scripted::*;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recorder {
    deny: Vec<&'static str>,
    asked: Mutex<Vec<Request>>,
}

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.asked.lock().unwrap().push(req.clone());
        if self.deny.contains(&req.action.as_str()) {
            Reply::Reject(None)
        } else {
            Reply::Once
        }
    }
}

impl Recorder {
    fn actions(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.action.clone())
            .collect()
    }
}

struct Env {
    cwd: PathBuf,
    root: PathBuf,
    gate: Gate,
    rec: Arc<Recorder>,
}

impl Env {
    fn new(cwd: &Path, root: &Path, deny: Vec<&'static str>) -> Env {
        let rec = Arc::new(Recorder {
            deny,
            ..Default::default()
        });
        Env {
            cwd: cwd.to_path_buf(),
            root: root.to_path_buf(),
            gate: Gate::new(vec![], rec.clone()),
            rec,
        }
    }
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            gate: &self.gate,
            shell: "/bin/sh".into(),
            session_id: "test".into(),
            limits: ToolOutputConfig::default(),
            client: reqwest::Client::new(),
            jobs: None,
            websearch: None,
            format: None,
            lsp: None,
            progress: None,
        }
    }
    async fn call(&self, name: &str, input: serde_json::Value) -> (String, bool) {
        tool::execute(&self.ctx(), name, &input).await
    }
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().canonicalize().unwrap();
    (d, p)
}

// --- file-access-path.test.ts -------------------------------------------

/// "resolves an active relative existing file target", "... prospective
/// file target", "accepts an explicit absolute in-location target without
/// external approval" and "follows an in-location symlink using ordinary
/// filesystem semantics": inside the project, named relative to it.
#[test]
fn oc_file_access_resolves_targets_inside_the_project() {
    let (_d, dir) = tmp();
    std::fs::write(dir.join("hello.txt"), "hello").unwrap();
    std::fs::create_dir(dir.join("src")).unwrap();
    std::fs::create_dir(dir.join("actual")).unwrap();
    std::os::unix::fs::symlink(dir.join("actual"), dir.join("linked")).unwrap();
    let env = Env::new(&dir, &dir, vec![]);
    let ctx = env.ctx();
    for (path, absolute, resource) in [
        ("hello.txt", dir.join("hello.txt"), "hello.txt"),
        ("src/new.txt", dir.join("src/new.txt"), "src/new.txt"),
        (
            &*dir.join("new.txt").display().to_string(),
            dir.join("new.txt"),
            "new.txt",
        ),
        (
            "linked/new.txt",
            dir.join("linked/new.txt"),
            "linked/new.txt",
        ),
    ] {
        let t = ctx.resolve(path);
        assert_eq!(
            (t.absolute, t.resource.as_str()),
            (absolute, resource),
            "{path}"
        );
        assert!(!t.external, "{path}");
    }
}

/// "requires external-directory authorization for a relative lexical
/// escape", "... for an explicit external absolute target", "resolves an
/// existing external file target" and "authorizes prospective external
/// descendants at their lexical parent". Adapted: the request names the
/// file and saves its folder.
#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_requires_external_approval_outside_the_project() {
    let (_d, base) = tmp();
    let dir = base.join("project");
    std::fs::create_dir(&dir).unwrap();
    let (_o, outside) = tmp();
    std::fs::write(outside.join("existing.txt"), "existing").unwrap();
    for (path, file) in [
        ("../outside.txt".to_string(), base.join("outside.txt")),
        (
            outside.join("new.txt").display().to_string(),
            outside.join("new.txt"),
        ),
        (
            outside.join("existing.txt").display().to_string(),
            outside.join("existing.txt"),
        ),
        (
            outside.join("new/nested/file.txt").display().to_string(),
            outside.join("new/nested/file.txt"),
        ),
    ] {
        let env = Env::new(&dir, &dir, vec![]);
        let t = env.ctx().resolve(&path);
        assert!(t.external, "{path}");
        assert_eq!(t.absolute, file);
        assert_eq!(t.resource, file.display().to_string());
        let (out, err) = env
            .call("write", json!({"path": path, "content": "x"}))
            .await;
        assert!(!err, "{out}");
        assert_eq!(env.rec.actions(), ["external_directory", "edit"], "{path}");
        let first = env.rec.asked.lock().unwrap()[0].clone();
        assert_eq!(first.resources, [file.display().to_string()], "{path}");
        assert_eq!(
            first.save,
            [format!("{}/*", file.parent().unwrap().display())],
            "{path}"
        );
    }
}

/// "allows a relative path outside the Location but inside the project
/// worktree": the working folder is below the project root.
#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_allows_a_path_outside_the_folder_but_inside_the_project() {
    let (_d, dir) = tmp();
    let active = dir.join("packages/opencode");
    std::fs::create_dir_all(&active).unwrap();
    std::fs::write(dir.join("README.md"), "readme").unwrap();
    let env = Env::new(&active, &dir, vec![]);
    let t = env.ctx().resolve("../../README.md");
    assert_eq!(t.absolute, dir.join("README.md"));
    assert!(!t.external);
    let (out, err) = env.call("read", json!({"path": "../../README.md"})).await;
    assert!(!err, "{out}");
    assert!(!env
        .rec
        .actions()
        .contains(&"external_directory".to_string()));
}

/// "resolves a prospective target below an external symlink lexically".
/// Differs on purpose: CodeSmith judges a path by where it really is, so a
/// file created through a link out of the project asks `external_directory`
/// first.
#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_judges_a_target_below_an_external_symlink_by_where_it_is() {
    let (_d, dir) = tmp();
    let (_o, outside) = tmp();
    std::os::unix::fs::symlink(&outside, dir.join("escape")).unwrap();
    let env = Env::new(&dir, &dir, vec![]);
    let t = env.ctx().resolve("escape/new.txt");
    assert_eq!(t.absolute, dir.join("escape/new.txt"));
    assert_eq!(t.resource, "escape/new.txt");
    assert!(t.external);
    let (out, err) = env
        .call("write", json!({"path": "escape/new.txt", "content": "x"}))
        .await;
    assert!(!err, "{out}");
    assert_eq!(env.rec.actions(), ["external_directory", "edit"]);
}

/// "expands a leading tilde against the home directory", "resolves a
/// tilde path as an external home target" and "treats a tilde path as
/// in-location when the location is home". Fixed: a bare `~` was a file
/// named `~` in the working folder; it is the home folder now. HOME is the
/// harness's temporary one.
#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_expands_a_leading_tilde_against_home() {
    isolate();
    let home = dirs::home_dir().unwrap();
    assert!(home.starts_with(std::env::temp_dir()));
    let (_d, dir) = tmp();
    let env = Env::new(&dir, &dir, vec![]);
    let ctx = env.ctx();
    assert_eq!(ctx.resolve("~").absolute, home);
    assert_eq!(ctx.resolve("~/notes.md").absolute, home.join("notes.md"));
    assert_eq!(ctx.resolve("~draft.md").absolute, dir.join("~draft.md"));
    assert_eq!(ctx.resolve("~\\notes.md").absolute, dir.join("~\\notes.md"));
    let t = ctx.resolve("~/notes.md");
    assert!(t.external);
    assert_eq!(t.resource, home.join("notes.md").display().to_string());
    let (out, err) = env
        .call("write", json!({"path": "~/notes.md", "content": "x"}))
        .await;
    assert!(!err, "{out}");
    assert_eq!(env.rec.actions(), ["external_directory", "edit"]);
    // Working in the home folder, it is the project's.
    let at_home = Env::new(&home, &home, vec![]);
    let t = at_home.ctx().resolve("~/notes.md");
    assert!(!t.external);
    assert_eq!(t.resource, "notes.md");
}

// --- file-access.test.ts ------------------------------------------------

/// "authorizes an external directory before the file's read rules"
#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_authorizes_an_external_directory_before_the_read() {
    let (_d, base) = tmp();
    let dir = base.join("project");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(base.join("notes.txt"), "notes").unwrap();
    let env = Env::new(&dir, &dir, vec![]);
    let (out, err) = env.call("read", json!({"path": "../notes.txt"})).await;
    assert!(!err, "{out}");
    assert_eq!(env.rec.actions(), ["external_directory", "read"]);
    let asked = env.rec.asked.lock().unwrap().clone();
    assert_eq!(
        asked[1].resources[0],
        base.join("notes.txt").display().to_string()
    );
}

/// "propagates ${action} denial without continuing authorization"
#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_propagates_a_denial_without_continuing() {
    for (deny, asked) in [
        ("external_directory", vec!["external_directory"]),
        ("read", vec!["external_directory", "read"]),
    ] {
        let (_d, base) = tmp();
        let dir = base.join("project");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(base.join("notes.txt"), "secret notes").unwrap();
        let env = Env::new(&dir, &dir, vec![deny]);
        let (out, err) = env.call("read", json!({"path": "../notes.txt"})).await;
        assert!(err, "{out}");
        assert!(!out.contains("secret notes"));
        assert_eq!(env.rec.actions(), asked, "{deny}");
    }
}

/// "reuses a sibling's directory approval only for the supplied recovery
/// call" and "checks the external directory for a target that is not a
/// sibling": a read whose name is near an existing file's (a narrow
/// no-break space for a space) reads that sibling without a second
/// `external_directory` request; another external file asks again.
#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_reuses_a_siblings_directory_approval_only_for_its_recovery() {
    let (_d, base) = tmp();
    let dir = base.join("project");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(base.join("report final.txt"), "final report").unwrap();
    std::fs::write(base.join("notes.txt"), "notes").unwrap();
    let env = Env::new(&dir, &dir, vec![]);
    let (out, err) = env
        .call("read", json!({"path": "../report\u{202f}final.txt"}))
        .await;
    assert!(!err, "{out}");
    assert!(out.contains("final report"), "{out}");
    let recovered = env.rec.actions();
    assert_eq!(
        recovered
            .iter()
            .filter(|a| *a == "external_directory")
            .count(),
        1,
        "{recovered:?}"
    );
    let (out, err) = env.call("read", json!({"path": "../notes.txt"})).await;
    assert!(!err, "{out}");
    let all = env.rec.actions();
    assert_eq!(
        all.iter().filter(|a| *a == "external_directory").count(),
        2,
        "{all:?}"
    );
}

// --- permission.test.ts -------------------------------------------------

/// "uses build permissions when the Session agent is omitted"
#[tokio::test(flavor = "multi_thread")]
async fn oc_permission_uses_build_permissions_when_the_agent_is_omitted() {
    let mut t = start(Setup {
        extra: json!({"agents": {"build": {"permissions": [{"action": "question", "resource": "*", "effect": "deny"}]}}}),
        ..Default::default()
    })
    .await;
    t.prompt("Hello").await.unwrap();
    assert_eq!(t.s.agent.id, "build");
    assert!(!tool_names(&t.requests()[0]).contains(&"question".to_string()));
    assert!(!t.s.allows("question", "*"));
}

/// "rejects sibling requests in the same step and leaves another step's
/// request pending". Adapted: a rejection stops the turn, so the step's
/// other call is not run or asked about.
#[tokio::test(flavor = "multi_thread")]
async fn oc_permission_rejecting_a_call_stops_its_sibling() {
    let mut t = start(Setup {
        reply: Reply::Reject(None),
        extra: json!({"permissions": [{"action": "edit", "resource": "*", "effect": "ask"}]}),
        ..Default::default()
    })
    .await;
    t.push([
        calls(&[
            ("call_first", "write", r#"{"path":"a.txt","content":"a"}"#),
            ("call_sibling", "write", r#"{"path":"b.txt","content":"b"}"#),
        ]),
        text("must not run"),
    ]);
    let _ = t.prompt("Write both").await;
    assert!(!t.path("a.txt").exists() && !t.path("b.txt").exists());
    let r = results(&t.s);
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|r| r.2), "{r:?}");
    assert_eq!(t.requests().len(), 1);
}
