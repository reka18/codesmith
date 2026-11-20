//! Google Gemini `streamGenerateContent` (Generative Language API).
//!
//! Gemini returns a thought signature with each function call and wants it
//! back when the call is replayed. It rides along in the tool-call id
//! (`<id>~<signature>`), so stored conversations need no extra fields.

use crate::{
    drive, merge_body, normalize_tool_history, post, schema, sse, stream_error, Decoder, Error,
    Event, EventStream, Message, Part, Provider, Request, Role, Usage,
};
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

fn parts(m: &Message, omit_ids: bool) -> Vec<Value> {
    let mut out = Vec::new();
    // A parallel batch may carry one signature, on its first call; unsigned
    // siblings after it are valid as they are.
    let mut signed = false;
    for p in &m.parts {
        match p {
            Part::Text(t) if !t.is_empty() => out.push(json!({"text": t})),
            Part::ToolCall { id, name, input } => {
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
            } => {
                let key = if *is_error { "error" } else { "output" };
                let mut r = json!({"name": name, "response": {key: content}});
                if !omit_ids {
                    r["id"] = json!(split_id(id).0);
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
    let mut contents: Vec<Value> = Vec::new();
    for m in &normalize_tool_history(&req.messages) {
        let role = if m.role == Role::User {
            "user"
        } else {
            "model"
        };
        // Function responses get a user turn of their own: Gemini rejects a
        // function-response turn that carries other parts.
        let (responses, rest): (Vec<Value>, Vec<Value>) = parts(m, omit_ids)
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
    let mut body = json!({"contents": contents});
    if !req.system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": req.system}]});
    }
    if !req.tools.is_empty() {
        let decls: Vec<Value> = req.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parametersJsonSchema": schema::project(&t.parameters, Some(schema::Sanitizer::Gemini))})).collect();
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
    ids: Vec<String>,
    finish: Option<String>,
}

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
        let prompt = u["promptTokenCount"].as_u64().unwrap_or(0)
            + u["toolUsePromptTokenCount"].as_u64().unwrap_or(0);
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
            if p["thought"].as_bool() == Some(true) {
                out.push(Event::ReasoningDelta(t.to_string()));
            } else {
                out.push(Event::TextDelta(t.to_string()));
            }
        }
    }
    if let Some(f) = c["finishReason"].as_str() {
        if FAILED.contains(&f) {
            return Err(Error::Provider {
                message: format!("Gemini stopped with {f}"),
                retryable: FAILED[..3].contains(&f),
            });
        }
        state.finish = Some(match f {
            "STOP" => "stop".into(),
            "MAX_TOKENS" => "length".into(),
            other => other.to_lowercase(),
        });
    }
    Ok(out)
}

pub fn finish(state: &mut State) -> Event {
    let reason = if !state.ids.is_empty() {
        "tool_calls".to_string()
    } else {
        state.finish.take().unwrap_or_else(|| "stop".into())
    };
    Event::Finish {
        reason,
        usage: state.usage,
    }
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
        Ok(vec![finish(&mut self.state)])
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
    Ok(drive(resp, Decode::default()))
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
        let a = handle(&mut s, &json!({"candidates": [{"content": {"parts": [{"text": "think", "thought": true}, {"text": "Hel"}]}}]})).unwrap();
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
            [Event::ToolCall { id, name, input }]
                if id.starts_with("tool_") && id.ends_with("~S1") && name == "read" && *input == json!({"path": "a"})
        ));
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
