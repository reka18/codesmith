//! OpenAI Chat Completions streaming (and OpenAI-compatible servers).

use crate::{
    merge_body, post, sse, Error, Event, EventStream, Message, Part, Provider, Request, Role, Usage,
};
use futures_util::StreamExt;
use serde_json::{json, Value};

pub fn body(req: &Request) -> Value {
    let mut messages = vec![json!({"role": "system", "content": req.system})];
    for m in &req.messages {
        messages.extend(convert(m));
    }
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}}))
            .collect();
    }
    if let Some(n) = req.max_tokens {
        body["max_tokens"] = json!(n);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    merge_body(&mut body, &req.body);
    body
}

fn convert(m: &Message) -> Vec<Value> {
    match m.role {
        Role::User => {
            let mut out = Vec::new();
            let mut text = Vec::new();
            for p in &m.parts {
                match p {
                    Part::Text(t) => text.push(t.as_str()),
                    Part::ToolResult { id, content, .. } => {
                        out.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
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
            let text: Vec<&str> = m
                .parts
                .iter()
                .filter_map(|p| {
                    if let Part::Text(t) = p {
                        Some(t.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            let calls: Vec<Value> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::ToolCall { id, name, input } => Some(json!({
                        "id": id, "type": "function",
                        "function": {"name": name, "arguments": input.to_string()},
                    })),
                    _ => None,
                })
                .collect();
            let mut msg = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text.join("")) }});
            if !calls.is_empty() {
                msg["tool_calls"] = json!(calls);
            }
            vec![msg]
        }
    }
}

#[derive(Debug, Default)]
pub struct State {
    calls: Vec<(String, String, String)>,
    finish: Option<String>,
    usage: Usage,
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
    for key in ["reasoning_content", "reasoning"] {
        if let Some(r) = delta[key].as_str().filter(|s| !s.is_empty()) {
            out.push(Event::ReasoningDelta(r.to_string()));
        }
    }
    if let Some(t) = delta["content"].as_str().filter(|s| !s.is_empty()) {
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
    if let Some(calls) = delta["tool_calls"].as_array() {
        for c in calls {
            let i = c["index"].as_u64().unwrap_or(state.calls.len() as u64) as usize;
            while state.calls.len() <= i {
                state.calls.push(Default::default());
            }
            let slot = &mut state.calls[i];
            if let Some(id) = c["id"].as_str() {
                slot.0 = id.to_string();
            }
            if let Some(n) = c["function"]["name"].as_str() {
                slot.1.push_str(n);
            }
            if let Some(a) = c["function"]["arguments"].as_str() {
                slot.2.push_str(a);
            }
        }
    }
    if let Some(f) = choice["finish_reason"].as_str() {
        state.finish = Some(f.to_string());
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
    Usage {
        input: n(&u["prompt_tokens"])
            .unwrap_or(0)
            .saturating_sub(cache_read + cache_write),
        output: n(&u["completion_tokens"]).unwrap_or(0),
        cache_read,
        cache_write,
    }
}

/// Events at end of stream: the accumulated tool calls, then `Finish`.
pub fn finish(state: &mut State) -> Vec<Event> {
    let mut out = Vec::new();
    for (n, (id, name, args)) in std::mem::take(&mut state.calls).into_iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let input = if args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&args).unwrap_or_else(|_| json!({"__invalid_json": args}))
        };
        let id = if id.is_empty() {
            format!("call_{n}")
        } else {
            id
        };
        out.push(Event::ToolCall { id, name, input });
    }
    let reason = state.finish.take().unwrap_or_else(|| "stop".into());
    out.push(Event::Finish {
        reason,
        usage: state.usage,
    });
    out
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
    let resp = post(rb, &body(req)).await?;
    let mut bytes = resp.bytes_stream();
    Ok(async_stream::stream! {
        let mut parser = sse::Parser::default();
        let mut state = State::default();
        let mut utf8 = Utf8::default();
        let mut done = false;
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk { Ok(c) => c, Err(e) => { yield Err(Error::from(e)); return; } };
            for frame in parser.push(&utf8.push(&chunk)) {
                if frame.data.trim() == "[DONE]" { done = true; break; }
                match serde_json::from_str::<Value>(&frame.data) {
                    Ok(v) => {
                        if let Some(e) = v.get("error") { yield Err(Error::Decode(e.to_string())); return; }
                        for ev in handle(&mut state, &v) { yield Ok(ev); }
                    }
                    Err(e) => { yield Err(Error::Decode(format!("{e}: {}", frame.data))); return; }
                }
            }
            if done { break; }
        }
        if !done {
            if let Some(frame) = parser.finish().filter(|f| f.data.trim() != "[DONE]") {
                if let Ok(v) = serde_json::from_str::<Value>(&frame.data) {
                    for ev in handle(&mut state, &v) { yield Ok(ev); }
                }
            }
        }
        for ev in finish(&mut state) { yield Ok(ev); }
    }
    .boxed())
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
        assert_eq!(convert(&m)[0]["content"], Value::Null);
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
            finish(&mut s),
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
        let ev = finish(&mut s);
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
            finish(&mut s)[0],
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
        assert_eq!(
            finish(&mut s)[0],
            Event::ToolCall {
                id: "call_0".into(),
                name: "glob".into(),
                input: json!({})
            }
        );
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
