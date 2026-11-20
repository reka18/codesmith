//! Parity with OpenCode's provider unit tests.
//!
//! The cases below port tests from OpenCode's `packages/ai/test` (MIT,
//! copyright the OpenCode authors): `provider/openai-chat.test.ts`,
//! `provider/openai-compatible-chat.test.ts`, `provider/openai-responses.test.ts`,
//! `provider/anthropic-messages.test.ts`, `provider/gemini.test.ts`,
//! `provider/empty-tool-choice.test.ts`, `tool-schema-projection.test.ts` and
//! `tool-history.test.ts`. Each case is named after the OpenCode test it
//! ports; an "(adapted)" suffix marks a case rephrased for CodeSmith's
//! message model (no system-update or tool messages, string tool results).
//! What was not ported, and why, is in `PARITY.md`.
//!
//! Requests are built with each protocol's real `body` function and streams
//! are decoded with each protocol's real `Decoder`.

use crate::{
    anthropic, decode_all, gemini, normalize_tool_history, openai, responses, schema, Error, Event,
    Message, Part, Request, Role, ToolSpec, Usage,
};
use serde_json::{json, Value};

/// Run named cases, reporting every failing name rather than the first.
fn run(cases: &[(&str, fn())]) {
    let failed: Vec<&str> = cases
        .iter()
        .filter(|(_, f)| std::panic::catch_unwind(f).is_err())
        .map(|(n, _)| *n)
        .collect();
    assert!(failed.is_empty(), "failing parity cases: {failed:#?}");
}

// ---- builders ---------------------------------------------------------------

fn user(t: &str) -> Message {
    Message::user(t)
}

fn asst(parts: Vec<Part>) -> Message {
    Message {
        role: Role::Assistant,
        parts,
    }
}

fn results(parts: Vec<Part>) -> Message {
    Message {
        role: Role::User,
        parts,
    }
}

fn text(t: &str) -> Part {
    Part::Text(t.into())
}

fn thinking(t: &str) -> Part {
    Part::Reasoning(t.into())
}

fn call(id: &str, name: &str, input: Value) -> Part {
    Part::ToolCall {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn result(id: &str, name: &str, content: &str) -> Part {
    Part::ToolResult {
        id: id.into(),
        name: name.into(),
        content: content.into(),
        is_error: false,
    }
}

fn error_result(id: &str, name: &str, content: &str) -> Part {
    Part::ToolResult {
        id: id.into(),
        name: name.into(),
        content: content.into(),
        is_error: true,
    }
}

fn req(model: &str, messages: Vec<Message>) -> Request {
    Request {
        model: model.into(),
        messages,
        ..Default::default()
    }
}

/// OpenCode's shared fixture request: concise system prompt, one prompt.
fn hello(model: &str) -> Request {
    Request {
        model: model.into(),
        system: "You are concise.".into(),
        messages: vec![user("Say hello.")],
        max_tokens: Some(20),
        temperature: Some(0.0),
        ..Default::default()
    }
}

fn tool(name: &str, parameters: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "Lookup data".into(),
        parameters,
    }
}

fn chat_at(url: &str, r: &Request) -> Value {
    openai::body_for(r, &openai::Compat::detect(url, &r.model))
}

const OPENAI: &str = "https://api.openai.com/v1";
const AZURE: &str = "https://opencode-test.openai.azure.com/openai/";
const COMPATIBLE: &str = "https://api.custom.test/v1";

// ---- streams ----------------------------------------------------------------

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn sse_done(events: &[Value]) -> String {
    sse(events) + "data: [DONE]\n\n"
}

fn chat(body: &str) -> Result<Vec<Event>, Error> {
    decode_all(openai::Decode::default(), body)
}

fn anth(body: &str) -> Result<Vec<Event>, Error> {
    decode_all(anthropic::Decode::default(), body)
}

fn resp(body: &str) -> Result<Vec<Event>, Error> {
    decode_all(responses::Decode::default(), body)
}

fn gem(body: &str) -> Result<Vec<Event>, Error> {
    decode_all(gemini::Decode::default(), body)
}

/// A decoded response in comparable form.
#[derive(Debug, Default)]
struct Out {
    text: String,
    reasoning: String,
    calls: Vec<(String, String, Value)>,
    finish: String,
    usage: Usage,
}

impl Out {
    fn inputs(&self) -> Vec<(&str, &Value)> {
        self.calls.iter().map(|(_, n, i)| (n.as_str(), i)).collect()
    }
    fn ids(&self) -> Vec<&str> {
        self.calls.iter().map(|(id, _, _)| id.as_str()).collect()
    }
}

fn out(events: Result<Vec<Event>, Error>) -> Out {
    let mut o = Out::default();
    let events = events.expect("stream decodes");
    let finishes = events
        .iter()
        .filter(|e| matches!(e, Event::Finish { .. }))
        .count();
    assert_eq!(finishes, 1, "exactly one Finish: {events:?}");
    assert!(matches!(events.last(), Some(Event::Finish { .. })));
    for e in events {
        match e {
            Event::TextDelta(t) => o.text.push_str(&t),
            Event::ReasoningDelta(t) => o.reasoning.push_str(&t),
            Event::ToolCall { id, name, input } => o.calls.push((id, name, input)),
            Event::Finish { reason, usage } => {
                o.finish = reason;
                o.usage = usage;
            }
        }
    }
    o
}

fn provider_err(r: Result<Vec<Event>, Error>) -> (String, bool) {
    match r {
        Err(Error::Provider { message, retryable }) => (message, retryable),
        other => panic!("expected a provider error, got {other:?}"),
    }
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

// ---- OpenAI Chat: request building -----------------------------------------

#[test]
fn openai_chat_requests() {
    run(&[
        ("openai-chat :: prepares OpenAI Chat payload", || {
            let b = chat_at(OPENAI, &hello("gpt-4o-mini"));
            assert_eq!(
                b["messages"],
                json!([
                    {"role": "system", "content": "You are concise."},
                    {"role": "user", "content": "Say hello."}
                ])
            );
            assert_eq!(b["stream"], true);
            assert_eq!(b["stream_options"], json!({"include_usage": true}));
            assert_eq!(b["store"], false);
            assert_eq!(b["max_completion_tokens"], 20);
            assert!(b.get("max_tokens").is_none());
            assert_eq!(b["temperature"].as_f64(), Some(0.0));
        }),
        ("openai-chat :: sends store only to OpenAI and Azure unless compatibility opts in", || {
            let r = req("gpt-4o-mini", vec![user("Hello")]);
            assert!(chat_at(COMPATIBLE, &r).get("store").is_none());
            assert_eq!(chat_at(OPENAI, &r)["store"], false);
            assert_eq!(chat_at(AZURE, &r)["store"], false);
        }),
        ("openai-chat :: omits empty and whitespace-only assistant messages", || {
            let r = req(
                "gpt-4o-mini",
                vec![
                    user("Before."),
                    asst(vec![]),
                    asst(vec![text("")]),
                    asst(vec![text(" \n\t ")]),
                    asst(vec![text("After.")]),
                ],
            );
            assert_eq!(
                chat_at(OPENAI, &r)["messages"],
                json!([
                    {"role": "user", "content": "Before."},
                    {"role": "assistant", "content": "After."}
                ])
            );
        }),
        ("openai-chat :: replays canonical reasoning as OpenAI-compatible reasoning_content", || {
            let r = req("gpt-4o-mini", vec![asst(vec![thinking("thinking"), text("Hello")])]);
            assert_eq!(
                chat_at(OPENAI, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello", "reasoning_content": "thinking"}])
            );
        }),
        ("openai-chat :: concatenates assistant text parts without adding separators", || {
            let r = req("gpt-4o-mini", vec![asst(vec![text("Hello"), text(" world")])]);
            assert_eq!(
                chat_at(OPENAI, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello world"}])
            );
        }),
        ("openai-chat :: prepares assistant tool-call and tool-result messages", || {
            let r = req(
                "gpt-4o-mini",
                vec![
                    user("What is the weather?"),
                    asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                    results(vec![result("call_1", "lookup", "{\"forecast\":\"sunny\"}")]),
                ],
            );
            assert_eq!(
                chat_at(OPENAI, &r)["messages"],
                json!([
                    {"role": "user", "content": "What is the weather?"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"query\":\"weather\"}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "{\"forecast\":\"sunny\"}"}
                ])
            );
        }),
        ("openai-chat :: replays Gemini thought signatures as tool call extra content", || {
            let r = req(
                "gemini-3-pro",
                vec![
                    user("Weather in Paris and Tokyo?"),
                    asst(vec![
                        call("call_1~sig_1", "lookup", json!({"city": "Paris"})),
                        call("call_2", "lookup", json!({"city": "Tokyo"})),
                    ]),
                    results(vec![
                        result("call_1~sig_1", "lookup", "Sunny"),
                        result("call_2", "lookup", "Rainy"),
                    ]),
                ],
            );
            let b = chat_at(COMPATIBLE, &r);
            assert_eq!(
                b["messages"][1]["tool_calls"],
                json!([
                    {"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"city\":\"Paris\"}"},
                     "extra_content": {"google": {"thought_signature": "sig_1"}}},
                    {"id": "call_2", "type": "function", "function": {"name": "lookup", "arguments": "{\"city\":\"Tokyo\"}"}}
                ])
            );
            assert_eq!(b["messages"][2]["tool_call_id"], "call_1");
        }),
        ("openai-chat :: limits OpenAI and Azure Chat tool call IDs to 40 characters", || {
            let id = format!("call_{}", "a".repeat(48));
            let r = req(
                "gpt-4o",
                vec![
                    asst(vec![call(&id, "lookup", json!({}))]),
                    results(vec![result(&id, "lookup", "Sunny")]),
                ],
            );
            for url in [OPENAI, AZURE] {
                let m = &chat_at(url, &r)["messages"];
                assert_eq!(m[0]["tool_calls"][0]["id"], id[..40]);
                assert_eq!(m[1]["tool_call_id"], id[..40]);
            }
        }),
        ("openai-chat :: preserves structured tool errors for the model", || {
            let e = "{\"error\":{\"type\":\"unknown\",\"message\":\"Tool execution interrupted\"}}";
            let r = req(
                "gpt-4o-mini",
                vec![
                    asst(vec![call("call_1", "bash", json!({}))]),
                    results(vec![error_result("call_1", "bash", e)]),
                ],
            );
            assert_eq!(
                chat_at(OPENAI, &r)["messages"][1],
                json!({"role": "tool", "tool_call_id": "call_1", "content": e})
            );
        }),
        ("openai-chat :: lowers reasoning-only assistant history", || {
            let r = req("gpt-4o-mini", vec![asst(vec![thinking("hidden")])]);
            assert_eq!(
                chat_at(OPENAI, &r)["messages"],
                json!([{"role": "assistant", "content": "", "reasoning_content": "hidden"}])
            );
        }),
        ("openai-chat :: applies serializable HTTP overlays after payload lowering (body)", || {
            let mut r = hello("gpt-4o-mini");
            r.body.insert("metadata".into(), json!({"source": "test"}));
            let b = chat_at(OPENAI, &r);
            assert_eq!(b["metadata"], json!({"source": "test"}));
            assert_eq!(b["stream"], true);
            assert_eq!(b["stream_options"], json!({"include_usage": true}));
        }),
        ("openai-compatible-chat :: matches AI SDK compatible basic request body fixture", || {
            let b = chat_at("https://api.deepseek.test/v1/", &hello("deepseek-chat"));
            assert_eq!(
                b["messages"],
                json!([
                    {"role": "system", "content": "You are concise."},
                    {"role": "user", "content": "Say hello."}
                ])
            );
            assert_eq!(b["max_tokens"], 20);
            assert!(b.get("max_completion_tokens").is_none());
            assert!(b.get("store").is_none());
            assert_eq!(b["stream_options"], json!({"include_usage": true}));
        }),
        ("openai-compatible-chat :: matches AI SDK compatible tool request body fixture", || {
            let schema = json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]});
            let mut r = req(
                "deepseek-chat",
                vec![
                    user("What is the weather?"),
                    asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                    results(vec![result("call_1", "lookup", "{\"forecast\":\"sunny\"}")]),
                ],
            );
            r.tools = vec![tool("lookup", schema.clone())];
            let b = chat_at("https://api.deepseek.test/v1/", &r);
            let m = &b["messages"];
            assert_eq!(m[0], json!({"role": "user", "content": "What is the weather?"}));
            assert_eq!(m[1]["content"], Value::Null);
            assert_eq!(
                m[1]["tool_calls"],
                json!([{"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"query\":\"weather\"}"}}])
            );
            assert_eq!(
                m[2],
                json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"forecast\":\"sunny\"}"})
            );
            assert_eq!(
                b["tools"][0],
                json!({"type": "function", "function": {"name": "lookup", "description": "Lookup data", "parameters": schema}})
            );
        }),
        ("openai-compatible-chat :: normalizes tool call IDs for the selected model family", || {
            let long = format!("call_{}", "a".repeat(48));
            for (model, id, want) in [
                ("mistral-small", "toolu_01CBhTTz95qkd9LJMdC9sf8t", "toolu01CB"),
                ("devstral-small", "abc", "abc000000"),
                ("codestral-latest", "toolu_01CBhTTz95", "toolu01CB"),
                ("pixtral-large", "toolu_01CBhTTz95", "toolu01CB"),
                ("open-mixtral-8x22b", "toolu_01CBhTTz95", "toolu01CB"),
                ("anthropic/claude-sonnet-4", "call|item/+", "call_item__"),
                ("openai/gpt-4o", long.as_str(), &long[..40]),
                ("ordinary-model", "call|item/+", "call|item/+"),
                ("zai-glm-5-2", "call_long_identifier", "call_long_identifier"),
            ] {
                let r = req(
                    model,
                    vec![
                        asst(vec![call(id, "lookup", json!({}))]),
                        results(vec![result(id, "lookup", "")]),
                    ],
                );
                let m = &chat_at(COMPATIBLE, &r)["messages"];
                assert_eq!(m[0]["tool_calls"][0]["id"], want, "{model}");
                assert_eq!(m[1]["tool_call_id"], want, "{model}");
            }
        }),
        ("openai-compatible-chat :: bridges tool results for Mistral-family models", || {
            for (model, bridge) in [
                ("mistral-small", true),
                ("devstral-small", true),
                ("codestral-latest", true),
                ("pixtral-large", true),
                ("open-mixtral-8x22b", true),
                ("ordinary-model", false),
            ] {
                // The user text in its own message, and sharing the results' message.
                let apart = vec![
                    asst(vec![call("call_1", "lookup", json!({}))]),
                    results(vec![result("call_1", "lookup", "Sunny")]),
                    user("What next?"),
                ];
                let shared = vec![
                    asst(vec![call("call_1", "lookup", json!({}))]),
                    results(vec![result("call_1", "lookup", "Sunny"), text("What next?")]),
                ];
                for msgs in [apart, shared] {
                    let b = chat_at(COMPATIBLE, &req(model, msgs));
                    let roles: Vec<&str> = b["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|m| m["role"].as_str().unwrap())
                        .collect();
                    if bridge {
                        assert_eq!(roles, ["assistant", "tool", "assistant", "user"], "{model}");
                        assert_eq!(b["messages"][2], json!({"role": "assistant", "content": "Done."}));
                    } else {
                        assert_eq!(roles, ["assistant", "tool", "user"], "{model}");
                    }
                }
            }
        }),
        ("openai-compatible-chat :: requires reasoning for DeepSeek models, providers, and endpoints", || {
            for (model, url, required) in [
                ("DeepSeek-V3", COMPATIBLE, true),
                ("custom-model", "https://API.DeepSeek.COM/v1", true),
                ("ordinary-model", COMPATIBLE, false),
            ] {
                let r = req(
                    model,
                    vec![
                        asst(vec![text("Hello")]),
                        asst(vec![call("call_1", "lookup", json!({}))]),
                        results(vec![result("call_1", "lookup", "Sunny")]),
                    ],
                );
                let b = chat_at(url, &r);
                for m in b["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant") {
                    if required {
                        assert_eq!(m["reasoning_content"], "", "{model}");
                    } else {
                        assert!(m.get("reasoning_content").is_none(), "{model}");
                    }
                }
            }
        }),
    ]);
}

