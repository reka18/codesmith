//! Parity with OpenCode's stream handling: block lifecycle events, streamed
//! tool input, partial output on errors (`error-partial.test.ts`) and Gemini
//! response continuation (`provider/gemini.test.ts`, "Gemini response
//! continuation"). The partial JSON parser's own ports are in
//! `partial_json.rs`.

use super::{anth, chat, gem, item_call, item_text, resp, run, sse};
use crate::{
    assemble, collect, drive, drive_resuming, gemini, openai, Collector, Error, Event, Part,
    Protocol, Reasoning, ReasoningMeta, Reopen, Response, Usage,
};
use crate::{FinishReason, Reason};
use futures_util::{FutureExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn ok(events: Result<Vec<Event>, Error>) -> Vec<Event> {
    events.expect("stream decodes")
}

fn text_events(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::TextStart(_) | Event::TextDelta(_) | Event::TextEnd(_)
            )
        })
        .cloned()
        .collect()
}

fn tool_events(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::ToolInputStart { .. }
                    | Event::ToolInputDelta { .. }
                    | Event::ToolInputEnd { .. }
                    | Event::ToolCall { .. }
            )
        })
        .cloned()
        .collect()
}

fn reason(events: &[Event]) -> String {
    match events.last() {
        Some(Event::Finish { reason, .. }) => reason.normalized.as_str().to_string(),
        other => panic!("no finish: {other:?}"),
    }
}

fn t(s: &str) -> Event {
    Event::TextDelta(s.into())
}
fn ts(s: &str) -> Event {
    Event::TextStart(s.into())
}
fn te(s: &str) -> Event {
    Event::TextEnd(s.into())
}

// ---- block lifecycle --------------------------------------------------------

