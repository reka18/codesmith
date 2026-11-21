//! GitHub Copilot's Auto model (after OpenCode's `GithubCopilotPlugin`
//! router): a session on `github-copilot/auto` asks Copilot's router
//! (`POST <api>/auto` with the latest prompt) which model of the account's
//! Auto pool serves it, and keeps that pick until its session token nears
//! expiry, the session is compacted, or the account changes. Requests for
//! the picked model carry the pick's `Copilot-Session-Token`, which unlocks
//! its Auto discount (Copilot rejects it for any other model). When the
//! router fails or picks a model this account's list lacks, the account's
//! chat fallback serves (else the first model of the Auto pool), and the
//! router is asked again five minutes later. Titles never route.
//!
//! The router is a Copilot endpoint on the account's own API: the request
//! goes through the provider's Copilot credentials, to the same origin.

use super::copilot_models::{parse, Remote, AUTO};
use codesmith_ai::Auth;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// The header a pick's session token goes in.
pub const TOKEN_HEADER: &str = "Copilot-Session-Token";
/// Picks are made again this long before their token expires.
const MARGIN_MS: i64 = 5 * 60_000;
/// How long a fallback serves before the router is asked again.
const RETRY_MS: i64 = 5 * 60_000;
const TIMEOUT: Duration = Duration::from_secs(5);

/// The model Copilot's router picked for a session.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub model: String,
    /// The account the pick was made for (the kept list's fingerprint).
    pub account: String,
    pub token: Option<String>,
    /// Until when it serves, in milliseconds since the epoch.
    pub until: i64,
}

fn routes() -> &'static Mutex<HashMap<String, Pick>> {
    static ROUTES: std::sync::OnceLock<Mutex<HashMap<String, Pick>>> = std::sync::OnceLock::new();
    ROUTES.get_or_init(Default::default)
}

/// Whether `model_id` is Copilot's Auto model.
pub fn is_auto(provider_id: &str, model_id: &str) -> bool {
    provider_id == super::copilot::ID && model_id == AUTO
}

/// The account's kept model list and its fingerprint (see [`super::live`]).
pub struct Account {
    pub id: String,
    pub models: Vec<Remote>,
}

impl Account {
    pub fn from_kept(kept: &Value) -> Option<Account> {
        let id = kept["account"].as_str()?.to_string();
        let raw = kept["models"].as_array()?;
        Some(Account {
            id,
            models: parse(raw),
        })
    }

    fn fallback(&self) -> Option<&Remote> {
        self.models
            .iter()
            .find(|m| m.chat_fallback)
            .or_else(|| self.models.iter().find(|m| m.auto_discount.is_some()))
    }
}

/// What the router is asked with: the session's latest prompt.
#[derive(Debug, Clone, Default)]
pub struct Prompt {
    pub text: String,
    pub has_image: bool,
}

/// Ask Copilot's router through `provider` (the Auto model's, whose base URL
/// is the account's API root); a fallback when it fails.
pub async fn route(
    client: &reqwest::Client,
    provider: &codesmith_ai::Provider,
    account: &Account,
    prompt: &Prompt,
    now: i64,
) -> Option<Pick> {
    let picked = ask(client, provider, prompt).await;
    if let Some((model, token, expires)) =
        picked.filter(|(m, ..)| account.models.iter().any(|r| r.id == *m))
    {
        return Some(Pick {
            model,
            account: account.id.clone(),
            token: Some(token),
            until: expires * 1000 - MARGIN_MS,
        });
    }
    account.fallback().map(|m| Pick {
        model: m.id.clone(),
        account: account.id.clone(),
        token: None,
        until: now + RETRY_MS,
    })
}

/// `(selected model, session token, expiry in seconds)`, or `None` when the
/// router fails or answers something else.
async fn ask(
    client: &reqwest::Client,
    provider: &codesmith_ai::Provider,
    prompt: &Prompt,
) -> Option<(String, String, i64)> {
    let root = provider.base_url.trim_end_matches('/');
    let mut body = json!({"prompt": prompt.text});
    if prompt.has_image {
        body["has_image"] = true.into();
    }
    let mut req = client
        .post(format!("{root}/auto"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("user-agent", codesmith_ai::copilot::user_agent())
        .header("x-github-api-version", codesmith_ai::copilot::API_VERSION)
        .timeout(TIMEOUT)
        .body(body.to_string())
        .build()
        .ok()?;
    for (k, v) in &provider.headers {
        if let (Ok(k), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            req.headers_mut().insert(k, v);
        }
    }
    match &provider.auth {
        Auth::Dynamic(a) => a.authorize(client, &mut req).await.ok()?,
        _ => {
            let key = provider.api_key.as_deref()?;
            req.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).ok()?,
            );
        }
    }
    let resp = client.execute(req).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    Some((
        v["selected_model"]["id"].as_str()?.to_string(),
        v["session_token"].as_str()?.to_string(),
        v["expires_at"].as_i64()?,
    ))
}

/// The pick serving session `session` for a request: the kept one while it
/// is current and for `account`, else a new one from [`route`]. `None` for
/// a title (titles never route) or when nothing can serve.
pub async fn resolve(
    client: &reqwest::Client,
    provider: &codesmith_ai::Provider,
    session: &str,
    title: bool,
    account: &Account,
    prompt: &Prompt,
    now: i64,
) -> Option<Pick> {
    if title {
        return None;
    }
    let kept = routes().lock().unwrap().get(session).cloned();
    if let Some(p) = kept.filter(|p| p.until > now && p.account == account.id) {
        return Some(p);
    }
    let pick = route(client, provider, account, prompt, now).await?;
    let mut all = routes().lock().unwrap();
    all.retain(|_, p| p.until > now);
    all.insert(session.to_string(), pick.clone());
    Some(pick)
}

/// The session token for a request of `session` to `model`: only the pick's
/// own model gets it.
pub fn token(session: &str, model: &str) -> Option<String> {
    let all = routes().lock().unwrap();
    all.get(session)
        .filter(|p| p.model == model)
        .and_then(|p| p.token.clone())
}

/// A compaction rewrote the session's prompt: its next request routes
/// again (the compaction request itself kept the pick).
pub fn compacted(session: &str) {
    if let Some(p) = routes().lock().unwrap().get_mut(session) {
        p.until = 0;
    }
}
