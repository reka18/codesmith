//! Google Gemini `streamGenerateContent` (Generative Language API).
//!
//! Gemini returns a thought signature with each function call and wants it
//! back when the call is replayed. It rides along in the tool-call id
//! (`<id>~<signature>`), so stored conversations need no extra fields.

use crate::{
    all_media, drive_resuming, effort_update, inline_required, merge_body, normalize_tool_history,
    schema, sse, stream_error, wrap_system_update, Decoder, Error, Event, EventStream,
    FinishReason, Hosted, HostedResult, Media, MediaKind, Message, Part, Protocol, Provider,
    Reason, ReasoningMeta, Reopen, Request, Role, ToolChoice, Usage,
};
use futures_util::FutureExt;
use serde_json::{json, Map, Value};

const SIG: char = '~';
/// Replayed on unsigned calls so Gemini 3 accepts history it did not sign
/// (calls made by another model, or before signatures existed).
pub const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// Split a tool-call id into its id and any thought signature.
pub fn split_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once(SIG) {
        Some((a, b)) if !b.is_empty() => (a, Some(b)),
        _ => (id, None),
    }
}

/// Gemini 1.x and 2.x reject explicit function call ids; every other model
/// (Gemini 3+, Gemma, anything unrecognized) gets them.
pub fn omits_call_ids(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let rest = m
        .strip_prefix("gemini-live-")
        .or_else(|| m.strip_prefix("gemini-"));
    let major: String = rest
        .unwrap_or("")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    major.parse::<u32>().is_ok_and(|n| n < 3)
}

/// Gemini 2.5 rejects media nested in function responses; it gets the media
/// in a user turn after them instead.
fn legacy_tool_media(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.match_indices("gemini-2").any(|(i, _)| {
        let rest = &m[i + "gemini-2".len()..];
        let mut c = rest.chars();
        matches!(c.next(), Some('.' | '-'))
            && c.next() == Some('5')
            && matches!(c.next(), None | Some('.' | '-'))
    })
}

/// An `inlineData` part, or `fileData` for http(s) video. `None` for media
/// [`check`] rejects.
fn media_part(m: &Media) -> Option<Value> {
    if let Some(url) = m.remote() {
        let http = url.starts_with("http://") || url.starts_with("https://");
        return (http && m.kind() == MediaKind::Video)
            .then(|| json!({"fileData": {"mimeType": m.mime, "fileUri": url}}));
    }
    Some(json!({"inlineData": {"mimeType": m.mime, "data": m.inline()?}}))
}

/// Media generateContent cannot carry: URLs other than http(s) video.
pub fn check(req: &Request) -> Result<(), Error> {
    crate::hosted::declared(req, Protocol::GoogleGemini)?;
    if all_media(req).any(|m| media_part(m).is_none()) {
        return Err(inline_required("Gemini"));
    }
    Ok(())
}

