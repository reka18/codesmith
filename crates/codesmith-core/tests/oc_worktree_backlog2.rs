//! Ports of the worktree cases in PARITY.md's porting backlog
//! (`packages/core/test/worktree.test.ts`, `git.test.ts`, MIT), against a
//! scratch repository. The harness's HOME keeps the user's Git config out.

mod scripted;

use codesmith_core::worktree::{self, Create, WorktreeConfig};
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A repository with one commit, and a folder for worktrees beside it.
fn repo() -> Option<(tempfile::TempDir, PathBuf, PathBuf)> {
    if !git_available() {
        return None;
    }
    scripted::isolate();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "readme\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    Some((tmp, repo, base.join("worktrees")))
}

fn cfg(dir: &Path) -> WorktreeConfig {
    WorktreeConfig {
        directory: Some(dir.display().to_string()),
        start: None,
    }
}

fn named(name: &str) -> Create {
    Create {
        name: Some(name.into()),
        ..Default::default()
    }
}

/// "creates and removes a git worktree directory" and "creates, lists, and
/// removes linked worktrees" (`git.test.ts`)
#[test]
fn oc_worktree_creates_lists_and_removes_a_worktree() {
    let Some((_t, repo, parent)) = repo() else {
        return;
    };
    let (dir, sub) = worktree::create(&cfg(&parent), &repo, &named("worktree")).unwrap();
    assert_eq!(dir, parent.join("worktree").canonicalize().unwrap());
    assert_eq!(sub, dir);
    let listed = worktree::list(&repo).unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].main);
    assert_eq!(listed[1].directory.canonicalize().unwrap(), dir);
    worktree::remove(&cfg(&parent), &repo, "worktree", false).unwrap();
    assert!(!dir.exists());
    assert_eq!(worktree::list(&repo).unwrap().len(), 1);
}

/// "defaults to the TUI worktree directory and suffixes duplicate names",
/// "adds a numeric suffix when a worktree directory already exists" and
/// "fails after ten worktree directory conflicts". Adapted: the default
/// folder is `<data>/worktree/<project>`.
#[test]
fn oc_worktree_suffixes_duplicate_names_and_gives_up_after_ten() {
    let Some((_t, repo, _)) = repo() else { return };
    let default = WorktreeConfig::default();
    let parent = worktree::base_dir(&default, &repo);
    assert!(parent.starts_with(codesmith_core::paths::data_dir().join("worktree")));
    let (first, _) = worktree::create(&default, &repo, &named("task")).unwrap();
    let (second, _) = worktree::create(&default, &repo, &named("task")).unwrap();
    assert_eq!(first.file_name().unwrap(), "task");
    assert_eq!(second.file_name().unwrap(), "task-2");
    assert!(first.join(".git").exists());
    for n in 3..=10 {
        std::fs::create_dir_all(parent.join(format!("task-{n}"))).unwrap();
    }
    let e = worktree::create(&default, &repo, &named("task")).unwrap_err();
    assert!(e.to_string().contains("already exists"), "{e}");
    worktree::remove(&default, &repo, "task", false).unwrap();
    worktree::remove(&default, &repo, "task-2", false).unwrap();
    let _ = std::fs::remove_dir_all(&parent);
}

