//! MCP Events (SEP-3415, the draft `io.modelcontextprotocol/events`
//! extension), as OpenCode follows it: `events/list` names the event types
//! a server offers, and an `events/stream` request opens push delivery. Its
//! `notifications/events/*` name the stream by the JSON-RPC id of that
//! request. Experimental: the extension may change.

use super::Client;
use serde_json::Value;
use std::sync::Weak;
use tokio::sync::mpsc;

pub const EXTENSION: &str = "io.modelcontextprotocol/events";
pub(super) const SUBSCRIPTION_ID: &str = "io.modelcontextprotocol/subscriptionId";
/// JSON-RPC errors that mean an `events/stream` request can never succeed as
/// sent: unknown method, bad arguments, and the extension's NotFound,
/// Forbidden and Unsupported.
pub(super) const REJECTED: [i64; 5] = [-32601, -32602, -32023, -32024, -32026];

/// One event type from `events/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct EventType {
    pub name: String,
    pub description: Option<String>,
    /// Any of "poll", "push" and "webhook".
    pub delivery: Vec<String>,
    pub input_schema: Option<Value>,
    pub payload_schema: Option<Value>,
}

impl EventType {
    pub(super) fn from_json(v: &Value) -> Option<EventType> {
        Some(EventType {
            name: v["name"].as_str()?.to_string(),
            description: v["description"].as_str().map(str::to_string),
            delivery: v["delivery"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d.as_str().map(str::to_string))
                .collect(),
            input_schema: v.get("inputSchema").filter(|s| s.is_object()).cloned(),
            payload_schema: v.get("payloadSchema").filter(|s| s.is_object()).cloned(),
        })
    }
}

/// One message on an event stream. Persist the latest `cursor` to resume.
#[derive(Debug, Clone, PartialEq)]
pub enum EventMessage {
    /// The stream is live. Sent first, and again with `truncated` when the
    /// server skipped events it could not replay.
    Active {
        cursor: Option<String>,
        truncated: bool,
    },
    Event {
        id: String,
        name: String,
        timestamp: String,
        data: Value,
        cursor: Option<String>,
    },
    /// Nothing matched; the server has checked up to `cursor`.
    Heartbeat { cursor: Option<String> },
    /// A recoverable failure, such as one failed upstream fetch; the stream stays open.
    Error { code: i64, message: String },
}

impl EventMessage {
    /// The position this message reports (`None`: the type has no replay).
    pub fn cursor(&self) -> Option<&str> {
        match self {
            EventMessage::Active { cursor, .. }
            | EventMessage::Event { cursor, .. }
            | EventMessage::Heartbeat { cursor } => cursor.as_deref(),
            EventMessage::Error { .. } => None,
        }
    }
}

/// How an event stream ended.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamError {
    /// The server refused the subscription (unknown type, bad arguments, no access).
    Rejected(String),
    /// The server ended the subscription (access revoked, type removed).
    Terminated(String),
    /// The stream broke or closed; it can be reopened from the last cursor.
    Disconnected(String),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Rejected(m) => write!(f, "the MCP server refused the event stream: {m}"),
            StreamError::Terminated(m) => write!(f, "the MCP server ended the event stream: {m}"),
            StreamError::Disconnected(m) => write!(f, "{m}"),
        }
    }
}

/// A `notifications/events/<kind>` notification as a stream message.
pub(super) fn parse(kind: &str, p: &Value) -> Option<Result<EventMessage, StreamError>> {
    let cursor = p["cursor"].as_str().map(str::to_string);
    let error = || {
        format!(
            "{} ({})",
            p["error"]["message"].as_str().unwrap_or("no reason given"),
            p["error"]["code"]
        )
    };
    Some(match kind {
        "active" => Ok(EventMessage::Active {
            cursor,
            truncated: p["truncated"].as_bool() == Some(true),
        }),
        "event" => Ok(EventMessage::Event {
            id: p["eventId"].as_str()?.to_string(),
            name: p["name"].as_str()?.to_string(),
            timestamp: p["timestamp"].as_str().unwrap_or("").to_string(),
            data: p.get("data").cloned().unwrap_or(Value::Null),
            cursor,
        }),
        "heartbeat" => Ok(EventMessage::Heartbeat { cursor }),
        "error" => Ok(EventMessage::Error {
            code: p["error"]["code"].as_i64().unwrap_or(0),
            message: p["error"]["message"].as_str().unwrap_or("").to_string(),
        }),
        "terminated" => Err(StreamError::Terminated(error())),
        _ => return None,
    })
}

/// An open `events/stream`. Dropping it cancels the request.
pub struct EventStream {
    rx: mpsc::UnboundedReceiver<Result<EventMessage, StreamError>>,
    task: tokio::task::JoinHandle<()>,
    client: Weak<Client>,
    id: u64,
}

