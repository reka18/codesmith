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
pub(crate) struct Ui {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    /// Everything read so far.
    pub(crate) screen: String,
    /// Where the chunk that held the last `wait_for` match starts, so the
    /// next wait also sees text that arrived in that same chunk.
    since: usize,
    /// The terminal as drawn: diff-based redraws split text in the byte
    /// stream, so waits also look at the screen itself. It keeps a
    /// scrollback, for mini mode.
    pub(crate) term: vt100::Parser,
}

impl Ui {
    /// The UI with `args`, or `None` where `script(1)` is missing.
    pub(crate) fn start(e: &Env, args: &str) -> Option<Ui> {
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
            since: 0,
            term: vt100::Parser::new(30, 100, 2000),
        })
    }

    /// Read output until `text` is drawn (after what was read before).
    pub(crate) async fn wait_for(&mut self, text: &str) {
        let mut buf = [0u8; 8192];
        // Text read before this wait but after the previous match's chunk
        // began counts too: two expected texts often arrive in one read.
        let before = self.since;
        let mut fresh = self.screen.split_off(before);
        // Where the chunk read last starts in `fresh`: the next wait looks
        // again from there.
        let mut last = 0;
        let stdout = &mut self.stdout;
        let term = &mut self.term;
        let on_screen = |term: &vt100::Parser| {
            let shown = term.screen().contents();
            shown
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains(text)
        };
        let seen = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if plain(&fresh).contains(text) || on_screen(term) {
                    return true;
                }
                let n = stdout.read(&mut buf).await.unwrap();
                if n == 0 {
                    return false;
                }
                term.process(&buf[..n]);
                last = fresh.len();
                fresh.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        })
        .await;
        self.since = before + last;
        self.screen.push_str(&fresh);
        assert_eq!(seen, Ok(true), "{text:?} was not drawn:\n{}", plain(&fresh));
    }

    pub(crate) async fn send(&mut self, bytes: &[u8]) {
        self.stdin.write_all(bytes).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Read all the UI writes until it exits.
    pub(crate) async fn read_to_end(&mut self) {
        let mut buf = [0u8; 8192];
        let (stdout, term, screen) = (&mut self.stdout, &mut self.term, &mut self.screen);
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        term.process(&buf[..n]);
                        screen.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            }
        })
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(20), self.child.wait()).await;
    }

    /// Quit with Ctrl+D on the empty prompt.
    pub(crate) async fn quit(mut self) {
        self.quit_reading().await;
    }

    /// Quit, reading (into `screen` and `term`) all the UI writes until it
    /// exits.
    pub(crate) async fn quit_reading(&mut self) {
        self.send(b"\x04").await;
        let mut buf = [0u8; 8192];
        let (stdout, term, screen) = (&mut self.stdout, &mut self.term, &mut self.screen);
        let _ = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        term.process(&buf[..n]);
                        screen.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
            }
        })
        .await;
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
    ui.wait_for("tab agents").await;
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
    ui.wait_for("tab agents").await;
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

// Fixed: an interrupt of a later turn, `/agent`, `/model` and `/undo`
// rebuilt the session and stopped its background commands with the old
// one, so they were never reported. As in OpenCode they keep running; the
// idle UI resumes the session when the command finishes.
#[tokio::test(flavor = "multi_thread")]
async fn background_commands_survive_an_interrupt_switches_and_undo() {
    let e = Env::new();
    let notified = M::last("<shell id=\"shell_1\"");
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "resumed",
                M::tools().and(notified),
                Reply::text("The build finished fine."),
            )
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 6; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("build it")).and(M::step(1)),
                Reply::text("The build is running."),
            )
            .then(
                "wait",
                M::tools().and(M::prompt("wait a bit")).and(M::step(0)),
                Reply::call("shell", json!({"command": "sleep 30", "description": "wait"})),
            ),
    )
    .await;
    // At medium verbosity the running command has its own row (at low it
    // is summed up as "Running command…").
    e.config(&m.url, json!({"tui": {"session": {"verbosity": "medium"}}}));
    let Some(mut ui) = Ui::start(&e, "--auto --prompt 'build it'") else {
        return;
    };
    ui.wait_for("tab agents").await;
    ui.send(b"wait a bit\r").await;
    ui.wait_for("sleep 30").await;
    // Interrupted once the idle prompt's title is back.
    ui.send(b"\x03").await;
    ui.wait_for("tab agents").await;
    ui.send(b"/agent plan\r/model fake/other\r/undo\r").await;
    ui.wait_for("the latest turn").await;
    ui.wait_for("The build finished fine.").await;
    ui.quit().await;
    let last = crate::model::messages(&m.by("resumed")[0])
        .last()
        .cloned()
        .unwrap();
    assert!(crate::model::content(&last).contains("built-ok"), "{last}");
    m.assert_done();
}

