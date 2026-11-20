//! Parity with OpenCode's endpoint, auth, billing, compatibility override
//! and request encoding tests: those cases of `provider/openai-chat.test.ts`,
//! `provider/openai-compatible-chat.test.ts`, `provider/openai-responses.test.ts`,
//! `provider/anthropic-messages.test.ts`, `provider/gemini.test.ts` and
//! `request-encoding.test.ts` (MIT, copyright the OpenCode authors).
//!
//! Requests are the real HTTP requests `Provider::prepare` builds (URL,
//! headers and encoded body), without sending them.

use super::*;
use crate::{azure_endpoint, Auth, Compatibility, Encoding, Provider};

fn provider(protocol: Protocol, base_url: &str, key: &str) -> Provider {
    Provider {
        protocol,
        base_url: base_url.into(),
        api_key: Some(key.into()),
        ..Default::default()
    }
}

fn prepare(p: &Provider, r: &Request) -> reqwest::Request {
    p.prepare(&reqwest::Client::new(), r)
        .expect("request builds")
}

fn header<'a>(r: &'a reqwest::Request, name: &str) -> Option<&'a str> {
    r.headers().get(name).map(|v| v.to_str().unwrap())
}

fn sent_body(r: &reqwest::Request) -> &[u8] {
    r.body().and_then(|b| b.as_bytes()).expect("buffered body")
}

fn json_body(r: &reqwest::Request) -> Value {
    serde_json::from_slice(sent_body(r)).expect("JSON body")
}

/// Azure configured as OpenCode's `Azure.configure({baseURL, apiKey,
/// headers: {authorization: "Bearer stale"}})`.
fn azure(protocol: Protocol, model: &str) -> Provider {
    let (base_url, query) = azure_endpoint(AZURE, model, None, &[], false);
    Provider {
        protocol,
        base_url,
        api_key: Some("azure-key".into()),
        headers: vec![("authorization".into(), "Bearer stale".into())],
        query,
        auth: Auth::Header("api-key".into()),
        ..Default::default()
    }
}

fn api_version(v: &str) -> Vec<(String, String)> {
    vec![("api-version".into(), v.into())]
}

#[test]
fn endpoint_and_auth() {
    run(&[
        ("openai-chat :: adds native query params to the Chat Completions URL", || {
            let mut p = provider(Protocol::OpenaiChat, "https://api.openai.test/v1/", "test");
            p.query = api_version("v1");
            let r = prepare(&p, &hello("gpt-4o-mini"));
            assert_eq!(r.url().as_str(), "https://api.openai.test/v1/chat/completions?api-version=v1");
        }),
        ("openai-chat :: uses Azure api-key header for static OpenAI Chat keys", || {
            let r = prepare(&azure(Protocol::OpenaiChat, "gpt-4o-mini"), &hello("gpt-4o-mini"));
            assert_eq!(
                r.url().as_str(),
                "https://opencode-test.openai.azure.com/openai/v1/chat/completions?api-version=v1"
            );
            assert_eq!(header(&r, "api-key"), Some("azure-key"));
            assert_eq!(header(&r, "authorization"), None);
        }),
        ("openai-compatible-chat :: posts to the configured compatible endpoint and parses text usage (adapted: no total token count)", || {
            let mut p = provider(Protocol::OpenaiChat, "https://api.deepseek.test/v1/", "test-key");
            p.query = api_version("2026-01-01");
            let mut hello = hello("deepseek-chat");
            hello.max_tokens = None;
            hello.temperature = None;
            let r = prepare(&p, &hello);
            assert_eq!(
                r.url().as_str(),
                "https://api.deepseek.test/v1/chat/completions?api-version=2026-01-01"
            );
            assert_eq!(header(&r, "authorization"), Some("Bearer test-key"));
            assert_eq!(header(&r, "content-type"), Some("application/json"));
            let b = json_body(&r);
            assert_eq!(b["model"], "deepseek-chat");
            assert_eq!(b["stream"], true);
            assert_eq!(
                b["messages"],
                json!([
                    {"role": "system", "content": "You are concise."},
                    {"role": "user", "content": "Say hello."},
                ])
            );
            let o = out(chat(&sse_done(&[
                delta(json!({"role": "assistant", "content": "Hello"})),
                delta(json!({"content": "!"})),
                delta_finish(json!({}), "stop"),
                json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}),
            ])));
            assert_eq!(o.text, "Hello!");
            assert_eq!(o.usage, usage(5, 2, 0, 0));
            assert_eq!(o.finish, "stop");
        }),
        ("openai-responses :: adds native query params to the Responses URL", || {
            let mut p = provider(Protocol::OpenaiResponses, "https://api.openai.test/v1/", "test");
            p.query = api_version("v1");
            let r = prepare(&p, &hello("gpt-4.1-mini"));
            assert_eq!(r.url().as_str(), "https://api.openai.test/v1/responses?api-version=v1");
        }),
        ("openai-responses :: uses Azure api-key header for static OpenAI Responses keys", || {
            let r = prepare(&azure(Protocol::OpenaiResponses, "gpt-4.1-mini"), &hello("gpt-4.1-mini"));
            assert_eq!(
                r.url().as_str(),
                "https://opencode-test.openai.azure.com/openai/v1/responses?api-version=v1"
            );
            assert_eq!(header(&r, "api-key"), Some("azure-key"));
            assert_eq!(header(&r, "authorization"), None);
        }),
    ]);
}

