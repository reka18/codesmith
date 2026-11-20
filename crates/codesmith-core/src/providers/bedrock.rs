//! Amazon Bedrock (after OpenCode's `AmazonBedrockPlugin` and the
//! `amazon-bedrock` packages): Converse on `bedrock-runtime`, or the
//! OpenAI- and Anthropic-compatible Mantle APIs on `bedrock-mantle`, each
//! with a Bedrock API key (`AWS_BEARER_TOKEN_BEDROCK`, sent as a bearer
//! token) or SigV4 over the AWS credential chain, in the configured region.
//!
//! Model ids are sent as given, so cross-region inference profiles
//! (`us.`, `eu.`, `apac.`, `global.` prefixes) and inference profile ARNs
//! work like plain model ids; the catalog lists the profile ids for models
//! that need one.

use super::{set, Env, Input, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{aws, Auth, Protocol};
use serde_json::Value;
use std::sync::Arc;

pub const ID: &str = "amazon-bedrock";
const PACKAGE: &str = "@opencode/ai/providers/amazon-bedrock";
/// The saved login method that names an AWS profile.
pub const PROFILE_METHOD: &str = "aws-profile";

/// Whether this plugin routes the model: the Bedrock provider, or a model
/// whose catalog package is one of Bedrock's (Mantle too).
pub fn owns(input: &Input) -> bool {
    input.provider_id == ID || input.package.is_some_and(|p| p.starts_with(PACKAGE))
}

/// The profile of a saved AWS profile login.
pub fn stored_profile(stored: Option<&Value>) -> Option<String> {
    let s = stored?;
    (s["type"] == "external" && s["method"] == PROFILE_METHOD)
        .then(|| s["metadata"]["profile"].as_str().map(str::to_string))
        .flatten()
        .filter(|p| !p.is_empty())
}

/// Save an AWS profile login (OpenCode's `aws-profile` method).
pub fn save_profile(file: &std::path::Path, profile: &str) -> Result<()> {
    crate::auth::set_entry_in(
        file,
        ID,
        serde_json::json!({"type": "external", "method": PROFILE_METHOD, "metadata": {"profile": profile}}),
    )
}

/// Whether the provider is usable without a key: a configured or saved
/// profile, or ambient AWS configuration.
pub fn available(settings: &super::Settings, stored: Option<&Value>, env: &Env) -> bool {
    set(&settings.profile).is_some() || stored_profile(stored).is_some() || aws::ambient(env)
}

/// Which Bedrock API a package (or, without one, the provider) speaks.
fn api(package: Option<&str>) -> (Protocol, &'static str) {
    match package.and_then(|p| p.strip_prefix(PACKAGE)) {
        Some("/mantle/responses" | "/mantle") => (Protocol::OpenaiResponses, "responses"),
        Some("/mantle/chat") => (Protocol::OpenaiChat, "chat"),
        Some("/mantle/messages") => (Protocol::AnthropicMessages, "messages"),
        _ => (Protocol::BedrockConverse, "converse"),
    }
}

pub fn route(input: &Input) -> Result<Route> {
    let s = &input.settings.plugin;
    let region = aws::region(set(&s.region), &input.env);
    let (detected, api) = api(input.package);
    let protocol = input.configured_protocol.unwrap_or(detected);
    let mantle = api != "converse" || protocol != Protocol::BedrockConverse;
    // `endpoint` is OpenCode's spelling of a private endpoint; `baseURL`
    // beats it. Catalog URLs carry `${AWS_REGION}`, which the provider's
    // region fills.
    let catalog = input.catalog_url.and_then(|u| {
        super::expand(u, |name| match name {
            "AWS_REGION" => Some(region.clone()),
            n => (input.env)(n),
        })
    });
    let base_url = input
        .configured_url
        .or(set(&s.endpoint))
        .map(str::to_string)
        .or(catalog)
        .unwrap_or_else(|| match (mantle, protocol) {
            (false, _) => format!("https://bedrock-runtime.{region}.amazonaws.com"),
            (true, Protocol::AnthropicMessages) => {
                format!("https://bedrock-mantle.{region}.api.aws/anthropic/v1")
            }
            (true, _) => format!("https://bedrock-mantle.{region}.api.aws/v1"),
        });
    let configured_key = set(&input.settings.api_key).map(str::to_string);
    let profile = set(&s.profile)
        .map(str::to_string)
        .or_else(|| stored_profile(input.stored.as_ref()));
    // A saved profile login signs with that profile and ignores an ambient
    // bearer token (OpenCode's `auth: "sigv4"` for external logins).
    let mode = set(&s.auth_mode)
        .map(str::to_ascii_lowercase)
        .or_else(|| stored_profile(input.stored.as_ref()).map(|_| "sigv4".into()));
    let key = match mode.as_deref() {
        Some("sigv4") => {
            if configured_key.is_some() {
                return Err(anyhow!("Amazon Bedrock SigV4 auth does not accept apiKey"));
            }
            None
        }
        Some("bearer") => Some(input.api_key.clone().ok_or_else(|| {
            anyhow!("Amazon Bedrock bearer auth requires apiKey (or AWS_BEARER_TOKEN_BEDROCK)")
        })?),
        Some(m) => {
            return Err(anyhow!(
                "provider {:?} has an invalid auth {m:?}; use bearer or sigv4",
                input.provider_id
            ))
        }
        None => input.api_key.clone(),
    };
    let auth = match &key {
        // A Bedrock API key wins over SigV4, as in the AWS SDK.
        Some(_) => Auth::Bearer,
        None => Auth::Dynamic(Arc::new(aws::SigV4::with_env(
            if mantle { "bedrock-mantle" } else { "bedrock" },
            &region,
            aws::Source::Chain { profile },
            input.env.clone(),
        ))),
    };
    Ok(Route {
        protocol,
        base_url,
        auth,
        api_key: key,
        headers: vec![],
        model_id: None,
    })
}
