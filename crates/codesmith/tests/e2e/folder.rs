//! Starting outside a repository: a warning, a question at a terminal, a
//! remembered yes for a loose folder, and a refusal in the home folder or
//! `/` with nobody to ask (a tailbus bridge's run in the home folder would
//! otherwise snapshot all of it).

use crate::harness::{Env, Out, BIN};
use crate::model::{Model, Reply, Script, M};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn world() -> (Env, Model) {
    let e = Env::new();
    let m = Model::start(Script::new().always("hi", M::prompt("hi"), Reply::text("Hello."))).await;
    e.config(&m.url, json!({}));
    (e, m)
}

fn loose(e: &Env) -> PathBuf {
    let d = e.root.join("loose");
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn run_in(e: &Env, dir: &Path, args: &[&str], vars: &[(&str, &str)]) -> Out {
    let mut c = e.command_in(dir, args);
    c.envs(vars.iter().copied());
    crate::harness::finish(c.spawn().unwrap()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repo_and_its_subfolders_start_without_a_word() {
    let (e, _m) = world().await;
    std::fs::create_dir_all(e.project.join("src/deep")).unwrap();
    for dir in [e.project.clone(), e.project.join("src/deep")] {
        let out = run_in(&e, &dir, &["run", "hi"], &[]).await;
        out.ok();
        assert!(!out.stderr.contains("repository"), "{}", out.both());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_terminal_a_loose_folder_warns_and_runs() {
    let (e, m) = world().await;
    let out = run_in(&e, &loose(&e), &["run", "hi"], &[]).await;
    out.ok();
    assert!(out.stdout.contains("Hello."), "{}", out.both());
    assert!(
        out.stderr.contains("isn't in a git repository"),
        "{}",
        out.both()
    );
    assert_eq!(m.steps().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_terminal_the_home_folder_and_root_are_refused() {
    let (e, m) = world().await;
    for dir in [e.home.clone(), PathBuf::from("/")] {
        let started = Instant::now();
        let out = run_in(&e, &dir, &["run", "hi"], &[]).await;
        assert_ne!(out.code, Some(0), "{}", out.both());
        assert!(out.stderr.contains("--any-folder"), "{}", out.both());
        assert!(
            out.stderr.contains("home folder") || out.stderr.contains("root folder"),
            "{}",
            out.both()
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }
    assert!(m.steps().is_empty(), "the model was called");
}

/// The bug a bridge hit: its `HOME` is its own folder, and the run starts
/// in the account's real home. That's still the home folder.
#[tokio::test(flavor = "multi_thread")]
async fn the_accounts_home_is_home_whatever_home_says() {
    let Some(acct) = codesmith::core::paths::account_home() else {
        eprintln!("skipped: no account home in the user database");
        return;
    };
    let (e, m) = world().await;
    let started = Instant::now();
    // The world's `HOME` is its own folder, as a bridge's is.
    let out = run_in(&e, &acct, &["run", "hi"], &[]).await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert!(out.stderr.contains("home folder"), "{}", out.both());
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(m.steps().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn any_folder_starts_in_the_home_folder() {
    let (e, m) = world().await;
    let out = run_in(&e, &e.home, &["run", "--any-folder", "hi"], &[]).await;
    out.ok();
    assert!(out.stdout.contains("Hello."), "{}", out.both());
    let out = run_in(
        &e,
        &e.home,
        &["run", "hi"],
        &[("CODESMITH_ANY_FOLDER", "1")],
    )
    .await;
    out.ok();
    // `0` is off.
    let out = run_in(
        &e,
        &e.home,
        &["run", "hi"],
        &[("CODESMITH_ANY_FOLDER", "0")],
    )
    .await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert_eq!(m.steps().len(), 2);
    // Still no snapshot of the home folder.
    let snaps = e.data.join("codesmith/snapshot");
    let n = std::fs::read_dir(&snaps).map(|d| d.count()).unwrap_or(0);
    assert_eq!(n, 0, "the home folder was snapshotted");
}

#[tokio::test(flavor = "multi_thread")]
async fn your_config_can_turn_the_loose_folder_warning_off_but_a_folders_cannot() {
    let (e, m) = world().await;
    let dir = loose(&e);
    // The folder's own config can't silence the warning about itself.
    std::fs::write(dir.join("codesmith.json"), r#"{"repo_warning": false}"#).unwrap();
    let out = run_in(&e, &dir, &["run", "hi"], &[]).await;
    out.ok();
    assert!(
        out.stderr.contains("isn't in a git repository"),
        "{}",
        out.both()
    );
    std::fs::remove_file(dir.join("codesmith.json")).unwrap();

    // Your own config can.
    let mut global: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(e.config_dir.join("codesmith.json")).unwrap(),
    )
    .unwrap();
    global["repo_warning"] = json!(false);
    std::fs::write(e.config_dir.join("codesmith.json"), global.to_string()).unwrap();
    let out = run_in(&e, &dir, &["run", "hi"], &[]).await;
    out.ok();
    assert!(!out.stderr.contains("repository"), "{}", out.both());
    // The home folder is still refused.
    let out = run_in(&e, &e.home, &["run", "hi"], &[]).await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert_eq!(m.steps().len(), 2);
}

/// `args` at a terminal (through `script(1)`) in `dir`, with `input`
/// piped on stdin first if given, answering `answer` when asked: the
/// terminal's output, or `None` without `script(1)`.
async fn at_terminal(e: &Env, dir: &Path, args: &str, answer: Option<&str>) -> Option<String> {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: script(1) is not installed");
        return None;
    }
    let inner = format!("stty rows 30 cols 200; exec '{BIN}' {args}");
    let sargs: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["-q", "/dev/null", "sh", "-c", &inner]
    } else {
        vec!["-qfec", &inner, "/dev/null"]
    };
    let mut cmd = tokio::process::Command::new("/usr/bin/script");
    cmd.args(&sargs)
        .current_dir(dir)
        .env_clear()
        .envs(e.vars())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut screen = String::new();
    let mut buf = [0u8; 8192];
    let mut answered = false;
    let _ = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let n = stdout.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            screen.push_str(&String::from_utf8_lossy(&buf[..n]));
            if !answered && screen.contains("[y/N]") {
                answered = true;
                match answer {
                    Some(a) => {
                        stdin.write_all(a.as_bytes()).await.unwrap();
                        stdin.flush().await.unwrap();
                    }
                    None => break,
                }
            }
        }
    })
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
    Some(screen)
}

#[tokio::test(flavor = "multi_thread")]
async fn at_a_terminal_a_loose_folder_is_asked_once() {
    let (e, m) = world().await;
    let dir = loose(&e);
    let Some(screen) = at_terminal(&e, &dir, "run hi", Some("n\n")).await else {
        return;
    };
    assert!(screen.contains("isn't in a git repository"), "{screen}");
    assert!(screen.contains("not started"), "{screen}");
    assert!(m.steps().is_empty());

    let screen = at_terminal(&e, &dir, "run hi", Some("y\n")).await.unwrap();
    assert!(screen.contains("Remembered"), "{screen}");
    assert!(screen.contains("Hello."), "{screen}");

    // Remembered: not asked again.
    let screen = at_terminal(&e, &dir, "run hi", None).await.unwrap();
    assert!(!screen.contains("[y/N]"), "asked again:\n{screen}");
    assert!(screen.contains("Hello."), "{screen}");
    let kept = std::fs::read_to_string(e.home.join(".state/codesmith/folders.json")).unwrap();
    assert!(kept.contains(&dir.display().to_string()), "{kept}");
}

#[tokio::test(flavor = "multi_thread")]
async fn at_a_terminal_the_home_folder_is_asked_every_time() {
    let (e, m) = world().await;
    let Some(screen) = at_terminal(&e, &e.home, "run hi", Some("y\n")).await else {
        return;
    };
    assert!(screen.contains("is your home folder"), "{screen}");
    assert!(screen.contains("Hello."), "{screen}");
    assert!(!screen.contains("Remembered"), "{screen}");
    let screen = at_terminal(&e, &e.home, "run hi", Some("y\n"))
        .await
        .unwrap();
    assert!(screen.contains("[y/N]"), "not asked again:\n{screen}");
    assert_eq!(m.steps().len(), 2);
    assert!(!e.home.join(".state/codesmith/folders.json").exists());
}

/// A message piped on stdin, as in `echo hi | codesmith run`: the
/// question still comes on the terminal.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_message_piped_the_question_still_comes() {
    let (e, m) = world().await;
    let Some(screen) = at_terminal(&e, &e.home, "run hi < /dev/null", Some("n\n")).await else {
        return;
    };
    assert!(screen.contains("[y/N]"), "{screen}");
    assert!(screen.contains("not started"), "{screen}");
    assert!(m.steps().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ui_asks_before_it_opens() {
    let (e, _m) = world().await;
    let dir = loose(&e);
    let Some(screen) = at_terminal(&e, &dir, "", Some("n\n")).await else {
        return;
    };
    assert!(screen.contains("isn't in a git repository"), "{screen}");
    assert!(screen.contains("not started"), "{screen}");
}
