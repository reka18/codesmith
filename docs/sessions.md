# Sessions

Every conversation is a session, saved to a local SQLite database:
`~/.local/share/codesmith/codesmith.db` (or `$CODESMITH_DB`). Nothing is
stored anywhere else. Sessions belong to the folder they were started in.
The UI, `codesmith run`, [`serve`](local-server.md) and
[ACP](editors-acp.md) all share them.

## Continuing

```sh
codesmith -c                    # the latest session in this folder, in the UI
codesmith -s ses_01J...         # a specific one
codesmith run -c "and now the docs"
```

Inside the UI, `/sessions` (or **Ctrl+X l**) opens a picker, and `/new`
starts over; each session opens in a [tab](interactive-ui.md#session-tabs)
of its own, and the open tabs are kept for the next start. New sessions get a short generated title ([which model writes it](configuration.md#session-titles)). `codesmith run
--title` sets one yourself.

## Undo and redo

At the start and end of each turn, CodeSmith snapshots the project into a
private git repository under the data folder (`snapshot/`). It never touches
your own `.git`, it works in folders that are not git repositories, and it
respects `.gitignore`.

- `/undo` (**Ctrl+X u**), or `codesmith session undo [ID]`, restores the
  files the latest turn changed and hides that turn from the conversation.
  **Only the files that turn changed are restored**, so edits you made
  yourself since then, to other files, stay.
- `/redo` (**Ctrl+X r**), or `codesmith session redo [ID]`, brings the turn
  and its changes back, until you send a new message.
- `codesmith session diff [ID]` shows the file changes since the latest turn
  began.

Set `"snapshots": false` to turn snapshots off; undo then has nothing to
restore. Snapshots are never taken when the project root is your home folder
or `/`.

## Forking

A fork is a new session with a copy of another's history that then goes its
own way; the original is left as it was.

- `/fork` in the UI picks where: the whole session, or before one of your
  prompts, which comes back in the input to edit and send. The fork opens
  in a [tab](interactive-ui.md#session-tabs) of its own (with tabs off, in
  place of the original).
- `codesmith session fork [ID]` forks through the latest message;
  `--before N` stops before message `N` (`codesmith session messages [ID]`
  lists them by number).
- [`serve`](local-server.md) has `POST /sessions/{id}/fork`, and
  [ACP](editors-acp.md) `session/fork`.

A fork is titled after its source (`Title (fork #1)`), starts with no usage,
and keeps the source's attached context and newest instructions. It leaves
out what was undone and a step still running. Before its first prompt the
model is told the conversation is a fork: work started before it (background
commands, subagents) belongs to the original, and files and branches are
shared with it.

## Archiving

An archived session keeps its history but stays out of session lists, the
UI's `/sessions` picker and `-c`, as in OpenCode.

```sh
codesmith session archive ID
codesmith session list --archived      # the archived ones
codesmith session unarchive ID
codesmith run --archived "check the build"   # a scripted run that stays out of lists
```

[`serve`](local-server.md) archives with `PATCH /sessions/{id} {archived}`.
A fork of an archived session is not archived.

## Shell commands

`!command` at the prompt (or `POST /sessions/{id}/shell` on
[`serve`](local-server.md)) runs a command as you, without the model. Its
output and exit code are recorded in the session, so the model sees them
with your next message. See [the UI](interactive-ui.md#shell-commands).

## Background commands

A shell command started with `background: true` (or one that outlives its
timeout) keeps running after the model answers, and the turn ends there, as
in OpenCode. When the command finishes, its output is reported to the model
once:

- **An idle session wakes up.** In the [UI](interactive-ui.md#background-commands),
  the [server](local-server.md#background-commands) and
  [editors over ACP](editors-acp.md#background-commands), the session is
  resumed as a new turn of its own: a `background command shell_1 finished`
  notice, then the model acting on the result. No prompt is needed.
- **A prompt sent first reports it.** If you send a message before the
  command finishes, or while it finishes, that turn carries the result, and
  the session is not woken for it again.
- **One-shot runs wait.** `codesmith run`, subagents and automations wait for
  their commands and resume the model with each result before they end. A
  command that never exits (a dev server) keeps them waiting until they are
  interrupted.

A session's commands last as long as the session, as in OpenCode: they keep
running when a later turn is interrupted, cancelled or aborted, when you
switch agent, mode, model or variant, and across undo and redo (they are not
file changes), and are reported when they finish. In the UI they keep
running in a [tab](interactive-ui.md#session-tabs) that is not shown, whose
session wakes up for them as the shown one does. They stop when the session
ends: when you close its tab in the UI (or, with tabs off, start or switch
to another session), delete or close it over the server or ACP, when the
program exits, and when you stop them explicitly (abort on an idle server
session, or the server's cancel route for one command). A command's output
file stays, and is named in the result.

Before tabs, switching sessions in the UI stopped the old session's
commands, since it watched one session only. Each tab now owns its
session's turn, commands and wake-ups, so they run on while the tab is
open, shown or not.

Over the [server](local-server.md#the-shell-daemon), commands run in its
shell daemon, so a graceful restart
([`POST /shell-daemon/handoff`](local-server.md#restarting-without-stopping-commands))
keeps them running: the next server takes each session's commands back as
its background jobs and reports them when they finish, including a command
a tool call was waiting on (the call itself ends as interrupted). Without a
handoff they stop with the server. The UI, `codesmith run` and ACP run
commands in their own process, which they end with.

## Export, import and delete

```sh
codesmith session list [--all] [--json]
codesmith session export ID > session.json
codesmith session import session.json        # or: ... | codesmith session import -
codesmith session delete ID                  # and its subagent sessions
```

An export is a self-contained JSON file with the session and its messages,
its media inline. An import (and a fork) stores that media in the blob store
again (see [media limits](configuration.md#media-limits));
`codesmith session migrate-media [ID]` does the same for sessions whose
records still hold their media inline.

## Long sessions: compaction

When a conversation nears the model's context window, older history is
replaced by a structured summary, and the newest exchanges are kept verbatim.
Run `/compact` (**Ctrl+X c**) to do it now. Compaction needs the model's
`limit.context` to know when to start. See
[compaction settings](configuration.md#compaction).

The summary request is the session's own request (the same system prompt,
tools and messages) with the summary instructions added at the end, so the
provider reads it from the prompt cache instead of processing the whole
conversation again. It is sized with the provider's token count from the
latest response. When the conversation no longer fits beside the summary,
the history before the kept turns is sent instead, and when even that does
not fit, a shortened transcript of it (oldest tool output first) without the
tool definitions; if the provider still rejects it as too long, the
transcript is shortened further and sent again, up to four times.

Once a conversation is within 60,000 tokens of that point (or half the
room left, on a small window), CodeSmith writes the summary in the
background, from everything before the latest response: the prefix the
provider just cached. When compaction is due it swaps that summary in and
keeps what came after it verbatim, so the session doesn't wait for a
summary. A background summary starts only while the latest response's
prompt is still in the cache (five minutes, or the model's `cache.ttl`), and
for the model that wrote it; one that falls too far behind (a tail of more
than 40% of the budget) or fails is replaced from a newer response, at
most twice between compactions. Turn it off with `"compaction":
{"background": false}`.

Models whose provider compacts conversations itself (OpenAI Responses) can
use that instead of a summary: see [native
compaction](configuration.md#native-compaction).

When the turn in progress is summarized too, CodeSmith tells the model to
carry on with a system update (see
[provider options](configuration.md#provider-options-and-variants)); it is
not shown or counted as a prompt. Sessions that contain one can't be opened
by CodeSmith builds older than this feature.

An idle session's prompt cache expires after a few minutes; with
[`warming`](configuration.md#prompt-cache-warming) on, CodeSmith sends small
keep-alive requests so the next prompt still reads it.

Images are large, so once a conversation's images pass 25 MiB, the oldest
are replaced by a note until 15 MiB remain.

## Usage statistics

`codesmith stats` summarizes your sessions: an activity calendar, prompts,
tokens, tool success and streaks, plus per-model, tool and cost tables on
request. See [`codesmith stats`](cli.md#codesmith-stats).

## Worktrees

`codesmith -w` (or `codesmith run -w "..."`) starts the session in a new git
worktree, so the agent works on a separate checkout while yours stays as it
is. `-w=NAME` (or `--worktree=NAME`) names it, otherwise it gets a name
like `swift-otter`. As in OpenCode, the name needs the `=`: in
`codesmith run -w "fix it"`, `fix it` is the message.

- New worktrees go under the data folder's `worktree/<project>/`, or under
  `worktree.directory` (an absolute path, or one relative to the project
  root; `~` is not expanded).
- They start detached at `HEAD`, and the session opens in the same subfolder
  you started from.
- CodeSmith config that git doesn't track (an uncommitted `codesmith.json`, a
  `.codesmith/` folder) is copied in.
- `worktree.start` runs a setup command in each new worktree, with
  `$CODESMITH_WORKTREE_BASE` and `$CODESMITH_WORKTREE_PATH` set.

```jsonc
{ "worktree": { "directory": "/Users/me/worktrees", "start": "npm ci" } }
```

Manage them with `codesmith worktree list`, `worktree create [NAME] [--from
REF] [-b BRANCH]` and `worktree remove NAME_OR_PATH [--force]`.
In the UI, `/worktrees` opens, makes and removes them and moves the session
to one, and the [open menu](interactive-ui.md#projects-worktrees-and-other-folders)
lists each project's worktrees. The model can list, create and remove them
too, with the [`codesmith` tool](tools.md#codesmith).
