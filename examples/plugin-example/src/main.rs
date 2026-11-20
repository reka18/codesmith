//! A custom CodeSmith with one plugin compiled in. Build and run it like the
//! stock binary: `cargo run -p codesmith-plugin-example -- run "..."`.
//!
//! The plugin adds:
//! - a `todo_scan` tool that lists TODO/FIXME comments in the project,
//! - a `/standup` command (its greeting configurable via plugin options),
//! - a guard that refuses `git push --force` to main.
//!
//! Configure it in codesmith.json: `"plugins": {"example": {"greeting": "Morning"}}`,
//! or turn it off with `"plugins": {"example": false}`.

use codesmith::core::config::Config;
use codesmith::plugin::{BoxFuture, Plugin, Tool, ToolCall, ToolContext};
use serde_json::{json, Value};
use std::sync::Arc;

struct Example;

impl Plugin for Example {
    fn id(&self) -> &str {
        "example"
    }

    fn config(&self, config: &mut Config, options: &Value) {
        let greeting = options["greeting"].as_str().unwrap_or("Hi");
        let command = json!({
            "description": "summarize recent work for a standup",
            "template": format!("{greeting}! Summarize what changed in this repository since yesterday (use git log), in three bullet points.")
        });
        if let Ok(c) = serde_json::from_value(command) {
            config.commands.insert("standup".into(), c);
        }
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(TodoScan)]
    }

    fn tool_before(&self, call: &mut ToolCall) -> Result<(), String> {
        let cmd = call.input["command"].as_str().unwrap_or("");
        if call.name == "shell"
            && cmd.contains("git push")
            && cmd.contains("--force")
            && cmd.contains("main")
        {
            return Err("The example plugin refuses force-pushes to main.".into());
        }
        Ok(())
    }
}

struct TodoScan;

impl Tool for TodoScan {
    fn name(&self) -> &str {
        "todo_scan"
    }
    fn description(&self) -> &str {
        "List TODO and FIXME comments in the project's tracked files, with file and line."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"limit": {"type": "integer", "description": "Most results to return (default 50)"}}})
    }
    fn execute<'a>(
        &'a self,
        ctx: ToolContext,
        input: Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let limit = input["limit"].as_u64().unwrap_or(50) as usize;
            let out = std::process::Command::new("git")
                .args(["grep", "-n", "-I", "-E", "TODO|FIXME"])
                .current_dir(&ctx.root)
                .output()
                .map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&out.stdout);
            let lines: Vec<&str> = text.lines().take(limit).collect();
            Ok(if lines.is_empty() {
                "No TODO or FIXME comments found.".into()
            } else {
                lines.join("\n")
            })
        })
    }
}

fn main() {
    codesmith::main(vec![Arc::new(Example)]);
}
