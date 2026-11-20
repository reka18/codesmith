//! Parity with OpenCode's provider option and reasoning replay tests: the
//! "Provider options", "Reasoning signatures and redacted thinking",
//! "Responses encrypted reasoning and output-item replay" and "Structured
//! reasoning replay on Chat" groups of `PARITY.md` (MIT, copyright the
//! OpenCode authors). Each case is named after the OpenCode test it ports.
//!
//! A decoded response becomes a message the way a session stores it, with
//! [`crate::collect`]; that message is then lowered again for the
//! continuation request.

use super::*;
use crate::{collect, Options, Provider, Reasoning, ReasoningMeta};

fn opts(v: Value) -> Options {
    Options::from_value(v)
}

fn with(mut r: Request, o: Value) -> Request {
    r.options = opts(o);
    r
}

/// A request with the provider's defaults under its options, as
/// `Provider::stream` sends it.
fn defaulted(protocol: Protocol, url: &str, mut r: Request) -> Request {
    r.options = crate::defaults(protocol, url, &r.model).merged(&r.options);
    r
}

fn events(r: Result<Vec<Event>, Error>) -> Vec<Event> {
    r.expect("stream decodes")
}

fn reasoning_with(text: &str, meta: ReasoningMeta) -> Part {
    Part::Reasoning(Reasoning::new(text, meta))
}

fn signed(text: &str, sig: &str) -> Part {
    let mut m = ReasoningMeta::of(Protocol::AnthropicMessages);
    m.signature = Some(sig.into());
    reasoning_with(text, m)
}

fn redacted(data: &str) -> Part {
    let mut m = ReasoningMeta::of(Protocol::AnthropicMessages);
    m.redacted = Some(data.into());
    reasoning_with("", m)
}

fn rs(text: &str, id: Option<&str>, encrypted: Option<&str>) -> Part {
    let mut m = ReasoningMeta::of(Protocol::OpenaiResponses);
    m.item_id = id.map(str::to_string);
    m.encrypted = encrypted.map(str::to_string);
    reasoning_with(text, m)
}

fn chat_reasoning(text: &str, field: Option<&str>, details: Option<Value>) -> Part {
    let mut m = ReasoningMeta::of(Protocol::OpenaiChat);
    m.field = field.map(str::to_string);
    m.details = details.map(|d| d.as_array().unwrap().clone());
    reasoning_with(text, m)
}