#[test]
fn block_lifecycle() {
    run(&[
        (
            "openai-responses :: routes reasoning summary events by output index",
            || {
                let evs = ok(resp(&sse(&[
                    json!({"type": "response.output_item.added", "output_index": 4, "item": {"type": "reasoning", "id": "rs_1"}}),
                    json!({"type": "response.reasoning_summary_part.added", "output_index": 4, "item_id": "wrong_reasoning", "summary_index": 0}),
                    json!({"type": "response.reasoning_summary_text.delta", "output_index": 4, "item_id": "wrong_reasoning", "summary_index": 0, "delta": "Thinking"}),
                    json!({"type": "response.reasoning_summary_part.done", "output_index": 4, "item_id": "wrong_reasoning", "summary_index": 0}),
                    json!({"type": "response.output_item.done", "output_index": 4, "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "state"}}),
                    json!({"type": "response.completed", "response": {"id": "resp_1"}}),
                ])));
                let mut meta = ReasoningMeta::of(Protocol::OpenaiResponses);
                meta.item_id = Some("rs_1".into());
                meta.encrypted = Some("state".into());
                assert_eq!(
                    collect(&evs),
                    [Part::Reasoning(Reasoning::new("Thinking", meta))]
                );
            },
        ),
        (
            "openai-responses :: routes native reasoning text deltas by output index",
            || {
                let evs = ok(resp(&sse(&[
                    json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "reasoning", "id": "rs_1"}}),
                    json!({"type": "response.reasoning_text.delta", "output_index": 1, "item_id": "wrong_reasoning", "delta": "Raw"}),
                    json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "reasoning", "id": "rs_1"}}),
                    json!({"type": "response.completed", "response": {"id": "resp_1"}}),
                ])));
                assert!(matches!(&collect(&evs)[..], [Part::Reasoning(r)] if r.text == "Raw"));
            },
        ),
        (
            "openai-responses :: closes sequential output messages before starting the next",
            || {
                let evs = ok(resp(&sse(&[
                    json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
                    json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "First"}),
                    json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1"}}),
                    json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_2"}}),
                    json!({"type": "response.output_text.delta", "item_id": "msg_2", "delta": "Second"}),
                    json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_2"}}),
                    json!({"type": "response.completed", "response": {"id": "resp_1"}}),
                ])));
                assert_eq!(
                    text_events(&evs),
                    [
                        ts("msg_1"),
                        t("First"),
                        te("msg_1"),
                        ts("msg_2"),
                        t("Second"),
                        te("msg_2")
                    ]
                );
                assert_eq!(
                    collect(&evs),
                    [item_text("First", "msg_1"), item_text("Second", "msg_2")]
                );
            },
        ),
        (
            "openai-responses :: concludes text at implicit message boundaries",
            || {
                let evs = ok(resp(&sse(&[
                    json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_0"}}),
                    json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
                    json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "First"}),
                    json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_2"}}),
                    json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": " late"}),
                    json!({"type": "response.output_text.delta", "item_id": "msg_0", "delta": " stale"}),
                    json!({"type": "response.output_text.delta", "item_id": "msg_2", "delta": "Second"}),
                    json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_2"}}),
                    json!({"type": "response.completed", "response": {"id": "resp_1"}}),
                ])));
                assert_eq!(
                    text_events(&evs),
                    [
                        ts("msg_1"),
                        t("First"),
                        te("msg_1"),
                        ts("msg_2"),
                        t("Second"),
                        te("msg_2")
                    ]
                );
            },
        ),
        (
            "openai-responses :: opens the tool lifecycle for a done-only function call",
            || {
                let evs = ok(resp(&sse(&[
                    json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": "{\"query\":\"weather\"}"}}),
                    json!({"type": "response.completed", "response": {"id": "resp_1"}}),
                ])));
                assert_eq!(
                    tool_events(&evs),
                    [
                        Event::ToolInputStart {
                            id: "call_1".into(),
                            name: "lookup".into()
                        },
                        Event::ToolInputEnd {
                            id: "call_1".into()
                        },
                        Event::ToolCall {
                            id: "call_1".into(),
                            name: "lookup".into(),
                            input: json!({"query": "weather"})
                        },
                    ]
                );
                assert_eq!(reason(&evs), "tool-calls");
            },
        ),
        (
            "openai-responses :: ignores duplicate item start events",
            || {
                let call = json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": ""}});
                let evs = ok(resp(&sse(&[
                    json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1"}}),
                    json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1"}}),
                    json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "summary_index": 0, "delta": "Think"}),
                    json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1"}}),
                    call.clone(),
                    call,
                    json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"query\":\"weather\"}"}),
                    json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": "{\"query\":\"weather\"}"}}),
                    json!({"type": "response.completed", "response": {"id": "resp_1"}}),
                ])));
                let count = |f: fn(&Event) -> bool| evs.iter().filter(|e| f(e)).count();
                assert_eq!(count(|e| matches!(e, Event::ReasoningStart(_))), 1);
                assert_eq!(count(|e| matches!(e, Event::ReasoningEnd(_))), 1);
                assert_eq!(count(|e| matches!(e, Event::ToolInputStart { .. })), 1);
                let parts = collect(&evs);
                assert!(matches!(&parts[0], Part::Reasoning(r) if r.text == "Think"));
                assert_eq!(
                    parts[1..],
                    [item_call(
                        "call_1",
                        "lookup",
                        json!({"query": "weather"}),
                        "fc_1"
                    )]
                );
            },
        ),
        (
            "openai-chat :: preserves scalar reasoning after content starts in one lifecycle",
            || {
                let details = json!([{"type": "reasoning.text", "text": "detail", "format": "unknown", "index": 0}]);
                let evs = ok(chat(&sse(&[
                    json!({"choices": [{"delta": {"reasoning_details": details}}]}),
                    json!({"choices": [{"delta": {"content": "Hello"}}]}),
                    json!({"choices": [{"delta": {"reasoning": "scalar"}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                ])));
                one_reasoning_lifecycle(&evs, "detailscalar", details);
            },
        ),
        (
            "openai-chat :: keeps one reasoning lifecycle across many content chunks",
            || {
                let details = json!([{"type": "reasoning.text", "text": "thinking", "format": "anthropic-claude-v1", "index": 0}]);
                let mut chunks = vec![
                    json!({"choices": [{"delta": {"reasoning": "thinking", "reasoning_details": details}}]}),
                ];
                chunks.extend(
                    (0..25).map(
                        |i| json!({"choices": [{"delta": {"content": format!("chunk-{i} ")}}]}),
                    ),
                );
                chunks.push(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
                let evs = ok(chat(&sse(&chunks)));
                one_reasoning_lifecycle(&evs, "thinking", details);
                let text: String = (0..25).map(|i| format!("chunk-{i} ")).collect();
                assert_eq!(
                    Response {
                        parts: collect(&evs),
                        ..Default::default()
                    }
                    .text(),
                    text
                );
            },
        ),
        ("anthropic-messages :: retains empty text blocks", || {
            let evs = ok(anth(&sse(&[
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
                json!({"type": "message_stop"}),
            ])));
            assert_eq!(collect(&evs), [Part::text("")]);
        }),
        (
            "gemini :: assigns unique ids to separated reasoning blocks",
            || {
                let part = |text: &str, thought: bool, sig: &str| json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text, "thought": thought, "thoughtSignature": sig}]}}]});
                let mut last = part("Y", false, "text_sig_y");
                last["candidates"][0]["finishReason"] = json!("STOP");
                let evs = ok(gem(&sse(&[
                    part("A", true, "reasoning_sig_a"),
                    part("X", false, "text_sig_x"),
                    part("B", true, "reasoning_sig_b"),
                    last,
                ])));
                let ids = |f: fn(&Event) -> Option<&String>| {
                    evs.iter().filter_map(f).cloned().collect::<Vec<_>>()
                };
                assert_eq!(
                    ids(|e| match e {
                        Event::ReasoningStart(id) => Some(id),
                        _ => None,
                    }),
                    ["reasoning-0", "reasoning-1"]
                );
                assert_eq!(
                    ids(|e| match e {
                        Event::TextStart(id) => Some(id),
                        _ => None,
                    }),
                    ["text-0", "text-1"]
                );
                let sig = |s: &str| {
                    let mut m = ReasoningMeta::of(Protocol::GoogleGemini);
                    m.signature = Some(s.into());
                    m
                };
                assert_eq!(
                    collect(&evs),
                    [
                        Part::Reasoning(Reasoning::new("A", sig("reasoning_sig_a"))),
                        // Each text part keeps its own signature.
                        Part::Text(crate::Text::new("X", sig("text_sig_x"))),
                        Part::Reasoning(Reasoning::new("B", sig("reasoning_sig_b"))),
                        Part::Text(crate::Text::new("Y", sig("text_sig_y"))),
                    ]
                );
            },
        ),
        (
            "gemini :: separates text blocks around streamed tool calls",
            || {
                let evs = ok(gem(&sse(&[json!({"candidates": [{
                    "content": {"role": "model", "parts": [
                        {"text": "before"},
                        {"functionCall": {"id": "call_1", "name": "lookup", "args": {"query": "weather"}}},
                        {"text": "after"},
                    ]},
                    "finishReason": "STOP",
                }]})])));
                let call = Event::ToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    input: json!({"query": "weather"}),
                };
                assert_eq!(
                    evs[..7],
                    [
                        ts("text-0"),
                        t("before"),
                        te("text-0"),
                        call,
                        ts("text-1"),
                        t("after"),
                        te("text-1")
                    ]
                );
                assert_eq!(
                    collect(&evs),
                    [
                        Part::Text("before".into()),
                        Part::ToolCall {
                            id: "call_1".into(),
                            name: "lookup".into(),
                            input: json!({"query": "weather"}),
                            meta: None,
                        },
                        Part::Text("after".into()),
                    ]
                );
                assert_eq!(reason(&evs), "tool-calls");
            },
        ),
    ]);
}

