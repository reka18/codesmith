# CodeSmith

A terminal coding agent, written in Rust. CodeSmith is a port of
[OpenCode](https://github.com/sst/opencode) aiming for feature parity with its
terminal agent. It is fully local: no hosted services, no accounts, no
telemetry. Point it at any model provider you like, including one running on
your own machines.

> **Status: early.** Feature-complete against OpenCode's terminal agent for
> the roadmap below, and pinned to OpenCode's behaviour by ported tests and
> replays of its recorded provider traffic, but young: expect rough edges.

## Install

Prebuilt binaries for macOS (Apple Silicon and Intel) and Linux (x86_64 and
arm64) are attached to each [GitHub Release](https://github.com/reka18/codesmith/releases)
(none are published yet; until then, build from source below). Releases are
published only after the macOS binaries are signed with a Developer ID and
notarized by Apple; the Linux binaries are statically linked (musl) and run on
any distribution.

**Install script** (macOS and Linux). Downloads the release tarball for your
platform, checks it against the release's `SHA256SUMS`, and puts `codesmith`
in `~/.local/bin`. It sends nothing anywhere else.

```sh
curl -fsSL https://raw.githubusercontent.com/reka18/codesmith/main/install.sh | sh
```

Set `CODESMITH_VERSION=0.1.0` to pick a version and `CODESMITH_INSTALL_DIR` to
install somewhere else. Read the script first if you prefer:
[install.sh](install.sh).

**Homebrew**

```sh
brew install reka18/tap/codesmith
```

**Manually**: download `codesmith-<version>-<target>.tar.gz` and `SHA256SUMS`
from the release, check with `shasum -a 256 -c SHA256SUMS --ignore-missing`,
unpack, and put `codesmith` on your `PATH`.

**From source** (Rust 1.85 or later):

```sh
cargo install --locked --git https://github.com/reka18/codesmith codesmith
# or, from a checkout
cargo install --locked --path crates/codesmith
```

Maintainers: see [RELEASING.md](RELEASING.md).

## Use

Run `codesmith` in a project for the interactive UI (`-c` continues the latest
session here, `-s ID` a specific one, `--prompt "..."` sends a first message):

- **Editing**: a real cursor. **Enter** sends; **Shift+Enter**, **Alt+Enter**
  or **Ctrl+J** adds a line. **←/→**, **Home/End** (**Ctrl+A/E**), and
  **Ctrl/Alt+←/→** (**Alt+B/F**) move by character, line and word;
  **Ctrl+W**/**Alt+Backspace** delete a word, **Ctrl+K** to the end of the
  line, **Ctrl+U** the whole prompt. **↑/↓** move between lines and, at the
  first or last line, through earlier messages. Pastes go in at the cursor.
- **Autocomplete**: typing `/` lists commands (built-in, custom and MCP
  prompts) and `@` lists project files (fuzzy, respecting `.gitignore`).
  **↑/↓** choose, **Tab** completes, **Enter** runs the command or attaches
  the file, **Esc** closes the list. Like OpenCode, each `@path` that names a
  file is attached to the message as a Read tool result (a directory as its
  listing, an image or PDF as itself), so the model sees it without a round
  trip. Pasting the path of an image or PDF, as dragging a file into the
  terminal does, inserts an `[Image 1]` or `[PDF 1]` placeholder; the file is
  attached when you send the prompt (delete the placeholder to drop it).
- **Transcript**: replies render as markdown: headings, bold/italic,
  inline code, links, lists and task lists, quotes, tables, and fenced code
  blocks with syntax highlighting. `edit` and `write` calls show a colored
  diff of the change; long tool output is collapsed to a few lines, and
  **Ctrl+O** expands it all. **Ctrl+T** shows reasoning; **PgUp/PgDn**
  scroll, **Ctrl+End** follows the bottom again.
- **Agents and models**: **Tab** / **Shift+Tab** cycle the primary agents
  (build, plan, …). `/model` and `/agent` without arguments, and `/sessions`,
  open pickers you can type into to filter. OpenCode's leader keys work too:
  **Ctrl+X** then **m** models, **a** agents, **l** sessions, **n** new
  session, **c** compact, **u**/**r** undo/redo, **h** help.
- **Status line**: context in use (the last call's tokens, and the share of
  the model's `limit.context` when configured), the session's cost when the
  model has a `cost`, and token totals.
- **Prompts**: permission requests and questions open as dialogs; an edit or
  write request previews its diff. In a permission dialog **y** allows once,
  **a** always allows, and **n**, **Esc** or **Enter** rejects (typing
  first turns Enter into "reject with this feedback"); allowing always takes
  a deliberate key. The request is shown with control characters escaped
  (`\x1b`, `\u{202e}`) and long runs of spaces or blank lines counted
  (`␣×700`), and a request too long to fit says how much is hidden and
  scrolls with **↑/↓**/**PgUp/PgDn**.
- **Esc** interrupts a running turn; **Ctrl+C** on an empty prompt quits
- `/new`, `/sessions`, `/undo`, `/redo`, `/agent`, `/model`, `/compact`,
  `/clear`, `/thinking`, `/help`, `/exit`, plus every custom and built-in
  command

For scripts and one-shot use, `codesmith run`:

```sh
codesmith run "explain what src/main.rs does"
codesmith run -m anthropic/claude-sonnet-4-5 "add a --verbose flag"
git diff | codesmith run "review this change"
codesmith run --agent plan "how should I restructure the config module?"
```

| Flag | Meaning |
|---|---|
| `-m, --model provider/model` | Model to use (otherwise `model` from config) |
| `--agent build\|plan\|<custom>` | `build` (default) can edit; `plan` is read-only. `general` and `explore` are subagents the model can delegate to |
| `-f, --file PATH` | Attach a file (repeatable): text goes into the message, images and PDFs are sent for the model to see |
| `--format json` | One JSON object per event, for scripting |
| `--auto` | Approve every permission request that no rule denies |
| `--thinking` | Show the model's reasoning |
| `-c, --continue` | Continue the most recent session in this directory |
| `-s, --session ID` | Continue a specific session |
| `--title TEXT` | Title a new session (otherwise one is generated) |

Without `--auto`, CodeSmith asks on the terminal before any action a rule
marks `ask`: `y` allow once, `a` always allow, `n` reject, or type feedback
for the model to reject with it. A bare Enter rejects (`[y/a/v/N]`). The
request is escaped the same way as in the UI, and one over 2000 characters
is shortened; `v` prints it in full.

## Skills and commands

**Skills** are folders with a `SKILL.md` (a `name`/`description` header and
instructions). The model sees the list and loads one with the `skill` tool when
a task matches. CodeSmith looks in `~/.config/codesmith/skills/`,
`~/.agents/skills/`, `~/.claude/skills/`, and in each project folder's
`.codesmith/skills/`, `.claude/skills/` and `.agents/skills/`.

**Commands** are prompt templates run as `codesmith run /name args`. Put them
in `.codesmith/commands/<name>.md` (or `~/.config/codesmith/commands/`, or
`commands` in config), optionally with `description`, `agent` and `model` in a
header. `$ARGUMENTS` is the whole argument string and `$1`…`$9` single
arguments. Built in: `/init` (write or improve `AGENTS.md`), `/calibrate` and
`/review` (see [Code review](#code-review)). `codesmith commands`
lists them.

```markdown
---
description: Explain a file in one sentence
---
Read $1 and explain what it does in exactly one sentence.
```

## Sessions

Every run is saved to a local SQLite database
(`~/.local/share/codesmith/codesmith.db`, or `$CODESMITH_DB`). New sessions
get a short generated title. When a conversation nears the model's context
window (`limit.context` in config), older history is replaced by a structured
summary and the newest exchanges are kept verbatim, so long sessions keep
going.

```sh
codesmith session list [--all] [--json]
codesmith session export <id> > session.json
codesmith session import session.json
codesmith session delete <id>
codesmith session diff  [id]   # file changes since the latest turn began
codesmith session undo  [id]   # restore the files and hide the latest turn
codesmith session redo  [id]   # bring it back (until a new turn is added)
```

Undo works by snapshotting the project at the start and end of each turn into
a private git repository under CodeSmith's data directory, and restores only
the files the turn changed, so your own later edits stay. It never touches your
own `.git`, works in folders that are not git repositories, and respects
`.gitignore`. Set `"snapshots": false` to turn it off.

Other commands: `codesmith models`, `codesmith debug paths|config|model|agents|prompt`.

## Tools

| Tool | What it does |
|---|---|
| `read` | Read a file (numbered lines, paged), an image or PDF (sent to the model as media, up to 20 MiB), or list a directory |
| `write` | Create or overwrite a file |
| `edit` | Exact find-and-replace, tolerant of typographic quotes, dashes and trailing whitespace; preserves CRLF/LF line endings |
| `glob` | Find files by glob pattern, respecting `.gitignore` |
| `grep` | Search file contents with ripgrep's regex engine |
| `shell` | Run a command; long-running ones move to the background and the agent is told when they finish. Large output is saved to a file |
| `webfetch` | Fetch a URL as markdown, text or HTML; redirect targets are checked against deny rules |
| `todowrite` | Keep a visible todo list for multi-step work |
| `subagent` | Delegate a task to a subagent (`general`, `explore`, or your own) in a child session. Subagents cannot start subagents unless `"subagent_depth"` allows more levels |
| `skill` | Load a skill's instructions when a task matches its description |
| `websearch` | Search the web through your own [SearXNG](https://docs.searxng.org/) instance (only offered when `websearch.url` is set) |
| `question` | Ask you multiple-choice or free-form questions on the terminal (not offered with `--auto` or without a terminal) |
| `lsp` | Definitions, references, hover, symbols and call hierarchy from your language servers (only offered when `lsp` is on) |

## Configuration

codesmith reads `codesmith.json` or `codesmith.jsonc` (comments and trailing
commas allowed), lowest to highest precedence:

1. `~/.config/codesmith/codesmith.json` (global)
2. the file named by `$CODESMITH_CONFIG`
3. from the project root (nearest `.git`) down to the working directory, each
   folder's `codesmith.json`
4. then, in the same order, each folder's `.codesmith/codesmith.json`
5. inline JSON in `$CODESMITH_CONFIG_CONTENT`

`{env:VAR}` and `{file:path}` are substituted anywhere in the text (not on `//`
comment lines). A variable goes in as is, so `"snapshots": {env:FLAG}` works,
unless that breaks the JSON, in which case it is escaped as string content;
file contents are trimmed and always escaped.

> **Project config is trusted, as in OpenCode.** A `codesmith.json` or
> `.codesmith/` folder inside a repository is applied like your own config:
> it can start MCP servers (which run programs), choose the shell, turn on
> the repository's own formatters and language servers, point a provider at
> any URL (and send it values from `{env:}` or `{file:}`), and add permission
> rules that come after — and so override — your global ones. Before running
> CodeSmith in a repository you don't trust, read its `codesmith.json[c]`
> and `.codesmith/` first.

**Compaction** starts when the prompt nears the model's `limit.context`,
leaving 15% (at least 16k tokens) free, and keeps about 15k tokens of the
newest turns verbatim beside the summary:

```jsonc
{ "compaction": { "auto": true, "buffer": "20%", "keep": { "tokens": 15000 } } }
```

`buffer` is tokens or a share of the window; `"auto": false` leaves
compaction to `/compact`, even when the provider rejects a prompt as too long.

```jsonc
{
  "model": "local/qwen3-coder",
  "providers": {
    "local": {
      "protocol": "openai-chat",            // or openai-responses, anthropic-messages, google-gemini
      "settings": { "baseURL": "http://127.0.0.1:8000/v1", "apiKey": "{env:LOCAL_API_KEY}" },
      "body": { "chat_template_kwargs": { "enable_thinking": false } },
      "models": { "qwen3-coder": { "limit": { "context": 32768, "output": 8192 } } }
    }
  },
  "permissions": [
    { "action": "shell", "resource": "rm *", "effect": "ask" },
    { "action": "shell", "resource": "git push *", "effect": "deny" }
  ],
  "agents": {
    "reviewer": { "system": "You review code. Never edit files.", "permissions": [{ "action": "edit", "resource": "*", "effect": "deny" }] }
  },
  "instructions": ["docs/STYLE.md"]
}
```

**Web search** uses a self-hosted SearXNG instance, never a hosted search
API. Enable the JSON format in its `settings.yml` (`search: formats: [html,
json]`) and point CodeSmith at it:

```jsonc
{ "websearch": { "url": "http://localhost:8888", "results": 8 } }
```

`anthropic` (`ANTHROPIC_API_KEY`) and `openai` (`OPENAI_API_KEY`) work with no
config. Any OpenAI-compatible server (oMLX, vLLM, llama.cpp, Ollama, LM Studio,
OpenRouter, …) works as a provider with `"protocol": "openai-chat"`.

**Reasoning effort and other provider options** go in a provider's or
model's `options` (OpenCode's provider options, camelCase or snake_case):
`reasoningEffort`, `reasoningSummary`, `textVerbosity`, `serviceTier`,
`include` (Responses), `thinking` and `effort` (Anthropic), `thinkingConfig`
(Gemini), `reasoning` (OpenRouter) and more; each protocol sends the ones it
knows. Named `variants` are picked with `provider/model#name` in `-m`, config,
`/model` or ACP, and `/variant` cycles them in the TUI. Common models also
have built-in variants (`anthropic/claude-sonnet-4-5#high`,
`openai/gpt-5#low`, `google/gemini-3-pro-preview#high`):

```jsonc
"models": { "gpt-5": {
  "options": { "textVerbosity": "low" },
  "variants": { "deep": { "reasoningEffort": "high" }, "fast": { "serviceTier": "priority" } }
} }
```

Reasoning is replayed with what the provider needs to accept it back
(Anthropic thinking signatures and redacted thinking, Responses encrypted
reasoning, Gemini thought signatures), so thinking models keep their
reasoning across tool calls.

**Permissions** are an ordered list of `{action, resource, effect}`; the last
matching rule wins and anything unmatched asks. Actions are tool names, with
`write` and `edit` sharing `edit`, plus `external_directory` for paths outside
the project. `*` matches anything; a trailing ` *` also matches the bare
command (`git *` matches `git`). By default everything is allowed except
reading `.env` files, which asks.

Shell commands are split into the commands they run, and each is checked on
its own: `git status && rm -rf build` is checked as `git status` and
`rm -rf build`, and so are commands inside `$(...)`, backticks, pipelines,
subshells, loops, conditionals and functions. Redirects stay part of the
command (`cat > out`), so a rule like `* > secret*` catches them. `cd`
targets outside the project need `external_directory` approval. A command
the scanner cannot analyze is refused rather than run unchecked. "Always
allow" saves the command's leading words (`git status *`, `npm run build *`).

**Instructions**: `~/.config/codesmith/AGENTS.md` (else
`~/.claude/CLAUDE.md`), then `AGENTS.md` files from the working directory up,
nearest first, to the project root, or to your home folder when the project
is inside it (with `CLAUDE.md` as a fallback in each folder), then anything
listed under `instructions`: paths or globs, `~/` for home, relative ones
looked for from the working directory up to the project root.

## Stats

`codesmith stats` summarizes your usage from the local session store: an
activity calendar, sessions, prompts, steps, tokens, tool success, active days
and your best streak. `--days N`, `--year Y` or `--all` pick the range (this
year by default), `--project .` limits it to the current project, and
`--models`, `--tools`, `--cost` or `--full` add per-model, tool reliability
(error rate, median duration) and cost tables. `--json` prints the numbers.

Cost needs prices, since nothing is fetched from a catalog: give a model a
`"cost"` in dollars per million tokens.

```jsonc
"models": { "claude-x": { "cost": { "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75 } } }
```

## Images and PDFs

`read` on a PNG, JPEG, GIF, WebP or PDF (recognized by its bytes, not its
name), `@path` mentions of one, pasted paths and `run -f` send the file to the
model natively: image and document blocks on Anthropic, `image_url` and
`file` parts on OpenAI Chat (tool-result media follows the tool messages as a
user message, since Chat has no images in tool results), `input_image` and
`input_file` on Responses, `inlineData` on Gemini. Files over 20 MiB are
refused. Images are fitted to 2000x2000 and 5 MiB of base64 before they are
sent: larger ones are scaled down (and opaque screenshots re-encoded as JPEG
when that is smaller); one that cannot be decoded or fitted is dropped with a
note to the model. The limits are configurable:

```jsonc
"media": { "image": { "auto_resize": true, "max_width": 2000, "max_height": 2000, "max_base64_bytes": 5242880 } }
```

With no model catalog, CodeSmith assumes a model can see images and PDFs
unless its config says otherwise (DeepSeek models are known to be text-only).
For a model that cannot, give its input modalities as in OpenCode; the model
then gets `ERROR: Cannot read "shot.png" (this model does not support image
input). Inform the user.` in place of the file, while the session keeps the
file, so switching to a vision model shows it again:

```jsonc
"models": { "qwen3-coder": { "modalities": { "input": ["text"], "output": ["text"] } } }
// or ["text", "image"] for images without PDFs; "attachment": false also means text only
```

Once a conversation's images pass 25 MiB, the oldest are replaced by a note
until 15 MiB remain.

## Worktrees

`codesmith -w` (or `codesmith run -w "..."`) starts the session in a new git
worktree, so the agent works on a separate checkout while yours stays as it
is; `-w name` names it, otherwise it gets a name like `swift-otter`.
Worktrees go under the data dir's `worktree/<project>/` (or
`"worktree": {"directory": "..."}`), start detached at `HEAD`, and keep you in
the same subfolder you started from. CodeSmith config that git does not track
(an uncommitted `codesmith.json`, `.codesmith/`) is copied in, and
`"worktree": {"start": "npm ci"}` runs a setup command in each new one with
`$CODESMITH_WORKTREE_BASE` and `$CODESMITH_WORKTREE_PATH` set.

`codesmith worktree list`, `worktree create [name] [--from REF] [-b BRANCH]`
and `worktree remove <name|path> [--force]` manage them.

## Formatters

With `"formatter": true`, files the agent writes or edits are formatted
afterwards, as in OpenCode. The 26 built-in formatters each turn on only when
the project uses them: `gofmt`, `rustfmt`, `zig`, `shfmt`, `terraform` and
the like when installed; `prettier`, `biome` and `oxfmt` when the nearest
`package.json` (or `biome.json`) asks for them and they are installed in
`node_modules`; `ruff` when the Python config mentions it; `clang-format` and
`ocamlformat` when their config file exists. Detection starts at the edited
file's folder, so each package of a monorepo uses its own setup. When a
formatter changes the file, the tool result tells the model to re-read it.

```jsonc
"formatter": {
  "rustfmt": { "disabled": true },
  "prettier": { "environment": { "NODE_OPTIONS": "--max-old-space-size=4096" } },
  "sqlfmt": { "command": ["sqlfmt", "$FILE"], "extensions": [".sql"] }
}
```

`"formatter": false` (or leaving it out) turns formatting off.

## Language servers

With `"lsp": true`, the agent hears from your language servers after it
writes or edits a file, as in OpenCode: the file is sent to the servers for
its type (`didOpen`, then `didChange`), and the errors they publish within
three seconds are added to the tool result, up to 20 per file, so the model
fixes them in the next step. `write` also lists errors in up to five other
files; warnings and hints are left out. It is off by default, as in
OpenCode, since servers use memory and can lag behind the files.

The built-in servers follow OpenCode's list (`rust-analyzer`, `gopls`,
`clangd`, `pyright`, `typescript-language-server`, `deno`, `zls`,
`sourcekit-lsp`, `jdtls`, `lua-language-server`, `bash-language-server` and
about 25 more), but none is ever downloaded: each is used only when it is on
`PATH`, or in the project's `node_modules/.bin` for JavaScript tooling (the
TypeScript server also needs the project's own `typescript`). A server
starts the first time a matching file is touched, in the nearest folder with
its project file (`Cargo.toml` widened to the Cargo workspace, `go.mod`,
`pyproject.toml`, a lockfile and so on); one that fails to start is not
retried, and an edit waits at most five seconds for one still starting.

```jsonc
"lsp": {
  "typescript": { "disabled": true },
  "rust": { "command": ["rust-analyzer"], "env": { "RUST_LOG": "error" } },
  "custom": {
    "command": ["custom-lsp-server", "--stdio"],
    "extensions": [".custom"],
    "initialization": { "preferences": { "strict": true } }
  }
}
```

An object keeps the built-ins on; `"lsp": false` (or leaving it out) turns
language servers off. `initialization` is sent as `initializationOptions` and
returned for `workspace/configuration`. With language servers on, the agent
also gets an `lsp` tool for `goToDefinition`, `findReferences`, `hover`,
`documentSymbol`, `workspaceSymbol`, `goToImplementation` and call
hierarchies (permission action `lsp`).

## Code review

Reviews are tuned to the codebase in two steps.

**`/calibrate`** (or `codesmith calibrate`) studies the whole repository and
chooses its review dimensions. CodeSmith gathers facts from git itself — the
file mix, how files group into areas, the most-changed files, and which files
and commits bug fixes have touched — then surveys each area with a read-only
agent in parallel, and has a final agent turn the facts and surveys into 4–12
dimensions specific to this code: its invariants, cross-module contracts,
formats, concurrency, security boundaries and the kinds of bugs its history
shows. Each dimension has the paths it covers, concrete checks and the
evidence behind it. They are saved to `.codesmith/review.json`; edit it as you
like and commit it so everyone reviews the same way.

**`/review [target]`** (or `codesmith review`) reviews uncommitted changes by
default, or a commit, a range (`a..b`), a branch, or a pull request (`#12`,
needs `gh`). It runs one independent reviewer per dimension that covers the
changed files, plus two that always run — correctness, and compliance with
`AGENTS.md`/`CLAUDE.md` — in parallel and read-only. Reviewers report only
high-signal issues they can show are real. Findings that describe the same
problem are merged, then every finding goes to its own verifier, which tries
to refute it; only findings confirmed at 80% confidence or more are reported,
most severe first. The report joins the session, so "fix 1" works next.
`--comment` posts it to the pull request.

```jsonc
"review": { "model": "provider/model", "concurrency": 3, "threshold": 80, "max_diff_chars": 60000 }
```

The method builds on Anthropic's
[code-review plugin](https://github.com/anthropics/claude-plugins-official/tree/main/plugins/code-review)
(Apache License 2.0); the adapted prompts, the license and a note of the
changes are in [`crates/codesmith-core/src/prompt/review/`](crates/codesmith-core/src/prompt/review/).

## Providers

Built in, with the key read from the usual environment variable:
`anthropic`, `openai` (Responses API), `google` (Gemini), `openrouter`, `groq`,
`deepseek`, `mistral`, `xai`, `together`, `fireworks`, `cerebras`, and local
servers that need no key: `ollama`, `lmstudio`, `llamacpp`. Any
OpenAI-compatible server can be added under `providers` as above.

- `codesmith models` lists providers and configured models;
  `codesmith models <provider>` asks that provider's API for its models.
- `codesmith auth login <provider>` saves a key on this machine (in the data
  dir's `auth.json`, owner-only); `auth list` shows where each provider's key
  comes from and `auth logout` removes one. Config and environment variables
  take precedence over saved keys.

## MCP servers

Tools from [Model Context Protocol](https://modelcontextprotocol.io) servers
appear to the model as `<server>_<tool>`, in OpenCode's config shape:

```json
{
  "mcp": {
    "servers": {
      "fs": { "type": "local", "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."] },
      "docs": { "type": "remote", "url": "http://127.0.0.1:8080/mcp", "headers": { "Authorization": "Bearer {env:DOCS_TOKEN}" } }
    }
  }
}
```

Local servers run over stdio (their stderr goes to the data dir's
`log/mcp-<name>.log`); remote servers use streamable HTTP. Each server can set
`disabled`, `environment`, `cwd` and `timeout` (`startup`, `catalog`,
`execution`, in milliseconds). Calls go through permissions like any other
tool, keyed by the tool's name: `{"action": "github_*", "resource": "*", "effect": "deny"}` hides a
server's tools from the model.
`codesmith mcp list` shows each server's status and tools. Images a tool
returns (and embedded image or PDF resources) reach the model as images, as
`read`'s do, fitted to the same limits; in Code Mode, whose results are
JSON for the program, they are replaced by a note.

**Code Mode.** As in OpenCode, MCP tools are offered through Code Mode by
default: instead of calling them one by one, the model writes a short
JavaScript program in a single `execute` call —
`const issues = await tools.github.list_issues({repo}); return issues.filter(...)`
— and gets back the result and any `console.log` output. The tools are listed
with TypeScript-style signatures in the system prompt. Programs run in
[Boa](https://boajs.dev), a JavaScript engine written in Rust, with no
filesystem, network, process or timer access of their own: the only way out
is the listed tools, each call permission-checked like a direct one, capped at
50 calls, three minutes, and bounded loops and recursion. Each program runs
in a child `codesmith` process that is killed at the timeout, with a CPU-time
limit, a 512 MiB resident-memory limit (plus an address-space limit on
Linux), and single buffers, typed arrays and repeated strings capped so that
`new ArrayBuffer(2**30)` fails with a RangeError. (A program embedding
`codesmith-core` without calling `codemode::child_main_if_requested` runs
programs on a thread instead, where only the loop, recursion and size caps
apply.) Set
`"codemode": false` on a server (or at the top level of the config) to offer
its tools as direct calls instead, which suits models that write JavaScript
poorly.

Remote servers that require OAuth show as needing authorization;
`codesmith mcp auth <name>` logs in through the browser (discovery, dynamic
client registration and PKCE per the MCP authorization spec, with the
callback on `127.0.0.1:19876`). Tokens are saved in the data dir's
`mcp-auth.json` (owner-only), only ever sent to the URL they were issued for,
and refreshed automatically; `codesmith mcp logout <name>` forgets them. Set
`"oauth": {"clientId": "...", "clientSecret": "...", "scope": "...", "port": 19876}`
to use a pre-registered client, or `"oauth": false` to turn it off.

**Beyond tools**, following OpenCode:

- A server's `instructions` (from its `initialize` reply) join the system
  prompt in an `<mcp_instructions>` block, but only while the session can
  reach at least one of its tools.
- Resources are read through two tools, `list_mcp_resources` (resources and
  URI templates, from one server or all) and `read_mcp_resource` (one URI),
  offered when a connected server has resources. Both are permission-checked:
  listing by server name, reading by `server:uri`, so
  `{"action": "read_mcp_resource", "resource": "docs:secret://*", "effect": "deny"}`
  keeps a server's secrets out.
- Prompts become slash commands named `/<server>:<prompt>`. Arguments fill the
  prompt's declared arguments in order (quote one with spaces); `codesmith
  commands` lists them with their argument names.
- `tools/list_changed` and `prompts/list_changed` notifications refresh the
  lists before the next model request, and `notifications/message` log lines
  go to the server's log file. Over HTTP, notifications and server requests
  inside a response's SSE stream are handled too.

## Automations

Automations run a prompt — or a Code Mode program — later or on a schedule,
without anyone at the keyboard, as in OpenCode. Ask the agent ("check the
build every 10 minutes", "remind me at 17:00 to …"; it has an `automation`
tool) or use the CLI:

```sh
codesmith automation add --name standup --cron "0 9 * * 1-5" --tz Europe/Paris \
  --prompt "Summarize yesterday's commits in three bullets"
codesmith automation add --name tidy --every "1 day" --agent build --prompt "…"
codesmith automation add --name once --at "in 30 minutes" --session ses_… --prompt "…"
codesmith automation add --name tickets --event tracker:ticket.created \
  --event-args '{"queue": "ops"}' --prompt "Triage the new ticket"
codesmith automation list | pause ID | resume ID | remove ID | log ID
```

Triggers: `--at` (once: "in 30 minutes", "2026-10-27T09:00", or an RFC 3339
time), `--every` ("10 minutes", "2 hours", "1 day") and `--cron` (five
fields, in `--tz` or local time). `--event server:event` runs once for each
event an MCP server pushes, for servers that offer MCP Events (the draft
`io.modelcontextprotocol/events` extension, as in OpenCode): the server must
be connected and advertise the event for push delivery, and `--event-args`
must match its input schema. Each run's prompt carries the event
(`{id, timestamp, data}`) as quoted, untrusted data; a program gets it as
`input.event`. The watch saves the server's cursor after every message, so
pausing and resuming (or a restart) picks up where it left off without
running an event twice; a stream that breaks is reopened up to three times.
A watch that cannot go on — a refused or ended subscription, a server that
cannot replay from the saved cursor, an event that does not match its
schema, a failed run, or a `watch_mcp_event` rule that denies the server —
is paused with the reason in `automation list`. A prompt goes to an existing session
(`--session`) or to a new session with an agent on each run. A Code Mode
program (`--code-file`) gets `input = {state, now, runs, args}` and may
return `{state, done, send: {prompt, to: {subagent}}}` to keep state between
runs, finish itself, or hand work to a session.

Automations are stored with your sessions and run by whichever CodeSmith is
open — the UI, `serve` or `acp` (`"automations": false` opts a process out) —
or by `codesmith automation serve`, which you can keep running under launchd
or systemd; a lease in the database keeps two processes from firing the same
run. Each run's outcome, session and answer are kept for `automation log`.

Automations are a way for a prompt-injected model to come back later, so they
are kept on a short leash:

- When the agent uses the `automation` tool, creating an automation (and
  watching an MCP server's events) asks you first, even though agents allow
  everything else by default; only a rule that names the action itself
  (`{"action": "automation", "resource": "*", "effect": "allow"}`, or
  `watch_mcp_event` with the server name) skips the question.
- A job the agent creates runs as that session's own agent or as a read-only
  subagent (`explore`, `review`): the plan agent cannot schedule work for
  build. Use `codesmith automation add --agent …` to pick any agent yourself.
- Runs are unattended. They follow your permission rules, anything that
  would ask is refused, and on top of that `shell`, `edit` (and `write`),
  `webfetch` and `external_directory` are denied, since a run may act on
  untrusted input such as an MCP event. Opt back in with rules under
  `automation.permissions`, which apply only to unattended runs, after those
  denies:

  ```json
  {
    "automation": {
      "permissions": [
        {"action": "shell", "resource": "cargo test*", "effect": "allow"},
        {"action": "edit", "resource": "CHANGELOG.md", "effect": "allow"}
      ]
    }
  }
  ```

  `{"action": "shell", "resource": "*", "effect": "allow"}` gives runs the
  whole shell back.

## Plugins

Plugins are native Rust, compiled into your own build of CodeSmith — no
scripting runtime, no downloads. A plugin implements
`codesmith::plugin::Plugin`, overriding only the hooks it needs, and a
three-line `main` builds a CodeSmith that includes it:

```rust
fn main() {
    codesmith::main(vec![std::sync::Arc::new(MyPlugin)]);
}
```

The hooks follow OpenCode's plugin API:

| Hook | What it can do |
| --- | --- |
| `config` | edit the merged config: add or change agents, commands, providers, MCP servers, formatters, permissions; receives the plugin's options |
| `tools` | add tools (permission-checked by name like any other) |
| `tool_before` / `tool_after` | change a tool call's input or refuse it; change what the model sees |
| `shell_before` | change a shell command or its environment |
| `request` | change the system prompt, messages, tools or body just before a model request |
| `permission` | answer a permission prompt instead of the user |
| `event` | observe turns, steps and tool calls |
| `idle` | continue a finished turn with another message (at most 5 times) |

Config enables plugins by id: `"plugins": {"my-plugin": {...options}}`, or
`false` to turn one off. `codesmith debug plugins` lists what is compiled in.
[`examples/plugin-example`](examples/plugin-example/src/main.rs) is a complete
one (a `todo_scan` tool, a `/standup` command and a force-push guard):
`cargo run -p codesmith-plugin-example -- debug plugins`.

## Editors (ACP)

`codesmith acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com)
on stdin/stdout, so editors that support ACP agents (such as Zed) can run
CodeSmith sessions. In Zed's settings:

```json
"agent_servers": { "CodeSmith": { "command": "codesmith", "args": ["acp"] } }
```

Sessions stream text, reasoning (with message ids), tool calls and plans
(from the todo list); edits and writes carry full-file diffs, also in their
permission requests, which appear in the editor. Agents (build, plan, …) are
modes, and the model and mode are config options the editor can switch.
Turns end with a stop reason (`end_turn`, `max_tokens`, `refusal`,
`max_turn_requests`, `cancelled`) and token usage, followed by the context
window use and cost when the model's limit (and price) is configured.
Slash commands, including `/review`, `/calibrate` and `/compact`, are
offered. Sessions can be listed, loaded (with their whole history
replayed), resumed, forked, cancelled, closed and deleted, and can be given
additional directories the agent may use without asking. When the editor
supports form elicitation, the agent can ask questions with the `question`
tool. MCP servers the editor passes in are added to the session (stdio and
HTTP). Sessions are the same ones the terminal UI and `codesmith run` use.

Prompts can carry images (the agent advertises `promptCapabilities.image`)
and embedded resources: text resources join the message, and image or PDF
blobs (recognized by their bytes) are sent to the model as attachments,
fitted to the `media` limits; an image that cannot be fitted refuses the
prompt. Images given only by URI and resource links are referenced in the
message for the agent to `read`. Images a tool returns appear in its
`tool_call_update` content, and a loaded session replays the prompt's
images too.

## Local server

`codesmith serve` exposes sessions to other programs on this machine (an
editor, a script) over HTTP on `127.0.0.1:4096`. It is opt-in and local.
Every request needs `Authorization: Bearer <token>`. The token is `--token`
(or `$CODESMITH_SERVER_TOKEN`); without one, the server uses the token in the
data dir's `serve-token` file (owner-only, created with a random token on
first use and printed at startup), so local clients can read it from there:

```sh
curl -H "Authorization: Bearer $(cat ~/.local/share/codesmith/serve-token)" \
  http://127.0.0.1:4096/health
```

Listening on anything but loopback requires an explicit `--token`. Because
web pages can reach loopback too, the server also refuses requests whose
`Host` isn't the address it listens on (`localhost`, `127.0.0.1` or `[::1]`
with its port, on loopback), requests with any `Origin` but its own, request
bodies that aren't `application/json`, and bodies over 8 MiB. Only sessions
of the directory it was started in are visible; permission and question ids
are random. `--auto` approves permission requests instead of asking the
client.

| Route | |
| --- | --- |
| `GET /health` | version, directory, model |
| `GET /sessions` · `POST /sessions {agent?, model?}` | list, create |
| `GET /sessions/{id}` · `DELETE /sessions/{id}` | info and messages, delete |
| `POST /sessions/{id}/messages {text, attachments?}` | run a turn; streams Server-Sent Events |
| `POST /sessions/{id}/abort` | stop the running turn |
| `POST /sessions/{id}/permissions/{rid} {reply, message?}` | answer `once`, `always` or `reject` |
| `POST /sessions/{id}/questions/{qid} {answers}` or `{dismiss: true}` | answer the agent's questions |
| `POST /sessions/{id}/undo` · `/redo` | undo or redo the latest turn's file changes |

A turn's events are `text`, `reasoning`, `tool_start`, `tool_end`, `step_end`,
`retry`, `compacted`, `todos`, `notice`, `permission`, `question` and finally
`done` (with `error` set if the turn failed or was aborted).

`attachments` sends images or PDFs with the message, each either inline,
`{"data": "<base64>", "mime": "image/png", "name": "shot.png"}`, or by path,
`{"path": "screenshots/shot.png"}`. The bytes decide the type (anything else
is refused), and images are fitted to the `media` limits. A path goes through
the same checks as the `read` tool: `external_directory` outside the project,
then `read`, asked over the turn's stream (or settled by your permission
rules), and a refusal ends the turn with an error before anything is sent.

## Roadmap

1. ~~Working agent: `run`, core tools, permissions, config, instructions~~
2. Sessions: ~~persistence, continue, titles, compaction, subagents, todos,
   webfetch, undo/redo snapshots, skills, commands, background shell commands, the question tool, web search (self-hosted
   SearXNG)~~
3. ~~Interactive terminal UI, local server API~~
4. ~~MCP (stdio, HTTP, OAuth), more providers, saved keys, formatters, worktrees, stats~~
5. ~~ACP, native Rust plugins, Code Mode, automations, MCP resources, prompts
   and event triggers~~

## License

MIT. CodeSmith is derived from OpenCode, © 2025 opencode, also MIT; see
[LICENSE](LICENSE). The code review prompts adapt Anthropic's code-review
plugin under the Apache License 2.0; see
[its notice](crates/codesmith-core/src/prompt/review/NOTICE.md).
