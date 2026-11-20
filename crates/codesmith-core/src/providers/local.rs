//! Models served by local servers (after OpenCode's `plugin/provider/local`,
//! `lmstudio`, `ollama` and `vllm`): LM Studio's `/api/v1/models`, Ollama's
//! `/api/tags` with `/api/show`, vLLM's `/health` and `/v1/models`, and the
//! OpenAI-compatible `/v1/models` of llama.cpp, oMLX and other servers.
//!
//! A discovered list is kept like any live list (see [`super::live`]), for
//! the endpoint it came from, so model resolution reads the models' context
//! windows and capabilities without asking again. Only the configured base
//! URL, or a built-in provider's default loopback address, is contacted:
//! `codesmith models <provider>` asks any configured endpoint, and the
//! listings that discover on their own ([`discover_local`]: `codesmith
//! models`, the model picker) ask only the ones
//! [`ProviderConfig::probe_unprompted`](crate::config::ProviderConfig::probe_unprompted)
//! allows: loopback ones, or any a provider's `local: true` vouches for.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

/// How a server lists its models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    LmStudio,
    Ollama,
    Vllm,
    /// OpenAI's `GET /v1/models` (llama.cpp, oMLX and the like).
    OpenAi,
}

/// The local providers built in, with their discovery and display name.
pub const BUILTIN: &[(&str, Kind, &str)] = &[
    ("lmstudio", Kind::LmStudio, "LM Studio"),
    ("ollama", Kind::Ollama, "Ollama"),
    ("vllm", Kind::Vllm, "vLLM"),
    ("llamacpp", Kind::OpenAi, "llama.cpp"),
];

/// How a provider's models are discovered: a built-in local provider's own
/// way, or `settings.discovery` (`lmstudio`, `ollama`, `vllm` or `openai`)
/// on a configured one.
pub fn kind(provider_id: &str, settings: &crate::config::ProviderSettings) -> Option<Kind> {
    if let Some(k) = settings.discovery.as_deref() {
        return match k {
            "lmstudio" => Some(Kind::LmStudio),
            "ollama" => Some(Kind::Ollama),
            "vllm" => Some(Kind::Vllm),
            "openai" => Some(Kind::OpenAi),
            _ => None,
        };
    }
    BUILTIN.iter().find(|b| b.0 == provider_id).map(|b| b.1)
}

/// Remote hosts and tunnels can take several seconds to connect.
const TIMEOUT: Duration = Duration::from_secs(15);

/// The server root of a base URL: the URL without its API suffix
/// (`/v1`, LM Studio's `/api/v1`, Ollama's `/api`), query or fragment.
/// `None` when it is not an http(s) URL.
pub fn root(kind: Kind, base_url: &str) -> Option<String> {
    let u = url::Url::parse(base_url.trim_end_matches('/')).ok()?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return None;
    }
    let path = u.path().trim_end_matches('/');
    let suffixes: &[&str] = match kind {
        Kind::LmStudio => &["/api/v1", "/v1"],
        Kind::Ollama => &["/v1", "/api"],
        Kind::Vllm | Kind::OpenAi => &["/v1"],
    };
    let prefix = suffixes
        .iter()
        .find_map(|s| path.strip_suffix(s))
        .unwrap_or(path)
        .to_string();
    let mut r = u.clone();
    r.set_path(&prefix);
    r.set_query(None);
    r.set_fragment(None);
    Some(r.to_string().trim_end_matches('/').to_string())
}

/// A discovered model, as kept (OpenCode's `LocalModel`).
fn model(id: &str, name: Option<&str>, family: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), json!(id));
    m.insert(
        "name".into(),
        json!(name.filter(|n| !n.is_empty()).unwrap_or(id)),
    );
    if let Some(f) = family.filter(|f| !f.is_empty()) {
        m.insert("family".into(), json!(f));
    }
    m
}

async fn get(client: &reqwest::Client, url: &str, key: Option<&str>) -> Result<reqwest::Response> {
    let mut rb = client
        .get(url)
        .header("accept", "application/json")
        .timeout(TIMEOUT);
    if let Some(k) = key.filter(|k| !k.is_empty()) {
        rb = rb.bearer_auth(k);
    }
    let r = rb
        .send()
        .await
        .map_err(|e| anyhow!("{}", codesmith_ai::Error::from(e)))?;
    if !r.status().is_success() {
        anyhow::bail!("{url}: HTTP {}", r.status().as_u16());
    }
    Ok(r)
}

