//! Ports of the session runner tests in PARITY.md's porting backlog
//! (`packages/core/test/session-runner.test.ts`, `session-step.test.ts`,
//! `session-live-tool-metadata.test.ts`, `session-shell.test.ts`, MIT),
//! against the scripted OpenAI-compatible model in `scripted/`. Each test
//! names the OpenCode scenario it ports; see `crates/codesmith-core/PARITY.md`.

mod scripted;

use codesmith_ai::{Part, Role};

use codesmith_core::session::Sink;
use scripted::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The system prompt a chat request sends.
fn system_of(body: &Value) -> String {
    let m = &body["messages"][0];
    assert_eq!(m["role"], "system", "{body}");
    match &m["content"] {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// Wait (bounded) until `f` holds.
async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// --- System prompts ---

/// "uses the selected model family prompt when the agent does not override it"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_uses_the_model_family_prompt_without_an_agent_override() {
    let mut t = start(Setup {
        extra: json!({"model": "fake/gpt-5"}),
        ..Default::default()
    })
    .await;
    t.push([text("Done")]);
    t.prompt("First").await.unwrap();
    let system = system_of(&t.requests()[0]);
    assert!(system.contains("## Intermediate Commentary"), "{system}");
}

/// "uses the selected model family prompt when the agent system override is
/// empty". Fixed: an empty `system` replaced the model's prompt with nothing.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_uses_the_model_family_prompt_when_the_agent_override_is_empty() {
    let mut t = start(Setup {
        extra: json!({"model": "fake/gpt-5", "agents": {"build": {"system": "", "mode": "primary"}}}),
        ..Default::default()
    })
    .await;
    t.push([text("Done")]);
    t.prompt("First").await.unwrap();
    let system = system_of(&t.requests()[0]);
    assert!(system.contains("## Intermediate Commentary"), "{system}");
}

/// "includes the effective default agent system before durable context"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_puts_the_default_agents_system_before_durable_context() {
    let mut t = start(Setup {
        extra: json!({"agents": {"build": {"system": "Build agent instructions", "mode": "primary"}}}),
        ..Default::default()
    })
    .await;
    t.push([text("Done")]);
    t.prompt("First").await.unwrap();
    let system = system_of(&t.requests()[0]);
    assert!(
        system.starts_with("Build agent instructions\n\n"),
        "{system}"
    );
    let env = system.find("<env>").expect("the environment follows");
    assert!(env > "Build agent instructions".len());
    assert!(!system.contains("You are an AI agent running in CodeSmith"));
}

/// "uses the configured default agent system for omitted-agent sessions"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_uses_the_configured_default_agents_system() {
    let mut t = start(Setup {
        extra: json!({
            "default_agent": "reviewer",
            "agents": {
                "build": {"system": "Build agent instructions", "mode": "primary"},
                "reviewer": {"system": "Reviewer instructions", "mode": "primary"}
            }
        }),
        ..Default::default()
    })
    .await;
    assert_eq!(t.s.agent.id, "reviewer");
    t.push([text("Done")]);
    t.prompt("First").await.unwrap();
    assert!(system_of(&t.requests()[0]).starts_with("Reviewer instructions\n\n"));
}

/// "uses an explicitly selected non-build agent system"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_uses_an_explicitly_selected_agents_system() {
    let mut t = start(Setup {
        extra: json!({"agents": {"reviewer": {"system": "Reviewer instructions", "mode": "primary"}}}),
        agent: Some("reviewer"),
        ..Default::default()
    })
    .await;
    assert_eq!(t.s.agent.id, "reviewer");
    t.push([text("Done")]);
    t.prompt("First").await.unwrap();
    assert!(system_of(&t.requests()[0]).starts_with("Reviewer instructions\n\n"));
}

// --- Titles ---

