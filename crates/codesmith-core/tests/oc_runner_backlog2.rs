//! Ports of the rest of the session runner cases in PARITY.md's porting
//! backlog (`packages/core/test/session-runner.test.ts`, MIT): the session
//! idle hook. OpenCode registers `experimental.idle` per test; CodeSmith's
//! plugins are installed once per process, so one plugin here answers for
//! the sessions each test registers with it.

mod scripted;

use codesmith_core::plugin::{self, Background, BoxFuture, Hook, IdleAction, Plugin, SessionIdle};
use scripted::*;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

type Behaviour = Box<dyn Fn(&SessionIdle) -> IdleAction + Send + Sync>;

#[derive(Default)]
struct Idles {
    behaviours: Mutex<HashMap<String, Behaviour>>,
}

struct IdlePlugin;

fn idles() -> &'static Idles {
    static IDLES: std::sync::OnceLock<Idles> = std::sync::OnceLock::new();
    IDLES.get_or_init(Idles::default)
}

impl Plugin for IdlePlugin {
    fn id(&self) -> &str {
        "oc-idle"
    }
    fn wants(&self, hook: Hook, session_id: &str) -> bool {
        hook == Hook::SessionIdle && idles().behaviours.lock().unwrap().contains_key(session_id)
    }
    fn session_idle<'a>(&'a self, idle: &'a SessionIdle<'a>) -> BoxFuture<'a, IdleAction> {
        let action = idles()
            .behaviours
            .lock()
            .unwrap()
            .get(idle.session_id)
            .map(|b| b(idle))
            .unwrap_or(IdleAction::Idle);
        Box::pin(async move { action })
    }
}

/// Install the plugin (once) and register `behaviour` for session `id`.
fn on_idle(id: &str, behaviour: impl Fn(&SessionIdle) -> IdleAction + Send + Sync + 'static) {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| plugin::install(vec![Arc::new(IdlePlugin)]));
    idles()
        .behaviours
        .lock()
        .unwrap()
        .insert(id.to_string(), Box::new(behaviour));
}

/// "continues instead of going idle when a session idle hook admits input
/// (resume: ${resume})". Adapted: the hook's message continues the turn
/// whether or not the session would have resumed for it.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_continues_instead_of_going_idle_when_the_idle_hook_admits_input() {
    let mut t = plain().await;
    let calls = Arc::new(Mutex::new(0));
    let c = calls.clone();
    on_idle(&t.s.id, move |_| {
        let mut n = c.lock().unwrap();
        *n += 1;
        if *n > 1 {
            IdleAction::Idle
        } else {
            IdleAction::Continue("Keep going".into())
        }
    });
    t.push([text("First answer"), text("Second answer")]);
    t.prompt("Start").await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 2);
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    let last = last_message(&reqs[1]);
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"], "Keep going");
    assert_eq!(texts(&t.s), ["First answer", "Second answer"]);
}

/// "runs the session idle hook only once the session would go idle": not
/// after the step that called a tool.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_runs_the_idle_hook_only_once_the_session_would_go_idle() {
    let mut t = plain().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (s, script) = (seen.clone(), t.script.clone());
    on_idle(&t.s.id, move |_| {
        s.lock().unwrap().push(script.lock().unwrap().log.len());
        IdleAction::Idle
    });
    std::fs::write(t.path("a.txt"), "hello").unwrap();
    t.push([
        call("call-echo", "read", json!({"path": "a.txt"})),
        text("Done"),
    ]);
    t.prompt("First").await.unwrap();
    assert_eq!(t.requests().len(), 2);
    assert_eq!(*seen.lock().unwrap(), [2]);
}

/// "passes background work of the session and its descendants to the idle
/// hook". Adapted: the session's own background shell commands (subagents
/// run inside their tool call, so a child leaves no background work).
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_passes_its_background_work_to_the_idle_hook() {
    let mut t = plain().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    on_idle(&t.s.id, move |idle| {
        s.lock().unwrap().push(idle.background.clone());
        IdleAction::Idle
    });
    let gate = t.path("go");
    let command = format!(
        "i=0; while [ ! -f '{}' ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done",
        gate.display()
    );
    t.push([
        call(
            "call-bg",
            "shell",
            json!({"command": command, "description": "wait", "background": true}),
        ),
        text("Waiting on background work"),
    ]);
    t.prompt("Start").await.unwrap();
    std::fs::write(&gate, "").unwrap();
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert!(
        matches!(&got[0][..], [Background::Shell { command: c, .. }] if *c == command),
        "{got:?}"
    );
    tokio::time::timeout(Duration::from_secs(60), t.s.wait_background())
        .await
        .unwrap();
}

