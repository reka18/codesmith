//! The interactive UI in a pseudo-terminal, through `script(1)` (no pty
//! crate needed): `--prompt` sends a message as the UI opens, the scripted
//! answer is drawn, the turn is stored, and Ctrl+D on the empty prompt quits.
//! Skipped where `script` is missing; the UI's own tests cover its parts.

use crate::harness::{user_texts, Env, BIN};
use crate::model::{Model, Reply, Script, M};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread")]
async fn the_ui_answers_a_prompt_and_quits() {
    if !std::path::Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: script(1) is not installed");
        return;
    }
    let e = Env::new();
    let m = Model::start(Script::new().titled("Greeting").then(
        "hello",
        M::tools().and(M::prompt("say hello")),
        Reply::text("Hello from the scripted model."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let inner = format!("stty rows 30 cols 100; exec '{BIN}' --prompt 'say hello'");
    let args: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["-q", "/dev/null", "sh", "-c", &inner]
    } else {
        vec!["-qfec", &inner, "/dev/null"]
    };
    let mut cmd = tokio::process::Command::new("/usr/bin/script");
    cmd.args(&args)
        .current_dir(&e.project)
        .env_clear()
        .envs(e.vars())
        .env("TERM", "xterm-256color")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut screen = String::new();
    let mut buf = [0u8; 8192];
    let seen = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let n = stdout.read(&mut buf).await.unwrap();
            if n == 0 {
                return false;
            }
            screen.push_str(&String::from_utf8_lossy(&buf[..n]));
            // Settled: the answer is drawn and the turn's usage is shown.
            let text = plain(&screen);
            if text.contains("Hello from the scripted model.") && text.contains("110 tokens") {
                return true;
            }
        }
    })
    .await;
    assert_eq!(
        seen,
        Ok(true),
        "the answer was not drawn:\n{}",
        plain(&screen)
    );
    assert!(plain(&screen).contains("› say hello"));
    // Quit with Ctrl+D on the empty prompt.
    stdin.write_all(b"\x04").await.unwrap();
    stdin.flush().await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("the UI did not quit")
        .unwrap();
    assert!(status.success(), "{status:?}");
    let list = e.store().list(None, false).unwrap();
    assert_eq!(list.len(), 1, "the session was saved");
    assert_eq!(user_texts(&e.history(&list[0].id)), ["say hello"]);
    m.assert_done();
}

/// The screen's text: escape sequences become spaces, runs of spaces one.
fn plain(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() || d == '~' {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The UI running in a pseudo-terminal, read and typed into.
struct Ui {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    screen: String,
}

impl Ui {
    /// The UI with `args`, or `None` where `script(1)` is missing.
    fn start(e: &Env, args: &str) -> Option<Ui> {
        if !std::path::Path::new("/usr/bin/script").exists() {
            eprintln!("skipped: script(1) is not installed");
            return None;
        }
        let inner = format!("stty rows 30 cols 100; exec '{BIN}' {args}");
        let script_args: Vec<&str> = if cfg!(target_os = "macos") {
            vec!["-q", "/dev/null", "sh", "-c", &inner]
        } else {
            vec!["-qfec", &inner, "/dev/null"]
        };
        let mut cmd = tokio::process::Command::new("/usr/bin/script");
        cmd.args(&script_args)
            .current_dir(&e.project)
            .env_clear()
            .envs(e.vars())
            .env("TERM", "xterm-256color")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().unwrap();
        Some(Ui {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            child,
            screen: String::new(),
        })
    }

    /// Read output until `text` is drawn (after what was read before).
    async fn wait_for(&mut self, text: &str) {
        let mut buf = [0u8; 8192];
        let mut fresh = String::new();
        let seen = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if plain(&fresh).contains(text) {
                    return true;
                }
                let n = self.stdout.read(&mut buf).await.unwrap();
                if n == 0 {
                    return false;
                }
                fresh.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        })
        .await;
        self.screen.push_str(&fresh);
        assert_eq!(seen, Ok(true), "{text:?} was not drawn:\n{}", plain(&fresh));
    }

    async fn send(&mut self, bytes: &[u8]) {
        self.stdin.write_all(bytes).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Quit with Ctrl+D on the empty prompt.
    async fn quit(mut self) {
        self.send(b"\x04").await;
        let status = tokio::time::timeout(Duration::from_secs(20), self.child.wait())
            .await
            .expect("the UI did not quit")
            .unwrap();
        assert!(status.success(), "{status:?}");
    }
}

// Interrupting (Ctrl+C) while the turn waits for a permission answer: the
// session reloads only once the turn has stopped and recorded the declined
// call, so the next prompt does not answer that call a second time.
#[tokio::test(flavor = "multi_thread")]
async fn interrupting_a_permission_prompt_answers_the_call_once() {
    let e = Env::new();
    e.write("victim.txt", "keep me\n");
    let m = Model::start(
        Script::new()
            .titled("T")
            .always(
                "delete",
                M::tools().and(M::prompt("delete the victim")),
                Reply::call(
                    "shell",
                    json!({"command": "rm victim.txt", "description": "Delete"}),
                ),
            )
            .always(
                "next",
                M::tools().and(M::prompt("carry on")),
                Reply::text("Carried on fine."),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}),
    );
    for round in 1..=3 {
        let Some(mut ui) = Ui::start(&e, "--prompt 'delete the victim'") else {
            return;
        };
        ui.wait_for("Permission required").await;
        // Keys are handled in order, so the prompt is sent once the
        // interrupt is through.
        ui.send(b"\x03carry on\r").await;
        ui.wait_for("Carried on fine.").await;
        ui.quit().await;
        let list = e.store().list(None, false).unwrap();
        assert_eq!(list.len(), round);
        for info in &list {
            let h = e.history(&info.id);
            let ids: Vec<String> = crate::harness::tool_results(&h)
                .iter()
                .map(|p| p["id"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(ids.len(), 1, "one result for the one call: {h:#?}");
        }
    }
    assert_eq!(e.read("victim.txt"), "keep me\n");
    for body in m.by("next") {
        let tool: Vec<&serde_json::Value> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect();
        assert_eq!(tool.len(), 1, "{body:#}");
    }
}

// `/agent` is saved with the session, so `-s` resumes with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_switched_agent_is_kept_for_resuming() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Resumable Chat").always(
        "hello",
        M::tools().and(M::prompt("say hello")),
        Reply::text("Hello there."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "--prompt 'say hello'") else {
        return;
    };
    // The header shows the title once the turn is over (only changed
    // cells are redrawn, so its first letters may already be on screen).
    ui.wait_for("sumable Chat").await;
    ui.send(b"/agent plan\r/model fake/other\r").await;
    ui.quit().await;
    let list = e.store().list(None, false).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].agent, "plan");
    assert_eq!(list[0].model, "fake/other");
}

