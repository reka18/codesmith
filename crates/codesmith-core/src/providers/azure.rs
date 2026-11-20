//! Azure (after OpenCode's `AzurePlugin`): the resource name from the saved
//! login, config or environment; Microsoft Entra ID tokens from the Azure
//! CLI (`az account get-access-token`) instead of a key; and the resource's
//! deployments as the provider's models.
//!
//! A login made with `codesmith auth login azure --cli` signs each request
//! with a token for its audience (`https://ai.azure.com` for Foundry
//! endpoints on `*.services.ai.azure.com`, Cognitive Services for the
//! rest), from the Azure CLI's own sign-in; CodeSmith stores no Azure
//! secret for it. `codesmith models azure` lists the deployments: through
//! the Azure Resource Manager API with an Azure CLI login (falling back to
//! the resource's own list), or the resource's own list with a key, and
//! keeps them (see [`super::live`]); each deployment becomes a model named
//! after it, with the catalog model it deploys as its template.

use super::{set, Field, KeyForm};
use anyhow::{anyhow, Result};
use codesmith_ai::Error as AiError;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ID: &str = "azure";
/// The saved login method of an Azure CLI login.
pub const CLI_METHOD: &str = "azure-cli";
pub const CLI_LABEL: &str = "Microsoft Entra ID (Azure CLI)";
pub const COGNITIVE_SCOPE: &str = "https://cognitiveservices.azure.com/.default";
pub const FOUNDRY_SCOPE: &str = "https://ai.azure.com/.default";
pub const MANAGEMENT_SCOPE: &str = "https://management.azure.com/.default";

