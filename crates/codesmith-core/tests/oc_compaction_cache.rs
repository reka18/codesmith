//! Ports of OpenCode's compaction prompt-cache tests
//! (`packages/core/test/session-compaction.test.ts` and the request-prefix
//! scenarios of `session-runner.test.ts`, MIT): a summary request is the
//! session's own request with the summary prompt appended, sized with the
//! provider's count while it applies. See `crates/codesmith-core/PARITY.md`.

mod compaction_harness;

use compaction_harness::*;
use serde_json::{json, Value};

/// OpenCode's fixture: a 200k window with a 32k output limit and a 25k
/// buffer, so automatic compaction starts at 175k.
const WINDOW: u64 = 200_000;
const OUTPUT: u32 = 32_000;
const BUFFER: u64 = WINDOW / 8;
const CEILING: u64 = WINDOW - BUFFER;
/// Past where a background summary starts, below the ceiling.
const SUMMARY_INPUT: u64 = CEILING * 9 / 10;

fn manual_only() -> Value {
    json!({"compaction": {"auto": false, "background": false, "buffer": BUFFER}})
}

fn max_tokens(body: &Value) -> u64 {
    body["max_completion_tokens"]
        .as_u64()
        .or(body["max_tokens"].as_u64())
        .unwrap()
}

/// `v` without its cache breakpoints.
fn uncached(mut v: Value) -> Value {
    match &mut v {
        Value::Object(m) => {
            m.remove("cache_control");
            for x in m.values_mut() {
                *x = uncached(x.take());
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                *x = uncached(x.take());
            }
        }
        _ => {}
    }
    v
}

fn messages(body: &Value) -> &Vec<Value> {
    body["messages"].as_array().unwrap()
}

/// "forked session compaction reuses the fork root prompt cache key"
/// (adapted: CodeSmith has no forks; the summary request carries the key
/// of the session's own requests).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_request_reuses_the_session_prompt_cache_key() {
    let (mut s, script, _d) =
        session_on("openai-responses", "gpt-5", WINDOW, OUTPUT, manual_only()).await;
    s.prompt("Summarize the forked conversation.", &mut Quiet)
        .await
        .unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let main = requests(&script, Kind::Main);
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0]["prompt_cache_key"], s.id.as_str());
    assert_eq!(summary[0]["prompt_cache_key"], main[0]["prompt_cache_key"]);
}

/// "summary cache prefix uses measured input with old reasoning
/// (background=false)": the characters of old reasoning overstate the
/// prompt, but the provider's count says the unchanged conversation fits.
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_cache_prefix_uses_measured_input_with_old_reasoning() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, manual_only()).await;
    push(
        &script,
        [
            MainReply {
                text: "Earlier answer".into(),
                reasoning: token_text(WINDOW * 3),
                prompt_tokens: SUMMARY_INPUT / 4,
                ..Default::default()
            },
            reply("Latest answer", SUMMARY_INPUT),
        ],
    );
    s.prompt("Earlier question", &mut Quiet).await.unwrap();
    s.prompt("Latest question", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let normal = requests(&script, Kind::Main).pop().unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    let sent = messages(&summary[0]);
    let n = messages(&normal).len();
    assert_eq!(sent[..n], messages(&normal)[..], "the session's prefix");
    assert_eq!(sent.len(), n + 2, "the latest answer, then the prompt");
    assert_eq!(
        sent.last().unwrap()["content"],
        codesmith_core::compaction::prompt(false)
    );
    assert_eq!(summary[0]["tools"], normal["tools"]);
    assert_eq!(max_tokens(&summary[0]), BUFFER);
}

/// "summary cache prefix still flattens genuinely oversized measured input"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_cache_prefix_still_flattens_genuinely_oversized_measured_input() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, manual_only()).await;
    push(&script, [reply("Latest answer", WINDOW + BUFFER)]);
    s.prompt(&token_text(CEILING), &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(summary[0].get("tools").is_none(), "no tool definitions");
    let sent = messages(&summary[0]);
    assert_eq!(sent.len(), 3, "system, the transcript, the prompt");
    assert!(sent[1]["content"]
        .to_string()
        .contains("characters omitted"));
}

