//! Sign in with ChatGPT (after OpenCode's `ChatGPTPlugin`,
//! `OpenAIContextCapsPlugin` and the ChatGPT session errors): OpenAI's
//! token sharing, which lets a ChatGPT subscription pay for the OpenAI API
//! instead of an API key.
//!
//! The sign-in is OAuth with PKCE against `auth.openai.com`, with a
//! loopback callback, asking for the `chatgpt.tokens.use.direct` scope for
//! the OpenAI API. It runs as an OAuth client of your own: CodeSmith ships
//! none and never borrows another product's (not OpenCode's, not Codex
//! CLI's). Give it with `--client-id`, `settings.clientId` on the `openai`
//! provider, or `CODESMITH_CHATGPT_CLIENT_ID`. OpenAI's agent registration
//! (OpenCode's first sign-in) is used only when you opt in, with
//! `--register` or `settings.register: true` ([`client_id`]): the sign-in
//! then asks as [`REGISTRATION`] with the agent name
//! CodeSmith, so OpenAI registers a client owned by you and returns its id
//! on the callback. OpenAI may refuse it ([`REGISTER_NOTICE`]). Either way the
//! client id is saved with the login, reauthorization reuses it, and a
//! callback naming a different client is refused. The ID token is checked
//! against OpenAI's signing keys, issuer, the client, its expiry and the
//! sign-in's nonce.
//!
//! Signed in, OpenAI requests go to `api.openai.com/v1` over HTTP (not the
//! Responses WebSocket) with the login's access token, refreshed at
//! OpenAI's token endpoint with the same client; only the models ChatGPT
//! serves are listed, at no per-token cost, their context capped as
//! ChatGPT plans cap it, compaction is the summary kind, and failures that
//! retrying cannot fix (a usage limit, an unsupported request) are not
//! retried and say what to do. An API key (`OPENAI_API_KEY`, configured or
//! saved) wins over the login.
//!
//! Codex CLI's own browser and headless sign-ins are not offered: they work
//! only as Codex CLI's OAuth client.

