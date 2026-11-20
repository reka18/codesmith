//! Poe (after OpenCode's `PoePlugin`): Chat Completions at
//! `https://api.poe.com/v1` with a key (`POE_API_KEY` or a saved one), or a
//! Poe-issued API key from a browser login (OAuth with PKCE, scope
//! `apikey:create`). That key is used until it expires, then needs another
//! login; there is no refresh.
//!
//! The login runs as an OAuth client registered with Poe, and CodeSmith
//! ships none of its own (nor borrows another project's): give its client
//! id with `--client-id`, `settings.clientId` or `CODESMITH_POE_CLIENT_ID`.

use super::oauth::{self, Callback, Params, Pkce, Received};
use super::{set, Input, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{Auth, Protocol};
use serde_json::{json, Value};
use std::time::Duration;

pub const ID: &str = "poe";
pub const CLIENT_ID_VAR: &str = "CODESMITH_POE_CLIENT_ID";
pub const METHOD: &str = "browser";
pub const LABEL: &str = "Login with Poe (browser)";
pub const BASE_URL: &str = "https://api.poe.com/v1";
/// JavaScript's largest date, which OpenCode saves for keys that never
/// expire.
pub const NEVER: u64 = 8_640_000_000_000_000;

/// Poe's OAuth endpoints (overridable in tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub client_id: String,
    pub authorize: String,
    pub token: String,
    pub issuer: String,
}

impl Endpoints {
    pub fn poe(client_id: &str) -> Endpoints {
        Endpoints {
            client_id: client_id.to_string(),
            authorize: "https://poe.com/oauth/authorize".into(),
            token: "https://api.poe.com/token".into(),
            issuer: "https://poe.com".into(),
        }
    }
}

/// The OAuth client to log in as: the flag, else `settings.clientId`, else
/// `CODESMITH_POE_CLIENT_ID`.
pub fn client_id(
    flag: Option<&str>,
    settings: &super::Settings,
    env: &super::Env,
) -> Result<String> {
    flag.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| set(&settings.client_id).map(str::to_string))
        .or_else(|| env(CLIENT_ID_VAR))
        .ok_or_else(|| {
            anyhow!(
                "Poe's browser login needs the client id of an OAuth client registered with Poe, and CodeSmith ships none.\n\
                 Register one with Poe (a public client using PKCE, scope apikey:create, redirect URI http://127.0.0.1/callback on any port) \
                 and pass its client id with --client-id, settings.clientId on the poe provider, or {CLIENT_ID_VAR}.\n\
                 Or skip the login: create an API key at https://poe.com/api_key and run `codesmith auth login poe`, or set POE_API_KEY."
            )
        })
}

/// The callback checks: state, Poe's issuer when the callback names one,
/// then an error or the code.
pub fn check(params: &Params, state: &str, issuer: &str) -> Result<String, String> {
    if params.get("state").map(String::as_str) != Some(state) {
        return Err("Invalid OAuth state".into());
    }
    // Poe's documented callbacks may omit `iss`; one that names another
    // issuer is refused.
    if params.get("iss").is_some_and(|i| i != issuer) {
        return Err("Invalid OAuth issuer".into());
    }
    oauth::check(params, state, "Poe")
}

/// A started login: open `url`, then [`finish`].
pub struct Login {
    pub url: String,
    pub callback: Callback,
    pkce: Pkce,
    endpoints: Endpoints,
}

pub async fn start(e: &Endpoints) -> Result<Login> {
    let callback = Callback::bind("Poe", 0, "/callback").await?;
    let pkce = Pkce::new();
    let mut url = url::Url::parse(&e.authorize)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &e.client_id)
        .append_pair("redirect_uri", &callback.redirect)
        .append_pair("scope", "apikey:create")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &pkce.state);
    Ok(Login {
        url: url.to_string(),
        callback,
        pkce,
        endpoints: e.clone(),
    })
}

