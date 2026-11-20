//! Anthropic Messages streaming.

use crate::gemini::split_id;
use crate::openai::scrub_id;
use crate::options::{claude_version, field, fit_budget, number};
use crate::{
    all_media, drive, inline_required, merge_body, normalize_tool_history, post, schema, sse,
    stream_error, unsupported_media, Decoder, Error, Event, EventStream, Media, MediaKind, Part,
    Protocol, Provider, ReasoningMeta, Request, Role, Usage,
};
use serde_json::{json, Map, Value};

pub(crate) const VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 32_000;
const MIN_THINKING_BUDGET: f64 = 1024.0;
const INTERLEAVED_BETA: &str = "interleaved-thinking-2025-05-14";
const COMPACT_BETA: &str = "compact-2026-01-12";
const FAST_BETA: &str = "fast-mode-2026-02-01";
const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// Whether unsigned reasoning must be dropped rather than replayed with an
/// empty signature (after OpenCode's `requireThinkingSignature`): Anthropic
/// rejects it, but Kimi, MiMo, Qwen and MiniMax servers accept it.
pub fn require_signature(base_url: &str, model: &str) -> bool {
    let url = base_url.to_ascii_lowercase();
    let m = model.to_ascii_lowercase();
    !(m.starts_with("kimi-")
        || url.contains("api.kimi.com/coding")
        || url.contains("api.moonshot.ai/anthropic")
        || url.contains("api.moonshot.cn/anthropic")
        || url.contains("xiaomimimo.com")
        || m.contains("mimo")
        || m.starts_with("qwen")
        || m.starts_with("minimax-"))
}

/// Claude 5.1 and later take thinking block-binding controls.
fn supports_binding(model: &str) -> bool {
    claude_version(model).is_some_and(|(_, major, minor)| major > 5 || (major == 5 && minor >= 1))
}

fn invalid_option(name: &str, why: &str) -> Error {
    Error::Invalid(format!(
        "Anthropic Messages provider option {name} is invalid: {why}"
    ))
}

fn thinking_off(t: &Value) -> bool {
    matches!(t["type"].as_str(), Some("disabled" | "between_tools"))
}

/// The wire `thinking` block from the `thinking` option.
fn lower_thinking(v: &Value, max_tokens: u32) -> Result<Value, Error> {
    let m = v
        .as_object()
        .ok_or_else(|| invalid_option("thinking", "expected an object"))?;
    let kind = m.get("type").and_then(Value::as_str).unwrap_or("");
    let mut out = Map::new();
    out.insert("type".into(), json!(kind));
    match kind {
        "enabled" => {
            let budget = field(m, "budgetTokens")
                .and_then(Value::as_f64)
                .ok_or_else(|| invalid_option("thinking", "enabled thinking needs budgetTokens"))?;
            out.insert(
                "budget_tokens".into(),
                number(fit_budget(budget, Some(max_tokens), MIN_THINKING_BUDGET)),
            );
        }
        "adaptive" => {}
        "disabled" | "between_tools" => return Ok(Value::Object(out)),
        other => {
            return Err(invalid_option(
                "thinking",
                &format!("unknown type {other:?}"),
            ))
        }
    }
    if let Some(d) = m.get("display").filter(|d| !d.is_null()) {
        let d = d
            .as_str()
            .ok_or_else(|| invalid_option("thinking", "display must be a string"))?;
        out.insert("display".into(), json!(d));
    }
    if let Some(b) = field(m, "blockBinding") {
        let b = b
            .as_object()
            .ok_or_else(|| invalid_option("thinking", "block_binding must be an object"))?;
        let mut binding = Map::new();
        if let Some(p) = field(b, "prefixMismatchBehavior") {
            let p = p.as_str().ok_or_else(|| {
                invalid_option("thinking", "prefix_mismatch_behavior must be a string")
            })?;
            binding.insert("prefix_mismatch_behavior".into(), json!(p));
        }
        out.insert("block_binding".into(), Value::Object(binding));
    }
    Ok(Value::Object(out))
}

fn is_ttl(s: &str) -> bool {
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && matches!(&s[digits..], "ms" | "s" | "m" | "h" | "d")
}