use super::oauth::{self, Callback, Pkce};
use super::{set, Input, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{Auth, Protocol};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const ID: &str = "openai";
pub const METHOD: &str = "chatgpt-token-sharing";
/// Logins of OpenCode's earlier browser method.
const LEGACY_METHOD: &str = "chatgpt-browser";
pub const LABEL: &str = "Sign in with ChatGPT";
pub const CLIENT_ID_VAR: &str = "CODESMITH_CHATGPT_CLIENT_ID";
/// OpenAI's agent registration: given as the client, the sign-in registers
/// a client of the user's own.
pub const REGISTRATION: &str = "dynamic_agent_client";
pub const AGENT_NAME: &str = "CodeSmith";
/// Said before a registering sign-in opens the browser.
pub const REGISTER_NOTICE: &str = "Registering a ChatGPT sign-in client of your own, named CodeSmith, with OpenAI (agent registration).\n\
     OpenAI may reject it: whether it accepts agent registration for CodeSmith is up to OpenAI. \
     If it is refused, sign in with a client ID of your own (--client-id) or use an OpenAI API key.";
/// Added to a registering sign-in's failure.
pub const REGISTER_REJECTED: &str = "OpenAI may have rejected registering a client for CodeSmith. \
     Sign in with a client ID of your own (--client-id, settings.clientId or CODESMITH_CHATGPT_CLIENT_ID) or use an OpenAI API key.";
pub const ISSUER: &str = "https://auth.openai.com";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const SHARING_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const TIMEOUT: Duration = Duration::from_secs(300);

/// The models a ChatGPT login serves.
pub const MODELS: [&str; 22] = [
    "gpt-5.5",
    "gpt-5.5-fast",
    "gpt-5.6-luna",
    "gpt-5.6-luna-fast",
    "gpt-5.6-sol",
    "gpt-5.6-sol-fast",
    "gpt-5.6-terra",
    "gpt-5.6-terra-fast",
    "gpt-6-astra",
    "gpt-6-astra-fast",
    "gpt-6-luna",
    "gpt-6-luna-fast",
    "gpt-6-sol",
    "gpt-6-sol-fast",
    "gpt-6.1-sol",
    "gpt-6.1-sol-fast",
    "gpt-6.1-terra",
    "gpt-6.1-terra-fast",
    "gpt-6.2-astra",
    "gpt-6.2-astra-fast",
    "gpt-6.2-sol",
    "gpt-6.2-sol-fast",
];

/// Failures retrying cannot fix.
const FINAL: [&str; 8] = [
    "subscription_sharing_usage_limit_exceeded",
    "subscription_sharing_workplace_individual_usage_limit_exceeded",
    "subscription_sharing_user_not_eligible",
    "subscription_sharing_unsupported_capability",
    "subscription_sharing_route_not_supported",
    "subscription_sharing_invalid_user",
    "chatpass_v2_scope_not_authorized",
    "chatpass_v2_invalid_authorization_context",
];

/// The sign-in methods `codesmith auth login openai` offers.
pub fn methods() -> Vec<(&'static str, &'static str)> {
    vec![(METHOD, LABEL), ("key", "Manually enter API Key")]
}

/// Whether `stored` is a ChatGPT login.
pub fn is_login(stored: Option<&Value>) -> bool {
    stored.is_some_and(|s| s["type"] == "oauth" && s["method"] == METHOD)
}

/// Whether OpenAI runs on a ChatGPT login: one is saved and no API key
/// overrides it.
pub fn signed_in(config_key: bool) -> bool {
    !config_key
        && std::env::var("OPENAI_API_KEY").map_or(true, |k| k.is_empty())
        && is_login(crate::auth::entry(ID).as_ref())
}

/// The OAuth client to sign in as. `register` (`--register`) asks for a
/// new registration, even over an existing login. Otherwise: the client an
/// existing login was issued, the flag, `settings.clientId`,
/// `CODESMITH_CHATGPT_CLIENT_ID`, then the registration when
/// `settings.register` opts in. Nothing else: the registration is sent only
/// on one of those two opt-ins (giving [`REGISTRATION`] as a client id is
/// refused and names them), and no other product's client is used.
pub fn client_id(
    saved: Option<&str>,
    flag: Option<&str>,
    register: bool,
    settings: &super::Settings,
    env: &super::Env,
) -> Result<String> {
    let flag = flag.map(str::trim).filter(|s| !s.is_empty());
    if register {
        return match flag {
            Some(_) => Err(anyhow!(
                "--register asks OpenAI for a new client; --client-id signs in as an existing one. Give one of them."
            )),
            None => Ok(REGISTRATION.into()),
        };
    }
    let given = saved
        .map(str::trim)
        .filter(|s| !s.is_empty() && !registers(s))
        .map(str::to_string)
        .or_else(|| flag.map(str::to_string))
        .or_else(|| set(&settings.client_id).map(str::to_string))
        .or_else(|| {
            env(CLIENT_ID_VAR)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        });
    if given.as_deref().is_some_and(registers) {
        return Err(anyhow!(
            "{REGISTRATION} is OpenAI's agent registration, not a client id. \
             Opt in to it with --register or settings.register: true on the openai provider (OpenAI may reject it)."
        ));
    }
    given
        .or_else(|| (settings.register == Some(true)).then(|| REGISTRATION.to_string()))
        .ok_or_else(|| {
            anyhow!(
                "Sign in with ChatGPT needs an OAuth client of your own, and CodeSmith ships none (nor uses another product's).\n\
                 Pass its id with --client-id, settings.clientId on the openai provider, or {CLIENT_ID_VAR}; \
                 or opt in to OpenAI's agent registration with --register or settings.register: true, \
                 which registers a client of your own named {AGENT_NAME} (OpenAI may reject it).\n\
                 Or use an OpenAI API key (`codesmith auth login openai`, or OPENAI_API_KEY)."
            )
        })
}

/// Whether signing in as `client_id` registers a new client.
pub fn registers(client_id: &str) -> bool {
    client_id == REGISTRATION
}

/// OpenAI's OAuth endpoints (overridable in tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub issuer: String,
    pub jwks: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            issuer: ISSUER.into(),
            jwks: format!("{ISSUER}/.well-known/jwks.json"),
        }
    }
}

impl Endpoints {
    fn token(&self) -> String {
        format!("{}/api/accounts/oauth/token", self.issuer)
    }
}

