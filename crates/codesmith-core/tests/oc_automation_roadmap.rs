//! Ports of OpenCode's automation tests (`automation.test.ts`,
//! `automation-event.test.ts` and `tool-automation.test.ts`, MIT) for the
//! automation roadmap groups: the backoff after a failed prompt, expiry,
//! the tools a program calls, and the `/automation` command; with a
//! regression test for each fix and their edge cases. OpenCode's clock is a
//! test clock; here time passing is simulated by moving the stored due,
//! retry and expiry times back.

mod handoff_support;
mod support;

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

// ----- Automation Expiry -----------------------------------------------------

fn every(e: &str) -> Trigger {
    Trigger::Every {
        every: e.into(),
        tz: None,
    }
}

fn ask(prompt: &str) -> Action {
    Action::Prompt {
        prompt: prompt.into(),
        to: helper(),
    }
}

/// Create an automation that expires at `expires`.
fn expiring(w: &World, trigger: Trigger, action: Action, expires: &str) -> au::Automation {
    au::create_with(
        &w.db,
        au::New {
            name: "poll".into(),
            directory: w.dir(),
            session_id: None,
            trigger,
            action,
        },
        &au::Opts {
            expires: Some(expires.into()),
        },
    )
    .unwrap()
}

fn try_expiring(w: &World, trigger: Trigger, expires: &str) -> anyhow::Result<au::Automation> {
    au::create_with(
        &w.db,
        au::New {
            name: "late".into(),
            directory: w.dir(),
            session_id: None,
            trigger,
            action: ask("x"),
        },
        &au::Opts {
            expires: Some(expires.into()),
        },
    )
}

/// Settle every claimed run, as the scheduler would after its turn.
fn settle_claims(w: &World) {
    w.sql(
        "UPDATE automation_run SET phase = 'settled' WHERE phase IN ('claimed', 'checked', 'admitted')",
        [],
    );
}

/// Claim what is due at `at`, settling it, and return the ids claimed.
fn claim_at(w: &World, at: i64) -> Vec<String> {
    let ids = au::claim_due(&w.db, at)
        .unwrap()
        .into_iter()
        .map(|(a, _)| a.id)
        .collect();
    settle_claims(w);
    ids
}

fn set_expires(w: &World, id: &str, at: i64) {
    w.sql(
        "UPDATE automation SET expires = ?2 WHERE id = ?1",
        rusqlite::params![id, at],
    );
}

// automation.test.ts: "stops at expires" (adapted: claimed at chosen times)
#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_stops_at_expires() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let a = expiring(&w, every("10 minutes"), ask("poll"), "in 25 minutes");
    let t0 = a.created;
    assert_eq!(a.expires, Some(t0 + 25 * MINUTE));
    for k in 1..=3 {
        claim_at(&w, t0 + k * 10 * MINUTE);
    }
    let a = w.get(&a.id);
    assert_eq!((a.status.as_str(), a.runs), ("done", 2), "{a:?}");
    let e = try_expiring(
        &w,
        Trigger::At {
            at: "in 2 hours".into(),
            tz: None,
        },
        "in 1 hour",
    )
    .unwrap_err();
    assert_eq!(e.to_string(), "The automation expires before it first runs");
}

// automation.test.ts: "never runs a slot that only comes up after expires,
// after downtime or on resume" (adapted: claimed and resumed at chosen
// times)
#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_never_runs_a_slot_that_only_comes_up_after_expires() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let poll = expiring(&w, every("10 minutes"), ask("poll"), "in 25 minutes");
    let once = expiring(
        &w,
        Trigger::At {
            at: "in 10 minutes".into(),
            tz: None,
        },
        ask("once"),
        "in 20 minutes",
    );
    au::set_status(&w.db, &once.id, "paused").unwrap();
    let t = poll.created + 30 * MINUTE;
    assert!(claim_at(&w, t).is_empty());
    let p = w.get(&poll.id);
    assert_eq!(
        (p.status.as_str(), p.runs, p.last_outcome.as_deref()),
        ("done", 0, Some("expired"))
    );
    let o = au::set_status_at(&w.db, &once.id, "active", t).unwrap();
    assert_eq!((o.status.as_str(), o.runs), ("done", 0), "{o:?}");
    assert!(claim_at(&w, t).is_empty());
    assert!(w.phases(&poll.id).is_empty() && w.phases(&once.id).is_empty());
}

