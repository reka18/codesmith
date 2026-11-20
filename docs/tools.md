# Tools

These are the tools the model can call. Each is checked against
[permissions](permissions.md) before it runs. An agent sees only the tools
its rules don't deny outright (`codesmith debug agents` lists them).

| Tool | What it does | Offered when |
|---|---|---|
| `read` | Read a file (numbered lines, paged), an image or PDF (sent to the model as media), or list a directory. | always |
| `write` | Create or overwrite a file. | the model is not one that edits with `patch` |
| `edit` | Exact find-and-replace. It tolerates typographic quotes, dashes and trailing whitespace in the search text, and keeps the file's CRLF or LF line endings. | the model is not one that edits with `patch` |
| `patch` | Add, update, move and delete files with one patch. See [below](#patch). | GPT-5 and later models, instead of `write` and `edit` |
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
| `experimental_image_generate` | Make or edit an image with an image model, in the background. See [below](#generating-images-and-videos). | an image model is reachable and `experimental.image_generate` is not `false` |

[Plugins](plugins.md) can add tools of their own.

There is no terminal tool: as in OpenCode, the model runs commands with
`shell`, and the server's [terminals](local-server.md#terminals) are for the
people using a client, not the model.

Providers can also run tools of their own, such as web search or code
execution, when a model's `hostedTools` option turns them on
([config](configuration.md#hosted-tools)). Their calls show as tool rows but
never run locally.

Before a call runs, its input is repaired against the tool's input schema
where the schema leaves no doubt, as OpenCode does: an object or array sent
as a JSON string is parsed, `"20"` becomes `20` for a number and `"false"`
`false` for a boolean, a lone value becomes a one-item array, an optional
field sent as `null` is dropped, and extras are dropped from an object that
allows none. MCP tools called from Code Mode are repaired the same way. A
call to `bash` when no tool has that name runs as `shell`, and its result
tells the model to call `shell` next time.

Each agent has a step limit, 200 by default (`agents.<id>.steps`). On the
last step, the model has to answer in text.

## Shell

- Commands run with `<shell> -c`, where the shell is `shell` from config,
  else `$SHELL`, else the platform default (`/bin/zsh` on macOS, Bash
  elsewhere). Fish and Nushell, and a shell that cannot be found, fall back
  to that default. `$CODESMITH_SESSION_ID` is set.
- Each command in a shell line is checked separately, including wrapped
  commands and redirect targets. See [shell commands](permissions.md#shell-commands).
- The default timeout is two minutes. A command that outlives its timeout,
  or one started with `background: true`, keeps running in the background
  with its output streaming to a file, and the agent is told when it
  finishes.
- Large output is shown from its end, and the full output is saved to a file
  the model can read ([tool output](configuration.md#tool-output)).
- While a command runs, front ends see its shell ID and its output as it
  comes (the server's `tool_progress` and `tool_output`
  [events](local-server.md#turn-events)).
- You can move the commands a turn is waiting on to the background
  (**Ctrl+X b** in the [UI](interactive-ui.md#keys), or the server's
  [`background`](local-server.md#stopping-and-backgrounding-commands)
  route): each call returns at once, the model is told you moved the work,
  and the command is reported when it finishes. The server can also cancel
  one command. A background command's timeout stops it; a foreground
  command's moves it to the background instead.
- A session can have its own environment for its commands
  ([server](local-server.md#session-environment)); otherwise they inherit
  CodeSmith's.

## Files and paths

Relative paths resolve against the working directory. Paths outside the
project need `external_directory` approval first
([details](permissions.md#paths-outside-the-project)). CodeSmith's own
scratch folder (`<data>/tmp`) is always allowed, and the model is told about
it for temporary files.

After `write`, `edit` or `patch`, the optional [formatter](language-servers-and-formatters.md#formatters)
runs, and [language server](language-servers-and-formatters.md#language-servers)
errors are added to the result.

## `patch`

GPT models from GPT-5 on edit files with `patch`, the format OpenAI's
models are trained on, and are not offered `write` and `edit`; every other
model gets `write` and `edit` and not `patch`. As in OpenCode, a model
counts when its name in the config or its provider id contains `gpt-`, and
neither contains `gpt-4` or `oss` (so `gpt-4.1`, `gpt-4o` and `gpt-oss-*`
keep `write` and `edit`). There is no setting to swap them; `edit`
[permission](permissions.md) rules govern all three, so denying `edit`
hides `patch` too.

A patch looks like this:

```
*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch
```

- An update's `@@` line may name a line to find first (to pick the right
  place among similar ones); lines starting with a space are context, `-`
  removed, `+` added. `*** End of File` after a chunk anchors it to the
  end of the file.
- Context and removed lines match exactly first, then ignoring trailing
  whitespace, then surrounding whitespace, then typographic quotes, dashes
  and spaces. A BOM is kept, and the file ends with a newline.
- A heredoc wrapper (`cat <<'EOF'` … `EOF`) around the patch is ignored.
- Every hunk is checked against the files first, in order (a later hunk
  sees what earlier ones did), so a patch that does not apply changes
  nothing. One `edit` request then covers every file, and the permission
  dialog shows each file's diff. Paths outside the project are approved
  one by one before anything is read.
- The files are then written in order. If a write fails, the error names
  the files already changed (`Completed before failure: …`); they stay
  changed, and [undo](sessions.md) restores them with the rest of the turn.
- An `Add File` overwrites a file that exists. A file that is not valid
  UTF-8 can be deleted but not updated.
- When a file the patch reads is not plainly readable (a `read` rule asks
  or denies, as for `.env`), the `edit` request comes before the files are
  read, so a patch that does not match cannot reveal their contents.

## `webfetch`

Fetches over HTTP(S) and converts HTML to Markdown unless asked for text or
raw HTML. Each redirect target is checked against your `deny` rules, so a
denied host can't be reached through a redirect.

## Images and PDFs

PNG, JPEG, GIF, WebP and PDF files, and MP4, QuickTime (`.mov`) and WebM
video, are sent to the model natively. CodeSmith recognizes them by their
bytes, not their names. They arrive through:

- `read` on the file,
- `@path` mentions and pasted paths in the [UI](interactive-ui.md#attaching-images-and-pdfs),
- `codesmith run -f FILE`,
- `attachments` on the [local server](local-server.md#attachments), ACP
  prompts, and images, PDFs and videos that MCP tools return.

Each provider gets its native form: image and document blocks on Anthropic;
`image_url` and `file` parts on OpenAI Chat (where media from a tool result
follows the tool messages as a user message, since Chat has no images in tool
results); `input_image` and `input_file` on Responses; `inlineData` on
Gemini. Video goes as `video_url` on Chat, `input_video` on Responses and
`inlineData` on Gemini.

Limits:

- Files over 20 MiB are refused. Videos are sent as they are, never
  re-encoded.
- Images are fitted to 2000×2000 pixels and 5 MiB of base64 before they are
  sent. Larger ones are scaled down, and opaque screenshots are re-encoded as
  JPEG when that is smaller. An image that can't be decoded or fitted is
  dropped with a note to the model. The limits are the
  [`media`](configuration.md#media-limits) settings.
- Once a conversation's images pass 25 MiB, the oldest are replaced by a
  note until 15 MiB remain.

A model's input modalities come from its config, else from the
[model catalog](configuration.md#the-model-catalog-modelsdev). For a model
neither knows, CodeSmith assumes it accepts images and PDFs (DeepSeek models
other than `-vl` ones are taken as text-only). To mark a model text-only, set
its input modalities as in OpenCode:

```jsonc
"models": { "qwen3-coder": { "modalities": { "input": ["text"], "output": ["text"] } } }
// ["text", "image"] for images without PDFs; "attachment": false also means text only
```

The model then gets `ERROR: Cannot read "shot.png" (this model does not
support image input). Inform the user.` in place of the file. The session
keeps the file, so switching to a vision model shows it again.

Video is different: it goes only to a model whose input modalities list
`video`, since most models refuse it. Gemini models (`gemini-*`, other than
embedding, image, live and TTS ones) are known to take it; for any other
model that does, add `"video"` to its `modalities.input`. Other models get
a note in place of the video saying it can't be read.

## Generating images and videos

`experimental_image_generate` makes an image, or edits the images it is
given, with an image model from the
[model catalog](configuration.md#the-model-catalog-modelsdev) (`type:
image`) of a provider CodeSmith has a key for. OpenAI and OpenAI-compatible
image APIs, OpenRouter, xAI and Google's Gemini image models are supported.
The model is, in order: the one the call names
(`provider/model#variant`), `experimental.image_generate.model` in config,
the newest image model of the session's provider, then the newest one
anywhere; with input images, only models that take images count. A
configured model that can't serve a call is skipped, and the result says so.

```jsonc
"experimental": { "image_generate": { "model": "openai/gpt-image-1" } }  // or false to turn it off
```

A call asks for `experimental_image_generate` permission (on the model),
`edit` on the path it saves to, `read` on input files and `webfetch` on
input URLs, then starts a background job and returns. Input images are PNG,
JPEG, WebP or GIF, at most 20 MiB each. When the image is ready it is saved
with the extension of its real format (to `path`, or the session's temp
folder) and the session is told, like a [background
command](sessions.md#background-commands); `codesmith run` waits for it.
Generated images are at most 20 MiB and are kept in the session's [blob
store](configuration.md#media-limits) too. Cancelling the job (as a
background command is cancelled) stops it.
