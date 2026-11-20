//! Amazon Bedrock Converse: request lowering, the event-stream decoder and
//! SigV4 over the wire (after OpenCode's `bedrock-converse.test.ts`). The
//! recorded streams in `fixtures/bedrock` come from OpenCode's recordings.

use base64::Engine;
use codesmith_ai::bedrock::{self, HeaderValue, Message as Frame};
use codesmith_ai::*;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn recorded(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/bedrock/{name}.b64",
        env!("CARGO_MANIFEST_DIR")
    );
    base64::engine::general_purpose::STANDARD
        .decode(std::fs::read_to_string(path).unwrap().trim())
        .unwrap()
}

fn event(kind: &str, payload: Value) -> Vec<u8> {
    Frame {
        headers: vec![
            (":event-type".into(), HeaderValue::String(kind.into())),
            (
                ":content-type".into(),
                HeaderValue::String("application/json".into()),
            ),
            (":message-type".into(), HeaderValue::String("event".into())),
        ],
        payload: payload.to_string().into_bytes(),
    }
    .encode()
}

fn exception(kind: &str, payload: Value) -> Vec<u8> {
    Frame {
        headers: vec![
            (":exception-type".into(), HeaderValue::String(kind.into())),
            (
                ":message-type".into(),
                HeaderValue::String("exception".into()),
            ),
        ],
        payload: payload.to_string().into_bytes(),
    }
    .encode()
}

fn stream(frames: &[Vec<u8>]) -> Vec<u8> {
    frames.concat()
}

fn stop(reason: &str) -> Vec<u8> {
    event("messageStop", json!({"stopReason": reason}))
}

fn request(model: &str) -> Request {
    Request {
        model: model.into(),
        messages: vec![Message::user("Say hello.")],
        ..Default::default()
    }
}

fn weather() -> ToolSpec {
    ToolSpec {
        name: "get_weather".into(),
        description: "Get current weather for a city.".into(),
        parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}),
    }
}

#[test]
fn oc_bedrock_prepares_the_recorded_text_request() {
    let mut req = request("us.amazon.nova-micro-v1:0");
    req.system = "Reply with the single word 'Hello'.".into();
    req.max_tokens = Some(16);
    req.temperature = Some(0.0);
    assert_eq!(
        bedrock::body(&req).unwrap(),
        json!({"modelId":"us.amazon.nova-micro-v1:0","messages":[{"role":"user","content":[{"text":"Say hello."}]}],"system":[{"text":"Reply with the single word 'Hello'."}],"inferenceConfig":{"maxTokens":16,"temperature":0.0}})
    );
    assert_eq!(
        bedrock::path(&req.model),
        "model/us.amazon.nova-micro-v1%3A0/converse-stream"
    );
}

#[test]
fn oc_bedrock_prepares_tool_config_with_tool_spec_and_tool_choice() {
    let mut req = request("us.amazon.nova-micro-v1:0");
    req.tools = vec![weather()];
    req.tool_choice = Some(ToolChoice::Tool("get_weather".into()));
    let b = bedrock::body(&req).unwrap();
    assert_eq!(
        b["toolConfig"],
        json!({"tools":[{"toolSpec":{"name":"get_weather","description":"Get current weather for a city.","inputSchema":{"json":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}}}],"toolChoice":{"tool":{"name":"get_weather"}}})
    );
    // `none` keeps the tools and leaves out the choice; blank descriptions go.
    req.tool_choice = Some(ToolChoice::None);
    req.tools[0].description = "  ".into();
    let b = bedrock::body(&req).unwrap();
    assert!(b["toolConfig"].get("toolChoice").is_none());
    assert!(b["toolConfig"]["tools"][0]["toolSpec"]
        .get("description")
        .is_none());
    req.tool_choice = Some(ToolChoice::Required);
    assert_eq!(
        bedrock::body(&req).unwrap()["toolConfig"]["toolChoice"],
        json!({"any": {}})
    );
}

