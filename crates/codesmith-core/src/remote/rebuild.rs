//! A running turn's messages, rebuilt from its events in serve's message
//! format (S20's `turn.partial`). Only the session's own events count: a
//! subagent's (those with `child`) belong to its own session.

use codesmith_ai::{Message, Part, Reasoning, Role};
use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct Rebuild {
    messages: Vec<Message>,
}

impl Rebuild {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The assistant message the next part goes in.
    fn assistant(&mut self) -> &mut Message {
        if self
            .messages
            .last()
            .is_none_or(|m| m.role != Role::Assistant)
        {
            self.messages.push(Message {
                role: Role::Assistant,
                parts: vec![],
            });
        }
        self.messages.last_mut().unwrap()
    }

    pub fn apply(&mut self, ev: &Value) {
        if ev.get("child").is_some_and(|c| !c.is_null()) {
            return;
        }
        let s = |k: &str| ev[k].as_str().unwrap_or("").to_string();
        match ev["type"].as_str().unwrap_or("") {
            "user_message" => self.messages.push(Message::user(s("text"))),
            "text" => {
                let m = self.assistant();
                match m.parts.last_mut() {
                    Some(Part::Text(t)) => t.text.push_str(&s("delta")),
                    _ => m.parts.push(Part::text(s("delta"))),
                }
            }
            "reasoning" => {
                let m = self.assistant();
                match m.parts.last_mut() {
                    Some(Part::Reasoning(r)) => r.text.push_str(&s("delta")),
                    _ => m.parts.push(Part::Reasoning(Reasoning {
                        text: s("delta"),
                        meta: None,
                    })),
                }
            }
            "tool_start" => {
                let part = Part::ToolCall {
                    id: s("id"),
                    name: s("name"),
                    input: ev["input"].clone(),
                    meta: None,
                };
                self.assistant().parts.push(part);
            }
            "tool_end" => {
                let part = Part::ToolResult {
                    id: s("id"),
                    name: s("name"),
                    content: s("output"),
                    is_error: ev["error"].as_bool().unwrap_or(false),
                    media: vec![],
                };
                // Results of one step share a user message, as stored.
                match self.messages.last_mut() {
                    Some(m)
                        if m.role == Role::User
                            && m.parts.iter().all(|p| matches!(p, Part::ToolResult { .. })) =>
                    {
                        m.parts.push(part)
                    }
                    _ => self.messages.push(Message {
                        role: Role::User,
                        parts: vec![part],
                    }),
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_turn_in_serves_message_format() {
        let mut r = Rebuild::default();
        for ev in [
            json!({"type": "turn_start"}),
            json!({"type": "user_message", "text": "list files"}),
            json!({"type": "text", "delta": "Look"}),
            json!({"type": "text", "delta": "ing."}),
            json!({"type": "tool_start", "id": "c1", "name": "shell", "input": {"command": "ls"}}),
            json!({"type": "tool_start", "id": "c9", "name": "read", "input": {}, "child": "ses_sub"}),
            json!({"type": "tool_end", "id": "c1", "name": "shell", "output": "a\n", "error": false}),
            json!({"type": "text", "delta": "Done."}),
        ] {
            r.apply(&ev);
        }
        let v = serde_json::to_value(r.messages()).unwrap();
        assert_eq!(v[0]["role"], "user");
        assert_eq!(
            v[1]["parts"][0],
            json!({"type": "text", "data": "Looking."})
        );
        assert_eq!(v[1]["parts"][1]["data"]["id"], "c1");
        assert_eq!(
            v[1]["parts"].as_array().unwrap().len(),
            2,
            "not the subagent's"
        );
        assert_eq!(v[2]["parts"][0]["data"]["content"], "a\n");
        assert_eq!(v[3]["parts"][0]["data"], "Done.");
    }
}
