//! A custom CodeSmith with one plugin compiled in. Build and run it like the
//! stock binary: `cargo run -p codesmith-plugin-example -- run "..."`.
//!
//! The plugin adds:
//! - a `todo_scan` tool that lists TODO/FIXME comments in the project (a
//!   long list asks permission first, through the plugin host),
//! - a `/standup` command (its greeting configurable via plugin options),
//! - a `/remember <note>` command that pins a note to the session as
//!   context the model sees (a session instruction entry),
//! - a guard that refuses `git push --force` to main,
//! - prompt and context hooks: a prompt mentioning a release is tagged, and
//!   the model is reminded to check the changelog for it,
//! - an HTTP hook that tags the agent's own model requests with a header.
//!
//! Configure it in codesmith.json: `"plugins": {"example": {"greeting": "Morning"}}`,
//! or turn it off with `"plugins": {"example": false}`.

use codesmith::core::config::Config;
use codesmith::plugin::{
    self, reqwest, AssertPermission, BoxFuture, Command, CommandContext, Context, Hook, Part,
    Plugin, Prompt, RequestKind, RequestScope, Tool, ToolCall, ToolContext,
};
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

    fn commands(&self) -> Vec<Arc<dyn Command>> {
        vec![Arc::new(Remember)]
    }

    // No async idle hook here, so none needs to run for this plugin.
    fn wants(&self, hook: Hook, _session_id: &str) -> bool {
        hook != Hook::SessionIdle
    }

    fn prompt(&self, prompt: &mut Prompt) {
        if prompt.text.to_lowercase().contains("release") {
            prompt.metadata = Some(json!({"example": {"topic": "release"}}));
        }
    }

    fn context(&self, ctx: &mut Context) {
        // The metadata is ours; the message text is the user's or the
        // model's, and untrusted.
        for m in &mut ctx.messages {
            if m.metadata
                .as_ref()
                .is_some_and(|v| v["example"]["topic"] == "release")
            {
                m.message
                    .parts
                    .insert(0, Part::text("(Check CHANGELOG.md before answering.)"));
            }
        }
    }

    fn http_request(&self, scope: &RequestScope, req: &mut reqwest::Request) {
        if scope.kind == RequestKind::Primary {
            req.headers_mut().insert(
                "x-example-plugin",
                reqwest::header::HeaderValue::from_static("1"),
            );
        }
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
            // A long list costs context: ask first, like a built-in tool
            // would (`{"action": "todo_scan.long", "effect": "allow"}`
            // in the permissions skips the question).
            if limit > 500 {
                let host = plugin::host().map_err(|e| e.to_string())?;
                host.assert_permission(AssertPermission {
                    session_id: ctx.session_id.clone(),
                    action: "todo_scan.long".into(),
                    resources: vec![ctx.root.display().to_string()],
                    title: Some(format!("List up to {limit} TODO comments")),
                    ..Default::default()
                })
                .await
                .map_err(|e| e.to_string())?;
            }
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

/// `/remember <note>`: pin a note to this session as context.
struct Remember;

impl Command for Remember {
    fn name(&self) -> &str {
        "remember"
    }
    fn description(&self) -> Option<&str> {
        Some("pin a note to this session as context the model sees")
    }
    fn execute<'a>(&'a self, ctx: CommandContext) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let host = plugin::host().map_err(|e| e.to_string())?;
            let note = ctx.args.trim();
            if note.is_empty() {
                host.remove_instruction(&ctx.session_id, "example.note")
            } else {
                host.put_instruction(&ctx.session_id, "example.note", &json!(note))
            }
            .map_err(|e| e.to_string())?;
            Ok(None)
        })
    }
}

fn main() {
    codesmith::main(vec![Arc::new(Example)]);
}
