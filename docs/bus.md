# The local bus

CodeSmith sessions on one machine can find each other and work together.
One session asks another to do something, the other reports progress and
answers, and the first carries on with the answer. It works like
[tailbus](https://github.com/reka18/tailbus), but only on this machine and
only for you: there is no network listener and no hosted service.

- [How it works](#how-it-works)
- [Names](#names)
- [What the model can do](#what-the-model-can-do)
- [When a request arrives](#when-a-request-arrives)
- [Permissions](#permissions)
- [In the UI](#in-the-ui)
- [On the command line](#on-the-command-line)
- [Reaching a session from tailbus](#reaching-a-session-from-tailbus)
- [Limits](#limits)
- [Security](#security)
- [Turning it off](#turning-it-off)

## How it works

The first session that joins starts a small broker, `codesmith __busd`. It
is the same program, so nothing extra is installed. The broker exits a
minute after the last session leaves, and the next session to join starts it
again. If it crashes, nothing it accepted is lost: messages are kept in a
SQLite mailbox until they are handled or a week has passed.

When CodeSmith is upgraded, the first session of the new version asks the
older broker to step down and starts its own. A session older than the
broker it finds keeps working, without the bus, and says why.

Who joins:

- **The interactive UI**, as the session it shows. With several tabs open,
  the tab on screen is on the bus; messages for the others wait until you
  switch to them.
- **`codesmith run`**, if a broker is already running. It can send and wait
  for replies, but takes no requests: they wait for the session to be
  opened in the UI. A run never starts a broker, and a run with `--tools`
  or `--no-tools` (a [bridge's](tailbus.md) headless run) stays off the bus.

Sessions served by `codesmith serve` or ACP, and automation runs, don't
join yet.

## Names

Every session on the bus has a name, which is its address. It comes from
the session's title (`fix-the-login-page`), with a number added when that
name is taken (`review-2`). A session that joins before it has a title is
called `session` until it gets one. Set your own with `/bus name reviewer`.
A name stays with its session: when you open the session again, it has the
same name, and messages sent to it while it was closed are waiting.

A name another running session has can't be taken. A name held by a
session that isn't running can be, with `/bus name`.

## What the model can do

A session on the bus has four tools:

| Tool | What it does |
|---|---|
| `bus_peers` | Lists the other sessions: name, project folder, agent, model, busy or idle. |
| `bus_send` | Sends a request, a progress note or a response. `to` is a name, or `*` for every other session in the same project folder (requests only). A response or progress note needs `reply_to`, the request's id. |
| `bus_inbox` | Lists messages to this session that haven't been handled. |
| `bus_wait` | Waits up to ten minutes (default five) for a response or progress note, optionally to one request. |

The `plan` agent can use `bus_peers`, `bus_inbox` and `bus_wait` but not
`bus_send`. Subagents get none of them; their parent session talks for
them.

## When a request arrives

A request starts a turn in the receiving session when it's idle, and waits
its turn when it's busy, like a queued prompt. The transcript shows it as
**⇄ Request from `name`**, so it can't be mistaken for something you
typed, and the model is told it's a request from another session, to weigh
as data: it never overrides your instructions.

The model answers with `bus_send`. If it doesn't, its final text is sent as
the response when the turn ends, so the sender always gets an answer. A
failed turn answers with its error, and a turn you interrupt answers that
you stopped it. After an interrupt, any other waiting messages hold until
`/bus resume`, so you have the session back.

A response to a request this session sent also starts a turn, so the
session can carry on with the work it handed off. A progress note is shown
but doesn't start one.

## Permissions

A turn another session started runs under **this** session's rules, exactly
like a turn you started. Nothing about the sender comes with the message:
not its rules, its approvals, its agent or its project.

When a rule asks, the dialog is headed **Permission required · requested
by `name`** and offers:

| Key | Answer |
|---|---|
| **y** | Allow once. |
| **s** | Allow for this session: until the session closes, never saved. |
| **n**, **Esc** or **Enter** | Reject. |

"Always allow" isn't offered, so another session can never be the reason a
rule is saved to your project. (Pressing the "always" key answers "for this
session".) A deny is a deny, and secret-file guards ask every time.

An automation run refuses anything that would ask, and `codesmith run`
without a terminal does too. `codesmith serve --auto` approves what no rule
denies for its own clients; served sessions aren't on the bus yet.

Two rules control the bus itself. Both allow by default; the resource is
the other session's name:

```jsonc
{
  "permissions": [
    { "action": "bus_send", "resource": "*", "effect": "allow" },
    // Take requests from `reviewer` only, and ask about anyone else.
    { "action": "bus_receive", "resource": "reviewer", "effect": "allow" },
    { "action": "bus_receive", "resource": "*", "effect": "ask" }
  ]
}
```

A `bus_receive` that asks shows the request before any turn starts. One
that denies (or a "no") answers the sender "Refused by the receiving
session."

## In the UI

| Command | |
|---|---|
| `/bus` | This session's name, the other sessions, what's shared and what's waiting. |
| `/bus name NAME` | What other sessions call this one. |
| `/bus share NAME` | Let [tailbus](tailbus.md) reach this session through a relay (`codesmith bridge init NAME` sets up the bridge). |
| `/bus unshare` | Stop that. |
| `/bus pause` · `/bus resume` | Take no new requests (they wait), and take them again. |

The status line shows the session's name on the bus (`⇄ reviewer`) and how
many messages wait for a turn.

## On the command line

```sh
codesmith bus peers            # the sessions on the bus (--json)
codesmith bus inbox [SESSION]  # what waits for a session (default: this folder's newest; --json)
```

Both only look; there is no command that sends. `codesmith bus relay` is
for [tailbus bridges](tailbus.md).

## Reaching a session from tailbus

A Claude Code session, or any other agent on your tailnet, can reach a
CodeSmith session that's open on your screen: share it with `/bus share
NAME`, and give tailbus a bridge that runs `codesmith bus relay --to NAME`.
`codesmith bridge init NAME` (choose Relay) sets that bridge up, and `/bus
share` says so. See [tailbus](tailbus.md).

## Limits

- A message is at most 64 KiB of text. To share a file, send its path.
- A session can send at most 30 requests a minute.
- A request sent from a turn that a request started is one hop further
  along a chain. A request that would be the fifth in a chain is refused, so
  two sessions can't keep asking each other forever.
- The broker keeps at most 10,000 unhandled messages and 128 connections.
- Messages expire after 7 days, handled or not.

Delivery is at least once: a session that crashes before it finishes a
request gets it again. A session acts on each message once, however often
it arrives.

## Security

The bus is yours alone. Its folder, `codesmith-bus-<uid>`, is in your
runtime folder (`$XDG_RUNTIME_DIR`, Linux's `/run/user/<uid>`, or macOS's
per-user temporary folder), mode 0700, never in `$HOME` or `$TMPDIR`. Both
ends check the other's user id on every connection, and the first line of
each connection must carry the broker's random 256-bit token, which only
you can read.

"CodeSmith sessions only" is a product boundary, not a security one: a
program running as you can read that token, just as it can already read
your sessions and keys. The boundary is your user account.

## Turning it off

Set `"bus": false` in [config](configuration.md), or `CODESMITH_BUS=0` in
the environment. `CODESMITH_BUS_DIR` puts the bus in another folder (to keep
two sets of sessions apart), and `CODESMITH_BUS_IDLE_MS` changes how long an
idle broker waits before it exits.