#[test]
fn oc_bedrock_lowers_tool_history_and_merges_parallel_results() {
    let mut req = request("us.amazon.nova-micro-v1:0");
    req.messages = vec![
        Message::user("Compare the weather in Paris and London."),
        Message {
            role: Role::Assistant,
            parts: vec![
                Part::tool_call("weather_paris", "get_weather", json!({"city": "Paris"})),
                Part::tool_call(
                    "weather_london",
                    "weather.get",
                    json!({"city": "London", "": 1, "nested": {"": 2, "k": 3}}),
                ),
            ],
        },
        Message {
            role: Role::User,
            parts: vec![
                Part::ToolResult {
                    id: "weather_paris".into(),
                    name: "get_weather".into(),
                    content: "sunny".into(),
                    is_error: false,
                    media: vec![],
                },
                Part::ToolResult {
                    id: "weather_london".into(),
                    name: "weather.get".into(),
                    content: "failed".into(),
                    is_error: true,
                    media: vec![],
                },
            ],
        },
    ];
    let b = bedrock::body(&req).unwrap();
    assert_eq!(
        b["messages"][1]["content"][1],
        json!({"toolUse": {"toolUseId": "weather_london", "name": "weather_get", "input": {"city": "London", "nested": {"k": 3}}}})
    );
    let results = b["messages"][2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[1],
        json!({"toolResult": {"toolUseId": "weather_london", "content": [{"text": "failed"}], "status": "error"}})
    );
}

fn image_result(model: &str) -> Value {
    let mut req = request(model);
    req.messages = vec![
        Message::user("look"),
        Message {
            role: Role::Assistant,
            parts: vec![Part::tool_call("c1", "read", json!({}))],
        },
        Message {
            role: Role::User,
            parts: vec![Part::ToolResult {
                id: "c1".into(),
                name: "read".into(),
                content: String::new(),
                is_error: false,
                media: vec![Media::base64("image/png", "aGk=")],
            }],
        },
    ];
    req.cache = Cache::off();
    bedrock::body(&req).unwrap()["messages"][2]["content"].clone()
}

#[test]
fn oc_bedrock_hoists_tool_result_images_except_for_claude_and_nova() {
    let image = json!({"image": {"format": "png", "source": {"bytes": "aGk="}}});
    assert_eq!(
        image_result("openai.gpt-oss-120b-1:0"),
        json!([
            {"toolResult": {"toolUseId": "c1", "content": [{"text": "See attached image."}], "status": "success"}},
            image.clone(),
        ])
    );
    for model in [
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "amazon.nova-pro-v1:0",
    ] {
        assert_eq!(
            image_result(model),
            json!([{"toolResult": {"toolUseId": "c1", "content": [image.clone()], "status": "success"}}]),
            "{model}"
        );
    }
}

#[test]
fn oc_bedrock_lowers_documents_with_unique_labels_and_rejects_unknown_media() {
    let mut req = request("amazon.nova-pro-v1:0");
    req.messages = vec![Message {
        role: Role::User,
        parts: vec![
            Part::Media(Media::base64("application/pdf", "aGk=").named("report.pdf")),
            Part::Media(Media::base64("application/pdf", "aGk=").named("report.v2.pdf")),
            Part::Media(Media::base64("text/plain", "aGk=").named("a_b.txt")),
        ],
    }];
    let c = bedrock::body(&req).unwrap()["messages"][0]["content"].clone();
    // Each renamed file is announced before its document.
    assert_eq!(
        c[1],
        json!({"document": {"format": "pdf", "name": "report", "source": {"bytes": "aGk="}}})
    );
    assert_eq!(
        c[2],
        json!({"text": "Attached file \"report.v2.pdf\" has document label \"report v2\"."})
    );
    assert_eq!(c[3]["document"]["name"], "report v2");
    assert_eq!(c[5]["document"]["name"], "a b");
    for (mime, why) in [
        (
            "image/svg+xml",
            "does not support image media type image/svg+xml",
        ),
        ("audio/wav", "does not support media type audio/wav"),
    ] {
        req.messages = vec![Message {
            role: Role::User,
            parts: vec![Part::Media(Media::base64(mime, "aGk="))],
        }];
        let e = Provider {
            protocol: Protocol::BedrockConverse,
            base_url: "http://127.0.0.1:1".into(),
            ..Default::default()
        }
        .prepare(&reqwest::Client::new(), &req)
        .unwrap_err()
        .to_string();
        assert!(e.contains(why), "{e}");
    }
    req.messages = vec![Message {
        role: Role::User,
        parts: vec![Part::Media(Media::url("image/png", "https://x/y.png"))],
    }];
    assert!(bedrock::body(&req)
        .unwrap_err()
        .to_string()
        .contains("requires inline media"));
}

