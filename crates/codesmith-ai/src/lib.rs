//! Model provider protocols. A `Provider` turns a provider-neutral `Request`
//! into a stream of provider-neutral `Event`s.

mod anthropic;
mod billing;
mod cache;
mod endpoint;
mod gemini;
mod hosted;
mod openai;
mod options;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod parity_ws;
pub mod partial_json;
#[cfg(test)]
mod replay_tests;
mod responses;
mod schema;
mod sse;
pub mod ws;

pub use cache::{Cache, Messages as CacheMessages, Placement as CachePlacement};
pub use endpoint::{azure_endpoint, azure_resource_url, Auth, Compatibility, Encoding};
pub use hosted::{Hosted, HostedResult};
pub use options::{builtin_variants, defaults, variant_from_config, Options, Variant};

use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// A mid-conversation system update: text the harness (not the user)
    /// adds to the conversation, such as an instruction to carry on after a
    /// compaction. Lowered per protocol (after OpenCode's `Message.system`):
    /// a native `system` message on Claude models that take one, a
    /// `developer` message on Responses, and escaped `<system-update>` user
    /// text elsewhere. Only its text parts are sent.
    ///
    /// Stored as `"role": "system"`; sessions written before this role
    /// existed never contain it, and older binaries cannot read records
    /// that do.
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Part {
    Text(String),
    /// Model reasoning, kept so it can be replayed to providers that want it.
    Reasoning(Reasoning),
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
    /// A tool the provider ran itself (web search, code execution, …),
    /// with its result: shown, stored and replayed, never run locally.
    Hosted(Hosted),
}

/// Model reasoning: the text shown to the user, and what the provider needs
/// to accept it back (a signature, encrypted state).
///
/// Stored as a plain string when there is no provider state, as sessions
/// written before provider state existed are; otherwise as
/// `{"text": ..., "meta": {...}}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "ReasoningRepr", into = "ReasoningRepr")]
pub struct Reasoning {
    pub text: String,
    pub meta: Option<ReasoningMeta>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ReasoningRepr {
    Plain(String),
    Full {
        #[serde(default)]
        text: String,
        #[serde(default)]
        meta: Option<ReasoningMeta>,
    },
}

impl From<ReasoningRepr> for Reasoning {
    fn from(r: ReasoningRepr) -> Self {
        match r {
            ReasoningRepr::Plain(text) => Reasoning { text, meta: None },
            ReasoningRepr::Full { text, meta } => Reasoning { text, meta },
        }
    }
}

impl From<Reasoning> for ReasoningRepr {
    fn from(r: Reasoning) -> Self {
        match r.meta {
            None => ReasoningRepr::Plain(r.text),
            meta => ReasoningRepr::Full { text: r.text, meta },
        }
    }
}

impl From<String> for Reasoning {
    fn from(text: String) -> Self {
        Reasoning { text, meta: None }
    }
}

impl From<&str> for Reasoning {
    fn from(text: &str) -> Self {
        Reasoning::from(text.to_string())
    }
}

impl std::ops::Deref for Reasoning {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl Reasoning {
    pub fn new(text: impl Into<String>, meta: ReasoningMeta) -> Self {
        Reasoning {
            text: text.into(),
            meta: Some(meta),
        }
    }

    /// The provider state, when it came from `protocol`.
    pub fn meta_for(&self, protocol: Protocol) -> Option<&ReasoningMeta> {
        self.meta.as_ref().filter(|m| m.protocol == Some(protocol))
    }
}

/// Opaque provider state that lets reasoning be replayed. Only the protocol
/// that produced it reads it back.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    /// Anthropic thinking signature, or Gemini `thoughtSignature`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Anthropic `redacted_thinking` data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted: Option<String>,
    /// OpenAI Responses reasoning item id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// OpenAI Responses `encrypted_content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<String>,
    /// Chat Completions `reasoning_details` (OpenRouter, Kimi).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<Value>>,
    /// Chat Completions `reasoning_opaque` (GitHub Copilot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque: Option<String>,
    /// The Chat Completions field the reasoning text streamed under
    /// (`reasoning_content`, `reasoning`), where it is sent back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

impl ReasoningMeta {
    pub fn of(protocol: Protocol) -> Self {
        ReasoningMeta {
            protocol: Some(protocol),
            ..Default::default()
        }
    }
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

