# Plugins

Plugins are native Rust, compiled into your own build of CodeSmith. There is
no scripting runtime and nothing is downloaded. A plugin implements
`codesmith::plugin::Plugin`, overriding only the hooks it needs, and a
three-line `main` builds a CodeSmith that includes it:

```rust
fn main() {
    codesmith::main(vec![std::sync::Arc::new(MyPlugin)]);
}
```

The resulting binary is a complete CodeSmith, with every command and flag of
the stock one.

## Hooks

The hooks follow OpenCode's plugin API:

| Hook | What it can do |
|---|---|
| `config` | Edit the merged config: add or change agents, commands, providers, MCP servers, formatters, permissions. Receives the plugin's options. |
| `tools` | Add tools. They are permission-checked by name, like any other tool. |
| `commands` | Add commands that run as code (OpenCode's callback commands): `/name args` calls the command's `execute` with the session id, folders, arguments and attachments. It returns a prompt to send (`Ok(Some(text))`), nothing (`Ok(None)`), or an error message. A config command of the same name replaces it. |
| `tool_before` | Change a tool call's input, or refuse it with a message the model sees. |
| `tool_after` | Change what the model sees from a tool call. |
| `shell_before` | Change a shell command, its folder or its environment. The permission checks run on the result. |
| `request` | Change the system prompt, messages, tools or body just before a model request. |
| `permission` | Answer a permission request instead of the user. It is consulted only for requests the rules would ask about, never for ones they deny. |
| `event` | Observe turns, steps and tool calls. |
| `idle` | Continue a finished turn with another message (at most 5 times per turn). |

## Turning plugins on and off

Every plugin compiled into the binary is on, unless config turns it off.
The plugin's entry under `plugins` is passed to its `config` hook as
options:

```jsonc
{ "plugins": { "example": { "greeting": "Morning" } } }   // on, with options
{ "plugins": { "example": false } }                        // off
```

`codesmith debug plugins` lists the plugins compiled in, and whether config
turns them off.

## Example

[`examples/plugin-example`](../examples/plugin-example/src/main.rs) is a
complete plugin. It adds:

- a `todo_scan` tool that lists TODO and FIXME comments in the project,
- a `/standup` command whose greeting is set by the plugin's options,
- a guard that refuses `git push --force` to `main`.

```rust
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
        if call.name == "shell" && cmd.contains("git push") && cmd.contains("--force") && cmd.contains("main") {
            return Err("The example plugin refuses force-pushes to main.".into());
        }
        Ok(())
    }
}

// `TodoScan` implements `codesmith::plugin::Tool`: name, description,
// JSON-schema parameters, and an async `execute`. See the full file.

fn main() {
    codesmith::main(vec![Arc::new(Example)]);
}
```

Try it from a checkout:

```sh
cargo run -p codesmith-plugin-example -- debug plugins
cargo run -p codesmith-plugin-example -- run /standup
```

A plugin's own `Cargo.toml` needs only `codesmith` (by path or git) and
whatever the plugin uses:

```toml
[dependencies]
codesmith = { git = "https://github.com/reka18/codesmith" }
serde_json = "1"
```
