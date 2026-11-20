//! The OpenAI Responses WebSocket transport (after OpenCode's
//! `route/transport/websocket.ts`, `open-responses-channel.ts` and the
//! session's `model-transport.ts`).
//!
//! A [`SessionTransport`] belongs to one conversation. It keeps one socket
//! open across the conversation's requests, sends each request as a
//! `response.create` message, and once a response completes sends only the
//! new input items of the next request with `previous_response_id` (see
//! [`continuation`]). Anything that keeps the socket from working falls back
//! to the ordinary HTTP request: a failed connect or send, an endpoint
//! without WebSocket support, a message too large for the socket, or a
//! socket that keeps failing mid-response.

pub mod continuation;
mod session;

pub use continuation::{Checkpoint, Create, Driver, Mode, Observation, Shape};
pub use session::SessionTransport;

use crate::{Error, Provider, Request};
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// The OpenAI Responses WebSocket protocol version, sent as `OpenAI-Beta`
/// unless the provider sets its own.
pub const PROTOCOL_HEADER: &str = "responses_websockets=2026-02-06";
/// The largest message accepted from the socket.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Where a WebSocket failure happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Prepare,
    Connect,
    Queue,
    Send,
    Receive,
    Close,
}

/// How far a request got before its exchange failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Nothing reached the provider; the request can go over HTTP.
    NotSent,
    /// The request may or may not have reached the provider.
    Ambiguous,
    /// The provider answered part of it.
    Accepted,
    /// The provider refused it.
    Rejected,
}

/// How to recover from a refused continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Send the request in full.
    RetryFull,
    /// Send it in full on a new connection.
    RotateAndRetryFull,
}

/// A WebSocket transport failure.
#[derive(Debug, Clone, PartialEq)]
pub struct WsError {
    pub message: String,
    /// A system error code (`ECONNREFUSED`), close code (`1009`) or one of
    /// the transport's own (`closed`, `message-too-large`, `idle-timeout`).
    pub code: Option<String>,
    pub phase: Phase,
    pub delivery: Option<Delivery>,
    pub url: Option<String>,
}

impl WsError {
    pub fn new(message: impl Into<String>, phase: Phase) -> Self {
        WsError {
            message: message.into(),
            code: None,
            phase,
            delivery: None,
            url: None,
        }
    }
    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    pub fn delivery(mut self, d: Delivery) -> Self {
        self.delivery = Some(d);
        self
    }
    fn at(mut self, url: &str) -> Self {
        self.url = Some(url.to_string());
        self
    }
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<WsError> for Error {
    fn from(e: WsError) -> Self {
        Error::Transport(crate::redact_urls(&format!("WebSocket: {}", e.message)))
    }
}

/// One inbound message.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
}

/// The URL and headers a connection opens with.
#[derive(Debug, Clone, PartialEq)]
pub struct Handshake {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// The sending half of a connection.
pub trait Outbound: Send {
    fn send_text(&mut self, text: String) -> BoxFuture<'_, Result<(), WsError>>;
    fn close(&mut self) -> BoxFuture<'_, ()>;
}

/// An open connection: its sending half and every inbound message, in
/// order, until it closes (a close or failure ends with an error).
pub struct Connection {
    pub outbound: Box<dyn Outbound>,
    /// Bounded (see [`INBOUND_FRAMES`]): a reader ahead of the consumer
    /// waits instead of queueing frames of up to 16 MiB without end.
    pub inbound: mpsc::Receiver<Result<Frame, WsError>>,
}

/// How many inbound frames a connection holds before its reader waits.
pub const INBOUND_FRAMES: usize = 16;

/// Opens connections.
pub trait Connector: Send + Sync {
    fn open(&self, handshake: Handshake) -> BoxFuture<'static, Result<Connection, WsError>>;
}

/// The route a Responses provider takes over WebSocket, which decides
/// whether an endpoint offers it and how it is addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    /// OpenAI (and OpenAI-compatible Responses servers).
    #[default]
    OpenAI,
    /// Azure OpenAI: only the v1 API on `*.openai.azure.com`.
    Azure,
    /// xAI: no OpenAI handshake header, and continuation only of stored
    /// responses.
    Xai,
}

