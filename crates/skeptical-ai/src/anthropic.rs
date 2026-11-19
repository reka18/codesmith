//! Anthropic Messages streaming.

use crate::openai::Utf8;
use crate::{
    merge_body, post, sse, Error, Event, EventStream, Part, Provider, Request, Role, Usage,
};
use futures_util::StreamExt;
use serde_json::{json, Value};

const VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 32_000;

pub fn body(req: &Request) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for m in &req.messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let content: Vec<Value> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) if !t.is_empty() => Some(json!({"type": "text", "text": t})),
                Part::ToolCall { id, name, input } => Some(json!({"type": "tool_use", "id": id, "name": name, "input": input})),
                Part::ToolResult { id, content, is_error, .. } => {
                    Some(json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}))
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
    let mut body = json!({
        "model": req.model,
        "system": req.system,
        "messages": messages,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
            .collect();
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    merge_body(&mut body, &req.body);
    body
}

#[derive(Debug, Default)]
pub struct State {
    /// Open content blocks by index: (kind, id, name, accumulated json).
    blocks: Vec<(String, String, String, String)>,
    reason: Option<String>,
    usage: Usage,
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
                state.blocks.push(Default::default());
            }
            let b = &v["content_block"];
            state.blocks[i] = (
                b["type"].as_str().unwrap_or("").to_string(),
                b["id"].as_str().unwrap_or("").to_string(),
                b["name"].as_str().unwrap_or("").to_string(),
                String::new(),
            );
            if let Some(t) = b["text"].as_str().filter(|s| !s.is_empty()) {
                out.push(Event::TextDelta(t.to_string()));
            }
        }
        "content_block_delta" => {
            let i = v["index"].as_u64().unwrap_or(0) as usize;
            let d = &v["delta"];
            match d["type"].as_str() {
                Some("text_delta") => out.push(Event::TextDelta(
                    d["text"].as_str().unwrap_or("").to_string(),
                )),
                Some("thinking_delta") => out.push(Event::ReasoningDelta(
                    d["thinking"].as_str().unwrap_or("").to_string(),
                )),
                Some("input_json_delta") => {
                    if let Some(b) = state.blocks.get_mut(i) {
                        b.3.push_str(d["partial_json"].as_str().unwrap_or(""));
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let i = v["index"].as_u64().unwrap_or(0) as usize;
            if let Some((kind, id, name, json)) = state.blocks.get(i).cloned() {
                if kind == "tool_use" {
                    let input = if json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&json)
                            .unwrap_or_else(|_| json!({"__invalid_json": json}))
                    };
                    out.push(Event::ToolCall { id, name, input });
                }
            }
        }
        "message_delta" => {
            if let Some(r) = v["delta"]["stop_reason"].as_str() {
                state.reason = Some(r.to_string());
            }
            if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                state.usage.output = o;
            }
        }
        "message_stop" => {
            out.push(Event::Finish {
                reason: state.reason.take().unwrap_or_else(|| "end_turn".into()),
                usage: state.usage,
            });
        }
        "error" => return Err(Error::Decode(v["error"].to_string())),
        _ => {}
    }
    Ok(out)
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
    let mut bytes = resp.bytes_stream();
    Ok(async_stream::stream! {
        let mut parser = sse::Parser::default();
        let mut state = State::default();
        let mut utf8 = Utf8::default();
        let mut finished = false;
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk { Ok(c) => c, Err(e) => { yield Err(Error::from(e)); return; } };
            for frame in parser.push(&utf8.push(&chunk)) {
                let v: Value = match serde_json::from_str(&frame.data) {
                    Ok(v) => v,
                    Err(e) => { yield Err(Error::Decode(format!("{e}: {}", frame.data))); return; }
                };
                let kind = frame.event.clone().or_else(|| v["type"].as_str().map(str::to_string)).unwrap_or_default();
                match handle(&mut state, &kind, &v) {
                    Ok(evs) => for ev in evs {
                        if matches!(ev, Event::Finish { .. }) { finished = true; }
                        yield Ok(ev);
                    },
                    Err(e) => { yield Err(e); return; }
                }
            }
        }
        if !finished {
            yield Err(Error::Transport("stream ended before message_stop".into()));
        }
    }
    .boxed())
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