// Edge: the boundaries. A slot at the expiry time runs, and is the last;
// one past it is not. A slot run after the expiry still runs on time (up
// to a minute late), not from a minute late on. Creation needs the first
// run at or before the expiry.
#[tokio::test(flavor = "multi_thread")]
async fn expiry_boundaries() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    // The slot at +10 minutes, with the expiry exactly on it: it runs.
    let a = expiring(&w, every("10 minutes"), ask("on"), "in 10 minutes");
    let slot = a.created + 10 * MINUTE;
    assert_eq!(claim_at(&w, slot), std::slice::from_ref(&a.id));
    let b = w.get(&a.id);
    assert_eq!((b.status.as_str(), b.runs, b.next_due), ("done", 1, None));

    // An expiry a millisecond before the second slot: the first is the last.
    let a = expiring(&w, every("10 minutes"), ask("one"), "in 10 minutes");
    set_expires(&w, &a.id, a.created + 20 * MINUTE - 1);
    claim_at(&w, a.created + 10 * MINUTE);
    assert_eq!(w.get(&a.id).status, "done");
    // ...and one exactly on it leaves the second to run.
    let a = expiring(&w, every("10 minutes"), ask("two"), "in 20 minutes");
    claim_at(&w, a.created + 10 * MINUTE);
    assert_eq!(w.get(&a.id).status, "active");
    claim_at(&w, a.created + 20 * MINUTE);
    let b = w.get(&a.id);
    assert_eq!((b.status.as_str(), b.runs), ("done", 2));

    // Claimed after the expiry: on time just under a minute late, expired
    // from a minute on.
    for (late, runs, outcome) in [(MINUTE - 1, 1, None), (MINUTE, 0, Some("expired"))] {
        let a = expiring(&w, every("10 minutes"), ask("late"), "in 10 minutes");
        let slot = a.created + 10 * MINUTE;
        set_expires(&w, &a.id, slot);
        claim_at(&w, slot + late);
        let b = w.get(&a.id);
        assert_eq!((b.status.as_str(), b.runs), ("done", runs), "{late}");
        if outcome.is_some() {
            assert_eq!(b.last_outcome.as_deref(), outcome);
        }
    }

    // Resumed exactly at its expiry, it goes on; a millisecond after, it is done.
    let a = expiring(&w, every("10 minutes"), ask("r"), "in 30 minutes");
    let e = a.expires.unwrap();
    au::set_status(&w.db, &a.id, "paused").unwrap();
    let b = au::set_status_at(&w.db, &a.id, "active", e - 10 * MINUTE).unwrap();
    assert_eq!((b.status.as_str(), b.next_due), ("active", Some(e)));
    au::set_status(&w.db, &a.id, "paused").unwrap();
    let b = au::set_status_at(&w.db, &a.id, "active", e).unwrap();
    assert_eq!(b.status, "done", "its next slot is past the expiry");
    au::set_status(&w.db, &a.id, "paused").unwrap();
    let b = au::set_status_at(&w.db, &a.id, "active", e + 1).unwrap();
    assert_eq!(
        (b.status.as_str(), b.last_outcome.as_deref()),
        ("done", Some("expired"))
    );

    // Creation: a first run exactly at the expiry is fine; one after is not.
    let at = || Trigger::At {
        at: "in 10 minutes".into(),
        tz: None,
    };
    assert!(try_expiring(&w, at(), "in 10 minutes").is_ok());
    let e = try_expiring(&w, at(), "in 9 minutes").unwrap_err();
    assert_eq!(e.to_string(), "The automation expires before it first runs");
    // An unreadable expiry, and a watch that expired already.
    let e = try_expiring(&w, at(), "whenever").unwrap_err();
    assert!(e.to_string().starts_with("expires:"), "{e}");
    let watch = Trigger::Event {
        server: "rich".into(),
        event: "ticket.created".into(),
        args: None,
    };
    let past = chrono::Utc::now() - chrono::Duration::minutes(1);
    assert!(try_expiring(&w, watch.clone(), &past.to_rfc3339()).is_err());
    assert!(try_expiring(&w, watch, "in 1 hour").is_ok());
    // An automation without an expiry is unaffected.
    let a = w.prompt(None, "forever", helper());
    assert_eq!(w.get(&a).expires, None);
}

/// Leave run 1 of `id` claimed, as a process that stopped would, then let
/// the automation expire: its slot was two minutes ago, its expiry one.
fn claimed_then_expired(w: &World, id: &str) {
    w.due_now(id);
    assert_eq!(au::claim_due(&w.db, now()).unwrap().len(), 1);
    run_due(w, id, 1, now() - 2 * MINUTE);
    set_expires(w, id, now() - MINUTE);
    w.stale();
}