/// This install's agent host id, kept in the data folder (made once).
pub fn host_id(file: &Path) -> Result<String> {
    if let Ok(id) = std::fs::read_to_string(file) {
        let id = id.trim();
        if id.starts_with("urn:uuid:") {
            return Ok(id.to_string());
        }
    }
    let id = format!("urn:uuid:{}", uuid_v4());
    crate::paths::write_private(file, id.as_bytes())?;
    Ok(id)
}

fn uuid_v4() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// One sign-in attempt.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub pkce: Pkce,
    pub nonce: String,
    pub host_id: String,
    /// The client asked for: an issued one, or [`REGISTRATION`].
    pub client_id: String,
    pub redirect: String,
}

impl Attempt {
    pub fn new(client_id: &str, host_id: &str, redirect: &str) -> Attempt {
        Attempt {
            pkce: Pkce::new(),
            nonce: Pkce::new().state,
            host_id: host_id.into(),
            client_id: client_id.into(),
            redirect: redirect.into(),
        }
    }

    fn registering(&self) -> bool {
        registers(&self.client_id)
    }
}

/// The authorization URL of `a`.
pub fn authorize_url(e: &Endpoints, a: &Attempt) -> String {
    let mut u = url::Url::parse(&format!("{}/api/accounts/authorize", e.issuer)).unwrap();
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("client_id", &a.client_id);
        if a.registering() {
            q.append_pair("agent_name_hint", AGENT_NAME);
        }
        q.append_pair("ext_agent_host_id", &a.host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &a.redirect)
            .append_pair("scope", SCOPE)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &a.pkce.state)
            .append_pair("nonce", &a.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &a.pkce.challenge);
    }
    u.to_string()
}

/// The client a callback settles on: the one asked for, unless it was the
/// registration, whose callback names the issued client. A callback naming
/// another client than an issued one asked for is refused.
pub fn callback_client(a: &Attempt, returned: Option<&str>) -> Result<String, String> {
    let returned = returned.filter(|r| !r.is_empty());
    if a.registering() {
        return returned
            .filter(|r| !registers(r))
            .map(str::to_string)
            .ok_or_else(|| "ChatGPT sign-in did not return a client ID. Connect again.".into());
    }
    match returned {
        Some(r) if r != a.client_id => {
            Err("ChatGPT returned a different client than this connection. Connect again.".into())
        }
        _ => Ok(a.client_id.clone()),
    }
}

async fn post(client: &reqwest::Client, url: &str, form: &[(&str, &str)]) -> Result<Value> {
    let resp = client
        .post(url)
        .header("accept", "application/json")
        .header("user-agent", codesmith_ai::copilot::user_agent())
        .form(form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| anyhow!("{}", codesmith_ai::Error::from(e)))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("Request failed: {}", status.as_u16()));
    }
    Ok(resp.json().await?)
}

/// The login to save from a token answer.
pub fn credential(tokens: &Value, client_id: &str, scopes: Option<&Value>) -> Result<Value> {
    let s = |k: &str| {
        tokens[k]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("ChatGPT's token answer has no {k}"))
    };
    let granted: Option<Vec<&str>> = tokens["scope"]
        .as_str()
        .map(|s| s.split(' ').filter(|x| !x.is_empty()).collect());
    let scopes = match granted {
        Some(g) => json!(g),
        None => scopes.cloned().unwrap_or_else(|| json!([])),
    };
    let lifetime = tokens["expires_in"].as_u64().unwrap_or(3600);
    Ok(json!({
        "type": "oauth",
        "method": METHOD,
        "access": s("access_token")?,
        "refresh": s("refresh_token")?,
        "expires": oauth::now_ms() + lifetime * 1000,
        "metadata": {"clientID": client_id, "scopes": scopes},
    }))
}