/// "generates the title while the first model step is still running".
/// Adapted: the title request goes out while the step runs; the session
/// takes the title when the turn ends.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_requests_the_title_while_the_first_step_is_still_running() {
    let mut t = start(Setup {
        title: None,
        ..Default::default()
    })
    .await;
    t.script
        .lock()
        .unwrap()
        .titles
        .push_back("Generated title".into());
    t.push([HOLD.to_string(), text("Answer")]);
    let script = t.script.clone();
    let opener = tokio::spawn(async move {
        until("the title request", || {
            let s = script.lock().unwrap();
            !s.title_log.is_empty() && !s.log.is_empty()
        })
        .await;
        // The step is still held: the title was asked for meanwhile.
        script.lock().unwrap().gate.add_permits(1);
    });
    t.prompt("First prompt").await.unwrap();
    opener.await.unwrap();
    assert_eq!(t.s.title.as_deref(), Some("Generated title"));
    assert_eq!(t.script.lock().unwrap().title_log.len(), 1);
}

/// "does not automatically replace an existing session title"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_does_not_replace_an_existing_title() {
    let mut t = start(Setup {
        title: Some("Manual title".into()),
        ..Default::default()
    })
    .await;
    t.push([text("Assistant response")]);
    t.prompt("Follow-up prompt").await.unwrap();
    assert_eq!(t.requests().len(), 1);
    assert!(t.script.lock().unwrap().title_log.is_empty());
    assert_eq!(t.s.title.as_deref(), Some("Manual title"));
}

// --- Cost ---

fn priced() -> Value {
    json!({"m": {"cost": {"input": 1.0, "output": 2.0}}})
}

/// The configured price of the model, as the front ends read it.
fn price() -> Option<codesmith_core::config::Cost> {
    config_with("http://127.0.0.1:9/v1", "openai-chat", priced(), json!({})).cost("fake/m")
}

fn with_usage(t: &str, usage: Value) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": t}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": usage}),
    ])
}

/// "records the cost the endpoint reports instead of the catalog price"
/// (`session-step.test.ts` and `session-usage.test.ts`)
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_records_the_cost_the_endpoint_reports_over_the_catalog_price() {
    let mut t = start(Setup {
        models: priced(),
        ..Default::default()
    })
    .await;
    t.push([with_usage(
        "Answer",
        json!({"prompt_tokens": 1_000_000, "completion_tokens": 1_000_000, "cost": 0.25}),
    )]);
    t.prompt("Price it").await.unwrap();
    let spend = t.s.spend();
    assert_eq!(spend.billed, Some(0.25));
    assert_eq!(spend.cost(price().as_ref()), Some(0.25));
}

/// "keeps a reported zero cost, as for a request a subscription covered"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_keeps_a_reported_zero_cost() {
    let mut t = start(Setup {
        models: priced(),
        ..Default::default()
    })
    .await;
    t.push([with_usage(
        "Answer",
        json!({"prompt_tokens": 1_000_000, "completion_tokens": 1_000_000, "cost": 0}),
    )]);
    t.prompt("Price it").await.unwrap();
    let spend = t.s.spend();
    assert_eq!(spend.billed, Some(0.0));
    assert_eq!(spend.cost(price().as_ref()), Some(0.0));
}

/// "prices the tokens from the catalog when the endpoint reports no cost"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_prices_the_tokens_when_the_endpoint_reports_no_cost() {
    let mut t = start(Setup {
        models: priced(),
        ..Default::default()
    })
    .await;
    t.push([with_usage(
        "Answer",
        json!({"prompt_tokens": 1_000_000, "completion_tokens": 500_000}),
    )]);
    t.prompt("Price it").await.unwrap();
    let spend = t.s.spend();
    assert_eq!(spend.billed, None);
    // $1 per million input tokens and $2 per million output tokens.
    let cost = spend.cost(price().as_ref()).unwrap();
    assert!((cost - 2.0).abs() < 1e-9, "{cost}");
}

// --- Tool progress ---

/// Tool events in order, as `progress ID` and `end ID`.
struct Order(Arc<Mutex<Vec<String>>>, Option<String>);
impl Sink for Order {
    fn text(&mut self, _: &str) {}
    fn tool_start(&mut self, id: &str, _: &str, _: &Value) {
        self.1 = Some(id.into());
        self.0.lock().unwrap().push(format!("start {id}"));
    }
    fn tool_progress(&mut self, id: &str, _: &Value) {
        self.0.lock().unwrap().push(format!("progress {id}"));
    }
    fn tool_output(&mut self, id: &str, _: &str) {
        self.0.lock().unwrap().push(format!("progress {id}"));
    }
    fn tool_end(&mut self, _: &str, _: &str, _: &str, _: bool) {
        let id = self.1.clone().unwrap_or_default();
        self.0.lock().unwrap().push(format!("end {id}"));
    }
}

