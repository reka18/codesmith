<p align="center">
  <img src="assets/codesmith-banner.jpg" alt="CodeSmith" width="100%">
</p>

# CodeSmith

A terminal coding agent, written in Rust, built to be safe to hand your
machine to.

- **Fully local.** No hosted services, no accounts, no telemetry. Sessions,
  keys and logs stay on your machine.
- **Careful by default.** Every shell command is checked, secret files
  ask, your keys are kept from the commands it runs, and a repository's
  own config is never read. See [Safety](#safety).
- **Any model.** Use Anthropic, OpenAI, Gemini or another hosted provider,
  or a model on your own hardware through Ollama, LM Studio, llama.cpp,
  vLLM, oMLX or any OpenAI-compatible server. Reasoning effort, thinking
  budgets and other provider options, with named variants
  (`provider/model#high`); see [Configuration](docs/configuration.md#provider-options-and-variants).
- **In the terminal, in your editor, or from scripts.** An interactive UI,
  `codesmith run` for one-shot use, ACP for editors such as Zed, and a local
  HTTP API.

> **Status: early.** CodeSmith is young, so expect rough edges.

## Why CodeSmith?

A coding agent is a tool you live in all day and hand to other people.
CodeSmith is built for that:

- **One binary, no runtime.** Install a single signed, notarized executable —
  no Node, no `npm`, no version drift. The Linux builds are static musl, so
  one file runs anywhere from Alpine to an old LTS.
- **Light and durable.** A compiled core keeps long sessions lean on memory
  and CPU, with no garbage-collection pauses, and keeps working without a
  package ecosystem shifting under it.
- **Safety where an agent touches your machine.** Permissions, secret-file
  guards, credential-free child environments and per-repo config kept out of
  the repository are enforced
  by a type-checked core with an explicit, layered policy model; see
  [Safety](#safety).
- **Behaviour you can trust.** Providers are tested against recorded real
  traffic, and features ship with regression, attack-style, edge and
  stress tests.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/reka18/codesmith/main/install.sh | sh
# or
brew install reka18/tap/codesmith
# or, from source (Rust 1.91+)
cargo install --locked --git https://github.com/reka18/codesmith codesmith
```

`codesmith upgrade` updates a copy installed from a release tarball (it
checks `SHA256SUMS` first); Homebrew and cargo installs update through those
tools. CodeSmith never checks for updates unless you
[opt in](docs/cli.md#update-checks).

Release binaries are for macOS (signed and notarized) and Linux (static, any
distribution). See [installation](docs/getting-started.md#install) for details
and manual downloads.

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
once), **a** (always, saved for the project), or **n**/**Esc**/**Enter**
(reject). For scripts:

```sh
codesmith run "explain what src/main.rs does"
git diff | codesmith run "review this change"
```

[Getting started](docs/getting-started.md) walks through all of this.

## Features

- **[Interactive UI](docs/interactive-ui.md)**: Markdown transcript with
  diffs, model, agent and session pickers, `@` file mentions, image and PDF
  attachments, and leader-key shortcuts.
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
  sandboxed JavaScript engine (with `fetch` and OpenAPI tools), resources,
  prompts, elicitation and events.
- **[Automations](docs/automations.md)**: prompts or programs on a schedule
  or on MCP events, with unattended runs locked down by default.
- **[Language servers and formatters](docs/language-servers-and-formatters.md)**:
  built-in lists for common languages, off until you turn them on.
- **[Editors (ACP)](docs/editors-acp.md)** and a
  **[local HTTP server](docs/local-server.md)** that requires a token.
- **[Plugins](docs/plugins.md)**: native Rust plugins compiled into your own
  build. Built in: `/goal`, which keeps a session working until a reviewer
  finds the objective met, Plan mode for the `plan` agent, and the
  `codesmith` tool for model search and worktrees.

The full guide is in [docs/](docs/README.md), with
[configuration](docs/configuration.md), [CLI](docs/cli.md) and
[troubleshooting](docs/troubleshooting.md) references.

## Safety

A coding agent runs commands and edits files as you. CodeSmith's job is to
make sure you see what it does before it matters, and that nothing it
reads or runs gets your secrets without you knowing. Each of these is
enforced in the core and covered by tests, including attack-style ones.

- **Every shell command is checked, not just the first.** A line like
  `git status && rm -rf build` is split into each command it would run,
  including those inside `$(…)`, pipelines, loops, `sudo`/`env`/`xargs`
  wrappers, `sh -c '…'` strings and commands handed to `git`, `ssh` or
  `awk`. Redirects are checked as writes. A line the scanner can't read
  runs only as far as your rules allow commands in general.
  [More](docs/permissions.md#shell-commands)
- **Secret files ask, whatever else allows.** Reading `.env` files,
  `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.netrc`, cloud and registry logins,
  keychain dumps and CodeSmith's own saved keys asks every time. A blanket
  allow or an "always" answer doesn't lift it; only a rule naming that path
  does. Symlinks and `..` are followed first.
  [More](docs/permissions.md#secret-files)
- **Commands don't inherit your keys.** The shell, MCP servers, language
  servers and formatters get your environment with API keys, tokens and
  passwords removed. [More](docs/configuration.md#what-commands-see-of-your-environment)
- **A repository can't configure CodeSmith.** A repository's
  `codesmith.json` and `.codesmith/` agents and settings are never read, so
  a clone can't add permission rules, point a provider at its own URL, read
  your secrets or run a program. Settings for one repository live in your
  own config folder, matched by its path.
  [More](docs/configuration.md#per-repo-config)
- **The project is a boundary.** Reading, writing or `cd`-ing outside the
  repository asks first. Starting outside any repository warns you, and
  your home folder or `/` asks every time (and is refused when nobody is
  at a terminal to confirm). [More](docs/cli.md#starting-outside-a-repository)
- **Prompts are hard to fool.** Enter rejects, so a stray key never
  approves. Hidden control characters and padding are shown, not
  rendered. A request from another session says who sent it, and can't
  be saved as "always".
  [More](docs/permissions.md#answering-a-request)
- **Unattended runs are locked down.** Automations, and `codesmith run`
  with no terminal, refuse anything that would ask. Automations can't use
  the shell, edit files, fetch URLs or call MCP tools unless you opt them
  back in. [More](docs/permissions.md#unattended-runs)
- **Undo for every turn.** File changes are snapshotted per turn, so
  `undo` puts them back. [More](docs/sessions.md)
- **Nothing leaves your machine but the model calls you set up.** No
  accounts, no telemetry, no update checks unless you opt in. Keys are
  stored readable only by you. The local server and the
  [local bus](docs/bus.md) listen on loopback or a private socket and
  need a token.
- **Signed releases.** `codesmith upgrade` checks a minisign-signed
  `SHA256SUMS` before replacing anything, and `install.sh` does too when
  minisign is installed. The macOS binary is signed and notarized. [More](docs/cli.md#codesmith-upgrade)

**What it isn't:** a sandbox. CodeSmith runs as you, with your access to
files and the network. Its checks are a careful policy layer, not an OS
jail; a program you let it run can do whatever you can. The shell check
is best effort for paths a command computes at run time.

> **What a repository still brings.** Its `AGENTS.md`, skills and commands
> reach the model as text, and a command's `` !`cmd` `` asks permission like
> any shell command. Read them in a repository you don't trust, or run with
> `CODESMITH_DISABLE_PROJECT_CONFIG=1` to leave them out. [More](docs/configuration.md#files-and-precedence)

## License and attribution

CodeSmith is MIT licensed; see [LICENSE](LICENSE), which also carries the
notices of the open-source work it includes.

- The code review prompts adapt the code-review plugin from Anthropic's
  `claude-plugins-official` repository, under the Apache License 2.0. See
  [its notice](crates/codesmith-core/src/prompt/review/NOTICE.md) and
  [license](crates/codesmith-core/src/prompt/review/LICENSE-APACHE-2.0).
- Some provider test fixtures are third-party HTTP recordings, vendored
  unchanged under their license. See
  [their notice](crates/codesmith-ai/tests/fixtures/opencode/NOTICE.md).

Maintainers: see [RELEASING.md](RELEASING.md) and
[provider tests](crates/codesmith-ai/PARITY.md).
