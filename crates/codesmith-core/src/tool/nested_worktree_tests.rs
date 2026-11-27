//! Worktrees nested in the project at `.worktrees/<name>` (CodeSmith's
//! default): from a session at the repository root their files are inside
//! the project for every tool, searches from the root leave them out, and
//! their `.git` file trips no check.

use super::testing::Env;
use super::*;
use crate::permission::{base_rules, Effect, Rule};
use serde_json::json;

fn rules() -> Vec<Rule> {
    let mut r = base_rules();
    // Outside the project asks; the recorder sees every such request.
    r.push(Rule::new("external_directory", "*", Effect::Ask));
    r
}

fn git(dir: &Path, args: &[&str]) {
    let out = crate::git::fixture()
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn have_git() -> bool {
    crate::git::fixture()
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A repository at the env's root with a nested worktree `.worktrees/w`.
fn nested(env: &Env) -> PathBuf {
    let root = env.root();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "maintenance.auto", "false"]);
    git(&root, &["config", "gc.auto", "0"]);
    env.write("src/lib.txt", "needle in main\n");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "first"]);
    let (wt, _) = crate::worktree::create(
        &crate::worktree::WorktreeConfig::default(),
        &root,
        &crate::worktree::Create {
            name: Some("w".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(wt, root.join(".worktrees/w"));
    std::fs::write(wt.join("src/lib.txt"), "needle in worktree\n").unwrap();
    wt
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nested_worktree_is_inside_the_project_for_every_file_tool() {
    if !have_git() {
        return;
    }
    let (env, rec) = Env::recording(rules());
    let wt = nested(&env);
    let ctx = env.ctx();
    for p in [
        ".worktrees/w/src/lib.txt",
        ".worktrees/w/.git",
        ".worktrees/w",
    ] {
        let t = ctx.resolve(p);
        assert!(!t.external, "{p}");
        assert_eq!(t.resource, p);
    }
    let abs = wt.join("src/lib.txt").display().to_string();
    assert!(!ctx.resolve(&abs).external);
    let out = Tool::Read
        .run(&ctx, &json!({"path": ".worktrees/w/src/lib.txt"}))
        .await
        .unwrap();
    assert!(out.content.contains("needle in worktree"));
    // Its `.git` file is an ordinary file to the tools: no guard, no
    // outside path.
    let out = Tool::Read
        .run(&ctx, &json!({"path": ".worktrees/w/.git"}))
        .await
        .unwrap();
    assert!(out.content.contains("gitdir:"));
    Tool::Edit
        .run(
            &ctx,
            &json!({"path": ".worktrees/w/src/lib.txt", "oldString": "needle in worktree", "newString": "edited"}),
        )
        .await
        .unwrap();
    Tool::Write
        .run(
            &ctx,
            &json!({"path": abs.replace("lib.txt", "new.txt"), "content": "written"}),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(wt.join("src/lib.txt")).unwrap(),
        "edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("src/new.txt")).unwrap(),
        "written"
    );
    assert!(
        rec.actions().is_empty(),
        "nothing asked, external_directory least of all: {:?}",
        rec.requests()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_shell_can_cd_and_write_into_a_nested_worktree_without_asking() {
    if !have_git() {
        return;
    }
    let (env, rec) = Env::recording(rules());
    let wt = nested(&env);
    let ctx = env.ctx();
    for command in [
        "cd .worktrees/w && echo one > from-cd.txt",
        "echo two > .worktrees/w/redirected.txt",
        "cd .worktrees/w/src && cat lib.txt > copied.txt",
        // `cd`, not `pushd`: `sh` is dash on Linux, which has no `pushd`.
        "cd .worktrees/w && git status --short > status.txt",
    ] {
        let r = Tool::Shell.run(&ctx, &json!({"command": command})).await;
        assert!(r.is_ok(), "{command}: {r:?}");
    }
    let r = Tool::Shell
        .run(
            &ctx,
            &json!({"command": "echo three > ok.txt", "workdir": ".worktrees/w"}),
        )
        .await;
    assert!(r.is_ok(), "{r:?}");
    for f in [
        "from-cd.txt",
        "redirected.txt",
        "src/copied.txt",
        "status.txt",
        "ok.txt",
    ] {
        assert!(wt.join(f).exists(), "{f}");
    }
    assert!(
        !rec.actions().iter().any(|a| a == "external_directory"),
        "{:?}",
        rec.requests()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_the_project_still_asks_beside_a_nested_worktree() {
    if !have_git() {
        return;
    }
    // Negative: the nested worktree widens nothing. A sibling of the
    // repository is outside it, and refusing the ask stops the write.
    let (env, rec) = Env::recording(rules());
    nested(&env);
    rec.deny_action("external_directory");
    let ctx = env.ctx();
    let sibling = tempfile::tempdir().unwrap();
    let target = sibling.path().canonicalize().unwrap().join("x.txt");
    let e = Tool::Write
        .run(
            &ctx,
            &json!({"path": target.display().to_string(), "content": "x"}),
        )
        .await
        .unwrap_err();
    assert!(e.contains("declined"), "{e}");
    assert!(!target.exists());
    let e = Tool::Shell
        .run(
            &ctx,
            &json!({"command": format!("echo x > {}", target.display())}),
        )
        .await
        .unwrap_err();
    assert!(!e.is_empty());
    assert!(!target.exists());
    assert!(rec.actions().iter().all(|a| a == "external_directory"));
}

#[tokio::test(flavor = "multi_thread")]
async fn grep_and_glob_from_the_main_checkout_leave_nested_worktrees_out() {
    if !have_git() {
        return;
    }
    let (env, _rec) = Env::recording(rules());
    let wt = nested(&env);
    let ctx = env.ctx();
    let out = Tool::Grep
        .run(&ctx, &json!({"pattern": "needle"}))
        .await
        .unwrap()
        .content;
    assert!(
        out.contains("src/lib.txt") && !out.contains(".worktrees"),
        "{out}"
    );
    for input in [
        json!({"pattern": "**/*.txt"}),
        json!({"pattern": "**/*.txt", "hidden": true}),
        json!({"pattern": "**/lib.txt", "hidden": true}),
    ] {
        let out = Tool::Glob.run(&ctx, &input).await.unwrap().content;
        assert!(
            out.contains("lib.txt") && !out.contains(".worktrees"),
            "{input}: {out}"
        );
    }
    // Searching the worktree itself still finds its files.
    let out = Tool::Grep
        .run(&ctx, &json!({"pattern": "needle", "path": ".worktrees/w"}))
        .await
        .unwrap()
        .content;
    assert!(out.contains("lib.txt"), "{out}");
    let out = Tool::Glob
        .run(
            &ctx,
            &json!({"pattern": "**/*.txt", "path": wt.display().to_string()}),
        )
        .await
        .unwrap()
        .content;
    assert!(out.contains("lib.txt"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_inside_a_nested_worktree_has_that_worktree_as_its_project() {
    if !have_git() {
        return;
    }
    let env = Env::new();
    let wt = nested(&env);
    let found = crate::vcs::marker(&wt.join("src")).map(|(r, _)| r);
    assert_eq!(found.as_deref(), Some(wt.as_path()));
    let found = crate::vcs::marker(&env.root().join("src")).map(|(r, _)| r);
    assert_eq!(found, Some(env.root()));
}
