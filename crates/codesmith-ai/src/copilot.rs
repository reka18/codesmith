//! GitHub Copilot's request conventions (after OpenCode's
//! `GithubCopilotPlugin`): the GitHub token as a bearer token on every
//! route (Messages included), Copilot's integration headers, the
//! `x-initiator` and vision flags read from the request body, the
//! interaction type of the step, and the account's own API endpoint, looked
//! up once per process from `/copilot_internal/user` (the endpoint saved at
//! login serves until then, and when the lookup fails).

use crate::{Error, Interaction, Request};
use serde_json::Value;
use std::time::Duration;

/// The provider id whose requests follow these conventions.
pub const ID: &str = "github-copilot";
pub const API_VERSION: &str = "2026-08-01";
pub const USER_API_VERSION: &str = "2025-04-01";

pub fn user_agent() -> String {
    format!("codesmith/{}", env!("CARGO_PKG_VERSION"))
}

/// The `X-Interaction-Type` of a step (Copilot's client vocabulary).
pub fn interaction_type(i: Interaction) -> &'static str {
    match i {
        Interaction::Agent => "conversation-agent",
        Interaction::Subagent => "conversation-subagent",
        Interaction::Background => "conversation-background",
        Interaction::Compaction => "conversation-compaction",
    }
}

/// The headers a step adds before its body is known: the interaction type
/// and id, and `x-initiator: agent` for any step the user did not start.
pub fn step_headers(req: &Request) -> Vec<(String, String)> {
    let mut h = vec![(
        "X-Interaction-Type".to_string(),
        interaction_type(req.interaction).to_string(),
    )];
    if let Some(id) = &req.prompt_cache_key {
        h.push(("X-Interaction-Id".into(), id.clone()));
    }
    if req.interaction != Interaction::Agent {
        h.push(("x-initiator".into(), "agent".into()));
    }
    h
}

fn has_part(parts: &Value, kind: &str) -> bool {
    parts
        .as_array()
        .is_some_and(|a| a.iter().any(|p| p["type"] == kind))
}

/// Whether the body is agent-initiated (its last message is not the user's
/// own) and whether it carries images (OpenCode's `requestMetadata`).
pub fn request_metadata(url: &str, body: &Value) -> (bool, bool) {
    if let Some(input) = body["input"].as_array() {
        let last = input.last();
        let agent = last.is_none_or(|l| l["role"] != "user");
        // Tool results carry their images in `output`, user messages in `content`.
        let vision = input.iter().any(|i| {
            has_part(&i["content"], "input_image") || has_part(&i["output"], "input_image")
        });
        return (agent, vision);
    }
    let Some(messages) = body["messages"].as_array() else {
        return (false, false);
    };
    let last = messages.last();
    if url.contains("completions") {
        let previous = messages.len().checked_sub(2).and_then(|i| messages.get(i));
        // Chat Completions moves tool-result images into a user message
        // after the tool messages.
        let agent = last.is_none_or(|l| l["role"] != "user")
            || previous.is_some_and(|p| p["role"] == "tool");
        let vision = messages
            .iter()
            .any(|m| has_part(&m["content"], "image_url"));
        return (agent, vision);
    }
    let content = last.and_then(|l| l["content"].as_array());
    let agent = last.is_none_or(|l| l["role"] != "user")
        || !content.is_some_and(|c| c.iter().any(|p| p["type"] != "tool_result"));
    let vision = messages.iter().any(|m| {
        m["content"].as_array().is_some_and(|c| {
            c.iter().any(|p| {
                p["type"] == "image"
                    || (p["type"] == "tool_result" && has_part(&p["content"], "image"))
            })
        })
    });
    (agent, vision)
}

/// The account's Copilot API endpoint from `/copilot_internal/user`.
pub async fn account_endpoint(
    client: &reqwest::Client,
    user_url: &str,
    token: &str,
) -> Result<Option<String>, Error> {
    let resp = client
        .get(user_url)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", user_agent())
        .header("x-github-api-version", USER_API_VERSION)
        .timeout(Duration::from_secs(5))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::Status {
            status: resp.status().as_u16(),
            body: String::new(),
        });
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| Error::Decode(e.without_url().to_string()))?;
    Ok(v["endpoints"]["api"]
        .as_str()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty()))
}

/// Copilot's credentials and headers as an [`crate::Authorizer`].
pub struct Copilot {
    token: String,
    /// The root the request URLs were built under.
    root: String,
    /// Where to look the account's endpoint up; `None` keeps `root` (a
    /// configured base URL).
    user_url: Option<String>,
    endpoint: tokio::sync::OnceCell<Option<String>>,
}

