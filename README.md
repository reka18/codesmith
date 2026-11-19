# skeptical

A terminal coding agent, written in Rust. skeptical is a port of
[OpenCode](https://github.com/sst/opencode) aiming for feature parity with its
terminal agent. It is fully local: no hosted services, no accounts, no
telemetry. Point it at any model provider you like, including one running on
your own machines.

> **Status: early.** `skeptical run` works end to end with the core tools,
> permissions, project instructions and persistent sessions (continue, titles,
> automatic compaction). The interactive terminal UI, MCP and the long tail of
> providers are still to come; see [Roadmap](#roadmap).

## Install

```sh
cargo install --path crates/skeptical
```

## Use

```sh
skeptical run "explain what src/main.rs does"
skeptical run -m anthropic/claude-sonnet-4-5 "add a --verbose flag"
git diff | skeptical run "review this change"
skeptical run --agent plan "how should I restructure the config module?"
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

Without `--auto`, skeptical asks on the terminal before any action a rule
marks `ask`: allow once, always allow, or reject with feedback for the model.

## Skills and commands

**Skills** are folders with a `SKILL.md` (a `name`/`description` header and
instructions). The model sees the list and loads one with the `skill` tool when
a task matches. skeptical looks in `~/.config/skeptical/skills/`,
`~/.agents/skills/`, `~/.claude/skills/`, and in each project folder's
`.skeptical/skills/`, `.claude/skills/` and `.agents/skills/`.

**Commands** are prompt templates run as `skeptical run /name args`. Put them
in `.skeptical/commands/<name>.md` (or `~/.config/skeptical/commands/`, or
`commands` in config), optionally with `description`, `agent` and `model` in a
header. `$ARGUMENTS` is the whole argument string and `$1`…`$9` single
arguments. Built in: `/init` (write or improve `AGENTS.md`) and `/review`
(review uncommitted changes, a commit, a branch or a PR). `skeptical commands`
lists them.

```markdown
---
description: Explain a file in one sentence
---
Read $1 and explain what it does in exactly one sentence.
```

## Sessions

Every run is saved to a local SQLite database
(`~/.local/share/skeptical/skeptical.db`, or `$SKEPTICAL_DB`). New sessions
get a short generated title. When a conversation nears the model's context
window (`limit.context` in config), older history is replaced by a structured
summary and the newest exchanges are kept verbatim, so long sessions keep
going.

```sh
skeptical session list [--all] [--json]
skeptical session export <id> > session.json
skeptical session import session.json
skeptical session delete <id>
skeptical session diff  [id]   # file changes since the latest turn began
skeptical session undo  [id]   # restore the files and hide the latest turn
skeptical session redo  [id]   # bring it back (until a new turn is added)
```

Undo works by snapshotting the project at the start of each turn into a
private git repository under skeptical's data directory. It never touches your
own `.git`, works in folders that are not git repositories, and respects
`.gitignore`. Set `"snapshots": false` to turn it off.

Other commands: `skeptical models`, `skeptical debug paths|config|model|agents|prompt`.

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

## Configuration

skeptical reads `skeptical.json` or `skeptical.jsonc` (comments and trailing
commas allowed), lowest to highest precedence:

1. `~/.config/skeptical/skeptical.json` (global)
2. the file named by `$SKEPTICAL_CONFIG`
3. from the project root (nearest `.git`) down to the working directory: each
   folder's `skeptical.json` and `.skeptical/skeptical.json`
4. inline JSON in `$SKEPTICAL_CONFIG_CONTENT`

`{env:VAR}` and `{file:path}` are substituted anywhere in the text.

```jsonc
{
  "model": "local/qwen3-coder",
  "providers": {
    "local": {
      "protocol": "openai-chat",            // or "anthropic-messages"
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
json]`) and point skeptical at it:

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
`~/.config/skeptical/AGENTS.md` and anything listed under `instructions`.

## Roadmap

1. ~~Working agent: `run`, core tools, permissions, config, instructions~~
2. Sessions: ~~persistence, continue, titles, compaction, subagents, todos,
   webfetch, undo/redo snapshots, skills, commands, background shell commands, the question tool, web search (self-hosted
   SearXNG)~~
3. Local server API and the interactive terminal UI
4. MCP, more providers and logins, formatters, worktrees, stats
5. Native Rust plugins, code mode, ACP, automations

## License

MIT. skeptical is derived from OpenCode, © 2025 opencode, also MIT; see
[LICENSE](LICENSE).