impl Route {
    pub fn name(self) -> &'static str {
        match self {
            Route::OpenAI => "OpenAI Responses",
            Route::Azure => "Azure OpenAI Responses",
            Route::Xai => "xAI Responses",
        }
    }

    /// The connection age after which a new one is opened before sending.
    pub fn rotate_after(self) -> Duration {
        match self {
            Route::Xai => Duration::from_secs(24 * 60),
            _ => Duration::from_secs(55 * 60),
        }
    }

    fn enabled(self, url: &reqwest::Url) -> bool {
        match self {
            Route::Azure => {
                url.scheme() == "https"
                    && url
                        .host_str()
                        .is_some_and(|h| h.to_ascii_lowercase().ends_with(".openai.azure.com"))
                    && url.path().ends_with("/openai/v1/responses")
                    && url
                        .query_pairs()
                        .find(|(k, _)| k == "api-version")
                        .is_some_and(|(_, v)| v == "v1")
            }
            _ => true,
        }
    }

    fn url(self, mut url: reqwest::Url) -> reqwest::Url {
        if self == Route::Azure {
            let kept: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(k, _)| k != "api-version")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            if kept.is_empty() {
                url.set_query(None);
            } else {
                url.query_pairs_mut().clear().extend_pairs(kept);
            }
        }
        url
    }

    fn shape(self) -> Option<Shape> {
        match self {
            // xAI continues only stored responses (`store: false` fails with
            // "Response with id=… not found"), rejects `instructions` next to
            // `previous_response_id`, and keeps the continued response's.
            Route::Xai => Some(|request| {
                if request.get("store") == Some(&Value::Bool(false)) {
                    return None;
                }
                let mut rest = request.clone();
                rest.remove("instructions");
                Some(rest)
            }),
            _ => None,
        }
    }

    /// The route for a provider id and its base URL.
    pub fn for_provider(provider_id: &str, base_url: &str) -> Route {
        let host = reqwest::Url::parse(base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        if provider_id == "azure" || host.ends_with(".openai.azure.com") {
            Route::Azure
        } else if provider_id == "xai" || host == "api.x.ai" {
            Route::Xai
        } else {
            Route::OpenAI
        }
    }
}

/// `https` to `wss`, `http` to `ws`.
pub fn to_websocket_url(url: &str) -> Result<String, WsError> {
    let invalid = |m: String| {
        WsError::new(m, Phase::Prepare)
            .code("invalid-url")
            .delivery(Delivery::NotSent)
            .at(url)
    };
    let mut u = reqwest::Url::parse(url).map_err(|e| invalid(e.to_string()))?;
    let scheme = match u.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => {
            return Err(invalid(format!(
                "Unsupported WebSocket URL protocol {other}:"
            )))
        }
    };
    u.set_scheme(scheme)
        .map_err(|_| invalid("Invalid WebSocket URL".into()))?;
    Ok(u.to_string())
}

/// One request prepared for either transport.
pub struct Prepared {
    /// The HTTP request (the fallback).
    pub http: reqwest::Request,
    /// The WebSocket exchange, when the route offers one for this endpoint.
    pub channel: Option<Channel>,
}

/// A request's WebSocket exchange: where to connect and the message.
pub struct Channel {
    pub handshake: Handshake,
    pub rotate_after: Duration,
    /// The `response.create` message as a whole request.
    pub message: Map<String, Value>,
    pub route: Route,
}

impl Channel {
    /// A driver for this exchange with the route's ordering checks.
    pub fn driver(&self) -> Driver {
        Driver::new(
            self.route.name(),
            self.message.clone(),
            order(self.route.name()),
            self.route.shape(),
        )
    }
}

/// The request with the protocol's defaults, checked (as `Provider::stream`
/// sends it).
pub(crate) fn resolve(provider: &Provider, req: &Request) -> Result<Request, Error> {
    let defaults = crate::options::defaults(provider.protocol, &provider.base_url, &req.model);
    let req = Request {
        options: defaults.merged(&req.options),
        ..req.clone()
    };
    crate::check(provider.protocol, &req)?;
    Ok(req)
}