#[test]
fn oc_bedrock_passes_top_k_effort_and_fits_thinking_budgets() {
    let mut req = request("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    req.max_tokens = Some(4000);
    req.options = Options::from_value(
        json!({"topK": 5, "thinking": {"type": "enabled", "budgetTokens": 9000}}),
    );
    let b = bedrock::body(&req).unwrap();
    assert_eq!(
        b["additionalModelRequestFields"],
        json!({"thinking": {"type": "enabled", "budget_tokens": 2000}, "top_k": 5})
    );
    // Nova 2 at high effort sends no maxTokens.
    let mut req = request("us.amazon.nova-2-lite-v1:0");
    req.max_tokens = Some(4000);
    req.options = Options::from_value(json!({"effort": "high"}));
    let b = bedrock::body(&req).unwrap();
    assert!(b.get("inferenceConfig").is_none(), "{b}");
    assert_eq!(
        b["additionalModelRequestFields"],
        json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": "high"}})
    );
    req.model = "us.xai.grok-4.6".into();
    req.options = Options::from_value(json!({"reasoningEffort": "low"}));
    assert_eq!(
        bedrock::body(&req).unwrap()["additionalModelRequestFields"],
        json!({"reasoning": {"effort": "low"}})
    );
    // No options, no model-specific fields.
    assert!(bedrock::body(&request("amazon.nova-pro-v1:0"))
        .unwrap()
        .get("additionalModelRequestFields")
        .is_none());
}

#[test]
fn oc_bedrock_claude_effort_takes_adaptive_thinking_and_binding() {
    let mut req = request("global.anthropic.claude-opus-5-1");
    req.options = Options::from_value(json!({"effort": "high"}));
    assert_eq!(
        bedrock::body(&req).unwrap()["additionalModelRequestFields"],
        json!({
            "thinking": {"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}},
            "output_config": {"effort": "high"},
            "anthropic_beta": ["thinking-binding-controls-2026-08-01"],
        })
    );
}

#[test]
fn oc_bedrock_lowers_system_updates_to_wrapped_user_text() {
    let mut req = request("amazon.nova-pro-v1:0");
    req.messages.push(Message::system("be <brief>"));
    let b = bedrock::body(&req).unwrap();
    assert_eq!(
        b["messages"],
        json!([{"role": "user", "content": [{"text": "Say hello."}, {"text": "<system-update>\nbe &lt;brief&gt;\n</system-update>"}]}])
    );
}

#[test]
fn oc_bedrock_emits_cache_points_for_claude_only() {
    let mut req = request("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    req.system = "sys".into();
    req.tools = vec![weather()];
    let b = bedrock::body(&req).unwrap();
    let cp = json!({"cachePoint": {"type": "default"}});
    assert_eq!(b["system"][1], cp);
    assert_eq!(b["toolConfig"]["tools"][1], cp);
    assert_eq!(b["messages"][0]["content"][1], cp);
    // A one-hour lifetime, except on releases that cache for five minutes.
    req.cache.ttl = Some("1h".into());
    assert_eq!(
        bedrock::body(&req).unwrap()["system"][1],
        json!({"cachePoint": {"type": "default", "ttl": "1h"}})
    );
    req.model = "anthropic.claude-3-7-sonnet-20250219-v1:0".into();
    assert_eq!(bedrock::body(&req).unwrap()["system"][1], cp);
    // Legacy Claude and other families get none.
    for model in [
        "anthropic.claude-3-haiku-20240307-v1:0",
        "amazon.nova-pro-v1:0",
    ] {
        req.model = model.into();
        let b = bedrock::body(&req).unwrap();
        assert_eq!(b["system"].as_array().unwrap().len(), 1, "{model}");
    }
    req.model = "us.anthropic.claude-sonnet-4-5-20250929-v1:0".into();
    req.cache = Cache::off();
    assert_eq!(
        bedrock::body(&req).unwrap()["system"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn oc_bedrock_drops_unsigned_and_foreign_reasoning() {
    let mut req = request("us.anthropic.claude-sonnet-4-5-20250929-v1:0");
    let mut signed = ReasoningMeta::of(Protocol::BedrockConverse);
    signed.signature = Some("sig".into());
    let mut redacted = ReasoningMeta::of(Protocol::BedrockConverse);
    redacted.redacted = Some("cmVk".into());
    let mut foreign = ReasoningMeta::of(Protocol::AnthropicMessages);
    foreign.signature = Some("other".into());
    req.messages.push(Message {
        role: Role::Assistant,
        parts: vec![
            Part::Reasoning(Reasoning::new("think", signed)),
            Part::Reasoning(Reasoning::new("", redacted)),
            Part::Reasoning(Reasoning::new("foreign", foreign)),
            Part::Reasoning(Reasoning::from("unsigned")),
            Part::text("answer"),
        ],
    });
    req.messages.push(Message::user("more"));
    req.cache = Cache::off();
    assert_eq!(
        bedrock::body(&req).unwrap()["messages"][1]["content"],
        json!([
            {"reasoningContent": {"reasoningText": {"text": "think", "signature": "sig"}}},
            {"reasoningContent": {"redactedContent": "cmVk"}},
            {"text": "answer"},
        ])
    );
}

#[test]
fn oc_bedrock_decodes_the_recorded_text_stream() {
    let events = bedrock::decode_all(&recorded("streams-text")).unwrap();
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Event::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text.trim(), "Hello");
    match events.last().unwrap() {
        Event::Finish { reason, usage, .. } => {
            assert_eq!(reason.normalized, Reason::Stop);
            assert!(usage.input > 0 && usage.output > 0, "{usage:?}");
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn oc_bedrock_decodes_the_recorded_tool_call_stream() {
    let events = bedrock::decode_all(&recorded("streams-a-tool-call")).unwrap();
    let call = events
        .iter()
        .find_map(|e| match e {
            Event::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(call, ("get_weather".into(), json!({"city": "Paris"})));
    match events.last().unwrap() {
        Event::Finish { reason, .. } => assert_eq!(reason.normalized, Reason::ToolCalls),
        e => panic!("{e:?}"),
    }
}

#[test]
fn oc_bedrock_decodes_frames_split_across_chunks_and_rejects_torn_ones() {
    let bytes = stream(&[
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"text": "Hi"}}),
        ),
        event("contentBlockStop", json!({"contentBlockIndex": 0})),
        stop("end_turn"),
    ]);
    let mut framer = bedrock::Framer::default();
    let mut frames = Vec::new();
    for b in &bytes {
        frames.extend(framer.push(std::slice::from_ref(b)).unwrap());
    }
    framer.finish().unwrap();
    assert_eq!(frames.len(), 3);
    let e = bedrock::decode_all(&bytes[..bytes.len() - 3])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("Incomplete Bedrock Converse event-stream frame"),
        "{e}"
    );
    let mut bad = bytes.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    assert!(bedrock::decode_all(&bad)
        .unwrap_err()
        .to_string()
        .contains("checksum mismatch"));
}

#[test]
fn oc_bedrock_maps_stop_reasons_and_cache_usage() {
    let events = bedrock::decode_all(&stream(&[
        event("metadata", json!({"usage": {"inputTokens": 10, "outputTokens": 5, "cacheReadInputTokens": 20, "cacheWriteInputTokens": 30}})),
        stop("model_context_window_exceeded"),
        event("metadata", json!({"metrics": {"latencyMs": 3}})),
    ]))
    .unwrap();
    match events.last().unwrap() {
        Event::Finish { reason, usage, .. } => {
            assert!(reason.is_length());
            assert_eq!(
                (
                    usage.input,
                    usage.cache_read,
                    usage.cache_write,
                    usage.output,
                    usage.total
                ),
                (10, 20, 30, 5, Some(65))
            );
        }
        e => panic!("{e:?}"),
    }
    let e = bedrock::decode_all(&stop("malformed_tool_use"))
        .unwrap_err()
        .to_string();
    assert_eq!(e, "Bedrock Converse stopped with malformed_tool_use");
    // A stream without a stop is incomplete.
    let e = bedrock::decode_all(&event("metadata", json!({"usage": {"inputTokens": 1}})))
        .unwrap_err()
        .to_string();
    assert!(e.contains("ended before messageStop"), "{e}");
}

#[test]
fn oc_bedrock_assembles_streamed_tool_input_and_ignores_orphan_deltas() {
    let events = bedrock::decode_all(&stream(&[
        event("contentBlockDelta", json!({"contentBlockIndex": 3, "delta": {"toolUse": {"input": "{}"}}})),
        event("contentBlockStart", json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "t1", "name": "lookup"}}})),
        event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"q\":"}}})),
        event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "\"x\"}"}}})),
        event("contentBlockStop", json!({"contentBlockIndex": 1})),
        stop("end_turn"),
    ]))
    .unwrap();
    let calls: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::ToolCall { .. }))
        .collect();
    assert_eq!(
        calls,
        [&Event::ToolCall {
            id: "t1".into(),
            name: "lookup".into(),
            input: json!({"q": "x"})
        }]
    );
    // A stop of `end_turn` after a tool call finishes as tool calls.
    assert!(
        matches!(events.last(), Some(Event::Finish { reason, .. }) if reason.normalized == Reason::ToolCalls)
    );
}