// ---- OpenAI Chat: stream decoding ------------------------------------------

fn delta(d: Value) -> Value {
    json!({"id": "chatcmpl_fixture", "choices": [{"delta": d, "finish_reason": null}], "usage": null})
}

fn delta_finish(d: Value, reason: &str) -> Value {
    json!({"id": "chatcmpl_fixture", "choices": [{"delta": d, "finish_reason": reason}], "usage": null})
}

#[test]
fn openai_chat_streams() {
    run(&[
        ("openai-chat :: parses text and usage stream fixtures", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"role": "assistant", "content": "Hello"})),
                delta(json!({"content": "!"})),
                delta_finish(json!({}), "stop"),
                json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7,
                    "prompt_tokens_details": {"cached_tokens": 1, "cache_write_tokens": 2},
                    "completion_tokens_details": {"reasoning_tokens": 0}}}),
            ])));
            assert_eq!(o.text, "Hello!");
            assert_eq!(o.finish, "stop");
            assert_eq!(o.usage, usage(2, 2, 1, 2));
        }),
        ("openai-chat :: preserves streamed refusals as ordinary assistant text", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"role": "assistant", "refusal": "I can't"})),
                delta(json!({"refusal": " help with that."})),
                delta_finish(json!({}), "stop"),
            ])));
            assert_eq!(o.text, "I can't help with that.");
            assert_eq!(o.finish, "stop");
        }),
        ("openai-chat :: joins content and refusal deltas into ordinary assistant text", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"refusal": "No."})),
                delta(json!({"content": " Alternative."})),
                delta(json!({"refusal": " Still no."})),
                delta_finish(json!({}), "stop"),
            ])));
            assert_eq!(o.text, "No. Alternative. Still no.");
        }),
        ("openai-chat :: parses and replays OpenAI-compatible reasoning fields (parse)", || {
            for field in ["reasoning_content", "reasoning", "reasoning_text"] {
                let o = out(chat(&sse_done(&[
                    delta(json!({ field: "thinking" })),
                    delta(json!({"content": "Hello"})),
                    delta_finish(json!({}), "stop"),
                ])));
                assert_eq!(o.reasoning, "thinking", "{field}");
                assert_eq!(o.text, "Hello");
            }
        }),
        ("openai-chat :: reads top-level reasoning tokens whether or not completion tokens include them", || {
            let output = |u: Value| {
                out(chat(&sse_done(&[delta_finish(json!({"content": "Hi"}), "stop"), json!({"choices": [], "usage": u})])))
                    .usage
                    .output
            };
            assert_eq!(output(json!({"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 62, "reasoning_tokens": 53})), 55);
            assert_eq!(output(json!({"prompt_tokens": 96, "completion_tokens": 50, "total_tokens": 146, "reasoning_tokens": 35})), 50);
        }),
        ("openai-chat :: assembles streamed tool call input", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "{\"query\""}}]})),
                delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": ":\"weather\"}"}}]})),
                delta_finish(json!({}), "tool_calls"),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
            assert_eq!(o.finish, "tool_calls");
        }),
        ("openai-chat :: preserves Gemini thought signatures on streamed parallel tool calls", || {
            // Gemini's OpenAI endpoint omits `index` and signs only the first call.
            let o = out(chat(&sse_done(&[
                delta(json!({"role": "assistant", "tool_calls": [{"extra_content": {"google": {"thought_signature": "sig_1"}},
                    "id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"city\":\"Paris\"}"}}]})),
                delta(json!({"role": "assistant", "tool_calls": [{"id": "call_2", "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"city\":\"Tokyo\"}"}}]})),
                delta_finish(json!({}), "stop"),
            ])));
            assert_eq!(o.ids(), ["call_1~sig_1", "call_2"]);
            assert_eq!(o.inputs(), [("lookup", &json!({"city": "Paris"})), ("lookup", &json!({"city": "Tokyo"}))]);
        }),
        ("openai-chat :: keeps extra content that arrives before the tool identity", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"tool_calls": [{"index": 0, "extra_content": {"google": {"thought_signature": "sig_1"}}, "function": {"arguments": "{"}}]})),
                delta(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "}"}}]})),
                delta_finish(json!({}), "tool_calls"),
            ])));
            assert_eq!(o.ids(), ["call_1~sig_1"]);
        }),
        ("openai-chat :: does not finalize streamed tool calls when content is filtered", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "{\"query\":\"weather\""}}]})),
                delta_finish(json!({}), "content_filter"),
            ])));
            assert!(o.calls.is_empty());
            assert_eq!(o.finish, "content_filter");
            let o = out(chat(&sse_done(&[
                delta(json!({"tool_calls": [{"index": 0, "id": "call_2", "function": {"arguments": "{}"}}]})),
                delta_finish(json!({}), "content_filter"),
            ])));
            assert!(o.calls.is_empty());
        }),
        ("openai-chat :: does not finalize streamed tool calls when output is truncated", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "{\"query\":\"weather\"}"}}]})),
                delta_finish(json!({}), "length"),
            ])));
            assert!(o.calls.is_empty());
            assert_eq!(o.finish, "length");
        }),
        ("openai-chat :: maps unknown finish reasons to unknown and keeps the raw value (raw)", || {
            let o = out(chat(&sse_done(&[delta(json!({"content": "Hello"})), delta_finish(json!({}), "future_reason")])));
            assert_eq!((o.text.as_str(), o.finish.as_str()), ("Hello", "future_reason"));
        }),
        ("openai-chat :: ignores empty identity fields on later tool call deltas", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "{"}}]})),
                delta(json!({"tool_calls": [{"index": 0, "id": "", "function": {"name": "", "arguments": "\"query\":\"weather\"}"}}]})),
                delta_finish(json!({}), "tool_calls"),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
        }),
        ("openai-chat :: buffers tool call deltas until the function name arrives", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"arguments": "{"}}]})),
                delta(json!({"tool_calls": [{"index": 0, "function": {"name": "lookup", "arguments": "\"query\":"}}]})),
                delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"weather\"}"}}]})),
                delta_finish(json!({}), "tool_calls"),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
        }),
        ("openai-chat :: finalizes a streamed tool call when the provider ends without a finish reason", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "{\"query\""}}]})),
                delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": ":\"weather\"}"}}]})),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
        }),
        ("openai-compatible-chat :: ignores events after the done sentinel", || {
            let body = sse_done(&[delta(json!({"content": "Hello"})), delta_finish(json!({}), "stop")])
                + &sse(&[delta(json!({"content": " late"}))]);
            let o = out(chat(&body));
            assert_eq!((o.text.as_str(), o.finish.as_str()), ("Hello", "stop"));
        }),
        ("openai-compatible-chat :: ignores bare null frames between deltas", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"role": "assistant", "content": "Hello"})),
                Value::Null,
                delta(json!({"content": "!"})),
                Value::Null,
                delta_finish(json!({}), "stop"),
            ])));
            assert_eq!(o.text, "Hello!");
        }),
        ("openai-compatible-chat :: accepts nullable usage and preserves provider fields", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"content": "Hello"})),
                delta_finish(json!({}), "stop"),
                json!({"choices": [], "usage": {"prompt_tokens": null, "completion_tokens": null, "total_tokens": null,
                    "prompt_tokens_details": {"cached_tokens": null, "vendor_cache_tokens": 3},
                    "completion_tokens_details": {"reasoning_tokens": null}, "cost": "0.001"}}),
            ])));
            assert_eq!(o.usage, Usage::default());
        }),
        ("openai-compatible-chat :: assembles indexless parallel tool calls across sparse chunks", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"tool_calls": [
                    {"id": "call_paris", "function": {"name": "weather", "arguments": "{\"city\":\""}},
                    {"index": null, "id": "call_london", "function": {"name": "weather", "arguments": "{\"city\":\""}}
                ]})),
                delta(json!({"tool_calls": [{"function": {"arguments": "London\"}"}}]})),
                delta(json!({"tool_calls": [{"id": "call_paris", "function": {"arguments": "Paris\"}"}}]})),
                delta_finish(json!({}), "tool_calls"),
            ])));
            assert_eq!(
                o.calls,
                vec![
                    ("call_paris".into(), "weather".into(), json!({"city": "Paris"})),
                    ("call_london".into(), "weather".into(), json!({"city": "London"})),
                ]
            );
        }),
        ("openai-compatible-chat :: infers stop when finish reasons are optional", || {
            let o = out(chat(&sse_done(&[delta(json!({"content": "Hello"})), delta_finish(json!({}), "")])));
            assert_eq!(o.finish, "stop");
        }),
        ("openai-compatible-chat :: classifies provider error finish reasons", || {
            let (m, retry) = provider_err(chat(&sse_done(&[delta_finish(json!({}), "network_error")])));
            assert!(m.contains("network error") && retry, "{m}");
            let (m, retry) = provider_err(chat(&sse_done(&[delta_finish(json!({}), "error")])));
            assert!(m.contains("finish_reason: error") && !retry, "{m}");
            assert_eq!(out(chat(&sse_done(&[delta_finish(json!({}), "future_reason")]))).finish, "future_reason");
        }),
        ("openai-compatible-chat :: preserves explicit provider error events", || {
            let (m, retry) = provider_err(chat(&sse(&[json!({"id": "chatcmpl_error",
                "error": {"code": 502, "message": "Provider disconnected", "details": {"upstream": "vendor"}}, "trace_id": "trace_1"})])));
            assert_eq!(m, "Provider disconnected");
            assert!(retry);
        }),
        ("openai-compatible-chat :: preserves late text and reasoning for replay without changing the finished tool call", || {
            let o = out(chat(&sse_done(&[
                delta(json!({"reasoning_content": "First"})),
                delta(json!({"content": "Hello"})),
                delta(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "{}"}}]})),
                delta_finish(json!({}), "tool_calls"),
                delta(json!({"content": " world"})),
                delta(json!({"refusal": " (refusal)"})),
                delta(json!({"reasoning_content": " then"})),
                delta(json!({"reasoning": " more"})),
                delta_finish(json!({"reasoning_text": " thoughts"}), "stop"),
                json!({"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}}),
            ])));
            assert_eq!(o.text, "Hello world (refusal)");
            assert_eq!(o.reasoning, "First then more thoughts");
            assert_eq!(o.finish, "tool_calls");
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({}))]);
            assert_eq!(o.usage, usage(11, 7, 0, 0));
        }),
    ]);
}

