//! Ports of OpenCode's user shell command tests (MIT): `session-shell.test.ts`,
//! the shell cases of `session-create.test.ts` and
//! `session-runner-message.test.ts`. Each test names the OpenCode test it
//! ports; see `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: OpenCode projects a shell message for its UI and admits the
//! completion notification to the inbox; CodeSmith records the
//! notification as a user message once the command ends, which front ends
//! show as the command (`user_shell::parse`). "Without waking the model"
//! is checked as no model request.

mod support;

use codesmith_core::user_shell::{self, State};
use std::time::Duration;
use support::{text, H};

/// The user commands' notifications in the stored history.
fn notifications(h: &H, id: &str) -> Vec<String> {
    h.store()
        .history(id)
        .unwrap()
        .iter()
        .map(text)
        .filter(|t| user_shell::parse(t).is_some())
        .collect()
}

/// Wait for the session's one running command; its id.
async fn running(jobs: &codesmith_core::tool::shell::Jobs) -> String {
    for _ in 0..500 {
        if let Some(r) = jobs.foreground().first() {
            return r.id.clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the command never started");
}

// session-create.test.ts: "runs a shell command and projects the started/ended shell message"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_runs_a_shell_command_in_the_session_environment() {
    let mut h = H::new().await;
    h.s.set_environment(Some(
        [(
            "OPENCODE_SESSION_ENV_TEST".to_string(),
            "attached".to_string(),
        )]
        .into_iter()
        .collect(),
    ));
    let command = "printf %s \"$OPENCODE_SESSION_ENV_TEST\"";
    let ran = h.s.shell(command).await.unwrap();
    assert_eq!(ran.state, State::Completed);
    assert_eq!(ran.exit, Some(0));
    assert_eq!(ran.command, command);
    assert!(ran.output.contains("attached"), "{}", ran.output);
    assert!(!ran.truncated);
    let id = h.s.id.clone();
    let shown = user_shell::parse(&notifications(&h, &id)[0]).unwrap();
    assert_eq!(shown.command, command);
    assert!(!shown.failed);
}

// session-create.test.ts: "still emits shell ended for a failing command"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_still_records_a_failing_command() {
    let mut h = H::new().await;
    let ran = h.s.shell("false").await.unwrap();
    assert_eq!(ran.state, State::Completed);
    assert_ne!(ran.exit, Some(0));
    assert!(ran.exit.is_some());
    let id = h.s.id.clone();
    assert!(
        user_shell::parse(&notifications(&h, &id)[0])
            .unwrap()
            .failed
    );
}

// session-runner-message.test.ts: "background user shells enter model context only through their completion notification"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_user_shells_reach_the_model_only_through_their_notification() {
    let mut h = H::new().await;
    h.s.shell("pwd").await.unwrap();
    assert!(h.requests().is_empty(), "no model call");
    h.prompt("next").await;
    let req = h.requests().last().unwrap().clone();
    let users: Vec<String> = req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        // The date, which a local server gets before the history.
        .filter(|c| !c.starts_with("<system-update>"))
        .collect();
    assert_eq!(users.len(), 2, "{users:?}");
    assert!(users[0].starts_with(user_shell::PREFIX), "{users:?}");
    assert!(users[0].contains("command=\"pwd\""), "{users:?}");
    assert!(
        users[0].contains(&h.root().display().to_string()),
        "{users:?}"
    );
    assert_eq!(users[1], "next");
}

// session-shell.test.ts: "records output and admits one completion without waking the model after exit ${exit}" (2 cases)
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_shell_records_output_and_one_completion_without_waking_the_model() {
    for exit in [0, 7] {
        let mut h = H::new().await;
        let command = format!("printf 'user output\\n'; printf 'user error\\n' >&2; exit {exit}");
        let ran = h.s.shell(&command).await.unwrap();
        assert_eq!(ran.exit, Some(exit));
        assert!(ran.output.contains("user output"));
        assert!(ran.output.contains("user error"));
        assert!(!ran.truncated);
        let id = h.s.id.clone();
        let history = h.store().history(&id).unwrap();
        assert_eq!(history.len(), 1);
        let t = text(&history[0]);
        assert!(t.contains("state=\"completed\""), "{t}");
        assert_eq!(user_shell::parse(&t).unwrap().command, command);
        assert!(t.contains("user output") && t.contains("user error"), "{t}");
        if exit == 0 {
            assert!(!t.contains("Exit code"), "{t}");
        } else {
            assert!(
                t.ends_with(&format!("\n\nExit code: {exit}\n</shell>")),
                "{t}"
            );
        }
        assert!(h.requests().is_empty(), "no model call");
    }
}