#[test]
fn oc_bedrock_decodes_reasoning_signatures_and_redacted_content() {
    let events = bedrock::decode_all(&stream(&[
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "Let me "}}})),
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "think"}}})),
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sig-1"}}})),
        event("contentBlockStop", json!({"contentBlockIndex": 0})),
        event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"reasoningContent": {"redactedContent": "YWJj"}}})),
        event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"reasoningContent": {"redactedContent": "ZGVm"}}})),
        // Index 2 never stops: it settles at the end.
        event("contentBlockDelta", json!({"contentBlockIndex": 2, "delta": {"reasoningContent": {"data": "opaque"}}})),
        event("contentBlockStop", json!({"contentBlockIndex": 1})),
        stop("end_turn"),
    ]))
    .unwrap();
    let parts = collect(&events);
    let metas: Vec<_> = parts
        .iter()
        .filter_map(|p| match p {
            Part::Reasoning(r) => Some((r.text.clone(), r.meta.clone().unwrap())),
            _ => None,
        })
        .collect();
    assert_eq!(metas[0].0, "Let me think");
    assert_eq!(metas[0].1.signature.as_deref(), Some("sig-1"));
    assert_eq!(metas[1].1.redacted.as_deref(), Some("YWJjZGVm"));
    assert_eq!(metas[2].1.redacted.as_deref(), Some("opaque"));
    let e = bedrock::decode_all(&event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"redactedContent": "!!"}}}),
    ))
    .unwrap_err()
    .to_string();
    assert!(e.contains("invalid base64"), "{e}");
}