/// A resource name becomes a hostname label and a query literal, so Azure's
/// letters, digits and hyphens are all it may hold.
pub fn valid_resource(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The resource of a saved key or Azure CLI login.
pub fn stored_resource(stored: Option<&Value>) -> Option<String> {
    let s = stored?;
    let usable = s["type"] == "api" || (s["type"] == "external" && s["method"] == CLI_METHOD);
    usable
        .then(|| s["metadata"]["resourceName"].as_str())
        .flatten()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
}

/// The resource name: the saved login's, else `settings.resourceName`, else
/// `AZURE_RESOURCE_NAME`, else `AZURE_COGNITIVE_SERVICES_RESOURCE_NAME`.
pub fn resource_name(
    settings: &crate::config::ProviderSettings,
    stored: Option<&Value>,
    env: &super::Env,
) -> Option<String> {
    stored_resource(stored)
        .or_else(|| set(&settings.resource_name).map(str::to_string))
        .or_else(|| env("AZURE_RESOURCE_NAME"))
        .or_else(|| env("AZURE_COGNITIVE_SERVICES_RESOURCE_NAME"))
}

/// `${AZURE_RESOURCE_NAME}` (and the legacy variable) in a URL template.
pub fn expand(url: &str, resource: &str) -> String {
    url.replace("${AZURE_RESOURCE_NAME}", resource)
        .replace("${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}", resource)
}

fn resource_field() -> Field {
    Field {
        key: "resourceName",
        title: "Enter Azure Resource Name",
        placeholder: "e.g. my-models",
        required: true,
    }
}

/// The key login, asking for the resource unless config names it.
pub fn key_form(settings: &crate::config::ProviderSettings) -> KeyForm {
    let configured = set(&settings.resource_name).is_some() || settings.base_url.is_some();
    KeyForm {
        label: "API key",
        fields: if configured {
            vec![]
        } else {
            vec![resource_field()]
        },
    }
}

/// The Azure CLI on `PATH`, if installed (the CLI login is offered only
/// then).
pub fn find_cli(env: &super::Env) -> Option<PathBuf> {
    let path = env("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(if cfg!(windows) { "az.cmd" } else { "az" }))
        .find(|p| p.is_file())
}

/// Save an Azure CLI login for `resource`.
pub fn save_cli_login(file: &std::path::Path, resource: Option<&str>) -> Result<()> {
    let mut entry = json!({"type": "external", "method": CLI_METHOD});
    if let Some(r) = resource {
        if !valid_resource(r) {
            return Err(anyhow!(
                "{r:?} is not an Azure resource name: use the letters, digits and hyphens of your Azure OpenAI or AI Services resource"
            ));
        }
        entry["metadata"] = json!({"resourceName": r});
    }
    crate::auth::set_entry_in(file, ID, entry)
}

/// Whether the saved login is an Azure CLI login.
pub fn cli_login(stored: Option<&Value>) -> bool {
    stored.is_some_and(|s| s["type"] == "external" && s["method"] == CLI_METHOD)
}

/// The audience of a request URL.
pub fn scope_for(url: &reqwest::Url) -> &'static str {
    let host = url.host_str().unwrap_or("");
    if host.ends_with(".services.ai.azure.com") && !url.path().starts_with("/models") {
        FOUNDRY_SCOPE
    } else {
        COGNITIVE_SCOPE
    }
}

/// Tokens from the Azure CLI, one per scope, reused until five minutes
/// before they expire.
#[derive(Debug)]
pub struct Cli {
    program: PathBuf,
    tokens: Mutex<HashMap<String, (String, u64)>>,
    calls: tokio::sync::Mutex<()>,
}

impl Cli {
    pub fn new(program: PathBuf) -> Cli {
        Cli {
            program,
            tokens: Mutex::new(HashMap::new()),
            calls: tokio::sync::Mutex::new(()),
        }
    }

    fn cached(&self, scope: &str) -> Option<String> {
        let now = super::oauth::now_ms();
        let t = self.tokens.lock().unwrap();
        t.get(scope)
            .filter(|(_, exp)| exp.saturating_sub(now) > 5 * 60_000)
            .map(|(t, _)| t.clone())
    }

    pub async fn token(&self, scope: &str) -> Result<String> {
        if let Some(t) = self.cached(scope) {
            return Ok(t);
        }
        let _one = self.calls.lock().await;
        if let Some(t) = self.cached(scope) {
            return Ok(t);
        }
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(&self.program)
                .args([
                    "account",
                    "get-access-token",
                    "--scope",
                    scope,
                    "--output",
                    "json",
                ])
                .env_remove(crate::SERVER_TOKEN_VAR)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| anyhow!("the Azure CLI did not answer within 10 seconds"))?
        .map_err(|e| anyhow!("running the Azure CLI: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!(
                "the Azure CLI could not get a token (run `az login`): {}",
                err.lines().next().unwrap_or("").trim()
            ));
        }
        let v: Value = serde_json::from_slice(&out.stdout)
            .map_err(|_| anyhow!("the Azure CLI's token answer is not JSON"))?;
        let access = v["accessToken"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("the Azure CLI's token answer has no accessToken"))?
            .to_string();
        let expires = match (&v["expires_on"], v["expiresOn"].as_str()) {
            (Value::Number(n), _) => n.as_u64().map(|s| s * 1000),
            (_, Some(s)) => parse_expires_on(s),
            _ => None,
        }
        .ok_or_else(|| anyhow!("Azure CLI returned an invalid token expiration"))?;
        self.tokens
            .lock()
            .unwrap()
            .insert(scope.to_string(), (access.clone(), expires));
        Ok(access)
    }
}

/// `expiresOn` as older Azure CLIs print it (local time, no zone): the
/// token is trusted only a little past the refresh margin, so it is
/// fetched again soon rather than kept on a guessed expiry.
fn parse_expires_on(s: &str) -> Option<u64> {
    (!s.trim().is_empty()).then(|| super::oauth::now_ms() + 6 * 60_000)
}

/// One Azure CLI per program, so tokens are shared across models.
pub fn cli(program: PathBuf) -> Arc<Cli> {
    static SHARED: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<Cli>>>> =
        std::sync::OnceLock::new();
    SHARED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(program.clone())
        .or_insert_with(|| Arc::new(Cli::new(program)))
        .clone()
}

/// Signs requests with an Azure CLI token for their audience, in place of
/// the key headers.
#[derive(Debug)]
pub struct CliAuth(pub Arc<Cli>);

