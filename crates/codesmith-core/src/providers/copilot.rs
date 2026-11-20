//! GitHub Copilot (after OpenCode's `GithubCopilotPlugin` and
//! `github-copilot/models`): a login through GitHub's device flow (on
//! github.com or a GitHub Enterprise host), whose token Copilot takes as is
//! (it does not expire; there is nothing to exchange or refresh), the
//! account's API endpoint, and each model on the Copilot API that serves it:
//! Anthropic Messages under `/v1`, OpenAI Responses or Chat Completions at
//! the root.

use super::{Input, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{copilot, Auth, Protocol};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const ID: &str = copilot::ID;
/// The variable that names the GitHub OAuth app to log in with.
pub const CLIENT_ID_VAR: &str = "CODESMITH_COPILOT_CLIENT_ID";
const METHOD: &str = "device";
/// Added to each poll interval, as OpenCode does.
const POLL_MARGIN: Duration = Duration::from_secs(3);

pub fn owns(input: &Input) -> bool {
    input.provider_id == ID
}

/// `company.ghe.com` from a URL or domain.
pub fn normalize_domain(input: &str) -> String {
    input
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// The `/copilot_internal/user` URL of github.com or an Enterprise host.
pub fn user_url(enterprise: Option<&str>) -> String {
    match enterprise {
        Some(e) => format!("https://api.{}/copilot_internal/user", normalize_domain(e)),
        None => "https://api.github.com/copilot_internal/user".into(),
    }
}

/// The default Copilot API of github.com or an Enterprise host.
pub fn default_root(enterprise: Option<&str>) -> String {
    match enterprise {
        Some(e) => format!("https://copilot-api.{}", normalize_domain(e)),
        None => "https://api.githubcopilot.com".into(),
    }
}

/// The root a login serves from until the account lookup answers: the
/// endpoint saved with it, else its host's default.
pub fn stored_root(metadata: &Value) -> String {
    metadata["apiEndpoint"]
        .as_str()
        .filter(|e| !e.is_empty())
        .map(|e| e.trim_end_matches('/').to_string())
        .unwrap_or_else(|| default_root(metadata["enterpriseUrl"].as_str()))
}

/// Why an account cannot chat, from its `/copilot_internal/user` answer.
/// Only an explicit `chat_enabled: false` blocks.
pub fn entitlement_error(user: &Value) -> Option<&'static str> {
    if user["chat_enabled"] != false {
        return None;
    }
    Some(if user["can_signup_for_limited"] == true {
        "This GitHub account is not signed up for GitHub Copilot. Sign up for Copilot Free at https://github.com/features/copilot/plans and connect again."
    } else {
        "This GitHub account does not have GitHub Copilot access. It needs an active Copilot subscription or a seat assigned by an organization."
    })
}

/// Which Copilot API serves a model the catalog does not describe: GPT-5
/// and later (but GPT-5 mini), Grok and MAI Code on Responses, the rest on
/// Chat Completions.
pub fn fallback_protocol(model: &str) -> Protocol {
    let gpt = model
        .strip_prefix("gpt-")
        .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse::<u32>().ok());
    if gpt.is_some_and(|n| n >= 5) && !model.starts_with("gpt-5-mini") {
        return Protocol::OpenaiResponses;
    }
    if model.starts_with("grok-") || model.starts_with("mai-code-") {
        return Protocol::OpenaiResponses;
    }
    Protocol::OpenaiChat
}

/// The catalog package's Copilot API, if it names one.
fn package_protocol(package: &str) -> Option<Protocol> {
    let p = package.strip_prefix("@opencode/ai/providers/")?;
    Some(match p {
        "anthropic" => Protocol::AnthropicMessages,
        "openai" | "openai/responses" | "openai-compatible-responses" => Protocol::OpenaiResponses,
        "openai-compatible" | "openai/chat" => Protocol::OpenaiChat,
        _ => return None,
    })
}

