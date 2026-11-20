# Local server

`codesmith serve` exposes sessions to other programs on this machine, such as
an editor plugin or a script, over HTTP. It is opt-in and local. It doesn't
talk to any hosted service.

```sh
codesmith serve                        # 127.0.0.1:4096
codesmith serve --port 5000 --auto
```

| Flag | Meaning |
|---|---|
| `--host ADDR` | Address to listen on (default `127.0.0.1`). Anything but loopback requires `--token`. |
| `--port N` | Port (default `4096`). |
| `--token TOKEN` | The bearer token clients must send (also `$CODESMITH_SERVER_TOKEN`). |
| `--auto` | Approve every permission request that no rule denies, instead of asking the client. |

## Authentication

The API runs shell commands and edits files, so **every request needs
`Authorization: Bearer <token>`**. The token is `--token` or
`$CODESMITH_SERVER_TOKEN`. Without either, the server uses the token in the
data folder's `serve-token` file. That file is readable only by you, is
created with a random token on first use, and is printed at startup, so local
clients can read it from there:

```sh
TOKEN=$(cat ~/.local/share/codesmith/serve-token)
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:4096/health
```

A token given as `--token TOKEN` is part of the command line, which other
local users and processes can read (with `ps`, for example). Prefer the
`serve-token` file or `$CODESMITH_SERVER_TOKEN`. CodeSmith takes the
variable out of its environment when it starts and removes it again from
every process it runs (shell commands, MCP and language servers,
formatters, git, Code Mode), so nothing it starts can use the token to
drive the server.

Web pages can reach loopback too, so the server also refuses:

- requests whose `Host` isn't the address it listens on (`localhost`,
  `127.0.0.1` or `[::1]` with its port, on loopback), which blocks DNS
  rebinding;
- requests with any `Origin` other than its own;
- request bodies that aren't `application/json`, and bodies over 8 MiB.

Only sessions of the folder the server was started in are visible, and
permission and question ids are random.

## Routes