// Fixed: an interrupt reloaded the session and the idle UI resumed it at
// once for background commands that had already finished, so a turn the
// user stopped (a resume waiting for another process's turn too) started
// again by itself. Those commands now wait for the next prompt.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupt_does_not_resume_for_commands_already_finished() {
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
                    json!({"command": "sleep 1; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "wait",
                M::tools().and(M::prompt("build it")).and(M::step(1)),
                Reply::call("shell", json!({"command": "sleep 30", "description": "wait"})),
            )
            .then(
                "next",
                M::tools().and(M::prompt("what happened")),
                Reply::text("It built fine."),
            ),
    )
    .await;
    // At medium verbosity the running command has its own row (at low it
    // is summed up as "Running command…").
    e.config(&m.url, json!({"tui": {"session": {"verbosity": "medium"}}}));
    let Some(mut ui) = Ui::start(&e, "--auto --prompt 'build it'") else {
        return;
    };
    ui.wait_for("sleep 30").await;
    // The background command ends while the foreground one still runs.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    ui.send(b"\x03").await;
    // Interrupted once the idle prompt's title is back.
    ui.wait_for("tab agents").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(m.unmatched().is_empty(), "resumed: {:#?}", m.unmatched());
    ui.send(b"what happened\r").await;
    ui.wait_for("It built fine.").await;
    ui.quit().await;
    let next = &m.by("next")[0];
    let reported = crate::model::messages(next)
        .iter()
        .filter(|m| crate::model::content(m).contains(shell))
        .count();
    assert_eq!(reported, 1, "{next:#}");
    m.assert_done();
}

// With tabs off, `/new` ends the current session: its background
// commands are stopped (the UI shows and wakes one session only) and never
// reported.
#[tokio::test(flavor = "multi_thread")]
async fn without_tabs_a_new_session_stops_the_old_ones_background_commands() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 2; touch ended", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("build it")).and(M::step(1)),
                Reply::text("The build is running."),
            ),
    )
    .await;
    e.config(&m.url, json!({"tui": {"tabs": {"mode": "off"}}}));
    let Some(mut ui) = Ui::start(&e, "--auto --prompt 'build it'") else {
        return;
    };
    ui.wait_for("tab agents").await;
    ui.send(b"/new\r").await;
    ui.wait_for("new session").await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    ui.quit().await;
    assert!(!e.project.join("ended").exists(), "the command ran on");
    assert!(m.unmatched().is_empty(), "{:#?}", m.unmatched());
    m.assert_done();
}

