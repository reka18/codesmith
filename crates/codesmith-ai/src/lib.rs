//! Model provider protocols. A `Provider` turns a provider-neutral `Request`
//! into a stream of provider-neutral `Event`s.

mod anthropic;
mod gemini;
mod openai;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod replay_tests;
mod responses;
mod schema;
mod sse;

use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Part {
    Text(String),
    /// Model reasoning, kept so it can be replayed to providers that want it.
    Reasoning(String),
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        name: String,
        content: String,
        is_error: bool,
        /// Images and files the tool returned beside its text (`read` on an
        /// image or PDF). Absent in sessions stored before media existed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        media: Vec<Media>,
    },
    /// An image, PDF or other file attached to a user message.
    Media(Media),
}

/// An image, PDF or other file for the model to see.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Media {
    /// IANA media type, e.g. `image/png` or `application/pdf`.
    pub mime: String,
    pub source: Source,
    /// The file's name, shown to providers that take one (PDF titles).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

/// Where a media payload lives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Inline bytes, base64-encoded (no `data:` prefix).
    Base64(String),
    /// A remote URL the provider fetches itself.
    Url(String),
}

/// The broad kind of a media type, which decides how protocols lower it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Pdf,
    Video,
    Audio,
    Other,
}

impl Media {
    pub fn base64(mime: impl Into<String>, data: impl Into<String>) -> Self {
        Media {
            mime: mime.into(),
            source: Source::Base64(data.into()),
            filename: None,
        }
    }

    pub fn url(mime: impl Into<String>, url: impl Into<String>) -> Self {
        Media {
            mime: mime.into(),
            source: Source::Url(url.into()),
            filename: None,
        }
    }

    /// A `data:<mime>;base64,<data>` URL as inline media.
    pub fn from_data_url(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("data:")?;
        let (meta, data) = rest.split_once(',')?;
        let mime = meta.strip_suffix(";base64")?;
        let mime = if mime.is_empty() {
            "application/octet-stream"
        } else {
            mime
        };
        Some(Media::base64(mime, data))
    }