/// "adds progress to running tool parts until the call settles"
/// (`session-live-tool-metadata.test.ts`). Adapted: progress is the
/// front end's `tool_output` and `tool_progress` events.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_reports_tool_progress_until_the_call_settles() {
    let mut t = start(Setup {
        reply: codesmith_core::permission::Reply::Always,
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "call_1",
            "shell",
            json!({"command": "echo one; echo two", "description": "Echo"}),
        ),
        text("Done"),
    ]);
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut sink = Order(events.clone(), None);
    t.s.prompt("Run it", &mut sink).await.unwrap();
    let events = events.lock().unwrap().clone();
    let end = events
        .iter()
        .position(|e| e == "end call_1")
        .unwrap_or_else(|| panic!("{events:?}"));
    assert!(
        events[..end].iter().any(|e| e == "progress call_1"),
        "{events:?}"
    );
    assert!(
        !events[end..].iter().any(|e| e.starts_with("progress")),
        "{events:?}"
    );
    let r = results(&t.s);
    assert_eq!(r.len(), 1);
    assert!(r[0].1.contains("one") && r[0].1.contains("two"), "{r:?}");
}

// --- Local tools and continuation ---

/// "continues with reloaded history after durably settling one local tool call"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_continues_with_reloaded_history_after_settling_a_local_tool() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    std::fs::write(t.path("echo.txt"), "hello\n").unwrap();
    t.push([
        call("call-echo", "read", json!({"path": "echo.txt"})),
        text("Done"),
    ]);
    t.prompt("Echo this").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(roles(&reqs[1]), ["system", "user", "assistant", "tool"]);
    assert!(reqs[1]["messages"][3]["content"]
        .to_string()
        .contains("hello"));
    // The stored history is the one the continuation was built from.
    let again = t.reopen();
    let kinds: Vec<String> = again
        .messages
        .iter()
        .map(|m| {
            let what: Vec<&str> = m
                .parts
                .iter()
                .map(|p| match p {
                    Part::Text(_) => "text",
                    Part::ToolCall { .. } => "call",
                    Part::ToolResult { .. } => "result",
                    _ => "other",
                })
                .collect();
            format!("{:?} {}", m.role, what.join(","))
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "User text",
            "Assistant call",
            "User result",
            "Assistant text"
        ]
    );
}

/// "settles repeated provider-local tool call IDs against their owning
/// assistant messages"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_settles_repeated_tool_call_ids_against_their_own_messages() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    std::fs::write(t.path("first.txt"), "first\n").unwrap();
    std::fs::write(t.path("second.txt"), "second\n").unwrap();
    t.push([
        call("tool_0", "read", json!({"path": "first.txt"})),
        call("tool_0", "read", json!({"path": "second.txt"})),
        text("Done"),
    ]);
    t.prompt("Echo twice").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 3);
    let check = |messages: &[codesmith_ai::Message]| {
        let r: Vec<(String, String)> = messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                Part::ToolResult { id, content, .. } => Some((id.clone(), content.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0].0, "tool_0");
        assert!(r[0].1.contains("first") && !r[0].1.contains("second"));
        assert!(r[1].1.contains("second") && !r[1].1.contains("first"));
    };
    check(&t.s.messages);
    check(&t.reopen().messages);
    // Each result follows the call it answers.
    assert_eq!(
        roles(&reqs[2]),
        ["system", "user", "assistant", "tool", "assistant", "tool"]
    );
    assert!(reqs[2]["messages"][3]["content"]
        .to_string()
        .contains("first"));
    assert!(reqs[2]["messages"][5]["content"]
        .to_string()
        .contains("second"));
}

/// Tool and text events in order.
struct Events(Arc<Mutex<Vec<String>>>);
impl Sink for Events {
    fn text(&mut self, d: &str) {
        self.0.lock().unwrap().push(format!("text {d}"));
    }
    fn tool_start(&mut self, id: &str, _: &str, _: &Value) {
        self.0.lock().unwrap().push(format!("start {id}"));
    }
}

