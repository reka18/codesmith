//! OpenAI Chat Completions streaming (and OpenAI-compatible servers).

use crate::cache;
use crate::gemini::split_id;
use crate::{
    all_media, drive, effort_update, inline_required, merge_body, normalize_tool_history, schema,
    sse, stream_error, unsupported_media, wrap_system_update, Decoder, Error, Event, EventStream,
    FinishReason, Media, MediaKind, Message, Part, Protocol, Provider, Reason, ReasoningMeta,
    Request, Role, ToolChoice, Usage,
};
use serde_json::{json, Map, Value};

/// What differs between Chat Completions servers, read from the endpoint and
/// model id the way OpenCode detects it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Compat {
    /// OpenAI and Azure: `max_completion_tokens` (reasoning models reject
    /// `max_tokens`) and an explicit `store: false`.
    pub openai: bool,
    /// Mistral-family models: 9-character alphanumeric tool call ids, an
    /// assistant turn between tool results and user text (`bridge`), and no
    /// replayed reasoning field.
    pub mistral: bool,
    /// An assistant `Done.` turn between tool results and user content.
    pub bridge: bool,
    /// Claude models behind a gateway: tool call ids limited to `[A-Za-z0-9_-]`.
    pub claude: bool,
    /// OpenAI models (direct or `openai/...` behind a gateway): tool call ids
    /// of at most 40 characters.
    pub short_ids: bool,
    /// DeepSeek (and a configured reasoning field): every assistant message
    /// carries a reasoning field, empty when there was none.
    pub require_reasoning: bool,
    /// The field the model declares it reads and streams reasoning under
    /// (OpenCode's `compatibility.reasoningField`); `None` detects it.
    pub reasoning_field: Option<String>,
    /// Z.ai GLM models after 4.5 stream tool calls only with `tool_stream`.
    pub zai_tool_stream: bool,
    /// The server takes a body `prompt_cache_key` (OpenAI, Azure, and the
    /// compatible APIs OpenCode enables it for); a `setCacheKey` option
    /// overrides.
    pub prompt_cache_key: bool,
    /// Which Anthropic-format cache breakpoints the server takes.
    pub cache_markers: Markers,
    /// xAI: the prompt cache key goes in the `x-grok-conv-id` header.
    pub xai: bool,
    /// The token limit goes in `max_completion_tokens` (OpenAI and Azure)
    /// rather than `max_tokens`.
    pub max_completion_tokens: bool,
    /// No `stream_options.include_usage` (only by explicit override).
    pub no_stream_usage: bool,
    /// A stream may end without a finish reason, which is then inferred
    /// (only by explicit override; otherwise such a stream is cut short).
    pub optional_finish: bool,
}

/// Cache breakpoints a Chat server accepts (`cache_control` in Anthropic's
/// format), after OpenCode's route list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Markers {
    /// None: the server caches by prefix on its own.
    #[default]
    None,
    /// Breakpoints for every model (DigitalOcean).
    All,
    /// A gateway: breakpoints for Anthropic and Qwen models (OpenRouter).
    Gateway,
    /// Alibaba DashScope: Qwen models, without tool breakpoints.
    Alibaba,
}

impl Compat {
    pub fn detect(base_url: &str, model: &str) -> Self {
        let url = base_url.to_ascii_lowercase();
        let id = model.to_ascii_lowercase();
        let openai = url.contains("api.openai.com")
            || url.contains(".openai.azure.com")
            || url.contains(".cognitiveservices.azure.com")
            || url.contains(".services.ai.azure.com");
        let mistral = ["mistral", "devstral", "codestral", "pixtral", "mixtral"]
            .iter()
            .any(|f| id.contains(f));
        Compat {
            openai,
            mistral,
            bridge: mistral,
            claude: id.contains("claude"),
            short_ids: openai || id.starts_with("openai/"),
            require_reasoning: url.contains("deepseek.com") || id.contains("deepseek"),
            reasoning_field: None,
            zai_tool_stream: (url.contains("api.z.ai") || url.contains("open.bigmodel.cn"))
                && !matches!(
                    id.as_str(),
                    "glm-4.5" | "glm-4.5-air" | "glm-4.5-flash" | "glm-4.5v"
                ),
            prompt_cache_key: openai
                || [
                    "api.cerebras.ai",
                    "api.deepinfra.com",
                    "inference.do-ai.run",
                    "api.fireworks.ai",
                    "api.llama.com",
                    "api.moonshot.ai",
                    "api.moonshot.cn",
                    "openrouter.ai",
                    "api.venice.ai",
                ]
                .iter()
                .any(|h| url.contains(h)),
            cache_markers: if url.contains("openrouter.ai") {
                Markers::Gateway
            } else if url.contains("inference.do-ai.run") {
                Markers::All
            } else if url.contains("dashscope") {
                Markers::Alibaba
            } else {
                Markers::None
            },
            xai: url.contains("api.x.ai"),
            max_completion_tokens: openai,
            no_stream_usage: false,
            optional_finish: false,
        }
    }

    /// Detected behaviour with the provider's explicit overrides applied
    /// (OpenCode's model `compatibility`).
    pub fn for_provider(p: &Provider, model: &str) -> Self {
        let mut c = Compat::detect(&p.base_url, model);
        let o = &p.compat;
        match o.max_tokens_field.as_deref() {
            Some("max_completion_tokens") => c.max_completion_tokens = true,
            Some("max_tokens") => c.max_completion_tokens = false,
            _ => {}
        }
        if let Some(u) = o.stream_usage {
            c.no_stream_usage = !u;
        }
        if p.name.eq_ignore_ascii_case("deepseek") {
            c.require_reasoning = true;
        }
        if let Some(f) = o.reasoning_field.as_deref().filter(|f| !f.is_empty()) {
            c.reasoning_field = Some(f.to_string());
            c.require_reasoning = true;
        }
        if let Some(r) = o.require_reasoning {
            c.require_reasoning = r;
        }
        if let Some(b) = o.require_assistant_after_tool {
            c.bridge = b;
        }
        if let Some(r) = o.require_finish_reason {
            c.optional_finish = !r;
        }
        c
    }

