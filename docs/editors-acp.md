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
- **Subagents.** What a subagent does streams too (see below).
- **Edits.** Edits and writes carry full-file diffs, in their tool calls and
  in their permission requests, which appear in the editor. A loaded
  session replays the same diffs: each call records the file's text before
  and after (while both fit in 256 KiB), so later changes to the file do
  not alter them. Edits made before CodeSmith recorded them replay their
  output only.
- **Compaction.** A compaction (`/compact`, or automatic near the context
  limit) streams as OpenCode's does to editors that advertise
  `session.compaction`: a `compaction_update` with status `in_progress`,
  the summary as `compaction_summary_chunk`s, then `completed` with the
  whole summary (`failed`, or `cancelled` when the turn is cancelled).
  Other editors get `session_info_update` markers under
  `_meta["codesmith/compaction"]` (`started`, then `completed` with the
  token counts before and after, or `failed`). A retried or nudged summary
  streams again from the start; the completion has the final one. A
  compaction with nothing to compact yet fails at once: `in_progress`
  then `failed` (or the `failed` marker alone). A replayed compaction
  whose summary is empty (a provider's own window) clears the summary.
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
- **Sessions** can be listed (newest first, 100 a page, with a
  `nextCursor` for the rest), loaded (with their whole history replayed),
  resumed, forked, cancelled, closed and deleted. They can be given
  additional directories the agent may use without asking; those are kept
  with the session and listed as its `additionalDirectories`, and loading,
  resuming or forking it replaces them with the ones asked for (none drops
  them). They are the same sessions the terminal UI and `codesmith run`
  use.
- **Questions.** When the editor supports form elicitation, the agent can ask
  questions with the `question` tool.
- **MCP servers** the editor passes in (`mcpServers`, stdio and HTTP) are
  added to the session, in place of a configured server of the same name:
  the editor stands for you, so they count as your own config. Their tools
  still go through your [permission rules](permissions.md) like any MCP
  server's, and what they return (results, elicitation forms) is tool
  output, not instructions. An HTTP server's credentials are the headers the
  editor sends; CodeSmith's saved MCP OAuth logins are not used for it.

## Subagents

Subagents run inside the turn that starts them, and their tool calls,
text and reasoning stream as they happen, marked as theirs, as in
OpenCode:

- Editors that advertise `_meta["opencode/child-session-updates"]: true`
  in their client capabilities (the agent advertises it too) get a
  subagent's activity apart, as `opencode/session/child_update`
  notifications: `{rootSessionId, childSessionId, parentSessionId, depth,
  title}` with `type: "status"` (`created`, `running`, then `completed`,
  `interrupted` or `failed` with an `error`) or `type: "update"` with the
  `session/update` it would have sent.
- Other editors get those updates in the session's own stream, under
  `_meta["opencode/child-session"]` (`id`, `parentID`, `depth`, `title`),
  with tool call ids prefixed by the subagent's session id and titles by
  its description.
- Either way, a subagent's permission requests go to the session as
  `session/request_permission`, pointing at its call (prefixed id, titled
  and marked as above).

Subagents that keep running after the turn (OpenCode's background
subagents) are not ported.

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
- The commands survive a cancelled turn and mode, model and effort
  switches, as in OpenCode: the rebuilt session waits for them and is woken
  when they finish. Reopening an open session (`session/load` or
  `session/resume`) hands them to the new one. A forked session starts
  without any; the original keeps its own.
- After a cancel, commands the cancelled turn had not reported yet do not
  wake the session again by themselves: the next wake waits for another
  command to finish, and that turn (or the next prompt) reports them all.
- A woken turn that fails is retried after a second, doubling up to a
  minute, rather than at once.
- Closing or deleting the session, or the end of the agent's input, stops
  the wait and its commands.

## Images and resources

Prompts can carry images (the agent advertises `promptCapabilities.image`)
and embedded resources:

- Text resources join the message.
- Image, PDF or video blobs, recognized by their bytes, are sent to the model as
  attachments, fitted to the [`media` limits](configuration.md#media-limits).
  An image that can't be fitted refuses the prompt.
- Local files linked (a `resource_link` or an image given only by a
  `file://` URI, or a Zed link with a `path`) are attached when the turn
  starts, as `@` mentions are on the terminal and as OpenCode attaches
  them: each goes through `read`'s permission checks (and
  `external_directory` outside the project, asked through the editor),
  then joins the message as the Read tool's output, or as an attachment
  for images, PDFs and videos. A file that can't be attached (missing, or
  refused) is noted in the message instead. A loaded session replays them
  as the resource links they came as.
- Other links are referenced in the message (`[name](uri)`).
- Images a tool returns appear in its `tool_call_update` content, and PDFs and
  videos as embedded binary resources (OpenCode sends images only). A loaded
  session replays the prompt's images too.

Permission requests go to the editor, which shows the same allow once,
always allow and reject choices as the terminal. See
[permissions](permissions.md).
