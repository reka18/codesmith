//! Where and how a request is sent (after OpenCode's route endpoint, auth
//! and `jsonPost`): the URL under the base URL with any query parameters,
//! the header that carries the key, the configured headers, and the body's
//! content encoding. Also the explicit per-model overrides of what the Chat
//! protocol otherwise detects from the endpoint and model id.

use crate::{Error, Protocol, Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How the API key is sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Auth {
    /// The protocol's own header: `Authorization: Bearer` (OpenAI Chat and
    /// Responses), `x-api-key` (Anthropic Messages), `x-goog-api-key`
    /// (Gemini).
    #[default]
    Default,
    /// The key as-is in this header (Azure's `api-key`). A configured
    /// `Authorization` header is not sent, so a stale bearer token cannot
    /// ride along.
    Header(String),
}

/// The request body's content encoding (OpenCode's `RequestEncoding`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// Plain JSON.
    #[default]
    None,
    Gzip,
    Zstd,
}

impl Encoding {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "" => Some(Encoding::None),
            "gzip" => Some(Encoding::Gzip),
            "zstd" => Some(Encoding::Zstd),
            _ => None,
        }
    }

    /// The compressed body and its `content-encoding`, or `None` for plain
    /// JSON. Compression is an optimization: if it fails, the body goes
    /// uncompressed rather than failing the request.
    fn compress(self, body: &[u8]) -> Option<(&'static str, Vec<u8>)> {
        use std::io::Write;
        match self {
            Encoding::None => None,
            Encoding::Gzip => {
                let mut e =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(body).ok()?;
                Some(("gzip", e.finish().ok()?))
            }
            Encoding::Zstd => Some(("zstd", zstd::bulk::compress(body, 0).ok()?)),
        }
    }
}

/// Explicit overrides of what OpenAI Chat detects from the endpoint and
/// model id (OpenCode's model `compatibility` and `capabilities.streamUsage`).
/// Unset fields keep the detected behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Compatibility {
    /// The field the output token limit goes in: `max_tokens` or
    /// `max_completion_tokens` (detected: the latter on OpenAI and Azure).
    #[serde(alias = "max_tokens_field")]
    pub max_tokens_field: Option<String>,
    /// `false` leaves out `stream_options.include_usage`, for servers that
    /// reject it (detected: always sent).
    #[serde(alias = "stream_usage")]
    pub stream_usage: Option<bool>,
    /// Whether every assistant message carries a reasoning field, empty when
    /// there was none (detected: DeepSeek).
    #[serde(alias = "require_reasoning")]
    pub require_reasoning: Option<bool>,
    /// Whether an assistant turn goes between tool results and the next user
    /// content (detected: Mistral-family models).
    #[serde(alias = "require_assistant_after_tool")]
    pub require_assistant_after_tool: Option<bool>,
}

/// Azure OpenAI's base URL and query parameters for `model` (after OpenCode's
/// Azure provider): on a `*.openai.azure.com` resource, the v1 API under
/// `<base>/v1` with `api-version` (default `v1`); with deployment-based URLs,
/// `<base>/deployments/<model>`; any other host (a proxy) as given, with only
/// the configured query parameters. Configured parameters win over
/// `api-version`.
pub fn azure_endpoint(
    base_url: &str,
    model: &str,
    api_version: Option<&str>,
    query: &[(String, String)],
    deployments: bool,
) -> (String, Vec<(String, String)>) {
    let base = base_url.trim().trim_end_matches('/');
    let mut q = vec![(
        "api-version".to_string(),
        api_version.unwrap_or("v1").to_string(),
    )];
    for (k, v) in query {
        q.retain(|(qk, _)| qk != k);
        q.push((k.clone(), v.clone()));
    }
    if deployments {
        return (format!("{base}/deployments/{model}"), q);
    }
    let azure_host = reqwest::Url::parse(base)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h.ends_with(".openai.azure.com"));
    if !azure_host {
        return (base.to_string(), query.to_vec());
    }
    (format!("{base}/v1"), q)
}

/// Azure's default base URL for a resource name.
pub fn azure_resource_url(resource: &str, protocol: Protocol) -> String {
    let r = resource.trim();
    match protocol {
        // Claude on Foundry serves Messages under the resource's AI services host.
        Protocol::AnthropicMessages => format!("https://{r}.services.ai.azure.com/anthropic/v1"),
        _ => format!("https://{r}.openai.azure.com/openai"),
    }
}

impl Provider {
    /// The URL for `path` under the base URL (query parameters are added
    /// when the request is built).
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url.trim_end_matches('/'))
    }

    /// The headers that carry the key, then the given (protocol and
    /// configured) headers.
    pub(crate) fn auth_headers(&self, headers: Vec<(String, String)>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(k) = self.api_key.as_deref() {
            out.push(match (&self.auth, self.protocol) {
                (Auth::Header(h), _) => (h.clone(), k.to_string()),
                (Auth::Default, Protocol::AnthropicMessages) => ("x-api-key".into(), k.into()),
                (Auth::Default, Protocol::GoogleGemini) => ("x-goog-api-key".into(), k.into()),
                (Auth::Default, _) => ("authorization".into(), format!("Bearer {k}")),
            });
        }
        let custom = matches!(self.auth, Auth::Header(_));
        out.extend(
            headers
                .into_iter()
                .filter(|(h, _)| !(custom && h.eq_ignore_ascii_case("authorization"))),
        );
        out
    }

    /// A GET of `path`, with query parameters, auth and configured headers.
    pub(crate) fn get(&self, client: &reqwest::Client, path: &str) -> reqwest::RequestBuilder {
        let mut rb = client.get(self.url(path));
        if !self.query.is_empty() {
            rb = rb.query(&self.query);
        }
        let mut extra = self.headers.clone();
        if self.protocol == Protocol::AnthropicMessages {
            extra.insert(
                0,
                ("anthropic-version".into(), crate::anthropic::VERSION.into()),
            );
        }
        for (k, v) in self.auth_headers(extra) {
            rb = rb.header(k, v);
        }
        rb
    }

    /// A JSON POST of `body` to `path`, with query parameters, auth, the
    /// given headers (which include the configured ones) and the configured
    /// body encoding.
    pub(crate) fn json_post(
        &self,
        client: &reqwest::Client,
        path: &str,
        headers: Vec<(String, String)>,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        let mut rb = client.post(self.url(path));
        if !self.query.is_empty() {
            rb = rb.query(&self.query);
        }
        for (k, v) in self.auth_headers(headers) {
            rb = rb.header(k, v);
        }
        let json = serde_json::to_vec(body).unwrap_or_default();
        rb = rb.header(reqwest::header::CONTENT_TYPE, "application/json");
        match self.encoding.compress(&json) {
            Some((name, bytes)) => rb
                .header(reqwest::header::CONTENT_ENCODING, name)
                .body(bytes),
            None => rb.body(json),
        }
    }
}

/// Send a request and fail with the status and a bounded body on non-2xx.
pub(crate) async fn send(rb: reqwest::RequestBuilder) -> Result<reqwest::Response, Error> {
    let resp = rb.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Status {
            status: status.as_u16(),
            body: text.chars().take(2000).collect(),
        });
    }
    Ok(resp)
}