/// Exchange the callback's code, check the answer allows token sharing and
/// carries a valid ID token, and return the login to save.
pub async fn exchange(
    client: &reqwest::Client,
    e: &Endpoints,
    a: &Attempt,
    code: &str,
    client_id: &str,
) -> Result<Value> {
    let tokens = post(
        client,
        &e.token(),
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", &a.pkce.verifier),
            ("redirect_uri", &a.redirect),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    let sharing = tokens["scope"]
        .as_str()
        .is_some_and(|s| s.split(' ').any(|x| x == SHARING_SCOPE));
    if !sharing {
        return Err(anyhow!("ChatGPT sign-in finished without token sharing. Sign in again and allow token sharing, or connect OpenAI with an API key."));
    }
    let id_token = tokens["id_token"]
        .as_str()
        .ok_or_else(|| anyhow!("ChatGPT sign-in did not return an ID token."))?;
    verify_id_token(client, id_token, client_id, &a.nonce, &e.jwks, &e.issuer).await?;
    credential(&tokens, client_id, None)
}

/// Check an ID token: signed (RS256) by a key of `jwks_url`, from `issuer`,
/// for `client_id`, unexpired, with the sign-in's `nonce` and a subject.
pub async fn verify_id_token(
    client: &reqwest::Client,
    token: &str,
    client_id: &str,
    nonce: &str,
    jwks_url: &str,
    issuer: &str,
) -> Result<()> {
    let invalid = |why: &str| anyhow!("ChatGPT sign-in returned an invalid ID token ({why}).");
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let parts: Vec<&str> = token.split('.').collect();
    let [head, body, sig] = parts[..] else {
        return Err(invalid("not a JWT"));
    };
    let header: Value = b64
        .decode(head)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| invalid("header"))?;
    if header["alg"] != "RS256" {
        return Err(invalid("algorithm"));
    }
    let claims: Value = b64
        .decode(body)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| invalid("claims"))?;
    let signature = b64.decode(sig).map_err(|_| invalid("signature"))?;
    let jwks: Value = client
        .get(jwks_url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| invalid("signing keys unavailable"))?
        .json()
        .await
        .map_err(|_| invalid("signing keys unreadable"))?;
    let kid = header["kid"].as_str();
    let signed = format!("{head}.{body}");
    let verified = jwks["keys"].as_array().into_iter().flatten().any(|k| {
        if k["kty"] != "RSA" || kid.is_some_and(|id| k["kid"].as_str() != Some(id)) {
            return false;
        }
        let (Some(n), Some(e)) = (
            k["n"].as_str().and_then(|n| b64.decode(n).ok()),
            k["e"].as_str().and_then(|e| b64.decode(e).ok()),
        ) else {
            return false;
        };
        ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
            .verify(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                signed.as_bytes(),
                &signature,
            )
            .is_ok()
    });
    if !verified {
        return Err(invalid("signature"));
    }
    if claims["iss"] != issuer {
        return Err(invalid("issuer"));
    }
    let aud_ok = match &claims["aud"] {
        Value::String(a) => a == client_id,
        Value::Array(a) => a.iter().any(|x| x == client_id),
        _ => false,
    };
    if !aud_ok {
        return Err(invalid("audience"));
    }
    let now = oauth::now_ms() / 1000;
    match claims["exp"].as_u64() {
        Some(exp) if exp > now => {}
        _ => return Err(invalid("expired")),
    }
    if claims["nonce"] != nonce {
        return Err(anyhow!(
            "ChatGPT sign-in returned an invalid ID token (ID token nonce does not match this sign-in attempt)."
        ));
    }
    if claims["sub"].as_str().is_none_or(|s| s.trim().is_empty()) {
        return Err(invalid("ID token subject is missing"));
    }
    Ok(())
}

/// The whole sign-in: listen on loopback, open the authorization URL with
/// `open`, wait for the callback, exchange it, and save the login as a new
/// credential (the one it replaces is kept). `pasted` takes redirect URLs
/// pasted from a browser on another machine.
pub async fn login(
    client: &reqwest::Client,
    file: &Path,
    e: &Endpoints,
    client_id: &str,
    open: impl FnOnce(&str),
    pasted: Option<tokio::sync::mpsc::Receiver<String>>,
) -> Result<()> {
    login_with(client, file, e, client_id, open, pasted, |m| {
        eprintln!("{m}")
    })
    .await
}