/// Azure's URL rules beyond the ported cases (OpenCode's `endpoint`).
#[test]
fn azure_urls() {
    let q = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    assert_eq!(
        azure_endpoint(AZURE, "m", Some("2025-04-01-preview"), &[], false),
        (
            "https://opencode-test.openai.azure.com/openai/v1".into(),
            q(&[("api-version", "2025-04-01-preview")])
        )
    );
    assert_eq!(
        azure_endpoint(
            AZURE,
            "dep",
            None,
            &q(&[("api-version", "x"), ("a", "b")]),
            true
        ),
        (
            "https://opencode-test.openai.azure.com/openai/deployments/dep".into(),
            q(&[("api-version", "x"), ("a", "b")])
        )
    );
    // A proxy in front of Azure gets only the configured parameters.
    assert_eq!(
        azure_endpoint(
            "https://proxy.test/azure/",
            "m",
            None,
            &q(&[("a", "b")]),
            false
        ),
        ("https://proxy.test/azure".into(), q(&[("a", "b")]))
    );
    assert_eq!(
        crate::azure_resource_url(" res ", Protocol::OpenaiResponses),
        "https://res.openai.azure.com/openai"
    );
    // Gemini keeps its own query beside the configured parameters.
    let mut p = provider(Protocol::GoogleGemini, "https://g.test/v1beta", "k");
    p.query = vec![("x".into(), "1".into())];
    let r = prepare(&p, &hello("gemini-x"));
    assert_eq!(
        r.url().as_str(),
        "https://g.test/v1beta/models/gemini-x:streamGenerateContent?alt=sse&x=1"
    );
    assert_eq!(header(&r, "x-goog-api-key"), Some("k"));
    // Anthropic keeps its version header with a custom key header.
    let mut p = provider(Protocol::AnthropicMessages, "https://a.test/v1", "k");
    p.auth = Auth::Header("api-key".into());
    let r = prepare(&p, &hello("claude-x"));
    assert_eq!(header(&r, "api-key"), Some("k"));
    assert_eq!(header(&r, "x-api-key"), None);
    assert!(header(&r, "anthropic-version").is_some());
}

fn billed(o: &Out) -> Option<f64> {
    o.usage.cost
}

const COPILOT_NANO_AIU: u64 = 4_473_525_000;
const COPILOT_USD: f64 = 0.04473525;

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-12)
}