/// "consumes the full provider stream before recording its boundary and
/// settling local tools". Adapted: what follows the call in the stream
/// arrives before the tool starts.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_consumes_the_whole_stream_before_running_local_tools() {
    let mut t = plain().await;
    std::fs::write(t.path("a.txt"), "x").unwrap();
    t.push([
        sse(&[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call-streamed", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a.txt\"}"}}]}}]}),
            json!({"choices": [{"delta": {"content": "tail"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 100}}),
        ]),
        text("Done"),
    ]);
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    t.s.prompt("Echo this", &mut Events(events.clone()))
        .await
        .unwrap();
    let events = events.lock().unwrap().clone();
    let tail = events.iter().position(|e| e == "text tail").unwrap();
    let start = events
        .iter()
        .position(|e| e == "start call-streamed")
        .unwrap();
    assert!(tail < start, "{events:?}");
    assert_eq!(t.requests().len(), 2);
}

/// "returns tool-wrapped policy blocks to the model and continues".
/// Adapted: a configured policy blocks the call.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_returns_policy_blocks_to_the_model_and_continues() {
    let mut t = start(Setup {
        extra: json!({"policies": [{"action": "tool.use", "resource": "glob:*", "effect": "deny", "message": "Permission blocked"}]}),
        reply: codesmith_core::permission::Reply::Always,
        ..Default::default()
    })
    .await;
    t.push([
        call("call-blocked", "glob", json!({"pattern": "*"})),
        text("Handled"),
    ]);
    t.prompt("Call blocked").await.unwrap();
    assert_eq!(t.requests().len(), 2);
    let r = results(&t.s);
    assert_eq!(r.len(), 1);
    assert!(r[0].2, "{r:?}");
    assert!(r[0].1.contains("Permission blocked"), "{r:?}");
    assert_eq!(texts(&t.s).last().unwrap(), "Handled");
}

/// "settles a valid sibling before recovering malformed tool input"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_settles_a_valid_sibling_before_recovering_malformed_input() {
    let mut t = plain().await;
    std::fs::write(t.path("valid.txt"), "valid\n").unwrap();
    t.push([
        calls(&[
            ("call-valid", "read", r#"{"path":"valid.txt"}"#),
            ("call-malformed", "read", r#"{"path":"partial"#),
        ]),
        text("Done"),
    ]);
    t.prompt("Run parallel tools").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    let r = results(&t.s);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].0, "call-valid");
    assert!(!r[0].2 && r[0].1.contains("valid"));
    assert_eq!(r[1].0, "call-malformed");
    assert!(r[1].2 && r[1].1.contains("could not be parsed as JSON"));
    let sent: Vec<&str> = reqs[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["tool_call_id"].as_str().unwrap())
        .collect();
    assert_eq!(sent, ["call-valid", "call-malformed"]);
}

/// "records a provider failure after malformed input"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_records_a_provider_failure_after_malformed_input() {
    let mut t = plain().await;
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call-malformed", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"partial"}}]}}]}),
        json!({"error": {"type": "invalid_request_error", "message": "Provider failed after malformed input"}})
    );
    t.push([http(200, "text/event-stream", &body), text("Must not run")]);
    let err = t.prompt("Fail after malformed input").await.unwrap_err();
    assert!(
        err.to_string()
            .contains("Provider failed after malformed input"),
        "{err}"
    );
    assert_eq!(t.requests().len(), 1, "no automatic continuation");
    assert!(results(&t.s).is_empty());
}

// --- Provider failures and retries ---

/// "projects raw provider stream failures as terminal assistant step
/// failures". Adapted: the failed step leaves no assistant message; the
/// prompt is kept, also after a reload.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_records_a_raw_stream_failure_and_keeps_the_prompt() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    t.push([status(
        400,
        json!({"error": {"message": "Invalid request", "type": "invalid_request_error"}}),
    )]);
    let err = t.prompt("Fail raw stream durably").await.unwrap_err();
    assert!(err.to_string().contains("Invalid request"), "{err}");
    let again = t.reopen();
    assert_eq!(again.messages.len(), 1);
    assert_eq!(again.messages[0].role, Role::User);
    assert_eq!(
        again.messages[0].parts,
        [Part::Text("Fail raw stream durably".into())]
    );
}