/// "runs the idle hook again when a background notice wakes the session"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_runs_the_idle_hook_again_when_a_background_notice_wakes_it() {
    let mut t = plain().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    on_idle(&t.s.id, move |idle| {
        s.lock().unwrap().push(idle.background.len());
        IdleAction::Idle
    });
    let gate = t.path("go");
    let command = format!(
        "i=0; while [ ! -f '{}' ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done",
        gate.display()
    );
    t.push([
        call(
            "call-bg",
            "shell",
            json!({"command": command, "description": "tests", "background": true}),
        ),
        text("Waiting for the tests"),
        text("Tests passed"),
    ]);
    t.prompt("Start").await.unwrap();
    std::fs::write(&gate, "").unwrap();
    tokio::time::timeout(Duration::from_secs(60), t.s.wait_background())
        .await
        .unwrap();
    t.s.resume_background(&mut Rec(t.seen.clone()))
        .await
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), [1, 0]);
    assert_eq!(t.requests().len(), 3);
    assert_eq!(texts(&t.s).last().map(String::as_str), Some("Tests passed"));
}

// --- Compaction from the runner ---

fn limited(context: u64, output: u64) -> Setup {
    Setup {
        models: json!({"m": {"limit": {"context": context, "output": output}}}),
        ..Default::default()
    }
}

fn with_usage(content: &str, usage: serde_json::Value) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": content}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": usage}),
    ])
}

fn max_tokens(body: &serde_json::Value) -> u64 {
    body["max_tokens"]
        .as_u64()
        .or(body["max_completion_tokens"].as_u64())
        .unwrap_or_else(|| panic!("no output limit: {body}"))
}

fn is_summary_request(body: &serde_json::Value) -> bool {
    body["messages"]
        .to_string()
        .contains("Do not continue the task or call tools.")
}

/// A session whose last step reported `prompt` tokens on a model with this
/// window and output limit; then a long prompt, answered by a summary and
/// a reply. The requests after the first.
async fn compacting(context: u64, output: u64, prompt: u64) -> (T, Vec<serde_json::Value>) {
    let mut t = start(limited(context, output)).await;
    t.push([with_usage(
        "Earlier answer",
        json!({"prompt_tokens": prompt, "completion_tokens": 10}),
    )]);
    t.prompt("Earlier question").await.unwrap();
    t.push([text("## Objective\n- Preserve the task"), text("Continued")]);
    t.prompt(&"Recent request ".repeat(400)).await.unwrap();
    let reqs = t.requests()[1..].to_vec();
    (t, reqs)
}

/// "gives the summary its full output limit when the conversation
/// overshot the threshold" and "keeps the summary its room on a small
/// window". Differs on purpose: past the ceiling the whole conversation
/// still goes to the summary unchanged while the window can fit a summary
/// (keeping the provider's cache), with the output limit fitted to the room
/// it leaves, where OpenCode shrinks the request to its budget and gives the
/// summary the whole reserve. The next step gets its own output limit.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_fits_the_summary_output_to_the_room_an_unchanged_conversation_leaves() {
    for (window, output, prompt) in [(200_000, 32_000, 185_000), (64_000, 16_000, 59_000)] {
        let (_t, reqs) = compacting(window, output, prompt).await;
        assert_eq!(reqs.len(), 2);
        assert!(is_summary_request(&reqs[0]));
        let summary = max_tokens(&reqs[0]);
        assert!(
            summary > 0 && summary < window - prompt,
            "{window}: {summary}"
        );
        assert_eq!(max_tokens(&reqs[1]), output, "{window}");
    }
}

