//! OpenAI Responses API streaming (`POST /responses`), stateless: every
//! request carries the whole conversation and nothing is stored server-side.

use crate::cache;
use crate::gemini::split_id;
use crate::{
    drive, merge_body, normalize_tool_history, post, schema, sse, stream_error, system_text,
    Decoder, Error, Event, EventStream, Media, MediaKind, Message, Part, Protocol, Provider,
    ReasoningMeta, Request, Role, ToolChoice, Usage,
};
use serde_json::{json, Map, Value};

/// Body fields from the request's provider options (after OpenCode's
/// `lowerGeneration`); a malformed option is dropped on its own.
fn lower_options(req: &Request) -> Map<String, Value> {
    let o = &req.options;
    let mut out = Map::new();
    let mut put = |k: &str, v: Value| {
        out.insert(k.into(), v);
    };
    if let Some(b) = o.bool("store") {
        put("store", json!(b));
    }
    if let Some(m) = o.string_record("metadata") {
        put("metadata", Value::Object(m));
    }
    if let Some(s) = o.str("safetyIdentifier") {
        put("safety_identifier", json!(s));
    }
    if let Some(b) = o
        .object("streamOptions")
        .and_then(|so| crate::options::field(so, "includeObfuscation"))
        .and_then(Value::as_bool)
    {
        put("stream_options", json!({"include_obfuscation": b}));
    }
    if let Some(n) = o.int("topLogprobs").filter(|n| (0..=20).contains(n)) {
        put("top_logprobs", json!(n));
    }
    if let Some(i) = o.strings("include").filter(|i| !i.is_empty()) {
        put("include", json!(i));
    }
    let mut reasoning = Map::new();
    if let Some(e) = o.reasoning_effort() {
        reasoning.insert("effort".into(), json!(e));
    }
    if let Some(s) = o
        .str("reasoningSummary")
        .filter(|s| matches!(*s, "auto" | "concise" | "detailed"))
    {
        reasoning.insert("summary".into(), json!(s));
    }
    for (key, wire) in [("reasoningMode", "mode"), ("reasoningContext", "context")] {
        if let Some(v) = o.str(key) {
            reasoning.insert(wire.into(), json!(v));
        }
    }
    if !reasoning.is_empty() {
        put("reasoning", Value::Object(reasoning));
    }
    if let Some(v) = o.str("textVerbosity") {
        put("text", json!({"verbosity": v}));
    }
    if let Some(v) = o.str("serviceTier") {
        put("service_tier", json!(v));
    }
    if let Some(v) = o
        .str("truncation")
        .filter(|t| matches!(*t, "auto" | "disabled"))
    {
        put("truncation", json!(v));
    }
    if let Some(n) = o.int("maxToolCalls") {
        put("max_tool_calls", json!(n));
    }
    if let Some(b) = o.bool("parallelToolCalls").or(req.parallel_tool_calls) {
        put("parallel_tool_calls", json!(b));
    }
    for (key, wire) in [
        ("topP", "top_p"),
        ("presencePenalty", "presence_penalty"),
        ("frequencyPenalty", "frequency_penalty"),
    ] {
        if let Some(n) = o.num(key) {
            put(wire, json!(n));
        }
    }
    // `allowedTools` restricts the declared tools (`tool_choice:
    // allowed_tools`); it only means something when tools are declared.
    if let Some(a) = o.object("allowedTools").filter(|_| !req.tools.is_empty()) {
        let names: Option<Vec<&str>> = crate::options::field(a, "toolNames")
            .and_then(Value::as_array)
            .and_then(|n| n.iter().map(Value::as_str).collect());
        let mode = a.get("mode").and_then(Value::as_str).unwrap_or("auto");
        if let Some(names) = names.filter(|n| !n.is_empty()) {
            if matches!(mode, "auto" | "none" | "required") {
                put(
                    "tool_choice",
                    json!({"type": "allowed_tools", "mode": mode, "tools": names.iter().map(|n| json!({"type": "function", "name": n})).collect::<Vec<_>>()}),
                );
            }
        }
    }
    out
}

