//! Ports of OpenCode's `packages/core/test/vcs.test.ts` (MIT) against the
//! diff viewer's Git side: `vcs.branches` is [`branches`], `vcs.base` is
//! [`base`], and `vcs.diff`/`vcs.status` are [`vcs_diff`] (a status is a
//! diff's file list and counts). Each repository is a temporary one with
//! the user's Git configuration left out of the commands that build it.

use super::*;
use std::path::Path;
use std::process::Command;

fn git_in(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .envs(env.iter().copied())
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
        ])
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn g(dir: &Path, args: &[&str]) -> String {
    git_in(dir, args, &[])
}

fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

fn commit_all(dir: &Path, message: &str) {
    g(dir, &["add", "-A"]);
    g(dir, &["commit", "-qm", message]);
}

/// A new repository on `main` (OpenCode's `withGit`), at its real path.
fn repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().canonicalize().unwrap();
    g(&p, &["init", "-q", "-b", "main"]);
    (d, p)
}

fn files(diff: &[FileDiff]) -> Vec<&str> {
    diff.iter().map(|f| f.file.as_str()).collect()
}

fn named(diff: &[FileDiff]) -> Vec<(&str, Status)> {
    diff.iter().map(|f| (f.file.as_str(), f.status)).collect()
}

/// A file's hunks as patch text (`-old`/`+new`/` context` lines).
fn patch(f: &FileDiff) -> String {
    let mut s = String::new();
    for l in f.hunks.iter().flat_map(|h| &h.lines) {
        s.push(match l.kind {
            LineKind::Add => '+',
            LineKind::Del => '-',
            LineKind::Context => ' ',
        });
        s.push_str(&l.text);
        s.push('\n');
    }
    s
}

fn diff(dir: &Path, source: Source, base: Option<&str>) -> Vec<FileDiff> {
    vcs_diff(dir, source, base).unwrap()
}

fn base_of(dir: &Path) -> Base {
    base(dir).unwrap().expect("a base")
}

fn no_base(dir: &Path) {
    assert_eq!(base(dir).unwrap_err(), "Choose a review base");
}

// vcs.test.ts: "lists local branches by recent activity" (adapted: the
// base picker filters and limits the list; the list is most recent first)
#[test]
fn oc_vcs_lists_local_branches_by_recent_activity() {
    let (_d, p) = repo();
    write(&p, "file.txt", "one\n");
    commit_all(&p, "initial");
    g(&p, &["checkout", "-qb", "z-recent"]);
    write(&p, "file.txt", "two\n");
    g(&p, &["add", "-A"]);
    let date = [
        ("GIT_AUTHOR_DATE", "2030-01-01T00:00:00Z"),
        ("GIT_COMMITTER_DATE", "2030-01-01T00:00:00Z"),
    ];
    git_in(&p, &["commit", "-qm", "recent"], &date);
    assert_eq!(branches(&p).unwrap(), ["z-recent", "main"]);
}

// vcs.test.ts: "lists remote branches without remote HEAD symrefs"
#[test]
fn oc_vcs_lists_remote_branches_without_remote_head_symrefs() {
    let (_d, p) = repo();
    write(&p, "file.txt", "one\n");
    commit_all(&p, "initial");
    let dir = p.display().to_string();
    g(&p, &["tag", "main"]);
    g(&p, &["remote", "add", "origin", &dir]);
    g(&p, &["remote", "add", "fork", &dir]);
    g(&p, &["fetch", "--quiet", "origin"]);
    g(&p, &["fetch", "--quiet", "fork"]);
    g(&p, &["remote", "set-head", "origin", "main"]);
    g(&p, &["remote", "set-head", "fork", "main"]);
    assert_eq!(branches(&p).unwrap(), ["main", "fork/main", "origin/main"]);
}

