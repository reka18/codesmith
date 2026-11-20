//! Parity with OpenCode's tests for provider state the model declares or
//! signs: the "Configured Chat reasoning fields", "Signed text and signature
//! overrides" and "Tool namespaces" groups of `PARITY.md` (MIT, copyright
//! the OpenCode authors). Each case is named after the OpenCode test it
//! ports.

use super::*;
use crate::{collect, Compatibility, Provider, Reasoning, ReasoningMeta};

fn chat_provider(compat: Compatibility) -> Provider {
    Provider {
        base_url: COMPATIBLE.into(),
        compat,
        ..Default::default()
    }
}

fn chat_for(p: &Provider, r: &Request) -> Value {
    openai::body_for(r, &openai::Compat::for_provider(p, &r.model))
}

fn field(f: &str) -> Compatibility {
    Compatibility {
        reasoning_field: Some(f.into()),
        ..Default::default()
    }
}

fn chat_reasoning(text: &str, field: Option<&str>, details: Option<Value>) -> Part {
    let mut m = ReasoningMeta::of(Protocol::OpenaiChat);
    m.field = field.map(str::to_string);
    m.details = details.map(|d| d.as_array().unwrap().clone());
    Part::Reasoning(Reasoning::new(text, m))
}

fn signed_details() -> Value {
    json!([{"type": "reasoning.text", "text": "thinking", "signature": "signed", "format": "anthropic-claude-v1", "index": 0}])
}