// Regression: a run left unsettled by a process that stopped, or waiting
// out its backoff, was handed off when it was picked up after the
// automation expired; it is failed now, and the automation is done.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_picked_up_after_the_expiry_is_not_handed_off() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let runner = w.runner();
    // Left claimed by a restart.
    let a = expiring(&w, every("1 hour"), ask("stale"), "in 3 hours");
    claimed_then_expired(&w, &a.id);
    runner.tick().await.unwrap();
    // Waiting out its backoff.
    let b = expiring(&w, every("1 hour"), ask("backoff"), "in 3 hours");
    refuse_writes(&w, &au::run_session_id(&b.id, 1));
    w.due_now(&b.id);
    runner.tick().await.unwrap();
    assert_eq!(w.phase(&b.id, 1).as_deref(), Some(au::CLAIMED));
    allow_writes(&w);
    run_due(&w, &b.id, 1, now() - 2 * MINUTE);
    set_expires(&w, &b.id, now() - MINUTE);
    retry_at(&w, &b.id, now() - 1);
    w.stale();
    runner.tick().await.unwrap();
    for id in [&a.id, &b.id] {
        let x = w.get(id);
        assert_eq!(
            (x.status.as_str(), x.runs, x.last_outcome.as_deref()),
            ("done", 1, Some("expired")),
            "{x:?}"
        );
        assert_eq!(w.phases(id), [(1, au::FAILED.to_string())]);
        assert_eq!(w.tagged(&au::message_id(id, 1)), 0);
        assert!(au::runs(&w.db, id).unwrap()[0].detail.contains("expired"));
    }
    assert_eq!(w.model.requests("stale") + w.model.requests("backoff"), 0);
    runner.release().unwrap();
}

// Across restarts: a program whose result was saved before the restart is
// not run again, nor handed off, after the expiry; an admitted run, handed
// off before it, finishes with one answer.
#[tokio::test(flavor = "multi_thread")]
async fn expiry_across_a_restart_keeps_checks_and_admissions() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let runner = w.runner();
    let checked = au::create_with(
        &w.db,
        au::New {
            name: "checked".into(),
            directory: w.dir(),
            session_id: None,
            trigger: every("1 hour"),
            action: Action::Code {
                code: "await tools.counter.post({ text: 'checked' }); return { state: 1, send: { prompt: 'late result', to: { subagent: 'helper' } } }".into(),
                args: None,
            },
        },
        &au::Opts {
            expires: Some("in 3 hours".into()),
        },
    )
    .unwrap();
    // The check runs, then its handoff is refused, leaving it checked.
    refuse_writes(&w, &au::run_session_id(&checked.id, 1));
    w.due_now(&checked.id);
    runner.tick().await.unwrap();
    assert_eq!(w.phase(&checked.id, 1).as_deref(), Some(au::CHECKED));
    allow_writes(&w);
    runner.release().unwrap();
    // An admitted run whose turn the restart interrupted.
    let admitted = expiring(&w, every("1 hour"), ask("hold the line"), "in 3 hours");
    w.model.hold();
    w.due_now(&admitted.id);
    let first = spawn_tick(w.runner());
    until("the admission", || {
        w.phase(&admitted.id, 1).as_deref() == Some(au::ADMITTED)
    })
    .await;
    first.kill(&w.db).await;
    w.model.release();
    for id in [&checked.id, &admitted.id] {
        run_due(&w, id, 1, now() - 2 * MINUTE);
        set_expires(&w, id, now() - MINUTE);
        retry_at(&w, id, now() - 1);
    }
    w.stale();
    let runner = w.runner();
    tick_until(&runner, "both runs to finish", || {
        au::pending(&w.db, &checked.id).unwrap().is_empty()
            && au::pending(&w.db, &admitted.id).unwrap().is_empty()
    })
    .await;
    assert_eq!(w.posts(), ["checked"], "the check is not run again");
    let c = w.get(&checked.id);
    assert_eq!(
        (
            c.status.as_str(),
            c.state.clone(),
            c.last_outcome.as_deref()
        ),
        ("done", serde_json::Value::Null, Some("expired"))
    );
    assert_eq!(w.tagged(&au::message_id(&checked.id, 1)), 0);
    assert_eq!(w.phase(&admitted.id, 1).as_deref(), Some(au::SETTLED));
    let child = au::run_session_id(&admitted.id, 1);
    assert_eq!(
        w.texts(&child)
            .iter()
            .filter(|t| t.starts_with("a:"))
            .count(),
        1
    );
    // The next claim finds it expired.
    w.due_now(&admitted.id);
    w.sql(
        "UPDATE automation SET next_due = ?2 WHERE id = ?1",
        rusqlite::params![admitted.id, now() - 2 * MINUTE],
    );
    runner.tick().await.unwrap();
    let a = w.get(&admitted.id);
    assert_eq!(
        (a.status.as_str(), a.runs, a.last_outcome.as_deref()),
        ("done", 1, Some("expired"))
    );
    runner.release().unwrap();
}