/// One reasoning start and end, and one reasoning part with the field and
/// details it streamed with.
fn one_reasoning_lifecycle(evs: &[Event], text: &str, details: Value) {
    let count = |f: fn(&Event) -> bool| evs.iter().filter(|e| f(e)).count();
    assert_eq!(count(|e| matches!(e, Event::ReasoningStart(_))), 1);
    assert_eq!(count(|e| matches!(e, Event::ReasoningEnd(_))), 1);
    let reasoning: Vec<Reasoning> = collect(evs)
        .into_iter()
        .filter_map(|p| match p {
            Part::Reasoning(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning.len(), 1);
    assert_eq!(reasoning[0].text, text);
    let meta = reasoning[0].meta.as_ref().unwrap();
    assert_eq!(meta.field.as_deref(), Some("reasoning"));
    assert_eq!(meta.details.as_ref().map(|d| json!(d)), Some(details));
}

// ---- streamed tool input ----------------------------------------------------

#[test]
fn streamed_tool_input() {
    // Each protocol that streams arguments opens the call's input, streams
    // it, and ends it just before the call; the collector shows the input
    // parsed so far while it forms.
    fn forming_after(evs: &[Event], n: usize) -> Value {
        let mut c = Collector::default();
        for e in &evs[..n] {
            c.push(e);
        }
        c.forming()
            .first()
            .map(|f| f.2.clone())
            .unwrap_or(Value::Null)
    }
    let chat_evs = ok(chat(&sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "write", "arguments": "{\"path\":\"a.rs\",\"content\":\"fn"}}]}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": " main\"}"}}]}, "finish_reason": "tool_calls"}]}),
    ])));
    let anth_evs = ok(anth(&sse(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call_1", "name": "write"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"a.rs\",\"content\":\"fn"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": " main\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ])));
    let resp_evs = ok(resp(&sse(&[
        json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "write", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"path\":\"a.rs\",\"content\":\"fn"}),
        json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": " main\"}"}),
        json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "write", "arguments": "{\"path\":\"a.rs\",\"content\":\"fn main\"}"}}),
        json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ])));
    for (name, evs) in [
        ("chat", chat_evs),
        ("anthropic", anth_evs),
        ("responses", resp_evs),
    ] {
        let delta = |s: &str| Event::ToolInputDelta {
            id: "call_1".into(),
            text: s.into(),
        };
        assert_eq!(
            tool_events(&evs),
            [
                Event::ToolInputStart {
                    id: "call_1".into(),
                    name: "write".into()
                },
                delta("{\"path\":\"a.rs\",\"content\":\"fn"),
                delta(" main\"}"),
                Event::ToolInputEnd {
                    id: "call_1".into()
                },
                Event::ToolCall {
                    id: "call_1".into(),
                    name: "write".into(),
                    input: json!({"path": "a.rs", "content": "fn main"})
                },
            ],
            "{name}"
        );
        let first = evs
            .iter()
            .position(|e| matches!(e, Event::ToolInputDelta { .. }))
            .unwrap();
        assert_eq!(
            forming_after(&evs, first + 1),
            json!({"path": "a.rs", "content": "fn"}),
            "{name}"
        );
        // The call is complete: nothing forms any more.
        assert_eq!(forming_after(&evs, evs.len()), Value::Null, "{name}");
    }
}