    /// The breakpoints this request gets (after OpenCode's `selectPolicy`):
    /// a `cacheFormat` option (`anthropic` or `alibaba`) declares the
    /// format for any server; otherwise the server's own.
    fn cache_policy(&self, req: &Request) -> cache::Policy {
        let auto = req.cache.placement == crate::CachePlacement::Auto;
        let resolved = |p| cache::resolve(&req.cache, p);
        match req.options.str("cacheFormat") {
            Some("alibaba") if auto => return cache::QWEN,
            Some("alibaba") => {
                return cache::Policy {
                    tools: false,
                    ..resolved(cache::AUTO)
                }
            }
            Some("anthropic") => return resolved(cache::AUTO),
            _ => {}
        }
        match self.cache_markers {
            Markers::None => cache::NONE,
            _ if !auto => resolved(cache::NONE),
            Markers::All => cache::AUTO,
            Markers::Gateway => cache::gateway_policy(&req.model),
            Markers::Alibaba if req.model.to_ascii_lowercase().starts_with("qwen") => cache::QWEN,
            Markers::Alibaba => cache::NONE,
        }
    }

    /// The tool call id as this server accepts it (any Gemini thought
    /// signature riding on the id is stripped first).
    pub fn call_id(&self, id: &str) -> String {
        let id = split_id(id).0;
        if self.mistral {
            // Mistral wants exactly 9 alphanumerics. OpenCode keeps the first
            // nine, which makes ids sharing a prefix (`tool<time>…`,
            // `toolu_01…`) collide; an id that does not already fit is
            // hashed in full instead, stably, so a call and its result agree.
            let alnum: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
            let mut s = if alnum.len() <= 9 {
                alnum
            } else {
                mistral_hash(id)
            };
            while s.len() < 9 {
                s.push('0');
            }
            s
        } else if self.claude {
            scrub_id(id)
        } else if self.short_ids {
            id.chars().take(40).collect()
        } else {
            id.to_string()
        }
    }
}

/// Tool ids limited to `[A-Za-z0-9_-]`, as Anthropic requires.
pub(crate) fn scrub_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The request body for a generic OpenAI-compatible server.
#[cfg(test)]
pub fn body(req: &Request) -> Value {
    body_for(req, &Compat::detect("", &req.model))
}