// vcs.test.ts: "reports modified, deleted, and untracked files"
#[test]
fn oc_vcs_reports_modified_deleted_and_untracked_files() {
    let (_d, p) = repo();
    write(&p, "keep.txt", "one\ntwo\n");
    write(&p, "gone.txt", "bye\n");
    commit_all(&p, "initial");
    write(&p, "keep.txt", "one\nthree\n");
    std::fs::remove_file(p.join("gone.txt")).unwrap();
    write(&p, "new.txt", "hello\nworld\n");
    let status: Vec<_> = diff(&p, Source::Working, None)
        .into_iter()
        .map(|f| (f.file, f.additions, f.deletions, f.status))
        .collect();
    assert_eq!(
        status,
        [
            ("gone.txt".to_string(), 0, 1, Status::Deleted),
            ("keep.txt".to_string(), 1, 1, Status::Modified),
            ("new.txt".to_string(), 2, 0, Status::Added),
        ]
    );
}

// vcs.test.ts: "diffs the working copy against HEAD with patches"
#[test]
fn oc_vcs_diffs_the_working_copy_against_head_with_patches() {
    let (_d, p) = repo();
    write(&p, "keep.txt", "one\ntwo\n");
    commit_all(&p, "initial");
    write(&p, "keep.txt", "one\nthree\n");
    write(&p, "spaced name.txt", "hello\n");
    let d = diff(&p, Source::Working, None);
    assert_eq!(
        named(&d),
        [
            ("keep.txt", Status::Modified),
            ("spaced name.txt", Status::Added)
        ]
    );
    assert!(patch(&d[0]).contains("-two\n+three\n"));
    assert_eq!((d[0].additions, d[0].deletions), (1, 1));
    assert!(patch(&d[1]).contains("+hello"));
    assert_eq!(d[1].additions, 1);
}

// vcs.test.ts: "respects the repository's line-ending conversion"
#[test]
fn oc_vcs_respects_the_repositorys_line_ending_conversion() {
    let (_d, p) = repo();
    g(&p, &["config", "core.autocrlf", "true"]);
    let body: String = (0..20).map(|i| format!("line-{i}\r\n")).collect();
    write(&p, "file.txt", &body);
    write(&p, "same.txt", &body);
    commit_all(&p, "initial");
    write(&p, "file.txt", &body.replace("line-10", "changed"));
    let d = diff(&p, Source::Working, None);
    let rows: Vec<_> = d
        .iter()
        .map(|f| (f.file.as_str(), f.additions, f.deletions))
        .collect();
    assert_eq!(rows, [("file.txt", 1, 1)]);
    let text = patch(&d[0]);
    assert!(text.contains("-line-10"), "{text}");
    assert!(text.contains("+changed"), "{text}");
}

// vcs.test.ts: "respects the repository's symlink setting"
#[test]
fn oc_vcs_respects_the_repositorys_symlink_setting() {
    let (_d, p) = repo();
    // A checkout that cannot make symlinks holds the target as a file.
    g(&p, &["config", "core.symlinks", "false"]);
    write(&p, "link", "target.txt");
    let hash = g(&p, &["hash-object", "-w", "link"]);
    g(
        &p,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{hash},link"),
        ],
    );
    g(&p, &["commit", "-qm", "initial"]);
    assert_eq!(diff(&p, Source::Working, None), []);
}

// vcs.test.ts: "respects the context option" (adapted: the viewer's
// context is fixed at OpenCode's twelve lines, so a change in the middle
// of a twenty-line file shows the whole file; there is no per-call option)
#[test]
fn oc_vcs_respects_the_context_option() {
    let (_d, p) = repo();
    let body: String = (0..20).map(|i| format!("line-{i}\n")).collect();
    write(&p, "file.txt", &body);
    commit_all(&p, "initial");
    write(&p, "file.txt", &body.replace("line-10", "changed"));
    let full = patch(&diff(&p, Source::Working, None)[0]);
    assert!(full.contains("line-0\n") && full.contains("line-19\n"));
    assert_eq!(CONTEXT, 12);
    // Twelve lines either side, no more: a longer file is cut.
    let long: String = (0..40).map(|i| format!("line-{i}\n")).collect();
    write(&p, "file.txt", &long);
    commit_all(&p, "longer");
    write(&p, "file.txt", &long.replace("line-20\n", "changed\n"));
    let tight = patch(&diff(&p, Source::Working, None)[0]);
    assert!(tight.contains(" line-8\n") && tight.contains(" line-32\n"));
    assert!(!tight.contains("line-7\n") && !tight.contains("line-33\n"));
}