// At the default (low) verbosity a run of tool calls is one summary row,
// as OpenCode draws it; ctrl+o opens it.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_of_tool_calls_is_summed_up_and_opens_with_ctrl_o() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Grouped")
            .then(
                "run",
                M::tools().and(M::prompt("check it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "echo grouped-output", "description": "check"}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("check it")).and(M::step(1)),
                Reply::text("Checked it."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "--auto --prompt 'check it'") else {
        return;
    };
    // Only changed cells are redrawn: the row was "+ Running command…".
    ui.wait_for("1 command").await;
    ui.wait_for("Checked it.").await;
    // (Its output may have been drawn as it ran, below the summary.)
    ui.send(b"\x0f").await;
    ui.wait_for("grouped-output").await;
    ui.quit().await;
    m.assert_done();
}

// A running command's output is drawn as it prints, in the call's row (as
// OpenCode's shell rows show it), and the result replaces it at the end.
#[tokio::test(flavor = "multi_thread")]
async fn a_running_command_shows_its_output_as_it_prints() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Live")
            .then(
                "run",
                M::tools().and(M::prompt("run it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "echo first-chunk; sleep 2; x=second; echo $x-chunk", "description": "print"}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("run it")).and(M::step(1)),
                Reply::text("It printed twice."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "--auto --prompt 'run it'") else {
        return;
    };
    // Live rows are marked `│`; the finished row's output `→`.
    ui.wait_for("│ first-chunk").await;
    // Drawn while the command still runs: its second line is not out yet.
    assert!(!plain(&ui.screen).contains("second-chunk"));
    ui.wait_for("It printed twice.").await;
    ui.quit().await;
    m.assert_done();
}

// A subagent's run is drawn in the row of the call that runs it, as
// OpenCode's subagent rows are: `General Subagent — <description>`, its
// calls as they run, and how it ended. Its permission prompt says which
// subagent asks.
#[tokio::test(flavor = "multi_thread")]
async fn a_subagents_activity_is_drawn_in_its_row() {
    use codesmith::core::tool::subagent::PREAMBLE;
    let e = Env::new();
    let parent = M::tools()
        .and(M::prompt("delegate it"))
        .and(M::prompt(PREAMBLE).not());
    let child = M::tools().and(M::prompt(PREAMBLE));
    let m = Model::start(
        Script::new()
            .titled("Delegated")
            .then(
                "delegate",
                parent.clone().and(M::step(0)),
                Reply::call(
                    "subagent",
                    json!({"agent": "general", "description": "Run the check", "prompt": "Run the check."}),
                ),
            )
            .then(
                "child-run",
                child.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "echo child-ran", "description": "Check"}),
                ),
            )
            .then(
                "child-answer",
                child.and(M::step(1)),
                Reply::text("The check ran."),
            )
            .then(
                "answer",
                parent.and(M::step(1)),
                Reply::text("Delegated fine."),
            ),
    )
    .await;
    e.config(
        &m.url,
        // Medium verbosity: at the default (low) the finished row folds into
        // its group's summary, so its ending may never be drawn.
        json!({
            "permissions": [{"action": "shell", "resource": "*", "effect": "ask"}],
            "tui": {"session": {"verbosity": "medium"}}
        }),
    );
    let Some(mut ui) = Ui::start(&e, "--prompt 'delegate it'") else {
        return;
    };
    ui.wait_for("asked by the general subagent · Run the check")
        .await;
    assert!(plain(&ui.screen).contains("General Subagent — Run the check"));
    ui.send(b"y").await;
    ui.wait_for("Delegated fine.").await;
    // The row's ending is usually drawn before the answer, but under load
    // it can land in a later frame: wait for it if it isn't there yet.
    if !plain(&ui.screen).contains("completed") {
        ui.wait_for("completed").await;
    }
    // Expanded, its calls and its text are shown.
    ui.send(b"\x0f").await;
    ui.wait_for("The check ran.").await;
    assert!(plain(&ui.screen).contains("shell"));
    ui.quit().await;
    m.assert_done();
}

// `!command` runs the command as the user (OpenCode's shell mode): drawn as
// a shell row, recorded for the model's next step, no model call.
#[tokio::test(flavor = "multi_thread")]
async fn a_bang_prompt_runs_a_shell_command_without_the_model() {
    let e = Env::new();
    let m = Model::start(Script::new()).await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "--prompt '!echo user-shell-output'") else {
        return;
    };
    ui.wait_for("1 command").await;
    ui.send(b"\x0f").await;
    ui.wait_for("user-shell-output").await;
    ui.quit().await;
    assert!(m.steps().is_empty(), "no model call");
    let id = e.store().list(None, false).unwrap()[0].id.clone();
    let h = e.history(&id);
    assert_eq!(h.len(), 1);
    assert!(h[0].to_string().contains("user-shell-output"));
}

/// Alt+Up and Alt+Shift+Down: the previous tab, and the next tab that is
/// unread or waiting for its user.
const PREVIOUS_TAB: &[u8] = b"\x1b[1;3A";
const NEXT_UNREAD_TAB: &[u8] = b"\x1b[1;4B";

