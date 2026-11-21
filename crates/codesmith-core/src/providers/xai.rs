//! xAI sign-in (after OpenCode's `XAIPlugin`): a SuperGrok subscription
//! login through xAI's OAuth device flow, beside API keys. The login's
//! access token is sent as a bearer token to the xAI API and refreshed (once
//! for every request that needs it, see [`super::credentials`]) at xAI's
//! token endpoint, its only other destination.
//!
//! The device flow runs as an OAuth client you register with xAI;
//! CodeSmith ships none and never borrows another product's. Give its id
//! with `--client-id`, `settings.clientId` or `XAI_OAUTH_CLIENT_ID`; it is
//! saved with the login, which always refreshes with it. A login saved by
//! the earlier browser method is moved to the device method.

use super::{set, Input, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{Auth, Protocol};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const ID: &str = "xai";
pub const METHOD: &str = "device";
const BROWSER_METHOD: &str = "browser";
pub const LABEL: &str = "SuperGrok Subscription";
pub const CLIENT_ID_VAR: &str = "XAI_OAUTH_CLIENT_ID";
pub const ISSUER: &str = "https://auth.x.ai/oauth2";
pub const API: &str = "https://api.x.ai/v1";
/// The scopes asked for unless `settings.oauthScope` says otherwise.
pub const SCOPE: &str = "openid profile email offline_access api:access";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const POLL_MARGIN: Duration = Duration::from_secs(3);

/// The login methods, as `codesmith auth login xai` offers them.
pub fn methods() -> Vec<(&'static str, &'static str)> {
    vec![(METHOD, LABEL), ("key", "Manually enter API Key")]
}

/// The OAuth client to log in as: the flag, `settings.clientId`, then
/// `XAI_OAUTH_CLIENT_ID`.
pub fn client_id(
    flag: Option<&str>,
    settings: &super::Settings,
    env: &super::Env,
) -> Result<String> {
    flag.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| set(&settings.client_id).map(str::to_string))
        .or_else(|| env(CLIENT_ID_VAR).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
        .ok_or_else(|| {
            anyhow!(
                "xAI's SuperGrok login needs the id of an OAuth client registered with xAI, and CodeSmith ships none.\n\
                 Pass it with --client-id, settings.clientId on the xai provider, or {CLIENT_ID_VAR}.\n\
                 Or skip the login: use an xAI API key (`codesmith auth login xai`, or XAI_API_KEY)."
            )
        })
}

/// xAI's OAuth endpoints (overridable in tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub client_id: String,
    pub issuer: String,
    pub scope: String,
}

impl Endpoints {
    pub fn xai(client_id: &str, scope: Option<&str>) -> Endpoints {
        Endpoints {
            client_id: client_id.into(),
            issuer: ISSUER.into(),
            scope: scope.unwrap_or(SCOPE).into(),
        }
    }
}

/// The code to show the user.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub verification_uri: String,
    /// The URI with the code filled in, when xAI gives one.
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    device_code: String,
    interval: Duration,
    expires: Duration,
}

async fn post(client: &reqwest::Client, url: &str, form: &[(&str, &str)]) -> Result<(u16, Value)> {
    let resp = client
        .post(url)
        .header("accept", "application/json")
        .header("user-agent", codesmith_ai::copilot::user_agent())
        .form(form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| anyhow!("{}", codesmith_ai::Error::from(e)))?;
    let status = resp.status().as_u16();
    let v = resp.json().await.unwrap_or(Value::Null);
    Ok((status, v))
}

fn positive(v: &Value, fallback: u64) -> u64 {
    v.as_f64()
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as u64)
        .unwrap_or(fallback)
}