/// Body fields from the request's provider options (after OpenCode's
/// Anthropic `fromRequest`); a malformed known option is an error, unknown
/// options are ignored.
fn lower_options(req: &Request) -> Result<Map<String, Value>, Error> {
    let o = &req.options;
    let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut out = Map::new();
    let string = |key: &str| -> Result<Option<String>, Error> {
        match o.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(invalid_option(key, "expected a string")),
        }
    };
    let mut thinking = match o.get("thinking") {
        Some(t) => Some(lower_thinking(t, max_tokens)?),
        None => None,
    };
    if supports_binding(&req.model) && !thinking.as_ref().is_some_and(thinking_off) {
        let mut t = thinking.unwrap_or_else(|| json!({"type": "adaptive"}));
        let mut binding = json!({"prefix_mismatch_behavior": "drop_block"});
        crate::options::deep_merge(&mut binding, &t["block_binding"]);
        if !binding.is_object() {
            binding = json!({"prefix_mismatch_behavior": "drop_block"});
        }
        t["block_binding"] = binding;
        thinking = Some(t);
    }
    if let Some(t) = thinking {
        out.insert("thinking".into(), t);
    }
    // `effort` wins over `output_config.effort`.
    let mut output = Map::new();
    if let Some(oc) = o.get("outputConfig") {
        let oc = oc
            .as_object()
            .ok_or_else(|| invalid_option("output_config", "expected an object"))?;
        if let Some(e) = oc.get("effort").filter(|e| !e.is_null()) {
            let e = e
                .as_str()
                .ok_or_else(|| invalid_option("output_config", "effort must be a string"))?;
            output.insert("effort".into(), json!(e));
        }
        if let Some(f) = oc.get("format").filter(|f| !f.is_null()) {
            if f["type"] != "json_schema" || !f["schema"].is_object() {
                return Err(invalid_option(
                    "output_config",
                    "format must be {type: json_schema, schema}",
                ));
            }
            output.insert("format".into(), f.clone());
        }
    }
    if let Some(e) = string("effort")? {
        output.insert("effort".into(), json!(e));
    }
    if !output.is_empty() {
        out.insert("output_config".into(), Value::Object(output));
    }
    for (key, wire) in [("serviceTier", "service_tier"), ("speed", "speed")] {
        if let Some(v) = string(key)? {
            out.insert(wire.into(), json!(v));
        }
    }
    if let Some(v) = o.get("inferenceGeo") {
        let v = v
            .as_str()
            .ok_or_else(|| invalid_option("inference_geo", "expected a string"))?;
        out.insert("inference_geo".into(), json!(v));
    }
    if let Some(m) = o.get("metadata") {
        let ok = m.as_object().is_some_and(|m| {
            m.get("user_id")
                .is_none_or(|u| u.is_string() || u.is_null())
        });
        if !ok {
            return Err(invalid_option("metadata", "user_id must be a string"));
        }
        out.insert("metadata".into(), m.clone());
    }
    if let Some(c) = o.get("container") {
        let ok = c.is_string()
            || c.as_object().is_some_and(|c| {
                c.get("id").is_none_or(|i| i.is_string() || i.is_null())
                    && c.get("skills").is_none_or(|s| {
                        s.is_null() || s.as_array().is_some_and(|a| a.iter().all(Value::is_object))
                    })
            });
        if !ok {
            return Err(invalid_option(
                "container",
                "expected an id or {id, skills}",
            ));
        }
        out.insert("container".into(), c.clone());
    }
    if let Some(c) = o.get("cacheControl") {
        let ok =
            c["type"] == "ephemeral" && c.get("ttl").is_none_or(|t| t.as_str().is_some_and(is_ttl));
        if !ok {
            return Err(invalid_option(
                "cache_control",
                "expected {type: ephemeral, ttl?}",
            ));
        }
        out.insert("cache_control".into(), c.clone());
    }
    if let Some(cm) = o.get("contextManagement") {
        let edits = cm["edits"]
            .as_array()
            .ok_or_else(|| invalid_option("contextManagement", "expected {edits: [...]}"))?;
        let mut lowered = Vec::new();
        for e in edits {
            let e = e
                .as_object()
                .filter(|e| e.get("type").and_then(Value::as_str) == Some("compact_20260112"))
                .ok_or_else(|| invalid_option("contextManagement", "unknown edit type"))?;
            let mut edit = json!({"type": "compact_20260112"});
            if let Some(t) = e.get("trigger") {
                edit["trigger"] = t.clone();
            }
            if let Some(p) = field(e, "pauseAfterCompaction") {
                edit["pause_after_compaction"] = p.clone();
            }
            if let Some(i) = e.get("instructions") {
                edit["instructions"] = i.clone();
            }
            lowered.push(edit);
        }
        out.insert("context_management".into(), json!({"edits": lowered}));
    }
    if let Some(n) = o.num("topP") {
        out.insert("top_p".into(), json!(n));
    }
    if let Some(n) = o.int("topK") {
        out.insert("top_k".into(), json!(n));
    }
    if let Some(s) = o.strings("stop") {
        out.insert("stop_sequences".into(), json!(s));
    }
    Ok(out)
}

