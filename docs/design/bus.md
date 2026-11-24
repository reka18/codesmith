# Design: the local bus

Status: **approved and built** (2026-11-24). The user guide is
[docs/bus.md](../bus.md); reaching sessions from tailbus is
[docs/tailbus.md](../tailbus.md).

The local bus lets CodeSmith sessions on one machine find each other and
exchange messages: one session asks another to do something, the other
reports progress and answers. It is modelled on tailbus, cut down to a single
machine.

## Decisions

These are settled; the rest of this document follows from them.

1. **A broker that starts when needed.** The first session to use the bus
   starts `codesmith __busd`, and it exits a minute after the last session
   leaves. It is the same binary, so nothing extra is installed. Registering
   it with launchd or systemd, so that it is always running, may come later
   as an option; it is not the default.
2. **A request starts work on its own.** The receiving session doesn't ask
   whether to accept the request. It is asked only when the work the request
   leads to hits a rule in its own permissions that asks or denies. Those
   prompts offer **allow once** and **allow for this session**.
3. **CodeSmith sessions only.** Other programs aren't members, and there is
   no command for sending messages from outside CodeSmith.
4. **No pipe workflow.** There is no plan → approve → result handoff, now or
   later. The bus carries messages; what a session does with one is up to
   that session.
5. **This machine only.** There is no network listener. Talking to other
   machines stays tailbus's job.
6. **tailbus reaches a session you have open through a relay.** A tailbus
   bridge runs `codesmith bus relay --to NAME`, which hands the message to
   the session shared as `NAME` (`/bus share NAME`) and prints its answer.
   CodeSmith never speaks tailbus's protocol. Only shared sessions can be
   reached. When the shared session isn't running, the relay refuses, or
   with `--offline run` continues the session in a headless run. This
   replaces the "tailbus connector" considered earlier.
7. **`codesmith run` is a good headless agent for a bridge**: the message
   on stdin alone, `--effort`, `--tools` / `--no-tools`, `--no-mcp`,
   `--usage-log`, and `CODESMITH_DB` for a session store per sender.

## How it differs from tailbus

| | tailbus | CodeSmith bus |
|---|---|---|
| Reach | Every machine on the tailnet | This machine |
| Broker | Always on, under launchd | Starts when needed, stops when idle |
| Members | Any agent with the MCP plugin | CodeSmith sessions |
| Addresses | `name@machine` | A session's name or id, or `*` for every session in a project |
| Pipe workflow | Yes | No |
| Hub and client roles | Yes | No; every session is an equal peer |

The messaging model is the same: request, progress and response messages,
`reply_to` threads, and incoming messages treated as data rather than as the
user's instructions.

## The broker

`codesmith __busd` reuses the lifecycle and security of the shell daemon
(`codesmith-core/src/daemon`), which already works this way.

- **Starting.** A session that wants the bus connects to the broker's socket.
  If nothing answers, it takes the start lock, starts `__busd` and connects
  again. Only one broker can hold the lock, so two sessions starting at once
  get the same broker.
- **Stopping.** With no sessions connected, the broker waits 60 seconds and
  then exits. A session connecting in that window keeps it running.
- **Crashing.** The mailbox is in SQLite, so nothing that was accepted is
  lost. The next session to use the bus starts a new broker. Sessions that
  lose their connection reconnect, with backoff, and register again.
