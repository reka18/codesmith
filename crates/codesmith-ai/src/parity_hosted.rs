//! Parity with OpenCode's hosted (provider-executed) tool tests: the hosted
//! tool cases of `provider/openai-responses.test.ts` and
//! `provider/anthropic-messages.test.ts` (MIT, copyright the OpenCode
//! authors).
//!
//! OpenCode stores a hosted call and its result as two parts with
//! `providerExecuted` and keys their replay state by provider; CodeSmith
//! stores one [`Part::Hosted`] naming the protocol that ran it. Its OpenCode
//! `storage` variants all lower the same way here (CodeSmith's Responses
//! requests are stateless), and cases about results from another provider
//! use a hosted part from another protocol ("adapted").

use super::*;
use crate::{collect, Hosted, HostedResult, Options};

fn hosted(protocol: Protocol, id: &str, name: &str, input: Value, value: Value) -> Part {
    Part::Hosted(Hosted {
        id: id.into(),
        name: name.into(),
        input,
        protocol,
        result: Some(HostedResult {
            is_error: false,
            value,
            media: vec![],
        }),
    })
}

fn responses_hosted(id: &str, name: &str, value: Value) -> Part {
    hosted(
        Protocol::OpenaiResponses,
        id,
        name,
        json!({"query": "effect 4"}),
        value,
    )
}

fn image(protocol: Protocol) -> Part {
    Part::Hosted(Hosted {
        id: "ig_1".into(),
        name: "image_generation".into(),
        input: json!({}),
        protocol,
        result: Some(HostedResult {
            is_error: false,
            value: json!({"type": "image_generation_call", "id": "ig_1", "status": "completed"}),
            media: vec![Media::base64("image/png", "AQID")],
        }),
    })
}

fn with_options(mut r: Request, options: Value) -> Request {
    r.options = Options::from_value(options);
    r
}

fn user_text(t: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": t}]})
}

fn user_image(url: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_image", "image_url": url}]})
}

/// A portable result's text, parsed back (JSON key order is not kept).
/// Adapted: CodeSmith wraps it as untrusted tool output, which OpenCode
/// does not; the envelope is checked and taken off.
fn portable(item: &Value) -> Value {
    let text = item["content"][0]["text"].as_str().expect("portable text");
    let open = format!("<{} tool=\"", crate::hosted::ENVELOPE_TAG);
    assert!(text.starts_with(&open), "{text}");
    assert!(text.contains("untrusted tool output"), "{text}");
    let body = text.split_once('\n').unwrap().1.split_once('\n').unwrap().1;
    let body = body
        .strip_suffix(&format!("\n</{}>", crate::hosted::ENVELOPE_TAG))
        .expect("closed envelope");
    serde_json::from_str(body).expect("portable JSON")
}

/// The hosted calls and results of a stream.
fn hosted_events(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| matches!(e, Event::HostedCall(_) | Event::HostedResult(_)))
        .cloned()
        .collect()
}

fn call_of(protocol: Protocol, id: &str, name: &str, input: Value) -> Event {
    Event::HostedCall(Hosted {
        id: id.into(),
        name: name.into(),
        input,
        protocol,
        result: None,
    })
}

fn result_of(protocol: Protocol, id: &str, name: &str, input: Value, value: Value) -> Event {
    Event::HostedResult(Hosted {
        id: id.into(),
        name: name.into(),
        input,
        protocol,
        result: Some(HostedResult {
            is_error: false,
            value,
            media: vec![],
        }),
    })
}

fn item_done(item: Value) -> Value {
    json!({"type": "response.output_item.done", "item": item})
}

fn completed_usage() -> Value {
    json!({"type": "response.completed", "response": {"usage": {"input_tokens": 5, "output_tokens": 1}}})
}

