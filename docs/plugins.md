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
| `request` | Change the system prompt, messages, tools or body just before a model request. It also runs for compaction summaries (`req.interaction` is `Compaction`), before the summary instructions are added, so a hook that shapes every request alike keeps the summary on the prompt cache. |
| `compaction` | Before a compaction summarizes: change the summary request (the session's system prompt, tools and whole conversation, without the summary instructions), or supply the summary yourself in `result`, and no model request is made. Runs before `request`; a supplied summary skips both. |
| `permission` | Answer a permission request instead of the user. It is consulted only for requests the rules would ask about, never for ones they deny. |
| `event` | Observe turns, steps and tool calls. |
| `idle` | Continue a finished turn with another message (at most 5 times per turn). |
| `prompt` | Change a user prompt as it joins the session, or keep metadata with it (stored with the message, never sent to the model). |
| `context` | Rewrite the model context just before a step's request is built: the system prompt, the messages (with the metadata `prompt` kept), and which tools are offered under which names. A tool renamed here runs when the model calls the new name. Edits reach that request only. Runs before `request`. |
| `http_request`, `http_response` | Change a model request's headers, URL or body just before it is sent, after the credentials are applied, or look at (or replace) the response. They run for every request a session makes; `scope.kind` says which: `Primary` (a step), `Compaction`, `Title` or `Generate`. A request left alone is sent as built. |
| `ws_handshake`, `ws_send`, `ws_receive` | The same for a Responses WebSocket: its URL and headers (a change opens a new socket), and each frame out and in. |
| `session_idle` | Async: the session finished its work and would go idle, perhaps with background shells running (`idle.background`). Continue it with a message, ask for a compaction and run again, or let it go idle. `idle.generate(prompt)` is a side request with the session's context (its history, then the prompt, tools off), for a reviewer. Runs after `idle` declined; at most 100 continuations per turn. |
| `interrupted` | The turn was stopped before it finished (the user interrupted it). |
| `wants` | Whether a hook has anything to do for a session. CodeSmith builds a hook's input (a copy of the history for `context`, the HTTP bridge, the idle hook's state) only when some plugin wants it, so a build whose plugins use none of these pays nothing for them. The default is yes. |

`tool_before` and `tool_after` also run for each call a Code Mode program
makes inside `execute`: a built-in under its own name, a `fetch` under
`webfetch` (the input is `url`, `method`, `headers` and a base64 `body`; the
output `tool_after` sees is the response as JSON, with a UTF-8 body as
`bodyText`), and an OpenAPI call under `openapi` (the input is
`{"tool": <name>, "input": <input>}`; changing `tool` has no effect). A veto
refuses that call, and the program's promise rejects with its message.

The hooks follow OpenCode's names where it has them: `context` is its session
`context` hook, `http_*` its `http.request`/`http.response`, `ws_*` its
`experimental.ws.*`, `session_idle` its `experimental.idle`.

### Trust

Plugins are trusted: they are Rust compiled into your build, with the same
access as CodeSmith itself. What a hook is handed is not always trusted,
though. Message text in `context` and `idle.messages`, tool input and
output, `idle.generate` replies, HTTP responses and inbound WebSocket frames
come from the model, the tools or the network. Treat them as untrusted
input: parse strictly, and never let them decide what a plugin runs or where
it sends credentials. The goal plugin is an example: the objective is
escaped and wrapped as data in every prompt, and the reviewer's verdict
must parse as one of five JSON shapes or it counts as no verdict.

## The plugin host

`codesmith::plugin::host()` is the process's host (OpenCode's plugin context:
`ctx.session`, `ctx.permission`, `ctx.storage`), over the session store. Its
calls fail with a `HostError` (`NotFound` for an unknown session):

