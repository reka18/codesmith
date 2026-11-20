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
- **Sessions** can be listed, loaded (with their whole history replayed),
  resumed, forked, cancelled, closed and deleted. They can be given
  additional directories the agent may use without asking. They are the same
  sessions the terminal UI and `codesmith run` use.
- **Questions.** When the editor supports form elicitation, the agent can ask
  questions with the `question` tool.
- **MCP servers** the editor passes in are added to the session (stdio and
  HTTP).

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