impl codesmith_ai::Authorizer for CliAuth {
    fn authorize<'a>(
        &'a self,
        _client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> BoxFuture<'a, Result<(), AiError>> {
        Box::pin(async move {
            let token = self
                .0
                .token(scope_for(req.url()))
                .await
                .map_err(|e| AiError::Invalid(e.to_string()))?;
            let h = req.headers_mut();
            h.remove("api-key");
            h.remove("x-api-key");
            h.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
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
        "Azure CLI (Microsoft Entra ID)".into()
    }
}

/// The authorizer of an Azure CLI login, when the CLI is installed.
pub fn cli_auth(env: &super::Env) -> Result<codesmith_ai::Auth> {
    let program = find_cli(env).ok_or_else(|| {
        anyhow!("the azure login uses the Azure CLI, which is not on PATH: install it, or log in with a key")
    })?;
    Ok(codesmith_ai::Auth::Dynamic(Arc::new(CliAuth(cli(program)))))
}

// ---------------------------------------------------------------------------
// Deployments

/// Where deployments are listed (overridable in tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// `https://<resource>.openai.azure.com/openai` with `{}` for the name.
    pub resource: String,
    pub management: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            resource: "https://{}.openai.azure.com/openai".into(),
            management: "https://management.azure.com".into(),
        }
    }
}

/// How the deployments are read.
pub enum Credential<'a> {
    Key(&'a str),
    Cli(&'a Cli),
}

/// A deployment: its name and the model it deploys.
pub type Deployment = (String, String);

async fn get_json(client: &reqwest::Client, req: reqwest::RequestBuilder) -> Result<Value> {
    let resp = req
        .header("accept", "application/json")
        .header("user-agent", codesmith_ai::copilot::user_agent())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow!("{}", codesmith_ai::Error::from(e)))?;
    let _ = client;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("Azure answered {}", status.as_u16()));
    }
    resp.json()
        .await
        .map_err(|_| anyhow!("Azure's answer is not JSON"))
}

/// The resource's own list (data-plane `2022-12-01`, the only version that
/// has it): succeeded deployments.
pub async fn resource_deployments(
    client: &reqwest::Client,
    e: &Endpoints,
    resource: &str,
    cred: &Credential<'_>,
) -> Result<Vec<Deployment>> {
    let url = format!(
        "{}/deployments?api-version=2022-12-01",
        e.resource.replace("{}", resource)
    );
    let rb = client.get(url);
    let rb = match cred {
        Credential::Key(k) => rb.header("api-key", *k),
        Credential::Cli(c) => rb.bearer_auth(c.token(COGNITIVE_SCOPE).await?),
    };
    let v = get_json(client, rb).await?;
    let data = v["data"]
        .as_array()
        .ok_or_else(|| anyhow!("Azure's deployment list has no data"))?;
    Ok(data
        .iter()
        .filter(|d| d["status"] == "succeeded")
        .filter_map(|d| {
            let id = d["id"].as_str().filter(|s| !s.is_empty())?;
            let model = d["model"].as_str().filter(|s| !s.is_empty())?;
            Some((id.to_string(), model.to_string()))
        })
        .collect())
}

/// The Resource Graph query that finds the resource's id across every
/// subscription the account can read.
pub fn resource_query(resource: &str) -> String {
    [
        "resources",
        "| where type =~ 'microsoft.cognitiveservices/accounts' and kind in~ ('AIServices', 'OpenAI')",
        "| extend resourceName = tostring(properties.customSubDomainName)",
        &format!("| where resourceName =~ '{resource}'"),
        "| project id",
        "| take 1",
    ]
    .join(" ")
}

/// The management API URL of the deployments of resource `id` (from the
/// Resource Graph's answer), which the management token is sent to: `id`
/// must be a plain ARM resource path, and the URL must stay on the
/// management endpoint.
pub fn deployments_url(management: &str, id: &str) -> Result<String> {
    let plain = id.starts_with("/subscriptions/")
        && !["@", "//", "..", "?", "#", "\\", "%"]
            .iter()
            .any(|bad| id.contains(bad))
        && !id.chars().any(|c| c.is_whitespace() || c.is_control());
    if !plain {
        return Err(anyhow!(
            "Azure returned a resource id that is not one: {id:?}"
        ));
    }
    let url = format!("{management}{id}/deployments?api-version=2024-10-01");
    let origin = reqwest::Url::parse(management)?.origin();
    if reqwest::Url::parse(&url).map(|u| u.origin()).ok() != Some(origin) {
        return Err(anyhow!(
            "Azure returned a resource id outside the management endpoint"
        ));
    }
    Ok(url)
}

