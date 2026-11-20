//! The agent loop's rules that do not need a live session: the text the
//! model sees when a step is cut short, and how stored history becomes model
//! context. Ported from OpenCode's session runner
//! (`packages/core/src/session/runner/{llm,step,max-steps,publish-llm-event,to-llm-message}.ts`);
//! `session.rs` applies them and `tests/runner_parity.rs` pins the behaviour.
//!
//! Deliberate differences from OpenCode:
//! - The final-step instruction (`MAX_STEPS_PROMPT`) is sent as a user
//!   message, not an assistant one: several providers reject or misread a
//!   trailing assistant message (prefill), and CodeSmith speaks to many.
//! - Agents without a `steps` setting still stop at `DEFAULT_MAX_STEPS`
//!   (OpenCode has no limit); the final step is forced to text either way.
//! - Tools run after the model's stream ends rather than while it streams,
//!   so a failed stream never leaves a half-run tool behind; retrying it
//!   starts from the prompt.

use codesmith_ai::{Message, Part, Protocol, Role};
use serde_json::{json, Value};

pub const MAX_STEPS_PROMPT: &str = "CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY.";

/// The result of a tool called on the final step anyway.
pub const TOOLS_DISABLED: &str = "Tools are disabled after the maximum agent steps";

/// Sent after a response cut off by the output token limit.
pub const CONTINUE_AFTER_OUTPUT_LIMIT: &str = "Your last response hit the output token limit (stop reason: length). Do not apologize, recap, or repeat yourself. Break the remaining work into smaller pieces.";

/// Whether a user-role message is one the engine wrote itself (an
/// output-limit nudge, the final-step instruction), not something the user
/// typed; views of a conversation leave these out.
pub fn is_engine_message(text: &str) -> bool {
    let t = text.trim();
    t == CONTINUE_AFTER_OUTPUT_LIMIT || t == MAX_STEPS_PROMPT.trim()
}

/// The turn's error after three truncated responses in a row.
pub const OUTPUT_LIMIT_EXHAUSTED: &str =
    "Response still truncated after two output token limit continuations";

/// Continuations allowed after a truncated response, per user prompt.
pub const OUTPUT_LIMIT_CONTINUATIONS: u32 = 2;

/// The turn's error when the provider's filter stopped the response.
pub const CONTENT_FILTERED: &str = "Provider blocked the response";

/// The result of a tool call the user declined.
pub const DECLINED: &str = "The user declined this tool call";

/// The result of a question the user dismissed (it also stops the turn).
pub const QUESTION_DISMISSED: &str = "The user dismissed this question";

/// The result of a call that was not run because the turn stopped first.
pub const TOOLS_INTERRUPTED: &str = "Tool execution interrupted";

const INPUT_MALFORMED: &str = "The tool input could not be parsed as JSON, so the call was not executed. Reissue the call with valid JSON arguments.";
const INPUT_LENGTH: &str = "The tool call was not executed because the output token limit (stop reason: length) was reached before you could complete the arguments. Break large payloads into smaller tool calls and reissue the call with complete arguments.";
const INPUT_EXCERPT_LIMIT: usize = 2048;

/// The result of a call the model named but this session does not offer.
pub fn unavailable(name: &str) -> String {
    format!("No tool named \"{name}\" is currently available. Please use a tool from the available tool list.")
}

/// The result recorded for a call a previous run left without one.
pub fn interrupted(name: &str) -> String {
    format!("{TOOLS_INTERRUPTED}: {name}")
}

/// The result of a call whose arguments were not valid JSON, with an
/// excerpt of what the model sent. `truncated` when the response hit the
/// output limit, which is the usual cause.
pub fn invalid_input(raw: &str, truncated: bool) -> String {
    let base = if truncated {
        INPUT_LENGTH
    } else {
        INPUT_MALFORMED
    };
    if raw.is_empty() {
        return base.to_string();
    }
    let total = raw.chars().count();
    let excerpt: String = raw.chars().take(INPUT_EXCERPT_LIMIT).collect();
    let shown = excerpt.chars().count();
    let more = if total > shown { "\n[truncated]" } else { "" };
    format!("{base}\n\nInput excerpt (first {shown} of {total} characters):\n{excerpt}{more}")
}

