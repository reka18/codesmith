//! Snowflake Cortex (after OpenCode's `SnowflakeCortexPlugin`): the
//! account's Cortex REST API
//! (`https://<account>.snowflakecomputing.com/api/v2/cortex/v1`), Chat
//! Completions or Messages per model, with a programmatic access token or
//! bearer token, or a browser login (Snowflake OAuth with PKCE) whose access
//! token is refreshed when it expires or is rejected, the rotated tokens
//! saved.
//!
//! The account comes from `SNOWFLAKE_ACCOUNT`, else the saved login, else
//! `settings.account`; the token from `SNOWFLAKE_CORTEX_TOKEN` or
//! `SNOWFLAKE_CORTEX_PAT`, else `settings.token` / `settings.apiKey`, else the
//! saved key or login. An environment token wins even over an expired login.
//!
//! The browser login uses Snowflake's built-in OAuth client for local
//! applications (`LOCAL_APPLICATION`, which every Snowflake account has, as
//! Snowflake's own drivers and CLI do), unless `settings.clientId` names a
//! security integration of your own.

use super::oauth::{self, Callback, Pkce};
use super::{set, stored_metadata, Field, Input, KeyForm, Route};
use anyhow::{anyhow, Result};
use base64::Engine;
use codesmith_ai::{Auth, Error as AiError, Protocol};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ID: &str = "snowflake-cortex";
pub const METHOD: &str = "browser";
pub const LABEL: &str = "Login with Snowflake (External Browser)";
/// Snowflake's built-in OAuth client for local applications.
pub const LOCAL_APPLICATION: &str = "LOCAL_APPLICATION";
pub const TOKEN_VARS: [&str; 2] = ["SNOWFLAKE_CORTEX_TOKEN", "SNOWFLAKE_CORTEX_PAT"];

fn account_field() -> Field {
    Field {
        key: "account",
        title: "Snowflake account",
        placeholder: "myorg-myaccount",
        required: true,
    }
}

/// The key login: a PAT or bearer token, and the account.
pub fn key_form() -> KeyForm {
    KeyForm {
        label: "Paste PAT or bearer token",
        fields: vec![account_field()],
    }
}

/// `myorg-myaccount` from an account identifier or URL.
pub fn normalize_account(value: &str) -> String {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let v = if lower.starts_with("https://") {
        &v[8..]
    } else if lower.starts_with("http://") {
        &v[7..]
    } else {
        v
    };
    let v = v.trim_end_matches('/');
    let suffix = ".snowflakecomputing.com";
    if v.to_ascii_lowercase().ends_with(suffix) {
        v[..v.len() - suffix.len()].to_string()
    } else {
        v.to_string()
    }
}

pub fn issuer(account: &str) -> String {
    format!("https://{account}.snowflakecomputing.com")
}

/// The account a model uses.
pub fn account(input: &Input) -> Option<String> {
    (input.env)("SNOWFLAKE_ACCOUNT")
        .or_else(|| {
            let s = input.stored.as_ref()?;
            s["metadata"]["account"].as_str().map(str::to_string)
        })
        .or_else(|| set(&input.settings.plugin.account).map(str::to_string))
        .map(|a| normalize_account(&a))
        .filter(|a| !a.is_empty())
}

/// An environment token.
pub fn env_token(env: &super::Env) -> Option<String> {
    TOKEN_VARS.iter().find_map(|v| env(v))
}

/// The scope of a login, with the optional role.
pub fn scope(role: Option<&str>) -> String {
    match role.map(str::trim).filter(|r| !r.is_empty()) {
        None => "refresh_token".into(),
        Some(r)
            if r.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            format!("refresh_token session:role:{r}")
        }
        Some(r) => format!("refresh_token session:role-encoded:{}", super::encode(r)),
    }
}

/// Snowflake's OAuth endpoints for an account (the issuer is overridable in
/// tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub account: String,
    pub issuer: String,
    pub client_id: String,
}

impl Endpoints {
    pub fn for_account(account: &str, client_id: Option<&str>) -> Endpoints {
        let account = normalize_account(account);
        Endpoints {
            issuer: issuer(&account),
            account,
            client_id: client_id.unwrap_or(LOCAL_APPLICATION).to_string(),
        }
    }
}