fn anthropic_provider(headers: &[(&str, &str)]) -> Provider {
    Provider {
        protocol: Protocol::AnthropicMessages,
        base_url: "https://api.anthropic.test/v1".into(),
        api_key: None,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn beta(p: &Provider, body: &Value) -> String {
    anthropic::headers(p, body)
        .into_iter()
        .find(|(k, _)| k == "anthropic-beta")
        .map(|(_, v)| v)
        .unwrap_or_default()
}

fn resp_base() -> Request {
    hello("gpt-4.1-mini")
}

fn resp_body(o: Value) -> Value {
    responses::body(&with(resp_base(), o))
}

fn anth_base() -> Request {
    hello("claude-sonnet-4-5")
}

fn gem_base() -> Request {
    hello("gemini-2.5-flash")
}

fn lookup() -> ToolSpec {
    tool("lookup", json!({"type": "object"}))
}

// ---- provider options --------------------------------------------------------

#[test]
fn openai_chat_options() {
    run(&[
        (
            "openai-chat :: maps OpenAI provider options to Chat options",
            || {
                let b = chat_at(
                    OPENAI,
                    &with(
                        req("gpt-4o-mini", vec![user("think")]),
                        json!({"reasoningEffort": "max"}),
                    ),
                );
                assert_eq!(b["store"], false);
                assert_eq!(b["reasoning_effort"], "max");
            },
        ),
        (
            "openai-chat :: keeps valid Chat options when a sibling option is malformed",
            || {
                let b = chat_at(
                    OPENAI,
                    &with(
                        req("gpt-4o-mini", vec![user("think")]),
                        json!({"store": true, "reasoningEffort": "max", "topLogprobs": 25}),
                    ),
                );
                assert_eq!(b["store"], true);
                assert_eq!(b["reasoning_effort"], "max");
            },
        ),
        (
            "openai-chat :: maps shared OpenAI options to Chat fields",
            || {
                let mut r = with(
                    req("gpt-5-nano", vec![user("Hello")]),
                    json!({"store": true, "serviceTier": "flex", "textVerbosity": "low", "parallelToolCalls": false,
                    "metadata": {"source": "test"}, "safetyIdentifier": "user-hash"}),
                );
                r.tools = vec![lookup()];
                let b = chat_at(OPENAI, &r);
                for (k, v) in [
                    ("store", json!(true)),
                    ("service_tier", json!("flex")),
                    ("verbosity", json!("low")),
                    ("parallel_tool_calls", json!(false)),
                    ("metadata", json!({"source": "test"})),
                    ("safety_identifier", json!("user-hash")),
                ] {
                    assert_eq!(b[k], v, "{k}");
                }
                assert!(b.get("text").is_none());
            },
        ),
        (
            "openai-chat :: passes through custom OpenAI-compatible reasoning effort strings",
            || {
                let b = chat_at(
                    COMPATIBLE,
                    &with(
                        req("m", vec![user("think")]),
                        json!({"reasoningEffort": "experimental"}),
                    ),
                );
                assert_eq!(b["reasoning_effort"], "experimental");
            },
        ),
        (
            "openai-compatible-chat :: enables ZAI tool streaming except for GLM 4.5 models",
            || {
                let prepare = |url: &str, id: &str| {
                    let mut r = req(id, vec![user("Use a tool.")]);
                    r.tools = vec![tool("lookup", json!({}))];
                    chat_at(url, &r)
                };
                assert_eq!(
                    prepare("https://api.z.ai/api/paas/v4", "glm-4.7")["tool_stream"],
                    true
                );
                for id in ["glm-4.5", "glm-4.5-air", "glm-4.5-flash", "glm-4.5v"] {
                    assert!(
                        prepare("https://open.bigmodel.cn/api/paas/v4", id)
                            .get("tool_stream")
                            .is_none(),
                        "{id}"
                    );
                }
            },
        ),
    ]);
}

#[test]
fn openai_responses_options() {
    run(&[
        ("openai-responses :: lowers semantic service tier options", || {
            let b = responses::body(&with(resp_base(), json!({"serviceTier": "priority"})));
            assert_eq!(b["service_tier"], "priority");
            assert!(b.get("serviceTier").is_none());
        }),
        ("openai-responses :: passes through custom OpenAI reasoning effort strings", || {
            assert_eq!(resp_body(json!({"reasoningEffort": "experimental"}))["reasoning"], json!({"effort": "experimental"}));
        }),
        ("openai-responses :: lowers reasoning mode and context into the reasoning object", || {
            let b = resp_body(json!({"reasoningEffort": "high", "reasoningMode": "pro", "reasoningContext": "current_turn"}));
            assert_eq!(b["reasoning"], json!({"effort": "high", "mode": "pro", "context": "current_turn"}));
            assert!(b.get("reasoningMode").is_none() && b.get("reasoningContext").is_none());
        }),
        ("openai-responses :: passes through custom OpenAI text verbosity strings", || {
            assert_eq!(resp_body(json!({"textVerbosity": "verbose"}))["text"], json!({"verbosity": "verbose"}));
        }),
        ("openai-responses :: passes through provider-defined and future service tiers", || {
            for tier in ["scale", "ultrafast", "future-tier"] {
                assert_eq!(resp_body(json!({"serviceTier": tier}))["service_tier"], tier);
            }
        }),
        ("openai-responses :: maps OpenAI provider options to Responses options (with the prompt cache key and tool choice)", || {
            let mut r = with(
                req("gpt-5.2", vec![user("think")]),
                json!({"presencePenalty": 0.25, "frequencyPenalty": -0.25,
                    "reasoningEffort": "high", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"],
                    "metadata": {"environment": "test", "tenant": "acme"}, "safetyIdentifier": "user_123",
                    "streamOptions": {"includeObfuscation": false}, "topLogprobs": 5, "truncation": "disabled",
                    "allowedTools": {"toolNames": ["read", "grep"], "mode": "required"}, "maxToolCalls": 4,
                    "parallelToolCalls": false}),
            );
            r.tools = vec![tool("read", json!({"type": "object"})), tool("grep", json!({"type": "object"}))];
            r.prompt_cache_key = Some("session_123".into());
            r.tool_choice = Some(crate::ToolChoice::None);
            let b = responses::body(&r);
            assert_eq!(b["store"], false);
            assert_eq!(b["prompt_cache_key"], "session_123");
            assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
            assert_eq!(b["reasoning"], json!({"effort": "high", "summary": "auto"}));
            assert!(b.get("text").is_none());
            assert_eq!(b["metadata"], json!({"environment": "test", "tenant": "acme"}));
            assert_eq!(b["safety_identifier"], "user_123");
            assert_eq!(b["stream_options"], json!({"include_obfuscation": false}));
            assert_eq!(b["top_logprobs"], 5);
            assert_eq!(b["presence_penalty"], 0.25);
            assert_eq!(b["frequency_penalty"], -0.25);
            assert_eq!(b["truncation"], "disabled");
            assert_eq!(
                b["tool_choice"],
                json!({"type": "allowed_tools", "mode": "required", "tools": [{"type": "function", "name": "read"}, {"type": "function", "name": "grep"}]})
            );
            assert_eq!(b["max_tool_calls"], 4);
            assert_eq!(b["parallel_tool_calls"], false);
        }),
        ("openai-responses :: drops a malformed provider option without discarding its siblings", || {
            let b = resp_body(json!({"topLogprobs": 25, "metadata": {"tenant": 7}, "reasoningEffort": "high", "serviceTier": "priority", "maxToolCalls": 4}));
            assert!(b.get("top_logprobs").is_none());
            assert!(b.get("metadata").is_none());
            assert_eq!(b["reasoning"], json!({"effort": "high"}));
            assert_eq!(b["service_tier"], "priority");
            assert_eq!(b["max_tool_calls"], 4);
        }),
        ("openai-responses :: accepts the full ResponseIncludable union", || {
            let i = json!(["reasoning.encrypted_content", "code_interpreter_call.outputs", "web_search_call.results"]);
            assert_eq!(resp_body(json!({"include": i}))["include"], i);
        }),
        ("openai-responses :: passes forward-compatible includable values through", || {
            let i = json!(["reasoning.encrypted_content", "bogus.thing"]);
            assert_eq!(resp_body(json!({"include": i}))["include"], i);
        }),
        ("openai-responses :: treats an explicit empty include as no include at all", || {
            assert!(resp_body(json!({"include": []})).get("include").is_none());
            let r = defaulted(Protocol::OpenaiResponses, OPENAI, with(resp_base(), json!({"include": []})));
            assert!(responses::body(&r).get("include").is_none());
        }),
        ("openai-responses :: passes an unknown includable value through", || {
            assert_eq!(resp_body(json!({"include": ["bogus.thing"]}))["include"], json!(["bogus.thing"]));
        }),
    ]);
}

#[test]
fn anthropic_messages_options() {
    run(&[
        ("anthropic-messages :: lowers adaptive thinking settings with effort", || {
            let b = anthropic::body(&with(anth_base(), json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "low"})));
            assert_eq!(b["thinking"], json!({"type": "adaptive", "display": "summarized"}));
            assert_eq!(b["output_config"], json!({"effort": "low"}));
        }),
        ("anthropic-messages :: normalizes enabled and disabled thinking settings", || {
            let mut r = with(anth_base(), json!({"thinking": {"type": "enabled", "budgetTokens": 1024}}));
            r.max_tokens = Some(4096);
            assert_eq!(anthropic::body(&r)["thinking"], json!({"type": "enabled", "budget_tokens": 1024}));
            r.options = opts(json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}));
            assert_eq!(anthropic::body(&r)["thinking"], json!({"type": "enabled", "budget_tokens": 2048}));
            let r = with(anth_base(), json!({"thinking": {"type": "disabled"}}));
            assert_eq!(anthropic::body(&r)["thinking"], json!({"type": "disabled"}));
        }),
        ("anthropic-messages :: fits the thinking budget to half the output limit", || {
            let thinking = |max: u32| {
                let mut r = with(anth_base(), json!({"thinking": {"type": "enabled", "budgetTokens": 31999}}));
                r.max_tokens = Some(max);
                anthropic::body(&r)["thinking"].clone()
            };
            assert_eq!(thinking(64_000), json!({"type": "enabled", "budget_tokens": 31999}));
            assert_eq!(thinking(20_000), json!({"type": "enabled", "budget_tokens": 10000}));
            assert_eq!(thinking(1_500), json!({"type": "enabled", "budget_tokens": 1024}));
        }),
        ("anthropic-messages :: rejects enabled thinking without a budget", || {
            let e = anthropic::check(&with(anth_base(), json!({"thinking": {"type": "enabled"}}))).unwrap_err();
            assert!(matches!(e, Error::Invalid(_)));
            assert!(e.to_string().contains("budgetTokens"), "{e}");
        }),
        ("anthropic-messages :: lowers passthrough provider options and accepts either key spelling", || {
            let snake = anthropic::body(&with(anth_base(), json!({"service_tier": "auto", "metadata": {"user_id": "user_1"},
                "container": {"id": "container_1"}, "inference_geo": "us", "cache_control": {"type": "ephemeral", "ttl": "1h"},
                "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}}})));
            let camel = anthropic::body(&with(anth_base(), json!({"serviceTier": "standard_only", "container": "container_2",
                "inferenceGeo": "eu", "cacheControl": {"type": "ephemeral"}, "outputConfig": {"effort": "low"}})));
            for (k, v) in [
                ("service_tier", json!("auto")),
                ("metadata", json!({"user_id": "user_1"})),
                ("container", json!({"id": "container_1"})),
                ("inference_geo", json!("us")),
                ("cache_control", json!({"type": "ephemeral", "ttl": "1h"})),
                ("output_config", json!({"format": {"type": "json_schema", "schema": {"type": "object"}}})),
            ] {
                assert_eq!(snake[k], v, "{k}");
            }
            for (k, v) in [
                ("service_tier", json!("standard_only")),
                ("container", json!("container_2")),
                ("inference_geo", json!("eu")),
                ("cache_control", json!({"type": "ephemeral"})),
                ("output_config", json!({"effort": "low"})),
            ] {
                assert_eq!(camel[k], v, "{k}");
            }
        }),
        ("anthropic-messages :: sends fast mode speed with its beta header", || {
            let b = anthropic::body(&with(anth_base(), json!({"speed": "fast"})));
            assert_eq!(b["speed"], "fast");
            assert_eq!(beta(&anthropic_provider(&[]), &b), "interleaved-thinking-2025-05-14,fast-mode-2026-02-01");
        }),
        ("anthropic-messages :: forwards unknown values for pass-through string enums", || {
            let b = anthropic::body(&with(anth_base(), json!({"service_tier": "future-tier", "thinking": {"type": "adaptive", "display": "future-display"}})));
            assert_eq!(b["service_tier"], "future-tier");
            assert_eq!(b["thinking"], json!({"type": "adaptive", "display": "future-display"}));
        }),
        ("anthropic-messages :: ignores unknown provider options and rejects malformed known ones", || {
            let r = with(anth_base(), json!({"unknownOption": true}));
            assert!(anthropic::check(&r).is_ok());
            assert!(anthropic::body(&r).get("unknownOption").is_none());
            for bad in [
                json!({"service_tier": 1}),
                json!({"metadata": {"user_id": 42}}),
                json!({"cache_control": {"type": "ephemeral", "ttl": "future-ttl"}}),
                json!({"output_config": {"format": {"type": "text"}}}),
                json!({"thinking": {"type": "automatic"}}),
            ] {
                assert!(matches!(anthropic::check(&with(anth_base(), bad.clone())), Err(Error::Invalid(_))), "{bad}");
            }
        }),
        ("anthropic-thinking-binding :: preserves explicit thinking settings and combines required beta headers", || {
            for thinking in [
                json!({"type": "disabled"}),
                json!({"type": "between_tools"}),
                json!({"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "error"}}),
            ] {
                let r = with(
                    req("claude-fable-5-1", vec![user("Hello")]),
                    json!({"thinking": thinking, "contextManagement": {"edits": [{"type": "compact_20260112"}]}}),
                );
                let b = anthropic::body(&r);
                assert_eq!(b["thinking"], thinking);
                let off = thinking["type"] != "adaptive";
                assert_eq!(
                    beta(&anthropic_provider(&[]), &b),
                    if off {
                        "interleaved-thinking-2025-05-14,compact-2026-01-12"
                    } else {
                        "interleaved-thinking-2025-05-14,compact-2026-01-12,thinking-binding-controls-2026-08-01"
                    }
                );
            }
        }),
    ]);
}

#[test]
fn anthropic_thinking_binding_defaults() {
    for (id, enabled) in [
        ("claude-fable-5-1", true),
        ("claude-mythos-5-1", true),
        ("claude-fable-5.1", true),
        ("anthropic/claude-fable-5.1", true),
        ("claude-fable-5-1@default", true),
        ("claude-fable-5-1@20260901", true),
        ("claude-sonnet-6", true),
        ("claude-opus-5-20260901", false),
        ("claude-opus-4-8", false),
        ("anthropic/claude-opus-4.8", false),
        ("claude-fable-5@default", false),
        ("kimi-k2.5", false),
    ] {
        // anthropic-thinking-binding :: thinking-binding defaults for ${id}
        let b = anthropic::body(&req(id, vec![user("Hello")]));
        let p = anthropic_provider(&[("anthropic-beta", "existing-beta")]);
        if enabled {
            assert_eq!(
                b["thinking"],
                json!({"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}}),
                "{id}"
            );
            assert_eq!(
                beta(&p, &b),
                "existing-beta,interleaved-thinking-2025-05-14,thinking-binding-controls-2026-08-01",
                "{id}"
            );
        } else {
            assert!(b.get("thinking").is_none(), "{id}");
            assert_eq!(
                beta(&p, &b),
                "existing-beta,interleaved-thinking-2025-05-14",
                "{id}"
            );
        }
    }
}

#[test]
fn gemini_options() {
    run(&[
        ("gemini :: normalizes Gemini thinking options", || {
            let b = gemini::body(&with(
                gem_base(),
                json!({"cachedContent": "cachedContents/example",
                "safetySettings": [{"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_ONLY_HIGH"}],
                "serviceTier": "priority", "thinkingConfig": {"thinkingBudget": 0, "includeThoughts": false, "thinkingLevel": "high"}}),
            ));
            assert_eq!(
                b["generationConfig"]["thinkingConfig"],
                json!({"thinkingBudget": 0, "includeThoughts": false, "thinkingLevel": "high"})
            );
            assert_eq!(b["cachedContent"], "cachedContents/example");
            assert_eq!(
                b["safetySettings"],
                json!([{"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_ONLY_HIGH"}])
            );
            assert_eq!(b["serviceTier"], "priority");
            let filtered = gemini::body(&with(
                gem_base(),
                json!({"thinkingConfig": {"thinkingBudget": "invalid", "includeThoughts": false}}),
            ));
            assert_eq!(
                filtered["generationConfig"]["thinkingConfig"],
                json!({"includeThoughts": false})
            );
            let defaulted = gemini::body(&with(
                gem_base(),
                json!({"thinkingConfig": {"thinkingLevel": "high"}}),
            ));
            assert_eq!(
                defaulted["generationConfig"]["thinkingConfig"],
                json!({"includeThoughts": true, "thinkingLevel": "high"})
            );
            let empty = gemini::body(&with(gem_base(), json!({"safetySettings": []})));
            assert_eq!(empty["safetySettings"], json!([]));
        }),
        (
            "gemini :: fits the thinking budget to half the output limit",
            || {
                let budget = |b: i64, max: u32| {
                    let mut r = with(gem_base(), json!({"thinkingConfig": {"thinkingBudget": b}}));
                    r.max_tokens = Some(max);
                    gemini::body(&r)["generationConfig"]["thinkingConfig"]["thinkingBudget"].clone()
                };
                assert_eq!(budget(32_768, 32_000), 16_000);
                assert_eq!(budget(8_000, 32_000), 8_000);
                assert_eq!(budget(-1, 32_000), -1);
                assert_eq!(budget(8_192, 1_000), 512);
            },
        ),
        (
            "gemini :: forwards standard Gemini generation options (adapted: knobs as options)",
            || {
                let mut r = with(
                    req("gemini-2.5-flash", vec![user("Say hello.")]),
                    json!({"topP": 0.8, "topK": 12, "frequencyPenalty": 0.3, "presencePenalty": 0.4, "seed": 42, "stop": ["done"]}),
                );
                r.max_tokens = Some(40);
                r.temperature = Some(0.25);
                assert_eq!(
                    gemini::body(&r)["generationConfig"],
                    json!({"maxOutputTokens": 40, "temperature": 0.25, "topP": 0.8, "topK": 12,
                    "frequencyPenalty": 0.3, "presencePenalty": 0.4, "seed": 42, "stopSequences": ["done"]})
                );
            },
        ),
    ]);
}

// ---- reasoning signatures and redacted thinking -----------------------------

/// The continuation request for a decoded Anthropic response.
fn anthropic_continuation(events: &[Event]) -> Value {
    anthropic::body(&req("claude-sonnet-4-5", vec![asst(collect(events))]))["messages"].clone()
}

#[test]
fn anthropic_reasoning_replay() {
    run(&[
        ("anthropic-messages :: lowers preserved Anthropic reasoning signature metadata", || {
            let r = req("claude-sonnet-4-5", vec![asst(vec![signed("thinking", "sig_1")])]);
            assert_eq!(
                anthropic::body(&r)["messages"],
                json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "thinking", "signature": "sig_1"}]}])
            );
        }),
        ("anthropic-messages :: filters whitespace-only text and removes empty messages (signed empty thinking)", || {
            let r = req("claude-sonnet-4-5", vec![asst(vec![thinking("")]), asst(vec![text(""), signed("", "sig_1")])]);
            assert_eq!(
                anthropic::body(&r)["messages"],
                json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": "sig_1"}]}])
            );
        }),
        ("anthropic-messages :: drops unsigned reasoning when signatures are required", || {
            let r = req("claude-sonnet-4-5", vec![asst(vec![thinking("unsigned reasoning")])]);
            assert_eq!(anthropic::body(&r)["messages"], json!([]));
        }),
        ("anthropic-messages :: infers empty-signature compatibility across Kimi providers (by endpoint and model)", || {
            for (url, model) in [
                ("https://api.moonshot.ai/anthropic", "kimi-k2.6"),
                ("https://api.kimi.com/coding/v1/", "k3"),
                ("https://compatible.test/v1/", "qwen3.8-flash"),
                ("https://compatible.test/v1/", "minimax-m3"),
            ] {
                let r = req(model, vec![asst(vec![thinking("unsigned reasoning")])]);
                assert_eq!(
                    anthropic::body_for(&r, url)["messages"],
                    json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "unsigned reasoning", "signature": ""}]}]),
                    "{model}"
                );
            }
        }),
        ("anthropic-messages :: round-trips redacted thinking as redacted_thinking blocks", || {
            let r = req("claude-sonnet-4-5", vec![asst(vec![redacted("opaque_1"), signed("visible", "sig_1")])]);
            assert_eq!(
                anthropic::body(&r)["messages"][0]["content"],
                json!([{"type": "redacted_thinking", "data": "opaque_1"}, {"type": "thinking", "thinking": "visible", "signature": "sig_1"}])
            );
        }),
        ("anthropic-messages :: preserves a reasoning signature when message_stop closes the block", || {
            let evs = events(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "thinking", "thinking": "", "signature": ""})),
                block_delta(0, json!({"type": "thinking_delta", "thinking": "Reasoning."})),
                block_delta(0, json!({"type": "signature_delta", "signature": "sig_1"})),
                message_delta("end_turn", 2),
                stop(),
            ])));
            assert_eq!(evs.iter().filter(|e| matches!(e, Event::ReasoningEnd(_))).count(), 1);
            assert_eq!(collect(&evs), [signed("Reasoning.", "sig_1")]);
            assert_eq!(
                anthropic_continuation(&evs),
                json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "Reasoning.", "signature": "sig_1"}]}])
            );
        }),
        ("anthropic-messages :: round-trips omitted thinking carried only by a signature delta", || {
            let evs = events(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "thinking", "thinking": "", "signature": ""})),
                block_delta(0, json!({"type": "signature_delta", "signature": "sig_1"})),
                block_stop(0),
                message_delta("end_turn", 1),
                stop(),
            ])));
            assert_eq!(collect(&evs), [signed("", "sig_1")]);
            assert_eq!(
                anthropic_continuation(&evs),
                json!([{"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": "sig_1"}]}])
            );
        }),
        ("anthropic-messages :: retains a thinking signature supplied in content_block_start", || {
            let evs = events(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "thinking", "thinking": "", "signature": "sig_1"})),
                block_stop(0),
                message_delta("end_turn", 1),
                stop(),
            ])));
            assert_eq!(collect(&evs), [signed("", "sig_1")]);
        }),
        ("anthropic-messages :: parses redacted thinking into empty reasoning with redactedData metadata", || {
            let evs = events(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "redacted_thinking", "data": "opaque_1"})),
                block_stop(0),
                block_start(1, json!({"type": "text", "text": ""})),
                block_delta(1, json!({"type": "text_delta", "text": "Hello"})),
                block_stop(1),
                message_delta("end_turn", 2),
                stop(),
            ])));
            assert_eq!(collect(&evs), [redacted("opaque_1"), text("Hello")]);
        }),
        ("anthropic-messages :: round-trips streamed redacted thinking with tool use into a continuation request", || {
            let data = "cmVkYWN0ZWQtdGhpbmtpbmc=";
            let evs = events(anth(&sse(&[
                start(5),
                block_start(0, json!({"type": "redacted_thinking", "data": data})),
                block_stop(0),
                block_start(1, json!({"type": "tool_use", "id": "call_1", "name": "lookup"})),
                block_delta(1, json!({"type": "input_json_delta", "partial_json": "{\"query\":\"weather\"}"})),
                block_stop(1),
                message_delta("tool_use", 1),
                stop(),
            ])));
            let mut r = req(
                "claude-sonnet-4-5",
                vec![user("Say hello."), asst(collect(&evs)), results(vec![result("call_1", "lookup", "sunny")])],
            );
            r.tools = vec![lookup()];
            r.cache = crate::Cache::off();
            assert_eq!(
                anthropic::body(&r)["messages"],
                json!([
                    {"role": "user", "content": [{"type": "text", "text": "Say hello."}]},
                    {"role": "assistant", "content": [
                        {"type": "redacted_thinking", "data": data},
                        {"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"query": "weather"}}
                    ]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "sunny"}]}
                ])
            );
        }),
    ]);
}