/// One assistant message's input items. Reasoning replays as a `reasoning`
/// item when it carries encrypted state from this API (nothing is stored
/// server-side, so an item without it cannot be resolved) and some output
/// follows it in the message (the API rejects a reasoning item without its
/// following item). Reasoning parts of one item merge.
fn assistant_items(m: &Message, input: &mut Vec<Value>) {
    let has_output = |from: usize| {
        m.parts[from..].iter().any(|p| match p {
            Part::Text(t) => !t.trim().is_empty(),
            Part::ToolCall { .. } => true,
            _ => false,
        })
    };
    let mut items: Vec<(String, usize)> = Vec::new();
    for (i, p) in m.parts.iter().enumerate() {
        match p {
            Part::Text(t) if !t.trim().is_empty() => input.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": t}]})),
            Part::ToolCall { id, name, input: args } => input.push(json!({"type": "function_call", "call_id": split_id(id).0, "name": name, "arguments": args.to_string()})),
            Part::Reasoning(r) => {
                let Some(meta) = r.meta_for(Protocol::OpenaiResponses) else {
                    continue;
                };
                let id = meta.item_id.clone().unwrap_or_default();
                if let Some(&(_, at)) = items.iter().find(|(i, _)| !id.is_empty() && *i == id) {
                    let item = &mut input[at];
                    if !r.text.is_empty() {
                        item["summary"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"type": "summary_text", "text": r.text}));
                    }
                    if let Some(e) = &meta.encrypted {
                        item["encrypted_content"] = json!(e);
                    }
                    continue;
                }
                if !has_output(i + 1) {
                    continue;
                }
                let mut item = json!({"type": "reasoning", "summary": []});
                if !id.is_empty() {
                    item["id"] = json!(id);
                }
                if !r.text.is_empty() {
                    item["summary"] = json!([{"type": "summary_text", "text": r.text}]);
                }
                if let Some(e) = &meta.encrypted {
                    item["encrypted_content"] = json!(e);
                }
                items.push((id, input.len()));
                input.push(item);
            }
            _ => {}
        }
    }
    // Items still without encrypted state cannot be replayed statelessly.
    for (_, at) in items.into_iter().rev() {
        if input[at].get("encrypted_content").is_none() {
            input.remove(at);
        }
    }
}

/// An `input_image` / `input_file` / `input_video` item (after OpenCode's
/// open-responses `lowerMedia`); every media type lowers.
fn lower_media(m: &Media) -> Value {
    match (m.kind(), m.remote()) {
        (MediaKind::Image, _) => json!({"type": "input_image", "image_url": m.location()}),
        (MediaKind::Video, _) => json!({"type": "input_video", "video_url": m.location()}),
        (_, Some(url)) => json!({"type": "input_file", "file_url": url}),
        (kind, None) => json!({
            "type": "input_file",
            "filename": m.filename.as_deref().unwrap_or(if kind == MediaKind::Pdf { "document.pdf" } else { "file" }),
            "file_data": m.location(),
        }),
    }
}

