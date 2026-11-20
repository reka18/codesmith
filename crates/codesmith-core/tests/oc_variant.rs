//! Provider-specific reasoning variant spellings (OpenCode's
//! `variant.test.ts`), driven through `catalog::variant`.

use codesmith_core::catalog::variant::{self, Budget, Reasoning, Support, Target};
use serde_json::{json, Value};

const P: &str = "@opencode/ai/providers/";

fn effort(values: &[&str]) -> Support {
    Support::Effort(Some(values.iter().map(|s| s.to_string()).collect()))
}

fn any_effort() -> Support {
    Support::Effort(None)
}

fn budget(min: Option<i64>, max: Option<i64>) -> Support {
    Support::Budget(Budget { min, max })
}

fn resolve_as(
    package: &str,
    model: &str,
    output: Option<i64>,
    provider: &str,
    s: &[Support],
) -> Value {
    let t = Target {
        package: Some(package),
        provider_id: provider,
        model_id: model,
        // OpenCode's default model limit.
        output: output.unwrap_or(32_000),
    };
    serde_json::to_value(variant::resolve(&t, s)).unwrap()
}

fn resolve(package: &str, model: &str, s: &[Support]) -> Value {
    resolve_as(package, model, None, "test", s)
}

fn ids(v: Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

fn pkg(p: &str) -> String {
    format!("{P}{p}")
}

#[test]
fn oc_variant_recognizes_claude_version_spellings_and_future_models() {
    for id in [
        "claude-sonnet-3.7",
        "claude-sonnet-3-7",
        "claude-3.7-sonnet",
        "claude-3-7-sonnet",
    ] {
        assert_eq!(
            resolve(&pkg("anthropic"), id, &[any_effort()]),
            json!([
                {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
                {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
            ]),
            "{id}"
        );
    }
    let all: Vec<Value> = ["low", "medium", "high", "xhigh", "max"]
        .iter()
        .map(|e| json!({"id": e, "settings": {"effort": e, "thinking": {"type": "adaptive", "display": "summarized"}}}))
        .collect();
    assert_eq!(
        resolve(&pkg("anthropic"), "claude-opus-6", &[any_effort()]),
        Value::Array(all)
    );
    assert_eq!(
        resolve_as(
            &pkg("anthropic"),
            "claude-haiku-4-5",
            Some(64_000),
            "test",
            &[budget(Some(1024), Some(64_000))]
        ),
        json!([
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
        ])
    );
}

fn responses(e: &str) -> Value {
    json!({"id": e, "settings": {"reasoningEffort": e, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}})
}

fn adaptive(e: &str) -> Value {
    json!({"id": e, "settings": {"effort": e, "thinking": {"type": "adaptive", "display": "summarized"}}})
}

fn chat(e: &str) -> Value {
    json!({"id": e, "settings": {"reasoningEffort": e}})
}

#[test]
fn oc_variant_spells_cloudflare_ai_gateway_variants_for_their_upstream_routes() {
    let p = pkg("cloudflare-ai-gateway");
    assert_eq!(
        resolve(&p, "openai/gpt-5.4", &[effort(&["low", "xhigh"])]),
        json!([responses("low"), responses("xhigh")])
    );
    assert_eq!(
        resolve(
            &p,
            "anthropic/claude-sonnet-4-6",
            &[effort(&["low", "high"])]
        ),
        json!([adaptive("low"), adaptive("high")])
    );
    assert_eq!(
        resolve(&p, "xai/grok-4.6", &[effort(&["low", "high"])]),
        json!([chat("low"), chat("high")])
    );
}

#[test]
fn oc_variant_spells_vercel_ai_gateway_variants_for_their_selected_routes() {
    let p = pkg("vercel-ai-gateway");
    assert_eq!(
        resolve(&p, "openai/gpt-5.4", &[effort(&["low", "xhigh"])]),
        json!([responses("low"), responses("xhigh")])
    );
    assert_eq!(
        ids(resolve(&p, "spacexai/grok-4.7", &[any_effort()])),
        ["low", "medium", "high"]
    );
    assert_eq!(
        resolve(
            &p,
            "anthropic/claude-sonnet-4.6",
            &[effort(&["none", "low", "high"])]
        ),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            adaptive("low"),
            adaptive("high"),
        ])
    );
    assert_eq!(
        resolve(&p, "meta/muse-spark-1.3", &[effort(&["low", "high"])]),
        json!([chat("low"), chat("high")])
    );
    assert_eq!(
        resolve_as(
            &p,
            "alibaba/qwen-3-32b",
            Some(38_912),
            "test",
            &[Support::Toggle, budget(Some(1), Some(38_912))]
        ),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 19456}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 38911}}},
        ])
    );
    assert_eq!(
        resolve(
            &format!("{p}/chat"),
            "anthropic/claude-sonnet-4.6",
            &[effort(&["high"])]
        ),
        json!([chat("high")])
    );
    assert_eq!(
        resolve(
            &format!("{p}/messages"),
            "anthropic/claude-sonnet-4.6",
            &[effort(&["high"])]
        ),
        json!([adaptive("high")])
    );
    assert_eq!(
        ids(resolve(
            &format!("{p}/responses"),
            "spacexai/grok-4.7",
            &[any_effort()]
        )),
        ["low", "medium", "high"]
    );
}