| Route | |
|---|---|
| `GET /health` | Version, directory, model. |
| `GET /sessions` | List sessions. |
| `POST /sessions {agent?, model?}` | Create a session. |
| `GET /sessions/{id}` | Session info and messages. |
| `DELETE /sessions/{id}` | Delete a session. |
| `POST /sessions/{id}/messages {text, attachments?}` | Run a turn. The response streams Server-Sent Events. |
| `GET /sessions/{id}/events` | Follow the session: every turn's events from now on, as Server-Sent Events, including the turns the server starts itself ([background commands](#background-commands)). |
| `POST /sessions/{id}/abort` | Stop the running turn, or the session's background commands when it is idle ([details](#aborting)). |
| `POST /sessions/{id}/permissions/{rid} {reply, message?}` | Answer a permission request: `once`, `always` or `reject` (with optional feedback). |
| `POST /sessions/{id}/questions/{qid} {answers}` or `{dismiss: true}` | Answer the agent's questions. `answers` is one list of strings per question. |
| `POST /sessions/{id}/undo`, `POST /sessions/{id}/redo` | Undo or redo the latest turn's file changes. |
| `GET /sessions/{id}/jobs` | The session's running shell commands: `id`, `command`, `pid`, `background`, and `output` (the file their output streams to) for background ones. |
| `POST /sessions/{id}/background` | Move the running turn's foreground commands to the background ([details](#stopping-and-backgrounding-commands)). |
| `POST /sessions/{id}/jobs/{jid}/cancel` | Stop one running command ([details](#stopping-and-backgrounding-commands)). |
| `GET /sessions/{id}/environment`, `PUT /sessions/{id}/environment {variables}` | The environment the session's shell commands run in ([details](#session-environment)). |
| `GET /events` | `filesystem.changed` events for the server's directory, as Server-Sent Events ([details](#file-watching)). |
| `POST /fs/watch {path}`, `POST /fs/unwatch {path}` | Watch, or stop watching, one file of the directory ([details](#file-watching)). |
| `GET /pty`, `POST /pty {command?, args?, cwd?, title?, env?, size?}` | List or create [terminals](#terminals). |
| `GET /pty/{pty}`, `PUT /pty/{pty} {title?, size?}`, `DELETE /pty/{pty}` | Inspect, rename or resize, remove a terminal. |
| `POST /pty/{pty}/connect-token`, `GET /pty/{pty}/connect` | A ticket for, and the WebSocket of, a terminal ([details](#attaching)). |
| `GET /sessions/{id}/terminals`, `POST /sessions/{id}/terminals {...}` | A session's terminals, removed with the session. |
| `GET /sessions/{id}/terminals/read?lines=N` | What the session's current terminal shows, as text. |

## Turn events

Each Server-Sent Event is a JSON object with a `type`:

| Type | Fields |
|---|---|
| `text` | `delta` |
| `reasoning` | `delta` |
| `media` | `mime`, `name` (or `null`), and `size` in bytes, or `url` for a remote file: an image or other file the model generated. Only its metadata is sent; read the bytes from the session's messages (`GET /sessions/{id}`). |
| `tool_start` | `id`, `name`, `input` |
| `tool_progress` | `id`, `metadata`: `{"shellID": "shell_1"}` once a `shell` call's command starts |
| `tool_output` | `id`, `delta`: output a foreground command printed, a few times a second while its call waits (under a flood, only the latest 64 KiB of each interval) |
| `tool_end` | `id`, `name`, `output`, `error` |
| `step_end` | `reason` (`stop`, `length`, `tool-calls`, `content-filter`, `error` or `unknown`), `finish` (`normalized`, the providers `raw` reason and, for a content filter, its `category` and `explanation`), `usage` |
| `retry` | `attempt`, `error`, `wait_ms` |
| `compacted` | `before`, `after` |
| `todos` | `todos` |
| `notice` | `message` |
| `permission` | `id`, `action`, `resources`, `save`, `title`, and `session` when a [subagent](#subagents) asks. Answer it with the permissions route. |
| `question` | `id`, `prompts`. Answer it with the questions route. |
| `child_start` | `session`, `parent`, `depth`, `title`, `agent`, `tool`: a [subagent](#subagents) started |
| `child_text` | `session`, `delta`: text the subagent wrote |
| `child_reasoning` | `session`, `delta` |
| `child_end` | `session`, `status` (`completed`, `interrupted` or `failed`), `error` |
| `done` | `error` (set if the turn failed or was aborted), `usage`, `title`; `resumed: true` on a turn the server started for a finished background command |

An open permission request or question is shown again to every client that
starts following the session (`GET /sessions/{id}/events`) while it is
open, so a client that lost the turn's stream can reconnect and answer it.
When no client can see it any more (the turn's stream closed and no one
follows) for 30 seconds, a permission request is refused ("No client is
connected to approve this.") and a question is dismissed, so the turn does
not wait forever.

### Subagents

When the model delegates with the `subagent` tool, the subagent runs as a
child session inside the call, and its activity streams on the same turn
(and to followers), as OpenCode's child-session events do:

1. `tool_start` for the `subagent` call (in the parent, no `session`).
2. `child_start`: `session` is the child session's id, `parent` the session
   that started it (this one, or a subagent for nested subagents), `depth`
   1 for a subagent of this session, `title` the call's description,
   `agent` the agent it runs, and `tool` the id of the `subagent` call.
3. The child's own calls as `tool_start`, `tool_progress`, `tool_output`
   and `tool_end`, with `session` set to the child and `name` prefixed with
   its agent (`general › shell`). Its permission requests carry `session`
   too. Its text and reasoning come as `child_text` and `child_reasoning`,
   not as the parent's `text`.
4. `child_end` with the `status`, and `error` when it failed.
5. `tool_end` for the `subagent` call, whose `output` is the subagent's
   report to the parent.

Every event names the innermost subagent running. The child session is
stored like any session (with the parent as its `parent_id`), so
`GET /sessions/{session}` shows its messages once it is over.

`GET /sessions/{id}` returns the session's info, `messages` and `todos`,
`busy` (a turn is running) and, while idle, `background`: how many
background commands are still running.

## Background commands

A turn ends when the model answers, even while shell commands it started
with `background: true` still run. When one finishes while the session is
idle, the server resumes the session by itself, as OpenCode does: a new turn
that starts with a `background command shell_1 finished (…)` notice and lets
the model act on the result. Nobody's request started it, so its events go
only to clients following `GET /sessions/{id}/events`, and it ends with a
`done` event carrying `resumed: true`. Its permission requests and questions
go to those followers too; with none following, a permission request is
refused (unless `--auto` or a rule settles it) and questions are dismissed.
`GET /sessions/{id}` shows its messages once it is over.

A message posted before the command finishes runs as usual and reports the
command itself, so the session is not resumed for it again. While a resumed
turn runs, the session is busy like any other turn: a message gets `409`, and
abort stops it. Follow the events stream to know when it is idle again.

`GET /sessions/{id}/events` streams every turn of the session, including
those started with `POST /messages` (their `done` comes on both streams),
until the client disconnects or the session is deleted. The followers stay
attached across aborts and undo.

## Stopping and backgrounding commands

`POST /sessions/{id}/background` is OpenCode's "move to background": every
shell command the running turn is waiting on keeps running in the
background, its call returns at once (`Command moved to the background
…`), and before its next step the model is told the user moved the work, so
it carries on instead of waiting. The command no longer has a timeout and
is reported when it finishes, like any background command. The response
lists what moved: `{"backgrounded": [{"id": "shell_1", "type": "shell",
"title": "<command>"}]}`.

`POST /sessions/{id}/jobs/{jid}/cancel` stops one command (SIGTERM to its
process group, then SIGKILL after 3 seconds) and answers `{"cancelled":
true}`, or `false` when no such command is running. A background command is
reported to the model as `<shell id="…" state="cancelled" …>Cancelled</shell>`
and its output file is kept; a foreground command's call fails with
`Command cancelled`.

## Session environment

`PUT /sessions/{id}/environment {"variables": {"NAME": "value", …}}` sets
the environment the session's shell commands run in, as OpenCode's route of
the same name does: a command then gets exactly these variables (plus
CodeSmith's markers, `AGENT`, `CODESMITH`, `AI_AGENT` and
`CODESMITH_SESSION_ID`) instead of the server's own. A client on another
terminal can send its own environment, so commands see its `PATH` and
toolchains. `{"variables": null}` goes back to the server's environment.
It applies to commands started afterwards, lasts while the server holds the
session (across aborts, undo and redo), and goes with `DELETE`.

## Aborting

`POST /sessions/{id}/abort` stops the running turn and reloads the session
from the store. Its background commands keep running (as in OpenCode) and
still resume the session when they finish; undo and redo keep them too. The
response is
`{"aborted": true}`, and the turn's stream (and every follower) gets `done`
with `error: "aborted"`. A tool parked in a blocking call only notices the
abort when that call returns: if the turn has not stopped within 10 seconds,
the response is `{"aborted": true, "stopping": true}` and the session stays
busy (`409` for new messages) until the turn ends and the session reloads.

With no turn running, abort stops the session's background commands and the
wait to resume for them, and answers `{"aborted": false,
"background_stopped": N}`; their results are not reported. With nothing to
stop it answers `{"aborted": false}`. `DELETE /sessions/{id}` also stops a
running turn, the background commands and the wait, and ends the
followers' streams.

## File watching

As OpenCode's server does, the server tells clients about files that change,
on `GET /events`:

```
event: filesystem.changed
data: {"type":"filesystem.changed","location":"/path/to/project","file":"/path/to/project/src/a.ts","event":"change"}
```

`event` is `add`, `change` or `unlink`. Two things are watched:

- the project's branch metadata, `.git/HEAD` (in the real Git directory,
  through a worktree's or a symlinked `.git`) or `.hg/branch`, from when the
  server starts, so a client sees branch switches; and
- files a client asks for with `POST /fs/watch {"path": "src/a.ts"}` (relative
  to the directory). The file must exist inside the directory, or the answer
  is 404. A watch lasts 24 hours; watching the file again renews it, and
  `POST /fs/unwatch` ends it. The directory holds at most 256 watches; one
  more drops the watch closest to expiring. An atomic save (write a temp
  file, rename it over) counts as a change.

With file watching turned off (`CODESMITH_DISABLE_FILEWATCHER=1`), `/fs/watch`
answers 503 and `/events` stays quiet. `watcher.ignore` naming `.git` stops the
branch watch. See [file watching](configuration.md#file-watching).

## Terminals

As OpenCode's server does, the server runs terminals for its clients: a
program (your shell by default) on a pseudo-terminal that keeps running
between requests, for a client to type into and show. The model has no
terminal tool; terminals are for the people using the client.

`POST /pty` starts one and answers with its info:

```json
{"id": "pty_…", "title": "Terminal 3f2a", "command": "/bin/zsh", "args": ["-l"],
 "cwd": "/path/to/project", "status": "running", "pid": 4242,
 "userTitle": null, "size": {"cols": 80, "rows": 24}}
```

- `command` defaults to `shell` from config, else `$SHELL`, else `/bin/sh`.
  A login-capable shell (`bash`, `zsh`, `sh`, `dash`, `ksh`, `fish`) gets
  `-l` after `args`, as in OpenCode.
- `cwd` defaults to the server's directory; a relative one is taken from it.
  It must be an existing directory (400 otherwise).
- `env` is added to the server's environment, with `TERM=xterm-256color`
  and `CODESMITH_TERMINAL=1`. The server token is never passed on.
- `size` is `{cols, rows}`, 80 × 24 by default.

`PUT /pty/{pty}` with `{"title": "build"}` sets the title (`null` goes back
to the one it was created with) and `{"size": {"cols": 120, "rows": 40}}`
resizes it. When the program exits, the terminal stays, with `status:
"exited"` and its `exitCode` (128 + the signal when a signal ended it), until
it is removed. `DELETE /pty/{pty}` stops it and forgets it (204).

A terminal made with `POST /sessions/{id}/terminals` belongs to that session
(`sessionId`) and is removed when the session is deleted. The session's
current terminal is the one last written to, resized or attached to; `GET
/sessions/{id}/terminals/read?lines=N` returns its last `N` lines (its height
by default, at most 65535) with control sequences taken out, or `null`.
OpenCode reads a terminal emulator's screen there; CodeSmith reads the kept
output, so full-screen programs come out roughly.

### Attaching

`GET /pty/{pty}/connect` is a WebSocket. The server sends what the terminal
kept, as text frames of at most 64 KiB, then one binary frame, a `0x00` byte
followed by `{"cursor": N}` (the output position after the replay), then the
terminal's output as it comes. Pass `?cursor=N` to replay only from `N` (a
cursor from an earlier connection) or `?cursor=-1` for no replay. Text
frames, and binary frames that are UTF-8, are typed into the terminal. The
socket closes with 1000 when the program exits or the terminal is removed,
and with 4404 when the terminal is gone or has already exited.

The WebSocket takes the bearer token like any route. A browser cannot send
that header on a WebSocket, so a client can mint a ticket instead: `POST
/pty/{pty}/connect-token` with the token and the header
`x-codesmith-ticket: 1` (which makes a browser ask first, so other sites
cannot) answers `{"ticket": "…", "expires_in": 60}`. Then
`/pty/{pty}/connect?ticket=…` opens without the token: once, for that
terminal, within 60 seconds. A bad ticket is refused (403). The `Host` and
`Origin` checks apply as everywhere.

### Limits

- At most 16 terminals run at once; one more is refused (429).
- Each keeps the last 2 MiB of its output; older output is dropped.
- The 25 most recently exited terminals are kept; older ones are removed.
- A WebSocket more than 4 MiB behind on output is closed rather than
  buffered without end.
- Writes wait at most two seconds for a program that isn't reading.

Stopping a terminal (removing it or its session, or stopping the server)
sends `SIGHUP` to its program's process group and to the terminal's
foreground group, then `SIGKILL` after half a second. When the program exits
on its own, whatever it left running in its group is killed too. Unlike
OpenCode, whose terminals live in a separate daemon that can outlast the
server, CodeSmith's end with the server.

## Example

```sh
H="Authorization: Bearer $TOKEN"
S=$(curl -s -H "$H" -H 'Content-Type: application/json' -d '{}' \
  http://127.0.0.1:4096/sessions | jq -r .id)
curl -N -H "$H" -H 'Content-Type: application/json' \
  -d '{"text": "list the TODOs in src/"}' \
  http://127.0.0.1:4096/sessions/$S/messages
```

## Attachments

`attachments` sends images, PDFs or videos with the message. Each one is either
inline, `{"data": "<base64>", "mime": "image/png", "name": "shot.png"}`, or
by path, `{"path": "screenshots/shot.png"}`. The bytes decide the type, and
anything that isn't an image, PDF or video is refused. Images are fitted to the
[`media` limits](configuration.md#media-limits).

A path goes through the same checks as the `read` tool: `external_directory`
if it is outside the project, then `read`. These are asked over the turn's
stream, or settled by your permission rules. A refusal ends the turn with an
error before anything is sent to the model.