// With tabs (on by default, as in OpenCode), `/new` opens a tab of its own
// and the first session goes on in the background: its command finishes,
// it resumes by itself, and its tab is marked unread. The tabs are kept
// for the next start, where the first loads when it is shown.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_tab_keeps_its_commands_and_resumes_by_itself() {
    let e = Env::new();
    let notified = M::last("<shell id=\"shell_1\"");
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                M::tools().and(M::prompt("build it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 3; touch ended", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                M::tools()
                    .and(M::prompt("build it"))
                    .and(M::step(1))
                    .and(notified.clone().not()),
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
    ui.wait_for("tab agents").await;
    ui.send(b"/new\r").await;
    ui.wait_for("new session").await;
    // The first tab resumes in the background and is marked unread.
    ui.wait_for("\u{2022} Build").await;
    assert!(e.project.join("ended").exists(), "the command was stopped");
    assert_eq!(m.by("resumed").len(), 1);
    ui.send(PREVIOUS_TAB).await;
    ui.wait_for("The build finished fine.").await;
    ui.quit().await;
    let kept = std::fs::read_to_string(e.data.join("codesmith/tui-tabs.json")).unwrap();
    assert!(kept.contains("\"title\": \"Build\""), "{kept}");

    // Kept for the next start: the tab is in the strip, and its session
    // loads when it is shown.
    let Some(mut ui) = Ui::start(&e, "--auto") else {
        return;
    };
    ui.wait_for("Build + New session").await;
    ui.send(PREVIOUS_TAB).await;
    ui.wait_for("The build finished fine.").await;
    ui.quit().await;
    m.assert_done();
}

// A permission request from a background tab: the tab is marked `!`, the
// shown one says so, and the next-unread key goes to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_background_tabs_permission_request_is_surfaced() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .titled("Touch")
            .then(
                "wait",
                M::tools().and(M::prompt("touch it")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 2", "description": "wait"}),
                ),
            )
            .then(
                "touch",
                M::tools().and(M::prompt("touch it")).and(M::step(1)),
                Reply::call(
                    "shell",
                    json!({"command": "touch done", "description": "touch"}),
                ),
            )
            .then(
                "answer",
                M::tools().and(M::prompt("touch it")).and(M::step(2)),
                Reply::text("Touched it."),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [
            {"action": "shell", "resource": "sleep *", "effect": "allow"},
            {"action": "shell", "resource": "touch *", "effect": "ask"}
        ]}),
    );
    let Some(mut ui) = Ui::start(&e, "--prompt 'touch it'") else {
        return;
    };
    // `/new` while the turn runs: it goes on in its tab.
    ui.wait_for("Running command").await;
    ui.send(b"/new\r").await;
    ui.wait_for("new session").await;
    ui.wait_for("needs your permission").await;
    // Titled when its turn ends.
    ui.wait_for("! New session + New session").await;
    assert!(!e.project.join("done").exists());
    ui.send(NEXT_UNREAD_TAB).await;
    ui.wait_for("Permission required").await;
    ui.send(b"y").await;
    ui.wait_for("Touched it.").await;
    ui.quit().await;
    assert!(e.project.join("done").exists());
    m.assert_done();
}

/// The first row of the screen: the tab strip.
fn strip(ui: &Ui) -> String {
    let shown = ui.term.screen().contents();
    let first = shown.lines().next().unwrap_or("");
    first.split_whitespace().collect::<Vec<_>>().join(" ")
}

// OpenCode's "keeps each visited session open", "tracks a temporary new
// session tab across close and creation" and "lists closed tabs newest
// first and reopens" (adapted: keys rather than a route and a menu). Each
// session gets a tab, the new session page becomes one once something is
// sent, closing a tab shows the one before it and reopening puts it back
// where it was; the tabs are kept for the next start.
#[tokio::test(flavor = "multi_thread")]
async fn oc_keeps_each_visited_session_open() {
    let e = Env::new();
    let m = Model::start(
        Script::new()
            .always(
                "title alpha",
                M::title().and(M::last("alpha")),
                Reply::text("Alpha chat"),
            )
            .always(
                "title beta",
                M::title().and(M::last("beta")),
                Reply::text("Beta chat"),
            )
            .always(
                "alpha",
                M::tools().and(M::prompt("alpha")),
                Reply::text("Answered alpha."),
            )
            .always(
                "beta",
                M::tools().and(M::prompt("beta")),
                Reply::text("Answered beta."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "--auto --prompt 'alpha'") else {
        return;
    };
    ui.wait_for("Answered alpha.").await;
    ui.wait_for("Alpha chat").await;
    ui.send(b"/new\r").await;
    ui.wait_for("Alpha chat + New session").await;
    ui.send(b"beta\r").await;
    ui.wait_for("Answered beta.").await;
    ui.wait_for("Alpha chat Beta chat").await;
    // Ctrl+X w closes the shown tab: the one shown before it comes back.
    ui.send(b"\x18w").await;
    ui.wait_for("Answered alpha.").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(strip(&ui), "Alpha chat");
    // Ctrl+X Shift+T reopens it where it was.
    ui.send(b"\x18T").await;
    ui.wait_for("Answered beta.").await;
    ui.wait_for("Alpha chat Beta chat").await;
    // Ctrl+X 1 goes to the first tab.
    ui.send(b"\x181").await;
    ui.wait_for("Answered alpha.").await;
    ui.quit().await;

    let Some(mut ui) = Ui::start(&e, "--auto") else {
        return;
    };
    ui.wait_for("Alpha chat Beta chat + New session").await;
    ui.quit().await;
    assert!(m.unmatched().is_empty(), "{:#?}", m.unmatched());
}

/// A server that answers every request with the model list `ids`, counting
/// the requests.
async fn model_list_server(
    ids: &'static [&'static str],
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let data: Vec<_> = ids.iter().map(|id| json!({"id": id})).collect();
                let body = json!({ "data": data }).to_string();
                let _ = s
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                    .await;
            });
        }
    });
    (format!("http://{addr}/v1"), hits)
}

