//! Amazon Bedrock's Converse API (after OpenCode's `bedrock-converse`
//! protocol): the request body, the AWS event-stream binary framing of
//! `ConverseStream`, and the stream events' state machine.

use crate::{
    effort_update, normalize_tool_history, system_text, wrap_system_update, Error, Event,
    EventStream, FinishReason, Media, Part, Protocol, Provider, Reason, ReasoningMeta, Request,
    Role, ToolChoice, Usage,
};
use base64::Engine;
use serde_json::{json, Map, Value};

pub(crate) const ROUTE: &str = "Bedrock Converse";
/// Claude on Bedrock wants the thinking budget below `maxTokens`, and at
/// least this.
const MIN_THINKING_BUDGET: f64 = 1024.0;
/// Bedrock-Claude shares Anthropic's four-breakpoint cap.
const BREAKPOINT_CAP: usize = 4;

/// The path of a model's `ConverseStream` (the id percent-encoded, so an
/// inference profile ARN or a `:0` version stays one segment).
pub fn path(model: &str) -> String {
    format!(
        "model/{}/converse-stream",
        crate::sigv4::uri_encode(model, false)
    )
}

// ---------------------------------------------------------------------------
// Request lowering
// ---------------------------------------------------------------------------

const IMAGE_FORMATS: [(&str, &str); 5] = [
    ("image/png", "png"),
    ("image/jpeg", "jpeg"),
    ("image/jpg", "jpeg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

const DOCUMENT_FORMATS: [(&str, &str); 9] = [
    ("application/pdf", "pdf"),
    ("text/csv", "csv"),
    ("application/msword", "doc"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    ("application/vnd.ms-excel", "xls"),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    ),
    ("text/html", "html"),
    ("text/plain", "txt"),
    ("text/markdown", "md"),
];

/// Media Converse cannot take, found before anything is sent.
pub fn check(req: &Request) -> Result<(), Error> {
    for m in crate::all_media(req) {
        lower_media(m, &mut Vec::new())?;
    }
    Ok(())
}

/// A document label: the file name without its extension, reduced to the
/// characters Converse allows, unique across the request.
fn document_name(filename: Option<&str>, names: &mut Vec<String>) -> String {
    let stem = filename.unwrap_or_default();
    let stem = match stem.rfind('.') {
        Some(i) => &stem[..i],
        None => stem,
    };
    let cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " ()[]-".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let mut base: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    base = base
        .chars()
        .take(200)
        .collect::<String>()
        .trim()
        .to_string();
    if base.is_empty() {
        base = "document".into();
    }
    let mut name = base.clone();
    let mut i = 2;
    while names.contains(&name) {
        let suffix = format!(" {i}");
        let keep: String = base.chars().take(200 - suffix.len()).collect();
        name = format!("{}{suffix}", keep.trim_end());
        i += 1;
    }
    names.push(name.clone());
    name
}

fn lower_media(m: &Media, names: &mut Vec<String>) -> Result<Vec<Value>, Error> {
    let mime = m.mime.to_ascii_lowercase();
    let data = || -> Result<String, Error> {
        let d = m.inline().ok_or_else(|| crate::inline_required(ROUTE))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(d.trim())
            .map_err(|_| Error::Invalid(format!("{ROUTE} media data must be valid base64")))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    };
    if let Some((_, f)) = IMAGE_FORMATS.iter().find(|(k, _)| *k == mime) {
        return Ok(vec![
            json!({"image": {"format": f, "source": {"bytes": data()?}}}),
        ]);
    }
    if mime.starts_with("image/") {
        return Err(Error::Invalid(format!(
            "{ROUTE} does not support image media type {}",
            m.mime
        )));
    }
    let Some((_, f)) = DOCUMENT_FORMATS.iter().find(|(k, _)| *k == mime) else {
        return Err(crate::unsupported_media(ROUTE, m));
    };
    let bytes = data()?;
    let name = document_name(m.filename.as_deref(), names);
    let block = json!({"document": {"format": f, "name": name, "source": {"bytes": bytes}}});
    Ok(match m.filename.as_deref() {
        Some(f) if f != name => vec![
            json!({"text": format!("Attached file {} has document label {}.", json!(f), json!(name))}),
            block,
        ],
        _ => vec![block],
    })
}

/// Tool names Converse accepts when they are replayed.
fn tool_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if s.is_empty() {
        "_".into()
    } else {
        s
    }
}

/// Tool input without empty keys, which Converse rejects.
fn clean_input(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(clean_input).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.clone(), clean_input(v)))
                .collect(),
        ),
        v => v.clone(),
    }
}