#[test]
fn oc_bedrock_classifies_stream_exceptions() {
    let e = bedrock::decode_all(&exception(
        "throttlingException",
        json!({"message": "Too many requests"}),
    ))
    .unwrap_err();
    assert_eq!(e.to_string(), "throttlingException: Too many requests");
    assert!(e.retryable());
    let e = bedrock::decode_all(&exception(
        "modelStreamErrorException",
        json!({"originalMessage": "upstream", "originalStatusCode": 400}),
    ))
    .unwrap_err();
    assert_eq!(e.to_string(), "modelStreamErrorException: upstream");
    let e = bedrock::decode_all(&exception(
        "validationException",
        json!({"message": "Input is too long"}),
    ))
    .unwrap_err();
    assert!(!e.retryable());
    let unmodeled = Frame {
        headers: vec![
            (":message-type".into(), HeaderValue::String("error".into())),
            (
                ":error-code".into(),
                HeaderValue::String("InternalFailure".into()),
            ),
            (":error-message".into(), HeaderValue::String("boom".into())),
        ],
        payload: vec![],
    }
    .encode();
    assert_eq!(
        bedrock::decode_all(&unmodeled).unwrap_err().to_string(),
        "InternalFailure: boom"
    );
    // Unknown events are skipped.
    assert!(bedrock::decode_all(&stream(&[
        event("somethingNew", json!({"x": 1})),
        stop("end_turn")
    ]))
    .is_ok());
}