#[test]
fn oc_variant_spells_xai_responses_variants_with_catalog_effort_levels() {
    let p = pkg("xai");
    assert_eq!(
        resolve(
            &p,
            "grok-4.6",
            &[effort(&["low", "medium", "high", "xhigh"])]
        ),
        json!([
            responses("low"),
            responses("medium"),
            responses("high"),
            responses("xhigh")
        ])
    );
    assert_eq!(
        resolve(&p, "grok-4.3", &[effort(&["none", "low"])]),
        json!([responses("none"), responses("low")])
    );
    assert_eq!(
        ids(resolve(&p, "grok-4.6", &[any_effort()])),
        ["low", "medium", "high"]
    );
}

#[test]
fn oc_variant_spells_bedrock_converse_claude_budgets_as_a_thinking_setting() {
    assert_eq!(
        resolve_as(
            &pkg("amazon-bedrock"),
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            Some(64_000),
            "test",
            &[budget(Some(1024), None)]
        ),
        json!([
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
        ])
    );
}

#[test]
fn oc_variant_spells_bedrock_converse_effort_for_grok_and_nova() {
    let s = [effort(&["low", "xhigh"])];
    let grok = |e: &str| json!({"id": e, "body": {"additionalModelRequestFields": {"reasoning": {"effort": e}}}});
    assert_eq!(
        resolve(&pkg("amazon-bedrock"), "us.xai.grok-4.6", &s),
        json!([grok("low"), grok("xhigh")])
    );
    let nova = |e: &str| json!({"id": e, "body": {"additionalModelRequestFields": {"reasoningConfig": {"type": "enabled", "maxReasoningEffort": e}}}});
    assert_eq!(
        resolve(&pkg("amazon-bedrock"), "us.amazon.nova-2-lite-v1:0", &s),
        json!([nova("low"), nova("xhigh")])
    );
}

#[test]
fn oc_variant_caps_alibaba_thinking_budget_variants_at_64k() {
    let s = [Support::Toggle, budget(None, None)];
    assert_eq!(
        resolve_as(&pkg("alibaba/chat"), "kimi-k2.6", Some(262_144), "test", &s),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "high", "settings": {"enableThinking": true, "thinkingBudget": 32000}},
            {"id": "max", "settings": {"enableThinking": true, "thinkingBudget": 63999}},
        ])
    );
    assert_eq!(
        resolve_as(
            &pkg("alibaba/messages"),
            "kimi-k2.6",
            Some(262_144),
            "test",
            &s
        ),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 32000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 63999}}},
        ])
    );
    assert_eq!(
        resolve_as(&pkg("alibaba/chat"), "kimi-k2.5", Some(32_768), "test", &s),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "high", "settings": {"enableThinking": true, "thinkingBudget": 16384}},
            {"id": "max", "settings": {"enableThinking": true, "thinkingBudget": 32767}},
        ])
    );
}