#[test]
fn gemini_reasoning_replay() {
    run(&[(
        "gemini :: preserves thoughtSignature for reasoning and tool-call continuation",
        || {
            let evs = events(gem(&sse(&[
                json!({"candidates": [{"content": {"role": "model", "parts": [
                {"text": "thinking", "thought": true},
                {"text": "", "thought": true, "thoughtSignature": "thought_sig"},
                {"functionCall": {"id": "provider_call", "name": "lookup", "args": {"query": "weather"}}, "thoughtSignature": "tool_sig"}
            ]}, "finishReason": "STOP"}]}),
            ])));
            let end = evs
                .iter()
                .position(|e| matches!(e, Event::ReasoningEnd(m) if m.signature.as_deref() == Some("thought_sig")))
                .expect("reasoning ends with its signature");
            let call = evs
                .iter()
                .position(|e| matches!(e, Event::ToolCall { .. }))
                .unwrap();
            assert!(end < call);
            let r = req(
                "gemini-3-flash-preview",
                // The session answers the call under its id, signature and all.
                vec![
                    asst(collect(&evs)),
                    results(vec![result("provider_call~tool_sig", "lookup", "done")]),
                ],
            );
            assert_eq!(
                gemini::body(&r)["contents"],
                json!([
                    {"role": "model", "parts": [
                        {"text": "thinking", "thought": true, "thoughtSignature": "thought_sig"},
                        {"functionCall": {"id": "provider_call", "name": "lookup", "args": {"query": "weather"}}, "thoughtSignature": "tool_sig"}
                    ]},
                    {"role": "user", "parts": [{"functionResponse": {"id": "provider_call", "name": "lookup", "response": {"output": "done"}}}]}
                ])
            );
        },
    )]);
}

