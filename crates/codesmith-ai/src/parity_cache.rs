//! Parity with OpenCode's prompt caching and tool choice tests: the caching
//! and tool choice cases of `provider/openai-chat.test.ts`,
//! `provider/openai-compatible-chat.test.ts`, `provider/openai-responses.test.ts`,
//! `provider/anthropic-messages.test.ts`, `provider/gemini.test.ts` and
//! `provider/empty-tool-choice.test.ts`, and `cache-policy.test.ts` (MIT,
//! copyright the OpenCode authors).
//!
//! OpenCode puts manual cache hints on individual parts; CodeSmith has only
//! the policy, so cases about manual hints put the markers in a `body`
//! override, which the policy keeps and counts the same way ("adapted").

use super::*;
use crate::{Options, Provider, Reasoning, ReasoningMeta};

fn opts(v: Value) -> Options {
    Options::from_value(v)
}

const XAI: &str = "https://api.x.ai/v1";
const DIGITALOCEAN: &str = "https://inference.do-ai.run/v1";
const DASHSCOPE: &str = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1";

fn eph() -> Value {
    json!({"type": "ephemeral"})
}

fn eph_ttl(ttl: &str) -> Value {
    json!({"type": "ephemeral", "ttl": ttl})
}

/// A one-prompt request with a prompt cache key.
fn keyed(model: &str, key: &str) -> Request {
    let mut r = req(model, vec![user("Hello")]);
    r.prompt_cache_key = Some(key.into());
    r
}

/// System prompt and one user message.
fn sys(model: &str, system: &str, prompt: &str) -> Request {
    let mut r = req(model, vec![user(prompt)]);
    r.system = system.into();
    r
}

fn custom(tools: bool, system: bool, messages: Option<CacheMessages>) -> Cache {
    Cache {
        placement: CachePlacement::Custom {
            tools,
            system,
            messages,
        },
        ttl: None,
    }
}

fn ttl(mut c: Cache, ttl: &str) -> Cache {
    c.ttl = Some(ttl.into());
    c
}

