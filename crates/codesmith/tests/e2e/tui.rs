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
