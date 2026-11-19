//! `todowrite`: the agent's working plan, shown to the user. Ported from
//! OpenCode's todo tool. The list lives in the session; the session handles
//! the call, this module defines the schema and formatting.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DESCRIPTION: &str = "Create and update a todo list for the current session. The user sees it as your working plan.

Use it proactively when a task has 3 or more distinct steps, or when the user gives several tasks or asks for a todo list. Skip it for a single simple task or a purely conversational request. When in doubt, use it.

- Send the full list on every call; it replaces the previous one.
- Mark items `in_progress` when you start them and `completed` as soon as they're actually done.
- Only call it when the list changes.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: Status,
}

pub fn spec() -> (String, Value) {
    (
        DESCRIPTION.into(),
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The updated todo list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Brief description of the task"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "Current status of the task: pending, in_progress, completed, cancelled"}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        }),
    )
}

pub fn parse(input: &Value) -> Result<Vec<Todo>, String> {
    let todos = input
        .get("todos")
        .ok_or("Missing required parameter `todos`")?;
    serde_json::from_value(todos.clone()).map_err(|e| format!("Invalid todos: {e}"))
}

/// The model-facing summary (OpenCode's `summarize`).
pub fn summarize(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "Todos cleared".into();
    }
    let count = |s: Status| todos.iter().filter(|t| t.status == s).count();
    let mut counts = vec![format!(
        "{}/{} completed",
        count(Status::Completed),
        todos.len()
    )];
    for (s, label) in [
        (Status::InProgress, "in progress"),
        (Status::Pending, "pending"),
        (Status::Cancelled, "cancelled"),
    ] {
        if count(s) > 0 {
            counts.push(format!("{} {label}", count(s)));
        }
    }
    let active: Vec<String> = todos
        .iter()
        .filter(|t| t.status == Status::InProgress)
        .map(|t| serde_json::to_string(&t.content).unwrap())
        .collect();
    let mut out = format!("Todos updated: {}", counts.join(", "));
    if !active.is_empty() {
        out.push_str(&format!(". In progress: {}", active.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(c: &str, s: Status) -> Todo {
        Todo {
            content: c.into(),
            status: s,
        }
    }

    #[test]
    fn summaries() {
        assert_eq!(summarize(&[]), "Todos cleared");
        let list = [
            t("a", Status::Completed),
            t("b", Status::InProgress),
            t("c", Status::Pending),
            t("d", Status::Pending),
        ];
        assert_eq!(
            summarize(&list),
            "Todos updated: 1/4 completed, 1 in progress, 2 pending. In progress: \"b\""
        );
        assert_eq!(
            summarize(&[t("x", Status::Cancelled)]),
            "Todos updated: 0/1 completed, 1 cancelled"
        );
    }

    #[test]
    fn parsing() {
        let v = json!({"todos": [{"content": "Write tests", "status": "in_progress"}]});
        assert_eq!(
            parse(&v).unwrap(),
            vec![t("Write tests", Status::InProgress)]
        );
        assert!(
            parse(&json!({"todos": [{"content": "x", "status": "doing"}]}))
                .unwrap_err()
                .starts_with("Invalid todos")
        );
        assert!(parse(&json!({})).is_err());
    }
}
