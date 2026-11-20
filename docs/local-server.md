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
| `POST /sessions/{id}/abort` | Stop the running turn. |
| `POST /sessions/{id}/permissions/{rid} {reply, message?}` | Answer a permission request: `once`, `always` or `reject` (with optional feedback). |
| `POST /sessions/{id}/questions/{qid} {answers}` or `{dismiss: true}` | Answer the agent's questions. `answers` is one list of strings per question. |
| `POST /sessions/{id}/undo`, `POST /sessions/{id}/redo` | Undo or redo the latest turn's file changes. |

## Turn events

Each Server-Sent Event is a JSON object with a `type`:

| Type | Fields |
|---|---|
| `text` | `delta` |
| `reasoning` | `delta` |
| `tool_start` | `id`, `name`, `input` |
| `tool_end` | `id`, `name`, `output`, `error` |
| `step_end` | `reason`, `usage` |
| `retry` | `attempt`, `error`, `wait_ms` |
| `compacted` | `before`, `after` |
| `todos` | `todos` |
| `notice` | `message` |
| `permission` | `id`, `action`, `resources`, `save`, `title`. Answer it with the permissions route. |
| `question` | `id`, `prompts`. Answer it with the questions route. |
| `done` | `error` (set if the turn failed or was aborted), `usage`, `title` |

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

`attachments` sends images or PDFs with the message. Each one is either
inline, `{"data": "<base64>", "mime": "image/png", "name": "shot.png"}`, or
by path, `{"path": "screenshots/shot.png"}`. The bytes decide the type, and
anything that isn't an image or PDF is refused. Images are fitted to the
[`media` limits](configuration.md#media-limits).

A path goes through the same checks as the `read` tool: `external_directory`
if it is outside the project, then `read`. These are asked over the turn's
stream, or settled by your permission rules. A refusal ends the turn with an
error before anything is sent to the model.