/// The parts of one message. Tool-result media is nested in its function
/// response, or (legacy models, and `fileData`, which nested is accepted
/// but not seen) added to `pending` for a user turn of its own.
fn parts(m: &Message, omit_ids: bool, legacy: bool, pending: &mut Vec<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    // A parallel batch may carry one signature, on its first call; unsigned
    // siblings after it are valid as they are.
    let mut signed = false;
    for p in &m.parts {
        match p {
            Part::Text(t) if !t.is_empty() => out.push(json!({"text": t.as_str()})),
            // Thoughts go back only with the signature Gemini gave them.
            Part::Reasoning(r) if m.role == Role::Assistant => {
                if let Some(sig) = r
                    .meta_for(Protocol::GoogleGemini)
                    .and_then(|m| m.signature.as_deref())
                {
                    out.push(json!({"text": r.text, "thought": true, "thoughtSignature": sig}));
                }
            }
            // Generated media goes back with the signature it came with.
            Part::Media(media) => out.extend(media_part(media).map(|mut v| {
                let sig = media
                    .meta_of("google")
                    .and_then(|g| g.get("thoughtSignature"));
                if let Some(sig) = sig.filter(|_| m.role == Role::Assistant) {
                    v["thoughtSignature"] = sig.clone();
                }
                v
            })),
            Part::Hosted(h) if m.role == Role::Assistant => {
                out.extend(crate::hosted::gemini_replay(h))
            }
            Part::ToolCall {
                id, name, input, ..
            } => {
                let (base, sig) = split_id(id);
                let mut call = json!({"name": name, "args": if input.is_object() { input.clone() } else { json!({}) }});
                if !omit_ids {
                    call["id"] = json!(base);
                }
                let mut v = json!({ "functionCall": call });
                match sig {
                    Some(s) => {
                        v["thoughtSignature"] = json!(s);
                        signed = true;
                    }
                    None if !signed => v["thoughtSignature"] = json!(SKIP_SIGNATURE),
                    None => {}
                }
                out.push(v);
            }
            Part::ToolResult {
                id,
                name,
                content,
                is_error,
                media,
            } => {
                let key = if *is_error { "error" } else { "output" };
                let mut r = json!({"name": name, "response": {key: content}});
                if !omit_ids {
                    r["id"] = json!(split_id(id).0);
                }
                let mut nested = Vec::new();
                for v in media.iter().filter_map(media_part) {
                    if legacy || v.get("fileData").is_some() {
                        pending.push(v);
                    } else {
                        nested.push(v);
                    }
                }
                if !nested.is_empty() {
                    r["parts"] = json!(nested);
                }
                out.push(json!({ "functionResponse": r }));
            }
            _ => {}
        }
    }
    out
}

fn is_response_turn(c: &Value) -> bool {
    c["parts"]
        .as_array()
        .is_some_and(|ps| ps.iter().any(|p| p.get("functionResponse").is_some()))
}

