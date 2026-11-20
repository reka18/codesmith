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
starts over. New sessions get a short generated title. `codesmith run
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

## Background commands

A shell command started with `background: true` (or one that outlives its
timeout) keeps running after the model answers, and the turn ends there, as
in OpenCode. When the command finishes, its output is reported to the model
once:

- **An idle session wakes up.** In the [UI](interactive-ui.md#background-commands)
  and the [server](local-server.md#background-commands), the session is
  resumed as a new turn of its own: a `background command shell_1 finished`
  notice, then the model acting on the result. No prompt is needed.
- **A prompt sent first reports it.** If you send a message before the
  command finishes, or while it finishes, that turn carries the result, and
  the session is not woken for it again.
- **One-shot runs wait.** `codesmith run`, subagents and automations wait for
  their commands and resume the model with each result before they end. A
  command that never exits (a dev server) keeps them waiting until they are
  interrupted.

A session's commands stop with it: when the program exits, when a turn is
interrupted or aborted (the session reloads), and when you switch session,
agent or model, or undo, in the UI. A command's output file stays, and is
named in the result.

## Export, import and delete

```sh
codesmith session list [--all] [--json]
codesmith session export ID > session.json
codesmith session import session.json        # or: ... | codesmith session import -
codesmith session delete ID                  # and its subagent sessions
```

An export is a self-contained JSON file with the session and its messages.

## Long sessions: compaction

When a conversation nears the model's context window, older history is
replaced by a structured summary, and the newest exchanges are kept verbatim.
Run `/compact` (**Ctrl+X c**) to do it now. Compaction needs the model's
`limit.context` to know when to start. See
[compaction settings](configuration.md#compaction).

Images are large, so once a conversation's images pass 25 MiB, the oldest
are replaced by a note until 15 MiB remain.

## Usage statistics

`codesmith stats` summarizes your sessions: an activity calendar, prompts,
tokens, tool success and streaks, plus per-model, tool and cost tables on
request. See [`codesmith stats`](cli.md#codesmith-stats).

## Worktrees

`codesmith -w` (or `codesmith run -w "..."`) starts the session in a new git
worktree, so the agent works on a separate checkout while yours stays as it
is. `-w NAME` names it, otherwise it gets a name like `swift-otter`.

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