#[test]
fn oc_variant_spells_workers_ai_thinking_controls_through_the_chat_template() {
    let p = pkg("cloudflare-workers-ai");
    let off = json!({"id": "none", "body": {"chat_template_kwargs": {"enable_thinking": false, "thinking": false}}});
    let on = json!({"id": "thinking", "body": {"chat_template_kwargs": {"enable_thinking": true, "thinking": true}}});
    assert_eq!(
        resolve(
            &p,
            "@cf/deepseek-ai/deepseek-v4-flash-0731",
            &[effort(&["none", "low", "high", "max"])]
        ),
        json!([off, chat("low"), chat("high"), chat("max")])
    );
    assert_eq!(
        resolve(&p, "@cf/zai-org/glm-4.7-flash", &[Support::Toggle]),
        json!([off, on])
    );
    assert_eq!(
        resolve(
            &p,
            "@cf/qwen/qwen3.8-27b",
            &[Support::Toggle, effort(&["low", "medium", "xhigh"])]
        ),
        json!([off, chat("low"), chat("medium"), chat("xhigh")])
    );
}

#[test]
fn oc_variant_spells_cohere_native_thinking_and_compatibility_effort_variants() {
    assert_eq!(
        resolve_as(
            &pkg("cohere"),
            "command-a-reasoning-08-2025",
            Some(32_000),
            "test",
            &[Support::Toggle, budget(Some(1), None)]
        ),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "high", "settings": {"thinking": {"type": "enabled", "tokenBudget": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "tokenBudget": 31999}}},
        ])
    );
    assert_eq!(
        resolve(
            &pkg("cohere/chat"),
            "north-mini-code-1-0",
            &[effort(&["none", "high"])]
        ),
        json!([chat("none"), chat("high")])
    );
}

#[test]
fn oc_variant_spells_chat_completions_variants_for_hosting_providers() {
    assert_eq!(
        resolve_as(
            &pkg("digitalocean"),
            "openai-gpt-5-nano",
            None,
            "digitalocean",
            &[effort(&["minimal", "low", "medium", "high"])]
        ),
        json!([chat("minimal"), chat("low"), chat("medium"), chat("high")])
    );
    let oc = pkg("openai-compatible");
    assert_eq!(
        resolve_as(
            &oc,
            "deepseek-ai/deepseek-v4-pro",
            None,
            "nvidia",
            &[effort(&["none", "high", "max"])]
        ),
        json!([
            {"id": "none", "body": {"chat_template_kwargs": {"thinking": false}}},
            {"id": "high", "body": {"chat_template_kwargs": {"thinking": true, "reasoning_effort": "high"}}},
            {"id": "max", "body": {"chat_template_kwargs": {"thinking": true, "reasoning_effort": "max"}}},
        ])
    );
    assert_eq!(
        resolve_as(
            &oc,
            "moonshotai/kimi-k2.6",
            None,
            "nvidia",
            &[effort(&["none", "low", "high", "max"])]
        ),
        json!([
            {"id": "none", "body": {"chat_template_kwargs": {"thinking": false}}},
            {"id": "thinking", "body": {"chat_template_kwargs": {"thinking": true}}},
        ])
    );
    assert_eq!(
        resolve_as(
            &oc,
            "moonshotai/kimi-k3",
            None,
            "nvidia",
            &[Support::Toggle, effort(&["low", "high", "max"])]
        ),
        json!([
            {"id": "none", "body": {"chat_template_kwargs": {"thinking": false}}},
            chat("low"),
            chat("high"),
            chat("max"),
        ])
    );
    assert_eq!(
        resolve_as(
            &oc,
            "minimaxai/minimax-m3",
            None,
            "nvidia",
            &[Support::Toggle]
        ),
        json!([
            {"id": "none", "body": {"chat_template_kwargs": {"thinking_mode": "disabled"}}},
            {"id": "thinking", "body": {"chat_template_kwargs": {"thinking_mode": "enabled"}}},
        ])
    );
    let baseten = |e: &str, on: bool| json!({"id": e, "settings": {"reasoningEffort": e}, "body": {"chat_template_args": {"enable_thinking": on}}});
    assert_eq!(
        resolve(
            &pkg("baseten"),
            "zai-org/GLM-5.2",
            &[effort(&["none", "high", "max"])]
        ),
        json!([
            baseten("none", false),
            baseten("high", true),
            baseten("max", true)
        ])
    );
    assert_eq!(
        resolve(&pkg("baseten"), "moonshotai/Kimi-K2.6", &[Support::Toggle]),
        json!([
            {"id": "none", "body": {"chat_template_args": {"enable_thinking": false}}},
            {"id": "thinking", "body": {"chat_template_args": {"enable_thinking": true}}},
        ])
    );
    assert_eq!(
        resolve(
            &pkg("deepinfra"),
            "zai-org/GLM-5.2",
            &[Support::Toggle, effort(&["low", "high", "xhigh"])]
        ),
        json!([
            {"id": "none", "body": {"reasoning": {"enabled": false}}},
            chat("low"),
            chat("high"),
            chat("xhigh"),
        ])
    );
}

