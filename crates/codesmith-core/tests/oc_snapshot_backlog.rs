//! Ports of the snapshot tests in PARITY.md's porting backlog
//! (`packages/core/test/snapshot.test.ts`, MIT). OpenCode snapshots a
//! Location inside its Git worktree; CodeSmith keeps a private repository
//! per session root, so a "Location" here is the root itself.

mod scripted;

use codesmith_core::snapshot::{Change, Snapshots};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A temporary directory with a project folder and a snapshot store; the
/// user's home and Git config are kept out.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Option<Fixture> {
        scripted::isolate();
        git_available().then(|| Fixture {
            dir: tempfile::tempdir().unwrap(),
        })
    }
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(p)
    }
    fn snapshots(&self, root: &Path) -> Snapshots {
        Snapshots::new(root, &self.path("store")).unwrap()
    }
}

fn write(p: &Path, text: &str) {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, text).unwrap();
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn paths(changes: &[Change]) -> Vec<String> {
    let mut p: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
    p.sort();
    p
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {:?}", out);
}

/// "captures and restores Location-scoped changes". Adapted: the root is
/// the snapshot's scope; a restore of one path leaves the others.
#[test]
fn oc_snapshot_captures_and_restores_only_the_selected_paths() {
    let Some(f) = Fixture::new() else { return };
    let root = f.path("project");
    write(&root.join("tracked.txt"), "one\n");
    let outside = f.path("outside.txt");
    write(&outside, "outside\n");
    let s = f.snapshots(&root);
    let before = s.capture().unwrap();
    write(&root.join("tracked.txt"), "two\n");
    write(&root.join("added.txt"), "added\n");
    write(&outside, "changed outside\n");
    let after = s.capture().unwrap();
    assert_eq!(
        paths(&s.changes(&before, &after).unwrap()),
        ["added.txt", "tracked.txt"]
    );
    s.restore_only(&before, Some(&["tracked.txt".to_string()]))
        .unwrap();
    assert_eq!(read(&root.join("tracked.txt")), "one\n");
    assert_eq!(read(&root.join("added.txt")), "added\n");
    assert_eq!(read(&outside), "changed outside\n");
}