/// Whether the model keeps tool-result images inside the result (Claude
/// and Nova); others get them beside it.
fn images_inline(model: &str) -> bool {
    model.contains("anthropic.claude-") || model.contains("amazon.nova-")
}

const LEGACY_CLAUDE: [&str; 4] = [
    "anthropic.claude-instant",
    "anthropic.claude-v1",
    "anthropic.claude-v2",
    "anthropic.claude-3-",
];

/// Claude releases that cache for five minutes only.
const CLAUDE_5M: [&str; 6] = [
    "anthropic.claude-3-5-sonnet-20241022-v2:0",
    "anthropic.claude-3-5-haiku-20241022-v1:0",
    "anthropic.claude-3-7-sonnet-20250219-v1:0",
    "anthropic.claude-sonnet-4-20250514-v1:0",
    "anthropic.claude-opus-4-20250514-v1:0",
    "anthropic.claude-opus-4-1-20250805-v1:0",
];

/// Cache points to spend, and whether a one-hour lifetime is honoured.
struct Breakpoints {
    supported: bool,
    long: bool,
    left: usize,
    ttl: Option<String>,
}

impl Breakpoints {
    fn new(req: &Request) -> Self {
        // Substring matching covers regional prefixes and model ARNs.
        let short = CLAUDE_5M.iter().any(|id| req.model.contains(id));
        let supported = req.model.contains("anthropic.claude-")
            && (short || !LEGACY_CLAUDE.iter().any(|id| req.model.contains(id)));
        Breakpoints {
            supported,
            long: !short,
            left: BREAKPOINT_CAP,
            ttl: req.cache.lifetime().map(str::to_string),
        }
    }

    /// A `cachePoint` block, if one is left.
    fn take(&mut self) -> Option<Value> {
        if !self.supported || self.left == 0 {
            return None;
        }
        self.left -= 1;
        let ttl = self.ttl.as_deref().filter(|t| *t != "1h" || self.long);
        Some(match ttl {
            Some(t) => json!({"cachePoint": {"type": "default", "ttl": t}}),
            None => json!({"cachePoint": {"type": "default"}}),
        })
    }
}

/// Add `content` to the last message when it has `role`, else as a new one.
fn push(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    match messages.last_mut().filter(|m| m["role"] == role) {
        Some(m) => m["content"].as_array_mut().unwrap().extend(content),
        None => messages.push(json!({"role": role, "content": content})),
    }
}

fn reasoning_block(r: &crate::Reasoning) -> Option<Value> {
    let meta = r.meta_for(Protocol::BedrockConverse);
    let signature = meta.and_then(|m| m.signature.as_deref());
    let redacted = meta.and_then(|m| m.redacted.as_deref());
    match (signature, redacted) {
        (None, Some(d)) => Some(json!({"reasoningContent": {"redactedContent": d}})),
        (Some(s), _) if !s.trim().is_empty() => {
            Some(json!({"reasoningContent": {"reasoningText": {"text": r.text, "signature": s}}}))
        }
        _ => None,
    }
}

fn lower_messages(
    req: &Request,
    bp: &mut Breakpoints,
    mark_last: bool,
) -> Result<Vec<Value>, Error> {
    let mistral = req.model.contains("mistral.");
    let id = |id: &str| -> String {
        if mistral {
            crate::openai::mistral_call_id(id)
        } else {
            id.to_string()
        }
    };
    let hoist = !images_inline(&req.model);
    let mut names = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    for m in normalize_tool_history(&req.messages) {
        let results = m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }));
        if !results && !pending.is_empty() {
            push(&mut messages, "user", std::mem::take(&mut pending));
        }
        match m.role {
            Role::System => {
                if effort_update(&m).is_some() || system_text(&m).is_empty() {
                    continue;
                }
                push(
                    &mut messages,
                    "user",
                    vec![json!({"text": wrap_system_update(&m)})],
                );
            }
            Role::Assistant => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text(t) if !t.text.is_empty() => {
                            content.push(json!({"text": t.text}))
                        }
                        Part::Reasoning(r) => content.extend(reasoning_block(r)),
                        Part::ToolCall {
                            id: call,
                            name,
                            input,
                            ..
                        } => content.push(json!({"toolUse": {
                            "toolUseId": id(call),
                            "name": tool_name(name),
                            "input": clean_input(input),
                        }})),
                        _ => {}
                    }
                }
                push(&mut messages, "assistant", content);
            }
            Role::User => {
                let mut content = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text(t) if !t.text.is_empty() => {
                            content.push(json!({"text": t.text}))
                        }
                        Part::Media(media) => content.extend(lower_media(media, &mut names)?),
                        Part::ToolResult {
                            id: call,
                            content: text,
                            is_error,
                            media,
                            ..
                        } => {
                            let mut items = vec![];
                            if !text.is_empty() || media.is_empty() {
                                items.push(json!({"text": text}));
                            }
                            for md in media {
                                for b in lower_media(md, &mut names)? {
                                    if hoist && b.get("image").is_some() {
                                        pending.push(b);
                                    } else {
                                        items.push(b);
                                    }
                                }
                            }
                            if items.is_empty() {
                                items.push(json!({"text": "See attached image."}));
                            }
                            let status = if *is_error { "error" } else { "success" };
                            content.push(json!({"toolResult": {
                                "toolUseId": id(call),
                                "content": items,
                                "status": status,
                            }}));
                        }
                        _ => {}
                    }
                }
                push(&mut messages, "user", content);
            }
        }
    }
    if !pending.is_empty() {
        push(&mut messages, "user", pending);
    }
    // A breakpoint after the last message's content.
    if let Some(last) = messages.last_mut().filter(|_| mark_last) {
        if let Some(cp) = bp.take() {
            last["content"].as_array_mut().unwrap().push(cp);
        }
    }
    Ok(messages)
}