/// "summary measured recovery shrinks from the rejected measured size"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_measured_recovery_shrinks_from_the_rejected_measured_size() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, manual_only()).await;
    let text = token_text(CEILING / 2);
    push(
        &script,
        [
            MainReply {
                text: String::new(),
                reasoning: token_text(CEILING / 2),
                prompt_tokens: CEILING / 2,
                ..Default::default()
            },
            reply("Latest answer", CEILING),
        ],
    );
    s.prompt(&text, &mut Quiet).await.unwrap();
    s.prompt(&text, &mut Quiet).await.unwrap();
    // Reject until the original exchange is actually shortened.
    let whole = text.clone();
    script.lock().unwrap().reject = Some(Box::new(move |b: &Value| b.to_string().contains(&whole)));
    s.compact(&mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    // The retained exchange fits the 85% and 70% targets; 50% of the
    // measured input forces it to be cut.
    assert_eq!(summary.len(), 4);
    assert!(summary[3]["messages"]
        .to_string()
        .contains("characters omitted"));
}

/// "summary measured recovery reuses usage only for the matching agent"
/// (adapted: the agent is switched by name and system prompt in place).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_measured_recovery_reuses_usage_only_for_the_matching_agent() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, manual_only()).await;
    push(
        &script,
        [MainReply {
            text: "Earlier answer".into(),
            reasoning: token_text(WINDOW * 3),
            prompt_tokens: SUMMARY_INPUT,
            ..Default::default()
        }],
    );
    s.prompt("Earlier question", &mut Quiet).await.unwrap();
    s.agent.name = "other".into();
    s.system = token_text(WINDOW - BUFFER / 5);
    s.compact(&mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(max_tokens(&summary[0]) <= BUFFER / 5);

    // On the measured agent, the measured prefix applies.
    let (mut s, script, _d) = session(WINDOW, OUTPUT, manual_only()).await;
    push(
        &script,
        [MainReply {
            text: "Earlier answer".into(),
            reasoning: token_text(WINDOW * 3),
            prompt_tokens: SUMMARY_INPUT,
            ..Default::default()
        }],
    );
    s.prompt("Earlier question", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let normal = requests(&script, Kind::Main).pop().unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0]["tools"], normal["tools"]);
    let n = messages(&normal).len();
    assert_eq!(messages(&summary[0])[..n], messages(&normal)[..]);
    assert_eq!(max_tokens(&summary[0]), BUFFER);
}

/// "preserves the session request prefix during ${reason} compaction
/// (${route.id})", on OpenAI Chat, Responses and Anthropic Messages
/// (adapted: no hooks or tool call, and the output limit the hook set is
/// the model's; the turn before is one answer that passed the threshold).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_preserves_the_session_request_prefix_during_compaction() {
    for (protocol, id, list) in [
        ("openai-chat", "gpt-5", "messages"),
        ("openai-responses", "gpt-5", "input"),
        ("anthropic-messages", "claude-sonnet-4-6", "messages"),
    ] {
        for manual in [true, false] {
            let (mut s, script, _d) = session_on(
                protocol,
                id,
                WINDOW,
                // OpenCode's hook asks for 4k of output, which leaves room
                // for the whole conversation.
                4_000,
                json!({"compaction": {"background": false}}),
            )
            .await;
            push(&script, [reply("Earlier answer", 185_000)]);
            s.prompt("Review these changes", &mut Quiet).await.unwrap();
            if manual {
                s.compact(&mut Quiet).await.unwrap();
            } else {
                push(&script, [reply("Continued", 1_000)]);
                s.prompt("Retained recent request", &mut Quiet)
                    .await
                    .unwrap();
            }
            // Cache breakpoints move to the newest message, as on every request.
            let normal = uncached(requests(&script, Kind::Main).remove(0));
            let compact = uncached(requests(&script, Kind::Summary).remove(0));
            let (n, c) = (
                normal[list].as_array().unwrap(),
                compact[list].as_array().unwrap(),
            );
            let what = format!("{protocol} manual={manual}");
            assert_eq!(c[..n.len()], n[..], "{what}");
            assert!(
                c.last().unwrap().to_string().contains("You MUST summarize"),
                "{what}"
            );
            assert_eq!(
                compact.to_string().contains("Retained recent request"),
                !manual,
                "{what}"
            );
            for field in ["model", "system", "tools", "instructions", "temperature"] {
                assert_eq!(compact.get(field), normal.get(field), "{what}: {field}");
            }
        }
    }
}

