//! Small provider plugins (after OpenCode's Cerebras, Kilo, LLM Gateway,
//! NVIDIA, ZenMux, OpenRouter and Databricks plugins): the integration
//! headers some gateways ask clients to send, and Databricks' workspace
//! host in its base URL.
//!
//! The headers name CodeSmith, not OpenCode: they attribute traffic to the
//! client that sends it.

use super::{Input, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{Auth, Protocol};

const REFERER: &str = "https://github.com/reka18/codesmith";
const TITLE: &str = "codesmith";

/// The integration headers for a provider's endpoint, as `(name, value)`;
/// configured headers of the same name win.
pub fn headers(provider_id: &str, base_url: &str) -> Vec<(&'static str, &'static str)> {
    let url = base_url.trim_end_matches('/');
    let referer = [("HTTP-Referer", REFERER), ("X-Title", TITLE)];
    if provider_id == "cerebras" || url.starts_with("https://api.cerebras.ai") {
        return vec![("X-Cerebras-3rd-Party-Integration", TITLE)];
    }
    match url {
        "https://api.kilo.ai/api/gateway" | "https://zenmux.ai/api/v1" => referer.to_vec(),
        "https://api.llmgateway.io/v1" => [referer.as_slice(), &[("X-Source", TITLE)]].concat(),
        "https://integrate.api.nvidia.com/v1" => [
            referer.as_slice(),
            &[("X-BILLING-INVOKE-ORIGIN", "CodeSmith")],
        ]
        .concat(),
        u if u.starts_with("https://openrouter.ai/api") => referer.to_vec(),
        _ => vec![],
    }
}

/// Add the integration headers under the configured ones.
pub fn apply(provider_id: &str, base_url: &str, headers_out: &mut Vec<(String, String)>) {
    for (k, v) in headers(provider_id, base_url) {
        if !headers_out.iter().any(|(h, _)| h.eq_ignore_ascii_case(k)) {
            headers_out.push((k.to_string(), v.to_string()));
        }
    }
}

pub const DATABRICKS: &str = "databricks";

/// Databricks fills `${DATABRICKS_HOST}` (given with or without a scheme)
/// in its catalog URL.
pub fn databricks(input: &Input) -> Result<Route> {
    let host = (input.env)("DATABRICKS_HOST").map(|h| {
        let h = h.trim();
        let h = h
            .strip_prefix("https://")
            .or_else(|| h.strip_prefix("http://"))
            .or_else(|| h.strip_prefix("HTTPS://"))
            .unwrap_or(h);
        h.trim_end_matches('/').to_string()
    });
    let template = input
        .catalog_url
        .unwrap_or("https://${DATABRICKS_HOST}/ai-gateway/mlflow/v1");
    let base_url = match input.configured_url {
        Some(u) => u.to_string(),
        None => super::expand(template, |name| match name {
            "DATABRICKS_HOST" => host.clone(),
            n => (input.env)(n),
        })
        .ok_or_else(|| {
            anyhow!("Databricks needs its workspace host: set DATABRICKS_HOST or settings.baseURL")
        })?,
    };
    Ok(Route {
        protocol: input.configured_protocol.unwrap_or(Protocol::OpenaiChat),
        base_url,
        auth: Auth::Default,
        api_key: input.api_key.clone(),
        headers: vec![],
        model_id: None,
    })
}