- **Upgrading.** Sessions and broker exchange versions when they connect. A
  session newer than the broker asks it to shut down cleanly (it stops
  accepting, finishes writing, and exits), then starts its own. An older
  session that the broker's protocol can't serve gets a clear error ("this
  session is older than the bus; restart it") and carries on without the
  bus. A broker never kills another broker.

### Security

- **Same user only.** The socket lives in a directory only you can use
  (`codesmith-busd-<uid>/` in your runtime directory, mode 0700, socket
  0600). Both ends check the other's uid on every connection
  (`getpeereid` / `SO_PEERCRED`) and refuse anyone else.
- **A per-broker secret.** The broker writes a random 256-bit token to its
  registration file (0600, written atomically, read with `O_NOFOLLOW`).
  Every frame carries it and it is compared in constant time.
- **The limit of "CodeSmith only".** A program running as you can read
  that token, just as it can already read your sessions and keys. "CodeSmith
  only" is a product boundary: there is no documented protocol and no
  sending command. It is not a security boundary between your own programs.
  The boundary is your user account.
- **Messages are data.** A session's model is told that messages from other
  sessions are not your instructions and never override you, the way the
  tailbus plugin does now.
- **No borrowed authority.** Whatever a request asks for, the receiving
  session acts only under its own permission rules. Nothing about the sender
  (its rules, approvals, agent or project) passes along with a message.
- **Limits.** A message body is at most 64 KiB. The broker holds at most 128
  connections and 10,000 undelivered messages. A sender can send at most 30
  requests a minute, and the broker refuses anything above that.

## Sessions on the bus

Every interactive session, `codesmith run`, session served by `codesmith
serve`, and automation run joins the bus unless `"bus": false` is set.

- **Names.** A session's name comes from its title, shortened to a slug, plus
  a short suffix if that name is taken (`fix-login`, `fix-login-2`). You can
  set one with `/bus name reviewer`. Names are unique on the machine, and the
  broker settles any clash. A session id is always an address too.
- **What peers can see.** Name, session id, project folder, agent, model,
  whether the session is busy, and when it joined. Nothing else.
- **Leaving.** A session leaves when it closes. If its process dies, the
  broker notices the connection drop at once. Messages to a session that
  isn't running wait in the mailbox until it opens again, or until they
  expire after 7 days.

## Messages

| Field | Meaning |
|---|---|
| `id` | A ULID, unique and in time order |
| `from`, `to` | Session ids. `to` may be `*` with a project folder, meaning every session in that project |
| `kind` | `request`, `progress` or `response` |
| `reply_to` | The request a progress note or response belongs to |
| `body` | Text, at most 64 KiB |
| `sent`, `delivered` | Times |

- Each message is delivered **at least once**. Receivers drop duplicates by
  id, so in practice each message is acted on once.
- Messages from one sender to one receiver arrive in the order they were
  sent.
- A request gets one response. Progress notes appear under the request
  without answering it.

## What a session does with a message

- **Request.** If the session is idle, the request starts a turn. If it is
  busy, the request waits and runs after the current turn, like a queued
  follow-up. The transcript shows it as a block headed **Request from
  `<name>`**, so it can't be confused with your own prompt.
- **Response.** A response to a request this session sent also starts a
  turn (or waits for the current one), so the session can continue the work
  it handed off.
- **Progress.** A progress note appears in the transcript but doesn't start
  a turn.

### Permissions

A turn started by a message runs under the receiving session's rules, exactly
like a turn you started. It differs in two ways:

- **The prompt says who is asking.** A permission dialog in a turn started by
  another session is headed "Requested by `<name>`" and offers:

  | Key | Answer |
  |---|---|
  | **y** | Allow once |
  | **s** | Allow for this session |
  | **n**, **Esc** or **Enter** | Reject |

  "Always allow" isn't offered, so another session can never be the reason a
  rule is saved to your project. "Allow for this session" lasts until the
  session closes and is never written to disk.
- **Unattended sessions** answer the way they already do: an automation
  refuses anything that would ask, and `codesmith run` without a terminal
  refuses too. A `serve --auto` session approves what no rule denies, as it
  does for its HTTP clients. That makes `serve --auto` the one place where a
  peer can get work done without a prompt, so its documentation will say so.

Sending and receiving are permissions too, so you can restrict them:

```jsonc
{
  "permissions": [
    { "action": "bus_send", "resource": "*", "effect": "allow" },
    { "action": "bus_receive", "resource": "reviewer", "effect": "allow" },
    { "action": "bus_receive", "resource": "*", "effect": "ask" }
  ]
}
```

The resource is the other session's name. Both default to `allow`. A
`bus_receive` that asks shows the request before it starts a turn, with the
same y / s / n answers. One that denies drops the request and answers the
sender "refused by the receiving session".

### Runaway loops

Two sessions could keep sending each other requests. To stop that:

- Every request carries a hop count: a request sent from a turn that a
  request started is one hop further. At 4 hops, sending is refused and the
  model is told why.
- The rate limit above caps how fast any session can send.
- `/bus pause` stops a session from accepting new requests until `/bus
  resume`.

## The model's tools

| Tool | What it does |
|---|---|
| `bus_peers` | Lists live sessions: name, id, project, agent, model, busy or idle |
| `bus_send` | Sends a request, a progress note or a response (`to`, `kind`, `body`, `reply_to`) |
| `bus_inbox` | Lists messages that haven't been handled |
| `bus_wait` | Waits up to a given time for a message, optionally one in reply to a given request |

The tools are native, with no MCP layer. The `plan` and `explore` agents can
use `bus_peers` and `bus_inbox` but not `bus_send`. Subagents get none of
them; their parent session talks for them.

## In the UI

- `/bus` opens a panel listing peers and the inbox. It is keyboard only, with
  no mouse capture.
- `/bus name <name>`, `/bus pause`, `/bus resume`.
- The status line shows the session's bus name and a count of unhandled
  messages.

`codesmith bus peers` and `codesmith bus inbox [SESSION]` print the same
information on the command line. They only read; there is no send command.

## The relay

- The relay finds the bus by the user's id, never by `$HOME` or `$TMPDIR`,
  which a bridge sets to its own. When it has to start the broker, it does
  so in the user's home (from the user database; `CODESMITH_RELAY_HOME` in
  tests), and the broker reuses the mailbox the last broker recorded in the
  bus directory (`mailbox.path`), so a relay never opens a second mailbox.
