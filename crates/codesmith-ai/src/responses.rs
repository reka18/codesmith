//! OpenAI Responses API streaming (`POST /responses`), stateless: every
//! request carries the whole conversation and nothing is stored server-side.

use crate::cache;
use crate::gemini::split_id;
use crate::{
    drive, effort_update, merge_body, meta_for, normalize_tool_history, resolve_effort_updates,
    schema, sse, stream_error, system_text, Decoder, Error, Event, EventStream, FinishReason,
    Hosted, Media, MediaKind, Message, Part, Protocol, Provider, Reason, ReasoningMeta, Request,
    Role, ToolChoice, Usage,
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

/// A provider-issued item id worth replaying: a nonempty prefix, `_`, and a
/// nonempty suffix (after OpenCode's `itemID`; the prefix is the
/// provider's own and need not name the item's type).
fn item_id(meta: Option<&ReasoningMeta>) -> Option<&str> {
    let id = meta?.item_id.as_deref()?;
    let sep = id.find('_')?;
    (sep > 0 && sep < id.len() - 1).then_some(id)
}

/// A `function_call` item, with its item id when it has a valid one.
fn function_call(id: &str, name: &str, args: &Value, meta: &Option<ReasoningMeta>) -> Value {
    let mut item = json!({"type": "function_call", "call_id": split_id(id).0, "name": name, "arguments": args.to_string()});
    if let Some(i) = item_id(meta_for(meta, Protocol::OpenaiResponses)) {
        item["id"] = json!(i);
    }
    item
}

/// One assistant message's input items (after OpenCode's `lowerMessages`).
/// Consecutive text of one message item (same item id and phase) is one
/// completed `message` item, with its id and phase when it has them.
/// Reasoning replays as a `reasoning` item when it carries encrypted state
/// from this API (nothing is stored server-side, so an item without it
/// cannot be resolved) and some output follows it in the message (the API
/// rejects a reasoning item without its following item). Reasoning parts of
/// one item merge. Media the model generated (Responses has no assistant
/// image item) replays as user input.
fn assistant_items(m: &Message, input: &mut Vec<Value>, video: VideoAs) {
    let has_output = |from: usize| {
        m.parts[from..].iter().any(|p| match p {
            Part::Text(t) => !t.trim().is_empty(),
            Part::ToolCall { .. } | Part::Hosted(_) => true,
            _ => false,
        })
    };
    // The message item text is going into: (id, phase, at).
    type Group<'a> = (Option<&'a str>, Option<&'a Option<String>>, usize);
    let mut group: Option<Group> = None;
    let mut items: Vec<(String, usize)> = Vec::new();
    let mut hosted = Vec::new();
    for (i, p) in m.parts.iter().enumerate() {
        if let Part::Text(t) = p {
            if t.trim().is_empty() {
                continue;
            }
            let meta = t.meta_for(Protocol::OpenaiResponses);
            let id = item_id(meta);
            let phase = meta.and_then(|m| m.phase.as_ref());
            let block = json!({"type": "output_text", "text": t.as_str()});
            match group {
                Some((gid, gphase, at)) if gid == id && gphase == phase => {
                    input[at]["content"].as_array_mut().unwrap().push(block);
                }
                _ => {
                    let mut item = json!({"type": "message", "role": "assistant", "status": "completed", "content": [block]});
                    if let Some(id) = id {
                        item["id"] = json!(id);
                    }
                    if let Some(phase) = phase {
                        item["phase"] = json!(phase);
                    }
                    group = Some((id, phase, input.len()));
                    input.push(item);
                }
            }
            continue;
        }
        group = None;
        match p {
            Part::ToolCall {
                id,
                name,
                input: args,
                meta,
            } => input.push(function_call(id, name, args, meta)),
            Part::Media(media) => input.push(
                json!({"type": "message", "role": "user", "content": [lower_media(media, video)]}),
            ),
            Part::Compaction(c) => {
                let mut item = json!({"type": "compaction", "encrypted_content": c.encrypted});
                if let Some(id) = &c.id {
                    item["id"] = json!(id);
                }
                input.push(item);
            }
            Part::Hosted(h) => input.extend(crate::hosted::responses_replay(h, &mut hosted, |m| {
                lower_media(m, video)
            })),
            Part::Reasoning(r) => {
                let Some(meta) = r.meta_for(Protocol::OpenaiResponses) else {
                    continue;
                };
                let id = item_id(Some(meta)).unwrap_or_default().to_string();
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

/// How a server takes video.
#[derive(Clone, Copy, PartialEq)]
enum VideoAs {
    /// `input_video` (the shared protocol).
    Video,
    /// A named `input_file` (xAI).
    File,
}

/// An `input_image` / `input_file` / `input_video` item (after OpenCode's
/// open-responses `lowerMedia`); every media type lowers. An image's
/// `openai.detail` option is passed on.
fn lower_media(m: &Media, video: VideoAs) -> Value {
    let mut v = match (m.kind(), m.remote()) {
        (MediaKind::Image, _) => json!({"type": "input_image", "image_url": m.location()}),
        (MediaKind::Video, Some(url)) if video == VideoAs::File => {
            json!({"type": "input_file", "filename": m.filename.as_deref().unwrap_or("file"), "file_url": url})
        }
        (MediaKind::Video, None) if video == VideoAs::File => {
            json!({"type": "input_file", "filename": m.filename.as_deref().unwrap_or("file"), "file_data": m.location()})
        }
        (MediaKind::Video, _) => json!({"type": "input_video", "video_url": m.location()}),
        (_, Some(url)) => json!({"type": "input_file", "file_url": url}),
        (kind, None) => json!({
            "type": "input_file",
            "filename": m.filename.as_deref().unwrap_or(if kind == MediaKind::Pdf { "document.pdf" } else { "file" }),
            "file_data": m.location(),
        }),
    };
    let detail = m.meta_of("openai").and_then(|o| o.get("detail"));
    if let Some(d) = detail.filter(|_| v["type"] != "input_video") {
        v["detail"] = d.clone();
    }
    v
}

/// Whether the model takes a per-message effort change as a
/// `configuration_update` item (after OpenCode's `supportsEffortUpdates`):
/// GPT-6 and later, except in `reasoningMode: pro` or with automatic
/// context management. An `effortUpdates` option overrides the detection.
fn effort_updates(req: &Request) -> bool {
    let o = &req.options;
    if o.get("contextManagement").is_some()
        || o.str("reasoningMode") == Some("pro")
        || req
            .body
            .get("reasoning")
            .is_some_and(|r| r["mode"] == "pro")
    {
        return false;
    }
    if let Some(b) = o.bool("effortUpdates") {
        return b;
    }
    let m = req.model.to_ascii_lowercase();
    let name = m.rsplit('/').next().unwrap_or(&m);
    let Some(rest) = name.strip_prefix("gpt-") else {
        return false;
    };
    let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = &rest[major.len()..];
    let after = after
        .strip_prefix('.')
        .map(|a| a.trim_start_matches(|c: char| c.is_ascii_digit()))
        .unwrap_or(after);
    (after.is_empty() || after.starts_with('-')) && major.parse::<u32>().is_ok_and(|n| n >= 6)
}

/// The effort a per-message change back to the default sends.
const DEFAULT_EFFORT: &str = "medium";

#[cfg(test)]
pub fn body(req: &Request) -> Value {
    body_for(req, "")
}

/// The request body for a server at `base_url`.
pub fn body_for(req: &Request, base_url: &str) -> Value {
    let url = base_url.to_ascii_lowercase();
    let video = if url.contains("api.x.ai") {
        VideoAs::File
    } else {
        VideoAs::Video
    };
    let (efforts, effort) = if effort_updates(req) {
        resolve_effort_updates(&req.messages, req.options.reasoning_effort())
    } else {
        (false, req.options.reasoning_effort().map(str::to_string))
    };
    let mut input = Vec::new();
    for m in &normalize_tool_history(&req.messages) {
        if let Some((e, _)) = effort_update(m) {
            if efforts {
                // Consecutive updates are rejected; the newer one wins.
                if input
                    .last()
                    .is_some_and(|l: &Value| l["type"] == "configuration_update")
                {
                    input.pop();
                }
                input.push(json!({"type": "configuration_update", "reasoning": {"effort": e.unwrap_or(DEFAULT_EFFORT)}}));
            }
            continue;
        }
        // A user message with attachments is one message item, in order.
        if m.role == Role::User && m.parts.iter().any(|p| matches!(p, Part::Media(_))) {
            let content: Vec<Value> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(json!({"type": "input_text", "text": t.as_str()})),
                    Part::Media(media) => Some(lower_media(media, video)),
                    _ => None,
                })
                .collect();
            for p in &m.parts {
                if let Part::ToolResult { .. } = p {
                    input.push(tool_output(p, video));
                }
            }
            input.push(json!({"type": "message", "role": "user", "content": content}));
            continue;
        }
        if m.role == Role::Assistant {
            assistant_items(m, &mut input, video);
            continue;
        }
        // A system update is a developer message in place (after OpenCode).
        if m.role == Role::System {
            input.push(json!({"type": "message", "role": "developer", "content": system_text(m)}));
            continue;
        }
        for p in &m.parts {
            match p {
                Part::Text(t) => input.push(json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": t.as_str()}]})),
                Part::ToolCall { id, name, input: args, meta } => input.push(function_call(id, name, args, meta)),
                Part::ToolResult { .. } => input.push(tool_output(p, video)),
                _ => {}
            }
        }
    }
    if url.contains("api.meta.ai") {
        hoist_videos(&mut input);
    }
    // The system prompt leads `input` as a developer message, as in
    // OpenCode: newer GPT models never cache top-level `instructions`.
    if !req.system.is_empty() {
        input.insert(
            0,
            json!({"type": "message", "role": "developer", "content": req.system}),
        );
    }
    namespace_calls(&mut input, &req.namespaces);
    let mut body = json!({"model": req.model, "input": input, "stream": true, "store": false});
    // Malformed hosted tools are rejected by `check` before anything is sent.
    let hosted = crate::hosted::declared(req, Protocol::OpenaiResponses).unwrap_or_default();
    if !req.tools.is_empty() || !req.namespaces.is_empty() || !hosted.is_empty() {
        let sanitizer = schema::for_model(&req.model);
        let function = |t: &crate::ToolSpec| json!({"type": "function", "name": t.name, "description": t.description, "parameters": schema::project(&t.parameters, sanitizer), "strict": false});
        let mut tools: Vec<Value> = req.tools.iter().map(function).collect();
        // Namespaces natively (OpenCode's), their leaves under their own
        // names; OpenAI requires a description.
        tools.extend(req.namespaces.iter().map(|n| {
            let description = n
                .description
                .clone()
                .unwrap_or_else(|| format!("Tools in the {} namespace.", n.name));
            json!({"type": "namespace", "name": n.name, "description": description,
                "tools": n.leaves().iter().map(function).collect::<Vec<_>>()})
        }));
        tools.extend(hosted.iter().cloned());
        body["tools"] = json!(tools);
        // An `allowedTools` option (in `lower_options`) wins.
        if let Some(c) = &req.tool_choice {
            body["tool_choice"] = match c {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool(name) => crate::hosted::responses_choice(req, &hosted)
                    .unwrap_or_else(|| json!({"type": "function", "name": name})),
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
    let mut options = lower_options(req);
    if efforts {
        let reasoning = options.entry("reasoning").or_insert_with(|| json!({}));
        match &effort {
            Some(e) => reasoning["effort"] = json!(e),
            None => {
                reasoning.as_object_mut().map(|r| r.remove("effort"));
            }
        }
        if reasoning.as_object().is_some_and(|r| r.is_empty()) {
            options.remove("reasoning");
        }
    }
    merge_body(&mut body, &options);
    merge_body(&mut body, &req.body);
    body
}

/// Reject malformed hosted tool declarations before anything is sent.
pub fn check(req: &Request) -> Result<(), Error> {
    crate::hosted::declared(req, Protocol::OpenaiResponses).map(drop)
}

/// A `function_call_output`; with media its output is a content array.
fn tool_output(p: &Part, video: VideoAs) -> Value {
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
            .chain(media.iter().map(|m| lower_media(m, video)))
            .collect::<Vec<_>>())
    };
    json!({"type": "function_call_output", "call_id": split_id(id).0, "output": output})
}

/// Meta cannot see video in tool results: each batch of consecutive
/// results sends its videos in a user message after it, and a result left
/// with nothing else says where its video went (after OpenCode's
/// `MetaResponses`).
fn hoist_videos(input: &mut Vec<Value>) {
    let mut out = Vec::with_capacity(input.len());
    let mut videos: Vec<Value> = Vec::new();
    let flush = |out: &mut Vec<Value>, videos: &mut Vec<Value>| {
        if !videos.is_empty() {
            out.push(json!({"type": "message", "role": "user", "content": std::mem::take(videos)}));
        }
    };
    for mut item in input.drain(..) {
        if item["type"] != "function_call_output" {
            flush(&mut out, &mut videos);
            out.push(item);
            continue;
        }
        if let Some(parts) = item["output"].as_array_mut() {
            let before = parts.len();
            let (moved, kept): (Vec<Value>, Vec<Value>) =
                parts.drain(..).partition(|p| p["type"] == "input_video");
            videos.extend(moved);
            item["output"] = if kept.is_empty() && before > 0 {
                json!("Video attached in the following user message.")
            } else {
                json!(kept)
            };
        }
        out.push(item);
    }
    flush(&mut out, &mut videos);
    *input = out;
}

/// A function call between `output_item.added` and its completion.
#[derive(Debug, Default, Clone)]
struct Pending {
    output_index: Option<u64>,
    item_id: String,
    call_id: String,
    name: String,
    args: String,
    /// The id its input streams under.
    stream_id: String,
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
    /// Its reasoning block has started.
    started: bool,
}

#[derive(Default)]
pub struct State {
    calls: usize,
    pending: Vec<Pending>,
    thinking: Option<Thinking>,
    /// The message item whose text block is open, and whether that is the
    /// item's own id (not one made up for an item without one).
    text: Option<String>,
    text_item: bool,
    /// Message items whose text streamed, and the phase each said it was
    /// in (`Some(None)` for `phase: null`).
    texted: Vec<String>,
    phases: Vec<(String, Option<String>)>,
    /// Message items by output index, which wins over an event's item id.
    indexed: Vec<(u64, String)>,
    /// Message items added so far, and those a later item concluded: late
    /// text for a concluded item is dropped.
    messages: Vec<String>,
    concluded: Vec<String>,
    /// Compaction checkpoints already delivered, by encrypted content.
    compactions: Vec<String>,
    /// Blocks started, for ids of blocks without an item id.
    blocks: usize,
    /// The item id first added at each output index: an event's output
    /// index names its item (GitHub Copilot sends a different id on every
    /// event of an item).
    outputs: std::collections::HashMap<u64, String>,
    pub done: bool,
}

impl State {
    /// Reasoning text, after a blank line when it starts a new summary part.
    fn reasoning(&mut self, text: &str, index: Option<u64>, out: &mut Vec<Event>) {
        if text.is_empty() {
            return;
        }
        if let Some(t) = &mut self.thinking {
            if !std::mem::replace(&mut t.started, true) {
                let id = match t.id.as_str() {
                    "" => format!("reasoning-{}", self.blocks),
                    id => id.to_string(),
                };
                self.blocks += 1;
                out.push(Event::ReasoningStart(id));
            }
            if std::mem::take(&mut t.separate) && t.streamed {
                out.push(Event::ReasoningDelta("\n\n".into()));
            }
            t.streamed = true;
            t.texted.extend(index);
        }
        out.push(Event::ReasoningDelta(text.to_string()));
    }

    /// Text for a message item: dropped when a later item concluded it,
    /// else in that item's text block.
    fn text(&mut self, item: Option<&str>, text: &str, out: &mut Vec<Event>) {
        if text.is_empty() || item.is_some_and(|i| self.concluded.iter().any(|c| c == i)) {
            return;
        }
        let same = match (&self.text, item) {
            (Some(open), Some(i)) => open == i,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if !same {
            self.end_text(out);
            let real = item.filter(|i| !i.is_empty());
            let id = match real {
                Some(i) => i.to_string(),
                None => format!("text-{}", self.blocks),
            };
            self.blocks += 1;
            out.push(Event::TextStart(id.clone()));
            self.text = Some(id);
            self.text_item = real.is_some();
        }
        if let Some(open) = self.text.as_ref().filter(|_| self.text_item) {
            if !self.texted.contains(open) {
                self.texted.push(open.clone());
            }
        }
        out.push(Event::TextDelta(text.to_string()));
    }

    /// End the open text block, with its message item's id and phase.
    fn end_text(&mut self, out: &mut Vec<Event>) {
        if let Some(id) = self.text.take() {
            out.push(Event::TextEnd(id.clone()));
            if std::mem::take(&mut self.text_item) {
                let mut meta = ReasoningMeta::of(Protocol::OpenaiResponses);
                meta.phase = self
                    .phases
                    .iter()
                    .find(|(i, _)| *i == id)
                    .map(|(_, p)| p.clone());
                meta.item_id = Some(id);
                out.push(Event::PartMeta(meta));
            }
        }
    }

    /// The message item a text event belongs to: by output index, then
    /// item id.
    fn message_item(&self, v: &Value) -> Option<String> {
        let by_index = v["output_index"]
            .as_u64()
            .and_then(|i| self.indexed.iter().find(|(x, _)| *x == i));
        by_index
            .map(|(_, id)| id.clone())
            .or_else(|| v["item_id"].as_str().map(str::to_string))
    }

    /// Note a message item's phase, when the item states one.
    fn phase(&mut self, item: &Value) {
        let (Some(id), Some(phase)) = (item["id"].as_str(), item.get("phase")) else {
            return;
        };
        let phase = phase.as_str().map(str::to_string);
        self.phases.retain(|(i, _)| i != id);
        self.phases.push((id.to_string(), phase));
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
    /// The event with its item ids resolved through its output index.
    fn normalize(&self, v: &Value) -> Value {
        let mut v = v.clone();
        if let Some(id) = v["output_index"]
            .as_u64()
            .and_then(|i| self.outputs.get(&i))
        {
            if v["item_id"].is_string() {
                v["item_id"] = json!(id);
            }
            if v["item"].is_object() {
                v["item"]["id"] = json!(id);
            }
        }
        v
    }

    /// Whether the open reasoning item is `item` (an item without an id
    /// takes any).
    fn thinking_is(&self, item: &str) -> bool {
        self.thinking
            .as_ref()
            .is_some_and(|t| t.id.is_empty() || t.id == item)
    }

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

    /// A complete call, then its item id.
    fn call(&mut self, call: &str, item_id: &str, name: &str, args: &str, out: &mut Vec<Event>) {
        let input = if args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(args).unwrap_or_else(|_| json!({"__invalid_json": args}))
        };
        let id = call_id(call, item_id);
        self.calls += 1;
        out.push(Event::ToolCall {
            id,
            name: name.to_string(),
            input,
        });
        if !item_id.is_empty() {
            let mut meta = ReasoningMeta::of(Protocol::OpenaiResponses);
            meta.item_id = Some(item_id.to_string());
            out.push(Event::PartMeta(meta));
        }
    }
}

/// A call's id: its call id, else its item id, else a fresh one.
fn call_id(call_id: &str, item_id: &str) -> String {
    [call_id, item_id]
        .into_iter()
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(crate::fresh_id)
}

/// A function call item's name: a call into a namespace is named
/// `<namespace>_<name>`, as the other protocols declare it.
fn call_name(it: &Value) -> String {
    match it["namespace"].as_str().filter(|n| !n.is_empty()) {
        Some(n) => format!("{n}_{}", item_str(it, "name")),
        None => item_str(it, "name").to_string(),
    }
}

/// Replayed calls to a declared namespace's tools go back with their
/// namespace and their name within it.
fn namespace_calls(input: &mut [Value], namespaces: &[crate::ToolNamespace]) {
    for item in input.iter_mut().filter(|i| i["type"] == "function_call") {
        let Some(name) = item["name"].as_str() else {
            continue;
        };
        let found = namespaces.iter().find_map(|n| {
            let leaf = name.strip_prefix(&n.name)?.strip_prefix('_')?;
            n.leaves()
                .iter()
                .any(|t| t.name == leaf)
                .then(|| (n.name.clone(), leaf.to_string()))
        });
        if let Some((namespace, leaf)) = found {
            item["namespace"] = json!(namespace);
            item["name"] = json!(leaf);
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

/// The rule that blocked a response, from GitHub Copilot's list of the
/// filters that ran: the blocking completion filter's `BLOCK` rule, or the
/// filter that fired in its `content_filter_results`.
fn filter_category(filters: &Value) -> Option<String> {
    let blocked: Vec<&Value> = filters
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["blocked"] == true)
        .collect();
    let f = blocked
        .iter()
        .find(|f| f["source_type"] == "completion")
        .or(blocked.first())?;
    f["content_filter_raw"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["action"] == "BLOCK" && r["result"] == true)
        .and_then(|r| r["label"].as_str())
        .map(str::to_string)
        .or_else(|| crate::filter_category(&[&f["content_filter_results"]]))
}

/// Events whose `item_id` the spec requires (OpenCode rejects them
/// without one).
const NEED_ITEM: [&str; 12] = [
    "response.output_text.delta",
    "response.output_text.done",
    "response.reasoning.delta",
    "response.reasoning_summary_text.delta",
    "response.reasoning_text.delta",
    "response.reasoning.done",
    "response.reasoning_summary_text.done",
    "response.reasoning_text.done",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
    "response.function_call_arguments.delta",
    "response.function_call_arguments.done",
];

/// The checks OpenCode makes before an event's handling: required item ids,
/// well-formed refusals, and one reasoning item at a time.
fn validate(state: &State, kind: &str, v: &Value) -> Result<(), Error> {
    let quoted = || crate::quote(&v.to_string());
    let missing = || Error::Decode(format!("{kind} is missing item_id: {}", quoted()));
    match kind {
        _ if NEED_ITEM.contains(&kind) && !v["item_id"].is_string() => Err(missing()),
        "response.refusal.delta" | "response.refusal.done" => {
            let value = if kind == "response.refusal.delta" {
                "delta"
            } else {
                "refusal"
            };
            match v["item_id"].is_string() && v[value].is_string() {
                true => Ok(()),
                false => Err(Error::Decode(format!("{kind} is malformed: {}", quoted()))),
            }
        }
        "response.output_item.added"
            if v["item"]["type"] == "reasoning"
                && state
                    .thinking
                    .as_ref()
                    .is_some_and(|t| t.id != item_str(&v["item"], "id")) =>
        {
            Err(Error::Decode(format!(
                "{kind} started reasoning before the previous item ended: {}",
                quoted()
            )))
        }
        _ => Ok(()),
    }
}

pub fn handle(state: &mut State, kind: &str, v: &Value) -> Result<Vec<Event>, Error> {
    let v = &state.normalize(v);
    validate(state, kind, v)?;
    if kind == "response.output_item.added" {
        if let (Some(i), Some(id)) = (
            v["output_index"].as_u64(),
            v["item"]["id"].as_str().filter(|s| !s.is_empty()),
        ) {
            state.outputs.entry(i).or_insert_with(|| id.to_string());
        }
    }
    let mut out = Vec::new();
    match kind {
        // Refusals are ordinary assistant text, and text goes only to the
        // message item being streamed (an orphan delta is dropped).
        "response.output_text.delta" | "response.refusal.delta" => {
            let item = state.message_item(v);
            if item.is_some() && state.messages.last() == item.as_ref() {
                let d = v["delta"].as_str().unwrap_or("");
                state.text(item.as_deref(), d, &mut out);
            }
        }
        // The final text of an item that streamed none is its only copy.
        "response.output_text.done" => {
            let item = state.message_item(v);
            let open = item.is_some() && state.messages.last() == item.as_ref();
            let item = item.as_deref();
            let streamed = match item {
                Some(i) => state.texted.iter().any(|t| t == i),
                None => state.text.is_some(),
            };
            if open && !streamed {
                let t = v["text"].as_str().unwrap_or("");
                state.text(item, t, &mut out);
            }
        }
        // A new message concludes the earlier ones, even without their done.
        "response.output_item.added" if v["item"]["type"] == "message" => {
            state.phase(&v["item"]);
            let id = item_str(&v["item"], "id");
            if let (Some(i), false) = (v["output_index"].as_u64(), id.is_empty()) {
                state.indexed.push((i, id.to_string()));
            }
            if !state.messages.iter().any(|m| m == id) {
                state.end_text(&mut out);
                let earlier = std::mem::take(&mut state.messages);
                state.concluded.extend(earlier);
                state.messages.push(id.to_string());
            }
        }
        "response.output_item.done" if v["item"]["type"] == "message" => {
            state.phase(&v["item"]);
            let id = item_str(&v["item"], "id");
            if state.text.as_deref() == Some(id) {
                state.end_text(&mut out);
            }
            if !id.is_empty() {
                state.concluded.push(id.to_string());
            }
        }
        // Reasoning goes only to the open reasoning item.
        "response.reasoning.delta"
        | "response.reasoning_summary_text.delta"
        | "response.reasoning_text.delta"
            if state.thinking_is(item_str(v, "item_id")) =>
        {
            let d = v["delta"].as_str().unwrap_or("");
            state.reasoning(d, v["summary_index"].as_u64(), &mut out);
        }
        // A final whose part streamed nothing carries the only copy.
        "response.reasoning.done"
        | "response.reasoning_summary_text.done"
        | "response.reasoning_text.done"
            if state.thinking_is(item_str(v, "item_id")) =>
        {
            let i = v["summary_index"].as_u64();
            let streamed = state.thinking.as_ref().is_some_and(|t| {
                t.texted.iter().any(|x| Some(*x) == i) || (i.is_none() && t.streamed)
            });
            if !streamed {
                state.reasoning(v["text"].as_str().unwrap_or(""), i, &mut out);
            }
        }
        "response.reasoning_summary_part.done" if state.thinking_is(item_str(v, "item_id")) => {
            if let Some(t) = &mut state.thinking {
                t.separate = true;
            }
        }
        "response.output_item.added" if v["item"]["type"] == "reasoning" => {
            let it = &v["item"];
            // A repeated start of the open item is not a new item.
            if state
                .thinking
                .as_ref()
                .is_some_and(|t| t.id == item_str(it, "id"))
            {
                return Ok(out);
            }
            state.end_thinking(&mut out);
            state.thinking = Some(Thinking {
                id: item_str(it, "id").into(),
                encrypted: it["encrypted_content"].as_str().map(str::to_string),
                ..Default::default()
            });
        }
        "response.output_item.done" if v["item"]["type"] == "compaction" => {
            if let Some(c) = compaction_item(&v["item"]) {
                if !state.compactions.contains(&c.encrypted) {
                    state.compactions.push(c.encrypted.clone());
                    out.push(Event::Compaction(c));
                }
            }
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
            let item_id = item_str(it, "id");
            if !item_id.is_empty() && state.pending.iter().any(|p| p.item_id == item_id) {
                return Ok(out);
            }
            let p = Pending {
                output_index: v["output_index"].as_u64(),
                item_id: item_id.into(),
                call_id: item_str(it, "call_id").into(),
                name: call_name(it),
                args: item_str(it, "arguments").into(),
                stream_id: call_id(item_str(it, "call_id"), item_id),
            };
            out.push(Event::ToolInputStart {
                id: p.stream_id.clone(),
                name: p.name.clone(),
            });
            if !p.args.is_empty() {
                out.push(Event::ToolInputDelta {
                    id: p.stream_id.clone(),
                    text: p.args.clone(),
                });
            }
            state.pending.push(p);
        }
        "response.function_call_arguments.delta" => {
            if let Some(p) = state.find(v) {
                let d = v["delta"].as_str().unwrap_or("");
                p.args.push_str(d);
                if !d.is_empty() {
                    out.push(Event::ToolInputDelta {
                        id: p.stream_id.clone(),
                        text: d.to_string(),
                    });
                }
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
            let at = state
                .pending
                .iter()
                .position(|p| p.item_id == item_id && !item_id.is_empty());
            let stream_id = match at {
                Some(i) => state.pending.remove(i).stream_id,
                // A call that arrives only completed still opens its input.
                None => {
                    let id = call_id(item_str(it, "call_id"), item_id);
                    out.push(Event::ToolInputStart {
                        id: id.clone(),
                        name: call_name(it),
                    });
                    id
                }
            };
            out.push(Event::ToolInputEnd { id: stream_id });
            state.call(
                item_str(it, "call_id"),
                item_id,
                &call_name(it),
                item_str(it, "arguments"),
                &mut out,
            );
        }
        // A hosted tool item is a call the provider ran, with its result.
        "response.output_item.done" => {
            if let Some(h) = crate::hosted::responses_item(&v["item"])? {
                state.end_text(&mut out);
                out.push(Event::HostedCall(Hosted {
                    result: None,
                    ..h.clone()
                }));
                out.push(Event::HostedResult(h));
            }
        }
        "response.completed" | "response.incomplete" => {
            // Reasoning never completed by an output item ends with what
            // its start said; the terminal output is not reconciled.
            state.end_thinking(&mut out);
            state.end_text(&mut out);
            let r = &v["response"];
            let complete = kind == "response.completed";
            // Calls never completed by an output item: the final response
            // output wins, then what streamed. An incomplete response's
            // calls are cut off, so they are dropped.
            for p in std::mem::take(&mut state.pending) {
                out.push(Event::ToolInputEnd {
                    id: p.stream_id.clone(),
                });
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
                let name = done
                    .filter(|o| o["name"].is_string())
                    .map(call_name)
                    .unwrap_or_else(|| p.name.clone());
                state.call(&p.stream_id, &p.item_id, &name, args, &mut out);
            }
            // A checkpoint the stream did not deliver as an item.
            for c in r["output"].as_array().into_iter().flatten() {
                if c["type"] != "compaction" {
                    continue;
                }
                if let Some(c) = compaction_item(c) {
                    if !state.compactions.contains(&c.encrypted) {
                        state.compactions.push(c.encrypted.clone());
                        out.push(Event::Compaction(c));
                    }
                }
            }
            let u = &r["usage"];
            let details = &u["input_tokens_details"];
            let cached = details["cached_tokens"].as_u64().unwrap_or(0);
            let written = details["cache_write_tokens"].as_u64().unwrap_or(0);
            let reported = (u["input_tokens"].as_u64(), u["output_tokens"].as_u64());
            let usage = Usage {
                input: reported.0.unwrap_or(0).saturating_sub(cached + written),
                output: reported.1.unwrap_or(0),
                cache_read: cached,
                cache_write: written,
                // Copilot reports its billing beside `response`.
                cost: crate::billing::billed(u, &[v]),
                reasoning: u["output_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                total: u["total_tokens"].as_u64().or(match reported {
                    (None, None) => None,
                    (i, o) => Some(i.unwrap_or(0) + o.unwrap_or(0)),
                }),
            };
            let raw = r["incomplete_details"]["reason"].as_str();
            let normalized = match raw {
                Some("max_output_tokens") => Reason::Length,
                Some("content_filter") => Reason::ContentFilter,
                _ if state.calls > 0 => Reason::ToolCalls,
                None if complete => Reason::Stop,
                _ => Reason::Unknown,
            };
            let mut reason = FinishReason {
                normalized,
                raw: raw.map(str::to_string),
                ..Default::default()
            };
            if reason.is_content_filter() {
                reason.category = filter_category(&r["content_filters"]);
            }
            state.done = true;
            out.push(Event::Finish {
                reason,
                usage,
                raw_usage: u.is_object().then(|| u.clone()),
            });
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
        let v: Value = serde_json::from_str(&f.data)
            .map_err(|e| Error::Decode(format!("{e}: {}", crate::quote(&f.data))))?;
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
    let resp = crate::endpoint::send(p, request(p, client, req)).await?;
    Ok(drive(resp, Decode::default()))
}

/// A `compaction` output item as a checkpoint.
fn compaction_item(item: &Value) -> Option<crate::Compaction> {
    Some(crate::Compaction {
        id: item["id"].as_str().map(String::from),
        encrypted: item["encrypted_content"].as_str()?.to_string(),
    })
}

/// Usage from a Responses `usage` object.
fn usage_of(u: &Value) -> Usage {
    let details = &u["input_tokens_details"];
    let cached = details["cached_tokens"].as_u64().unwrap_or(0);
    let written = details["cache_write_tokens"].as_u64().unwrap_or(0);
    let input = u["input_tokens"].as_u64();
    let output = u["output_tokens"].as_u64();
    Usage {
        input: input.unwrap_or(0).saturating_sub(cached + written),
        output: output.unwrap_or(0),
        cache_read: cached,
        cache_write: written,
        reasoning: u["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0),
        total: u["total_tokens"].as_u64().or_else(|| {
            (input.is_some() || output.is_some()).then(|| input.unwrap_or(0) + output.unwrap_or(0))
        }),
        ..Default::default()
    }
}

/// Trigger compaction (OpenCode's `ResponsesCheckpoint`): the whole
/// conversation, as every step sends it, with a `compaction_trigger` item
/// after it; the response is one checkpoint standing for all of it.
pub(crate) async fn compact_trigger(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<crate::Compacted, Error> {
    let stream = stream_trigger(p, client, req).await?;
    let r = crate::assemble(stream, |_| {}).await.map_err(|f| f.error)?;
    trigger_window(r)
}

/// The trigger request over HTTP, streamed.
pub(crate) async fn stream_trigger(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<EventStream, Error> {
    let mut body = body_for(req, &p.base_url);
    if let Some(input) = body["input"].as_array_mut() {
        input.push(json!({"type": "compaction_trigger"}));
    }
    let rb = p.json_post(client, "responses", crate::openai::headers(p, req), &body);
    let resp = crate::endpoint::send(p, rb).await?;
    Ok(drive(resp, Decode::default()))
}

/// A trigger's response as the window it installs: exactly one checkpoint
/// (anything else is invalid output, never retried or kept).
pub(crate) fn trigger_window(r: crate::Response) -> Result<crate::Compacted, Error> {
    let parts: Vec<Part> = r
        .parts
        .into_iter()
        .filter(|p| matches!(p, Part::Compaction(_)))
        .collect();
    match parts.len() {
        0 => Err(Error::Decode(
            "the compaction response returned no checkpoint".into(),
        )),
        1 => Ok(crate::Compacted {
            window: vec![Message {
                role: Role::Assistant,
                parts,
            }],
            usage: r.usage,
        }),
        n => Err(Error::Decode(format!(
            "the compaction response returned {n} checkpoints; expected one"
        ))),
    }
}

/// Endpoint compaction (OpenCode's `ResponsesCompaction`): `/compact` takes
/// the conversation with the request controls it accepts and returns the
/// whole replacement window, which replays unchanged. The echoed system
/// prompt is left out; the next request leads with its own.
pub(crate) async fn compact_endpoint(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<crate::Compacted, Error> {
    let full = body_for(req, &p.base_url);
    let mut body = serde_json::Map::new();
    for k in [
        "model",
        "input",
        "instructions",
        "previous_response_id",
        "service_tier",
        "reasoning",
        "text",
        "include",
        "parallel_tool_calls",
        "tools",
        "prompt_cache_key",
        "prompt_cache_retention",
        "prompt_cache_options",
    ] {
        if let Some(v) = full.get(k).filter(|v| !v.is_null()) {
            body.insert(k.into(), v.clone());
        }
    }
    let rb = p.json_post(
        client,
        "responses/compact",
        crate::openai::headers(p, req),
        &Value::Object(body),
    );
    let resp = crate::endpoint::send(p, rb).await?;
    let text = resp.text().await.map_err(Error::from)?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| Error::Decode(format!("compaction response: {e}")))?;
    let invalid = || {
        Error::Decode(format!(
            "unexpected compaction response: {}",
            crate::redact_urls(&text.chars().take(500).collect::<String>())
        ))
    };
    let mut window = Vec::new();
    for item in v["output"].as_array().ok_or_else(invalid)? {
        match (item["type"].as_str(), item["role"].as_str()) {
            (Some("compaction"), _) => window.push(Message {
                role: Role::Assistant,
                parts: vec![Part::Compaction(compaction_item(item).ok_or_else(invalid)?)],
            }),
            (Some("message"), Some(role @ ("user" | "assistant"))) => {
                // The provider's items, user ones included, are its state
                // and not the user's words: marked so (see
                // [`Message::is_provider_state`]).
                let parts: Vec<Part> = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .map(|t| {
                        Part::Text(crate::Text {
                            text: t.to_string(),
                            meta: Some(ReasoningMeta::of(Protocol::OpenaiResponses)),
                        })
                    })
                    .collect();
                if parts.is_empty() {
                    return Err(invalid());
                }
                window.push(Message {
                    role: if role == "user" {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    parts,
                });
            }
            // The system prompt sent as a developer message, echoed back;
            // reasoning items replay only with their output.
            _ => {}
        }
    }
    Ok(crate::Compacted {
        window,
        usage: usage_of(&v["usage"]),
    })
}

/// The Responses request for `req`.
pub(crate) fn request(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> reqwest::RequestBuilder {
    p.json_post(
        client,
        "responses",
        crate::openai::headers(p, req),
        &body_for(req, &p.base_url),
    )
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
                {"type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "ok"}]},
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
                "response.output_item.added",
                json!({"item": {"type": "reasoning", "id": "rs_1"}}),
            ),
            (
                "response.reasoning_summary_text.delta",
                json!({"item_id": "rs_1", "delta": "plan"}),
            ),
            (
                "response.output_item.done",
                json!({"item": {"type": "reasoning", "id": "rs_1"}}),
            ),
            (
                "response.output_item.added",
                json!({"item": {"type": "message", "id": "msg_1"}}),
            ),
            (
                "response.output_text.delta",
                json!({"item_id": "msg_1", "delta": "Hi"}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"item_id": "fc_1", "delta": "{\"pa"}),
            ),
            (
                "response.output_item.done",
                json!({"item": {"type": "function_call", "call_id": "c9", "name": "read", "arguments": "{\"path\":\"a\"}"}}),
            ),
            (
                "response.output_item.done",
                json!({"item": {"type": "message", "id": "msg_1"}}),
            ),
            (
                "response.completed",
                json!({"response": {"usage": {"input_tokens": 100, "output_tokens": 9, "input_tokens_details": {"cached_tokens": 40}}}}),
            ),
        ] {
            all.extend(handle(&mut s, k, &v).unwrap());
        }
        assert_eq!(
            crate::content(all),
            vec![
                Event::ReasoningDelta("plan".into()),
                Event::ReasoningEnd(ReasoningMeta {
                    item_id: Some("rs_1".into()),
                    ..ReasoningMeta::of(Protocol::OpenaiResponses)
                }),
                Event::TextDelta("Hi".into()),
                Event::ToolCall {
                    id: "c9".into(),
                    name: "read".into(),
                    input: json!({"path": "a"})
                },
                Event::Finish {
                    reason: Reason::ToolCalls.into(),
                    usage: Usage {
                        input: 60,
                        output: 9,
                        cache_read: 40,
                        cache_write: 0,
                        reasoning: 0,
                        total: Some(109),
                        ..Default::default()
                    },
                    raw_usage: Some(
                        json!({"input_tokens": 100, "output_tokens": 9, "input_tokens_details": {"cached_tokens": 40}})
                    ),
                },
            ]
        );
        assert!(s.done);
    }

    #[test]
    fn content_filter_names_the_blocking_rule() {
        let category = |filters: Value| {
            let mut s = State::default();
            let ev = handle(&mut s, "response.incomplete", &json!({"response": {"incomplete_details": {"reason": "content_filter"}, "content_filters": filters}})).unwrap();
            match ev.last() {
                Some(Event::Finish { reason, .. }) => reason.category.clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            category(json!([
                {"source_type": "prompt", "blocked": false},
                {"source_type": "completion", "blocked": true, "content_filter_raw": [
                    {"action": "ALLOW", "label": "other", "result": true},
                    {"action": "BLOCK", "label": "copyright", "result": true}]}
            ])),
            Some("copyright".into())
        );
        assert_eq!(
            category(
                json!([{"blocked": true, "content_filter_results": {"hate": {"filtered": true}}}])
            ),
            Some("hate".into())
        );
        assert_eq!(category(json!(null)), None);
    }

    #[test]
    fn incomplete_and_failures() {
        let mut s = State::default();
        let ev = handle(&mut s, "response.incomplete", &json!({"response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {}}})).unwrap();
        assert!(matches!(&ev[0], Event::Finish { reason, .. } if reason.is_length()));
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
