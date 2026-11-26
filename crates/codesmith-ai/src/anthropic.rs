//! Anthropic Messages streaming.

use crate::cache;
use crate::gemini::split_id;
use crate::openai::scrub_id;
use crate::options::{claude_version, field, fit_budget, number};
use crate::{
    all_media, drive, effort_update, inline_required, merge_body, normalize_tool_history,
    resolve_effort_updates, schema, sse, stream_error, system_text, unsupported_media,
    wrap_system_update, Decoder, Error, Event, EventStream, FinishReason, Hosted, Media, MediaKind,
    Message, Part, Protocol, Provider, Reason, ReasoningMeta, Request, Role, ToolChoice, Usage,
};
use serde_json::{json, Map, Value};

pub(crate) const VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 32_000;
const MIN_THINKING_BUDGET: f64 = 1024.0;
const INTERLEAVED_BETA: &str = "interleaved-thinking-2025-05-14";
const COMPACT_BETA: &str = "compact-2026-01-12";
const FAST_BETA: &str = "fast-mode-2026-02-01";
pub(crate) const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
const OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
/// The effort a per-message change back to the default sends.
const DEFAULT_EFFORT: &str = "high";

/// Whether unsigned reasoning must be dropped rather than replayed with an
/// empty signature (after OpenCode's `requireThinkingSignature`): Anthropic
/// rejects it, but Kimi, MiMo, Qwen and MiniMax servers accept it. An
/// explicit `requireSignature` compatibility setting wins.
pub fn require_signature(p: &Provider, model: &str) -> bool {
    if let Some(r) = p.compat.require_signature {
        return r;
    }
    let url = p.base_url.to_ascii_lowercase();
    let m = model.to_ascii_lowercase();
    let name = p.name.to_ascii_lowercase();
    !(matches!(
        name.as_str(),
        "kimi-for-coding" | "moonshotai" | "moonshotai-cn"
    ) || name.contains("xiaomi")
        || m.starts_with("kimi-")
        || url.contains("api.kimi.com/coding")
        || url.contains("api.moonshot.ai/anthropic")
        || url.contains("api.moonshot.cn/anthropic")
        || url.contains("xiaomimimo.com")
        || m.contains("mimo")
        || m.starts_with("qwen")
        || m.starts_with("minimax-"))
}