pub fn body_for(req: &Request, compat: &Compat) -> Value {
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    // Chat Completions has no media in tool messages: tool-result media is
    // collected and sent as one user message after the tool messages.
    let mut attachments: Vec<Value> = Vec::new();
    for m in &normalize_tool_history(&req.messages) {
        // Chat has no per-message effort change.
        if effort_update(m).is_some() {
            continue;
        }
        if m.role == Role::System {
            system_update(&mut messages, &mut attachments, m, compat);
            continue;
        }
        if m.role == Role::Assistant {
            flush_attachments(&mut messages, &mut attachments, compat);
            messages.extend(convert(m, compat));
            continue;
        }
        let mut rest = Vec::new();
        for p in &m.parts {
            match p {
                Part::ToolResult {
                    id, content, media, ..
                } => {
                    messages.push(json!({"role": "tool", "tool_call_id": compat.call_id(id), "content": content}));
                    attachments.extend(media.iter().filter_map(lower_media));
                }
                other => rest.push(other.clone()),
            }
        }
        if rest
            .iter()
            .any(|p| matches!(p, Part::Text(_) | Part::Media(_)))
        {
            flush_attachments(&mut messages, &mut attachments, compat);
            bridge(&mut messages, compat);
            messages.extend(convert(
                &Message {
                    role: Role::User,
                    parts: rest,
                },
                compat,
            ));
        }
    }
    flush_attachments(&mut messages, &mut attachments, compat);
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if compat.no_stream_usage {
        body.as_object_mut().map(|b| b.remove("stream_options"));
    }
    if compat.openai {
        body["store"] = json!(false);
    }
    if !req.tools.is_empty() {
        let sanitizer = schema::for_model(&req.model);
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": schema::project(&t.parameters, sanitizer)}}))
            .collect();
        if let Some(c) = &req.tool_choice {
            body["tool_choice"] = match c {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool(name) => json!({"type": "function", "function": {"name": name}}),
            };
        }
        // OpenAI rejects `parallel_tool_calls` without tools.
        if let Some(p) = req.parallel_tool_calls {
            body["parallel_tool_calls"] = json!(p);
        }
    }
    let policy = compat.cache_policy(req);
    let key_ok = req
        .options
        .bool("setCacheKey")
        .unwrap_or(compat.prompt_cache_key);
    if let Some(k) = cache::prompt_cache_key(req).filter(|_| key_ok && !compat.xai) {
        body["prompt_cache_key"] = json!(k);
    }
    // Breakpoints carry the lifetime when there are any.
    if let Some((k, v)) = cache::openai_ttl(req.cache.lifetime()).filter(|_| !policy.places()) {
        body[k] = v;
    }
    if let Some(n) = req.max_tokens {
        let field = if compat.max_completion_tokens {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body[field] = json!(n);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if compat.zai_tool_stream && !req.tools.is_empty() {
        body["tool_stream"] = json!(true);
    }
    merge_body(&mut body, &lower_options(req, compat));
    merge_body(&mut body, &req.body);
    cache::mark_chat(&mut body, policy, req.cache.lifetime());
    body
}

/// Request headers: the provider's own, and xAI's conversation affinity
/// header carrying the prompt cache key (sent whether or not caching is on,
/// as OpenCode does).
pub fn headers(p: &Provider, req: &Request) -> Vec<(String, String)> {
    let mut out = p.headers.clone();
    if p.base_url.to_ascii_lowercase().contains("api.x.ai") {
        if let Some(k) = req.prompt_cache_key.as_deref().filter(|k| !k.is_empty()) {
            if !out
                .iter()
                .any(|(h, _)| h.eq_ignore_ascii_case("x-grok-conv-id"))
            {
                out.push(("x-grok-conv-id".into(), k.into()));
            }
        }
    }
    out
}

/// Body fields from the request's provider options (after OpenCode's Chat
/// `lowerOptions`, plus the reasoning switches OpenAI-compatible servers
/// read: OpenRouter's `reasoning`, Moonshot, Z.ai and DeepSeek `thinking`,
/// Qwen `enable_thinking`/`thinking_budget`, Groq `include_reasoning`). A
/// malformed option is dropped on its own.
fn lower_options(req: &Request, compat: &Compat) -> Map<String, Value> {
    let o = &req.options;
    let mut out = Map::new();
    if compat.openai {
        if let Some(b) = o.bool("store") {
            out.insert("store".into(), json!(b));
        }
    }
    for (key, wire) in [
        ("reasoningEffort", "reasoning_effort"),
        ("textVerbosity", "verbosity"),
        ("serviceTier", "service_tier"),
        ("safetyIdentifier", "safety_identifier"),
    ] {
        if let Some(v) = o.str(key) {
            out.insert(wire.into(), json!(v));
        }
    }
    if let Some(b) = o
        .bool("parallelToolCalls")
        .filter(|_| !req.tools.is_empty())
    {
        out.insert("parallel_tool_calls".into(), json!(b));
    }
    if let Some(m) = o.string_record("metadata") {
        out.insert("metadata".into(), Value::Object(m));
    }
    for (key, wire) in [
        ("topP", "top_p"),
        ("frequencyPenalty", "frequency_penalty"),
        ("presencePenalty", "presence_penalty"),
    ] {
        if let Some(n) = o.num(key) {
            out.insert(wire.into(), json!(n));
        }
    }
    if let Some(n) = o.int("seed") {
        out.insert("seed".into(), json!(n));
    }
    if let Some(s) = o.strings("stop") {
        out.insert("stop".into(), json!(s));
    }
    // OpenRouter passes `reasoning.max_tokens` on as the upstream thinking
    // budget, which must stay under the output limit.
    if let Some(r) = o.object("reasoning") {
        let mut r = r.clone();
        if let Some(n) = r.get("max_tokens").and_then(Value::as_f64) {
            let fitted = crate::options::fit_budget(n, req.max_tokens, 1024.0);
            r.insert("max_tokens".into(), crate::options::number(fitted));
        }
        out.insert("reasoning".into(), Value::Object(r));
    }
    if let Some(t) = o.object("thinking") {
        out.insert("thinking".into(), Value::Object(t.clone()));
    }
    for (key, wire) in [
        ("enableThinking", "enable_thinking"),
        ("includeReasoning", "include_reasoning"),
    ] {
        if let Some(b) = o.bool(key) {
            out.insert(wire.into(), json!(b));
        }
    }
    if let Some(n) = o.int("thinkingBudget") {
        out.insert("thinking_budget".into(), json!(n));
    }
    out
}

/// Mistral wants an assistant turn between tool results and user content.
fn bridge(messages: &mut Vec<Value>, compat: &Compat) {
    if compat.bridge && messages.last().is_some_and(|l| l["role"] == "tool") {
        messages.push(json!({"role": "assistant", "content": "Done."}));
    }
}

fn flush_attachments(messages: &mut Vec<Value>, attachments: &mut Vec<Value>, compat: &Compat) {
    if attachments.is_empty() {
        return;
    }
    bridge(messages, compat);
    messages.push(json!({"role": "user", "content": std::mem::take(attachments)}));
}

/// A system update (after OpenCode's Chat lowering): Chat has no role for
/// one mid-conversation, so it is wrapped user text. It joins the media of
/// the tool results before it, or the user message before it, or starts a
/// user message of its own. Unlike OpenCode, Mistral still gets its
/// assistant turn between tool results and the update.
fn system_update(
    messages: &mut Vec<Value>,
    attachments: &mut Vec<Value>,
    m: &Message,
    compat: &Compat,
) {
    let text = wrap_system_update(m);
    if !attachments.is_empty() {
        bridge(messages, compat);
        let mut content = std::mem::take(attachments);
        content.push(json!({"type": "text", "text": text}));
        messages.push(json!({"role": "user", "content": content}));
        return;
    }
    match messages.last_mut().filter(|l| l["role"] == "user") {
        Some(last) => match &mut last["content"] {
            Value::String(prev) => *prev = format!("{prev}\n{text}"),
            Value::Array(parts) => parts.push(json!({"type": "text", "text": text})),
            other => *other = json!(text),
        },
        None => {
            bridge(messages, compat);
            messages.push(json!({"role": "user", "content": text}));
        }
    }
}

/// A media content part (after OpenCode's `lowerMedia`): PDFs as inline
/// `file` parts, images and video by URL or data URL. `None` for media
/// [`check`] rejects.
fn lower_media(m: &Media) -> Option<Value> {
    match m.kind() {
        MediaKind::Pdf => Some(json!({"type": "file", "file": {
            "filename": m.filename.as_deref().unwrap_or("document.pdf"),
            "file_data": m.data_url()?,
        }})),
        MediaKind::Image => Some(json!({"type": "image_url", "image_url": {"url": m.location()}})),
        MediaKind::Video => Some(json!({"type": "video_url", "video_url": {"url": m.location()}})),
        _ => None,
    }
}

/// Media Chat Completions cannot carry: PDFs by URL, and anything but
/// images, video and PDFs.
pub fn check(req: &Request) -> Result<(), Error> {
    for m in all_media(req) {
        match m.kind() {
            MediaKind::Pdf if m.inline().is_none() => return Err(inline_required(ROUTE)),
            MediaKind::Pdf | MediaKind::Image | MediaKind::Video => {}
            _ => return Err(unsupported_media(ROUTE, m)),
        }
    }
    Ok(())
}

const ROUTE: &str = "OpenAI Chat";

fn convert(m: &Message, compat: &Compat) -> Vec<Value> {
    match m.role {
        // Lowered by `system_update` before this is reached.
        Role::System => vec![],
        Role::User => {
            let has_media = m.parts.iter().any(|p| matches!(p, Part::Media(_)));
            if has_media {
                let content: Vec<Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) => Some(json!({"type": "text", "text": t.as_str()})),
                        Part::Media(media) => lower_media(media),
                        _ => None,
                    })
                    .collect();
                return vec![json!({"role": "user", "content": content})];
            }
            let text: Vec<&str> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect();
            if text.is_empty() {
                return vec![];
            }
            vec![json!({"role": "user", "content": text.join("\n\n")})]
        }
        Role::Assistant => {
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls = Vec::new();
            // Structured reasoning this API streamed, replayed verbatim, and
            // the field the model streamed its reasoning text under.
            let mut details: Option<Vec<Value>> = None;
            let mut opaque: Option<&str> = None;
            let mut field: Option<&str> = None;
            let mut structured_only = true;
            let mut any_reasoning = false;
            for p in &m.parts {
                match p {
                    Part::Text(t) => text.push_str(t),
                    Part::Reasoning(r) => {
                        any_reasoning = true;
                        reasoning.push_str(r);
                        let meta = r.meta_for(Protocol::OpenaiChat);
                        match meta.and_then(|m| m.details.as_ref()) {
                            Some(d) => details
                                .get_or_insert_with(Vec::new)
                                .extend(d.iter().map(replay_detail)),
                            None => structured_only = false,
                        }
                        if let Some(o) = meta.and_then(|m| m.opaque.as_deref()) {
                            opaque = Some(o);
                        }
                        if field.is_none() {
                            field = meta.and_then(|m| m.field.as_deref());
                        }
                    }
                    Part::ToolCall {
                        id, name, input, ..
                    } => {
                        let mut call = json!({
                            "id": compat.call_id(id), "type": "function",
                            "function": {"name": name, "arguments": input.to_string()},
                        });
                        // Gemini's OpenAI endpoint wants its signature back.
                        if let (_, Some(sig)) = split_id(id) {
                            call["extra_content"] = json!({"google": {"thought_signature": sig}});
                        }
                        calls.push(call);
                    }
                    _ => {}
                }
            }
            if text.trim().is_empty() {
                text.clear();
            }
            if compat.mistral {
                reasoning.clear();
                details = None;
                opaque = None;
                any_reasoning = false;
            }
            if text.is_empty() && calls.is_empty() && !any_reasoning {
                return vec![];
            }
            let content = if text.is_empty() && !calls.is_empty() {
                Value::Null
            } else {
                json!(text)
            };
            let mut msg = json!({"role": "assistant", "content": content});
            if !calls.is_empty() {
                msg["tool_calls"] = json!(calls);
            }
            let kimi = details.as_ref().is_some_and(|d| {
                d.iter()
                    .any(|d| matches!(d["type"].as_str(), Some("summary" | "encrypted")))
            });
            // The scalar field (after OpenCode): the one the model streamed
            // under, else `reasoning_content`; fully structured reasoning
            // (OpenRouter) is the details list itself, but Kimi wants
            // `reasoning_content` beside its details.
            let configured = compat.reasoning_field.as_deref();
            let field = match field {
                _ if configured.is_some() && (compat.require_reasoning || any_reasoning) => {
                    configured
                }
                _ if !any_reasoning => compat.require_reasoning.then_some("reasoning_content"),
                Some(f) => Some(f),
                None if !structured_only || compat.require_reasoning || kimi => {
                    Some("reasoning_content")
                }
                None => None,
            };
            let scalar = if !any_reasoning || (opaque.is_some() && reasoning.is_empty()) {
                compat.require_reasoning.then_some("")
            } else {
                Some(reasoning.as_str())
            };
            // A model that declares another reasoning field reads reasoning
            // there: structured state from earlier turns (perhaps another
            // model's) is not sent.
            if configured.is_none_or(|f| f == "reasoning_details") {
                if let Some(d) = details {
                    msg["reasoning_details"] = json!(d);
                }
                if let Some(o) = opaque {
                    msg["reasoning_opaque"] = json!(o);
                }
            }
            if let (Some(f), Some(t)) = (field.filter(|f| *f != "reasoning_details"), scalar) {
                msg[f] = json!(t);
            }
            vec![msg]
        }
    }
}