/// The raw arguments of a call the provider could not parse.
pub fn invalid_json(input: &Value) -> Option<&str> {
    input
        .get("__invalid_json")
        .map(|v| v.as_str().unwrap_or_default())
}

/// A finish reason meaning the output token limit cut the response off.
pub fn is_length(reason: &str) -> bool {
    matches!(
        reason,
        "length" | "max_tokens" | "max_output_tokens" | "model_length"
    )
}

/// A finish reason meaning the provider's safety filter stopped the response.
pub fn is_content_filter(reason: &str) -> bool {
    matches!(
        reason.to_ascii_lowercase().as_str(),
        "content_filter"
            | "content-filter"
            | "refusal"
            | "safety"
            | "recitation"
            | "prohibited_content"
            | "blocklist"
            | "spii"
            | "image_safety"
    )
}

/// The body fields that stop a provider from calling tools, so a forced
/// final step can keep its tool definitions (and the cached prefix).
pub fn tool_choice_none(protocol: Protocol) -> (&'static str, Value) {
    match protocol {
        Protocol::OpenaiChat | Protocol::OpenaiResponses => ("tool_choice", json!("none")),
        Protocol::AnthropicMessages => ("tool_choice", json!({"type": "none"})),
        Protocol::GoogleGemini => (
            "toolConfig",
            json!({"functionCallingConfig": {"mode": "NONE"}}),
        ),
    }
}

/// An assistant message with nothing to replay.
fn is_empty_assistant(m: &Message) -> bool {
    m.role == Role::Assistant
        && m.parts.iter().all(|p| match p {
            Part::Text(t) => t.is_empty(),
            // Reasoning is not replayed without its provider signature.
            Part::Reasoning(_) => true,
            _ => false,
        })
}