// ---- partial output on errors -------------------------------------------------

fn scripted(events: Vec<Result<Event, Error>>) -> crate::EventStream {
    futures_util::stream::iter(events).boxed()
}

fn overloaded() -> Error {
    Error::Provider {
        message: "Overloaded".into(),
        retryable: true,
    }
}

#[tokio::test]
async fn error_partial() {
    // error-partial :: generate keeps the output that arrived before a mid-stream failure
    let usage = Usage {
        input: 5,
        output: 2,
        ..Default::default()
    };
    let text_then_fail = || {
        scripted(vec![
            Ok(ts("text-0")),
            Ok(t("Partial answer")),
            Ok(Event::Finish {
                reason: FinishReason::new(Reason::Stop, "stop"),
                usage,
                raw_usage: None,
            }),
            Err(overloaded()),
        ])
    };
    let f = assemble(text_then_fail(), |_| {}).await.unwrap_err();
    assert!(matches!(&f.error, Error::Provider { message, .. } if message == "Overloaded"));
    let partial = f.partial.unwrap();
    assert_eq!(partial.text(), "Partial answer");
    assert_eq!(partial.parts, [Part::Text("Partial answer".into())]);
    assert_eq!(partial.usage.output, 2);
    assert_eq!(partial.reason.normalized, Reason::Error);

    // error-partial :: generate keeps the same error when the failure comes before any output
    let f = assemble(scripted(vec![Err(overloaded())]), |_| {})
        .await
        .unwrap_err();
    assert!(f.partial.is_none());
    assert_eq!(f.to_string(), "Overloaded");

    // error-partial :: stream keeps the output that arrived before a mid-stream failure
    let mut seen = String::new();
    let f = assemble(text_then_fail(), |e| {
        if let Event::TextDelta(d) = e {
            seen.push_str(d)
        }
    })
    .await
    .unwrap_err();
    assert_eq!(seen, "Partial answer");
    assert_eq!(f.partial.unwrap().text(), "Partial answer");

    // error-partial :: rejects with the root-exported AIError carrying partial
    let body = sse(&[
        json!({"choices": [{"delta": {"content": "Partial answer"}}]}),
        json!({"error": {"message": "Overloaded"}}),
    ]);
    let stream = drive(chunked(vec![body], false), openai::Decode::default());
    let f = assemble(stream, |_| {}).await.unwrap_err();
    assert_eq!(f.error.to_string(), "Overloaded");
    assert_eq!(f.partial.unwrap().text(), "Partial answer");

    // A stream cut off before its finish fails with what it streamed, and a
    // complete one assembles.
    let f = assemble(scripted(vec![Ok(t("Cut"))]), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(f.error, Error::Transport(_)));
    assert_eq!(f.partial.unwrap().text(), "Cut");
    let r = assemble(
        scripted(vec![
            Ok(t("Done")),
            Ok(Event::Finish {
                reason: FinishReason::new(Reason::Stop, "stop"),
                usage,
                raw_usage: None,
            }),
        ]),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(
        r,
        Response {
            parts: vec![Part::Text("Done".into())],
            reason: FinishReason::new(Reason::Stop, "stop"),
            usage,
            raw_usage: None,
        }
    );
}

// ---- Gemini response continuation ---------------------------------------------

/// A response whose body yields `chunks`, then (when `drop`) a connection
/// reset.
fn chunked(chunks: Vec<String>, drop: bool) -> reqwest::Response {
    let items = chunks
        .into_iter()
        .map(|c| Ok::<_, std::io::Error>(c.into_bytes()))
        .chain(drop.then(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset",
            ))
        }));
    let s = futures_util::stream::iter(items.collect::<Vec<_>>());
    reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(s)))
}