// ---- Responses encrypted reasoning -------------------------------------------

fn reasoning_ends(evs: &[Event]) -> Vec<&ReasoningMeta> {
    evs.iter()
        .filter_map(|e| match e {
            Event::ReasoningEnd(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn resp_meta(id: Option<&str>, encrypted: Option<&str>) -> ReasoningMeta {
    let mut m = ReasoningMeta::of(Protocol::OpenaiResponses);
    m.item_id = id.map(str::to_string);
    m.encrypted = encrypted.map(str::to_string);
    m
}

#[test]
fn openai_responses_reasoning_replay() {
    run(&[
        ("openai-responses :: requests encrypted reasoning by default (OpenAI endpoint)", || {
            let r = defaulted(Protocol::OpenaiResponses, OPENAI, req("gpt-4.1-mini", vec![user("hi")]));
            assert_eq!(responses::body(&r)["include"], json!(["reasoning.encrypted_content"]));
            let other = defaulted(Protocol::OpenaiResponses, COMPATIBLE, req("gpt-4.1-mini", vec![user("hi")]));
            assert!(responses::body(&other).get("include").is_none());
        }),
        ("openai-responses :: requests encrypted reasoning by default for GPT-5 reasoning models", || {
            let b = responses::body(&defaulted(Protocol::OpenaiResponses, OPENAI, req("gpt-5.2", vec![user("hi")])));
            assert_eq!(b["store"], false);
            assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
            assert_eq!(b["reasoning"], json!({"effort": "medium", "summary": "auto"}));
        }),
        ("openai-responses :: lets callers opt out of the GPT-5 default include", || {
            let r = defaulted(Protocol::OpenaiResponses, OPENAI, with(req("gpt-5.2", vec![user("hi")]), json!({"include": []})));
            assert!(responses::body(&r).get("include").is_none());
        }),
        ("openai-responses :: accepts empty IDs for native reasoning text deltas", || {
            let evs = events(resp(&sse(&[
                json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "reasoning", "id": ""}}),
                json!({"type": "response.reasoning_text.delta", "output_index": 1, "item_id": "", "delta": "Raw"}),
                json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "reasoning", "id": "", "encrypted_content": "state"}}),
                json!({"type": "response.completed", "response": {"id": "resp_1"}}),
            ])));
            assert_eq!(collect(&evs), [rs("Raw", None, Some("state"))]);
        }),
        ("openai-responses :: preserves encrypted reasoning metadata for continuation", || {
            let evs = events(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1"}}),
                json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "thinking"}),
                json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "encrypted-state", "summary": [{"type": "summary_text", "text": "thinking"}]}}),
                json!({"type": "response.completed", "response": {"id": "resp_1"}}),
            ])));
            assert_eq!(reasoning_ends(&evs), [&resp_meta(Some("rs_1"), Some("encrypted-state"))]);
        }),
        ("openai-responses :: ignores terminal reasoning output when item completion is missing", || {
            let evs = events(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": null}}),
                json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "summary_index": 0}),
                json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "summary_index": 0, "delta": "Checked the diff."}),
                json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_1", "summary_index": 0}),
                json!({"type": "response.completed", "response": {"id": "resp_1", "output": [{"type": "reasoning", "id": "rs_1", "encrypted_content": "terminal-state", "summary": [{"type": "summary_text", "text": "Checked the diff."}]}]}}),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "Checked the diff.");
            assert_eq!(collect(&evs), [rs("Checked the diff.", Some("rs_1"), None)]);
        }),
        ("openai-responses :: does not repeat reasoning already finalized by an output item", || {
            let item = json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "encrypted-state"});
            let evs = events(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": null}}),
                json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "Thinking"}),
                json!({"type": "response.output_item.done", "item": item}),
                json!({"type": "response.completed", "response": {"output": [item]}}),
            ])));
            assert_eq!(reasoning_ends(&evs).len(), 1);
            assert_eq!(collect(&evs), [rs("Thinking", Some("rs_1"), Some("encrypted-state"))]);
        }),
        ("openai-responses :: recovers pending function calls without reconciling terminal reasoning", || {
            let evs = events(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": null}}),
                json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "Thinking"}),
                json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": ""}}),
                json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"query\":\"wea"}),
                json!({"type": "response.completed", "response": {"output": [
                    {"type": "reasoning", "id": "rs_1", "encrypted_content": "terminal-state"},
                    {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": "{\"query\":\"weather\"}"}
                ]}}),
            ])));
            assert_eq!(reasoning_ends(&evs), [&resp_meta(Some("rs_1"), None)]);
            let o = out(Ok(evs));
            assert_eq!(o.inputs(), [("lookup", &json!({"query": "weather"}))]);
            assert_eq!(o.finish, "tool_calls");
        }),
        ("openai-responses :: reconciles reasoning summaries that arrive only as finals", || {
            let evs = events(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": null}}),
                json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "summary_index": 0}),
                json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_1", "summary_index": 0, "text": "Checked the diff."}),
                json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_1", "summary_index": 0}),
                json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Checked the diff."}], "encrypted_content": "encrypted-state"}}),
                json!({"type": "response.completed", "response": {"id": "resp_1"}}),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "Checked the diff.");
            let mut r = with(req("gpt-4.1-mini", vec![asst(collect(&evs))]), json!({"include": ["reasoning.encrypted_content"]}));
            // (adapted) CodeSmith replays a reasoning item only before the
            // output that follows it, as the API requires.
            r.messages[0].parts.push(text("Done."));
            assert_eq!(
                responses::body(&r)["input"][0],
                json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Checked the diff."}], "encrypted_content": "encrypted-state"})
            );
        }),
        ("openai-responses :: does not duplicate reasoning finals after streamed deltas", || {
            let evs = events(resp(&sse(&[
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": null}}),
                json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "summary_index": 0}),
                json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "summary_index": 0, "delta": "Streamed"}),
                json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_1", "summary_index": 0, "text": "Streamed"}),
                json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_1", "summary_index": 0}),
                json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "encrypted-state"}}),
                json!({"type": "response.completed", "response": {"id": "resp_1"}}),
            ])));
            let deltas: Vec<&Event> = evs.iter().filter(|e| matches!(e, Event::ReasoningDelta(_))).collect();
            assert_eq!(deltas, [&Event::ReasoningDelta("Streamed".into())]);
        }),
        ("openai-responses :: continues a stateless reasoning conversation", || {
            let r = req(
                "gpt-4.1-mini",
                vec![
                    user("What changed?"),
                    asst(vec![rs("Checked the previous diff.", Some("rs_1"), Some("encrypted-state")), text("The parser changed.")]),
                    user("Summarize it."),
                ],
            );
            assert_eq!(
                responses::body(&r)["input"],
                json!([
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What changed?"}]},
                    {"type": "reasoning", "id": "rs_1", "encrypted_content": "encrypted-state", "summary": [{"type": "summary_text", "text": "Checked the previous diff."}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "The parser changed."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Summarize it."}]}
                ])
            );
        }),
        ("openai-responses :: preserves assistant content order around reasoning items (adapted: no message status)", || {
            let r = req("gpt-4.1-mini", vec![asst(vec![text("Before."), rs("Checked order.", Some("rs_1"), Some("encrypted-state")), text("After.")])]);
            assert_eq!(
                responses::body(&r)["input"],
                json!([
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Before."}]},
                    {"type": "reasoning", "id": "rs_1", "encrypted_content": "encrypted-state", "summary": [{"type": "summary_text", "text": "Checked order."}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "After."}]}
                ])
            );
        }),
        ("openai-responses :: joins streamed summary blocks into one continuation reasoning item (adapted: with the answer)", || {
            let r = req(
                "gpt-4.1-mini",
                vec![asst(vec![rs("First", Some("rs_1"), None), rs("Second", Some("rs_1"), Some("encrypted-state")), text("Done.")])],
            );
            assert_eq!(
                responses::body(&r)["input"][0],
                json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "encrypted-state",
                    "summary": [{"type": "summary_text", "text": "First"}, {"type": "summary_text", "text": "Second"}]})
            );
        }),
        ("openai-responses :: drops empty reasoning items without summary or encrypted content (adapted: no message status)", || {
            let r = req(
                "gpt-4.1-mini",
                vec![
                    user("What changed?"),
                    asst(vec![rs("", Some("rs_6aa28a10c05cf9f566f44022:rs_01a08aeb51217b92a5b853a0cb5b20ca"), None), text("The parser changed.")]),
                    user("Summarize it."),
                ],
            );
            assert_eq!(
                responses::body(&r)["input"],
                json!([
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What changed?"}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "The parser changed."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Summarize it."}]}
                ])
            );
        }),
    ]);
}