/// The `anthropic-beta` values a body needs (after OpenCode's
/// `requiredBetas`): interleaved thinking always, compaction, fast mode and
/// thinking block binding when used.
pub fn betas(body: &Value) -> Vec<&'static str> {
    let mut out = vec![INTERLEAVED_BETA];
    if body.get("context_management").is_some() {
        out.push(COMPACT_BETA);
    }
    if body.get("speed").is_some() {
        out.push(FAST_BETA);
    }
    let t = &body["thinking"];
    if t.is_object() && !thinking_off(t) && t.get("block_binding").is_some() {
        out.push(BINDING_BETA);
    }
    out
}

/// Request headers for a body: the provider's own, with the required betas
/// appended to any `anthropic-beta` it sets.
pub fn headers(p: &Provider, body: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut beta: Vec<String> = Vec::new();
    for (k, v) in &p.headers {
        if k.eq_ignore_ascii_case("anthropic-beta") {
            beta.extend(
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        } else {
            out.push((k.clone(), v.clone()));
        }
    }
    for b in betas(body) {
        if !beta.iter().any(|x| x == b) {
            beta.push(b.to_string());
        }
    }
    out.push(("anthropic-beta".into(), beta.join(",")));
    out
}

/// Anthropic accepts tool ids matching `[A-Za-z0-9_-]+` (any length).
fn tool_id(id: &str) -> String {
    scrub_id(split_id(id).0)
}

const ROUTE: &str = "Anthropic Messages";

/// An image or document block (after OpenCode's `lowerMedia`): http(s)
/// images and PDFs by URL, inline PDFs and plain text as documents, inline
/// images as images. `None` for media [`check`] rejects.
fn lower_media(m: &Media) -> Option<Value> {
    let mime = m.mime.to_ascii_lowercase();
    let document = |source: Value| {
        let mut b = json!({"type": "document", "source": source});
        if let Some(f) = m.filename.as_deref().filter(|f| !f.is_empty()) {
            b["title"] = json!(f);
        }
        b
    };
    if let Some(url) = m.remote().map(str::trim).filter(|u| is_http(u)) {
        return match m.kind() {
            MediaKind::Image => {
                Some(json!({"type": "image", "source": {"type": "url", "url": url}}))
            }
            MediaKind::Pdf => Some(document(json!({"type": "url", "url": url}))),
            _ => None,
        };
    }
    let data = m.inline()?;
    if mime == "text/plain" {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .ok()?;
        return Some(document(
            json!({"type": "text", "media_type": "text/plain", "data": String::from_utf8_lossy(&bytes)}),
        ));
    }
    match m.kind() {
        MediaKind::Pdf => Some(document(
            json!({"type": "base64", "media_type": "application/pdf", "data": data}),
        )),
        MediaKind::Image => Some(
            json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}}),
        ),
        _ => None,
    }
}