/// One authorizer per token and root, so the account lookup runs once per
/// process.
fn authorizer(token: &str, root: &str, user_url: Option<String>) -> Arc<copilot::Copilot> {
    type Key = (String, String, Option<String>);
    static SHARED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Key, Arc<copilot::Copilot>>>,
    > = std::sync::OnceLock::new();
    use sha2::Digest;
    let fp = format!("{:x}", sha2::Sha256::digest(token.as_bytes()));
    SHARED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry((fp, root.to_string(), user_url.clone()))
        .or_insert_with(|| {
            Arc::new(copilot::Copilot::new(
                token.to_string(),
                root.to_string(),
                user_url,
            ))
        })
        .clone()
}

pub fn route(input: &Input) -> Result<Route> {
    let login = input
        .stored
        .as_ref()
        .filter(|s| s["type"] == "oauth")
        .cloned();
    // COPILOT_GITHUB_TOKEN (or a configured key) beats the saved login.
    let token = input
        .api_key
        .clone()
        .or_else(|| {
            let l = login.as_ref()?;
            l["refresh"]
                .as_str()
                .or(l["access"].as_str())
                .map(str::to_string)
        })
        .ok_or_else(|| {
            anyhow!("GitHub Copilot needs a login: run `codesmith auth login github-copilot` (or set COPILOT_GITHUB_TOKEN)")
        })?;
    let metadata = login
        .as_ref()
        .map(|l| l["metadata"].clone())
        .unwrap_or(Value::Null);
    let protocol = input.configured_protocol.unwrap_or_else(|| {
        input
            .package
            .and_then(package_protocol)
            .unwrap_or_else(|| fallback_protocol(input.model_id))
    });
    let (root, lookup) = match input.configured_url {
        Some(u) => (u.trim_end_matches('/').to_string(), None),
        None => (
            stored_root(&metadata),
            Some(user_url(metadata["enterpriseUrl"].as_str())),
        ),
    };
    // Copilot serves Messages under /v1 and its OpenAI APIs at the root.
    let base_url = match (protocol, input.configured_url) {
        (Protocol::AnthropicMessages, None) => format!("{root}/v1"),
        _ => root.clone(),
    };
    Ok(Route {
        protocol,
        base_url,
        auth: Auth::Dynamic(authorizer(&token, &root, lookup)),
        api_key: None,
        headers: vec![],
    })
}

/// GitHub's device-flow endpoints (overridable in tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// The client id of the GitHub OAuth app the login runs as.
    pub client_id: String,
    pub device: String,
    pub token: String,
    pub user: String,
}

impl Endpoints {
    pub fn for_host(client_id: &str, enterprise: Option<&str>) -> Endpoints {
        let domain = enterprise
            .map(normalize_domain)
            .unwrap_or_else(|| "github.com".into());
        Endpoints {
            client_id: client_id.to_string(),
            device: format!("https://{domain}/login/device/code"),
            token: format!("https://{domain}/login/oauth/access_token"),
            user: user_url(enterprise),
        }
    }
}

/// The code to show the user.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub verification_uri: String,
    pub user_code: String,
    device_code: String,
    interval: Duration,
}

async fn post(client: &reqwest::Client, url: &str, body: Value) -> Result<Value> {
    let resp = client
        .post(url)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("user-agent", copilot::user_agent())
        .json(&body)
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

/// The GitHub OAuth app to log in as: the `--client-id` flag, else
/// `settings.clientId`, else `CODESMITH_COPILOT_CLIENT_ID`. CodeSmith has no
/// app of its own and does not borrow another project's, so without one
/// the login explains how to get one.
pub fn client_id(
    flag: Option<&str>,
    settings: &super::Settings,
    env: &super::Env,
) -> Result<String> {
    flag.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| super::set(&settings.client_id).map(str::to_string))
        .or_else(|| env(CLIENT_ID_VAR))
        .ok_or_else(|| {
            anyhow!(
                "GitHub Copilot's device login needs the client id of a GitHub OAuth app, and CodeSmith ships none.\n\
                 Register one at https://github.com/settings/applications/new (any homepage and callback URL), \
                 tick \"Enable Device Flow\", and pass its client id with --client-id, \
                 settings.clientId on the github-copilot provider, or {CLIENT_ID_VAR}.\n\
                 Or skip the login: set COPILOT_GITHUB_TOKEN to a GitHub token with Copilot access."
            )
        })
}