pub fn body(req: &Request) -> Value {
    let mut input = Vec::new();
    for m in &normalize_tool_history(&req.messages) {
        // A user message with attachments is one message item, in order.
        if m.role == Role::User && m.parts.iter().any(|p| matches!(p, Part::Media(_))) {
            let content: Vec<Value> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(json!({"type": "input_text", "text": t})),
                    Part::Media(media) => Some(lower_media(media)),
                    _ => None,
                })
                .collect();
            for p in &m.parts {
                if let Part::ToolResult { .. } = p {
                    input.push(tool_output(p));
                }
            }
            input.push(json!({"type": "message", "role": "user", "content": content}));
            continue;
        }
        if m.role == Role::Assistant {
            assistant_items(m, &mut input);
            continue;
        }
        // A system update is a developer message in place (after OpenCode).
        if m.role == Role::System {
            input.push(json!({"type": "message", "role": "developer", "content": system_text(m)}));
            continue;
        }
        for p in &m.parts {
            match p {
                Part::Text(t) => input.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": t}]})),
                Part::ToolCall { id, name, input: args } => input.push(json!({"type": "function_call", "call_id": split_id(id).0, "name": name, "arguments": args.to_string()})),
                Part::ToolResult { .. } => input.push(tool_output(p)),
                _ => {}
            }
        }
    }
    // The system prompt leads `input` as a developer message, as in
    // OpenCode: newer GPT models never cache top-level `instructions`.
    if !req.system.is_empty() {
        input.insert(
            0,
            json!({"type": "message", "role": "developer", "content": req.system}),
        );
    }
    let mut body = json!({"model": req.model, "input": input, "stream": true, "store": false});
    if !req.tools.is_empty() {
        let sanitizer = schema::for_model(&req.model);
        body["tools"] = req.tools.iter().map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": schema::project(&t.parameters, sanitizer), "strict": false})).collect();
        // An `allowedTools` option (in `lower_options`) wins.
        if let Some(c) = &req.tool_choice {
            body["tool_choice"] = match c {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool(name) => json!({"type": "function", "name": name}),
            };
        }
    }
    // Sent unless caching is off or a `setCacheKey: false` option says the
    // server rejects it.
    if let Some(k) =
        cache::prompt_cache_key(req).filter(|_| req.options.bool("setCacheKey") != Some(false))
    {
        body["prompt_cache_key"] = json!(k);
    }
    if let Some((k, v)) = cache::openai_ttl(req.cache.lifetime()) {
        body[k] = v;
    }
    if let Some(n) = req.max_tokens {
        body["max_output_tokens"] = json!(n);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    merge_body(&mut body, &lower_options(req));
    merge_body(&mut body, &req.body);
    body
}

/// A `function_call_output`; with media its output is a content array.
fn tool_output(p: &Part) -> Value {
    let Part::ToolResult {
        id, content, media, ..
    } = p
    else {
        return Value::Null;
    };
    let output = if media.is_empty() {
        json!(content)
    } else {
        let text = (!content.is_empty()).then(|| json!({"type": "input_text", "text": content}));
        json!(text
            .into_iter()
            .chain(media.iter().map(lower_media))
            .collect::<Vec<_>>())
    };
    json!({"type": "function_call_output", "call_id": split_id(id).0, "output": output})
}

/// A function call between `output_item.added` and its completion.
#[derive(Debug, Default, Clone)]
struct Pending {
    output_index: Option<u64>,
    item_id: String,
    call_id: String,
    name: String,
    args: String,
}

/// A reasoning output item between `output_item.added` and its completion.
#[derive(Debug, Default)]
struct Thinking {
    id: String,
    encrypted: Option<String>,
    /// Some summary text has been sent as deltas.
    streamed: bool,
    /// Summary indices that streamed text.
    texted: Vec<u64>,
    /// A finished summary part; the next one starts after a blank line.
    separate: bool,
}

#[derive(Default)]
pub struct State {
    calls: usize,
    pending: Vec<Pending>,
    thinking: Option<Thinking>,
    pub done: bool,
}

impl State {
    /// Reasoning text, after a blank line when it starts a new summary part.
    fn reasoning(&mut self, text: &str, index: Option<u64>, out: &mut Vec<Event>) {
        if text.is_empty() {
            return;
        }
        if let Some(t) = &mut self.thinking {
            if std::mem::take(&mut t.separate) && t.streamed {
                out.push(Event::ReasoningDelta("\n\n".into()));
            }
            t.streamed = true;
            t.texted.extend(index);
        }
        out.push(Event::ReasoningDelta(text.to_string()));
    }