fn is_http(u: &str) -> bool {
    let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Media Messages cannot carry: anything but images, PDFs and plain text,
/// and remote media other than http(s) images and PDFs.
pub fn check(req: &Request) -> Result<(), Error> {
    lower_options(req)?;
    for m in all_media(req) {
        let remote_ok = m.remote().is_some_and(|u| {
            is_http(u.trim()) && matches!(m.kind(), MediaKind::Image | MediaKind::Pdf)
        });
        if m.remote().is_some() && !remote_ok {
            return Err(inline_required(ROUTE));
        }
        if lower_media(m).is_none() {
            return Err(unsupported_media(ROUTE, m));
        }
    }
    Ok(())
}

/// The request body for the Anthropic API itself.
#[cfg(test)]
pub fn body(req: &Request) -> Value {
    body_for(req, "")
}

/// A reasoning part as a thinking block (after OpenCode's lowering): signed
/// thinking with its signature, redacted thinking as `redacted_thinking`,
/// and unsigned text with an empty signature where the server accepts it.
fn lower_reasoning(r: &crate::Reasoning, require_signature: bool) -> Option<Value> {
    let meta = r.meta_for(Protocol::AnthropicMessages);
    let signature = meta.and_then(|m| m.signature.as_deref());
    if let (None, Some(data)) = (signature, meta.and_then(|m| m.redacted.as_deref())) {
        return Some(json!({"type": "redacted_thinking", "data": data}));
    }
    match signature.filter(|s| !s.trim().is_empty()) {
        Some(sig) => Some(json!({"type": "thinking", "thinking": r.text, "signature": sig})),
        None if !r.text.trim().is_empty() && !require_signature => {
            Some(json!({"type": "thinking", "thinking": r.text, "signature": ""}))
        }
        None => None,
    }
}

/// The request body for a server at `base_url`.
pub fn body_for(req: &Request, base_url: &str) -> Value {
    let require_signature = require_signature(base_url, &req.model);
    let mut messages: Vec<Value> = Vec::new();
    for m in &normalize_tool_history(&req.messages) {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content: Vec<Value> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                // Whitespace-only text blocks are rejected.
                Part::Text(t) if !t.trim().is_empty() => Some(json!({"type": "text", "text": t})),
                Part::ToolCall { id, name, input } => Some(json!({"type": "tool_use", "id": tool_id(id), "name": name, "input": input})),
                Part::Reasoning(r) if m.role == Role::Assistant => lower_reasoning(r, require_signature),
                Part::Media(media) => lower_media(media),
                Part::ToolResult { id, content, is_error, media, .. } => {
                    // Media rides inside the result as image/document blocks.
                    let content = if media.is_empty() {
                        json!(content)
                    } else {
                        let text = (!content.is_empty()).then(|| json!({"type": "text", "text": content}));
                        json!(text.into_iter().chain(media.iter().filter_map(lower_media)).collect::<Vec<_>>())
                    };
                    let mut b = json!({"type": "tool_result", "tool_use_id": tool_id(id), "content": content});
                    if *is_error {
                        b["is_error"] = json!(true);
                    }
                    Some(b)
                }
                _ => None,
            })
            .collect();
        if content.is_empty() {
            continue;
        }
        // The API requires alternating roles; fold consecutive same-role messages.
        if let Some(last) = messages.last_mut().filter(|l| l["role"] == role) {
            last["content"].as_array_mut().unwrap().extend(content);
        } else {
            messages.push(json!({"role": role, "content": content}));
        }
    }
    // Tool results must lead their user turn.
    for m in messages.iter_mut().filter(|m| m["role"] == "user") {
        let blocks = m["content"].as_array_mut().unwrap();
        let (mut results, rest): (Vec<Value>, Vec<Value>) =
            blocks.drain(..).partition(|b| b["type"] == "tool_result");
        results.extend(rest);
        *blocks = results;
    }
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
    });
    if !req.system.is_empty() {
        body["system"] = json!(req.system);
    }
    if !req.tools.is_empty() {
        let sanitizer = schema::for_model(&req.model);
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": schema::project(&t.parameters, sanitizer)}))
            .collect();
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    // Malformed options are rejected by `check` before a request is sent.
    if let Ok(fields) = lower_options(req) {
        merge_body(&mut body, &fields);
    }
    merge_body(&mut body, &req.body);
    body
}

#[derive(Debug, Default, Clone)]
struct Block {
    kind: String,
    id: String,
    name: String,
    json: String,
    /// Complete input sent with `content_block_start`.
    input: Option<Value>,
    /// Thinking signature, or redacted thinking data.
    signature: String,
    redacted: Option<String>,
    stopped: bool,
}