fn invalid_option(name: &str, why: &str) -> Error {
    Error::Invalid(format!("{ROUTE} option {name}: {why}"))
}

/// Claude 4.6 and later pair effort with adaptive thinking.
fn adaptive_claude(model: &str) -> bool {
    crate::options::claude_version(model)
        .is_some_and(|(_, major, minor)| major > 4 || (major == 4 && minor >= 6))
}

/// Where each model family reads a reasoning effort.
fn effort_fields(model: &str, effort: &str) -> Value {
    if model.contains("anthropic") {
        json!({"output_config": {"effort": effort}})
    } else if model.contains("openai.gpt-oss") {
        json!({"reasoning_effort": effort})
    } else if model.contains("openai.") || model.contains("xai.") {
        json!({"reasoning": {"effort": effort}})
    } else {
        json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": effort}})
    }
}

fn thinking_disabled(body: &Map<String, Value>) -> bool {
    matches!(
        body.get("additionalModelRequestFields")
            .and_then(|f| f["thinking"]["type"].as_str()),
        Some("disabled" | "between_tools")
    )
}

/// The request body (after OpenCode's `BedrockConverse.fromRequest`).
pub fn body(req: &Request) -> Result<Value, Error> {
    let req = &*crate::flatten_namespaces(req);
    let o = &req.options;
    let effort = o.str("effort").or_else(|| o.reasoning_effort());
    let high = |v: Option<&str>| v == Some("high");
    // Nova 2 rejects `maxTokens` at high effort.
    let nova2 = req.model.contains("amazon.nova-2-");
    let body_high = req
        .body
        .get("additionalModelRequestFields")
        .is_some_and(|f| f["reasoningConfig"]["maxReasoningEffort"] == "high");
    let max_tokens = if nova2 && (high(effort) || body_high) {
        None
    } else {
        req.max_tokens
    };
    let mut thinking: Option<Value> = match o.get("thinking") {
        None => None,
        Some(Value::Object(t)) => match t.get("type").and_then(Value::as_str) {
            Some("enabled") => {
                let budget = crate::options::field(t, "budgetTokens")
                    .and_then(Value::as_f64)
                    .ok_or_else(|| invalid_option("thinking", "budgetTokens must be a number"))?;
                let budget = crate::options::fit_budget(budget, max_tokens, MIN_THINKING_BUDGET);
                Some(json!({"type": "enabled", "budget_tokens": crate::options::number(budget)}))
            }
            Some("adaptive" | "disabled") => Some(Value::Object(t.clone())),
            _ => {
                return Err(invalid_option(
                    "thinking",
                    "type must be enabled, adaptive or disabled",
                ))
            }
        },
        Some(_) => return Err(invalid_option("thinking", "must be an object")),
    };
    if thinking.is_none() && effort.is_some() && adaptive_claude(&req.model) {
        thinking = Some(json!({"type": "adaptive"}));
    }
    // Claude 5.1+ binds each thinking signature to its prefix; ask Bedrock
    // to drop affected blocks rather than fail when the prefix changes.
    let disabled = thinking.as_ref().is_some_and(|t| t["type"] == "disabled");
    let mut binding = false;
    if !disabled && !thinking_disabled(&req.body) && crate::anthropic::supports_binding(&req.model)
    {
        let t = thinking.get_or_insert_with(|| json!({"type": "adaptive"}));
        t["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
        binding = true;
    }
    let mut extra = Map::new();
    if let Some(t) = thinking {
        extra.insert("thinking".into(), t);
    }
    if let Some(k) = o.int("topK") {
        extra.insert("top_k".into(), json!(k));
    }
    if let Some(e) = effort {
        if let Value::Object(f) = effort_fields(&req.model, e) {
            extra.extend(f);
        }
    }
    if binding {
        extra.insert(
            "anthropic_beta".into(),
            json!([crate::anthropic::BINDING_BETA]),
        );
    }

    let mut bp = Breakpoints::new(req);
    let policy = crate::cache::resolve(&req.cache, crate::cache::AUTO);
    if !policy.places() {
        bp.supported = false;
    }
    let mut body = json!({});
    // Tools, then system, then messages: the highest-impact prefixes first.
    if !req.tools.is_empty() {
        let sanitizer = crate::schema::for_model(&req.model);
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut spec = json!({
                    "name": t.name,
                    "inputSchema": {"json": crate::schema::project(&t.parameters, sanitizer)},
                });
                if !t.description.trim().is_empty() {
                    spec["description"] = json!(t.description);
                }
                json!({"toolSpec": spec})
            })
            .collect();
        if policy.tools {
            tools.extend(bp.take());
        }
        let mut config = json!({"tools": tools});
        // Converse has no `none`: the definitions stay for the cache and only
        // the choice is left out.
        match &req.tool_choice {
            Some(ToolChoice::Auto) => config["toolChoice"] = json!({"auto": {}}),
            Some(ToolChoice::Required) => config["toolChoice"] = json!({"any": {}}),
            Some(ToolChoice::Tool(name)) => config["toolChoice"] = json!({"tool": {"name": name}}),
            Some(ToolChoice::None) | None => {}
        }
        body["toolConfig"] = config;
    }
    if !req.system.is_empty() {
        let mut system = vec![json!({"text": req.system})];
        if policy.system {
            system.extend(bp.take());
        }
        body["system"] = json!(system);
    }
    let messages = lower_messages(req, &mut bp, policy.messages.is_some())?;
    // Keys in order: modelId, messages, system, ….
    let mut ordered = Map::new();
    ordered.insert("modelId".into(), json!(req.model));
    ordered.insert("messages".into(), json!(messages));
    if let Some(s) = body.get("system") {
        ordered.insert("system".into(), s.clone());
    }
    let top_p = o.num("topP");
    let stop = o.strings("stopSequences").filter(|s| !s.is_empty());
    if max_tokens.is_some() || req.temperature.is_some() || top_p.is_some() || stop.is_some() {
        let mut ic = Map::new();
        if let Some(m) = max_tokens {
            ic.insert("maxTokens".into(), json!(m));
        }
        if let Some(t) = req.temperature {
            ic.insert("temperature".into(), json!(t));
        }
        if let Some(p) = top_p {
            ic.insert("topP".into(), json!(p));
        }
        if let Some(s) = stop {
            ic.insert("stopSequences".into(), json!(s));
        }
        ordered.insert("inferenceConfig".into(), Value::Object(ic));
    }
    if let Some(t) = body.get("toolConfig") {
        ordered.insert("toolConfig".into(), t.clone());
    }
    if !extra.is_empty() {
        ordered.insert("additionalModelRequestFields".into(), Value::Object(extra));
    }
    if let Some(t) = o.str("serviceTier") {
        ordered.insert("serviceTier".into(), json!({"type": t}));
    }
    if let Some(g) = o.object("guardrail") {
        let id = g.get("identifier").and_then(Value::as_str);
        let version = g.get("version").and_then(Value::as_str);
        let (Some(id), Some(version)) = (id, version) else {
            return Err(invalid_option(
                "guardrail",
                "identifier and version must be strings",
            ));
        };
        let mut c = json!({"guardrailIdentifier": id, "guardrailVersion": version});
        for k in ["trace", "streamProcessingMode"] {
            if let Some(v) = g.get(k).filter(|v| v.is_string()) {
                c[k] = v.clone();
            }
        }
        ordered.insert("guardrailConfig".into(), c);
    }
    if let Some(f) = o.object("outputFormat") {
        let schema = f
            .get("schema")
            .filter(|s| s.is_object())
            .ok_or_else(|| invalid_option("outputFormat", "schema must be an object"))?;
        let mut js = json!({"schema": schema.to_string()});
        for k in ["name", "description"] {
            if let Some(v) = f.get(k).filter(|v| v.is_string()) {
                js[k] = v.clone();
            }
        }
        let kind = f
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("json_schema");
        ordered.insert(
            "outputConfig".into(),
            json!({"textFormat": {"type": kind, "structure": {"jsonSchema": js}}}),
        );
    }
    if let Some(m) = o.string_record("requestMetadata") {
        ordered.insert("requestMetadata".into(), Value::Object(m));
    }
    let mut body = Value::Object(ordered);
    // Body overrides merge field by field into the model-specific fields.
    for (k, v) in &req.body {
        match (body.get_mut(k), v) {
            (Some(Value::Object(b)), Value::Object(t)) if k == "additionalModelRequestFields" => {
                let mut base = Value::Object(std::mem::take(b));
                crate::options::deep_merge(&mut base, &Value::Object(t.clone()));
                body[k] = base;
            }
            _ => {
                body[k] = v.clone();
            }
        }
    }
    Ok(body)
}