// ---- OpenAI Responses -------------------------------------------------------

#[test]
fn openai_responses_requests() {
    run(&[
        ("openai-responses :: preserves function schemas", || {
            let schema = json!({"type": "object", "anyOf": [
                {"type": "object", "properties": {"path": {"type": "string"},
                    "reference": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                    "limit": {"type": "integer", "maximum": 2000}}, "required": ["path"]},
                {"type": "object", "properties": {"resource": {"type": "string"},
                    "limit": {"type": "integer", "maximum": 51200}}, "required": ["resource"]}
            ]});
            let mut r = hello("gpt-4.1-mini");
            r.tools = vec![ToolSpec { name: "read".into(), description: "Read a path or resource.".into(), parameters: schema.clone() }];
            assert_eq!(
                responses::body(&r)["tools"],
                json!([{"type": "function", "name": "read", "description": "Read a path or resource.", "strict": false, "parameters": schema}])
            );
        }),
        ("openai-responses :: prepares function call and function output input items", || {
            let r = req(
                "gpt-4.1-mini",
                vec![
                    user("What is the weather?"),
                    asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                    results(vec![result("call_1", "lookup", "{\"forecast\":\"sunny\"}")]),
                ],
            );
            let b = responses::body(&r);
            assert_eq!(
                b["input"],
                json!([
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What is the weather?"}]},
                    {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"query\":\"weather\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "{\"forecast\":\"sunny\"}"}
                ])
            );
            assert_eq!(b["store"], false);
            assert_eq!(b["stream"], true);
            assert!(b.get("tools").is_none() && b.get("tool_choice").is_none());
        }),
        ("openai-responses :: preserves structured tool errors / keeps primitive and non-JSON tool errors as plain text", || {
            for content in ["{\"error\":{\"type\":\"unknown\",\"message\":\"Tool execution interrupted\"},\"content\":[],\"structured\":{}}", "503", "Error: boom"] {
                let r = req(
                    "gpt-4.1-mini",
                    vec![
                        asst(vec![call("call_1", "bash", json!({}))]),
                        results(vec![error_result("call_1", "bash", content)]),
                    ],
                );
                assert_eq!(responses::body(&r)["input"][1]["output"], content);
            }
        }),
    ]);
}

fn added_call(id: &str, call_id: &str, name: &str) -> Value {
    json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": ""}})
}

fn done_call(id: &str, call_id: &str, name: &str, args: &str) -> Value {
    json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": args}})
}

fn args_delta(item: &str, d: &str) -> Value {
    json!({"type": "response.function_call_arguments.delta", "item_id": item, "delta": d})
}

fn args_done(item: &str, a: &str) -> Value {
    json!({"type": "response.function_call_arguments.done", "item_id": item, "arguments": a})
}

fn completed() -> Value {
    json!({"type": "response.completed", "response": {"id": "resp_1"}})
}

#[test]
fn openai_responses_streams() {
    run(&[
        ("openai-responses :: ignores untyped SSE frames", || {
            let o = out(resp(&sse(&[
                json!({}),
                json!({"type": "response.created", "response": {"id": "resp_1"}}),
                json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "PO"}),
                json!({"gateway": "keep-alive"}),
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "NG"}),
                json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1"}}),
                completed(),
            ])));
            assert_eq!((o.text.as_str(), o.finish.as_str()), ("PONG", "stop"));
        }),
        ("openai-responses :: parses text and usage stream fixtures", || {
            let o = out(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Hello"}),
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "!"}),
                json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7,
                    "input_tokens_details": {"cached_tokens": 1, "cache_write_tokens": 2}, "output_tokens_details": {"reasoning_tokens": 0}}}}),
            ])));
            assert_eq!(o.text, "Hello!");
            assert_eq!(o.usage, usage(2, 2, 1, 2));
        }),
        ("openai-responses :: parses reasoning summary stream fixtures", || {
            let o = out(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1"}}),
                json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "thinking"}),
                json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Hello"}),
                json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_1"}),
                completed(),
            ])));
            assert_eq!((o.reasoning.as_str(), o.text.as_str(), o.finish.as_str()), ("thinking", "Hello", "stop"));
        }),
        ("openai-responses :: preserves standard refusal content as ordinary assistant text", || {
            let o = out(resp(&sse(&[
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_refusal", "content": []}}),
                json!({"type": "response.content_part.added", "item_id": "msg_refusal", "output_index": 0, "content_index": 0, "part": {"type": "refusal", "refusal": ""}}),
                json!({"type": "response.refusal.delta", "item_id": "msg_refusal", "output_index": 0, "content_index": 0, "delta": "I can't"}),
                json!({"type": "response.refusal.delta", "item_id": "msg_refusal", "output_index": 0, "content_index": 0, "delta": " help with that."}),
                json!({"type": "response.refusal.done", "item_id": "msg_refusal", "output_index": 0, "content_index": 0, "refusal": "I can't help with that."}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "msg_refusal", "content": [{"type": "refusal", "refusal": "I can't help with that."}]}}),
                completed(),
            ])));
            assert_eq!((o.text.as_str(), o.finish.as_str()), ("I can't help with that.", "stop"));
        }),
        ("openai-responses :: routes assistant text by output index when its item id disagrees", || {
            let o = out(resp(&sse(&[
                json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "message", "id": "msg_1"}}),
                json!({"type": "response.output_text.delta", "output_index": 2, "item_id": "wrong_message", "delta": "Indexed"}),
                json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "message", "id": "msg_1"}}),
                completed(),
            ])));
            assert_eq!(o.text, "Indexed");
        }),
        ("openai-responses :: falls back to item ids when an output index was not registered", || {
            let o = out(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
                json!({"type": "response.output_text.delta", "output_index": 9, "item_id": "msg_1", "delta": "Fallback"}),
                completed(),
            ])));
            assert_eq!(o.text, "Fallback");
        }),
        ("openai-responses :: routes interleaved function calls by output index", || {
            let first = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "first", "arguments": ""});
            let second = json!({"type": "function_call", "id": "fc_2", "call_id": "call_2", "name": "second", "arguments": ""});
            let with_args = |item: &Value, a: &str| {
                let mut i = item.clone();
                i["arguments"] = json!(a);
                i
            };
            let o = out(resp(&sse(&[
                json!({"type": "response.output_item.added", "output_index": 1, "item": first}),
                json!({"type": "response.output_item.added", "output_index": 3, "item": second}),
                json!({"type": "response.function_call_arguments.delta", "output_index": 1, "item_id": "fc_2", "delta": "{\"a\":"}),
                json!({"type": "response.function_call_arguments.delta", "output_index": 3, "item_id": "fc_1", "delta": "{\"b\":"}),
                json!({"type": "response.function_call_arguments.done", "output_index": 3, "item_id": "fc_1", "arguments": "{\"b\":2}"}),
                json!({"type": "response.function_call_arguments.done", "output_index": 1, "item_id": "fc_2", "arguments": "{\"a\":1}"}),
                json!({"type": "response.output_item.done", "output_index": 1, "item": with_args(&first, "{\"a\":1}")}),
                json!({"type": "response.output_item.done", "output_index": 3, "item": with_args(&second, "{\"b\":2}")}),
                completed(),
            ])));
            assert_eq!(
                o.calls,
                vec![
                    ("call_1".into(), "first".into(), json!({"a": 1})),
                    ("call_2".into(), "second".into(), json!({"b": 2})),
                ]
            );
        }),
        ("openai-responses :: maps incomplete response reasons (raw)", || {
            let finish = |d: Value| {
                out(resp(&sse(&[json!({"type": "response.incomplete", "response": {"id": "resp_incomplete", "incomplete_details": d}})]))).finish
            };
            assert_eq!(finish(json!({"reason": "max_output_tokens"})), "length");
            assert_eq!(finish(json!({"reason": "content_filter"})), "content_filter");
            assert_eq!(finish(json!({})), "incomplete");
            assert_eq!(finish(json!({"reason": "provider_limit"})), "provider_limit");
        }),
        ("openai-responses :: assembles streamed function call input", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\""),
                args_delta("fc_item_1", ":\"weather\"}"),
                done_call("fc_item_1", "call_1", "lookup", "{\"query\":\"weather\"}"),
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 1}}}),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
            assert_eq!(o.finish, "tool_calls");
            assert_eq!(o.usage, usage(5, 1, 0, 0));
        }),
        ("openai-responses :: emits only missing function arguments from the arguments done event", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\""),
                args_done("fc_item_1", "{\"query\":\"weather\"}"),
                done_call("fc_item_1", "call_1", "lookup", "{\"query\":\"weather\"}"),
                completed(),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
        }),
        ("openai-responses :: streams complete function arguments supplied only by the arguments done event", || {
            let o = out(resp(&sse(&[added_call("fc_item_1", "call_1", "lookup"), args_done("fc_item_1", "{\"query\":\"weather\"}"), completed()])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
            assert_eq!(o.finish, "tool_calls");
        }),
        ("openai-responses :: does not repeat function arguments already supplied by deltas", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\":\"weather\"}"),
                args_done("fc_item_1", "{\"query\":\"weather\"}"),
                completed(),
            ])));
            assert_eq!(o.inputs(), [("lookup", &json!({"query": "weather"}))]);
        }),
        ("openai-responses :: uses authoritative arguments done input without emitting a mismatched delta", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\":\"streamed\"}"),
                args_done("fc_item_1", "{\"query\":\"final\"}"),
                completed(),
            ])));
            assert_eq!(o.inputs(), [("lookup", &json!({"query": "final"}))]);
        }),
        ("openai-responses :: lets completed output item arguments override the arguments done event", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_done("fc_item_1", "{\"query\":\"arguments-done\"}"),
                done_call("fc_item_1", "call_1", "lookup", "{\"query\":\"output-item-done\"}"),
                completed(),
            ])));
            assert_eq!(o.inputs(), [("lookup", &json!({"query": "output-item-done"}))]);
        }),
        ("openai-responses :: treats empty completed output item arguments as authoritative", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\":\"streamed\"}"),
                done_call("fc_item_1", "call_1", "lookup", ""),
                completed(),
            ])));
            assert_eq!(o.inputs(), [("lookup", &json!({}))]);
        }),
        ("openai-responses :: uses completed response output when output item completion is missing", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\":\"wea"),
                json!({"type": "response.completed", "response": {"id": "resp_1", "output": [
                    {"type": "function_call", "id": "fc_item_1", "call_id": "call_1", "name": "lookup", "arguments": "{\"query\":\"weather\"}"}]}}),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
            assert_eq!(o.finish, "tool_calls");
        }),
        ("openai-responses :: lets completed response output override arguments done", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_done("fc_item_1", "{\"query\":\"arguments-done\"}"),
                json!({"type": "response.completed", "response": {"output": [
                    {"type": "function_call", "id": "fc_item_1", "call_id": "call_1", "name": "lookup", "arguments": "{\"query\":\"completed\"}"}]}}),
            ])));
            assert_eq!(o.inputs(), [("lookup", &json!({"query": "completed"}))]);
        }),
        ("openai-responses :: preserves explicit empty arguments from completed response output", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\":\"streamed\"}"),
                json!({"type": "response.completed", "response": {"output": [
                    {"type": "function_call", "id": "fc_item_1", "call_id": "call_1", "name": "lookup", "arguments": ""}]}}),
            ])));
            assert_eq!(o.inputs(), [("lookup", &json!({}))]);
        }),
        ("openai-responses :: does not repeat function calls already finalized by an output item", || {
            let item = done_call("fc_item_1", "call_1", "lookup", "{\"query\":\"weather\"}");
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                item.clone(),
                json!({"type": "response.completed", "response": {"output": [item["item"]]}}),
            ])));
            assert_eq!(o.calls.len(), 1);
        }),
        ("openai-responses :: does not finalize pending function calls from incomplete response output", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\":\"partial"),
                json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}, "output": [
                    {"type": "function_call", "id": "fc_item_1", "call_id": "call_1", "name": "lookup", "arguments": "{\"query\":\"partial"}]}}),
            ])));
            assert!(o.calls.is_empty());
            assert_eq!(o.finish, "length");
        }),
        ("openai-responses :: finalizes a pending function call at response completion", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 1}}}),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({}))]);
            assert_eq!(o.finish, "tool_calls");
        }),
        ("openai-responses :: rejects authoritative incomplete final function arguments (kept for repair)", || {
            let o = out(resp(&sse(&[
                added_call("fc_item_1", "call_1", "lookup"),
                args_delta("fc_item_1", "{\"query\":\"streamed\"}"),
                done_call("fc_item_1", "call_1", "lookup", "{\"query\":\"partial"),
                completed(),
            ])));
            assert_eq!(o.inputs(), [("lookup", &json!({"__invalid_json": "{\"query\":\"partial"}))]);
        }),
        ("openai-responses :: rejects incomplete function arguments when output_item.added is absent (kept for repair)", || {
            let o = out(resp(&sse(&[done_call("fc_item_1", "call_1", "lookup", "{\"query\":\"partial"), completed()])));
            assert_eq!(o.inputs(), [("lookup", &json!({"__invalid_json": "{\"query\":\"partial"}))]);
            assert_eq!(o.finish, "tool_calls");
        }),
        ("openai-responses :: retains function call item metadata when output_item.added is absent", || {
            let o = out(resp(&sse(&[done_call("fc_item_1", "call_1", "lookup", "{\"query\":\"weather\"}"), completed()])));
            assert_eq!(o.ids(), ["call_1"]);
        }),
    ]);
}