#[test]
fn configured_chat_reasoning_fields() {
    run(&[
        ("openai-chat :: writes reasoning to a configured custom field on every assistant message", || {
            let p = chat_provider(field("vendor_reasoning"));
            let r = req(
                "gpt-4o-mini",
                vec![
                    asst(vec![chat_reasoning("thinking", Some("reasoning"), None), text("Hello")]),
                    asst(vec![text("Done")]),
                ],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([
                    {"role": "assistant", "content": "Hello", "vendor_reasoning": "thinking"},
                    {"role": "assistant", "content": "Done", "vendor_reasoning": ""},
                ])
            );
        }),
        ("openai-chat :: preserves observed reasoning fields when reasoning is required", || {
            let p = chat_provider(Compatibility {
                require_reasoning: Some(true),
                ..Default::default()
            });
            let r = req(
                "gpt-4o-mini",
                vec![
                    asst(vec![chat_reasoning("thinking", Some("reasoning_text"), None), text("Hello")]),
                    asst(vec![text("Done")]),
                ],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([
                    {"role": "assistant", "content": "Hello", "reasoning_text": "thinking"},
                    {"role": "assistant", "content": "Done", "reasoning_content": ""},
                ])
            );
        }),
        ("openai-chat :: omits empty configured reasoning fields when reasoning is explicitly optional", || {
            let p = chat_provider(Compatibility {
                reasoning_field: Some("reasoning_text".into()),
                require_reasoning: Some(false),
                ..Default::default()
            });
            let r = req(
                "gpt-4o-mini",
                vec![asst(vec![thinking("thinking"), text("Hello")]), asst(vec![text("Done")])],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([
                    {"role": "assistant", "content": "Hello", "reasoning_text": "thinking"},
                    {"role": "assistant", "content": "Done"},
                ])
            );
        }),
        ("openai-chat :: rejects reasoning fields that conflict with assistant message fields", || {
            let p = chat_provider(field("content"));
            let r = req("gpt-4o-mini", vec![asst(vec![thinking("thinking")])]);
            match p.prepare(&reqwest::Client::new(), &r) {
                Err(Error::Invalid(m)) => assert!(m.contains("reserved field content"), "{m}"),
                other => panic!("expected an invalid request, got {other:?}"),
            }
        }),
        ("openai-chat :: orders metadata-only reasoning before refusal output", || {
            let evs = chat(&sse(&[
                json!({"choices": [{"delta": {"reasoning_details": []}}]}),
                delta(json!({"refusal": "I can't help with that."})),
                delta_finish(json!({}), "stop"),
            ]))
            .unwrap();
            let mut meta = ReasoningMeta::of(Protocol::OpenaiChat);
            meta.details = Some(vec![]);
            assert_eq!(
                collect(&evs),
                [Part::Reasoning(Reasoning::new("", meta)), text("I can't help with that.")]
            );
        }),
        ("openai-chat :: does not replay reasoning details when the model declares another reasoning field", || {
            let p = chat_provider(field("reasoning_content"));
            let r = req(
                "gpt-4o-mini",
                vec![asst(vec![
                    chat_reasoning("thinking", Some("reasoning"), Some(signed_details())),
                    text("Hello"),
                ])],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello", "reasoning_content": "thinking"}])
            );
        }),
        ("openai-chat :: keeps replayed reasoning details when the model declares reasoning_details", || {
            let p = chat_provider(field("reasoning_details"));
            let r = req(
                "gpt-4o-mini",
                vec![asst(vec![
                    chat_reasoning("thinking", Some("reasoning"), Some(signed_details())),
                    text("Hello"),
                ])],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello", "reasoning_details": signed_details()}])
            );
        }),
        ("openai-chat :: parses and replays a configured custom reasoning field", || {
            let p = chat_provider(field("vendor_reasoning"));
            let compat = openai::Compat::for_provider(&p, "gpt-4o-mini");
            let evs = decode_all(
                openai::Decode::new(&compat),
                &sse(&[
                    json!({"choices": [{"delta": {"vendor_reasoning": "thinking"}}]}),
                    json!({"choices": [{"delta": {"content": "Hello"}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                ]),
            )
            .unwrap();
            assert_eq!(out(Ok(evs.clone())).reasoning, "thinking");
            let parts = collect(&evs);
            assert_eq!(
                parts[0],
                chat_reasoning("thinking", Some("vendor_reasoning"), None)
            );
            let r = req("gpt-4o-mini", vec![asst(parts)]);
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello", "vendor_reasoning": "thinking"}])
            );
        }),
    ]);
}

fn anthropic_signed(text: &str, sig: &str) -> Part {
    let mut m = ReasoningMeta::of(Protocol::AnthropicMessages);
    m.signature = Some(sig.into());
    Part::Reasoning(Reasoning::new(text, m))
}

fn gemini_signed_text(t: &str, sig: &str) -> Part {
    let mut m = ReasoningMeta::of(Protocol::GoogleGemini);
    m.signature = Some(sig.into());
    Part::Text(crate::Text::new(t, m))
}

#[test]
fn signed_text_and_signature_overrides() {
    run(&[
        ("anthropic-messages :: prepares the composed native continuation request (adapted)", || {
            // (adapted) OpenCode's continuation scenario with the features
            // Anthropic replays natively; the tool result is its JSON text,
            // and without caching the system prompt is a plain string
            // (OpenCode sends one text block).
            let mut r = req(
                "claude-sonnet-4-5",
                vec![
                    results(vec![text("What is shown here?"), media(png("AAECAw=="))]),
                    asst(vec![
                        anthropic_signed("I inspected the previous turn.", "sig_continuation_1"),
                        text("It shows a small test image."),
                    ]),
                    user("Check the weather in Paris before continuing."),
                    asst(vec![call("call_weather_1", "get_weather", json!({"city": "Paris"}))]),
                    results(vec![result("call_weather_1", "get_weather", r#"{"temperature":22}"#)]),
                    asst(vec![text("Paris is 22 degrees.")]),
                    user("Continue from this conversation in one short sentence."),
                ],
            );
            r.system = "You are concise. Continue from the provided history.".into();
            r.tools = vec![ToolSpec {
                name: "get_weather".into(),
                description: "Get current weather for a city.".into(),
                parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}),
            }];
            r.max_tokens = Some(80);
            r.temperature = Some(0.0);
            let b = anthropic::body(&off(r));
            assert_eq!(b["system"], "You are concise. Continue from the provided history.");
            assert_eq!(
                b["messages"],
                json!([
                    {"role": "user", "content": [
                        {"type": "text", "text": "What is shown here?"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAECAw=="}},
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "I inspected the previous turn.", "signature": "sig_continuation_1"},
                        {"type": "text", "text": "It shows a small test image."},
                    ]},
                    {"role": "user", "content": [{"type": "text", "text": "Check the weather in Paris before continuing."}]},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "call_weather_1", "name": "get_weather", "input": {"city": "Paris"}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_weather_1", "content": r#"{"temperature":22}"#}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "Paris is 22 degrees."}]},
                    {"role": "user", "content": [{"type": "text", "text": "Continue from this conversation in one short sentence."}]},
                ])
            );
            assert_eq!(b["tools"].as_array().unwrap().len(), 1);
            assert_eq!(b["tools"][0]["name"], "get_weather");
        }),
        ("anthropic-messages :: lets an explicit signature requirement override inference", || {
            let p = Provider {
                name: "kimi-for-coding".into(),
                base_url: "https://api.kimi.com/coding/v1/".into(),
                compat: Compatibility {
                    require_signature: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            };
            let r = req("k3", vec![asst(vec![thinking("unsigned reasoning")])]);
            assert_eq!(anthropic::body_with(&r, &p)["messages"], json!([]));
            // And the other way: Anthropic itself, told to accept it.
            let p = Provider {
                compat: Compatibility {
                    require_signature: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            };
            let r = req("claude-sonnet-4-5", vec![asst(vec![thinking("unsigned reasoning")])]);
            assert_eq!(
                anthropic::body_with(&r, &p)["messages"],
                json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "unsigned reasoning", "signature": ""}]}])
            );
        }),
        ("anthropic-messages :: round-trips compatible provider metadata in its own namespace (adapted: no stop sequence)", || {
            // (adapted) CodeSmith has one provider per stream, so its state
            // is not keyed by a namespace, and the stop sequence is not
            // kept.
            let result = json!([{"type": "web_search_result", "url": "https://example.com",
                "citations": [{"type": "web_search_result_location", "cited_text": "Example"}]}]);
            let evs = anth(&sse(&[
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 5, "custom_start": true}}}),
                block_start(0, json!({"type": "thinking", "thinking": "Thinking."})),
                block_delta(0, json!({"type": "signature_delta", "signature": "custom_sig"})),
                block_stop(0),
                block_start(1, json!({"type": "redacted_thinking", "data": "custom_redacted"})),
                block_stop(1),
                block_start(2, json!({"type": "server_tool_use", "id": "custom_tool", "name": "web_search", "input": {"query": "example"}})),
                block_stop(2),
                block_start(3, json!({"type": "web_search_tool_result", "tool_use_id": "custom_tool", "content": result})),
                block_stop(3),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": "custom_stop"}, "usage": {"output_tokens": 2, "custom_terminal": true}}),
                stop(),
            ]))
            .unwrap();
            let mut redacted = ReasoningMeta::of(Protocol::AnthropicMessages);
            redacted.redacted = Some("custom_redacted".into());
            let parts = collect(&evs);
            assert_eq!(
                parts,
                [
                    anthropic_signed("Thinking.", "custom_sig"),
                    Part::Reasoning(Reasoning::new("", redacted)),
                    Part::Hosted(crate::Hosted {
                        id: "custom_tool".into(),
                        name: "web_search".into(),
                        input: json!({"query": "example"}),
                        protocol: Protocol::AnthropicMessages,
                        result: Some(crate::HostedResult {
                            is_error: false,
                            value: result.clone(),
                            media: vec![],
                        }),
                    }),
                ]
            );
            let o = out(Ok(evs));
            let raw = o.raw_usage.expect("usage payload");
            assert_eq!(raw["custom_start"], true);
            assert_eq!(raw["custom_terminal"], true);
            let r = off(req("custom-model", vec![asst(parts)]));
            assert_eq!(
                anthropic::body_for(&r, "https://compatible.test/v1")["messages"],
                json!([{"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "Thinking.", "signature": "custom_sig"},
                    {"type": "redacted_thinking", "data": "custom_redacted"},
                    {"type": "server_tool_use", "id": "custom_tool", "name": "web_search", "input": {"query": "example"}},
                    {"type": "web_search_tool_result", "tool_use_id": "custom_tool", "content": result},
                ]}])
            );
        }),
        ("gemini :: preserves thoughtSignature on visible text parts", || {
            let evs = gem(&sse(&[candidate(
                json!([{"text": "All done.", "thoughtSignature": "text_sig"}]),
                Some("STOP"),
            )]))
            .unwrap();
            let parts = collect(&evs);
            assert_eq!(parts, [gemini_signed_text("All done.", "text_sig")]);
            assert_eq!(
                gemini::body(&req("gemini-2.5-flash", vec![asst(parts)]))["contents"],
                json!([{"role": "model", "parts": [{"text": "All done.", "thoughtSignature": "text_sig"}]}])
            );
        }),
        ("gemini :: flushes a trailing empty signed text part at block close", || {
            let evs = gem(&sse(&[candidate(
                json!([{"text": "Working."}, {"text": "", "thoughtSignature": "tail_sig"}]),
                Some("STOP"),
            )]))
            .unwrap();
            let at = evs
                .iter()
                .position(|e| matches!(e, Event::TextEnd(id) if id == "text-0"))
                .expect("text end");
            assert_eq!(
                evs[..at].iter().filter(|e| matches!(e, Event::TextDelta(_))).collect::<Vec<_>>(),
                [&Event::TextDelta("Working.".into())]
            );
            let mut meta = ReasoningMeta::of(Protocol::GoogleGemini);
            meta.signature = Some("tail_sig".into());
            assert_eq!(evs[at + 1], Event::PartMeta(meta));
            assert_eq!(collect(&evs), [gemini_signed_text("Working.", "tail_sig")]);
            assert_eq!(
                gemini::body(&req("gemini-2.5-flash", vec![asst(collect(&evs))]))["contents"],
                json!([{"role": "model", "parts": [{"text": "Working.", "thoughtSignature": "tail_sig"}]}])
            );
        }),
    ]);
}