/// The `ConverseStream` request for `req`.
pub(crate) fn request(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<reqwest::RequestBuilder, Error> {
    let body = body(req)?;
    Ok(p.json_post(client, &path(&req.model), p.headers.clone(), &body))
}

pub async fn stream(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<EventStream, Error> {
    let resp = crate::endpoint::send(p, request(p, client, req)?)
        .await
        .map_err(|e| profile_hint(e, &req.model))?;
    Ok(drive(resp))
}

/// Whether a model id names an inference profile (a geography prefix or
/// an ARN).
pub fn is_profile(model: &str) -> bool {
    model.starts_with("arn:")
        || [
            "us.", "eu.", "apac.", "global.", "us-gov.", "jp.", "au.", "ca.",
        ]
        .iter()
        .any(|p| model.starts_with(p))
}

/// A model that Bedrock serves only through an inference profile answers a
/// bare id with a 400; say which ids to use instead.
pub fn profile_hint(e: Error, model: &str) -> Error {
    match e {
        Error::Status { status: 400, body }
            if (body.contains("on-demand throughput") || body.contains("inference profile"))
                && !is_profile(model) =>
        {
            Error::Status {
                status: 400,
                body: format!(
                    "{body} (use a cross-region inference profile id, such as us.{model}, eu.{model}, apac.{model} or global.{model}, or an inference profile ARN)"
                ),
            }
        }
        e => e,
    }
}

// ---------------------------------------------------------------------------
// AWS event-stream framing
// ---------------------------------------------------------------------------

/// A header value of an event-stream message.
#[derive(Debug, Clone, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    Int(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

impl HeaderValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            HeaderValue::String(s) => Some(s),
            _ => None,
        }
    }
}

