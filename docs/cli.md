# Command line

Every command accepts `-h`/`--help`. `codesmith --version` prints the version.

| Command | Purpose |
|---|---|
| [`codesmith`](#codesmith) | The interactive UI. |
| [`mini`](#codesmith-mini) | The interactive UI in the terminal's scrollback. |
| [`run`](#codesmith-run) | Run one prompt and print the result. |
| [`generate`](#codesmith-generate) | One model call outside any session's history: text or an image. |
| [`session`](#codesmith-session) | List, export, import, delete, undo, redo, diff sessions; migrate their media. |
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

## `codesmith mini`

Opens the interactive UI in [mini mode](interactive-ui.md#mini-mode): the
transcript goes into the terminal's scrollback, with a live footer for the
prompt. Piped stdin, then `--prompt`, is the first message.

| Flag | Meaning |
|---|---|
| `-m, --model provider/model` | Model to use. |
| `--agent ID` | Agent to start with. |
| `-c, --continue` | Continue the most recent session in this folder. |
| `-s, --session ID` | Continue this session. |
| `--prompt TEXT` | Send this prompt as soon as the UI opens. |
| `--auto` | Approve every permission request that no rule denies. |
| `--replay`, `--no-replay` | Write a continued session's history, and the transcript again after a resize (default: `tui.mini.replay`, on). |
| `--replay-limit N` | The newest N prompts and answers a replay writes (default: 200). |

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
| `--agent ID` | Agent to use (default `build`). `plan` is read-only. |
| `-f, --file PATH` | Attach a file (repeatable). Text goes into the message, and images, PDFs and videos are sent for the model to see. |
| `--format default\|json` | `json` prints one JSON object per event, for scripts. |
| `--auto` | Approve every permission request that no rule denies. |
| `--thinking` | Show the model's reasoning. |
| `-c, --continue` | Continue the most recent session in this folder. |
| `-s, --session ID` | Continue this session. |
| `--title TEXT` | Title for a new session (otherwise one is generated). |
| `--archived` | Archive the session when the run ends, so it stays out of session lists (scripted or CI runs). |
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
models it offers.

## `codesmith auth`

```sh
codesmith auth login PROVIDER    # prompts without echo, or reads stdin
codesmith auth list              # where each provider's key comes from
codesmith auth logout PROVIDER
```

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

Cost needs prices, since nothing is fetched from a catalog: give each model a
[`cost`](configuration.md#adding-or-overriding-a-provider).

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
codesmith mcp auth NAME      # OAuth login for a remote server (opens the browser)
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