    pub fn named(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    /// The base64 payload of inline media.
    pub fn inline(&self) -> Option<&str> {
        match &self.source {
            Source::Base64(d) => Some(d),
            Source::Url(_) => None,
        }
    }

    /// The URL of remote media.
    pub fn remote(&self) -> Option<&str> {
        match &self.source {
            Source::Url(u) => Some(u),
            Source::Base64(_) => None,
        }
    }

    /// Inline media as a `data:` URL.
    pub fn data_url(&self) -> Option<String> {
        self.inline()
            .map(|d| format!("data:{};base64,{d}", self.mime))
    }

    /// Inline media as a data URL, or the remote URL.
    pub fn location(&self) -> String {
        self.data_url()
            .unwrap_or_else(|| self.remote().unwrap_or_default().to_string())
    }

    /// Bytes of inline payload (base64 characters); remote media has none.
    pub fn inline_len(&self) -> usize {
        self.inline().map_or(0, str::len)
    }

    pub fn kind(&self) -> MediaKind {
        let m = self.mime.to_ascii_lowercase();
        if m == "application/pdf" {
            MediaKind::Pdf
        } else if m.starts_with("image/") {
            MediaKind::Image
        } else if m.starts_with("video/") {
            MediaKind::Video
        } else if m.starts_with("audio/") {
            MediaKind::Audio
        } else {
            MediaKind::Other
        }
    }
}

/// Every media part of a request: user attachments and tool-result media.
pub(crate) fn all_media(req: &Request) -> impl Iterator<Item = &Media> {
    req.messages.iter().flat_map(|m| {
        m.parts.iter().flat_map(|p| match p {
            Part::Media(media) => std::slice::from_ref(media),
            Part::ToolResult { media, .. } => media.as_slice(),
            _ => &[],
        })
    })
}

/// The error for media a protocol needs inline but got as a URL.
pub(crate) fn inline_required(route: &str) -> Error {
    Error::Invalid(format!(
        "{route} requires inline media (bytes or base64); url sources must be materialized first"
    ))
}

/// The error for media a protocol cannot lower.
pub(crate) fn unsupported_media(route: &str, m: &Media) -> Error {
    Error::Invalid(format!("{route} does not support media type {}", m.mime))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            parts: vec![Part::Text(text.into())],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub parameters: Value,
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Extra top-level body fields, merged last (e.g. `chat_template_kwargs`).
    pub body: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    Finish {
        reason: String,
        usage: Usage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// OpenAI Chat Completions, also used by most OpenAI-compatible servers.
    OpenaiChat,
    /// Anthropic Messages.
    AnthropicMessages,
    /// OpenAI Responses (stateless; nothing stored server-side).
    OpenaiResponses,
    /// Google Gemini generateContent.
    GoogleGemini,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("request failed: {0}")]
    Transport(String),
    #[error("bad response: {0}")]
    Decode(String),
    /// An error the provider reported inside a 2xx stream (an `error` event,
    /// `response.failed`, a failing finish reason).
    #[error("{message}")]
    Provider { message: String, retryable: bool },
    /// A request the protocol cannot express (media it does not accept),
    /// found before anything is sent.
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    /// Whether retrying the same request may succeed.
    pub fn retryable(&self) -> bool {
        match self {
            Error::Status { status, .. } => {
                *status == 408 || *status == 409 || *status == 429 || *status >= 500
            }
            Error::Transport(_) => true,
            Error::Decode(_) | Error::Invalid(_) => false,
            Error::Provider { retryable, .. } => *retryable,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Transport(e.to_string())
    }
}

pub type EventStream = BoxStream<'static, Result<Event, Error>>;

#[derive(Debug, Clone)]
pub struct Provider {
    pub protocol: Protocol,
    /// Base URL including the version path, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl Provider {
    pub async fn stream(
        &self,
        client: &reqwest::Client,
        req: &Request,
    ) -> Result<EventStream, Error> {
        check(self.protocol, req)?;
        match self.protocol {
            Protocol::OpenaiChat => openai::stream(self, client, req).await,
            Protocol::AnthropicMessages => anthropic::stream(self, client, req).await,
            Protocol::OpenaiResponses => responses::stream(self, client, req).await,
            Protocol::GoogleGemini => gemini::stream(self, client, req).await,
        }
    }
}

/// Reject a request with media `protocol` cannot carry, before sending it.
pub fn check(protocol: Protocol, req: &Request) -> Result<(), Error> {
    match protocol {
        Protocol::OpenaiChat => openai::check(req),
        Protocol::AnthropicMessages => anthropic::check(req),
        Protocol::OpenaiResponses => Ok(()),
        Protocol::GoogleGemini => gemini::check(req),
    }
}

impl Provider {
    /// Model ids the provider reports, sorted.
    pub async fn list_models(&self, client: &reqwest::Client) -> Result<Vec<String>, Error> {
        let base = self.base_url.trim_end_matches('/');
        let mut rb = match self.protocol {
            Protocol::GoogleGemini => client.get(format!("{base}/models?pageSize=1000")),
            _ => client.get(format!("{base}/models")),
        };
        if let Some(k) = &self.api_key {
            rb = match self.protocol {
                Protocol::AnthropicMessages => rb
                    .header("x-api-key", k)
                    .header("anthropic-version", anthropic::VERSION),
                Protocol::GoogleGemini => rb.header("x-goog-api-key", k),
                _ => rb.bearer_auth(k),
            };
        }
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(Error::Status {
                status: status.as_u16(),
                body: text.chars().take(2000).collect(),
            });
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| Error::Decode(e.to_string()))?;
        Ok(model_ids(self.protocol, &v))
    }
}

pub(crate) fn model_ids(protocol: Protocol, v: &Value) -> Vec<String> {
    let mut ids: Vec<String> = match protocol {
        Protocol::GoogleGemini => v["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| {
                m["supportedGenerationMethods"]
                    .as_array()
                    .map(|a| a.iter().any(|x| x == "generateContent"))
                    .unwrap_or(true)
            })
            .filter_map(|m| {
                m["name"]
                    .as_str()
                    .map(|n| n.trim_start_matches("models/").to_string())
            })
            .collect(),
        _ => v["data"]
            .as_array()
            .or(v["models"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().or(m["name"].as_str()).map(str::to_string))
            .collect(),
    };
    ids.sort();
    ids.dedup();
    ids
}

/// Send a JSON POST and fail with the status and a bounded body on non-2xx.
pub(crate) async fn post(
    rb: reqwest::RequestBuilder,
    body: &Value,
) -> Result<reqwest::Response, Error> {
    let resp = rb.json(body).send().await?;
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

pub(crate) fn merge_body(body: &mut Value, extra: &serde_json::Map<String, Value>) {
    if let Value::Object(map) = body {
        for (k, v) in extra {
            map.insert(k.clone(), v.clone());
        }
    }
}

/// An error reported inside a stream, from its code (or type) and message.
/// The text reads `code: message`, falling back to whichever is present and
/// then to the raw payload. Rate limits, overloads and server faults are
/// retryable; so is a payload without any detail when `bare_is_transient`.
pub(crate) fn stream_error(
    code: Option<&str>,
    message: Option<&str>,
    raw: &Value,
    bare_is_transient: bool,
) -> Error {
    let code = code.filter(|c| !c.is_empty());
    let message = message.filter(|m| !m.is_empty());
    let text = match (code, message) {
        (Some(c), Some(m)) => format!("{c}: {m}"),
        (Some(c), None) => c.to_string(),
        (None, Some(m)) => m.to_string(),
        (None, None) => raw.to_string(),
    };
    let retryable = match code {
        Some(c) => transient_code(c),
        None => message.is_none() && bare_is_transient,
    };
    Error::Provider {
        message: text,
        retryable,
    }
}

/// Error codes, types and statuses that name a transient provider fault.
pub(crate) fn transient_code(code: &str) -> bool {
    let c = code.to_ascii_lowercase();
    if let Ok(n) = c.parse::<u16>() {
        return n == 408 || n == 409 || n == 429 || n >= 500;
    }
    [
        "rate_limit",
        "ratelimit",
        "too_many_requests",
        "usage_limit",
        "overloaded",
        "server_error",
        "internal",
        "unavailable",
        "timeout",
        "deadline",
        "resource_exhausted",
        "network",
        "api_error",
        "capacity",
    ]
    .iter()
    .any(|k| c.contains(k))
}

/// A fresh tool-call id for providers that send none (or repeat one). Unique
/// across requests too, so ids never collide later in a conversation.
pub(crate) fn fresh_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("tool_{t:x}{n:x}")
}

const MISSING_RESULT: &str = "Tool result missing";
const EMPTY_RESULT: &str = "(no tool output)";

/// Repair tool history before it is sent (after OpenCode's
/// `normalizeToolHistory`). Providers reject a tool call without a result,
/// other content between a call and its result, and (some) empty results, so:
/// every call gets a result before the next assistant turn (a missing one
/// becomes an error result), user text that arrives while calls are pending
/// moves after their results, empty results say so, and results take their
/// name from the matching call.
pub fn normalize_tool_history(messages: &[Message]) -> Vec<Message> {
    let mut names = std::collections::HashMap::new();
    for m in messages {
        for p in &m.parts {
            if let Part::ToolCall { id, name, .. } = p {
                names.insert(id.clone(), name.clone());
            }
        }
    }
    let user = |parts: Vec<Part>| Message {
        role: Role::User,
        parts,
    };
    let mut out: Vec<Message> = Vec::with_capacity(messages.len() + 1);
    // Calls of the latest assistant turn still waiting for a result, and user
    // content held back until they have one.
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut deferred: Vec<Message> = Vec::new();
    let settle = |out: &mut Vec<Message>,
                  pending: &mut Vec<(String, String)>,
                  deferred: &mut Vec<Message>| {
        if !pending.is_empty() {
            out.push(user(
                pending
                    .drain(..)
                    .map(|(id, name)| Part::ToolResult {
                        id,
                        name,
                        content: MISSING_RESULT.into(),
                        is_error: true,
                        media: vec![],
                    })
                    .collect(),
            ));
        }
        out.append(deferred);
    };
    for m in messages {
        if m.role == Role::Assistant {
            settle(&mut out, &mut pending, &mut deferred);
            pending = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                    _ => None,
                })
                .collect();
            out.push(m.clone());
            continue;
        }
        if !m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. })) && pending.is_empty() {
            out.push(m.clone());
            continue;
        }
        let mut results = Vec::new();
        let mut rest = Vec::new();
        for p in &m.parts {
            match p {
                Part::ToolResult {
                    id,
                    name,
                    content,
                    is_error,
                    media,
                } => {
                    pending.retain(|(pid, _)| pid != id);
                    results.push(Part::ToolResult {
                        id: id.clone(),
                        name: names.get(id).cloned().unwrap_or_else(|| name.clone()),
                        // A result that is only media is not empty.
                        content: if content.is_empty() && media.is_empty() {
                            EMPTY_RESULT.into()
                        } else {
                            content.clone()
                        },
                        is_error: *is_error,
                        media: media.clone(),
                    });
                }
                other => rest.push(other.clone()),
            }
        }
        if !results.is_empty() {
            out.push(user(results));
        }
        if !rest.is_empty() {
            if pending.is_empty() {
                out.push(user(rest));
            } else {
                deferred.push(user(rest));
            }
        }
        if pending.is_empty() {
            out.append(&mut deferred);
        }
    }
    settle(&mut out, &mut pending, &mut deferred);
    out
}

