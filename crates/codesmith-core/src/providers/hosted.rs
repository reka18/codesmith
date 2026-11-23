//! Model lists of hosted providers, read from each provider's own API with
//! the credentials CodeSmith already has for it: Anthropic's and the
//! OpenAI-compatible providers' `GET /models`, Gemini's `models.list`.
//!
//! A list is kept in `live-models.json` (see [`super::live`]) for the
//! account it was fetched with (a fingerprint, never the key) and stands in
//! for the catalog's models of that provider. Each model keeps the
//! catalog's details when the catalog knows it, and otherwise what the API
//! said about it (name, context size, release time), so a model released
//! after the catalog snapshot is listed and runs.
//!
//! Where the API says how a model reasons, its effort levels come from
//! there too: Anthropic's `capabilities.effort` (and `capabilities.thinking`),
//! OpenRouter's `reasoning.supported_efforts`, Gemini's `thinking`. A model
//! the API says does not reason has no effort levels; one it says reasons
//! without naming levels keeps the catalog's (or the protocol's defaults).
//!
//! Nothing is fetched while idle: the TUI asks again when a model or
//! provider picker opens and the kept list is missing, older than
//! [`MAX_AGE_MS`], or for another account, and `codesmith models PROVIDER`
//! asks every time.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::Path;

/// The built-in hosted providers whose model list is read from their API.
pub const PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "openrouter",
    "groq",
    "deepseek",
    "mistral",
    "xai",
    "together",
    "fireworks",
    "cerebras",
];

/// How long a kept list is used before opening a picker asks again: a day.
pub const MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;

/// At most this many models are kept from one answer.
const MAX_MODELS: usize = 5000;

/// Whether `pid`'s model list comes from its API.
pub fn hosted(pid: &str) -> bool {
    PROVIDERS.contains(&pid)
}

/// The account a hosted list is kept for: the key's fingerprint, so a list
/// fetched with another key (or none) is not shown.
pub fn account(key: Option<&str>) -> String {
    match key.filter(|k| !k.is_empty()) {
        Some(k) => format!("key:{}", super::live::fingerprint(k)),
        None => "key:none".into(),
    }
}

/// Whether a kept list should be fetched again: missing, kept for another
/// account, older than [`MAX_AGE_MS`], or stamped in the future (a clock
/// that moved back).
pub fn stale(kept: Option<&Value>, account: &str, now_ms: u64) -> bool {
    let Some(k) = kept else {
        return true;
    };
    if k["account"].as_str() != Some(account) {
        return true;
    }
    let fetched = k["fetched"].as_u64().unwrap_or(0);
    fetched > now_ms.saturating_add(60_000) || now_ms.saturating_sub(fetched) >= MAX_AGE_MS
}

/// Fetch and keep `pid`'s list with the resolved provider's endpoint and
/// credentials: the chat model ids. On a failure the list kept before stays.
pub async fn refresh(
    client: &reqwest::Client,
    path: &Path,
    pid: &str,
    p: &codesmith_ai::Provider,
) -> Result<Vec<String>> {
    let v = p
        .list_models_json(client)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("listing {pid} models"))?;
    let raw = normalize(&v);
    if raw.is_empty() {
        anyhow::bail!("listing {pid} models: the answer lists no models");
    }
    let ids = chat_ids(&raw);
    super::live::save_in(path, pid, &account(p.api_key.as_deref()), &p.base_url, raw)?;
    Ok(ids)
}

/// The chat models' ids of a normalized list.
pub fn chat_ids(raw: &[Value]) -> Vec<String> {
    raw.iter()
        .filter(|m| m["kind"] == "chat")
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect()
}