// session-shell.test.ts: "records a ${outcome.status} shell and admits its completion without waking the model" (2 cases)
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_shell_records_a_cancelled_or_timed_out_command() {
    for killed in [true, false] {
        let mut h = H::new().await;
        let jobs = h.s.jobs();
        let command = "echo timeout started; sleep 20";
        let stop = async {
            let id = running(&jobs).await;
            if killed {
                assert!(jobs.cancel(&id));
            } else {
                assert!(jobs.set_timeout(&id, 1));
            }
        };
        let (ran, ()) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(h.s.shell(command), stop)
        })
        .await
        .expect("the command was stopped");
        let ran = ran.unwrap();
        let id = h.s.id.clone();
        let t = notifications(&h, &id).pop().unwrap();
        if killed {
            assert_eq!(ran.state, State::Cancelled);
            assert_eq!(ran.exit, None);
            assert!(t.contains("state=\"cancelled\""), "{t}");
            assert!(t.contains("Cancelled"), "{t}");
        } else {
            assert_eq!(ran.state, State::Completed);
            assert!(t.contains("timeout started"), "{t}");
            assert!(t.contains("\n\nCommand timed out. If it needs longer and isn't waiting for input, rerun with a larger timeout.\nExit code: "), "{t}");
        }
        assert!(jobs.foreground().is_empty());
        assert!(h.requests().is_empty(), "no model call");
    }
}

// session-shell.test.ts: "admits a spawn failure without waking the model before failing the caller"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_shell_records_a_spawn_failure_before_failing() {
    let mut h = H::new().await;
    std::fs::remove_dir_all(h.project.path()).unwrap();
    let command = "echo should-not-start";
    assert!(h.s.shell(command).await.is_err());
    let id = h.s.id.clone();
    let history = h.store().history(&id).unwrap();
    assert_eq!(history.len(), 1);
    let shown = user_shell::parse(&text(&history[0])).unwrap();
    assert_eq!(shown.state, State::Error);
    assert_eq!(shown.command, command);
    assert!(h.requests().is_empty(), "no model call");
}

// session-shell.test.ts: "keeps the shell and completion alive when the waiting caller is interrupted"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_shell_outlives_an_interrupted_caller() {
    let mut h = H::new().await;
    h.prompt("hello").await;
    let id = h.s.id.clone();
    let jobs = h.s.jobs();
    let marker = h.outside.path().join("go");
    let command = format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done; echo detached finished",
        marker.display()
    );
    let interrupted = tokio::time::timeout(Duration::from_millis(300), h.s.shell(&command)).await;
    assert!(interrupted.is_err(), "the call was interrupted");
    assert_eq!(jobs.foreground().len(), 1, "still running");
    assert!(notifications(&h, &id).is_empty());

    std::fs::write(&marker, "").unwrap();
    let mut found = Vec::new();
    for _ in 0..500 {
        found = notifications(&h, &id);
        if !found.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("detached finished"), "{found:?}");
    assert!(found[0].contains("state=\"completed\""));
}

// session-create.test.ts: "copies only settled shell messages into forks"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_copies_only_settled_shell_messages_into_forks() {
    let mut h = H::new().await;
    h.prompt("Run a shell").await;
    let id = h.s.id.clone();
    let marker = h.outside.path().join("go");
    let command = format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done; echo complete",
        marker.display()
    );
    // While it runs it is not in the history, so a fork has none of it.
    let _ = tokio::time::timeout(Duration::from_millis(200), h.s.shell(&command)).await;
    let mut st = h.store();
    st.fork(&id, "running").unwrap();
    std::fs::write(&marker, "").unwrap();
    for _ in 0..500 {
        if !notifications(&h, &id).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    st.fork(&id, "completed").unwrap();
    assert!(notifications(&h, "running").is_empty());
    let copied = notifications(&h, "completed");
    assert_eq!(copied.len(), 1);
    assert!(copied[0].contains("complete"));
}