pub fn body(req: &Request) -> Value {
    let omit_ids = omits_call_ids(&req.model);
    let legacy = legacy_tool_media(&req.model);
    let mut contents: Vec<Value> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    let flush = |contents: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            let mut ps = vec![json!({"text": "Attached media from tool result:"})];
            ps.append(pending);
            contents.push(json!({"role": "user", "parts": ps}));
        }
    };
    for m in &normalize_tool_history(&req.messages) {
        // Gemini has no per-message effort change.
        if effort_update(m).is_some() {
            continue;
        }
        if !m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. })) {
            flush(&mut contents, &mut pending);
        }
        // A system update is wrapped user text (after OpenCode), in a user
        // turn of its own after function responses.
        let wrapped;
        let m = if m.role == Role::System {
            wrapped = Message::user(wrap_system_update(m));
            &wrapped
        } else {
            m
        };
        let role = if m.role == Role::User {
            "user"
        } else {
            "model"
        };
        // Function responses get a user turn of their own: Gemini rejects a
        // function-response turn that carries other parts.
        let (responses, rest): (Vec<Value>, Vec<Value>) = parts(m, omit_ids, legacy, &mut pending)
            .into_iter()
            .partition(|p| p.get("functionResponse").is_some());
        for (ps, responding) in [(responses, true), (rest, false)] {
            if ps.is_empty() {
                continue;
            }
            match contents.last_mut() {
                Some(last) if last["role"] == role && is_response_turn(last) == responding => {
                    last["parts"].as_array_mut().unwrap().extend(ps)
                }
                _ => contents.push(json!({"role": role, "parts": ps})),
            }
        }
    }
    flush(&mut contents, &mut pending);
    let mut body = json!({"contents": contents});
    if !req.system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": req.system}]});
    }
    // Malformed hosted tools are rejected by `check` before anything is sent.
    let hosted = crate::hosted::declared(req, Protocol::GoogleGemini).unwrap_or_default();
    if !hosted.is_empty() {
        body["tools"] = json!(hosted);
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parametersJsonSchema": schema::project(&t.parameters, Some(schema::Sanitizer::Gemini))})).collect();
        body["tools"] = std::iter::once(json!({"functionDeclarations": decls}))
            .chain(hosted)
            .collect();
        if let Some(c) = &req.tool_choice {
            let config = match c {
                ToolChoice::Auto => json!({"mode": "AUTO"}),
                ToolChoice::None => json!({"mode": "NONE"}),
                ToolChoice::Required => json!({"mode": "ANY"}),
                ToolChoice::Tool(name) => json!({"mode": "ANY", "allowedFunctionNames": [name]}),
            };
            body["toolConfig"] = json!({"functionCallingConfig": config});
        }
    }
    let mut gen = Map::new();
    if let Some(n) = req.max_tokens {
        gen.insert("maxOutputTokens".into(), json!(n));
    }
    if let Some(t) = req.temperature {
        gen.insert("temperature".into(), json!(t));
    }
    let o = &req.options;
    for (key, wire) in [
        ("topP", "topP"),
        ("frequencyPenalty", "frequencyPenalty"),
        ("presencePenalty", "presencePenalty"),
    ] {
        if let Some(n) = o.num(key) {
            gen.insert(wire.into(), json!(n));
        }
    }
    for key in ["topK", "seed"] {
        if let Some(n) = o.int(key) {
            gen.insert(key.into(), json!(n));
        }
    }
    if let Some(s) = o.strings("stop") {
        gen.insert("stopSequences".into(), json!(s));
    }
    if let Some(t) = o.object("thinkingConfig") {
        gen.insert("thinkingConfig".into(), thinking_config(t, req.max_tokens));
    }
    if !gen.is_empty() {
        body["generationConfig"] = Value::Object(gen);
    }
    if let Some(c) = o.str("cachedContent") {
        body["cachedContent"] = json!(c);
    }
    if let Some(s) = o.get("safetySettings").filter(|s| {
        s.as_array().is_some_and(|a| {
            a.iter()
                .all(|x| x["category"].is_string() && x["threshold"].is_string())
        })
    }) {
        body["safetySettings"] = s.clone();
    }
    if let Some(t) = o.str("serviceTier") {
        body["serviceTier"] = json!(t);
    }
    merge_body(&mut body, &req.body);
    body
}

const MIN_THINKING_BUDGET: f64 = 512.0;

/// `generationConfig.thinkingConfig` from the option (after OpenCode):
/// malformed fields are dropped, thoughts are included unless turned off,
/// and the budget leaves room for the answer.
fn thinking_config(t: &Map<String, Value>, max_tokens: Option<u32>) -> Value {
    use crate::options::{field, fit_budget, number};
    let mut out = Map::new();
    if let Some(b) = field(t, "thinkingBudget").and_then(Value::as_f64) {
        out.insert(
            "thinkingBudget".into(),
            number(fit_budget(b, max_tokens, MIN_THINKING_BUDGET)),
        );
    }
    let include = field(t, "includeThoughts")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    out.insert("includeThoughts".into(), json!(include));
    if let Some(l) = field(t, "thinkingLevel").and_then(Value::as_str) {
        out.insert("thinkingLevel".into(), json!(l));
    }
    Value::Object(out)
}

/// One request's `usageMetadata`: the prompt with tool-use prompt, cached,
/// visible output, thoughts, total, billed cost (absent counts stay absent), and the
/// object as it came.
#[derive(Debug, Clone, Default)]
struct RequestUsage {
    prompt: Option<u64>,
    cached: u64,
    visible: Option<u64>,
    thoughts: u64,
    total: Option<u64>,
    /// The cost the request reports billed (each request is billed on its
    /// own).
    cost: Option<f64>,
    raw: Value,
}

impl RequestUsage {
    fn of(u: &Value) -> Self {
        let n = |k: &str| u[k].as_u64();
        let prompt = match (n("promptTokenCount"), n("toolUsePromptTokenCount")) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        RequestUsage {
            prompt,
            cached: n("cachedContentTokenCount").unwrap_or(0),
            visible: n("candidatesTokenCount"),
            thoughts: n("thoughtsTokenCount").unwrap_or(0),
            total: n("totalTokenCount"),
            cost: crate::billing::billed(u, &[]),
            raw: u.clone(),
        }
    }