enum Reply {
    Body(Vec<Value>),
    Dropped(Vec<Value>),
}

impl Reply {
    fn response(self) -> reqwest::Response {
        match self {
            Reply::Body(v) => chunked(vec![sse(&v)], false),
            Reply::Dropped(v) => chunked(vec![sse(&v)], true),
        }
    }
}

struct Continued {
    events: Vec<Event>,
    error: Option<Error>,
    /// Every request body: the first, then each continuation.
    bodies: Vec<Value>,
}

impl Continued {
    fn text(&self) -> String {
        Response {
            parts: collect(&self.events),
            ..Default::default()
        }
        .text()
    }
}

/// Run a Gemini response through the continuing driver, serving `replies` in
/// order and recording each request body.
async fn continued(replies: Vec<Reply>) -> Continued {
    let first = json!({"contents": [{"role": "user", "parts": [{"text": "Count."}]}]});
    let bodies = Arc::new(Mutex::new(vec![first.clone()]));
    let mut replies = replies.into_iter();
    let resp = replies.next().unwrap().response();
    let rest = Arc::new(Mutex::new(replies.collect::<Vec<_>>().into_iter()));
    let log = bodies.clone();
    let reopen: Reopen = Box::new(move |token: String| {
        log.lock().unwrap().push(gemini::continued(&first, &token));
        let next = rest.lock().unwrap().next();
        async move {
            next.map(Reply::response)
                .ok_or_else(|| Error::Transport("no more replies".into()))
        }
        .boxed()
    });
    let all: Vec<_> = drive_resuming(resp, gemini::Decode::default(), Some(reopen))
        .collect()
        .await;
    let mut events = Vec::new();
    let mut error = None;
    for e in all {
        match e {
            Ok(e) => events.push(e),
            Err(e) => error = Some(e),
        }
    }
    let bodies = bodies.lock().unwrap().clone();
    Continued {
        events,
        error,
        bodies,
    }
}

fn text(value: &str, extra: Value) -> Value {
    let mut c = json!({"content": {"role": "model", "parts": [{"text": value}]}});
    if let Some(o) = extra.as_object() {
        for (k, v) in o {
            c[k] = v.clone();
        }
    }
    json!({"candidates": [c]})
}

