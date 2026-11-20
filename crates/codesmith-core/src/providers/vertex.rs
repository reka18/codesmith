//! Google Vertex AI (after OpenCode's `GoogleVertexPlugin` and the
//! `google-vertex` packages): Gemini through `generateContent`, Claude
//! through Anthropic Messages (`streamRawPredict`), and the
//! OpenAI-compatible endpoints the catalog lists, in a project and location,
//! authenticated with Application Default Credentials (or an access token,
//! or for Gemini an Express Mode API key).

use super::{set, Env, Input, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{google, Auth, Protocol};
use serde_json::Value;
use std::sync::Arc;

pub const ID: &str = "google-vertex";
/// The older provider id for Claude on Vertex.
pub const ANTHROPIC_ID: &str = "google-vertex-anthropic";
const PACKAGE: &str = "@opencode/ai/providers/google-vertex";
/// The saved login method that names a project and location.
pub const ADC_METHOD: &str = "google-adc";

pub fn owns(input: &Input) -> bool {
    input.provider_id == ID
        || input.provider_id == ANTHROPIC_ID
        || input.package.is_some_and(|p| p.starts_with(PACKAGE))
}

fn stored(stored: Option<&Value>, key: &str) -> Option<String> {
    let s = stored?;
    (s["type"] == "external" && s["method"] == ADC_METHOD)
        .then(|| s["metadata"][key].as_str().map(str::to_string))
        .flatten()
        .filter(|v| !v.is_empty())
}

/// The project: a saved login's, the configured one, then
/// `GOOGLE_VERTEX_PROJECT`, `GOOGLE_CLOUD_PROJECT`, `GCP_PROJECT`,
/// `GCLOUD_PROJECT`.
pub fn project(settings: &super::Settings, login: Option<&Value>, env: &Env) -> Option<String> {
    stored(login, "project")
        .or_else(|| set(&settings.project).map(str::to_string))
        .or_else(|| {
            [
                "GOOGLE_VERTEX_PROJECT",
                "GOOGLE_CLOUD_PROJECT",
                "GCP_PROJECT",
                "GCLOUD_PROJECT",
            ]
            .iter()
            .find_map(|k| env(k))
        })
}

/// The location: a saved login's, the configured one, then
/// `GOOGLE_VERTEX_LOCATION`, `GOOGLE_CLOUD_LOCATION`, `VERTEX_LOCATION`,
/// then `global`.
pub fn location(settings: &super::Settings, login: Option<&Value>, env: &Env) -> String {
    stored(login, "location")
        .or_else(|| set(&settings.location).map(str::to_string))
        .or_else(|| {
            [
                "GOOGLE_VERTEX_LOCATION",
                "GOOGLE_CLOUD_LOCATION",
                "VERTEX_LOCATION",
            ]
            .iter()
            .find_map(|k| env(k))
        })
        .unwrap_or_else(|| "global".into())
}

/// The API host of a location: the global host, the Regional Endpoint
/// Platform for the `eu` and `us` multi-regions, else the regional one.
pub fn host(location: &str) -> String {
    match location {
        "global" => "aiplatform.googleapis.com".into(),
        "eu" | "us" => format!("aiplatform.{location}.rep.googleapis.com"),
        l => format!("{l}-aiplatform.googleapis.com"),
    }
}

/// A catalog URL template with the project, location and host filled in.
pub fn expand(template: &str, project: Option<&str>, location: &str, env: &Env) -> Option<String> {
    super::expand(template, |name| match name {
        "GOOGLE_VERTEX_PROJECT" => project.map(str::to_string),
        "GOOGLE_VERTEX_LOCATION" => Some(location.to_string()),
        "GOOGLE_VERTEX_ENDPOINT" => Some(host(location)),
        n => env(n),
    })
}

/// Whether the provider is usable: a project resolves (Vertex
/// authenticates through ADC rather than a key).
pub fn available(settings: &super::Settings, login: Option<&Value>, env: &Env) -> bool {
    project(settings, login, env).is_some()
}

fn api(input: &Input) -> Protocol {
    match input.package.and_then(|p| p.strip_prefix(PACKAGE)) {
        Some("/messages") => Protocol::AnthropicMessages,
        Some("/chat" | "/mistral") => Protocol::OpenaiChat,
        Some("/responses") => Protocol::OpenaiResponses,
        Some(_) => Protocol::GoogleGemini,
        None if input.provider_id == ANTHROPIC_ID || input.model_id.starts_with("claude") => {
            Protocol::AnthropicMessages
        }
        None => Protocol::GoogleGemini,
    }
}

/// One ADC source per process, so its token cache outlives a model
/// resolution.
fn adc(env: &Env) -> Arc<google::Adc> {
    static SHARED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<Option<std::path::PathBuf>, Arc<google::Adc>>>,
    > = std::sync::OnceLock::new();
    let key = google::adc_file(env);
    SHARED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| Arc::new(google::Adc::new(env.clone())))
        .clone()
}

