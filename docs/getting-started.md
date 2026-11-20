# Getting started

## Install

Prebuilt binaries for macOS (Apple Silicon and Intel) and Linux (x86_64 and
arm64) are attached to each
[GitHub Release](https://github.com/reka18/codesmith/releases). None have been
published yet, so for now build from source as described below. Releases are
published only after the macOS binaries are signed with a Developer ID and
notarized by Apple. The Linux binaries are statically linked (musl) and run
on any distribution.

**Install script** (macOS and Linux). It downloads the release tarball for
your platform, checks it against the release's `SHA256SUMS`, and puts
`codesmith` in `~/.local/bin`. It sends nothing anywhere else.

```sh
curl -fsSL https://raw.githubusercontent.com/reka18/codesmith/main/install.sh | sh
```

Set `CODESMITH_VERSION=0.1.0` to pick a version, and `CODESMITH_INSTALL_DIR`
to install somewhere else. You can [read the script](../install.sh) first.

**Homebrew**

```sh
brew install reka18/tap/codesmith
```

**Manually.** Download `codesmith-<version>-<target>.tar.gz` and
`SHA256SUMS` from the release. Check them with
`shasum -a 256 -c SHA256SUMS --ignore-missing`, unpack, and put `codesmith`
on your `PATH`.

**From source** (Rust 1.91 or later):

```sh
cargo install --locked --git https://github.com/reka18/codesmith codesmith
# or, from a checkout
cargo install --locked --path crates/codesmith
```

Maintainers: see [RELEASING.md](../RELEASING.md).

## Choose a model

CodeSmith has no default model and no account of its own. It needs a model
from a provider you choose.

**A hosted provider.** Set its key and pick a model:

```sh
export ANTHROPIC_API_KEY=...          # or: codesmith auth login anthropic
codesmith -m anthropic/claude-sonnet-4-5
```

The built-in providers are `anthropic`, `openai`, `google`, `azure`,
`openrouter`, `groq`, `deepseek`, `mistral`, `xai`, `together`, `fireworks`
and `cerebras`. See [providers](configuration.md#built-in-providers) for their
key variables (`azure` also needs its resource name).

**A local server.** `ollama`, `lmstudio` and `llamacpp` are built in at their
default ports, with no key:

```sh
codesmith -m ollama/qwen3-coder
```

Any other OpenAI-compatible server (vLLM, oMLX, a llama.cpp server on another
port, and so on) takes a few lines of config. Put this in
`~/.config/codesmith/codesmith.json`:

```jsonc
{
  "model": "local/qwen3-coder",
  "providers": {
    "local": {
      "protocol": "openai-chat",
      "settings": { "baseURL": "http://127.0.0.1:8000/v1" },
      "models": {
        "qwen3-coder": { "limit": { "context": 32768, "output": 8192 } }
      }
    }
  }
}
```

Setting `limit.context` lets CodeSmith compact long sessions before they
overflow the window. `codesmith debug model` shows what the model resolves
to, and `codesmith models local` asks the server which models it has.

## First session

In a project folder:

```sh
codesmith
```

Type a request and press **Enter**. The agent reads, searches, edits and runs
commands to do it. Everything inside the project is allowed by default,
except reading `.env` files and anything outside the project, which ask
first. To be asked before shell commands too, add a rule (see
[permissions](permissions.md)):

```jsonc
{ "permissions": [{ "action": "shell", "resource": "*", "effect": "ask" }] }
```

When a request opens a dialog: **y** allows once, **a** always allows for
this session, and **n**, **Esc** or **Enter** reject.

A few things to try:

- **Tab** switches to the read-only `plan` agent, for thinking a change
  through before making it.
- `@src/main.rs` attaches a file. Pasting an image's path attaches the image.
- `/undo` puts back the files the last turn changed.
- `/init` writes an `AGENTS.md` that tells future sessions about the project.
- `/help` lists keys and commands.

See [the interactive UI](interactive-ui.md) for the rest.

## Scripts and one-shot use

```sh
codesmith run "explain what src/main.rs does"
git diff | codesmith run "write a commit message for this"
codesmith run --format json "list the TODOs" > todos.jsonl
```

See [`codesmith run`](cli.md#codesmith-run).

## Next steps

- [Configuration](configuration.md): every key, providers, instructions.
- [Agents and subagents](agents-and-subagents.md) and
  [skills and commands](skills-and-commands.md).
- [MCP servers](mcp.md), [code review](code-review.md),
  [automations](automations.md).
- [Editors (ACP)](editors-acp.md) and the [local server](local-server.md).