    /// A mid-conversation system update (see [`Role::System`]).
    pub fn system(text: impl Into<String>) -> Self {
        Message {
            role: Role::System,
            parts: vec![Part::Text(text.into())],
        }
    }
}

/// The text of a system update: its text parts, one per line (OpenCode's
/// `joinText`). System updates carry text only; other parts are not sent.
pub(crate) fn system_text(m: &Message) -> String {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A system update as user text, for routes without a native role for it
/// (OpenCode's `wrapSystemUpdate`): visibly lower-authority, in its original
/// place, and escaped so the text cannot close the wrapper.
pub(crate) fn wrap_system_update(m: &Message) -> String {
    let text = system_text(m)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<system-update>\n{text}\n</system-update>")
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
    /// Provider options (reasoning effort, thinking, service tier, …),
    /// lowered per protocol.
    pub options: Options,
    /// Extra top-level body fields, merged last (e.g. `chat_template_kwargs`).
    pub body: serde_json::Map<String, Value>,
    /// Prompt caching; on by default (see [`Cache`]).
    pub cache: Cache,
    /// A key that stays the same for a conversation (the session id), so
    /// OpenAI-style providers route its requests to the same prompt cache.
    pub prompt_cache_key: Option<String>,
    /// How the model may use the declared tools; `None` leaves it to the
    /// provider (the model chooses). Sent only when tools are declared.
    pub tool_choice: Option<ToolChoice>,
    /// Whether the model may call several tools in one turn; `None` keeps
    /// the provider default. A `parallelToolCalls` option wins over it.
    pub parallel_tool_calls: Option<bool>,
}

/// OpenCode's `toolChoice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model chooses whether to call tools.
    Auto,
    /// No tool calls; the tool definitions stay (and with them the cached
    /// prefix).
    None,
    /// The model must call some tool.
    Required,
    /// The model must call this tool.
    Tool(String),
}

/// Token counts for a response (or a sum of them). `input` is the
/// non-cached prompt; `output` includes reasoning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The USD the endpoint reports it billed for the response
    /// (OpenRouter's `cost`, GitHub Copilot's credits), when it reports
    /// one. On a total, the sum of the steps that reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// The part of `output` spent on hidden reasoning, when reported.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning: u64,
    /// The provider's total, or the sum of the reported counts; `None` when
    /// the provider reported neither input nor output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Usage {
    /// Every token counted: the provider's total, or the sum of the counts.
    pub fn tokens(&self) -> u64 {
        self.total
            .unwrap_or(self.input + self.cache_read + self.cache_write + self.output)
    }

    /// Add another response's counts and billed cost. The total stays
    /// unknown while both are; otherwise a side without one counts its
    /// tokens.
    pub fn add(&mut self, u: &Usage) {
        self.total = match (self.total, u.total) {
            (None, None) => None,
            _ => Some(self.tokens() + u.tokens()),
        };
        self.input += u.input;
        self.output += u.output;
        self.cache_read += u.cache_read;
        self.cache_write += u.cache_write;
        self.reasoning += u.reasoning;
        if let Some(c) = u.cost {
            self.cost = Some(self.cost.unwrap_or(0.0) + c);
        }
    }
}

/// OpenCode's finish-reason algebra: what every protocol's reasons
/// normalize to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    #[default]
    Stop,
    /// The output token limit (or the context window) cut the response off.
    Length,
    ToolCalls,
    /// A safety filter or refusal stopped the response.
    ContentFilter,
    Error,
    /// A reason the protocol does not know.
    Unknown,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Stop => "stop",
            Reason::Length => "length",
            Reason::ToolCalls => "tool-calls",
            Reason::ContentFilter => "content-filter",
            Reason::Error => "error",
            Reason::Unknown => "unknown",
        }
    }
}

/// Why a response finished: the normalized reason, the provider's own, and
/// for a content-filter finish the policy area and explanation the provider
/// gave (OpenCode's `FinishReasonDetails`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishReason {
    pub normalized: Reason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

impl FinishReason {
    /// `normalized` with the provider's reason `raw`.
    pub fn new(normalized: Reason, raw: impl Into<String>) -> Self {
        FinishReason {
            normalized,
            raw: Some(raw.into()),
            ..Default::default()
        }
    }

    /// A reason inferred rather than reported.
    pub fn inferred(normalized: Reason) -> Self {
        FinishReason {
            normalized,
            ..Default::default()
        }
    }

