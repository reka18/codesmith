//! Ports of OpenCode's automation tests (`automation.test.ts`,
//! `automation-event.test.ts` and `tool-automation.test.ts`, MIT) for the
//! automation roadmap groups: the backoff after a failed prompt, expiry,
//! the tools a program calls, and the `/automation` command; with a
//! regression test for each fix and their edge cases. OpenCode's clock is a
//! test clock; here time passing is simulated by moving the stored due,
//! retry and expiry times back.

mod handoff_support;

use codesmith_core::automation::{self as au, Action, Target, Trigger};
use codesmith_core::store::now;
use handoff_support::*;
use serde_json::json;

const MINUTE: i64 = 60_000;

fn helper() -> Target {
    Target::Subagent {
        subagent: "helper".into(),
    }
}

/// Make every record written into session `id` fail, as a database error
/// during the handoff would.
fn refuse_writes(w: &World, id: &str) {
    w.sql(
        &format!("CREATE TRIGGER refuse BEFORE INSERT ON record WHEN NEW.session_id = '{id}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"),
        [],
    );
}

fn allow_writes(w: &World) {
    w.sql("DROP TRIGGER IF EXISTS refuse", []);
}

/// Move `id`'s retry time to `at`.
fn retry_at(w: &World, id: &str, at: i64) {
    w.sql(
        "UPDATE automation SET retry_at = ?2 WHERE id = ?1",
        rusqlite::params![id, at],
    );
}

/// Move the slot run `run` of `id` was claimed for to `at`.
fn run_due(w: &World, id: &str, run: u64, at: i64) {
    w.sql(
        "UPDATE automation_run SET due = ?3 WHERE automation_id = ?1 AND run = ?2",
        rusqlite::params![id, run as i64, at],
    );
}

fn once() -> Trigger {
    Trigger::At {
        at: "in 1 minute".into(),
        tz: None,
    }
}

// ----- Automation Failure Handling -------------------------------------------

// automation.test.ts: "backs off for five minutes when a prompt fails, then
// runs once" (adapted: runs are counted at claim, so the failed run counts
// and its retry is the same run; time passing is simulated by moving the
// retry time back)
#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_backs_off_for_five_minutes_when_a_prompt_fails_then_runs_once() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.create(
        None,
        once(),
        Action::Prompt {
            prompt: "flaky".into(),
            to: helper(),
        },
    );
    let child = au::run_session_id(&id, 1);
    refuse_writes(&w, &child);
    w.due_now(&id);
    let runner = w.runner();
    let before = now();
    runner.tick().await.unwrap();
    let after = now();
    let a = w.get(&id);
    assert_eq!(
        (a.status.as_str(), a.runs, a.last_outcome.as_deref()),
        ("done", 1, Some("failed")),
        "{a:?}"
    );
    assert_eq!(w.phase(&id, 1).as_deref(), Some(au::CLAIMED));
    let retry = a.retry_at.expect("a retry time");
    assert!(
        retry >= before + 5 * MINUTE && retry <= after + 5 * MINUTE,
        "five minutes later: {}",
        retry - before
    );
    allow_writes(&w);

    // Not retried on every tick: four minutes on, a minute is left.
    retry_at(&w, &id, now() + MINUTE);
    w.stale();
    runner.tick().await.unwrap();
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 0);
    assert_eq!(w.model.requests("flaky"), 0);

    // Five minutes on, it runs once, late.
    retry_at(&w, &id, now() - 1);
    run_due(&w, &id, 1, now() - 5 * MINUTE);
    w.stale();
    runner.tick().await.unwrap();
    let a = w.get(&id);
    assert_eq!(
        (a.status.as_str(), a.runs, a.last_outcome.as_deref()),
        ("done", 1, Some("late")),
        "{a:?}"
    );
    assert_eq!(w.phases(&id), [(1, au::SETTLED.to_string())]);
    assert_eq!(w.texts(&child), ["u:flaky", "a:answer to: flaky"]);
    assert_eq!(au::runs(&w.db, &id).unwrap()[0].outcome, "late");
    w.stale();
    runner.tick().await.unwrap();
    assert_eq!(w.model.requests("flaky"), 1, "it ran once");
}