/// The management API's list, every page of it (a list is complete or an
/// error); page links must stay on the management endpoint, which every
/// page's token is for.
pub async fn management_deployments(
    client: &reqwest::Client,
    e: &Endpoints,
    resource: &str,
    cli: &Cli,
) -> Result<Vec<Deployment>> {
    let token = cli.token(MANAGEMENT_SCOPE).await?;
    let found = get_json(
        client,
        client
            .post(format!(
                "{}/providers/Microsoft.ResourceGraph/resources?api-version=2022-10-01",
                e.management
            ))
            .bearer_auth(&token)
            .json(&json!({"query": resource_query(resource)})),
    )
    .await?;
    let id = found["data"][0]["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Azure resource {resource:?} was not found"))?;
    let origin = reqwest::Url::parse(&e.management)?.origin();
    let mut url = deployments_url(&e.management, id)?;
    let mut out = vec![];
    loop {
        let page = get_json(client, client.get(&url).bearer_auth(&token)).await?;
        for d in page["value"].as_array().into_iter().flatten() {
            let (Some(name), Some(model)) = (
                d["name"].as_str().filter(|s| !s.is_empty()),
                d["properties"]["model"]["name"]
                    .as_str()
                    .filter(|s| !s.is_empty()),
            ) else {
                continue;
            };
            if d["properties"]["provisioningState"] == "Succeeded" {
                out.push((name.to_string(), model.to_string()));
            }
        }
        match page["nextLink"].as_str() {
            None => break,
            Some(next) => {
                if reqwest::Url::parse(next).map(|u| u.origin()).ok() != Some(origin.clone()) {
                    return Err(anyhow!(
                        "Azure returned a deployment page outside the management endpoint"
                    ));
                }
                url = next.to_string();
            }
        }
    }
    Ok(out)
}

/// The deployments: through the management API with an Azure CLI login
/// (reporting its error if the resource's own list fails too), else the
/// resource's own list; sorted by name.
pub async fn deployments(
    client: &reqwest::Client,
    e: &Endpoints,
    resource: &str,
    cred: &Credential<'_>,
) -> Result<Vec<Deployment>> {
    if !valid_resource(resource) {
        return Err(anyhow!("{resource:?} is not an Azure resource name"));
    }
    let mut list = match cred {
        Credential::Cli(c) => match management_deployments(client, e, resource, c).await {
            Ok(l) => l,
            Err(err) => resource_deployments(client, e, resource, cred)
                .await
                .map_err(|_| err)?,
        },
        Credential::Key(_) => resource_deployments(client, e, resource, cred).await?,
    };
    list.sort();
    Ok(list)
}

/// The catalog model a deployment deploys: by its model name, else by its
/// own name (Azure spells some models unlike the catalog), ignoring case.
fn template<'a>(
    templates: &'a [crate::catalog::Model],
    d: &Deployment,
) -> Option<&'a crate::catalog::Model> {
    let by = |n: &str| templates.iter().find(|m| m.id.eq_ignore_ascii_case(n));
    by(&d.1).or_else(|| by(&d.0))
}

/// Deployments as models: each named after the deployment (lowercase, as
/// the catalog's ids), with its catalog model's limits, prices and routes.
/// Deployments of models outside the catalog are left to configuration.
pub fn derive(raw: &[Value], templates: &[crate::catalog::Model]) -> Vec<crate::catalog::Model> {
    let mut out: Vec<crate::catalog::Model> = vec![];
    for r in raw {
        let (Some(name), Some(model)) = (r["name"].as_str(), r["model"].as_str()) else {
            continue;
        };
        let d = (name.to_string(), model.to_string());
        let Some(t) = template(templates, &d) else {
            continue;
        };
        let mut m = t.clone();
        let id = name.to_ascii_lowercase();
        m.name = if id == t.id.to_ascii_lowercase() {
            t.name.clone()
        } else {
            format!("{} ({name})", t.name)
        };
        m.id = id;
        m.model_id = name.to_string();
        out.retain(|o| o.id != m.id);
        out.push(m);
    }
    out
}

/// Deployments as the kept list's entries.
pub fn raw(list: &[Deployment]) -> Vec<Value> {
    list.iter()
        .map(|(n, m)| json!({"name": n, "model": m}))
        .collect()
}