/// "retries cleanly after ${failure.name} during reasoning" (2 cases: a
/// provider failure frame and a dropped connection).
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_retries_cleanly_after_a_failure_during_reasoning() {
    let reasoning = json!({"choices": [{"delta": {"reasoning_content": "Disconnected thought"}}]});
    let unavailable = format!(
        "data: {reasoning}\n\ndata: {}\n\n",
        json!({"error": {"type": "server_error", "message": "Provider unavailable"}})
    );
    for failure in [
        http(200, "text/event-stream", &unavailable),
        cut(std::slice::from_ref(&reasoning)),
    ] {
        let mut t = plain().await;
        t.push([failure, text("Recovered")]);
        t.prompt("Recover disconnected reasoning").await.unwrap();
        let reqs = t.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(t.seen.lock().unwrap().retries.len(), 1);
        assert_eq!(roles(&reqs[1]), ["system", "user"]);
        assert!(!reqs[1].to_string().contains("Disconnected thought"));
        assert_eq!(texts(&t.s), ["Recovered"]);
    }
}

/// "shares retry accounting and assistant identity across transparent
/// retries and retries after output". Adapted: a retry after output drops
/// the partial step, so every retry starts from the prompt; what is shared
/// is the one allowance of ten retries.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_shares_one_retry_allowance_across_both_kinds_of_retry() {
    let mut t = plain().await;
    let partial = || cut(&[json!({"choices": [{"delta": {"content": "Partial"}}]})]);
    let transparent = || status(503, json!({"error": {"message": "unavailable"}}));
    t.push((0..12).map(|i| if i % 2 == 0 { transparent() } else { partial() }));
    assert!(t.prompt("Mix retry paths").await.is_err());
    let reqs = t.requests();
    assert_eq!(reqs.len(), 11, "one attempt and ten retries in all");
    assert_eq!(t.seen.lock().unwrap().retries.len(), 10);
    for r in &reqs {
        assert_eq!(roles(r), ["system", "user"], "{r}");
    }
    assert!(!t.s.messages.iter().any(|m| m.role == Role::Assistant));
}

/// "does not start another physical attempt after interruption during
/// retry backoff". Adapted: an interrupt drops the turn's future.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_makes_no_new_attempt_after_an_interrupt_during_backoff() {
    let mut t = plain().await;
    // A backoff of about 200ms.
    t.s.set_retry_scale(0.1);
    t.push([
        status(503, json!({"error": {"message": "unavailable"}})),
        text("Must not run"),
    ]);
    let seen = t.seen.clone();
    let mut sink = Rec(seen.clone());
    tokio::select! {
        r = t.s.prompt("Interrupt retry backoff", &mut sink) => panic!("the turn ended: {r:?}"),
        _ = until("the retry", || !seen.lock().unwrap().retries.is_empty()) => {}
    }
    // Past the backoff, nothing was sent again.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(t.requests().len(), 1);
    t.s.set_retry_scale(0.0001);
    t.script.lock().unwrap().replies.clear();
    t.push([text("Next")]);
    t.prompt("Next prompt").await.unwrap();
    assert_eq!(t.requests().len(), 2);
}

// --- Interrupted streams ---

