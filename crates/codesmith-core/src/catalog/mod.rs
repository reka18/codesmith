//! The model catalog from models.dev: providers, their models, and each
//! model's capabilities (modalities, tool calls, reasoning), limits, costs
//! and generated reasoning variants. Config resolution fills what a model's
//! config leaves unset from here (see [`crate::config`]).
//!
//! By default it never touches the network: the catalog is
//! `$CODESMITH_MODELS_PATH` (a catalog file) if set, else the cached copy
//! under the cache folder, else the snapshot bundled with the binary.
//! Fetching is opt-in (config's `catalog.fetch` or `catalog.url`,
//! `$CODESMITH_MODELS_FETCH=1` or `$CODESMITH_MODELS_URL`, or a one-shot
//! `codesmith models --refresh`), from `https://models.dev`'s v1
//! `/api.json` unless a mirror is named; `$CODESMITH_DISABLE_MODELS_FETCH=1`
//! forbids it outright. When on, long-running commands refresh in the
//! background when the cache is over five minutes old, revalidating with
//! the stored ETag. The request carries a generic `User-Agent` and nothing
//! about the user.
//!
//! Pinned to OpenCode's `models-dev.ts` and `plugin/models-dev.ts` by the
//! `oc_catalog_*` tests (`tests/oc_catalog.rs`).

mod json;
mod v1;
pub mod variant;

pub use json::Json;
pub use variant::{Budget, Reasoning, Variant};

