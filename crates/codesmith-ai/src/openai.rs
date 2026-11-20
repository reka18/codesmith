//! OpenAI Chat Completions streaming (and OpenAI-compatible servers).

use crate::gemini::split_id;
use crate::{
    drive, merge_body, normalize_tool_history, post, schema, sse, stream_error, Decoder, Error,
    Event, EventStream, Message, Part, Provider, Request, Role, Usage,
};
use serde_json::{json, Value};

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
    for m in &normalize_tool_history(&req.messages) {
        if compat.mistral
            && m.role == Role::User
            && messages.last().is_some_and(|l: &Value| l["role"] == "tool")
            && m.parts.iter().any(|p| matches!(p, Part::Text(_)))
        {
            // Mistral wants an assistant turn between tool results and user text.
            let (results, rest): (Vec<Part>, Vec<Part>) = m
                .parts
                .iter()
                .cloned()
                .partition(|p| matches!(p, Part::ToolResult { .. }));
            messages.extend(convert(
                &Message {
                    role: Role::User,
                    parts: results,
                },
                compat,
            ));
            messages.push(json!({"role": "assistant", "content": "Done."}));
            messages.extend(convert(
                &Message {
                    role: Role::User,
                    parts: rest,
                },
                compat,
            ));
            continue;
        }
        messages.extend(convert(m, compat));
    }
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
    merge_body(&mut body, &req.body);
    body
}

fn convert(m: &Message, compat: &Compat) -> Vec<Value> {
    match m.role {
        Role::User => {
            let mut out = Vec::new();
            let mut text = Vec::new();
            for p in &m.parts {
                match p {
                    Part::Text(t) => text.push(t.as_str()),
                    Part::ToolResult { id, content, .. } => {
                        out.push(json!({"role": "tool", "tool_call_id": compat.call_id(id), "content": content}));
                    }
                    _ => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text.join("\n\n")}));
            }
            out
        }
        Role::Assistant => {
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls = Vec::new();
            for p in &m.parts {
                match p {
                    Part::Text(t) => text.push_str(t),
                    Part::Reasoning(r) => reasoning.push_str(r),
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
            }
            if text.is_empty() && calls.is_empty() && reasoning.is_empty() {
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
            if !reasoning.is_empty() || compat.require_reasoning {
                msg["reasoning_content"] = json!(reasoning);
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
    if let Some(r) = ["reasoning_content", "reasoning", "reasoning_text"]
        .iter()
        .find_map(|k| delta[k].as_str().filter(|s| !s.is_empty()))
    {
        out.push(Event::ReasoningDelta(r.to_string()));
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
