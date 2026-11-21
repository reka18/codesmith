# Troubleshooting

## Inspecting what CodeSmith sees

| Command | Answers |
|---|---|
| `codesmith debug paths` | Where are config, data, tool output, scratch and cache files? |
| `codesmith debug config` | Which config files loaded, and what did they merge to? |
| `codesmith debug model [provider/model]` | Which URL, protocol, limits and extra body does a model use? |
| `codesmith debug agents` | Which tools does each agent have, and which rules apply, in order? |
| `codesmith debug prompt [--agent ID]` | What system prompt (with instructions and skills) does an agent get here? |
| `codesmith debug plugins` | Which plugins are compiled in, and are any turned off? |
| `codesmith auth list` | Where does each provider's key come from? |
| `codesmith mcp list` | Do MCP servers connect, and what tools do they offer? |

## Common problems

**`no model configured`.** Pass `-m provider/model`, or set `"model"` in
your [config](configuration.md). There is no default model.

**`unknown provider`.** The part before the first `/` must be a
[built-in provider](configuration.md#built-in-providers) or an entry under
`providers`.

**`provider "x" has no settings.baseURL`.** A custom provider needs
`settings.baseURL`.

**Authentication errors (401/403).** Check `codesmith auth list`. Remember
the order: `settings.apiKey`, then environment variables, then saved keys.
An empty or stale variable in your shell takes precedence over a key saved
with `auth login`.

**`OpenAI Chat stream ended without finish_reason`.** CodeSmith treats a
Chat stream that stops without a finish reason as cut short and retries the
step, as OpenCode does. If a server always ends its streams that way, set
`"compatibility": {"requireFinishReason": false}` on the provider or model
(see [compatibility overrides](configuration.md#compatibility-overrides)).
Malformed stream events (fields of the wrong type, a tool call without an id)
fail the step as invalid provider output, without a retry.

**Responses requests fail over the WebSocket.** The built-in `openai`
provider, `azure` on its v1 API and `xai` set to `openai-responses` carry
session steps over the Responses WebSocket. A proxy or gateway that can't take it usually falls back to HTTP
on its own; to stay on HTTP from the start, set
`"settings": {"transport": "http"}` on the provider (see
[Responses over WebSocket](configuration.md#responses-over-websocket)).

**`git push` or `git fetch` hangs or fails asking for a passphrase or
password.** Commands run without a terminal to answer on, and CodeSmith
does not relay git's or ssh's prompts to the UI (an answer there could be
read by the model, which runs the commands; see
[PARITY.md](../crates/codesmith/PARITY.md#ssh-askpass)). Load the key into
`ssh-agent` (`ssh-add`) before starting CodeSmith, and use a git credential
helper for HTTPS remotes, so nothing asks.

**A config file is rejected.** CodeSmith stops on any file that doesn't
parse, and names it, rather than skip it and maybe drop a `deny` rule. JSONC
comments and trailing commas are fine. Check `{env:}` values that contain
quotes.

**Every command asks for approval / nothing asks.** Rules are last-match-wins
across all config files, global first, then project files. A project's
`codesmith.json` can override your global rules. `codesmith debug agents`
shows the final list for each agent. See [permissions](permissions.md).

**A shell command is refused as "cannot be checked".** CodeSmith refuses
commands it can't split into visible commands (computed command names,
`eval "$x"`, arithmetic on text) whenever any rule could deny or ask about
some command. Rewrite the command so each part is visible, or run it
yourself.

**Access outside the project keeps asking.** `external_directory` asks by
default. Answer **a** to allow that folder (or its repository) for the
project from then on, or add an `allow` rule for it. See [paths outside the project](permissions.md#paths-outside-the-project).

**Compaction never happens.** It needs the model's `limit.context`. See
[compaction](configuration.md#compaction).

**The model says it can't see an image.** The model's `modalities` may say
text only, or the image was too large to fit the `media` limits. See
[images and PDFs](tools.md#images-and-pdfs).

**An MCP server doesn't connect.** Look at `log/mcp-<name>.log` in the data
folder (local servers' stderr goes there). Raise `timeout.startup` for slow
starters. Remote servers that need OAuth show as needing authorization: run
`codesmith mcp auth <name>`.

**Language server errors don't show.** `lsp` is off by default. The server
must be on `PATH` (or in `node_modules/.bin`), and one that failed to start
is not retried until you restart CodeSmith.

**Language servers miss changes made outside CodeSmith (Linux).** inotify
needs a watch per directory, and CodeSmith takes at most half of
`fs.inotify.max_user_watches`; directories past that are not watched. Raise
the limit (`sudo sysctl fs.inotify.max_user_watches=524288`), or leave large
generated folders out with [`watcher.ignore`](configuration.md#file-watching).

**`codesmith serve` returns 401 or 403.** Send
`Authorization: Bearer $(cat ~/.local/share/codesmith/serve-token)` (or the
`--token` you started it with). Requests with a foreign `Origin`, a `Host`
other than the server's own address, or a body that isn't JSON are refused.
See [local server](local-server.md).

**Undo says snapshots are not kept, or there is nothing to undo.**
Snapshots need `git` on `PATH`, and `"snapshots"` must not be `false`. They
are never taken when the project root is your home folder or `/` (a folder
without `.git` above it counts as its own root), since that would copy far too
much.

## Logs

`--print-logs` is accepted by every command for compatibility, but it
currently prints nothing. Error messages go to stderr. MCP servers' output
goes to `log/mcp-<name>.log` in the data folder.

## Starting fresh

Almost everything CodeSmith stores lives in the data folder
(`~/.local/share/codesmith`): the session database, snapshots, saved keys
(`auth.json`), MCP tokens (`mcp-auth.json`), the server token, worktrees,
plan files, the UI's prompt history and model choices, tool output and
logs. Deleting a file there removes just that piece. Deleting
`codesmith.db` removes all sessions and automations. The cache folder
(`~/.cache/codesmith`: the model catalog, downloaded skills, the update
check) can be deleted at any time, and the shell daemon keeps its runtime
files under `~/.local/state/codesmith`.

If `auth.json` or `mcp-auth.json` is damaged so that it no longer parses,
saving a key or a token stops with an error that names the file instead of
replacing it, and leaves a copy beside it as `auth.json.bak` (or
`mcp-auth.json.bak`). Fix the file or delete it, then log in again.