#[test]
fn oc_variant_spells_gitlab_duo_variants_for_gitlab_ai_provider_routes() {
    let gitlab = |id: &str, output: Option<i64>, s: &[Support]| {
        resolve_as("aisdk:gitlab-ai-provider", id, output, "gitlab", s)
    };
    let duo =
        |e: &str| json!({"id": e, "settings": {"thinking": {"type": "adaptive", "effort": e}}});
    assert_eq!(
        gitlab(
            "duo-chat-opus-5-5",
            None,
            &[effort(&["low", "medium", "high", "xhigh", "max"])]
        ),
        json!([
            duo("low"),
            duo("medium"),
            duo("high"),
            duo("xhigh"),
            duo("max")
        ])
    );
    assert_eq!(
        gitlab(
            "duo-chat-sonnet-5",
            None,
            &[Support::Toggle, effort(&["low", "max"])]
        ),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            duo("low"),
            duo("max"),
        ])
    );
    assert_eq!(
        gitlab(
            "duo-chat-fable-5-1",
            None,
            &[Support::Toggle, effort(&["high"])]
        ),
        json!([duo("high")])
    );
    assert_eq!(
        gitlab("duo-chat-opus-4-5", Some(64_000), &[any_effort()]),
        json!([
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}},
        ])
    );
    assert_eq!(
        gitlab(
            "duo-chat-gpt-6-sol",
            None,
            &[effort(&["none", "low", "max"])]
        ),
        json!([chat("none"), chat("low"), chat("max")])
    );
    assert_eq!(
        gitlab("duo-chat-gpt-5-3-codex", None, &[effort(&["low"])]),
        json!([chat("low")])
    );
    assert_eq!(gitlab("duo-chat-gpt-5-4", None, &[]), json!([]));
    assert_eq!(
        gitlab("duo-workflow-opus-5-5", None, &[any_effort()]),
        json!([])
    );
}