/// [`login`], telling `wrong_paste` (not the terminal) about a pasted URL
/// that is not this login's.
pub async fn login_with(
    client: &reqwest::Client,
    file: &Path,
    e: &Endpoints,
    client_id: &str,
    open: impl FnOnce(&str),
    pasted: Option<tokio::sync::mpsc::Receiver<String>>,
    wrong_paste: impl Fn(&str),
) -> Result<()> {
    let mut cb = Callback::bind("ChatGPT", 0, "/auth/callback").await?;
    let host = host_id(&crate::paths::data_dir().join("chatgpt-agent-host-id"))?;
    let a = Attempt::new(client_id, &host, &cb.redirect);
    open(&authorize_url(e, &a));
    let state = a.pkce.state.clone();
    let check = |p: &oauth::Params| {
        let code = oauth::check(p, &state, "ChatGPT")?;
        callback_client(&a, p.get("client_id").map(String::as_str))?;
        Ok(code)
    };
    let rejected = |err: anyhow::Error| {
        if a.registering() {
            err.context(REGISTER_REJECTED)
        } else {
            err
        }
    };
    let got = cb
        .wait(check, pasted, wrong_paste, TIMEOUT)
        .await
        .map_err(rejected)?;
    let issued = callback_client(&a, got.params.get("client_id").map(String::as_str))
        .map_err(|e| rejected(anyhow!(e)))?;
    match exchange(client, e, &a, &got.code, &issued).await {
        Ok(login) => {
            super::credentials::create_in(
                file,
                super::credentials::New {
                    provider: ID.into(),
                    value: login,
                    label: Some("ChatGPT".into()),
                    ..Default::default()
                },
            )?;
            got.finish(Ok(())).await;
            Ok(())
        }
        Err(err) => {
            got.finish(Err(&format!("{err:#}"))).await;
            Err(rejected(err))
        }
    }
}

/// Exchange a login's refresh token with the client it was issued to.
pub async fn refresh(client: &reqwest::Client, e: &Endpoints, login: &Value) -> Result<Value> {
    let client_id = login["metadata"]["clientID"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!("This ChatGPT connection has no registered client ID. Connect again.")
        })?;
    let tokens = post(
        client,
        &e.token(),
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", login["refresh"].as_str().unwrap_or("")),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    credential(&tokens, client_id, Some(&login["metadata"]["scopes"]))
}

/// Where the model lists saved with logins of the earlier method are kept.
pub fn models_file() -> PathBuf {
    crate::paths::data_dir().join("chatgpt-models.json")
}

/// Move token-sharing logins of the earlier method (those with an issued
/// client) to [`METHOD`], keeping their saved model lists in `models` by
/// client. Codex logins are left alone. Returns how many moved.
pub fn migrate(file: &Path, models: &Path) -> Result<usize> {
    let mut moved = 0;
    for c in super::credentials::list_in(file, ID) {
        let v = &c.value;
        if v["type"] != "oauth" || v["method"] != LEGACY_METHOD {
            continue;
        }
        let Some(client) = v["metadata"]["clientID"].as_str() else {
            continue;
        };
        if let Some(list) = v["metadata"].get("models").filter(|m| m.is_array()) {
            let _lock = crate::auth::FileLock::acquire(models)?;
            let mut all: serde_json::Map<String, Value> = crate::auth::read_for_update(models)?;
            all.entry(format!("models:{client}"))
                .or_insert_with(|| list.clone());
            crate::paths::write_private(models, serde_json::to_string_pretty(&all)?.as_bytes())?;
        }
        let mut nv = v.clone();
        nv["method"] = METHOD.into();
        if let Some(m) = nv["metadata"].as_object_mut() {
            m.remove("models");
        }
        super::credentials::update_in(
            file,
            &c.id,
            super::credentials::Update {
                value: Some(nv),
                ..Default::default()
            },
        )?;
        moved += 1;
    }
    Ok(moved)
}

/// The account's models at `base` (`/models`): the listed ones the API
/// supports. Fails when none are, or the list cannot be read.
pub async fn fetch_models(client: &reqwest::Client, token: &str, base: &str) -> Result<Vec<Value>> {
    let resp = client
        .get(format!("{}/models", base.trim_end_matches('/')))
        .bearer_auth(token)
        .header("user-agent", codesmith_ai::copilot::user_agent())
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow!("{}", codesmith_ai::Error::from(e)))?;
    if !resp.status().is_success() {
        return Err(anyhow!("Request failed: {}", resp.status().as_u16()));
    }
    let v: Value = resp.json().await?;
    const KEEP: [&str; 7] = [
        "slug",
        "display_name",
        "visibility",
        "supported_in_api",
        "context_window",
        "input_modalities",
        "supported_reasoning_levels",
    ];
    let models: Vec<Value> = v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"] == "list" && m["supported_in_api"] == true)
        .map(|m| {
            let mut o = serde_json::Map::new();
            for k in KEEP {
                if let Some(x) = m.get(k) {
                    o.insert(k.into(), x.clone());
                }
            }
            Value::Object(o)
        })
        .collect();
    if models.is_empty() {
        return Err(anyhow!("No ChatGPT models are available for this account."));
    }
    Ok(models)
}