| Call | What it does |
|---|---|
| `create(NewSession)` | Create a session. A child (`parent_id`) starts in its parent's folder with its agent and model. |
| `get(id)`, `set_title(id, title)`, `remove(id)` | Look up, rename, or remove a session and its children. |
| `list(&ListQuery)` | A page of sessions, most recently updated first, filtered by folder, parent (`Parent::Roots`, `Parent::Of(id)`) and title search. `page.next` is a cursor that carries the query: the next page needs only it. |
| `put_instruction(id, key, value)`, `list_instructions(id)`, `remove_instruction(id, key)` | Session instruction entries: context the model sees, in the system prompt or as an update at the session's next step ("The context under ... changed"). Keys are `^[a-z0-9][a-z0-9._-]*$`, values at most 256 KiB of JSON. |
| `compact(id, delivery, message_id)` | Admit a compaction: the session compacts at its next step (`Delivery::Steer`, the default) or before its next prompt (`Delivery::Queue`). |
| `assert_permission(AssertPermission)` | Async. Check a permission for a running session as a built-in tool would: its rules, its "always" approvals, then the `permission` hooks or the user. An `Err(PermissionError)` is the outcome to hand on (a tool returns its text as its error): `Blocked` by a rule, `Corrected` with the user's feedback, or `Declined`, after which the turn stops as it does for a declined tool. |
| `storage(plugin_id)` | The plugin's own key-value store (`get`, `set`, `remove`, `scan` by prefix), kept in the session store. |

Tests and embedders with their own store install a host with
`plugin::set_host`.

## The goal plugin

Every build has three built-in plugins: `goal`, `plan` and `codesmith`
(OpenCode's goal and plan plugins and its `opencode` tools; the
[`codesmith` tool](tools.md#codesmith) is described with the tools). `/goal
<objective>` keeps the session working until the objective is met: when the
session would go idle, a reviewer (a side request with the session's
context) answers with a verdict, and the session continues with the
reviewer's reason and next step until it says done. The goal pauses when
the reviewer says the user is needed, after 50 rounds, after three rounds in
a row without progress, when the reviewer gives no usable answer twice, when
the session fails or is stopped, when nothing ran for 30 minutes while it
waited on a background shell, and when CodeSmith restarted while it was
active. `/goal pause`, `/goal resume` and `/goal clear` control it, and
`/goal` alone shows it. Turn it off with `"plugins": {"goal": false}`.

## The plan plugin

The `plan` plugin runs Plan mode for the [`plan` agent](agents-and-subagents.md).
When a session's agent is `plan`, a reminder that Plan mode is on, naming
the Plan directory, joins the conversation before the next prompt; when
the session switches to another agent, a reminder that it is off. The
reminders are saved in the session's history, not added to the system
prompt, so switching agents keeps the provider's prompt cache. If a
compaction or an undo drops the reminder, the next request adds it again.
A reminder quoted inside a larger message (a compaction summary, say)
doesn't count.

The `plan` agent may write plan files only in the Plan directory,
`~/.local/share/codesmith/plan` (`$XDG_DATA_HOME/codesmith/plan`). Nothing
creates it until a plan file is written. A write anywhere else fails with
"Cannot use write to modify files outside the Plan directory: ...".
Reading outside the project needs no prompt for the `plan` agent (see
[permissions](permissions.md#defaults)).

Turn the plugin off with `"plugins": {"plan": false}`: no reminders, and a
blocked write reports the plain permission error. The `plan` agent's rules
stay as they are.

## Turning plugins on and off

Every plugin compiled into the binary is on, unless config turns it off.
The plugin's entry under `plugins` is passed to its `config` hook as
options:

```jsonc
{ "plugins": { "example": { "greeting": "Morning" } } }   // on, with options
{ "plugins": { "example": false } }                        // off
```

`codesmith debug plugins` lists the plugins compiled in (the built-in `goal`,
`plan` and `codesmith` among them), and whether config turns them off.

## Example

[`examples/plugin-example`](../examples/plugin-example/src/main.rs) is a
complete plugin. It adds:

- a `todo_scan` tool that lists TODO and FIXME comments in the project (a
  long list asks permission first, through the host),
- a `/standup` command whose greeting is set by the plugin's options,
- a `/remember <note>` command that pins a note to the session as an
  instruction entry,
- a guard that refuses `git push --force` to `main`,
- prompt and context hooks that remind the model to check the changelog
  when a prompt mentions a release,
- an HTTP hook that tags the agent's own model requests with a header.

The heart of it:

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
