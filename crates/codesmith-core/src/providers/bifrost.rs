//! Bifrost (github.com/maximhq/bifrost), an open-source LLM gateway, as a
//! built-in provider: models are reached through the gateway with a
//! virtual key, and named as Bifrost names them, `<upstream>/<model>`
//! (`bifrost/openai/some-model`), passed through unchanged.
//!
//! - **Where.** The gateway's root URL is `settings.baseURL` or
//!   `BIFROST_BASE_URL`; there is no default host. A trailing `/v1` is
//!   dropped, so either form works.
//! - **Key.** The virtual key (`BIFROST_API_KEY`, `settings.apiKey`, or
//!   `codesmith auth login bifrost`) goes in `x-bf-vk` on every request,
//!   unless `settings.apiKeyHeader` names another header.
//! - **Which API.** A model whose id starts with `anthropic/` goes to
//!   Bifrost's Anthropic Messages route (`<root>/anthropic/v1/messages`);
//!   every other one to its unified OpenAI Chat API
//!   (`<root>/v1/chat/completions`), a Claude model served by another
//!   upstream (`fireworks/anthropic/…`) included. A model's `protocol`
//!   setting picks another.
//! - **Which route.** `settings.api` (`BIFROST_API`, or a model's own
//!   `api`) picks the OpenAI-shaped route family: `unified`
//!   (`<root>/v1`, the default) or `openai` (Bifrost's OpenAI integration,
//!   `<root>/openai/v1`), for Chat and Responses alike. `openaiBaseURL`,
//!   `anthropicBaseURL`, `geminiBaseURL`, `modelsURL` and `quotaURL` (each
//!   with its `BIFROST_…` variable) put one surface elsewhere ([`Urls`]).
//!   A route the gateway refuses as one it doesn't serve fails with the
//!   URL and the setting; nothing else is tried.
//! - **Models.** The list is the gateway's `GET <root>/v1/models` for the
//!   virtual key, which Bifrost filters to what the key may use. It is
//!   fetched fresh when a session starts ([`session_start`]) and kept in
//!   `live-models.json` under a hash of the gateway and key, never the key
//!   itself; the kept list is only the fallback when that fetch fails.
//!   A model outside the key's list is refused.
//! - **Effort.** A model's levels are those the same model has reached
//!   directly: what the gateway's list says about its reasoning, else the
//!   catalog's entry for the model at the end of its id
//!   (`fireworks/anthropic/claude-x` reads `anthropic/claude-x`), else the
//!   protocol's family rules. They are sent in the chosen API's form:
//!   `thinking` and `effort` on Anthropic Messages, the `reasoning` object
//!   (`effort`, or `max_tokens` for a token budget) on Chat.

use super::{set, Input, Route};
use crate::catalog::{self, variant};
use anyhow::{anyhow, Result};
use codesmith_ai::{Auth, Protocol};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

pub const ID: &str = codesmith_ai::bifrost::ID;
pub const NAME: &str = "Bifrost";
pub const URL_VAR: &str = "BIFROST_BASE_URL";
pub const KEY_VAR: &str = "BIFROST_API_KEY";
pub const KEY_HEADER: &str = codesmith_ai::bifrost::KEY_HEADER;

/// How long one page of the model list may take.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(4);
/// How long a session's start waits for the list before going on with
/// the saved one.
pub const START_WAIT: Duration = Duration::from_secs(5);
/// The page size asked for, and the most pages read (Bifrost's own
/// `DefaultPageSize` and `MaxPaginationRequests`).
const PAGE_SIZE: usize = 1000;
const MAX_PAGES: usize = 20;
/// At most this many models are kept.
const MAX_MODELS: usize = 20_000;

/// The mark on a listed model whose effort levels are known (from the
/// gateway or the catalog), as opposed to one left to the family rules.
const KNOWN: &str = "bifrost";

/// The gateway's root URL: `settings.baseURL`, else `BIFROST_BASE_URL`,
/// without a trailing `/` or `/v1`.
pub fn root(settings: &crate::config::ProviderSettings, env: &super::Env) -> Option<String> {
    set(&settings.base_url)
        .map(str::to_string)
        .or_else(|| env(URL_VAR))
        .map(|u| normalize_root(&u))
        .filter(|u| !u.is_empty())
}