    fn output(&self) -> u64 {
        self.visible.unwrap_or(0) + self.thoughts
    }
}

#[derive(Default)]
pub struct State {
    /// This request's usage, and that of the earlier requests of a
    /// continued response.
    usage: Option<RequestUsage>,
    continued: Vec<RequestUsage>,
    ids: Vec<String>,
    finish: Option<FinishReason>,
    /// The open thought block's id, and the signature it has so far.
    reasoning: Option<String>,
    signature: Option<String>,
    /// The open text block's id.
    text: Option<String>,
    /// Blocks of each kind so far, for ids.
    texts: usize,
    thoughts: usize,
    /// This request's latest continuation checkpoint and raw finish reason.
    token: Option<String>,
    raw_finish: Option<String>,
    /// Output arrived after the latest checkpoint: continuing from that
    /// checkpoint would send it again.
    past_checkpoint: bool,
    /// Why the prompt was blocked, when it was.
    blocked: Option<String>,
    /// The code execution call awaiting its result.
    executing: Option<String>,
    /// The latest search grounding, reported once the response ends.
    grounding: Option<Value>,
}

/// The sum of two billed costs, either of which may be unreported.
fn add_cost(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

/// Gemini's finish reason for a response it continues in another request.
const CONTINUATION: &str = "CONTINUATION";

impl State {
    /// Close an open thought block; a signed one ends with its signature.
    fn end_thinking(&mut self, out: &mut Vec<Event>) {
        if self.reasoning.take().is_some() {
            if let Some(sig) = self.signature.take() {
                let mut meta = ReasoningMeta::of(Protocol::GoogleGemini);
                meta.signature = Some(sig);
                out.push(Event::ReasoningEnd(meta));
            }
        }
    }

    fn end_text(&mut self, out: &mut Vec<Event>) {
        if let Some(id) = self.text.take() {
            out.push(Event::TextEnd(id));
        }
    }

    fn next_id(&mut self, text: bool) -> String {
        let (kind, n) = match text {
            true => ("text", &mut self.texts),
            false => ("reasoning", &mut self.thoughts),
        };
        *n += 1;
        format!("{kind}-{}", *n - 1)
    }

    /// The response's usage: the first request's input (later requests are
    /// billed for the same prompt again) with every request's output. The
    /// total is left out when neither the prompt nor the visible output was
    /// reported.
    fn usage(&self) -> (Usage, Option<Value>) {
        let cur = self.usage.clone();
        let all: Vec<&RequestUsage> = self.continued.iter().chain(cur.as_ref()).collect();
        let Some(first) = all.first() else {
            return (Usage::default(), None);
        };
        let prompt = first.prompt.unwrap_or(0);
        let output = all.iter().map(|u| u.output()).sum::<u64>();
        let known = first.prompt.is_some() || all.iter().any(|u| u.visible.is_some());
        let total = match all.as_slice() {
            [one] => one.total,
            _ => None,
        }
        .or(known.then_some(prompt + output));
        let usage = Usage {
            input: prompt.saturating_sub(first.cached),
            output,
            cache_read: first.cached,
            cache_write: 0,
            cost: all.iter().fold(None, |sum, u| add_cost(sum, u.cost)),
            reasoning: all.iter().map(|u| u.thoughts).sum(),
            total,
        };
        // A continued response's raw usage sums every request's counts.
        let raw = match all.as_slice() {
            [one] => one.raw.clone(),
            many => {
                let mut sum = Map::new();
                for k in [
                    "promptTokenCount",
                    "toolUsePromptTokenCount",
                    "cachedContentTokenCount",
                    "candidatesTokenCount",
                    "thoughtsTokenCount",
                    "totalTokenCount",
                ] {
                    let counts: Vec<u64> = many.iter().filter_map(|u| u.raw[k].as_u64()).collect();
                    if !counts.is_empty() {
                        sum.insert(k.into(), json!(counts.iter().sum::<u64>()));
                    }
                }
                sum.insert("requests".into(), json!(many.len()));
                Value::Object(sum)
            }
        };
        (usage, Some(raw))
    }
}

/// Finish reasons that mean a safety filter withheld the output.
const CONTENT_FILTERED: [&str; 10] = [
    "IMAGE_SAFETY",
    "RECITATION",
    "SAFETY",
    "BLOCKLIST",
    "PROHIBITED_CONTENT",
    "SPII",
    "MODEL_ARMOR",
    "IMAGE_PROHIBITED_CONTENT",
    "IMAGE_RECITATION",
    "LANGUAGE",
];

/// Finish reasons that mean the output is unusable; the first three are
/// worth a retry.
const FAILED: [&str; 6] = [
    "MALFORMED_RESPONSE",
    "MALFORMED_FUNCTION_CALL",
    "UNEXPECTED_TOOL_CALL",
    "NO_IMAGE",
    "TOO_MANY_TOOL_CALLS",
    "MISSING_THOUGHT_SIGNATURE",
];

pub fn handle(state: &mut State, v: &Value) -> Result<Vec<Event>, Error> {
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        let code = e["status"]
            .as_str()
            .map(str::to_string)
            .or_else(|| e["code"].as_u64().map(|n| n.to_string()));
        let err = stream_error(code.as_deref(), e["message"].as_str(), v, true);
        return Err(
            match (err, e["message"].as_str().filter(|m| !m.is_empty())) {
                (Error::Provider { retryable, .. }, Some(m)) => Error::Provider {
                    message: m.to_string(),
                    retryable,
                },
                (err, _) => err,
            },
        );
    }
    let mut out = Vec::new();
    if let Some(u) = v.get("usageMetadata").filter(|u| u.is_object()) {
        // Tool-use prompt tokens are input too.
        state.usage = Some(RequestUsage::of(u));
    }
    let Some(c) = v["candidates"].get(0) else {
        if let Some(r) = v["promptFeedback"]["blockReason"].as_str() {
            state.blocked = Some(r.to_string());
        }
        return Ok(out);
    };
    // The latest checkpoint, which a finished candidate without one clears.
    let finish = c["finishReason"]
        .as_str()
        .filter(|f| *f != "FINISH_REASON_UNSPECIFIED");
    let checkpoint = c["continuationToken"].as_str();
    match checkpoint {
        Some(t) => state.token = Some(t.to_string()),
        None if finish.is_some() => state.token = None,
        None => {}
    }
    if let Some(f) = finish {
        state.raw_finish = Some(f.to_string());
    }
    if let Some(g) = c.get("groundingMetadata").filter(|g| g.is_object()) {
        state.grounding = Some(g.clone());
    }
    for p in c["content"]["parts"].as_array().into_iter().flatten() {
        // Code the server ran, and what it printed.
        if let Some(code) = p.get("executableCode") {
            state.end_thinking(&mut out);
            state.end_text(&mut out);
            let id = crate::fresh_id();
            state.executing = Some(id.clone());
            out.push(Event::HostedCall(Hosted::call(
                Protocol::GoogleGemini,
                id,
                "code_execution",
                code.clone(),
            )));
            continue;
        }
        if let Some(r) = p.get("codeExecutionResult") {
            state.end_thinking(&mut out);
            state.end_text(&mut out);
            let id = state.executing.take().unwrap_or_else(crate::fresh_id);
            let result = HostedResult {
                is_error: r["outcome"].as_str().is_some_and(|o| o != "OUTCOME_OK"),
                value: r.clone(),
                media: vec![],
            };
            out.push(Event::HostedResult(
                Hosted::call(Protocol::GoogleGemini, id, "code_execution", json!({})).with(result),
            ));
            continue;
        }
        if p["thought"].as_bool() == Some(true) && p.get("functionCall").is_none() {
            let text = p["text"].as_str().filter(|t| !t.is_empty());
            if text.is_some() {
                state.end_text(&mut out);
            }
            if state.reasoning.is_none() {
                let id = state.next_id(false);
                out.push(Event::ReasoningStart(id.clone()));
                state.reasoning = Some(id);
            }
            if let Some(s) = p["thoughtSignature"].as_str().filter(|s| !s.is_empty()) {
                state.signature = Some(s.to_string());
            }
            if let Some(t) = text {
                out.push(Event::ReasoningDelta(t.to_string()));
            }
            continue;
        }
        if let Some(f) = p.get("functionCall") {
            state.end_thinking(&mut out);
            state.end_text(&mut out);
            // A repeated or missing id gets a fresh one.
            let base = f["id"]
                .as_str()
                .filter(|id| !id.is_empty() && !state.ids.iter().any(|s| s == id))
                .map(str::to_string)
                .unwrap_or_else(crate::fresh_id);
            state.ids.push(base.clone());
            let id = match p["thoughtSignature"].as_str() {
                Some(s) => format!("{base}{SIG}{s}"),
                None => base,
            };
            out.push(Event::ToolCall {
                id,
                name: f["name"].as_str().unwrap_or("").to_string(),
                input: f
                    .get("args")
                    .filter(|a| a.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            });
        } else if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
            state.end_thinking(&mut out);
            if state.text.is_none() {
                let id = state.next_id(true);
                out.push(Event::TextStart(id.clone()));
                state.text = Some(id);
            }
            out.push(Event::TextDelta(t.to_string()));
        }
    }
    // A checkpoint covers the output of its own chunk.
    if checkpoint.is_some() {
        state.past_checkpoint = false;
    } else if out.iter().any(|e| {
        matches!(
            e,
            Event::TextDelta(_) | Event::ReasoningDelta(_) | Event::ToolCall { .. }
        )
    }) {
        state.past_checkpoint = true;
    }
    if let Some(f) = finish.filter(|f| *f != CONTINUATION) {
        if FAILED.contains(&f) {
            return Err(Error::Provider {
                message: format!("Gemini stopped with {f}"),
                retryable: FAILED[..3].contains(&f),
            });
        }
        let normalized = match f {
            "STOP" => Reason::Stop,
            "MAX_TOKENS" => Reason::Length,
            _ if CONTENT_FILTERED.contains(&f) => Reason::ContentFilter,
            _ => Reason::Unknown,
        };
        state.finish = Some(FinishReason::new(normalized, f));
    }
    Ok(out)
}