/// An event watch on the rich server following `plan`, prompting a new
/// helper session each event, that expires at `expires`.
fn watch_until(w: &World, plan: serde_json::Value, expires: &str) -> String {
    let file = w.data.path().join("plan.json");
    std::fs::write(&file, plan.to_string()).unwrap();
    expiring(
        w,
        Trigger::Event {
            server: "rich".into(),
            event: "ticket.created".into(),
            args: Some(json!({"queue": "plan", "plan": file.display().to_string()})),
        },
        ask("triage"),
        expires,
    )
    .id
}

fn streams(w: &World) -> usize {
    std::fs::read_to_string(w.data.path().join("plan.json.log"))
        .unwrap_or_default()
        .lines()
        .count()
}

// automation-event.test.ts: "stops the stream without a wake: ${stop}" (the
// expired row; adapted: the expiry is moved to now once the stream is open)
#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_stops_the_stream_without_a_wake_when_it_expires() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = watch_until(&w, json!([{"events": []}]), "in 1 hour");
    let runner = w.runner();
    tick_until(&runner, "the stream", || streams(&w) == 1).await;
    set_expires(&w, &id, now());
    tick_until(&runner, "the expiry", || w.get(&id).status != "active").await;
    let a = w.get(&id);
    assert_eq!(
        (
            a.status.as_str(),
            a.last_outcome.as_deref(),
            a.runs,
            a.state.clone()
        ),
        ("done", Some("expired"), 0, serde_json::Value::Null)
    );
    assert_eq!(streams(&w), 1);
    assert!(au::runs(&w.db, &id).unwrap().is_empty());
    assert_eq!(w.model.requests("triage"), 0);
    runner.tick().await.unwrap();
    assert_eq!(runner.watching(), 0);
    runner.release().unwrap();
}

// Regression: a watch on a stream that sends nothing ended only when the
// stream did (a minute of silence); it ends at its expiry now.
#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_watch_ends_at_its_expiry() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = watch_until(&w, json!([{"quiet": true}]), "in 1 hour");
    set_expires(&w, &id, now() + 1_500);
    let runner = w.runner();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while w.get(&id).status == "active" {
        assert!(
            std::time::Instant::now() < end,
            "it ends at the expiry, before the stream's minute of silence"
        );
        runner.tick().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let a = w.get(&id);
    assert_eq!(
        (a.status.as_str(), a.last_outcome.as_deref(), a.runs),
        ("done", Some("expired"), 0)
    );
    assert!(streams(&w) <= 1);
    runner.release().unwrap();
}

// Across restarts: a watch whose expiry passed while no process ran ends
// without opening its stream.
#[tokio::test(flavor = "multi_thread")]
async fn a_watch_that_expired_while_stopped_ends_without_opening_its_stream() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = watch_until(&w, json!([{"events": [["e1", "c1", "one"]]}]), "in 1 hour");
    set_expires(&w, &id, now() - 1);
    let runner = w.runner();
    tick_until(&runner, "the expiry", || w.get(&id).status != "active").await;
    let a = w.get(&id);
    assert_eq!(
        (a.status.as_str(), a.last_outcome.as_deref(), a.runs),
        ("done", Some("expired"), 0)
    );
    assert_eq!(streams(&w), 0);
    runner.release().unwrap();
}

/// What the model's last tool call returned.
fn tool_result(h: &support::H) -> String {
    let req = h.requests().pop().unwrap();
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .unwrap()
}