#[test]
fn openai_responses_hosted_tools() {
    run(&[
        ("openai-responses :: lowers the hosted OpenAI image generation tool", || {
            let mut r = with_options(
                req("gpt-4.1-mini", vec![user("Show me a rooftop garden.")]),
                json!({"hostedTools": [{"type": "image_generation", "action": "generate", "quality": "high", "size": "1024x1024"}]}),
            );
            r.tool_choice = Some(ToolChoice::Tool("image_generation".into()));
            check(Protocol::OpenaiResponses, &r).unwrap();
            let b = responses::body(&r);
            assert_eq!(
                b["tools"],
                json!([{"type": "image_generation", "action": "generate", "quality": "high", "size": "1024x1024"}])
            );
            assert_eq!(b["tool_choice"], json!({"type": "image_generation"}));
        }),
        ("openai-responses :: rejects invalid hosted image generation options locally", || {
            let r = with_options(
                req("gpt-4.1-mini", vec![user("Show me a rooftop garden.")]),
                json!({"hostedTools": [{"type": "image_generation", "outputCompression": -1, "partialImages": 4, "size": "bogus"}]}),
            );
            match check(Protocol::OpenaiResponses, &r) {
                Err(Error::Invalid(m)) => {
                    assert!(m.contains("image generation tool options are invalid"), "{m}")
                }
                other => panic!("{other:?}"),
            }
        }),
        ("openai-responses :: replays complete hosted tool items when storage is enabled", || {
            let item = json!({"type": "web_search_call", "id": "ws_1", "status": "completed"});
            let r = with_options(
                req(
                    "gpt-4.1-mini",
                    vec![
                        asst(vec![responses_hosted("ws_1", "web_search", item.clone())]),
                        user("Continue."),
                    ],
                ),
                json!({"store": true}),
            );
            assert_eq!(responses::body(&r)["input"], json!([item, user_text("Continue.")]));
        }),
        ("openai-responses :: replays stateless hosted tool results as native provider items", || {
            let item = json!({"type": "web_search_call", "id": "ws_1", "status": "completed"});
            let r = with_options(
                req(
                    "gpt-4.1-mini",
                    vec![
                        user("Search."),
                        asst(vec![responses_hosted("ws_1", "web_search", item.clone())]),
                        user("Continue."),
                    ],
                ),
                json!({"store": false}),
            );
            assert_eq!(
                responses::body(&r)["input"],
                json!([user_text("Search."), item, user_text("Continue.")])
            );
        }),
        ("openai-responses :: replays OpenAI hosted tool extensions but rejects foreign and unknown items (adapted: the rejected ones wrapped as untrusted tool output)", || {
            let items = [
                json!({"type": "computer_call", "id": "computer_1", "status": "completed", "action": {"type": "click", "x": 1, "y": 2}}),
                json!({"type": "x_search_call", "id": "x_search_1", "status": "completed"}),
                json!({"type": "future_call", "id": "future_1", "status": "completed"}),
            ];
            let r = req(
                "gpt-4.1-mini",
                items
                    .iter()
                    .map(|item| {
                        let id = item["id"].as_str().unwrap();
                        let name = item["type"].as_str().unwrap();
                        asst(vec![responses_hosted(id, name, item.clone())])
                    })
                    .collect(),
            );
            let input = responses::body(&r)["input"].clone();
            assert_eq!(input[0], items[0]);
            assert_eq!(portable(&input[1]), items[1]);
            assert_eq!(portable(&input[2]), items[2]);
            assert_eq!(input.as_array().unwrap().len(), 3);
        }),
        ("openai-responses :: preserves foreign hosted tool results as portable message content when storage is enabled (adapted: a result from Anthropic Messages, wrapped as untrusted tool output)", || {
            let item = json!({"type": "web_search_call", "id": "ws_1", "status": "completed"});
            let r = with_options(
                req(
                    "gpt-4.1-mini",
                    vec![
                        asst(vec![hosted(Protocol::AnthropicMessages, "ws_1", "web_search", json!({"query": "effect 4"}), item.clone())]),
                        user("Continue."),
                    ],
                ),
                json!({"store": true}),
            );
            let input = responses::body(&r)["input"].clone();
            assert_eq!(input[0]["role"], "user");
            assert_eq!(portable(&input[0]), item);
            assert_eq!(input[1], user_text("Continue."));
        }),
        ("openai-responses :: does not replay hosted tool items whose result id differs from provider metadata (adapted: wrapped as untrusted tool output)", || {
            let item = json!({"type": "web_search_call", "id": "ws_other", "status": "completed"});
            let r = req("gpt-4.1-mini", vec![asst(vec![responses_hosted("ws_1", "web_search", item.clone())])]);
            let input = responses::body(&r)["input"].clone();
            assert_eq!(input.as_array().unwrap().len(), 1);
            assert_eq!(portable(&input[0]), item);
        }),
        ("openai-responses :: falls back to portable hosted results when stored item metadata is malformed (adapted: wrapped as untrusted tool output)", || {
            let item = |id: &str| json!({"type": "web_search_call", "id": id, "status": "completed"});
            let r = with_options(
                req(
                    "gpt-4.1-mini",
                    vec![
                        asst(vec![responses_hosted("ws_1", "web_search", item("ws_1"))]),
                        asst(vec![responses_hosted("bad ref", "web_search", item("bad ref"))]),
                    ],
                ),
                json!({"store": true}),
            );
            let input = responses::body(&r)["input"].clone();
            assert_eq!(input[0], item("ws_1"));
            assert_eq!(portable(&input[1]), item("bad ref"));
            assert_eq!(input.as_array().unwrap().len(), 2);
        }),
        ("openai-responses :: continues stateless hosted image generation with the generated image", || {
            let r = with_options(
                req(
                    "gpt-4.1-mini",
                    vec![
                        user("Generate a black triangle."),
                        asst(vec![image(Protocol::OpenaiResponses)]),
                        user("Make it blue."),
                    ],
                ),
                json!({"hostedTools": [{"type": "image_generation", "action": "edit"}]}),
            );
            let b = responses::body(&r);
            assert_eq!(b["store"], false);
            assert_eq!(
                b["input"],
                json!([
                    user_text("Generate a black triangle."),
                    user_image("data:image/png;base64,AQID"),
                    user_text("Make it blue."),
                ])
            );
        }),
        ("openai-responses :: preserves foreign hosted images as portable image content when storage is enabled (adapted: an image from Gemini)", || {
            let r = with_options(
                req("gpt-4.1-mini", vec![asst(vec![image(Protocol::GoogleGemini)])]),
                json!({"store": true}),
            );
            assert_eq!(
                responses::body(&r)["input"],
                json!([user_image("data:image/png;base64,AQID")])
            );
        }),
        ("openai-responses :: decodes web_search_call as provider-executed tool-call + tool-result", || {
            let item = json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "effect 4"}});
            let events = resp(&sse(&[
                json!({"type": "response.output_item.added", "item": item}),
                item_done(item.clone()),
                completed_usage(),
            ]))
            .unwrap();
            let input = json!({"type": "search", "query": "effect 4"});
            assert_eq!(
                hosted_events(&events),
                vec![
                    call_of(Protocol::OpenaiResponses, "ws_1", "web_search", input.clone()),
                    result_of(Protocol::OpenaiResponses, "ws_1", "web_search", input, item),
                ]
            );
            // Hosted calls are not the model's to run: the turn is done.
            assert_eq!(out(Ok(events)).finish, "stop");
        }),
        ("openai-responses :: decodes computer_call as provider-executed tool-call + tool-result", || {
            let item = json!({"type": "computer_call", "id": "computer_1", "call_id": "call_1", "status": "completed", "action": {"type": "click", "x": 100, "y": 200}});
            let events = resp(&sse(&[item_done(item.clone()), completed_usage()])).unwrap();
            let input = json!({"type": "click", "x": 100, "y": 200});
            assert_eq!(
                hosted_events(&events),
                vec![
                    call_of(Protocol::OpenaiResponses, "computer_1", "computer_use", input.clone()),
                    result_of(Protocol::OpenaiResponses, "computer_1", "computer_use", input, item),
                ]
            );
        }),
        ("openai-responses :: replays hosted image results as portable content regardless of storage", || {
            let item = json!({"type": "image_generation_call", "id": "ig_1", "status": "completed", "result": "AQID", "action": "generate", "output_format": "png"});
            let events = resp(&sse(&[item_done(item), completed_usage()])).unwrap();
            let parts = collect(&events);
            let [Part::Hosted(h)] = parts.as_slice() else {
                panic!("{parts:?}")
            };
            assert_eq!((h.id.as_str(), h.name.as_str()), ("ig_1", "image_generation"));
            let r = h.result.as_ref().unwrap();
            assert_eq!(r.media, vec![Media::base64("image/png", "AQID")]);
            assert!(!r.is_error);
            for store in [false, true] {
                let r = with_options(req("gpt-4.1-mini", vec![asst(parts.clone())]), json!({"store": store}));
                assert_eq!(
                    responses::body(&r)["input"],
                    json!([user_image("data:image/png;base64,AQID")])
                );
            }
        }),
        ("openai-responses :: preserves failed hosted tool results as portable error content (adapted: wrapped as untrusted tool output)", || {
            let item = json!({"type": "web_search_call", "id": "ws_failed", "status": "failed", "error": {"code": "search_failed", "message": "Search unavailable"}});
            let events = resp(&sse(&[item_done(item.clone()), completed()])).unwrap();
            let parts = collect(&events);
            let [Part::Hosted(h)] = parts.as_slice() else {
                panic!("{parts:?}")
            };
            let r = h.result.as_ref().unwrap();
            assert!(r.is_error);
            assert_eq!(r.value, item["error"]);
            assert_eq!(h.id, "ws_failed");
            let r = with_options(req("gpt-4.1-mini", vec![asst(parts.clone())]), json!({"store": true}));
            let input = responses::body(&r)["input"].clone();
            assert_eq!(input.as_array().unwrap().len(), 1);
            assert_eq!(input[0]["role"], "user");
            assert_eq!(portable(&input[0]), item["error"]);
        }),
        ("openai-responses :: rejects malformed image generation base64", || {
            let err = resp(&sse(&[
                item_done(json!({"type": "image_generation_call", "id": "ig_bad", "status": "completed", "result": "%%%"})),
                json!({"type": "response.completed", "response": {}}),
            ]))
            .unwrap_err();
            assert!(
                matches!(&err, Error::Decode(m) if m.contains("invalid image base64")),
                "{err:?}"
            );
        }),
        ("openai-responses :: decodes code_interpreter_call as provider-executed events with code input", || {
            let item = json!({"type": "code_interpreter_call", "id": "ci_1", "status": "completed", "code": "print(1+1)", "container_id": "cnt_xyz", "outputs": [{"type": "logs", "logs": "2\n"}]});
            let events = resp(&sse(&[item_done(item.clone()), completed_usage()])).unwrap();
            let input = json!({"code": "print(1+1)", "container_id": "cnt_xyz"});
            assert_eq!(
                hosted_events(&events),
                vec![
                    call_of(Protocol::OpenaiResponses, "ci_1", "code_interpreter", input.clone()),
                    result_of(Protocol::OpenaiResponses, "ci_1", "code_interpreter", input, item),
                ]
            );
        }),
    ]);
}