/// Exchange the code for Poe's API key: the login to save, or why not.
pub async fn exchange(
    client: &reqwest::Client,
    e: &Endpoints,
    code: &str,
    redirect: &str,
    verifier: &str,
) -> Result<Value, String> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", e.client_id.as_str()),
        ("code", code),
        ("redirect_uri", redirect),
        ("code_verifier", verifier),
    ];
    let v = match oauth::post_form(client, &e.token, &form, &[]).await {
        Err(_) => return Err("Poe token exchange request failed".into()),
        Ok(Err((status, body))) => {
            return Err(match oauth::error_detail(&body, &[code, verifier]) {
                Some(d) => format!("Poe token exchange failed: {d}"),
                None => format!("Poe token exchange failed ({status})"),
            })
        }
        Ok(Ok(v)) => v,
    };
    token_login(&v, oauth::now_ms())
}

/// The login a token answer gives: a non-blank key without spaces, and an
/// expiry that is a whole number of seconds (or none).
pub fn token_login(v: &Value, now: u64) -> Result<Value, String> {
    let invalid = || "Invalid Poe token response".to_string();
    let key = v["api_key"].as_str().map(str::trim).unwrap_or("");
    if key.is_empty() || key.contains(char::is_whitespace) {
        return Err(invalid());
    }
    let expires = match &v["api_key_expires_in"] {
        Value::Null => NEVER,
        Value::Number(n) => {
            let secs = n.as_u64().ok_or_else(invalid)?;
            now.checked_add(secs.checked_mul(1000).ok_or_else(invalid)?)
                .filter(|e| *e <= NEVER)
                .ok_or_else(|| "Invalid Poe API key expiry".to_string())?
        }
        _ => return Err(invalid()),
    };
    Ok(json!({"type": "oauth", "method": METHOD, "access": key, "refresh": "", "expires": expires}))
}

/// Wait for the redirect (or a pasted one), exchange the code, and tell the
/// browser the result.
pub async fn finish(
    client: &reqwest::Client,
    mut login: Login,
    pasted: Option<tokio::sync::mpsc::Receiver<String>>,
    wrong_paste: impl Fn(&str),
    timeout: Duration,
) -> Result<Value> {
    let state = login.pkce.state.clone();
    let issuer = login.endpoints.issuer.clone();
    let got: Received = login
        .callback
        .wait(|p| check(p, &state, &issuer), pasted, wrong_paste, timeout)
        .await?;
    let result = exchange(
        client,
        &login.endpoints,
        &got.code,
        &login.callback.redirect,
        &login.pkce.verifier,
    )
    .await;
    got.finish(result.as_ref().map(|_| ()).map_err(String::as_str))
        .await;
    result.map_err(|e| anyhow!(e))
}

/// The whole browser login, saved to `file`.
pub async fn login(
    client: &reqwest::Client,
    file: &std::path::Path,
    e: &Endpoints,
    open: impl FnOnce(&str),
    pasted: Option<tokio::sync::mpsc::Receiver<String>>,
) -> Result<()> {
    let l = start(e).await?;
    open(&l.url);
    let entry = finish(
        client,
        l,
        pasted,
        |m| eprintln!("{m}; still waiting"),
        Duration::from_secs(600),
    )
    .await?;
    crate::auth::set_entry_in(file, ID, entry)
}

/// The key of a saved browser login, while it lasts.
pub fn stored_key(stored: Option<&Value>, now: u64) -> Result<Option<String>> {
    let Some(s) = stored.filter(|s| s["type"] == "oauth") else {
        return Ok(None);
    };
    if s["expires"].as_u64().unwrap_or(0) <= now {
        return Err(anyhow!(
            "Poe API key expired. Log in with Poe again (`codesmith auth login poe --oauth`)."
        ));
    }
    Ok(s["access"].as_str().map(str::to_string))
}

pub fn route(input: &Input) -> Result<Route> {
    let api_key = match &input.api_key {
        Some(k) => Some(k.clone()),
        None => stored_key(input.stored.as_ref(), oauth::now_ms())?,
    };
    Ok(Route {
        protocol: input.configured_protocol.unwrap_or(Protocol::OpenaiChat),
        base_url: input
            .configured_url
            .or(input.catalog_url)
            .unwrap_or(BASE_URL)
            .to_string(),
        auth: Auth::Default,
        api_key,
        headers: vec![],
        model_id: None,
    })
}