// The model picker lists what the local servers serve (as `codesmith
// models` does), asking them again each time it opens.
#[tokio::test(flavor = "multi_thread")]
async fn the_model_picker_lists_discovered_local_models() {
    let e = Env::new();
    let m = Model::start(Script::new()).await;
    let (url, hits) = model_list_server(&["qwen-discovered"]).await;
    // The built-in local providers are marked not local, so nothing else on
    // this machine is asked.
    let off = json!({"local": false});
    e.config(
        &m.url,
        json!({"providers": {
            "fake": {
                "protocol": "openai-chat",
                "settings": {"baseURL": m.url, "apiKey": "test-key"},
                "models": {"m": {"limit": {"context": 100000}}}
            },
            "omlx": {"name": "oMLX", "settings": {"baseURL": url, "discovery": "openai"}},
            "lmstudio": off, "ollama": off, "vllm": off, "llamacpp": off,
        }}),
    );
    let Some(mut ui) = Ui::start(&e, "") else {
        return;
    };
    ui.wait_for("fake/m").await;
    ui.send(b"/model\r").await;
    ui.wait_for("omlx/qwen-discovered").await;
    ui.wait_for("oMLX · local").await;
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Esc closes the picker (alone, not as the start of a sequence).
    ui.send(b"\x1b").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Opened again, it asks again.
    ui.send(b"/model\r").await;
    ui.wait_for("omlx/qwen-discovered").await;
    let asked = tokio::time::timeout(Duration::from_secs(5), async {
        while hits.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(asked.is_ok(), "the second open did not ask again");
    ui.send(b"\x1b").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ui.quit().await;
}

// app-lifecycle.test.tsx: "boots on the simulation renderer with the
// terminal title enabled", "session lifecycle updates the terminal title
// and prints the epilogue after cleanup" and "session title generated
// while an untitled session is loading remains visible" (adapted: the
// title comes with the turn that generated it; the window title follows
// it and never goes back, then is cleared on exit, before the summary).
#[tokio::test(flavor = "multi_thread")]
async fn oc_the_terminal_title_follows_the_session_and_the_exit_prints_the_summary() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Greeting").then(
        "hello",
        M::tools().and(M::prompt("say hello")),
        Reply::text("Hello from the scripted model."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "--prompt 'say hello'") else {
        return;
    };
    ui.wait_for("Hello from the scripted model.").await;
    ui.wait_for("110 tokens").await;
    ui.quit_reading().await;
    let raw = ui.screen.clone();
    let boot = raw.find("\x1b]0;CodeSmith\x07").expect("the boot title");
    let titled = raw
        .find("\x1b]0;CS | Greeting\x07")
        .expect("the session title");
    let cleared = raw.rfind("\x1b]0;\x07").expect("the title cleared on exit");
    assert!(boot < titled && titled < cleared, "{raw:?}");
    assert!(!raw[titled..].contains("\x1b]0;CodeSmith\x07"), "{raw:?}");
    let id = e.store().list(None, false).unwrap()[0].id.clone();
    let after = plain(&raw[cleared..]);
    assert!(after.contains("Session Greeting"), "{after}");
    assert!(
        after.contains(&format!("Continue codesmith -s {id}")),
        "{after}"
    );
    m.assert_done();
}

// app-lifecycle.test.tsx: "SIGHUP clears title and disposes scoped
// resources once" (adapted: the UI closes as on exit).
#[tokio::test(flavor = "multi_thread")]
async fn oc_sighup_clears_the_title_and_closes_the_ui() {
    let e = Env::new();
    let m = Model::start(Script::new()).await;
    e.config(&m.url, json!({}));
    let Some(mut ui) = Ui::start(&e, "") else {
        return;
    };
    ui.wait_for("/help for commands").await;
    let script = ui.child.id().unwrap();
    // `script` runs the UI as its child.
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("pgrep")
            .args(["-P", &script.to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let pid: i32 = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.trim().parse().ok())
        .expect("the UI's process");
    // SAFETY: signalling a process this test started.
    unsafe {
        libc::kill(pid, libc::SIGHUP);
    }
    ui.read_to_end().await;
    assert!(ui.screen.contains("\x1b]0;\x07"), "{:?}", ui.screen);
}
