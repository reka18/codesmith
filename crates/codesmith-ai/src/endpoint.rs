//! Where and how a request is sent (after OpenCode's route endpoint, auth
//! and `jsonPost`): the URL under the base URL with any query parameters,
//! the header that carries the key, the configured headers, and the body's
//! content encoding. Also the explicit per-model overrides of what the Chat
//! protocol otherwise detects from the endpoint and model id.

use crate::{Error, Protocol, Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Hooks around the HTTP requests (and Responses WebSocket frames) a
/// provider sends, run just before each request goes out and as each
/// response comes in: after the credentials are applied, so a hook sees and
/// may change what is actually sent. A request a hook leaves alone is sent
/// as it was built, body untouched. Responses and inbound frames come from
/// the network and are untrusted.
pub trait Interceptor: Send + Sync {
    /// Change the request (headers, URL, body) or leave it.
    fn request(&self, _req: &mut reqwest::Request) {}
    /// Look at, or replace, the response before it is read.
    fn response(&self, _res: &mut reqwest::Response) {}
    /// Change the URL or headers a WebSocket opens with (a change opens a
    /// new socket).
    fn ws_handshake(&self, _handshake: &mut crate::ws::Handshake) {}
    /// Change an outbound frame; the replacement is sent verbatim.
    fn ws_send(&self, _frame: &mut String) {}
    /// Change an inbound frame before it is decoded.
    fn ws_receive(&self, _frame: &mut String) {}
}

/// A provider's [`Interceptor`].
#[derive(Clone)]
pub struct Hooks(pub std::sync::Arc<dyn Interceptor>);

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hooks")
    }
}

/// How the API key is sent.
#[derive(Debug, Clone, Default)]
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
    /// The key as `Bearer <key>` in this header (Cloudflare AI Gateway's
    /// `cf-aig-authorization`); a configured `Authorization` header is not
    /// sent.
    BearerHeader(String),
    /// The key as `Authorization: Bearer` whatever the protocol (Bedrock
    /// API keys, GitHub Copilot's Messages route).
    Bearer,
    /// Credentials applied to each built request just before it is sent
    /// (AWS SigV4, Google ADC tokens, GitHub Copilot's account endpoint).
    /// The key is not sent.
    Dynamic(std::sync::Arc<dyn Authorizer>),
}

impl PartialEq for Auth {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Auth::Default, Auth::Default) | (Auth::Bearer, Auth::Bearer) => true,
            (Auth::Header(a), Auth::Header(b)) => a == b,
            (Auth::BearerHeader(a), Auth::BearerHeader(b)) => a == b,
            (Auth::Dynamic(a), Auth::Dynamic(b)) => std::sync::Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for Auth {}

/// Credentials that need the final request (its URL, headers and body) or
/// some I/O before it goes out: signing, a token exchange, an endpoint
/// lookup. Implementations cache what they can; they never log secrets.
pub trait Authorizer: Send + Sync + std::fmt::Debug {
    /// Add credentials to (or otherwise adjust) `req` before it is sent.
    fn authorize<'a>(
        &'a self,
        client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> futures_util::future::BoxFuture<'a, Result<(), Error>>;

    /// A short description for `codesmith models --verbose` style output.
    fn describe(&self) -> String {
        "dynamic credentials".into()
    }

    /// The provider rejected the credentials (401): true when they were
    /// renewed, so the request is sent once more.
    fn rejected<'a>(
        &'a self,
        _client: &'a reqwest::Client,
    ) -> futures_util::future::BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
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
    /// `false` lets a stream end without a finish reason, inferring one
    /// (detected: a finish reason is required).
    #[serde(alias = "require_finish_reason")]
    pub require_finish_reason: Option<bool>,
    /// The assistant message field reasoning is read from and written to
    /// (detected: whichever of `reasoning_content`, `reasoning`,
    /// `reasoning_text` the model streams). Setting it makes reasoning
    /// required unless `requireReasoning` says otherwise.
    #[serde(alias = "reasoning_field")]
    pub reasoning_field: Option<String>,
    /// Anthropic Messages: whether unsigned reasoning is dropped rather than
    /// replayed with an empty signature (detected: dropped, except on Kimi,
    /// MiMo, Qwen and MiniMax servers).
    #[serde(alias = "require_signature")]
    pub require_signature: Option<bool>,
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
        let key = self
            .api_key
            .as_deref()
            .filter(|_| !matches!(self.auth, Auth::Dynamic(_)));
        if let Some(k) = key {
            out.push(match (&self.auth, self.protocol) {
                (Auth::Header(h), _) => (h.clone(), k.to_string()),
                (Auth::BearerHeader(h), _) => (h.clone(), format!("Bearer {k}")),
                (Auth::Default, Protocol::AnthropicMessages) => ("x-api-key".into(), k.into()),
                (Auth::Default, Protocol::GoogleGemini) => ("x-goog-api-key".into(), k.into()),
                _ => ("authorization".into(), format!("Bearer {k}")),
            });
        }
        let custom = matches!(self.auth, Auth::Header(_) | Auth::BearerHeader(_));
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