/// `url` without surrounding spaces, trailing slashes or a trailing `/v1`.
pub fn normalize_root(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    u.strip_suffix("/v1")
        .unwrap_or(u)
        .trim_end_matches('/')
        .to_string()
}

/// The API a model is reached through, by the first upstream in its id:
/// `anthropic/…` speaks Anthropic Messages, every other upstream the
/// unified Chat API. A configured protocol (the model's, then the
/// provider's) wins.
pub fn protocol_for(model_id: &str, configured: Option<Protocol>) -> Protocol {
    configured.unwrap_or_else(|| {
        if model_id.split('/').next() == Some("anthropic") {
            Protocol::AnthropicMessages
        } else {
            Protocol::OpenaiChat
        }
    })
}

/// The base URL of `protocol` on the gateway at `root`, with the default
/// (unified) API.
pub fn surface(root: &str, protocol: Protocol) -> String {
    match protocol {
        Protocol::AnthropicMessages => format!("{root}/anthropic/v1"),
        Protocol::GoogleGemini => format!("{root}/genai/v1beta"),
        _ => Api::Unified.root(root),
    }
}

pub const API_VAR: &str = "BIFROST_API";
pub const OPENAI_URL_VAR: &str = "BIFROST_OPENAI_BASE_URL";
pub const ANTHROPIC_URL_VAR: &str = "BIFROST_ANTHROPIC_BASE_URL";
pub const GEMINI_URL_VAR: &str = "BIFROST_GEMINI_BASE_URL";
pub const MODELS_URL_VAR: &str = "BIFROST_MODELS_URL";
pub const QUOTA_URL_VAR: &str = "BIFROST_QUOTA_URL";

/// Which of Bifrost's OpenAI-shaped route families a model is sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    /// Bifrost's unified API, `<root>/v1`.
    Unified,
    /// Bifrost's OpenAI integration, `<root>/openai/v1`.
    Openai,
}

impl Api {
    pub fn parse(s: &str) -> Option<Api> {
        match s.trim().to_ascii_lowercase().as_str() {
            "unified" => Some(Api::Unified),
            "openai" => Some(Api::Openai),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Api::Unified => "unified",
            Api::Openai => "openai",
        }
    }

    /// The OpenAI-shaped root of this API on the gateway at `root`.
    pub fn root(self, root: &str) -> String {
        match self {
            Api::Unified => format!("{root}/v1"),
            Api::Openai => format!("{root}/openai/v1"),
        }
    }
}

/// A URL the gateway is reached at, and the setting it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub url: String,
    pub from: String,
}

/// Every URL a Bifrost gateway is reached at, each from its own setting
/// or the gateway's root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Urls {
    pub root: String,
    pub api: Api,
    /// Where `api` came from: `settings.api`, `BIFROST_API` or the default.
    pub api_from: String,
    /// The configured OpenAI-shaped root, replacing what `api` picks.
    pub openai_override: Option<Resolved>,
    pub anthropic: Resolved,
    pub gemini: Resolved,
    pub models: Resolved,
    pub quota: Resolved,
}

const SETTINGS: &str = "providers.bifrost.settings";

/// An `api` value, or an error naming where it came from.
fn parse_api(v: &str, from: &str) -> Result<Api> {
    Api::parse(v).ok_or_else(|| {
        let shown: String = v.chars().filter(|c| !c.is_control()).take(40).collect();
        anyhow!("{from} must be \"unified\" or \"openai\", not {shown:?}")
    })
}

/// A URL setting (`settings.<name>`, else the variable `var`), without
/// surrounding spaces or a trailing `/`; an error when it isn't an HTTP URL.
fn url_setting(
    value: &Option<String>,
    name: &str,
    var: &str,
    env: &super::Env,
) -> Result<Option<Resolved>> {
    let (url, from) = match set(value) {
        Some(v) => (v.to_string(), format!("{SETTINGS}.{name}")),
        None => match env(var).filter(|v| !v.trim().is_empty()) {
            Some(v) => (v, var.to_string()),
            None => return Ok(None),
        },
    };
    let url = url.trim().trim_end_matches('/').to_string();
    let lower = url.to_ascii_lowercase();
    let host = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"));
    if host.is_none_or(|h| h.is_empty() || h.starts_with('/')) {
        anyhow::bail!("{from} must be an http:// or https:// URL");
    }
    Ok(Some(Resolved { url, from }))
}

