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

/// The base URL of `protocol` on the gateway at `root`.
pub fn surface(root: &str, protocol: Protocol) -> String {
    match protocol {
        Protocol::AnthropicMessages => format!("{root}/anthropic/v1"),
        Protocol::GoogleGemini => format!("{root}/genai/v1beta"),
        _ => format!("{root}/v1"),
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
    let protocol = protocol_for(
        input.model_id,
        input.model_protocol.or(input.configured_protocol),
    );
    Ok(Route {
        protocol,
        base_url: surface(&root, protocol),
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

/// Read the whole model list for `p`'s key (every page), from the gateway
/// at `root`.
pub async fn fetch(
    client: &reqwest::Client,
    p: &codesmith_ai::Provider,
    root: &str,
) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut url = format!("{root}/v1/models?page_size={PAGE_SIZE}");
        if let Some(t) = &token {
            url.push_str("&page_token=");
            url.push_str(&super::encode(t));
        }
        let v = codesmith_ai::bifrost::get_json(p, client, &url, PAGE_TIMEOUT)
            .await
            .map_err(|e| anyhow!("{e}"))?;
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
    root: &str,
) -> Result<Vec<String>> {
    if p.api_key.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!(
            "Bifrost needs a virtual key: set {KEY_VAR}, settings.apiKey, or run `codesmith auth login bifrost`"
        );
    }
    let data = fetch(client, p, root)
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
                .filter(|_| s["effort"].is_null() && s["reasoning"]["effort"].is_null());
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
/// the Chat API's `reasoning` object, the Responses API's effort.
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
                "Bifrost: couldn't fetch this key's model list ({error}), and none is saved; models aren't checked until it can be fetched"
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
    let root = root(&p.settings, &super::process_env())?;
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
            let r = tokio::time::timeout(START_WAIT, refresh(&client, &path, &m.provider, &root))
                .await
                .unwrap_or_else(|_| Err(anyhow!("the gateway took too long")));
            match r {
                Ok(ids) => Outcome::Fresh(ids.len()),
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
                        None => Outcome::Missing { error },
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

/// Whether `model` may be used with the kept list `kept` (for `account`):
/// `Ok` when it's listed or no list is kept for this key.
pub fn check(kept: Option<&Value>, account: &str, model: &str) -> Result<()> {
    let Some(k) = kept.filter(|k| k["account"].as_str() == Some(account)) else {
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

/// The key's budgets (`GET <root>/api/governance/virtual-keys/quota`,
/// authenticated by the virtual key alone); `None` when the read fails.
pub async fn quota(
    client: &reqwest::Client,
    p: &codesmith_ai::Provider,
    root: &str,
) -> Option<Vec<Budget>> {
    let url = format!("{root}/api/governance/virtual-keys/quota");
    let v = codesmith_ai::bifrost::get_json(p, client, &url, QUOTA_TIMEOUT)
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
