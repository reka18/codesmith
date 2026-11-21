//! Ports of OpenCode's durable handoff tests (`automation-event.test.ts`,
//! `automation.test.ts`, the PARITY group "Automation Durable Handoffs").
//!
//! Adapted where CodeSmith differs: a run is counted when it is claimed (so
//! `runs` includes runs whose handoff was refused), CodeSmith automations
//! have no expiry, and a handoff is the prompt written into the target
//! session in the same transaction that commits the automation's state;
//! OpenCode's "wake" is the turn that then runs.

mod handoff_support;

use codesmith_core::automation::{self as au, Action, Target, Trigger};
use handoff_support::*;
use serde_json::{json, Value};

/// An event automation on the `rich` server following `plan`.
fn watch_plan(w: &World, session: Option<&str>, plan: Value, action: Action) -> String {
    let file = w.data.path().join("plan.json");
    std::fs::write(&file, plan.to_string()).unwrap();
    w.create(
        session,
        Trigger::Event {
            server: "rich".into(),
            event: "ticket.created".into(),
            args: Some(json!({"queue": "plan", "plan": file.display().to_string()})),
        },
        action,
    )
}

fn stream_cursors(w: &World) -> Vec<Value> {
    std::fs::read_to_string(w.data.path().join("plan.json.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn user_prompts(w: &World, id: &str) -> Vec<String> {
    w.texts(id)
        .into_iter()
        .filter(|t| t.starts_with("u:"))
        .collect()
}

async fn settles_a_durable_handoff(outcome: &str) {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let id = watch_plan(
        &w,
        Some(&creator),
        json!([{"events": [["first", "c1", "one"]]}, {"events": []}]),
        Action::Code {
            code: format!(
                "return {{ state: input.event.id, send: {{ prompt: 'Delivered' }}, done: {} }}",
                outcome == "done"
            ),
            args: None,
        },
    );
    if outcome == "reject" {
        // The handoff's write fails (OpenCode's failed wake before commit).
        w.sql(
            &format!("CREATE TRIGGER refuse BEFORE INSERT ON record WHEN NEW.session_id = '{creator}' BEGIN SELECT RAISE(ABORT, 'Wake failed'); END"),
            [],
        );
    }
    let runner = w.runner();
    tick_until(&runner, "the run to settle", || {
        let a = w.get(&id);
        a.runs == 1
            && au::pending(&w.db, &id).unwrap().is_empty()
            && (outcome != "reject" || a.status == "paused")
    })
    .await;
    let a = w.get(&id);
    let pos = a.event.clone().unwrap_or_default();
    if outcome == "reject" {
        assert_eq!(
            (
                a.status.as_str(),
                a.last_outcome.as_deref(),
                a.state.clone()
            ),
            ("paused", Some("failed"), Value::Null)
        );
        assert!(
            a.last_error
                .as_deref()
                .is_some_and(|e| e.contains("Wake failed")),
            "{a:?}"
        );
        assert_ne!(pos.id.as_deref(), Some("first"), "the cursor did not move");
        assert_ne!(pos.cursor.as_deref(), Some("c1"));
        assert!(user_prompts(&w, &creator).is_empty());
    } else {
        let status = if outcome == "done" { "done" } else { "active" };
        assert_eq!(
            (
                a.status.as_str(),
                a.last_outcome.as_deref(),
                a.state.clone()
            ),
            (status, Some("fired"), json!("first"))
        );
        assert_eq!(
            (pos.cursor.as_deref(), pos.id.as_deref()),
            (Some("c1"), Some("first"))
        );
        assert_eq!(user_prompts(&w, &creator), ["u:Delivered"]);
    }
    au::remove(&w.db, &id).unwrap();
    runner.tick().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_settles_a_durable_handoff_continue() {
    settles_a_durable_handoff("continue").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_settles_a_durable_handoff_done() {
    settles_a_durable_handoff("done").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_settles_a_durable_handoff_reject() {
    settles_a_durable_handoff("reject").await;
}

async fn checkpoints_an_accepted_handoff_across_a_pause(local: bool) {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let action = if local {
        Action::Code {
            code: "return { state: input.event.id, send: { prompt: 'hold: Delivered' } }".into(),
            args: None,
        }
    } else {
        Action::Prompt {
            prompt: "hold: Review.".into(),
            to: Target::Session {
                session: creator.clone(),
            },
        }
    };
    let id = watch_plan(
        &w,
        Some(&creator),
        json!([{"events": [["first", "c1", "one"]]}, {"events": []}]),
        action,
    );
    w.model.hold();
    let runner = w.runner();
    tick_until(&runner, "the admitted turn", || {
        w.model.requests("hold") == 1
    })
    .await;
    assert_eq!(
        user_prompts(&w, &creator).len(),
        1,
        "the handoff is durable"
    );
    if local {
        au::set_status(&w.db, &id, "paused").unwrap();
    } else {
        // Another process pauses it.
        w.sql(
            "UPDATE automation SET status = 'paused' WHERE id = ?1",
            [&id],
        );
        runner.tick().await.unwrap();
    }
    w.model.release();
    until("the accepted run to settle", || {
        au::pending(&w.db, &id).unwrap().is_empty()
    })
    .await;
    tick_until(&runner, "the paused watch to stop", || {
        runner.watching() == 0
    })
    .await;
    let a = w.get(&id);
    assert_eq!(
        (a.status.as_str(), a.runs, a.last_outcome.as_deref()),
        ("paused", 1, Some("fired"))
    );
    assert_eq!(a.state, if local { json!("first") } else { Value::Null });
    au::set_status(&w.db, &id, "active").unwrap();
    tick_until(&runner, "the watch to reopen", || {
        stream_cursors(&w).len() == 2
    })
    .await;
    assert_eq!(stream_cursors(&w)[1], json!("c1"));
    assert_eq!(user_prompts(&w, &creator).len(), 1, "nothing ran twice");
    au::remove(&w.db, &id).unwrap();
    runner.tick().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_checkpoints_an_accepted_handoff_across_a_local_pause() {
    checkpoints_an_accepted_handoff_across_a_pause(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_checkpoints_an_accepted_handoff_across_a_reconciled_pause() {
    checkpoints_an_accepted_handoff_across_a_pause(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_event_does_not_checkpoint_a_rejected_handoff_or_commit_its_terminal_state() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let id = watch_plan(
        &w,
        Some(&creator),
        json!([
            {"events": [["p1", "c1", "pending"], ["t2", "c2", "failure"]]},
            {"events": []}
        ]),
        Action::Code {
            code: "return { state: { first: input.event.id }, done: input.event.data.title === 'failure', send: { prompt: 'hold: triage', to: { subagent: 'helper' } } }".into(),
            args: None,
        },
    );
    w.model.hold();
    let runner = w.runner();
    tick_until(&runner, "the first handoff", || {
        w.model.requests("hold") == 1
    })
    .await;
    // The creating session's policy changes before the next event.
    w.db.set_permissions(
        &creator,
        &serde_json::from_value::<Vec<_>>(json!([
            {"action": "*", "resource": "*", "effect": "allow"},
            {"action": "subagent", "resource": "helper", "effect": "deny"}
        ]))
        .unwrap(),
    )
    .unwrap();
    w.model.release();
    tick_until(&runner, "the pause", || w.get(&id).status == "paused").await;
    let a = w.get(&id);
    assert_eq!(a.last_outcome.as_deref(), Some("failed"));
    assert_eq!(
        a.last_error.as_deref(),
        Some("Paused: Cannot start subagent helper: Permission denied: subagent")
    );
    assert_eq!(a.state, json!({"first": "p1"}));
    // Runs are counted at claim: p1, and t2 whose handoff was refused.
    assert_eq!(a.runs, 2);
    assert_eq!(au::runs(&w.db, &id).unwrap()[1].outcome, "failed");
    assert_eq!(w.children(&creator).len(), 1, "no child for t2");
    au::set_status(&w.db, &id, "active").unwrap();
    tick_until(&runner, "the watch to reopen", || {
        stream_cursors(&w).len() == 2
    })
    .await;
    assert_eq!(stream_cursors(&w)[1], json!("c1"));
    au::remove(&w.db, &id).unwrap();
    runner.tick().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_hands_a_failed_check_to_one_quiet_child_with_the_creators_permissions() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let rules = json!([{"action": "edit", "resource": "*", "effect": "deny"}]);
    let creator = w.creator(rules.clone());
    let id = w.code(
        Some(&creator),
        "const t = await tools.counter.post({ text: 'read thread' }); if (t.startsWith('posted')) return { send: { to: { subagent: 'helper' }, prompt: 'Investigate the failed check' } }",
        None,
    );
    w.due_now(&id);
    w.runner().tick().await.unwrap();
    let child = au::run_session_id(&id, 1);
    assert_eq!(w.children(&creator), std::slice::from_ref(&child));
    let info = w.db.get(&child).unwrap().unwrap();
    assert_eq!(
        (info.agent.as_str(), info.parent_id.as_deref()),
        ("helper", Some(creator.as_str()))
    );
    assert_eq!(
        serde_json::to_value(w.db.permissions(&child).unwrap().unwrap()).unwrap(),
        rules
    );
    assert_eq!(
        w.texts(&child),
        [
            "u:Investigate the failed check",
            "a:answer to: Investigate the failed check"
        ]
    );
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 1);
    assert_eq!(w.posts().len(), 1);
    let a = w.get(&id);
    assert_eq!((a.status.as_str(), a.runs), ("active", 1));
    let detail = &au::runs(&w.db, &id).unwrap()[0].detail;
    assert!(
        detail.starts_with(&format!("Started subagent {child}")),
        "{detail}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_keeps_the_handoff_visible_and_logs_within_the_utf8_output_budget() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let owner = w.session_with(&format!("ses_{}", "x".repeat(128)), None, json!([]));
    let id = w.code(
        Some(&owner),
        "console.log('界'.repeat(10900)); return { send: { prompt: 'ready' } }",
        None,
    );
    w.due_now(&id);
    w.runner().tick().await.unwrap();
    let detail = &au::runs(&w.db, &id).unwrap()[0].detail;
    assert!(
        detail.starts_with(&format!("Prompted {owner}\n界")),
        "{}",
        &detail[..80]
    );
    assert!(detail.contains("界…"));
    assert!(
        detail.ends_with("answer to: ready"),
        "the reply stays visible"
    );
    assert!(detail.len() <= 32 * 1024, "{}", detail.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_bounds_failed_code_output_including_the_pause_reason_in_utf8_bytes() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let id = w.code(
        Some(&creator),
        "console.log('starting check')\nthrow new Error('check failed: ' + '\\uD83E\\uDDEA'.repeat(20000))",
        None,
    );
    let runner = w.runner();
    for strike in 1..=3 {
        w.due_now(&id);
        runner.tick().await.unwrap();
        let a = w.get(&id);
        let output = a.last_error.clone().unwrap_or_default();
        assert!(output.len() <= 32 * 1024, "{}", output.len());
        assert_eq!(
            a.status,
            if strike == 3 { "paused" } else { "active" },
            "strike {strike}"
        );
        assert!(
            output.contains("check failed: "),
            "{}",
            &output[..output.len().min(300)]
        );
        assert!(output.ends_with("🧪…"));
        if strike == 3 {
            assert!(
                output.starts_with("Paused after 3 failed runs: "),
                "{}",
                &output[..60]
            );
        }
        let detail = &au::runs(&w.db, &id).unwrap()[strike - 1].detail;
        assert!(detail.len() <= 32 * 1024);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_bad_targets_and_session_or_agent_policy_never_deliver_or_commit_the_rest_of_the_result(
) {
    let Some(w) = World::new(
        json!({"permissions": [{"action": "subagent", "resource": "helper", "effect": "ask"}]}),
    )
    .await
    else {
        return;
    };
    let creator = w.creator(json!([]));
    let other = w.creator(json!([]));
    let code = "if (input.runs === 1) return { state: 'before' }; return { state: 'after', done: true, send: { prompt: 'act', to: input.args } }";
    let targets = [
        json!({"session": other}),
        json!({"subagent": "internal"}),
        json!({"subagent": "build"}),
        json!({"subagent": "missing"}),
        json!({"subagent": "helper"}),
    ];
    let rows: Vec<String> = targets
        .iter()
        .map(|to| w.code(Some(&creator), code, Some(to.clone())))
        .collect();
    // A nested session that allows the child itself: depth alone blocks it.
    let nested = w.session(
        Some(&creator),
        json!([{"action": "subagent", "resource": "helper", "effect": "allow"}]),
    );
    let deep = w.code(Some(&nested), code, Some(json!({"subagent": "helper"})));
    let denied = w.creator(json!([{"action": "subagent", "resource": "helper", "effect": "deny"}]));
    let blocked = w.code(Some(&denied), code, Some(json!({"subagent": "helper"})));
    let runner = w.runner();
    let all: Vec<&String> = rows.iter().chain([&deep, &blocked]).collect();
    for _ in 0..2 {
        for id in &all {
            w.due_now(id);
        }
        runner.tick().await.unwrap();
    }
    for id in &all {
        let a = w.get(id);
        assert_eq!(
            (
                a.status.as_str(),
                a.last_outcome.as_deref(),
                a.state.clone()
            ),
            ("paused", Some("failed"), json!("before")),
            "{a:?}"
        );
        assert!(a.last_error.as_deref().unwrap().starts_with("Paused:"));
        assert!(au::pending(&w.db, id).unwrap().is_empty());
        assert_eq!(w.tagged(&au::message_id(id, 2)), 0);
    }
    let error = |id: &str| w.get(id).last_error.unwrap();
    assert_eq!(error(&deep), "Paused: Subagent depth limit reached (1)");
    assert_eq!(
        error(&blocked),
        "Paused: Cannot start subagent helper: Permission denied: subagent"
    );
    let errors: Vec<String> = rows.iter().map(|r| error(r)).collect();
    assert!(
        errors[0].contains("must return nothing or"),
        "{}",
        errors[0]
    );
    assert_eq!(errors[1], "Paused: Subagent internal is hidden");
    assert_eq!(errors[2], "Paused: Agent build is primary-only");
    assert_eq!(errors[3], "Paused: Unknown subagent: missing");
    assert!(
        errors[4].contains("subagent helper: Permission subagent needs approval"),
        "{}",
        errors[4]
    );
    assert_eq!(w.children(&creator), std::slice::from_ref(&nested));
    assert!(w.children(&nested).is_empty());
    assert!(w.children(&denied).is_empty());
    assert!(w.texts(&other).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_retries_failed_admission_without_committing_state_or_done_then_settles_the_handoff_once(
) {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let id = w.code(
        Some(&creator),
        "await tools.counter.post({ text: 'checked' }); return { state: 42, done: true, send: { prompt: 'ready' } }",
        None,
    );
    // The handoff's write fails (OpenCode: a conflicting inbox item).
    w.sql(
        &format!("CREATE TRIGGER refuse BEFORE INSERT ON record WHEN NEW.session_id = '{creator}' BEGIN SELECT RAISE(ABORT, 'conflict'); END"),
        [],
    );
    w.due_now(&id);
    let runner = w.runner();
    runner.tick().await.unwrap();
    let a = w.get(&id);
    assert_eq!(
        (
            a.status.as_str(),
            a.state.clone(),
            a.last_outcome.as_deref()
        ),
        ("active", Value::Null, Some("failed"))
    );
    let p = &au::pending(&w.db, &id).unwrap()[0];
    assert_eq!((p.phase.as_str(), p.attempts), (au::CHECKED, 1));
    assert!(a
        .retry_at
        .is_some_and(|t| t > codesmith_core::store::now() + 50_000));
    w.sql("DROP TRIGGER refuse", []);
    // Not before the retry time.
    w.stale();
    runner.tick().await.unwrap();
    assert!(w.texts(&creator).is_empty());
    w.sql(
        "UPDATE automation SET retry_at = ?2 WHERE id = ?1",
        rusqlite::params![id, codesmith_core::store::now() - 1],
    );
    w.stale();
    runner.tick().await.unwrap();
    assert_eq!(w.texts(&creator), ["u:ready", "a:answer to: ready"]);
    let a = w.get(&id);
    assert_eq!(
        (a.status.as_str(), a.runs, a.state.clone()),
        ("done", 1, json!(42))
    );
    assert_eq!(w.posts(), ["checked"], "the check ran once");
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn oc_automation_pauses_on_a_missing_handoff_directory_without_rerunning_the_check() {
    let Some(w) = World::new(json!({})).await else {
        return;
    };
    let creator = w.creator(json!([]));
    let id = w.code(
        Some(&creator),
        "await tools.counter.post({ text: 'checked' }); return { state: 1, done: true, send: { prompt: 'ready' } }",
        None,
    );
    // The handoff waits for a turn in the session; meanwhile the process
    // stops and the folder (a removed worktree) goes away.
    assert!(w.db.lease_session(&creator, "other", 60_000).unwrap());
    w.due_now(&id);
    let r = spawn_tick(w.runner());
    until("the saved check", || {
        w.phase(&id, 1).as_deref() == Some(au::CHECKED)
    })
    .await;
    r.kill(&w.db).await;
    w.db.release_session(&creator, "other").unwrap();
    std::fs::remove_dir_all(w.project.path()).unwrap();
    w.stale();
    let runner = w.runner();
    runner.tick().await.unwrap();
    let a = w.get(&id);
    assert_eq!(
        (a.status.as_str(), a.runs, a.state.clone()),
        ("paused", 1, Value::Null)
    );
    assert!(
        a.last_error.as_deref().unwrap().contains(&w.dir()),
        "{:?}",
        a.last_error
    );
    w.stale();
    runner.tick().await.unwrap();
    assert_eq!(w.posts(), ["checked"]);
    assert!(w.texts(&creator).is_empty());
    assert_eq!(w.tagged(&au::message_id(&id, 1)), 0);
}