/// Once a request's body ends: the token to continue the response with, when
/// the request stopped at a checkpoint (`CONTINUATION`, no finish, or a
/// dropped connection) rather than finishing.
pub fn resume(state: &mut State, dropped: bool) -> Result<Option<String>, Error> {
    let token = state.token.take();
    let finish = state.raw_finish.take();
    let unfinished = finish.as_deref().is_none_or(|f| f == CONTINUATION);
    let repeats = std::mem::take(&mut state.past_checkpoint);
    match token {
        // Gemini would generate the output after the checkpoint again, and
        // what was already passed on cannot be taken back: a dropped request
        // fails with its connection error, an ended one as cut short, and
        // the caller retries the whole response.
        Some(_) if unfinished && repeats => match dropped {
            true => Ok(None),
            false => Err(Error::Transport(
                "the Gemini response ended past its last checkpoint".into(),
            )),
        },
        Some(token) if unfinished => {
            // Usage is cumulative within one request; the next counts anew.
            state.continued.extend(state.usage.take());
            state.blocked = None;
            Ok(Some(token))
        }
        _ if !dropped && finish.as_deref() == Some(CONTINUATION) => Err(Error::Decode(
            "Gemini continued the response without a token".into(),
        )),
        _ => Ok(None),
    }
}

/// The events that end the stream: open blocks' ends, then `Finish`.
pub fn finish(state: &mut State) -> Result<Vec<Event>, Error> {
    if state.finish.is_none() {
        if let Some(r) = state.blocked.take() {
            return Err(Error::Decode(format!("prompt blocked: {r}")));
        }
    }
    let mut out = Vec::new();
    state.end_thinking(&mut out);
    state.end_text(&mut out);
    // Searches the model grounded its answer in, with the sources found.
    if let Some(g) = state.grounding.take() {
        let queries = g["webSearchQueries"].as_array().filter(|q| !q.is_empty());
        if let Some(queries) = queries {
            let call = Hosted::call(
                Protocol::GoogleGemini,
                crate::fresh_id(),
                "google_search",
                json!({"queries": queries}),
            );
            out.push(Event::HostedCall(call.clone()));
            out.push(Event::HostedResult(call.with(HostedResult {
                is_error: false,
                value: g.get("groundingChunks").cloned().unwrap_or(json!([])),
                media: vec![],
            })));
        }
    }
    // Gemini finishes a turn of calls with `STOP`.
    let mut reason = state.finish.take().unwrap_or_default();
    if !state.ids.is_empty() && matches!(reason.normalized, Reason::Stop) {
        reason.normalized = Reason::ToolCalls;
    }
    let (usage, raw_usage) = state.usage();
    out.push(Event::Finish {
        reason,
        usage,
        raw_usage,
    });
    Ok(out)
}