// vcs.test.ts: "diffs before the first commit"
#[test]
fn oc_vcs_diffs_before_the_first_commit() {
    let (_d, p) = repo();
    write(&p, "new.txt", "hello\n");
    let working = diff(&p, Source::Working, None);
    let rows: Vec<_> = working
        .iter()
        .map(|f| (f.file.as_str(), f.additions, f.deletions, f.status))
        .collect();
    assert_eq!(rows, [("new.txt", 1, 0, Status::Added)]);
    assert!(patch(&working[0]).contains("+hello"));
    assert_eq!(base(&p).unwrap(), None);
    assert_eq!(diff(&p, Source::Branch, None), working);
    assert_eq!(diff(&p, Source::Committed, None), []);
}

// vcs.test.ts: "diffs a feature branch against the default branch"
#[test]
fn oc_vcs_diffs_a_feature_branch_against_the_default_branch() {
    let (_d, p) = repo();
    write(&p, "file.txt", "one\n");
    commit_all(&p, "initial");
    assert_eq!(diff(&p, Source::Branch, None), []);
    g(&p, &["checkout", "-qb", "feature", "main"]);
    write(&p, "file.txt", "one\ntwo\n");
    commit_all(&p, "feature change");
    let d = diff(&p, Source::Branch, None);
    assert_eq!(named(&d), [("file.txt", Status::Modified)]);
    assert!(patch(&d[0]).contains("+two"));
}

// vcs.test.ts: "separates committed, combined, and staged/unstaged/untracked
// working changes"
#[test]
fn oc_vcs_separates_committed_combined_and_working_changes() {
    let (_d, p) = repo();
    write(&p, "file.txt", "base\n");
    commit_all(&p, "initial");
    g(&p, &["checkout", "-qb", "feature", "main"]);
    write(&p, "file.txt", "committed\n");
    commit_all(&p, "feature");
    write(&p, "staged.txt", "staged\n");
    g(&p, &["add", "staged.txt"]);
    write(&p, "file.txt", "unstaged\n");
    write(&p, "untracked.txt", "untracked\n");
    let committed = diff(&p, Source::Committed, None);
    assert_eq!(files(&committed), ["file.txt"]);
    assert!(patch(&committed[0]).contains("-base\n+committed\n"));
    assert_eq!((committed[0].additions, committed[0].deletions), (1, 1));
    let combined = diff(&p, Source::Branch, None);
    assert_eq!(
        files(&combined),
        ["file.txt", "staged.txt", "untracked.txt"]
    );
    assert!(patch(&combined[0]).contains("-base\n+unstaged\n"));
    let working = diff(&p, Source::Working, None);
    assert_eq!(files(&working), ["file.txt", "staged.txt", "untracked.txt"]);
    assert!(patch(&working[0]).contains("-committed\n+unstaged\n"));
}

// vcs.test.ts: "keeps a locally undone commit visible only in committed
// and working modes"
#[test]
fn oc_vcs_keeps_a_locally_undone_commit_visible_only_in_committed_and_working() {
    let (_d, p) = repo();
    write(&p, "file.txt", "base\n");
    commit_all(&p, "initial");
    g(&p, &["checkout", "-qb", "feature", "main"]);
    write(&p, "file.txt", "committed\n");
    commit_all(&p, "feature");
    write(&p, "file.txt", "base\n");
    assert_eq!(diff(&p, Source::Branch, None), []);
    assert!(patch(&diff(&p, Source::Committed, None)[0]).contains("-base\n+committed\n"));
    assert!(patch(&diff(&p, Source::Working, None)[0]).contains("-committed\n+base\n"));
}

// vcs.test.ts: "shows local WIP on the default branch"
#[test]
fn oc_vcs_shows_local_wip_on_the_default_branch() {
    let (_d, p) = repo();
    write(&p, "file.txt", "base\n");
    commit_all(&p, "initial");
    write(&p, "file.txt", "dirty\n");
    assert_eq!(
        base_of(&p),
        Base {
            name: "main".into(),
            reference: "refs/heads/main".into()
        }
    );
    assert_eq!(
        diff(&p, Source::Branch, None),
        diff(&p, Source::Working, None)
    );
    assert_eq!(diff(&p, Source::Committed, None), []);
}

