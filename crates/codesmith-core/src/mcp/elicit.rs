//! MCP elicitation (`elicitation/create`), as OpenCode's MCP client answers
//! it: the server's request becomes a [`crate::form`] owned by `global`, the
//! user answers it through the question UI of the session whose tool call
//! caused it, and the form's answer goes back to the server.
//!
//! - A form-mode request with no fields is accepted at once, without a form.
//! - A URL-mode request is one external field: the user opens the URL and
//!   confirms. The server's `notifications/elicitation/complete` for its
//!   `elicitationId` answers it too.
//! - The server giving up (`notifications/cancelled` for the request)
//!   cancels the form; so does dismissing it, or nobody being there to ask.

use crate::form::{self, Choice, Create, Field, State};
use serde_json::{json, Value};

/// The key of a URL elicitation's one field.
pub const URL_FIELD: &str = "elicitation";

/// A form field for one requested property (OpenCode's `toElicitationField`).
pub fn to_field(key: &str, property: &Value, required: bool) -> Field {
    let text = |k: &str| property.get(k).and_then(Value::as_str).map(str::to_string);
    let description = text("description");
    // Some servers give machine titles like "string with format email".
    let machine = |t: &str| {
        let t = t.trim().to_ascii_lowercase();
        ["boolean", "string", "number", "integer", "array", "object"]
            .iter()
            .any(|ty| {
                t == *ty
                    || t.starts_with(&format!("{ty} with"))
                    || t.starts_with(&format!("{ty} in"))
            })
    };
    let title = text("title")
        .filter(|t| !machine(t))
        .or_else(|| description.clone())
        .unwrap_or_else(|| key.to_string());
    let base = form::Base {
        key: key.to_string(),
        description: description.filter(|d| *d != title),
        title: Some(title),
        required,
        when: None,
    };
    let num = |k: &str| property.get(k).and_then(Value::as_f64);
    let count = |k: &str| property.get(k).and_then(Value::as_u64).map(|n| n as usize);
    match property.get("type").and_then(Value::as_str) {
        Some("boolean") => Field::Boolean {
            base,
            default: property.get("default").and_then(Value::as_bool),
        },
        Some(t @ ("number" | "integer")) => {
            let (minimum, maximum, default) = (num("minimum"), num("maximum"), num("default"));
            if t == "number" {
                Field::Number {
                    base,
                    minimum,
                    maximum,
                    default,
                }
            } else {
                Field::Integer {
                    base,
                    minimum,
                    maximum,
                    default,
                }
            }
        }
        Some("array") => {
            let items = &property["items"];
            let options = match items.get("anyOf").and_then(Value::as_array) {
                Some(any) => any
                    .iter()
                    .map(|o| Choice {
                        value: o["const"].as_str().unwrap_or_default().into(),
                        label: o["title"].as_str().unwrap_or_default().into(),
                        description: None,
                    })
                    .collect(),
                None => items["enum"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|v| Choice {
                        value: v.into(),
                        label: v.into(),
                        description: None,
                    })
                    .collect(),
            };
            Field::Multiselect {
                base,
                options,
                min_items: count("minItems"),
                max_items: count("maxItems"),
                custom: false,
                default: property
                    .get("default")
                    .and_then(|d| serde_json::from_value(d.clone()).ok()),
            }
        }
        _ => {
            let options: Option<Vec<Choice>> =
                if let Some(one) = property.get("oneOf").and_then(Value::as_array) {
                    Some(
                        one.iter()
                            .map(|o| Choice {
                                value: o["const"].as_str().unwrap_or_default().into(),
                                label: o["title"].as_str().unwrap_or_default().into(),
                                description: None,
                            })
                            .collect(),
                    )
                } else {
                    property
                        .get("enum")
                        .and_then(Value::as_array)
                        .map(|values| {
                            values
                                .iter()
                                .enumerate()
                                .filter_map(|(i, v)| {
                                    let v = v.as_str()?;
                                    let label = property["enumNames"][i].as_str().unwrap_or(v);
                                    Some(Choice {
                                        value: v.into(),
                                        label: label.into(),
                                        description: None,
                                    })
                                })
                                .collect()
                        })
                };
            Field::String {
                base,
                format: text("format"),
                min_length: count("minLength"),
                max_length: count("maxLength"),
                pattern: None,
                default: text("default"),
                custom: false,
                options,
            }
        }
    }
}

/// The form for an elicitation `params` from `server`; `None` for a form
/// with no fields, which is accepted as it is.
pub fn form_for(server: &str, params: &Value) -> Option<Create> {
    let message = params["message"].as_str().unwrap_or_default();
    let mut metadata = json!({"kind": "mcp-elicitation", "server": server, "message": message});
    let fields = if params["mode"] == "url" {
        if let Some(id) = params["elicitationId"].as_str() {
            metadata["elicitationID"] = json!(id);
        }
        vec![Field::External {
            key: URL_FIELD.into(),
            url: params["url"].as_str().unwrap_or_default().into(),
            title: None,
            description: None,
        }]
    } else {
        let schema = &params["requestedSchema"];
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        schema["properties"]
            .as_object()
            .map(|p| {
                p.iter()
                    .map(|(k, v)| to_field(k, v, required.contains(&k.as_str())))
                    .collect()
            })
            .unwrap_or_default()
    };
    if fields.is_empty() {
        return None;
    }
    Some(Create {
        id: None,
        session_id: form::GLOBAL.into(),
        title: format!("{server} is requesting input"),
        metadata: Some(metadata),
        fields,
    })
}

/// The `elicitation/create` result for a settled form: its answers for a
/// form request, only the action for a URL one.
pub fn result(state: &State, url: bool) -> Value {
    match state {
        State::Answered { answer } if url => {
            let _ = answer;
            json!({"action": "accept"})
        }
        State::Answered { answer } => json!({"action": "accept", "content": answer}),
        _ => json!({"action": "cancel"}),
    }
}