fn continuation(token: Option<&str>) -> Value {
    let mut c = json!({"content": {"role": "model", "parts": [{"text": ""}]}, "finishReason": "CONTINUATION"});
    if let Some(t) = token {
        c["continuationToken"] = json!(t);
    }
    json!({"candidates": [c]})
}

fn with_usage(mut v: Value, usage: Value) -> Value {
    v["usageMetadata"] = usage;
    v
}

fn finish_of(c: &Continued) -> (String, Usage) {
    match c.events.last() {
        Some(Event::Finish { reason, usage, .. }) => {
            (reason.normalized.as_str().to_string(), *usage)
        }
        other => panic!("no finish: {other:?} ({:?})", c.error),
    }
}

#[tokio::test]
async fn gemini_response_continuation() {
    let none = json!({});

    // gemini :: stitches text split across three requests into one block
    let c = continued(vec![
        Reply::Body(vec![
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "plan", "thought": true, "thoughtSignature": "sig_1"}]}}]}),
            text("THIRTEEN THOUSAND TWO", none.clone()),
            continuation(Some("token-1")),
        ]),
        Reply::Body(vec![
            text(" HUNDRED SEVEN", none.clone()),
            // The finishing chunk's token supersedes an earlier checkpoint's.
            text("TY-TWO", json!({"continuationToken": "checkpoint"})),
            continuation(Some("token-2")),
        ]),
        Reply::Body(vec![text(" DONE", json!({"finishReason": "STOP"}))]),
    ])
    .await;
    assert!(c.error.is_none(), "{:?}", c.error);
    assert_eq!(c.text(), "THIRTEEN THOUSAND TWO HUNDRED SEVENTY-TWO DONE");
    let kinds: Vec<&str> = c
        .events
        .iter()
        .filter_map(|e| match e {
            Event::ReasoningStart(_) => Some("reasoning-start"),
            Event::ReasoningDelta(_) => Some("reasoning-delta"),
            Event::ReasoningEnd(_) => Some("reasoning-end"),
            Event::TextStart(_) => Some("text-start"),
            Event::TextEnd(_) => Some("text-end"),
            Event::Finish { .. } => Some("finish"),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "reasoning-start",
            "reasoning-delta",
            "reasoning-end",
            "text-start",
            "text-end",
            "finish"
        ]
    );
    assert!(c
        .events
        .iter()
        .any(|e| matches!(e, Event::ReasoningEnd(m) if m.signature.as_deref() == Some("sig_1"))));
    assert_eq!(finish_of(&c).0, "stop");
    assert_eq!(c.bodies.len(), 3);
    assert!(c.bodies[0].get("continuationToken").is_none());
    let with = |t: &str| {
        let mut b = c.bodies[0].clone();
        b["continuationToken"] = json!(t);
        b
    };
    assert_eq!(c.bodies[1], with("token-1"));
    assert_eq!(c.bodies[2], with("token-2"));

    // gemini :: finishes with a signed tool call from a later request
    let c = continued(vec![
        Reply::Body(vec![text("Looking", none.clone()), continuation(Some("token-1"))]),
        Reply::Body(vec![json!({"candidates": [{
            "content": {"role": "model", "parts": [
                {"text": " it up."},
                {"functionCall": {"id": "call_1", "name": "lookup", "args": {}}, "thoughtSignature": "call_sig"},
            ]},
            "finishReason": "STOP",
        }]})]),
    ])
    .await;
    assert_eq!(c.text(), "Looking it up.");
    assert_eq!(
        c.events
            .iter()
            .filter(|e| matches!(e, Event::TextStart(_)))
            .count(),
        1
    );
    assert!(c
        .events
        .iter()
        .any(|e| matches!(e, Event::ToolCall { id, .. } if id == "call_1~call_sig")));
    assert_eq!(finish_of(&c).0, "tool-calls");

    // gemini :: forgets a block once the resumed request finishes
    let c = continued(vec![
        Reply::Body(vec![
            text("THIRTEEN", json!({"continuationToken": "checkpoint-1"})),
            json!({"promptFeedback": {"blockReason": "OTHER"}}),
        ]),
        Reply::Body(vec![text(" THOUSAND", json!({"finishReason": "STOP"}))]),
    ])
    .await;
    assert_eq!(c.text(), "THIRTEEN THOUSAND");
    assert_eq!(finish_of(&c).0, "stop");

    // gemini :: resumes a dropped stream from its latest checkpoint
    let c = continued(vec![
        Reply::Dropped(vec![
            text("THIRTEEN", none.clone()),
            text(" THOUSAND", json!({"continuationToken": "checkpoint-1"})),
        ]),
        Reply::Body(vec![text(" TWO", json!({"finishReason": "STOP"}))]),
    ])
    .await;
    assert_eq!(c.text(), "THIRTEEN THOUSAND TWO");
    assert_eq!(finish_of(&c).0, "stop");
    assert_eq!(c.bodies[1]["continuationToken"], "checkpoint-1");

    // Not in OpenCode (which resumes and repeats the output): output after
    // the latest checkpoint was already passed on, and a continued request
    // would generate it again (a call would run twice), so a request that
    // drops or ends past its checkpoint is not continued; it fails
    // retryably and the whole response is retried.
    let replayed = |first: Reply| async {
        continued(vec![
            first,
            Reply::Body(vec![json!({"candidates": [{
                "content": {"role": "model", "parts": [
                    {"text": " THOUSAND"},
                    {"functionCall": {"id": "call_1", "name": "lookup", "args": {}}},
                ]},
                "finishReason": "STOP",
            }]})]),
        ])
        .await
    };
    let past = || {
        vec![
            text("THIRTEEN", json!({"continuationToken": "checkpoint-1"})),
            json!({"candidates": [{"content": {"role": "model", "parts": [
                {"text": " THOUSAND"},
                {"functionCall": {"id": "call_1", "name": "lookup", "args": {}}},
            ]}}]}),
        ]
    };
    for first in [Reply::Dropped(past()), Reply::Body(past())] {
        let c = replayed(first).await;
        assert_eq!(c.bodies.len(), 1, "not continued");
        assert!(
            c.error.as_ref().is_some_and(Error::retryable),
            "{:?}",
            c.error
        );
        assert_eq!(
            c.events
                .iter()
                .filter(|e| matches!(e, Event::ToolCall { .. }))
                .count(),
            1
        );
        assert!(!c.events.iter().any(|e| matches!(e, Event::Finish { .. })));
    }

    // gemini :: counts the usage of a dropped request once resumed
    let c = continued(vec![
        Reply::Dropped(vec![
            with_usage(
                text("THIRTEEN", none.clone()),
                json!({"promptTokenCount": 120, "candidatesTokenCount": 5}),
            ),
            with_usage(
                text(" THOUSAND", json!({"continuationToken": "checkpoint-1"})),
                json!({"promptTokenCount": 142, "candidatesTokenCount": 32768, "totalTokenCount": 32910}),
            ),
        ]),
        Reply::Body(vec![with_usage(
            text(" TWO", json!({"finishReason": "STOP"})),
            json!({"promptTokenCount": 32910, "candidatesTokenCount": 100, "totalTokenCount": 33010}),
        )]),
    ])
    .await;
    assert_eq!(
        finish_of(&c).1,
        Usage {
            input: 142,
            output: 32868,
            total: Some(33010),
            ..Default::default()
        }
    );
    // Complete billed counts stay in the raw usage.
    assert!(matches!(
        c.events.last(),
        Some(Event::Finish { raw_usage: Some(raw), .. }) if *raw == json!({
            "promptTokenCount": 33052,
            "candidatesTokenCount": 32868,
            "totalTokenCount": 65920,
            "requests": 2,
        })
    ));

    // gemini :: continues a stream that ends at a checkpoint without finishing
    let c = continued(vec![
        Reply::Body(vec![text(
            "THIRTEEN",
            json!({"continuationToken": "checkpoint-1"}),
        )]),
        Reply::Body(vec![text(" THOUSAND", json!({"finishReason": "STOP"}))]),
    ])
    .await;
    assert_eq!(c.text(), "THIRTEEN THOUSAND");
    assert_eq!(c.bodies[1]["continuationToken"], "checkpoint-1");

    // gemini :: fails a stream dropped before any checkpoint
    let c = continued(vec![Reply::Dropped(vec![text("Partial", none.clone())])]).await;
    assert!(
        matches!(c.error, Some(Error::Transport(_))),
        "{:?}",
        c.error
    );
    assert_eq!(c.bodies.len(), 1);

    // gemini :: fails a continuation without a token
    let c = continued(vec![Reply::Body(vec![
        text("Partial", none.clone()),
        continuation(None),
    ])])
    .await;
    let e = c.error.expect("fails");
    assert!(!e.retryable());
    assert!(e
        .to_string()
        .contains("Gemini continued the response without a token"));
    assert_eq!(c.bodies.len(), 1);

    // gemini :: does not continue a length finish
    let c = continued(vec![Reply::Body(vec![text(
        "Partial",
        json!({"finishReason": "MAX_TOKENS", "continuationToken": "token-1"}),
    )])])
    .await;
    assert_eq!(finish_of(&c).0, "length");
    assert_eq!(c.bodies.len(), 1);

    // gemini :: reports the first request's input with all output
    let usage = [
        json!({"promptTokenCount": 142, "toolUsePromptTokenCount": 5, "candidatesTokenCount": 32768, "totalTokenCount": 32915}),
        json!({"promptTokenCount": 32910, "candidatesTokenCount": 32768, "totalTokenCount": 65678}),
        json!({"promptTokenCount": 65678, "cachedContentTokenCount": 32911, "candidatesTokenCount": 32768, "totalTokenCount": 98446}),
        json!({"promptTokenCount": 98446, "cachedContentTokenCount": 65679, "toolUsePromptTokenCount": 7, "candidatesTokenCount": 7257, "totalTokenCount": 105710}),
    ];
    let c = continued(vec![
        Reply::Body(vec![
            with_usage(
                text("a", none.clone()),
                json!({"promptTokenCount": 120, "candidatesTokenCount": 10, "totalTokenCount": 130}),
            ),
            with_usage(continuation(Some("token-1")), usage[0].clone()),
        ]),
        Reply::Body(vec![with_usage(
            continuation(Some("token-2")),
            usage[1].clone(),
        )]),
        Reply::Body(vec![with_usage(
            continuation(Some("token-3")),
            usage[2].clone(),
        )]),
        Reply::Body(vec![with_usage(
            text("b", json!({"finishReason": "STOP"})),
            usage[3].clone(),
        )]),
    ])
    .await;
    assert_eq!(
        finish_of(&c).1,
        Usage {
            input: 147,
            output: 105561,
            total: Some(105708),
            ..Default::default()
        }
    );
    assert!(matches!(
        c.events.last(),
        Some(Event::Finish { raw_usage: Some(raw), .. }) if *raw == json!({
            "promptTokenCount": 197176,
            "toolUsePromptTokenCount": 12,
            "cachedContentTokenCount": 98590,
            "candidatesTokenCount": 105561,
            "totalTokenCount": 302749,
            "requests": 4,
        })
    ));
    assert_eq!(c.text(), "ab");
}

