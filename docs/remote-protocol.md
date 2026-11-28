# The remote protocol

> **In development**, like [remote access](remote.md) itself. This page
> describes the parts built so far: the transport, pairing, the error
> shape, and watching sessions (the read routes and the events). Control
> is added as it's built. The JSON Schema for every body is
> [remote-protocol.schema.json](remote-protocol.schema.json).

This is the wire protocol between a CodeSmith machine's gateway
(`codesmith __remoted`) and a client such as a phone app. It's public:
anyone may write a client.

- [Transport](#transport)
- [Requests](#requests)
- [Errors](#errors)
- [Pairing](#pairing)
- [The check code](#the-check-code)
- [Paired connections](#paired-connections)
- [Sessions](#sessions)
- [Events](#events)
- [Resuming](#resuming)
- [Rebuilding a turn](#rebuilding-a-turn)
- [Versioning](#versioning)

## Transport

The machine and each device have an ed25519 key. A key's id (its **node
id**) is the public key as 64 lower-case hex characters. The client dials
the machine's node id over [iroh](https://iroh.computer) 1.5: QUIC with
TLS 1.3, authenticated by both keys, direct or through an encrypted relay.
The machine opens no listening TCP port.

The machine doesn't publish its addresses anywhere (no DNS or pkarr
lookup): a client keeps the relay URL and direct addresses from the pairing
link, and dials the node id it pinned from the link, never another.

| ALPN | For |
|---|---|
| `codesmith/pair/1` | Claiming a pairing code. |
| `codesmith/remote/1` | Everything a paired device does. |

The machine closes a connection with these application error codes:

| Code | When |
|---|---|
| 0 | The gateway is stopping, too many connections are open, or a pairing connection is done. |
| 1 | On `codesmith/remote/1`: the device key isn't paired, or was just revoked. |
| 2 | A connection whose ALPN the gateway doesn't serve. (In practice the TLS handshake already fails for an ALPN other than the two above, with `no_application_protocol`, so no connection is made.) |
| 3 | On `codesmith/pair/1`: no pairing code is live (no pairing dialog is open). |

## Requests

Each bidirectional stream carries one HTTP/1.1 request and its response,
then both sides finish the stream. The connection is the authentication:

- there's no `Authorization` header;
- `Host` is ignored;
- a request with an `Origin` header is refused (400 `bad_request`);
- a body needs a `Content-Length` (no transfer encodings) and may be at
  most 64 KiB (413 `too_large`); the head may be at most 16 KiB;
- a request must arrive within 10 seconds of the stream opening (30 on
  `codesmith/remote/1`).

Responses are `application/json` with a `Content-Length`.

## Errors

Every error body is:

```json
{"error": {"code": "code_expired", "message": "that pairing code is wrong, used or expired; scan the QR code again"}}
```

| Code | Status | |
|---|---|---|
| `bad_request` | 400 | A malformed request or body. |
| `not_found` | 404 | No such route, or a session the device has no grant for (never 403, so a device can't probe which sessions exist). |
| `forbidden_scope` | 403 | A control route on a session the device may only watch. |
| `pairing_refused` | 403 | The terminal refused the pairing, or didn't answer in time. |
| `code_expired` | 410 | The pairing code is wrong, already used, or expired. |
| `too_large` | 413 | The request is too large. |
| `rate_limited` | 429 | Too many bad claims from this device key. |
| `unavailable` | 503 | The process holding the session didn't answer in time. |
| `internal` | 500 | The machine couldn't save the pairing. |

`message` is for people; clients act on `code`. New codes may be added: a
client treats an unknown code by its status.

## Pairing

### The link

The QR code holds a link:

```
codesmith://pair?v=1&n=<node id>&r=<relay URL>&a=<addresses>&m=<machine>&s=<session>&c=<code>
```

| Key | | |
|---|---|---|
| `v` | required | `1`. |
| `n` | required | The machine's node id. |
| `r` | optional | Its home relay URL. |
| `a` | optional | Direct addresses, comma separated (`ip:port`, IPv6 in brackets). |
| `m` | required | The machine's name. |
| `s` | optional | The session the pairing offers. |
| `c` | required | The pairing code. |

Every value is percent-encoded, leaving only RFC 3986's unreserved
characters (`A-Z a-z 0-9 - . _ ~`) as they are; `+` is a plus, not a space.
CodeSmith's links pass the app team's published `pairing.json` vectors.

The **pairing code** is 16 bytes from the OS RNG as base64url without
padding (22 characters). It's valid for 180 seconds and for one claim, and
only one is live per machine. While the pairing dialog is open, a new code
replaces each one that runs out.

### The claim

On `codesmith/pair/1`:

```
POST /v1/pair/claim
{"code": "pn00cOpwBxfLmPR62H1EaQ",
 "device": {"name": "Ray's iPhone", "platform": "ios"},
 "push": {"kind": "apns", "token": "…"},
 "unlock_key": {"alg": "ES256", "spki": "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE…"}}
```

- The **device id** is the node id the connection authenticated, never
  anything in the body.
- `device.name` loses control and text-direction characters and is cut to
  64 characters; `device.platform` the same, cut to 32. Neither may be
  empty after that.
- `push` (optional): `kind` is 1 to 32 of `A-Z a-z 0-9 - _`; `token` is
  opaque, 1 to 4096 bytes. It's stored for wake-ups, which come later.
- `unlock_key` (optional): `alg` is `ES256`; `spki` is the DER
  SubjectPublicKeyInfo of a P-256 key, standard base64 with padding. It's
  stored with the device for unlocking terminals later, and replaced only
  by pairing again.

The machine answers:

| Status | When |
|---|---|
| 200 | The terminal allowed it. |
| 400 `bad_request` | The body is malformed. This doesn't use up the code. |
| 403 `pairing_refused` | The terminal refused, or nobody answered within 90 seconds. |
| 410 `code_expired` | The code is wrong, used or expired. |
| 429 `rate_limited` | This device key made more than 5 bad claims in 60 seconds, and is locked out for 5 minutes. |

A claim with the right code uses it up at once, whatever the answer. The
request is held while the terminal asks, for at most 90 seconds. Claims
from a locked-out key are refused before the code is looked at, so they
never use it up; the lockout is per device key, so another phone can still
claim.

A 200's body:

```json
{"machine": {"id": "<node id>", "name": "Rays-Mac-mini"},
 "device": {"id": "<the device's node id>", "name": "Ray's iPhone", "platform": "ios"},
 "grants": [{"session": "ses_1a21547aca5e297de", "scope": "control"}]}
```

`grants` lists every grant the device now has. A grant's `session` is a
session id, or `*` for every session on the machine, now and future;
`scope` is `control` or `watch`. A device paired with no session has an
empty list. A device that was already paired keeps its grants and gets the
new one (replacing any it had on that session).

## The check code

Both screens show a check code, so the person can see that the phone
asking is the one in their hand. It's computed over exactly these bytes
(UTF-8, `\n` between fields, no trailing newline):

```
codesmith-pair-check/1
<code>
<machine node id, lower-case hex>
<device node id, lower-case hex>
```

Take SHA-256 of them, read the first 8 bytes as a big-endian unsigned
integer, take it mod 10^12, zero-pad it to 12 digits and show it in groups
of three: `779 852 318 371`.

For example, code `pn00cOpwBxfLmPR62H1EaQ`, machine
`aa980150be84faa3788d7d170f02984f084bcaeb6871fe1fe7805ff2505a375c` and
device `f8820d751e90d88eca3b848440efc98ae226f1552a6fb5c76c7ad09c96832dc6`
give `779 852 318 371`.

## Paired connections

On `codesmith/remote/1`, only a paired, unrevoked device key is accepted;
any other connection closes with code 1. Revoking a device closes its open
connections with code 1 at once.

Every route checks the device's grants. A session it has no grant for
answers 404 `not_found` on every route, and the lists leave it out.

| Route | |
|---|---|
| `GET /v1/machine` | The machine and what it supports. |
| `GET /v1/sessions` | The sessions the device has grants for. |
| `GET /v1/sessions/{id}` | A snapshot of one: its messages, and where its event stream goes on from. |
| `GET /v1/sessions/{id}/messages` | Its messages, a page at a time. |
| `GET /v1/sessions/{id}/events` | Its events, as they happen (Server-Sent Events). |
| `GET /v1/events` | The events of every session the device has grants for. |

Control (sending prompts, answering permission requests, aborting) comes
later. Until then, any other method on a session's routes answers 403
`forbidden_scope` for a device that may only watch it, and 404 for one with
control.

### The machine

```json
{"id": "<node id>", "name": "Rays-Mac-mini", "version": "0.8.0", "protocol": 1,
 "features": ["watch"]}
```

`features` lists what this machine supports; a client hides what isn't
listed. So far it's `watch` alone. Later ones include `session_reply`,
`terminal` and `push`.

## Sessions

A session is **held** while a process has it open: the TUI, `codesmith
serve` or an editor over ACP. That process numbers its events, and the
machine reads its snapshots from it. A session no process holds is
**closed**: read-only, from the session store. Each process registers the
sessions it holds with the gateway itself.

`GET /v1/sessions` is a list, held sessions first:

```json
[{"id": "ses_1a2b", "title": "fix the login page", "agent": "build",
  "model": "anthropic/claude-sonnet-4-5", "directory": "/Users/ray/app",
  "busy": true, "owner": "tui", "scope": "control"}]
```

`owner` is `tui`, `serve`, `acp` or `closed`. `busy` is a JSON boolean:
whether a turn is running. With a grant for every session (`*`), the list
has the held sessions and the 200 most recently changed closed ones.

`GET /v1/sessions/{id}` adds:

| Field | |
|---|---|
| `epoch` | Random per process holding the session; `closed` for a closed one. |
| `events_seq` | The last event seq whose effect is in this snapshot. 0 before any event. |
| `turn` | `null` while idle; `{id, start_seq}` while a turn runs. |
| `messages` | The stored history, in serve's record format: a role, then parts with a type and data (text as a string or `{text}`; `tool_call {id, name, input}`; `tool_result {id, name, content, is_error}`). |
| `seqs` | Each message's record seq (`null` for a summary checkpoint). |
| `effort` | The reasoning effort, when one was set. |

While a turn runs, `messages` has the finished turns only, and
`events_seq` is the seq just before the turn's `turn_start`: the running
turn's events all come after it. Subscribe from `<epoch>:<events_seq>` and
the stream replays the turn from its start.

When the running turn is over 8 MiB, `turn` also has `partial` (the turn
so far, in the same message format) and `partial_seq`. Add `partial` to
`messages` and subscribe from `partial_seq` instead. `partial_seq` counts
only with `partial` there.

`GET /v1/sessions/{id}/messages?limit=N&before=SEQ` gives the newest
`limit` messages (50 by default, at most 500) whose record seq is below
`before`, oldest first: `{"messages": [...], "seqs": [...], "more": true}`.
`more` says whether older ones remain.

## Events

An event stream is Server-Sent Events on the request's stream, which stays
open: `content-type: text/event-stream`, then events, each as

```
id: <epoch>:<seq>
event: <type>
data: <the event as one line of JSON>

```

and a `: ping` comment every 15 seconds while there are none.

Every event has an envelope:

```json
{"v": 1, "epoch": "9c0e5f7a1b2c3d4e", "seq": 42, "session": "ses_1a2b",
 "turn": "trn_9c0e5f7a1b2c3d4e_7", "ts": 1764300000042, "type": "text", "delta": "Look"}
```

- `epoch` is random per process holding the session. It changes when that
  process restarts or another one takes the session.
- `seq` rises by one per session, across turns, from 1.
- `session` is the session's id, on every event. A subagent's events also
  name the subagent's session in `child`.
- `turn` is the running turn's id, unique across epochs, or `null` between
  turns.
- `ts` is milliseconds since the Unix epoch.

| Type | Fields |
|---|---|
| `turn_start` | `origin: {kind, name}`: `kind` is `terminal` (a prompt on this machine: the TUI, an editor or a `serve` client), `device`, `bus` (another session, `name` saying which) or `resumed` (the machine resumed the session itself when a background command finished). |
| `user_message` | `text`, the prompt exactly as it was typed in the terminal or posted, never with a skill's or command's content joined in, and `attachments`. |
| `text`, `reasoning` | `delta`. |
| `tool_start` | `id`, `name`, `input`. |
| `tool_output` | `id`, `delta`: a running command's output so far. |
| `tool_end` | `id`, `name`, `output`, `error` (a boolean). |
| `permission` | `id`, `action`, `resources`, `title`, `save`. |
| `question` | `id`, `prompts`. |
| `prompt_resolved` | `id` (the permission's or question's), `kind`, `reply`, `by: {kind, name}`. |
| `notice` | `message`. |
| `retry` | `attempt`, `error`, `wait_ms`. |
| `todos`, `media`, `step_end`, `compacted`, `child_start`, `child_end`, `child_text`, `child_reasoning`, `tool_progress` | As `codesmith serve` sends them. |
| `config` | `agent`, `model`, `effort`: the selection changed. |
| `queued` | `count`, `next`: prompts waiting for the running turn. |
| `done` | `error`: `null` when the turn ended cleanly, else why (`aborted`, `interrupted`, `cancelled`, or the failure). |
| `session_status` | `busy: false`: the session really is idle now. It can come a little after `done`, for example after an abort. |
| `reset` | `reason`. See [Resuming](#resuming). |

Clients ignore types and fields they don't know. Every boolean is a JSON
boolean.

**Merging.** Consecutive `text`, `reasoning`, `tool_output` (of one call),
`child_text` and `child_reasoning` deltas are sent at most ten a second per
kind: the deltas of a tenth of a second come as one event.

**Size.** No event is over 1 MiB. A longer one has its longest text cut,
ending with `[… N more bytes not sent]`, and says `"truncated": true`.

**Prompts.** When a stream opens, the permission requests and questions
still open that the client hasn't been sent are sent again, as they were,
without an SSE id. A replay of a running turn has all of its own, answered
or not, with their `prompt_resolved`.

`GET /v1/events` sends every granted session's events from now on, with
the same ids. It doesn't replay: resume a session with its own stream.

## Resuming

Send `Last-Event-ID: <epoch>:<seq>` (the last id applied) on
`GET /v1/sessions/{id}/events`. The stream replays every event after it,
each with its original seq, then goes on live. A client that skips seqs at
or before the one it has applied never applies anything twice, `done`
included.

The machine keeps, per session, the events of finished turns up to 2,000
events or 4 MiB, and every event of the running turn from its
`turn_start`, up to 8 MiB. To keep the running turn under that, consecutive
deltas of one block are merged into one event that has the last seq and
says `"merged_from": <first seq>`. A resume from inside a merged event gets
the rest of it, from the next seq, so a replayed event never begins at or
before the cursor.

When it can't resume, the stream sends one `reset` and ends:

```
event: reset
data: {"v": 1, "session": "ses_1a2b", "type": "reset", "reason": "gap"}

```

| `reason` | When |
|---|---|
| `epoch` | The cursor is from another epoch: the process restarted, another one holds the session, or it was closed. |
| `gap` | The events after the cursor are older than the buffer. |
| `turn_too_large` | The running turn is over 8 MiB and the events after the cursor are gone. The snapshot has `turn.partial`. |

Reload the session (`GET /v1/sessions/{id}`) and subscribe from its
cursor. A fresh snapshot's cursor doesn't reset. A `reset` with reason
`history` also comes as an ordinary numbered event when the history
changed outside a turn (an undo, a redo, `/compact`): reload then too.
Treat a reason you don't know as a reload.

A closed session's stream, from `closed:0` (its snapshot's cursor), stays
open with pings. When a process opens the session, it sends `reset`
`epoch` and ends.

## Rebuilding a turn

A client that loads a snapshot during a turn and subscribes from its
cursor ends with the same transcript as one that watched the turn live.
The app team's `remote_events.json` fixture pins the client's side: the
cursor's decision for each event (apply, skip, or reset, including
`merged` for a merged event that overlaps what was applied), the plan a
snapshot gives, and scripted turns cut at every point. CodeSmith's tests
run it against what the machine sends.

Two things differ in the stored history. It has no prompt cards: a
snapshot taken after a turn ends shows its messages, not its permission
requests and questions. And a stored user message can be longer than the
turn's `user_message`, when the prompt brought in a skill, a command's
template or a mentioned file's text.

## Versioning

The ALPNs and the link's `v` carry the version. A change a client must
understand gets a new one; new fields, routes, event types and error codes
don't, and clients ignore what they don't know.