/// A token request: the login to save (with the account), or why not.
pub async fn token(
    client: &reqwest::Client,
    e: &Endpoints,
    form: &[(&str, &str)],
    previous_refresh: Option<&str>,
) -> Result<Value> {
    let basic = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", e.client_id, e.client_id));
    let mut form: Vec<(&str, &str)> = form.to_vec();
    form.push(("client_id", &e.client_id));
    let url = format!("{}/oauth/token-request", e.issuer);
    let headers = [
        ("authorization", format!("Basic {basic}")),
        ("user-agent", codesmith_ai::copilot::user_agent()),
    ];
    let v = match oauth::post_form(client, &url, &form, &headers).await? {
        Err((status, body)) => {
            let secrets: Vec<&str> = form.iter().map(|(_, v)| *v).collect();
            let detail = oauth::error_detail(&body, &secrets).unwrap_or_default();
            return Err(anyhow!(
                "Snowflake token request failed ({status}){}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ));
        }
        Ok(v) => v,
    };
    let access = v["access_token"]
        .as_str()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow!("Snowflake's token response has no access_token"))?;
    let refresh = v["refresh_token"]
        .as_str()
        .filter(|r| !r.is_empty())
        .or(previous_refresh)
        .ok_or_else(|| anyhow!("Snowflake token response did not include refresh_token"))?;
    let expires = oauth::now_ms() + v["expires_in"].as_u64().unwrap_or(600) * 1000;
    Ok(json!({
        "type": "oauth",
        "method": METHOD,
        "access": access,
        "refresh": refresh,
        "expires": expires,
        "metadata": {"account": e.account},
    }))
}

/// A started browser login: open `url`, then [`finish`].
pub struct Login {
    pub url: String,
    pub callback: Callback,
    pkce: Pkce,
    endpoints: Endpoints,
}

pub async fn start(e: &Endpoints, role: Option<&str>) -> Result<Login> {
    if e.account.is_empty() {
        return Err(anyhow!("Snowflake account is required"));
    }
    let callback = Callback::bind("Snowflake", 0, "/").await?;
    let pkce = Pkce::new();
    let mut url = url::Url::parse(&format!("{}/oauth/authorize", e.issuer))?;
    url.query_pairs_mut()
        .append_pair("client_id", &e.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &callback.redirect)
        .append_pair("state", &pkce.state)
        .append_pair("scope", &scope(role))
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(Login {
        url: url.to_string(),
        callback,
        pkce,
        endpoints: e.clone(),
    })
}

pub async fn finish(
    client: &reqwest::Client,
    login: Login,
    pasted: Option<tokio::sync::mpsc::Receiver<String>>,
    timeout: Duration,
) -> Result<Value> {
    finish_with(
        client,
        login,
        pasted,
        |m| eprintln!("{m}; still waiting"),
        timeout,
    )
    .await
}

/// [`finish`], telling `wrong_paste` (not the terminal) about a pasted URL
/// that is not this login's.
pub async fn finish_with(
    client: &reqwest::Client,
    mut login: Login,
    pasted: Option<tokio::sync::mpsc::Receiver<String>>,
    wrong_paste: impl Fn(&str),
    timeout: Duration,
) -> Result<Value> {
    let state = login.pkce.state.clone();
    let got = login
        .callback
        .wait(
            |p| oauth::check(p, &state, "Snowflake"),
            pasted,
            wrong_paste,
            timeout,
        )
        .await?;
    let result = token(
        client,
        &login.endpoints,
        &[
            ("grant_type", "authorization_code"),
            ("code", &got.code),
            ("redirect_uri", &login.callback.redirect),
            ("code_verifier", &login.pkce.verifier),
        ],
        None,
    )
    .await;
    let msg = result.as_ref().err().map(|e| e.to_string());
    got.finish(match &msg {
        None => Ok(()),
        Some(m) => Err(m.as_str()),
    })
    .await;
    result
}

/// The whole browser login, saved to `file`.
pub async fn login(
    client: &reqwest::Client,
    file: &std::path::Path,
    e: &Endpoints,
    role: Option<&str>,
    open: impl FnOnce(&str),
) -> Result<()> {
    login_with(
        client,
        file,
        e,
        role,
        open,
        Some(oauth::stdin_lines()),
        |m| eprintln!("{m}; still waiting"),
    )
    .await
}

/// [`login`] with redirects pasted through `pasted` and wrong pastes told
/// to `wrong_paste` (the sign-in picker's, not the terminal's).
pub async fn login_with(
    client: &reqwest::Client,
    file: &std::path::Path,
    e: &Endpoints,
    role: Option<&str>,
    open: impl FnOnce(&str),
    pasted: Option<tokio::sync::mpsc::Receiver<String>>,
    wrong_paste: impl Fn(&str),
) -> Result<()> {
    let l = start(e, role).await?;
    open(&l.url);
    let entry = finish_with(client, l, pasted, wrong_paste, Duration::from_secs(600)).await?;
    crate::auth::set_entry_in(file, ID, entry)
}

/// A saved browser login, refreshed when it expires or is rejected, and
/// saved again when it changes.
#[derive(Debug)]
pub struct Refreshing {
    login: Mutex<Value>,
    endpoints: Endpoints,
    file: PathBuf,
    refreshes: tokio::sync::Mutex<()>,
}

impl Refreshing {
    pub fn new(login: Value, endpoints: Endpoints, file: PathBuf) -> Refreshing {
        Refreshing {
            login: Mutex::new(login),
            endpoints,
            file,
            refreshes: tokio::sync::Mutex::new(()),
        }
    }

    fn current(&self) -> (String, String, u64) {
        let l = self.login.lock().unwrap();
        (
            l["access"].as_str().unwrap_or("").to_string(),
            l["refresh"].as_str().unwrap_or("").to_string(),
            l["expires"].as_u64().unwrap_or(0),
        )
    }

    /// Exchange the refresh token (once at a time) unless `stale` was
    /// already replaced.
    pub async fn refresh(&self, client: &reqwest::Client, stale: &str) -> Result<String> {
        let _one = self.refreshes.lock().await;
        let (access, refresh, _) = self.current();
        if access != stale {
            return Ok(access);
        }
        let fresh = token(
            client,
            &self.endpoints,
            &[("grant_type", "refresh_token"), ("refresh_token", &refresh)],
            Some(&refresh),
        )
        .await?;
        let access = fresh["access"].as_str().unwrap_or("").to_string();
        *self.login.lock().unwrap() = fresh.clone();
        crate::auth::set_entry_in(&self.file, ID, fresh)?;
        Ok(access)
    }

    pub fn snapshot(&self) -> Value {
        self.login.lock().unwrap().clone()
    }
}

impl codesmith_ai::Authorizer for Refreshing {
    fn authorize<'a>(
        &'a self,
        client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> BoxFuture<'a, Result<(), AiError>> {
        Box::pin(async move {
            let (mut access, _, expires) = self.current();
            if expires <= oauth::now_ms() {
                access = self
                    .refresh(client, &access)
                    .await
                    .map_err(|e| AiError::Invalid(e.to_string()))?;
            }
            let h = req.headers_mut();
            h.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {access}"))
                    .map_err(|e| AiError::Invalid(e.to_string()))?,
            );
            h.insert(
                reqwest::header::USER_AGENT,
                reqwest::header::HeaderValue::from_str(&codesmith_ai::copilot::user_agent())
                    .map_err(|e| AiError::Invalid(e.to_string()))?,
            );
            Ok(())
        })
    }

    fn describe(&self) -> String {
        format!("Snowflake login for {}", self.endpoints.account)
    }

    fn rejected<'a>(&'a self, client: &'a reqwest::Client) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let (access, _, _) = self.current();
            self.refresh(client, &access).await.is_ok()
        })
    }
}

