# Design: remote access from a phone

Status: **design settled; being built.** Ray answered the open questions on
2026-11-26 and approved building on 2026-11-28. Milestone M1's first two
parts are built: the machine key, the gateway and pairing, then watching
(the owners' registration, the read routes, events and resuming); see
[the user guide](../remote.md) and [the protocol](../remote-protocol.md).

Remote access lets you pair a phone with a CodeSmith session by scanning a QR
code, then follow and drive that session from the phone. The phone and the
terminal show the same session live. You can pair any number of sessions,
on any number of machines. The wire protocol is public, so anyone can write
a client. Skeptic Dog's phone app is the first one.

## Decisions

These are settled; the rest of this document follows from them.

1. **Nothing reaches the network until you opt in.** No socket opens and no
   relay is contacted until you run `/mobile`, `codesmith remote pair` or
   `codesmith remote start`, or set `remote.enabled: true`. A test checks
   this with the network blocked.
2. **The transport is iroh** (QUIC with TLS 1.3, keyed by ed25519 node ids,
   with hole punching and an encrypted relay as the fallback). The phone dials
   the machine's public key, and the machine opens no listening TCP port.
3. **Relays are Skeptic Dog's only.** The default relay map never includes
   n0's public relays. `/mobile` stays out of releases until Skeptic Dog
   provides the relay URLs. Development builds may use n0's preset.
   `remote.relay` can also be one URL of your own, or `"none"` (direct
   addresses only: LAN and tailnet).
4. **Push wake-ups turn on with `/mobile`,** and the first run of `/mobile`
   says so. `remote.notify: "connected-only"` turns them off. A push never
   contains content: no titles, code or commands.
5. **Built in, dormant by default.** The `remote` cargo feature is on in
   release builds. It does nothing until you opt in, and it can be built
   without.
6. **The gateway runs while it's needed.** `codesmith __remoted` is in the
   same binary. It keeps running while any device is paired and
   `remote.enabled` is on. `codesmith remote stop` ends it, and
   `codesmith remote status` shows that it's running.
7. **What a phone with control may do.** With no extra grant from the
   terminal, it may:
   - answer permission requests with once, session or reject (only "always",
     which saves a rule, stays terminal-only);
   - switch agent (Plan to Build), model and effort;
   - approve reads of secret-guarded files;
   - run slash commands, including those that expand to shell commands,
     which still go through the normal permission checks.
8. **A remote terminal needs an unlock every time it opens** (see
   [The remote terminal](#the-remote-terminal)). The Mac checks the unlock,
   not the app. It's on by default; `remote.terminal_unlock: false` turns it
   off.
9. **No payment or licence logic in CodeSmith.** A client's own service may
   charge for whatever it likes. CodeSmith speaks only the open protocol.

## What this means for safety

A paired phone with `control` can do nearly anything the terminal user can:
under decision 7 it can approve its own prompts. Treat pairing a phone like
handing over the keyboard. What protects you:

- **Pairing takes you, at the terminal.** It needs a fresh QR code and your
  `y` at a real terminal, and you compare a check code on both screens
  before saying yes.
- **Each phone has its own key.** You can revoke one at once, which closes
  its connections.
- **Grants are per session.** Access to every session is a separate,
  double-confirmed choice.
- **Opening a terminal needs Face ID, Touch ID or your password,** checked on
  the Mac, every time.
- **Everything is audited.** Each action a phone takes goes in an audit log
  (never content), shown by `/mobile log`.

## Pieces

| Piece | What it is |
|---|---|
| Machine key | `remote-key` in the data folder: an ed25519 secret, 0600, made on first use, written atomically. Its public key is the machine's node id. |
| `codesmith __remoted` | The gateway: one per user per machine, with a lock file and a 0700 runtime folder, as `__shelld` has. It takes over from an older version on upgrade, as the bus broker does. |
| `remote.db` | SQLite: devices, grants, the audit log, push tokens and unlock keys. |
| Owners | The TUI, `codesmith serve` and ACP register the sessions they hold with the gateway over a Unix socket, authenticated as `__shelld`'s is (a token in a 0600 registration file, plus peer uid checks). |
| `/mobile` and `codesmith remote …` | Pairing, the device list, the audit log, revoking, start, stop, id. |

The gateway forwards each device request to the process that owns the
session, and fans that process's events out to every device with a grant on
it. A granted session that no process holds is shown read-only, as `closed`,
with its history from the store.

## Settings

Every `remote.*` setting is read **only from your own global config**. A
project's config (or one in a repository you clone) can never turn remote
access on, change the relay or push endpoint, or turn off the terminal
unlock. CodeSmith warns if a project config contains a `remote` key.

```jsonc
"remote": {
  "enabled": false,            // set by /mobile
  "relay": "default",          // "default" (Skeptic Dog's) | a relay URL | "none"
  "prompt_timeout": "30m",     // how long a prompt waits for any answer; 0 waits forever
  "notify": "push",            // "push" | "connected-only"
  "push_endpoint": "default",
  "terminal": true,            // false: no remote terminals on this machine
  "terminal_unlock": true      // false: terminals open without an unlock
}
```

`remote.prompt_timeout` applies to sessions with a `control` grant. It
replaces the fixed 30 s refusal of prompts nobody can see, which would
otherwise refuse prompts while the phone is asleep.

## Protecting the remote files from the agent

The agent runs shell commands as you, so it must not be able to pair itself
a device or read the keys:

- **Secret-file guards.** These files get the built-in guards that always
  ask, whatever saved approvals say: `remote-key`, `remote.db`, the gateway's
  registration file and anything else in its runtime folder.
- **Shell commands that always ask.** When the agent runs
  `codesmith remote …` (except `id` and `status`), it always asks, as
  keychain reads do.
- **Confirmation needs a real terminal.** Pairing reads its confirmation from
  the controlling terminal, never from stdin. A piped `y`, or a run with no
  terminal (the agent's own shell commands have none), refuses.

## Pairing

### The QR code and link

`/mobile` (or `codesmith remote pair`) starts the gateway if needed, waits up
to 5 s for its home relay, and shows:

- a QR code drawn with Unicode half-blocks, with a quiet zone and error
  correction level L (the mouse is never captured);
- the machine name, the session title and a countdown;
- the `codesmith://pair?…` link as text, under a "copy link" key.

The link is
`codesmith://pair?v=1&n=<node id>&r=<relay URL>&a=<addresses>&m=<machine>&s=<session>&c=<code>`.
Values are percent-encoded using RFC 3986 unreserved characters only, and
`r`, `a` and `s` are optional. CodeSmith's builder must emit links that pass
the clients' published test vectors (see
[Tests](#tests)).

The pairing code:

- is 16 bytes from the OS RNG, as base64url without padding;
- is valid for 180 s and single use, burned by its first claim;
- is one per gateway at a time: opening `/mobile` elsewhere replaces it;
- is replaced with a new one when it expires, until you close the dialog.

### The claim

A phone connects on ALPN `codesmith/pair/1` and sends
`POST /v1/pair/claim {code, device: {name, platform}, push?, unlock_key?}`.

- **Device id:** the node id the connection authenticated, never anything
  in the body.
- **Name:** control characters are stripped, and it's cut to 64 characters.
- **Response:** the request is held while the terminal asks, for at most
  90 s. The answer is 200, 400, 403 `pairing_refused`, 410 `code_expired`
  (wrong, used or expired) or 429 `rate_limited`.
- **Rate limit:** per device key. More than 5 bad claims in 60 s locks that
  key out for 5 minutes, and a locked key's claims don't burn the live code.

### The check code

Both the phone and the terminal show a check code derived from the pairing,
so you can confirm that the phone asking is the one in your hand:

```
SHA-256("codesmith-pair-check/1\n" + code + "\n" + machine_node_id_hex + "\n" + device_node_id_hex)
```

Node ids are lower-cased. The code shown has 12 digits: the first 8 bytes of
the hash as a big-endian integer, mod 10^12, zero-padded and shown in groups
of three (`123 456 789 012`). Taking 8 bytes rather than 5 keeps the digits
evenly spread.

Why 12 and not 6: someone who sees your QR has up to 180 s to make device
keys until one matches the code you'll compare. Six digits take about a
million tries, which is seconds on one machine. Twelve take about a
trillion, which can't be done in that window.

### The confirmation

```
Allow "Ray's iPhone" (ios) to control "fix-the-login-page"?
Check code on the phone: 123 456 789 012
[y] yes   [w] watch only   [a] all sessions on this machine   [n] no
```

- `y` grants `control` on this session, and `w` grants `watch`.
- `a` grants `control` on every session, now and future, and asks a second
  time: `Type "all" to confirm`.
- Enter alone, `n`, Esc, or no answer within 90 s refuses. Only `y` or `yes`
  allows.
- A device that's already paired gets only the new grant, and the prompt
  says "(paired device)".

## Connections and routes

On ALPN `codesmith/remote/1`, only a paired, unrevoked device key is
accepted (otherwise the connection closes with code 1). Each bidirectional
stream carries one HTTP/1.1 request and its response; event streams stay
open. The connection is the authentication. There's no `Authorization`
header, the `Host` header is ignored, and a request with an `Origin` header
is refused.

Every route checks the device's grant. A session the device has no grant
for answers 404, never 403, so devices can't probe which sessions exist.

- **Phase A (watching):** the routes are `GET /v1/machine`, `/v1/sessions`,
  `/v1/sessions/{id}`, `/v1/sessions/{id}/messages` (paged), and the event
  streams `/v1/sessions/{id}/events` and `/v1/events`. Payloads follow
  `codesmith serve`'s.
- **Phase B (control):** sending messages (202 at once, 409 if busy), the
  queue, abort, permission and question answers, background and jobs,
  config (agent, model, effort), slash commands, undo and redo, read state,
  diffs and media. A `watch` device gets 403 `forbidden_scope` on all of
  these.
- **Never offered to devices:** shell commands outside a turn, fork, delete,
  archive, the environment, and editing saved permission rules.

### Events

Every event carries an envelope:
`{"v": 1, "epoch": "<random per owning process>", "seq": N, "session", "turn", "ts", "type", …}`.

- **Resume:** the SSE id is `<epoch>:<seq>`. `Last-Event-ID` replays from a
  per-session buffer (2,000 events or 4 MiB). A different epoch, or a gap
  older than the buffer, gets a single `reset`.
- **Open prompts:** open permission requests and questions are sent again
  when a client connects.
- **New types:** `turn_start` (with its origin: terminal, device, bus or
  resumed), `user_message` (prompts typed in the terminal too),
  `prompt_resolved`, `config`, `queued` and `reset`. Clients ignore types
  they don't know.
- **Delivery:** text and reasoning deltas are merged to at most 10 a second
  per stream, and a heartbeat comment is sent every 15 s.

### Prompts answered in two places

A permission request or question shows in the terminal and on every device
with control at the same time. The first answer wins, and every side gets
`prompt_resolved`.

A key meant for one prompt must never answer the next. Each prompt has an
id. A keypress answers the prompt that was on screen when it was pressed,
and input is ignored for 300 ms after a prompt appears or is replaced
(when the phone answered first, for example).

In the terminal, a turn a phone started shows `📱 From <device name>`, as bus
requests show `⇄ Request from …`.

## The remote terminal

A control device can open an interactive terminal in the session's working
directory. It's built on `codesmith serve`'s session terminals and the shell
daemon, with these limits:

- at most 4 terminals per device and 16 per machine;
- `remote.terminal: false` turns them off;
- a `watch` device gets 403.

It's a plain terminal, like `serve`'s session terminals: its shell doesn't
run under the session's permission rules. The unlock is its protection.

The routes are `POST /v1/sessions/{id}/terminals` to open, `GET` to list,
`PUT`/`DELETE /v1/terminals/{pty}` to resize, rename or close, and an
attach upgrade carrying framed data, resize, cursor and exit messages.
Opening, closing and attaching are audited; what's typed is not.

### The unlock

When `remote.terminal_unlock` is on (the default), opening a terminal needs
a passkey or a password, every time. One unlock covers one terminal.

- **Passkey.** At pairing, the phone sends
  `unlock_key: {alg: "ES256", spki: "<base64 DER SubjectPublicKeyInfo>"}`.
  It's a P-256 key in the Secure Enclave or Android Keystore that needs Face
  ID, Touch ID or the device credential for each signature, kept apart from
  the iroh device key. It's stored with the device and can be replaced only
  by pairing again.
- **Password.** You set it at the Mac's terminal with
  `codesmith remote password`, never from a device. Only an Argon2id hash is
  stored, in `remote.db`.

The open request answers 401
`{"error": {"code": "unlock_required"}, "challenge": "<base64url, 32 bytes>", "expires_in": 60, "methods": [...]}`.
`methods` lists only what's set up: `passkey` when the device registered a
key, `password` when one is set. With neither, terminals can't be opened from
that device, and the error says how to set one up.

The phone repeats the request with
`unlock: {method: "passkey", signature: "<base64 DER>"}` or
`{method: "password", password: "…"}`. A passkey signature is ECDSA P-256
over SHA-256 of exactly these bytes, UTF-8, with `\n` between fields and no
trailing newline:

```
codesmith-terminal-unlock/1
<purpose: "open" or "attach">
<machine node id, lower-case hex>
<device node id, lower-case hex>
<session id>
<pty id, or "-" when opening>
<challenge, base64url as sent>
```

The challenge is single use, expires in 60 s, and is stored with the exact
purpose, device, session and pty it was issued for. A signature for any
other values fails.

- **Lockout:** 10 failed unlocks in a row lock terminals for that device for
  5 minutes. Every unlock, failure and lockout is audited, never the
  password or signature.
- **Attaching:** reattaching to a terminal it opened needs no new unlock
  from the same device. A different device attaching must unlock with
  purpose `attach`.

## Push wake-ups

When a granted session needs an answer, or a turn ends or fails, and a
device has no open connection, the gateway posts a wake-up to the push
endpoint:

```json
{"v": 1, "sealed_token": "...", "kind": "attention", "machine": "<node id>",
 "ts": 1764110000000, "nonce": "<base64url, 16 bytes>", "sig": "<base64url>"}
```

`sig` is the machine key's ed25519 signature over exactly these bytes, with
`\n` between fields and no trailing newline:

```
codesmith-push/1
<machine node id, lower-case hex>
<kind>
<ts, decimal ms>
<nonce>
<sealed_token>
```

The endpoint must refuse a `ts` more than 5 minutes from its clock, and a
nonce it has already seen in that window. That makes a captured wake-up
useless to replay.

## Order

1. **A1, A2, A4, A5:** the machine key, the gateway, the QR and the claim,
   with `codesmith remote pair`. This lets a phone pair before the TUI
   adapters exist.
2. **A6–A8:** the read routes, events, and the owners' adapters (the TUI,
   `serve` and ACP).
3. **B:** control from the phone.
4. **B2:** the remote terminal and its unlock.
5. **C:** push wake-ups.

## To verify before building

- The iroh crate version and API names in the brief (`iroh = "1.5"`,
  presets, `endpoint.online()`), its minimum Rust version against ours
  (1.91), and its licence.
- How much it adds to the binary's size and build time.
- That iroh advertises a tailnet (`100.x`) address among its direct
  addresses, with a test.

## Tests

As well as unit tests, every phase needs end-to-end tests over loopback iroh
with relays off and fake device keys:

- **Pairing:** the right code and `y` give a grant. The same code again, or a
  wrong one, gets 410. Six bad claims lock that key out (429) and don't burn
  the live code. Enter alone, `n` and a timeout refuse. `a` needs its second
  confirmation, and a piped `y` with no terminal refuses.
- **Access:** an unpaired key is closed with code 1. A revoked device's open
  connection closes. A grant on session A gives 404 for B.
- **Config:** no `remote.*` setting from a project config takes effect.
- **Agent protection:** the agent's reads of the remote files, and its
  `codesmith remote` commands, always ask.
- **Mirroring:** a prompt typed in the terminal shows on the device, and a
  device's prompt shows its origin in the terminal. A prompt answered on one
  side resolves on the other, and a key pressed as a prompt is replaced
  answers nothing.
- **Resume:** reconnecting with `Last-Event-ID` leaves no gap. An old gap or
  a new epoch gets `reset`.
- **Scopes:** a `watch` device gets 403 on every control route.
- **Unlock:** the right signature opens a terminal. A replayed challenge,
  another purpose, session, pty or device, an expired challenge, a bad
  password and the lockout all fail. With `terminal_unlock: false`, no
  unlock is asked.
- **Push:** the signature verifies over the exact bytes above, and a changed
  field fails.
- **Pairing link:** the builder's links pass the clients' published test
  vectors.
- **Dormant by default:** with nothing opted in, no socket opens and no relay
  is contacted.

## Decisions on the open questions

Ray decided these on 2026-11-26:

1. The check code has 12 digits (see [The check code](#the-check-code)).
2. A remote terminal is a plain terminal; the unlock is its protection.
3. The lockout is 10 failed unlocks in a row, then 5 minutes.
