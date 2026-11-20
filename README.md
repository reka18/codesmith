# CodeSmith

A terminal coding agent, written in Rust. CodeSmith is a port of
[OpenCode](https://github.com/anomalyco/opencode)'s terminal agent, and aims
to match its features.

- **Fully local.** No hosted services, no accounts, no telemetry. Sessions,
  keys and logs stay on your machine.
- **Any model.** Use Anthropic, OpenAI, Gemini or another hosted provider,
  or a model on your own hardware through Ollama, LM Studio, llama.cpp,
  vLLM, oMLX or any OpenAI-compatible server. Reasoning effort, thinking
  budgets and other provider options, with named variants
  (`provider/model#high`); see [Configuration](docs/configuration.md#provider-options-and-variants).
- **In the terminal, in your editor, or from scripts.** An interactive UI,
  `codesmith run` for one-shot use, ACP for editors such as Zed, and a local
  HTTP API.

> **Status: early.** CodeSmith covers OpenCode's terminal agent feature set,
> and is pinned to OpenCode's behaviour by ported tests and replays of its
> recorded provider traffic. It is young, though, so expect rough edges.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/reka18/codesmith/main/install.sh | sh
# or
brew install reka18/tap/codesmith
# or, from source (Rust 1.91+)
cargo install --locked --git https://github.com/reka18/codesmith codesmith
```

Release binaries are for macOS (signed and notarized) and Linux (static, any
distribution). None have been published yet, so build from source for now.
See [installation](docs/getting-started.md#install) for details and manual
downloads.

## Quick start

With a hosted provider:

```sh
export ANTHROPIC_API_KEY=...        # or: codesmith auth login anthropic
cd your-project
codesmith -m anthropic/claude-sonnet-4-5
```

With a local model, set a default in `~/.config/codesmith/codesmith.json`:

```jsonc
{
  "model": "local/qwen3-coder",
  "providers": {
    "local": {
      "protocol": "openai-chat",
      "settings": { "baseURL": "http://127.0.0.1:8000/v1" },
      "models": { "qwen3-coder": { "limit": { "context": 32768, "output": 8192 } } }
    }
  }
}
```

Then run `codesmith` in a project. Ollama, LM Studio and llama.cpp at their
default ports need no config at all: `codesmith -m ollama/qwen3-coder`.

Type a request and press **Enter**. Permission prompts take **y** (allow
once), **a** (always, for this session), or **n**/**Esc**/**Enter**
(reject). For scripts:

```sh
codesmith run "explain what src/main.rs does"
git diff | codesmith run "review this change"
```

[Getting started](docs/getting-started.md) walks through all of this.

## Features

- **[Interactive UI](docs/interactive-ui.md)**: Markdown transcript with
  diffs, model, agent and session pickers, `@` file mentions, image and PDF
  attachments, and OpenCode's leader keys.
- **[Tools](docs/tools.md)**: read, write, edit, glob, grep, shell (with
  background commands), web fetch, self-hosted web search, todos, questions,
  and native image and PDF input.
- **[Permissions](docs/permissions.md)**: last-match-wins rules. Every
  command in a shell line is checked separately, including wrappers,
  substitutions and redirects. Paths outside the project ask.
- **[Agents and subagents](docs/agents-and-subagents.md)**: `build`, a
  read-only `plan`, `explore` and `general` subagents, and your own agents.
- **[Skills and commands](docs/skills-and-commands.md)**: `SKILL.md`
  folders, and prompt templates as `/commands`.
- **[Sessions](docs/sessions.md)**: stored locally. Continue, undo and redo
  file changes per turn, export and import, automatic compaction, usage
  stats, and isolated git worktrees.
- **[Code review](docs/code-review.md)**: `/calibrate` chooses review
  dimensions for the codebase, and `/review` runs parallel reviewers with
  independent verification.
- **[MCP](docs/mcp.md)**: local and remote servers, OAuth, Code Mode in a
  sandboxed JavaScript engine, resources, prompts and events.
- **[Automations](docs/automations.md)**: prompts or programs on a schedule
  or on MCP events, with unattended runs locked down by default.
- **[Language servers and formatters](docs/language-servers-and-formatters.md)**:
  OpenCode's built-in lists, off until you turn them on.
- **[Editors (ACP)](docs/editors-acp.md)** and a
  **[local HTTP server](docs/local-server.md)** that requires a token.
- **[Plugins](docs/plugins.md)**: native Rust plugins compiled into your own
  build, and a built-in `/goal` that keeps a session working until a
  reviewer finds the objective met.

The full guide is in [docs/](docs/README.md), with
[configuration](docs/configuration.md), [CLI](docs/cli.md) and
[troubleshooting](docs/troubleshooting.md) references.

## Project config is trusted

> **Read a repository's CodeSmith config before you run CodeSmith in it.**
> As in OpenCode, a `codesmith.json` or `.codesmith/` folder inside a
> repository is applied as if you had written it. It can start MCP servers
> (which run programs), choose the shell, turn on the repository's own
> formatters and language servers, point a provider at any URL and send it
> values from `{env:}` or `{file:}`, and add permission rules that come after
> your global ones, and so override them. In a repository you don't trust,
> read its `codesmith.json[c]` and `.codesmith/` first, or run with
> `CODESMITH_DISABLE_PROJECT_CONFIG=1`.

## License and attribution

CodeSmith is MIT licensed; see [LICENSE](LICENSE). It is derived from
[OpenCode](https://github.com/anomalyco/opencode), © 2025 opencode, also
under the MIT License, and that notice is kept in [LICENSE](LICENSE).

- The code review prompts adapt the code-review plugin from Anthropic's
  `claude-plugins-official` repository, under the Apache License 2.0. See
  [its notice](crates/codesmith-core/src/prompt/review/NOTICE.md) and
  [license](crates/codesmith-core/src/prompt/review/LICENSE-APACHE-2.0).
- The provider test fixtures include HTTP recordings made by the OpenCode
  project, vendored unchanged under its MIT License. See
  [their notice](crates/codesmith-ai/tests/fixtures/opencode/NOTICE.md).

Maintainers: see [RELEASING.md](RELEASING.md) and
[provider parity](crates/codesmith-ai/PARITY.md).
