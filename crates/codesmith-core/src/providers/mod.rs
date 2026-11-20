//! Provider plugins (after OpenCode's `plugin/provider/*`): providers whose
//! endpoint, protocol or credentials need more than a base URL and a key.
//! Amazon Bedrock signs with AWS SigV4 (or takes a Bedrock API key),
//! Google Vertex AI authenticates with Application Default Credentials, and
//! GitHub Copilot logs in with GitHub's device flow and routes each model to
//! the API that serves it.

pub mod bedrock;
pub mod cloudflare;
pub mod copilot;
pub mod gateways;
pub mod vertex;

use codesmith_ai::{Auth, Protocol};
use serde::Deserialize;
use serde_json::Value;

/// The environment, injectable for tests.
pub type Env = codesmith_ai::aws::Env;

/// Settings only some providers read (`settings.region`, …).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Amazon Bedrock: the AWS region (default `AWS_REGION`, then
    /// `AWS_DEFAULT_REGION`, then `us-east-1`).
    pub region: Option<String>,
    /// Amazon Bedrock: the shared-config profile SigV4 credentials come
    /// from.
    pub profile: Option<String>,
    /// Amazon Bedrock: a private (VPC) endpoint, used as the base URL.
    pub endpoint: Option<String>,
    /// Amazon Bedrock: `bearer` (an API key) or `sigv4` (AWS credentials,
    /// ignoring `AWS_BEARER_TOKEN_BEDROCK`). Unset: a key when there is
    /// one, else SigV4.
    #[serde(rename = "auth")]
    pub auth_mode: Option<String>,
    /// Google Vertex AI: the project.
    pub project: Option<String>,
    /// Google Vertex AI: the location (default `global`).
    pub location: Option<String>,
    /// Google Vertex AI: an OAuth access token to use instead of ADC.
    #[serde(alias = "accessToken")]
    pub access_token: Option<String>,
    /// GitHub Copilot: the client id of the GitHub OAuth app that
    /// `codesmith auth login github-copilot` logs in as.
    #[serde(alias = "clientId")]
    pub client_id: Option<String>,
    /// Cloudflare AI Gateway and Workers AI: the account id.
    #[serde(alias = "accountId")]
    pub account_id: Option<String>,
    /// Cloudflare AI Gateway: the gateway id (default `default`).
    #[serde(alias = "gatewayId")]
    pub gateway_id: Option<String>,
}

/// What a provider plugin is given.
pub struct Input<'a> {
    pub provider_id: &'a str,
    /// The provider's settings (built-in, catalog and configured).
    pub settings: &'a crate::config::ProviderSettings,
    /// The configured base URL, which always wins.
    pub configured_url: Option<&'a str>,
    /// The configured protocol, which always wins.
    pub configured_protocol: Option<Protocol>,
    /// The model's catalog package and (unexpanded) catalog base URL.
    pub package: Option<&'a str>,
    pub catalog_url: Option<&'a str>,
    /// The model id sent to the provider.
    pub model_id: &'a str,
    /// The key found in config, the key variables or the saved keys.
    pub api_key: Option<String>,
    /// The saved login, if any (see [`crate::auth::entry_in`]).
    pub stored: Option<Value>,
    pub env: Env,
}

/// How to reach a model of a provider plugin.
#[derive(Debug, Clone)]
pub struct Route {
    pub protocol: Protocol,
    pub base_url: String,
    pub auth: Auth,
    pub api_key: Option<String>,
    /// Headers added to the configured ones.
    pub headers: Vec<(String, String)>,
    /// The model id to send, when the provider spells it differently from
    /// the catalog (variants still read the catalog's).
    pub model_id: Option<String>,
}

/// A question a key login asks beside the key (OpenCode's form fields);
/// the answers are saved with the key as its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub key: &'static str,
    pub title: &'static str,
    pub placeholder: &'static str,
    pub required: bool,
}

/// How `codesmith auth login` asks for a provider's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyForm {
    pub label: &'static str,
    pub fields: Vec<Field>,
}

/// The key login of a provider whose login asks more than the key.
pub fn key_form(
    id: &str,
    settings: &crate::config::ProviderSettings,
    env: &Env,
) -> Option<KeyForm> {
    match id {
        cloudflare::GATEWAY => Some(cloudflare::gateway_form(settings, env)),
        cloudflare::WORKERS => Some(cloudflare::workers_form(settings, env)),
        _ => None,
    }
}