#[tokio::test]
async fn gemini_does_not_resume_a_response_cut_off_by_its_own_timeout() {
    // gemini :: does not resume a response cut off by its own timeout
    // (adapted: a reqwest read timeout stands for OpenCode's request timeout)
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = l.accept().await.unwrap();
        // Read the request (a bodiless POST) before answering it.
        let mut req = Vec::new();
        let mut buf = [0u8; 1024];
        while !req.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = s.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
        }
        let body = sse(&[text(
            "THIRTEEN",
            json!({"continuationToken": "checkpoint-1"}),
        )]);
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).await.unwrap();
        // Stall without ending the body.
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    let client = reqwest::Client::builder()
        .read_timeout(std::time::Duration::from_millis(300))
        .build()
        .unwrap();
    let resp = client.post(format!("http://{addr}/")).send().await.unwrap();
    let reopened = Arc::new(Mutex::new(0));
    let count = reopened.clone();
    let reopen: Reopen = Box::new(move |_| {
        *count.lock().unwrap() += 1;
        async { Err(Error::Transport("not resumed".into())) }.boxed()
    });
    let all: Vec<_> = drive_resuming(resp, gemini::Decode::default(), Some(reopen))
        .collect()
        .await;
    let last = all.last().unwrap();
    assert!(
        matches!(last, Err(Error::Transport(m)) if m != "not resumed"),
        "{last:?}"
    );
    assert_eq!(*reopened.lock().unwrap(), 0);
}