// vcs.test.ts: "keeps unpushed default-branch commits in the resolved review"
#[test]
fn oc_vcs_keeps_unpushed_default_branch_commits_in_the_resolved_review() {
    let (_d, p) = repo();
    write(&p, "file.txt", "base\n");
    commit_all(&p, "initial");
    g(&p, &["remote", "add", "origin", &p.display().to_string()]);
    g(&p, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    g(
        &p,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    write(&p, "file.txt", "committed\n");
    commit_all(&p, "unpushed");
    write(&p, "file.txt", "dirty\n");
    let b = base_of(&p);
    assert_eq!(
        b,
        Base {
            name: "main".into(),
            reference: "refs/remotes/origin/main".into()
        }
    );
    let r = Some(b.reference.as_str());
    assert!(patch(&diff(&p, Source::Committed, r)[0]).contains("-base\n+committed\n"));
    assert!(patch(&diff(&p, Source::Branch, r)[0]).contains("-base\n+dirty\n"));
    assert!(patch(&diff(&p, Source::Working, None)[0]).contains("-committed\n+dirty\n"));
}

// vcs.test.ts: "scopes committed diffs to nested paths and retains binary,
// rename, and deletion metadata" (adapted: a binary file has no hunks
// rather than a "Binary files" patch)
#[test]
fn oc_vcs_scopes_committed_diffs_to_nested_paths_with_binary_rename_and_deletion() {
    let (_d, p) = repo();
    write(&p, "outside.txt", "base\n");
    write(&p, "nested/old.txt", "rename me\n");
    write(&p, "nested/gone.txt", "delete me\n");
    std::fs::write(p.join("nested/image.png"), [137, 80, 78, 71, 0, 1]).unwrap();
    commit_all(&p, "initial");
    g(&p, &["checkout", "-qb", "feature", "main"]);
    write(&p, "outside.txt", "changed\n");
    std::fs::rename(p.join("nested/old.txt"), p.join("nested/new.txt")).unwrap();
    std::fs::remove_file(p.join("nested/gone.txt")).unwrap();
    std::fs::write(p.join("nested/image.png"), [137, 80, 78, 71, 0, 2]).unwrap();
    commit_all(&p, "feature");
    let nested = p.join("nested");
    let committed = diff(&nested, Source::Committed, None);
    assert_eq!(
        named(&committed),
        [
            ("nested/gone.txt", Status::Deleted),
            ("nested/image.png", Status::Modified),
            ("nested/new.txt", Status::Added),
            ("nested/old.txt", Status::Deleted),
        ]
    );
    assert!(patch(&committed[0]).contains("-delete me"));
    assert_eq!((committed[1].additions, committed[1].deletions), (0, 0));
    assert!(committed[1].hunks.is_empty(), "a binary file has no hunks");
    assert!(patch(&committed[2]).contains("+rename me"));
    // Working changes leave the committed review as it was.
    std::fs::remove_file(p.join("nested/image.png")).unwrap();
    write(&p, "nested/new.txt", "dirty\n");
    write(&p, "nested/gone.txt", "untracked resurrection\n");
    g(&p, &["add", "nested/new.txt"]);
    assert_eq!(diff(&nested, Source::Committed, None), committed);
    // The top of the repository sees the file outside the folder too.
    assert!(files(&diff(&p, Source::Committed, None)).contains(&"outside.txt"));
    assert!(!files(&diff(&nested, Source::Working, None)).contains(&"outside.txt"));
}

// vcs.test.ts: "uses an explicit diff base without treating upstream or Git
// config as a parent"
#[test]
fn oc_vcs_uses_an_explicit_diff_base_without_treating_upstream_as_a_parent() {
    let (_d, p) = repo();
    write(&p, "file.txt", "base\n");
    commit_all(&p, "initial");
    g(&p, &["checkout", "-qb", "release"]);
    write(&p, "release.txt", "release\n");
    commit_all(&p, "release");
    g(&p, &["checkout", "-qb", "feature"]);
    write(&p, "feature.txt", "feature\n");
    commit_all(&p, "feature");
    g(&p, &["branch", "--set-upstream-to=release", "feature"]);
    g(
        &p,
        &[
            "config",
            "branch.feature.opencode-merge-base",
            "refs/heads/release",
        ],
    );
    no_base(&p);
    assert_eq!(
        files(&diff(&p, Source::Committed, Some("release"))),
        ["feature.txt"]
    );
    assert_eq!(
        files(&diff(&p, Source::Committed, None)),
        ["feature.txt", "release.txt"]
    );
    assert_eq!(
        files(&diff(&p, Source::Branch, Some("release"))),
        ["feature.txt"]
    );
    assert_eq!(
        files(&diff(&p, Source::Branch, None)),
        ["feature.txt", "release.txt"]
    );
    no_base(&p);
}

// vcs.test.ts: "reports missing explicit bases and missing default bases
// without calling them clean"
#[test]
fn oc_vcs_reports_missing_bases_without_calling_them_clean() {
    let (_d, p) = repo();
    write(&p, "file.txt", "base\n");
    commit_all(&p, "initial");
    g(&p, &["branch", "-m", "feature"]);
    no_base(&p);
    assert!(vcs_diff(&p, Source::Committed, None).is_err());
    assert_eq!(
        vcs_diff(&p, Source::Branch, None).unwrap_err(),
        "No review base available"
    );
    assert!(vcs_diff(&p, Source::Branch, Some("missing")).is_err());
    assert_eq!(diff(&p, Source::Working, Some("missing")), []);
}

// vcs.test.ts: "diffs explicit remote-only bases without borrowing another
// fork's branch"
#[test]
fn oc_vcs_diffs_explicit_remote_only_bases_without_borrowing_another_forks() {
    let (_d, p) = repo();
    write(&p, "file.txt", "base\n");
    commit_all(&p, "initial");
    g(
        &p,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/team/project.git",
        ],
    );
    g(
        &p,
        &[
            "remote",
            "add",
            "other",
            "https://github.com/other/project.git",
        ],
    );
    g(&p, &["update-ref", "refs/remotes/other/release", "HEAD"]);
    write(&p, "release.txt", "release parent\n");
    g(&p, &["checkout", "-qb", "feature"]);
    commit_all(&p, "release parent");
    g(&p, &["update-ref", "refs/remotes/origin/release", "HEAD"]);
    write(&p, "feature.txt", "feature\n");
    commit_all(&p, "feature");
    let origin = Some("refs/remotes/origin/release");
    let other = Some("refs/remotes/other/release");
    assert_eq!(files(&diff(&p, Source::Committed, origin)), ["feature.txt"]);
    assert_eq!(
        files(&diff(&p, Source::Committed, None)),
        ["feature.txt", "release.txt"]
    );
    assert_eq!(
        files(&diff(&p, Source::Committed, other)),
        ["feature.txt", "release.txt"]
    );
    g(&p, &["update-ref", "-d", "refs/remotes/origin/release"]);
    assert!(vcs_diff(&p, Source::Committed, origin).is_err());
    assert_eq!(
        files(&diff(&p, Source::Committed, other)),
        ["feature.txt", "release.txt"]
    );
}

// vcs.test.ts: "reviews every feature commit and local changes against
// inferred v2, not the unrelated default"
#[test]
fn oc_vcs_reviews_against_the_inferred_parent_not_the_unrelated_default() {
    let (_d, p) = repo();
    write(&p, "base.txt", "base\n");
    commit_all(&p, "initial");
    g(
        &p,
        &[
            "remote",
            "add",
            "origin",
            "https://example.com/team/repo.git",
        ],
    );
    g(&p, &["update-ref", "refs/remotes/origin/dev", "HEAD"]);
    g(
        &p,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/dev",
        ],
    );
    write(&p, "v2.txt", "v2 parent\n");
    commit_all(&p, "v2");
    g(&p, &["update-ref", "refs/remotes/origin/v2", "HEAD"]);
    g(&p, &["checkout", "-qb", "feature", "origin/v2"]);
    g(
        &p,
        &[
            "config",
            "branch.feature.opencode-merge-base",
            "refs/remotes/origin/dev",
        ],
    );
    for f in ["feature", "second", "third"] {
        write(&p, &format!("{f}.txt"), &format!("{f}\n"));
        commit_all(&p, f);
    }
    write(&p, "untracked.txt", "local\n");
    let config = std::fs::read_to_string(p.join(".git/config")).unwrap();
    let v2 = Base {
        name: "v2".into(),
        reference: "refs/remotes/origin/v2".into(),
    };
    let b = base_of(&p);
    assert_eq!(b, v2);
    let r = Some(b.reference.as_str());
    assert_eq!(
        files(&diff(&p, Source::Committed, r)),
        ["feature.txt", "second.txt", "third.txt"]
    );
    assert_eq!(
        files(&diff(&p, Source::Branch, r)),
        ["feature.txt", "second.txt", "third.txt", "untracked.txt"]
    );
    assert_eq!(
        files(&diff(&p, Source::Committed, None)),
        ["feature.txt", "second.txt", "third.txt", "v2.txt"]
    );
    // Reading the base writes nothing.
    assert_eq!(
        std::fs::read_to_string(p.join(".git/config")).unwrap(),
        config
    );
    assert!(!p.join(".git/opencode-review.json").exists());
    g(
        &p,
        &["config", "branch.feature.opencode-merge-base", "missing"],
    );
    assert_eq!(base_of(&p), v2);
    g(&p, &["checkout", "-qb", "uncertain", "HEAD"]);
    no_base(&p);
}