fn lookup() -> ToolSpec {
    ToolSpec {
        name: "lookup".into(),
        description: "Look up a value".into(),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

fn body_override(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

/// Every `cache_control` value in an Anthropic body, in breakpoint order.
fn markers(b: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for t in b["tools"].as_array().into_iter().flatten() {
        out.extend(t.get("cache_control").cloned());
    }
    for s in b["system"].as_array().into_iter().flatten() {
        out.extend(s.get("cache_control").cloned());
    }
    for m in b["messages"].as_array().into_iter().flatten() {
        for c in m["content"].as_array().into_iter().flatten() {
            out.extend(c.get("cache_control").cloned());
        }
    }
    out
}

fn signed(text: &str) -> Part {
    let mut meta = ReasoningMeta::of(Protocol::AnthropicMessages);
    meta.signature = Some("sig_1".into());
    Part::Reasoning(Reasoning::new(text, meta))
}

// ---- prompt caching ----------------------------------------------------------

#[test]
fn prompt_caching() {
    run(&[
        ("openai-chat :: maps the request prompt cache key when the compatibility flag is set (adapted: a setCacheKey option)", || {
            let mut r = keyed("compatible-model", "session_123");
            r.options = opts(json!({"setCacheKey": true}));
            assert_eq!(chat_at(COMPATIBLE, &r)["prompt_cache_key"], "session_123");
        }),
        ("openai-chat :: sends the cache TTL as a request option when no markers carry it", || {
            let mut r = req("compatible-model", vec![user("Hello")]);
            r.cache = ttl(Cache::default(), "1h");
            assert_eq!(chat_at(COMPATIBLE, &r)["prompt_cache_options"], json!({"ttl": "1h"}));
        }),
        ("openai-chat :: omits the prompt cache key without the compatibility flag", || {
            let b = chat_at(COMPATIBLE, &keyed("compatible-model", "session_123"));
            assert!(b.get("prompt_cache_key").is_none(), "{b}");
        }),
        ("openai-chat :: omits the prompt cache key when caching is disabled", || {
            let r = keyed("gpt-4o-mini", "session_123");
            assert_eq!(chat_at(OPENAI, &r)["prompt_cache_key"], "session_123");
            assert!(chat_at(OPENAI, &off(r)).get("prompt_cache_key").is_none());
        }),
        ("openai-chat :: maps the xAI Chat prompt cache key to conversation affinity header only", || {
            let r = keyed("grok-4.5", "session_123");
            let p = Provider {
                protocol: Protocol::OpenaiChat,
                base_url: XAI.into(),
                api_key: Some("test".into()),
                headers: vec![],
                ..Default::default()
            };
            assert!(openai::headers(&p, &r).contains(&("x-grok-conv-id".into(), "session_123".into())));
            assert!(chat_at(XAI, &r).get("prompt_cache_key").is_none());
        }),
        ("openai-compatible-chat :: places cache breakpoints only when compatibility declares the format (adapted: a cacheFormat option)", || {
            let prepare = |format: Option<&str>| {
                let mut r = sys("claude-sonnet-5", "You are concise.", "Say hello.");
                r.tools = vec![tool("lookup", json!({}))];
                if let Some(f) = format {
                    r.options = opts(json!({"cacheFormat": f}));
                }
                chat_at(COMPATIBLE, &r)
            };
            let marked = json!([
                {"role": "system", "content": [{"type": "text", "text": "You are concise.", "cache_control": eph()}]},
                {"role": "user", "content": [{"type": "text", "text": "Say hello.", "cache_control": eph()}]},
            ]);
            let declared = prepare(Some("anthropic"));
            assert_eq!(declared["messages"], marked);
            assert_eq!(declared["tools"][0]["function"]["name"], "lookup");
            assert_eq!(declared["tools"][0]["cache_control"], eph());
            assert!(!declared.to_string().contains("\"ttl\""));
            let alibaba = prepare(Some("alibaba"));
            assert_eq!(alibaba["messages"], marked);
            assert!(alibaba["tools"][0].get("cache_control").is_none());
            assert!(!prepare(None).to_string().contains("cache_control"));
        }),
        ("openai-responses :: sends the cache TTL as prompt_cache_options, or prompt_cache_retention for 24h", || {
            let at = |model: &str, t: Option<&str>| {
                let mut r = req(model, vec![user("hi")]);
                if let Some(t) = t {
                    r.cache = ttl(Cache::default(), t);
                }
                responses::body(&r)
            };
            let options = at("gpt-5.6", Some("30m"));
            let retention = at("gpt-5.5", Some("24h"));
            let unset = at("gpt-5.6", None);
            assert_eq!(options["prompt_cache_options"], json!({"ttl": "30m"}));
            assert!(options.get("prompt_cache_retention").is_none());
            assert_eq!(retention["prompt_cache_retention"], "24h");
            assert!(retention.get("prompt_cache_options").is_none());
            assert!(unset.get("prompt_cache_options").is_none() && unset.get("prompt_cache_retention").is_none());
        }),
        ("openai-responses :: maps the request prompt cache key", || {
            let b = responses::body(&keyed("gpt-4.1-mini", "request_cache"));
            assert_eq!(b["prompt_cache_key"], "request_cache");
        }),
        ("openai-responses :: omits the prompt cache key when caching is disabled", || {
            let b = responses::body(&off(keyed("gpt-4.1-mini", "request_cache")));
            assert!(b.get("prompt_cache_key").is_none());
        }),
        ("openai-responses :: omits the prompt cache key when capabilities disable it (adapted: a setCacheKey option)", || {
            let mut r = keyed("gpt-4.1-mini", "request_cache");
            assert_eq!(responses::body(&r)["prompt_cache_key"], "request_cache");
            r.options = opts(json!({"setCacheKey": false}));
            assert!(responses::body(&r).get("prompt_cache_key").is_none());
        }),
        ("anthropic-messages :: sends the hint ttl on cache_control (adapted: the policy's ttl)", || {
            let mut r = sys("claude-sonnet-4-5", "system", "hi");
            r.cache = ttl(Cache::default(), "1h");
            assert_eq!(
                anthropic::body(&r)["system"],
                json!([{"type": "text", "text": "system", "cache_control": eph_ttl("1h")}])
            );
        }),
        ("anthropic-messages :: emits cache_control on tool definitions and tool-result blocks (adapted: placed by the default policy)", || {
            let mut r = req("claude-sonnet-4-5", vec![
                user("What's the weather?"),
                asst(vec![call("call_1", "lookup", json!({}))]),
                results(vec![result("call_1", "lookup", "{\"temp\":72}")]),
            ]);
            r.tools = vec![tool("lookup", json!({"type": "object", "properties": {}}))];
            let b = anthropic::body(&r);
            assert_eq!(b["tools"][0]["cache_control"], eph());
            assert_eq!(b["messages"], json!([
                {"role": "user", "content": [{"type": "text", "text": "What's the weather?"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "lookup", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "{\"temp\":72}", "cache_control": eph()}]},
            ]));
        }),
        ("anthropic-messages :: drops cache_control breakpoints past the 4-per-request cap (adapted: markers from a body override)", || {
            let mut r = req("claude-sonnet-4-5", vec![user("hi")]);
            let blocks: Vec<Value> = ["a", "b", "c", "d", "e", "f"]
                .iter()
                .map(|t| json!({"type": "text", "text": t, "cache_control": eph()}))
                .collect();
            r.body = body_override(json!({"system": blocks}));
            let b = anthropic::body(&r);
            let system = b["system"].as_array().unwrap();
            assert_eq!(system.iter().filter(|s| s.get("cache_control").is_some()).count(), 4);
            assert!(system[4].get("cache_control").is_none() && system[5].get("cache_control").is_none());
            assert!(b["messages"][0]["content"][0].get("cache_control").is_none());
        }),
        ("anthropic-messages :: spends breakpoint budget on tools before system before messages (adapted: markers from a body override)", || {
            let mut r = req("claude-sonnet-4-5", vec![user("hi")]);
            let tools: Vec<Value> = ["t1", "t2", "t3", "t4"]
                .iter()
                .map(|n| json!({"name": n, "description": n, "input_schema": {"type": "object", "properties": {}}, "cache_control": eph()}))
                .collect();
            r.body = body_override(json!({
                "tools": tools,
                "system": [{"type": "text", "text": "system-tail", "cache_control": eph()}],
                "messages": [{"role": "user", "content": [{"type": "text", "text": "message-tail", "cache_control": eph()}]}],
            }));
            let b = anthropic::body(&r);
            assert!(b["tools"].as_array().unwrap().iter().all(|t| t.get("cache_control").is_some()));
            assert!(b["system"][0].get("cache_control").is_none());
            assert!(b["messages"][0]["content"][0].get("cache_control").is_none());
        }),
        ("codesmith :: Anthropic automatic caching (the cacheControl option) takes one of the four slots", || {
            let mut r = sys("claude-sonnet-4-5", "Sys", "hi");
            r.tools = vec![lookup()];
            r.options = opts(json!({"cacheControl": {"type": "ephemeral"}}));
            r.body = body_override(json!({"system": [
                {"type": "text", "text": "a", "cache_control": eph()},
                {"type": "text", "text": "b", "cache_control": eph()},
                {"type": "text", "text": "c", "cache_control": eph()},
                {"type": "text", "text": "d", "cache_control": eph()},
            ]}));
            let b = anthropic::body(&r);
            assert_eq!(b["cache_control"], eph());
            assert_eq!(markers(&b).len(), 3, "{b}");
            assert!(b["tools"][0].get("cache_control").is_none());
        }),
        ("codesmith :: a prompt cache key is clamped to 64 characters", || {
            let long = "k".repeat(80);
            let b = responses::body(&keyed("gpt-4.1-mini", &long));
            assert_eq!(b["prompt_cache_key"].as_str().unwrap().len(), 64);
        }),
        ("codesmith :: OpenRouter Chat caches Anthropic models with breakpoints and others without", || {
            let at = |model: &str| {
                let mut r = sys(model, "Sys", "hi");
                r.tools = vec![lookup()];
                chat_at("https://openrouter.ai/api/v1", &r)
            };
            let claude = at("anthropic/claude-sonnet-4.5");
            assert_eq!(claude["tools"][0]["cache_control"], eph());
            assert_eq!(claude["messages"][0]["content"][0]["cache_control"], eph());
            assert_eq!(claude["messages"][1]["content"][0]["cache_control"], eph());
            let qwen = at("qwen/qwen3-coder");
            assert!(qwen["tools"][0].get("cache_control").is_none());
            assert_eq!(qwen["messages"][1]["content"][0]["cache_control"], eph());
            assert!(!at("openai/gpt-5").to_string().contains("cache_control"));
            assert!(!at("google/gemini-3-pro").to_string().contains("cache_control"));
        }),
        ("codesmith :: Chat tool results take the tail breakpoint on a text part", || {
            let r = req("claude-sonnet-5", vec![
                user("Look it up."),
                asst(vec![call("call_1", "lookup", json!({}))]),
                results(vec![result("call_1", "lookup", "OK")]),
            ]);
            let r = with_options(r, json!({"cacheFormat": "anthropic"}));
            let b = chat_at(COMPATIBLE, &r);
            assert_eq!(
                b["messages"][2],
                json!({"role": "tool", "tool_call_id": "call_1", "content": [{"type": "text", "text": "OK", "cache_control": eph()}]})
            );
            assert_eq!(b["messages"][0]["content"], "Look it up.");
        }),
        ("codesmith :: config cache settings", || {
            assert_eq!(Cache::from_value(&json!(false)), Some(Cache::off()));
            assert_eq!(Cache::from_value(&json!("none")), Some(Cache::off()));
            assert_eq!(Cache::from_value(&json!(true)), Some(Cache::default()));
            assert_eq!(Cache::from_value(&json!({"ttl": "1h"})), Some(ttl(Cache::default(), "1h")));
            assert_eq!(
                Cache::from_value(&json!({"tools": true, "messages": {"tail": 2}})),
                Some(custom(true, false, Some(CacheMessages::Tail(2))))
            );
            assert_eq!(Cache::from_value(&json!({"enabled": false, "ttl": "1h"})), Some(Cache::off()));
            assert_eq!(Cache::from_value(&json!({"ttl": "soon"})), None);
            assert_eq!(Cache::from_value(&json!(3)), None);
        }),
    ]);
}

fn with_options(mut r: Request, o: Value) -> Request {
    r.options = opts(o);
    r
}

// ---- cache-policy.test.ts ----------------------------------------------------

#[test]
fn cache_policy() {
    run(&[
        ("cache-policy :: undefined cache resolves to 'auto' (the recommended default)", || {
            let b = anthropic::body(&sys("claude-sonnet-4-5", "You are concise.", "hi"));
            assert_eq!(b["system"], json!([{"type": "text", "text": "You are concise.", "cache_control": eph()}]));
            assert_eq!(b["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": eph()}]}]));
        }),
        ("cache-policy :: 'auto' marks the last tool, first and last system parts, and final message boundary on Anthropic (adapted: one system block)", || {
            let mut r = req("claude-sonnet-4-5", vec![user("first user"), asst(vec![text("assistant reply")]), user("latest user message")]);
            r.system = "Base agent".into();
            r.tools = vec![tool("t1", json!({"type": "object", "properties": {}}))];
            let b = anthropic::body(&r);
            assert_eq!(b["tools"][0]["cache_control"], eph());
            assert_eq!(b["system"], json!([{"type": "text", "text": "Base agent", "cache_control": eph()}]));
            assert_eq!(b["messages"], json!([
                {"role": "user", "content": [{"type": "text", "text": "first user"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "assistant reply"}]},
                {"role": "user", "content": [{"type": "text", "text": "latest user message", "cache_control": eph()}]},
            ]));
        }),
        ("cache-policy :: 'auto' emits Anthropic cache markers on Vertex (adapted: any Anthropic Messages endpoint)", || {
            let mut r = sys("claude-opus-4-8", "You are concise.", "hi");
            r.tools = vec![lookup()];
            let b = anthropic::body_for(&r, "https://aiplatform.googleapis.com/v1/projects/test/locations/global/publishers/anthropic/models");
            assert_eq!(b["tools"][0]["cache_control"], eph());
            assert_eq!(b["system"][0]["cache_control"], eph());
            assert_eq!(b["messages"][0]["content"][0]["cache_control"], eph());
        }),
        ("cache-policy :: 'auto' emits cache_control markers on DigitalOcean", || {
            let mut r = sys("anthropic-claude-fable-5.1", "You are concise.", "hi");
            r.tools = vec![lookup()];
            let b = chat_at(DIGITALOCEAN, &r);
            assert_eq!(b["tools"][0]["function"]["name"], "lookup");
            assert_eq!(b["tools"][0]["cache_control"], eph());
            assert_eq!(b["messages"], json!([
                {"role": "system", "content": [{"type": "text", "text": "You are concise.", "cache_control": eph()}]},
                {"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": eph()}]},
            ]));
        }),
        ("cache-policy :: Alibaba chat caches the system and conversation tail without marking tools", || {
            let mut r = sys("qwen3.8-max", "You are concise.", "hi");
            r.tools = vec![lookup()];
            let b = chat_at(DASHSCOPE, &r);
            assert_eq!(b["messages"][0]["content"][0]["cache_control"], eph());
            assert_eq!(b["messages"][1]["content"][0]["cache_control"], eph());
            assert!(b["tools"][0].get("cache_control").is_none());
        }),
        ("cache-policy :: Alibaba chat omits automatic cache markers when cache is none", || {
            let b = chat_at(DASHSCOPE, &off(sys("qwen3.8-max", "You are concise.", "hi")));
            assert!(!b.to_string().contains("cache_control"));
        }),
        ("cache-policy :: Alibaba chat does not assume non-Qwen models support cache markers", || {
            for model in ["kimi-k3", "glm-5.2", "deepseek-v4-flash-0731", "MiniMax-M2.5"] {
                let b = chat_at(DASHSCOPE, &sys(model, "You are concise.", "hi"));
                assert!(!b.to_string().contains("cache_control"), "{model}");
            }
        }),
        ("cache-policy :: 'auto' emits Anthropic cache markers on Anthropic-compatible routes", || {
            let b = anthropic::body_for(&sys("compatible", "You are concise.", "hi"), "https://messages.example.test/v1");
            assert_eq!(b["system"], json!([{"type": "text", "text": "You are concise.", "cache_control": eph()}]));
            assert_eq!(b["messages"][0]["content"][0]["cache_control"], eph());
        }),
        ("cache-policy :: 'auto' emits cache markers on ${route} (×6: alibaba, cloudflare-ai-gateway, meta, minimax, moonshot, zai-coding messages)", || {
            for (url, model) in [
                ("https://dashscope-intl.aliyuncs.com/apps/anthropic/v1", "qwen3.8-max"),
                ("https://gateway.ai.cloudflare.com/v1/test/test/anthropic/v1", "anthropic/claude-sonnet-4-6"),
                ("https://api.llama.com/anthropic/v1", "muse-spark-1.3"),
                ("https://api.minimax.io/anthropic/v1", "MiniMax-M3"),
                ("https://api.moonshot.ai/anthropic/v1", "kimi-k3"),
                ("https://api.z.ai/api/anthropic/v1", "glm-5.3"),
            ] {
                let b = anthropic::body_for(&sys(model, "Sys", "hi"), url);
                assert_eq!(b["system"], json!([{"type": "text", "text": "Sys", "cache_control": eph()}]), "{url}");
                assert_eq!(b["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": eph()}]}]), "{url}");
            }
        }),
        ("cache-policy :: 'auto' is a no-op on OpenAI (implicit caching protocol)", || {
            let flat = chat_at(OPENAI, &sys("gpt-4o-mini", "Sys", "hi")).to_string();
            assert!(!flat.contains("cache_control") && !flat.contains("cachePoint"));
        }),
        ("cache-policy :: 'auto' is a no-op on Gemini (out-of-band caching protocol)", || {
            let flat = gemini::body(&sys("gemini-2.5-flash", "Sys", "hi")).to_string();
            assert!(!flat.contains("cache_control") && !flat.contains("cachePoint"));
        }),
        ("cache-policy :: 'none' disables auto placement even when manual hints exist (adapted: no manual hints)", || {
            let mut r = sys("claude-sonnet-4-5", "Sys", "hi");
            r.tools = vec![tool("t1", json!({"type": "object", "properties": {}}))];
            let b = anthropic::body(&off(r));
            assert!(!b.to_string().contains("cache_control"), "{b}");
        }),
        ("cache-policy :: granular object form: tools-only marks just tools", || {
            let mut r = sys("claude-sonnet-4-5", "Sys", "hi");
            r.tools = vec![tool("t1", json!({"type": "object", "properties": {}}))];
            r.cache = custom(true, false, None);
            let b = anthropic::body(&r);
            assert_eq!(b["tools"][0]["cache_control"], eph());
            assert_eq!(markers(&b).len(), 1);
        }),
        ("cache-policy :: auto policy preserves manual CacheHints on other parts (adapted: markers from a body override)", || {
            let mut r = req("claude-sonnet-4-5", vec![user("hi")]);
            r.body = body_override(json!({"system": [
                {"type": "text", "text": "first system", "cache_control": eph_ttl("1h")},
                {"type": "text", "text": "last system"},
            ]}));
            let b = anthropic::body(&r);
            assert_eq!(b["system"][0]["cache_control"], eph_ttl("1h"));
            assert_eq!(b["system"][1]["cache_control"], eph());
            assert_eq!(b["messages"][0]["content"][0]["cache_control"], eph());
        }),
        ("cache-policy :: auto policy stays within the four-breakpoint cap when preserving manual hints (adapted: markers from a body override)", || {
            let mut r = req("claude-sonnet-4-5", vec![user("hi")]);
            r.tools = vec![tool("t1", json!({"type": "object", "properties": {}}))];
            r.body = body_override(json!({"system": [
                {"type": "text", "text": "Base agent"},
                {"type": "text", "text": "Manual context", "cache_control": eph_ttl("1h")},
                {"type": "text", "text": "Project instructions"},
            ]}));
            let b = anthropic::body(&r);
            assert!(b["tools"][0].get("cache_control").is_some());
            let system: Vec<bool> = b["system"].as_array().unwrap().iter().map(|s| s.get("cache_control").is_some()).collect();
            assert_eq!(system, [true, true, true]);
            assert_eq!(b["system"][1]["cache_control"], eph_ttl("1h"));
            assert!(b["messages"][0]["content"][0].get("cache_control").is_none());
            assert_eq!(markers(&b).len(), 4);
        }),
        ("cache-policy :: a ttl-only policy keeps automatic placement", || {
            let mut r = sys("claude-sonnet-4-5", "Sys", "hi");
            r.cache = ttl(Cache::default(), "1h");
            let b = anthropic::body(&r);
            assert_eq!(b["system"], json!([{"type": "text", "text": "Sys", "cache_control": eph_ttl("1h")}]));
            assert_eq!(b["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": eph_ttl("1h")}]}]));
        }),
        ("cache-policy :: ttl in the policy flows through to wire markers", || {
            let mut r = sys("claude-sonnet-4-5", "Sys", "hi");
            r.cache = ttl(custom(false, true, None), "1h");
            assert_eq!(anthropic::body(&r)["system"], json!([{"type": "text", "text": "Sys", "cache_control": eph_ttl("1h")}]));
        }),
        ("cache-policy :: messages: { tail: 2 } marks the last 2 message boundaries", || {
            let mut r = req("claude-sonnet-4-5", vec![user("u1"), asst(vec![text("a1")]), user("u2"), asst(vec![text("a2")])]);
            r.cache = custom(false, false, Some(CacheMessages::Tail(2)));
            let m = anthropic::body(&r)["messages"].clone();
            assert!(m[0]["content"][0].get("cache_control").is_none());
            assert!(m[1]["content"][0].get("cache_control").is_none());
            assert_eq!(m[2]["content"][0]["cache_control"], eph());
            assert_eq!(m[3]["content"][0]["cache_control"], eph());
        }),
        ("cache-policy :: 'latest-assistant' marks the last assistant message", || {
            let mut r = req("claude-sonnet-4-5", vec![user("u1"), asst(vec![text("a1")]), user("u2")]);
            r.cache = custom(false, false, Some(CacheMessages::LatestAssistant));
            let m = anthropic::body(&r)["messages"].clone();
            assert!(m[0]["content"][0].get("cache_control").is_none());
            assert_eq!(m[1]["content"][0]["cache_control"], eph());
            assert!(m[2]["content"][0].get("cache_control").is_none());
        }),
        ("cache-policy :: skips non-cacheable parts when placing message breakpoints (adapted: thinking blocks for compaction parts)", || {
            let mut r = req("claude-sonnet-4-5", vec![asst(vec![call("tc_1", "lookup", json!({})), signed("summary")])]);
            r.cache = custom(false, false, Some(CacheMessages::LatestAssistant));
            let m = anthropic::body(&r)["messages"].clone();
            assert_eq!(m[0]["content"][0]["type"], "tool_use");
            assert_eq!(m[0]["content"][0]["cache_control"], eph());
            assert!(m[0]["content"][1].get("cache_control").is_none());
            r.messages = vec![asst(vec![signed("summary")])];
            assert!(!anthropic::body(&r).to_string().contains("cache_control"));
        }),
    ]);
}