/// The URLs of the gateway at `root`, from `settings` and the environment.
pub fn urls_at(
    root: &str,
    settings: &crate::config::ProviderSettings,
    env: &super::Env,
) -> Result<Urls> {
    let s = &settings.plugin;
    let (api, api_from) = match set(&s.api) {
        Some(v) => (
            parse_api(v, &format!("{SETTINGS}.api"))?,
            format!("{SETTINGS}.api"),
        ),
        None => match env(API_VAR).filter(|v| !v.trim().is_empty()) {
            Some(v) => (parse_api(&v, API_VAR)?, API_VAR.to_string()),
            None => (Api::Unified, "the default".to_string()),
        },
    };
    let or = |r: Option<Resolved>, url: String| {
        r.unwrap_or(Resolved {
            url,
            from: format!("{SETTINGS}.baseURL"),
        })
    };
    Ok(Urls {
        root: root.to_string(),
        api,
        api_from,
        openai_override: url_setting(&s.openai_base_url, "openaiBaseURL", OPENAI_URL_VAR, env)?,
        anthropic: or(
            url_setting(
                &s.anthropic_base_url,
                "anthropicBaseURL",
                ANTHROPIC_URL_VAR,
                env,
            )?,
            surface(root, Protocol::AnthropicMessages),
        ),
        gemini: or(
            url_setting(&s.gemini_base_url, "geminiBaseURL", GEMINI_URL_VAR, env)?,
            surface(root, Protocol::GoogleGemini),
        ),
        models: or(
            url_setting(&s.models_url, "modelsURL", MODELS_URL_VAR, env)?,
            format!("{root}/v1/models"),
        ),
        quota: or(
            url_setting(&s.quota_url, "quotaURL", QUOTA_URL_VAR, env)?,
            format!("{root}/api/governance/virtual-keys/quota"),
        ),
    })
}

/// The gateway's URLs, when its root is set.
pub fn urls(settings: &crate::config::ProviderSettings, env: &super::Env) -> Result<Option<Urls>> {
    match root(settings, env) {
        Some(r) => urls_at(&r, settings, env).map(Some),
        None => Ok(None),
    }
}

impl Urls {
    /// The OpenAI-shaped root a model uses: its own `api` (`model_api`),
    /// else `openaiBaseURL`, else the provider's `api`.
    pub fn openai(&self, model_api: Option<(Api, String)>) -> Resolved {
        if let Some((api, from)) = model_api {
            return Resolved {
                url: api.root(&self.root),
                from,
            };
        }
        if let Some(o) = &self.openai_override {
            return o.clone();
        }
        Resolved {
            url: self.api.root(&self.root),
            from: format!("api \"{}\" ({})", self.api.name(), self.api_from),
        }
    }

    /// The base URL of `protocol`.
    pub fn base(&self, protocol: Protocol, model_api: Option<(Api, String)>) -> Resolved {
        match protocol {
            Protocol::AnthropicMessages => self.anthropic.clone(),
            Protocol::GoogleGemini => self.gemini.clone(),
            _ => self.openai(model_api),
        }
    }

    /// One line per surface: its URL and the setting it came from.
    pub fn describe(&self) -> Vec<String> {
        let o = self.openai(None);
        vec![
            format!(
                "openai:    {}/chat/completions, {}/responses (from {})",
                o.url, o.url, o.from
            ),
            format!(
                "anthropic: {}/messages (from {})",
                self.anthropic.url, self.anthropic.from
            ),
            format!(
                "gemini:    {}/models/<id>:streamGenerateContent (from {})",
                self.gemini.url, self.gemini.from
            ),
            format!("models:    {} (from {})", self.models.url, self.models.from),
            format!("quota:     {} (from {})", self.quota.url, self.quota.from),
        ]
    }
}