/// A provider's answer as kept entries: `{"id", "kind", "name"?,
/// "context"?, "output"?, "released"?, "reasoning"?, "efforts"?}`, one per
/// model id, in the answer's
/// order. Anything that is not a model with a usable id is skipped.
pub fn normalize(v: &Value) -> Vec<Value> {
    let list = v["models"]
        .as_array()
        .or_else(|| v["data"].as_array())
        .or_else(|| v.as_array());
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in list.into_iter().flatten() {
        if out.len() >= MAX_MODELS {
            break;
        }
        let Some(obj) = m.as_object() else {
            continue;
        };
        let Some(id) = model_id(obj) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut e = Map::new();
        e.insert("kind".into(), json!(kind(&id, obj)));
        if let Some(n) = display_name(&id, obj) {
            e.insert("name".into(), json!(n));
        }
        if let Some(c) = tokens(obj, CONTEXT_FIELDS) {
            e.insert("context".into(), json!(c));
        }
        if let Some(o) = tokens(obj, OUTPUT_FIELDS) {
            e.insert("output".into(), json!(o));
        }
        if let Some(r) = released(obj) {
            e.insert("released".into(), json!(r));
        }
        if let Some(r) = reasoning(obj) {
            e.insert("reasoning".into(), json!(r.supported));
            if !r.efforts.is_empty() {
                e.insert("efforts".into(), json!(r.efforts));
            }
        }
        e.insert("id".into(), json!(id));
        out.push(Value::Object(e));
    }
    out
}

/// The model's id: Gemini's `name` without `models/`, else `id` (or
/// `name`). Ids are short, printable and without spaces.
fn model_id(m: &Map<String, Value>) -> Option<String> {
    let raw = if m.contains_key("supportedGenerationMethods") {
        m.get("name")?.as_str()?.trim_start_matches("models/")
    } else {
        m.get("id")
            .and_then(Value::as_str)
            .or_else(|| m.get("name").and_then(Value::as_str))?
    };
    let id = raw.trim();
    let ok = !id.is_empty()
        && id.len() <= 200
        && !id.chars().any(|c| c.is_control() || c.is_whitespace())
        && !id.starts_with('/')
        && !id.ends_with('/');
    ok.then(|| id.to_string())
}

/// A human name the API gives, when it is not just the id.
fn display_name(id: &str, m: &Map<String, Value>) -> Option<String> {
    let gemini = m.contains_key("supportedGenerationMethods");
    let candidates: &[&str] = if gemini {
        &["displayName"]
    } else {
        &["display_name", "displayName", "name"]
    };
    candidates
        .iter()
        .filter_map(|k| m.get(*k)?.as_str())
        .map(|n| n.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|n| n.trim().to_string())
        .find(|n| !n.is_empty() && n != id && n.chars().count() <= 120)
}

const CONTEXT_FIELDS: &[&str] = &[
    "context_length",
    "context_window",
    "max_context_length",
    "inputTokenLimit",
    "max_input_tokens",
    "max_model_len",
];
const OUTPUT_FIELDS: &[&str] = &[
    "outputTokenLimit",
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
];

/// The first plausible token count among `fields`, then OpenRouter's
/// `top_provider` copy of it.
fn tokens(m: &Map<String, Value>, fields: &[&str]) -> Option<u64> {
    let plausible = |v: &Value| v.as_u64().filter(|n| (1..=100_000_000).contains(n));
    fields
        .iter()
        .find_map(|f| m.get(*f).and_then(plausible))
        .or_else(|| {
            let top = m.get("top_provider")?.as_object()?;
            fields.iter().find_map(|f| top.get(*f).and_then(plausible))
        })
}

/// When the model came out, in milliseconds since the epoch: OpenAI's
/// `created` (seconds), Anthropic's `created_at` (RFC 3339).
fn released(m: &Map<String, Value>) -> Option<i64> {
    // 2000-01-01 to 2100-01-01, in milliseconds.
    let plausible = |ms: i64| {
        (946_684_800_000..4_102_444_800_000)
            .contains(&ms)
            .then_some(ms)
    };
    if let Some(n) = m.get("created").and_then(Value::as_i64) {
        // Seconds, unless it is already milliseconds.
        let ms = if n > 100_000_000_000 {
            n
        } else {
            n.saturating_mul(1000)
        };
        return plausible(ms);
    }
    let s = m.get("created_at").and_then(Value::as_str)?;
    let t = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    plausible(t.timestamp_millis())
}