// ---- tool choice and parallel tool calls -------------------------------------

#[test]
fn tool_choice() {
    run(&[
        ("openai-chat :: maps disableParallelToolUse to parallel_tool_calls only when tools are present (adapted: the request's parallel_tool_calls)", || {
            let mut r = hello("gpt-4o-mini");
            r.parallel_tool_calls = Some(false);
            let without = chat_at(OPENAI, &r);
            r.tools = vec![tool("lookup", json!({"type": "object"}))];
            let with = chat_at(OPENAI, &r);
            r.parallel_tool_calls = None;
            let unset = chat_at(OPENAI, &r);
            assert_eq!(with["parallel_tool_calls"], false);
            assert!(without.get("parallel_tool_calls").is_none());
            for k in ["parallel_tool_calls", "service_tier", "verbosity", "metadata", "safety_identifier"] {
                assert!(unset.get(k).is_none(), "{k}");
            }
        }),
        ("openai-compatible-chat :: prepares generic Chat target (adapted: no endpoint query or strict flag)", || {
            let mut r = hello("deepseek-chat");
            r.tools = vec![tool("lookup", json!({"type": "object"}))];
            r.tool_choice = Some(ToolChoice::Required);
            assert_eq!(
                chat_at("https://api.deepseek.test/v1/", &r),
                json!({"model": "deepseek-chat", "messages": [
                    {"role": "system", "content": "You are concise."},
                    {"role": "user", "content": "Say hello."}
                ], "tools": [{"type": "function", "function": {"name": "lookup", "description": "Lookup data", "parameters": {"type": "object"}}}],
                "tool_choice": "required", "stream": true, "stream_options": {"include_usage": true},
                "max_tokens": 20, "temperature": 0.0})
            );
        }),
        ("openai-responses :: maps the canonical parallel tool setting with provider-option precedence", || {
            let mut r = hello("gpt-4.1-mini");
            r.parallel_tool_calls = Some(false);
            assert_eq!(responses::body(&r)["parallel_tool_calls"], false);
            r.parallel_tool_calls = Some(true);
            assert_eq!(responses::body(&r)["parallel_tool_calls"], true);
            r.parallel_tool_calls = Some(false);
            r.options = opts(json!({"parallelToolCalls": true}));
            assert_eq!(responses::body(&r)["parallel_tool_calls"], true);
        }),
        ("anthropic-messages :: keeps tools and sends tool_choice none", || {
            let mut r = off(req("claude-sonnet-4-5", vec![
                user("What is the weather?"),
                asst(vec![call("call_1", "lookup", json!({"query": "weather"}))]),
                results(vec![result("call_1", "lookup", "{\"forecast\":\"sunny\"}")]),
            ]));
            r.tools = vec![ToolSpec {
                name: "lookup".into(),
                description: "Look things up".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }];
            r.tool_choice = Some(ToolChoice::None);
            let b = anthropic::body(&r);
            assert_eq!(b["tools"], json!([{"name": "lookup", "description": "Look things up", "input_schema": {"type": "object", "properties": {}}}]));
            assert_eq!(b["tool_choice"], json!({"type": "none"}));
        }),
        ("gemini :: keeps tools and sends function calling mode NONE", || {
            let mut r = req("gemini-2.5-flash", vec![user("Say hello.")]);
            r.tools = vec![tool("lookup", json!({"type": "object"}))];
            r.tool_choice = Some(ToolChoice::None);
            let b = gemini::body(&r);
            assert_eq!(b["contents"], json!([{"role": "user", "parts": [{"text": "Say hello."}]}]));
            assert_eq!(b["tools"][0]["functionDeclarations"][0]["name"], "lookup");
            assert_eq!(b["tools"][0]["functionDeclarations"][0]["description"], "Lookup data");
            assert_eq!(b["toolConfig"], json!({"functionCallingConfig": {"mode": "NONE"}}));
        }),
        ("empty-tool-choice :: ${route.id} preserves ${choice.type} tool choice with active tools (×8)", || {
            for (choice, wire, chat, responses_tool) in [
                (ToolChoice::Auto, json!("auto"), None, None),
                (ToolChoice::None, json!("none"), None, None),
                (ToolChoice::Required, json!("required"), None, None),
                (
                    ToolChoice::Tool("lookup".into()),
                    Value::Null,
                    Some(json!({"type": "function", "function": {"name": "lookup"}})),
                    Some(json!({"type": "function", "name": "lookup"})),
                ),
            ] {
                let mut r = req("gpt-4.1-mini", vec![user("Look up the value.")]);
                r.tools = vec![lookup()];
                r.tool_choice = Some(choice);
                assert_eq!(chat_at(OPENAI, &r)["tool_choice"], chat.unwrap_or(wire.clone()));
                assert_eq!(responses::body(&r)["tool_choice"], responses_tool.unwrap_or(wire));
            }
        }),
        ("empty-tool-choice :: OpenAI Responses omits allowed tool choice without tool definitions", || {
            for tools in [vec![], vec![lookup()]] {
                let mut r = req("gpt-4.1-mini", vec![user("Look up the value.")]);
                let empty = tools.is_empty();
                r.tools = tools;
                r.tool_choice = Some(ToolChoice::Required);
                r.options = opts(json!({"allowedTools": {"mode": "required", "toolNames": ["lookup"]}}));
                let b = responses::body(&r);
                if empty {
                    assert!(b.get("tool_choice").is_none(), "{b}");
                } else {
                    assert_eq!(b["tool_choice"], json!({"type": "allowed_tools", "mode": "required", "tools": [{"type": "function", "name": "lookup"}]}));
                }
            }
        }),
        ("codesmith :: Anthropic named and required tool choice carry the parallel setting", || {
            let mut r = off(req("claude-sonnet-4-5", vec![user("Look it up.")]));
            r.tools = vec![lookup()];
            r.tool_choice = Some(ToolChoice::Tool("lookup".into()));
            r.parallel_tool_calls = Some(false);
            assert_eq!(anthropic::body(&r)["tool_choice"], json!({"type": "tool", "name": "lookup", "disable_parallel_tool_use": true}));
            r.tool_choice = Some(ToolChoice::Required);
            assert_eq!(anthropic::body(&r)["tool_choice"], json!({"type": "any", "disable_parallel_tool_use": true}));
            r.tool_choice = None;
            assert_eq!(anthropic::body(&r)["tool_choice"], json!({"type": "auto", "disable_parallel_tool_use": true}));
            r.parallel_tool_calls = None;
            assert!(anthropic::body(&r).get("tool_choice").is_none());
            r.tools = vec![];
            r.tool_choice = Some(ToolChoice::None);
            assert!(anthropic::body(&r).get("tool_choice").is_none());
        }),
        ("codesmith :: Gemini named tool choice allows only that function", || {
            let mut r = req("gemini-2.5-flash", vec![user("Look it up.")]);
            r.tools = vec![lookup()];
            r.tool_choice = Some(ToolChoice::Tool("lookup".into()));
            assert_eq!(gemini::body(&r)["toolConfig"], json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["lookup"]}}));
            r.tools = vec![];
            assert!(gemini::body(&r).get("toolConfig").is_none());
        }),
        ("codesmith :: a named tool choice needs a name", || {
            let mut r = req("gpt-4.1-mini", vec![user("hi")]);
            r.tools = vec![lookup()];
            r.tool_choice = Some(ToolChoice::Tool(String::new()));
            assert!(invalid(Protocol::OpenaiChat, &r).contains("OpenAI Chat tool choice requires a tool name"));
        }),
    ]);
}