    /// End the open reasoning item, with its replay state.
    fn end_thinking(&mut self, out: &mut Vec<Event>) {
        if let Some(t) = self.thinking.take() {
            let mut meta = ReasoningMeta::of(Protocol::OpenaiResponses);
            meta.item_id = Some(t.id).filter(|i| !i.is_empty());
            meta.encrypted = t.encrypted;
            out.push(Event::ReasoningEnd(meta));
        }
    }
}

impl State {
    /// The pending call an event refers to: by output index, then item id.
    fn find(&mut self, v: &Value) -> Option<&mut Pending> {
        let idx = v["output_index"].as_u64();
        let item = v["item_id"].as_str();
        let at = self
            .pending
            .iter()
            .position(|p| idx.is_some() && p.output_index == idx)
            .or_else(|| {
                self.pending
                    .iter()
                    .position(|p| item.is_some_and(|i| p.item_id == i))
            })?;
        Some(&mut self.pending[at])
    }

    fn call(&mut self, call_id: &str, item_id: &str, name: &str, args: &str) -> Event {
        let input = if args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(args).unwrap_or_else(|_| json!({"__invalid_json": args}))
        };
        let id = [call_id, item_id]
            .into_iter()
            .find(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(crate::fresh_id);
        self.calls += 1;
        Event::ToolCall {
            id,
            name: name.to_string(),
            input,
        }
    }
}

fn item_str<'a>(it: &'a Value, key: &str) -> &'a str {
    it[key].as_str().unwrap_or("")
}

/// `code: message` from an error object, with the raw payload as fallback.
fn failure(e: &Value, raw: &Value, bare_is_transient: bool) -> Error {
    stream_error(
        e["code"].as_str().or(e["type"].as_str()),
        e["message"].as_str(),
        raw,
        bare_is_transient,
    )
}