/// Whether `body` (an HTTP error body or a failed stream event) is a
/// ChatGPT failure retrying cannot fix.
pub fn is_final(body: &str) -> bool {
    FINAL.iter().any(|c| body.contains(c))
}

/// What to tell the user about a ChatGPT failure in `body`.
pub fn guidance(body: &str) -> Option<&'static str> {
    const G: [(&str, &str); 10] = [
        ("subscription_sharing_usage_limit_exceeded", "ChatGPT usage limit reached. Check ChatGPT Settings → Usage for details."),
        ("subscription_sharing_workplace_individual_usage_limit_exceeded", "ChatGPT workspace usage limit reached. Ask your workspace admin to raise your limit, or check ChatGPT Settings → Usage."),
        ("subscription_sharing_user_not_eligible", "This ChatGPT account isn't eligible for token sharing. Use an OpenAI API key instead."),
        ("subscription_sharing_usage_unavailable", "ChatGPT usage is unavailable right now. Try again later."),
        ("subscription_sharing_unsupported_capability", "ChatGPT sign-in does not support part of this request. Remove the unsupported feature, or use an OpenAI API key."),
        ("subscription_sharing_route_not_supported", "ChatGPT sign-in does not serve this endpoint. Check the configured endpoint, or use an OpenAI API key."),
        ("subscription_sharing_invalid_user", "This ChatGPT sign-in is no longer valid. Reconnect to ChatGPT."),
        ("subscription_sharing_user_unavailable", "Your ChatGPT account is unavailable right now. Try again later."),
        ("chatpass_v2_scope_not_authorized", "This ChatGPT sign-in isn't authorized for this request. Sign in again and allow token sharing."),
        ("chatpass_v2_invalid_authorization_context", "This ChatGPT sign-in isn't authorized for this request. Sign in again."),
    ];
    G.iter().find(|(c, _)| body.contains(c)).map(|(_, g)| *g)
}

/// The input window ChatGPT plans allow a model: 272k, and 500k from
/// gpt-6.1 on (later models assumed to keep it); Astra models stay at 272k.
pub fn context_cap(model: &str) -> u64 {
    const LEGACY: u64 = 272_000;
    const CURRENT: u64 = 500_000;
    let Some(rest) = model.strip_prefix("gpt-") else {
        return CURRENT;
    };
    let mut nums = rest
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()
        .unwrap_or("")
        .split('.');
    let Some(major) = nums.next().and_then(|n| n.parse::<u32>().ok()) else {
        return CURRENT;
    };
    if model.contains("-astra") {
        return LEGACY;
    }
    let minor = nums.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
    if major > 6 || (major == 6 && minor >= 1) {
        CURRENT
    } else {
        LEGACY
    }
}

/// `(context, input)` for a model of `output` tokens under a ChatGPT login,
/// when the cap is below `(context, input)`.
pub fn capped(model: &str, context: u64, input: Option<u64>, output: u64) -> Option<(u64, u64)> {
    let cap = context_cap(model);
    if input.unwrap_or(context) <= cap {
        return None;
    }
    Some((cap + output, cap))
}

/// What [`Refreshing`] describes itself as.
pub const DESCRIBE: &str = "ChatGPT sign-in";

/// Sends the saved login's access token, refreshing it first when due.
#[derive(Debug)]
pub struct Refreshing {
    pub file: PathBuf,
    pub endpoints: Endpoints,
}