- A share records where the sharing process keeps its config, keys and
  sessions (`HOME`, `XDG_*_HOME`, `CODESMITH_DB`; absolute paths only).
  `--offline run` applies exactly those, so the headless run continues the
  same session with the same keys, never the bridge's.
- `--id {ID}` makes tailbus's at-least-once delivery safe: a repeat of a
  carried message gets the first one's answer (or waits for it) instead of a
  second turn.
- Each relayed request comes from a fresh relay member with its own id, and
  only a relay may carry one; a relay can't send requests of its own or
  share anything.

## Where the build differs from the plan

- **Who joins.** The interactive UI (as the tab on screen) and `codesmith
  run` (when a broker already runs; it takes replies, not requests, and
  never starts a broker, since under a bridge its folders aren't the
  user's). A narrowed run (`--tools`, `--no-tools`) stays off the bus.
  Sessions under `serve` and ACP, and automation runs, don't join yet.
- **Authentication.** The token is checked on a connection's first line;
  every later line rides on that authenticated, uid-checked connection.
- **The `explore` agent** is a subagent, and subagents get no bus tools, so
  only `plan` has the look-only set.
- **An unanswered request** is answered with the turn's final text (or its
  error, or "stopped by its user" after an interrupt), so a sender never
  waits forever.
- **After an interrupt** the waiting messages hold until `/bus resume`, so
  the user has the session back.
- **Order.** Messages are kept and delivered in the order they were
  accepted (SQLite row order); ids made in the same millisecond don't sort
  in time order, which the property test caught.
- **Untitled sessions** are called `session` until they get a title, then
  take a name from it; a name chosen with `/bus name` stays.

## Out of scope

- The pipe workflow (decision 4).
- Other machines, and a bridge to tailbus.
- Members other than CodeSmith (decision 3).
- Attachments. A body is text; to share a file, send its path.

## Tests

Following the project's test policy:

- **End to end:** two and three real processes find each other, exchange
  requests, progress and responses, and continue the work. A message to a
  closed session is delivered when it reopens.
- **Lifecycle:** sessions racing to start a broker get the same one. The
  broker stops after the idle window and not before. A broker killed with
  SIGKILL loses nothing that it accepted. A newer session replaces an older
  broker, and an older session degrades cleanly.
- **Security and negative:** a connection from another uid is refused, as is
  a wrong token, a socket directory with the wrong owner or mode, a symlinked
  registration file, an oversized body, and a flood over the rate limit. The
  hop limit holds. A message never carries approvals across sessions. "Always"
  is never offered in a dialog for a turn a peer started.
- **Edge:** name clashes and renames, `*` sending to an empty project, a
  session that leaves mid-turn, expiry after 7 days, a full mailbox.
- **Property (bounded):** for random interleavings of sends, crashes and
  reconnects, every accepted message is delivered at least once, acted on
  once, and in per-pair order.
- **Stress:** 50 sessions each sending to all the others, with no lost or
  reordered messages and with memory bounded.