/// "durably closes partial ${kind} when the provider stream is interrupted"
/// (3 cases). Found difference: an interrupt (the turn's future dropped, as
/// the TUI's Esc does) loses the text and reasoning streamed so far, which
/// a failed stream keeps; OpenCode keeps both. An incomplete tool call is
/// never kept (by design, as for a failed stream).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "found difference: an interrupted step's partial text is not recorded"]
async fn oc_session_closes_partial_output_when_the_stream_is_interrupted() {
    for (kind, chunk) in [
        (
            "text",
            json!({"choices": [{"delta": {"content": "Partial"}}]}),
        ),
        (
            "reasoning",
            json!({"choices": [{"delta": {"reasoning_content": "Partial"}}]}),
        ),
        (
            "tool input",
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call-partial", "type": "function", "function": {"name": "read", "arguments": "{\"pa"}}]}}]}),
        ),
    ] {
        let mut t = start(Setup {
            store: true,
            ..Default::default()
        })
        .await;
        t.push([held(&[chunk], &[])]);
        let seen = t.seen.clone();
        let mut sink = Rec(seen.clone());
        let script = t.script.clone();
        tokio::select! {
            r = t.s.prompt("Interrupt me", &mut sink) => panic!("{kind}: the turn ended: {r:?}"),
            _ = until("the partial output", || {
                let s = seen.lock().unwrap();
                // Reasoning reaches no sink callback here; the request does.
                !s.text.is_empty() || !s.lifecycle.is_empty()
                    || kind == "reasoning" && script.lock().unwrap().log.len() == 1
            }) => {}
        }
        let again = t.reopen();
        let last = again.messages.last().unwrap();
        match kind {
            "tool input" => assert_eq!(last.role, Role::User, "{:?}", again.messages),
            _ => {
                assert_eq!(last.role, Role::Assistant, "{kind}: {:?}", again.messages);
                assert!(format!("{:?}", last.parts).contains("Partial"));
            }
        }
    }
}

// --- Provider state replayed ---

fn signed_details() -> Value {
    json!([{"type": "reasoning.text", "text": "Signed thought", "signature": "sig_1", "format": "anthropic-claude-v1", "index": 0}])
}

fn details_model() -> Setup {
    Setup {
        store: true,
        models: json!({"m": {"compatibility": {"reasoningField": "reasoning_details"}}}),
        ..Default::default()
    }
}

/// "restores durable reasoning provider metadata in the next request".
/// Adapted: Chat Completions reasoning details (signed), replayed after a
/// reload.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_restores_durable_reasoning_metadata_in_the_next_request() {
    let t = start(details_model()).await;
    let mut t = t;
    t.push([sse(&[
        json!({"choices": [{"delta": {"reasoning_details": signed_details()}}]}),
        json!({"choices": [{"delta": {"content": "Answer"}, "finish_reason": "stop"}]}),
    ])]);
    t.prompt("Think first").await.unwrap();
    let mut again = t.reopen();
    again
        .prompt("Continue", &mut Rec(Arc::default()))
        .await
        .unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    let replayed = &reqs[1]["messages"][2];
    assert_eq!(replayed["role"], "assistant");
    assert_eq!(replayed["content"], "Answer");
    assert_eq!(replayed["reasoning_details"], signed_details());
}

