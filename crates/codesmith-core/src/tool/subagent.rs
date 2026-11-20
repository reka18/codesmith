//! `subagent`: delegate a task to an agent in a child session. The session
//! runs the child; this module defines the schema and result text.
//!
//! OpenCode runs subagents in the background and delivers the `<subagent>`
//! report as a later message; here the call runs the child to completion and
//! the same report is the tool's result (an error result when the child
//! failed or was cancelled). There is no "background work still running"
//! list: a child waits for its own background commands before it finishes.
//! Unknown agents get a list of the available ones, where OpenCode says only
//! "Unknown agent".

use serde_json::{json, Value};

pub fn spec(agents: &[(String, String)]) -> (String, Value) {
    let mut description = [
        "Spawns an agent in a child session to work on the specified task.",
        "The call runs the subagent to completion and returns its final answer and a sessionID.",
        "New child sessions start with fresh context, so include all relevant context and instructions when you don't pass a sessionID.",
        "Pass the returned sessionID to continue that specific child conversation.",
        "Launch several subagents in parallel only for independent work.",
    ]
    .join("\n");
    if !agents.is_empty() {
        description.push_str("\n\nAvailable agents:");
        for (id, desc) in agents {
            description.push_str(&format!("\n- {id}: {desc}"));
        }
    }
    (
        description,
        json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string", "description": "The type of specialized agent to use for this task. If the user asks for a subagent by a name that is not one of the available subagents, they most likely mean a model: pick a suitable agent and pass the name through the model parameter instead."},
                "description": {"type": "string", "description": "A short 3-5 word label for the task, displayed to the user"},
                "prompt": {"type": "string", "description": "The task for the subagent to perform"},
                "model": {"type": "string", "description": "NEVER set this unless the user explicitly asks for a particular model. The value is written as \"providerID/modelID\"."},
                "sessionID": {"type": "string", "description": "Send a follow-up to an existing subagent session. Omit to start a new one."}
            },
            "required": ["agent", "description", "prompt"]
        }),
    )
}

/// The first line of a new child session's prompt.
pub const PREAMBLE: &str = "You are a subagent spawned by another session.";

/// What a subagent's report says when its last message has no text.
pub const NO_TEXT: &str = "Subagent completed without a text response.";

/// How a subagent's run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Completed,
    Error,
    /// The user stopped it (declined a tool call or dismissed a question).
    Cancelled,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Completed => "completed",
            State::Error => "error",
            State::Cancelled => "cancelled",
        }
    }
}

/// What the parent sees when a subagent finishes: OpenCode's `<subagent>`
/// report, with the child's final text, or the failure.
pub fn report(session_id: &str, state: State, description: &str, body: &str) -> String {
    let body = body.trim();
    let body = if body.is_empty() && state == State::Completed {
        NO_TEXT
    } else {
        body
    };
    format!(
        "<subagent sessionID=\"{session_id}\" state=\"{}\" description=\"{description}\">\n{body}\n</subagent>",
        state.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_lists_agents() {
        let (d, p) = spec(&[("explore".into(), "Fast search".into())]);
        assert!(d.ends_with("Available agents:\n- explore: Fast search"));
        assert_eq!(p["required"], json!(["agent", "description", "prompt"]));
        let (d, _) = spec(&[]);
        assert!(!d.contains("Available agents"));
    }

    // subagent-completion.ts `deliver`: the report envelope.
    #[test]
    fn oc_subagent_report_format() {
        assert_eq!(
            report("ses_1", State::Completed, "Find it", " found it \n"),
            "<subagent sessionID=\"ses_1\" state=\"completed\" description=\"Find it\">\nfound it\n</subagent>"
        );
        assert!(report("s", State::Completed, "d", "").contains(NO_TEXT));
        assert_eq!(
            report("s", State::Error, "d", "Disconnected"),
            "<subagent sessionID=\"s\" state=\"error\" description=\"d\">\nDisconnected\n</subagent>"
        );
        assert!(report("s", State::Cancelled, "d", "Subagent cancelled")
            .contains("state=\"cancelled\""));
    }
}