// Regression: a prompt whose handoff failed was retried a minute later,
// like a program's; it waits five minutes now, and a program's still one.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_prompt_handoff_waits_five_minutes_and_a_programs_one() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let prompt = w.prompt(None, "flaky", helper());
    let program = w.code(
        None,
        "return { send: { prompt: 'flaky program', to: { subagent: 'helper' } } }",
        None,
    );
    refuse_writes(&w, &au::run_session_id(&prompt, 1));
    w.sql(
        &format!("CREATE TRIGGER refuse2 BEFORE INSERT ON record WHEN NEW.session_id = '{}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END", au::run_session_id(&program, 1)),
        [],
    );
    w.due_now(&prompt);
    w.due_now(&program);
    let runner = w.runner();
    let before = now();
    runner.tick().await.unwrap();
    let after = now();
    for (id, wait) in [(&prompt, 5 * MINUTE), (&program, MINUTE)] {
        let a = w.get(id);
        let retry = a.retry_at.expect("a retry time");
        assert!(
            retry >= before + wait && retry <= after + wait,
            "{:?}: {} ms",
            a.action,
            retry - before
        );
        assert_eq!(a.last_outcome.as_deref(), Some("failed"));
    }
    allow_writes(&w);
    w.sql("DROP TRIGGER refuse2", []);
    runner.release().unwrap();
}

// Regression: a run handed off a minute or more after its slot was
// labelled by when it was claimed; it is late now, and on time just
// under a minute (the boundary).
#[tokio::test(flavor = "multi_thread")]
async fn a_run_handed_off_a_minute_after_its_slot_is_late() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let runner = w.runner();
    for (offset, label) in [(MINUTE - 5_000, "fired"), (MINUTE, "late")] {
        let id = w.prompt(None, &format!("slot {label}"), helper());
        w.due_now(&id);
        // Claimed on time, then left by a process that stopped.
        let claimed = au::claim_due(&w.db, now()).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(au::pending(&w.db, &id).unwrap()[0].label, "fired");
        // The slot was `offset` ago when it is handed off.
        run_due(&w, &id, 1, now() - offset);
        w.stale();
        tick_until(&runner, "the hand-off", || {
            w.phase(&id, 1).as_deref() == Some(au::SETTLED)
        })
        .await;
        assert_eq!(au::runs(&w.db, &id).unwrap()[0].outcome, label);
        assert_eq!(w.get(&id).last_outcome.as_deref(), Some(label));
        au::remove(&w.db, &id).unwrap();
    }
    runner.release().unwrap();
}

// Edge: a prompt that keeps failing is not retried forever: the third
// failed handoff in a row fails the run and pauses it, and resuming
// starts over, with the run still counted once.
#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_that_keeps_failing_pauses_on_the_third_handoff() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.prompt(None, "flaky", helper());
    let child = au::run_session_id(&id, 1);
    refuse_writes(&w, &child);
    w.due_now(&id);
    let runner = w.runner();
    runner.tick().await.unwrap();
    for n in 2..=3 {
        assert_eq!(w.get(&id).status, "active", "attempt {n}");
        retry_at(&w, &id, now() - 1);
        w.stale();
        runner.tick().await.unwrap();
    }
    let a = w.get(&id);
    assert_eq!((a.status.as_str(), a.runs), ("paused", 1), "{a:?}");
    assert!(a
        .last_error
        .as_deref()
        .is_some_and(|e| e.starts_with("Paused after 3 failed handoffs")));
    assert_eq!(w.phases(&id), [(1, au::FAILED.to_string())]);
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 0);
    allow_writes(&w);
    let a = au::set_status(&w.db, &id, "active").unwrap();
    assert_eq!((a.failures, a.retry_at), (0, None));
    runner.release().unwrap();
}