#[test]
fn openai_responses_errors() {
    run(&[
        ("openai-responses :: fails with a typed rate limit for provider error frames", || {
            let (m, retry) = provider_err(resp(&sse(&[json!({"type": "error", "code": "rate_limit_exceeded", "message": "Slow down"})])));
            assert_eq!(m, "rate_limit_exceeded: Slow down");
            assert!(retry);
        }),
        ("openai-responses :: falls back to error code when no message is present", || {
            assert_eq!(provider_err(resp(&sse(&[json!({"type": "error", "code": "internal_error"})]))), ("internal_error".into(), true));
        }),
        ("openai-responses :: falls back to error code when message is empty", || {
            assert_eq!(
                provider_err(resp(&sse(&[json!({"type": "error", "code": "internal_error", "message": ""})]))),
                ("internal_error".into(), true)
            );
        }),
        ("openai-responses :: surfaces response.failed details from response.error", || {
            let (m, retry) = provider_err(resp(&sse(&[json!({"type": "response.failed", "response": {"id": "resp_failed_1",
                "error": {"code": "server_error", "message": "Upstream model unavailable"}}})])));
            assert_eq!(m, "server_error: Upstream model unavailable");
            assert!(retry);
        }),
        ("openai-responses :: retains the token-sharing response.failed event for retry hooks", || {
            let (m, retry) = provider_err(resp(&sse(&[json!({"type": "response.failed", "response": {"id": "resp_usage_limit",
                "error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "Rate limit exceeded"}}})])));
            assert!(m.contains("subscription_sharing_usage_limit_exceeded"));
            assert!(retry);
        }),
        ("openai-responses :: surfaces response.failed code when no nested message is present", || {
            assert_eq!(
                provider_err(resp(&sse(&[json!({"type": "response.failed", "response": {"id": "resp_failed_2", "error": {"code": "invalid_prompt"}}})]))),
                ("invalid_prompt".into(), false)
            );
        }),
        ("openai-responses :: surfaces error event details nested under response.error", || {
            let (m, retry) = provider_err(resp(&sse(&[json!({"type": "error",
                "response": {"error": {"code": "context_length_exceeded", "message": "prompt too long"}}})])));
            assert_eq!(m, "context_length_exceeded: prompt too long");
            assert!(!retry);
        }),
        ("openai-responses :: surfaces error event details nested under error", || {
            let (m, retry) = provider_err(resp(&sse(&[json!({"type": "error", "sequence_number": 2, "error": {
                "type": "invalid_request_error", "code": "context_length_exceeded", "message": "prompt too long", "param": "input"}})])));
            assert_eq!(m, "context_length_exceeded: prompt too long");
            assert!(!retry);
        }),
        ("openai-responses :: accepts nullable fields in spec-compliant error events", || {
            assert_eq!(
                provider_err(resp(&sse(&[json!({"type": "error", "code": null, "message": "Something went wrong", "param": null, "sequence_number": 1})]))),
                ("Something went wrong".into(), false)
            );
        }),
        ("openai-responses :: falls back to the raw payload when error is null", || {
            let (m, retry) = provider_err(resp(&sse(&[json!({"type": "error", "error": null})])));
            assert!(m.contains("\"error\":null"), "{m}");
            assert!(!retry);
        }),
        ("openai-responses :: classifies a detail-free error event as a transient provider failure", || {
            let (m, retry) = provider_err(resp(&sse(&[json!({"type": "error", "sequence_number": 2})])));
            assert!(m.contains("\"type\":\"error\""), "{m}");
            assert!(retry);
        }),
        ("openai-responses :: keeps the raw response payload when response.failed has no error payload", || {
            let (m, retry) = provider_err(resp(&sse(&[json!({"type": "response.failed", "response": {"id": "resp_failed_3"}})])));
            assert!(m.contains("\"resp_failed_3\""), "{m}");
            assert!(!retry);
        }),
    ]);
}

// ---- Anthropic Messages -----------------------------------------------------

#[test]
fn anthropic_messages_requests() {
    run(&[
        (
            "anthropic-messages :: prepares Anthropic Messages target (without cache hints)",
            || {
                assert_eq!(
                    anthropic::body(&hello("claude-sonnet-4-5")),
                    json!({"model": "claude-sonnet-4-5", "system": "You are concise.",
                    "messages": [{"role": "user", "content": [{"type": "text", "text": "Say hello."}]}],
                    "stream": true, "max_tokens": 20, "temperature": 0.0})
                );
            },
        ),
        (
            "anthropic-messages :: omits empty system text while preserving whitespace",
            || {
                let mut r = hello("claude-sonnet-4-5");
                r.system = String::new();
                assert!(anthropic::body(&r).get("system").is_none());
                r.system = " ".into();
                assert_eq!(anthropic::body(&r)["system"], " ");
            },
        ),
        (
            "anthropic-messages :: filters whitespace-only text and removes empty messages",
            || {
                let r = req(
                    "claude-sonnet-4-5",
                    vec![
                        user(" \n\t"),
                        results(vec![]),
                        results(vec![
                            text(""),
                            text("  Keep this spacing.  "),
                            text(" \n\t"),
                        ]),
                        asst(vec![text(" \n\t")]),
                        asst(vec![]),
                        asst(vec![thinking("")]),
                    ],
                );
                assert_eq!(
                    anthropic::body(&r)["messages"],
                    json!([{"role": "user", "content": [{"type": "text", "text": "  Keep this spacing.  "}]}])
                );
            },
        ),
        (
            "anthropic-messages :: prepares tool call and tool result messages",
            || {
                let r = req(
                    "claude-sonnet-4-5",
                    vec![
                        user("What is the weather?"),
                        asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                        results(vec![result("call_1", "lookup", "{\"forecast\":\"sunny\"}")]),
                    ],
                );
                assert_eq!(
                    anthropic::body(&r),
                    json!({"model": "claude-sonnet-4-5", "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "What is the weather?"}]},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"query": "weather"}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "{\"forecast\":\"sunny\"}"}]}
                ], "stream": true, "max_tokens": 32000})
                );
            },
        ),
        (
            "anthropic-messages :: scrubs outbound tool call IDs without truncating them",
            || {
                let id = format!("functions.lookup:1|{}", "x".repeat(64));
                let scrubbed = format!("functions_lookup_1_{}", "x".repeat(64));
                let r = req(
                    "claude-sonnet-4-5",
                    vec![
                        asst(vec![call(&id, "lookup", json!({}))]),
                        results(vec![result(&id, "lookup", "done")]),
                    ],
                );
                let m = &anthropic::body(&r)["messages"];
                assert_eq!(m[0]["content"][0]["id"], scrubbed.as_str());
                assert_eq!(m[1]["content"][0]["tool_use_id"], scrubbed.as_str());
            },
        ),
        (
            "anthropic-messages :: batches parallel tool results into one Anthropic user message",
            || {
                let r = req(
                    "claude-sonnet-4-5",
                    vec![
                        user("Check both cities."),
                        asst(vec![
                            text("I'll check both."),
                            call("call_paris", "weather", json!({"city": "Paris"})),
                            call("call_london", "weather", json!({"city": "London"})),
                        ]),
                        results(vec![result(
                            "call_paris",
                            "weather",
                            "{\"temperature\":22}",
                        )]),
                        results(vec![result(
                            "call_london",
                            "weather",
                            "{\"temperature\":18}",
                        )]),
                    ],
                );
                let m = anthropic::body(&r)["messages"].clone();
                assert_eq!(m.as_array().unwrap().len(), 3);
                assert_eq!(
                    m[2],
                    json!({"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "call_paris", "content": "{\"temperature\":22}"},
                        {"type": "tool_result", "tool_use_id": "call_london", "content": "{\"temperature\":18}"}
                    ]})
                );
            },
        ),
        (
            "anthropic-messages :: moves a system update between a local tool call and its result after the result (adapted: user text)",
            || {
                let r = req(
                    "claude-sonnet-4-5",
                    vec![
                        asst(vec![call("call_1", "lookup", json!({}))]),
                        user("Update."),
                        results(vec![result("call_1", "lookup", "done")]),
                    ],
                );
                assert_eq!(
                    anthropic::body(&r)["messages"],
                    json!([
                        {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "lookup", "input": {}}]},
                        {"role": "user", "content": [
                            {"type": "tool_result", "tool_use_id": "call_1", "content": "done"},
                            {"type": "text", "text": "Update."}
                        ]}
                    ])
                );
            },
        ),
        ("codesmith :: tool ids drop a Gemini thought signature before scrubbing", || {
            let r = req(
                "claude-sonnet-4-5",
                vec![
                    asst(vec![call("call_0~c2lnL+==", "lookup", json!({}))]),
                    results(vec![result("call_0~c2lnL+==", "lookup", "done")]),
                ],
            );
            let m = &anthropic::body(&r)["messages"];
            assert_eq!(m[0]["content"][0]["id"], "call_0");
            assert_eq!(m[1]["content"][0]["tool_use_id"], "call_0");
        }),
    ]);
}

fn start(input: u64) -> Value {
    json!({"type": "message_start", "message": {"usage": {"input_tokens": input}}})
}

fn block_start(i: u64, block: Value) -> Value {
    json!({"type": "content_block_start", "index": i, "content_block": block})
}

fn block_delta(i: u64, d: Value) -> Value {
    json!({"type": "content_block_delta", "index": i, "delta": d})
}

fn block_stop(i: u64) -> Value {
    json!({"type": "content_block_stop", "index": i})
}

fn message_delta(reason: &str, output: u64) -> Value {
    json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": output}})
}

fn stop() -> Value {
    json!({"type": "message_stop"})
}

#[test]
fn anthropic_messages_streams() {
    run(&[
        ("anthropic-messages :: parses text, reasoning, and usage stream fixtures", || {
            let o = out(anth(&sse(&[
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 5, "cache_read_input_tokens": 1}}}),
                block_start(0, json!({"type": "text", "text": ""})),
                block_delta(0, json!({"type": "text_delta", "text": "Hello"})),
                block_delta(0, json!({"type": "text_delta", "text": "!"})),
                block_stop(0),
                block_start(1, json!({"type": "thinking", "thinking": ""})),
                block_delta(1, json!({"type": "thinking_delta", "thinking": "thinking"})),
                block_delta(1, json!({"type": "signature_delta", "signature": "sig_1"})),
                block_stop(1),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": "\n\nHuman:"}, "usage": {"output_tokens": 2}}),
                stop(),
            ])));
            assert_eq!((o.text.as_str(), o.reasoning.as_str(), o.finish.as_str()), ("Hello!", "thinking", "end_turn"));
            assert_eq!(o.usage, usage(5, 2, 1, 0));
        }),
        ("anthropic-messages :: preserves terminal state across usage-only message deltas", || {
            let o = out(anth(&sse(&[
                start(5),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": "X"}, "usage": {"output_tokens": 8}}),
                json!({"type": "message_delta", "delta": {}, "usage": {"output_tokens": 10}}),
                stop(),
            ])));
            assert_eq!(o.finish, "end_turn");
            assert_eq!(o.usage, usage(5, 10, 0, 0));
        }),
        ("anthropic-messages :: requires message_stop before completing a streamed message", || {
            let r = anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "text", "text": ""})),
                block_delta(0, json!({"type": "text_delta", "text": "Hello"})),
                block_stop(0),
                message_delta("end_turn", 1),
            ]));
            assert!(matches!(r, Err(Error::Transport(_))), "{r:?}");
        }),
        ("anthropic-messages :: ignores unknown named SSE events", || {
            let named = |e: &str, d: &str| format!("event: {e}\ndata: {d}\n\n");
            let body = [
                named("message_start", &start(5).to_string()),
                named("proxy.stats", "not json"),
                named("content_block_start", &block_start(0, json!({"type": "text", "text": ""})).to_string()),
                named("content_block_delta", &block_delta(0, json!({"type": "text_delta", "text": "Hello"})).to_string()),
                named("content_block_stop", &block_stop(0).to_string()),
                named("message_delta", &message_delta("end_turn", 1).to_string()),
                named("message_stop", &stop().to_string()),
                named("proxy.done", "still not json"),
            ]
            .concat();
            let o = out(anth(&body));
            assert_eq!((o.text.as_str(), o.finish.as_str()), ("Hello", "end_turn"));
        }),
        ("anthropic-messages :: ignores unknown content block and delta variants", || {
            let o = out(anth(&sse(&[
                start(5),
                json!({"type": "future_event", "content_block": 42, "delta": 42}),
                block_start(0, json!({"type": "future_block", "text": 42})),
                block_delta(0, json!({"text": "ignored"})),
                block_delta(0, json!({"type": "future_delta", "text": 42})),
                block_delta(0, json!({"type": "text_delta", "text": "hidden"})),
                block_delta(0, json!({"type": "thinking_delta", "thinking": "hidden"})),
                block_delta(0, json!({"type": "signature_delta", "signature": "hidden"})),
                block_stop(0),
                block_start(1, json!({"type": "text", "text": ""})),
                block_delta(1, json!({"type": "text_delta", "text": "Hello"})),
                block_stop(1),
                message_delta("end_turn", 1),
                stop(),
            ])));
            assert_eq!((o.text.as_str(), o.reasoning.as_str()), ("Hello", ""));
        }),
        ("anthropic-messages :: maps nullable input tokens and preserves unknown Anthropic usage fields", || {
            let o = out(anth(&sse(&[
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 5, "cache_read_input_tokens": 2, "service_tier": "standard",
                    "cache_creation": {"ephemeral_5m_input_tokens": 1}, "server_tool_use": {"web_search_requests": 1},
                    "output_tokens_details": {"thinking_tokens": 3}}}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": null, "output_tokens": 8,
                    "future_terminal": {"requests": 4}}}),
                stop(),
            ])));
            assert_eq!(o.usage, usage(5, 8, 2, 0));
        }),
        ("anthropic-messages :: retains complete tool input from content_block_start", || {
            let o = out(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"query": "weather"}})),
                block_stop(0),
                message_delta("tool_use", 1),
                stop(),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
        }),
        ("anthropic-messages :: assembles streamed tool call input", || {
            let o = out(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "tool_use", "id": "call_1", "name": "lookup", "input": {}})),
                block_delta(0, json!({"type": "input_json_delta", "partial_json": "{\"query\""})),
                block_delta(0, json!({"type": "input_json_delta", "partial_json": ":\"weather\"}"})),
                block_stop(0),
                message_delta("tool_use", 1),
                stop(),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
            assert_eq!(o.finish, "tool_use");
        }),
        ("anthropic-messages :: ignores tool input deltas without a matching tool start", || {
            let o = out(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "text", "text": ""})),
                block_delta(0, json!({"type": "text_delta", "text": "Hello"})),
                block_delta(1, json!({"type": "input_json_delta", "partial_json": "{\"query\":\"orphaned\"}"})),
                block_stop(0),
                message_delta("end_turn", 1),
                stop(),
            ])));
            assert_eq!(o.text, "Hello");
            assert!(o.calls.is_empty());
        }),
        ("anthropic-messages :: settles pending tool calls at message_stop", || {
            let o = out(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "tool_use", "id": "call_1", "name": "lookup"})),
                block_delta(0, json!({"type": "input_json_delta", "partial_json": "{\"query\":\"weather\"}"})),
                message_delta("tool_use", 1),
                stop(),
            ])));
            assert_eq!(o.calls, vec![("call_1".into(), "lookup".into(), json!({"query": "weather"}))]);
        }),
        ("anthropic-messages :: assembles and persists multiple tool calls from one Anthropic response", || {
            let o = out(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "tool_use", "id": "call_paris", "name": "weather", "input": {}})),
                block_delta(0, json!({"type": "input_json_delta", "partial_json": "{\"city\":\"Paris\"}"})),
                block_stop(0),
                block_start(1, json!({"type": "tool_use", "id": "call_london", "name": "weather", "input": {}})),
                block_delta(1, json!({"type": "input_json_delta", "partial_json": "{\"city\":\"London\"}"})),
                block_stop(1),
                message_delta("tool_use", 2),
                stop(),
            ])));
            assert_eq!(o.ids(), ["call_paris", "call_london"]);
            assert_eq!(o.inputs(), [("weather", &json!({"city": "Paris"})), ("weather", &json!({"city": "London"}))]);
        }),
    ]);
}

