//! Ports of OpenCode's plan plugin tests (`packages/core/test/plugin/`
//! `plan.test.ts`, MIT) through a session: the Plan mode reminders a
//! session saves, and the plan agent's writes. The pure rules are ported in
//! `src/plugin/plan.rs`. See `crates/codesmith-core/PARITY.md`.

mod plugin_support;

use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::plugin::{self, plan};
use codesmith_core::session::Session;
use codesmith_core::store::Store;
use plugin_support::{loaded, options, serial, serve, text, tool, Collect, Seen};
use serde_json::json;
use std::sync::Arc;

/// The plan plugin alone (the goal plugin would open the default store),
/// and a scratch data folder for the Plan directory.
fn setup() -> tempfile::TempDir {
    plugin::install(vec![Arc::new(plan::PlanPlugin)]);
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
    std::env::set_var("HOME", dir.path().join("home"));
    std::env::set_var("CODESMITH_CONFIG_DIR", dir.path().join("config"));
    dir
}

fn session(
    dir: &std::path::Path,
    base: &str,
    agent: &str,
    resume: Option<String>,
    reply: Reply,
) -> Session {
    session_with(dir, base, agent, resume, reply, json!({}))
}

fn session_with(
    dir: &std::path::Path,
    base: &str,
    agent: &str,
    resume: Option<String>,
    reply: Reply,
    extra: serde_json::Value,
) -> Session {
    let l = loaded(dir, base, extra);
    let mut o = options();
    o.agent = Some(agent.into());
    o.resume = resume;
    let store = Store::open(&dir.join("db.sqlite")).unwrap();
    Session::new(
        &l,
        dir.to_path_buf(),
        o,
        Arc::new(Fixed(reply)),
        Some(store),
    )
    .unwrap()
}

/// The text of each non-system message of a request.
fn texts(r: &Seen) -> Vec<String> {
    r.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|m| m["content"].to_string())
        .collect()
}

fn count(r: &Seen, needle: &str) -> usize {
    texts(r).iter().filter(|t| t.contains(needle)).count()
}

// plan.test.ts: "reconciles a missing enter reminder into the request and persists it"
#[tokio::test(flavor = "multi_thread")]
async fn oc_plan_session_saves_the_enter_reminder_before_the_prompt() {
    let _g = serial();
    let dir = setup();
    let m = serve(|_| text("ok")).await;
    let mut s = session(dir.path(), &m.base, "plan", None, Reply::Once);
    let mut out = Collect::default();
    s.prompt("what agent are you?", &mut out).await.unwrap();
    s.prompt("and now?", &mut out).await.unwrap();
    let reqs = m.requests();
    let first = texts(&reqs[0]);
    assert!(first[0].contains("You are in Plan mode"), "{first:?}");
    assert!(first[1].contains("what agent are you?"), "{first:?}");
    // Saved: the next turn has it once, where it was.
    assert_eq!(count(&reqs[1], "You are in Plan mode"), 1);
    assert!(texts(&reqs[1])[0].contains("You are in Plan mode"));
    // Not in the system prompt, which stays the same across agents.
    let system = reqs[0].body["messages"][0]["content"].to_string();
    assert!(!system.contains("Plan mode"), "{system}");
}

// plan.test.ts: "injects enter and leave reminders on agent switches"
// (adapted: the switch is a session resumed with another agent)
#[tokio::test(flavor = "multi_thread")]
async fn oc_plan_session_leaves_plan_mode_when_the_agent_changes() {
    let _g = serial();
    let dir = setup();
    let m = serve(|_| text("ok")).await;
    let mut s = session(dir.path(), &m.base, "plan", None, Reply::Once);
    let mut out = Collect::default();
    s.prompt("plan it", &mut out).await.unwrap();
    let id = s.id.clone();
    drop(s);
    let mut s = session(dir.path(), &m.base, "build", Some(id), Reply::Once);
    s.prompt("ok implement it", &mut out).await.unwrap();
    s.prompt("more", &mut out).await.unwrap();
    let reqs = m.requests();
    let t = texts(&reqs[1]);
    let leave = t.iter().position(|x| x.contains("NO LONGER in Plan mode"));
    let prompt = t.iter().position(|x| x.contains("ok implement it"));
    assert!(leave.is_some() && leave < prompt, "{t:?}");
    assert_eq!(count(&reqs[2], "NO LONGER in Plan mode"), 1);
    assert_eq!(count(&reqs[2], "You are in Plan mode"), 1);
}