// The tool takes an expiry, shows it in the approval and the listing, and
// refuses one before the first run.
#[tokio::test(flavor = "multi_thread")]
async fn the_tool_creates_an_automation_that_expires() {
    let mut h = support::H::with(
        json!({"permissions": [{"action": "automation", "resource": "*", "effect": "allow"}]}),
        None,
    )
    .await;
    let input = json!({"action": "create", "name": "poll", "prompt": "poll", "trigger": {"type": "every", "every": "10 minutes"}, "expires": "in 25 minutes"});
    h.prompt(&format!("call automation {input}")).await;
    let out = tool_result(&h);
    assert!(out.starts_with("Created aut"), "{out}");
    let id = out.split_whitespace().nth(1).unwrap().to_string();
    let a = au::get(&h.store(), &id).unwrap().unwrap();
    assert_eq!(a.expires, Some(a.created + 25 * MINUTE));
    h.prompt(r#"call automation {"action": "list"}"#).await;
    assert!(tool_result(&h).contains("until"), "{}", tool_result(&h));
    let bad = json!({"action": "create", "name": "late", "prompt": "x", "trigger": {"type": "at", "at": "in 2 hours"}, "expires": "in 1 hour"});
    h.prompt(&format!("call automation {bad}")).await;
    assert_eq!(
        tool_result(&h),
        "The automation expires before it first runs"
    );
    assert_eq!(au::list(&h.store(), None).unwrap().len(), 1);
}

// ----- Automation Tool Extraction --------------------------------------------

fn loaded(w: &World) -> codesmith_core::config::Loaded {
    (w.loader())(w.project.path()).unwrap()
}

async fn authorize(w: &World, session: Option<&str>, code: &str) -> anyhow::Result<Vec<String>> {
    au::authorize_code(&w.db, &loaded(w), session, code).await
}

/// Run `id` once now and wait for the run to finish.
async fn run_once(w: &World, runner: &au::Runner, id: &str) {
    let before = w.phases(id).len();
    w.due_now(id);
    tick_until(runner, "the run", || {
        let p = w.phases(id);
        p.len() > before && p.iter().all(|(_, p)| p == au::SETTLED || p == au::FAILED)
    })
    .await;
}

// automation.test.ts: "extracts tools at creation, asks once without save,
// and runs unattended after" (adapted: a program is created by the user
// from the CLI, which checks the tools read from it then;
// `automation.permissions` is the grant for unattended runs, and nothing
// asks at run time)
#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_extracts_tools_at_creation_and_runs_unattended_after() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let code = "await tools.counter.post({ text: 'run ' + input.runs })\nif (input.runs > 99) await tools.counter.wait({ until: 'never' })\nawait tools.counter.post({ text: 'again ' + input.runs })\nreturn";
    assert_eq!(
        authorize(&w, Some(&creator), code).await.unwrap(),
        ["counter.post", "counter.wait"]
    );
    let id = w.code(Some(&creator), code, None);
    assert_eq!(
        w.get(&id).tools,
        Some(vec!["counter.post".to_string(), "counter.wait".to_string()])
    );
    let runner = w.runner();
    run_once(&w, &runner, &id).await;
    run_once(&w, &runner, &id).await;
    assert_eq!(w.posts(), ["run 1", "again 1", "run 2", "again 2"]);
    let a = w.get(&id);
    assert_eq!((a.runs, a.last_outcome.as_deref()), (2, Some("fired")));
    runner.release().unwrap();
}

// automation.test.ts: "asks for resource-scoped ask rules at creation, and
// still needs a person for them at run time" (adapted: an MCP call has no
// resource, so a rule that asks about the tool refuses the program at
// creation, a rule scoped to resources does not apply to it, and one that
// asks added after creation fails the run before the program runs)
#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_checks_ask_rules_at_creation_and_still_needs_a_person_at_run_time() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let code = "await tools.counter.post({ text: 'hi' })";
    let asks = w.creator(json!([{"action": "counter_post", "resource": "*", "effect": "ask"}]));
    let e = authorize(&w, Some(&asks), code).await.unwrap_err();
    assert!(e.to_string().contains("needs approval"), "{e}");
    let scoped =
        w.creator(json!([{"action": "counter_post", "resource": "vault:*", "effect": "ask"}]));
    assert_eq!(
        authorize(&w, Some(&scoped), code).await.unwrap(),
        ["counter.post"]
    );

    // The creator asks about the tool only after the automation was made.
    let creator = w.creator(json!([]));
    let id = w.code(Some(&creator), code, None);
    let rules: Vec<codesmith_core::permission::Rule> = serde_json::from_value(
        json!([{"action": "counter_post", "resource": "*", "effect": "ask"}]),
    )
    .unwrap();
    w.db.set_permissions(&creator, &rules).unwrap();
    let runner = w.runner();
    run_once(&w, &runner, &id).await;
    assert!(w.posts().is_empty());
    let a = w.get(&id);
    assert_eq!((a.runs, a.failures, a.status.as_str()), (1, 1, "active"));
    assert!(a
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("needs approval")));
    runner.release().unwrap();
}

// automation.test.ts: "asks for execute when its tools are already allowed"
// (adapted: running the program is the `execute` action; a rule that asks
// about it or denies it refuses the program at creation, tools allowed or
// not, since nobody is there to approve it at run time)
#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_checks_execute_when_its_tools_are_already_allowed() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    for (effect, says) in [("ask", "needs approval"), ("deny", "denied")] {
        let s = w.creator(json!([{"action": "execute", "resource": "*", "effect": effect}]));
        for code in [
            "await tools.counter.post({ text: 'x' })",
            "return { state: 1 }",
        ] {
            let e = authorize(&w, Some(&s), code).await.unwrap_err();
            assert!(
                e.to_string().contains("execute") && e.to_string().contains(says),
                "{effect} {code}: {e}"
            );
        }
    }
    assert!(w.posts().is_empty());
}