// vcs.test.ts: "ignores stale, deleted, renamed, and self-mirroring
// creation hints"
#[test]
fn oc_vcs_ignores_stale_deleted_renamed_and_self_mirroring_creation_hints() {
    let (_d, p) = repo();
    write(&p, "base.txt", "base\n");
    commit_all(&p, "initial");
    g(&p, &["branch", "parent"]);
    g(&p, &["checkout", "-qb", "feature", "parent"]);
    assert_eq!(base_of(&p).name, "parent");
    g(&p, &["branch", "-m", "renamed"]);
    no_base(&p);
    g(&p, &["checkout", "-qb", "deleted", "parent"]);
    g(&p, &["branch", "-D", "parent"]);
    no_base(&p);
    g(&p, &["update-ref", "refs/remotes/origin/mirror", "HEAD"]);
    g(&p, &["checkout", "-qb", "mirror", "origin/mirror"]);
    no_base(&p);
    g(&p, &["checkout", "-qb", "stale", "main"]);
    g(&p, &["checkout", "-q", "--orphan", "unrelated"]);
    g(&p, &["commit", "-qm", "unrelated"]);
    g(&p, &["branch", "-f", "main", "HEAD"]);
    g(&p, &["checkout", "-q", "stale"]);
    no_base(&p);
    g(&p, &["checkout", "-qb", "rebased", "unrelated"]);
    g(&p, &["reset", "-q", "--soft", "stale"]);
    no_base(&p);
}