impl codesmith_ai::Authorizer for Refreshing {
    fn authorize<'a>(
        &'a self,
        client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> futures_util::future::BoxFuture<'a, Result<(), codesmith_ai::Error>> {
        Box::pin(async move {
            let fail = codesmith_ai::Error::Invalid;
            let now = oauth::now_ms();
            let login = super::credentials::refresh_in(
                &self.file,
                ID,
                |v| super::credentials::expiring(v, now),
                |v| async move { refresh(client, &self.endpoints, &v).await },
            )
            .await
            .map_err(|e| fail(format!("{e:#}")))?
            .filter(|l| is_login(Some(l)))
            .ok_or_else(|| {
                fail("Sign in with ChatGPT again: `codesmith auth login openai --oauth`".into())
            })?;
            let access = login["access"].as_str().unwrap_or("");
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {access}"))
                .map_err(|_| fail("the saved ChatGPT login is not a valid token".into()))?;
            req.headers_mut()
                .insert(reqwest::header::AUTHORIZATION, value);
            Ok(())
        })
    }

    fn describe(&self) -> String {
        DESCRIBE.into()
    }
}

/// Whether a ChatGPT login (not an API key) drives an OpenAI request.
pub fn owns(input: &Input) -> bool {
    input.provider_id == ID && input.api_key.is_none() && is_login(input.stored.as_ref())
}

pub fn route(input: &Input) -> Result<Route> {
    if crate::auth::entry(ID).is_some_and(|e| e["method"] == LEGACY_METHOD) {
        migrate(&crate::paths::auth_file(), &models_file())?;
    }
    let protocol = input
        .configured_protocol
        .or_else(|| input.package.and_then(crate::catalog::protocol))
        .unwrap_or(Protocol::OpenaiResponses);
    let base_url = input
        .configured_url
        .unwrap_or(RESOURCE)
        .trim_end_matches('/')
        .to_string();
    // The token is for the OpenAI API, and goes nowhere else.
    if !oauth::same_origin(&base_url, RESOURCE) {
        return Err(anyhow!(
            "A ChatGPT sign-in sends its token only to {RESOURCE}, not to {base_url}: remove the openai provider's baseURL, or use an API key"
        ));
    }
    Ok(Route {
        protocol,
        base_url,
        auth: Auth::Dynamic(Arc::new(Refreshing {
            file: crate::paths::auth_file(),
            endpoints: Endpoints::default(),
        })),
        api_key: None,
        headers: vec![],
        model_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // provider-openai-context-caps.test.ts: "caps OpenAI models under ${methodID}"
    #[test]
    fn oc_chatgpt_caps_openai_models() {
        for (id, cap) in [
            ("gpt-5.5", 272_000),
            ("gpt-5.6-sol", 272_000),
            ("gpt-6-luna", 272_000),
            ("gpt-6.1-sol", 500_000),
            ("gpt-6.2-astra", 272_000),
            ("gpt-7", 500_000),
            ("gpt-7-astra", 272_000),
        ] {
            assert_eq!(context_cap(id), cap, "{id}");
        }
        assert_eq!(
            capped("gpt-5.5", 1_050_000, None, 128_000),
            Some((400_000, 272_000))
        );
        assert_eq!(capped("gpt-5.5", 200_000, None, 128_000), None);
        assert_eq!(
            capped("gpt-6.1-sol", 1_050_000, Some(922_000), 128_000),
            Some((628_000, 500_000))
        );
    }

    // session-error.test.ts: "types the ChatGPT usage limit whichever way the failure arrives"
    #[test]
    fn oc_chatgpt_types_the_usage_limit_whichever_way_it_arrives() {
        let http =
            r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"Limit"}}"#;
        let stream = r#"{"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}"#;
        for body in [http, stream] {
            assert_eq!(
                guidance(body),
                Some("ChatGPT usage limit reached. Check ChatGPT Settings → Usage for details.")
            );
        }
    }

    // session-error.test.ts: "types the ChatGPT workspace usage limit"
    #[test]
    fn oc_chatgpt_types_the_workspace_usage_limit() {
        let body = r#"{"error":{"code":"subscription_sharing_workplace_individual_usage_limit_exceeded"}}"#;
        assert!(guidance(body).unwrap().contains("workspace admin"));
    }

    #[test]
    fn host_ids_are_kept_and_shaped_like_uuids() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("host");
        let a = host_id(&f).unwrap();
        assert!(regex::Regex::new(
            r"^urn:uuid:[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
        )
        .unwrap()
        .is_match(&a));
        assert_eq!(host_id(&f).unwrap(), a);
    }
}