impl Provider {
    /// The request `rb` builds, with any [`Auth::Dynamic`] credentials
    /// applied.
    pub(crate) async fn authorized(
        &self,
        rb: reqwest::RequestBuilder,
    ) -> Result<(reqwest::Client, reqwest::Request), Error> {
        let (client, req) = rb.build_split();
        let mut req = req?;
        if let Auth::Dynamic(a) = &self.auth {
            a.authorize(&client, &mut req).await?;
        }
        Ok((client, req))
    }
}

/// Send a request (with the provider's dynamic credentials) and fail with
/// the status and a bounded body on non-2xx.
pub(crate) async fn send(
    p: &Provider,
    rb: reqwest::RequestBuilder,
) -> Result<reqwest::Response, Error> {
    let (client, mut req) = p.authorized(rb).await?;
    let again = req.try_clone();
    if let Some(h) = &p.hooks {
        h.0.request(&mut req);
    }
    let mut resp = client.execute(req).await?;
    if let (Auth::Dynamic(a), Some(mut again)) = (&p.auth, again) {
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED && a.rejected(&client).await {
            a.authorize(&client, &mut again).await?;
            if let Some(h) = &p.hooks {
                h.0.request(&mut again);
            }
            resp = client.execute(again).await?;
        }
    }
    if let Some(h) = &p.hooks {
        h.0.response(&mut resp);
    }
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Status {
            status: status.as_u16(),
            body: crate::redact_urls(&text.chars().take(2000).collect::<String>()),
        });
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Hands out `token-<n>`, renewing when rejected.
    #[derive(Debug, Default)]
    struct Renewing(AtomicUsize);

    impl Authorizer for Renewing {
        fn authorize<'a>(
            &'a self,
            _client: &'a reqwest::Client,
            req: &'a mut reqwest::Request,
        ) -> futures_util::future::BoxFuture<'a, Result<(), Error>> {
            let n = self.0.load(Ordering::SeqCst);
            req.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer token-{n}").parse().unwrap(),
            );
            Box::pin(async { Ok(()) })
        }
        fn rejected<'a>(
            &'a self,
            _client: &'a reqwest::Client,
        ) -> futures_util::future::BoxFuture<'a, bool> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { true })
        }
    }

    /// Answers 401 until it sees `token-1`.
    async fn server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let auth = req
                    .lines()
                    .find_map(|l| l.strip_prefix("authorization: ").map(str::to_string))
                    .unwrap_or_default();
                log.lock().unwrap().push(auth.clone());
                let status = if auth == "bearer token-1" { 200 } else { 401 };
                let _ = s
                    .write_all(
                        format!("HTTP/1.1 {status} X\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}")
                            .as_bytes(),
                    )
                    .await;
            }
        });
        (url, seen)
    }

    #[tokio::test]
    async fn a_rejected_dynamic_credential_is_renewed_and_the_request_sent_once_more() {
        let (url, seen) = server().await;
        let p = Provider {
            protocol: Protocol::OpenaiChat,
            name: String::new(),
            base_url: url,
            api_key: None,
            headers: vec![],
            query: vec![],
            auth: Auth::Dynamic(std::sync::Arc::new(Renewing::default())),
            encoding: Encoding::None,
            compat: Compatibility::default(),
            hooks: None,
        };
        let client = reqwest::Client::new();
        let resp = send(&p, client.post(p.url("x")).body("{}")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(*seen.lock().unwrap(), ["bearer token-0", "bearer token-1"]);
        // Rejected again after the renewal: no third try.
        let (url, seen) = server().await;
        let p = Provider {
            base_url: url,
            auth: Auth::Dynamic(std::sync::Arc::new(Renewing(AtomicUsize::new(5)))),
            ..p
        };
        let e = send(&p, client.post(p.url("x")).body("{}"))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Status { status: 401, .. }), "{e:?}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