/// "stops after required automatic compaction fails": the summary request
/// is refused, and the prompt is not sent without it. (A background summary
/// started after the first turn is refused too.)
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_stops_after_required_automatic_compaction_fails() {
    let mut t = start(limited(4_000, 50)).await;
    t.push([with_usage(
        "Earlier answer",
        json!({"prompt_tokens": 3_950, "completion_tokens": 10}),
    )]);
    t.prompt("Earlier question").await.unwrap();
    let refused = || {
        status(
            400,
            json!({"error": {"message": "summary unavailable", "type": "invalid_request_error"}}),
        )
    };
    t.push([refused(), refused(), refused(), text("Must not run")]);
    let e = t
        .prompt(&"Recent exact request ".repeat(180))
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("summary unavailable"), "{e:#}");
    let reqs = t.requests()[1..].to_vec();
    assert!(!reqs.is_empty() && reqs.len() <= 2, "{}", reqs.len());
    assert!(reqs.iter().all(is_summary_request));
    assert!(!texts(&t.s).iter().any(|x| x == "Must not run"));
}

/// Wait until the scripted model has had `n` requests, then drop `turn`
/// (as an interrupt does); whether it had finished.
async fn drop_after_request<F: std::future::Future>(t: &Shared, n: usize, turn: F) -> bool {
    let seen = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while t.lock().unwrap().log.len() < n {
            assert!(tokio::time::Instant::now() < deadline, "no request {n}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        _ = turn => true,
        _ = seen => false,
    }
}

/// "records cancelled manual compaction without surfacing an internal
/// failure". Adapted: an interrupt drops the compaction while its summary
/// streams; nothing is recorded and the history stays as it was.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_cancelled_manual_compaction_leaves_the_history() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.push([text("Earlier answer")]);
    t.prompt("Earlier question").await.unwrap();
    let before = t.s.messages.clone();
    t.push([held(
        &[json!({"choices": [{"delta": {"content": "## Objective\n- Partial summary"}}]})],
        &[],
    )]);
    let script = t.script.clone();
    let finished = {
        let mut sink = Rec(t.seen.clone());
        drop_after_request(&script, 2, t.s.compact(&mut sink)).await
    };
    assert!(!finished);
    script.lock().unwrap().gate.add_permits(1);
    assert_eq!(t.s.messages, before);
    let stored = t.reopen();
    assert_eq!(stored.messages, before);
}

/// "interrupts overflow recovery while the summary provider is running".
/// Adapted: the interrupt drops the turn; no summary replaces the history.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_interrupts_overflow_recovery_while_the_summary_streams() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.push([text("Earlier answer")]);
    t.prompt("Earlier question").await.unwrap();
    t.push([
        status(
            400,
            json!({"error": {"message": "This model's maximum context length is 32768 tokens", "type": "invalid_request_error"}}),
        ),
        held(
            &[json!({"choices": [{"delta": {"content": "## Objective\n- Interrupted"}}]})],
            &[],
        ),
    ]);
    let script = t.script.clone();
    let finished = {
        let mut sink = Rec(t.seen.clone());
        drop_after_request(&script, 3, t.s.prompt("Continue", &mut sink)).await
    };
    assert!(!finished);
    script.lock().unwrap().gate.add_permits(1);
    let reqs = t.requests();
    assert!(is_summary_request(&reqs[2]));
    assert!(!t
        .reopen()
        .messages
        .iter()
        .any(|m| format!("{m:?}").contains("Interrupted")));
}

// --- Agent switches ---

/// "updates selected-agent skill instructions after an agent switch".
/// Adapted: the switch is the session resumed with another agent (as the
/// front ends switch at the next prompt); the skills the new agent may not
/// use are withdrawn before its prompt.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_updates_the_skill_list_after_an_agent_switch() {
    let t = start(Setup {
        store: true,
        extra: json!({"agents": {"reviewer": {"mode": "primary", "permissions": [
            {"action": "skill", "resource": "deploy", "effect": "deny"}
        ]}}}),
        ..Default::default()
    })
    .await;
    for (name, what) in [("deploy", "Deploy things"), ("review", "Review changes")] {
        let d = t.path(&format!(".codesmith/skills/{name}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {what}\n---\nBody\n"),
        )
        .unwrap();
    }
    t.push([text("first"), text("second")]);
    let session = |agent: Option<&str>, resume: Option<String>| {
        let loaded = t.loaded();
        let opts = codesmith_core::session::Options {
            agent: agent.map(String::from),
            model: None,
            resume,
            title: Some("Test".into()),
            parent: None,
            id: None,
            questioner: None,
            mcp: None,
        };
        let root = loaded.root.clone();
        codesmith_core::session::Session::new(
            &loaded,
            root,
            opts,
            Arc::new(codesmith_core::permission::Fixed(
                codesmith_core::permission::Reply::Once,
            )),
            Some(t.store()),
        )
        .unwrap()
    };
    let mut build = session(None, None);
    build
        .prompt("First", &mut Rec(Arc::default()))
        .await
        .unwrap();
    let mut reviewer = session(Some("reviewer"), Some(build.id.clone()));
    reviewer
        .prompt("Second", &mut Rec(Arc::default()))
        .await
        .unwrap();
    let reqs = t.requests();
    let first = reqs[0].to_string();
    assert!(first.contains("<id>deploy</id>") && first.contains("<id>review</id>"));
    let second = reqs[1]["messages"].to_string();
    let withdrawn = second
        .rfind("no longer available")
        .unwrap_or_else(|| panic!("{second}"));
    assert!(second[withdrawn..].contains("deploy"), "{second}");
    assert!(second.rfind("Second").unwrap() > withdrawn);
}