// automation.test.ts: "rejects dynamic tool access, unknown tools, shell,
// recursive automation tools and denied permissions" (adapted: the
// shortest interval is a minute)
#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_rejects_dynamic_access_unknown_tools_shell_recursive_tools_and_denies() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let create = |code: &str| {
        au::create(
            &w.db,
            au::New {
                name: "bad".into(),
                directory: w.dir(),
                session_id: None,
                trigger: every("1 minute"),
                action: Action::Code {
                    code: code.into(),
                    args: None,
                },
            },
        )
    };
    for (code, says) in [
        ("const x = {", "SyntaxError"),
        ("return await tools.counter[name]({})", "statically"),
        ("const c = tools.counter; await c.post({})", "not alias"),
        (
            r#"await tools.shell({ command: "pwd" })"#,
            "Tool shell cannot be used inside an automation",
        ),
        (
            "return await tools.automation({})",
            "Tool automation cannot be used inside an automation",
        ),
    ] {
        let e = create(code).unwrap_err().to_string();
        assert!(e.contains(says), "{code}: {e}");
        let e = authorize(&w, None, code).await.unwrap_err().to_string();
        assert!(e.contains(says), "{code}: {e}");
    }
    let e = authorize(&w, None, "return await tools.counter.missing({})")
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "Unknown Code Mode tool: counter.missing");
    let fast = au::create(
        &w.db,
        au::New {
            name: "fast".into(),
            directory: w.dir(),
            session_id: None,
            trigger: every("10 seconds"),
            action: Action::Code {
                code: "return".into(),
                args: None,
            },
        },
    );
    assert!(fast.is_err());
    let denies = w.creator(json!([{"action": "counter_post", "resource": "*", "effect": "deny"}]));
    let e = authorize(&w, Some(&denies), "await tools.counter.post({ text: 'x' })")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("denied"), "{e}");
    // A tool no opt-in allows is denied in unattended runs.
    let e = authorize(&w, None, "await tools.rich.grow({})")
        .await
        .unwrap_err();
    assert!(e.to_string().contains("automation.permissions"), "{e}");
    assert!(au::list(&w.db, None).unwrap().is_empty());
}

// Regression: a run was offered every MCP tool the rules allow; it is
// offered only those read from the program when it was created, so a
// program changed since (or one whose calls the scan missed) cannot reach
// another.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_not_read_from_the_program_is_not_offered_at_run_time() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let id = w.code(None, "await tools.counter.post({ text: 'ok' })", None);
    let ready = w.data.path().join("ready");
    std::fs::write(&ready, "").unwrap();
    let changed = Action::Code {
        code: format!(
            "await tools.counter.wait({{ until: {:?} }})\nawait tools.counter.post({{ text: 'changed' }})",
            ready.display().to_string()
        ),
        args: None,
    };
    w.sql(
        "UPDATE automation SET action = ?2 WHERE id = ?1",
        rusqlite::params![id, serde_json::to_string(&changed).unwrap()],
    );
    let runner = w.runner();
    run_once(&w, &runner, &id).await;
    assert!(w.posts().is_empty(), "{:?}", w.posts());
    let a = w.get(&id);
    assert_eq!((a.failures, a.last_outcome.as_deref()), (1, Some("failed")));
    runner.release().unwrap();
}

// Regression: a program whose calls cannot be read was created and failed
// at its runs; it is refused when created.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_whose_calls_cannot_be_read_is_refused_at_creation() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let r = au::create(
        &w.db,
        au::New {
            name: "dynamic".into(),
            directory: w.dir(),
            session_id: None,
            trigger: every("1 hour"),
            action: Action::Code {
                code: "for (const n of ['post']) await tools.counter[n]({ text: 'x' })".into(),
                args: None,
            },
        },
    );
    assert!(r.is_err());
    assert!(au::list(&w.db, None).unwrap().is_empty());
}

// Edge: an automation stored before its tools were recorded has them read
// at each run: a program whose calls can be read runs with only those, and
// one whose calls cannot is paused at once, without running.
#[tokio::test(flavor = "multi_thread")]
async fn a_program_stored_before_its_tools_were_recorded_is_read_at_its_run() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let fine = w.code(None, "await tools.counter.post({ text: 'legacy' })", None);
    let dynamic = w.code(None, "await tools.counter.post({ text: 'x' })", None);
    let code = Action::Code {
        code: "const t = tools.counter; await t.post({ text: 'dynamic' })".into(),
        args: None,
    };
    w.sql(
        "UPDATE automation SET action = ?2 WHERE id = ?1",
        rusqlite::params![dynamic, serde_json::to_string(&code).unwrap()],
    );
    w.sql("UPDATE automation SET tools = NULL", []);
    assert_eq!(w.get(&fine).tools, None);
    let runner = w.runner();
    run_once(&w, &runner, &fine).await;
    run_once(&w, &runner, &dynamic).await;
    assert_eq!(w.posts(), ["legacy"]);
    let d = w.get(&dynamic);
    assert_eq!(d.status, "paused");
    assert!(d
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("not alias")));
    runner.release().unwrap();
}

