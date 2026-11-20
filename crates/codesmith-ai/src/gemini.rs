//! Google Gemini `streamGenerateContent` (Generative Language API).
//!
//! Gemini returns a thought signature with each function call and wants it
//! back when the call is replayed. It rides along in the tool-call id
//! (`<id>~<signature>`), so stored conversations need no extra fields.

use crate::openai::Utf8;
use crate::{
    merge_body, post, sse, Error, Event, EventStream, Message, Part, Provider, Request, Role, Usage,
};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};

const SIG: char = '~';

/// Split a tool-call id into its id and any thought signature.
pub fn split_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once(SIG) {
        Some((a, b)) if !b.is_empty() => (a, Some(b)),
        _ => (id, None),
    }
}

/// JSON Schema trimmed to the OpenAPI subset Gemini accepts.
pub fn schema(v: &Value) -> Value {
    const KEEP: [&str; 20] = [
        "type",
        "format",
        "title",
        "description",
        "nullable",
        "enum",
        "maxItems",
        "minItems",
        "properties",
        "required",
        "minProperties",
        "maxProperties",
        "minLength",
        "maxLength",
        "pattern",
        "anyOf",
        "items",
        "minimum",
        "maximum",
        "default",
    ];
    let Value::Object(m) = v else {
        return v.clone();
    };
    let mut out = Map::new();
    for (k, val) in m {
        if !KEEP.contains(&k.as_str()) {
            continue;
        }
        let val = match k.as_str() {
            "properties" => Value::Object(
                val.as_object()
                    .map(|p| p.iter().map(|(n, s)| (n.clone(), schema(s))).collect())
                    .unwrap_or_default(),
            ),
            "items" => schema(val),
            "anyOf" => Value::Array(
                val.as_array()
                    .map(|a| a.iter().map(schema).collect())
                    .unwrap_or_default(),
            ),
            "enum" => Value::Array(
                val.as_array()
                    .map(|a| {
                        a.iter()
                            .map(|e| {
                                e.as_str()
                                    .map(|s| json!(s))
                                    .unwrap_or_else(|| json!(e.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            "type" if val.is_array() => {
                // ["string", "null"] → "string" + nullable
                let types: Vec<&str> = val
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                if types.contains(&"null") {
                    out.insert("nullable".into(), json!(true));
                }
                json!(types.into_iter().find(|t| *t != "null").unwrap_or("string"))
            }
            _ => val.clone(),
        };
        out.insert(k.clone(), val);
    }
    if out.get("type").and_then(Value::as_str) == Some("object")
        && out
            .get("properties")
            .map(|p| p.as_object().is_some_and(|o| o.is_empty()))
            .unwrap_or(true)
    {
        out.remove("properties");
        out.remove("required");
    }
    Value::Object(out)
}

fn parts(m: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    for p in &m.parts {
        match p {
            Part::Text(t) if !t.is_empty() => out.push(json!({"text": t})),
            Part::ToolCall { id, name, input } => {
                let (_, sig) = split_id(id);
                let mut v = json!({"functionCall": {"name": name, "args": if input.is_object() { input.clone() } else { json!({}) }}});
                if let Some(s) = sig {
                    v["thoughtSignature"] = json!(s);
                }
                out.push(v);
            }
            Part::ToolResult {
                name,
                content,
                is_error,
                ..
            } => {
                let key = if *is_error { "error" } else { "output" };
                out.push(json!({"functionResponse": {"name": name, "response": {key: content}}}));
            }
            _ => {}
        }
    }
    out
}

pub fn body(req: &Request) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    for m in &req.messages {
        let role = if m.role == Role::User {
            "user"
        } else {
            "model"
        };
        let ps = parts(m);
        if ps.is_empty() {
            continue;
        }
        match contents.last_mut() {
            Some(last) if last["role"] == role => last["parts"].as_array_mut().unwrap().extend(ps),
            _ => contents.push(json!({"role": role, "parts": ps})),
        }
    }
    let mut body = json!({"contents": contents});
    if !req.system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": req.system}]});
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": schema(&t.parameters)})).collect();
        body["tools"] = json!([{"functionDeclarations": decls}]);
    }
    let mut gen = Map::new();
    if let Some(n) = req.max_tokens {
        gen.insert("maxOutputTokens".into(), json!(n));
    }
    if let Some(t) = req.temperature {
        gen.insert("temperature".into(), json!(t));
    }
    if !gen.is_empty() {
        body["generationConfig"] = Value::Object(gen);
    }
    merge_body(&mut body, &req.body);
    body
}

#[derive(Default)]
pub struct State {
    usage: Usage,
    calls: usize,
    finish: Option<String>,
}

pub fn handle(state: &mut State, v: &Value) -> Result<Vec<Event>, Error> {
    if let Some(e) = v.get("error") {
        return Err(Error::Decode(
            e["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| e.to_string()),
        ));
    }
    let mut out = Vec::new();
    if let Some(u) = v.get("usageMetadata") {
        let prompt = u["promptTokenCount"].as_u64().unwrap_or(0);
        let cached = u["cachedContentTokenCount"].as_u64().unwrap_or(0);
        state.usage.input = prompt.saturating_sub(cached);
        state.usage.cache_read = cached;
        state.usage.output = u["candidatesTokenCount"].as_u64().unwrap_or(0)
            + u["thoughtsTokenCount"].as_u64().unwrap_or(0);
    }
    let Some(c) = v["candidates"].get(0) else {
        if let Some(r) = v["promptFeedback"]["blockReason"].as_str() {
            return Err(Error::Decode(format!("prompt blocked: {r}")));
        }
        return Ok(out);
    };
    for p in c["content"]["parts"].as_array().into_iter().flatten() {
        if let Some(f) = p.get("functionCall") {
            let base = f["id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{}", state.calls));
            state.calls += 1;
            let id = match p["thoughtSignature"].as_str() {
                Some(s) => format!("{base}{SIG}{s}"),
                None => base,
            };
            out.push(Event::ToolCall {
                id,
                name: f["name"].as_str().unwrap_or("").to_string(),
                input: f.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        } else if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
            if p["thought"].as_bool() == Some(true) {
                out.push(Event::ReasoningDelta(t.to_string()));
            } else {
                out.push(Event::TextDelta(t.to_string()));
            }
        }
    }
    if let Some(f) = c["finishReason"].as_str() {
        state.finish = Some(match f {
            "STOP" => "stop".into(),
            "MAX_TOKENS" => "length".into(),
            other => other.to_lowercase(),
        });
    }
    Ok(out)
}

pub fn finish(state: &mut State) -> Event {
    let reason = if state.calls > 0 {
        "tool_calls".to_string()
    } else {
        state.finish.take().unwrap_or_else(|| "stop".into())
    };
    Event::Finish {
        reason,
        usage: state.usage,
    }
}

pub async fn stream(
    p: &Provider,
    client: &reqwest::Client,
    req: &Request,
) -> Result<EventStream, Error> {
    let url = format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        p.base_url.trim_end_matches('/'),
        req.model
    );
    let mut rb = client.post(url);
    if let Some(k) = &p.api_key {
        rb = rb.header("x-goog-api-key", k);
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
        let mut frames = Vec::new();
        while let Some(chunk) = bytes.next().await {
            let chunk = match chunk { Ok(c) => c, Err(e) => { yield Err(Error::from(e)); return; } };
            frames.extend(parser.push(&utf8.push(&chunk)));
            for frame in frames.drain(..) {
                let v: Value = match serde_json::from_str(&frame.data) {
                    Ok(v) => v,
                    Err(e) => { yield Err(Error::Decode(format!("{e}: {}", frame.data))); return; }
                };
                match handle(&mut state, &v) {
                    Ok(evs) => for ev in evs { yield Ok(ev); },
                    Err(e) => { yield Err(e); return; }
                }
            }
        }
        if let Some(frame) = parser.finish() {
            if let Ok(v) = serde_json::from_str::<Value>(&frame.data) {
                match handle(&mut state, &v) {
                    Ok(evs) => for ev in evs { yield Ok(ev); },
                    Err(e) => { yield Err(e); return; }
                }
            }
        }
        yield Ok(finish(&mut state));
    }
    .boxed())
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
            body: Map::new(),
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
                }],
            },
            Message::user("<shell>done</shell>"),
        ];
        let b = body(&req(msgs));
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "sys");
        let c = b["contents"].as_array().unwrap();
        assert_eq!(
            c.len(),
            3,
            "the tool result and the next user text merge into one user turn"
        );
        assert_eq!(c[1]["role"], "model");
        assert_eq!(
            c[1]["parts"],
            json!([{"text": "ok"}, {"functionCall": {"name": "read", "args": {"path": "a"}}, "thoughtSignature": "SIGX"}])
        );
        assert_eq!(
            c[2]["parts"][0],
            json!({"functionResponse": {"name": "read", "response": {"output": "data"}}})
        );
        assert_eq!(c[2]["parts"][1]["text"], "<shell>done</shell>");
        let p = &b["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(
            p,
            &json!({"type": "object", "properties": {"path": {"type": "string", "nullable": true, "description": "p"}, "n": {"type": "integer", "enum": ["1", "2"]}}, "required": ["path"]})
        );
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 100);
    }

    #[test]
    fn empty_object_schemas_drop_properties() {
        assert_eq!(
            schema(&json!({"type": "object", "properties": {}})),
            json!({"type": "object"})
        );
    }

    #[test]
    fn stream_events() {
        let mut s = State::default();
        let a = handle(&mut s, &json!({"candidates": [{"content": {"parts": [{"text": "think", "thought": true}, {"text": "Hel"}]}}]})).unwrap();
        assert_eq!(
            a,
            vec![
                Event::ReasoningDelta("think".into()),
                Event::TextDelta("Hel".into())
            ]
        );
        let b = handle(&mut s, &json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "read", "args": {"path": "a"}}, "thoughtSignature": "S1"}]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 50, "cachedContentTokenCount": 20, "candidatesTokenCount": 7, "thoughtsTokenCount": 3}})).unwrap();
        assert_eq!(
            b,
            vec![Event::ToolCall {
                id: "call_0~S1".into(),
                name: "read".into(),
                input: json!({"path": "a"})
            }]
        );
        assert_eq!(
            finish(&mut s),
            Event::Finish {
                reason: "tool_calls".into(),
                usage: Usage {
                    input: 30,
                    output: 10,
                    cache_read: 20,
                    cache_write: 0
                }
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
        assert!(handle(
            &mut s,
            &json!({"promptFeedback": {"blockReason": "SAFETY"}})
        )
        .unwrap_err()
        .to_string()
        .contains("SAFETY"));
        handle(&mut s, &json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "MAX_TOKENS"}]})).unwrap();
        assert!(matches!(finish(&mut s), Event::Finish { reason, .. } if reason == "length"));
    }
}