/// The route of a Bifrost model.
pub fn route(input: &Input) -> Result<Route> {
    let root = input
        .configured_url
        .map(normalize_root)
        .filter(|u| !u.is_empty())
        .or_else(|| root(input.settings, &input.env))
        .ok_or_else(|| {
            anyhow!("Bifrost needs the gateway's URL: set {URL_VAR} or settings.baseURL on the bifrost provider")
        })?;
    let urls = urls_at(&root, input.settings, &input.env)?;
    let protocol = protocol_for(
        input.model_id,
        input.model_protocol.or(input.configured_protocol),
    );
    let model_api = match input.model_api.map(str::trim).filter(|s| !s.is_empty()) {
        Some(v) => {
            let from = format!("providers.bifrost.models.{}.api", input.model_id);
            Some((parse_api(v, &from)?, from))
        }
        None => None,
    };
    Ok(Route {
        protocol,
        base_url: urls.base(protocol, model_api).url,
        auth: Auth::Header(KEY_HEADER.into()),
        api_key: input.api_key.clone(),
        headers: vec![],
        model_id: None,
    })
}

/// Whether Bifrost is set up: a gateway URL and a virtual key.
pub fn ready(cfg: &crate::config::Config) -> bool {
    if cfg.provider_disabled(ID) {
        return false;
    }
    let Some(p) = cfg.provider(ID) else {
        return false;
    };
    root(&p.settings, &super::process_env()).is_some()
        && cfg.provider_key(ID, &p).is_some_and(|k| !k.is_empty())
}

/// The account a kept list belongs to: a hash of the gateway and the key.
pub fn account(root: &str, key: Option<&str>) -> String {
    format!(
        "vk:{}",
        super::live::fingerprint(&format!("{root}\n{}", key.unwrap_or("")))
    )
}

/// Whether a kept list is a Bifrost one.
pub fn is_account(a: &str) -> bool {
    a.starts_with("vk:")
}

/// The catalog provider of a Bifrost upstream name.
fn catalog_provider(upstream: &str) -> &str {
    match upstream {
        "gemini" => "google",
        "vertex" => "google-vertex",
        "bedrock" => "amazon-bedrock",
        "fireworks" => "fireworks-ai",
        "together" => "togetherai",
        other => other,
    }
}

/// The catalog's entry for the model a Bifrost id ends in: `<up>/<model>`
/// first, then each shorter tail (`fireworks/anthropic/claude-x` reads
/// `fireworks-ai`'s `anthropic/claude-x`, then `anthropic`'s `claude-x`).
pub fn upstream_model<'a>(c: &'a catalog::Catalog, id: &str) -> Option<&'a catalog::Model> {
    let parts: Vec<&str> = id.split('/').collect();
    (0..parts.len().saturating_sub(1)).find_map(|i| {
        let p = catalog_provider(parts[i]);
        c.model(p, &parts[i + 1..].join("/"))
    })
}

