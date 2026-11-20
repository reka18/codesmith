//! GitLab sign-in (after OpenCode's `GitLabPlugin` OAuth): OAuth with PKCE
//! against gitlab.com or a self-managed instance (kept with its relative URL
//! root), saved with the instance and the application it was issued by, and
//! refreshed with that same application (GitLab binds refresh tokens to it
//! and rotates them on every refresh, so concurrent refreshes share one
//! exchange).
//!
//! The login runs as an OAuth application you register on the instance
//! (redirect URI `http://127.0.0.1:8080/callback`, scope `api`, not
//! "Confidential"); CodeSmith ships none of its own and never borrows
//! another project's. Give its application id with `--client-id`,
//! `settings.clientId` or `GITLAB_OAUTH_CLIENT_ID`.
//!
//! GitLab Duo's chat and workflow models run through GitLab's AI gateway,
//! which CodeSmith does not speak yet; the login is kept for when it does.

use super::oauth::{self, Callback, Pkce};
use super::set;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ID: &str = "gitlab";
pub const METHOD: &str = "pkce";
pub const LABEL: &str = "Login with GitLab (OAuth)";
pub const CLIENT_ID_VAR: &str = "GITLAB_OAUTH_CLIENT_ID";
pub const PORT: u16 = 8080;
pub const SCOPE: &str = "api";
pub const DEFAULT_INSTANCE: &str = "https://gitlab.com";

/// The redirect GitLab matches exactly, so the port is fixed.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// The instance a login defaults to: `GITLAB_INSTANCE_URL`, else gitlab.com.
pub fn default_instance(env: &super::Env) -> String {
    env("GITLAB_INSTANCE_URL").unwrap_or_else(|| DEFAULT_INSTANCE.into())
}

/// An instance URL without a trailing slash, keeping a relative URL root
/// (`https://example.com/gitlab`).
pub fn normalize_instance(answer: Option<&str>, env: &super::Env) -> Result<String> {
    let raw = answer
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_instance(env));
    let u = url::Url::parse(&raw).map_err(|_| anyhow!("{raw:?} is not a GitLab instance URL"))?;
    if !matches!(u.scheme(), "https" | "http") {
        return Err(anyhow!("{raw:?} is not a GitLab instance URL"));
    }
    let origin = u.origin().ascii_serialization();
    Ok(format!("{origin}{}", u.path().trim_end_matches('/')))
}

/// The application to log in with: the flag, `settings.clientId`, then
/// `GITLAB_OAUTH_CLIENT_ID`.
pub fn client_id(
    flag: Option<&str>,
    settings: &super::Settings,
    env: &super::Env,
    instance: &str,
) -> Result<String> {
    flag.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| set(&settings.client_id).map(str::to_string))
        .or_else(|| env(CLIENT_ID_VAR).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
        .ok_or_else(|| {
            let host = url::Url::parse(instance)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| instance.to_string());
            anyhow!(
                "The GitLab login needs an OAuth application on {host}, and CodeSmith ships none.\n\
                 Register one at {instance}/-/user_settings/applications with redirect URI {} and the `{SCOPE}` scope \
                 (not marked \"Confidential\"), then pass its application id with --client-id, settings.clientId on the gitlab provider, or {CLIENT_ID_VAR}.",
                redirect_uri(PORT)
            )
        })
}

fn describe_client(client_id: &str) -> String {
    let short: String = client_id.chars().take(12).collect();
    format!("client_id used: {short}...")
}

/// GitLab answers a reused code, a PKCE mismatch, a redirect mismatch and
/// a confidential application alike (`invalid_grant`), so the error lists
/// the likely causes without the code or verifier.
fn describe_grant_failure(detail: &str, client_id: &str, redirect: &str) -> String {
    let hints = [
        "the authorization code was already used, or came from an older login attempt. Codes are single-use and bound to one PKCE verifier, so a stale browser tab or a reloaded callback page fails here. Start a completely fresh login.".to_string(),
        format!("redirect_uri sent: {redirect} — the application must register this exactly"),
        describe_client(client_id),
        "confirm that application registers the redirect URI above, grants the `api` scope, and is NOT marked \"Confidential\" (PKCE requires a public client).".to_string(),
    ];
    format!("{detail}\n\nLikely causes:\n- {}", hints.join("\n- "))
}

fn describe_refresh_failure(detail: &str, client_id: &str) -> String {
    format!(
        "{detail}\n\nThe GitLab refresh token was revoked, expired, or issued to a different OAuth application ({}). Sign in to GitLab again.",
        describe_client(client_id)
    )
}

#[derive(Clone, Copy)]
enum Grant {
    Code,
    Refresh,
}