impl EventStream {
    pub(super) fn new(
        rx: mpsc::UnboundedReceiver<Result<EventMessage, StreamError>>,
        task: tokio::task::JoinHandle<()>,
        client: Weak<Client>,
        id: u64,
    ) -> EventStream {
        EventStream {
            rx,
            task,
            client,
            id,
        }
    }

    /// The next message, or how the stream ended. `None` only after an end.
    pub async fn next(&mut self) -> Option<Result<EventMessage, StreamError>> {
        self.rx.recv().await
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
        let Some(c) = self.client.upgrade() else {
            return;
        };
        c.shared.streams.lock().unwrap().remove(&self.id);
        let id = self.id;
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let params = serde_json::json!({"requestId": id, "reason": "watch stopped"});
                let _ = c.notify("notifications/cancelled", Some(params)).await;
            });
        }
    }
}

/// A small JSON Schema check, failing closed: types, `required`, `enum`,
/// nested objects and array items. Like OpenCode, an object may not carry
/// properties its schema does not list.
pub fn matches(schema: &Value, v: &Value) -> bool {
    let Some(s) = schema.as_object() else {
        return schema.as_bool().unwrap_or(true);
    };
    if let Some(options) = s.get("enum").and_then(Value::as_array) {
        if !options.contains(v) {
            return false;
        }
    }
    if let Some(c) = s.get("const") {
        if c != v {
            return false;
        }
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(list) = s.get(key).and_then(Value::as_array) {
            if !list.iter().any(|sub| matches(sub, v)) {
                return false;
            }
        }
    }
    let type_ok = |t: &str| match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => true,
    };
    match s.get("type") {
        Some(Value::String(t)) if !type_ok(t) => return false,
        Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(type_ok) => {
            return false
        }
        _ => {}
    }
    if let Some(o) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        for r in s
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !o.contains_key(r) {
                return false;
            }
        }
        let extra = s.get("additionalProperties");
        for (k, val) in o {
            match props.and_then(|p| p.get(k)) {
                Some(sub) => {
                    if !matches(sub, val) {
                        return false;
                    }
                }
                None => match extra {
                    Some(Value::Object(_)) => {
                        if !matches(extra.unwrap(), val) {
                            return false;
                        }
                    }
                    Some(Value::Bool(true)) => {}
                    // No listed property and no open schema: excess, rejected.
                    _ if props.is_some() || extra == Some(&Value::Bool(false)) => return false,
                    _ => {}
                },
            }
        }
    }
    if let (Some(items), Some(a)) = (s.get("items"), v.as_array()) {
        if !a.iter().all(|x| matches(items, x)) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_checks() {
        let s = json!({"type": "object", "properties": {"repo": {"type": "string"}, "n": {"type": "integer"}}, "required": ["repo"]});
        assert!(matches(&s, &json!({"repo": "a/b"})));
        assert!(matches(&s, &json!({"repo": "a/b", "n": 3})));
        assert!(!matches(&s, &json!({})), "required");
        assert!(!matches(&s, &json!({"repo": 1})), "type");
        assert!(!matches(&s, &json!({"repo": "x", "extra": 1})), "excess");
        assert!(!matches(&s, &json!({"repo": "x", "n": 1.5})));
        assert!(matches(&json!({"type": "object"}), &json!({"anything": 1})));
        assert!(matches(
            &json!({"type": "object", "properties": {}, "additionalProperties": true}),
            &json!({"x": 1})
        ));
        assert!(matches(&json!({"enum": ["a", "b"]}), &json!("a")));
        assert!(!matches(&json!({"enum": ["a", "b"]}), &json!("c")));
        assert!(matches(
            &json!({"type": "array", "items": {"type": "string"}}),
            &json!(["a"])
        ));
        assert!(!matches(&json!({"type": ["string", "null"]}), &json!(1)));
    }

    #[test]
    fn notifications() {
        let p = json!({"eventId": "e1", "name": "push", "timestamp": "t", "data": {"x": 1}, "cursor": "c1"});
        assert_eq!(
            parse("event", &p),
            Some(Ok(EventMessage::Event {
                id: "e1".into(),
                name: "push".into(),
                timestamp: "t".into(),
                data: json!({"x": 1}),
                cursor: Some("c1".into())
            }))
        );
        assert_eq!(
            parse("active", &json!({"truncated": true})),
            Some(Ok(EventMessage::Active {
                cursor: None,
                truncated: true
            }))
        );
        assert!(matches!(
            parse("terminated", &json!({"error": {"code": -32024, "message": "revoked"}})),
            Some(Err(StreamError::Terminated(m))) if m.contains("revoked")
        ));
        assert_eq!(
            parse("event", &json!({})),
            None,
            "an event needs an id and name"
        );
        assert_eq!(parse("other", &json!({})), None);
    }
}