/// One protocol's stream decoding, frame by frame, independent of I/O.
pub(crate) trait Decoder: Send + 'static {
    /// Events for one SSE frame.
    fn frame(&mut self, f: &sse::Frame) -> Result<Vec<Event>, Error>;
    /// Whether the stream is complete; later frames are not read.
    fn done(&self) -> bool;
    /// Events once input ends (or `done`), or an error for a truncated stream.
    fn end(&mut self) -> Result<Vec<Event>, Error>;
}

/// Decode a complete SSE body; used by tests to run the real decoders.
#[cfg(test)]
pub(crate) fn decode_all<D: Decoder>(mut d: D, body: &str) -> Result<Vec<Event>, Error> {
    let mut parser = sse::Parser::default();
    let mut frames = parser.push(body);
    frames.extend(parser.finish());
    let mut out = Vec::new();
    for f in &frames {
        if d.done() {
            break;
        }
        out.extend(d.frame(f)?);
    }
    out.extend(d.end()?);
    Ok(out)
}

pub(crate) fn drive<D: Decoder>(resp: reqwest::Response, mut d: D) -> EventStream {
    use futures_util::StreamExt;
    let mut bytes = resp.bytes_stream();
    async_stream::stream! {
        let mut parser = sse::Parser::default();
        let mut utf8 = openai::Utf8::default();
        'read: while let Some(chunk) = bytes.next().await {
            let chunk = match chunk { Ok(c) => c, Err(e) => { yield Err(Error::from(e)); return; } };
            for frame in parser.push(&utf8.push(&chunk)) {
                match d.frame(&frame) {
                    Ok(evs) => for ev in evs { yield Ok(ev); },
                    Err(e) => { yield Err(e); return; }
                }
                if d.done() { break 'read; }
            }
        }
        if !d.done() {
            // A last frame without its blank line; a torn one is dropped.
            if let Some(frame) = parser.finish() {
                match d.frame(&frame) {
                    Ok(evs) => for ev in evs { yield Ok(ev); },
                    Err(Error::Decode(_)) => {}
                    Err(e) => { yield Err(e); return; }
                }
            }
        }
        match d.end() {
            Ok(evs) => for ev in evs { yield Ok(ev); },
            Err(e) => yield Err(e),
        }
    }
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses() {
        for s in [408, 409, 429, 500, 502, 503, 529] {
            assert!(
                Error::Status {
                    status: s,
                    body: String::new()
                }
                .retryable(),
                "{s}"
            );
        }
        for s in [400, 401, 403, 404, 422] {
            assert!(
                !Error::Status {
                    status: s,
                    body: String::new()
                }
                .retryable(),
                "{s}"
            );
        }
        assert!(Error::Transport("reset".into()).retryable());
        assert!(!Error::Decode("bad".into()).retryable());
    }

    #[test]
    fn protocol_names() {
        assert_eq!(
            serde_json::to_value(Protocol::OpenaiChat).unwrap(),
            "openai-chat"
        );
        assert_eq!(
            serde_json::from_value::<Protocol>("anthropic-messages".into()).unwrap(),
            Protocol::AnthropicMessages
        );
        for (name, p) in [
            ("openai-responses", Protocol::OpenaiResponses),
            ("google-gemini", Protocol::GoogleGemini),
        ] {
            assert_eq!(serde_json::from_value::<Protocol>(name.into()).unwrap(), p);
        }
    }

    #[test]
    fn model_listings() {
        use serde_json::json;
        assert_eq!(
            model_ids(
                Protocol::OpenaiChat,
                &json!({"data": [{"id": "b"}, {"id": "a"}]})
            ),
            ["a", "b"]
        );
        assert_eq!(
            model_ids(
                Protocol::GoogleGemini,
                &json!({"models": [
                    {"name": "models/gemini-x", "supportedGenerationMethods": ["generateContent"]},
                    {"name": "models/embed", "supportedGenerationMethods": ["embedContent"]}
                ]})
            ),
            ["gemini-x"]
        );
    }

    #[test]
    fn extra_body_overrides() {
        let mut body = serde_json::json!({"model": "m", "stream": true});
        let mut extra = serde_json::Map::new();
        extra.insert(
            "chat_template_kwargs".into(),
            serde_json::json!({"enable_thinking": false}),
        );
        extra.insert("model".into(), "override".into());
        merge_body(&mut body, &extra);
        assert_eq!(body["model"], "override");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    }
}