#[test]
fn anthropic_messages_hosted_tools() {
    run(&[
        ("anthropic-messages :: keeps malformed server tool input terminal", || {
            let err = anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "server_tool_use", "id": "call_1", "name": "web_search"})),
                block_delta(0, json!({"type": "input_json_delta", "partial_json": "{\"query\":\"partial"})),
                block_stop(0),
            ]))
            .unwrap_err();
            assert!(
                err.to_string()
                    .contains("Invalid JSON input for Anthropic Messages tool call web_search"),
                "{err}"
            );
        }),
        ("anthropic-messages :: decodes server_tool_use + web_search_tool_result as provider-executed events", || {
            let content = json!([{"type": "web_search_result", "url": "https://example.com", "title": "Example"}]);
            let events = anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "server_tool_use", "id": "srvtoolu_abc", "name": "web_search"})),
                block_delta(0, json!({"type": "input_json_delta", "partial_json": "{\"query\":\"effect 4\"}"})),
                block_stop(0),
                block_start(1, json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_abc", "content": content})),
                block_stop(1),
                block_start(2, json!({"type": "text", "text": ""})),
                block_delta(2, json!({"type": "text_delta", "text": "Found it."})),
                block_stop(2),
                message_delta("end_turn", 8),
                stop(),
            ]))
            .unwrap();
            let a = Protocol::AnthropicMessages;
            assert_eq!(
                hosted_events(&events),
                vec![
                    call_of(a, "srvtoolu_abc", "web_search", json!({"query": "effect 4"})),
                    result_of(a, "srvtoolu_abc", "web_search", json!({}), content.clone()),
                ]
            );
            // Stored as one part: the call with its result.
            assert_eq!(
                collect(&events),
                vec![
                    hosted(a, "srvtoolu_abc", "web_search", json!({"query": "effect 4"}), content),
                    text("Found it."),
                ]
            );
            let o = out(Ok(events));
            assert_eq!(o.text, "Found it.");
            assert_eq!(o.reason, FinishReason::new(Reason::Stop, "end_turn"));
            assert!(o.calls.is_empty());
        }),
        ("anthropic-messages :: decodes web_search_tool_result_error as provider-executed error result", || {
            let events = anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "server_tool_use", "id": "srvtoolu_x", "name": "web_search"})),
                block_delta(0, json!({"type": "input_json_delta", "partial_json": "{\"query\":\"q\"}"})),
                block_stop(0),
                block_start(1, json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_x", "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}})),
                block_stop(1),
                message_delta("end_turn", 1),
                stop(),
            ]))
            .unwrap();
            let results: Vec<&Hosted> = events
                .iter()
                .filter_map(|e| match e {
                    Event::HostedResult(h) => Some(h),
                    _ => None,
                })
                .collect();
            let [h] = results.as_slice() else {
                panic!("{events:?}")
            };
            assert_eq!((h.id.as_str(), h.name.as_str()), ("srvtoolu_x", "web_search"));
            assert!(h.result.as_ref().unwrap().is_error);
            assert_eq!(h.summary().as_deref(), Some("max_uses_exceeded"));
        }),
        ("anthropic-messages :: round-trips provider-executed assistant content into server tool blocks", || {
            let r = off(req(
                "claude-sonnet-4-5",
                vec![
                    user("Search for something."),
                    asst(vec![
                        hosted(
                            Protocol::AnthropicMessages,
                            "srvtoolu.abc",
                            "web_search",
                            json!({"query": "effect 4"}),
                            json!([{"url": "https://example.com"}]),
                        ),
                        text("Found it."),
                    ]),
                    user("Thanks."),
                ],
            ));
            check(Protocol::AnthropicMessages, &r).unwrap();
            assert_eq!(
                anthropic::body(&r)["messages"],
                json!([
                    {"role": "user", "content": [{"type": "text", "text": "Search for something."}]},
                    {"role": "assistant", "content": [
                        {"type": "server_tool_use", "id": "srvtoolu_abc", "name": "web_search", "input": {"query": "effect 4"}},
                        {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_abc", "content": [{"url": "https://example.com"}]},
                        {"type": "text", "text": "Found it."},
                    ]},
                    {"role": "user", "content": [{"type": "text", "text": "Thanks."}]},
                ])
            );
        }),
        ("anthropic-messages :: rejects round-trip for unknown server tool names", || {
            let r = req(
                "claude-sonnet-4-5",
                vec![asst(vec![hosted(
                    Protocol::AnthropicMessages,
                    "srvtoolu_abc",
                    "future_server_tool",
                    json!({}),
                    json!({}),
                )])],
            );
            match check(Protocol::AnthropicMessages, &r) {
                Err(Error::Invalid(m)) => assert!(m.contains("future_server_tool"), "{m}"),
                other => panic!("{other:?}"),
            }
        }),
    ]);
}

