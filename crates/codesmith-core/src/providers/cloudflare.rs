//! Cloudflare AI Gateway and Workers AI (after OpenCode's
//! `CloudflareAIGatewayPlugin`, `CloudflareWorkersAIPlugin` and the
//! `cloudflare-ai-gateway` and `cloudflare-workers-ai` packages).
//!
//! Both need the Cloudflare account id, and the gateway its gateway id:
//! from the saved login (`codesmith auth login` asks for them beside the
//! token), else `settings.accountId` / `settings.gatewayId`, else
//! `CLOUDFLARE_ACCOUNT_ID` / `CLOUDFLARE_GATEWAY_ID`. A configured
//! `baseURL` is used as is.
//!
//! AI Gateway sends `openai/…` models to the gateway's OpenAI Responses
//! passthrough and `anthropic/…` models to its Anthropic Messages
//! passthrough (the REST API rejects parts of those request shapes), with
//! the token in `cf-aig-authorization`; every other model goes to the
//! account's OpenAI-compatible `/ai/v1` endpoint with the gateway named in
//! `cf-aig-gateway-id`. Workers AI is that `/ai/v1` endpoint with the
//! token as a bearer token.

use super::{set, stored_metadata, Field, Input, KeyForm, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{Auth, Protocol};

pub const GATEWAY: &str = "cloudflare-ai-gateway";
pub const WORKERS: &str = "cloudflare-workers-ai";

fn account_field() -> Field {
    Field {
        key: "accountId",
        title: "Enter your Cloudflare Account ID",
        placeholder: "e.g. 1234567890abcdef1234567890abcdef",
        required: true,
    }
}

fn gateway_field() -> Field {
    Field {
        key: "gatewayId",
        title: "Enter your Cloudflare AI Gateway ID",
        placeholder: "e.g. my-gateway",
        required: true,
    }
}

/// The key login of AI Gateway, asking for the ids the environment and
/// config do not give.
pub fn gateway_form(settings: &crate::config::ProviderSettings, env: &super::Env) -> KeyForm {
    let mut fields = vec![];
    if settings.base_url.is_none() {
        if env("CLOUDFLARE_ACCOUNT_ID").is_none() && set(&settings.plugin.account_id).is_none() {
            fields.push(account_field());
        }
        if env("CLOUDFLARE_GATEWAY_ID").is_none() && set(&settings.plugin.gateway_id).is_none() {
            fields.push(gateway_field());
        }
    }
    KeyForm {
        label: "Gateway API token",
        fields,
    }
}

/// The key login of Workers AI, asking for the account id unless it is
/// known.
pub fn workers_form(settings: &crate::config::ProviderSettings, env: &super::Env) -> KeyForm {
    let known = settings.base_url.is_some()
        || set(&settings.plugin.account_id).is_some()
        || env("CLOUDFLARE_ACCOUNT_ID").is_some();
    KeyForm {
        label: "API key",
        fields: if known { vec![] } else { vec![account_field()] },
    }
}

/// A saved login's answer, then the setting, then the variable.
fn pick(input: &Input, key: &str, setting: &Option<String>, var: &str) -> Option<String> {
    stored_metadata(input.stored.as_ref(), key)
        .or_else(|| set(setting).map(str::to_string))
        .or_else(|| (input.env)(var))
}

/// The account and gateway ids a model of AI Gateway uses.
pub fn gateway_ids(input: &Input) -> (Option<String>, Option<String>) {
    let s = &input.settings.plugin;
    (
        pick(input, "accountId", &s.account_id, "CLOUDFLARE_ACCOUNT_ID"),
        pick(input, "gatewayId", &s.gateway_id, "CLOUDFLARE_GATEWAY_ID"),
    )
}

/// The account a model of Workers AI uses.
pub fn workers_account(input: &Input) -> Option<String> {
    pick(
        input,
        "accountId",
        &input.settings.plugin.account_id,
        "CLOUDFLARE_ACCOUNT_ID",
    )
}

/// The account's OpenAI-compatible endpoint.
pub fn rest_url(account: &str) -> String {
    format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1",
        super::encode(account)
    )
}

fn need_account(provider: &str) -> anyhow::Error {
    anyhow!(
        "{provider} needs a Cloudflare account id: run `codesmith auth login {provider}`, \
         or set settings.accountId or CLOUDFLARE_ACCOUNT_ID (or settings.baseURL)"
    )
}

pub fn gateway(input: &Input) -> Result<Route> {
    let (account, gateway) = gateway_ids(input);
    let gateway = gateway
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| "default".to_string());
    let model = input.model_id;
    let route = |protocol, base_url: String, auth, headers, model_id| Route {
        protocol,
        base_url,
        auth,
        api_key: input.api_key.clone(),
        headers,
        model_id,
    };
    // A custom endpoint stands in for the REST API: every model keeps it,
    // with its gateway model id.
    if let Some(url) = input.configured_url {
        let protocol = input.configured_protocol.unwrap_or_else(|| {
            if model.starts_with("openai/") {
                Protocol::OpenaiResponses
            } else if model.starts_with("anthropic/") {
                Protocol::AnthropicMessages
            } else {
                Protocol::OpenaiChat
            }
        });
        let headers = vec![("cf-aig-gateway-id".to_string(), gateway)];
        return Ok(route(
            protocol,
            url.to_string(),
            Auth::Bearer,
            headers,
            None,
        ));
    }
    let account = account.ok_or_else(|| need_account(GATEWAY))?;
    let passthrough = |api: &str| {
        format!(
            "https://gateway.ai.cloudflare.com/v1/{}/{}/{api}",
            super::encode(&account),
            super::encode(&gateway)
        )
    };
    let token = || Auth::BearerHeader("cf-aig-authorization".into());
    Ok(if let Some(m) = model.strip_prefix("openai/") {
        route(
            Protocol::OpenaiResponses,
            passthrough("openai"),
            token(),
            vec![],
            Some(m.to_string()),
        )
    } else if let Some(m) = model.strip_prefix("anthropic/") {
        route(
            Protocol::AnthropicMessages,
            passthrough("anthropic/v1"),
            token(),
            vec![],
            Some(m.replace('.', "-")),
        )
    } else {
        route(
            Protocol::OpenaiChat,
            rest_url(&account),
            Auth::Bearer,
            vec![("cf-aig-gateway-id".to_string(), gateway)],
            model.strip_prefix("workers-ai/").map(str::to_string),
        )
    })
}

pub fn workers(input: &Input) -> Result<Route> {
    let base_url = match input.configured_url {
        Some(u) => u.to_string(),
        None => rest_url(&workers_account(input).ok_or_else(|| need_account(WORKERS))?),
    };
    Ok(Route {
        protocol: input.configured_protocol.unwrap_or(Protocol::OpenaiChat),
        base_url,
        auth: Auth::Bearer,
        api_key: input.api_key.clone(),
        headers: vec![],
        model_id: None,
    })
}