#[derive(Debug, Default)]
struct Call {
    id: String,
    name: String,
    args: String,
    signature: Option<String>,
    /// The id its input streams under, once its name is known.
    streaming: Option<String>,
}

#[derive(Debug, Default)]
pub struct State {
    calls: Vec<Call>,
    finish: Option<FinishReason>,
    usage: Usage,
    raw_usage: Option<Value>,
    /// `reasoning_details` seen so far (merged), and whether any came.
    details: Vec<Value>,
    details_seen: bool,
    /// The latest `reasoning_opaque`.
    opaque: Option<String>,
    /// The field scalar reasoning text streamed under.
    field: Option<String>,
    /// The field the model declares (read before the known ones).
    configured: Option<String>,
    /// The response's one text block and one reasoning block have started.
    text: bool,
    reasoning: bool,
    /// The stream may end without a finish reason ([`Compat::optional_finish`]).
    pub optional_finish: bool,
}

/// A `reasoning_details` entry in a dialect CodeSmith knows: OpenRouter's
/// `reasoning.text` / `reasoning.summary` / `reasoning.encrypted`, and
/// Kimi's bare `summary` / `encrypted`.
fn known_detail(d: &Value) -> bool {
    match d["type"].as_str() {
        Some("reasoning.text" | "reasoning.summary") => true,
        Some("reasoning.encrypted") => d["data"].is_string(),
        Some("summary") => d["summary"].is_string(),
        Some("encrypted") => d["encrypted"].is_string(),
        _ => false,
    }
}

/// The text a detail shows, if any.
fn detail_text(d: &Value) -> Option<&str> {
    match d["type"].as_str() {
        Some("reasoning.text") => d["text"].as_str(),
        Some("reasoning.summary" | "summary") => d["summary"].as_str(),
        _ => None,
    }
}

/// Merge a detail delta into the previous entry: consecutive text or
/// summary deltas of one block join; encrypted entries never merge.
fn merge_detail(prev: &mut Value, d: &Value) -> bool {
    let kind = d["type"].as_str().unwrap_or("");
    let key = match kind {
        "reasoning.text" => "text",
        "reasoning.summary" | "summary" => "summary",
        _ => return false,
    };
    if prev["type"] != kind {
        return false;
    }
    for k in ["id", "index", "format"] {
        let (a, b) = (&prev[k], &d[k]);
        if !a.is_null() && !b.is_null() && a != b {
            return false;
        }
    }
    let (a, b) = (&prev["signature"], &d["signature"]);
    if a.as_str().is_some_and(|s| !s.is_empty())
        && b.as_str().is_some_and(|s| !s.is_empty())
        && a != b
    {
        return false;
    }
    let joined = format!(
        "{}{}",
        prev[key].as_str().unwrap_or(""),
        d[key].as_str().unwrap_or("")
    );
    if let (Some(p), Some(n)) = (prev.as_object_mut(), d.as_object()) {
        for (k, v) in n {
            if !v.is_null() && !(k == "signature" && v.as_str() == Some("")) {
                p.insert(k.clone(), v.clone());
            }
        }
        p.insert(key.into(), json!(joined));
    }
    true
}

/// A stored detail as it is sent back: Kimi rejects its stream-only `index`.
fn replay_detail(d: &Value) -> Value {
    match d["type"].as_str() {
        Some("summary") => json!({"type": "summary", "summary": d["summary"]}),
        Some("encrypted") => json!({"type": "encrypted", "encrypted": d["encrypted"]}),
        _ => d.clone(),
    }
}

impl State {
    /// A stream from a server with `compat`.
    pub fn new(compat: &Compat) -> Self {
        State {
            configured: compat.reasoning_field.clone(),
            optional_finish: compat.optional_finish,
            ..Default::default()
        }
    }