#[test]
fn billed_cost() {
    run(&[
        (
            "openai-chat :: reports the cost the endpoint billed",
            || {
                let o = out(chat(&sse_done(&[
                    delta_finish(json!({"content": "Hello"}), "stop"),
                    json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2, "cost": 0, "opencode": {"allowance": 0.0042}}}),
                ])));
                assert_eq!(
                    (o.usage.input, o.usage.output, billed(&o)),
                    (5, 2, Some(0.0))
                );
            },
        ),
        (
            "openai-chat :: converts the GitHub Copilot billing beside usage to USD",
            || {
                let o = out(chat(&sse_done(&[
                    delta_finish(json!({"content": "Hello"}), "stop"),
                    json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2}, "copilot_usage": {"total_nano_aiu": COPILOT_NANO_AIU, "token_details": []}}),
                ])));
                assert_eq!((o.usage.input, o.usage.output), (5, 2));
                assert!(close(billed(&o), COPILOT_USD), "{:?}", billed(&o));
            },
        ),
        (
            "openai-chat :: drops a malformed billed cost without failing the stream",
            || {
                let o = out(chat(&sse_done(&[
                    delta_finish(json!({"content": "Hello"}), "stop"),
                    json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2, "cost": -1, "opencode": {"allowance": "lots"}}}),
                ])));
                assert_eq!(o.text, "Hello");
                assert_eq!((o.usage.input, o.usage.output, billed(&o)), (5, 2, None));
            },
        ),
        (
            "openai-responses :: reports the cost the endpoint billed",
            || {
                let o = out(resp(&sse(&[
                    json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
                    json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Hello"}),
                    json!({"type": "response.completed", "response": {"id": "resp_1", "usage": {"input_tokens": 5, "output_tokens": 2, "cost": 0, "opencode": {"allowance": 0.0042}}}}),
                ])));
                assert_eq!(
                    (o.usage.input, o.usage.output, billed(&o)),
                    (5, 2, Some(0.0))
                );
            },
        ),
        (
            "anthropic-messages :: reports the cost the endpoint billed",
            || {
                let o = out(anth(&sse(&[
                    json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2, "cost": 0, "opencode": {"allowance": 0.0042}}}),
                    json!({"type": "message_stop"}),
                ])));
                assert_eq!(
                    (o.usage.input, o.usage.output, billed(&o)),
                    (5, 2, Some(0.0))
                );
            },
        ),
        (
            "anthropic-messages :: converts the GitHub Copilot billing on the message delta to USD",
            || {
                let o = out(anth(&sse(&[
                    json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}, "copilot_usage": {"total_nano_aiu": COPILOT_NANO_AIU}}),
                    json!({"type": "message_stop"}),
                ])));
                assert_eq!((o.usage.input, o.usage.output), (5, 2));
                assert!(close(billed(&o), COPILOT_USD), "{:?}", billed(&o));
            },
        ),
        ("gemini :: reports the cost the endpoint billed", || {
            let o = out(gem(&sse(&[json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "Hello"}]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 5, "toolUsePromptTokenCount": 3, "candidatesTokenCount": 2, "cost": 0, "opencode": {"allowance": 0.0042}},
            })])));
            assert_eq!(
                (o.usage.input, o.usage.output, billed(&o)),
                (8, 2, Some(0.0))
            );
        }),
    ]);
}

#[test]
fn compatibility_overrides() {
    run(&[
        (
            "openai-chat :: omits stream usage options when compatibility disables them",
            || {
                let mut p = provider(Protocol::OpenaiChat, "https://api.openai.test/v1/", "test");
                let r = req("gpt-4o-mini", vec![user("Hello")]);
                assert_eq!(
                    json_body(&prepare(&p, &r))["stream_options"],
                    json!({"include_usage": true})
                );
                p.compat = Compatibility {
                    stream_usage: Some(false),
                    ..Default::default()
                };
                let b = json_body(&prepare(&p, &r));
                assert!(b.get("stream_options").is_none(), "{b}");
            },
        ),
        (
            "openai-compatible-chat :: configures the max tokens request field",
            || {
                let mut p = provider(Protocol::OpenaiChat, COMPATIBLE, "k");
                p.compat = Compatibility {
                    max_tokens_field: Some("max_completion_tokens".into()),
                    ..Default::default()
                };
                let mut r = req("custom-model", vec![user("Say hello.")]);
                r.max_tokens = Some(20);
                let b = json_body(&prepare(&p, &r));
                assert_eq!(b["max_completion_tokens"], 20);
                assert!(b.get("max_tokens").is_none(), "{b}");
            },
        ),
    ]);
}