#[test]
fn anthropic_messages_errors() {
    run(&[
        ("anthropic-messages :: fails with a typed provider error for stream error frames", || {
            let r = anth(&sse(&[json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})]));
            assert_eq!(provider_err(r), ("overloaded_error: Overloaded".into(), true));
        }),
        ("anthropic-messages :: ignores untyped stream frames but reads untyped error envelopes as errors", || {
            let o = out(anth(&sse(&[
                json!({}),
                start(5),
                block_start(0, json!({"type": "text", "text": ""})),
                json!({"gateway": "keep-alive"}),
                block_delta(0, json!({"type": "text_delta", "text": "OK"})),
                block_stop(0),
                message_delta("end_turn", 1),
                stop(),
            ])));
            assert_eq!(o.text, "OK");
            let r = anth(&sse(&[json!({"error": {"type": "overloaded_error", "message": "Overloaded"}})]));
            assert_eq!(provider_err(r), ("overloaded_error: Overloaded".into(), true));
        }),
        ("anthropic-messages :: classifies prompt-too-long provider errors", || {
            let (m, retry) = provider_err(anth(&sse(&[json!({"type": "error",
                "error": {"type": "invalid_request_error", "message": "prompt is too long: 210000 tokens"}})])));
            assert_eq!(m, "invalid_request_error: prompt is too long: 210000 tokens");
            assert!(!retry);
        }),
        ("anthropic-messages :: falls back to error type when no message is present", || {
            let r = anth(&sse(&[json!({"type": "error", "error": {"type": "overloaded_error", "message": ""}})]));
            assert_eq!(provider_err(r), ("overloaded_error".into(), true));
        }),
        ("anthropic-messages :: falls back to a stable default when error payload is absent", || {
            let r = anth(&sse(&[json!({"type": "error"})]));
            assert_eq!(provider_err(r), ("Anthropic Messages stream error".into(), false));
        }),
    ]);
}

