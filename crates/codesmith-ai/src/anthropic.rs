//! Anthropic Messages streaming.

use crate::gemini::split_id;
use crate::openai::scrub_id;
use crate::{
    drive, merge_body, normalize_tool_history, post, schema, sse, stream_error, Decoder, Error,
    Event, EventStream, Part, Provider, Request, Role, Usage,
};
use serde_json::{json, Value};

pub(crate) const VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Anthropic accepts tool ids matching `[A-Za-z0-9_-]+` (any length).
fn tool_id(id: &str) -> String {
    scrub_id(split_id(id).0)
}

pub fn body(req: &Request) -> Value {
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
                Part::ToolResult { id, content, is_error, .. } => {
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
    stopped: bool,
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
            state.blocks[i] = Some(Block {
                kind,
                id: b["id"].as_str().unwrap_or("").to_string(),
                name: b["name"].as_str().unwrap_or("").to_string(),
                input: b.get("input").cloned(),
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
                _ => {}
            }
        }
        "content_block_stop" => {
            let i = v["index"].as_u64().unwrap_or(0) as usize;
            if let Some(Some(b)) = state.blocks.get_mut(i) {
                if b.kind == "tool_use" && !b.stopped {
                    b.stopped = true;
                    out.push(tool_call(b));
                }
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
            // Tool blocks whose stop never came settle here.
            for b in state.blocks.iter_mut().flatten() {
                if b.kind == "tool_use" && !b.stopped {
                    b.stopped = true;
                    out.push(tool_call(b));
                }
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
    for (k, v) in &p.headers {
        rb = rb.header(k, v);
    }
    let resp = post(rb, &body(req)).await?;
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