fn default_window() -> Value {
    json!({"compaction": {"auto": false, "background": false}})
}

/// "summary compaction past the ceiling keeps the conversation unchanged
/// while the window fits"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_past_the_ceiling_keeps_the_conversation_unchanged_while_the_window_fits(
) {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, default_window()).await;
    // About 182k estimated tokens: past the 170k ceiling, within the 184k
    // left after reserving room for the summary.
    // (Here with the system prompt and tool definitions counted in.)
    s.prompt("hi", &mut Quiet).await.unwrap();
    let each = (181_000 - s.estimate_prompt()) as usize / 2 * 4;
    s.messages.clear();
    let first = format!("first {}", "x".repeat(each));
    s.prompt(&first, &mut Quiet).await.unwrap();
    s.prompt(&format!("second {}", "x".repeat(each)), &mut Quiet)
        .await
        .unwrap();
    let size = s.estimate_prompt();
    assert!(size > 170_000 && size < 184_000, "{size}");
    s.compact(&mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    let sent = messages(&summary[0]);
    assert_eq!(sent.len(), 6, "system, two exchanges, the prompt");
    assert_eq!(sent[1]["content"], first.as_str());
    assert!(max_tokens(&summary[0]) < 20_000);
}

/// "summary compaction sends the history before the tail unchanged when the
/// whole conversation does not fit" (adapted: `keep` is raised so the 30k
/// latest exchange is the kept tail; CodeSmith keeps whole turns within it).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_sends_the_history_before_the_tail_unchanged_when_the_whole_does_not_fit(
) {
    let (mut s, script, _d) = session(
        WINDOW,
        OUTPUT,
        json!({"compaction": {"auto": false, "background": false, "keep": {"tokens": 40_000}}}),
    )
    .await;
    let older = format!("older {}", "x".repeat(640_000));
    s.prompt(&older, &mut Quiet).await.unwrap();
    s.prompt(&format!("latest {}", "x".repeat(120_000)), &mut Quiet)
        .await
        .unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    let sent = messages(&summary[0]);
    assert_eq!(sent.len(), 4, "system, the older exchange, the prompt");
    assert_eq!(sent[1]["content"], older.as_str());
    assert_eq!(
        sent[3]["content"],
        codesmith_core::compaction::prompt(false)
    );
}

/// "compaction lowers the output limit when it sends a request it could
/// not shrink": a previous summary of about 250k tokens always stays whole.
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_lowers_the_output_limit_when_it_sends_a_request_it_could_not_shrink() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, default_window()).await;
    script
        .lock()
        .unwrap()
        .summaries
        .push_back(format!("## Objective\n{}", "x".repeat(1_000_000)));
    s.prompt("First question", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    s.prompt("Next question", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 2);
    assert_eq!(max_tokens(&summary[1]), 1_024);
}

/// "compaction keeps the output limit for a request that fits its target"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_keeps_the_output_limit_for_a_request_that_fits_its_target() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, default_window()).await;
    // About 160k estimated tokens, under the 170k target.
    s.prompt(&"x".repeat(640_000), &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert_eq!(max_tokens(&summary[0]), 30_000);
}