/// Claude 5.1 and later take thinking block-binding controls.
pub(crate) fn supports_binding(model: &str) -> bool {
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

/// The wire `tool_choice` (after OpenCode's `lowerToolChoice`): `auto`,
/// `none`, `any` or a named `tool`, with `disable_parallel_tool_use` when
/// the request says whether parallel calls are allowed.
fn tool_choice(req: &Request) -> Option<Value> {
    let disable = req.parallel_tool_calls.map(|p| !p);
    let mut v = match &req.tool_choice {
        None if disable.is_none() => return None,
        None | Some(ToolChoice::Auto) => json!({"type": "auto"}),
        Some(ToolChoice::None) => return Some(json!({"type": "none"})),
        Some(ToolChoice::Required) => json!({"type": "any"}),
        Some(ToolChoice::Tool(name)) => json!({"type": "tool", "name": name}),
    };
    if let Some(d) = disable {
        v["disable_parallel_tool_use"] = json!(d);
    }
    Some(v)
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
        let ok = c["type"] == "ephemeral"
            && c.get("ttl")
                .is_none_or(|t| t.as_str().is_some_and(cache::is_ttl));
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
pub fn betas(body: &Value) -> Vec<String> {
    let mut out = vec![INTERLEAVED_BETA.to_string()];
    if body.get("context_management").is_some() {
        out.push(COMPACT_BETA.into());
    }
    if body.get("speed").is_some() {
        out.push(FAST_BETA.into());
    }
    let t = &body["thinking"];
    if t.is_object() && !thinking_off(t) && t.get("block_binding").is_some() {
        out.push(BINDING_BETA.into());
    }
    // Server tools still in beta.
    for t in body["tools"].as_array().into_iter().flatten() {
        if let Some(b) = t["type"].as_str().and_then(crate::hosted::anthropic_beta) {
            if !out.contains(&b) {
                out.push(b);
            }
        }
    }
    let messages = body["messages"].as_array().into_iter().flatten();
    if messages
        .filter(|m| m["role"] == "system")
        .any(|m| m.get("output_config").is_some())
    {
        out.push(OUTPUT_CONFIG_BETA.into());
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
        if !beta.contains(&b) {
            beta.push(b);
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

/// A media part's `anthropic` options (after OpenCode's `MediaOptions`):
/// `file_id`, `title`, `context`, `citations: {enabled}` and
/// `transformations: {oversized_image}`, with Anthropic's wire names.
fn media_options(m: &Media) -> Result<Map<String, Value>, Error> {
    let Some(o) = m.meta.get("anthropic") else {
        return Ok(Map::new());
    };
    let bad = |why: &str| Error::Invalid(format!("{ROUTE} media options are invalid: {why}"));
    let o = o.as_object().ok_or_else(|| bad("expected an object"))?;
    for key in ["file_id", "title", "context"] {
        if o.get(key).is_some_and(|v| !v.is_string()) {
            return Err(bad(&format!("{key} must be a string")));
        }
    }
    if let Some(c) = o.get("citations") {
        if !c
            .as_object()
            .is_some_and(|c| c.get("enabled").is_some_and(Value::is_boolean))
        {
            return Err(bad("citations must be {enabled: boolean}"));
        }
    }
    if let Some(t) = o.get("transformations") {
        let ok = t.as_object().is_some_and(|t| {
            t.get("oversized_image")
                .is_none_or(|v| matches!(v.as_str(), Some("downsize" | "error")))
        });
        if !ok {
            return Err(bad(
                "transformations.oversized_image must be \"downsize\" or \"error\"",
            ));
        }
    }
    Ok(o.clone())
}

/// An image, document or video block (after OpenCode's `lowerMedia`):
/// video inline or by any URL; else a file id from the part's `anthropic`
/// options, http(s) images and PDFs by URL, inline PDFs and plain text as
/// documents, and inline images as images, with the part's other
/// `anthropic` options. `None` for media [`check`] rejects.
fn lower_media(m: &Media) -> Option<Value> {
    let mime = m.mime.to_ascii_lowercase();
    let options = media_options(m).ok()?;
    let opt = |k: &str| options.get(k).filter(|v| !v.is_null());
    if m.kind() == MediaKind::Video {
        let source = match (m.inline(), m.remote()) {
            (Some(data), _) => json!({"type": "base64", "media_type": mime, "data": data}),
            (None, Some(url)) => json!({"type": "url", "url": url}),
            (None, None) => return None,
        };
        return Some(json!({"type": "video", "source": source}));
    }
    let file = opt("file_id").map(|id| json!({"type": "file", "file_id": id}));
    let image = |source: Value| {
        let mut b = json!({"type": "image", "source": source});
        if let Some(t) = opt("transformations") {
            b["transformations"] = t.clone();
        }
        b
    };
    let document = |source: Value| {
        let mut b = json!({"type": "document", "source": source});
        let title = opt("title").cloned().or_else(|| {
            m.filename
                .as_deref()
                .filter(|f| !f.is_empty())
                .map(|f| json!(f))
        });
        if let Some(t) = title {
            b["title"] = t;
        }
        for key in ["context", "citations"] {
            if let Some(v) = opt(key) {
                b[key] = v.clone();
            }
        }
        b
    };
    if let Some(f) = file {
        return Some(match m.kind() {
            MediaKind::Image => image(f),
            _ => document(f),
        });
    }
    if let Some(url) = m.remote().map(str::trim).filter(|u| is_http(u)) {
        return match m.kind() {
            MediaKind::Image => Some(image(json!({"type": "url", "url": url}))),
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
        MediaKind::Image => Some(image(
            json!({"type": "base64", "media_type": mime, "data": data}),
        )),
        _ => None,
    }
}

fn is_http(u: &str) -> bool {
    let l = u.get(..8).unwrap_or(u).to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Media Messages cannot carry: anything but images, PDFs, plain text and
/// video, remote media other than http(s) images and PDFs and video, and
/// malformed `anthropic` media options.
pub fn check(req: &Request) -> Result<(), Error> {
    lower_options(req)?;
    crate::hosted::declared(req, Protocol::AnthropicMessages)?;
    crate::hosted::anthropic_check(req)?;
    for m in all_media(req) {
        let options = media_options(m)?;
        let remote_ok = options.get("file_id").is_some_and(Value::is_string)
            || m.kind() == MediaKind::Video
            || m.remote().is_some_and(|u| {
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

/// Meta cannot see video in tool results: a user turn's result videos move
/// after its blocks (after OpenCode's `MetaMessages`).
fn hoist_videos(messages: &mut [Value]) {
    for m in messages.iter_mut().filter(|m| m["role"] == "user") {
        let blocks = m["content"].as_array_mut().unwrap();
        let mut videos = Vec::new();
        for b in blocks.iter_mut().filter(|b| b["type"] == "tool_result") {
            if let Some(content) = b["content"].as_array_mut() {
                let (moved, kept): (Vec<Value>, Vec<Value>) =
                    content.drain(..).partition(|c| c["type"] == "video");
                videos.extend(moved);
                *content = kept;
            }
        }
        blocks.extend(videos);
    }
}

/// The request body for the Anthropic API itself.
#[cfg(test)]
pub fn body(req: &Request) -> Value {
    body_for(req, "")
}

/// The request body for a server at `base_url`, with nothing configured.
#[cfg(test)]
pub fn body_for(req: &Request, base_url: &str) -> Value {
    let p = Provider {
        base_url: base_url.into(),
        ..Default::default()
    };
    body_with(req, &p)
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

/// Whether the model takes native mid-conversation system messages (after
/// OpenCode's `supportsNativeSystemUpdates`): Claude Opus 4.8 and every
/// Claude 5 model, and later versions. A `systemUpdates` option overrides
/// the detection (OpenCode's `capabilities.systemUpdates`).
fn system_updates(req: &Request) -> bool {
    if let Some(b) = req.options.bool("systemUpdates") {
        return b;
    }
    match claude_version(&req.model) {
        Some((family, 4, minor)) if family == "opus" => minor >= 8,
        Some((_, major, _)) => major >= 5,
        None => false,
    }
}

/// Whether the model takes a per-message effort change (after OpenCode's
/// `supportsEffortUpdates`): Claude Opus 5 and every Claude 5.1 model, and
/// later versions, unless thinking is off. An `effortUpdates` option
/// overrides the detection (OpenCode's `capabilities.reasoning.updates`).
fn effort_updates(req: &Request) -> bool {
    if req.options.get("thinking").is_some_and(thinking_off) {
        return false;
    }
    if let Some(b) = req.options.bool("effortUpdates") {
        return b;
    }
    match claude_version(&req.model) {
        Some((family, major, minor)) => {
            (family == "opus" && major >= 5) || major > 5 || (major == 5 && minor >= 1)
        }
        None => false,
    }
}

/// The requested top-level effort: `effort`, else `outputConfig.effort`.
fn requested_effort(req: &Request) -> Option<&str> {
    req.options.str("effort").or_else(|| {
        req.options
            .object("outputConfig")
            .and_then(|o| o.get("effort"))
            .and_then(Value::as_str)
    })
}

/// Add a text block to the latest user turn, or start one.
fn append_to_user(messages: &mut Vec<Value>, text: String) {
    let block = json!({"type": "text", "text": text});
    match messages.last_mut().filter(|l| l["role"] == "user") {
        Some(last) => last["content"].as_array_mut().unwrap().push(block),
        None => messages.push(json!({"role": "user", "content": [block]})),
    }
}

/// Send the held system updates: as native `system` messages after a user
/// turn (tool results count), else as wrapped text in a user turn.
fn release(messages: &mut Vec<Value>, held: &mut Vec<&Message>) {
    let after_user = messages
        .iter()
        .rev()
        .find(|m| m["role"] != "system")
        .is_some_and(|m| m["role"] == "user");
    for m in held.drain(..) {
        if after_user {
            messages.push(
                json!({"role": "system", "content": [{"type": "text", "text": system_text(m)}]}),
            );
        } else {
            append_to_user(messages, wrap_system_update(m));
        }
    }
}

/// A hosted part from this API as its `server_tool_use` and result blocks.
/// Hosted parts from other protocols are left out, and so is a call whose
/// result never came (Anthropic rejects a server tool use without one).
fn hosted_blocks(h: &crate::Hosted) -> Vec<Value> {
    if h.protocol != Protocol::AnthropicMessages || h.result.is_none() {
        return vec![];
    }
    let input = if h.input.is_object() {
        h.input.clone()
    } else {
        json!({})
    };
    // `check` rejects results of unknown server tools.
    match (&h.result, crate::hosted::anthropic_result_type(&h.name)) {
        (Some(r), Some(kind)) => vec![
            json!({"type": "server_tool_use", "id": tool_id(&h.id), "name": h.name, "input": input}),
            json!({"type": kind, "tool_use_id": tool_id(&h.id), "content": r.value}),
        ],
        _ => vec![],
    }
}

/// The request body for provider `p`.
pub fn body_with(req: &Request, p: &Provider) -> Value {
    let req = &*crate::flatten_namespaces(req);
    let require_signature = require_signature(p, &req.model);
    let mut messages: Vec<Value> = Vec::new();
    // System updates on models that take them natively are held until the
    // next assistant turn (or the end), the nearest spot after a user turn
    // where Anthropic accepts a `system` message; elsewhere they stay in
    // place as wrapped user text.
    let native = system_updates(req);
    // Effort updates are native system messages in place (Anthropic takes
    // them anywhere), with the top-level effort frozen at the one before
    // the first; elsewhere they are dropped.
    let (efforts, effort) = if effort_updates(req) {
        resolve_effort_updates(&req.messages, requested_effort(req))
    } else {
        (false, requested_effort(req).map(str::to_string))
    };
    let mut held: Vec<&Message> = Vec::new();
    let messages_in = normalize_tool_history(&req.messages);
    for m in &messages_in {
        if let Some((e, _)) = effort_update(m) {
            if efforts {
                messages.push(json!({"role": "system", "content": [], "output_config": {"effort": e.unwrap_or(DEFAULT_EFFORT)}}));
            }
            continue;
        }
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => {
                if native {
                    held.push(m);
                } else {
                    append_to_user(&mut messages, wrap_system_update(m));
                }
                continue;
            }
        };
        let content: Vec<Value> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                // Several blocks, flattened below.
                Part::Hosted(h) if m.role == Role::Assistant => Some(Value::Array(hosted_blocks(h))),
                // Whitespace-only text blocks are rejected.
                Part::Text(t) if !t.trim().is_empty() => Some(json!({"type": "text", "text": t.as_str()})),
                Part::ToolCall { id, name, input, .. } => Some(json!({"type": "tool_use", "id": tool_id(id), "name": name, "input": input})),
                Part::Reasoning(r) if m.role == Role::Assistant => lower_reasoning(r, require_signature),
                // Anthropic takes no media from the assistant; media another
                // model generated is left out, as in OpenCode.
                Part::Media(media) if m.role != Role::Assistant => lower_media(media),
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
            .flat_map(|b| match b {
                Value::Array(blocks) => blocks,
                b => vec![b],
            })
            .collect();
        if content.is_empty() {
            continue;
        }
        if m.role == Role::Assistant {
            release(&mut messages, &mut held);
        }
        // The API requires alternating roles; fold consecutive same-role messages.
        if let Some(last) = messages.last_mut().filter(|l| l["role"] == role) {
            last["content"].as_array_mut().unwrap().extend(content);
        } else {
            messages.push(json!({"role": role, "content": content}));
        }
    }
    release(&mut messages, &mut held);
    if p.base_url.to_ascii_lowercase().contains("api.meta.ai") {
        hoist_videos(&mut messages);
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
    // Malformed server tools are rejected by `check` before anything is sent.
    let hosted = crate::hosted::declared(req, Protocol::AnthropicMessages).unwrap_or_default();
    if !req.tools.is_empty() || !hosted.is_empty() {
        let sanitizer = schema::for_model(&req.model);
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": schema::project(&t.parameters, sanitizer)}))
            .chain(hosted)
            .collect();
        // Anthropic rejects `tool_choice` without tools.
        if let Some(c) = tool_choice(req) {
            body["tool_choice"] = c;
        }
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    // Malformed options are rejected by `check` before a request is sent.
    if let Ok(mut fields) = lower_options(req) {
        if efforts {
            if let Some(Value::Object(oc)) = fields.get_mut("output_config") {
                match &effort {
                    Some(e) => oc.insert("effort".into(), json!(e)),
                    None => oc.remove("effort"),
                };
                if oc.is_empty() {
                    fields.remove("output_config");
                }
            } else if let Some(e) = &effort {
                fields.insert("output_config".into(), json!({"effort": e}));
            }
        }
        merge_body(&mut body, &fields);
    }
    merge_body(&mut body, &req.body);
    let policy = cache::resolve(&req.cache, cache::AUTO);
    cache::mark_anthropic(&mut body, policy, req.cache.lifetime());
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

/// The block's id in lifecycle events.
fn block_id(kind: &str, i: usize) -> String {
    format!("{kind}-{i}")
}

/// Events for a block's end: its text end, tool call or reasoning state.
fn close_block(b: &mut Block, i: usize) -> Result<Vec<Event>, Error> {
    if b.stopped {
        return Ok(vec![]);
    }
    Ok(match b.kind.as_str() {
        "text" => {
            b.stopped = true;
            vec![Event::TextEnd(block_id("text", i))]
        }
        "tool_use" => {
            b.stopped = true;
            vec![Event::ToolInputEnd { id: b.id.clone() }, tool_call(b)]
        }
        // A call the server ran: its input is final, so malformed input
        // ends the stream (no local tool can report it back).
        "server_tool_use" => {
            b.stopped = true;
            vec![Event::HostedCall(server_tool_call(b)?)]
        }
        "thinking" | "redacted_thinking" => {
            b.stopped = true;
            vec![reasoning_end(b)]
        }
        _ => vec![],
    })
}

#[derive(Debug, Default)]
pub struct State {
    /// Content blocks by index.
    blocks: Vec<Option<Block>>,
    reason: Option<FinishReason>,
    /// The usage objects merged, as they came.
    raw_usage: Option<Map<String, Value>>,
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

/// A `server_tool_use` block as a hosted call.
fn server_tool_call(b: &Block) -> Result<Hosted, Error> {
    let input = if b.json.trim().is_empty() {
        b.input
            .clone()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    } else {
        serde_json::from_str(&b.json).map_err(|_| {
            Error::Decode(format!(
                "Invalid JSON input for {ROUTE} tool call {}",
                b.name
            ))
        })?
    };
    Ok(Hosted::call(
        Protocol::AnthropicMessages,
        b.id.clone(),
        &b.name,
        input,
    ))
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

/// A Messages stop reason in the common algebra; a refusal carries the
/// category and explanation its `stop_details` give.
fn finish_reason(raw: &str, details: &Value) -> FinishReason {
    let normalized = match raw {
        "end_turn" | "stop_sequence" | "pause_turn" => Reason::Stop,
        "max_tokens" | "model_context_window_exceeded" => Reason::Length,
        "tool_use" => Reason::ToolCalls,
        "refusal" => Reason::ContentFilter,
        _ => Reason::Unknown,
    };
    let text = |k: &str| {
        details[k]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    FinishReason {
        category: text("category"),
        explanation: text("explanation"),
        ..FinishReason::new(normalized, raw)
    }
}

impl State {
    fn merge_usage(&mut self, u: &Value) {
        if let Some(o) = u.as_object() {
            let raw = self.raw_usage.get_or_insert_with(Map::new);
            for (k, v) in o {
                raw.insert(k.clone(), v.clone());
            }
        }
    }
}

const STREAM_ROUTE: &str = "anthropic/anthropic-messages";

/// Events whose payload OpenCode checks.
const CHECKED: [&str; 9] = [
    "message",
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "ping",
    "error",
];

/// Content blocks and deltas CodeSmith reads (others are skipped unchecked).
const BLOCKS: [&str; 6] = [
    "compaction",
    "text",
    "thinking",
    "redacted_thinking",
    "tool_use",
    "server_tool_use",
];
const DELTAS: [&str; 5] = [
    "compaction_delta",
    "text_delta",
    "thinking_delta",
    "signature_delta",
    "input_json_delta",
];

fn usage_ok(u: &Value) -> bool {
    use crate::shape::{array_of, field, null_num, opt_num};
    let counts = |u: &Value| {
        u.is_object()
            && null_num(u, "input_tokens")
            && opt_num(u, "output_tokens")
            && null_num(u, "cache_creation_input_tokens")
            && null_num(u, "cache_read_input_tokens")
            && field(u, "server_tool_use", true, |s| {
                s.is_object() && opt_num(s, "web_search_requests")
            })
            && field(u, "output_tokens_details", true, |d| {
                d.is_object() && opt_num(d, "thinking_tokens")
            })
    };
    counts(u) && array_of(u, "iterations", false, counts)
}

/// A content block as OpenCode's stream schema reads it.
fn block_ok(b: &Value) -> bool {
    b.is_object()
        && b["type"].is_string()
        && crate::shape::all(
            b,
            &[
                "id",
                "name",
                "text",
                "thinking",
                "signature",
                "data",
                "tool_use_id",
            ],
            crate::shape::opt_str,
        )
}

/// A content or message delta as OpenCode's stream schema reads it.
fn delta_ok(d: &Value) -> bool {
    use crate::shape::{all, field, null_str, opt_str};
    d.is_object()
        && null_str(d, "content")
        && all(
            d,
            &["type", "text", "thinking", "partial_json", "signature"],
            opt_str,
        )
        && all(d, &["stop_reason", "stop_sequence"], null_str)
        && field(d, "stop_details", true, |s| {
            s.is_object() && all(s, &["category", "explanation"], null_str)
        })
}

/// OpenCode's checks of a recognized event: its own fields, any block or
/// delta it carries, and what its type requires. `Ok(false)` skips an
/// unknown content block or delta type.
fn validate(kind: &str, v: &Value) -> Result<bool, Error> {
    use crate::shape::{all, field, opt_num, opt_str};
    if !CHECKED.contains(&kind) {
        return Ok(true);
    }
    let invalid = || Err(crate::shape::invalid(STREAM_ROUTE, v));
    let event_ok = opt_num(v, "index")
        && field(v, "message", false, |m| {
            m.is_object() && field(m, "usage", false, usage_ok)
        })
        && field(v, "usage", false, usage_ok)
        && field(v, "error", false, |e| {
            e.is_object() && all(e, &["type", "message"], opt_str)
        });
    if !event_ok
        || (kind != "content_block_start" && !field(v, "content_block", false, block_ok))
        || (kind != "content_block_delta" && !field(v, "delta", false, delta_ok))
    {
        return invalid();
    }
    match kind {
        "content_block_start" => {
            let b = &v["content_block"];
            let Some(t) = b["type"].as_str().filter(|_| b.is_object()) else {
                return invalid();
            };
            if !BLOCKS.contains(&t) && !t.ends_with("_tool_result") {
                return Ok(true);
            }
            if !block_ok(b) {
                return invalid();
            }
            if t == "tool_use" || t == "server_tool_use" {
                let Some(i) = v["index"].as_u64() else {
                    return Err(Error::Decode(format!("Anthropic {t} missing index")));
                };
                if b["id"].as_str().is_none_or(str::is_empty) {
                    return Err(Error::Decode(format!(
                        "Anthropic tool_use missing id at index {i}"
                    )));
                }
            }
            Ok(true)
        }
        "content_block_delta" => {
            let d = &v["delta"];
            if !d.is_object() {
                return invalid();
            }
            if d["type"].as_str().is_some_and(|t| !DELTAS.contains(&t)) {
                return Ok(false);
            }
            match delta_ok(d) {
                true => Ok(true),
                false => invalid(),
            }
        }
        "message_delta" if !v.get("delta").is_some_and(delta_ok) => invalid(),
        _ => Ok(true),
    }
}

pub fn handle(state: &mut State, event: &str, v: &Value) -> Result<Vec<Event>, Error> {
    let mut out = Vec::new();
    if !validate(event, v)? {
        return Ok(out);
    }
    match event {
        "message_start" => {
            let u = &v["message"]["usage"];
            state.usage.input = u["input_tokens"].as_u64().unwrap_or(0);
            state.usage.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
            state.usage.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            state.usage.output = u["output_tokens"].as_u64().unwrap_or(0);
            state.merge_usage(u);
        }
        "content_block_start" => {
            let i = v["index"].as_u64().unwrap_or(0) as usize;
            while state.blocks.len() <= i {
                state.blocks.push(None);
            }
            let b = &v["content_block"];
            let kind = b["type"].as_str().unwrap_or("").to_string();
            match kind.as_str() {
                "text" => {
                    out.push(Event::TextStart(block_id("text", i)));
                    if let Some(t) = b["text"].as_str().filter(|s| !s.is_empty()) {
                        out.push(Event::TextDelta(t.to_string()));
                    }
                }
                "thinking" | "redacted_thinking" => {
                    out.push(Event::ReasoningStart(block_id("reasoning", i)));
                    if let Some(t) = b["thinking"].as_str().filter(|s| !s.is_empty()) {
                        out.push(Event::ReasoningDelta(t.to_string()));
                    }
                }
                "tool_use" => out.push(Event::ToolInputStart {
                    id: b["id"].as_str().unwrap_or("").to_string(),
                    name: b["name"].as_str().unwrap_or("").to_string(),
                }),
                // Server tool results arrive whole, for an earlier
                // `server_tool_use`.
                k => {
                    if let Some(name) = crate::hosted::anthropic_result_tool(k) {
                        let id = b["tool_use_id"].as_str().unwrap_or("").to_string();
                        let result = crate::hosted::anthropic_result(&b["content"]);
                        out.push(Event::HostedResult(
                            Hosted::call(Protocol::AnthropicMessages, id, name, json!({}))
                                .with(result),
                        ));
                    }
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
                        let text = d["partial_json"].as_str().unwrap_or("");
                        b.json.push_str(text);
                        if b.kind == "tool_use" && !b.stopped && !text.is_empty() {
                            out.push(Event::ToolInputDelta {
                                id: b.id.clone(),
                                text: text.to_string(),
                            });
                        }
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
                out.extend(close_block(b, i)?);
            }
        }
        "message_delta" => {
            if let Some(r) = v["delta"]["stop_reason"].as_str() {
                state.reason = Some(finish_reason(r, &v["delta"]["stop_details"]));
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
            // Copilot reports its billing beside the delta's usage.
            if let Some(c) = crate::billing::billed(u, &[v]) {
                state.usage.cost = Some(c);
            }
            state.merge_usage(u);
        }
        "message_stop" => {
            // Blocks whose stop never came settle here.
            for (i, b) in state.blocks.iter_mut().enumerate() {
                if let Some(b) = b {
                    out.extend(close_block(b, i)?);
                }
            }
            state.done = true;
            let raw_usage = state.raw_usage.take().map(Value::Object);
            let mut usage = state.usage;
            if let Some(u) = &raw_usage {
                usage.reasoning = u["output_tokens_details"]["thinking_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage.total =
                    Some(usage.input + usage.cache_read + usage.cache_write + usage.output);
            }
            out.push(Event::Finish {
                reason: state.reason.take().unwrap_or_else(|| Reason::Stop.into()),
                usage,
                raw_usage,
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
            Err(_) => {
                return Err(Error::Decode(format!(
                    "Invalid {STREAM_ROUTE} stream event: {}",
                    f.data
                )))
            }
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
    let resp = crate::endpoint::send(p, request(p, client, req)).await?;
    let served = crate::bifrost::served(p, &resp);
    Ok(crate::bifrost::with_served(
        served,
        drive(resp, Decode::default()),
    ))
}

/// The Messages request for `req`.
pub(crate) fn request(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> reqwest::RequestBuilder {
    let mut body = body_with(req, p);
    let mut h = vec![("anthropic-version".to_string(), VERSION.to_string())];
    h.extend(headers(p, &body));
    if vertex(p) {
        // Vertex takes the model in the path and its own API version in
        // the body.
        if let Some(b) = body.as_object_mut() {
            b.remove("model");
            b.insert("anthropic_version".into(), json!(VERTEX_VERSION));
        }
        return p.json_post(client, &format!("{}:streamRawPredict", req.model), h, &body);
    }
    p.json_post(client, "messages", h, &body)
}

/// Claude on Vertex AI's `anthropic_version`.
const VERTEX_VERSION: &str = "vertex-2023-10-16";

/// Whether `p` is Claude on Vertex AI (its models live under
/// `publishers/anthropic`).
pub(crate) fn vertex(p: &Provider) -> bool {
    p.name == "google-vertex-anthropic" || p.base_url.contains("/publishers/anthropic/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    // An image Gemini generated, in history switched to Claude: Anthropic
    // takes no assistant media, so it is left out (as in OpenCode).
    #[test]
    fn generated_assistant_media_is_left_out() {
        let png = Media::base64("image/png", "AAECAw==")
            .with_meta("google", json!({"thoughtSignature": "sig_1"}));
        let req = Request {
            model: "claude".into(),
            messages: vec![
                Message::user("Draw a cat"),
                Message {
                    role: Role::Assistant,
                    parts: vec![Part::text("Here you go."), Part::Media(png)],
                },
                Message::user("Thanks"),
            ],
            ..Default::default()
        };
        assert_eq!(
            body(&req)["messages"][1]["content"],
            json!([{"type": "text", "text": "Here you go."}])
        );
    }

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
                            meta: None,
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
            cache: crate::Cache::off(),
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
            vec![Event::TextStart("text-0".into())]
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
            vec![
                Event::ToolInputEnd { id: "t".into() },
                Event::ToolCall {
                    id: "t".into(),
                    name: "glob".into(),
                    input: json!({"pattern": "*.rs"})
                }
            ]
        );
        handle(
            &mut s,
            "message_delta",
            &json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 40}}),
        )
        .unwrap();
        assert_eq!(
            handle(&mut s, "message_stop", &json!({})).unwrap(),
            vec![
                Event::TextEnd("text-0".into()),
                Event::Finish {
                    reason: FinishReason::new(Reason::ToolCalls, "tool_use"),
                    usage: Usage {
                        input: 12,
                        output: 40,
                        cache_read: 5,
                        cache_write: 0,
                        reasoning: 0,
                        total: Some(57),
                        ..Default::default()
                    },
                    raw_usage: Some(
                        json!({"input_tokens": 12, "cache_read_input_tokens": 5, "output_tokens": 40})
                    ),
                }
            ]
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
                reason: FinishReason::new(Reason::Stop, "end_turn"),
                usage: Usage {
                    input: 290,
                    output: 27,
                    cache_read: 114,
                    cache_write: 0,
                    reasoning: 0,
                    total: Some(431),
                    ..Default::default()
                },
                raw_usage: Some(
                    json!({"input_tokens": 290, "cache_read_input_tokens": 114, "output_tokens": 27})
                ),
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
            crate::content(handle(&mut s, "content_block_stop", &json!({"index": 0})).unwrap()),
            vec![Event::ToolCall {
                id: "t".into(),
                name: "x".into(),
                input: json!({})
            }]
        );
    }
}