// ---- Gemini -----------------------------------------------------------------

fn gemini_turns(model: &str, messages: Vec<Message>) -> Value {
    gemini::body(&req(model, messages))["contents"].clone()
}

fn sentinel_call(id: Option<&str>, args: Value) -> Value {
    let mut c = json!({"name": "lookup", "args": args});
    if let Some(id) = id {
        c["id"] = json!(id);
    }
    json!({"functionCall": c, "thoughtSignature": gemini::SKIP_SIGNATURE})
}

#[test]
fn gemini_requests() {
    run(&[
        ("gemini :: prepares Gemini target", || {
            assert_eq!(
                gemini::body(&hello("gemini-2.5-flash")),
                json!({"contents": [{"role": "user", "parts": [{"text": "Say hello."}]}],
                    "systemInstruction": {"parts": [{"text": "You are concise."}]},
                    "generationConfig": {"maxOutputTokens": 20, "temperature": 0.0}})
            );
        }),
        ("gemini :: keeps system updates separate from function responses (adapted: user text)", || {
            let c = gemini_turns(
                "gemini-2.5-flash",
                vec![
                    asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                    results(vec![result("call_1", "lookup", "done"), text("Update.")]),
                    user("Later update."),
                ],
            );
            assert_eq!(c.as_array().unwrap().len(), 3);
            assert_eq!(c[0], json!({"role": "model", "parts": [sentinel_call(None, json!({"query": "weather"}))]}));
            assert_eq!(c[1]["parts"].as_array().unwrap().len(), 1);
            assert_eq!(c[1]["parts"][0]["functionResponse"]["name"], "lookup");
            assert_eq!(c[2], json!({"role": "user", "parts": [{"text": "Update."}, {"text": "Later update."}]}));
        }),
        ("gemini :: merges parallel tool results into one function-response turn", || {
            let c = gemini_turns(
                "gemini-2.5-flash",
                vec![
                    asst(vec![
                        call("call_1", "lookup", json!({"query": "weather"})),
                        call("call_2", "lookup", json!({"query": "time"})),
                    ]),
                    results(vec![result("call_1", "lookup", "sunny")]),
                    results(vec![result("call_2", "lookup", "noon")]),
                ],
            );
            assert_eq!(
                c,
                json!([
                    {"role": "model", "parts": [sentinel_call(None, json!({"query": "weather"})), sentinel_call(None, json!({"query": "time"}))]},
                    {"role": "user", "parts": [
                        {"functionResponse": {"name": "lookup", "response": {"output": "sunny"}}},
                        {"functionResponse": {"name": "lookup", "response": {"output": "noon"}}}
                    ]}
                ])
            );
        }),
        ("gemini :: lowers function call ids for gemini 3 models", || {
            let c = gemini_turns(
                "gemini-3-flash-preview",
                vec![
                    asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                    results(vec![result("call_1", "lookup", "done")]),
                ],
            );
            assert_eq!(c[0]["parts"][0], sentinel_call(Some("call_1"), json!({"query": "weather"})));
            assert_eq!(c[1]["parts"][0]["functionResponse"]["id"], "call_1");
        }),
        ("gemini :: omits function call ids entirely for pre-gemini-3 models", || {
            for model in ["gemini-2.5-flash", "gemini-1.5-flash"] {
                let c = gemini_turns(
                    model,
                    vec![
                        asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                        results(vec![result("call_1", "lookup", "done")]),
                    ],
                );
                assert!(!c.to_string().contains("\"id\""), "{model}: {c}");
            }
        }),
        ("gemini :: includes function call ids for non-gemini model ids", || {
            let c = gemini_turns(
                "gemma-3-27b-it",
                vec![
                    asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                    results(vec![result("call_1", "lookup", "done")]),
                ],
            );
            assert_eq!(c[0]["parts"][0], sentinel_call(Some("call_1"), json!({"query": "weather"})));
            assert_eq!(c[1]["parts"][0]["functionResponse"]["id"], "call_1");
        }),
        ("gemini :: replays unsigned Gemini 3 tool calls with the validator bypass sentinel", || {
            let c = gemini_turns("gemini-3-flash-preview", vec![asst(vec![call("tool_0", "lookup", json!({"query": "weather"}))])]);
            assert_eq!(c[0]["parts"][0]["thoughtSignature"], gemini::SKIP_SIGNATURE);
        }),
        ("gemini :: leaves unsigned parallel calls unchanged after a signed Gemini 3 call", || {
            let c = gemini_turns(
                "gemini-3-flash-preview",
                vec![asst(vec![
                    call("tool_0~parallel_signature", "lookup", json!({"query": "weather"})),
                    call("tool_1", "lookup", json!({"query": "news"})),
                    call("tool_2", "lookup", json!({"query": "sports"})),
                ])],
            );
            assert_eq!(
                c[0]["parts"],
                json!([
                    {"functionCall": {"id": "tool_0", "name": "lookup", "args": {"query": "weather"}}, "thoughtSignature": "parallel_signature"},
                    {"functionCall": {"id": "tool_1", "name": "lookup", "args": {"query": "news"}}},
                    {"functionCall": {"id": "tool_2", "name": "lookup", "args": {"query": "sports"}}}
                ])
            );
        }),
        ("gemini :: adds the validator bypass sentinel to every call in an unsigned Gemini 3 batch", || {
            let c = gemini_turns(
                "gemini-3-flash-preview",
                vec![asst(vec![call("tool_0", "lookup", json!({"query": "weather"})), call("tool_1", "lookup", json!({"query": "news"}))])],
            );
            assert_eq!(
                c[0]["parts"],
                json!([sentinel_call(Some("tool_0"), json!({"query": "weather"})), sentinel_call(Some("tool_1"), json!({"query": "news"}))])
            );
        }),
    ]);
}

fn gemini_params(schema: Value) -> Value {
    let mut r = hello("gemini-2.5-flash");
    r.tools = vec![tool("lookup", schema)];
    gemini::body(&r)["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"].clone()
}

#[test]
fn gemini_tool_schemas() {
    run(&[
        (
            "gemini :: normalizes the JSON Schema shapes Gemini rejects",
            || {
                let got = gemini_params(json!({
                    "type": "object",
                    "required": ["ttl", "items", "parent", "missing"],
                    "properties": {
                        "ttl": {"type": "number", "minimum": 0, "exclusiveMinimum": true, "maximum": 60, "exclusiveMaximum": false},
                        "pair": {"type": "array", "items": [{"type": "string"}, {"type": "integer"}], "additionalItems": false},
                        "items": {"type": "array", "items": {"type": "string"}},
                        "required": {"type": "boolean"},
                        "params": {"$ref": "#/$defs/Json"},
                        "tree": {"$ref": "#/$defs/Node"},
                        "chain": {"$ref": "#/$defs/Link"},
                        "batch": {"$ref": "#/$defs/Batch"},
                        "self": {"$ref": "#"},
                        "parent": {"$ref": "#"},
                        "cart": {"type": "object", "default": {"items": ["a"]}}
                    },
                    "dependentRequired": {"items": ["ttl"]},
                    "$defs": {
                        "Json": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"$ref": "#/$defs/Json"}},
                            {"type": "object", "additionalProperties": {"$ref": "#/$defs/Json"}}]},
                        "Node": {"type": "object", "properties": {"child": {"$ref": "#/$defs/Node"}}},
                        "Link": {"type": "object", "required": ["next"], "properties": {"next": {"$ref": "#/$defs/Link"}}},
                        "Batch": {"type": "array", "minItems": 1, "items": {"$ref": "#/$defs/Batch"}}
                    }
                }));
                assert_eq!(
                    got,
                    json!({
                        "type": "object",
                        "required": ["ttl", "items", "parent"],
                        "properties": {
                            "ttl": {"type": "number", "exclusiveMinimum": 0, "maximum": 60},
                            "pair": {"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}], "items": false},
                            "items": {"type": "array", "items": {"type": "string"}},
                            "required": {"type": "boolean"},
                            "params": {"$ref": "#/$defs/Json"},
                            "tree": {"$ref": "#/$defs/Node"},
                            "chain": {"$ref": "#/$defs/Link"},
                            "batch": {"$ref": "#/$defs/Batch"},
                            "self": {"$ref": "#"},
                            "parent": {},
                            "cart": {"type": "object", "default": {"items": ["a"]}}
                        },
                        "dependentRequired": {"items": ["ttl"]},
                        "$defs": {
                            "Json": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"$ref": "#/$defs/Json"}},
                                {"type": "object", "additionalProperties": {}}]},
                            "Node": {"type": "object", "properties": {"child": {"$ref": "#/$defs/Node"}}},
                            "Link": {"type": "object", "required": ["next"], "properties": {"next": {}}},
                            "Batch": {"type": "array", "minItems": 1, "items": {}}
                        }
                    })
                );
            },
        ),
        (
            "gemini :: preserves nested empty object tool schemas",
            || {
                let s = json!({"type": "object", "required": ["options"],
                "properties": {"options": {"type": "object", "description": "Optional provider settings", "properties": {}}}});
                assert_eq!(gemini_params(s.clone()), s);
            },
        ),
        (
            "gemini :: sends standard JSON Schema tool parameters unchanged",
            || {
                let s = json!({
                    "type": "object", "required": ["level"],
                    "properties": {
                        "level": {"type": "integer", "enum": [1, 2, 3]},
                        "status": {"type": ["number", "string"], "description": "Status filter"},
                        "maybe": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                        "kind": {"const": "fixed"},
                        "mode": {"enum": ["fast", "safe"]},
                        "tags": {"type": "array"},
                        "env": {"type": "object", "additionalProperties": {"type": "string"}},
                        "action": {"oneOf": [
                            {"type": "object", "properties": {"type": {"const": "move"}, "to": {"type": "string"}}},
                            {"type": "object", "properties": {"type": {"const": "delete"}}}
                        ]},
                        "item": {"$ref": "#/$defs/Item"}
                    },
                    "$defs": {"Item": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}
                });
                assert_eq!(gemini_params(s.clone()), s);
            },
        ),
    ]);
}

fn candidate(parts: Value, finish: Option<&str>) -> Value {
    let mut c = json!({"content": {"role": "model", "parts": parts}});
    if let Some(f) = finish {
        c["finishReason"] = json!(f);
    }
    json!({"candidates": [c]})
}