/// "creates a git worktree from a selected branch"
#[test]
fn oc_worktree_creates_a_worktree_from_a_selected_branch() {
    let Some((_t, repo, parent)) = repo() else {
        return;
    };
    git(&repo, &["branch", "feature-base"]);
    std::fs::write(repo.join("later.txt"), "later\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "later"]);
    let opts = Create {
        name: Some("worktree".into()),
        from: Some("feature-base".into()),
        branch: None,
    };
    let (dir, _) = worktree::create(&cfg(&parent), &repo, &opts).unwrap();
    assert_eq!(
        git(&dir, &["rev-parse", "HEAD"]),
        git(&repo, &["rev-parse", "feature-base"])
    );
    assert!(!dir.join("later.txt").exists());
}

/// "does not interpret a branch as a git option"
#[test]
fn oc_worktree_does_not_interpret_a_branch_as_a_git_option() {
    let Some((_t, repo, parent)) = repo() else {
        return;
    };
    let opts = Create {
        name: Some("worktree".into()),
        from: Some("--no-checkout".into()),
        branch: None,
    };
    assert!(worktree::create(&cfg(&parent), &repo, &opts).is_err());
    assert!(!parent.join("worktree").join("README.md").exists());
}

/// "rejects a missing source directory"
#[test]
fn oc_worktree_rejects_a_missing_source_directory() {
    let Some((_t, repo, parent)) = repo() else {
        return;
    };
    let missing = repo.join("missing");
    assert!(worktree::create(&cfg(&parent), &missing, &named("worktree")).is_err());
    assert!(!parent.join("worktree").exists());
}

/// "requires force to remove a dirty git worktree"
#[test]
fn oc_worktree_requires_force_to_remove_a_dirty_worktree() {
    let Some((_t, repo, parent)) = repo() else {
        return;
    };
    let (dir, _) = worktree::create(&cfg(&parent), &repo, &named("worktree")).unwrap();
    std::fs::write(dir.join("dirty.txt"), "dirty").unwrap();
    assert!(worktree::remove(&cfg(&parent), &repo, "worktree", false).is_err());
    assert!(dir.exists());
    worktree::remove(&cfg(&parent), &repo, "worktree", true).unwrap();
    assert!(!dir.exists());
}

/// "runs the project setup script with worktree paths"
#[test]
fn oc_worktree_runs_the_setup_command_with_worktree_paths() {
    let Some((_t, repo, parent)) = repo() else {
        return;
    };
    let config = WorktreeConfig {
        directory: Some(parent.display().to_string()),
        start: Some(
            "printf '%s\\n%s\\n%s\\n' \"$CODESMITH_WORKTREE_BASE\" \"$CODESMITH_WORKTREE_PATH\" \"$PWD\" > setup.txt"
                .into(),
        ),
    };
    let (dir, _) = worktree::create(&config, &repo, &named("worktree")).unwrap();
    let lines: Vec<String> = std::fs::read_to_string(dir.join("setup.txt"))
        .unwrap()
        .lines()
        .map(|l| {
            PathBuf::from(l)
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        })
        .collect();
    let d = dir.display().to_string();
    assert_eq!(lines, [repo.display().to_string(), d.clone(), d]);
    worktree::remove(&config, &repo, "worktree", true).unwrap();
}

/// "reads no remote and no root commits from an unborn repository"
/// (`git.test.ts`) and "returns git global for repo with no commits and no
/// remote" (`project.test.ts`). Adapted: without a commit the project is
/// named by its path.
#[test]
fn oc_project_id_of_an_unborn_repository_is_its_path() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    for d in [&a, &b] {
        std::fs::create_dir(d).unwrap();
        git(d, &["init", "-q"]);
    }
    assert_eq!(worktree::project_id(&a), worktree::project_id(&a));
    assert_ne!(worktree::project_id(&a), worktree::project_id(&b));
}

/// "falls back to root commit when origin is missing and the cached id is
/// corrupt", "resolves from nested directories to repo root" and "linked
/// worktree returns opened worktree directory and previous from common
/// dir". Adapted: the project id is the root commit's, from any folder of
/// the repository or its worktrees.
#[test]
fn oc_project_id_is_the_root_commit_from_anywhere_in_the_repository() {
    let Some((_t, repo, parent)) = repo() else {
        return;
    };
    let root = git(&repo, &["rev-list", "--max-parents=0", "HEAD"]);
    let id = worktree::project_id(&repo);
    assert_eq!(id, root[..12]);
    std::fs::create_dir_all(repo.join("nested/deeper")).unwrap();
    assert_eq!(worktree::project_id(&repo.join("nested/deeper")), id);
    let (dir, _) = worktree::create(&cfg(&parent), &repo, &named("linked")).unwrap();
    assert_eq!(worktree::project_id(&dir), id);
    worktree::remove(&cfg(&parent), &repo, "linked", true).unwrap();
}