pub fn route(input: &Input) -> Result<Route> {
    let s = &input.settings.plugin;
    let protocol = input.configured_protocol.unwrap_or_else(|| api(input));
    let project = project(s, input.stored.as_ref(), &input.env);
    let location = location(s, input.stored.as_ref(), &input.env);
    // As in Google's SDK, a configured project or location selects Google
    // credentials, so an ambient Express Mode key cannot replace them.
    let explicit = set(&s.project).is_some() || set(&s.location).is_some();
    let key = set(&input.settings.api_key)
        .map(str::to_string)
        .or_else(|| input.api_key.clone().filter(|_| !explicit));
    let token = set(&s.access_token).map(str::to_string);
    if key.is_some() && token.is_some() {
        return Err(anyhow!(
            "Google Vertex apiKey cannot be combined with accessToken"
        ));
    }
    let gemini = protocol == Protocol::GoogleGemini;
    let endpoint_model = input.model_id.starts_with("endpoints/");
    let key = key.filter(|_| gemini && !endpoint_model);
    let need_project = || {
        project.clone().ok_or_else(|| {
            anyhow!(
                "Google Vertex requires a project: set settings.project, GOOGLE_VERTEX_PROJECT or GOOGLE_CLOUD_PROJECT, or run `codesmith auth login google-vertex --project <id>`"
            )
        })
    };
    let host = host(&location);
    let base_url = match (input.configured_url, input.catalog_url) {
        (Some(u), _) => u.to_string(),
        (None, Some(t)) => expand(t, project.as_deref(), &location, &input.env).ok_or_else(
            || match need_project() {
                Err(e) => e,
                Ok(_) => anyhow!("unresolved variables in the catalog URL {t}"),
            },
        )?,
        (None, None) => match protocol {
            Protocol::GoogleGemini if key.is_some() => {
                "https://aiplatform.googleapis.com/v1/publishers/google".to_string()
            }
            Protocol::GoogleGemini => format!(
                "https://{host}/v1beta1/projects/{}/locations/{location}{}",
                need_project()?,
                if endpoint_model {
                    ""
                } else {
                    "/publishers/google"
                }
            ),
            Protocol::AnthropicMessages => format!(
                "https://{host}/v1/projects/{}/locations/{location}/publishers/anthropic/models",
                need_project()?
            ),
            _ => format!(
                "https://{host}/v1/projects/{}/locations/{location}/endpoints/openapi",
                need_project()?
            ),
        },
    };
    let (auth, api_key) = match (key, token) {
        (Some(k), _) => (Auth::Default, Some(k)),
        (None, Some(t)) => (Auth::Bearer, Some(t)),
        (None, None) => (Auth::Dynamic(adc(&input.env)), None),
    };
    Ok(Route {
        protocol,
        base_url,
        auth,
        api_key,
        headers: vec![],
    })
}

/// Save a project (and location) login (OpenCode's `google-adc` method).
pub fn save_login(file: &std::path::Path, project: &str, location: Option<&str>) -> Result<()> {
    let mut metadata = serde_json::json!({"project": project});
    if let Some(l) = location {
        metadata["location"] = l.into();
    }
    crate::auth::set_entry_in(
        file,
        ID,
        serde_json::json!({"type": "external", "method": ADC_METHOD, "metadata": metadata}),
    )
}

/// Projects to suggest: gcloud's configured ones, then the ADC file's quota
/// project (read only; nothing contacts Google).
pub fn suggested_projects(env: &Env) -> Vec<String> {
    let dir = google::gcloud_dir(env);
    let active = std::fs::read_to_string(dir.join("active_config"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into());
    let mut configs: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join("configurations"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("config_"))
        })
        .collect();
    let active = format!("config_{active}");
    configs.sort_by_key(|p| {
        (
            p.file_name().and_then(|n| n.to_str()) != Some(active.as_str()),
            p.clone(),
        )
    });
    let mut out: Vec<String> = Vec::new();
    for c in configs {
        let text = std::fs::read_to_string(c).unwrap_or_default();
        if let Some(p) = text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "project").then(|| v.trim().to_string())
        }) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    if let Some(q) = google::quota_project(env) {
        if !out.contains(&q) {
            out.push(q);
        }
    }
    out
}
