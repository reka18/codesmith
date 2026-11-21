//! CodeSmith's own Mercurial (the project id, the named branch, status
//! and the working copy and branch reviews) in a repository whose
//! `.hg/hgrc` tries to run a program: hooks for every command CodeSmith
//! runs, an extension loaded from a Python file (by absolute and by
//! repository-relative path), a pager, a merge tool, an ssh command, an
//! alias and an `%include`d file with another hook. The parent's
//! `HGRCPATH` names a planted file too, and the user's own `~/.hgrc` has
//! a hook. Every operation still works, and no marker is ever written.
//!
//! A real Mercurial runs this; without one it is skipped. Nothing here
//! reads the user's own config: HOME and XDG_CONFIG_HOME point into a
//! temporary folder.

#![cfg(unix)]

use codesmith_core::vcs::{self, Mode};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The Mercurial to test with: on `PATH`, else Homebrew's.
fn find_hg() -> Option<PathBuf> {
    let works = |p: &Path| {
        Command::new(p) // hg-guard: test fixture
            .arg("--version")
            .env("HGRCPATH", "")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("hg"))
        .chain([PathBuf::from("/opt/homebrew/bin/hg")])
        .find(|p| p.is_file() && works(p))
}

/// Raw Mercurial for building fixtures, with no config at all.
fn fixture_hg(hg: &Path, dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(hg) // hg-guard: test fixture
        .args(args)
        .current_dir(dir)
        .env("HGPLAIN", "1")
        .env("HGRCPATH", "")
        .env_remove("HGRCSKIPREPO")
        .output()
        .unwrap()
}

