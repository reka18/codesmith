//! Ports of OpenCode's Mercurial tests (`project.test.ts`'s marker cases,
//! `vcs-hg.test.ts`). The cases that run `hg` are skipped where it is not
//! installed, as OpenCode's are.

use codesmith_core::vcs::{self, Kind, Mode};
use std::path::Path;
use std::process::Command;

fn git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let run = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    run(&["init", "-q"]);
    run(&[
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "root",
    ]);
}

fn real(p: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap()
}

// project.test.ts

#[test]
fn oc_project_prefers_git_when_both_git_and_mercurial_metadata_exist() {
    let d = tempfile::tempdir().unwrap();
    git_repo(d.path());
    std::fs::create_dir(d.path().join(".hg")).unwrap();
    let p = vcs::resolve(d.path());
    assert_eq!(p.vcs, Some(Kind::Git));
    assert_ne!(p.id, vcs::GLOBAL);
}

#[test]
fn oc_project_prefers_the_nearest_mercurial_marker_over_an_outer_git_repository() {
    let d = tempfile::tempdir().unwrap();
    git_repo(d.path());
    let nested = d.path().join("nested");
    std::fs::create_dir_all(nested.join(".hg")).unwrap();
    std::fs::create_dir(nested.join("app")).unwrap();
    let p = vcs::resolve(&real(&nested.join("app")));
    assert_eq!(p.vcs, Some(Kind::Hg));
    assert_eq!(p.directory, real(&nested));
    assert_eq!(
        codesmith_core::config::project_root(&real(&nested.join("app"))),
        real(&nested)
    );
}

#[test]
fn oc_project_prefers_the_nearest_git_marker_over_an_outer_mercurial_repository() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join(".hg")).unwrap();
    let nested = d.path().join("nested");
    std::fs::create_dir_all(nested.join("app")).unwrap();
    git_repo(&nested);
    let p = vcs::resolve(&real(&nested.join("app")));
    assert_eq!(p.vcs, Some(Kind::Git));
    assert_eq!(p.directory, real(&nested));
}

#[test]
fn oc_project_returns_global_id_for_unreadable_mercurial_metadata() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join(".hg")).unwrap();
    let p = vcs::resolve(d.path());
    assert_eq!(p.vcs, Some(Kind::Hg));
    assert_eq!(p.id, vcs::GLOBAL);
}

// vcs-hg.test.ts

#[test]
fn oc_mercurial_rejects_unsupported_review_modes_and_bases_without_requiring_hg() {
    let d = tempfile::tempdir().unwrap();
    let e = vcs::hg_diff(d.path(), Mode::Committed, None).unwrap_err();
    assert!(
        e.to_string().contains("does not support committed reviews"),
        "{e}"
    );
    assert!(vcs::hg_diff(d.path(), Mode::Branch, Some("release")).is_err());
    // `/review` refuses a target in a Mercurial project the same way.
    std::fs::create_dir(d.path().join(".hg")).unwrap();
    let e = codesmith_core::review::target::resolve(d.path(), Some("release")).unwrap_err();
    assert!(e.to_string().contains("explicit bases"), "{e}");
}

fn have_hg() -> bool {
    Command::new("hg")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn hg(dir: &Path, args: &[&str]) {
    let ok = Command::new("hg")
        .args(args)
        .current_dir(dir)
        .env("HGPLAIN", "1")
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "hg {args:?}");
}

fn commit_all(dir: &Path, message: &str) {
    hg(dir, &["addremove", "-q"]);
    hg(dir, &["commit", "-q", "-m", message, "-u", "test"]);
}

#[test]
fn oc_vcs_mercurial() {
    if !have_hg() {
        eprintln!("hg is not installed; skipping");
        return;
    }
    // reports modified, missing, and untracked files
    let d = tempfile::tempdir().unwrap();
    let dir = d.path();
    hg(dir, &["init"]);
    std::fs::write(dir.join("keep.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dir.join("gone.txt"), "bye\n").unwrap();
    commit_all(dir, "initial");
    std::fs::write(dir.join("keep.txt"), "one\nthree\n").unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    std::fs::write(dir.join("new.txt"), "hello\nworld\n").unwrap();
    let status: Vec<(String, usize, usize, &str)> = vcs::hg_status(dir)
        .into_iter()
        .map(|f| (f.file, f.additions, f.deletions, f.status))
        .collect();
    assert_eq!(
        status,
        [
            ("gone.txt".to_string(), 0, 1, "deleted"),
            ("keep.txt".to_string(), 1, 1, "modified"),
            ("new.txt".to_string(), 2, 0, "added"),
        ]
    );
    let diff = vcs::hg_diff(dir, Mode::Working, None).unwrap();
    assert!(diff
        .iter()
        .find(|f| f.file == "new.txt")
        .unwrap()
        .patch
        .contains("+hello"));
    assert!(diff
        .iter()
        .find(|f| f.file == "gone.txt")
        .unwrap()
        .patch
        .contains("-bye"));
    // diffs a named branch against the default branch
    let d = tempfile::tempdir().unwrap();
    let dir = d.path();
    hg(dir, &["init"]);
    std::fs::write(dir.join("file.txt"), "one\n").unwrap();
    commit_all(dir, "initial");
    assert!(vcs::hg_diff(dir, Mode::Branch, None).unwrap().is_empty());
    hg(dir, &["branch", "-q", "feature"]);
    std::fs::write(dir.join("file.txt"), "one\ntwo\n").unwrap();
    commit_all(dir, "feature change");
    let diff = vcs::hg_diff(dir, Mode::Branch, None).unwrap();
    assert_eq!(diff.len(), 1);
    assert_eq!(
        (diff[0].file.as_str(), diff[0].status),
        ("file.txt", "modified")
    );
    assert!(diff[0].patch.contains("+two"));
}

#[test]
fn a_synthesized_patch_counts_its_lines() {
    let p = vcs::synthesize("a b.txt", "", "x\ny\n");
    assert!(p.starts_with("diff --git a/a b.txt b/a b.txt\nnew file mode 100644\n--- /dev/null\n+++ b/a b.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n"));
    assert_eq!(codesmith_core::review::target::diff_files(&p), ["a b.txt"]);
}