/// The other overrides: reasoning fields and the Mistral assistant bridge,
/// either way round.
#[test]
fn more_compatibility_overrides() {
    let history = vec![
        user("Hi"),
        asst(vec![call("call_1", "lookup", json!({}))]),
        results(vec![result("call_1", "lookup", "ok")]),
        user("Thanks"),
    ];
    let mut p = provider(Protocol::OpenaiChat, COMPATIBLE, "k");
    let r = req("devstral-small", history.clone());
    let roles = |b: &Value| -> Vec<String> {
        b["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        roles(&json_body(&prepare(&p, &r))),
        ["user", "assistant", "tool", "assistant", "user"]
    );
    p.compat.require_assistant_after_tool = Some(false);
    assert_eq!(
        roles(&json_body(&prepare(&p, &r))),
        ["user", "assistant", "tool", "user"]
    );
    let r = req("custom-model", history);
    p.compat.require_assistant_after_tool = Some(true);
    p.compat.require_reasoning = Some(true);
    let b = json_body(&prepare(&p, &r));
    assert_eq!(
        roles(&b),
        ["user", "assistant", "tool", "assistant", "user"]
    );
    assert_eq!(b["messages"][1]["reasoning_content"], "");
    // DeepSeek is detected; the override turns it off.
    let mut p = provider(Protocol::OpenaiChat, "https://api.deepseek.com/v1", "k");
    let r = req(
        "deepseek-chat",
        vec![user("Hi"), asst(vec![text("Hello")]), user("Again")],
    );
    assert_eq!(
        json_body(&prepare(&p, &r))["messages"][1]["reasoning_content"],
        ""
    );
    p.compat.require_reasoning = Some(false);
    let b = json_body(&prepare(&p, &r));
    assert!(b["messages"][1].get("reasoning_content").is_none(), "{b}");
    // And OpenAI's field goes back to `max_tokens` on request.
    let mut p = provider(Protocol::OpenaiChat, OPENAI, "k");
    p.compat.max_tokens_field = Some("max_tokens".into());
    let b = json_body(&prepare(&p, &hello("gpt-4o-mini")));
    assert_eq!(b["max_tokens"], 20);
    assert!(b.get("max_completion_tokens").is_none(), "{b}");
}

fn encoded(encoding: Encoding) -> reqwest::Request {
    let mut p = provider(Protocol::OpenaiChat, "https://api.openai.test/v1/", "test");
    p.encoding = encoding;
    prepare(&p, &req("gpt-4.1-mini", vec![user("Hello")]))
}

#[test]
fn request_encoding() {
    run(&[
        (
            "request-encoding :: compresses the JSON body with the requested encoding",
            || {
                let r = encoded(Encoding::Zstd);
                assert_eq!(header(&r, "content-encoding"), Some("zstd"));
                let plain = zstd::stream::decode_all(sent_body(&r)).unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&plain).unwrap()["model"],
                    "gpt-4.1-mini"
                );
                let r = encoded(Encoding::Gzip);
                assert_eq!(header(&r, "content-encoding"), Some("gzip"));
                let mut plain = Vec::new();
                std::io::Read::read_to_end(
                    &mut flate2::read::GzDecoder::new(sent_body(&r)),
                    &mut plain,
                )
                .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&plain).unwrap()["model"],
                    "gpt-4.1-mini"
                );
            },
        ),
        (
            "request-encoding :: sends plain JSON when unset or none",
            || {
                for r in [encoded(Encoding::default()), encoded(Encoding::None)] {
                    assert_eq!(header(&r, "content-encoding"), None);
                    assert_eq!(json_body(&r)["model"], "gpt-4.1-mini");
                }
            },
        ),
    ]);
}