/// `chat`, or why a model is not one: what the API says it does, then what
/// its id says (embeddings, speech, images, moderation and the like).
fn kind(id: &str, m: &Map<String, Value>) -> &'static str {
    // Gemini: only models that generate content.
    if let Some(methods) = m
        .get("supportedGenerationMethods")
        .and_then(Value::as_array)
    {
        if !methods.iter().any(|x| x == "generateContent") {
            return "other";
        }
    }
    // Together: `type` is chat, language, code, embedding, image, ….
    if let Some(t) = m.get("type").and_then(Value::as_str) {
        if !matches!(t, "chat" | "language" | "code" | "model") {
            return "other";
        }
    }
    // Mistral: what the model can do.
    if m.get("capabilities")
        .and_then(|c| c.get("completion_chat"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return "other";
    }
    // OpenRouter: what the model makes.
    if let Some(out) = m
        .get("architecture")
        .and_then(|a| a.get("output_modalities"))
        .and_then(Value::as_array)
    {
        if !out.iter().any(|x| x == "text") {
            return "other";
        }
    }
    let lower = id.to_ascii_lowercase();
    const NOT_CHAT: &[&str] = &[
        "embed",
        "whisper",
        "tts",
        "dall-e",
        "moderation",
        "transcribe",
        "realtime",
        "rerank",
        "davinci",
        "babbage",
        "audio",
        "speech",
        "imagen",
        "image",
        "sora",
        "veo-",
        "guard",
        "aqa",
        "-ocr",
    ];
    if NOT_CHAT.iter().any(|w| lower.contains(w)) {
        return "other";
    }
    "chat"
}

/// How a model reasons, as its API says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiReasoning {
    pub supported: bool,
    /// The effort levels, lowest first; empty when the API names none.
    pub efforts: Vec<String>,
}

/// Effort levels from lowest to highest; others sort after them.
const EFFORT_ORDER: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// At most this many effort levels are kept for one model.
const MAX_EFFORTS: usize = 16;

/// An effort level name: short, lower-case letters, digits, `-` and `_`.
fn effort_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 24
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && s != "default"
        && s != "supported"
}

/// `levels` deduplicated, the known ones lowest first, then the others in
/// the API's order.
fn ordered(levels: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in levels {
        if effort_name(&l) && !out.contains(&l) && out.len() < MAX_EFFORTS {
            out.push(l);
        }
    }
    let rank = |l: &String| {
        EFFORT_ORDER
            .iter()
            .position(|k| k == l)
            .unwrap_or(EFFORT_ORDER.len())
    };
    out.sort_by_key(rank);
    out
}

/// What the API says about the model's reasoning, if anything.
pub fn reasoning(m: &Map<String, Value>) -> Option<ApiReasoning> {
    // Anthropic: `capabilities.effort` names each level it supports, and
    // `capabilities.thinking` whether the model thinks at all.
    if let Some(caps) = m.get("capabilities").and_then(Value::as_object) {
        let supported =
            |v: Option<&Value>| v.and_then(|v| v.get("supported")).and_then(Value::as_bool);
        let effort = caps.get("effort").and_then(Value::as_object);
        let thinking = supported(caps.get("thinking"));
        if effort.is_some() || thinking.is_some() {
            let efforts = match effort {
                Some(e) if supported(Some(&Value::Object(e.clone()))) == Some(true) => ordered(
                    e.iter()
                        .filter(|(k, v)| *k != "supported" && supported(Some(v)) == Some(true))
                        .map(|(k, _)| k.clone()),
                ),
                _ => vec![],
            };
            let on = !efforts.is_empty() || thinking == Some(true);
            return Some(ApiReasoning {
                supported: on,
                efforts,
            });
        }
    }
    // OpenRouter: a `reasoning` object for models that reason, with the
    // levels allowed (highest first; null for any).
    if let Some(r) = m.get("reasoning").and_then(Value::as_object) {
        let mandatory = r.get("mandatory").and_then(Value::as_bool) == Some(true);
        let efforts = r
            .get("supported_efforts")
            .and_then(Value::as_array)
            .map(|a| {
                ordered(
                    a.iter()
                        .filter_map(Value::as_str)
                        .filter(|e| !(mandatory && *e == "none"))
                        .map(str::to_string),
                )
            })
            .unwrap_or_default();
        return Some(ApiReasoning {
            supported: true,
            efforts,
        });
    }
    // OpenRouter without a `reasoning` object: whether the parameters it
    // takes include reasoning ones.
    if let Some(params) = m.get("supported_parameters").and_then(Value::as_array) {
        let reasons = params.iter().any(|p| {
            matches!(
                p.as_str(),
                Some("reasoning" | "reasoning_effort" | "include_reasoning")
            )
        });
        return Some(ApiReasoning {
            supported: reasons,
            efforts: vec![],
        });
    }
    // Gemini: whether the model thinks.
    if m.contains_key("supportedGenerationMethods") {
        if let Some(t) = m.get("thinking").and_then(Value::as_bool) {
            return Some(ApiReasoning {
                supported: t,
                efforts: vec![],
            });
        }
    }
    None
}

