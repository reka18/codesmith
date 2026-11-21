//! Provider options: per-request settings such as reasoning effort, thinking
//! budgets, service tier and text verbosity (OpenCode's `providerOptions`).
//!
//! Options are a flat JSON object with OpenCode's camelCase keys
//! (`reasoningEffort`, `thinking`, `thinkingConfig`, `serviceTier`, …);
//! snake_case spellings are accepted too. Each protocol lowers the options it
//! knows into its own wire fields and ignores the rest, so one model config
//! can carry options for any protocol. Most protocols drop a malformed
//! option without discarding its siblings; Anthropic Messages, like
//! OpenCode, rejects a malformed known option before anything is sent.

use crate::Protocol;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Provider options for one request.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Options(Map<String, Value>);

impl<'de> Deserialize<'de> for Options {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Options::from_map(Map::deserialize(d)?))
    }
}

/// `reasoning_effort` → `reasoningEffort`.
fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut up = false;
    for c in key.chars() {
        if c == '_' && !out.is_empty() {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Merge `top` into `base`: objects merge by key, anything else replaces.
pub(crate) fn deep_merge(base: &mut Value, top: &Value) {
    match (base, top) {
        (Value::Object(b), Value::Object(t)) => {
            for (k, v) in t {
                match b.get_mut(k) {
                    Some(slot) if slot.is_object() && v.is_object() => deep_merge(slot, v),
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, t) => *b = t.clone(),
    }
}

impl Options {
    /// Options from a JSON object; top-level keys are normalized to
    /// camelCase (a snake_case key wins over its camelCase twin, as
    /// Anthropic's wire spelling does in OpenCode). Nested objects are kept
    /// as written.
    pub fn from_map(map: Map<String, Value>) -> Self {
        let mut out = Map::new();
        let mut snake = Vec::new();
        for (k, v) in map {
            let c = camel(&k);
            if c != k {
                snake.push((c, v));
            } else {
                out.entry(c).or_insert(v);
            }
        }
        for (c, v) in snake {
            out.insert(c, v);
        }
        Options(out)
    }

    /// Options from a JSON value; anything but an object is no options.
    pub fn from_value(v: Value) -> Self {
        match v {
            Value::Object(m) => Options::from_map(m),
            _ => Options::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    /// Set one option (the key is normalized like [`Options::from_map`]).
    pub fn set(&mut self, key: &str, value: Value) {
        self.0.insert(camel(key), value);
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.0.remove(&camel(key))
    }

    /// Layer `top` over these options: later options win per key, and
    /// objects (`thinking`, `thinkingConfig`, …) merge by key.
    pub fn merge(&mut self, top: &Options) {
        let mut base = Value::Object(std::mem::take(&mut self.0));
        deep_merge(&mut base, &Value::Object(top.0.clone()));
        if let Value::Object(m) = base {
            self.0 = m;
        }
    }

    /// These options under `top`.
    pub fn merged(mut self, top: &Options) -> Self {
        self.merge(top);
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    /// A string option; any other type is ignored.
    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        let v = self.get(key)?;
        v.as_i64()
            .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
    }

    pub fn num(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    pub fn object(&self, key: &str) -> Option<&Map<String, Value>> {
        self.get(key)?.as_object()
    }

    /// A list of strings; a list with anything else in it is ignored.
    pub fn strings(&self, key: &str) -> Option<Vec<String>> {
        self.get(key)?
            .as_array()?
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect()
    }

    /// A string-to-string record; one with any other value is ignored.
    pub fn string_record(&self, key: &str) -> Option<Map<String, Value>> {
        let m = self.object(key)?;
        m.values().all(Value::is_string).then(|| m.clone())
    }

    /// The portable reasoning effort (`reasoningEffort`).
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.str("reasoningEffort")
    }
}

/// A field of a nested option object under either spelling
/// (`budgetTokens` / `budget_tokens`).
pub(crate) fn field<'a>(m: &'a Map<String, Value>, camel_key: &str) -> Option<&'a Value> {
    let snake: String = camel_key
        .chars()
        .flat_map(|c| {
            if c.is_ascii_uppercase() {
                vec!['_', c.to_ascii_lowercase()]
            } else {
                vec![c]
            }
        })
        .collect();
    m.get(&snake)
        .or_else(|| m.get(camel_key))
        .filter(|v| !v.is_null())
}

/// A thinking budget that leaves room for the answer: at most half the
/// output limit, but never below the provider's minimum (OpenCode's
/// `fitThinkingBudget`).
pub(crate) fn fit_budget(budget: f64, max_tokens: Option<u32>, minimum: f64) -> f64 {
    match max_tokens {
        Some(max) if budget > max as f64 / 2.0 => minimum.max((max as f64 / 2.0).floor()),
        _ => budget,
    }
}

/// A number as JSON, integral when it is one.
pub(crate) fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// The options a provider applies under the request's own (OpenCode's route
/// and model defaults): OpenAI's Responses API is stateless here, so it asks
/// for encrypted reasoning to replay, GPT-5 reasoning models get a medium
/// effort with reasoning summaries, and the models known to take a text
/// verbosity get a low one ([`verbosity_default`]). A configured option of
/// the same name (`null` included) wins over each.
pub fn defaults(protocol: Protocol, base_url: &str, model: &str) -> Options {
    let mut o = Options::default();
    if protocol != Protocol::OpenaiResponses {
        return o;
    }
    let url = base_url.to_ascii_lowercase();
    let mantle = url.contains("bedrock-mantle.");
    let openai = url.contains("api.openai.com")
        || url.contains(".openai.azure.com")
        || url.contains(".cognitiveservices.azure.com")
        || url.contains(".services.ai.azure.com")
        // Bedrock Mantle's Responses route asks for the same.
        || mantle;
    let id = model.to_ascii_lowercase();
    // Cloudflare AI Gateway passes `openai/…` models through to OpenAI's
    // Responses API (the model id sent without the prefix).
    let cloudflare_openai =
        url.contains("gateway.ai.cloudflare.com") && url.trim_end_matches('/').ends_with("/openai");
    let verbosity_id = if mantle {
        id.strip_prefix("openai.")
    } else if openai || cloudflare_openai {
        Some(id.as_str())
    } else {
        None
    };
    if verbosity_id.is_some_and(verbosity_default) {
        o.set("textVerbosity", "low".into());
    }
    if !openai {
        return o;
    }
    o.set(
        "include",
        serde_json::json!(["reasoning.encrypted_content"]),
    );
    if id.contains("gpt-5") && !id.contains("gpt-5-chat") && !id.contains("gpt-5-pro") {
        o.set("reasoningEffort", "medium".into());
        o.set("reasoningSummary", "auto".into());
    }
    o
}

/// Whether OpenAI model `id` (lowercase, without a route's prefix) gets a
/// low text verbosity by default, as OpenCode's verbosity plugin decides:
/// GPT-6, and GPT-5, 5.1 to 5.6, mini and nano, but no `-chat` or `-image`
/// model. Newer GPT-5 minor versions stay unset until their support is
/// known.
pub fn verbosity_default(id: &str) -> bool {
    if id.contains("gpt-6") {
        return true;
    }
    if id.contains("-chat") || id.contains("-image") {
        return false;
    }
    let ends = |s: &str| s.is_empty() || s.starts_with(['.', ':', '-']);
    id.match_indices("gpt-5").any(|(i, _)| {
        let bounded = i == 0 || matches!(id.as_bytes()[i - 1], b'/' | b'.');
        let rest = &id[i + "gpt-5".len()..];
        let minor = rest
            .strip_prefix('.')
            .is_some_and(|r| r.starts_with(['1', '2', '3', '4', '5', '6']) && ends(&r[1..]));
        let small = ["-mini", "-nano"]
            .iter()
            .any(|v| rest.strip_prefix(v).is_some_and(ends));
        bounded && (rest.is_empty() || minor || small)
    })
}

/// Request options under a model variant: the variant's options, body
/// fields and headers layered over the model's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    #[serde(default, skip_serializing_if = "Options::is_empty")]
    pub options: Options,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub body: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

impl Variant {
    fn opts(v: Value) -> Variant {
        Variant {
            options: Options::from_value(v),
            ..Default::default()
        }
    }
}

const EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// Claude family, major and minor version from a model id
/// (`claude-sonnet-4-5`, `claude-3-7-sonnet`, `anthropic/claude-opus-4.8`).
pub(crate) fn claude_version(id: &str) -> Option<(String, u32, u32)> {
    let id = id.to_ascii_lowercase();
    let start = id.find("claude-")? + "claude-".len();
    let rest = &id[start..];
    let mut parts = rest.split(['-', '.', '@', ':']);
    let a = parts.next()?;
    let b = parts.next();
    let c = parts.next();
    let small = |s: Option<&str>| {
        s.filter(|s| s.len() <= 2)
            .and_then(|s| s.parse::<u32>().ok())
    };
    if let Ok(major) = a.parse::<u32>() {
        // Version first: claude-3-7-sonnet, claude-3-opus.
        let (minor, family) = match small(b) {
            Some(m) => (m, c?),
            None => (0, b?),
        };
        return Some((family.to_string(), major, minor));
    }
    let major = b?.parse::<u32>().ok()?;
    Some((a.to_string(), major, small(c).unwrap_or(0)))
}

/// Built-in reasoning variants for a model, after OpenCode's
/// `Variant.resolve` with effort support: what a `provider/model#variant`
/// suffix selects when the config names no variant of that name.
pub fn builtin_variants(
    protocol: Protocol,
    model: &str,
    output_limit: Option<u32>,
) -> Vec<(String, Variant)> {
    use serde_json::json;
    let id = model.to_ascii_lowercase();
    let efforts = |values: &[&str], f: &dyn Fn(&str) -> Value| -> Vec<(String, Variant)> {
        values
            .iter()
            .map(|e| (e.to_string(), Variant::opts(f(e))))
            .collect()
    };
    // OpenCode's `budgets`: a high variant at half the room, a max one at all of it.
    let budgets = |min: u32, ceiling: u32, f: &dyn Fn(u32) -> Value| -> Vec<(String, Variant)> {
        let limit = output_limit.unwrap_or(ceiling);
        let max = (ceiling.min(limit)).saturating_sub(1);
        if max == 0 {
            return vec![];
        }
        let high = min.max(max.div_ceil(2)).min(max);
        vec![
            ("high".into(), Variant::opts(f(high))),
            ("max".into(), Variant::opts(f(max))),
        ]
    };
    match protocol {
        // Converse takes Claude's thinking and effort options as they are.
        Protocol::BedrockConverse if id.contains("anthropic.") => {
            builtin_variants(Protocol::AnthropicMessages, model, output_limit)
        }
        Protocol::BedrockConverse => vec![],
        // Claude Code lists each model's efforts (see `claude_agent`).
        Protocol::ClaudeAgent => vec![],
        Protocol::OpenaiChat => {
            if id.contains('/') {
                // Gateways (OpenRouter style) take a `reasoning` object.
                efforts(&EFFORTS, &|e| json!({"reasoning": {"effort": e}}))
            } else {
                efforts(&EFFORTS, &|e| json!({"reasoningEffort": e}))
            }
        }
        Protocol::OpenaiResponses => efforts(
            &["none", "minimal", "low", "medium", "high", "xhigh"],
            &|e| json!({"reasoningEffort": e, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}),
        ),
        Protocol::AnthropicMessages => {
            let Some((family, major, minor)) = claude_version(model) else {
                return vec![];
            };
            let manual = (major == 3 && minor == 7) || (major == 4 && minor < 6);
            let opus45 = family == "opus" && major == 4 && minor == 5;
            if manual && !opus45 {
                return budgets(
                    1024,
                    32_000,
                    &|t| json!({"thinking": {"type": "enabled", "budgetTokens": t}}),
                );
            }
            let thinking = if opus45 {
                let tokens = 16_000u32.min(output_limit.unwrap_or(32_000).saturating_sub(1));
                if tokens < 1024 {
                    return vec![];
                }
                json!({"type": "enabled", "budgetTokens": tokens})
            } else {
                json!({"type": "adaptive", "display": "summarized"})
            };
            let values: &[&str] = if major == 4 && minor == 6 {
                &["low", "medium", "high", "max"]
            } else {
                &["low", "medium", "high", "xhigh", "max"]
            };
            efforts(
                values,
                &|e| json!({"thinking": thinking.clone(), "effort": e}),
            )
        }
        Protocol::GoogleGemini => {
            if id.contains("gemini-2") {
                budgets(
                    512,
                    output_limit.unwrap_or(65_536),
                    &|t| json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": t}}),
                )
            } else {
                efforts(
                    &EFFORTS,
                    &|e| json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": e}}),
                )
            }
        }
    }
}

/// A model variant from config: either `{"options": {...}, "body": {...},
/// "headers": {...}}` (OpenCode's `settings` is accepted for `options`) or
/// a bare options object.
pub fn variant_from_config(v: &Value) -> Variant {
    let Some(m) = v.as_object() else {
        return Variant::default();
    };
    let overlay = ["options", "settings", "body", "headers"]
        .iter()
        .any(|k| m.contains_key(*k));
    if !overlay {
        return Variant::opts(v.clone());
    }
    let mut options = Options::default();
    for k in ["settings", "options"] {
        if let Some(o) = m.get(k) {
            options.merge(&Options::from_value(o.clone()));
        }
    }
    Variant {
        options,
        body: m
            .get("body")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        headers: m
            .get("headers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_normalize_and_merge() {
        let o = Options::from_value(
            json!({"reasoning_effort": "high", "serviceTier": "flex", "service_tier": "auto", "thinking": {"budget_tokens": 1}}),
        );
        assert_eq!(o.reasoning_effort(), Some("high"));
        assert_eq!(o.str("serviceTier"), Some("auto"));
        let top = Options::from_value(json!({"thinking": {"type": "enabled"}}));
        let m = o.merged(&top);
        assert_eq!(
            m.get("thinking"),
            Some(&json!({"budget_tokens": 1, "type": "enabled"}))
        );
    }

    #[test]
    fn budgets_fit_half_the_output() {
        assert_eq!(fit_budget(31_999.0, Some(64_000), 1024.0), 31_999.0);
        assert_eq!(fit_budget(31_999.0, Some(20_000), 1024.0), 10_000.0);
        assert_eq!(fit_budget(31_999.0, Some(1_500), 1024.0), 1024.0);
        assert_eq!(fit_budget(-1.0, Some(32_000), 512.0), -1.0);
    }

    #[test]
    fn claude_versions() {
        for (id, want) in [
            ("claude-sonnet-4-5", ("sonnet", 4, 5)),
            ("claude-sonnet-4-5-20250929", ("sonnet", 4, 5)),
            ("claude-3-7-sonnet-latest", ("sonnet", 3, 7)),
            ("anthropic/claude-opus-4.8", ("opus", 4, 8)),
            ("claude-fable-5-1@20260901", ("fable", 5, 1)),
            ("claude-opus-5-20260901", ("opus", 5, 0)),
            ("claude-sonnet-6", ("sonnet", 6, 0)),
            ("claude-3-opus-20240229", ("opus", 3, 0)),
        ] {
            let (f, a, b) = claude_version(id).unwrap();
            assert_eq!((f.as_str(), a, b), want, "{id}");
        }
        assert!(claude_version("kimi-k2.5").is_none());
    }

    #[test]
    fn builtin_variants_per_protocol() {
        let names = |p, m| -> Vec<String> {
            builtin_variants(p, m, None)
                .into_iter()
                .map(|v| v.0)
                .collect()
        };
        assert_eq!(
            names(Protocol::AnthropicMessages, "claude-sonnet-4-5"),
            ["high", "max"]
        );
        let v = builtin_variants(Protocol::AnthropicMessages, "claude-sonnet-4-5", None);
        assert_eq!(
            v[0].1.options.get("thinking"),
            Some(&json!({"type": "enabled", "budgetTokens": 16000}))
        );
        assert_eq!(
            names(Protocol::AnthropicMessages, "claude-opus-4-8"),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(
            names(Protocol::OpenaiResponses, "gpt-5"),
            ["none", "minimal", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(
            names(Protocol::GoogleGemini, "gemini-3-pro"),
            ["low", "medium", "high"]
        );
        assert_eq!(
            names(Protocol::GoogleGemini, "gemini-2.5-pro"),
            ["high", "max"]
        );
    }

    #[test]
    fn config_variants() {
        let bare = variant_from_config(&json!({"reasoning_effort": "high"}));
        assert_eq!(bare.options.reasoning_effort(), Some("high"));
        let full = variant_from_config(
            &json!({"settings": {"reasoningEffort": "low"}, "body": {"x": 1}, "headers": {"h": "v"}}),
        );
        assert_eq!(full.options.reasoning_effort(), Some("low"));
        assert_eq!(full.body["x"], 1);
        assert_eq!(full.headers, [("h".to_string(), "v".to_string())]);
    }
}