fn ok(hg: &Path, dir: &Path, args: &[&str]) {
    let o = fixture_hg(hg, dir, args);
    assert!(
        o.status.success(),
        "hg {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn commit_all(hg: &Path, dir: &Path, message: &str) {
    ok(hg, dir, &["addremove", "-q"]);
    ok(hg, dir, &["commit", "-q", "-m", message, "-u", "test"]);
}

fn assert_clean(marker: &Path, what: &str) {
    assert!(
        !marker.exists(),
        "{what} ran a planted program: {}",
        std::fs::read_to_string(marker).unwrap_or_default()
    );
}

/// One test: it sets HOME and PATH for the whole binary.
#[test]
fn no_planted_mercurial_config_runs() {
    let Some(hg) = find_hg() else {
        eprintln!("skipped: hg is not installed");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(home.join(".config")).unwrap();
    let marker = base.join("MARKER");
    let m = marker.display().to_string();
    let path = format!(
        "{}:{}",
        hg.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // SAFETY: the only test in this binary, before it runs anything.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    }

    // A project on a named branch, with a modified, a missing and an
    // untracked file.
    let root = base.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    ok(&hg, &root, &["init"]);
    std::fs::write(root.join("keep.txt"), "one\ntwo\n").unwrap();
    std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
    commit_all(&hg, &root, "initial");
    ok(&hg, &root, &["branch", "-q", "feature"]);
    std::fs::write(root.join("keep.txt"), "one\nthree\n").unwrap();
    commit_all(&hg, &root, "feature change");

    // The plants. Each writes its name to the marker.
    let hook = |name: &str| format!("echo {name} >> '{m}'");
    let evil_ext = format!("open({m:?}, 'a').write('extension\\n')\n");
    std::fs::write(base.join("evil_ext.py"), &evil_ext).unwrap();
    std::fs::write(root.join(".hg/evil_rel.py"), &evil_ext).unwrap();
    let evil_sh = base.join("evil.sh");
    std::fs::write(&evil_sh, format!("#!/bin/sh\necho program >> '{m}'\ncat\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&evil_sh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = evil_sh.display();
    std::fs::write(
        base.join("included.rc"),
        format!(
            "[hooks]\npost-status = {}\npre-log.inc = {}\n",
            hook("included-hook"),
            hook("included-log")
        ),
    )
    .unwrap();
    let mut hooks = String::from("[hooks]\n");
    for cmd in [
        "status", "diff", "log", "cat", "branch", "version", "config",
    ] {
        hooks.push_str(&format!("pre-{cmd} = {}\n", hook(&format!("pre-{cmd}"))));
        hooks.push_str(&format!("post-{cmd} = {}\n", hook(&format!("post-{cmd}"))));
    }
    std::fs::write(
        root.join(".hg/hgrc"),
        format!(
            "%include {inc}\n{hooks}\
             [extensions]\nevil = {abs}\nevilrel = .hg/evil_rel.py\n\
             [ui]\npaginate = true\nmerge = {e}\nssh = {e}\neditor = {e}\n\
             [pager]\npager = {e}\nattend-status = true\nattend-diff = true\n\
             [merge-tools]\nevil.executable = {e}\n\
             [alias]\nbranch = !{e}\n",
            inc = base.join("included.rc").display(),
            abs = base.join("evil_ext.py").display(),
        ),
    )
    .unwrap();

    // Mercurial runs these plants when asked plainly, as CodeSmith used
    // to: the regression this guards against.
    let o = fixture_hg(&hg, &root, &["status"]);
    assert!(o.status.success());
    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        ran.contains("pre-status") && ran.contains("extension"),
        "the planted config should run under plain hg: {ran:?}"
    );
    std::fs::remove_file(&marker).unwrap();

    // The user's own hook, and an inherited HGRCPATH naming a planted file.
    std::fs::write(
        home.join(".hgrc"),
        format!(
            "[ui]\nusername = t\n[hooks]\npre-status = {}\n",
            hook("user-hook")
        ),
    )
    .unwrap();
    std::fs::write(
        base.join("planted.rc"),
        format!("[hooks]\npre-status = {}\n", hook("inherited-hgrcpath")),
    )
    .unwrap();
    // SAFETY: as above.
    unsafe { std::env::set_var("HGRCPATH", base.join("planted.rc")) };

    // The working copy's changes.
    std::fs::write(root.join("keep.txt"), "one\nfour\n").unwrap();
    std::fs::remove_file(root.join("gone.txt")).unwrap();
    std::fs::write(root.join("new.txt"), "hello\n").unwrap();

    let p = vcs::resolve(&root);
    assert_eq!(p.vcs, Some(vcs::Kind::Hg));
    assert_ne!(p.id, vcs::GLOBAL, "the root changeset is read");
    assert_eq!(p.id.len(), 40, "{}", p.id);
    assert_clean(&marker, "the project id");

    assert_eq!(vcs::hg_branch(&root).as_deref(), Some("feature"));
    assert_clean(&marker, "the branch");

    let status: Vec<(String, &str)> = vcs::hg_status(&root)
        .into_iter()
        .map(|f| (f.file, f.status))
        .collect();
    assert_eq!(
        status,
        [
            ("gone.txt".to_string(), "deleted"),
            ("keep.txt".to_string(), "modified"),
            ("new.txt".to_string(), "added"),
        ]
    );
    assert_clean(&marker, "status");

    let working = vcs::hg_diff(&root, Mode::Working, None).unwrap();
    let patch = |files: &[vcs::FileDiff], f: &str| {
        files
            .iter()
            .find(|d| d.file == f)
            .map(|d| d.patch.clone())
            .unwrap_or_default()
    };
    assert!(patch(&working, "keep.txt").contains("+four"));
    assert!(patch(&working, "gone.txt").contains("-bye"), "hg cat ran");
    assert!(patch(&working, "new.txt").contains("+hello"));
    assert_clean(&marker, "the working copy diff");

    let branch = vcs::hg_diff(&root, Mode::Branch, None).unwrap();
    assert!(patch(&branch, "keep.txt").contains("+four"), "{branch:?}");
    assert_clean(&marker, "the branch diff");

    let t = codesmith_core::review::target::resolve(&root, None).unwrap();
    assert!(t.diff.contains("+four"), "{}", t.diff);
    assert_clean(&marker, "the review");
}
