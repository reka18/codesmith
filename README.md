# CodeSmith

A terminal coding agent, written in Rust. CodeSmith is a port of
[OpenCode](https://github.com/sst/opencode) aiming for feature parity with its
terminal agent. It is fully local: no hosted services, no accounts, no
telemetry. Point it at any model provider you like, including one running on
your own machines.

> **Status: early.** The interactive terminal UI and `codesmith run` work end
> to end with the core tools, permissions, project instructions, persistent
> sessions, subagents, skills, commands and undo. MCP, the local server API and
> the long tail of providers are still to come; see [Roadmap](#roadmap).

## Install

```sh
cargo install --path crates/codesmith
```

## Use

Run `codesmith` in a project for the interactive UI (`-c` continues the latest
session here, `-s ID` a specific one, `--prompt "..."` sends a first message):

- **Enter** sends, **Alt+Enter** or **Ctrl+J** adds a line, **↑/↓** recalls
  earlier messages, **PgUp/PgDn** scroll
- **Esc** interrupts a running turn; **Ctrl+C** on an empty prompt quits
- Permission requests and questions open as dialogs
- `/new`, `/sessions`, `/undo`, `/redo`, `/agent`, `/model`, `/compact`,
  `/clear`, `/help`, `/exit`, plus every custom and built-in command

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
| `-f, --file PATH` | Attach a file's contents (repeatable) |
| `--format json` | One JSON object per event, for scripting |
| `--auto` | Approve every permission request that no rule denies |
| `--thinking` | Show the model's reasoning |
| `-c, --continue` | Continue the most recent session in this directory |
| `-s, --session ID` | Continue a specific session |
| `--title TEXT` | Title a new session (otherwise one is generated) |

Without `--auto`, CodeSmith asks on the terminal before any action a rule
marks `ask`: allow once, always allow, or reject with feedback for the model.

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

Undo works by snapshotting the project at the start of each turn into a
private git repository under CodeSmith's data directory. It never touches your
own `.git`, works in folders that are not git repositories, and respects
`.gitignore`. Set `"snapshots": false` to turn it off.

Other commands: `codesmith models`, `codesmith debug paths|config|model|agents|prompt`.

## Tools

| Tool | What it does |
|---|---|
| `read` | Read a file (numbered lines, paged) or list a directory |
| `write` | Create or overwrite a file |
| `edit` | Exact find-and-replace, tolerant of typographic quotes, dashes and trailing whitespace; preserves CRLF/LF line endings |
| `glob` | Find files by glob pattern, respecting `.gitignore` |
| `grep` | Search file contents with ripgrep's regex engine |
| `shell` | Run a command; long-running ones move to the background and the agent is told when they finish. Large output is saved to a file |
| `webfetch` | Fetch a URL as markdown, text or HTML; each redirect is permission-checked |
| `todowrite` | Keep a visible todo list for multi-step work |
| `subagent` | Delegate a task to a subagent (`general`, `explore`, or your own) in a child session |
| `skill` | Load a skill's instructions when a task matches its description |
| `websearch` | Search the web through your own [SearXNG](https://docs.searxng.org/) instance (only offered when `websearch.url` is set) |
| `question` | Ask you multiple-choice or free-form questions on the terminal (not offered with `--auto` or without a terminal) |
| `lsp` | Definitions, references, hover, symbols and call hierarchy from your language servers (only offered when `lsp` is on) |

## Configuration

codesmith reads `codesmith.json` or `codesmith.jsonc` (comments and trailing
commas allowed), lowest to highest precedence:

1. `~/.config/codesmith/codesmith.json` (global)
2. the file named by `$CODESMITH_CONFIG`
3. from the project root (nearest `.git`) down to the working directory: each
   folder's `codesmith.json` and `.codesmith/codesmith.json`
4. inline JSON in `$CODESMITH_CONFIG_CONTENT`

`{env:VAR}` and `{file:path}` are substituted anywhere in the text.

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

**Permissions** are an ordered list of `{action, resource, effect}`; the last
matching rule wins and anything unmatched asks. Actions are tool names, with
`write` and `edit` sharing `edit`, plus `external_directory` for paths outside
the project. `*` matches anything; a trailing ` *` also matches the bare
command (`git *` matches `git`). By default everything is allowed except
reading `.env` files, which asks.

**Instructions**: `AGENTS.md` files from the project root down to the working
directory (with `CLAUDE.md` as a fallback), plus
`~/.config/codesmith/AGENTS.md` and anything listed under `instructions`.

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
`codesmith mcp list` shows each server's status and tools.

**Code Mode.** As in OpenCode, MCP tools are offered through Code Mode by
default: instead of calling them one by one, the model writes a short
JavaScript program in a single `execute` call —
`const issues = await tools.github.list_issues({repo}); return issues.filter(...)`
— and gets back the result and any `console.log` output. The tools are listed
with TypeScript-style signatures in the system prompt. Programs run in
[Boa](https://boajs.dev), a JavaScript engine written in Rust, with no
filesystem, network, process or timer access of their own: the only way out
is the listed tools, each call permission-checked like a direct one, capped at
50 calls, three minutes, and bounded loops and recursion. Set
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
codesmith automation list | pause ID | resume ID | remove ID | log ID
```

Triggers: `--at` (once: "in 30 minutes", "2026-10-27T09:00", or an RFC 3339
time), `--every` ("10 minutes", "2 hours", "1 day") and `--cron` (five
fields, in `--tz` or local time). A prompt goes to an existing session
(`--session`) or to a new session with an agent on each run. A Code Mode
program (`--code-file`) gets `input = {state, now, runs, args}` and may
return `{state, done, send: {prompt, to: {subagent}}}` to keep state between
runs, finish itself, or hand work to a session.

Automations are stored with your sessions and run by whichever CodeSmith is
open — the UI, `serve` or `acp` (`"automations": false` opts a process out) —
or by `codesmith automation serve`, which you can keep running under launchd
or systemd; a lease in the database keeps two processes from firing the same
run. Runs are unattended: they follow your permission rules, but anything
that would ask is refused. Each run's outcome, session and answer are kept for
`automation log`. Triggers from MCP server events are not supported yet.

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

Sessions stream text, reasoning, tool calls and plans (from the todo list);
permission requests appear in the editor; agents (build, plan, …) are modes;
slash commands, including `/review` and `/calibrate`, are offered; and
sessions can be listed, loaded (with their history replayed), resumed,
cancelled, closed and deleted. MCP servers the editor passes in are added to
the session (stdio and HTTP). Sessions are the same ones the terminal UI and
`codesmith run` use.

## Local server

`codesmith serve` exposes sessions to other programs on this machine (an
editor, a script) over HTTP on `127.0.0.1:4096`. It is opt-in and local:
anything but loopback requires `--token` (or `$CODESMITH_SERVER_TOKEN`), which
clients send as `Authorization: Bearer <token>`. `--auto` approves permission
requests instead of asking the client.

| Route | |
| --- | --- |
| `GET /health` | version, directory, model |
| `GET /sessions` · `POST /sessions {agent?, model?}` | list, create |
| `GET /sessions/{id}` · `DELETE /sessions/{id}` | info and messages, delete |
| `POST /sessions/{id}/messages {text}` | run a turn; streams Server-Sent Events |
| `POST /sessions/{id}/abort` | stop the running turn |
| `POST /sessions/{id}/permissions/{rid} {reply, message?}` | answer `once`, `always` or `reject` |
| `POST /sessions/{id}/questions/{qid} {answers}` or `{dismiss: true}` | answer the agent's questions |
| `POST /sessions/{id}/undo` · `/redo` | undo or redo the latest turn's file changes |

A turn's events are `text`, `reasoning`, `tool_start`, `tool_end`, `step_end`,
`retry`, `compacted`, `todos`, `notice`, `permission`, `question` and finally
`done` (with `error` set if the turn failed or was aborted).

## Roadmap

1. ~~Working agent: `run`, core tools, permissions, config, instructions~~
2. Sessions: ~~persistence, continue, titles, compaction, subagents, todos,
   webfetch, undo/redo snapshots, skills, commands, background shell commands, the question tool, web search (self-hosted
   SearXNG)~~
3. ~~Interactive terminal UI, local server API~~
4. ~~MCP (stdio, HTTP, OAuth), more providers, saved keys, formatters, worktrees, stats~~
5. ~~ACP, native Rust plugins, Code Mode, automations~~ (MCP event triggers to come)

## License

MIT. CodeSmith is derived from OpenCode, © 2025 opencode, also MIT; see
[LICENSE](LICENSE). The code review prompts adapt Anthropic's code-review
plugin under the Apache License 2.0; see
[its notice](crates/codesmith-core/src/prompt/review/NOTICE.md).