/// One event-stream message: `[total length][headers length][prelude CRC]
/// [headers][payload][message CRC]`, big-endian, CRC32 over what precedes.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub headers: Vec<(String, HeaderValue)>,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.as_str())
    }

    /// The message encoded (for tests and fixtures).
    pub fn encode(&self) -> Vec<u8> {
        let mut h = Vec::new();
        for (k, v) in &self.headers {
            h.push(k.len() as u8);
            h.extend_from_slice(k.as_bytes());
            match v {
                HeaderValue::Bool(true) => h.push(0),
                HeaderValue::Bool(false) => h.push(1),
                HeaderValue::Int(i) => {
                    h.push(5);
                    h.extend_from_slice(&i.to_be_bytes());
                }
                HeaderValue::Bytes(b) => {
                    h.push(6);
                    h.extend_from_slice(&(b.len() as u16).to_be_bytes());
                    h.extend_from_slice(b);
                }
                HeaderValue::String(s) => {
                    h.push(7);
                    h.extend_from_slice(&(s.len() as u16).to_be_bytes());
                    h.extend_from_slice(s.as_bytes());
                }
                HeaderValue::Timestamp(t) => {
                    h.push(8);
                    h.extend_from_slice(&t.to_be_bytes());
                }
                HeaderValue::Uuid(u) => {
                    h.push(9);
                    h.extend_from_slice(u);
                }
            }
        }
        let total = 12 + h.len() + self.payload.len() + 4;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(h.len() as u32).to_be_bytes());
        let prelude = crc32fast::hash(&out);
        out.extend_from_slice(&prelude.to_be_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(&self.payload);
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }
}

