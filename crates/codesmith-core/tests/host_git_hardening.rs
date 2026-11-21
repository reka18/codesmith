//! CodeSmith's own git (snapshots, worktrees, `/review`, project ids)
//! in a repository whose config and hooks try to run a program: a
//! post-checkout (and every other) hook, `core.fsmonitor`, a clean/smudge
//! /process filter, `diff.external`, a textconv driver, a pager, an
//! `includeIf`-included file and the same planted in a worktree config.
//! Every operation still works, and no marker file is ever written.
//!
//! The whole binary also runs with `GIT_DIR`, `GIT_WORK_TREE` and
//! `GIT_INDEX_FILE` pointing at a decoy repository (as inside a git hook):
//! CodeSmith's git must act on the repository it was given and leave the
//! decoy untouched.
//!
//! Nothing here reads the user's own config: HOME, XDG_CONFIG_HOME and
//! git's global config point into a temporary folder.

#![cfg(unix)]

use codesmith_core::snapshot::Snapshots;
use codesmith_core::worktree;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    decoy: PathBuf,
}

/// Raw git for building fixtures, never affected by the decoy variables.
fn fixture_git(dir: &Path, args: &[&str]) -> String {
    let mut c = Command::new("git"); // git-guard: test fixture
    for v in codesmith_core::git::LOCAL_ENV_VARS {
        c.env_remove(v);
    }
    let out = c
        .current_dir(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn have_git() -> bool {
    Command::new("git") // git-guard: test fixture
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn marker(w: &World) -> PathBuf {
    w.root.parent().unwrap().join("MARKER")
}

fn world() -> &'static World {
    static W: OnceLock<World> = OnceLock::new();
    W.get_or_init(|| {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let home = base.join("home");
        std::fs::create_dir_all(home.join(".config")).unwrap();
        let global = base.join("gitconfig");
        std::fs::write(
            &global,
            "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        // SAFETY: set once, before any test thread runs git.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
            std::env::set_var("XDG_DATA_HOME", home.join(".local/share"));
            std::env::set_var("GIT_CONFIG_GLOBAL", &global);
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }

        // The decoy, which no operation may touch.
        let decoy = base.join("decoy");
        std::fs::create_dir_all(&decoy).unwrap();
        fixture_git(&decoy, &["init", "-q", "-b", "main"]);
        std::fs::write(decoy.join("d.txt"), "decoy\n").unwrap();
        fixture_git(&decoy, &["add", "."]);
        fixture_git(&decoy, &["commit", "-qm", "decoy"]);

        let root = base.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        fixture_git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join(".gitattributes"), "* filter=evil diff=evil\n").unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        fixture_git(&root, &["add", "."]);
        fixture_git(&root, &["commit", "-qm", "first"]);

        // The planted program, and everything that would run it.
        let mark = base.join("MARKER");
        let evil = base.join("evil.sh");
        std::fs::write(
            &evil,
            format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\ncat\n", mark.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = evil.display();
        let hooks = root.join(".git/hooks");
        for h in [
            "post-checkout",
            "pre-commit",
            "post-commit",
            "post-index-change",
            "reference-transaction",
            "pre-auto-gc",
            "post-rewrite",
            "fsmonitor-watchman",
        ] {
            std::fs::copy(&evil, hooks.join(h)).unwrap();
        }
        let included = base.join("included.cfg");
        std::fs::write(
            &included,
            format!("[core]\n\tfsmonitor = {e}\n[diff]\n\texternal = {e}\n"),
        )
        .unwrap();
        let mut cfg = std::fs::read_to_string(root.join(".git/config")).unwrap();
        cfg.push_str(&format!(
            "[core]\n\tfsmonitor = {e}\n\tpager = {e}\n\tsshCommand = {e}\n\taskPass = {e}\n\
             [diff]\n\texternal = {e}\n\
             [diff \"evil\"]\n\ttextconv = {e}\n\tcommand = {e}\n\
             [filter \"evil\"]\n\tclean = {e}\n\tsmudge = {e}\n\tprocess = {e}\n\trequired = true\n\
             [pager]\n\tdiff = {e}\n\tstatus = {e}\n\
             [credential]\n\thelper = !{e}\n\
             [includeIf \"gitdir:/\"]\n\tpath = {}\n",
            included.display()
        ));
        std::fs::write(root.join(".git/config"), cfg).unwrap();

        // Inherited repository-location variables, as inside a hook.
        // SAFETY: as above.
        unsafe {
            std::env::set_var("GIT_DIR", decoy.join(".git"));
            std::env::set_var("GIT_WORK_TREE", &decoy);
            std::env::set_var("GIT_INDEX_FILE", decoy.join(".git/index"));
        }
        World {
            _tmp: tmp,
            root,
            decoy,
        }
    })
}

fn decoy_state(w: &World) -> (String, String, Vec<u8>) {
    (
        std::fs::read_to_string(w.decoy.join(".git/config")).unwrap(),
        fixture_git(&w.decoy, &["status", "--porcelain"]),
        std::fs::read(w.decoy.join(".git/index")).unwrap(),
    )
}

fn assert_clean(w: &World, what: &str) {
    let m = marker(w);
    assert!(
        !m.exists(),
        "{what} ran a planted program: {}",
        std::fs::read_to_string(&m).unwrap_or_default()
    );
}

/// One test, so the operations run one after another in the one repo.
#[test]
fn no_planted_program_runs_and_the_decoy_is_untouched() {
    if !have_git() {
        eprintln!("skipped: git is not installed");
        return;
    }
    let w = world();
    let decoy = decoy_state(w);
    let root = &w.root;

    // Status and the project id.
    std::fs::write(root.join("a.txt"), "two\n").unwrap();
    let status = codesmith_core::review::target::git(root, &["status", "--porcelain"]).unwrap();
    assert!(status.contains("a.txt"), "{status}");
    assert_clean(w, "git status");
    let id = worktree::project_id(root);
    let first = fixture_git(root, &["rev-list", "--max-parents=0", "HEAD"]);
    assert!(first.starts_with(&id), "{id} vs {first}");
    assert_clean(w, "the project id");

    // The review diff of uncommitted changes.
    let t = codesmith_core::review::target::resolve(root, None).unwrap();
    assert!(
        t.diff.contains("-one") && t.diff.contains("+two"),
        "{}",
        t.diff
    );
    assert_clean(w, "the review diff");

    // Snapshot capture, diff and restore; then the same with a hook and
    // a filter planted in the snapshot repository itself.
    let snaps_base = root.parent().unwrap().join("snaps");
    let s = Snapshots::new(root, &snaps_base).unwrap();
    let before = s.capture().unwrap();
    std::fs::write(root.join("a.txt"), "three\n").unwrap();
    let after = s.capture().unwrap();
    assert!(s.diff(&before, &after).unwrap().contains("+three"));
    s.restore(&before).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "two\n"
    );
    assert_clean(w, "a snapshot");
    let snap_git = std::fs::read_dir(&snaps_base)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let evil = root.parent().unwrap().join("evil.sh");
    std::fs::create_dir_all(snap_git.join("hooks")).unwrap();
    std::fs::copy(&evil, snap_git.join("hooks/post-checkout")).unwrap();
    let mut cfg = std::fs::read_to_string(snap_git.join("config")).unwrap();
    cfg.push_str(&format!(
        "[filter \"evil\"]\n\tclean = {e}\n\tsmudge = {e}\n[core]\n\tfsmonitor = {e}\n",
        e = evil.display()
    ));
    std::fs::write(snap_git.join("config"), cfg).unwrap();
    std::fs::write(root.join("a.txt"), "four\n").unwrap();
    let later = s.capture().unwrap();
    s.restore(&after).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "three\n"
    );
    s.restore(&later).unwrap();
    assert_clean(w, "a snapshot with a planted snapshot config");

    // Worktree create, list and remove.
    let wt_base = root.parent().unwrap().join("worktrees");
    let cfg = worktree::WorktreeConfig {
        directory: Some(wt_base.display().to_string()),
        start: None,
    };
    let (wt, _) = worktree::create(
        &cfg,
        root,
        &worktree::Create {
            name: Some("hostile".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(wt.join("a.txt").is_file());
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "one\n",
        "checked out without the smudge filter"
    );
    assert_clean(w, "creating a worktree");
    // A worktree-scoped config planted too.
    fixture_git(root, &["config", "extensions.worktreeConfig", "true"]);
    std::fs::write(
        root.join(".git/worktrees/hostile/config.worktree"),
        format!("[core]\n\tfsmonitor = {}\n", evil.display()),
    )
    .unwrap();
    let listed = worktree::list(root).unwrap();
    assert_eq!(listed.len(), 2);
    worktree::remove(&cfg, root, "hostile", true).unwrap();
    assert!(!wt.exists());
    assert_clean(w, "removing a worktree");

    // The decoy is as it was, and the project's own index is what the
    // operations used.
    assert_eq!(decoy_state(w), decoy, "the decoy repository was touched");
    assert_eq!(
        std::fs::read_to_string(w.decoy.join("d.txt")).unwrap(),
        "decoy\n"
    );
}
