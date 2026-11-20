//! Model provider protocols. A `Provider` turns a provider-neutral `Request`
//! into a stream of provider-neutral `Event`s.

mod anthropic;
mod gemini;
mod openai;
mod responses;
mod sse;

use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Part {
    Text(String),
    /// Model reasoning, kept so it can be replayed to providers that want it.
    Reasoning(String),
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        name: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            parts: vec![Part::Text(text.into())],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input object.
    pub parameters: Value,
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Extra top-level body fields, merged last (e.g. `chat_template_kwargs`).
    pub body: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    Finish {
        reason: String,
        usage: Usage,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// OpenAI Chat Completions, also used by most OpenAI-compatible servers.
    OpenaiChat,
    /// Anthropic Messages.
    AnthropicMessages,
    /// OpenAI Responses (stateless; nothing stored server-side).
    OpenaiResponses,
    /// Google Gemini generateContent.
    GoogleGemini,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("request failed: {0}")]
    Transport(String),
    #[error("bad response: {0}")]
    Decode(String),
}

impl Error {
    /// Whether retrying the same request may succeed.
    pub fn retryable(&self) -> bool {
        match self {
            Error::Status { status, .. } => {
                *status == 408 || *status == 409 || *status == 429 || *status >= 500
            }
            Error::Transport(_) => true,
            Error::Decode(_) => false,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Transport(e.to_string())
    }
}

pub type EventStream = BoxStream<'static, Result<Event, Error>>;

#[derive(Debug, Clone)]
pub struct Provider {
    pub protocol: Protocol,
    /// Base URL including the version path, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl Provider {
    pub async fn stream(
        &self,
        client: &reqwest::Client,
        req: &Request,
    ) -> Result<EventStream, Error> {
        match self.protocol {
            Protocol::OpenaiChat => openai::stream(self, client, req).await,
            Protocol::AnthropicMessages => anthropic::stream(self, client, req).await,
            Protocol::OpenaiResponses => responses::stream(self, client, req).await,
            Protocol::GoogleGemini => gemini::stream(self, client, req).await,
        }
    }
}

impl Provider {
    /// Model ids the provider reports, sorted.
    pub async fn list_models(&self, client: &reqwest::Client) -> Result<Vec<String>, Error> {
        let base = self.base_url.trim_end_matches('/');
        let mut rb = match self.protocol {
            Protocol::GoogleGemini => client.get(format!("{base}/models?pageSize=1000")),
            _ => client.get(format!("{base}/models")),
        };
        if let Some(k) = &self.api_key {
            rb = match self.protocol {
                Protocol::AnthropicMessages => rb
                    .header("x-api-key", k)
                    .header("anthropic-version", anthropic::VERSION),
                Protocol::GoogleGemini => rb.header("x-goog-api-key", k),
                _ => rb.bearer_auth(k),
            };
        }
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(Error::Status {
                status: status.as_u16(),
                body: text.chars().take(2000).collect(),
            });
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| Error::Decode(e.to_string()))?;
        Ok(model_ids(self.protocol, &v))
    }
}

pub(crate) fn model_ids(protocol: Protocol, v: &Value) -> Vec<String> {
    let mut ids: Vec<String> = match protocol {
        Protocol::GoogleGemini => v["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| {
                m["supportedGenerationMethods"]
                    .as_array()
                    .map(|a| a.iter().any(|x| x == "generateContent"))
                    .unwrap_or(true)
            })
            .filter_map(|m| {
                m["name"]
                    .as_str()
                    .map(|n| n.trim_start_matches("models/").to_string())
            })
            .collect(),
        _ => v["data"]
            .as_array()
            .or(v["models"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().or(m["name"].as_str()).map(str::to_string))
            .collect(),
    };
    ids.sort();
    ids.dedup();
    ids
}

/// Send a JSON POST and fail with the status and a bounded body on non-2xx.
pub(crate) async fn post(
    rb: reqwest::RequestBuilder,
    body: &Value,
) -> Result<reqwest::Response, Error> {
    let resp = rb.json(body).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Status {
            status: status.as_u16(),
            body: text.chars().take(2000).collect(),
        });
    }
    Ok(resp)
}

pub(crate) fn merge_body(body: &mut Value, extra: &serde_json::Map<String, Value>) {
    if let Value::Object(map) = body {
        for (k, v) in extra {
            map.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses() {
        for s in [408, 409, 429, 500, 502, 503, 529] {
            assert!(
                Error::Status {
                    status: s,
                    body: String::new()
                }
                .retryable(),
                "{s}"
            );
        }
        for s in [400, 401, 403, 404, 422] {
            assert!(
                !Error::Status {
                    status: s,
                    body: String::new()
                }
                .retryable(),
                "{s}"
            );
        }
        assert!(Error::Transport("reset".into()).retryable());
        assert!(!Error::Decode("bad".into()).retryable());
    }

    #[test]
    fn protocol_names() {
        assert_eq!(
            serde_json::to_value(Protocol::OpenaiChat).unwrap(),
            "openai-chat"
        );
        assert_eq!(
            serde_json::from_value::<Protocol>("anthropic-messages".into()).unwrap(),
            Protocol::AnthropicMessages
        );
        for (name, p) in [
            ("openai-responses", Protocol::OpenaiResponses),
            ("google-gemini", Protocol::GoogleGemini),
        ] {
            assert_eq!(serde_json::from_value::<Protocol>(name.into()).unwrap(), p);
        }
    }

    #[test]
    fn model_listings() {
        use serde_json::json;
        assert_eq!(
            model_ids(
                Protocol::OpenaiChat,
                &json!({"data": [{"id": "b"}, {"id": "a"}]})
            ),
            ["a", "b"]
        );
        assert_eq!(
            model_ids(
                Protocol::GoogleGemini,
                &json!({"models": [
                    {"name": "models/gemini-x", "supportedGenerationMethods": ["generateContent"]},
                    {"name": "models/embed", "supportedGenerationMethods": ["embedContent"]}
                ]})
            ),
            ["gemini-x"]
        );
    }

    #[test]
    fn extra_body_overrides() {
        let mut body = serde_json::json!({"model": "m", "stream": true});
        let mut extra = serde_json::Map::new();
        extra.insert(
            "chat_template_kwargs".into(),
            serde_json::json!({"enable_thinking": false}),
        );
        extra.insert("model".into(), "override".into());
        merge_body(&mut body, &extra);
        assert_eq!(body["model"], "override");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    }
}

#[cfg(test)]
mod serde_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip_as_json() {
        let m = Message {
            role: Role::Assistant,
            parts: vec![
                Part::Reasoning("r".into()),
                Part::Text("t".into()),
                Part::ToolCall {
                    id: "c".into(),
                    name: "read".into(),
                    input: json!({"path": "a"}),
                },
                Part::ToolResult {
                    id: "c".into(),
                    name: "read".into(),
                    content: "x".into(),
                    is_error: false,
                },
            ],
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["parts"][1], json!({"type": "text", "data": "t"}));
        assert_eq!(v["parts"][2]["data"]["name"], "read");
        assert_eq!(serde_json::from_value::<Message>(v).unwrap(), m);
    }
}
