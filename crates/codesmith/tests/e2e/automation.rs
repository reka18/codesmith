//! Automations through the CLI: add, list, run what is due, the run log,
//! pause and resume, and one-shot `--at` automations.
//!
//! There is no CLI way to make an automation due now, so the tests move its
//! `next_due` into the past in the database (as `automation serve` would
//! find it a while later).

use crate::harness::{user_texts, Env};
use crate::model::{Model, Reply, Script, M};
use serde_json::json;

fn due_now(e: &Env, id: &str) {
    let c = rusqlite::Connection::open(&e.db).unwrap();
    let n = c
        .execute(
            "UPDATE automation SET next_due = ?2 WHERE id = ?1",
            rusqlite::params![id, codesmith::core::store::now() - 1_000],
        )
        .unwrap();
    assert_eq!(n, 1, "no automation {id}");
}

fn created_id(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no id in {stdout:?}"))
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeating_prompt_runs_when_due_and_logs_its_runs() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Daily summary").always(
        "summary",
        M::tools().and(M::prompt("summarize the day")),
        Reply::text("Nothing happened."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let out = e
        .run(&[
            "automation",
            "add",
            "--name",
            "daily",
            "--every",
            "10 minutes",
            "--prompt",
            "summarize the day",
        ])
        .await;
    out.ok();
    assert!(out.stdout.starts_with("Created "), "{}", out.both());
    let id = created_id(&out.stdout);
    let list = e.run(&["automation", "list"]).await;
    list.ok();
    assert!(
        list.stdout.contains(&id) && list.stdout.contains("daily"),
        "{}",
        list.both()
    );

    // Not due yet: nothing fires and the model is not called.
    let out = e.run(&["automation", "run-due"]).await;
    out.ok();
    assert_eq!(out.stdout.trim(), "0 fired");
    assert!(m.seen().is_empty());

    due_now(&e, &id);
    let out = e.run(&["automation", "run-due"]).await;
    out.ok();
    assert_eq!(out.stdout.trim(), "1 fired", "{}", out.both());
    assert_eq!(m.by("summary").len(), 1);

    // Due again right away: a second run-due must fire it too.
    due_now(&e, &id);
    let out = e.run(&["automation", "run-due"]).await;
    out.ok();
    assert_eq!(out.stdout.trim(), "1 fired", "{}", out.both());
    assert_eq!(m.by("summary").len(), 2);

    let log = e.run(&["automation", "log", &id]).await;
    log.ok();
    let lines: Vec<&str> = log.stdout.lines().filter(|l| l.starts_with('#')).collect();
    assert_eq!(lines.len(), 2, "{}", log.both());
    assert!(
        lines[0].starts_with("#1 ") && lines[0].contains(" fired · ses_"),
        "{}",
        log.stdout
    );
    // Each run is a new session in the project with the prompt.
    let sessions = e
        .store()
        .list(Some(&e.project.display().to_string()), false)
        .unwrap();
    assert_eq!(sessions.len(), 2);
    for s in &sessions {
        assert_eq!(user_texts(&e.history(&s.id)), ["summarize the day"]);
        assert!(log.stdout.contains(&s.id));
    }
    // The next run is in the future again.
    let a = codesmith::core::automation::get(&e.store(), &id)
        .unwrap()
        .unwrap();
    assert!(a.next_due.unwrap() > codesmith::core::store::now() + 9 * 60_000);
    assert_eq!(a.runs, 2);

    // Paused: due but not fired; resumed: fired.
    e.run(&["automation", "pause", &id]).await.ok();
    due_now(&e, &id);
    let out = e.run(&["automation", "run-due"]).await;
    assert_eq!(out.stdout.trim(), "0 fired");
    let out = e.run(&["automation", "resume", &id]).await;
    out.ok();
    assert!(out.stdout.contains("is active"), "{}", out.both());
    due_now(&e, &id);
    let out = e.run(&["automation", "run-due"]).await;
    assert_eq!(out.stdout.trim(), "1 fired", "{}", out.both());
    e.run(&["automation", "remove", &id]).await.ok();
    let list = e.run(&["automation", "list"]).await;
    assert!(!list.stdout.contains(&id));
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_at_runs_once() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Reminder").then(
        "remind",
        M::tools().and(M::prompt("remind me to stretch")),
        Reply::text("Stretch!"),
    ))
    .await;
    e.config(&m.url, json!({}));
    let out = e
        .run(&[
            "automation",
            "add",
            "--name",
            "stretch",
            "--at",
            "in 1 minutes",
            "--prompt",
            "remind me to stretch",
        ])
        .await;
    out.ok();
    let id = created_id(&out.stdout);
    due_now(&e, &id);
    let out = e.run(&["automation", "run-due"]).await;
    assert_eq!(out.stdout.trim(), "1 fired", "{}", out.both());
    let a = codesmith::core::automation::get(&e.store(), &id)
        .unwrap()
        .unwrap();
    assert_eq!((a.status.as_str(), a.next_due), ("done", None));
    let out = e.run(&["automation", "run-due"]).await;
    assert_eq!(out.stdout.trim(), "0 fired");
    let out = e.run(&["automation", "resume", &id]).await;
    assert_eq!(
        out.code,
        Some(1),
        "a finished one-shot cannot resume: {}",
        out.both()
    );
    m.assert_done();
}