pub fn handle(state: &mut State, kind: &str, v: &Value) -> Result<Vec<Event>, Error> {
    let mut out = Vec::new();
    match kind {
        // Refusals are ordinary assistant text.
        "response.output_text.delta" | "response.refusal.delta" => out.push(Event::TextDelta(
            v["delta"].as_str().unwrap_or("").to_string(),
        )),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let d = v["delta"].as_str().unwrap_or("");
            state.reasoning(d, v["summary_index"].as_u64(), &mut out);
        }
        // A final whose part streamed nothing carries the only copy.
        "response.reasoning_summary_text.done" => {
            let i = v["summary_index"].as_u64();
            let streamed = state.thinking.as_ref().is_some_and(|t| {
                t.texted.iter().any(|x| Some(*x) == i) || (i.is_none() && t.streamed)
            });
            if !streamed {
                state.reasoning(v["text"].as_str().unwrap_or(""), i, &mut out);
            }
        }
        "response.reasoning_summary_part.done" => {
            if let Some(t) = &mut state.thinking {
                t.separate = true;
            } else {
                out.push(Event::ReasoningDelta("\n\n".into()));
            }
        }
        "response.output_item.added" if v["item"]["type"] == "reasoning" => {
            state.end_thinking(&mut out);
            let it = &v["item"];
            state.thinking = Some(Thinking {
                id: item_str(it, "id").into(),
                encrypted: it["encrypted_content"].as_str().map(str::to_string),
                ..Default::default()
            });
        }
        "response.output_item.done" if v["item"]["type"] == "reasoning" => {
            let it = &v["item"];
            if state.thinking.is_none() {
                state.thinking = Some(Thinking {
                    id: item_str(it, "id").into(),
                    ..Default::default()
                });
            }
            if !state.thinking.as_ref().is_some_and(|t| t.streamed) {
                let summary: Vec<&str> = it["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s["text"].as_str())
                    .filter(|s| !s.is_empty())
                    .collect();
                state.reasoning(&summary.join("\n\n"), None, &mut out);
            }
            if let (Some(t), Some(e)) = (&mut state.thinking, it["encrypted_content"].as_str()) {
                t.encrypted = Some(e.to_string());
            }
            state.end_thinking(&mut out);
        }
        "response.output_item.added" if v["item"]["type"] == "function_call" => {
            let it = &v["item"];
            state.pending.push(Pending {
                output_index: v["output_index"].as_u64(),
                item_id: item_str(it, "id").into(),
                call_id: item_str(it, "call_id").into(),
                name: item_str(it, "name").into(),
                args: item_str(it, "arguments").into(),
            });
        }
        "response.function_call_arguments.delta" => {
            if let Some(p) = state.find(v) {
                p.args.push_str(v["delta"].as_str().unwrap_or(""));
            }
        }
        "response.function_call_arguments.done" => {
            if let (Some(p), Some(a)) = (state.find(v), v["arguments"].as_str()) {
                p.args = a.to_string();
            }
        }
        // The completed item is authoritative, even with empty arguments.
        "response.output_item.done" if v["item"]["type"] == "function_call" => {
            let it = &v["item"];
            let item_id = item_str(it, "id");
            state
                .pending
                .retain(|p| !(p.item_id == item_id && !item_id.is_empty()));
            out.push(state.call(
                item_str(it, "call_id"),
                item_id,
                item_str(it, "name"),
                item_str(it, "arguments"),
            ));
        }
        "response.completed" | "response.incomplete" => {
            // Reasoning never completed by an output item ends with what
            // its start said; the terminal output is not reconciled.
            state.end_thinking(&mut out);
            let r = &v["response"];
            let complete = kind == "response.completed";
            // Calls never completed by an output item: the final response
            // output wins, then what streamed. An incomplete response's
            // calls are cut off, so they are dropped.
            for p in std::mem::take(&mut state.pending) {
                if !complete {
                    continue;
                }
                let done = r["output"].as_array().into_iter().flatten().find(|o| {
                    o["type"] == "function_call"
                        && ((!p.item_id.is_empty() && o["id"] == p.item_id.as_str())
                            || (!p.call_id.is_empty() && o["call_id"] == p.call_id.as_str()))
                });
                let args = done
                    .and_then(|o| o["arguments"].as_str())
                    .unwrap_or(&p.args);
                let name = done.and_then(|o| o["name"].as_str()).unwrap_or(&p.name);
                let ev = state.call(&p.call_id, &p.item_id, name, args);
                out.push(ev);
            }
            let u = &r["usage"];
            let details = &u["input_tokens_details"];
            let cached = details["cached_tokens"].as_u64().unwrap_or(0);
            let written = details["cache_write_tokens"].as_u64().unwrap_or(0);
            let usage = Usage {
                input: u["input_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(cached + written),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: cached,
                cache_write: written,
            };
            let reason = match r["incomplete_details"]["reason"].as_str() {
                Some("max_output_tokens") => "length".to_string(),
                Some(other) => other.to_string(),
                None if state.calls > 0 => "tool_calls".into(),
                None if complete => "stop".into(),
                None => "incomplete".into(),
            };
            state.done = true;
            out.push(Event::Finish { reason, usage });
        }
        "response.failed" => {
            let r = &v["response"];
            return Err(match r.get("error").filter(|e| e.is_object()) {
                Some(e) => failure(e, v, false),
                None => stream_error(None, None, v, false),
            });
        }
        "error" => {
            // The error sits under `error`, (from some proxies) under
            // `response.error`, or at the top level.
            return Err(
                match [&v["error"], &v["response"]["error"]]
                    .into_iter()
                    .find(|e| e.is_object())
                {
                    Some(e) => failure(e, v, true),
                    // An explicit `error: null` is no sign of a transient fault.
                    None => stream_error(
                        v["code"].as_str(),
                        v["message"].as_str(),
                        v,
                        v.get("error").is_none(),
                    ),
                },
            );
        }
        _ => {}
    }
    Ok(out)
}

#[derive(Default)]
pub struct Decode {
    state: State,
}

impl Decoder for Decode {
    fn frame(&mut self, f: &sse::Frame) -> Result<Vec<Event>, Error> {
        if f.data.trim() == "[DONE]" {
            return Ok(vec![]);
        }
        let v: Value =
            serde_json::from_str(&f.data).map_err(|e| Error::Decode(format!("{e}: {}", f.data)))?;
        let kind = v["type"]
            .as_str()
            .map(str::to_string)
            .or(f.event.clone())
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
            Err(Error::Transport(
                "stream ended before response.completed".into(),
            ))
        }
    }
}