#[cfg(test)]
mod serde_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip_as_json() {
        let m = Message {
            role: Role::Assistant,
            parts: vec![
                Part::Reasoning("r".into()),
                Part::Text("t".into()),
                Part::ToolCall {
                    id: "c".into(),
                    name: "read".into(),
                    input: json!({"path": "a"}),
                },
                Part::ToolResult {
                    id: "c".into(),
                    name: "read".into(),
                    content: "x".into(),
                    is_error: false,
                    media: vec![],
                },
            ],
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["parts"][1], json!({"type": "text", "data": "t"}));
        assert_eq!(v["parts"][2]["data"]["name"], "read");
        assert_eq!(serde_json::from_value::<Message>(v).unwrap(), m);
        // No `media` key when there is none, so older readers still parse it.
        assert!(v_has_no_media(&m));
    }

    fn v_has_no_media(m: &Message) -> bool {
        let v = serde_json::to_value(m).unwrap();
        v["parts"][3]["data"].get("media").is_none()
    }

    #[test]
    fn results_stored_before_media_still_load() {
        let old = json!({"role": "user", "parts": [{"type": "tool_result", "data": {"id": "c", "name": "read", "content": "x", "is_error": false}}]});
        let m: Message = serde_json::from_value(old).unwrap();
        assert!(matches!(&m.parts[0], Part::ToolResult { media, .. } if media.is_empty()));
    }

    #[test]
    fn media_round_trips() {
        let m = Message {
            role: Role::User,
            parts: vec![
                Part::Text("see".into()),
                Part::Media(Media::base64("image/png", "AAEC").named("a.png")),
                Part::ToolResult {
                    id: "c".into(),
                    name: "read".into(),
                    content: "Image read successfully".into(),
                    is_error: false,
                    media: vec![Media::url("image/png", "https://x.test/a.png")],
                },
            ],
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v["parts"][1],
            json!({"type": "media", "data": {"mime": "image/png", "source": {"base64": "AAEC"}, "filename": "a.png"}})
        );
        assert_eq!(serde_json::from_value::<Message>(v).unwrap(), m);
    }

    #[test]
    fn data_urls() {
        let m = Media::from_data_url("data:image/jpeg;base64,/9j/").unwrap();
        assert_eq!((m.mime.as_str(), m.inline()), ("image/jpeg", Some("/9j/")));
        assert_eq!(m.data_url().unwrap(), "data:image/jpeg;base64,/9j/");
        assert!(Media::from_data_url("data:text/plain,hi").is_none());
        assert_eq!(m.kind(), MediaKind::Image);
        assert_eq!(Media::base64("application/pdf", "").kind(), MediaKind::Pdf);
    }
}
