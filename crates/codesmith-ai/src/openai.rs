//! OpenAI Chat Completions streaming (and OpenAI-compatible servers).

use crate::gemini::split_id;
use crate::{
    all_media, drive, inline_required, merge_body, normalize_tool_history, post, schema, sse,
    stream_error, unsupported_media, Decoder, Error, Event, EventStream, Media, MediaKind, Message,
    Part, Protocol, Provider, ReasoningMeta, Request, Role, Usage,
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
    /// assistant turn between tool results and user text, and no replayed
    /// reasoning field.
    pub mistral: bool,
    /// Claude models behind a gateway: tool call ids limited to `[A-Za-z0-9_-]`.
    pub claude: bool,
    /// OpenAI models (direct or `openai/...` behind a gateway): tool call ids
    /// of at most 40 characters.
    pub short_ids: bool,
    /// DeepSeek: every assistant message carries `reasoning_content`.
    pub require_reasoning: bool,
    /// Z.ai GLM models after 4.5 stream tool calls only with `tool_stream`.
    pub zai_tool_stream: bool,
}

impl Compat {
    pub fn detect(base_url: &str, model: &str) -> Self {
        let url = base_url.to_ascii_lowercase();
        let id = model.to_ascii_lowercase();
        let openai = url.contains("api.openai.com")
            || url.contains(".openai.azure.com")
            || url.contains(".cognitiveservices.azure.com")
            || url.contains(".services.ai.azure.com");
        Compat {
            openai,
            mistral: ["mistral", "devstral", "codestral", "pixtral", "mixtral"]
                .iter()
                .any(|f| id.contains(f)),
            claude: id.contains("claude"),
            short_ids: openai || id.starts_with("openai/"),
            require_reasoning: url.contains("deepseek.com") || id.contains("deepseek"),
            zai_tool_stream: (url.contains("api.z.ai") || url.contains("open.bigmodel.cn"))
                && !matches!(
                    id.as_str(),
                    "glm-4.5" | "glm-4.5-air" | "glm-4.5-flash" | "glm-4.5v"
                ),
        }
    }