// vcs.test.ts: "creates detached worktrees without review metadata and
// requires an explicit base without a named hint" (adapted: the worktrees
// come from `worktree::create`, CodeSmith's `/worktree`)
#[test]
fn oc_vcs_detached_worktrees_have_no_review_metadata_and_need_an_explicit_base() {
    use codesmith_core::worktree::{create, Create, WorktreeConfig};
    let (d, p) = repo();
    write(&p, "base.txt", "base\n");
    commit_all(&p, "initial");
    g(
        &p,
        &[
            "remote",
            "add",
            "origin",
            "https://example.com/team/repo.git",
        ],
    );
    g(&p, &["update-ref", "refs/remotes/origin/dev", "HEAD"]);
    g(
        &p,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/dev",
        ],
    );
    write(&p, "v2.txt", "v2\n");
    commit_all(&p, "v2");
    g(&p, &["update-ref", "refs/remotes/origin/v2", "HEAD"]);
    let holder = tempfile::tempdir_in(d.path()).unwrap();
    let cfg = WorktreeConfig {
        directory: Some(holder.path().display().to_string()),
        start: None,
    };
    let make = |from: &Path, name: &str, start: Option<&str>| {
        create(
            &cfg,
            from,
            &Create {
                name: Some(name.into()),
                from: start.map(String::from),
                branch: None,
            },
        )
        .unwrap()
        .0
    };
    let git_dir = |w: &Path| {
        let dir = g(w, &["rev-parse", "--absolute-git-dir"]);
        std::path::PathBuf::from(dir)
    };
    let linked = make(&p, "linked", Some("origin/v2"));
    assert_eq!(current_branch(&linked), None, "detached");
    assert!(!git_dir(&linked).join("opencode-review.json").exists());
    no_base(&linked);
    assert_eq!(diff(&linked, Source::Committed, Some("origin/v2")), []);
    assert_eq!(
        files(&diff(&linked, Source::Committed, Some("origin/dev"))),
        ["v2.txt"]
    );
    g(&linked, &["checkout", "-qb", "child"]);
    no_base(&linked);
    g(&linked, &["checkout", "-qb", "named", "origin/v2"]);
    assert_eq!(
        base_of(&linked),
        Base {
            name: "v2".into(),
            reference: "refs/remotes/origin/v2".into()
        }
    );
    assert!(!git_dir(&linked).join("opencode-review.json").exists());
    let from_head = make(&p, "from-head", None);
    assert!(!git_dir(&from_head).join("opencode-review.json").exists());
    let detached = make(&from_head, "detached-source", None);
    assert!(!git_dir(&detached).join("opencode-review.json").exists());
}