    fn text(&mut self, t: &str, out: &mut Vec<Event>) {
        if !std::mem::replace(&mut self.text, true) {
            out.push(Event::TextStart("text-0".into()));
        }
        out.push(Event::TextDelta(t.to_string()));
    }

    fn reasoning(&mut self, t: &str, out: &mut Vec<Event>) {
        if !std::mem::replace(&mut self.reasoning, true) {
            out.push(Event::ReasoningStart("reasoning-0".into()));
        }
        out.push(Event::ReasoningDelta(t.to_string()));
    }

    /// The slot a tool-call delta belongs to: its index when sent, else the
    /// call with its id, else a new call for a new id, else the latest call.
    fn slot(&mut self, c: &Value) -> &mut Call {
        let id = c["id"].as_str().filter(|s| !s.is_empty());
        let i = match c["index"].as_u64() {
            Some(i) => i as usize,
            None => match id.and_then(|id| self.calls.iter().position(|s| s.id == id)) {
                Some(i) => i,
                None if id.is_some() || self.calls.is_empty() => self.calls.len(),
                None => self.calls.len() - 1,
            },
        };
        while self.calls.len() <= i {
            self.calls.push(Call::default());
        }
        &mut self.calls[i]
    }
}

const STREAM_ROUTE: &str = "openai/openai-chat";

/// Whether a chunk's fields have the types OpenCode's Chat event schema
/// requires. `content` may also be a list of typed chunks (Mistral).
fn valid_chunk(v: &Value) -> bool {
    use crate::shape::{all, array_of, field, null_num, null_str};
    let usage = |u: &Value| {
        u.is_object()
            && all(
                u,
                &[
                    "prompt_tokens",
                    "completion_tokens",
                    "total_tokens",
                    "cached_tokens",
                    "prompt_cache_hit_tokens",
                    "cache_read_input_tokens",
                    "cache_created_input_tokens",
                    "reasoning_tokens",
                ],
                null_num,
            )
            && field(u, "prompt_tokens_details", true, |d| {
                d.is_object() && all(d, &["cached_tokens", "cache_write_tokens"], null_num)
            })
            && field(u, "completion_tokens_details", true, |d| {
                d.is_object()
                    && all(
                        d,
                        &[
                            "reasoning_tokens",
                            "accepted_prediction_tokens",
                            "rejected_prediction_tokens",
                        ],
                        null_num,
                    )
            })
    };
    let call = |c: &Value| {
        c.is_object()
            && null_num(c, "index")
            && null_str(c, "id")
            && field(c, "function", true, |f| {
                f.is_object() && all(f, &["name", "arguments"], null_str)
            })
    };
    let delta = |d: &Value| {
        d.is_object()
            && field(d, "content", true, |c| c.is_string() || c.is_array())
            && all(
                d,
                &[
                    "refusal",
                    "reasoning_content",
                    "reasoning",
                    "reasoning_text",
                    "reasoning_opaque",
                ],
                null_str,
            )
            && array_of(d, "tool_calls", true, call)
    };
    let choice = |c: &Value| {
        c.is_object()
            && field(c, "delta", true, delta)
            && all(c, &["finish_reason", "native_finish_reason"], null_str)
            && field(c, "usage", true, usage)
    };
    array_of(v, "choices", true, choice) && field(v, "usage", true, usage)
}

