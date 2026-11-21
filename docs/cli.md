# Command line

Every command accepts `-h`/`--help`. `codesmith --version` prints the version.

| Command | Purpose |
|---|---|
| [`codesmith`](#codesmith) | The interactive UI. |
| [`run`](#codesmith-run) | Run one prompt and print the result. |
| [`generate`](#codesmith-generate) | One model call outside any session's history: text or an image. |
| [`session`](#codesmith-session) | List, archive, export, import, delete, fork, undo, redo, diff sessions; migrate their media. |
| [`permissions`](#codesmith-permissions) | List, edit or remove saved approvals. |
| [`models`](#codesmith-models) | List providers and models. |
| [`auth`](#codesmith-auth) | Save, list or remove API keys. |
| [`commands`](#codesmith-commands) | List commands. |
| [`review`, `calibrate`](#codesmith-review-and-calibrate) | Code review. |
| [`stats`](#codesmith-stats) | Usage statistics. |
| [`worktree`](#codesmith-worktree) | Manage git worktrees. |
| [`mcp`](#codesmith-mcp) | MCP server status and OAuth. |
| [`automation`](#codesmith-automation) | Scheduled and event-driven runs. |
| [`acp`](#codesmith-acp) | Agent Client Protocol for editors. |
| [`serve`](#codesmith-serve) | Local HTTP API. |
| [`debug`](#codesmith-debug) | Inspect config and internals. |
| [`upgrade`](#codesmith-upgrade) | Upgrade to the latest or a given release. |

## `codesmith`

Opens the [interactive UI](interactive-ui.md).

| Flag | Meaning |
|---|---|
| `-m, --model provider/model` | Model to use (otherwise `model` from config). |
| `--agent ID` | Agent to start with (otherwise `default_agent`, else `build`). |
| `-c, --continue` | Continue the most recent session in this folder. |
| `-s, --session ID` | Continue this session. |
| `--prompt TEXT` | Send this prompt as soon as the UI opens. |
| `-w, --worktree[=NAME]` | Start in a new git [worktree](sessions.md#worktrees). |
| `--auto` | Approve every permission request that no rule denies. |
| `--print-logs` | Accepted on every command, but has no effect yet. |

## `codesmith run`

Runs one prompt without the UI and prints the answer. Piped stdin is appended
to the message.

```sh
codesmith run "explain what src/main.rs does"
codesmith run -m anthropic/claude-sonnet-4-5 "add a --verbose flag"
git diff | codesmith run "review this change"
codesmith run --agent plan "how should I restructure the config module?"
codesmith run -f screenshot.png "what's wrong with this layout?"
codesmith run -c "now add tests for it"
codesmith run /standup                  # a custom command, with optional arguments
codesmith run --format json "list the TODOs" | jq .
```

| Flag | Meaning |
|---|---|
| `-m, --model provider/model` | Model to use. |
| `--agent ID` | Agent to use (otherwise `default_agent`, else `build`). `plan` is read-only. |
| `-f, --file PATH` | Attach a file (repeatable). Text goes into the message, and images, PDFs and videos are sent for the model to see. |
| `--format default\|json` | `json` prints one JSON object per event, for scripts. |
| `--auto` | Approve every permission request that no rule denies. |
| `--thinking` | Show the model's reasoning. |
| `-c, --continue` | Continue the most recent session in this folder. |
| `-s, --session ID` | Continue this session. |
| `--title TEXT` | Title for a new session (otherwise one is generated). |
| `--archived` | Archive the session before its prompt runs, so it stays out of session lists even if the run fails or is stopped (scripted or CI runs). |
| `-w, --worktree[=NAME]` | Run in a new git worktree (a name needs the `=`). |

Without `--auto`, requests that a rule marks `ask` are asked on the
terminal. See [answering a request](permissions.md#answering-a-request). The
`question` tool is not offered without a terminal or with `--auto`.

## `codesmith generate`

```sh
codesmith generate [-m provider/model] PROMPT          # text from a chat model; nothing is stored
codesmith generate --session ID PROMPT                 # from that session's context; only its usage is added
codesmith generate -m provider/model --image out.png PROMPT   # an image, saved with its real extension
codesmith generate -m provider/model --questions q.json PROMPT # a decision model's answers, as JSON
```

A decision (classifier) model, models.dev `type: decision`, answers named
questions about the prompt with probabilities rather than text. The
questions file is a JSON array of `{"type": "predicate", "name",
"instructions", "criteria": {"true", "false"}}`, `{"type": "choice",
"name", "instructions", "choices": [{"value", "description"}]}` and
`{"type": "score", "name", "instructions", "levels": [{"label",
"description"}]}`; the answers come back in question order (a predicate's
`probability`, a choice's `choice` and per-value `probabilities`, a
score's probability-weighted `score`), with the cost the provider
reported. System One hosts (TypeSafe, OpenRouter, OpenCode Zen,
llama.cpp-compatible servers) and Google's decisions API are supported.
A provider configured with a `baseURL` and no key, such as LM Studio, is
called without auth.

A call is retried only when the provider never ran it (a refused
connection, a throttle, an overloaded server, at most three times), since
each attempt may be billed. With `--session`, the request is what the
session's next step would send (its system prompt, history and tools,
with any instruction change since then as an update) followed by the
prompt; the history is not changed, and a failure is retried from the
start like a step.

## `codesmith session`

```sh
codesmith session list [--all] [--archived] [--search TEXT] [--json]   # this folder's sessions (--all: every folder; --archived: the archived ones; --search: titles containing TEXT)
codesmith session archive ID              # keep it, out of lists
codesmith session unarchive ID
codesmith session export ID > session.json
codesmith session import session.json     # or - for stdin
codesmith session delete ID               # also deletes its subagent sessions
codesmith session messages [ID]           # its messages, numbered
codesmith session fork  [ID] [--before N] # a copy through the latest message, or up to message N
codesmith session diff  [ID]              # file changes since the latest turn began
codesmith session undo  [ID]              # restore the files and hide the latest turn
codesmith session redo  [ID]              # bring it back
codesmith session migrate-media [ID]      # move inline media into the blob store (default: every session)
```

`messages`, `fork`, `diff`, `undo` and `redo` default to the latest session in this folder. See
[sessions](sessions.md).

## `codesmith permissions`

```sh
codesmith permissions [list] [--all] [--json]   # this project's saved approvals (--all: every project's)
codesmith permissions edit ID --resource 'git diff *' [--action shell]
codesmith permissions remove ID [ID...]
```

What "always" answers saved for the project (see
[saved approvals](permissions.md#saved-approvals)). An edit or removal
applies to running sessions at their next request.

## `codesmith models`

`codesmith models` lists the built-in and configured providers with their
configured models. `codesmith models PROVIDER` asks that provider's API which
models it offers. `--refresh` first fetches the
[model catalog](configuration.md#the-model-catalog-modelsdev) into the cache,
even when background fetching is off.

## `codesmith auth`

```sh
codesmith auth login PROVIDER    # prompts without echo, or reads stdin
codesmith auth list              # where each provider's key comes from
codesmith auth logout PROVIDER
```

| `auth login` flag | Meaning |
|---|---|
| `--oauth` | Sign in through the provider's OAuth instead of pasting a key: `openai` ([Sign in with ChatGPT](configuration.md#sign-in-with-chatgpt)), `xai` ([SuperGrok](configuration.md#xai-supergrok-login)), [`poe`](configuration.md#poe), [`gitlab`](configuration.md#gitlab-sign-in), [`digitalocean`](configuration.md#digitalocean), [`snowflake-cortex`](configuration.md#snowflake-cortex). |
| `--client-id ID` | Your OAuth app's client id, for `github-copilot` (device flow) or with `--oauth`. |
| `--enterprise HOST` | `github-copilot`: a GitHub Enterprise host (`company.ghe.com`). |
| `--profile NAME` | `amazon-bedrock`: sign with this AWS profile instead of saving a key. |
| `--cli` | `azure`: use Azure CLI (`az login`) tokens instead of a key. |
| `--project ID`, `--location L` | `google-vertex`: Application Default Credentials in this project (location default `global`). |
| `--field KEY=VALUE` | Another answer the login asks for, such as `accountId=…` for Cloudflare (asked in the terminal otherwise). |

Keys are saved in the data folder's `auth.json`, readable only by you.
Config and environment variables take precedence over saved keys. See
[providers](configuration.md#saved-keys).

## `codesmith commands`

Lists the commands available as `codesmith run /name ...` and in the UI:
built-in, custom, and MCP prompts (with their argument names), each with its
description. See [skills and commands](skills-and-commands.md).

## `codesmith review` and `calibrate`

```sh
codesmith calibrate [-m provider/model]
codesmith review [TARGET] [--comment] [-m provider/model]
```

`TARGET` is a commit, a range (`a..b`), a branch, or a pull request (`#12`,
which needs `gh`). The default is uncommitted changes. `--comment` posts the
review to the pull request. See [code review](code-review.md).

## `codesmith stats`

Summarizes usage from the local session store: an activity calendar,
sessions, prompts, steps, tokens, tool success, active days and your best
streak.

| Flag | Meaning |
|---|---|
| `--days N` | The last N days (0 or 1: today). |
| `--year Y` | A calendar year (default: this year so far). |
| `--all` | All time. |
| `--project PATH` | Only sessions in this project (`.` for the current one) or folder. |
| `--models` | Add the per-model table. |
| `--tools` | Add the tool reliability table (error rate, median duration). |
| `--cost` | Add cost and token details. |
| `--full` | Every table. |
| `--limit N` | Rows per table (default 10). |
| `--json` | Print the numbers as JSON. |

In a terminal narrower than 68 columns the model and tool tables put each
row on two lines (the name, then its numbers), so nothing is cut off.

Cost needs prices: the [model catalog](configuration.md#the-model-catalog-modelsdev)'s,
or a model's own [`cost`](configuration.md#adding-or-overriding-a-provider).

## `codesmith worktree`

```sh
codesmith worktree list
codesmith worktree create [NAME] [--from REF] [-b BRANCH]   # prints the path
codesmith worktree remove NAME_OR_PATH [--force]
```

See [worktrees](sessions.md#worktrees).

## `codesmith mcp`

```sh
codesmith mcp list           # connect to each server and list its status and tools
codesmith mcp auth [NAME]    # OAuth login for a remote server (opens the browser; without NAME, pick one in a terminal)
codesmith mcp logout NAME    # forget its tokens
```

See [MCP](mcp.md).

## `codesmith automation`

```sh
codesmith automation add --name NAME (--at T | --every I | --cron C | --event SERVER:EVENT) \
  (--prompt TEXT | --code-file FILE) [options]
codesmith automation list [--all]
codesmith automation pause ID
codesmith automation resume ID
codesmith automation remove ID
codesmith automation log ID
codesmith automation serve       # run automations in the foreground (launchd, systemd)
codesmith automation run-due     # fire whatever is due once and exit
```

See [automations](automations.md) for the options.

## `codesmith acp`

Speaks the Agent Client Protocol on stdin and stdout, for editors such as Zed.
See [editors](editors-acp.md).

## `codesmith serve`

```sh
codesmith serve [--host 127.0.0.1] [--port 4096] [--token TOKEN] [--auto]
```

A local HTTP API that always requires a bearer token. Without `--token` (or
`$CODESMITH_SERVER_TOKEN`), the token in the data folder's `serve-token` is
used, and it is generated on first use. Any host other than loopback requires
an explicit `--token`. See [local server](local-server.md).

## `codesmith debug`

| Command | Shows |
|---|---|
| `debug paths` | The config, data, tool-output and scratch folders. |
| `debug config` | The merged config and the files it came from. |
| `debug agents` | Every agent, its tools and its final permission rules. |
| `debug plugins` | The plugins compiled into this binary, and whether config turns them off. |
| `debug model [provider/model[#variant]]` | What a model resolves to: provider, URL, protocol, limits, extra body, provider options (JSON), the selected variant, and every variant with its options. |
| `debug prompt [-m MODEL] [--agent ID]` | The system prompt an agent would get here. |

## `codesmith upgrade`

```sh
codesmith upgrade [VERSION] [-m, --method binary|brew|cargo] [--check]
```

Also `codesmith update`. Without `VERSION` it upgrades to the latest
published release of [reka18/codesmith](https://github.com/reka18/codesmith/releases)
(drafts and prereleases are never "latest"); `VERSION` may have a leading
`v`, and may be older than the running one (a rollback). `--check` only says
whether another release is out.

How it upgrades depends on where this copy lives:

| Installed with | `codesmith upgrade` |
|---|---|
| `install.sh` or a release tarball | Downloads `codesmith-<version>-<target>.tar.gz` and `SHA256SUMS`, checks the tarball against it, checks that the binary in it runs and reports `<version>`, then renames it over the running binary (so a failure at any step leaves the old one untouched). |
| Homebrew (a `Cellar/codesmith` path) | Changes nothing; tells you to run `brew upgrade reka18/tap/codesmith`. |
| `cargo install` (`~/.cargo/bin`) | Changes nothing; prints the `cargo install` command. |
| A source checkout (`target/...`) | Changes nothing. |
| A system folder (`/usr/bin`, `/usr/lib`, the Nix or Guix store, Snap, Flatpak, MacPorts' `/opt/local`) | Changes nothing; update it with the package manager that owns it. |

A copy found in Homebrew's, cargo's or a system package manager's folders is
never replaced, whatever `--method` says. If the method can't be detected at
all, pass `--method binary` for a copy you installed from a tarball. The
folder holding the binary must be writable: the new binary is written to a
temporary folder beside it, so the final rename is atomic on one filesystem,
and that folder is removed however the upgrade ends.

The upgrade makes plain GET requests with a `codesmith/<version>` user agent,
to `https://github.com/reka18/codesmith/releases/...` only (and the download
host GitHub redirects to), over HTTPS. `$CODESMITH_RELEASES_URL` points it at
a mirror with the same layout (`<url>/latest` redirecting to
`<url>/tag/v<version>`, and `<url>/download/v<version>/<file>`).

### Update checks

Off by default. OpenCode checks for updates on start by default; CodeSmith
only does if the **global** config (`codesmith.json[c]` in the config folder,
never a project's) opts in:

```jsonc
{ "update": "notify" }   // or "disable" (the default); OpenCode's "autoupdate" is read too
```

With `notify`, the interactive UI checks at most once a day in
the background, keeps the answer in the cache folder (`update-check.json`),
and after the UI exits prints a line such as
`CodeSmith 0.3.0 is available (this is 0.2.0). Run: codesmith upgrade` (or
the `brew`/`cargo` command for those installs). It never waits for the
network. `"auto"` is accepted, but CodeSmith never replaces itself unattended:
it behaves as `notify`. `$CODESMITH_DISABLE_AUTOUPDATE=1` turns checks off
whatever the config says.