/// A token request: the login to save, or why not.
async fn exchange(
    client: &reqwest::Client,
    instance: &str,
    client_id: &str,
    form: &[(&str, &str)],
    redirect: &str,
    grant: Grant,
) -> Result<Value, String> {
    let mut form = form.to_vec();
    form.push(("client_id", client_id));
    let url = format!("{instance}/oauth/token");
    let v = match oauth::post_form(client, &url, &form, &[]).await {
        Err(_) => return Err("GitLab token exchange request failed".into()),
        Ok(Err((status, body))) => {
            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let error = parsed["error"].as_str().unwrap_or("");
            let detail = parsed["error_description"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or(Some(error).filter(|s| !s.is_empty()))
                .map(str::to_string)
                .unwrap_or_else(|| format!("HTTP {status}"));
            let rejected = error == "invalid_grant" || error == "invalid_client";
            return Err(match (rejected, grant) {
                (true, Grant::Code) => describe_grant_failure(&detail, client_id, redirect),
                (true, Grant::Refresh) => describe_refresh_failure(&detail, client_id),
                (false, _) => detail,
            });
        }
        Ok(Ok(v)) => v,
    };
    let (Some(access), Some(refresh)) = (
        v["access_token"].as_str().filter(|s| !s.is_empty()),
        v["refresh_token"].as_str().filter(|s| !s.is_empty()),
    ) else {
        return Err("Invalid GitLab token response".into());
    };
    let life = v["expires_in"].as_u64().unwrap_or(7200);
    Ok(json!({
        "type": "oauth",
        "method": METHOD,
        "access": access,
        "refresh": refresh,
        "expires": oauth::now_ms() + life * 1000,
        "metadata": {"instanceUrl": instance, "clientID": client_id},
    }))
}

/// Where a login runs (the port is overridable in tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub instance: String,
    pub client_id: String,
    pub port: u16,
}

/// A started login: open `url`, then [`finish`].
pub struct Login {
    pub url: String,
    pub callback: Callback,
    pkce: Pkce,
    endpoints: Endpoints,
}

pub async fn start(e: &Endpoints) -> Result<Login> {
    let callback = Callback::bind("GitLab", e.port, "/callback")
        .await
        .map_err(|err| {
            if format!("{err:#}").contains("another program is using it") {
                anyhow!(
                    "GitLab login needs local port {}, but it is already in use. Stop the process using that port and try again.",
                    e.port
                )
            } else {
                err
            }
        })?;
    let pkce = Pkce::new();
    let mut url = url::Url::parse(&format!("{}/oauth/authorize", e.instance))?;
    url.query_pairs_mut()
        .append_pair("client_id", &e.client_id)
        .append_pair("redirect_uri", &callback.redirect)
        .append_pair("response_type", "code")
        .append_pair("state", &pkce.state)
        .append_pair("scope", SCOPE)
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
    mut login: Login,
    pasted: Option<tokio::sync::mpsc::Receiver<String>>,
    timeout: Duration,
) -> Result<Value> {
    let state = login.pkce.state.clone();
    let got = login
        .callback
        .wait(
            |p| oauth::check(p, &state, "GitLab"),
            pasted,
            |m| eprintln!("{m}; still waiting"),
            timeout,
        )
        .await?;
    let redirect = login.callback.redirect.clone();
    let result = exchange(
        client,
        &login.endpoints.instance,
        &login.endpoints.client_id,
        &[
            ("grant_type", "authorization_code"),
            ("code", &got.code),
            ("redirect_uri", &redirect),
            ("code_verifier", &login.pkce.verifier),
        ],
        &redirect,
        Grant::Code,
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
) -> Result<()> {
    let l = start(e).await?;
    open(&l.url);
    let entry = finish(
        client,
        l,
        Some(oauth::stdin_lines()),
        Duration::from_secs(600),
    )
    .await?;
    crate::auth::set_entry_in(file, ID, entry)
}

type Flight = Arc<tokio::sync::OnceCell<Result<Value, String>>>;
/// Refreshes in flight (no deadline) or just finished (kept until the
/// deadline), by instance and refresh token.
type Flights = Mutex<HashMap<String, (Flight, Option<std::time::Instant>)>>;

fn flights() -> &'static Flights {
    static F: std::sync::OnceLock<Flights> = std::sync::OnceLock::new();
    F.get_or_init(Default::default)
}

/// Refresh a saved login with the application that issued it, sharing one
/// exchange between concurrent refreshes of the same token; a success is
/// reused for a minute (a caller still holding the old token gets the
/// rotated one), a failure is not.
pub async fn refresh(
    client: &reqwest::Client,
    login: &Value,
    env: &super::Env,
    port: u16,
) -> Result<Value> {
    let instance = login["metadata"]["instanceUrl"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_instance(env));
    let client_id = login["metadata"]["clientID"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| env(CLIENT_ID_VAR).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
        .ok_or_else(|| {
            anyhow!(
                "This GitLab login records no OAuth application, and CodeSmith refreshes only with the one that issued it: set {CLIENT_ID_VAR} to that application's id, or sign in to GitLab again"
            )
        })?;
    let token = login["refresh"].as_str().unwrap_or("").to_string();
    let key = format!("{instance}\0{token}");
    let flight = {
        let mut all = flights().lock().unwrap();
        let now = std::time::Instant::now();
        all.retain(|_, (_, until)| until.is_none_or(|u| u > now));
        all.entry(key.clone())
            .or_insert_with(|| (Arc::new(tokio::sync::OnceCell::new()), None))
            .0
            .clone()
    };
    let redirect = redirect_uri(port);
    let out = flight
        .get_or_init(|| async {
            exchange(
                client,
                &instance,
                &client_id,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &token),
                    ("redirect_uri", &redirect),
                ],
                &redirect,
                Grant::Refresh,
            )
            .await
        })
        .await
        .clone();
    let mut all = flights().lock().unwrap();
    match &out {
        Ok(_) => {
            if let Some(f) = all.get_mut(&key) {
                f.1 = Some(std::time::Instant::now() + Duration::from_secs(60));
            }
        }
        Err(_) => {
            all.remove(&key);
        }
    }
    out.map_err(|e| anyhow!(e))
}

/// The instance a saved login was issued against.
pub fn stored_instance(stored: Option<&Value>) -> Option<String> {
    let s = stored?;
    (s["type"] == "oauth")
        .then(|| s["metadata"]["instanceUrl"].as_str())
        .flatten()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