/// One refresher per saved login, so concurrent requests share a refresh.
fn refresher(login: &Value, account: &str) -> Arc<Refreshing> {
    static SHARED: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Arc<Refreshing>>>> =
        std::sync::OnceLock::new();
    let key = format!("{account}\n{}", login["refresh"].as_str().unwrap_or(""));
    SHARED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| {
            Arc::new(Refreshing::new(
                login.clone(),
                Endpoints::for_account(account, None),
                crate::paths::auth_file(),
            ))
        })
        .clone()
}

/// The protocol of a model: configured, else its catalog package's, else
/// Chat Completions.
fn protocol(input: &Input) -> Protocol {
    input
        .configured_protocol
        .or_else(|| input.package.and_then(crate::catalog::protocol))
        .unwrap_or(Protocol::OpenaiChat)
}

pub fn route(input: &Input) -> Result<Route> {
    let account = account(input);
    let base_url = match input.configured_url {
        Some(u) => match &account {
            Some(a) => u.replace("${SNOWFLAKE_ACCOUNT}", a),
            None => u.to_string(),
        },
        None => {
            let a = account.clone().ok_or_else(|| {
                anyhow!("Snowflake Cortex needs the account: run `codesmith auth login snowflake-cortex`, or set SNOWFLAKE_ACCOUNT or settings.account")
            })?;
            input
                .catalog_url
                .map(|u| u.replace("${SNOWFLAKE_ACCOUNT}", &a))
                .unwrap_or_else(|| format!("{}/api/v2/cortex/v1", issuer(&a)))
        }
    };
    let protocol = protocol(input);
    let route = |auth, api_key| Route {
        protocol,
        base_url: base_url.clone(),
        auth,
        api_key,
        headers: vec![(
            "user-agent".to_string(),
            codesmith_ai::copilot::user_agent(),
        )],
        model_id: None,
    };
    // An environment token, then a configured one, then the saved key.
    if let Some(t) = env_token(&input.env)
        .or_else(|| set(&input.settings.plugin.token).map(str::to_string))
        .or_else(|| input.api_key.clone())
    {
        return Ok(route(Auth::Bearer, Some(t)));
    }
    match input.stored.as_ref() {
        Some(s) if s["type"] == "oauth" => {
            let account = stored_metadata(Some(&json!({"type": "api", "metadata": s["metadata"]})), "account")
                .map(|a| normalize_account(&a))
                .or(account)
                .unwrap_or_default();
            Ok(route(Auth::Dynamic(refresher(s, &account)), None))
        }
        _ => Err(anyhow!(
            "Snowflake Cortex needs a token: run `codesmith auth login snowflake-cortex` (a PAT) or `--oauth` (browser), or set SNOWFLAKE_CORTEX_TOKEN or SNOWFLAKE_CORTEX_PAT"
        )),
    }
}
