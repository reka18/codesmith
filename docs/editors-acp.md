# Editors (ACP)

`codesmith acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com)
(v1) on stdin and stdout, so editors that support ACP agents, such as Zed,
can run CodeSmith sessions. The wire format follows OpenCode's ACP agent.

## Zed

In Zed's `settings.json`:

```json
"agent_servers": { "CodeSmith": { "command": "codesmith", "args": ["acp"] } }
```

CodeSmith reads its config from the project the editor opens, as it does on
the terminal (see [configuration](configuration.md)).

## What works

- **Streaming.** Text, reasoning (with message ids), tool calls, and plans
  (from the todo list) stream to the editor.
- **Edits.** Edits and writes carry full-file diffs, in their tool calls and
  in their permission requests, which appear in the editor.
- **Agents and models.** Agents (build, plan, ...) are modes. The model and
  mode are config options the editor can switch. A model with
  [variants](configuration.md#provider-options-and-variants) also gets an
  **Effort** option (`effort`, category `thought_level`) listing them plus
  `default`, which drops the variant. A model can also be picked with its
  variant, as `provider/model#variant` or `provider/model/variant`. Switching
  to another model resets the effort to `default`; the same model keeps it.
  A switch during a turn takes effect when the turn ends, and is saved with
  the session (as `provider/model#variant`). Every switch is also announced
  with a `config_option_update`. "Always" permission answers stay in force
  for the session through these switches and through cancelled turns.
- **Turn results.** Each turn ends with a stop reason (`end_turn`,
  `max_tokens`, `refusal`, `max_turn_requests`, `cancelled`) and token usage,
  followed by the context window use and cost when the model's limit and
  price are configured.
- **Commands.** Slash commands, including `/review`, `/calibrate` and
  `/compact`, are offered.
- **Background commands** wake the idle session when they finish (see
  below).
- **Sessions** can be listed, loaded (with their whole history replayed),
  resumed, forked, cancelled, closed and deleted. They can be given
  additional directories the agent may use without asking. They are the same
  sessions the terminal UI and `codesmith run` use.
- **Questions.** When the editor supports form elicitation, the agent can ask
  questions with the `question` tool.
- **MCP servers** the editor passes in are added to the session (stdio and
  HTTP).

## Background commands

A shell command started with `background: true` keeps running after the
model answers, and the prompt ends with `end_turn` while it runs (see
[background commands](sessions.md#background-commands)). When it finishes
and the session is idle, the agent wakes the session by itself, as
OpenCode's does:

- The model's reaction streams as ordinary `session/update` notifications
  for the session (`agent_message_chunk` under a new `messageId`,
  `tool_call`, `tool_call_update`, `plan`, ...), with no `session/prompt` to
  answer and no marker for its start or end; a `usage_update` follows when
  the context window is known. Its permission requests and questions go to
  the editor as usual.
- A prompt sent while the session waits takes over: its turn reports the
  command (once), and the session is not woken for it again.
- A prompt sent while a woken turn runs waits for that turn to end, then
  runs as usual. OpenCode steers such a prompt into the running turn; here
  it starts the next one. A second prompt meanwhile is refused as busy.
- `session/cancel` stops a woken turn like any other: its open permission
  request is withdrawn, its call in flight fails, the session reloads, and a
  prompt waiting behind it is answered with `cancelled`. A cancel while the
  session is idle leaves the wait alone.
- Closing, deleting or reopening the session stops the wait and its
  commands. A cancelled turn, or a mode or model switch, also stops the
  session's commands, as in the terminal UI.

## Images and resources

Prompts can carry images (the agent advertises `promptCapabilities.image`)
and embedded resources:

- Text resources join the message.
- Image or PDF blobs, recognized by their bytes, are sent to the model as
  attachments, fitted to the [`media` limits](configuration.md#media-limits).
  An image that can't be fitted refuses the prompt.
- Images given only by URI, and resource links, are referenced in the
  message for the agent to `read`.
- Images a tool returns appear in its `tool_call_update` content, and a
  loaded session replays the prompt's images too.

Permission requests go to the editor, which shows the same allow once,
always allow and reject choices as the terminal. See
[permissions](permissions.md).