// vcs.test.ts: "rejects a branch creation anchor removed by rebase"
#[test]
fn oc_vcs_rejects_a_branch_creation_anchor_removed_by_rebase() {
    let (_d, p) = repo();
    write(&p, "base.txt", "base\n");
    commit_all(&p, "initial");
    g(&p, &["checkout", "-qb", "parent", "main"]);
    write(&p, "parent.txt", "parent\n");
    commit_all(&p, "parent");
    g(&p, &["checkout", "-qb", "feature", "parent"]);
    write(&p, "feature.txt", "feature\n");
    commit_all(&p, "feature");
    assert_eq!(base_of(&p).name, "parent");
    g(&p, &["rebase", "-q", "--onto", "main", "parent", "feature"]);
    no_base(&p);
}

// vcs.test.ts: "does not infer beyond the bounded creation-reflog window"
#[test]
fn oc_vcs_does_not_infer_beyond_the_bounded_creation_reflog_window() {
    let (_d, p) = repo();
    write(&p, "file.txt", "base\n");
    commit_all(&p, "initial");
    g(&p, &["checkout", "-qb", "feature", "main"]);
    write(&p, "file.txt", "feature\n");
    commit_all(&p, "feature");
    assert_eq!(base_of(&p).name, "main");
    let before = g(&p, &["rev-parse", "main"]);
    let after = g(&p, &["rev-parse", "HEAD"]);
    let updates: String = (0..260)
        .map(|i| {
            let to = if i % 2 == 0 { &before } else { &after };
            format!("start\nupdate refs/heads/feature {to}\nprepare\ncommit\n")
        })
        .collect();
    let mut child = Command::new("git")
        .current_dir(&p)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["update-ref", "-m", "test", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(updates.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    no_base(&p);
}

// Regression for the folder scope (vcs.test.ts's nested-path case): a
// session folder whose name has spaces, non-ASCII letters and pathspec
// magic sees only its own files, and so does a symlink to it.
#[test]
fn oc_vcs_scopes_to_an_oddly_named_or_symlinked_folder_literally() {
    let (_d, p) = repo();
    let outside = tempfile::tempdir().unwrap();
    let odd = "dé jà*[x]";
    write(&p, &format!("{odd}/in.txt"), "a\n");
    write(&p, "déjà other/out.txt", "a\n");
    write(&p, "dé jàZ[x]/glob-match.txt", "a\n");
    commit_all(&p, "initial");
    write(&p, &format!("{odd}/in.txt"), "b\n");
    write(&p, "déjà other/out.txt", "b\n");
    write(&p, "dé jàZ[x]/glob-match.txt", "b\n");
    write(&p, &format!("{odd}/new.txt"), "n\n");
    let nested = p.join(odd);
    let want = [format!("{odd}/in.txt"), format!("{odd}/new.txt")];
    assert_eq!(files(&diff(&nested, Source::Working, None)), want);
    let link = outside.path().join("link");
    std::os::unix::fs::symlink(&nested, &link).unwrap();
    assert_eq!(files(&diff(&link, Source::Working, None)), want);
    // The top of the repository sees everything.
    assert_eq!(diff(&p, Source::Working, None).len(), 4);
}