/// Start a device authorization.
pub async fn start(client: &reqwest::Client, e: &Endpoints) -> Result<DeviceCode> {
    let (status, v) = post(
        client,
        &format!("{}/device/code", e.issuer),
        &[("client_id", &e.client_id), ("scope", &e.scope)],
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("xAI request failed ({status})"));
    }
    let s = |k: &str| {
        v[k].as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("xAI's device authorization answer has no {k}"))
    };
    Ok(DeviceCode {
        verification_uri: s("verification_uri")?,
        verification_uri_complete: v["verification_uri_complete"].as_str().map(str::to_string),
        user_code: s("user_code")?,
        device_code: s("device_code")?,
        interval: Duration::from_secs(positive(&v["interval"], 5).max(1)),
        expires: Duration::from_secs(positive(&v["expires_in"], 300)),
    })
}

/// When a token answer's access token expires (ms): `expires_in`, else the
/// JWT's `exp`, else an hour.
fn expiry(tokens: &Value, now: u64) -> u64 {
    if let Some(s) = tokens["expires_in"].as_f64().filter(|n| *n > 0.0) {
        return now + (s * 1000.0) as u64;
    }
    use base64::Engine;
    let exp = tokens["access_token"]
        .as_str()
        .and_then(|t| t.split('.').nth(1))
        .and_then(|p| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(p)
                .ok()
        })
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|c| c["exp"].as_u64());
    exp.map(|e| e * 1000).unwrap_or(now + 3_600_000)
}

/// The login to save from a token answer.
fn login_from(
    tokens: &Value,
    client_id: &str,
    current_refresh: Option<&str>,
    metadata: Option<&Value>,
) -> Result<Value> {
    let access = tokens["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow!("xAI's token answer has no access_token"))?;
    let refresh = tokens["refresh_token"]
        .as_str()
        .or(current_refresh)
        .ok_or_else(|| anyhow!("xAI token response is missing refresh_token"))?;
    let mut meta = metadata.cloned().unwrap_or_else(|| json!({}));
    meta["clientID"] = client_id.into();
    Ok(json!({
        "type": "oauth",
        "method": METHOD,
        "access": access,
        "refresh": refresh,
        "expires": expiry(tokens, super::oauth::now_ms()),
        "metadata": meta,
    }))
}

/// Poll until the user approves (or denies) the code; the login to save.
pub async fn finish(
    client: &reqwest::Client,
    e: &Endpoints,
    code: &DeviceCode,
    margin: Duration,
) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + code.expires;
    let mut wait = code.interval;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("xAI device authorization timed out"));
        }
        tokio::time::sleep(wait + margin).await;
        let (status, v) = post(
            client,
            &format!("{}/token", e.issuer),
            &[
                ("grant_type", DEVICE_GRANT),
                ("client_id", &e.client_id),
                ("device_code", &code.device_code),
            ],
        )
        .await?;
        if (200..300).contains(&status) {
            return login_from(&v, &e.client_id, None, None);
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => wait += Duration::from_secs(5),
            Some("access_denied" | "authorization_denied") => {
                return Err(anyhow!("xAI device authorization was denied"))
            }
            Some("expired_token") => {
                return Err(anyhow!("xAI device code expired - please re-run login"))
            }
            other => {
                let detail = v["error_description"].as_str().or(other);
                return Err(anyhow!(
                    "xAI device token exchange failed ({status}){}",
                    detail.map(|d| format!(": {d}")).unwrap_or_default()
                ));
            }
        }
    }
}

/// The whole device login, showing the code through `show`, saved to `file`
/// as a new credential (the one it replaces is kept).
pub async fn login(
    client: &reqwest::Client,
    file: &Path,
    e: &Endpoints,
    show: impl FnOnce(&DeviceCode),
) -> Result<()> {
    let code = start(client, e).await?;
    show(&code);
    let login = finish(client, e, &code, POLL_MARGIN).await?;
    super::credentials::create_in(
        file,
        super::credentials::New {
            provider: ID.into(),
            value: login,
            label: Some(LABEL.into()),
            ..Default::default()
        },
    )?;
    Ok(())
}