/// The package whose reasoning controls a hosted provider's API describes,
/// for spelling its effort levels.
fn reasoning_package(pid: &str) -> Option<&'static str> {
    Some(match pid {
        "anthropic" => "@opencode/ai/providers/anthropic",
        "openrouter" => "@opencode/ai/providers/openrouter",
        "google" => "@opencode/ai/providers/google",
        _ => return None,
    })
}

/// Apply what the API said about a model's reasoning to it: no levels for
/// a model that does not reason, the API's levels when it names them, and
/// otherwise the template's (or the protocol's defaults for a model the
/// catalog lacks).
fn apply_reasoning(pid: &str, e: &Value, m: &mut crate::catalog::Model) {
    use crate::catalog::variant;
    let Some(supported) = e["reasoning"].as_bool() else {
        return;
    };
    if !supported {
        m.variants.clear();
        m.reasoning = variant::Reasoning::default();
        return;
    }
    let Some(package) = reasoning_package(pid) else {
        return;
    };
    let efforts: Vec<String> = e["efforts"]
        .as_array()
        .map(|a| ordered(a.iter().filter_map(Value::as_str).map(str::to_string)))
        .unwrap_or_default();
    if efforts.is_empty() && !m.variants.is_empty() {
        return;
    }
    m.reasoning = variant::Reasoning {
        supported: true,
        effort: (!efforts.is_empty()).then(|| efforts.clone()),
        ..Default::default()
    };
    let supports = if efforts.is_empty() {
        variant::default_supports()
    } else {
        variant::supports(&m.reasoning)
    };
    let id = m.id.clone();
    let target = variant::Target {
        package: Some(package),
        provider_id: pid,
        model_id: &id,
        output: m.limit.output as i64,
    };
    m.variants = variant::resolve(&target, &supports);
    m.package = Some(package.to_string());
}

/// A kept list as catalog models: the catalog's model where it knows the
/// id, else one built from what the API said.
pub fn derive(
    pid: &str,
    raw: &[Value],
    templates: &[crate::catalog::Model],
) -> Vec<crate::catalog::Model> {
    let mut out = Vec::new();
    for e in raw {
        let Some(id) = e["id"].as_str() else {
            continue;
        };
        let kind = e["kind"].as_str().unwrap_or("chat");
        if let Some(t) = templates.iter().find(|t| t.id == id) {
            let mut m = t.clone();
            if kind != "chat" {
                m.kind = kind.to_string();
            }
            apply_reasoning(pid, e, &mut m);
            out.push(m);
            continue;
        }
        let mut m = super::live::blank(pid, id);
        m.kind = kind.to_string();
        if let Some(n) = e["name"].as_str() {
            m.name = n.to_string();
        }
        if let Some(c) = e["context"].as_u64() {
            m.limit.context = c;
            m.limit.output = m.limit.output.min(c);
        }
        if let Some(o) = e["output"].as_u64() {
            m.limit.output = o.min(m.limit.context);
        }
        if let Some(r) = e["released"].as_i64() {
            m.time.released = r;
        }
        apply_reasoning(pid, e, &mut m);
        out.push(m);
    }
    out
}

#[cfg(test)]
#[path = "hosted_tests.rs"]
mod tests;