/// Stored history as the model should see it: empty assistant turns are
/// left out, and a tool call left without a result (the turn was stopped
/// while it ran) gets an error result, since providers reject unanswered calls.
pub fn model_messages(history: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    let mut i = 0;
    while i < history.len() {
        let m = &history[i];
        i += 1;
        if is_empty_assistant(m) {
            continue;
        }
        out.push(m.clone());
        if m.role != Role::Assistant {
            continue;
        }
        let calls: Vec<(&str, &str)> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall { id, name, .. } => Some((id.as_str(), name.as_str())),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            continue;
        }
        let answered: Vec<&str> = history
            .get(i)
            .filter(|n| n.role == Role::User)
            .map(|n| {
                n.parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::ToolResult { id, .. } => Some(id.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let missing: Vec<Part> = calls
            .iter()
            .filter(|(id, _)| !answered.contains(id))
            .map(|(id, name)| Part::ToolResult {
                id: id.to_string(),
                name: name.to_string(),
                content: interrupted(name),
                is_error: true,
                media: Vec::new(),
            })
            .collect();
        if missing.is_empty() {
            continue;
        }
        match history.get(i) {
            // Results are present for some calls: add the rest to them.
            Some(next) if !answered.is_empty() => {
                let mut next = next.clone();
                next.parts.splice(0..0, missing);
                out.push(next);
                i += 1;
            }
            _ => out.push(Message {
                role: Role::User,
                parts: missing,
            }),
        }
    }
    out
}

/// Results for the calls of a trailing assistant message that never got
/// any, to record before the next prompt.
pub fn unsettled(history: &[Message]) -> Option<Message> {
    let last = history.last().filter(|m| m.role == Role::Assistant)?;
    let parts: Vec<Part> = last
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall { id, name, .. } => Some(Part::ToolResult {
                id: id.clone(),
                name: name.clone(),
                content: interrupted(name),
                is_error: true,
                media: Vec::new(),
            }),
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then_some(Message {
        role: Role::User,
        parts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(parts: Vec<Part>) -> Message {
        Message {
            role: Role::Assistant,
            parts,
        }
    }
    fn call(id: &str) -> Part {
        Part::ToolCall {
            id: id.into(),
            name: "echo".into(),
            input: json!({}),
        }
    }
    fn result(id: &str) -> Part {
        Part::ToolResult {
            id: id.into(),
            name: "echo".into(),
            content: "ok".into(),
            is_error: false,
            media: Vec::new(),
        }
    }

    // session-runner-message.test.ts: "omits empty assistant turns"
    #[test]
    fn oc_session_message_omits_empty_assistant_turns() {
        let history = vec![
            Message::user("hi"),
            assistant(vec![]),
            assistant(vec![Part::Text(String::new())]),
            assistant(vec![Part::Reasoning(String::new().into())]),
            assistant(vec![Part::Text("Partial".into())]),
        ];
        let got = model_messages(&history);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].parts, vec![Part::Text("Partial".into())]);
    }

    // session-runner-message.test.ts: "omits unfinished reasoning from an interrupted assistant message"
    #[test]
    fn oc_session_message_omits_reasoning_only_assistant_turns() {
        let history = vec![assistant(vec![Part::Reasoning(
            "Completed summary part".into(),
        )])];
        assert!(model_messages(&history).is_empty());
    }

    // session-runner.test.ts: "durably fails local tools left running by a prior process before continuing"
    #[test]
    fn oc_session_unanswered_calls_get_interrupted_results() {
        let history = vec![Message::user("go"), assistant(vec![call("a"), call("b")])];
        let got = model_messages(&history);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[2].parts,
            vec![
                Part::ToolResult {
                    id: "a".into(),
                    name: "echo".into(),
                    content: "Tool execution interrupted: echo".into(),
                    media: Vec::new(),
                    is_error: true
                },
                Part::ToolResult {
                    id: "b".into(),
                    name: "echo".into(),
                    content: "Tool execution interrupted: echo".into(),
                    media: Vec::new(),
                    is_error: true
                },
            ]
        );
        assert_eq!(unsettled(&history).unwrap().parts, got[2].parts);
        // Mid-history, followed by the next prompt.
        let history = vec![
            assistant(vec![call("a")]),
            Message::user("next"),
            assistant(vec![call("c"), call("d")]),
            Message {
                role: Role::User,
                parts: vec![result("d")],
            },
        ];
        let got = model_messages(&history);
        assert_eq!(got.len(), 5);
        assert!(
            matches!(&got[1].parts[0], Part::ToolResult { id, is_error: true, .. } if id == "a")
        );
        assert_eq!(got[2], Message::user("next"));
        assert!(
            matches!(&got[4].parts[0], Part::ToolResult { id, is_error: true, .. } if id == "c")
        );
        assert_eq!(got[4].parts[1], result("d"));
        assert!(unsettled(&history).is_none());
    }

    #[test]
    fn answered_history_is_unchanged() {
        let history = vec![
            Message::user("go"),
            assistant(vec![Part::Text("x".into()), call("a")]),
            Message {
                role: Role::User,
                parts: vec![result("a")],
            },
            assistant(vec![Part::Text("done".into())]),
        ];
        assert_eq!(model_messages(&history), history);
    }

    // publish-llm-event.ts INPUT_FAILURES
    #[test]
    fn oc_session_invalid_tool_input_explains_and_quotes_the_input() {
        assert_eq!(
            invalid_input("{\"text\":\"partial", false),
            "The tool input could not be parsed as JSON, so the call was not executed. Reissue the call with valid JSON arguments.\n\nInput excerpt (first 16 of 16 characters):\n{\"text\":\"partial"
        );
        assert!(invalid_input("{", true)
            .starts_with("The tool call was not executed because the output token limit"));
        let long = "x".repeat(3000);
        assert!(invalid_input(&long, false).ends_with("\n[truncated]"));
        assert!(invalid_input(&long, false).contains("(first 2048 of 3000 characters)"));
        assert_eq!(invalid_input("", false), INPUT_MALFORMED);
    }

    #[test]
    fn tool_choice_none_per_protocol() {
        assert_eq!(
            tool_choice_none(Protocol::OpenaiChat),
            ("tool_choice", json!("none"))
        );
        assert_eq!(
            tool_choice_none(Protocol::AnthropicMessages),
            ("tool_choice", json!({"type": "none"}))
        );
        assert_eq!(tool_choice_none(Protocol::GoogleGemini).0, "toolConfig");
    }

    #[test]
    fn finish_reasons() {
        for r in ["length", "max_tokens"] {
            assert!(is_length(r));
        }
        assert!(!is_length("stop"));
        for r in ["content_filter", "refusal", "safety", "SAFETY"] {
            assert!(is_content_filter(r), "{r}");
        }
        assert!(!is_content_filter("end_turn"));
        assert_eq!(
            unavailable("missing"),
            "No tool named \"missing\" is currently available. Please use a tool from the available tool list."
        );
    }
}