/// An answer saved with a key login (`{"type": "api", "key", "metadata"}`).
pub fn stored_metadata(stored: Option<&Value>, key: &str) -> Option<String> {
    let s = stored?;
    (s["type"] == "api")
        .then(|| s["metadata"][key].as_str())
        .flatten()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Save a key with its login answers.
pub fn save_key(
    file: &std::path::Path,
    provider: &str,
    key: &str,
    metadata: serde_json::Map<String, Value>,
) -> anyhow::Result<()> {
    let mut entry = serde_json::json!({"type": "api", "key": key});
    if !metadata.is_empty() {
        entry["metadata"] = Value::Object(metadata);
    }
    crate::auth::set_entry_in(file, provider, entry)
}

/// `encodeURIComponent`.
pub(crate) fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The providers here, with their names and key variables.
pub const PROVIDERS: &[(&str, &str, &[&str])] = &[
    (
        "amazon-bedrock",
        "Amazon Bedrock",
        &["AWS_BEARER_TOKEN_BEDROCK"],
    ),
    // The Express Mode key is Vertex's only key variable; project,
    // location and the ADC file configure Google credentials.
    (
        "google-vertex",
        "Google Vertex AI",
        &["GOOGLE_VERTEX_API_KEY"],
    ),
    (
        "google-vertex-anthropic",
        "Google Vertex AI (Anthropic)",
        &[],
    ),
    // GITHUB_TOKEN, which models.dev lists, is usually set by git and CI
    // without Copilot access; GitHub's Copilot CLI reads this one.
    (
        "github-copilot",
        "GitHub Copilot",
        &["COPILOT_GITHUB_TOKEN"],
    ),
    // DATABRICKS_HOST, which models.dev lists too, only fills the URL.
    ("databricks", "Databricks", &["DATABRICKS_TOKEN"]),
    // models.dev also lists the account and gateway id variables, which
    // are not keys.
    (
        cloudflare::GATEWAY,
        "Cloudflare AI Gateway",
        &["CLOUDFLARE_API_TOKEN", "CF_AIG_TOKEN"],
    ),
    (
        cloudflare::WORKERS,
        "Cloudflare Workers AI",
        &[
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_WORKERS_AI_TOKEN",
            "CLOUDFLARE_API_TOKEN",
        ],
    ),
];

/// The provider config a plugin provider starts from.
pub fn builtin(id: &str) -> Option<crate::config::ProviderConfig> {
    let (_, name, env) = PROVIDERS.iter().find(|p| p.0 == id)?;
    Some(crate::config::ProviderConfig {
        name: Some(name.to_string()),
        env: env.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    })
}

/// The route of a model, when a provider plugin owns it.
pub fn route(input: &Input) -> anyhow::Result<Option<Route>> {
    if bedrock::owns(input) {
        return bedrock::route(input).map(Some);
    }
    if vertex::owns(input) {
        return vertex::route(input).map(Some);
    }
    if copilot::owns(input) {
        return copilot::route(input).map(Some);
    }
    match input.provider_id {
        gateways::DATABRICKS => return gateways::databricks(input).map(Some),
        cloudflare::GATEWAY => return cloudflare::gateway(input).map(Some),
        cloudflare::WORKERS => return cloudflare::workers(input).map(Some),
        _ => {}
    }
    Ok(None)
}

/// Whether a plugin provider is usable without a key (ambient AWS
/// configuration, …): OpenCode turns such providers on.
pub fn available(id: &str, settings: &Settings, env: &Env) -> bool {
    match id {
        "amazon-bedrock" => bedrock::available(settings, crate::auth::entry(id).as_ref(), env),
        copilot::ID => {
            env("COPILOT_GITHUB_TOKEN").is_some()
                || crate::auth::entry(id).is_some_and(|e| e["type"] == "oauth")
        }
        vertex::ID | vertex::ANTHROPIC_ID => {
            vertex::available(settings, crate::auth::entry(vertex::ID).as_ref(), env)
        }
        _ => false,
    }
}

/// The provider whose saved login a provider uses (Claude on Vertex shares
/// Vertex's).
pub fn login_id(id: &str) -> &str {
    match id {
        vertex::ANTHROPIC_ID => vertex::ID,
        id => id,
    }
}

/// The process environment (empty values unset).
pub fn process_env() -> Env {
    codesmith_ai::aws::process_env()
}

/// How a plugin provider without a key authenticates, for `auth list`.
pub fn describe_login(id: &str, stored: Option<&Value>, settings: &Settings) -> Option<String> {
    match id {
        bedrock::ID => {
            let env = process_env();
            if let Some(p) = bedrock::stored_profile(stored) {
                Some(format!("saved AWS profile {p}"))
            } else if let Some(p) = set(&settings.profile) {
                Some(format!("AWS profile {p}"))
            } else if codesmith_ai::aws::ambient(&env) {
                Some("AWS credential chain".into())
            } else {
                None
            }
        }
        copilot::ID => {
            let s = stored?;
            (s["type"] == "oauth").then(|| match s["metadata"]["enterpriseUrl"].as_str() {
                Some(e) => format!("saved GitHub login ({e})"),
                None => "saved GitHub login".to_string(),
            })
        }
        vertex::ID | vertex::ANTHROPIC_ID => {
            let env = process_env();
            let project = vertex::project(settings, stored, &env)?;
            let location = vertex::location(settings, stored, &env);
            let creds = if set(&settings.access_token).is_some() {
                "an access token".to_string()
            } else {
                match codesmith_ai::google::adc_file(&env) {
                    Some(f) => format!("ADC {}", f.display()),
                    None => "ADC (metadata server)".to_string(),
                }
            };
            Some(format!("project {project} in {location}, {creds}"))
        }
        _ => None,
    }
}

/// `${NAME}` in a catalog URL template replaced by `value(NAME)`; `None`
/// when one stays unresolved, names a variable a catalog may not read
/// ([`crate::catalog::url_variable`]) or the result is not a secure URL.
pub(crate) fn expand(template: &str, value: impl Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after.find('}')?;
        let name = &after[..end];
        if !crate::catalog::url_variable(name) {
            return None;
        }
        out.push_str(&value(name)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    crate::catalog::secure_url(&out).then_some(out)
}

/// A string setting, empty counting as unset.
pub(crate) fn set(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}