impl std::fmt::Debug for Copilot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Copilot").field("root", &self.root).finish()
    }
}

impl Copilot {
    pub fn new(token: String, root: String, user_url: Option<String>) -> Self {
        Copilot {
            token,
            root: root.trim_end_matches('/').to_string(),
            user_url,
            endpoint: tokio::sync::OnceCell::new(),
        }
    }

    /// The account's API endpoint, when it differs from the root.
    async fn endpoint(&self, client: &reqwest::Client) -> Option<&str> {
        let url = self.user_url.as_deref()?;
        self.endpoint
            .get_or_init(|| async {
                account_endpoint(client, url, &self.token)
                    .await
                    .ok()
                    .flatten()
            })
            .await
            .as_deref()
            .filter(|e| *e != self.root)
    }
}

fn header(v: &str) -> reqwest::header::HeaderValue {
    reqwest::header::HeaderValue::from_str(v)
        .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""))
}

impl crate::Authorizer for Copilot {
    fn authorize<'a>(
        &'a self,
        client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> futures_util::future::BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            if let Some(e) = self.endpoint(client).await {
                let url = req.url().as_str().to_string();
                if let Some(rest) = url.strip_prefix(&self.root) {
                    if let Ok(u) = reqwest::Url::parse(&format!("{e}{rest}")) {
                        *req.url_mut() = u;
                    }
                }
            }
            let body: Value = req
                .body()
                .and_then(|b| b.as_bytes())
                .and_then(|b| serde_json::from_slice(b).ok())
                .unwrap_or(Value::Null);
            let (agent, vision) = request_metadata(req.url().as_str(), &body);
            let h = req.headers_mut();
            h.remove("x-api-key");
            h.insert(
                reqwest::header::AUTHORIZATION,
                header(&format!("Bearer {}", self.token)),
            );
            h.insert(reqwest::header::USER_AGENT, header(&user_agent()));
            h.insert("openai-intent", header("conversation-edits"));
            h.insert("x-github-api-version", header(API_VERSION));
            // A step that declared itself agent-initiated stays so; the body
            // only ever escalates to `agent`.
            if agent {
                h.insert("x-initiator", header("agent"));
            } else if !h.contains_key("x-initiator") {
                h.insert("x-initiator", header("user"));
            }
            if vision {
                h.insert("copilot-vision-request", header("true"));
            }
            Ok(())
        })
    }

    fn describe(&self) -> String {
        "GitHub Copilot login".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_initiator_and_vision_from_each_api_shape() {
        // Responses: a tool image in `output` is vision, a trailing tool
        // output is the agent's.
        let body = json!({"input": [
            {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "function_call_output", "output": [{"type": "input_image", "image_url": "data:"}]},
        ]});
        assert_eq!(request_metadata("https://x/responses", &body), (true, true));
        let body = json!({"input": [{"role": "user", "content": [{"type": "input_text"}]}]});
        assert_eq!(
            request_metadata("https://x/responses", &body),
            (false, false)
        );
        // Chat: tool images moved into a user message after tool messages.
        let body = json!({"messages": [
            {"role": "tool", "content": "r"},
            {"role": "user", "content": [{"type": "image_url"}]},
        ]});
        assert_eq!(
            request_metadata("https://x/chat/completions", &body),
            (true, true)
        );
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(
            request_metadata("https://x/chat/completions", &body),
            (false, false)
        );
        // Messages: a user turn of tool results only is the agent's.
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "content": [{"type": "image"}]},
        ]}]});
        assert_eq!(
            request_metadata("https://x/v1/messages", &body),
            (true, true)
        );
        let body = json!({"messages": [{"role": "user", "content": [{"type": "text"}]}]});
        assert_eq!(
            request_metadata("https://x/v1/messages", &body),
            (false, false)
        );
    }

    #[test]
    fn classifies_steps() {
        let mut r = Request {
            prompt_cache_key: Some("ses_1".into()),
            ..Default::default()
        };
        assert_eq!(
            step_headers(&r),
            [
                (
                    "X-Interaction-Type".to_string(),
                    "conversation-agent".to_string()
                ),
                ("X-Interaction-Id".to_string(), "ses_1".to_string()),
            ]
        );
        r.interaction = Interaction::Subagent;
        assert_eq!(step_headers(&r)[0].1, "conversation-subagent");
        assert_eq!(step_headers(&r)[2], ("x-initiator".into(), "agent".into()));
        r.interaction = Interaction::Background;
        assert_eq!(step_headers(&r)[0].1, "conversation-background");
        r.interaction = Interaction::Compaction;
        assert_eq!(step_headers(&r)[0].1, "conversation-compaction");
    }
}