#[test]
fn gemini_streams() {
    run(&[
        (
            "gemini :: parses text, reasoning, and usage stream fixtures",
            || {
                let o = out(gem(&sse(&[
                    candidate(json!([{"text": "thinking", "thought": true}]), None),
                    candidate(json!([{"text": "Hello"}]), None),
                    candidate(json!([{"text": "!"}]), Some("STOP")),
                    json!({"usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "thoughtsTokenCount": 1}}),
                ])));
                assert_eq!(
                    (o.reasoning.as_str(), o.text.as_str(), o.finish.as_str()),
                    ("thinking", "Hello!", "stop")
                );
                assert_eq!(o.usage, usage(5, 3, 0, 0));
            },
        ),
        (
            "gemini :: counts cumulative tool-use prompts once alongside cached input",
            || {
                let mut a = candidate(json!([{"text": "Working"}]), None);
                a["usageMetadata"] = json!({"promptTokenCount": 271, "cachedContentTokenCount": 100, "toolUsePromptTokenCount": 200});
                let mut b = candidate(json!([{"text": " done"}]), Some("STOP"));
                b["usageMetadata"] = json!({"promptTokenCount": 271, "cachedContentTokenCount": 100, "toolUsePromptTokenCount": 480,
                "candidatesTokenCount": 130, "thoughtsTokenCount": 158});
                assert_eq!(out(gem(&sse(&[a, b]))).usage, usage(651, 288, 100, 0));
            },
        ),
        ("gemini :: ignores unknown response parts", || {
            let o = out(gem(&sse(&[candidate(
                json!([{"text": "Hello "}, {"executableCode": {"language": "PYTHON", "code": "print('ignored')"}}, {"text": "world"}]),
                Some("STOP"),
            )])));
            assert_eq!(o.text, "Hello world");
        }),
        (
            "gemini :: emits streamed tool calls and maps finish reason",
            || {
                let mut c = candidate(
                    json!([{"functionCall": {"name": "lookup", "args": {"query": "weather"}}}]),
                    Some("STOP"),
                );
                c["usageMetadata"] = json!({"promptTokenCount": 5, "candidatesTokenCount": 1});
                let o = out(gem(&sse(&[c])));
                assert_eq!(o.inputs(), [("lookup", &json!({"query": "weather"}))]);
                assert_eq!(o.finish, "tool_calls");
                assert_eq!(o.usage, usage(5, 1, 0, 0));
            },
        ),
        (
            "gemini :: defaults omitted function call args to an empty object",
            || {
                let o = out(gem(&sse(&[candidate(
                    json!([{"functionCall": {"name": "ping"}}]),
                    Some("STOP"),
                )])));
                assert!(o.calls[0].0.starts_with("tool_"));
                assert_eq!(o.inputs(), [("ping", &json!({}))]);
            },
        ),
        ("gemini :: maps tool calls without a finish reason", || {
            for finish in [None, Some("FINISH_REASON_UNSPECIFIED")] {
                let o = out(gem(&sse(&[candidate(
                    json!([{"functionCall": {"name": "lookup", "args": {"query": "weather"}}}]),
                    finish,
                )])));
                assert_eq!(o.finish, "tool_calls");
            }
        }),
        (
            "gemini :: assigns unique ids to multiple streamed tool calls",
            || {
                let o = out(gem(&sse(&[candidate(
                    json!([{"functionCall": {"id": "call_0", "name": "lookup", "args": {"query": "weather"}}},
                       {"functionCall": {"name": "lookup", "args": {"query": "news"}}}]),
                    Some("STOP"),
                )])));
                assert_eq!(o.ids()[0], "call_0");
                assert!(o.ids()[1].starts_with("tool_"));
            },
        ),
        (
            "gemini :: replaces repeated supplier ids with fresh fallback ids",
            || {
                let o = out(gem(&sse(&[candidate(
                    json!([{"functionCall": {"id": "dup_call", "name": "lookup", "args": {"query": "weather"}}},
                       {"functionCall": {"id": "dup_call", "name": "lookup", "args": {"query": "news"}}}]),
                    Some("STOP"),
                )])));
                assert_eq!(o.ids()[0], "dup_call");
                assert!(o.ids()[1].starts_with("tool_"));
            },
        ),
        (
            "gemini :: assigns distinct unique fallback ids across separate requests",
            || {
                let body = sse(&[candidate(
                    json!([{"functionCall": {"name": "lookup", "args": {"query": "weather"}}}]),
                    Some("STOP"),
                )]);
                let (a, b) = (out(gem(&body)), out(gem(&body)));
                assert_ne!(a.ids(), b.ids());
            },
        ),
        (
            "gemini :: maps length and content-filter finish reasons (raw)",
            || {
                assert_eq!(
                    out(gem(&sse(&[candidate(json!([]), Some("MAX_TOKENS"))]))).finish,
                    "length"
                );
                assert_eq!(
                    out(gem(&sse(&[candidate(json!([]), Some("SAFETY"))]))).finish,
                    "safety"
                );
            },
        ),
        (
            "gemini :: preserves blocking finishes and rejects invalid-output finish reasons",
            || {
                for raw in [
                    "MODEL_ARMOR",
                    "IMAGE_PROHIBITED_CONTENT",
                    "IMAGE_RECITATION",
                    "LANGUAGE",
                    "IMAGE_OTHER",
                ] {
                    assert_eq!(
                        out(gem(&sse(&[candidate(json!([]), Some(raw))]))).finish,
                        raw.to_lowercase()
                    );
                }
                for raw in [
                    "MALFORMED_FUNCTION_CALL",
                    "UNEXPECTED_TOOL_CALL",
                    "NO_IMAGE",
                    "TOO_MANY_TOOL_CALLS",
                    "MISSING_THOUGHT_SIGNATURE",
                    "MALFORMED_RESPONSE",
                ] {
                    let event = json!({"candidates": [{"finishReason": raw, "finishMessage": "Provider detail"}], "responseId": "failure"});
                    let (m, retry) = provider_err(gem(&sse(&[event])));
                    assert_eq!(m, format!("Gemini stopped with {raw}"));
                    assert_eq!(
                        retry,
                        matches!(
                            raw,
                            "MALFORMED_RESPONSE"
                                | "MALFORMED_FUNCTION_CALL"
                                | "UNEXPECTED_TOOL_CALL"
                        ),
                        "{raw}"
                    );
                }
            },
        ),
        ("gemini :: survives explicit null usage counts", || {
            let o = out(gem(&sse(&[
                candidate(json!([{"text": "Hi"}]), None),
                json!({"usageMetadata": {"promptTokenCount": null, "toolUsePromptTokenCount": null, "candidatesTokenCount": 5}}),
            ])));
            assert_eq!(o.text, "Hi");
            assert_eq!(o.usage, usage(0, 5, 0, 0));
        }),
        (
            "gemini :: survives null candidates, content, parts, and finish reason",
            || {
                let o = out(gem(&sse(&[
                    json!({"candidates": null}),
                    json!({"candidates": [{"content": {"role": "model", "parts": null}}]}),
                    json!({"candidates": [{"content": null, "finishReason": null}]}),
                    candidate(json!([{"text": "Hello"}]), Some("STOP")),
                ])));
                assert_eq!((o.text.as_str(), o.finish.as_str()), ("Hello", "stop"));
            },
        ),
        (
            "gemini :: treats a null thought flag on a text part as visible output",
            || {
                let o = out(gem(&sse(&[candidate(
                    json!([{"text": "Visible", "thought": null}]),
                    Some("STOP"),
                )])));
                assert_eq!((o.text.as_str(), o.reasoning.as_str()), ("Visible", ""));
            },
        ),
        ("gemini :: fails invalid stream events", || {
            assert!(matches!(gem("data: {not json}\n\n"), Err(Error::Decode(_))));
        }),
        (
            "codesmith :: classifies in-stream Gemini errors by status",
            || {
                let e = |v: Value| provider_err(gem(&sse(&[v])));
                assert_eq!(
                    e(
                        json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED", "message": "Quota exceeded"}})
                    ),
                    ("Quota exceeded".into(), true)
                );
                assert_eq!(
                    e(
                        json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "message": "Bad schema"}})
                    ),
                    ("Bad schema".into(), false)
                );
            },
        ),
    ]);
}

// ---- shared: tool choice, tool schemas, tool history, HTTP errors -----------

#[test]
fn empty_tool_choice() {
    run(&[(
        "empty-tool-choice :: openai-chat / openai-responses omit tool choice without active tools",
        || {
            for messages in [
                vec![user("Say OK.")],
                vec![
                    user("Look up the value."),
                    asst(vec![call("call_1", "lookup", json!({}))]),
                    results(vec![result("call_1", "lookup", "OK")]),
                ],
            ] {
                let r = req("gpt-4.1-mini", messages);
                for b in [chat_at(OPENAI, &r), responses::body(&r)] {
                    assert!(
                        b.get("tool_choice").is_none() && b.get("tools").is_none(),
                        "{b}"
                    );
                }
            }
        },
    )]);
}

fn chat_params(model: &str, schema: Value) -> Value {
    let mut r = req(model, vec![user("Use the tool.")]);
    r.tools = vec![tool("lookup", schema)];
    chat_at(OPENAI, &r)["tools"][0]["function"]["parameters"].clone()
}