/// A price per million tokens from Bifrost's per-token string, when it is
/// a positive number.
fn per_million(v: &Value) -> Option<f64> {
    let n = match v {
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    (n.is_finite() && n > 0.0).then_some(n * 1_000_000.0)
}

/// The rates of a listed model's `pricing`, kept per million tokens (as
/// configured costs are) and per request; a missing or zero price is left
/// out.
fn pricing(m: &Map<String, Value>) -> Option<Value> {
    let p = m.get("pricing")?.as_object()?;
    let mut out = Map::new();
    for (from, to, scale) in [
        ("prompt", "input", true),
        ("completion", "output", true),
        ("input_cache_read", "cache_read", true),
        ("input_cache_write", "cache_write", true),
        ("request", "request", false),
    ] {
        let Some(v) = p.get(from) else { continue };
        let n = if scale {
            per_million(v)
        } else {
            per_million(v).map(|n| n / 1_000_000.0)
        };
        if let Some(n) = n {
            out.insert(to.into(), json!(n));
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// The gateway's answer as kept entries: [`super::hosted::normalize`]'s,
/// with each model's `pricing`.
pub fn normalize(data: &[Value]) -> Vec<Value> {
    let mut out = super::hosted::normalize(&json!({ "data": data }));
    for e in &mut out {
        let id = e["id"].as_str().unwrap_or("").to_string();
        let raw = data
            .iter()
            .filter_map(Value::as_object)
            .find(|m| m.get("id").and_then(Value::as_str) == Some(id.as_str()));
        if let Some(p) = raw.and_then(pricing) {
            e["pricing"] = p;
        }
    }
    out
}

/// A failed GET of a gateway URL (`setting` picks it) as an error: a route
/// the gateway doesn't serve names the URL and the setting, never the key.
fn get_error(
    e: codesmith_ai::Error,
    p: &codesmith_ai::Provider,
    url: &str,
    setting: &str,
) -> anyhow::Error {
    use codesmith_ai::bifrost as bf;
    match &e {
        codesmith_ai::Error::Status { status, body } if bf::is_route_refusal(*status, body) => {
            let shown = reqwest::Url::parse(url)
                .map(|u| bf::shown_url(&u))
                .unwrap_or_else(|_| codesmith_ai::redact_urls(url));
            anyhow!(
                "{shown} answered HTTP {status} ({}): the gateway doesn't serve this route; set {setting} to where it does",
                bf::gateway_message(body, p.api_key.as_deref())
            )
        }
        _ => anyhow!("{}", bf::redact(&e.to_string(), p.api_key.as_deref())),
    }
}

/// Read the whole model list for `p`'s key (every page), from `urls`'s
/// model list.
pub async fn fetch(
    client: &reqwest::Client,
    p: &codesmith_ai::Provider,
    urls: &Urls,
) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    let list = &urls.models.url;
    let sep = if list.contains('?') { '&' } else { '?' };
    let setting = format!("{SETTINGS}.modelsURL ({MODELS_URL_VAR})");
    for _ in 0..MAX_PAGES {
        let mut url = format!("{list}{sep}page_size={PAGE_SIZE}");
        if let Some(t) = &token {
            url.push_str("&page_token=");
            url.push_str(&super::encode(t));
        }
        let v = codesmith_ai::bifrost::get_json(p, client, &url, PAGE_TIMEOUT)
            .await
            .map_err(|e| get_error(e, p, list, &setting))?;
        let page = v["data"]
            .as_array()
            .ok_or_else(|| anyhow!("the gateway's answer has no data list"))?;
        all.extend(page.iter().cloned());
        if all.len() >= MAX_MODELS {
            break;
        }
        token = v["next_page_token"]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= 4096)
            .map(str::to_string);
        if token.is_none() || page.is_empty() {
            break;
        }
    }
    Ok(all)
}

/// Fetch and keep the list for `p`'s key: the chat model ids. On a
/// failure the list kept before stays.
pub async fn refresh(
    client: &reqwest::Client,
    path: &Path,
    p: &codesmith_ai::Provider,
    urls: &Urls,
) -> Result<Vec<String>> {
    let root = urls.root.as_str();
    if p.api_key.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!(
            "Bifrost needs a virtual key: set {KEY_VAR}, settings.apiKey, or run `codesmith auth login bifrost`"
        );
    }
    let data = fetch(client, p, urls)
        .await
        .map_err(|e| anyhow!("listing Bifrost models: {e:#}"))?;
    let raw = normalize(&data);
    let ids = super::hosted::chat_ids(&raw);
    super::live::save_in(path, ID, &account(root, p.api_key.as_deref()), root, raw)?;
    Ok(ids)
}

/// An effort level and, for a token budget, its size.
#[derive(Debug, Clone, PartialEq)]
pub struct Level {
    pub id: String,
    pub budget: Option<i64>,
}

/// The levels of generated variants: their ids, with any token budget
/// they set.
pub fn levels_of(variants: &[variant::Variant]) -> Vec<Level> {
    variants
        .iter()
        .map(|v| {
            let s = v.settings.clone().map(Value::Object).unwrap_or_default();
            let budget = s["thinking"]["budgetTokens"]
                .as_i64()
                .or_else(|| s["reasoning"]["max_tokens"].as_i64())
                .or_else(|| s["thinkingConfig"]["thinkingBudget"].as_i64())
                .filter(|_| {
                    s["effort"].is_null()
                        && s["reasoning"]["effort"].is_null()
                        && s["thinkingConfig"]["thinkingLevel"].is_null()
                })
                // Gemini's `thinkingBudget: 0` is the `none` level, not a budget.
                .filter(|n| *n > 0 || v.id != "none");
            Level {
                id: v.id.clone(),
                budget,
            }
        })
        .collect()
}

fn settings_variant(id: &str, s: Value) -> variant::Variant {
    variant::Variant {
        id: id.to_string(),
        settings: match s {
            Value::Object(m) => Some(m),
            _ => None,
        },
        body: None,
        headers: None,
    }
}

/// `levels` spelled for `protocol`: Anthropic's `thinking` and `effort`,
/// the Chat API's `reasoning` object, the Responses API's effort,
/// Gemini's `thinkingConfig`.
pub fn spell(levels: &[Level], protocol: Protocol) -> Vec<variant::Variant> {
    levels
        .iter()
        .map(|l| {
            let s = match (protocol, l.budget, l.id.as_str()) {
                (Protocol::AnthropicMessages, Some(n), _) => {
                    json!({"thinking": {"type": "enabled", "budgetTokens": n}})
                }
                (Protocol::AnthropicMessages, None, "none") => {
                    json!({"thinking": {"type": "disabled"}})
                }
                (Protocol::AnthropicMessages, None, e) => json!({
                    "thinking": {"type": "adaptive", "display": "summarized"},
                    "effort": e
                }),
                (Protocol::GoogleGemini, Some(n), _) => {
                    json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": n}})
                }
                (Protocol::GoogleGemini, None, "none") => {
                    json!({"thinkingConfig": {"thinkingBudget": 0}})
                }
                (Protocol::GoogleGemini, None, e) => {
                    json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": e}})
                }
                (Protocol::OpenaiResponses, _, e) => json!({
                    "reasoningEffort": e,
                    "reasoningSummary": "auto",
                    "include": ["reasoning.encrypted_content"]
                }),
                (_, Some(n), _) => json!({"reasoning": {"max_tokens": n}}),
                (_, None, e) => json!({"reasoning": {"effort": e}}),
            };
            settings_variant(&l.id, s)
        })
        .collect()
}

/// A listed model's variants for `protocol`, when its levels are known;
/// `None` leaves them to the protocol's family rules.
pub fn variants(m: &catalog::Model, protocol: Protocol) -> Option<Vec<variant::Variant>> {
    if m.package.as_deref() != Some(KNOWN) {
        return None;
    }
    if protocol == protocol_for(&m.id, None) {
        return Some(m.variants.clone());
    }
    Some(spell(&levels_of(&m.variants), protocol))
}

/// The ordinary levels for a model said to reason without naming any.
fn default_levels() -> Vec<Level> {
    ["low", "medium", "high"]
        .iter()
        .map(|e| Level {
            id: e.to_string(),
            budget: None,
        })
        .collect()
}

/// A kept list as catalog models: each with the details of the catalog's
/// entry for its upstream model when there is one, what the gateway said
/// over them, and its effort levels spelled for its API.
pub fn derive(raw: &[Value], c: Option<&catalog::Catalog>) -> Vec<catalog::Model> {
    let mut out = Vec::new();
    for e in raw {
        let Some(id) = e["id"].as_str() else {
            continue;
        };
        let template = c.and_then(|c| upstream_model(c, id));
        let mut m = match template {
            Some(t) => catalog::Model {
                id: id.to_string(),
                model_id: id.to_string(),
                provider_id: ID.to_string(),
                ..t.clone()
            },
            None => super::live::blank(ID, id),
        };
        m.kind = e["kind"].as_str().unwrap_or("chat").to_string();
        m.enabled = true;
        // Bifrost prices and routes; the catalog's headers and body are the
        // upstream's own.
        m.headers = None;
        m.body = None;
        m.settings = None;
        if let Some(n) = e["name"].as_str() {
            m.name = n.to_string();
        }
        if let Some(cx) = e["context"].as_u64() {
            m.limit.context = cx;
            m.limit.output = m.limit.output.min(cx);
        }
        if let Some(o) = e["output"].as_u64() {
            m.limit.output = o.min(m.limit.context.max(1));
        }
        if let Some(r) = e["released"].as_i64() {
            m.time.released = r;
        }
        let protocol = protocol_for(id, None);
        // The template's own variants were spelled for its provider: its
        // levels are what the model offers reached directly.
        let catalog_levels = template.map(|t| levels_of(&t.variants));
        let levels = match (e["reasoning"].as_bool(), catalog_levels) {
            (Some(false), _) => Some(vec![]),
            (Some(true), from_catalog) => {
                let api: Vec<Level> = e["efforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|l| Level {
                        id: l.to_string(),
                        budget: None,
                    })
                    .collect();
                Some(if !api.is_empty() {
                    api
                } else {
                    from_catalog
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(default_levels)
                })
            }
            (None, from_catalog) => from_catalog,
        };
        match levels {
            Some(levels) => {
                // Reached on Anthropic Messages, a Claude catalog entry's own
                // variants are already in that API's form.
                let direct = template.filter(|t| {
                    protocol == Protocol::AnthropicMessages
                        && t.provider_id == "anthropic"
                        && levels_of(&t.variants) == levels
                });
                m.variants = match direct {
                    Some(t) => t.variants.clone(),
                    None => spell(&levels, protocol),
                };
                m.reasoning.supported = !levels.is_empty();
                m.package = Some(KNOWN.into());
            }
            None => {
                m.variants = vec![];
                m.package = None;
            }
        }
        out.push(m);
    }
    out
}

