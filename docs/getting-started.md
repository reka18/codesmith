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
your platform, checks it against the release's `SHA256SUMS` (and, if
[minisign](https://jedisct1.github.io/minisign/) is installed, checks that
`SHA256SUMS` is signed by a CodeSmith release key), and puts `codesmith` in
`~/.local/bin`. It sends nothing anywhere else.

```sh
curl -fsSL https://raw.githubusercontent.com/reka18/codesmith/main/install.sh | sh
```

Set `CODESMITH_VERSION=0.1.0` to pick a version, `CODESMITH_INSTALL_DIR`
to install somewhere else, and `CODESMITH_BASE_URL` to download the release
files from a mirror. You can [read the script](../install.sh) first.

**Homebrew**

```sh
brew install reka18/tap/codesmith
```

**Manually.** Download `codesmith-<version>-<target>.tar.gz`,
`SHA256SUMS` and `SHA256SUMS.minisig` from the release. Check the signature
with `minisign -V -P <key> -m SHA256SUMS`, using the key in
[`release_key.pub`](../crates/codesmith/src/release_key.pub), and that its
trusted comment names the version; then check the tarball with
`shasum -a 256 -c SHA256SUMS --ignore-missing`, unpack, and put `codesmith`
on your `PATH`.

**From source** (Rust 1.91 or later):

```sh
cargo install --locked --git https://github.com/reka18/codesmith codesmith
# or, from a checkout
cargo install --locked --path crates/codesmith
```

**Upgrading.** `codesmith upgrade` upgrades a copy from the install script
or a tarball, after checking the signature of `SHA256SUMS` with the release
key built into it and the new tarball against `SHA256SUMS`. Homebrew
and cargo copies are left to `brew upgrade` and `cargo install`. CodeSmith
never checks for updates on its own unless you
[opt in](cli.md#update-checks). See [`codesmith upgrade`](cli.md#codesmith-upgrade).

Maintainers: see [RELEASING.md](../RELEASING.md).

## Choose a model

CodeSmith has no account of its own. It needs a model from a provider you
choose. With no model set (no `-m` and no `"model"` in config), a session
runs the newest model available to you, as OpenCode does: the newest chat
model, by catalog release date, of a configured provider, a provider whose
key you have set or saved, or a local server. Set `"model"` to choose.

**A hosted provider.** Set its key and pick a model:

```sh
export ANTHROPIC_API_KEY=...          # or: codesmith auth login anthropic
codesmith -m anthropic/claude-sonnet-4-5
```

The built-in providers are `anthropic`, `openai`, `google`, `azure`,
`openrouter`, `groq`, `deepseek`, `mistral`, `xai`, `together`, `fireworks`
and `cerebras`. See [providers](configuration.md#built-in-providers) for their
key variables (`azure` also needs its resource name).

Several sign in their own way:

```sh
export CODESMITH_COPILOT_CLIENT_ID=Iv1.…          # your GitHub OAuth app, device flow on
codesmith auth login github-copilot              # GitHub's device flow (or set COPILOT_GITHUB_TOKEN)
codesmith -m github-copilot/claude-sonnet-4.6

export AWS_PROFILE=work                           # or AWS keys, SSO, AWS_BEARER_TOKEN_BEDROCK
codesmith -m amazon-bedrock/us.anthropic.claude-sonnet-4-5-20250929-v1:0

gcloud auth application-default login             # Application Default Credentials
codesmith auth login google-vertex --project my-project
codesmith -m google-vertex/gemini-2.5-pro
```

The Copilot login needs a GitHub OAuth app of your own (CodeSmith ships
none); [GitHub Copilot](configuration.md#github-copilot) says how to register
one. See [Amazon Bedrock](configuration.md#amazon-bedrock),
[Google Vertex AI](configuration.md#google-vertex-ai) and
[GitHub Copilot](configuration.md#github-copilot).

Others take a key plus an account or resource, or a browser login:

```sh
codesmith auth login cloudflare-ai-gateway       # token, then account and gateway ids
codesmith -m cloudflare-ai-gateway/anthropic/claude-sonnet-4.6

az login && codesmith auth login azure --cli --field resourceName=my-resource
codesmith models azure                            # lists the resource's deployments
codesmith -m azure/my-deployment

export SNOWFLAKE_ACCOUNT=myorg-myaccount SNOWFLAKE_CORTEX_PAT=...
codesmith -m snowflake-cortex/claude-sonnet-4-6

codesmith auth login poe --oauth --client-id <your Poe OAuth client>
```

**Your Claude Code login.** With [Claude Code](https://code.claude.com/docs/en/setup)
installed and signed in (`claude auth login`), the `claude-agent` provider
runs it as the model, with CodeSmith's tools, permissions and history.
CodeSmith never sees or stores its credentials:

```sh
codesmith auth login claude-agent      # checks claude and saves its model list
codesmith -m claude-agent/sonnet
```

See [Claude Agent](configuration.md#claude-agent-your-claude-code-login).

Browser logins for Poe, DigitalOcean, GitLab and ChatGPT, and xAI's device
login, run as an OAuth application you register yourself (CodeSmith ships
none); each section of
[the configuration guide](configuration.md#providers-and-models) says how
(for ChatGPT you can instead opt in to OpenAI's agent registration with
`codesmith auth login openai --oauth --register`, which OpenAI may reject):
[Cloudflare](configuration.md#cloudflare-ai-gateway-and-workers-ai),
[Azure](configuration.md#azure-openai), [Poe](configuration.md#poe),
[DigitalOcean](configuration.md#digitalocean),
[GitLab](configuration.md#gitlab-sign-in),
[ChatGPT](configuration.md#sign-in-with-chatgpt),
[xAI](configuration.md#xai-supergrok-login),
[Snowflake Cortex](configuration.md#snowflake-cortex),
[SAP AI Core](configuration.md#sap-ai-core) and
[Modal](configuration.md#live-model-lists-modal-github-copilot-azure).

**A local server.** `ollama`, `lmstudio`, `vllm` and `llamacpp` are built in
at their default ports, with no key, and `codesmith models` lists what each
running one has loaded (see
[local model discovery](configuration.md#local-model-discovery-ollama-lm-studio-vllm-llamacpp)):

```sh
codesmith -m ollama/qwen3-coder
```

Any other OpenAI-compatible server (oMLX, a llama.cpp server on another
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
[Using a local server](configuration.md#using-a-local-server-omlx-lm-studio-llamacpp-ollama)
has the recommended settings: thinking, context limits, session titles and
keeping the prompt cached.

## First session

In a project folder:

```sh
codesmith
```

Type a request and press **Enter**. The agent reads, searches, edits and runs
commands to do it. Everything inside the project is allowed by default,
except reading secret files (`.env`, `~/.ssh`, saved keys) and anything
outside the project, which ask
first. To be asked before shell commands too, add a rule (see
[permissions](permissions.md)):

```jsonc
{ "permissions": [{ "action": "shell", "resource": "*", "effect": "ask" }] }
```

When a request opens a dialog: **y** allows once, **a** always allows (saved
for the project), and **n**, **Esc** or **Enter** reject.

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