/// The most of a model list or model details read from a server.
pub const MAX_ANSWER: usize = 4 * 1024 * 1024;

/// A response's JSON body, read up to [`MAX_ANSWER`] bytes.
async fn json_capped(mut r: reqwest::Response, url: &str) -> Result<Value> {
    if r.content_length().is_some_and(|n| n > MAX_ANSWER as u64) {
        anyhow::bail!("{url}: the answer is larger than {} MiB", MAX_ANSWER >> 20);
    }
    let mut body = Vec::new();
    while let Some(chunk) = r
        .chunk()
        .await
        .map_err(|e| anyhow!("{}", codesmith_ai::Error::from(e)))?
    {
        if body.len() + chunk.len() > MAX_ANSWER {
            anyhow::bail!("{url}: the answer is larger than {} MiB", MAX_ANSWER >> 20);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| anyhow!("{url}: the answer is not JSON"))
}

async fn get_json(client: &reqwest::Client, url: &str, key: Option<&str>) -> Result<Value> {
    let r = get(client, url, key).await?;
    json_capped(r, url).await
}

/// What is kept of an Ollama `/api/show` answer: only what the model list
/// reads (its capabilities, family and context length).
fn show_summary(info: &Value) -> Value {
    let context: Map<String, Value> = info["model_info"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.ends_with(".context_length"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut out = json!({
        "capabilities": info["capabilities"].as_array().cloned().unwrap_or_default(),
        "model_info": context,
    });
    if let Some(f) = info["details"]["family"].as_str() {
        out["details"] = json!({"family": f});
    }
    out
}

/// The server's models, as kept: `{id, name, family?, tools?, vision?,
/// context?, type?}`, sorted by id. `previous` is the list kept before,
/// whose Ollama details are reused for an unchanged digest.
pub async fn discover(
    client: &reqwest::Client,
    kind: Kind,
    root: &str,
    key: Option<&str>,
    previous: &[Value],
) -> Result<Vec<Value>> {
    let mut out: Vec<Map<String, Value>> = match kind {
        Kind::LmStudio => {
            lmstudio(&get_json(client, &format!("{root}/api/v1/models"), key).await?)?
        }
        Kind::Ollama => ollama(client, root, key, previous).await?,
        Kind::Vllm => {
            get(client, &format!("{root}/health"), key).await?;
            vllm(&get_json(client, &format!("{root}/v1/models"), key).await?)?
        }
        Kind::OpenAi => openai(&get_json(client, &format!("{root}/v1/models"), key).await?)?,
    };
    out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(out.into_iter().map(Value::Object).collect())
}

fn list<'a>(v: &'a Value, field: &str) -> Result<&'a Vec<Value>> {
    v[field]
        .as_array()
        .ok_or_else(|| anyhow!("the answer has no {field} list"))
}

/// LM Studio: language and decision models, with the smallest loaded
/// context (else the maximum).
pub fn lmstudio(v: &Value) -> Result<Vec<Map<String, Value>>> {
    let mut out = vec![];
    for m in list(v, "models")? {
        let kind = m["type"].as_str().unwrap_or("");
        let Some(key) = m["key"].as_str().filter(|k| !k.is_empty()) else {
            continue;
        };
        if kind != "llm" && kind != "decision" {
            continue;
        }
        let mut e = model(key, m["display_name"].as_str(), m["architecture"].as_str());
        if kind == "decision" {
            e.insert("type".into(), json!("decision"));
        }
        if let Some(t) = m["capabilities"]["trained_for_tool_use"].as_bool() {
            e.insert("tools".into(), json!(t));
        }
        if let Some(v) = m["capabilities"]["vision"]
            .as_bool()
            .or(m["has_vision_adapter"].as_bool())
        {
            e.insert("vision".into(), json!(v));
        }
        let loaded: Vec<i64> = m["loaded_instances"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["config"]["context_length"].as_i64())
            .collect();
        let context = loaded
            .iter()
            .min()
            .copied()
            .or(m["max_context_length"].as_i64());
        if let Some(c) = context {
            e.insert("context".into(), json!(c));
        }
        out.push(e);
    }
    Ok(out)
}

/// vLLM: the models it serves itself, with their maximum length.
pub fn vllm(v: &Value) -> Result<Vec<Map<String, Value>>> {
    Ok(list(v, "data")?
        .iter()
        .filter(|m| m["owned_by"] == "vllm")
        .filter_map(|m| {
            let id = m["id"].as_str().filter(|i| !i.is_empty())?;
            let mut e = model(id, None, None);
            if let Some(c) = m["max_model_len"].as_i64() {
                e.insert("context".into(), json!(c));
            }
            Some(e)
        })
        .collect())
}

/// OpenAI's model list, with the context window the server reports, if it
/// reports one (vLLM's `max_model_len`, llama.cpp's `meta.n_ctx_train`,
/// `context_length` and the like).
pub fn openai(v: &Value) -> Result<Vec<Map<String, Value>>> {
    Ok(list(v, "data")?
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str().filter(|i| !i.is_empty())?;
            let mut e = model(id, m["name"].as_str(), None);
            let context = [
                &m["max_model_len"],
                &m["context_length"],
                &m["context_window"],
                &m["max_context_length"],
                &m["meta"]["n_ctx"],
                &m["meta"]["n_ctx_train"],
            ]
            .into_iter()
            .find_map(|c| c.as_i64());
            if let Some(c) = context {
                e.insert("context".into(), json!(c));
            }
            Some(e)
        })
        .collect())
}

