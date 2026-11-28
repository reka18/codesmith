# Remote access from a phone

> **In development.** This is being built and is not in a release yet. It
> ships when Skeptic Dog's relays are ready and Ray says so. What this page
> describes is what's built so far: pairing a phone and managing paired
> devices. Following and driving a session from the phone come next.

Remote access lets you pair a phone with CodeSmith by scanning a QR code.
Skeptic Dog's CodeSmith Mobile app is the first client; the
[protocol](remote-protocol.md) is public, so anyone can write another.

- [Before you start](#before-you-start)
- [Pairing a phone](#pairing-a-phone)
- [The confirmation](#the-confirmation)
- [Devices and revoking](#devices-and-revoking)
- [The gateway](#the-gateway)
- [Settings](#settings)
- [What a phone can and can't do yet](#what-a-phone-can-and-cant-do-yet)
- [Security](#security)
- [Building without it](#building-without-it)

## Before you start

Nothing about remote access touches the network until you opt in. No
socket opens and no relay is contacted until you run `/mobile`,
`codesmith remote pair` or `codesmith remote start`, or set
`"remote": {"enabled": true}` yourself. Those first three set it for you,
and say so.

Pairing a phone is like handing it your keyboard: a phone with control of a
session will be able to do nearly anything you can do in it. Pair only
phones you hold, and compare the check code every time.

## Pairing a phone

In a session, type `/mobile`. Or, from a shell:

```sh
codesmith remote pair --session SESSION_ID
```

CodeSmith starts its gateway if it isn't running, waits up to five seconds
for its relay, and shows:

- a QR code to scan with the app;
- this machine's name, the session, and the relay;
- the `codesmith://pair?…` link as text, for a phone that can't scan;
- a countdown. The code in the QR lasts three minutes, then a new one
  shows, until you close the dialog with `q` or Esc. `c` copies the link,
  where your terminal allows it.

| Command | Offers the phone |
|---|---|
| `/mobile` | Control of this session. |
| `/mobile watch` | Watching this session only. |
| `/mobile all` | Every session on this machine, now and future. |
| `codesmith remote pair --session ID` | Control of that session (`--watch` for watching only). |
| `codesmith remote pair --all` | Every session on this machine. |
| `codesmith remote pair` | Nothing yet: the phone is paired, and you grant it sessions later. |

Each code works once. Opening `/mobile` somewhere else replaces the code
showing here: one pairing dialog runs at a time.

## The confirmation

When the phone scans the code, the terminal asks:

```
Allow "Ray's iPhone" (ios, f8820d751e) to control "fix-the-login-page"?
Check code on the phone: 779 852 318 371
[y] yes   [w] watch only   [a] all sessions on this machine   [n] no
(1:29 left) Answer:
```

The phone shows the same 12-digit check code. **Compare them before you
answer.** If they differ, the phone asking isn't the one in your hand:
answer `n`.

- `y` or `yes`, then Enter, gives the phone control of the session.
- `w` gives it watching only.
- `a` gives it every session on this machine, now and future, and asks
  again: you must type `all`.
- `n`, Enter alone, Esc, anything else, or no answer within 90 seconds
  refuses.

Keys pressed in the moment the question appears are ignored, so a key
meant for something else can't answer it. A phone that's already paired is
marked `(paired device)` and gets only the new grant.

The answer is read from your terminal itself, never from piped input: a
`y` piped into `codesmith remote pair`, or a run with no terminal (as the
agent's own commands are), refuses before anything starts.

## Devices and revoking

```sh
codesmith remote devices          # paired devices and their grants
codesmith remote devices --all    # revoked ones too
codesmith remote revoke DEVICE    # its id, a prefix of it, or its name
```

In a session, `/mobile devices` and `/mobile revoke DEVICE` do the same.
Revoking a device unpairs it and closes its connections at once. To use it
again, pair it again.

## The gateway

The gateway, `codesmith __remoted`, is the part a phone connects to. It's
the same program as CodeSmith, one per user on each machine. It runs while
a device is paired and `remote.enabled` is on, or while a pairing dialog is
open, and stops a minute after there's nothing to do.

| Command | |
|---|---|
| `codesmith remote start` | Starts the gateway (and turns `remote.enabled` on). |
| `codesmith remote stop` | Stops it. |
| `codesmith remote status [--json]` | Whether it's running, its relay, its direct addresses and connections, and how many devices are paired. |
| `codesmith remote id` | This machine's node id (it makes the machine key the first time). |

`/mobile status`, `/mobile id` and `/mobile stop` do the same in a session.

The gateway opens no listening TCP port. A phone reaches it by its node id,
through a relay or straight over your network or tailnet. When CodeSmith is
upgraded, the new version's gateway takes over from the old one.

## Settings

Remote access is set only in your own global config
(`~/.config/codesmith/codesmith.json`). A `remote` key anywhere else (a
per-repo file in `projects/`, `$CODESMITH_CONFIG`,
`$CODESMITH_CONFIG_CONTENT`) is ignored, and `codesmith debug config` warns
about it, so a project can never turn remote access on or change its
relay.

```jsonc
"remote": {
  "enabled": false,     // set by /mobile, `codesmith remote pair` and `start`
  "relay": "default"    // "default", a relay URL of your own, or "none"
}
```

| Key | Default | |
|---|---|---|
| `enabled` | `false` | The gateway runs only while this is on. Turning it off stops a running gateway within a few seconds. |
| `relay` | `"default"` | `"default"` is Skeptic Dog's relays (`relay-us.codesmith.skepticdogsoftware.com`; more regions to come). n0's public relays are never used by default. A URL (`https://…`) uses that one relay. `"none"` uses direct addresses only: your LAN and tailnet. |

A wrong value is an error, not a fallback, so a typo can't send traffic
somewhere you didn't mean.

The design's other settings (`prompt_timeout`, `notify`, `push_endpoint`,
`terminal`, `terminal_unlock`) come with the features they control.

## What a phone can and can't do yet

**It can:** pair with this machine and keep a connection open while it's
paired.

**It can't yet:** see sessions, follow them, send prompts, answer
permission requests, open terminals or get push notifications. Those come
in the next steps; until then every request a paired phone makes answers
"not found". Even then, a phone will never run shell commands outside a
turn, fork, delete or archive sessions, see your environment, or edit your
saved permission rules.

## Security

- **Keys.** The machine key is `remote-key` in the data folder
  (`~/.local/share/codesmith`), only readable by you. Each phone has its
  own key; a device's id is the key its connection proved, never what it
  says about itself.
- **The code.** It's 16 random bytes, valid for three minutes and for one
  claim. A phone that makes more than five bad claims in a minute is locked
  out for five minutes, and its claims while locked can't use up the live
  code.
- **The check code** has 12 digits, so someone who sees your QR code can't
  make a key that matches the code you'll compare within its three
  minutes.
- **From the agent.** The agent runs commands as you, so the remote files
  are guarded: reading or writing `remote-key`, `remote.db` or the
  gateway's folder always asks, whatever you've allowed before. Its
  `codesmith remote` commands always ask too, except `id` and `status`.
- **The gateway's socket** is yours alone: in a private folder, checked
  for your user id, with a token only you can read.
- **Names.** A device's name is cut to 64 characters, with control and
  text-direction characters removed, so it can't disguise the prompt.

## Building without it

Remote access is the `remote` cargo feature, on by default. Build without
it with:

```sh
cargo build --release --no-default-features
```

Such a build has no `codesmith remote` command, `/mobile` says it's
missing, and no networking code for it is compiled in. The guards on the
remote files stay, in case another build left them.
