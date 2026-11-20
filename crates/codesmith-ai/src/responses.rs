//! OpenAI Responses API streaming (`POST /responses`), stateless: every
//! request carries the whole conversation and nothing is stored server-side.

use crate::openai::Utf8;
use crate::{
    merge_body, post, sse, Error, Event, EventStream, Part, Provider, Request, Role, Usage,
};
use futures_util::StreamExt;
use serde_json::{json, Value};

pub fn body(req: &Request) -> Value {
    let mut input = Vec::new();
    for m in &req.messages {
        for p in &m.parts {
            match (&m.role, p) {
                (Role::User, Part::Text(t)) => input.push(json!({"role": "user", "content": [{"type": "input_text", "text": t}]})),
                (Role::Assistant, Part::Text(t)) if !t.is_empty() => input.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": t}]})),
                (_, Part::ToolCall { id, name, input: args }) => input.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": args.to_string()})),
                (_, Part::ToolResult { id, content, .. }) => input.push(json!({"type": "function_call_output", "call_id": id, "output": content})),
                _ => {}
            }
        }
    }
    let mut body = json!({"model": req.model, "input": input, "stream": true, "store": false});
    if !req.system.is_empty() {
        body["instructions"] = json!(req.system);
    }
    if !req.tools.is_empty() {
        body["tools"] = req.tools.iter().map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false})).collect();
    }
    if let Some(n) = req.max_tokens {
        body["max_output_tokens"] = json!(n);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    merge_body(&mut body, &req.body);
    body
}

#[derive(Default)]
pub struct State {
    calls: usize,
    pub done: bool,
}

pub fn handle(state: &mut State, kind: &str, v: &Value) -> Result<Vec<Event>, Error> {
    let mut out = Vec::new();
    match kind {
        "response.output_text.delta" => out.push(Event::TextDelta(
            v["delta"].as_str().unwrap_or("").to_string(),
        )),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => out.push(
            Event::ReasoningDelta(v["delta"].as_str().unwrap_or("").to_string()),
        ),
        "response.reasoning_summary_part.done" => out.push(Event::ReasoningDelta("\n\n".into())),
        "response.output_item.done" if v["item"]["type"] == "function_call" => {
            let it = &v["item"];
            let args = it["arguments"].as_str().unwrap_or("");
            let input = if args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(args).unwrap_or_else(|_| json!({"__invalid_json": args}))
            };
            let id = it["call_id"]
                .as_str()
                .or(it["id"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{}", state.calls));
            state.calls += 1;
            out.push(Event::ToolCall {
                id,
                name: it["name"].as_str().unwrap_or("").to_string(),
                input,
            });
        }
        "response.completed" | "response.incomplete" => {
            let r = &v["response"];
            let u = &r["usage"];
            let cached = u["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0);
            let usage = Usage {
                input: u["input_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(cached),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: cached,
                cache_write: 0,
            };
            let reason = if state.calls > 0 {
                "tool_calls".to_string()
            } else {
                match r["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => "length".into(),
                    Some(other) => other.to_string(),
                    None => "stop".into(),
                }
            };
            state.done = true;
            out.push(Event::Finish { reason, usage });
        }
        "response.failed" => {
            let e = &v["response"]["error"];
            return Err(Error::Decode(
                e["message"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string()),
            ));
        }
        "error" => {
            return Err(Error::Decode(
                v["message"]
                    .as_str()
                    .or(v["error"]["message"].as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string()),
            ))
        }
        _ => {}
    }
    Ok(out)
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
    for (k, v) in &p.headers {
        rb = rb.header(k, v);
    }
    let resp = post(rb, &body(req)).await?;
    let mut bytes = resp.bytes_stream();
    Ok(async_stream::stream! {
        let mut parser = sse::Parser::default();
        let mut state = State::default();
        let mut utf8 = Utf8::default();
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk { Ok(c) => c, Err(e) => { yield Err(Error::from(e)); return; } };
            for frame in parser.push(&utf8.push(&chunk)) {
                if frame.data.trim() == "[DONE]" { continue; }
                let v: Value = match serde_json::from_str(&frame.data) {
                    Ok(v) => v,
                    Err(e) => { yield Err(Error::Decode(format!("{e}: {}", frame.data))); return; }
                };
                let kind = v["type"].as_str().map(str::to_string).or(frame.event.clone()).unwrap_or_default();
                match handle(&mut state, &kind, &v) {
                    Ok(evs) => for ev in evs { yield Ok(ev); },
                    Err(e) => { yield Err(e); return; }
                }
                if state.done { return; }
            }
        }
        yield Err(Error::Transport("stream ended before response.completed".into()));
    }
    .boxed())
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
            body: Default::default(),
        };
        let b = body(&req);
        assert_eq!(b["instructions"], "sys");
        assert_eq!(b["store"], false);
        assert_eq!(b["max_output_tokens"], 64);
        assert_eq!(
            b["input"],
            json!([
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
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