/// Ollama: the models that complete text, with `/api/show`'s capabilities
/// and context, asked again only when a model's digest changed. A model
/// whose details cannot be read is left out.
async fn ollama(
    client: &reqwest::Client,
    root: &str,
    key: Option<&str>,
    previous: &[Value],
) -> Result<Vec<Map<String, Value>>> {
    let tags = get_json(client, &format!("{root}/api/tags"), key).await?;
    let mut summaries: Vec<Value> = list(&tags, "models")?
        .iter()
        .filter(|m| m["model"].as_str().is_some_and(|m| !m.is_empty()))
        .cloned()
        .collect();
    summaries.sort_by(|a, b| a["model"].as_str().cmp(&b["model"].as_str()));
    let show = |m: Value| async move {
        let id = m["model"].as_str().unwrap_or_default().to_string();
        let digest = m["digest"].as_str().unwrap_or_default().to_string();
        let saved = previous
            .iter()
            .find(|p| p["id"] == id.as_str() && p["digest"] == digest.as_str())
            .and_then(|p| p.get("show").cloned());
        let info = match saved {
            Some(s) => s,
            None => {
                let mut rb = client
                    .post(format!("{root}/api/show"))
                    .header("accept", "application/json")
                    .json(&json!({"model": id}))
                    .timeout(TIMEOUT);
                if let Some(k) = key.filter(|k| !k.is_empty()) {
                    rb = rb.bearer_auth(k);
                }
                let r = rb.send().await.ok()?;
                if !r.status().is_success() {
                    return None;
                }
                show_summary(&json_capped(r, &format!("{root}/api/show")).await.ok()?)
            }
        };
        Some((m, digest, info))
    };
    use futures_util::StreamExt;
    let shown: Vec<_> = futures_util::stream::iter(summaries)
        .map(show)
        .buffered(4)
        .collect()
        .await;
    let mut out = vec![];
    for (m, digest, info) in shown.into_iter().flatten() {
        let caps: Vec<&str> = info["capabilities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str())
            .collect();
        if !caps.contains(&"completion") {
            continue;
        }
        let id = m["model"].as_str().unwrap_or_default();
        let family = info["details"]["family"]
            .as_str()
            .filter(|f| !f.is_empty())
            .or(m["details"]["family"].as_str());
        let mut e = model(id, m["name"].as_str(), family);
        e.insert("tools".into(), json!(caps.contains(&"tools")));
        e.insert("vision".into(), json!(caps.contains(&"vision")));
        let context = info["model_info"].as_object().and_then(|i| {
            i.iter()
                .filter(|(k, _)| k.ends_with(".context_length"))
                .find_map(|(_, v)| v.as_i64().filter(|c| *c > 0))
        });
        if let Some(c) = context {
            e.insert("context".into(), json!(c));
        }
        e.insert("digest".into(), json!(digest));
        e.insert("show".into(), info);
        out.push(e);
    }
    Ok(out)
}

/// A kept list as catalog models: OpenCode's defaults for a custom model
/// (200,000 tokens of context, 32,000 out) under what the server said.
/// Decision models are kept as `decision` models.
pub fn derive(provider_id: &str, raw: &[Value]) -> Vec<crate::catalog::Model> {
    raw.iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            let mut c = super::live::blank(provider_id, id);
            c.name = m["name"].as_str().unwrap_or(id).to_string();
            c.family = m["family"].as_str().map(str::to_string);
            c.capabilities.tools = m["tools"].as_bool().unwrap_or(false);
            if m["vision"].as_bool() == Some(true) {
                c.capabilities.input.push("image".into());
            }
            if let Some(n) = m["context"].as_i64().filter(|n| *n > 0) {
                c.limit.context = n as u64;
            }
            if m["type"] == "decision" {
                c.kind = "decision".into();
                c.capabilities.output = vec!["decision".into()];
            }
            Some(c)
        })
        .collect()
}