fn frame_error(why: &str) -> Error {
    Error::Decode(format!(
        "Failed to decode {ROUTE} event-stream frame: {why}"
    ))
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Decode one complete message.
pub fn decode_message(b: &[u8]) -> Result<Message, Error> {
    if b.len() < 16 {
        return Err(frame_error("shorter than the 16-byte minimum"));
    }
    let total = be32(&b[0..4]) as usize;
    let hlen = be32(&b[4..8]) as usize;
    if total != b.len() {
        return Err(frame_error("length mismatch"));
    }
    if crc32fast::hash(&b[0..8]) != be32(&b[8..12]) {
        return Err(frame_error("prelude checksum mismatch"));
    }
    if crc32fast::hash(&b[..total - 4]) != be32(&b[total - 4..]) {
        return Err(frame_error("message checksum mismatch"));
    }
    if 12 + hlen > total - 4 {
        return Err(frame_error("headers overrun the message"));
    }
    let mut h = &b[12..12 + hlen];
    let mut headers = Vec::new();
    let take = |h: &mut &[u8], n: usize| -> Result<Vec<u8>, Error> {
        if h.len() < n {
            return Err(frame_error("truncated header"));
        }
        let (a, rest) = h.split_at(n);
        *h = rest;
        Ok(a.to_vec())
    };
    while !h.is_empty() {
        let n = take(&mut h, 1)?[0] as usize;
        let name = String::from_utf8(take(&mut h, n)?).map_err(|_| frame_error("header name"))?;
        let kind = take(&mut h, 1)?[0];
        let int = |bytes: Vec<u8>| bytes.iter().fold(0i64, |a, b| (a << 8) | *b as i64);
        let value = match kind {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Int(take(&mut h, 1)?[0] as i8 as i64),
            3 => {
                let v = take(&mut h, 2)?;
                HeaderValue::Int(i16::from_be_bytes([v[0], v[1]]) as i64)
            }
            4 => {
                let v = take(&mut h, 4)?;
                HeaderValue::Int(i32::from_be_bytes([v[0], v[1], v[2], v[3]]) as i64)
            }
            5 => HeaderValue::Int(int(take(&mut h, 8)?)),
            6 | 7 => {
                let l = take(&mut h, 2)?;
                let v = take(&mut h, u16::from_be_bytes([l[0], l[1]]) as usize)?;
                if kind == 6 {
                    HeaderValue::Bytes(v)
                } else {
                    HeaderValue::String(
                        String::from_utf8(v).map_err(|_| frame_error("header value"))?,
                    )
                }
            }
            8 => HeaderValue::Timestamp(int(take(&mut h, 8)?)),
            9 => {
                let v = take(&mut h, 16)?;
                let mut u = [0u8; 16];
                u.copy_from_slice(&v);
                HeaderValue::Uuid(u)
            }
            _ => return Err(frame_error("unknown header type")),
        };
        headers.push((name, value));
    }
    Ok(Message {
        headers,
        payload: b[12 + hlen..total - 4].to_vec(),
    })
}

/// Splits a byte stream into event-stream messages.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    /// The complete messages `chunk` finishes.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Message>, Error> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut at = 0;
        while self.buf.len() - at >= 4 {
            let total = be32(&self.buf[at..at + 4]) as usize;
            if total < 16 {
                return Err(frame_error("shorter than the 16-byte minimum"));
            }
            if self.buf.len() - at < total {
                break;
            }
            out.push(decode_message(&self.buf[at..at + total])?);
            at += total;
        }
        self.buf.drain(..at);
        Ok(out)
    }

    /// An error when bytes of an unfinished message remain.
    pub fn finish(&self) -> Result<(), Error> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(Error::Decode(format!(
                "Incomplete {ROUTE} event-stream frame: {} buffered bytes remain at end of stream",
                self.buf.len()
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Stream events
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Tool {
    id: String,
    name: String,
    json: String,
}

/// The parser state of one response.
#[derive(Debug, Default)]
pub struct State {
    tools: std::collections::BTreeMap<u64, Tool>,
    text: std::collections::BTreeSet<u64>,
    reasoning: std::collections::BTreeSet<u64>,
    signatures: std::collections::BTreeMap<u64, String>,
    redacted: std::collections::BTreeMap<u64, Vec<u8>>,
    /// `data` (the Anthropic spelling of redacted thinking) per block.
    redacted_data: std::collections::BTreeMap<u64, String>,
    reason: Option<FinishReason>,
    usage: Option<(Usage, Value)>,
    tool_calls: bool,
}

/// The Bedrock exceptions worth retrying.
fn transient(kind: &str) -> bool {
    matches!(
        kind,
        "throttlingException"
            | "serviceUnavailableException"
            | "internalServerException"
            | "modelStreamErrorException"
            | "modelNotReadyException"
    )
}

fn reasoning_meta(state: &State, i: u64) -> ReasoningMeta {
    let mut meta = ReasoningMeta::of(Protocol::BedrockConverse);
    if let Some(s) = state.signatures.get(&i) {
        meta.signature = Some(s.clone());
    } else if let Some(r) = state.redacted.get(&i) {
        meta.redacted = Some(base64::engine::general_purpose::STANDARD.encode(r));
    } else if let Some(d) = state.redacted_data.get(&i) {
        meta.redacted = Some(d.clone());
    }
    meta
}

fn finish_reason(raw: &str) -> FinishReason {
    let r = match raw {
        "end_turn" | "stop_sequence" => Reason::Stop,
        "max_tokens" | "model_context_window_exceeded" => Reason::Length,
        "tool_use" => Reason::ToolCalls,
        "content_filtered" | "guardrail_intervened" => Reason::ContentFilter,
        _ => Reason::Unknown,
    };
    FinishReason::new(r, raw)
}

/// Events for one decoded message.
pub fn handle(state: &mut State, m: &Message) -> Result<Vec<Event>, Error> {
    let kind = m.header(":message-type");
    if kind == Some("error") {
        let code = m.header(":error-code");
        let message = m.header(":error-message");
        let text = [code, message]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(": ");
        return Err(Error::Provider {
            message: if text.is_empty() {
                format!("{ROUTE} event-stream error")
            } else {
                text
            },
            retryable: code.is_some_and(transient),
        });
    }
    let event = match kind {
        Some("event") => m.header(":event-type"),
        Some("exception") => m.header(":exception-type"),
        _ => None,
    };
    let Some(event) = event else {
        return Ok(vec![]);
    };
    if m.payload.is_empty() {
        return Ok(vec![]);
    }
    let text = String::from_utf8_lossy(&m.payload);
    let v: Value = serde_json::from_str(&text).map_err(|_| {
        Error::Decode(format!(
            "Failed to parse {ROUTE} event-stream payload: {}",
            crate::quote(&text)
        ))
    })?;
    if !v.is_object() {
        return Err(Error::Decode(format!(
            "Failed to parse {ROUTE} event-stream payload: {}",
            crate::quote(&text)
        )));
    }
    if kind == Some("exception") {
        let message = v["message"]
            .as_str()
            .or(v["originalMessage"].as_str())
            .unwrap_or("Bedrock Converse stream error");
        let status = v["originalStatusCode"].as_u64();
        return Err(Error::Provider {
            message: format!("{event}: {message}"),
            retryable: transient(event) || status.is_some_and(|s| s == 429 || s >= 500),
        });
    }
    handle_event(state, event, &v)
}

/// Events for one decoded event (its `:event-type` and JSON payload).
pub fn handle_event(state: &mut State, event: &str, v: &Value) -> Result<Vec<Event>, Error> {
    let mut out = Vec::new();
    let index = v["contentBlockIndex"].as_u64().unwrap_or(0);
    match event {
        "contentBlockStart" => {
            if let Some(t) = v["start"]["toolUse"].as_object() {
                let id = t.get("toolUseId").and_then(Value::as_str).unwrap_or("");
                let name = t.get("name").and_then(Value::as_str).unwrap_or("");
                out.push(Event::ToolInputStart {
                    id: id.into(),
                    name: name.into(),
                });
                state.tools.insert(
                    index,
                    Tool {
                        id: id.into(),
                        name: name.into(),
                        json: String::new(),
                    },
                );
            }
        }
        "contentBlockDelta" => {
            let d = &v["delta"];
            if let Some(t) = d["text"].as_str().filter(|t| !t.is_empty()) {
                if state.text.insert(index) {
                    out.push(Event::TextStart(format!("text-{index}")));
                }
                out.push(Event::TextDelta(t.into()));
            } else if let Some(r) = d["reasoningContent"].as_object() {
                let mut open = |out: &mut Vec<Event>| {
                    if state.reasoning.insert(index) {
                        out.push(Event::ReasoningStart(format!("reasoning-{index}")));
                    }
                };
                if let Some(b) = r.get("redactedContent").and_then(Value::as_str) {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b)
                        .map_err(|_| {
                            Error::Decode(format!(
                                "{ROUTE} reasoningContent.redactedContent contains invalid base64 data"
                            ))
                        })?;
                    open(&mut out);
                    state.redacted.entry(index).or_default().extend(bytes);
                }
                if let Some(t) = r.get("text").and_then(Value::as_str) {
                    open(&mut out);
                    if !t.is_empty() {
                        out.push(Event::ReasoningDelta(t.into()));
                    }
                }
                if let Some(s) = r
                    .get("signature")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    open(&mut out);
                    state.signatures.insert(index, s.into());
                }
                if let Some(d) = r.get("data").and_then(Value::as_str) {
                    open(&mut out);
                    state.redacted.remove(&index);
                    state.redacted_data.insert(index, d.into());
                }
            } else if let Some(input) = d["toolUse"]["input"].as_str() {
                // A delta for a block that is not open is dropped.
                if let Some(t) = state.tools.get_mut(&index) {
                    t.json.push_str(input);
                    if !input.is_empty() {
                        out.push(Event::ToolInputDelta {
                            id: t.id.clone(),
                            text: input.into(),
                        });
                    }
                }
            }
        }
        "contentBlockStop" => {
            if let Some(t) = state.tools.remove(&index) {
                let input = if t.json.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&t.json)
                        .unwrap_or_else(|_| json!({"__invalid_json": t.json}))
                };
                out.push(Event::ToolInputEnd { id: t.id.clone() });
                out.push(Event::ToolCall {
                    id: t.id,
                    name: t.name,
                    input,
                });
                state.tool_calls = true;
            } else {
                if state.text.remove(&index) {
                    out.push(Event::TextEnd(format!("text-{index}")));
                }
                if state.reasoning.remove(&index) {
                    out.push(Event::ReasoningEnd(reasoning_meta(state, index)));
                }
                state.signatures.remove(&index);
                state.redacted.remove(&index);
                state.redacted_data.remove(&index);
            }
        }
        "messageStop" => {
            let raw = v["stopReason"].as_str().unwrap_or("");
            if raw == "malformed_model_output" || raw == "malformed_tool_use" {
                return Err(Error::Provider {
                    message: format!("{ROUTE} stopped with {raw}"),
                    retryable: false,
                });
            }
            state.reason = Some(finish_reason(raw));
        }
        "metadata" => {
            let u = &v["usage"];
            if u.is_object() {
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                // Bedrock reports input apart from cache reads and writes.
                let usage = Usage {
                    input: n("inputTokens"),
                    output: n("outputTokens"),
                    cache_read: n("cacheReadInputTokens"),
                    cache_write: n("cacheWriteInputTokens"),
                    total: u["totalTokens"].as_u64().or(Some(
                        n("inputTokens")
                            + n("cacheReadInputTokens")
                            + n("cacheWriteInputTokens")
                            + n("outputTokens"),
                    )),
                    ..Default::default()
                };
                state.usage = Some((usage, u.clone()));
            }
        }
        _ => {}
    }
    Ok(out)
}