#[derive(Default)]
pub struct Decode {
    state: State,
}

impl Decoder for Decode {
    fn frame(&mut self, f: &sse::Frame) -> Result<Vec<Event>, Error> {
        let v: Value =
            serde_json::from_str(&f.data).map_err(|e| Error::Decode(format!("{e}: {}", f.data)))?;
        handle(&mut self.state, &v)
    }
    fn done(&self) -> bool {
        false
    }
    fn end(&mut self) -> Result<Vec<Event>, Error> {
        finish(&mut self.state)
    }
    fn resume(&mut self, dropped: bool) -> Result<Option<String>, Error> {
        resume(&mut self.state, dropped)
    }
}

/// The request body that continues a response from `token`.
pub fn continued(body: &Value, token: &str) -> Value {
    let mut b = body.clone();
    b["continuationToken"] = json!(token);
    b
}

pub async fn stream(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<EventStream, Error> {
    let body = body(req);
    let resp = crate::endpoint::send(post_body(p, client, &req.model, &body)).await?;
    // A response Gemini splits across requests continues with the same
    // body and the latest checkpoint's token.
    let (p, client, model) = (p.clone(), client.clone(), req.model.clone());
    let reopen: Reopen = Box::new(move |token: String| {
        let rb = post_body(&p, &client, &model, &continued(&body, &token));
        async move { crate::endpoint::send(rb).await }.boxed()
    });
    Ok(drive_resuming(resp, Decode::default(), Some(reopen)))
}

/// The generateContent request for `req`.
pub(crate) fn request(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> reqwest::RequestBuilder {
    post_body(p, client, &req.model, &body(req))
}

fn post_body(
    p: &Provider,
    client: &reqwest::Client,
    model: &str,
    body: &Value,
) -> reqwest::RequestBuilder {
    let path = format!("models/{model}:streamGenerateContent?alt=sse");
    p.json_post(client, &path, p.headers.clone(), body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolSpec;

    fn req(messages: Vec<Message>) -> Request {
        Request {
            model: "gemini-x".into(),
            system: "sys".into(),
            messages,
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "Read".into(),
                parameters: json!({"$schema": "x", "type": "object", "additionalProperties": false, "properties": {"path": {"type": ["string", "null"], "description": "p"}, "n": {"type": "integer", "enum": [1, 2]}}, "required": ["path"]}),
            }],
            max_tokens: Some(100),
            temperature: None,
            options: Default::default(),
            body: Map::new(),
            ..Default::default()
        }
    }

    #[test]
    fn request_body() {
        let msgs = vec![
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Reasoning("r".into()),
                    Part::Text("ok".into()),
                    Part::ToolCall {
                        id: "call_0~SIGX".into(),
                        name: "read".into(),
                        input: json!({"path": "a"}),
                        meta: None,
                    },
                ],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "call_0~SIGX".into(),
                    name: "read".into(),
                    content: "data".into(),
                    is_error: false,
                    media: vec![],
                }],
            },
            Message::user("<shell>done</shell>"),
        ];
        let b = body(&req(msgs));
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "sys");
        let c = b["contents"].as_array().unwrap();
        assert_eq!(
            c.len(),
            4,
            "the tool result keeps a turn of its own; the next user text follows it"
        );
        assert_eq!(c[1]["role"], "model");
        assert_eq!(
            c[1]["parts"],
            json!([{"text": "ok"}, {"functionCall": {"id": "call_0", "name": "read", "args": {"path": "a"}}, "thoughtSignature": "SIGX"}])
        );
        assert_eq!(
            c[2]["parts"],
            json!([{"functionResponse": {"id": "call_0", "name": "read", "response": {"output": "data"}}}])
        );
        assert_eq!(
            c[3],
            json!({"role": "user", "parts": [{"text": "<shell>done</shell>"}]})
        );
        let p = &b["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
        assert_eq!(
            p,
            &json!({"type": "object", "additionalProperties": false, "properties": {"path": {"type": ["string", "null"], "description": "p"}, "n": {"type": "integer", "enum": [1, 2]}}, "required": ["path"]})
        );
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 100);
    }

    #[test]
    fn stream_events() {
        let mut s = State::default();
        let a = crate::content(handle(&mut s, &json!({"candidates": [{"content": {"parts": [{"text": "think", "thought": true}, {"text": "Hel"}]}}]})).unwrap());
        assert_eq!(
            a,
            vec![
                Event::ReasoningDelta("think".into()),
                Event::TextDelta("Hel".into())
            ]
        );
        let b = handle(&mut s, &json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "read", "args": {"path": "a"}}, "thoughtSignature": "S1"}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 50, "cachedContentTokenCount": 20, "candidatesTokenCount": 7, "thoughtsTokenCount": 3}})).unwrap();
        assert!(matches!(
            &b[..],
            [Event::TextEnd(_), Event::ToolCall { id, name, input }]
                if id.starts_with("tool_") && id.ends_with("~S1") && name == "read" && *input == json!({"path": "a"})
        ));
        assert_eq!(
            finish(&mut s).unwrap().pop().unwrap(),
            Event::Finish {
                reason: FinishReason::new(Reason::ToolCalls, "STOP"),
                usage: Usage {
                    input: 30,
                    output: 10,
                    cache_read: 20,
                    cache_write: 0,
                    reasoning: 3,
                    total: Some(60),
                    ..Default::default()
                },
                raw_usage: Some(
                    json!({"promptTokenCount": 50, "cachedContentTokenCount": 20, "candidatesTokenCount": 7, "thoughtsTokenCount": 3})
                ),
            }
        );
        assert_eq!(split_id("call_0~S1"), ("call_0", Some("S1")));
        assert_eq!(split_id("call_0"), ("call_0", None));
    }

    #[test]
    fn errors_and_blocks() {
        let mut s = State::default();
        assert!(handle(&mut s, &json!({"error": {"message": "quota"}}))
            .unwrap_err()
            .to_string()
            .contains("quota"));
        // A blocked prompt fails once the stream ends without a finish.
        handle(
            &mut s,
            &json!({"promptFeedback": {"blockReason": "SAFETY"}}),
        )
        .unwrap();
        assert!(finish(&mut s).unwrap_err().to_string().contains("SAFETY"));
        let mut s = State::default();
        handle(&mut s, &json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "MAX_TOKENS"}]})).unwrap();
        assert!(
            matches!(finish(&mut s).unwrap().pop().unwrap(), Event::Finish { reason, .. } if reason.is_length())
        );
    }
}