/// Prepare `req` (already resolved) for both transports from the same final
/// HTTP request: the WebSocket message is its body without the HTTP-only
/// fields, and the handshake carries its URL and headers.
pub fn prepare(
    provider: &Provider,
    client: &reqwest::Client,
    req: &Request,
    route: Route,
) -> Result<Prepared, Error> {
    let http = crate::responses::request(provider, client, req).build()?;
    let enabled = route.enabled(http.url());
    let channel = if enabled {
        let body = crate::responses::body_for(req, &provider.base_url);
        let Value::Object(mut message) = body else {
            return Err(Error::Invalid(format!(
                "{} WebSocket body must be a JSON object",
                route.name()
            )));
        };
        for k in ["stream", "stream_options", "background"] {
            message.remove(k);
        }
        let mut message_with_type = Map::new();
        message_with_type.insert("type".into(), json!("response.create"));
        message_with_type.extend(message);
        let url = to_websocket_url(route.url(http.url().clone()).as_str()).map_err(Error::from)?;
        let mut headers: Vec<(String, String)> = http
            .headers()
            .iter()
            .filter(|(k, _)| {
                !matches!(
                    k.as_str(),
                    "content-length" | "content-encoding" | "content-type"
                )
            })
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        if route == Route::OpenAI && !headers.iter().any(|(k, _)| k == "openai-beta") {
            headers.push(("openai-beta".into(), PROTOCOL_HEADER.into()));
        }
        Some(Channel {
            handshake: Handshake { url, headers },
            rotate_after: route.rotate_after(),
            message: message_with_type,
            route,
        })
    } else {
        None
    };
    Ok(Prepared { http, channel })
}

/// The route's ordering checks (OpenCode's channel observer): one
/// `response.created` with an id before any other response event, the same
/// response id throughout, and nothing after the terminal event. Keepalives
/// and provider notifications may come first.
pub fn order(name: &'static str) -> continuation::Base {
    let mut response_id: Option<String> = None;
    let mut terminal = false;
    Box::new(move |_create, frame, event| {
        let kind = event["type"].as_str().unwrap_or("");
        let invalid = |m: String| Err(Error::Decode(format!("{m}: {}", crate::quote(frame))));
        if terminal {
            return invalid(format!("{name} emitted {kind} after a terminal event"));
        }
        let id = event["response"]["id"].as_str();
        let failure = || {
            crate::responses::handle(&mut crate::responses::State::default(), kind, event)
                .err()
                .unwrap_or_else(|| Error::Provider {
                    message: format!("{name} stream error"),
                    retryable: false,
                })
        };
        match kind {
            "error" => {
                terminal = true;
                Ok(Observation::ProviderFailure(failure()))
            }
            "response.failed" => {
                terminal = true;
                if let (Some(seen), Some(id)) = (&response_id, id) {
                    if seen != id {
                        return invalid(format!("{name} response ID changed during execution"));
                    }
                }
                Ok(Observation::ProviderFailure(failure()))
            }
            "response.created" => {
                if response_id.is_some() {
                    return invalid(format!("{name} emitted duplicate response.created"));
                }
                let Some(id) = id.filter(|i| !i.is_empty()) else {
                    return invalid(format!("{name} response.created is missing response.id"));
                };
                response_id = Some(id.to_string());
                Ok(Observation::Frame(frame.to_string()))
            }
            k if !k.starts_with("response.") => Ok(Observation::Frame(frame.to_string())),
            _ => {
                let Some(seen) = &response_id else {
                    return invalid(format!("{name} emitted {kind} before response.created"));
                };
                if id.is_some_and(|id| id != seen) {
                    return invalid(format!("{name} response ID changed during execution"));
                }
                Ok(match kind {
                    "response.completed" => {
                        terminal = true;
                        Observation::Completed {
                            frame: frame.to_string(),
                            checkpoint: None,
                        }
                    }
                    "response.incomplete" => {
                        terminal = true;
                        Observation::Incomplete(frame.to_string())
                    }
                    _ => Observation::Frame(frame.to_string()),
                })
            }
        }
    })
}

// --- tokio-tungstenite ---

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Connections over tokio-tungstenite with rustls and the webpki roots.
#[derive(Clone)]
pub struct Tungstenite {
    tls: Arc<rustls::ClientConfig>,
}

impl Default for Tungstenite {
    fn default() -> Self {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Tungstenite { tls: Arc::new(tls) }
    }
}

/// A system error code for a failed connect, as Node reports them.
fn system_code(e: &std::io::Error) -> Option<String> {
    use std::io::ErrorKind as K;
    let code = match e.kind() {
        K::ConnectionRefused => "ECONNREFUSED",
        K::ConnectionReset => "ECONNRESET",
        K::ConnectionAborted => "ECONNABORTED",
        K::TimedOut => "ETIMEDOUT",
        K::HostUnreachable => "EHOSTUNREACH",
        K::NetworkUnreachable => "ENETUNREACH",
        K::AddrNotAvailable => "EADDRNOTAVAIL",
        _ if e.to_string().contains("lookup") || e.to_string().contains("resolve") => "ENOTFOUND",
        _ => return None,
    };
    Some(code.to_string())
}