struct Seen {
    head: String,
}

async fn serve(body: Vec<u8>, seen: Arc<Mutex<Vec<Seen>>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut s, _)) = l.accept().await {
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 65536];
            loop {
                let n = s.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(i) = text.find("\r\n\r\n") {
                    let len = text[..i]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            seen.lock().unwrap().push(Seen {
                head: String::from_utf8_lossy(&buf).to_string(),
            });
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.amazon.eventstream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes()).await;
            let _ = s.write_all(&body).await;
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn oc_bedrock_signs_with_static_credentials_in_the_configured_region() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let url = serve(recorded("streams-text"), seen.clone()).await;
    let signer = aws::SigV4::new(
        "bedrock",
        "eu-west-1",
        aws::Source::Static(sigv4::Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "SECRETKEY".into(),
            session_token: Some("SESSION".into()),
        }),
    );
    let p = Provider {
        protocol: Protocol::BedrockConverse,
        name: "amazon-bedrock".into(),
        base_url: url,
        auth: Auth::Dynamic(Arc::new(signer)),
        api_key: Some("ignored-key".into()),
        ..Default::default()
    };
    let mut s = p
        .stream(
            &reqwest::Client::new(),
            &request("us.amazon.nova-micro-v1:0"),
        )
        .await
        .unwrap();
    use futures_util::StreamExt;
    let mut text = String::new();
    while let Some(e) = s.next().await {
        if let Event::TextDelta(t) = e.unwrap() {
            text.push_str(&t);
        }
    }
    assert_eq!(text.trim(), "Hello");
    let head = seen.lock().unwrap()[0].head.to_ascii_lowercase();
    assert!(
        head.starts_with("post /model/us.amazon.nova-micro-v1%3a0/converse-stream "),
        "{head}"
    );
    assert!(
        head.contains("authorization: aws4-hmac-sha256 credential=akidexample/"),
        "{head}"
    );
    assert!(head.contains("/eu-west-1/bedrock/aws4_request"), "{head}");
    assert!(head.contains("x-amz-security-token: session"), "{head}");
    assert!(!head.contains("ignored-key"), "{head}");
    assert!(!head.contains("secretkey"), "{head}");
}

#[tokio::test]
async fn oc_bedrock_sends_an_api_key_as_a_bearer_token() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let url = serve(recorded("streams-text"), seen.clone()).await;
    let p = Provider {
        protocol: Protocol::BedrockConverse,
        base_url: url,
        api_key: Some("bedrock-api-key".into()),
        ..Default::default()
    };
    let mut s = p
        .stream(
            &reqwest::Client::new(),
            &request("us.amazon.nova-micro-v1:0"),
        )
        .await
        .unwrap();
    use futures_util::StreamExt;
    while s.next().await.is_some() {}
    let head = seen.lock().unwrap()[0].head.to_ascii_lowercase();
    assert!(
        head.contains("authorization: bearer bedrock-api-key"),
        "{head}"
    );
    assert!(!head.contains("x-amz-date"), "{head}");
}

#[test]
fn bare_ids_of_profile_only_models_get_an_inference_profile_hint() {
    let e = bedrock::profile_hint(
        Error::Status {
            status: 400,
            body: "Invocation of model ID anthropic.claude-opus-4-7 with on-demand throughput isn't supported.".into(),
        },
        "anthropic.claude-opus-4-7",
    )
    .to_string();
    assert!(e.contains("such as us.anthropic.claude-opus-4-7"), "{e}");
    assert!(bedrock::is_profile("global.anthropic.claude-opus-4-7"));
    assert!(bedrock::is_profile(
        "arn:aws:bedrock:us-east-1:1:inference-profile/x"
    ));
    let e = bedrock::profile_hint(
        Error::Status {
            status: 400,
            body: "on-demand throughput".into(),
        },
        "us.x",
    )
    .to_string();
    assert!(!e.contains("such as"));
}