/// The events once the stream ends: blocks left open close, and the finish
/// follows (a stream that never stopped is an error).
pub fn finish(state: &mut State) -> Result<Vec<Event>, Error> {
    let Some(mut reason) = state.reason.take() else {
        return Err(Error::Transport(format!(
            "{ROUTE} stream ended before messageStop"
        )));
    };
    let mut out = Vec::new();
    for i in std::mem::take(&mut state.text) {
        out.push(Event::TextEnd(format!("text-{i}")));
    }
    for i in std::mem::take(&mut state.reasoning) {
        out.push(Event::ReasoningEnd(reasoning_meta(state, i)));
    }
    if reason.normalized == Reason::Stop && state.tool_calls {
        reason.normalized = Reason::ToolCalls;
    }
    let (usage, raw) = match state.usage.take() {
        Some((u, raw)) => (u, Some(raw)),
        None => (Usage::default(), None),
    };
    out.push(Event::Finish {
        reason,
        usage,
        raw_usage: raw,
    });
    Ok(out)
}

/// Decode a whole response body (for tests and replay).
pub fn decode_all(bytes: &[u8]) -> Result<Vec<Event>, Error> {
    let mut framer = Framer::default();
    let mut state = State::default();
    let mut out = Vec::new();
    for m in framer.push(bytes)? {
        out.extend(handle(&mut state, &m)?);
    }
    framer.finish()?;
    out.extend(finish(&mut state)?);
    Ok(out)
}

fn drive(resp: reqwest::Response) -> EventStream {
    use futures_util::StreamExt;
    async_stream::stream! {
        let mut bytes = resp.bytes_stream();
        let mut framer = Framer::default();
        let mut state = State::default();
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => { yield Err(Error::from(e)); return; }
            };
            let messages = match framer.push(&chunk) {
                Ok(m) => m,
                Err(e) => { yield Err(e); return; }
            };
            for m in messages {
                match handle(&mut state, &m) {
                    Ok(evs) => for ev in evs { yield Ok(ev); },
                    Err(e) => { yield Err(e); return; }
                }
            }
        }
        if let Err(e) = framer.finish() { yield Err(e); return; }
        match finish(&mut state) {
            Ok(evs) => for ev in evs { yield Ok(ev); },
            Err(e) => yield Err(e),
        }
    }
    .boxed()
}