// The model starts a background command and answers, so the turn ends;
// when the command finishes the idle UI resumes the session by itself (as OpenCode does) with its result.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_background_command_resumes_the_idle_ui() {
    let e = Env::new();
    let ask = M::tools().and(M::prompt("build it"));
    let notified = M::last("<shell id=\"shell_1\"");
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                ask.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 3; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                ask.clone().and(M::step(1)).and(notified.clone().not()),
                Reply::text("The build is running."),
            )
            .then(
                "resumed",
                M::tools().and(notified),
                Reply::text("The build finished fine."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "--auto --prompt 'build it'") else {
        return;
    };
    // The turn is over (the idle prompt's title is back) before the
    // command finishes; the status line's count is checked by the view's
    // own tests, as only changed cells are redrawn here.
    ui.wait_for("Tab agents").await;
    ui.wait_for("background command shell_1 finished").await;
    ui.wait_for("The build finished fine.").await;
    ui.quit().await;
    let last = crate::model::messages(&m.by("resumed")[0])
        .last()
        .cloned()
        .unwrap();
    assert!(crate::model::content(&last).contains("built-ok"), "{last}");
    m.assert_done();
}

// A prompt sent while the UI waits for a background command runs at once
// and reports the command itself; the session is not resumed for it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_while_the_ui_waits_reports_the_command_once() {
    let e = Env::new();
    let shell = "<shell id=\"shell_1\"";
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 3; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("build it")).and(M::step(1)),
                Reply::text("The build is running."),
            )
            .then(
                "check",
                M::tools().and(M::prompt("check on it")).and(M::step(0)),
                Reply::call("shell", json!({"command": "sleep 4", "description": "wait"})),
            )
            .then(
                "notified",
                M::tools().and(M::last(shell)),
                Reply::text("It built after all."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "--auto --prompt 'build it'") else {
        return;
    };
    // The turn is over once the idle prompt's title is back.
    ui.wait_for("Tab agents").await;
    ui.send(b"check on it\r").await;
    ui.wait_for("It built after all.").await;
    // Nothing is resumed afterwards.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    ui.quit().await;
    let dones = m.by("notified");
    assert_eq!(dones.len(), 1);
    let reported = crate::model::messages(&dones[0])
        .iter()
        .filter(|m| crate::model::content(m).contains(shell))
        .count();
    assert_eq!(reported, 1, "{:#}", dones[0]);
    assert!(m.unmatched().is_empty(), "{:#?}", m.unmatched());
    m.assert_done();
}