/// The end of a thinking or redacted thinking block, with its replay state.
fn reasoning_end(b: &Block) -> Event {
    let mut meta = ReasoningMeta::of(Protocol::AnthropicMessages);
    if !b.signature.is_empty() {
        meta.signature = Some(b.signature.clone());
    }
    meta.redacted = b.redacted.clone();
    Event::ReasoningEnd(meta)
}

/// Events for a block's end: its tool call or reasoning state.
fn close_block(b: &mut Block) -> Option<Event> {
    if b.stopped {
        return None;
    }
    match b.kind.as_str() {
        "tool_use" => {
            b.stopped = true;
            Some(tool_call(b))
        }
        "thinking" | "redacted_thinking" => {
            b.stopped = true;
            Some(reasoning_end(b))
        }
        _ => None,
    }
}

#[derive(Debug, Default)]
pub struct State {
    /// Content blocks by index.
    blocks: Vec<Option<Block>>,
    reason: Option<String>,
    usage: Usage,
    pub done: bool,
}

impl State {
    fn block(&self, i: usize) -> Option<&Block> {
        self.blocks.get(i).and_then(Option::as_ref)
    }
}

fn tool_call(b: &Block) -> Event {
    let input = if !b.json.trim().is_empty() {
        serde_json::from_str(&b.json).unwrap_or_else(|_| json!({"__invalid_json": b.json}))
    } else {
        b.input
            .clone()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    };
    Event::ToolCall {
        id: b.id.clone(),
        name: b.name.clone(),
        input,
    }
}

/// The error inside an `error` event (`{type, message}`).
fn failure(v: &Value) -> Error {
    let e = &v["error"];
    if !e.is_object() {
        return Error::Provider {
            message: "Anthropic Messages stream error".into(),
            retryable: false,
        };
    }
    stream_error(e["type"].as_str(), e["message"].as_str(), v, false)
}