// --- Compaction retries ---

fn unavailable() -> String {
    status(503, json!({"error": {"message": "Provider unavailable"}}))
}

/// A response that promises more body than it sends: the connection drops
/// mid-stream.
fn dropped() -> String {
    let chunk = json!({"choices": [{"delta": {"content": "## Objective\n- Partial draft"}}]});
    let body = format!("data: {chunk}\n\n");
    format!(
        "HTTP/1.1 200 X\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len() + 100
    )
}

/// "restarts compaction drafts after transient failures and unsuccessful
/// finishes". Adapted: a dropped stream, an unavailable provider and a
/// rate limit, each retried with the same request; the partial draft is
/// not kept.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_restarts_compaction_drafts_after_transient_failures() {
    let mut t = plain().await;
    t.push([text("Earlier answer")]);
    t.prompt("Earlier question").await.unwrap();
    t.push([
        dropped(),
        unavailable(),
        http(
            429,
            "application/json",
            &json!({"error": {"message": "rate limited"}}).to_string(),
        ),
        text("## Objective\n- Accepted summary"),
    ]);
    t.s.compact(&mut Rec(t.seen.clone())).await.unwrap();
    let summaries: Vec<serde_json::Value> = t.requests()[1..].to_vec();
    assert_eq!(summaries.len(), 4);
    assert!(summaries.iter().all(|r| *r == summaries[0]));
    let first = format!("{:?}", t.s.messages[0]);
    assert!(first.contains("Accepted summary") && !first.contains("Partial draft"));
}

/// "bounds compaction network retries across a template correction": the
/// reminder does not start the retry allowance over.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_bounds_compaction_retries_across_a_template_correction() {
    let mut t = plain().await;
    t.push([text("Earlier answer")]);
    t.prompt("Earlier question").await.unwrap();
    let before = t.s.messages.clone();
    t.push([unavailable(), text("Not a summary")]);
    t.push((0..30).map(|_| unavailable()));
    let e = t.s.compact(&mut Rec(t.seen.clone())).await.unwrap_err();
    assert!(format!("{e:#}").contains("Provider unavailable"), "{e:#}");
    // The first, its retry (the reply without the template), the reminder,
    // and the rest of one retry allowance: far fewer than the failures on offer.
    let summaries = t.requests().len() - 1;
    assert!((3..30).contains(&summaries), "{summaries}");
    assert_eq!(t.s.messages, before);
}

/// "compacts before requesting an undersized configured context limit": a
/// conversation already larger than the configured window is summarized
/// before the next request, which then goes out once.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_compacts_before_requesting_an_undersized_context_limit() {
    let mut t = start(limited(1, 1_000)).await;
    t.push([text("Earlier answer")]);
    let _ = t.prompt("Earlier question").await;
    let before = t.requests().len();
    t.push([
        text("## Objective\n- Recover undersized limit"),
        text("Recovered"),
    ]);
    t.prompt("Continue").await.unwrap();
    let reqs = t.requests()[before..].to_vec();
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    assert!(is_summary_request(&reqs[0]));
    assert!(!is_summary_request(&reqs[1]));
    assert_eq!(texts(&t.s).last().map(String::as_str), Some("Recovered"));
}
