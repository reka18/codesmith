# The interactive UI

Run `codesmith` in a project to open the terminal UI.

```sh
codesmith                          # new session
codesmith -c                       # continue the latest session in this folder
codesmith -s ses_01J...            # continue a specific session
codesmith --prompt "fix the failing test"   # send a first message right away
codesmith -m ollama/qwen3-coder --agent plan
codesmith -w                       # work in a new git worktree (see sessions.md)
codesmith --auto                   # approve every request no rule denies
```

See [CLI](cli.md#codesmith) for every flag.

## The screen

- **Transcript.** Replies render as Markdown: headings, bold and italic,
  inline code, links, lists and task lists, quotes, tables, and fenced code
  blocks with syntax highlighting. `edit` and `write` calls show a colored
  diff. Long tool output is collapsed to a few lines. A running shell
  command shows the last lines it printed (marked `│`) until its result
  replaces them. Images and files a tool returned, and images a model
  generates (Gemini image models), are listed by type and size (`▣`).
- **Header.** The agent, model, session id and title.
- **Status line.** Context in use (the last call's tokens, and the share of
  the model's `limit.context` when it is configured), the session's cost when
  the model has a `cost`, token totals, and how many
  [background commands](#background-commands) are running.

## Keys

### Editing the prompt

| Key | Action |
|---|---|
| **Enter** | Send. |
| **Shift+Enter**, **Alt+Enter**, **Ctrl+J** | New line. |
| **←/→**, **Ctrl+B/F** | Move by character. |
| **Ctrl/Alt+←/→**, **Alt+B/F** | Move by word. |
| **Home/End**, **Ctrl+A/E** | Start or end of the line. |
| **↑/↓** | Move between lines; at the first or last line, step through earlier messages. |
| **Backspace**, **Delete**, **Ctrl+D** | Delete a character. |
| **Ctrl+W**, **Alt+Backspace**, **Ctrl+Backspace** | Delete the previous word. |
| **Alt+D** | Delete the next word. |
| **Ctrl+K** | Delete to the end of the line. |
| **Ctrl+U** | Clear the prompt. |

Pastes go in at the cursor.

### Everything else

| Key | Action |
|---|---|
| **Tab** / **Shift+Tab** | Next or previous primary agent (`build`, `plan`, your own). |
| **Esc** | Interrupt the running turn; when idle, clear the prompt. |
| **Ctrl+C** | Interrupt a running turn; on an empty prompt, quit; otherwise clear the prompt. |
| **Ctrl+D** on an empty prompt | Quit. |
| **Ctrl+O** | Expand or collapse all tool output. |
| **Ctrl+T** | Show or hide the model's reasoning. |
| **PgUp/PgDn** | Scroll the transcript. |
| **Ctrl+End** | Follow the bottom again. |
| **Ctrl+X**, then a key | OpenCode's leader keys: **m** models, **a** agents, **l** sessions, **n** new session, **c** compact, **u** undo, **r** redo, **h** help, **b** move the running commands to the background. |

## Slash commands

| Command | What it does |
|---|---|
| `/new` | Start a new session. |
| `/sessions` | Pick a session in this folder to continue. |
| `/undo`, `/redo` | Undo or redo the latest turn's file changes. See [sessions](sessions.md#undo-and-redo). |
| `/agent [id]` | Switch agent, or pick one. |
| `/model [provider/model]` | Switch model, or pick one. |
| `/variant [name]` | Switch to the model's next [variant](configuration.md#provider-options-and-variants), or to the one named; `/variant default` drops it. |
| `/compact` | Summarize older history now. See [compaction](configuration.md#compaction). |
| `/clear` | Clear the screen. The session is kept. |
| `/thinking` | Show or hide reasoning. |
| `/help` | Keys and commands. |
| `/exit` (`/quit`, `/q`) | Quit. |
| `/init` | Write or improve `AGENTS.md` for the project. |
| `/calibrate`, `/review [target]` | [Code review](code-review.md). |
| `/<name> [args]` | Run a [custom command](skills-and-commands.md#commands) or an [MCP prompt](mcp.md#prompts) (`/server:prompt`). |

Commands that change the session wait until the current turn is finished.
Press **Esc** first to interrupt it.

## Pickers

`/model`, `/agent` and `/sessions` without arguments open a picker (and so do
**Ctrl+X m**, **a** and **l**). Type to filter. **↑/↓** or **Ctrl+P/N** move,
**PgUp/PgDn** jump, **Enter** chooses, and **Esc** closes. The model picker
lists the models configured under `providers`, plus the current one. To use
any other model, type `/model provider/model`.

## Autocomplete and `@` mentions

Typing `/` lists commands: built-in, custom, and MCP prompts. Typing `@`
lists project files, matched fuzzily and respecting `.gitignore`. **↑/↓**
choose, **Tab** completes, **Enter** runs the command or inserts the file,
and **Esc** closes the list.

Each `@path` in a message that names a file is attached to the message as a
`read` result, as in OpenCode, so the model sees it without a round trip. A
directory is attached as its listing, and an image or PDF as itself.

## Attaching images and PDFs

Paste the path of an image or PDF (dragging a file into most terminals
pastes its path). The path is replaced by an `[Image 1]` or `[PDF 1]`
placeholder, and the file is attached when you send the prompt. Delete the
placeholder to drop it. See [images and PDFs](tools.md#images-and-pdfs) for
the formats and limits.

## Background commands

A turn ends when the model answers, even while shell commands it started
with `background: true` still run; the status line shows `1 background
command running`. When one finishes while the UI is idle, the session wakes
up by itself: a `background command shell_1 finished (…)` notice appears and
the model carries on with the result as a new turn, which **Esc** interrupts
like any other.

A prompt you send before that goes first: the wait is cancelled and your
turn reports the finished command (or the next one does, if it is still
running), so each result reaches the model once. The commands keep running
when you interrupt a later turn, switch with `/agent`, `/model`, `/variant`
or **Tab**, and across `/undo` and `/redo`; the reloaded session reports
them. Leaving the session with `/new` or `/sessions` (for a different one)
stops them, and so does quitting. See
[background commands](sessions.md#background-commands).

## Permission and question dialogs

Permission requests and the agent's questions open as dialogs:

- **Permission:** **y** allows once, **a** always allows (for this session),
  and **n**, **Esc** or **Enter** reject. Type first, and Enter rejects with
  your text as feedback to the model. Edits and writes show a diff preview.
  See [answering a request](permissions.md#answering-a-request).
- **Question:** the `question` tool asks multiple-choice or free-form
  questions, one at a time. Type an option's number (several, separated by
  commas or spaces, when more than one may be picked) or your own answer, and
  press **Enter**. **Esc** dismisses the questions.