// Edge: at run time, a tool that is gone (its server removed from the
// config, or down) or that a rule added since denies fails the run as a
// strike, before the program runs, and it is tried again at the next slot.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_gone_or_denied_since_creation_fails_the_run_before_it_runs() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let id = w.code(
        Some(&creator),
        "await tools.counter.post({ text: 'before' })\nawait tools.rich.grow({})",
        None,
    );
    // rich's tools are not opted in: refused before `counter.post` runs.
    let runner = w.runner();
    run_once(&w, &runner, &id).await;
    assert!(w.posts().is_empty());
    assert!(w
        .get(&id)
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("rich.grow")));
    // Gone: a program calling a server the config no longer has.
    let gone = w.code(
        Some(&creator),
        "await tools.counter.post({ text: 'gone' })",
        None,
    );
    w.sql(
        "UPDATE automation SET tools = '[\"counter.post\",\"old.tool\"]' WHERE id = ?1",
        [&gone],
    );
    run_once(&w, &runner, &gone).await;
    let g = w.get(&gone);
    assert_eq!((g.status.as_str(), g.failures), ("active", 1));
    assert!(g
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("Unknown Code Mode tool: old.tool")));
    assert!(w.posts().is_empty());
    runner.release().unwrap();
}

// ----- Automation Command ----------------------------------------------------

/// An asker that records what it was asked and answers `reply`.
struct Recording {
    asked: std::sync::Mutex<Vec<codesmith_core::permission::Request>>,
    reply: codesmith_core::permission::Reply,
}

impl codesmith_core::permission::Asker for Recording {
    fn ask(&self, req: &codesmith_core::permission::Request) -> codesmith_core::permission::Reply {
        self.asked.lock().unwrap().push(req.clone());
        self.reply.clone()
    }
}

/// The model's answer to an `/automation` prompt: it calls the automation
/// tool to create what was asked for.
fn creates_on_request() -> support::Reply_ {
    std::sync::Arc::new(|body: &serde_json::Value| {
        let last = body["messages"].as_array()?.last()?.clone();
        let text = last["content"].as_str().unwrap_or("");
        if last["role"] != "user" || !text.contains("Request: every morning at 9") {
            return None;
        }
        let args = json!({"action": "create", "name": "ci", "prompt": "Check CI and report failures", "trigger": {"type": "cron", "cron": "0 9 * * *"}});
        Some(
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "automation", "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}]}),
        )
    })
}

/// A stored session in `h`'s project with this asker, on `agent`.
fn session_with(
    h: &support::H,
    asker: std::sync::Arc<dyn codesmith_core::permission::Asker>,
    agent: Option<&str>,
) -> codesmith_core::session::Session {
    codesmith_core::session::Session::new(
        &h.loaded,
        h.root(),
        codesmith_core::session::Options {
            agent: agent.map(str::to_string),
            model: None,
            resume: None,
            title: Some("Parent".into()),
            parent: None,
            id: None,
            questioner: None,
            mcp: None,
        },
        asker,
        Some(h.store()),
    )
    .unwrap()
}

/// Run `/automation <args>` in `s` as a front end does.
async fn run_automation_command(
    h: &support::H,
    s: &mut codesmith_core::session::Session,
    line: &str,
) {
    use codesmith_core::command;
    let all = command::discover(
        command::builtins(&h.loaded.root),
        &[],
        &h.loaded.config.commands,
    );
    let (name, args) = command::find_invocation(line, &all).unwrap();
    assert_eq!(name, "automation");
    s.run_command(line, &all[&name], &args, vec![], &mut support::Quiet)
        .await
        .unwrap();
}

