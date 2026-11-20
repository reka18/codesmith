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
//!
//! Not in OpenCode: a server may have [`MAX_PENDING`] elicitations open at
//! once and start [`MAX_PER_MINUTE`] a minute ([`Budget`]); more are
//! declined without asking. A URL elicitation's URL must be `https` (it is
//! shown for the user to open, never opened); another is declined.

use crate::form::{self, Choice, Create, Field, State};
use serde_json::{json, Value};

/// The key of a URL elicitation's one field.
pub const URL_FIELD: &str = "elicitation";

/// The most elicitations one server may have waiting on the user.
pub const MAX_PENDING: usize = 4;
/// The most elicitations one server may start in a minute.
pub const MAX_PER_MINUTE: usize = 10;

/// One server's elicitation allowance: how many wait on the user now, and
/// when the ones of the last minute started.
#[derive(Default)]
pub struct Budget {
    inner: std::sync::Mutex<(usize, std::collections::VecDeque<std::time::Instant>)>,
}

/// An elicitation counted against a [`Budget`] until dropped.
pub struct Permit(std::sync::Arc<Budget>);

impl Budget {
    /// Count one more elicitation started at `now`, or `None` when the
    /// server already has [`MAX_PENDING`] open or started
    /// [`MAX_PER_MINUTE`] in the minute before.
    pub fn start(self: &std::sync::Arc<Self>, now: std::time::Instant) -> Option<Permit> {
        let mut g = self.inner.lock().unwrap();
        let (pending, recent) = &mut *g;
        while recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= std::time::Duration::from_secs(60))
        {
            recent.pop_front();
        }
        if *pending >= MAX_PENDING || recent.len() >= MAX_PER_MINUTE {
            return None;
        }
        *pending += 1;
        recent.push_back(now);
        Some(Permit(self.clone()))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut g = self.0.inner.lock().unwrap();
        g.0 = g.0.saturating_sub(1);
    }
}

/// A URL elicitation's URL as it is shown, when it is one to show: an
/// absolute `https` URL (normalized, so it carries no raw control
/// characters).
pub fn shown_url(params: &Value) -> Option<String> {
    let u = url::Url::parse(params["url"].as_str()?).ok()?;
    (u.scheme() == "https" && u.host().is_some()).then(|| u.to_string())
}

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
            url: shown_url(params).unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn a_server_gets_a_few_elicitations_at_once_and_a_few_a_minute() {
        let b = Arc::new(Budget::default());
        let t = Instant::now();
        let open: Vec<Permit> = (0..MAX_PENDING).map(|_| b.start(t).unwrap()).collect();
        assert!(b.start(t).is_none(), "over the pending cap");
        drop(open);
        // Settled ones free their places, but the minute's count stays.
        let mut started = MAX_PENDING;
        while started < MAX_PER_MINUTE {
            drop(b.start(t + Duration::from_secs(1)).unwrap());
            started += 1;
        }
        assert!(
            b.start(t + Duration::from_secs(59)).is_none(),
            "over the rate"
        );
        assert!(b.start(t + Duration::from_secs(60)).is_some());
    }

    #[test]
    fn only_https_urls_are_shown() {
        let url = |u: &str| shown_url(&json!({"url": u}));
        assert_eq!(
            url("https://example.com/a?b=1").as_deref(),
            Some("https://example.com/a?b=1")
        );
        for bad in [
            "http://example.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ssh://host",
            "/relative",
            "",
        ] {
            assert_eq!(url(bad), None, "{bad}");
        }
        assert_eq!(shown_url(&json!({})), None);
        // Control characters are escaped, not shown raw.
        let u = url("https://example.com/\u{1b}[2J").unwrap();
        assert!(!u.contains('\u{1b}'), "{u}");
    }
}