// plan.test.ts: "does nothing for non-plan sessions without plan history"
#[tokio::test(flavor = "multi_thread")]
async fn oc_plan_session_adds_nothing_without_plan_history() {
    let _g = serial();
    let dir = setup();
    let m = serve(|_| text("ok")).await;
    let mut s = session(dir.path(), &m.base, "build", None, Reply::Once);
    s.prompt("hello", &mut Collect::default()).await.unwrap();
    assert_eq!(count(&m.requests()[0], "Plan mode"), 0);
}

// plan.test.ts: "rewrites blocked mutation failures with the Plan directory",
// "allows edits only inside the Plan directory" and "allows external
// directories without asking", through the tools: with the Plan directory
// outside the project, and inside it (a project in the home folder).
#[tokio::test(flavor = "multi_thread")]
async fn oc_plan_session_writes_only_plan_files() {
    for inside in [false, true] {
        writes_only_plan_files(inside).await;
    }
}

async fn writes_only_plan_files(inside: bool) {
    let _g = serial();
    let dir = setup();
    let project = if inside {
        dir.path().to_path_buf()
    } else {
        dir.path().join("project")
    };
    std::fs::create_dir_all(&project).unwrap();
    let plan_file = plan::directory().join("work.md");
    let target = plan_file.display().to_string();
    let m = serve(
        move |r| match r.body["messages"].as_array().unwrap().len() {
            3 => tool("c1", "write", json!({"path": "source.ts", "content": "x"})),
            5 => tool("c2", "write", json!({"path": target, "content": "# Plan"})),
            _ => text("done"),
        },
    )
    .await;
    // Every prompt is declined: the Plan directory needs none.
    let mut s = session(&project, &m.base, "plan", None, Reply::Reject(None));
    s.prompt("write the plan", &mut Collect::default())
        .await
        .unwrap();
    let reqs = m.requests();
    let blocked = reqs[1].body.to_string();
    assert!(
        blocked.contains("Cannot use write to modify files outside the Plan directory"),
        "{blocked}"
    );
    assert!(blocked.contains(&plan::directory().display().to_string()));
    assert!(!project.join("source.ts").exists());
    assert_eq!(
        std::fs::read_to_string(&plan_file).unwrap(),
        "# Plan",
        "inside: {inside}"
    );
}

// plan.test.ts: "allows external directories without asking", through the
// tools: the plan agent reads a file outside the project with every prompt
// declined, still cannot write there, and a configured
// `external_directory` deny still blocks the read.
#[tokio::test(flavor = "multi_thread")]
async fn oc_plan_session_reads_outside_the_project_without_asking() {
    for denied in [false, true] {
        reads_outside(denied).await;
    }
}

async fn reads_outside(denied: bool) {
    let _g = serial();
    let dir = setup();
    let project = dir.path().join("project");
    let outside = dir.path().join("elsewhere");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let file = outside.join("lib.rs");
    std::fs::write(&file, "fn outside_marker() {}").unwrap();
    let read = file.display().to_string();
    let written = outside.join("new.rs");
    let write = written.display().to_string();
    let m = serve(
        move |r| match r.body["messages"].as_array().unwrap().len() {
            3 => tool("c1", "read", json!({"path": read})),
            5 => tool("c2", "write", json!({"path": write, "content": "x"})),
            _ => text("done"),
        },
    )
    .await;
    let extra = if denied {
        json!({"permissions": [{"action": "external_directory", "resource": "*", "effect": "deny"}]})
    } else {
        json!({})
    };
    let mut s = session_with(&project, &m.base, "plan", None, Reply::Reject(None), extra);
    s.prompt("look around", &mut Collect::default())
        .await
        .unwrap();
    let reqs = m.requests();
    let after_read = reqs[1].body.to_string();
    assert_eq!(
        after_read.contains("outside_marker"),
        !denied,
        "denied: {denied}: {after_read}"
    );
    if denied {
        assert!(after_read.contains("Permission denied"), "{after_read}");
    }
    assert!(!written.exists(), "the plan agent wrote outside");
}

// Turning the plan plugin off drops its reminders.
#[tokio::test(flavor = "multi_thread")]
async fn plan_session_adds_no_reminders_with_the_plugin_off() {
    let _g = serial();
    let dir = setup();
    let m = serve(|_| text("ok")).await;
    let l = loaded(dir.path(), &m.base, json!({"plugins": {"plan": false}}));
    let mut o = options();
    o.agent = Some("plan".into());
    let mut s = Session::new(
        &l,
        dir.path().to_path_buf(),
        o,
        Arc::new(Fixed(Reply::Once)),
        None,
    )
    .unwrap();
    s.prompt("hello", &mut Collect::default()).await.unwrap();
    assert_eq!(count(&m.requests()[0], "Plan mode"), 0);
}