    /// The tool call id as this server accepts it (any Gemini thought
    /// signature riding on the id is stripped first).
    pub fn call_id(&self, id: &str) -> String {
        let id = split_id(id).0;
        if self.mistral {
            let mut s: String = id
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(9)
                .collect();
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
    }
    if let Some(n) = req.max_tokens {
        let field = if compat.openai {
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
    body
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
    if compat.mistral && messages.last().is_some_and(|l| l["role"] == "tool") {
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
        Role::User => {
            let has_media = m.parts.iter().any(|p| matches!(p, Part::Media(_)));
            if has_media {
                let content: Vec<Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) => Some(json!({"type": "text", "text": t})),
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
                    Part::ToolCall { id, name, input } => {
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
            let field = match field {
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
            if let Some(d) = details {
                msg["reasoning_details"] = json!(d);
            }
            if let Some(o) = opaque {
                msg["reasoning_opaque"] = json!(o);
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
}

#[derive(Debug, Default)]
pub struct State {
    calls: Vec<Call>,
    finish: Option<String>,
    usage: Usage,
    /// `reasoning_details` seen so far (merged), and whether any came.
    details: Vec<Value>,
    details_seen: bool,
    /// The latest `reasoning_opaque`.
    opaque: Option<String>,
    /// The field scalar reasoning text streamed under.
    field: Option<String>,
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

/// Events produced by one `data:` chunk.
pub fn handle(state: &mut State, v: &Value) -> Vec<Event> {
    let mut out = Vec::new();
    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
        state.usage = usage(u);
    }
    let Some(choice) = v["choices"].get(0) else {
        return out;
    };
    let delta = &choice["delta"];
    // Servers name the field differently; some send the same text under two.
    let scalar = ["reasoning_content", "reasoning", "reasoning_text"]
        .iter()
        .find_map(|k| Some((*k, delta[k].as_str().filter(|s| !s.is_empty())?)));
    if let Some((k, r)) = scalar {
        state.field.get_or_insert_with(|| k.to_string());
        out.push(Event::ReasoningDelta(r.to_string()));
    }
    if let Some(ds) = delta["reasoning_details"].as_array() {
        state.details_seen = true;
        for d in ds.iter().filter(|d| known_detail(d)) {
            // Details show as reasoning when no scalar reasoning streams.
            if state.field.is_none() {
                if let Some(t) = detail_text(d).filter(|t| !t.is_empty()) {
                    out.push(Event::ReasoningDelta(t.to_string()));
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
    if let Some(t) = delta["content"].as_str().filter(|s| !s.is_empty()) {
        out.push(Event::TextDelta(t.to_string()));
    }
    // A refusal is ordinary assistant text.
    if let Some(t) = delta["refusal"].as_str().filter(|s| !s.is_empty()) {
        out.push(Event::TextDelta(t.to_string()));
    }
    // Mistral streams content as typed chunks, reasoning among them.
    for part in delta["content"].as_array().into_iter().flatten() {
        match part["type"].as_str() {
            Some("text") => {
                if let Some(t) = part["text"].as_str().filter(|s| !s.is_empty()) {
                    out.push(Event::TextDelta(t.to_string()));
                }
            }
            Some("thinking") => {
                for t in part["thinking"].as_array().into_iter().flatten() {
                    if let Some(t) = t["text"].as_str().filter(|s| !s.is_empty()) {
                        out.push(Event::ReasoningDelta(t.to_string()));
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
        if let Some(a) = c["function"]["arguments"].as_str() {
            slot.args.push_str(a);
        }
        if let Some(sig) = c["extra_content"]["google"]["thought_signature"].as_str() {
            slot.signature = Some(sig.to_string());
        }
    }
    // The first finish reason stands; some servers send late chunks after it.
    if let Some(f) = choice["finish_reason"].as_str().filter(|f| !f.is_empty()) {
        state.finish.get_or_insert_with(|| f.to_string());
    }
    out
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
    let prompt = n(&u["prompt_tokens"]).unwrap_or(0);
    let completion = n(&u["completion_tokens"]).unwrap_or(0);
    // A top-level `reasoning_tokens` (GitHub Copilot) is outside
    // `completion_tokens` when the total counts it separately.
    let extra = match (n(&u["reasoning_tokens"]), n(&u["total_tokens"])) {
        (Some(r), Some(total)) if total >= prompt + completion + r => r,
        _ => 0,
    };
    Usage {
        input: prompt.saturating_sub(cache_read + cache_write),
        output: completion + extra,
        cache_read,
        cache_write,
    }
}

/// Events at end of stream: the accumulated tool calls, then `Finish`.
/// Calls cut off by the token limit or a content filter are dropped, and a
/// finish reason that reports a provider failure is an error.
pub fn finish(state: &mut State) -> Result<Vec<Event>, Error> {
    let reason = state.finish.take().unwrap_or_else(|| "stop".into());
    match reason.as_str() {
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
    if !matches!(reason.as_str(), "length" | "content_filter") {
        for c in calls {
            if c.name.is_empty() {
                continue;
            }
            let input = if c.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.args).unwrap_or_else(|_| json!({"__invalid_json": c.args}))
            };
            let id = if c.id.is_empty() {
                crate::fresh_id()
            } else {
                c.id
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
        Ok(handle(&mut self.state, &v))
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
    let url = format!("{}/chat/completions", p.base_url.trim_end_matches('/'));
    let mut rb = client.post(url);
    if let Some(k) = &p.api_key {
        rb = rb.bearer_auth(k);
    }
    for (k, v) in &p.headers {
        rb = rb.header(k, v);
    }
    let compat = Compat::detect(&p.base_url, &req.model);
    let resp = post(rb, &body_for(req, &compat)).await?;
    Ok(drive(resp, Decode::default()))
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
            handle(
                &mut s,
                &json!({"choices": [{"delta": {"reasoning_content": "hmm"}}]})
            ),
            vec![Event::ReasoningDelta("hmm".into())]
        );
        assert_eq!(
            handle(&mut s, &json!({"choices": [{"delta": {"content": "Hel"}}]})),
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
            finish(&mut s).unwrap(),
            vec![Event::Finish {
                reason: "stop".into(),
                usage: Usage {
                    input: 6,
                    output: 3,
                    cache_read: 4,
                    cache_write: 0
                }
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
                cache_write: 5759
            }
        );
        assert_eq!(
            u(json!({"prompt_tokens": 101, "completion_tokens": 9, "cached_tokens": 101})),
            Usage {
                input: 0,
                output: 9,
                cache_read: 101,
                cache_write: 0
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
                cache_write: 2
            }
        );
    }

    #[test]
    fn typed_content_chunks() {
        let mut s = State::default();
        assert_eq!(
            handle(
                &mut s,
                &json!({"choices": [{"delta": {"content": [
                    {"type": "thinking", "thinking": [{"type": "text", "text": "17 times"}]},
                    {"type": "text", "text": "391"}
                ]}}]})
            ),
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
        let ev = finish(&mut s).unwrap();
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
        assert!(matches!(&ev[2], Event::Finish { reason, .. } if reason == "tool_calls"));
    }

    #[test]
    fn invalid_tool_arguments_are_kept_for_repair() {
        let mut s = State::default();
        handle(
            &mut s,
            &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "a", "function": {"name": "read", "arguments": "{oops"}}]}}]}),
        );
        assert_eq!(
            finish(&mut s).unwrap()[0],
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
            &json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "glob", "arguments": ""}}]}}]}),
        );
        assert!(matches!(
            &finish(&mut s).unwrap()[0],
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