fn connect_error(e: tokio_tungstenite::tungstenite::Error, url: &str) -> WsError {
    use tokio_tungstenite::tungstenite::Error as E;
    let code = match &e {
        E::Io(io) => system_code(io),
        E::Http(resp) => Some(resp.status().as_u16().to_string()),
        _ => None,
    };
    let mut err = WsError::new(format!("Failed to open WebSocket: {e}"), Phase::Connect)
        .delivery(Delivery::NotSent)
        .at(url);
    err.code = code;
    err
}

fn read_error(e: tokio_tungstenite::tungstenite::Error, url: &str) -> WsError {
    use tokio_tungstenite::tungstenite::Error as E;
    match e {
        E::Capacity(_) => {
            WsError::new("WebSocket message exceeds the 16 MiB limit", Phase::Receive)
                .code("message-too-large")
        }
        E::ConnectionClosed | E::AlreadyClosed => {
            WsError::new("WebSocket closed", Phase::Close).code("closed")
        }
        e => WsError::new(format!("WebSocket error: {e}"), Phase::Receive).code("message"),
    }
    .at(url)
}

struct TungsteniteOut {
    sink: futures_util::stream::SplitSink<Socket, tokio_tungstenite::tungstenite::Message>,
    reader: tokio::task::JoinHandle<()>,
    url: String,
}

impl Drop for TungsteniteOut {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Outbound for TungsteniteOut {
    fn send_text(&mut self, text: String) -> BoxFuture<'_, Result<(), WsError>> {
        Box::pin(async move {
            use tokio_tungstenite::tungstenite::Error as E;
            self.sink
                .send(tokio_tungstenite::tungstenite::Message::text(text))
                .await
                .map_err(|e| {
                    let delivery = match e {
                        E::AlreadyClosed | E::ConnectionClosed => Delivery::NotSent,
                        _ => Delivery::Ambiguous,
                    };
                    WsError::new(
                        format!("Failed to send WebSocket message: {e}"),
                        Phase::Send,
                    )
                    .delivery(delivery)
                    .at(&self.url)
                })
        })
    }

    fn close(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.sink.close()).await;
            self.reader.abort();
        })
    }
}

impl Connector for Tungstenite {
    fn open(&self, handshake: Handshake) -> BoxFuture<'static, Result<Connection, WsError>> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
        let tls = self.tls.clone();
        Box::pin(async move {
            let url = handshake.url.clone();
            let mut request = url.as_str().into_client_request().map_err(|e| {
                WsError::new(format!("Invalid WebSocket URL: {e}"), Phase::Prepare)
                    .code("invalid-url")
                    .delivery(Delivery::NotSent)
                    .at(&url)
            })?;
            for (k, v) in &handshake.headers {
                if let (Ok(k), Ok(v)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(v),
                ) {
                    request.headers_mut().insert(k, v);
                }
            }
            let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(MAX_FRAME_BYTES))
                .max_frame_size(Some(MAX_FRAME_BYTES));
            let (socket, _) = tokio_tungstenite::connect_async_tls_with_config(
                request,
                Some(config),
                true,
                Some(tokio_tungstenite::Connector::Rustls(tls)),
            )
            .await
            .map_err(|e| connect_error(e, &url))?;
            let (sink, mut stream) = socket.split();
            let (tx, inbound) = mpsc::channel(INBOUND_FRAMES);
            let reader_url = url.clone();
            let reader = tokio::spawn(async move {
                use tokio_tungstenite::tungstenite::Message as M;
                loop {
                    let next = match stream.next().await {
                        None => Err(WsError::new("WebSocket closed", Phase::Close)
                            .code("closed")
                            .at(&reader_url)),
                        Some(Ok(M::Text(t))) => Ok(Frame::Text(t.as_str().to_string())),
                        Some(Ok(M::Binary(b))) => Ok(Frame::Binary(b.to_vec())),
                        Some(Ok(M::Close(frame))) => {
                            let (code, reason) = frame
                                .map(|f| (u16::from(f.code), f.reason.as_str().to_string()))
                                .unwrap_or((1005, String::new()));
                            let mut message = format!("WebSocket closed with code {code}");
                            if !reason.is_empty() {
                                message.push_str(&format!(": {reason}"));
                            }
                            Err(WsError::new(message, Phase::Close)
                                .code(code.to_string())
                                .at(&reader_url))
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => Err(read_error(e, &reader_url)),
                    };
                    let end = next.is_err();
                    if tx.send(next).await.is_err() || end {
                        return;
                    }
                }
            });
            Ok(Connection {
                outbound: Box::new(TungsteniteOut { sink, reader, url }),
                inbound,
            })
        })
    }
}