/// Start a device authorization.
pub async fn start(client: &reqwest::Client, e: &Endpoints) -> Result<DeviceCode> {
    let v = post(
        client,
        &e.device,
        json!({"client_id": e.client_id, "scope": "read:user"}),
    )
    .await?;
    let s = |k: &str| {
        v[k].as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("GitHub's device authorization answer has no {k}"))
    };
    Ok(DeviceCode {
        verification_uri: s("verification_uri")?,
        user_code: s("user_code")?,
        device_code: s("device_code")?,
        interval: Duration::from_secs(v["interval"].as_u64().unwrap_or(5).max(1)),
    })
}

/// Poll until the user approves (or denies) the code, check the account
/// can use Copilot, and return the login to save.
pub async fn finish(
    client: &reqwest::Client,
    e: &Endpoints,
    code: &DeviceCode,
    enterprise: Option<&str>,
    margin: Duration,
) -> Result<Value> {
    let mut wait = code.interval;
    let access = loop {
        tokio::time::sleep(wait + margin).await;
        let v = post(
            client,
            &e.token,
            json!({
                "client_id": e.client_id,
                "device_code": code.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }),
        )
        .await?;
        if let Some(t) = v["access_token"].as_str().filter(|t| !t.is_empty()) {
            break t.to_string();
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                wait = match v["interval"].as_u64().filter(|i| *i > 0) {
                    Some(i) => Duration::from_secs(i),
                    None => wait + Duration::from_secs(5),
                };
            }
            Some(err) => return Err(anyhow!("Device authorization failed: {err}")),
            None => return Err(anyhow!("Device authorization failed")),
        }
    };
    // Only an explicit entitlement answer blocks; a failed lookup must not
    // turn a GitHub hiccup into a denial.
    let user: Option<Value> = async {
        let r = client
            .get(&e.user)
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {access}"))
            .header("user-agent", copilot::user_agent())
            .header("x-github-api-version", copilot::USER_API_VERSION)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json().await.ok()
    }
    .await;
    if let Some(why) = user.as_ref().and_then(entitlement_error) {
        return Err(anyhow!(why));
    }
    let endpoint = user
        .as_ref()
        .and_then(|u| u["endpoints"]["api"].as_str())
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    let mut metadata = serde_json::Map::new();
    if let Some(d) = enterprise {
        metadata.insert("enterpriseUrl".into(), normalize_domain(d).into());
    }
    if let Some(ep) = endpoint {
        metadata.insert("apiEndpoint".into(), ep.into());
    }
    let mut login = json!({
        "type": "oauth",
        "method": METHOD,
        "access": access,
        "refresh": access,
        "expires": 0,
    });
    if !metadata.is_empty() {
        login["metadata"] = Value::Object(metadata);
    }
    Ok(login)
}

/// The whole device login, showing the code through `show`, saved to
/// `file`.
pub async fn login(
    client: &reqwest::Client,
    file: &std::path::Path,
    client_id: &str,
    enterprise: Option<&str>,
    show: impl FnOnce(&DeviceCode),
) -> Result<()> {
    let e = Endpoints::for_host(client_id, enterprise);
    let code = start(client, &e).await?;
    show(&code);
    let login = finish(client, &e, &code, enterprise, POLL_MARGIN).await?;
    crate::auth::set_entry_in(file, ID, login)
}

/// Models Copilot hides for itself: `gpt-5-chat-latest` is a chat-only
/// alias that clashes with Copilot's GPT-5 Responses route.
pub fn hidden(model: &str) -> bool {
    model == "gpt-5-chat-latest"
}
