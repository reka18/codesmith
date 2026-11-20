# Tools

These are the tools the model can call. Each is checked against
[permissions](permissions.md) before it runs. An agent sees only the tools
its rules don't deny outright (`codesmith debug agents` lists them).

| Tool | What it does | Offered when |
|---|---|---|
| `read` | Read a file (numbered lines, paged), an image or PDF (sent to the model as media), or list a directory. | always |
| `write` | Create or overwrite a file. | always |
| `edit` | Exact find-and-replace. It tolerates typographic quotes, dashes and trailing whitespace in the search text, and keeps the file's CRLF or LF line endings. | always |
| `glob` | Find files by glob pattern, respecting `.gitignore`. | always |
| `grep` | Search file contents with ripgrep's regex engine. | always |
| `shell` | Run a command. See [below](#shell). | always |
| `webfetch` | Fetch a URL as Markdown, text or HTML. | always |
| `websearch` | Search through your own SearXNG instance. | `websearch.url` is set ([config](configuration.md#web-search)) |
| `todowrite` | Keep a visible todo list for multi-step work. | always |
| `subagent` | Delegate a task to a subagent in a child session. | the agent has subagents to call ([agents](agents-and-subagents.md#subagents)) |
| `skill` | Load a skill's instructions. | skills exist ([skills](skills-and-commands.md#skills)) |
| `question` | Ask you multiple-choice or free-form questions. | someone can answer (not with `--auto` or without a terminal) |
| `lsp` | Definitions, references, hover, symbols, implementations and call hierarchy. | `lsp` is on ([language servers](language-servers-and-formatters.md#the-lsp-tool)) |
| `automation` | Schedule, list, pause, resume or remove automations. | the session is stored and `automation` is not denied ([automations](automations.md)) |
| `execute` | Run a Code Mode program over MCP tools. | MCP tools are in Code Mode ([MCP](mcp.md#code-mode)) |
| `<server>_<tool>` | An MCP tool, as a direct call. | `codemode` is off for that server |
| `list_mcp_resources`, `read_mcp_resource` | Browse and read MCP resources. | a connected server has resources ([MCP](mcp.md#resources)) |

[Plugins](plugins.md) can add tools of their own.

Providers can also run tools of their own, such as web search or code
execution, when a model's `hostedTools` option turns them on
([config](configuration.md#hosted-tools)). Their calls show as tool rows but
never run locally.

Each agent has a step limit, 200 by default (`agents.<id>.steps`). On the
last step, the model has to answer in text.

## Shell

- Commands run with `<shell> -c`, where the shell is `shell` from config,
  else `$SHELL`, else `/bin/sh`. `$CODESMITH_SESSION_ID` is set.
- Each command in a shell line is checked separately, including wrapped
  commands and redirect targets. See [shell commands](permissions.md#shell-commands).
- The default timeout is two minutes. A command that outlives its timeout,
  or one started with `background: true`, keeps running in the background
  with its output streaming to a file, and the agent is told when it
  finishes.
- Large output is shown from its end, and the full output is saved to a file
  the model can read ([tool output](configuration.md#tool-output)).

## Files and paths

Relative paths resolve against the working directory. Paths outside the
project need `external_directory` approval first
([details](permissions.md#paths-outside-the-project)). CodeSmith's own
scratch folder (`<data>/tmp`) is always allowed, and the model is told about
it for temporary files.

After `write` or `edit`, the optional [formatter](language-servers-and-formatters.md#formatters)
runs, and [language server](language-servers-and-formatters.md#language-servers)
errors are added to the result.

## `webfetch`

Fetches over HTTP(S) and converts HTML to Markdown unless asked for text or
raw HTML. Each redirect target is checked against your `deny` rules, so a
denied host can't be reached through a redirect.

## Images and PDFs

PNG, JPEG, GIF, WebP and PDF files are sent to the model natively. CodeSmith
recognizes them by their bytes, not their names. They arrive through:

- `read` on the file,
- `@path` mentions and pasted paths in the [UI](interactive-ui.md#attaching-images-and-pdfs),
- `codesmith run -f FILE`,
- `attachments` on the [local server](local-server.md#attachments), ACP
  prompts, and images that MCP tools return.

Each provider gets its native form: image and document blocks on Anthropic;
`image_url` and `file` parts on OpenAI Chat (where media from a tool result
follows the tool messages as a user message, since Chat has no images in tool
results); `input_image` and `input_file` on Responses; `inlineData` on
Gemini.

Limits:

- Files over 20 MiB are refused.
- Images are fitted to 2000×2000 pixels and 5 MiB of base64 before they are
  sent. Larger ones are scaled down, and opaque screenshots are re-encoded as
  JPEG when that is smaller. An image that can't be decoded or fitted is
  dropped with a note to the model. The limits are the
  [`media`](configuration.md#media-limits) settings.
- Once a conversation's images pass 25 MiB, the oldest are replaced by a
  note until 15 MiB remain.

With no model catalog, CodeSmith assumes a model accepts images and PDFs
unless its config says otherwise. DeepSeek models (other than `-vl` ones) are
known to be text-only. For a text-only model, set its input modalities as in
OpenCode:

```jsonc
"models": { "qwen3-coder": { "modalities": { "input": ["text"], "output": ["text"] } } }
// ["text", "image"] for images without PDFs; "attachment": false also means text only
```

The model then gets `ERROR: Cannot read "shot.png" (this model does not
support image input). Inform the user.` in place of the file. The session
keeps the file, so switching to a vision model shows it again.