// ---- structured Chat reasoning -------------------------------------------------

/// The continuation request for a decoded Chat response.
fn chat_continuation(events: &[Event]) -> Value {
    chat_at(COMPATIBLE, &req("m", vec![asst(collect(events))]))["messages"].clone()
}

fn chunk(d: Value) -> Value {
    json!({"choices": [{"delta": d}]})
}

fn chunk_finish(d: Value, reason: &str) -> Value {
    json!({"choices": [{"delta": d, "finish_reason": reason}]})
}

#[test]
fn openai_chat_structured_reasoning() {
    run(&[
        ("openai-chat :: keeps replayed reasoning details when the model declares no reasoning field", || {
            let details = json!([{"type": "reasoning.text", "text": "thinking", "signature": "signed", "format": "anthropic-claude-v1", "index": 0}]);
            let r = req("m", vec![asst(vec![chat_reasoning("thinking", Some("reasoning"), Some(details.clone())), text("Hello")])]);
            assert_eq!(
                chat_at(COMPATIBLE, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello", "reasoning": "thinking", "reasoning_details": details}])
            );
        }),
        ("openai-chat :: preserves and replays reasoning details alongside scalar reasoning", || {
            let details = json!([
                {"type": "reasoning.text", "text": "thinking", "format": "anthropic-claude-v1", "index": 0},
                {"type": "reasoning.encrypted", "data": "opaque", "format": "anthropic-claude-v1", "index": 1}
            ]);
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning": "thinking", "reasoning_details": [details[0]]})),
                chunk(json!({"reasoning_details": [details[1]]})),
                chunk_finish(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "lookup", "arguments": "{\"query\":\"weather\"}"}}]}), "tool_calls"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "thinking");
            assert_eq!(collect(&evs)[0], chat_reasoning("thinking", Some("reasoning"), Some(details.clone())));
            assert_eq!(
                chat_continuation(&evs)[0],
                json!({"role": "assistant", "content": null, "reasoning": "thinking", "reasoning_details": details,
                    "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "lookup", "arguments": "{\"query\":\"weather\"}"}}]})
            );
        }),
        ("openai-chat :: uses reasoning details as display fallback without inventing a scalar replay field", || {
            let details = json!([
                {"type": "reasoning.summary", "summary": "thinking", "format": "openai-responses-v1", "index": 0},
                {"type": "reasoning.encrypted", "data": "opaque", "format": "openai-responses-v1", "index": 1}
            ]);
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning_details": [details[0]]})),
                chunk(json!({"reasoning_details": [details[1]]})),
                chunk(json!({"content": "Hello"})),
                chunk_finish(json!({}), "stop"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "thinking");
            assert_eq!(chat_continuation(&evs), json!([{"role": "assistant", "content": "Hello", "reasoning_details": details}]));
        }),
        ("openai-chat :: drops unknown reasoning details while using scalar display text", || {
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning": "thinking", "reasoning_details": [{"type": "reasoning.future", "format": "provider-v2", "state": {"opaque": true}}]})),
                chunk(json!({"content": "Hello"})),
                chunk_finish(json!({}), "stop"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "thinking");
            assert_eq!(
                chat_continuation(&evs),
                json!([{"role": "assistant", "content": "Hello", "reasoning": "thinking", "reasoning_details": []}])
            );
        }),
        ("openai-chat :: merges Kimi summary deltas by index and replays details without the streaming index", || {
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning_content": "Let me"})),
                chunk(json!({"reasoning_content": " think"})),
                chunk(json!({"reasoning_details": [{"index": 0, "type": "summary", "summary": "Plan"}]})),
                chunk(json!({"reasoning_details": [{"index": 0, "type": "summary", "summary": " tools"}]})),
                chunk(json!({"reasoning_details": [{"index": 1, "type": "encrypted", "encrypted": "opaque"}]})),
                chunk(json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_time", "arguments": "{}"}}]})),
                chunk_finish(json!({}), "tool_calls"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "Let me think");
            assert_eq!(
                collect(&evs)[0],
                chat_reasoning("Let me think", None, Some(json!([
                    {"index": 0, "type": "summary", "summary": "Plan tools"},
                    {"index": 1, "type": "encrypted", "encrypted": "opaque"}
                ])))
            );
            assert_eq!(
                chat_continuation(&evs)[0],
                json!({"role": "assistant", "content": null,
                    "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_time", "arguments": "{}"}}],
                    "reasoning_content": "Let me think",
                    "reasoning_details": [{"type": "summary", "summary": "Plan tools"}, {"type": "encrypted", "encrypted": "opaque"}]})
            );
        }),
        ("openai-chat :: displays Kimi summaries and replays reasoning_content when no scalar reasoning streams", || {
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning_details": [{"index": 0, "type": "summary", "summary": "Plan"}]})),
                chunk(json!({"reasoning_details": [{"index": 0, "type": "summary", "summary": " tools"}]})),
                chunk(json!({"reasoning_details": [{"index": 1, "type": "encrypted", "encrypted": "opaque"}]})),
                chunk(json!({"content": "Hello"})),
                chunk_finish(json!({}), "stop"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "Plan tools");
            assert_eq!(
                chat_continuation(&evs),
                json!([{"role": "assistant", "content": "Hello", "reasoning_content": "Plan tools",
                    "reasoning_details": [{"type": "summary", "summary": "Plan tools"}, {"type": "encrypted", "encrypted": "opaque"}]}])
            );
        }),
        ("openai-chat :: strips the streaming index from previously stored Kimi details", || {
            let r = req("m", vec![asst(vec![chat_reasoning("thinking", None, Some(json!([
                {"index": 0, "type": "summary", "summary": "thinking"},
                {"index": 1, "type": "encrypted", "encrypted": "opaque"}
            ])))])]);
            assert_eq!(
                chat_at(COMPATIBLE, &r)["messages"],
                json!([{"role": "assistant", "content": "", "reasoning_content": "thinking",
                    "reasoning_details": [{"type": "summary", "summary": "thinking"}, {"type": "encrypted", "encrypted": "opaque"}]}])
            );
        }),
        ("openai-chat :: merges consecutive OpenRouter summary deltas and replays them unmodified", || {
            let merged = json!([
                {"type": "reasoning.summary", "summary": "Plan tools", "format": "openai-responses-v1", "index": 0},
                {"type": "reasoning.encrypted", "data": "opaque", "format": "openai-responses-v1", "index": 0}
            ]);
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning_details": [{"type": "reasoning.summary", "summary": "Plan", "format": "openai-responses-v1", "index": 0}]})),
                chunk(json!({"reasoning_details": [{"type": "reasoning.summary", "summary": " tools", "format": "openai-responses-v1", "index": 0}]})),
                chunk(json!({"reasoning_details": [merged[1]]})),
                chunk(json!({"content": "Hello"})),
                chunk_finish(json!({}), "stop"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "Plan tools");
            assert_eq!(chat_continuation(&evs), json!([{"role": "assistant", "content": "Hello", "reasoning_details": merged}]));
        }),
        ("openai-chat :: uses scalar display text for signature-only reasoning details", || {
            let details = json!([{"type": "reasoning.text", "signature": "signed", "format": "provider-v2", "index": 0}]);
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning": "thinking", "reasoning_details": details})),
                chunk(json!({"content": "Hello"})),
                chunk_finish(json!({}), "stop"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "thinking");
            assert_eq!(collect(&evs)[0], chat_reasoning("thinking", Some("reasoning"), Some(details)));
        }),
        ("openai-chat :: preserves an explicitly empty reasoning details array", || {
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning_details": []})),
                chunk(json!({"content": "Hello"})),
                chunk_finish(json!({}), "stop"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "");
            assert_eq!(chat_continuation(&evs), json!([{"role": "assistant", "content": "Hello", "reasoning_details": []}]));
        }),
        ("openai-chat :: attaches signature-only details that arrive after content", || {
            let merged = json!([{"type": "reasoning.text", "text": "thinking", "signature": "signed", "format": "anthropic-claude-v1", "index": 0}]);
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning": "thinking", "reasoning_details": [{"type": "reasoning.text", "text": "thinking", "format": "anthropic-claude-v1", "index": 0}]})),
                chunk(json!({"content": "Hello"})),
                chunk(json!({"reasoning_details": [{"type": "reasoning.text", "signature": "signed", "format": "anthropic-claude-v1", "index": 0}]})),
                chunk_finish(json!({}), "stop"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "thinking");
            assert_eq!(reasoning_ends(&evs).len(), 1);
            let parts = collect(&evs);
            assert_eq!(parts.iter().filter(|p| matches!(p, Part::Reasoning(_))).count(), 1);
            assert_eq!(parts[0], chat_reasoning("thinking", Some("reasoning"), Some(merged.clone())));
            assert_eq!(
                chat_continuation(&evs),
                json!([{"role": "assistant", "content": "Hello", "reasoning": "thinking", "reasoning_details": merged}])
            );
        }),
        ("openai-chat :: preserves metadata-only reasoning arriving after the finish reason", || {
            let details = json!([{"type": "reasoning.encrypted", "data": "opaque", "format": "openai-responses-v1", "index": 0}]);
            let evs = events(chat(&sse_done(&[chunk_finish(json!({}), "stop"), chunk(json!({"reasoning_details": details}))])));
            assert_eq!(collect(&evs), [chat_reasoning("", None, Some(details.clone()))]);
            assert_eq!(chat_continuation(&evs), json!([{"role": "assistant", "content": "", "reasoning_details": details}]));
        }),
        ("openai-chat :: flushes details-only display reasoning when the stream ends", || {
            let details = json!([{"type": "reasoning.summary", "summary": "summary", "format": "openai-responses-v1", "index": 0}]);
            let evs = events(chat(&sse_done(&[chunk(json!({"reasoning_details": details})), chunk_finish(json!({}), "stop")])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "summary");
            assert_eq!(collect(&evs), [chat_reasoning("summary", None, Some(details))]);
        }),
        ("openai-chat :: replays details from multiple reasoning parts in order", || {
            let first = json!({"type": "reasoning.text", "text": "first", "signature": "signed-0", "index": 0});
            let second = json!({"type": "reasoning.text", "text": "second", "signature": "signed-1", "index": 1});
            let r = req("m", vec![asst(vec![
                chat_reasoning("first", None, Some(json!([first]))),
                chat_reasoning("second", Some("reasoning"), Some(json!([second]))),
            ])]);
            assert_eq!(
                chat_at(COMPATIBLE, &r)["messages"],
                json!([{"role": "assistant", "content": "", "reasoning": "firstsecond", "reasoning_details": [first, second]}])
            );
        }),
        ("openai-chat :: retains scalar replay for mixed structured reasoning parts", || {
            let detail = json!({"type": "reasoning.encrypted", "data": "opaque", "index": 0});
            let r = req("m", vec![asst(vec![chat_reasoning("A", None, Some(json!([detail]))), thinking("B")])]);
            assert_eq!(
                chat_at(COMPATIBLE, &r)["messages"],
                json!([{"role": "assistant", "content": "", "reasoning_content": "AB", "reasoning_details": [detail]}])
            );
        }),
        ("openai-compatible-chat :: replays the latest reasoning_opaque with the concatenated reasoning text", || {
            let evs = events(chat(&sse_done(&[
                chunk(json!({"reasoning_text": "Read the readme first."})),
                chunk(json!({"reasoning_opaque": "opaque-first", "tool_calls": [{"index": 0, "id": "call_first", "function": {"name": "read", "arguments": "{}"}}]})),
                chunk(json!({"reasoning_text": " Then the manifest."})),
                chunk(json!({"reasoning_opaque": "opaque-second", "tool_calls": [{"index": 1, "id": "call_second", "function": {"name": "read", "arguments": "{}"}}]})),
                chunk_finish(json!({}), "tool_calls"),
            ])));
            let o = out(Ok(evs.clone()));
            assert_eq!(o.reasoning, "Read the readme first. Then the manifest.");
            assert_eq!(o.ids(), ["call_first", "call_second"]);
            let ends = reasoning_ends(&evs);
            assert_eq!(ends.len(), 1);
            assert_eq!(
                (ends[0].field.as_deref(), ends[0].opaque.as_deref()),
                (Some("reasoning_text"), Some("opaque-second"))
            );
            let m = &chat_continuation(&evs)[0];
            assert_eq!(m["content"], Value::Null);
            assert_eq!(m["reasoning_text"], "Read the readme first. Then the manifest.");
            assert_eq!(m["reasoning_opaque"], "opaque-second");
        }),
        ("openai-compatible-chat :: keeps a reasoning_opaque that arrives without reasoning text", || {
            let evs = events(chat(&sse_done(&[
                chunk(json!({"content": "Checking.", "reasoning_opaque": "opaque-only"})),
                chunk(json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "read", "arguments": "{}"}}]})),
                chunk_finish(json!({}), "tool_calls"),
            ])));
            assert_eq!(out(Ok(evs.clone())).reasoning, "");
            assert_eq!(reasoning_ends(&evs)[0].opaque.as_deref(), Some("opaque-only"));
            assert_eq!(
                chat_continuation(&evs)[0],
                json!({"role": "assistant", "content": "Checking.",
                    "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{}"}}],
                    "reasoning_opaque": "opaque-only"})
            );
        }),
    ]);
}