/// Exchange a login's refresh token at the issuer, with the client it was
/// issued to.
pub async fn refresh(
    client: &reqwest::Client,
    issuer: &str,
    login: &Value,
    env: &super::Env,
) -> Result<Value> {
    let client_id = login["metadata"]["clientID"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| env(CLIENT_ID_VAR).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
        .ok_or_else(|| {
            anyhow!("This xAI login records no OAuth client, and CodeSmith refreshes only with the one that issued it: set {CLIENT_ID_VAR} to that client's id, or sign in again")
        })?;
    let refresh = login["refresh"].as_str().unwrap_or("");
    let (status, v) = post(
        client,
        &format!("{issuer}/token"),
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", &client_id),
        ],
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("xAI request failed ({status})"));
    }
    login_from(&v, &client_id, Some(refresh), Some(&login["metadata"]))
}

/// Move logins saved by the earlier browser method to the device method.
/// Returns how many moved.
pub fn migrate(file: &Path) -> Result<usize> {
    let mut moved = 0;
    for c in super::credentials::list_in(file, ID) {
        if c.value["type"] == "oauth" && c.value["method"] == BROWSER_METHOD {
            let mut v = c.value.clone();
            v["method"] = METHOD.into();
            super::credentials::update_in(
                file,
                &c.id,
                super::credentials::Update {
                    value: Some(v),
                    ..Default::default()
                },
            )?;
            moved += 1;
        }
    }
    Ok(moved)
}

/// Sends the saved login's access token, refreshing it first when it is due.
#[derive(Debug)]
pub struct Refreshing {
    pub file: PathBuf,
    pub issuer: String,
}

impl codesmith_ai::Authorizer for Refreshing {
    fn authorize<'a>(
        &'a self,
        client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> futures_util::future::BoxFuture<'a, Result<(), codesmith_ai::Error>> {
        Box::pin(async move {
            let fail = codesmith_ai::Error::Invalid;
            let now = super::oauth::now_ms();
            let env = super::process_env();
            let login = super::credentials::refresh_in(
                &self.file,
                ID,
                |v| super::credentials::expiring(v, now),
                |v| async move { refresh(client, &self.issuer, &v, &env).await },
            )
            .await
            .map_err(|e| fail(format!("{e:#}")))?
            .ok_or_else(|| {
                fail("xAI needs a login: run `codesmith auth login xai --oauth`".into())
            })?;
            let access = login["access"].as_str().unwrap_or("");
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {access}"))
                .map_err(|_| fail("the saved xAI login is not a valid token".into()))?;
            req.headers_mut()
                .insert(reqwest::header::AUTHORIZATION, value);
            Ok(())
        })
    }

    fn describe(&self) -> String {
        "xAI SuperGrok login".into()
    }
}

/// Whether a saved login (not an API key) drives requests.
pub fn owns(input: &Input) -> bool {
    input.provider_id == ID
        && input.api_key.is_none()
        && input.stored.as_ref().is_some_and(|s| s["type"] == "oauth")
}

pub fn route(input: &Input) -> Result<Route> {
    if input
        .stored
        .as_ref()
        .is_some_and(|s| s["method"] == BROWSER_METHOD)
    {
        migrate(&crate::paths::auth_file())?;
    }
    let protocol = input
        .configured_protocol
        .or_else(|| input.package.and_then(crate::catalog::protocol))
        .unwrap_or(Protocol::OpenaiResponses);
    let base_url = input
        .configured_url
        .or(input.catalog_url)
        .unwrap_or(API)
        .trim_end_matches('/')
        .to_string();
    Ok(Route {
        protocol,
        base_url,
        auth: Auth::Dynamic(Arc::new(Refreshing {
            file: crate::paths::auth_file(),
            issuer: ISSUER.into(),
        })),
        api_key: None,
        headers: vec![],
        model_id: None,
    })
}