fn user_texts(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| match &m["content"] {
            serde_json::Value::String(s) => s.clone(),
            v => v[0]["text"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

// tool-automation.test.ts: "/automation hands the request to the model with
// the automation instructions" (adapted: a built-in command run in the
// session, which the model answers)
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_automation_hands_the_request_to_the_model_with_the_automation_instructions() {
    let mut h = support::H::new().await;
    let line = "/automation   every morning at 9 check CI  ";
    let all = codesmith_core::command::builtins(&h.loaded.root);
    let (name, args) = codesmith_core::command::find_invocation(line, &all).unwrap();
    h.s.run_command(line, &all[&name], &args, vec![], &mut support::Quiet)
        .await
        .unwrap();
    let reqs = h.requests();
    assert_eq!(reqs.len(), 1);
    let texts = user_texts(&reqs[0]);
    let asked: Vec<&String> = texts.iter().filter(|t| t.contains("Request:")).collect();
    assert_eq!(asked.len(), 1, "{texts:?}");
    let text = asked[0];
    assert!(text.contains("every morning at 9 check CI"), "{text}");
    assert!(!text.contains("$ARGUMENTS"), "{text}");
    assert!(
        text.contains("`automation` tool") && text.contains("expires"),
        "{text}"
    );
    assert!(!text.to_lowercase().contains("opencode"));
    // The model gets the tool to act on it with.
    assert!(reqs[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["name"] == "automation"));
}

// The command is only a prompt: what it leads to goes through the
// automation tool, which asks, refuses on a decline, honours a deny and is
// not offered to an agent denied it, as when the model calls it unbidden.
#[tokio::test(flavor = "multi_thread")]
async fn the_automation_command_cannot_exceed_the_automation_tools_permissions() {
    use codesmith_core::permission::Reply;
    let line = "/automation every morning at 9 check CI";
    let recording = |reply: Reply| {
        std::sync::Arc::new(Recording {
            asked: Default::default(),
            reply,
        })
    };

    // Asked, and declined: nothing is created.
    let h = support::H::with(json!({}), Some(creates_on_request())).await;
    let asker = recording(Reply::Reject(None));
    let mut s = session_with(&h, asker.clone(), None);
    run_automation_command(&h, &mut s, line).await;
    let asked = asker.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(
        (asked[0].action.as_str(), asked[0].resources.as_slice()),
        ("automation", &["create".to_string()][..])
    );
    assert!(
        asked[0].title.contains("Check CI and report failures"),
        "{}",
        asked[0].title
    );
    assert!(au::list(&h.store(), None).unwrap().is_empty());
    // A declined request ends the turn; the tool's answer is in the session.
    let history = format!("{:?}", h.store().history(&s.id).unwrap());
    assert!(history.contains("declined"), "{history}");

    // Asked, and approved: created, as through the tool itself.
    let asker = recording(Reply::Once);
    let mut s = session_with(&h, asker.clone(), None);
    run_automation_command(&h, &mut s, line).await;
    assert_eq!(asker.asked.lock().unwrap().len(), 1);
    let made = au::list(&h.store(), None).unwrap();
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].session_id.as_deref(), Some(s.id.as_str()));

    // A rule denying it wins over an approval, and nothing is asked.
    let h = support::H::with(
        json!({"permissions": [{"action": "automation", "resource": "create", "effect": "deny"}]}),
        Some(creates_on_request()),
    )
    .await;
    let asker = recording(Reply::Always);
    let mut s = session_with(&h, asker.clone(), None);
    run_automation_command(&h, &mut s, line).await;
    assert!(asker.asked.lock().unwrap().is_empty());
    assert!(au::list(&h.store(), None).unwrap().is_empty());
    assert!(
        tool_result(&h).contains("Permission denied"),
        "{}",
        tool_result(&h)
    );

    // An agent denied the tool is not offered it, so the command can't reach it.
    let h = support::H::with(
        json!({"agents": {"build": {"permissions": [{"action": "automation", "resource": "*", "effect": "deny"}]}}}),
        Some(creates_on_request()),
    )
    .await;
    let asker = recording(Reply::Always);
    let mut s = session_with(&h, asker.clone(), Some("build"));
    run_automation_command(&h, &mut s, line).await;
    let first = &h.requests()[0];
    assert!(!first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["function"]["name"] == "automation"));
    assert!(asker.asked.lock().unwrap().is_empty());
    assert!(au::list(&h.store(), None).unwrap().is_empty());
}

// Edge: an empty request still sends the instructions, with nothing left
// of the placeholder; and a project's own `/automation` replaces it.
#[tokio::test(flavor = "multi_thread")]
async fn the_automation_command_with_no_request_or_overridden() {
    let mut h = support::H::new().await;
    let all = codesmith_core::command::builtins(&h.loaded.root);
    let (name, args) = codesmith_core::command::find_invocation("/automation", &all).unwrap();
    assert_eq!((name.as_str(), args.as_str()), ("automation", ""));
    h.s.run_command(
        "/automation",
        &all[&name],
        &args,
        vec![],
        &mut support::Quiet,
    )
    .await
    .unwrap();
    let texts = user_texts(&h.requests()[0]);
    let text = texts.last().unwrap();
    assert!(
        text.contains("Request: \n") && text.contains("If the request is empty"),
        "{text}"
    );
    assert!(
        au::list(&h.store(), None).unwrap().is_empty(),
        "nothing is created by the command itself"
    );

    let configured: std::collections::BTreeMap<String, codesmith_core::command::Command> = [(
        "automation".to_string(),
        codesmith_core::command::Command {
            template: "Mine: $ARGUMENTS".into(),
            ..Default::default()
        },
    )]
    .into();
    let all = codesmith_core::command::discover(
        codesmith_core::command::builtins(&h.loaded.root),
        &[],
        &configured,
    );
    assert_eq!(all["automation"].template, "Mine: $ARGUMENTS");
}