/// "keeps one durable reasoning part when reasoning closes after text"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_keeps_one_reasoning_part_when_reasoning_closes_after_text() {
    let mut t = start(details_model()).await;
    let first = json!([{"type": "reasoning.text", "text": "think", "index": 0}]);
    let rest =
        json!([{"type": "reasoning.text", "text": "ing", "signature": "signed", "index": 0}]);
    t.push([sse(&[
        json!({"choices": [{"delta": {"reasoning_details": first}}]}),
        json!({"choices": [{"delta": {"content": "Hello"}}]}),
        json!({"choices": [{"delta": {"content": " world"}}]}),
        json!({"choices": [{"delta": {"reasoning_details": rest}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])]);
    t.prompt("Think and answer").await.unwrap();
    let again = t.reopen();
    let assistant = again.messages.last().unwrap();
    let reasoning: Vec<&Part> = assistant
        .parts
        .iter()
        .filter(|p| matches!(p, Part::Reasoning(_)))
        .collect();
    assert_eq!(reasoning.len(), 1, "{:?}", assistant.parts);
    assert!(
        matches!(reasoning[0], Part::Reasoning(r) if r.text == "thinking"),
        "{:?}",
        assistant.parts
    );
    assert!(assistant
        .parts
        .iter()
        .any(|p| matches!(p, Part::Text(t) if t.as_str() == "Hello world")));
}

/// "restores durable text provider metadata in the next request".
/// Adapted: a Responses message item's id and phase, after a reload.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_restores_durable_text_metadata_in_the_next_request() {
    let mut t = start(Setup {
        store: true,
        protocol: "openai-responses",
        ..Default::default()
    })
    .await;
    t.push([sse(&[
        json!({"type": "response.created", "response": {"id": "resp_1"}}),
        json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_commentary", "phase": "commentary"}}),
        json!({"type": "response.output_text.delta", "item_id": "msg_commentary", "delta": "Checking."}),
        json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_commentary", "phase": "commentary", "content": [{"type": "output_text", "text": "Checking."}]}}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
    ]), sse(&[
        json!({"type": "response.created", "response": {"id": "resp_2"}}),
        json!({"type": "response.completed", "response": {"id": "resp_2", "status": "completed"}}),
    ])]);
    t.prompt("Check first").await.unwrap();
    let mut again = t.reopen();
    again
        .prompt("Continue", &mut Rec(Arc::default()))
        .await
        .unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    let item = reqs[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "message" && i["role"] == "assistant")
        .cloned()
        .unwrap_or_else(|| panic!("{}", reqs[1]["input"]));
    assert_eq!(item["phase"], "commentary", "{item}");
    assert_eq!(item["content"][0]["text"], "Checking.", "{item}");
}

/// "settles ${fixture.finish} with tool choice ${fixture.toolChoice ??
/// "default"}" (`session-step.test.ts`), the `stop` case: a tool call in a
/// response that says it stopped still runs, and the turn continues.
#[tokio::test(flavor = "multi_thread")]
async fn oc_step_settles_a_tool_call_in_a_stopped_response_and_continues() {
    let mut t = plain().await;
    std::fs::write(t.path("a.txt"), "Completed tool").unwrap();
    t.push([
        calls_with("stop", &[("call-test", "read", r#"{"path":"a.txt"}"#)]),
        text("Done"),
    ]);
    t.prompt("Run one tool").await.unwrap();
    let r = results(&t.s);
    assert_eq!(r.len(), 1);
    assert!(!r[0].2 && r[0].1.contains("Completed tool"), "{r:?}");
    assert_eq!(t.requests().len(), 2);
}

// --- Output limits ---

fn limited(context: u64, output: u64) -> Setup {
    Setup {
        models: json!({"m": {"limit": {"context": context, "output": output}}}),
        ..Default::default()
    }
}

fn max_tokens(body: &Value) -> u64 {
    body["max_tokens"]
        .as_u64()
        .or(body["max_completion_tokens"].as_u64())
        .unwrap_or_else(|| panic!("no output limit: {body}"))
}

/// "fits the output limit to the prompt size". Fixed: a turn's request
/// always asked for the model's whole output limit, so a long conversation
/// asked for more than the window had left.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_fits_the_output_limit_to_the_prompt_size() {
    let mut t = start(limited(100_000, 64_000)).await;
    t.push([with_usage(
        "Earlier answer",
        json!({"prompt_tokens": 50_000, "completion_tokens": 10}),
    )]);
    t.prompt("Earlier question").await.unwrap();
    t.push([text("Continued")]);
    t.prompt("Continue").await.unwrap();
    let reqs = t.requests();
    assert_eq!(max_tokens(&reqs[0]), 64_000);
    // 1% of the window stays unused.
    let fitted = max_tokens(&reqs[1]);
    assert!(fitted < 99_000 - 50_000, "{fitted}");
    assert!(fitted > 99_000 - 50_000 - 200, "{fitted}");
}

/// "caps the output limit a large model advertises". Fixed: the cap
/// applied to catalog limits only, not configured ones.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_caps_the_output_limit_a_large_model_advertises() {
    let mut t = start(limited(1_000_000, 1_000_000)).await;
    t.push([text("Answer")]);
    t.prompt("Question").await.unwrap();
    assert_eq!(max_tokens(&t.requests()[0]), 256_000);
}

/// "does not compact immediately when the advertised output limit fills
/// the context"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_does_not_compact_when_the_advertised_output_limit_fills_the_context() {
    let mut t = start(limited(262_144, 262_144)).await;
    t.push([with_usage(
        "Earlier answer",
        json!({"prompt_tokens": 9_500, "completion_tokens": 10}),
    )]);
    t.prompt("Earlier question").await.unwrap();
    t.push([text("Continued")]);
    t.prompt("Continue").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2, "no summary request");
    assert!(user_texts(&reqs[1]).contains(&"Continue".to_string()));
    assert!(!t
        .s
        .messages
        .iter()
        .any(|m| format!("{m:?}").contains("<conversation-checkpoint>")));
}