/// The account a local list is kept for: the endpoint it came from, so a
/// list from another endpoint is not used.
pub fn account(root: &str) -> String {
    format!("endpoint:{root}")
}

/// Discover and keep a local provider's models: their ids. On a failure
/// the list kept before stays.
pub async fn refresh(
    client: &reqwest::Client,
    path: &Path,
    provider_id: &str,
    kind: Kind,
    base_url: &str,
    key: Option<&str>,
) -> Result<Vec<String>> {
    let root = root(kind, base_url)
        .ok_or_else(|| anyhow!("{provider_id}: {base_url:?} is not an http(s) URL"))?;
    let account = account(&root);
    // Details are reused only for the same endpoint and key.
    let auth = key.map(super::live::fingerprint).unwrap_or_default();
    let previous: Vec<Value> = super::live::load_in(path, provider_id)
        .filter(|k| k["account"] == account.as_str())
        .and_then(|k| k["models"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m["auth"] == auth.as_str())
        .collect();
    let mut raw = discover(client, kind, &root, key, &previous)
        .await
        .with_context(|| format!("discovering {provider_id} models at {root}"))?;
    for m in &mut raw {
        m["auth"] = json!(auth);
    }
    let ids = raw
        .iter()
        .filter(|m| m["type"] != "decision")
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect();
    super::live::save_in(path, provider_id, &account, base_url, raw)?;
    Ok(ids)
}

/// The local providers: the built-in ones and those configured with
/// `settings.discovery`.
pub fn providers(config: &crate::config::Config) -> Vec<String> {
    let mut ids: Vec<String> = BUILTIN.iter().map(|b| b.0.to_string()).collect();
    for (id, p) in &config.providers {
        if p.settings.discovery.is_some() && !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    ids
}

/// Discover the models of every local provider that may be asked without
/// being asked (see [`discover_providers`]).
pub async fn discover_local(config: &crate::config::Config, path: &Path) -> Vec<String> {
    discover_providers(config, path, &providers(config)).await
}

/// The key an unprompted discovery of provider `id` sends: its key only
/// when the user configured its base URL, never to a built-in default port,
/// where whoever listens (another local user, say) is not vouched for.
pub fn unprompted_key(
    config: &crate::config::Config,
    id: &str,
    p: &crate::config::ProviderConfig,
) -> Option<String> {
    let configured = config
        .providers
        .get(id)
        .is_some_and(|c| c.settings.base_url.is_some());
    config.provider_key(id, p).filter(|_| configured)
}

/// Discover the models of the providers `ids` whose endpoint (the
/// configured base URL, or the built-in default) is on this machine, or
/// that `local: true` vouches for, at once and briefly: a server that is
/// not running is skipped and its kept list stays. The providers that
/// answered.
pub async fn discover_providers(
    config: &crate::config::Config,
    path: &Path,
    ids: &[String],
) -> Vec<String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default();
    let jobs = ids.iter().filter_map(|id| {
        let p = config.provider(id)?;
        let kind = kind(id, &p.settings)?;
        let base = p
            .settings
            .base_url
            .clone()
            .filter(|u| p.probe_unprompted(u))?;
        let key = unprompted_key(config, id, &p);
        let client = client.clone();
        Some(async move {
            refresh(&client, path, id, kind, &base, key.as_deref())
                .await
                .ok()
                .map(|_| id.clone())
        })
    });
    futures_util::future::join_all(jobs)
        .await
        .into_iter()
        .flatten()
        .collect()
}