use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::Digest;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// One provider of the catalog with its models.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub info: ProviderInfo,
    pub models: Vec<Model>,
    /// Environment variables that may hold its credentials, in order.
    pub environment: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub activation: String,
    /// The OpenCode package (entrypoint) most of its models use.
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Settings {
    #[serde(rename = "baseURL", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The real host behind a compatible package.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// `chat`, `decision`, `image` or `video`; only chat models are offered
    /// for sessions.
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<Compatibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// The model's own package, when it differs from the provider's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
    pub capabilities: Capabilities,
    pub variants: Vec<Variant>,
    pub time: Time,
    pub cost: Vec<Cost>,
    /// `alpha`, `beta`, `deprecated` or `active`.
    pub status: String,
    pub enabled: bool,
    pub limit: Limit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Map<String, Value>>,
    /// The reasoning controls the variants were generated from.
    #[serde(skip)]
    pub reasoning: Reasoning,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compatibility {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_input: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Capabilities {
    pub tools: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freeform: Option<Vec<String>>,
    pub input: Vec<String>,
    pub output: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Time {
    /// Release date, in milliseconds since the epoch (0 when unknown).
    pub released: i64,
}

/// Dollars per million tokens. The first entry is the default; the others
/// apply above a context size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Cost {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    pub input: f64,
    pub output: f64,
    pub cache: CacheCost,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct CacheCost {
    pub read: f64,
    pub write: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Tier {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub size: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Limit {
    pub context: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    pub output: u64,
}

impl Model {
    /// Takes and makes text (or says nothing about it).
    pub fn supports_text(&self) -> bool {
        let text = |v: &[String]| v.is_empty() || v.iter().any(|m| m.starts_with("text"));
        text(&self.capabilities.input) && text(&self.capabilities.output)
    }

    /// The default (untiered) price.
    pub fn base_cost(&self) -> Option<Cost> {
        self.cost.iter().find(|c| c.tier.is_none()).copied()
    }
}

// ---------------------------------------------------------------------------
// Reading models.dev's v2 shape. Only the fields CodeSmith reads are decoded;
// a model that doesn't match is skipped rather than failing the catalog.

/// The model types kept. Only chat models are offered for sessions.
pub const TYPES: [&str; 4] = ["chat", "decision", "image", "video"];

/// The experimental v2 API, limited to the kept model types.
pub const CATALOG_PATH: &str = "/experimental/v2.0/api.json?type=chat,decision,image,video";

/// The public models.dev catalog, fetched as its v1 `/api.json`.
pub const DEFAULT_SOURCE: &str = "https://models.dev";

/// The v1 catalog path, what models.dev serves.
pub const V1_PATH: &str = "/api.json";

/// Packages that build their URL from provider settings (region, resource,
/// account, project…): the catalog's URL for them is a template or an SDK
/// default, so it is not kept.
const BUILDS_OWN_URL: [&str; 15] = [
    "@opencode/ai/providers/amazon-bedrock",
    "@opencode/ai/providers/azure/chat",
    "@opencode/ai/providers/azure/images",
    "@opencode/ai/providers/azure/messages",
    "@opencode/ai/providers/azure/responses",
    "@opencode/ai/providers/cloudflare-ai-gateway",
    "@opencode/ai/providers/cloudflare-ai-gateway/decisions",
    "@opencode/ai/providers/cloudflare-workers-ai",
    "@opencode/ai/providers/cloudflare-workers-ai/decisions",
    "@opencode/ai/providers/cloudflare-workers-ai/images",
    "@opencode/ai/providers/google-vertex/gemini",
    "@opencode/ai/providers/google-vertex/images",
    "@opencode/ai/providers/google-vertex/messages",
    "aisdk:@jerome-benoit/sap-ai-provider-v2",
    "aisdk:watsonx-ai-provider",
];

const COMPATIBLE: [&str; 2] = [
    "@opencode/ai/providers/openai-compatible",
    "@opencode/ai/providers/anthropic-compatible",
];

/// The `@opencode/ai/providers/*` entrypoints OpenCode ships, the ones the
/// catalog may name. A newer catalog may name one this list lacks; such an
/// API counts as having no package.
const BUILTIN_PACKAGES: &[&str] = &[
    "alibaba",
    "alibaba/chat",
    "alibaba/messages",
    "alibaba/responses",
    "alibaba/images",
    "alibaba/embeddings",
    "amazon-bedrock",
    "amazon-bedrock/mantle",
    "amazon-bedrock/mantle/chat",
    "amazon-bedrock/mantle/messages",
    "amazon-bedrock/mantle/responses",
    "anthropic",
    "anthropic-compatible",
    "azure",
    "azure/chat",
    "azure/responses",
    "azure/images",
    "azure/embeddings",
    "azure/messages",
    "baseten",
    "baseten/decisions",
    "byteplus/videos",
    "cerebras",
    "cloudflare-ai-gateway",
    "cloudflare-ai-gateway/decisions",
    "cloudflare-workers-ai",
    "cloudflare-workers-ai/decisions",
    "cloudflare-workers-ai/images",
    "cohere",
    "cohere/chat",
    "cohere/embeddings",
    "deepinfra",
    "deepinfra/embeddings",
    "deepseek",
    "digitalocean",
    "digitalocean/decisions",
    "digitalocean/embeddings",
    "digitalocean/images",
    "digitalocean/responses",
    "fireworks",
    "fireworks/embeddings",
    "google",
    "google/decisions",
    "google/interactions",
    "google/embeddings",
    "google/images",
    "google/videos",
    "google-vertex",
    "google-vertex/chat",
    "google-vertex/gemini",
    "google-vertex/interactions",
    "google-vertex/messages",
    "google-vertex/mistral",
    "google-vertex/responses",
    "google-vertex/embeddings",
    "google-vertex/images",
    "google-vertex-messages",
    "groq",
    "meta",
    "meta/chat",
    "meta/images",
    "meta/messages",
    "meta/responses",
    "minimax",
    "minimax/chat",
    "minimax/images",
    "minimax/messages",
    "minimax/responses",
    "minimax/videos",
    "mistral",
    "moonshot",
    "moonshot/chat",
    "moonshot/messages",
    "moonshot/responses",
    "openai",
    "openai/chat",
    "openai/responses",
    "openai/images",
    "openai/embeddings",
    "openai-compatible",
    "openai-compatible/responses",
    "openai-compatible/images",
    "openai-compatible/decisions",
    "openai-compatible/embeddings",
    "openai-compatible-responses",
    "opencode/decisions",
    "openrouter",
    "openrouter/decisions",
    "openrouter/embeddings",
    "openrouter/images",
    "openrouter/videos",
    "openrouter/responses",
    "togetherai",
    "typesafe-ai/decisions",
    "venice",
    "venice/responses",
    "volcengine/videos",
    "vercel-ai-gateway",
    "vercel-ai-gateway/chat",
    "vercel-ai-gateway/messages",
    "vercel-ai-gateway/responses",
    "vercel-ai-gateway/decisions",
    "vercel-ai-gateway/images",
    "vercel-ai-gateway/embeddings",
    "xai",
    "xai/images",
    "xai/videos",
    "zai",
    "zai/chat",
    "zai/images",
    "zai-coding-plan",
    "zai-coding-plan/chat",
    "zai-coding-plan/messages",
    "zai-coding-plan/responses",
];

/// `aisdk:x` → `x`; the old `@opencode-ai/ai/` scope → `@opencode/ai/`.
pub fn package_name(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("@opencode-ai/ai/") {
        return format!("@opencode/ai/{rest}");
    }
    value.strip_prefix("aisdk:").unwrap_or(value).to_string()
}

fn builtin_package(name: &str) -> bool {
    name.strip_prefix("@opencode/ai/providers/")
        .is_some_and(|p| BUILTIN_PACKAGES.contains(&p))
}

/// AI SDK packages and the native packages that replace them (OpenCode's
/// `AISDKNative.native`).
fn native(npm: &str, provider: &str, model: &str, shape: Option<&str>) -> Option<String> {
    let p = |s: &str| Some(format!("@opencode/ai/providers/{s}"));
    let protocols = |name: &str| match npm {
        "@ai-sdk/openai-compatible" => p(&format!("{name}/chat")),
        "@ai-sdk/anthropic" => p(&format!("{name}/messages")),
        "@ai-sdk/openai" => p(&format!("{name}/responses")),
        _ => None,
    };
    let host = match provider {
        "alibaba"
        | "alibaba-cn"
        | "alibaba-coding-plan"
        | "alibaba-coding-plan-cn"
        | "alibaba-token-plan"
        | "alibaba-token-plan-cn" => protocols("alibaba"),
        "kimi-for-coding" | "moonshotai" | "moonshotai-cn" => protocols("moonshot"),
        "meta" => protocols("meta"),
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan" => {
            protocols("minimax")
        }
        "zai-coding-plan" | "zhipuai-coding-plan" => protocols("zai-coding-plan"),
        "baseten" if npm == "@ai-sdk/openai-compatible" => p("baseten"),
        "cloudflare-ai-gateway"
            if [
                "@ai-sdk/anthropic",
                "@ai-sdk/openai",
                "@ai-sdk/openai-compatible",
                "ai-gateway-provider",
            ]
            .contains(&npm) =>
        {
            p("cloudflare-ai-gateway")
        }
        "cohere" if npm == "@ai-sdk/openai-compatible" => p("cohere/chat"),
        "cloudflare-workers-ai" if npm == "@ai-sdk/openai-compatible" => p("cloudflare-workers-ai"),
        "deepseek" if npm == "@ai-sdk/openai-compatible" => p("deepseek"),
        "digitalocean" if npm == "@ai-sdk/openai-compatible" => p("digitalocean"),
        "fireworks-ai" if npm == "@ai-sdk/openai-compatible" => p("fireworks"),
        "google-vertex" if npm == "@ai-sdk/openai-compatible" => p("google-vertex/chat"),
        "google-vertex" if npm == "@ai-sdk/mistral" => p("google-vertex/mistral"),
        "zai" | "zhipuai" if npm == "@ai-sdk/openai-compatible" => p("zai/chat"),
        _ => None,
    };
    if host.is_some() {
        return host;
    }
    if npm == "@ai-sdk/amazon-bedrock/mantle" {
        return Some(if model.contains("anthropic.") {
            "@opencode/ai/providers/amazon-bedrock/mantle/messages".into()
        } else if model.contains("gpt-oss") {
            "@opencode/ai/providers/amazon-bedrock/mantle/chat".into()
        } else {
            "@opencode/ai/providers/amazon-bedrock/mantle/responses".into()
        });
    }
    if npm == "@ai-sdk/azure" && shape == Some("completions") {
        return p("azure/chat");
    }
    let mapped = match npm {
        "@ai-sdk/amazon-bedrock" => "amazon-bedrock",
        "@ai-sdk/alibaba" => "alibaba/chat",
        "@ai-sdk/anthropic" => "anthropic",
        "@ai-sdk/azure" => "azure/responses",
        "@ai-sdk/cerebras" => "cerebras",
        "@ai-sdk/cohere" => "cohere",
        "@ai-sdk/deepinfra" => "deepinfra",
        "@ai-sdk/google" => "google",
        "@ai-sdk/google-vertex" => "google-vertex",
        "@ai-sdk/google-vertex/anthropic" => "google-vertex/messages",
        "@ai-sdk/groq" => "groq",
        "@ai-sdk/mistral" => "mistral",
        "@ai-sdk/openai" => "openai",
        "@ai-sdk/openai-compatible" => "openai-compatible",
        "@ai-sdk/togetherai" => "togetherai",
        "@ai-sdk/xai" => "xai",
        "@ai-sdk/gateway" => "vercel-ai-gateway",
        "@openrouter/ai-sdk-provider" => "openrouter",
        "ai-gateway-provider" => "cloudflare-ai-gateway",
        "venice-ai-sdk-provider" => "venice",
        _ => return None,
    };
    p(mapped)
}

/// The CodeSmith protocol a package speaks, for the packages CodeSmith can
/// run; `None` for the rest (Bedrock, Vertex, Azure, gateways with their
/// own signing, image and video endpoints, …).
pub fn protocol(package: &str) -> Option<codesmith_ai::Protocol> {
    use codesmith_ai::Protocol::*;
    if package == "aisdk:@ai-sdk/openai-compatible" {
        return Some(OpenaiChat);
    }
    let p = package_name(package);
    let p = p.strip_prefix("@opencode/ai/providers/")?;
    Some(match p {
        "openai"
        | "openai/responses"
        | "openai-compatible/responses"
        | "openai-compatible-responses"
        | "xai"
        | "openrouter/responses"
        | "digitalocean/responses"
        | "venice/responses"
        | "alibaba/responses"
        | "minimax/responses"
        | "moonshot/responses"
        | "meta"
        | "meta/responses"
        | "zai-coding-plan/responses" => OpenaiResponses,
        "openai-compatible"
        | "openai/chat"
        | "groq"
        | "cerebras"
        | "deepseek"
        | "fireworks"
        | "mistral"
        | "togetherai"
        | "openrouter"
        | "digitalocean"
        | "deepinfra"
        | "venice"
        | "moonshot"
        | "moonshot/chat"
        | "alibaba"
        | "alibaba/chat"
        | "zai"
        | "zai/chat"
        | "zai-coding-plan"
        | "zai-coding-plan/chat"
        | "minimax/chat"
        | "meta/chat"
        | "baseten"
        | "cloudflare-workers-ai"
        | "cohere/chat" => OpenaiChat,
        "anthropic"
        | "anthropic-compatible"
        | "minimax"
        | "minimax/messages"
        | "moonshot/messages"
        | "alibaba/messages"
        | "zai-coding-plan/messages"
        | "meta/messages" => AnthropicMessages,
        "google" | "google-vertex" | "google-vertex/gemini" => GoogleGemini,
        "google-vertex/messages" | "google-vertex-messages" | "amazon-bedrock/mantle/messages" => {
            AnthropicMessages
        }
        "google-vertex/chat" | "amazon-bedrock/mantle/chat" => OpenaiChat,
        "google-vertex/responses" | "amazon-bedrock/mantle" | "amazon-bedrock/mantle/responses" => {
            OpenaiResponses
        }
        "amazon-bedrock" => BedrockConverse,
        _ => return None,
    })
}

/// The package a protocol's requests are spelled for, when a model moves
/// to a protocol its catalog package does not speak.
pub fn protocol_package(p: codesmith_ai::Protocol) -> &'static str {
    use codesmith_ai::Protocol::*;
    match p {
        OpenaiChat => "@opencode/ai/providers/openai/chat",
        OpenaiResponses => "@opencode/ai/providers/openai/responses",
        AnthropicMessages => "@opencode/ai/providers/anthropic",
        GoogleGemini => "@opencode/ai/providers/google",
        BedrockConverse => "@opencode/ai/providers/amazon-bedrock",
        ClaudeAgent => "@opencode/claude-agent",
    }
}

struct Route {
    package: String,
    freeform: Option<Vec<String>>,
    base_url: Option<String>,
    compatibility: Option<Compatibility>,
}

struct Api {
    base_url: Option<String>,
    freeform: Option<Vec<String>>,
    aisdk: Option<String>,
    opencodeai: Option<String>,
    compatibility: Option<(Option<String>, Option<String>, Option<String>)>,
}

/// `Some(None)` for an absent field, `None` for one of the wrong type.
fn opt<'a, T>(v: &'a Json, key: &str, f: impl Fn(&'a Json) -> Option<T>) -> Option<Option<T>> {
    match v.get(key) {
        None => Some(None),
        Some(x) => f(x).map(Some),
    }
}

fn decode_api(v: &Json) -> Option<Api> {
    if !v.is_object() {
        return None;
    }
    let base_url = opt(v, "base_url", |x| x.str().map(str::to_string))?;
    let freeform = match v.get("capabilities") {
        None => None,
        Some(c) => {
            if !c.is_object() {
                return None;
            }
            match c.get("tools") {
                None => None,
                Some(t) => {
                    if !t.is_object() {
                        return None;
                    }
                    opt(t, "freeform", Json::strings)?
                }
            }
        }
    };
    let (aisdk, opencodeai) = match v.get("sdk") {
        None => (None, None),
        Some(s) if s.is_object() => (
            opt(s, "aisdk", |x| x.str().map(str::to_string))?,
            opt(s, "opencodeai", |x| x.str().map(str::to_string))?,
        ),
        Some(_) => return None,
    };
    let compatibility = match v.get("compatibility") {
        None => None,
        Some(c) if c.is_object() => {
            let s = |k: &str| opt(c, k, |x| x.str().map(str::to_string));
            Some((
                s("reasoning_field")?,
                s("max_tokens_field")?,
                s("media_input")?,
            ))
        }
        Some(_) => return None,
    };
    Some(Api {
        base_url,
        freeform,
        aisdk,
        opencodeai,
        compatibility,
    })
}

/// A newer catalog may name an `@opencode/ai` entrypoint this build doesn't
/// know; that counts as no package.
fn loadable(opencodeai: Option<&str>) -> Option<String> {
    let o = opencodeai?;
    let name = package_name(o);
    if !name.starts_with("@opencode/ai/") || builtin_package(&name) {
        Some(o.to_string())
    } else {
        None
    }
}

fn route(provider: &str, model: &SourceModel) -> Option<Route> {
    // A chat model uses its first API with a package. Other types only run
    // through the entrypoint for their type (`providers/<P>/images`).
    let apis: Vec<(&str, Api, Option<String>)> = model
        .api
        .iter()
        .filter_map(|(protocol, v)| {
            let api = decode_api(v)?;
            let o = loadable(api.opencodeai.as_deref());
            Some((protocol.as_str(), api, o))
        })
        .collect();
    let suffix = format!("/{}s", model.kind);
    let (protocol, api, opencodeai) = apis.into_iter().find(|(_, api, o)| {
        if model.kind == "chat" {
            o.is_some() || api.aisdk.is_some()
        } else {
            o.as_deref().is_some_and(|o| o.ends_with(&suffix))
        }
    })?;
    let package = match opencodeai {
        Some(o) => o,
        None => {
            let aisdk = api.aisdk.as_deref()?;
            let shape = match protocol {
                "responses" => Some("responses"),
                "chat-completions" => Some("completions"),
                _ => None,
            };
            native(aisdk, provider, &model.id, shape).unwrap_or_else(|| format!("aisdk:{aisdk}"))
        }
    };
    let compatibility = api.compatibility.and_then(|(r, m, media)| {
        let c = Compatibility {
            reasoning_field: r.filter(|r| !r.is_empty()),
            max_tokens_field: m.filter(|m| m == "max_completion_tokens" || m == "max_tokens"),
            media_input: media.filter(|m| ["assets", "state", "root"].contains(&m.as_str())),
        };
        (c != Compatibility::default()).then_some(c)
    });
    let base_url = api
        .base_url
        .filter(|u| !u.is_empty() && !BUILDS_OWN_URL.contains(&package.as_str()));
    Some(Route {
        freeform: api.freeform.map(|f| {
            f.into_iter()
                .filter(|x| ["text", "regex", "lark"].contains(&x.as_str()))
                .collect()
        }),
        package,
        base_url,
        compatibility,
    })
}

struct SourceModel {
    id: String,
    kind: String,
    name: String,
    family: Option<String>,
    release_date: String,
    status: Option<String>,
    input: Vec<String>,
    output: Vec<String>,
    tools: bool,
    reasoning: Reasoning,
    temperature: Option<bool>,
    limit: (Option<f64>, Option<f64>, Option<f64>),
    cost: Option<SourceCost>,
    api: Vec<(String, Json)>,
    modes: Vec<(String, Json)>,
}

#[derive(Clone, Default)]
struct CachePrices {
    read: Option<f64>,
    write: Option<f64>,
    explicit_write: Option<f64>,
}

#[derive(Clone)]
struct Override {
    when: Vec<(String, Json)>,
    price: Option<f64>,
    cache: Option<CachePrices>,
}

#[derive(Clone)]
struct PriceSet {
    unit: Option<String>,
    price: f64,
    cache: Option<CachePrices>,
    overrides: Vec<Override>,
}

#[derive(Clone, Default)]
struct SourceCost {
    input: Option<PriceSet>,
    output: Option<PriceSet>,
    multipliers: Vec<(Vec<(String, Json)>, Json)>,
}

fn decode_reasoning(v: &Json) -> Option<Reasoning> {
    match v.get("supported")?.bool()? {
        false => Some(Reasoning::default()),
        true => {
            let int = |x: &Json| x.num().filter(|n| n.fract() == 0.0).map(|n| n as i64);
            let budget = match v.get("budget") {
                None => None,
                Some(b) if b.is_object() => Some(Budget {
                    min: opt(b, "min", int)?,
                    max: opt(b, "max", int)?,
                }),
                Some(_) => return None,
            };
            Some(Reasoning {
                supported: true,
                toggle: opt(v, "toggle", Json::bool)?,
                effort: opt(v, "effort", Json::strings)?,
                budget,
            })
        }
    }
}

/// A cache write price: a number, or one per TTL (the first is kept).
fn cache_write(v: &Json) -> Option<f64> {
    match v {
        Json::Object(o) => {
            let prices: Option<Vec<f64>> = o.iter().map(|(_, p)| p.num()).collect();
            Some(prices?.first().copied().unwrap_or(f64::NAN))
        }
        _ => v.num(),
    }
}

fn decode_cache(v: &Json) -> Option<CachePrices> {
    if !v.is_object() {
        return None;
    }
    let write = |x: &Json| cache_write(x).map(|w| (!w.is_nan()).then_some(w));
    let explicit_write = match v.get("explicit") {
        None => None,
        Some(e) if e.is_object() => opt(e, "write", write)?.flatten(),
        Some(_) => return None,
    };
    Some(CachePrices {
        read: opt(v, "read", Json::num)?,
        write: opt(v, "write", write)?.flatten(),
        explicit_write,
    })
}

fn decode_prices(v: &Json, cache: bool) -> Option<PriceSet> {
    if !v.is_object() {
        return None;
    }
    let overrides = match v.get("overrides") {
        None => vec![],
        Some(o) => o
            .array()?
            .iter()
            .map(|o| {
                Some(Override {
                    when: o.get("when")?.entries()?.to_vec(),
                    price: opt(o, "price", Json::num)?,
                    cache: if cache {
                        opt(o, "cache", decode_cache)?
                    } else {
                        None
                    },
                })
            })
            .collect::<Option<_>>()?,
    };
    Some(PriceSet {
        unit: opt(v, "unit", |x| x.str().map(str::to_string))?,
        price: v.get("price")?.num()?,
        cache: if cache {
            opt(v, "cache", decode_cache)?
        } else {
            None
        },
        overrides,
    })
}

fn valid_factor(v: &Json) -> bool {
    match v {
        Json::Object(o) => o.iter().all(|(_, f)| valid_factor(f)),
        _ => v.num().is_some(),
    }
}

fn decode_cost(v: &Json) -> Option<SourceCost> {
    if !v.is_object() {
        return None;
    }
    let (input, output) = match v.get("text") {
        None => (None, None),
        Some(t) if t.is_object() => (
            opt(t, "input", |x| decode_prices(x, true))?,
            opt(t, "output", |x| decode_prices(x, false))?,
        ),
        Some(_) => return None,
    };
    let multipliers = match v.get("multipliers") {
        None => vec![],
        Some(m) => m
            .array()?
            .iter()
            .map(|m| {
                let f = m.get("factor")?;
                valid_factor(f).then_some(())?;
                Some((m.get("when")?.entries()?.to_vec(), f.clone()))
            })
            .collect::<Option<_>>()?,
    };
    Some(SourceCost {
        input,
        output,
        multipliers,
    })
}

fn decode_model(v: &Json) -> Option<SourceModel> {
    let s = |k: &str| v.get(k)?.str().map(str::to_string);
    let os = |k: &str| opt(v, k, |x| x.str().map(str::to_string));
    let modalities = v.get("modalities")?;
    let caps = v.get("capabilities")?;
    let limit = match v.get("limit") {
        None => (None, None, None),
        Some(l) if l.is_object() => (
            opt(l, "context", Json::num)?,
            opt(l, "input", Json::num)?,
            opt(l, "output", Json::num)?,
        ),
        Some(_) => return None,
    };
    let modes = match v.get("experimental") {
        None => vec![],
        Some(e) if e.is_object() => match e.get("modes") {
            None => vec![],
            Some(m) => m.entries()?.to_vec(),
        },
        Some(_) => return None,
    };
    Some(SourceModel {
        id: s("id")?,
        kind: s("type")?,
        name: s("name")?,
        family: os("family")?,
        release_date: s("release_date")?,
        status: os("status")?,
        input: modalities.get("input")?.strings()?,
        output: modalities.get("output")?.strings()?,
        tools: caps.get("tools")?.get("supported")?.bool()?,
        reasoning: decode_reasoning(caps.get("reasoning")?)?,
        temperature: opt(caps, "temperature", Json::bool)?,
        limit,
        cost: opt(v, "cost", decode_cost)?,
        api: v.get("api")?.entries()?.to_vec(),
        modes,
    })
}

struct Mode {
    service_tier: Option<String>,
    cost: Option<SourceCost>,
    body: Option<Map<String, Value>>,
    headers: Option<Map<String, Value>>,
}

fn decode_mode(v: &Json) -> Option<Mode> {
    if !v.is_object() {
        return None;
    }
    let record = |x: &Json| x.is_object().then(|| x.to_value().as_object().cloned())?;
    let headers = |x: &Json| {
        let m = record(x)?;
        m.values().all(Value::is_string).then_some(m)
    };
    Some(Mode {
        service_tier: opt(v, "service_tier", |x| x.str().map(str::to_string))?,
        cost: opt(v, "cost", decode_cost)?,
        body: opt(v, "body", record)?,
        headers: opt(v, "headers", headers)?,
    })
}

fn most_common<T: PartialEq + Clone>(values: &[T]) -> Option<T> {
    let mut counts: Vec<(T, usize)> = Vec::new();
    for v in values {
        match counts.iter_mut().find(|(x, _)| x == v) {
            Some(c) => c.1 += 1,
            None => counts.push((v.clone(), 1)),
        }
    }
    let mut best: Option<(T, usize)> = None;
    for (v, n) in counts {
        if best.as_ref().is_none_or(|b| n > b.1) {
            best = Some((v, n));
        }
    }
    best.map(|b| b.0)
}

/// `YYYY-MM-DD`, `YYYY-MM`, `YYYY` or an RFC 3339 time, in milliseconds
/// since the epoch; 0 when it doesn't parse (as `Date.parse`'s NaN is).
pub fn released(date: &str) -> i64 {
    use chrono::NaiveDate;
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .or_else(|_| NaiveDate::parse_from_str(&format!("{date}-01"), "%Y-%m-%d"))
        .or_else(|_| NaiveDate::parse_from_str(&format!("{date}-01-01"), "%Y-%m-%d"));
    if let Ok(d) = day {
        // `get`: chrono skips leading whitespace, which may be multibyte.
        if date
            .get(..4)
            .is_some_and(|y| y.chars().all(|c| c.is_ascii_digit()))
        {
            return d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
        }
    }
    chrono::DateTime::parse_from_rfc3339(date).map_or(0, |t| t.timestamp_millis())
}

fn modality(v: &str) -> String {
    if v == "application/pdf" {
        return "pdf".into();
    }
    let kind = v.split('/').next().unwrap_or(v);
    if ["text", "image", "audio", "video"].contains(&kind) {
        kind.into()
    } else {
        v.into()
    }
}

fn modalities(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in values {
        let m = modality(v);
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

fn zero_cost() -> Vec<Cost> {
    vec![Cost::default()]
}

/// The one cache write price kept: the first TTL listed, else the explicit
/// cache's.
fn write_price(c: Option<&CachePrices>) -> Option<f64> {
    c.and_then(|c| c.write.or(c.explicit_write))
}

/// Only per-token text prices are kept. Context overrides become tiers;
/// other conditions (service tier, time of day…) are left out.
fn cost(input: Option<&SourceCost>) -> Vec<Cost> {
    let per_token = |u: &Option<String>| u.as_deref().is_none_or(|u| u == "token");
    let (Some(ti), Some(to)) = (
        input.and_then(|c| c.input.as_ref()),
        input.and_then(|c| c.output.as_ref()),
    ) else {
        return zero_cost();
    };
    if !per_token(&ti.unit) || !per_token(&to.unit) {
        return zero_cost();
    }
    let base = Cost {
        tier: None,
        input: ti.price,
        output: to.price,
        cache: CacheCost {
            read: ti.cache.as_ref().and_then(|c| c.read).unwrap_or(0.0),
            write: write_price(ti.cache.as_ref()).unwrap_or(0.0),
        },
    };
    // An override merges field by field, so a tier keeps every base price it doesn't set.
    let mut tiers: Vec<Cost> = Vec::new();
    let size = |when: &[(String, Json)]| -> Option<f64> {
        if when.len() != 1 || when[0].0 != "context" {
            return None;
        }
        let c = &when[0].1;
        if !c.is_object() {
            return None;
        }
        let gt = opt(c, "gt", Json::num)?;
        let gte = opt(c, "gte", Json::num)?;
        gt.or(gte)
    };
    let mut tier = |when: &[(String, Json)]| -> Option<usize> {
        let s = size(when)?;
        if let Some(i) = tiers
            .iter()
            .position(|t| t.tier.is_some_and(|t| t.size == s))
        {
            return Some(i);
        }
        tiers.push(Cost {
            tier: Some(Tier {
                kind: "context",
                size: s,
            }),
            ..base
        });
        Some(tiers.len() - 1)
    };
    let mut slots = Vec::new();
    for o in &ti.overrides {
        slots.push((tier(&o.when), o, true));
    }
    for o in &to.overrides {
        slots.push((tier(&o.when), o, false));
    }
    for (slot, o, is_input) in slots {
        let Some(i) = slot else { continue };
        let entry = &mut tiers[i];
        if is_input {
            entry.input = o.price.unwrap_or(entry.input);
            entry.cache.read = o
                .cache
                .as_ref()
                .and_then(|c| c.read)
                .unwrap_or(entry.cache.read);
            entry.cache.write = write_price(o.cache.as_ref()).unwrap_or(entry.cache.write);
        } else {
            entry.output = o.price.unwrap_or(entry.output);
        }
    }
    let mut out = vec![base];
    out.extend(tiers);
    out
}

/// Prices an object factor doesn't name keep factor 1.
fn factor_at(factor: &Json, path: &[&str]) -> f64 {
    let mut node = Some(factor);
    for key in path {
        node = match node {
            Some(n @ Json::Object(_)) => n.get(key),
            other => other,
        };
    }
    match node {
        None => 1.0,
        Some(Json::Object(o)) => o.first().and_then(|(_, v)| v.num()).unwrap_or(1.0),
        Some(n) => n.num().unwrap_or(1.0),
    }
}

/// A mode without its own cost takes the base prices scaled by its service
/// tier's multiplier (the mode name by default). Base context tiers stay.
fn mode_cost(model: &SourceModel, base: &[Cost], name: &str, mode: &Mode) -> Vec<Cost> {
    let tiers = base.get(1..).unwrap_or_default();
    if let Some(c) = &mode.cost {
        let mut out = vec![cost(Some(c))[0]];
        out.extend_from_slice(tiers);
        return out;
    }
    let tier = mode.service_tier.as_deref().unwrap_or(name);
    let factor = model.cost.as_ref().and_then(|c| {
        c.multipliers.iter().find_map(|(when, f)| {
            if when.len() != 1 || when[0].0 != "service_tier" {
                return None;
            }
            let equals = when[0].1.get("equals").and_then(Json::str);
            (equals == Some(tier)).then_some(f)
        })
    });
    let (Some(factor), Some(d)) = (factor, base.first()) else {
        return base.to_vec();
    };
    let scale = |price: f64, path: &[&str]| (price * factor_at(factor, path) * 1e10).round() / 1e10;
    let mut out = vec![Cost {
        tier: None,
        input: scale(d.input, &["text", "input", "price"]),
        output: scale(d.output, &["text", "output", "price"]),
        cache: CacheCost {
            read: scale(d.cache.read, &["text", "input", "cache", "read"]),
            write: scale(d.cache.write, &["text", "input", "cache", "write"]),
        },
    }];
    out.extend_from_slice(tiers);
    out
}

#[allow(clippy::too_many_arguments)]
fn model_info(
    provider_id: &str,
    provider_package: &str,
    provider_url: Option<&str>,
    id: String,
    model: &SourceModel,
    route: &Route,
    name: Option<String>,
    cost: Vec<Cost>,
    request: Option<&Mode>,
    variants: Vec<Variant>,
) -> Model {
    let settings = Settings {
        base_url: route
            .base_url
            .clone()
            .filter(|u| Some(u.as_str()) != provider_url),
        provider: COMPATIBLE
            .contains(&route.package.as_str())
            .then(|| provider_id.to_string()),
    };
    let status = match model.status.as_deref() {
        Some(s @ ("alpha" | "beta" | "deprecated")) => s.to_string(),
        _ => "active".into(),
    };
    let limit = |v: Option<f64>| v.map(|n| n.max(0.0) as u64);
    Model {
        id,
        model_id: model.id.clone(),
        provider_id: provider_id.to_string(),
        kind: model.kind.clone(),
        name: name.unwrap_or_else(|| model.name.clone()),
        compatibility: route.compatibility.clone(),
        family: model.family.clone(),
        package: (route.package != provider_package).then(|| route.package.clone()),
        settings: (settings != Settings::default()).then_some(settings),
        capabilities: Capabilities {
            tools: model.tools,
            freeform: route.freeform.clone(),
            input: modalities(&model.input),
            output: modalities(&model.output),
            temperature: model.temperature,
        },
        variants,
        time: Time {
            released: released(&model.release_date),
        },
        cost,
        status,
        enabled: true,
        limit: Limit {
            context: limit(model.limit.0).unwrap_or(0),
            input: limit(model.limit.1),
            output: limit(model.limit.2).unwrap_or(0),
        },
        headers: request.and_then(|m| m.headers.clone()),
        body: request.and_then(|m| m.body.clone()),
        reasoning: model.reasoning.clone(),
    }
}

/// Normalize a models.dev catalog (v2, or v1 converted to v2).
pub fn normalize(catalog: &Json) -> Vec<Snapshot> {
    let converted;
    let catalog = if v1::is_v1(catalog) {
        converted = v1::to_v2(catalog);
        &converted
    } else {
        catalog
    };
    let mut out = Vec::new();
    for (_, item) in catalog.entries().unwrap_or_default() {
        let (Some(pid), Some(pname), Some(env), Some(models)) = (
            item.get("id").and_then(Json::str),
            item.get("name").and_then(Json::str),
            item.get("env").and_then(Json::strings),
            item.get("models").and_then(Json::entries),
        ) else {
            continue;
        };
        let sources: Vec<(SourceModel, Route)> = models
            .iter()
            .filter_map(|(_, v)| {
                let m = decode_model(v)?;
                if !TYPES.contains(&m.kind.as_str()) {
                    return None;
                }
                let r = route(pid, &m)?;
                Some((m, r))
            })
            .collect();
        if sources.is_empty() {
            continue;
        }
        // The provider defaults are what most of its models use. A model that
        // builds its own URL would inherit a provider baseURL, so then every
        // model carries its own instead.
        let packages: Vec<String> = sources.iter().map(|s| s.1.package.clone()).collect();
        let Some(package) = most_common(&packages) else {
            continue;
        };
        let urls: Vec<Option<String>> = sources.iter().map(|s| s.1.base_url.clone()).collect();
        let base_url = if urls.iter().all(Option::is_some) {
            most_common(&urls).flatten()
        } else {
            None
        };
        let info = ProviderInfo {
            id: pid.to_string(),
            name: pname.to_string(),
            activation: "auto".into(),
            package: package.clone(),
            settings: base_url.clone().map(|u| Settings {
                base_url: Some(u),
                provider: None,
            }),
        };
        let mut list = Vec::new();
        for (m, r) in &sources {
            let base_cost = cost(m.cost.as_ref());
            let variants = variant::resolve(
                &variant::Target {
                    package: Some(&r.package),
                    provider_id: pid,
                    model_id: &m.id,
                    output: m.limit.2.unwrap_or(0.0) as i64,
                },
                &variant::supports(&m.reasoning),
            );
            let url = base_url.as_deref();
            list.push(model_info(
                pid,
                &package,
                url,
                m.id.clone(),
                m,
                r,
                None,
                base_cost.clone(),
                None,
                variants.clone(),
            ));
            for (mode, v) in &m.modes {
                let Some(options) = decode_mode(v) else {
                    continue;
                };
                let mut chars = mode.chars();
                let title = match chars.next() {
                    Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                };
                list.push(model_info(
                    pid,
                    &package,
                    url,
                    format!("{}-{mode}", m.id),
                    m,
                    r,
                    Some(format!("{} {title}", m.name)),
                    mode_cost(m, &base_cost, mode, &options),
                    Some(&options),
                    variants.clone(),
                ));
            }
        }
        out.push(Snapshot {
            info,
            models: list,
            environment: env,
        });
    }
    out
}

/// Parse and normalize catalog text.
pub fn decode_snapshot(text: &str) -> serde_json::Result<Vec<Snapshot>> {
    Ok(normalize(&Json::parse(text)?))
}

/// Whether catalog text (fetched, so untrusted) is a JSON object that
/// normalizes in full without panicking. A catalog is only cached once it
/// passes, so a bad one cannot fail every later start.
pub fn usable(text: &str) -> bool {
    std::panic::catch_unwind(|| {
        index(text).is_some()
            && Json::parse(text).is_ok_and(|j| {
                j.is_object() && {
                    let _ = offered(&normalize(&j));
                    true
                }
            })
    })
    .unwrap_or(false)
}

/// Stamped on a cache entry that passed [`usable`] in this build; an entry
/// without it is checked again before it is used.
const CHECKED: &str = concat!("codesmith ", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// What CodeSmith offers from the catalog (OpenCode's models.dev plugin).

/// Catalog entries Bedrock Runtime only serves through inference profiles.
const BEDROCK_PROFILE_ONLY: [&str; 19] = [
    "amazon.nova-2-lite-v1:0",
    "anthropic.claude-fable-5",
    "anthropic.claude-fable-5-1",
    "anthropic.claude-haiku-4-5-20251001-v1:0",
    "anthropic.claude-haiku-5-5",
    "anthropic.claude-opus-4-1-20250805-v1:0",
    "anthropic.claude-opus-4-5-20251101-v1:0",
    "anthropic.claude-opus-4-7",
    "anthropic.claude-opus-4-8",
    "anthropic.claude-opus-5",
    "anthropic.claude-sonnet-4-5-20250929-v1:0",
    "anthropic.claude-sonnet-5",
    "anthropic.claude-sonnet-5-5",
    "deepseek.r1-v1:0",
    "meta.llama4-maverick-17b-instruct-v1:0",
    "meta.llama4-scout-17b-instruct-v1:0",
    "mistral.pixtral-large-2502-v1:0",
    "writer.palmyra-x4-v1:0",
    "writer.palmyra-x5-v1:0",
];

/// Deprecated aliases replaced by the canonical Azure and Vertex providers.
const LEGACY_ALIASES: [&str; 2] = ["azure-cognitive-services", "google-vertex-anthropic"];

/// The catalog as offered: legacy provider aliases, deprecated models,
/// chat models that don't do text and Bedrock profile-only models left out.
pub fn offered(data: &[Snapshot]) -> Vec<Snapshot> {
    data.iter()
        .filter(|p| !LEGACY_ALIASES.contains(&p.info.id.as_str()))
        .map(|p| Snapshot {
            models: p
                .models
                .iter()
                .filter(|m| {
                    m.status != "deprecated"
                        && (m.kind != "chat" || m.supports_text())
                        && !(p.info.id == "amazon-bedrock"
                            && BEDROCK_PROFILE_ONLY.contains(&m.model_id.as_str()))
                })
                .cloned()
                .collect(),
            ..p.clone()
        })
        .collect()
}

/// A loaded catalog, as offered, with lookups. A catalog read from text
/// is indexed by provider and each provider normalized on first use, so a
/// model lookup reads only its provider's part of a multi-megabyte catalog.
#[derive(Debug, Default)]
pub struct Catalog {
    text: Option<Arc<str>>,
    slots: Vec<Slot>,
}

#[derive(Debug)]
struct Slot {
    key: String,
    range: std::ops::Range<usize>,
    value: OnceLock<Option<Snapshot>>,
}

/// A catalog's top-level entries as byte ranges of `text`, in order;
/// `None` when it is not a JSON object.
fn index(text: &str) -> Option<Vec<(String, std::ops::Range<usize>)>> {
    use serde::de::{Deserializer, MapAccess, Visitor};
    use serde_json::value::RawValue;
    struct Top<'a>(Vec<(String, &'a RawValue)>);
    impl<'de> serde::Deserialize<'de> for Top<'de> {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> Visitor<'de> for V {
                type Value = Top<'de>;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a catalog object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Top<'de>, A::Error> {
                    let mut out: Vec<(String, &'de RawValue)> = Vec::new();
                    let mut at: HashMap<String, usize> = HashMap::new();
                    while let Some((k, v)) = m.next_entry::<String, &'de RawValue>()? {
                        match at.get(&k) {
                            Some(&i) => out[i].1 = v,
                            None => {
                                at.insert(k.clone(), out.len());
                                out.push((k, v));
                            }
                        }
                    }
                    Ok(Top(out))
                }
            }
            d.deserialize_map(V)
        }
    }
    let top: Top = serde_json::from_str(text).ok()?;
    let base = text.as_ptr() as usize;
    Some(
        top.0
            .into_iter()
            .map(|(k, v)| {
                let start = v.get().as_ptr() as usize - base;
                (k, start..start + v.get().len())
            })
            .collect(),
    )
}

impl Catalog {
    pub fn new(data: &[Snapshot]) -> Catalog {
        Catalog {
            text: None,
            slots: offered(data)
                .into_iter()
                .map(|p| Slot {
                    key: p.info.id.clone(),
                    range: 0..0,
                    value: OnceLock::from(Some(p)),
                })
                .collect(),
        }
    }

    /// A catalog read lazily from its text (keyed by provider id, as
    /// models.dev's are); `None` when the text is not a JSON object.
    pub fn from_text(text: Arc<str>) -> Option<Catalog> {
        let slots = index(&text)?
            .into_iter()
            .map(|(key, range)| Slot {
                key,
                range,
                value: OnceLock::new(),
            })
            .collect();
        Some(Catalog {
            text: Some(text),
            slots,
        })
    }

    fn load<'a>(&'a self, slot: &'a Slot) -> Option<&'a Snapshot> {
        slot.value
            .get_or_init(|| {
                let text = self.text.as_deref()?;
                let v = Json::parse(&text[slot.range.clone()]).ok()?;
                // A provider that cannot be read is left out, never a crash.
                std::panic::catch_unwind(|| {
                    let one = normalize(&Json::Object(vec![(slot.key.clone(), v)]));
                    offered(&one).into_iter().next()
                })
                .ok()
                .flatten()
            })
            .as_ref()
    }

    pub fn provider(&self, id: &str) -> Option<&Snapshot> {
        let slot = self.slots.iter().find(|s| s.key == id)?;
        self.load(slot).filter(|p| p.info.id == id)
    }

    /// Every provider (each normalized now if it wasn't yet).
    pub fn providers(&self) -> impl Iterator<Item = &Snapshot> {
        self.slots.iter().filter_map(|s| self.load(s))
    }

    /// A provider's key variables, reading only that field when the
    /// provider hasn't been loaded.
    pub fn environment(&self, id: &str) -> Vec<String> {
        #[derive(serde::Deserialize)]
        struct Env {
            #[serde(default)]
            env: Vec<String>,
        }
        let Some(slot) = self.slots.iter().find(|s| s.key == id) else {
            return vec![];
        };
        if let Some(p) = slot.value.get() {
            return p
                .as_ref()
                .map(|p| p.environment.clone())
                .unwrap_or_default();
        }
        self.text
            .as_deref()
            .and_then(|t| serde_json::from_str::<Env>(&t[slot.range.clone()]).ok())
            .map(|e| e.env)
            .unwrap_or_default()
    }

    /// Provider ids, without reading the providers.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().map(|s| s.key.as_str())
    }

    /// A chat model, by its catalog id. Other types are only listed by type.
    pub fn model(&self, provider: &str, id: &str) -> Option<&Model> {
        self.provider(provider)?
            .models
            .iter()
            .find(|m| m.id == id && m.kind == "chat")
    }

    /// A provider's models of the given types (chat when empty), by id.
    pub fn models(&self, provider: &str, types: &[&str]) -> Vec<&Model> {
        let types: &[&str] = if types.is_empty() { &["chat"] } else { types };
        let mut out: Vec<&Model> = self
            .provider(provider)
            .map(|p| {
                p.models
                    .iter()
                    .filter(|m| types.contains(&m.kind.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

// ---------------------------------------------------------------------------
// Fetching, caching and refreshing.

/// Where the catalog comes from.
#[derive(Debug, Clone)]
pub struct Options {
    /// The catalog server (`$CODESMITH_MODELS_URL`, else config's
    /// `catalog.url`); empty or unset means [`DEFAULT_SOURCE`].
    pub url: Option<String>,
    /// A catalog file read instead of the cache (`$CODESMITH_MODELS_PATH`).
    pub file: Option<PathBuf>,
    /// Whether the background refresh fetches. Off by default; see
    /// [`Options::resolve`].
    pub fetch: bool,
    /// Whether the bundled snapshot is the floor when nothing is cached.
    pub snapshot: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            url: None,
            file: None,
            fetch: false,
            snapshot: true,
        }
    }
}

fn truthy(v: Option<String>) -> bool {
    v.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Whether `$CODESMITH_DISABLE_MODELS_FETCH` forbids every fetch, even an
/// explicit `codesmith models --refresh`.
pub fn fetch_disabled() -> bool {
    truthy(std::env::var("CODESMITH_DISABLE_MODELS_FETCH").ok())
}

impl Options {
    /// Options from the environment alone.
    pub fn from_env() -> Options {
        Options::resolve(None, None)
    }

    /// Options from the environment over config's `catalog.fetch` and
    /// `catalog.url`. Fetching is off unless asked for:
    /// `$CODESMITH_DISABLE_MODELS_FETCH` turns it off whatever else says;
    /// otherwise `$CODESMITH_MODELS_FETCH`, then `catalog.fetch`, decide,
    /// and without either a configured URL (`$CODESMITH_MODELS_URL` or
    /// `catalog.url`) turns it on.
    pub fn resolve(fetch: Option<bool>, url: Option<&str>) -> Options {
        let url = std::env::var("CODESMITH_MODELS_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .or_else(|| url.map(str::to_string).filter(|u| !u.is_empty()));
        let env_fetch = std::env::var("CODESMITH_MODELS_FETCH")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| truthy(Some(v)));
        Options {
            fetch: !fetch_disabled() && env_fetch.or(fetch).unwrap_or(url.is_some()),
            url,
            file: std::env::var_os("CODESMITH_MODELS_PATH")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            snapshot: true,
        }
    }
}

/// What a refresh did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refreshed {
    /// The cache was fresh enough; nothing was fetched.
    Fresh,
    /// Fetched (or revalidated) and the same as the cached catalog.
    Unchanged,
    /// A new catalog replaced the cached one.
    Updated,
}

/// A small key-value store for the cached catalog.
pub trait Kv: Send + Sync {
    fn get(&self, key: &str) -> Option<Value>;
    fn set(&self, key: &str, value: Value) -> anyhow::Result<()>;
    fn remove(&self, key: &str);
}

/// Each key a JSON file in one folder.
pub struct FileKv {
    pub dir: PathBuf,
}

impl FileKv {
    fn path(&self, key: &str) -> PathBuf {
        let name: String = key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        self.dir.join(format!("{name}.json"))
    }
}

impl Kv for FileKv {
    fn get(&self, key: &str) -> Option<Value> {
        let text = std::fs::read_to_string(self.path(key)).ok()?;
        Some(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn set(&self, key: &str, value: Value) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(key);
        // Written whole and renamed, so another process never reads half.
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&value)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn remove(&self, key: &str) {
        let _ = std::fs::remove_file(self.path(key));
    }
}

/// An in-memory store.
#[derive(Default)]
pub struct MemoryKv {
    pub values: Mutex<HashMap<String, Value>>,
}

impl Kv for MemoryKv {
    fn get(&self, key: &str) -> Option<Value> {
        self.values.lock().unwrap().get(key).cloned()
    }

    fn set(&self, key: &str, value: Value) -> anyhow::Result<()> {
        self.values.lock().unwrap().insert(key.to_string(), value);
        Ok(())
    }

    fn remove(&self, key: &str) {
        self.values.lock().unwrap().remove(key);
    }
}

/// The SHA-256 of a catalog body, kept so a refresh can skip a
/// byte-identical catalog.
pub fn body_digest(text: &str) -> String {
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The cache key for a catalog source, versioned so a v1 entry is never
/// read as v2.
pub fn cache_key(source: &str) -> String {
    if source == DEFAULT_SOURCE {
        return "models-dev:v2:catalog".into();
    }
    format!("models-dev:v2:catalog:{}", &body_digest(source)[..16])
}

/// The cache is fresh for this long; the background refresh runs this often.
pub const TTL: Duration = Duration::from_secs(5 * 60);
/// One fetch, retries included, may take this long.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The catalog bundled with the binary: OpenCode's committed snapshot of
/// the v2 API, trimmed to the fields read here and gzipped
/// (`scripts/models-snapshot.py` rebuilds it).
static BUNDLED: &[u8] = include_bytes!("snapshot.json.gz");

/// The bundled snapshot's text, decompressed once per process.
pub fn bundled_text() -> Arc<str> {
    static CELL: OnceLock<Arc<str>> = OnceLock::new();
    CELL.get_or_init(|| {
        use std::io::Read;
        let mut text = String::new();
        match flate2::read::GzDecoder::new(BUNDLED).read_to_string(&mut text) {
            Ok(_) => text.into(),
            Err(_) => "{}".into(),
        }
    })
    .clone()
}

/// The bundled snapshot, every provider normalized.
pub fn bundled() -> Arc<Vec<Snapshot>> {
    static CELL: OnceLock<Arc<Vec<Snapshot>>> = OnceLock::new();
    CELL.get_or_init(|| Arc::new(decode_snapshot(&bundled_text()).unwrap_or_default()))
        .clone()
}

/// A generic `User-Agent`: the program and its version, nothing else.
pub fn user_agent() -> String {
    format!("codesmith/{}", env!("CARGO_PKG_VERSION"))
}

struct Response {
    text: String,
    etag: Option<String>,
}

#[derive(Debug)]
struct Status(u16);

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}

impl std::error::Error for Status {}

/// Whether a fetched catalog's URL template may fill `${name}` from the
/// environment. The catalog is untrusted, so only names that configure
/// where a provider lives (account, project, region, host, ...) qualify,
/// never one that may hold a secret: anything else leaves the URL
/// unresolved. These are the shapes models.dev's URLs use
/// (`CLOUDFLARE_ACCOUNT_ID`, `GOOGLE_VERTEX_PROJECT`, `AWS_REGION`,
/// `DATABRICKS_HOST`, `NEON_AI_GATEWAY_BASE_URL`, ...).
pub fn url_variable(name: &str) -> bool {
    const SUFFIXES: [&str; 14] = [
        "_ACCOUNT",
        "_ACCOUNT_ID",
        "_GATEWAY_ID",
        "_PRODUCT_ID",
        "_PROJECT",
        "_PROJECT_ID",
        "_LOCATION",
        "_REGION",
        "_HOST",
        "_ENDPOINT",
        "_RESOURCE_NAME",
        "_DEPLOYMENT_URL",
        "_BASE_URL",
        "_WORKSPACE",
    ];
    const SECRET: [&str; 6] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"];
    let upper = name.to_ascii_uppercase();
    SUFFIXES.iter().any(|s| upper.ends_with(s)) && !SECRET.iter().any(|s| upper.contains(s))
}

/// Whether a catalog base URL, once filled in, may be used: `https`, or
/// `http` to this machine only.
pub fn secure_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    match u.scheme() {
        "https" => u.host().is_some(),
        "http" => match u.host() {
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

/// The largest catalog body read (models.dev's is under 10 MB).
pub const MAX_BODY: usize = 32 * 1024 * 1024;

/// A response body as text, failing past [`MAX_BODY`] instead of
/// buffering whatever the server sends.
async fn read_body(mut res: reqwest::Response) -> anyhow::Result<String> {
    if res.content_length().is_some_and(|n| n > MAX_BODY as u64) {
        anyhow::bail!("the catalog is larger than {MAX_BODY} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY {
            anyhow::bail!("the catalog is larger than {MAX_BODY} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8(body)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// A loaded catalog: its text (read lazily) or normalized snapshots, with
/// the full list and the offered catalog made from them on first use.
struct Loaded {
    raw: Raw,
    all: OnceLock<Arc<Vec<Snapshot>>>,
    catalog: OnceLock<Arc<Catalog>>,
}

enum Raw {
    Text(Arc<str>),
    Data(Arc<Vec<Snapshot>>),
}

impl Loaded {
    fn new(raw: Raw) -> Arc<Loaded> {
        Arc::new(Loaded {
            raw,
            all: OnceLock::new(),
            catalog: OnceLock::new(),
        })
    }
}

/// The catalog service: reads, caches and refreshes the catalog.
pub struct ModelsDev {
    source: String,
    options: Options,
    kv: Arc<dyn Kv>,
    key: String,
    meta_key: String,
    client: reqwest::Client,
    state: Mutex<Option<Arc<Loaded>>>,
    refresh_lock: tokio::sync::Mutex<()>,
    refreshed: tokio::sync::watch::Sender<u64>,
    fixed: bool,
}

impl std::fmt::Debug for ModelsDev {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("ModelsDev")
            .field("source", &self.source)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl ModelsDev {
    pub fn new(options: Options, kv: Arc<dyn Kv>) -> ModelsDev {
        let source = options
            .url
            .clone()
            .filter(|u| !u.is_empty())
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| DEFAULT_SOURCE.to_string());
        let key = cache_key(&source);
        // The v1 catalog was cached under unversioned keys; free it.
        let old = key.replace("models-dev:v2:", "models-dev:");
        kv.remove(&old);
        kv.remove(&format!("{old}:meta"));
        ModelsDev {
            meta_key: format!("{key}:meta"),
            key,
            source,
            options,
            kv,
            client: reqwest::Client::new(),
            state: Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
            refreshed: tokio::sync::watch::channel(0).0,
            fixed: false,
        }
    }

    /// A service that serves these snapshots and never fetches.
    pub fn fixed(data: Vec<Snapshot>) -> ModelsDev {
        let s = ModelsDev::new(
            Options {
                fetch: false,
                snapshot: false,
                ..Options::default()
            },
            Arc::new(MemoryKv::default()),
        );
        *s.state.lock().unwrap() = Some(Loaded::new(Raw::Data(Arc::new(data))));
        ModelsDev { fixed: true, ..s }
    }

    /// The process-wide catalog, cached under the cache folder. The first
    /// caller's options win.
    pub fn global(options: Options) -> Arc<ModelsDev> {
        static CELL: OnceLock<Arc<ModelsDev>> = OnceLock::new();
        CELL.get_or_init(|| {
            Arc::new(ModelsDev::new(
                options,
                Arc::new(FileKv {
                    dir: crate::paths::cache_dir().join("models"),
                }),
            ))
        })
        .clone()
    }

    pub fn options(&self) -> &Options {
        &self.options
    }

    /// The catalog server.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The cache key this source's catalog is stored under.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The catalog, loaded once and kept until a refresh replaces it.
    /// Concurrent callers share one load.
    pub fn get(&self) -> Arc<Vec<Snapshot>> {
        let l = self.loaded();
        l.all
            .get_or_init(|| match &l.raw {
                Raw::Text(t) => Arc::new(
                    std::panic::catch_unwind(|| decode_snapshot(t))
                        .ok()
                        .and_then(Result::ok)
                        .unwrap_or_default(),
                ),
                Raw::Data(d) => d.clone(),
            })
            .clone()
    }

    /// The catalog as offered, with lookups.
    pub fn catalog(&self) -> Arc<Catalog> {
        let l = self.loaded();
        l.catalog
            .get_or_init(|| {
                Arc::new(match &l.raw {
                    Raw::Text(t) => Catalog::from_text(t.clone()).unwrap_or_default(),
                    Raw::Data(d) => Catalog::new(d),
                })
            })
            .clone()
    }

    fn loaded(&self) -> Arc<Loaded> {
        let mut state = self.state.lock().unwrap();
        if let Some(s) = &*state {
            return s.clone();
        }
        let loaded = Loaded::new(self.populate());
        *state = Some(loaded.clone());
        loaded
    }

    /// Bumped (and sent) each time a refresh replaces the catalog.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.refreshed.subscribe()
    }

    fn invalidate(&self) {
        if !self.fixed {
            *self.state.lock().unwrap() = None;
        }
    }

    /// A catalog text that is a JSON object.
    fn object(text: String) -> Option<Arc<str>> {
        let t: Arc<str> = text.into();
        index(&t).map(|_| t)
    }

    fn read_cache(&self) -> Option<Arc<str>> {
        let value = self.kv.get(&self.key)?;
        let body = value
            .get("updatedAt")
            .filter(|u| u.is_number())
            .and(value.get("body"))
            .and_then(Value::as_str)
            .filter(|_| value.get("digest").is_none_or(Value::is_string))
            .filter(|b| {
                value.get("checked").and_then(Value::as_str) == Some(CHECKED) || {
                    let ok = usable(b);
                    if ok {
                        let mut v = value.clone();
                        v["checked"] = CHECKED.into();
                        let _ = self.kv.set(&self.key, v);
                    }
                    ok
                }
            })
            .and_then(|b| Self::object(b.to_string()));
        if body.is_none() {
            self.kv.remove(&self.key);
            self.kv.remove(&self.meta_key);
        }
        body
    }

    fn read_file(&self) -> Option<Arc<str>> {
        Self::object(std::fs::read_to_string(self.options.file.as_ref()?).ok()?)
    }

    fn populate(&self) -> Raw {
        if let Some(t) = self.read_file() {
            return Raw::Text(t);
        }
        if self.options.file.is_none() {
            if let Some(t) = self.read_cache() {
                return Raw::Text(t);
            }
        }
        if self.options.snapshot {
            return Raw::Text(bundled_text());
        }
        if !self.options.fetch {
            return Raw::Data(Arc::new(vec![]));
        }
        // Nothing to serve without the network: fetch now, on a thread of
        // its own so this works inside or outside an async runtime.
        let text = std::thread::scope(|s| {
            s.spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                rt.block_on(async {
                    let r = self.fetch_api(None).await.ok()??;
                    if !usable(&r.text) {
                        return None;
                    }
                    let t = Self::object(r.text.clone())?;
                    self.write_cache(&r.text, r.etag.as_deref(), None);
                    Some(t)
                })
            })
            .join()
            .ok()
            .flatten()
        });
        match text {
            Some(t) => Raw::Text(t),
            None => Raw::Data(Arc::new(vec![])),
        }
    }

    fn write_meta(&self, updated_at: i64, digest: Option<&str>, etag: Option<&str>) {
        let mut meta = json!({ "updatedAt": updated_at });
        if let Some(d) = digest {
            meta["digest"] = d.into();
        }
        if let Some(e) = etag {
            meta["etag"] = e.into();
        }
        let _ = self.kv.set(&self.meta_key, meta);
    }

    /// Best effort: a failed cache write never stops the catalog loading.
    fn write_cache(&self, text: &str, etag: Option<&str>, digest: Option<String>) {
        let digest = digest.unwrap_or_else(|| body_digest(text));
        let now = chrono::Utc::now().timestamp_millis();
        let entry = json!({"updatedAt": now, "digest": digest, "body": text, "checked": CHECKED});
        if self.kv.set(&self.key, entry).is_ok() {
            self.write_meta(now, Some(&digest), etag);
        }
    }

    async fn fetch_path(&self, path: &str, etag: Option<&str>) -> anyhow::Result<Option<Response>> {
        let url = format!("{}{path}", self.source);
        let attempt = || async {
            let mut req = self
                .client
                .get(&url)
                .header(reqwest::header::USER_AGENT, user_agent());
            if let Some(e) = etag {
                req = req.header(reqwest::header::IF_NONE_MATCH, e);
            }
            req.send().await
        };
        let run = async {
            // Transient errors and responses are retried twice.
            let mut delay = Duration::from_millis(200);
            let mut tries = 0;
            let res = loop {
                let transient = match attempt().await {
                    Ok(r) if [408, 429, 500, 502, 503, 504].contains(&r.status().as_u16()) => {
                        Err(Ok(r))
                    }
                    Ok(r) => Ok(r),
                    Err(e) => Err(Err(e)),
                };
                match transient {
                    Ok(r) => break r,
                    Err(last) if tries >= 2 => match last {
                        Ok(r) => break r,
                        Err(e) => return Err(anyhow::Error::from(e)),
                    },
                    Err(_) => {
                        tries += 1;
                        let jitter = 0.8 + rand::random::<f64>() * 0.4;
                        tokio::time::sleep(delay.mul_f64(jitter)).await;
                        delay *= 2;
                    }
                }
            };
            let status = res.status().as_u16();
            if etag.is_some() && status == 304 {
                return Ok(None);
            }
            if !(200..300).contains(&status) {
                return Err(Status(status).into());
            }
            let etag = res
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            Ok(Some(Response {
                text: read_body(res).await?,
                etag,
            }))
        };
        tokio::time::timeout(FETCH_TIMEOUT, run)
            .await
            .map_err(|_| anyhow::anyhow!("models catalog fetch timed out"))?
    }

    async fn fetch_api(&self, etag: Option<&str>) -> anyhow::Result<Option<Response>> {
        // models.dev serves the v1 catalog; a mirror may have the v2 one.
        if self.source == DEFAULT_SOURCE {
            return self.fetch_path(V1_PATH, etag).await;
        }
        match self.fetch_path(CATALOG_PATH, etag).await {
            // A custom mirror may only serve the v1 catalog.
            Err(e) if e.downcast_ref::<Status>().is_some_and(|s| s.0 == 404) => {
                self.fetch_path(V1_PATH, etag).await
            }
            r => r,
        }
    }

    /// Fetch the catalog when the cache is stale (or always, with `force`),
    /// revalidating with the stored ETag. A new catalog replaces the cached
    /// one and is announced through [`ModelsDev::subscribe`]; an unchanged
    /// one only records the check. Errors are swallowed.
    pub async fn refresh(&self, force: bool) {
        let _ = self.try_refresh(force).await;
    }

    /// [`ModelsDev::refresh`], saying what happened or why it failed.
    pub async fn try_refresh(&self, force: bool) -> anyhow::Result<Refreshed> {
        let _guard = self.refresh_lock.lock().await;
        let stored = self
            .kv
            .get(&self.meta_key)
            .or_else(|| self.kv.get(&self.key))
            .and_then(|v| {
                let updated = v.get("updatedAt")?.as_f64()? as i64;
                let digest = v.get("digest").and_then(Value::as_str).map(str::to_string);
                let etag = v.get("etag").and_then(Value::as_str).map(str::to_string);
                Some((updated, digest, etag))
            });
        let now = chrono::Utc::now().timestamp_millis();
        if let Some((updated, ..)) = &stored {
            if !force && now - updated < TTL.as_millis() as i64 {
                return Ok(Refreshed::Fresh);
            }
        }
        let etag = if force {
            None
        } else {
            stored.as_ref().and_then(|s| s.2.clone())
        };
        let response = self.fetch_api(etag.as_deref()).await?;
        let digest = response.as_ref().map(|r| body_digest(&r.text));
        if let (false, Some((_, sd, se))) = (force, &stored) {
            if response.is_none() || sd.is_some() && *sd == digest {
                let etag = response
                    .as_ref()
                    .and_then(|r| r.etag.clone())
                    .or(se.clone());
                self.write_meta(now, sd.as_deref(), etag.as_deref());
                return Ok(Refreshed::Unchanged);
            }
        }
        let response = response.ok_or_else(|| anyhow::anyhow!("unexpected 304 response"))?;
        if !usable(&response.text) {
            anyhow::bail!("the fetched catalog could not be read");
        }
        let same = stored
            .as_ref()
            .is_some_and(|s| s.1.is_some() && s.1 == digest);
        self.write_cache(&response.text, response.etag.as_deref(), digest);
        self.invalidate();
        self.refreshed.send_modify(|n| *n += 1);
        Ok(if same {
            Refreshed::Unchanged
        } else {
            Refreshed::Updated
        })
    }

    /// Refresh now and every [`TTL`] in the background, when fetching is on
    /// and there is an async runtime. Never blocks the caller.
    pub fn spawn_refresh(self: &Arc<Self>) {
        if !self.options.fetch || self.fixed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let me = self.clone();
        handle.spawn(async move {
            loop {
                me.refresh(false).await;
                tokio::time::sleep(TTL).await;
            }
        });
    }
}