/// Events produced by one `data:` chunk. A chunk of the wrong shape, tool
/// call changes after the finish reason, and a call still without a name
/// when the response finishes are invalid output (as in OpenCode).
pub fn handle(state: &mut State, v: &Value) -> Result<Vec<Event>, Error> {
    if !v.is_null() && !(v.is_object() && valid_chunk(v)) {
        return Err(crate::shape::invalid(STREAM_ROUTE, v));
    }
    let mut out = Vec::new();
    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
        state.usage = usage(u);
        state.usage.cost = crate::billing::billed(u, &[v]);
        state.raw_usage = Some(u.clone());
    }
    let Some(choice) = v["choices"].get(0) else {
        return Ok(out);
    };
    let delta = &choice["delta"];
    let changes_call = |c: &Value| {
        let set = |x: &Value| x.as_str().is_some_and(|s| !s.is_empty());
        set(&c["id"]) || set(&c["function"]["name"]) || set(&c["function"]["arguments"])
    };
    let calls = delta["tool_calls"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if state.finish.is_some() && calls.iter().any(changes_call) {
        return Err(Error::Decode(format!(
            "OpenAI Chat received content after the finish reason: {v}"
        )));
    }
    // Servers name the field differently; some send the same text under
    // two. A field the model declares is read first, and is the one stored.
    let scalar = state
        .configured
        .iter()
        .map(String::as_str)
        .chain(["reasoning_content", "reasoning", "reasoning_text"])
        .find_map(|k| Some((k, delta[k].as_str().filter(|s| !s.is_empty())?)));
    if let Some((k, r)) = scalar {
        let k = state.configured.clone().unwrap_or_else(|| k.to_string());
        state.field.get_or_insert(k);
        state.reasoning(r, &mut out);
    }
    if let Some(ds) = delta["reasoning_details"].as_array() {
        state.details_seen = true;
        for d in ds.iter().filter(|d| known_detail(d)) {
            // Details show as reasoning when no scalar reasoning streams.
            if state.field.is_none() {
                if let Some(t) = detail_text(d).filter(|t| !t.is_empty()) {
                    state.reasoning(t, &mut out);
                }
            }
            let merged = state
                .details
                .last_mut()
                .is_some_and(|prev| merge_detail(prev, d));
            if !merged {
                state.details.push(d.clone());
            }
        }
    }
    if let Some(o) = delta["reasoning_opaque"].as_str().filter(|s| !s.is_empty()) {
        state.opaque = Some(o.to_string());
    }
    // Reasoning that is only state (details without text, an opaque value)
    // still comes before the output that follows it.
    let output = [&delta["content"], &delta["refusal"]]
        .iter()
        .any(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        || delta["tool_calls"]
            .as_array()
            .is_some_and(|c| !c.is_empty());
    if (state.details_seen || state.opaque.is_some()) && output && !state.reasoning {
        state.reasoning = true;
        out.push(Event::ReasoningStart("reasoning-0".into()));
    }
    if let Some(t) = delta["content"].as_str().filter(|s| !s.is_empty()) {
        state.text(t, &mut out);
    }
    // A refusal is ordinary assistant text.
    if let Some(t) = delta["refusal"].as_str().filter(|s| !s.is_empty()) {
        state.text(t, &mut out);
    }
    // Mistral streams content as typed chunks, reasoning among them.
    for part in delta["content"].as_array().into_iter().flatten() {
        match part["type"].as_str() {
            Some("text") => {
                if let Some(t) = part["text"].as_str().filter(|s| !s.is_empty()) {
                    state.text(t, &mut out);
                }
            }
            Some("thinking") => {
                for t in part["thinking"].as_array().into_iter().flatten() {
                    if let Some(t) = t["text"].as_str().filter(|s| !s.is_empty()) {
                        state.reasoning(t, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    for c in delta["tool_calls"].as_array().into_iter().flatten() {
        let slot = state.slot(c);
        // Later deltas may repeat or blank the identity; the first one wins.
        if let Some(id) = c["id"].as_str().filter(|s| !s.is_empty()) {
            if slot.id.is_empty() {
                slot.id = id.to_string();
            }
        }
        if let Some(n) = c["function"]["name"].as_str().filter(|s| !s.is_empty()) {
            if slot.name.is_empty() {
                slot.name = n.to_string();
            }
        }
        let before = slot.args.len();
        if let Some(a) = c["function"]["arguments"].as_str() {
            slot.args.push_str(a);
        }
        if let Some(sig) = c["extra_content"]["google"]["thought_signature"].as_str() {
            slot.signature = Some(sig.to_string());
        }
        // Input streams once the call has a name; arguments sent before it
        // go out with its start.
        let from = match &slot.streaming {
            Some(_) => before,
            None if !slot.name.is_empty() => {
                let id = match slot.id.as_str() {
                    "" => crate::fresh_id(),
                    id => id.to_string(),
                };
                slot.streaming = Some(id.clone());
                out.push(Event::ToolInputStart {
                    id,
                    name: slot.name.clone(),
                });
                0
            }
            None => slot.args.len(),
        };
        if let (Some(id), Some(text)) = (&slot.streaming, slot.args.get(from..)) {
            if !text.is_empty() {
                out.push(Event::ToolInputDelta {
                    id: id.clone(),
                    text: text.to_string(),
                });
            }
        }
    }
    // The first finish reason stands; some servers send late chunks after it.
    if let Some(f) = choice["finish_reason"].as_str().filter(|f| !f.is_empty()) {
        if state.finish.is_none() {
            let reason = finish_reason(choice, f);
            // A finish that confirms the calls needs each one named; one
            // cut off by the limit or a filter is dropped anyway.
            let confirms = !reason.is_length() && !reason.is_content_filter();
            if confirms && state.calls.iter().any(|c| c.name.is_empty()) {
                return Err(Error::Decode(format!(
                    "OpenAI Chat tool call delta is missing id or name: {v}"
                )));
            }
            state.finish = Some(reason);
        }
    }
    Ok(out)
}

/// `prompt_tokens` includes cache reads and writes; `Usage.input` does not.
/// Providers disagree on where the cache counts live.
fn usage(u: &Value) -> Usage {
    let n = |v: &Value| v.as_u64();
    let cache_read = n(&u["prompt_tokens_details"]["cached_tokens"])
        .or(n(&u["prompt_cache_hit_tokens"]))
        .or(n(&u["cached_tokens"]))
        .or(n(&u["cache_read_input_tokens"]))
        .unwrap_or(0);
    let cache_write = n(&u["prompt_tokens_details"]["cache_write_tokens"])
        .or(n(&u["cache_created_input_tokens"]))
        .unwrap_or(0);
    let reported = (n(&u["prompt_tokens"]), n(&u["completion_tokens"]));
    let prompt = reported.0.unwrap_or(0);
    let completion = reported.1.unwrap_or(0);
    let reasoning = n(&u["completion_tokens_details"]["reasoning_tokens"]);
    // A top-level `reasoning_tokens` (GitHub Copilot) is outside
    // `completion_tokens` when the total counts it separately.
    let extra = match (n(&u["reasoning_tokens"]), n(&u["total_tokens"])) {
        (Some(r), Some(total)) if total >= prompt + completion + r => r,
        _ => 0,
    };
    let output = completion + extra;
    Usage {
        input: prompt.saturating_sub(cache_read + cache_write),
        output,
        cache_read,
        cache_write,
        cost: None,
        reasoning: reasoning.or(n(&u["reasoning_tokens"])).unwrap_or(0),
        total: n(&u["total_tokens"]).or(match reported {
            (None, None) => None,
            _ => Some(prompt + output),
        }),
    }
}

/// A Chat finish reason in the common algebra. Servers' aliases for the
/// output limit (`max_tokens`, vLLM's `model_length`) count as length too.
/// A content-filter finish names the filter that fired, when the chunk says
/// (GitHub Copilot's `copilot_annotations`, Azure's
/// `content_filter_results`).
fn finish_reason(choice: &Value, raw: &str) -> FinishReason {
    let normalized = match raw {
        "stop" | "end" => Reason::Stop,
        "length" | "max_tokens" | "max_output_tokens" | "model_length" => Reason::Length,
        "content_filter" => Reason::ContentFilter,
        "function_call" | "tool_calls" => Reason::ToolCalls,
        // Failures, raised when the stream ends.
        "error" | "network_error" => return FinishReason::new(Reason::Error, raw),
        _ => Reason::Unknown,
    };
    let native = choice["native_finish_reason"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(raw);
    let mut r = FinishReason::new(normalized, native);
    if raw == "content_filter" {
        r.category = crate::filter_category(&[
            &choice["delta"]["copilot_annotations"],
            &choice["content_filter_results"],
        ]);
    }
    r
}

/// Events at end of stream: the accumulated tool calls, then `Finish`.
/// Calls cut off by the token limit or a content filter are dropped, and a
/// finish reason that reports a provider failure is an error.
pub fn finish(state: &mut State) -> Result<Vec<Event>, Error> {
    if state.finish.is_none() && !state.optional_finish {
        return Err(Error::Transport(
            "OpenAI Chat stream ended without finish_reason".into(),
        ));
    }
    let reason = state.finish.take().unwrap_or_else(|| Reason::Stop.into());
    let failed = reason.normalized == Reason::Error;
    match reason.raw.as_deref().filter(|_| failed).unwrap_or("") {
        "network_error" => {
            return Err(Error::Provider {
                message: "provider reported a network error (finish_reason: network_error)".into(),
                retryable: true,
            })
        }
        "error" => {
            return Err(Error::Provider {
                message: "provider reported an error (finish_reason: error)".into(),
                retryable: false,
            })
        }
        _ => {}
    }
    let mut out = Vec::new();
    if state.text {
        out.push(Event::TextEnd("text-0".into()));
    }
    let named = state
        .field
        .as_deref()
        .is_some_and(|f| f != "reasoning_content");
    if state.details_seen || state.opaque.is_some() || named {
        let mut meta = ReasoningMeta::of(Protocol::OpenaiChat);
        // `reasoning_content` is where reasoning goes back by default.
        meta.field = state.field.take().filter(|f| f != "reasoning_content");
        if state.details_seen {
            meta.details = Some(std::mem::take(&mut state.details));
        }
        meta.opaque = state.opaque.take();
        out.push(Event::ReasoningEnd(meta));
    }
    let calls = std::mem::take(&mut state.calls);
    let keep = !reason.is_length() && !reason.is_content_filter();
    for c in calls {
        if let Some(id) = &c.streaming {
            out.push(Event::ToolInputEnd { id: id.clone() });
        }
        if keep && !c.name.is_empty() {
            let input = if c.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.args).unwrap_or_else(|_| json!({"__invalid_json": c.args}))
            };
            let id = match (c.id.is_empty(), c.streaming) {
                (false, _) => c.id,
                (true, Some(id)) => id,
                (true, None) => crate::fresh_id(),
            };
            let id = match c.signature {
                Some(sig) => format!("{id}~{sig}"),
                None => id,
            };
            out.push(Event::ToolCall {
                id,
                name: c.name,
                input,
            });
        }
    }
    out.push(Event::Finish {
        reason,
        usage: state.usage,
        raw_usage: state.raw_usage.take(),
    });
    Ok(out)
}

/// An `error` object inside a stream chunk.
pub(crate) fn chunk_error(v: &Value) -> Error {
    let e = &v["error"];
    let code = e["code"]
        .as_str()
        .map(str::to_string)
        .or_else(|| e["code"].as_u64().map(|n| n.to_string()))
        .or_else(|| e["type"].as_str().map(str::to_string));
    let err = stream_error(code.as_deref(), e["message"].as_str(), v, true);
    // A bare status code is not worth showing next to the message.
    match (err, e["message"].as_str().filter(|m| !m.is_empty())) {
        (Error::Provider { retryable, .. }, Some(m)) if e["code"].is_number() => Error::Provider {
            message: m.to_string(),
            retryable,
        },
        (err, _) => err,
    }
}

#[derive(Default)]
pub struct Decode {
    state: State,
    done: bool,
}

impl Decode {
    /// A decoder for a server with `compat`: its stream rules and declared
    /// reasoning field.
    pub fn new(compat: &Compat) -> Self {
        Decode {
            state: State::new(compat),
            done: false,
        }
    }
}

impl Decoder for Decode {
    fn frame(&mut self, f: &sse::Frame) -> Result<Vec<Event>, Error> {
        if f.data.trim() == "[DONE]" {
            self.done = true;
            return Ok(vec![]);
        }
        let v: Value =
            serde_json::from_str(&f.data).map_err(|e| Error::Decode(format!("{e}: {}", f.data)))?;
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return Err(chunk_error(&v));
        }
        handle(&mut self.state, &v)
    }
    fn done(&self) -> bool {
        self.done
    }
    fn end(&mut self) -> Result<Vec<Event>, Error> {
        finish(&mut self.state)
    }
}

pub async fn stream(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<EventStream, Error> {
    let resp = crate::endpoint::send(request(p, client, req)).await?;
    let compat = Compat::for_provider(p, &req.model);
    Ok(drive(resp, Decode::new(&compat)))
}

/// Compatibility overrides Chat cannot honour: a declared reasoning field
/// that is one of the assistant message's own fields.
pub fn check_compat(p: &Provider) -> Result<(), Error> {
    match p.compat.reasoning_field.as_deref() {
        Some(f @ ("role" | "content" | "refusal" | "tool_calls")) => Err(Error::Invalid(format!(
            "{ROUTE} reasoning field conflicts with reserved field {f}"
        ))),
        _ => Ok(()),
    }
}

/// The Chat Completions request for `req`.
pub(crate) fn request(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> reqwest::RequestBuilder {
    let compat = Compat::for_provider(p, &req.model);
    p.json_post(
        client,
        "chat/completions",
        headers(p, req),
        &body_for(req, &compat),
    )
}

/// Incremental UTF-8 decoding that never splits a multi-byte character.
#[derive(Default)]
pub(crate) struct Utf8 {
    pending: Vec<u8>,
}

impl Utf8 {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => {
                let s = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                return s;
            }
        };
        let rest = self.pending.split_off(valid);

        String::from_utf8(std::mem::replace(&mut self.pending, rest)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolSpec;

    fn handle(s: &mut State, v: &Value) -> Vec<Event> {
        super::handle(s, v).expect("valid chunk")
    }

    #[test]
    fn body_shape() {
        let req = Request {
            model: "m".into(),
            system: "sys".into(),
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Reasoning("think".into()),
                        Part::Text("ok".into()),
                        Part::ToolCall {
                            id: "c1".into(),
                            name: "read".into(),
                            input: json!({"path": "a"}),
                            meta: None,
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        id: "c1".into(),
                        name: "read".into(),
                        content: "data".into(),
                        is_error: false,
                        media: vec![],
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "d".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: Some(100),
            ..Default::default()
        };
        let b = body(&req);
        assert_eq!(
            b["messages"][0],
            json!({"role": "system", "content": "sys"})
        );
        assert_eq!(b["messages"][1], json!({"role": "user", "content": "hi"}));
        assert_eq!(b["messages"][2]["content"], "ok");
        assert_eq!(
            b["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(
            b["messages"][3],
            json!({"role": "tool", "tool_call_id": "c1", "content": "data"})
        );
        assert_eq!(b["tools"][0]["function"]["name"], "read");
        assert_eq!(b["max_tokens"], 100);
        assert_eq!(b["stream"], true);
    }

    #[test]
    fn assistant_without_text_has_null_content() {
        let m = Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall {
                id: "c".into(),
                name: "x".into(),
                input: json!({}),
                meta: None,
            }],
        };
        assert_eq!(convert(&m, &Compat::default())[0]["content"], Value::Null);
    }

    #[test]
    fn no_tools_key_without_tools() {
        assert!(body(&Request::default()).get("tools").is_none());
    }

    #[test]
    fn streams_text_reasoning_and_usage() {
        let mut s = State::default();
        assert_eq!(
            crate::content(handle(
                &mut s,
                &json!({"choices": [{"delta": {"reasoning_content": "hmm"}}]})
            )),
            vec![Event::ReasoningDelta("hmm".into())]
        );
        assert_eq!(
            crate::content(handle(
                &mut s,
                &json!({"choices": [{"delta": {"content": "Hel"}}]})
            )),
            vec![Event::TextDelta("Hel".into())]
        );
        handle(
            &mut s,
            &json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        );
        handle(
            &mut s,
            &json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 4}}}),
        );
        assert_eq!(
            crate::content(finish(&mut s).unwrap()),
            vec![Event::Finish {
                reason: FinishReason::new(Reason::Stop, "stop"),
                usage: Usage {
                    input: 6,
                    output: 3,
                    cache_read: 4,
                    cache_write: 0,
                    reasoning: 0,
                    total: Some(13),
                    ..Default::default()
                },
                raw_usage: Some(
                    json!({"prompt_tokens": 10, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 4}})
                ),
            }]
        );
    }

    #[test]
    fn usage_input_excludes_cache_reads_and_writes() {
        let u = |v: Value| usage(&v);
        assert_eq!(
            u(
                json!({"prompt_tokens": 5762, "completion_tokens": 4, "cache_created_input_tokens": 5759, "cache_read_input_tokens": 0, "prompt_tokens_details": {"cached_tokens": 0}})
            ),
            Usage {
                input: 3,
                output: 4,
                cache_read: 0,
                cache_write: 5759,
                reasoning: 0,
                total: Some(5766),
                ..Default::default()
            }
        );
        assert_eq!(
            u(json!({"prompt_tokens": 101, "completion_tokens": 9, "cached_tokens": 101})),
            Usage {
                input: 0,
                output: 9,
                cache_read: 101,
                cache_write: 0,
                reasoning: 0,
                total: Some(110),
                ..Default::default()
            }
        );
        assert_eq!(
            u(
                json!({"prompt_tokens": 20, "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 8, "cache_write_tokens": 2}})
            ),
            Usage {
                input: 10,
                output: 1,
                cache_read: 8,
                cache_write: 2,
                reasoning: 0,
                total: Some(21),
                ..Default::default()
            }
        );
    }

    #[test]
    fn typed_content_chunks() {
        let mut s = State::default();
        assert_eq!(
            crate::content(handle(
                &mut s,
                &json!({"choices": [{"delta": {"content": [
                    {"type": "thinking", "thinking": [{"type": "text", "text": "17 times"}]},
                    {"type": "text", "text": "391"}
                ]}}]})
            )),
            vec![
                Event::ReasoningDelta("17 times".into()),
                Event::TextDelta("391".into())
            ]
        );
    }

    #[test]
    fn accumulates_parallel_tool_calls() {
        let mut s = State::default();
        handle(
            &mut s,
            &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "a", "function": {"name": "read", "arguments": "{\"pa"}}]}}]}),
        );
        handle(
            &mut s,
            &json!({"choices": [{"delta": {"tool_calls": [{"index": 1, "id": "b", "function": {"name": "glob", "arguments": "{}"}}]}}]}),
        );
        handle(
            &mut s,
            &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"x\"}"}}]}, "finish_reason": "tool_calls"}]}),
        );
        let ev = crate::content(finish(&mut s).unwrap());
        assert_eq!(
            ev[0],
            Event::ToolCall {
                id: "a".into(),
                name: "read".into(),
                input: json!({"path": "x"})
            }
        );
        assert_eq!(
            ev[1],
            Event::ToolCall {
                id: "b".into(),
                name: "glob".into(),
                input: json!({})
            }
        );
        assert!(matches!(&ev[2], Event::Finish { reason, .. } if reason.as_str() == "tool_calls"));
    }

    #[test]
    fn invalid_tool_arguments_are_kept_for_repair() {
        let mut s = State::default();
        handle(
            &mut s,
            &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "a", "function": {"name": "read", "arguments": "{oops"}}]}, "finish_reason": "tool_calls"}]}),
        );
        assert_eq!(
            crate::content(finish(&mut s).unwrap())[0],
            Event::ToolCall {
                id: "a".into(),
                name: "read".into(),
                input: json!({"__invalid_json": "{oops"})
            }
        );
    }

    #[test]
    fn missing_call_id_gets_one() {
        let mut s = State::default();
        handle(
            &mut s,
            &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "glob", "arguments": ""}}]}, "finish_reason": "tool_calls"}]}),
        );
        assert!(matches!(
            &crate::content(finish(&mut s).unwrap())[0],
            Event::ToolCall { id, name, input } if id.starts_with("tool_") && name == "glob" && *input == json!({})
        ));
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut d = Utf8::default();
        let bytes = "é✓".as_bytes();
        assert_eq!(d.push(&bytes[..1]), "");
        assert_eq!(d.push(&bytes[1..3]), "é");
        assert_eq!(d.push(&bytes[3..]), "✓");
    }
}

