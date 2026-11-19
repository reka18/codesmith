//! `subagent`: delegate a task to an agent in a child session. The session
//! runs the child; this module defines the schema and result text.
//!
//! OpenCode runs subagents in the background and notifies the parent; here
//! the call runs the child to completion and returns its final answer.

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

/// What the parent sees when a subagent finishes.
pub fn result(session_id: &str, agent: &str, answer: &str) -> String {
    let answer = answer.trim();
    format!(
        "<subagent sessionID=\"{session_id}\" agent=\"{agent}\" state=\"completed\">\n{}\n</subagent>",
        if answer.is_empty() { "(The subagent finished without a final message.)" } else { answer }
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

    #[test]
    fn results() {
        assert_eq!(result("ses_1", "explore", " found it \n"), "<subagent sessionID=\"ses_1\" agent=\"explore\" state=\"completed\">\nfound it\n</subagent>");
        assert!(result("s", "a", "").contains("without a final message"));
    }
}