/// CodeSmith's own cases: declaring hosted tools from config, Gemini code
/// execution and grounding, and parts from other protocols.
#[test]
fn hosted_tools_beyond_opencode() {
    run(&[
        ("declares Anthropic server tools with their betas", || {
            let r = with_options(
                off(req("claude-sonnet-4-5", vec![user("hi")])),
                json!({"hostedTools": ["web_search", "web_fetch", {"type": "code_execution_20250522", "name": "code_execution"}, {"type": "web_search_20250305", "name": "web_search", "maxUses": 3}]}),
            );
            check(Protocol::AnthropicMessages, &r).unwrap();
            let b = anthropic::body(&r);
            assert_eq!(
                b["tools"],
                json!([
                    {"type": "web_search_20250305", "name": "web_search"},
                    {"type": "web_fetch_20250910", "name": "web_fetch"},
                    {"type": "code_execution_20250522", "name": "code_execution"},
                    {"type": "web_search_20250305", "name": "web_search", "max_uses": 3},
                ])
            );
            let betas = anthropic::betas(&b);
            assert!(
                betas.contains(&"web-fetch-2025-09-10".to_string()),
                "{betas:?}"
            );
            assert!(
                betas.contains(&"code-execution-2025-05-22".to_string()),
                "{betas:?}"
            );
            let bad = with_options(
                req("claude-sonnet-4-5", vec![user("hi")]),
                json!({"hostedTools": ["bogus"]}),
            );
            assert!(matches!(
                check(Protocol::AnthropicMessages, &bad),
                Err(Error::Invalid(_))
            ));
        }),
        (
            "declares Responses hosted tools beside function tools",
            || {
                let mut r = with_options(
                    req("gpt-5", vec![user("hi")]),
                    json!({"hosted_tools": ["web_search", "code_interpreter"]}),
                );
                r.tools = vec![tool("read", json!({"type": "object"}))];
                let b = responses::body(&r);
                assert_eq!(b["tools"][0]["type"], "function");
                assert_eq!(b["tools"][1], json!({"type": "web_search"}));
                assert_eq!(
                    b["tools"][2],
                    json!({"type": "code_interpreter", "container": {"type": "auto"}})
                );
                // Chat has no hosted tools.
                assert!(chat_at(OPENAI, &r)["tools"].as_array().unwrap().len() == 1);
            },
        ),
        (
            "declares, decodes and replays Gemini code execution and search grounding",
            || {
                let mut r = with_options(
                    req("gemini-3-pro", vec![user("hi")]),
                    json!({"hostedTools": ["google_search", "code_execution"]}),
                );
                assert_eq!(
                    gemini::body(&r)["tools"],
                    json!([{"googleSearch": {}}, {"codeExecution": {}}])
                );
                r.tools = vec![tool("read", json!({"type": "object"}))];
                let tools = gemini::body(&r)["tools"].clone();
                assert!(tools[0].get("functionDeclarations").is_some());
                assert_eq!(tools[1], json!({"googleSearch": {}}));

                let events = gem(&sse(&[json!({"candidates": [{
                "content": {"role": "model", "parts": [
                    {"executableCode": {"language": "PYTHON", "code": "print(1+1)"}},
                    {"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "2\n"}},
                    {"text": "It is 2."},
                ]},
                "groundingMetadata": {"webSearchQueries": ["one plus one"], "groundingChunks": [{"web": {"uri": "https://example.com"}}]},
                "finishReason": "STOP",
            }]})]))
            .unwrap();
                let parts = collect(&events);
                let [Part::Hosted(code), Part::Text(t), Part::Hosted(search)] = parts.as_slice()
                else {
                    panic!("{parts:?}")
                };
                assert_eq!(t, "It is 2.");
                assert_eq!(
                    code.input,
                    json!({"language": "PYTHON", "code": "print(1+1)"})
                );
                assert_eq!(code.summary().as_deref(), Some("2"));
                assert_eq!(search.name, "google_search");
                assert_eq!(search.input, json!({"queries": ["one plus one"]}));
                assert_eq!(search.summary().as_deref(), Some("1 result"));
                assert_eq!(out(Ok(events)).finish, "stop");

                let r = req(
                    "gemini-3-pro",
                    vec![user("hi"), asst(parts.clone()), user("more")],
                );
                assert_eq!(
                    gemini::body(&r)["contents"][1]["parts"],
                    json!([
                        {"executableCode": {"language": "PYTHON", "code": "print(1+1)"}},
                        {"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "2\n"}},
                        {"text": "It is 2."},
                    ])
                );
            },
        ),
        (
            "leaves hosted parts of other protocols out of Anthropic, Gemini and Chat requests",
            || {
                let item = json!({"type": "web_search_call", "id": "ws_1", "status": "completed"});
                let r = req(
                    "m",
                    vec![
                        user("hi"),
                        asst(vec![
                            responses_hosted("ws_1", "web_search", item),
                            text("ok"),
                        ]),
                        user("more"),
                    ],
                );
                check(Protocol::AnthropicMessages, &r).unwrap();
                assert_eq!(
                    anthropic::body(&r)["messages"][1]["content"],
                    json!([{"type": "text", "text": "ok"}])
                );
                assert_eq!(
                    gemini::body(&r)["contents"][1]["parts"],
                    json!([{"text": "ok"}])
                );
                assert_eq!(chat_at(OPENAI, &r)["messages"][1]["content"], "ok");
            },
        ),
    ]);
}