/// The rates the gateway's list gives a model, per million tokens (and
/// `request` per call), for the key `account`.
pub fn gateway_rates(kept: &Value, account: &str, model: &str) -> Option<Map<String, Value>> {
    if kept["account"].as_str() != Some(account) {
        return None;
    }
    kept["models"]
        .as_array()?
        .iter()
        .find(|m| m["id"].as_str() == Some(model))?
        .get("pricing")?
        .as_object()
        .cloned()
}

/// What starting a session's list fetch came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The gateway answered with this many models.
    Fresh(usize),
    /// It didn't; the list saved at `saved` (ms since the epoch) stands in.
    Saved { error: String, saved: u64 },
    /// It didn't, and no list is saved for this key.
    Missing { error: String },
}

impl Outcome {
    /// What to tell the user, if anything.
    pub fn notice(&self) -> Option<String> {
        match self {
            Outcome::Fresh(_) => None,
            Outcome::Saved { error, saved } => Some(format!(
                "Bifrost: couldn't fetch this key's model list ({error}); using the list saved {}",
                ago(*saved)
            )),
            Outcome::Missing { error } => Some(format!(
                "Bifrost: couldn't fetch this key's model list ({error}), and none is saved; its models are refused until it can be fetched"
            )),
        }
    }
}

fn ago(ms: u64) -> String {
    let secs = super::oauth::now_ms().saturating_sub(ms) / 1000;
    match secs {
        0..=119 => "just now".into(),
        120..=7199 => format!("{} minutes ago", secs / 60),
        7200..=172_799 => format!("{} hours ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// The session-start fetch of the list, shared by everyone waiting on it.
pub type Pending = futures_util::future::Shared<futures_util::future::BoxFuture<'static, Outcome>>;

/// Start fetching the list for the configured key, as a session starts,
/// when Bifrost is set up (a URL and a key) and not disabled. It runs
/// beside the rest of the start; [`wait`] for it where the list is needed.
/// A fetch already running is shared rather than started twice.
pub fn session_start(cfg: &crate::config::Config) -> Option<Pending> {
    session_start_in(cfg, &super::live::file())
}

pub fn session_start_in(cfg: &crate::config::Config, path: &Path) -> Option<Pending> {
    use futures_util::FutureExt;
    use std::sync::Mutex;
    static RUNNING: Mutex<Option<(String, Pending)>> = Mutex::new(None);
    if cfg.provider_disabled(ID) || tokio::runtime::Handle::try_current().is_err() {
        return None;
    }
    let p = cfg.provider(ID)?;
    let urls = urls(&p.settings, &super::process_env()).ok()??;
    let root = urls.root.clone();
    let m = cfg.resolve_model(&format!("{ID}/_")).ok()?;
    m.provider.api_key.as_deref().filter(|k| !k.is_empty())?;
    let account = account(&root, m.provider.api_key.as_deref());
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((a, f)) = running.as_ref() {
        if *a == account && f.peek().is_none() {
            return Some(f.clone());
        }
    }
    let path = path.to_path_buf();
    let task = tokio::spawn({
        let account = account.clone();
        async move {
            let client = reqwest::Client::new();
            let r = tokio::time::timeout(START_WAIT, refresh(&client, &path, &m.provider, &urls))
                .await
                .unwrap_or_else(|_| Err(anyhow!("the gateway took too long")));
            match r {
                Ok(ids) => {
                    unlisted(&account, None);
                    Outcome::Fresh(ids.len())
                }
                Err(e) => {
                    let error = codesmith_ai::bifrost::redact(
                        &format!("{e:#}"),
                        m.provider.api_key.as_deref(),
                    );
                    match super::live::load_in(&path, ID)
                        .filter(|k| k["account"].as_str() == Some(account.as_str()))
                    {
                        Some(k) => Outcome::Saved {
                            error,
                            saved: k["fetched"].as_u64().unwrap_or(0),
                        },
                        None => {
                            unlisted(&account, Some((&root, &error)));
                            Outcome::Missing { error }
                        }
                    }
                }
            }
        }
    });
    let f: Pending = async move {
        task.await.unwrap_or(Outcome::Missing {
            error: "the fetch stopped".into(),
        })
    }
    .boxed()
    .shared();
    *running = Some((account, f.clone()));
    Some(f)
}

/// Wait for a session-start fetch: its outcome, or `None` when it takes
/// longer than `limit` (the saved list stands in meanwhile).
pub async fn wait(f: Pending, limit: Duration) -> Option<Outcome> {
    tokio::time::timeout(limit, f).await.ok()
}

/// The keys whose session-start fetch failed with no list saved: the
/// gateway's URL and why, by account.
static UNLISTED: std::sync::Mutex<Vec<(String, String, String)>> =
    std::sync::Mutex::new(Vec::new());

/// Note that `account`'s list couldn't be fetched (`Some((root, why))`)
/// and none is saved, or that it now can be (`None`).
fn unlisted(account: &str, failed: Option<(&str, &str)>) {
    let mut u = UNLISTED.lock().unwrap_or_else(|e| e.into_inner());
    u.retain(|(a, _, _)| a != account);
    if let Some((root, why)) = failed {
        u.push((account.to_string(), root.to_string(), why.to_string()));
    }
}

/// Whether `model` may be used with the kept list `kept` (for `account`):
/// `Ok` when it's listed, or when no list is kept for this key and none
/// has failed to come either (nothing has asked the gateway yet). When the
/// gateway couldn't be reached at a session's start and no list is saved,
/// every model is refused, as nothing says what the key allows.
pub fn check(kept: Option<&Value>, account: &str, model: &str) -> Result<()> {
    let Some(k) = kept.filter(|k| k["account"].as_str() == Some(account)) else {
        let u = UNLISTED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, root, why)) = u.iter().find(|(a, _, _)| a == account) {
            return Err(anyhow!(
                "bifrost/{model}: the Bifrost gateway at {root} couldn't be reached to list the models this key allows ({why}), and there's no saved list yet; check the URL, the key and the gateway, then try again"
            ));
        }
        return Ok(());
    };
    let ids: Vec<&str> = k["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .collect();
    if ids.contains(&model) {
        return Ok(());
    }
    Err(anyhow!(
        "bifrost/{model} isn't among the {} models this Bifrost virtual key may use; `codesmith models bifrost` lists them",
        ids.len()
    ))
}

/// A budget's spend at one moment, from the gateway's quota answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    pub id: String,
    pub usage: f64,
    pub last_reset: String,
}

/// How long a quota read may take.
pub const QUOTA_TIMEOUT: Duration = Duration::from_millis(1500);

/// The key's budgets (`GET <quota URL>`, by default
/// `<root>/api/governance/virtual-keys/quota`, authenticated by the virtual
/// key alone); `None` when the read fails.
pub async fn quota(
    client: &reqwest::Client,
    p: &codesmith_ai::Provider,
    url: &str,
) -> Option<Vec<Budget>> {
    let v = codesmith_ai::bifrost::get_json(p, client, url, QUOTA_TIMEOUT)
        .await
        .ok()?;
    Some(budgets(&v))
}

/// The budgets of a quota answer.
pub fn budgets(v: &Value) -> Vec<Budget> {
    v["budgets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            Some(Budget {
                id: b["id"].as_str()?.to_string(),
                usage: b["current_usage"].as_f64().filter(|u| u.is_finite())?,
                last_reset: b["last_reset"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// What the key spent between two quota reads, by the first budget (by
/// id) present in both whose cycle didn't reset in between: its id and
/// the difference. `None` without a budget, or when every one reset.
pub fn spent(before: &[Budget], after: &[Budget]) -> Option<(String, f64)> {
    let mut ids: Vec<&Budget> = before.iter().collect();
    ids.sort_by(|a, b| a.id.cmp(&b.id));
    ids.into_iter().find_map(|b| {
        let a = after.iter().find(|a| a.id == b.id)?;
        (a.last_reset == b.last_reset && a.usage >= b.usage)
            .then(|| (b.id.clone(), a.usage - b.usage))
    })
}

#[cfg(test)]
#[path = "bifrost_tests.rs"]
mod tests;