    /// The output token limit cut the response off.
    pub fn is_length(&self) -> bool {
        self.normalized == Reason::Length
    }

    /// A safety filter or refusal stopped the response.
    pub fn is_content_filter(&self) -> bool {
        self.normalized == Reason::ContentFilter
    }

    /// The provider's reason, or the normalized one when it gave none.
    pub fn as_str(&self) -> &str {
        self.raw.as_deref().unwrap_or(self.normalized.as_str())
    }
}

impl From<Reason> for FinishReason {
    fn from(r: Reason) -> Self {
        FinishReason::inferred(r)
    }
}

/// The first entry of `sources` (JSON objects) naming a filter that fired:
/// `{"name": {"filtered": true}}`, or any truthy value (OpenCode's
/// `contentFilterCategory`).
pub(crate) fn filter_category(sources: &[&Value]) -> Option<String> {
    sources
        .iter()
        .filter_map(|s| s.as_object())
        .flatten()
        .find(|(_, v)| match v.as_object() {
            Some(o) if o.contains_key("filtered") => o["filtered"] == true,
            _ => truthy(v),
        })
        .map(|(k, _)| k.clone())
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// One piece of a streamed response.
///
/// Text, reasoning and tool input arrive in blocks, as in OpenCode: a block
/// opens with its start event (an id unique within the response), and its
/// deltas belong to the latest open block of their kind. Protocols that
/// stream reasoning without block boundaries (Chat) open one reasoning block
/// for the whole response.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TextStart(String),
    TextDelta(String),
    TextEnd(String),
    ReasoningStart(String),
    ReasoningDelta(String),
    /// The current reasoning block is complete, with the provider state
    /// needed to replay it. A block that streamed no text (redacted or
    /// omitted thinking) is still a block.
    ReasoningEnd(ReasoningMeta),
    /// A tool call started streaming its arguments; its [`Event::ToolCall`]
    /// follows its [`Event::ToolInputEnd`]. `id` is the call's id as
    /// streamed (the final call's id may carry provider state after it).
    ToolInputStart {
        id: String,
        name: String,
    },
    /// Raw argument text for a started tool call (see
    /// [`partial_json::partial_input`] for the value so far).
    ToolInputDelta {
        id: String,
        text: String,
    },
    ToolInputEnd {
        id: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool the provider runs itself was called (its `result` is
    /// `None`). It is never run locally.
    HostedCall(Hosted),
    /// A hosted tool's result (`result` is set), for the call with its id.
    HostedResult(Hosted),
    Finish {
        reason: FinishReason,
        usage: Usage,
        /// The provider's usage payload as it came, for counts CodeSmith
        /// does not normalize (OpenCode's `usage.providerMetadata`).
        raw_usage: Option<Value>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// OpenAI Chat Completions, also used by most OpenAI-compatible servers.
    #[default]
    OpenaiChat,
    /// Anthropic Messages.
    AnthropicMessages,
    /// OpenAI Responses (stateless; nothing stored server-side).
    OpenaiResponses,
    /// Google Gemini generateContent.
    GoogleGemini,
}

impl Protocol {
    /// The protocol's name in errors (OpenCode's route names).
    pub fn route(self) -> &'static str {
        match self {
            Protocol::OpenaiChat => "OpenAI Chat",
            Protocol::AnthropicMessages => "Anthropic Messages",
            Protocol::OpenaiResponses => "OpenAI Responses",
            Protocol::GoogleGemini => "Gemini",
        }
    }
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

#[derive(Debug, Clone, Default)]
pub struct Provider {
    pub protocol: Protocol,
    /// Base URL including the version path, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Query parameters added to every request URL (Azure's `api-version`).
    pub query: Vec<(String, String)>,
    /// Which header carries `api_key`.
    pub auth: Auth,
    /// The request body's content encoding.
    pub encoding: Encoding,
    /// Explicit overrides of detected OpenAI Chat behaviour.
    pub compat: Compatibility,
}

impl Provider {
    pub async fn stream(
        &self,
        client: &reqwest::Client,
        req: &Request,
    ) -> Result<EventStream, Error> {
        let defaults = options::defaults(self.protocol, &self.base_url, &req.model);
        let owned;
        let req = if defaults.is_empty() {
            req
        } else {
            owned = Request {
                options: defaults.merged(&req.options),
                ..req.clone()
            };
            &owned
        };
        check(self.protocol, req)?;
        match self.protocol {
            Protocol::OpenaiChat => openai::stream(self, client, req).await,
            Protocol::AnthropicMessages => anthropic::stream(self, client, req).await,
            Protocol::OpenaiResponses => responses::stream(self, client, req).await,
            Protocol::GoogleGemini => gemini::stream(self, client, req).await,
        }
    }

    /// The HTTP request `stream` would send for `req`, without sending it
    /// (for `debug` output and tests).
    pub fn prepare(
        &self,
        client: &reqwest::Client,
        req: &Request,
    ) -> Result<reqwest::Request, Error> {
        let defaults = options::defaults(self.protocol, &self.base_url, &req.model);
        let req = Request {
            options: defaults.merged(&req.options),
            ..req.clone()
        };
        check(self.protocol, &req)?;
        let rb = match self.protocol {
            Protocol::OpenaiChat => openai::request(self, client, &req),
            Protocol::AnthropicMessages => anthropic::request(self, client, &req),
            Protocol::OpenaiResponses => responses::request(self, client, &req),
            Protocol::GoogleGemini => gemini::request(self, client, &req),
        };
        Ok(rb.build()?)
    }
}

/// Builds an assistant message's parts from its stream events, in stream
/// order: a text or reasoning block is one part, and a reasoning block ends
/// at its [`Event::ReasoningEnd`]. Deltas outside any block join the latest
/// part of their kind or start one. Reasoning from a protocol with one
/// reasoning block per response (Chat) is one part, however it interleaves
/// with text.
#[derive(Debug, Default)]
pub struct Collector {
    parts: Vec<Part>,
    open: Option<Open>,
    /// A reasoning part closed by other content rather than by its end.
    loose: Option<usize>,
    /// Tool calls whose arguments are streaming: id, name, raw arguments.
    forming: Vec<(String, String, String)>,
}

#[derive(Debug)]
enum Open {
    Text(String),
    Reasoning(String),
}

impl Collector {
    pub fn push(&mut self, ev: &Event) {
        match ev {
            Event::TextStart(_) => {
                self.close();
                self.open = Some(Open::Text(String::new()));
            }
            Event::TextEnd(_) => {
                if matches!(self.open, Some(Open::Text(_))) {
                    self.close();
                }
            }
            Event::TextDelta(d) if !d.is_empty() => match &mut self.open {
                Some(Open::Text(t)) => t.push_str(d),
                _ => {
                    self.close();
                    self.open = Some(Open::Text(d.clone()));
                }
            },
            Event::ReasoningStart(_) => {
                if !matches!(self.open, Some(Open::Reasoning(_))) {
                    self.close();
                    self.loose = None;
                    self.open = Some(Open::Reasoning(String::new()));
                }
            }
            Event::ReasoningDelta(d) if !d.is_empty() => match (&mut self.open, self.loose) {
                (Some(Open::Reasoning(t)), _) => t.push_str(d),
                (_, Some(i)) => {
                    if let Some(Part::Reasoning(r)) = self.parts.get_mut(i) {
                        r.text.push_str(d);
                    }
                }
                _ => {
                    self.close();
                    self.open = Some(Open::Reasoning(d.clone()));
                }
            },
            Event::ReasoningEnd(meta) => {
                self.loose = None;
                if let Some(Open::Reasoning(t)) = &mut self.open {
                    let text = std::mem::take(t);
                    self.open = None;
                    self.parts
                        .push(Part::Reasoning(Reasoning::new(text, meta.clone())));
                    return;
                }
                // State that arrives after its reasoning closed (Chat sends
                // it at the end of the stream) joins that reasoning.
                let late = self.parts.iter_mut().rev().find_map(|p| match p {
                    Part::Reasoning(r) if r.meta.is_none() => Some(r),
                    _ => None,
                });
                match late {
                    Some(r) => r.meta = Some(meta.clone()),
                    None => {
                        self.close();
                        self.parts
                            .push(Part::Reasoning(Reasoning::new("", meta.clone())));
                    }
                }
            }
            Event::ToolInputStart { id, name } => {
                if !self.forming.iter().any(|f| f.0 == *id) {
                    self.forming.push((id.clone(), name.clone(), String::new()));
                }
            }
            Event::ToolInputDelta { id, text } => {
                if let Some(f) = self.forming.iter_mut().find(|f| f.0 == *id) {
                    f.2.push_str(text);
                }
            }
            Event::ToolInputEnd { id } => self.forming.retain(|f| f.0 != *id),
            Event::HostedCall(h) => {
                self.close();
                self.parts.push(Part::Hosted(h.clone()));
            }
            Event::HostedResult(h) => {
                let call = self.parts.iter_mut().rev().find_map(|p| match p {
                    Part::Hosted(c) if c.id == h.id && c.result.is_none() => Some(c),
                    _ => None,
                });
                match call {
                    Some(c) => c.result = h.result.clone(),
                    None => {
                        self.close();
                        self.parts.push(Part::Hosted(h.clone()));
                    }
                }
            }
            Event::ToolCall { id, name, input } => {
                self.close();
                self.parts.push(Part::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
            }
            _ => {}
        }
    }

    fn close(&mut self) {
        match self.open.take() {
            Some(Open::Text(t)) => self.parts.push(Part::Text(t)),
            // A block that opened but streamed nothing and has no state.
            Some(Open::Reasoning(t)) if t.is_empty() => {}
            Some(Open::Reasoning(t)) => {
                self.loose = Some(self.parts.len());
                self.parts.push(Part::Reasoning(t.into()));
            }
            None => {}
        }
    }

    /// Tool calls whose arguments are still streaming, with the input
    /// parsed so far: `(id, name, input)`.
    pub fn forming(&self) -> Vec<(&str, &str, Value)> {
        self.forming
            .iter()
            .map(|(id, name, raw)| (id.as_str(), name.as_str(), partial_json::partial_input(raw)))
            .collect()
    }

    /// The parts so far, in order.
    pub fn finish(mut self) -> Vec<Part> {
        self.close();
        self.parts
    }
}

/// The parts of the message a complete event stream describes.
pub fn collect<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<Part> {
    let mut c = Collector::default();
    for e in events {
        c.push(e);
    }
    c.finish()
}

/// A model response assembled from its stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    pub parts: Vec<Part>,
    /// Why the response finished; [`Reason::Error`] for the partial output
    /// of a failed stream.
    pub reason: FinishReason,
    pub usage: Usage,
    /// The provider's usage payload as it came.
    pub raw_usage: Option<Value>,
}

impl Response {
    /// The visible text, joined.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The reasoning text, joined.
    pub fn reasoning(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::Reasoning(r) => Some(r.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The complete tool calls: `(id, name, input)`.
    pub fn tool_calls(&self) -> Vec<(&str, &str, &Value)> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall { id, name, input } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }
}

/// A response stream that failed, with the output that arrived before the
/// failure (OpenCode's `AIError.partial`).
#[derive(Debug)]
pub struct Failure {
    pub error: Error,
    /// What streamed before the failure, if anything did: its text,
    /// reasoning and complete tool calls, with the reason [`Reason::Error`].
    /// Tool calls whose arguments were still streaming are left out. Boxed
    /// to keep the error small.
    pub partial: Option<Box<Response>>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl From<Failure> for Error {
    fn from(f: Failure) -> Self {
        f.error
    }
}

/// Consume a response stream into its [`Response`], showing `each` every
/// event as it arrives. A stream that fails, or ends without finishing,
/// fails with what it streamed so far.
pub async fn assemble(
    mut stream: EventStream,
    mut each: impl FnMut(&Event),
) -> Result<Response, Failure> {
    use futures_util::StreamExt;
    let mut parts = Collector::default();
    let mut finish: Option<(FinishReason, Usage, Option<Value>)> = None;
    let fail =
        |error: Error, parts: Collector, finish: Option<(FinishReason, Usage, Option<Value>)>| {
            let parts = parts.finish();
            let (usage, raw_usage) = finish.map(|f| (f.1, f.2)).unwrap_or_default();
            Failure {
                error,
                partial: (!parts.is_empty()).then(|| {
                    Box::new(Response {
                        parts,
                        reason: Reason::Error.into(),
                        usage,
                        raw_usage,
                    })
                }),
            }
        };
    while let Some(ev) = stream.next().await {
        let ev = match ev {
            Ok(ev) => ev,
            Err(e) => return Err(fail(e, parts, finish)),
        };
        each(&ev);
        if let Event::Finish {
            reason,
            usage,
            raw_usage,
        } = &ev
        {
            finish = Some((reason.clone(), *usage, raw_usage.clone()));
        }
        parts.push(&ev);
    }
    match finish {
        Some((reason, usage, raw_usage)) => Ok(Response {
            parts: parts.finish(),
            reason,
            usage,
            raw_usage,
        }),
        None => Err(fail(
            Error::Transport("the response stream ended before the model finished".into()),
            parts,
            None,
        )),
    }
}

/// Reject a request with media `protocol` cannot carry, or (Anthropic)
/// malformed provider options, before sending it.
pub fn check(protocol: Protocol, req: &Request) -> Result<(), Error> {
    if let Some(ToolChoice::Tool(name)) = &req.tool_choice {
        if name.is_empty() {
            return Err(Error::Invalid(format!(
                "{} tool choice requires a tool name",
                protocol.route()
            )));
        }
    }
    match protocol {
        Protocol::OpenaiChat => openai::check(req),
        Protocol::AnthropicMessages => anthropic::check(req),
        Protocol::OpenaiResponses => responses::check(req),
        Protocol::GoogleGemini => gemini::check(req),
    }
}

impl Provider {
    /// Model ids the provider reports, sorted.
    pub async fn list_models(&self, client: &reqwest::Client) -> Result<Vec<String>, Error> {
        let rb = match self.protocol {
            Protocol::GoogleGemini => self.get(client, "models?pageSize=1000"),
            _ => self.get(client, "models"),
        };
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
/// becomes an error result), user text and system updates that arrive while
/// calls are pending move after their results, empty results say so, and results take their
/// name from the matching call. A call answered twice (a stopped turn whose
/// result raced the one recorded for it later) keeps its first result only;
/// providers reject a second.
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
    // Calls that already have a result (given, or filled in as missing).
    let mut answered = std::collections::HashSet::new();
    let settle = |out: &mut Vec<Message>,
                  pending: &mut Vec<(String, String)>,
                  deferred: &mut Vec<Message>,
                  answered: &mut std::collections::HashSet<String>| {
        if !pending.is_empty() {
            answered.extend(pending.iter().map(|(id, _)| id.clone()));
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
            settle(&mut out, &mut pending, &mut deferred, &mut answered);
            // An id a later turn uses again (some providers repeat them)
            // needs a result again.
            for p in &m.parts {
                if let Part::ToolCall { id, .. } = p {
                    answered.remove(id);
                }
            }
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
                    if names.contains_key(id) && !answered.insert(id.clone()) {
                        continue;
                    }
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
            // User content or a system update, keeping its role.
            let rest = Message {
                role: m.role.clone(),
                parts: rest,
            };
            if pending.is_empty() {
                out.push(rest);
            } else {
                deferred.push(rest);
            }
        }
        if pending.is_empty() {
            out.append(&mut deferred);
        }
    }
    settle(&mut out, &mut pending, &mut deferred, &mut answered);
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
    /// Once one response body ends (`dropped`: its connection failed part
    /// way), the token to continue the same response with in another
    /// request (Gemini), if any. The decoder carries on with that request's
    /// frames.
    fn resume(&mut self, _dropped: bool) -> Result<Option<String>, Error> {
        Ok(None)
    }
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

/// The content events of a stream, without block lifecycle events; for
/// tests that check content only.
#[cfg(test)]
pub(crate) fn content(events: impl IntoIterator<Item = Event>) -> Vec<Event> {
    events
        .into_iter()
        .filter(|e| {
            !matches!(
                e,
                Event::TextStart(_)
                    | Event::TextEnd(_)
                    | Event::ReasoningStart(_)
                    | Event::ToolInputStart { .. }
                    | Event::ToolInputDelta { .. }
                    | Event::ToolInputEnd { .. }
            )
        })
        .collect()
}

/// Sends the request again with a continuation token.
pub(crate) type Reopen = Box<
    dyn FnMut(String) -> futures_util::future::BoxFuture<'static, Result<reqwest::Response, Error>>
        + Send,
>;

pub(crate) fn drive<D: Decoder>(resp: reqwest::Response, d: D) -> EventStream {
    drive_resuming(resp, d, None)
}

/// Like [`drive`], continuing the response in new requests (`reopen`) while
/// the decoder has a continuation token for it.
pub(crate) fn drive_resuming<D: Decoder>(
    resp: reqwest::Response,
    mut d: D,
    mut reopen: Option<Reopen>,
) -> EventStream {
    use futures_util::StreamExt;
    async_stream::stream! {
        let mut resp = resp;
        loop {
            let mut bytes = resp.bytes_stream();
            let mut parser = sse::Parser::default();
            let mut utf8 = openai::Utf8::default();
            let mut dropped = None;
            'read: while let Some(chunk) = bytes.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        // A connection failure may resume; a timeout ends
                        // the response.
                        let timeout = e.is_timeout();
                        let e = Error::from(e);
                        if timeout || reopen.is_none() { yield Err(e); return; }
                        dropped = Some(e);
                        break 'read;
                    }
                };
                for frame in parser.push(&utf8.push(&chunk)) {
                    match d.frame(&frame) {
                        Ok(evs) => for ev in evs { yield Ok(ev); },
                        Err(e) => { yield Err(e); return; }
                    }
                    if d.done() { break 'read; }
                }
            }
            if dropped.is_none() && !d.done() {
                // A last frame without its blank line; a torn one is dropped.
                if let Some(frame) = parser.finish() {
                    match d.frame(&frame) {
                        Ok(evs) => for ev in evs { yield Ok(ev); },
                        Err(Error::Decode(_)) => {}
                        Err(e) => { yield Err(e); return; }
                    }
                }
            }
            let token = match d.resume(dropped.is_some()) {
                Ok(t) => t,
                Err(e) => { yield Err(e); return; }
            };
            match (token, reopen.as_mut()) {
                (Some(token), Some(reopen)) => match reopen(token).await {
                    Ok(r) => resp = r,
                    Err(e) => { yield Err(e); return; }
                },
                _ => {
                    if let Some(e) = dropped { yield Err(e); return; }
                    break;
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
    fn reasoning_with_provider_state_round_trips() {
        // Sessions stored before provider state hold reasoning as a string,
        // and plain reasoning is still written that way.
        let old = json!({"type": "reasoning", "data": "r"});
        let p: Part = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(p, Part::Reasoning("r".into()));
        assert_eq!(serde_json::to_value(&p).unwrap(), old);
        let mut meta = ReasoningMeta::of(Protocol::AnthropicMessages);
        meta.signature = Some("sig".into());
        let signed = Part::Reasoning(Reasoning::new("t", meta));
        let v = serde_json::to_value(&signed).unwrap();
        assert_eq!(
            v,
            json!({"type": "reasoning", "data": {"text": "t", "meta": {"protocol": "anthropic-messages", "signature": "sig"}}})
        );
        assert_eq!(serde_json::from_value::<Part>(v).unwrap(), signed);
    }

    #[test]
    fn collector_keeps_stream_order_and_reasoning_blocks() {
        let signed = |s: &str| {
            let mut m = ReasoningMeta::of(Protocol::AnthropicMessages);
            m.signature = Some(s.into());
            m
        };
        let parts = collect(&[
            Event::ReasoningDelta("a".into()),
            Event::ReasoningEnd(signed("s1")),
            Event::ReasoningEnd(signed("s2")),
            Event::TextDelta("x".into()),
            Event::ToolCall {
                id: "c".into(),
                name: "n".into(),
                input: json!({}),
            },
            Event::ReasoningDelta("b".into()),
            Event::ReasoningEnd(signed("s3")),
            Event::ToolCall {
                id: "d".into(),
                name: "n".into(),
                input: json!({}),
            },
        ]);
        assert_eq!(parts.len(), 6);
        assert_eq!(parts[0], Part::Reasoning(Reasoning::new("a", signed("s1"))));
        assert_eq!(parts[1], Part::Reasoning(Reasoning::new("", signed("s2"))));
        assert_eq!(parts[2], Part::Text("x".into()));
        assert_eq!(parts[4], Part::Reasoning(Reasoning::new("b", signed("s3"))));
        // Reasoning without block ends (Chat) stays one part around text,
        // and state sent at the end joins it.
        let chat = ReasoningMeta::of(Protocol::OpenaiChat);
        let parts = collect(&[
            Event::ReasoningDelta("a".into()),
            Event::TextDelta("x".into()),
            Event::ReasoningDelta("b".into()),
            Event::ReasoningEnd(chat.clone()),
        ]);
        assert_eq!(
            parts,
            [
                Part::Reasoning(Reasoning::new("ab", chat)),
                Part::Text("x".into())
            ]
        );
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
