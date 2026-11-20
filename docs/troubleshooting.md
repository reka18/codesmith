# Troubleshooting

## Inspecting what CodeSmith sees

| Command | Answers |
|---|---|
| `codesmith debug paths` | Where are config, data, tool output and scratch files? |
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
default. Answer **a** to allow that folder for the session, or add an
`allow` rule for it. See [paths outside the project](permissions.md#paths-outside-the-project).

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

Everything CodeSmith stores lives in the data folder
(`~/.local/share/codesmith`): the session database, snapshots, saved keys
(`auth.json`), MCP tokens (`mcp-auth.json`), the server token, worktrees,
tool output and logs. Deleting a file there removes just that piece.
Deleting `codesmith.db` removes all sessions and automations.

If `auth.json` or `mcp-auth.json` is damaged so that it no longer parses,
saving a key or a token stops with an error that names the file instead of
replacing it, and leaves a copy beside it as `auth.json.bak` (or
`mcp-auth.json.bak`). Fix the file or delete it, then log in again.