/// "recovers from a corrupt index and ignores a stale index lock". Fixed: a
/// lock left by a killed capture failed every later capture and undo.
#[test]
fn oc_snapshot_recovers_from_a_corrupt_index_and_a_stale_lock() {
    let Some(f) = Fixture::new() else { return };
    let root = f.path("project");
    write(&root.join("tracked.txt"), "one\n");
    let s = f.snapshots(&root);
    let before = s.capture().unwrap();
    let store: Vec<PathBuf> = std::fs::read_dir(f.path("store"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(store.len(), 1, "one repository for the root");
    std::fs::write(store[0].join("index"), [0u8; 512]).unwrap();
    std::fs::write(store[0].join("index.lock"), "").unwrap();
    write(&root.join("tracked.txt"), "two\n");
    let after = s.capture().unwrap();
    assert_eq!(paths(&s.changes(&before, &after).unwrap()), ["tracked.txt"]);
    s.restore(&before).unwrap();
    assert_eq!(read(&root.join("tracked.txt")), "one\n");
}

/// "captures a Location in a directory whose name starts with two dots"
#[test]
fn oc_snapshot_captures_a_root_whose_name_starts_with_two_dots() {
    let Some(f) = Fixture::new() else { return };
    let root = f.path("project/..scope");
    write(&root.join("tracked.txt"), "one\n");
    let s = f.snapshots(&root);
    let before = s.capture().unwrap();
    write(&root.join("tracked.txt"), "two\n");
    let after = s.capture().unwrap();
    assert_eq!(paths(&s.changes(&before, &after).unwrap()), ["tracked.txt"]);
}

/// "captures and restores a Location opened through a symlinked path"
#[cfg(unix)]
#[test]
fn oc_snapshot_captures_and_restores_a_root_opened_through_a_symlink() {
    let Some(f) = Fixture::new() else { return };
    let project = f.path("project");
    write(&project.join("scope/tracked.txt"), "one\n");
    let link = f.path("link");
    std::os::unix::fs::symlink(&project, &link).unwrap();
    let s = f.snapshots(&link.join("scope"));
    let before = s.capture().unwrap();
    write(&link.join("scope/tracked.txt"), "two\n");
    let after = s.capture().unwrap();
    assert_eq!(paths(&s.changes(&before, &after).unwrap()), ["tracked.txt"]);
    s.restore_only(&before, Some(&["tracked.txt".to_string()]))
        .unwrap();
    assert_eq!(read(&project.join("scope/tracked.txt")), "one\n");
}

/// "restores many files from several trees and removes paths absent from
/// them". Adapted: one restore per tree, as undo restores each turn's
/// paths from its own start.
#[test]
fn oc_snapshot_restores_files_from_several_trees_and_removes_absent_ones() {
    let Some(f) = Fixture::new() else { return };
    let root = f.path("project");
    write(&root.join("a.txt"), "a1\n");
    write(&root.join("b[1].txt"), "b1\n");
    let s = f.snapshots(&root);
    let first = s.capture().unwrap();
    write(&root.join("a.txt"), "a2\n");
    write(&root.join("c.txt"), "c2\n");
    let second = s.capture().unwrap();
    write(&root.join("a.txt"), "a3\n");
    write(&root.join("b[1].txt"), "b3\n");
    write(&root.join("c.txt"), "c3\n");
    write(&root.join("d.txt"), "d3\n");
    let own = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    s.restore_only(&second, Some(&own(&["a.txt", "d.txt"])))
        .unwrap();
    s.restore_only(&first, Some(&own(&["b[1].txt", "c.txt"])))
        .unwrap();
    assert_eq!(read(&root.join("a.txt")), "a2\n");
    assert_eq!(read(&root.join("b[1].txt")), "b1\n");
    assert!(!root.join("c.txt").exists());
    assert!(!root.join("d.txt").exists());
}

/// "restores and diffs a selection too long for one command line"
#[test]
fn oc_snapshot_restores_and_diffs_a_selection_too_long_for_one_command_line() {
    let Some(f) = Fixture::new() else { return };
    let root = f.path("project");
    let names: Vec<String> = (0..300)
        .map(|i| format!("deep/{}-{i}.txt", "n".repeat(90)))
        .collect();
    for n in &names {
        write(&root.join(n), "one\n");
    }
    let s = f.snapshots(&root);
    let first = s.capture().unwrap();
    for n in &names {
        write(&root.join(n), "two\n");
    }
    let second = s.capture().unwrap();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(paths(&s.changes(&first, &second).unwrap()), sorted);
    let diff = s.diff(&first, &second).unwrap();
    assert_eq!(diff.matches("\n+two").count(), 300);
    s.restore_only(&first, Some(&names)).unwrap();
    assert!(names.iter().all(|n| read(&root.join(n)) == "one\n"));
}

/// "restores the other files when removing one path fails". Fixed: the
/// first file that could not be removed stopped the restore, so files
/// after it kept the turn's changes.
#[cfg(unix)]
#[test]
fn oc_snapshot_restores_the_other_files_when_removing_one_fails() {
    use std::os::unix::fs::PermissionsExt;
    let Some(f) = Fixture::new() else { return };
    let root = f.path("project");
    write(&root.join("c.txt"), "old\n");
    std::fs::create_dir_all(root.join("locked")).unwrap();
    let s = f.snapshots(&root);
    let first = s.capture().unwrap();
    write(&root.join("c.txt"), "changed\n");
    write(&root.join("locked/new.txt"), "new\n");
    let locked = root.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = s.restore(&first);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Run as root, the removal succeeds; either way c.txt is restored.
    if root.join("locked/new.txt").exists() {
        assert!(r.is_err(), "the failed removal is reported");
    }
    assert_eq!(read(&root.join("c.txt")), "old\n");
}

/// "isolates snapshot indexes by canonical Git worktree". Adapted: each
/// session root, a linked worktree included, has its own repository.
#[test]
fn oc_snapshot_isolates_linked_worktrees() {
    let Some(f) = Fixture::new() else { return };
    let project = f.path("project");
    write(&project.join("tracked.txt"), "main\n");
    git(&project, &["init", "--quiet"]);
    git(&project, &["add", "."]);
    git(&project, &["commit", "--quiet", "-m", "init"]);
    let linked = f.path("linked");
    git(
        &project,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    let main = f.snapshots(&project);
    let other = f.snapshots(&linked);
    let main_before = main.capture().unwrap();
    let other_before = other.capture().unwrap();
    write(&linked.join("tracked.txt"), "linked\n");
    let main_after = main.capture().unwrap();
    let other_after = other.capture().unwrap();
    assert!(main.changes(&main_before, &main_after).unwrap().is_empty());
    assert_eq!(
        paths(&other.changes(&other_before, &other_after).unwrap()),
        ["tracked.txt"]
    );
    assert_eq!(read(&project.join("tracked.txt")), "main\n");
}
