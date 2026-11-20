//! Incremental continuation over a Responses WebSocket (after OpenCode's
//! `open-responses-continuation.ts`): once a response completes on a
//! connection, the next request on it sends only the input items after that
//! response's input and output, with `previous_response_id`, when nothing
//! else about the request changed.

use super::Recovery;
use crate::Error;
use serde_json::{json, Map, Value};

/// The request and output of a completed response, which the next request
/// on the same connection may continue from.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    pub response_id: String,
    /// The `response.create` fields sent (without `previous_response_id`).
    pub request: Map<String, Value>,
    /// The response's output items.
    pub output: Vec<Value>,
}

/// Whether a `response.create` carries the whole request or continues a
/// checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Full,
    Incremental,
}

/// One `response.create` message.
#[derive(Debug, Clone, PartialEq)]
pub struct Create {
    pub message: String,
    pub mode: Mode,
}

/// What one inbound frame means for the exchange.
#[derive(Debug)]
pub enum Observation {
    /// An ordinary frame.
    Frame(String),
    /// `response.completed`, with the checkpoint the next request may
    /// continue from.
    Completed {
        frame: String,
        checkpoint: Option<Checkpoint>,
    },
    /// `response.incomplete`.
    Incomplete(String),
    /// An `error` or `response.failed` frame.
    ProviderFailure(Error),
    /// The provider refused the continuation; the request should be sent in
    /// full (on a new connection for [`Recovery::RotateAndRetryFull`]).
    Rejected { error: Error, recovery: Recovery },
}

impl Observation {
    pub fn terminal(&self) -> bool {
        !matches!(self, Observation::Frame(_))
    }
}

/// The fields to send next to `previous_response_id` on an incremental
/// step, or `None` to send the step in full. Whether omitted fields carry
/// over from the continued response is provider behaviour the route knows.
pub type Shape = fn(&Map<String, Value>) -> Option<Map<String, Value>>;

/// Classifies a decoded frame before continuation handling (the route's
/// ordering checks in production).
pub type Base = Box<dyn FnMut(&Create, &str, &Value) -> Result<Observation, Error> + Send>;

/// Builds each `response.create` for one request and follows the frames that
/// answer it.
pub struct Driver {
    name: &'static str,
    request: Map<String, Value>,
    base: Base,
    shape: Option<Shape>,
    output: Vec<Value>,
}

impl Driver {
    /// `request` is the `response.create` message as a whole request.
    pub fn new(
        name: &'static str,
        mut request: Map<String, Value>,
        base: Base,
        shape: Option<Shape>,
    ) -> Self {
        request.remove("previous_response_id");
        Driver {
            name,
            request,
            base,
            shape,
            output: Vec::new(),
        }
    }

    /// The message to send, continuing `checkpoint` when the request only
    /// appends to it.
    pub fn create(&mut self, checkpoint: Option<&Checkpoint>) -> Create {
        self.output.clear();
        let full = || Create {
            message: Value::Object(self.request.clone()).to_string(),
            mode: Mode::Full,
        };
        let Some(previous) = checkpoint.filter(|c| !c.response_id.trim().is_empty()) else {
            return full();
        };
        // Ask the route first: diffing the history is wasted when it declines.
        let fields = match self.shape {
            Some(shape) => shape(&self.request),
            None => Some(self.request.clone()),
        };
        let Some(mut fields) = fields else {
            return full();
        };
        let Some(delta) = incremental(&self.request, previous) else {
            return full();
        };
        fields.insert("input".into(), Value::Array(delta));
        fields.insert("previous_response_id".into(), json!(previous.response_id));
        Create {
            message: Value::Object(fields).to_string(),
            mode: Mode::Incremental,
        }
    }