pub fn handle(state: &mut State, event: &str, v: &Value) -> Result<Vec<Event>, Error> {
    let mut out = Vec::new();
    match event {
        "message_start" => {
            let u = &v["message"]["usage"];
            state.usage.input = u["input_tokens"].as_u64().unwrap_or(0);
            state.usage.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
            state.usage.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            state.usage.output = u["output_tokens"].as_u64().unwrap_or(0);
        }
        "content_block_start" => {
            let i = v["index"].as_u64().unwrap_or(0) as usize;
            while state.blocks.len() <= i {
                state.blocks.push(None);
            }
            let b = &v["content_block"];
            let kind = b["type"].as_str().unwrap_or("").to_string();
            if kind == "text" {
                if let Some(t) = b["text"].as_str().filter(|s| !s.is_empty()) {
                    out.push(Event::TextDelta(t.to_string()));
                }
            }
            if kind == "thinking" {
                if let Some(t) = b["thinking"].as_str().filter(|s| !s.is_empty()) {
                    out.push(Event::ReasoningDelta(t.to_string()));
                }
            }
            state.blocks[i] = Some(Block {
                id: b["id"].as_str().unwrap_or("").to_string(),
                name: b["name"].as_str().unwrap_or("").to_string(),
                input: b.get("input").cloned(),
                signature: b["signature"].as_str().unwrap_or("").to_string(),
                redacted: (kind == "redacted_thinking")
                    .then(|| b["data"].as_str().unwrap_or("").to_string()),
                kind,
                ..Default::default()
            });
        }
        "content_block_delta" => {
            let i = v["index"].as_u64().unwrap_or(0) as usize;
            let d = &v["delta"];
            // Deltas go to their block; one never started is taken at its word.
            let kind = state.block(i).map(|b| b.kind.as_str());
            match d["type"].as_str() {
                Some("text_delta") if matches!(kind, None | Some("text")) => out.push(
                    Event::TextDelta(d["text"].as_str().unwrap_or("").to_string()),
                ),
                Some("thinking_delta") if matches!(kind, None | Some("thinking")) => out.push(
                    Event::ReasoningDelta(d["thinking"].as_str().unwrap_or("").to_string()),
                ),
                Some("input_json_delta") => {
                    if let Some(Some(b)) = state.blocks.get_mut(i) {
                        b.json.push_str(d["partial_json"].as_str().unwrap_or(""));
                    }
                }
                Some("signature_delta") => {
                    if let Some(Some(b)) = state.blocks.get_mut(i) {
                        b.signature.push_str(d["signature"].as_str().unwrap_or(""));
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let i = v["index"].as_u64().unwrap_or(0) as usize;
            if let Some(Some(b)) = state.blocks.get_mut(i) {
                out.extend(close_block(b));
            }
        }
        "message_delta" => {
            if let Some(r) = v["delta"]["stop_reason"].as_str() {
                state.reason = Some(r.to_string());
            }
            // The final delta carries authoritative totals; some
            // Anthropic-compatible servers only report input here.
            let u = &v["usage"];
            for (field, key) in [
                (&mut state.usage.input, "input_tokens"),
                (&mut state.usage.output, "output_tokens"),
                (&mut state.usage.cache_read, "cache_read_input_tokens"),
                (&mut state.usage.cache_write, "cache_creation_input_tokens"),
            ] {
                if let Some(n) = u[key].as_u64() {
                    *field = n;
                }
            }
        }
        "message_stop" => {
            // Blocks whose stop never came settle here.
            for b in state.blocks.iter_mut().flatten() {
                out.extend(close_block(b));
            }
            state.done = true;
            out.push(Event::Finish {
                reason: state.reason.take().unwrap_or_else(|| "end_turn".into()),
                usage: state.usage,
            });
        }
        "error" => return Err(failure(v)),
        // An untyped envelope carrying an error is still an error.
        "" if v.get("error").is_some_and(Value::is_object) => return Err(failure(v)),
        _ => {}
    }
    Ok(out)
}

const EVENTS: [&str; 8] = [
    "message_start",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "message_delta",
    "message_stop",
    "error",
    "ping",
];

#[derive(Default)]
pub struct Decode {
    state: State,
}

impl Decoder for Decode {
    fn frame(&mut self, f: &sse::Frame) -> Result<Vec<Event>, Error> {
        let v: Value = match serde_json::from_str(&f.data) {
            Ok(v) => v,
            // Proxies add named events of their own; skip what is not ours.
            Err(_) if f.event.as_deref().is_some_and(|e| !EVENTS.contains(&e)) => {
                return Ok(vec![])
            }
            Err(e) => return Err(Error::Decode(format!("{e}: {}", f.data))),
        };
        let kind = f
            .event
            .clone()
            .or_else(|| v["type"].as_str().map(str::to_string))
            .unwrap_or_default();
        handle(&mut self.state, &kind, &v)
    }
    fn done(&self) -> bool {
        self.state.done
    }
    fn end(&mut self) -> Result<Vec<Event>, Error> {
        if self.state.done {
            Ok(vec![])
        } else {
            Err(Error::Transport("stream ended before message_stop".into()))
        }
    }
}

pub async fn stream(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<EventStream, Error> {
    let url = format!("{}/messages", p.base_url.trim_end_matches('/'));
    let mut rb = client.post(url).header("anthropic-version", VERSION);
    if let Some(k) = &p.api_key {
        rb = rb.header("x-api-key", k);
    }
    let body = body_for(req, &p.base_url);
    for (k, v) in headers(p, &body) {
        rb = rb.header(k, v);
    }
    let resp = post(rb, &body).await?;
    Ok(drive(resp, Decode::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    #[test]
    fn body_folds_roles_and_maps_parts() {
        let req = Request {
            model: "claude".into(),
            system: "sys".into(),
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Reasoning("r".into()),
                        Part::Text("ok".into()),
                        Part::ToolCall {
                            id: "t1".into(),
                            name: "read".into(),
                            input: json!({"path": "a"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        id: "t1".into(),
                        name: "read".into(),
                        content: "x".into(),
                        is_error: true,
                        media: vec![],
                    }],
                },
                Message::user("more"),
            ],
            ..Default::default()
        };
        let b = body(&req);
        assert_eq!(b["system"], "sys");
        assert_eq!(b["max_tokens"], DEFAULT_MAX_TOKENS);
        let m = b["messages"].as_array().unwrap();
        assert_eq!(
            m.len(),
            3,
            "the tool result and the next user text fold into one user turn"
        );
        assert_eq!(
            m[1]["content"].as_array().unwrap().len(),
            2,
            "reasoning is not replayed"
        );
        assert_eq!(
            m[1]["content"][1],
            json!({"type": "tool_use", "id": "t1", "name": "read", "input": {"path": "a"}})
        );
        assert_eq!(
            m[2]["content"][0],
            json!({"type": "tool_result", "tool_use_id": "t1", "content": "x", "is_error": true})
        );
        assert_eq!(m[2]["content"][1], json!({"type": "text", "text": "more"}));
    }

    #[test]
    fn stream_events() {
        let mut s = State::default();
        handle(&mut s, "message_start", &json!({"message": {"usage": {"input_tokens": 12, "cache_read_input_tokens": 5, "output_tokens": 1}}})).unwrap();
        assert_eq!(
            handle(
                &mut s,
                "content_block_start",
                &json!({"index": 0, "content_block": {"type": "text", "text": ""}})
            )
            .unwrap(),
            vec![]
        );
        assert_eq!(
            handle(
                &mut s,
                "content_block_delta",
                &json!({"index": 0, "delta": {"type": "text_delta", "text": "Hi"}})
            )
            .unwrap(),
            vec![Event::TextDelta("Hi".into())]
        );
        handle(
            &mut s,
            "content_block_start",
            &json!({"index": 1, "content_block": {"type": "tool_use", "id": "t", "name": "glob"}}),
        )
        .unwrap();
        handle(&mut s, "content_block_delta", &json!({"index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"pattern\":"}})).unwrap();
        handle(&mut s, "content_block_delta", &json!({"index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"*.rs\"}"}})).unwrap();
        assert_eq!(
            handle(&mut s, "content_block_stop", &json!({"index": 1})).unwrap(),
            vec![Event::ToolCall {
                id: "t".into(),
                name: "glob".into(),
                input: json!({"pattern": "*.rs"})
            }]
        );
        handle(
            &mut s,
            "message_delta",
            &json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 40}}),
        )
        .unwrap();
        assert_eq!(
            handle(&mut s, "message_stop", &json!({})).unwrap(),
            vec![Event::Finish {
                reason: "tool_use".into(),
                usage: Usage {
                    input: 12,
                    output: 40,
                    cache_read: 5,
                    cache_write: 0
                }
            }]
        );
    }

    #[test]
    fn message_delta_usage_overrides_start() {
        let mut s = State::default();
        handle(
            &mut s,
            "message_start",
            &json!({"message": {"usage": {"input_tokens": 0, "output_tokens": 0}}}),
        )
        .unwrap();
        handle(&mut s, "message_delta", &json!({"delta": {"stop_reason": "end_turn"}, "usage": {"input_tokens": 290, "cache_read_input_tokens": 114, "output_tokens": 27}})).unwrap();
        assert_eq!(
            handle(&mut s, "message_stop", &json!({})).unwrap(),
            vec![Event::Finish {
                reason: "end_turn".into(),
                usage: Usage {
                    input: 290,
                    output: 27,
                    cache_read: 114,
                    cache_write: 0
                }
            }]
        );
    }

    #[test]
    fn thinking_and_errors() {
        let mut s = State::default();
        assert_eq!(
            handle(
                &mut s,
                "content_block_delta",
                &json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}})
            )
            .unwrap(),
            vec![Event::ReasoningDelta("hm".into())]
        );
        assert!(handle(
            &mut s,
            "error",
            &json!({"error": {"type": "overloaded_error"}})
        )
        .is_err());
    }

    #[test]
    fn empty_tool_input_is_an_empty_object() {
        let mut s = State::default();
        handle(
            &mut s,
            "content_block_start",
            &json!({"index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "x"}}),
        )
        .unwrap();
        assert_eq!(
            handle(&mut s, "content_block_stop", &json!({"index": 0})).unwrap(),
            vec![Event::ToolCall {
                id: "t".into(),
                name: "x".into(),
                input: json!({})
            }]
        );
    }
}