/// OpenCode's bundled native packages (`provider.ts`), less the image,
/// video, decision and embedding APIs.
const NATIVE: &[&str] = &[
    "alibaba",
    "alibaba/chat",
    "alibaba/messages",
    "alibaba/responses",
    "amazon-bedrock",
    "amazon-bedrock/mantle",
    "amazon-bedrock/mantle/chat",
    "amazon-bedrock/mantle/messages",
    "amazon-bedrock/mantle/responses",
    "anthropic-compatible",
    "anthropic",
    "azure",
    "azure/chat",
    "azure/messages",
    "azure/responses",
    "baseten",
    "cerebras",
    "cloudflare-ai-gateway",
    "cloudflare-workers-ai",
    "cohere",
    "cohere/chat",
    "deepinfra",
    "deepseek",
    "digitalocean",
    "digitalocean/responses",
    "fireworks",
    "google-vertex-messages",
    "google-vertex",
    "google-vertex/chat",
    "google-vertex/gemini",
    "google-vertex/interactions",
    "google-vertex/messages",
    "google-vertex/mistral",
    "google-vertex/responses",
    "google",
    "google/interactions",
    "groq",
    "meta",
    "meta/chat",
    "meta/messages",
    "meta/responses",
    "minimax",
    "minimax/chat",
    "minimax/messages",
    "minimax/responses",
    "mistral",
    "moonshot",
    "moonshot/chat",
    "moonshot/messages",
    "moonshot/responses",
    "openai-compatible-responses",
    "openai-compatible",
    "openai-compatible/responses",
    "openai",
    "openai/chat",
    "openai/responses",
    "openrouter",
    "openrouter/responses",
    "togetherai",
    "venice",
    "venice/responses",
    "vercel-ai-gateway",
    "vercel-ai-gateway/chat",
    "vercel-ai-gateway/messages",
    "vercel-ai-gateway/responses",
    "xai",
    "zai-coding-plan",
    "zai-coding-plan/chat",
    "zai-coding-plan/messages",
    "zai-coding-plan/responses",
    "zai",
    "zai/chat",
];

#[test]
fn oc_variant_spells_reasoning_variants_for_every_bundled_native_package() {
    let s = [Support::Toggle, any_effort(), budget(None, None)];
    let missing: Vec<&str> = NATIVE
        .iter()
        .copied()
        .filter(|p| {
            resolve_as(&pkg(p), "test-model", Some(32_000), "test", &s)
                .as_array()
                .unwrap()
                .is_empty()
        })
        .collect();
    assert!(missing.is_empty(), "{missing:?}");
}

#[test]
fn oc_variant_spells_mantle_variants_for_the_api_each_model_is_routed_through() {
    assert_eq!(
        resolve(
            &pkg("amazon-bedrock/mantle/messages"),
            "anthropic.claude-opus-4-8",
            &[effort(&["low", "max"])]
        ),
        json!([adaptive("low"), adaptive("max")])
    );
    assert_eq!(
        resolve(
            &pkg("amazon-bedrock/mantle/chat"),
            "openai.gpt-oss-120b",
            &[any_effort()]
        ),
        json!([chat("low"), chat("medium"), chat("high")])
    );
}

#[test]
fn oc_variant_spells_google_interactions_variants_as_thinking_levels() {
    for p in ["google/interactions", "google-vertex/interactions"] {
        let level = |e: &str| json!({"id": e, "settings": {"thinkingLevel": e, "thinkingSummaries": "auto"}});
        assert_eq!(
            resolve(&pkg(p), "gemini-3.5-pro", &[effort(&["low", "high"])]),
            json!([level("low"), level("high")]),
            "{p}"
        );
    }
}

#[test]
fn oc_variant_reads_models_dev_v2_reasoning_as_variant_support() {
    assert_eq!(variant::supports(&Reasoning::default()), vec![]);
    assert_eq!(
        variant::supports(&Reasoning {
            supported: true,
            ..Default::default()
        }),
        vec![]
    );
    assert_eq!(
        variant::supports(&Reasoning {
            supported: true,
            toggle: Some(true),
            effort: Some(vec!["default".into(), "low".into()]),
            budget: Some(Budget {
                min: Some(1024),
                max: Some(8000)
            }),
        }),
        vec![
            Support::Toggle,
            effort(&["low"]),
            budget(Some(1024), Some(8000))
        ]
    );
}