    /// Classify one inbound frame of the exchange started by `create`.
    pub fn observe(&mut self, create: &Create, frame: &str) -> Result<Observation, Error> {
        let event: Value = serde_json::from_str(frame)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| {
                Error::Decode(format!("Invalid {} WebSocket event: {frame}", self.name))
            })?;
        let observation = (self.base)(create, frame, &event)?;
        if event["type"] == "response.output_item.done" && event["item"].is_object() {
            self.output.push(event["item"].clone());
        }
        if let Observation::ProviderFailure(error) = observation {
            let detail = error_detail(&event);
            return Ok(match detail.code.as_deref() {
                Some("previous_response_not_found") => rejected(error, Recovery::RetryFull),
                Some("websocket_connection_limit_reached") => {
                    rejected(error, Recovery::RotateAndRetryFull)
                }
                // Only the continuation tells an incremental send from a full
                // one, so an unclassified invalid request there is retried in
                // full; Codex reports a stale previous_response_id that way,
                // with no code. Classified failures (a context overflow) keep
                // the session's own recovery.
                None if create.mode == Mode::Incremental
                    && detail.kind.as_deref() == Some("invalid_request_error") =>
                {
                    rejected(error, Recovery::RetryFull)
                }
                _ => Observation::ProviderFailure(error),
            });
        }
        let Observation::Completed { frame, .. } = observation else {
            return Ok(observation);
        };
        // A compaction trigger installs a different context window.
        let compacting = self.request["input"]
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i["type"] == "compaction_trigger"));
        let response_id = event["response"]["id"]
            .as_str()
            .filter(|id| !id.trim().is_empty());
        let checkpoint = match (compacting, response_id) {
            (false, Some(id)) => Some(Checkpoint {
                response_id: id.to_string(),
                request: self.request.clone(),
                output: self.checkpoint_output(&event["response"]["output"]),
            }),
            _ => None,
        };
        Ok(Observation::Completed { frame, checkpoint })
    }

    /// The completed response's output, or what `output_item.done` streamed.
    /// Completion can re-encrypt reasoning; the session replays the item
    /// already streamed.
    fn checkpoint_output(&self, output: &Value) -> Vec<Value> {
        match output.as_array().filter(|o| !o.is_empty()) {
            Some(items) => items
                .iter()
                .map(|item| {
                    if item["type"] == "reasoning" && !item["id"].is_null() {
                        if let Some(done) = self
                            .output
                            .iter()
                            .find(|d| d["type"] == item["type"] && d["id"] == item["id"])
                        {
                            return done.clone();
                        }
                    }
                    item.clone()
                })
                .collect(),
            None => self.output.clone(),
        }
    }
}

/// The new input items, when `request` only appends to the checkpoint's
/// input and output and changes nothing else.
fn incremental(request: &Map<String, Value>, checkpoint: &Checkpoint) -> Option<Vec<Value>> {
    let input = request.get("input")?.as_array()?;
    let previous = checkpoint.request.get("input")?.as_array()?;
    if canonical(&invariant(request)) != canonical(&invariant(&checkpoint.request)) {
        return None;
    }
    let baseline: Vec<&Value> = previous.iter().chain(&checkpoint.output).collect();
    if input.len() <= baseline.len() {
        return None;
    }
    let same = baseline
        .iter()
        .zip(input)
        .all(|(a, b)| canonical(&comparable(a)) == canonical(&comparable(b)));
    same.then(|| input[baseline.len()..].to_vec())
}

/// Every field but the input.
fn invariant(request: &Map<String, Value>) -> Value {
    let mut rest = request.clone();
    for k in ["type", "input", "previous_response_id"] {
        rest.remove(k);
    }
    Value::Object(rest)
}

/// JSON with sorted keys, whatever the map's ordering.
fn canonical(v: &Value) -> String {
    match v {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&m[k])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        other => other.to_string(),
    }
}

/// An item as it counts for model input: response-only metadata (ids,
/// statuses, annotations, logprobs) is dropped and tool arguments compare
/// as JSON.
fn comparable(v: &Value) -> Value {
    let Some(item) = v.as_object() else {
        return v.clone();
    };
    let kind = item.get("type").and_then(Value::as_str);
    let field = |k: &str| item.get(k).cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    let mut put = |k: &str, v: Value| {
        if !v.is_null() || item.contains_key(k) {
            out.insert(k.into(), v);
        }
    };
    match kind {
        Some("message") if item.get("role").and_then(Value::as_str) == Some("assistant") => {
            put("role", json!("assistant"));
            let content = match item.get("content") {
                Some(Value::Array(parts)) => Value::Array(
                    parts
                        .iter()
                        .map(|p| match p["type"].as_str() {
                            Some("output_text") => {
                                json!({"type": "output_text", "text": p["text"]})
                            }
                            _ => p.clone(),
                        })
                        .collect(),
                ),
                other => other.cloned().unwrap_or(Value::Null),
            };
            put("content", content);
            if let Some(phase) = item.get("phase") {
                out.insert("phase".into(), phase.clone());
            }
        }
        Some("function_call") => {
            put("type", field("type"));
            put("call_id", field("call_id"));
            put("name", field("name"));
            let args = match item.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!(s)),
                other => other.cloned().unwrap_or(Value::Null),
            };
            put("arguments", args);
        }
        Some("custom_tool_call") => {
            for k in ["type", "call_id", "name", "input"] {
                put(k, field(k));
            }
        }
        Some("reasoning") => {
            for k in ["type", "summary", "encrypted_content"] {
                put(k, field(k));
            }
        }
        _ => return v.clone(),
    }
    Value::Object(out)
}

/// The code, type and message of an error frame (`error`, under `error`,
/// or under `response.error`).
#[derive(Debug, Default)]
pub(crate) struct Detail {
    pub code: Option<String>,
    pub kind: Option<String>,
}

pub(crate) fn error_detail(event: &Value) -> Detail {
    let source = [&event["error"], &event["response"]["error"]]
        .into_iter()
        .find(|e| e.is_object())
        .unwrap_or(event);
    let s = |k: &str| {
        source[k]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Detail {
        code: s("code"),
        kind: s("type").filter(|t| t != "error"),
    }
}

fn rejected(error: Error, recovery: Recovery) -> Observation {
    Observation::Rejected { error, recovery }
}