pub async fn stream(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<EventStream, Error> {
    let url = format!("{}/responses", p.base_url.trim_end_matches('/'));
    let mut rb = client.post(url);
    if let Some(k) = &p.api_key {
        rb = rb.bearer_auth(k);
    }
    for (k, v) in crate::openai::headers(p, req) {
        rb = rb.header(k, v);
    }
    let resp = post(rb, &body(req)).await?;
    Ok(drive(resp, Decode::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolSpec};

    #[test]
    fn request_body() {
        let req = Request {
            model: "gpt-x".into(),
            system: "sys".into(),
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Reasoning("r".into()),
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
                description: "Read".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: Some(64),
            temperature: None,
            options: Default::default(),
            body: Default::default(),
            ..Default::default()
        };
        let b = body(&req);
        assert!(b.get("instructions").is_none());
        assert_eq!(b["store"], false);
        assert_eq!(b["max_output_tokens"], 64);
        assert_eq!(
            b["input"],
            json!([
                {"type": "message", "role": "developer", "content": "sys"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]},
                {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "data"}
            ])
        );
        assert_eq!(
            b["tools"][0],
            json!({"type": "function", "name": "read", "description": "Read", "parameters": {"type": "object"}, "strict": false})
        );
    }

    #[test]
    fn stream_events() {
        let mut s = State::default();
        let mut all = Vec::new();
        for (k, v) in [
            ("response.created", json!({})),
            (
                "response.reasoning_summary_text.delta",
                json!({"delta": "plan"}),
            ),
            ("response.output_text.delta", json!({"delta": "Hi"})),
            (
                "response.function_call_arguments.delta",
                json!({"delta": "{\"pa"}),
            ),
            (
                "response.output_item.done",
                json!({"item": {"type": "function_call", "call_id": "c9", "name": "read", "arguments": "{\"path\":\"a\"}"}}),
            ),
            (
                "response.output_item.done",
                json!({"item": {"type": "message"}}),
            ),
            (
                "response.completed",
                json!({"response": {"usage": {"input_tokens": 100, "output_tokens": 9, "input_tokens_details": {"cached_tokens": 40}}}}),
            ),
        ] {
            all.extend(handle(&mut s, k, &v).unwrap());
        }
        assert_eq!(
            all,
            vec![
                Event::ReasoningDelta("plan".into()),
                Event::TextDelta("Hi".into()),
                Event::ToolCall {
                    id: "c9".into(),
                    name: "read".into(),
                    input: json!({"path": "a"})
                },
                Event::Finish {
                    reason: "tool_calls".into(),
                    usage: Usage {
                        input: 60,
                        output: 9,
                        cache_read: 40,
                        cache_write: 0
                    }
                },
            ]
        );
        assert!(s.done);
    }

    #[test]
    fn incomplete_and_failures() {
        let mut s = State::default();
        let ev = handle(&mut s, "response.incomplete", &json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {}}})).unwrap();
        assert!(matches!(&ev[0], Event::Finish { reason, .. } if reason == "length"));
        assert!(handle(
            &mut s,
            "response.failed",
            &json!({"response": {"error": {"message": "overloaded"}}})
        )
        .unwrap_err()
        .to_string()
        .contains("overloaded"));
        assert!(handle(&mut s, "error", &json!({"message": "bad key"}))
            .unwrap_err()
            .to_string()
            .contains("bad key"));
    }
}