#[test]
fn oc_variant_spells_messages_variants_for_each_provider() {
    let manual = |e: &str| json!({"id": e, "settings": {"effort": e, "thinking": {"type": "enabled", "budgetTokens": 16000}}});
    assert_eq!(
        resolve(
            &pkg("anthropic"),
            "claude-opus-4-5",
            &[effort(&["low", "high"]), budget(Some(1024), None)]
        ),
        json!([manual("low"), manual("high")])
    );
    assert_eq!(
        resolve(
            &pkg("anthropic"),
            "claude-opus-4-8",
            &[effort(&["low", "max"])]
        ),
        json!([adaptive("low"), adaptive("max")])
    );
    assert_eq!(
        resolve(&pkg("minimax/messages"), "MiniMax-M3", &[Support::Toggle]),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "adaptive"}}},
        ])
    );
    let plain = |e: &str| json!({"id": e, "settings": {"effort": e}});
    assert_eq!(
        resolve(
            &pkg("moonshot/messages"),
            "k3",
            &[Support::Toggle, effort(&["low", "high", "max"])]
        ),
        json!([plain("low"), plain("high"), plain("max")])
    );
    let enabled =
        |e: &str| json!({"id": e, "settings": {"effort": e, "thinking": {"type": "enabled"}}});
    assert_eq!(
        resolve(
            &pkg("alibaba/messages"),
            "qwen3.8-max",
            &[
                Support::Toggle,
                effort(&["low", "medium", "xhigh"]),
                budget(Some(1024), Some(262_144))
            ]
        ),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            enabled("low"),
            enabled("medium"),
            enabled("xhigh"),
        ])
    );
    assert_eq!(
        resolve(
            &pkg("zai-coding-plan/messages"),
            "glm-5.3",
            &[
                Support::Toggle,
                effort(&["low", "high", "max"]),
                budget(Some(1024), None)
            ]
        ),
        json!([enabled("low"), enabled("high"), enabled("max")])
    );
}

#[test]
fn oc_variant_spells_chat_completions_variants_for_direct_providers() {
    let on = |e: &str| json!({"id": e, "settings": {"reasoningEffort": e}, "body": {"thinking": {"type": "enabled"}}});
    assert_eq!(
        resolve(
            &pkg("deepseek"),
            "deepseek-v4-flash",
            &[Support::Toggle, effort(&["low", "high", "max"])]
        ),
        json!([
            {"id": "none", "body": {"thinking": {"type": "disabled"}}},
            on("low"),
            on("high"),
            on("max"),
        ])
    );
    assert_eq!(
        resolve(
            &pkg("moonshot/chat"),
            "kimi-k3",
            &[Support::Toggle, effort(&["low", "high", "max"])]
        ),
        json!([chat("low"), chat("high"), chat("max")])
    );
    assert_eq!(
        resolve(&pkg("moonshot/chat"), "kimi-k2.6", &[Support::Toggle]),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "enabled"}}},
        ])
    );
    let qwen =
        |e: &str| json!({"id": e, "settings": {"enableThinking": true, "reasoningEffort": e}});
    assert_eq!(
        resolve(
            &pkg("alibaba/chat"),
            "qwen3.8-max",
            &[
                Support::Toggle,
                effort(&["low", "medium", "xhigh"]),
                budget(Some(0), Some(262_144))
            ]
        ),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            qwen("low"),
            qwen("medium"),
            qwen("xhigh"),
        ])
    );
    assert_eq!(
        resolve_as(
            &pkg("alibaba/chat"),
            "qwen3.7-plus",
            Some(300_000),
            "test",
            &[Support::Toggle, budget(Some(0), Some(262_144))]
        ),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "high", "settings": {"enableThinking": true, "thinkingBudget": 32000}},
            {"id": "max", "settings": {"enableThinking": true, "thinkingBudget": 63999}},
        ])
    );
    let glm = |e: &str| json!({"id": e, "settings": {"thinking": {"type": "enabled", "clear_thinking": false}, "reasoningEffort": e}});
    assert_eq!(
        resolve(
            &pkg("zai/chat"),
            "glm-5.3",
            &[Support::Toggle, effort(&["low", "high", "max"])]
        ),
        json!([glm("low"), glm("high"), glm("max")])
    );
    assert_eq!(
        resolve(&pkg("zai/chat"), "glm-4.7", &[Support::Toggle]),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "enabled", "clear_thinking": false}}},
        ])
    );
}