#[test]
fn tool_schema_projection() {
    run(&[
        ("tool-schema-projection :: declares every tool schema root as an object", || {
            let union = json!({"anyOf": [{"type": "object", "properties": {"a": {"type": "string"}}},
                {"type": "object", "properties": {"b": {"type": "string"}}}]});
            let exclusive = json!({"oneOf": [{"type": "object"}, {"type": "object", "required": ["a"]}]});
            let typed = |v: &Value| {
                let mut v = v.clone();
                v["type"] = json!("object");
                v
            };
            let parameterless = json!({"type": "object", "properties": {}, "additionalProperties": false, "description": "No input."});
            assert_eq!(chat_params("gpt-6-luna", parameterless.clone()), parameterless);
            assert_eq!(chat_params("gpt-6-luna", json!({})), json!({"type": "object"}));
            assert_eq!(
                chat_params("gpt-6-luna", json!({"description": "Query", "properties": {"q": {"type": "string"}}})),
                json!({"type": "object", "description": "Query", "properties": {"q": {"type": "string"}}})
            );
            assert_eq!(chat_params("gpt-6-luna", union.clone()), typed(&union));
            assert_eq!(chat_params("gpt-6-luna", exclusive.clone()), typed(&exclusive));
            let object = json!({"type": "object", "properties": {}});
            assert_eq!(chat_params("gpt-6-luna", object.clone()), object);
            assert_eq!(
                chat_params("kimi-k3", json!({"properties": {"mode": {"enum": ["fast"]}}})),
                json!({"type": "object", "properties": {"mode": {"type": "string", "enum": ["fast"]}}})
            );
            // The `{ not: { type: "null" } }` empty-struct marker goes.
            assert_eq!(
                schema::object_root(&json!({"not": {"type": "null"}, "properties": {}})),
                json!({"type": "object", "properties": {}})
            );
        }),
        ("tool-schema-projection :: selects tool schema handling from the model name", || {
            let original = json!({"type": "object", "required": ["mode", "missing"], "properties": {"mode": {"enum": ["fast", "safe"]}}});
            let mut gemini = original.clone();
            gemini["required"] = json!(["mode"]);
            let mut moonshot = original.clone();
            moonshot["properties"]["mode"]["type"] = json!("string");
            assert_eq!(chat_params("google/Gemini-3.8-Flash", original.clone()), gemini);
            assert_eq!(chat_params("moonshotai/Kimi-K3", original.clone()), moonshot);
            assert_eq!(chat_params("gpt-6-luna", original.clone()), original);
        }),
        ("tool-schema-projection :: applies model compatibility without changing schema semantics", || {
            let got = chat_params(
                "kimi-k2",
                json!({"type": "object", "anyOf": [{"type": "object", "properties": {
                    "tuple": {"type": "array", "items": [{"type": "string"}, {"type": "number"}]},
                    "linked": {"$ref": "#/$defs/Linked", "description": "drop me"}}}]}),
            );
            assert_eq!(
                got,
                json!({"type": "object", "anyOf": [{"type": "object", "properties": {
                    "tuple": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "number"}]}},
                    "linked": {"$ref": "#/$defs/Linked"}}}]})
            );
        }),
        ("tool-schema-projection :: moonshot strips $ref siblings and converts tuple arrays to a schema object", || {
            let got = schema::moonshot(&json!({
                "type": "object",
                "properties": {
                    "linked": {"$ref": "#/$defs/Linked", "description": "drop me"},
                    "tuple": {"type": "array", "items": [{"type": "string"}, {"type": "number"}]},
                    "prefixTuple": {"type": "array", "prefixItems": [{"type": "boolean"}, {"type": "string"}]},
                    "closedTuple": {"type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}], "items": false},
                    "restTuple": {"type": "array", "prefixItems": [{"type": "string"}], "items": {"type": "number"}},
                    "prefixItems": {"type": "string"},
                    "unevaluatedItems": {"type": "boolean"},
                    "items": {"type": "array", "items": {"type": "string"}},
                    "cart": {"type": "object", "default": {"items": ["a", "b"], "$ref": "#/literal", "count": 1},
                        "const": {"enum": ["literal"]}, "examples": [{"items": ["x", "y"]}]}
                },
                "dependentRequired": {"items": ["cart"]},
                "dependencies": {"items": ["cart"], "prefixItems": {"required": ["items"]}}
            }));
            assert_eq!(
                got,
                json!({
                    "type": "object",
                    "properties": {
                        "linked": {"$ref": "#/$defs/Linked"},
                        "tuple": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "number"}]}},
                        "prefixTuple": {"type": "array", "items": {"anyOf": [{"type": "boolean"}, {"type": "string"}]}},
                        "closedTuple": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "number"}]}},
                        "restTuple": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "number"}]}},
                        "prefixItems": {"type": "string"},
                        "unevaluatedItems": {"type": "boolean"},
                        "items": {"type": "array", "items": {"type": "string"}},
                        "cart": {"type": "object", "default": {"items": ["a", "b"], "$ref": "#/literal", "count": 1},
                            "const": {"enum": ["literal"]}, "examples": [{"items": ["x", "y"]}]}
                    },
                    "dependentRequired": {"items": ["cart"]},
                    "dependencies": {"items": ["cart"], "prefixItems": {"required": ["items"]}}
                })
            );
        }),
        ("tool-schema-projection :: moonshot derives a type for untyped enums", || {
            let got = schema::moonshot(&json!({
                "type": "object",
                "properties": {
                    "kind": {"description": "The kind of flag", "enum": ["boolean", "string"]},
                    "level": {"enum": [1, 2.5]},
                    "optional": {"enum": [null, "a"]},
                    "choice": {"anyOf": [{"enum": [true, false]}, {"type": "null"}]},
                    "list": {"type": "array", "items": {"enum": ["x"]}},
                    "map": {"type": "object", "additionalProperties": {"enum": ["y"]}},
                    "typed": {"type": "string", "enum": ["a", null]},
                    "mixed": {"enum": ["a", 1]}
                },
                "$defs": {"Mode": {"enum": ["fast"]}}
            }));
            assert_eq!(
                got,
                json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "description": "The kind of flag", "enum": ["boolean", "string"]},
                        "level": {"type": "number", "enum": [1, 2.5]},
                        "optional": {"type": ["string", "null"], "enum": [null, "a"]},
                        "choice": {"anyOf": [{"type": "boolean", "enum": [true, false]}, {"type": "null"}]},
                        "list": {"type": "array", "items": {"type": "string", "enum": ["x"]}},
                        "map": {"type": "object", "additionalProperties": {"type": "string", "enum": ["y"]}},
                        "typed": {"type": "string", "enum": ["a", null]},
                        "mixed": {"enum": ["a", 1]}
                    },
                    "$defs": {"Mode": {"type": "string", "enum": ["fast"]}}
                })
            );
        }),
    ]);
}

fn roles_and_parts(ms: &[Message]) -> Vec<(Role, Vec<Part>)> {
    ms.iter()
        .map(|m| (m.role.clone(), m.parts.clone()))
        .collect()
}

fn missing(id: &str) -> Part {
    error_result(id, id, "Tool result missing")
}

#[test]
fn tool_history() {
    run(&[
        ("tool-history :: fills missing local results before the next step", || {
            let got = normalize_tool_history(&[
                asst(vec![call("first", "first", json!({})), call("second", "second", json!({}))]),
                results(vec![result("first", "wrong", "done")]),
                user("Continue."),
                asst(vec![call("trailing", "trailing", json!({}))]),
            ]);
            assert_eq!(
                roles_and_parts(&got),
                roles_and_parts(&[
                    asst(vec![call("first", "first", json!({})), call("second", "second", json!({}))]),
                    results(vec![result("first", "first", "done")]),
                    results(vec![missing("second")]),
                    user("Continue."),
                    asst(vec![call("trailing", "trailing", json!({}))]),
                    results(vec![missing("trailing")]),
                ])
            );
        }),
        ("tool-history :: fills missing results for trailing calls without replacing available results", || {
            let calls = asst(vec![call("answered", "answered", json!({})), call("unanswered", "unanswered", json!({}))]);
            let answered = results(vec![result("answered", "answered", "done")]);
            assert_eq!(
                normalize_tool_history(&[calls.clone(), answered.clone()]),
                vec![calls, answered, results(vec![missing("unanswered")])]
            );
        }),
        ("tool-history :: normalizes empty results without changing whitespace", || {
            let got = normalize_tool_history(&[
                asst(vec![call("text", "text", json!({})), call("error", "error", json!({})), call("whitespace", "whitespace", json!({}))]),
                results(vec![result("text", "text", "")]),
                results(vec![error_result("error", "error", "")]),
                results(vec![result("whitespace", "whitespace", "   ")]),
            ]);
            let parts: Vec<Part> = got[1..].iter().flat_map(|m| m.parts.clone()).collect();
            assert_eq!(
                parts,
                vec![
                    result("text", "text", "(no tool output)"),
                    error_result("error", "error", "(no tool output)"),
                    result("whitespace", "whitespace", "   "),
                ]
            );
        }),
        ("tool-history :: leaves unmatched history unchanged", || {
            let orphan = results(vec![result("orphan", "orphan", "ignored")]);
            assert_eq!(normalize_tool_history(std::slice::from_ref(&orphan)), vec![orphan]);
        }),
        ("tool-history :: uses a matching call as the complete tool identity", || {
            let got = normalize_tool_history(&[
                asst(vec![call("call_1", "lookup", json!({}))]),
                results(vec![result("call_1", "wrong", "done")]),
            ]);
            assert_eq!(got[1].parts, vec![result("call_1", "lookup", "done")]);
        }),
        ("tool-history :: moves system updates after the results of pending calls (adapted: user text)", || {
            let calls = asst(vec![call("first", "first", json!({})), call("second", "second", json!({}))]);
            let got = normalize_tool_history(&[
                calls.clone(),
                results(vec![result("first", "first", "one")]),
                user("First update."),
                user("Second update."),
                results(vec![result("second", "second", "two")]),
                user("Continue."),
            ]);
            assert_eq!(
                got,
                vec![
                    calls,
                    results(vec![result("first", "first", "one")]),
                    results(vec![result("second", "second", "two")]),
                    user("First update."),
                    user("Second update."),
                    user("Continue."),
                ]
            );
            let one = asst(vec![call("first", "first", json!({}))]);
            assert_eq!(
                normalize_tool_history(&[one.clone(), user("Update."), user("Continue.")]),
                vec![one.clone(), results(vec![missing("first")]), user("Update."), user("Continue.")]
            );
            assert_eq!(
                normalize_tool_history(&[one.clone(), user("Update.")]),
                vec![one, results(vec![missing("first")]), user("Update.")]
            );
        }),
        ("tool-history :: keeps system updates in place when no call is pending (adapted: user text)", || {
            let history = vec![
                user("Start."),
                user("Update."),
                asst(vec![call("first", "first", json!({}))]),
                results(vec![result("first", "first", "one")]),
                user("Update."),
                user("Continue."),
            ];
            assert_eq!(normalize_tool_history(&history), history);
        }),
    ]);
}

#[test]
fn http_status_errors() {
    run(&[(
        "openai-chat / openai-responses / anthropic-messages :: fails HTTP provider errors before stream parsing (status classification)",
        || {
            let body = "{\"error\":{\"message\":\"Bad request\",\"type\":\"invalid_request_error\"}}";
            let e = Error::Status { status: 400, body: body.into() };
            assert!(!e.retryable());
            assert!(e.to_string().contains("Bad request"));
            // openai-responses :: retains the token-sharing HTTP 429 body for retry hooks
            let body = "{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\",\"message\":\"Rate limit exceeded\"}}";
            let e = Error::Status { status: 429, body: body.into() };
            assert!(e.retryable());
            assert!(e.to_string().contains("subscription_sharing_usage_limit_exceeded"));
        },
    )]);
}

// ---- transport: the shared streaming driver ----------------------------------

/// A response whose body yields `chunks`, then either an I/O error or (when
/// `hang`) never ends.
fn response(chunks: Vec<&'static str>, fail: bool, hang: bool) -> reqwest::Response {
    use futures_util::StreamExt;
    let items = chunks
        .into_iter()
        .map(|c| Ok::<_, std::io::Error>(bytes_of(c)))
        .chain(fail.then(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "ECONNRESET",
            ))
        }));
    let s = futures_util::stream::iter(items.collect::<Vec<_>>());
    let body = if hang {
        reqwest::Body::wrap_stream(s.chain(futures_util::stream::pending()))
    } else {
        reqwest::Body::wrap_stream(s)
    };
    reqwest::Response::from(http::Response::new(body))
}

fn bytes_of(s: &'static str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

#[tokio::test]
async fn streaming_driver() {
    use futures_util::StreamExt;
    let hello = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n";
    let stop =
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    // openai-chat :: finishes at the done sentinel without waiting for response EOF
    let events: Vec<_> = crate::drive(
        response(vec![hello, stop], false, true),
        openai::Decode::default(),
    )
    .collect()
    .await;
    assert!(
        matches!(events.last(), Some(Ok(Event::Finish { .. }))),
        "{events:?}"
    );

    // openai-chat :: surfaces transport errors that occur mid-stream
    let events: Vec<_> = crate::drive(
        response(vec![hello], true, false),
        openai::Decode::default(),
    )
    .collect()
    .await;
    assert!(matches!(&events[0], Ok(Event::TextDelta(t)) if t == "Hello"));
    assert!(
        matches!(events.last(), Some(Err(Error::Transport(_)))),
        "{events:?}"
    );
    assert!(events.last().unwrap().as_ref().unwrap_err().retryable());

    // openai-chat :: surfaces transport errors before the first stream frame
    let events: Vec<_> = crate::drive(response(vec![], true, false), openai::Decode::default())
        .collect()
        .await;
    assert!(
        matches!(events.as_slice(), [Err(Error::Transport(_))]),
        "{events:?}"
    );

    // openai-chat :: short-circuits the upstream stream when the consumer takes a prefix
    let first: Vec<_> = crate::drive(
        response(vec![hello], false, true),
        openai::Decode::default(),
    )
    .take(1)
    .collect()
    .await;
    assert!(matches!(first.as_slice(), [Ok(Event::TextDelta(_))]));

    // A response cut off before its terminal event is a transport error
    // (anthropic-messages :: requires message_stop ...), and a final frame
    // without its blank line still counts (openai-responses).
    let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n";
    let events: Vec<_> = crate::drive(
        response(vec![start], false, false),
        anthropic::Decode::default(),
    )
    .collect()
    .await;
    assert!(matches!(events.last(), Some(Err(Error::Transport(_)))));
    let done = "data: {\"type\":\"response.completed\",\"response\":{}}";
    let events: Vec<_> = crate::drive(
        response(vec![done], false, false),
        responses::Decode::default(),
    )
    .collect()
    .await;
    assert!(
        matches!(events.as_slice(), [Ok(Event::Finish { .. })]),
        "{events:?}"
    );
}