/// 9 base-62 characters from a 64-bit FNV-1a hash of the whole id.
fn mistral_hash(id: &str) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (0..9)
        .map(|_| {
            let d = DIGITS[(h % 62) as usize] as char;
            h /= 62;
            d
        })
        .collect()
}

#[cfg(test)]
mod mistral_id_tests {
    use super::*;

    #[test]
    fn long_ids_with_a_shared_prefix_do_not_collide() {
        let c = Compat::detect("http://localhost:1234/v1", "devstral-small");
        let a = c.call_id("tool18e9a0001");
        let b = c.call_id("tool18e9a0002");
        let x = c.call_id("toolu_01A0aaaaaaaaaaaa");
        let y = c.call_id("toolu_01A0bbbbbbbbbbbb");
        assert_ne!(a, b);
        assert_ne!(x, y);
        for id in [&a, &b, &x, &y] {
            assert!(
                id.len() == 9 && id.chars().all(|c| c.is_ascii_alphanumeric()),
                "{id}"
            );
        }
        assert_eq!(c.call_id("tool18e9a0001"), a, "stable");
        assert_eq!(
            c.call_id("abc"),
            "abc000000",
            "short ids keep OpenCode's padding"
        );
        assert_eq!(
            c.call_id("call_12345"),
            "call12345",
            "ids that already fit are kept"
        );
    }
}
