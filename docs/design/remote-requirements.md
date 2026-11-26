# Requirements: remote access from a phone

Status: **draft, being agreed with the CodeSmith Mobile team.** Nothing here
is built yet, and nothing will be until Ray approves it.

This is the checklist CodeSmith must meet for remote access. The reasons and
details are in [remote.md](remote.md); the app team's original request is
codesmithmobile-dev's upstream brief of 2026-11-25. Each requirement has an
id, so commits, tests and messages can refer to it. Where a requirement
differs from the brief, it's marked **Changed** and says how. The app team
must confirm each of those before building starts.

Words: **must** is required for the phase to ship. A **device** is a paired
phone or other client. The **gateway** is `codesmith __remoted`.

## G. General

| Id | Requirement |
|---|---|
| G1 | Nothing opens a socket, contacts a relay or calls a push endpoint until the user runs `/mobile`, `codesmith remote pair` or `codesmith remote start`, or sets `remote.enabled: true`. A test checks this with the network blocked. |
| G2 | Remote access is behind the cargo feature `remote`. It's on in release builds, and the workspace builds and passes its tests without it. |
| G3 | CodeSmith contains no payment, subscription or licence logic. |
| G4 | The protocol is public: `docs/remote-protocol.md` describes every route, status, event and signed byte string, and `docs/remote-protocol.schema.json` gives a JSON Schema for every body. |
| G5 | Every `remote.*` setting is read only from the user's global config. A `remote` key in a project config has no effect and gets a warning. |
| G6 | `/mobile` doesn't ship in a release until the Skeptic Dog relay URLs exist (G7). Until then it's in development builds only. |
| G7 | **Changed** (brief: n0's preset is the default). `remote.relay: "default"` means Skeptic Dog's relays only, never n0's public ones. Development builds may use n0's preset. A relay URL means that one relay; `"none"` means direct addresses only. |

## I. Identity and the gateway

| Id | Requirement |
|---|---|
| I1 | The machine key is `remote-key` in the data folder: 32 raw ed25519 secret bytes, mode 0600, made on first use and written atomically. |
| I2 | The node id is iroh's `Display` of the public key (64 lower-case hex characters). `codesmith remote id` prints it. |
| I3 | The gateway is one per user per machine, with a lock file and a 0700 runtime folder, and takes over from an older version on upgrade. |
| I4 | The gateway runs while any device is paired and `remote.enabled` is on. `codesmith remote stop` ends it, and `codesmith remote status` reports it. |
| I5 | The endpoint accepts the ALPNs `codesmith/pair/1` and `codesmith/remote/1`. Any other ALPN is closed with error code 2. |
| I6 | On `codesmith/pair/1`, a connection with no pairing code live is closed with error code 3. |
| I7 | On `codesmith/remote/1`, only a paired, unrevoked device key is accepted; any other is closed with error code 1. Revoking a device closes its open connections at once. |
| I8 | Each bidirectional stream carries one HTTP/1.1 request and its response. There's no `Authorization` header, `Host` is ignored, and a request with an `Origin` header is refused. |
| I9 | The TUI, `codesmith serve` and ACP register their sessions with the gateway over a Unix socket, authenticated by a token in a 0600 file plus a peer uid check. |
| I10 | State is in `remote.db`: devices, grants, the audit log, push tokens and unlock keys. |
| I11 | The direct addresses include a tailnet (`100.x`) address when there is one, with a test. |

## P. Pairing

| Id | Requirement |
|---|---|
| P1 | `/mobile` and `codesmith remote pair [--session ID] [--all] [--watch]` start the gateway if needed, wait up to 5 s for the home relay, and show a QR code with Unicode half-blocks, a quiet zone and error correction level L. The mouse is never captured. |
| P2 | Under the QR: the machine name, the session title, a countdown, and the `codesmith://pair?…` link as text under a copy key. |
| P3 | The link is `codesmith://pair?v=1&n=&r=&a=&m=&s=&c=` exactly as in the brief's A4, with every value percent-encoded using RFC 3986 unreserved characters only. Its builder passes the app's published test vectors. |
| P4 | The pairing code is 16 bytes from the OS RNG as base64url without padding, valid 180 s, single use, one live code per gateway, and replaced when it expires until the dialog is closed. |
| P5 | `POST /v1/pair/claim {code, device: {name, platform}, push?, unlock_key?}`. The device id is the connection's node id, never the body. The name has control characters stripped and is cut to 64 characters. Statuses as in the brief's A5. |
| P6 | **Changed** (brief: one global limit). The claim rate limit is per device key: more than 5 bad claims in 60 s lock that key out for 5 minutes (429), and a locked key's claims don't burn the live code. |
| P7 | **Changed** (brief: the code's last four characters). The check code shown on both screens is derived from SHA-256 over `"codesmith-pair-check/1\n" + code + "\n" + machine id + "\n" + device id` (ids lower-case hex). Its length is open question 1. |
| P8 | The terminal asks `Allow "<name>" (<platform>, <10-hex short id>) to control "<session>"?`, with the check code, and `[y] yes [w] watch only [a] all sessions [n] no`. A paired device's prompt says "(paired device)" and adds only the new grant. |
| P9 | Only `y` or `yes` allows. Enter alone, `n`, Esc or 90 s without an answer refuses. **Changed:** `a` asks a second time (`Type "all" to confirm`). |
| P10 | The confirmation is read from the controlling terminal, never stdin. With no terminal, or a piped answer, pairing refuses. |

## S. Sessions, events and mirroring (Phase A)

| Id | Requirement |
|---|---|
| S1 | The read routes are those in the brief's A6, with payloads following `codesmith serve`'s. |
| S2 | Every route checks the device's grant. A session without a grant answers 404, never 403. `/v1/sessions` lists only granted sessions. |
| S3 | A granted session no process holds shows `owner: "closed"`, read-only, with its history from the store. |
| S4 | **Changed** (brief: no epoch). Every event has the envelope `{v, epoch, seq, session, turn, ts, type}`, where `epoch` is random per owning process. The SSE id is `<epoch>:<seq>`. |
| S5 | `Last-Event-ID` replays from a per-session buffer of 2,000 events or 4 MiB. A different epoch, or a gap older than the buffer, gets one `reset`. |
| S6 | Open permission requests and questions are sent again when a client connects. |
| S7 | The new event types are `turn_start` (with origin), `user_message` (including prompts typed in the terminal), `prompt_resolved`, `config`, `queued` and `reset`. |
| S8 | Text and reasoning deltas are merged to at most 10 a second per stream, and a `: ping` heartbeat is sent every 15 s. |
| S9 | A prompt shows in the terminal and on every control device at once. The first answer wins, and every side gets `prompt_resolved`. |
| S10 | A key press answers only the prompt that was on screen when it was pressed, and terminal input is ignored for 300 ms after a prompt appears or is replaced. |
| S11 | In the terminal, a turn a device started shows `📱 From <device name>`. |

## C. Control (Phase B)

| Id | Requirement |
|---|---|
| C1 | The control routes are those in the brief's Phase B. A message answers 202 with its turn id at once, or 409 `busy`. |
| C2 | A permission reply may be `once`, `session` or `reject`. `always` is refused with 400. |
| C3 | A control device may switch agent, model and effort; a change takes effect at the end of the turn. The config logic moves from ACP into core. |
| C4 | A control device may run slash commands, including ones that expand to shell commands, which still go through the normal permission checks. Slash command expansion moves into core. |
| C5 | A control device may approve reads of secret-guarded files. |
| C6 | `permission` events carry `origin`, and for edit, write and patch, a unified diff of at most 256 KiB. |
| C7 | `remote.prompt_timeout` (default `30m`, `0` forever) replaces serve's fixed 30 s refusal for sessions with a control grant. |
| C8 | A `watch` device gets 403 `forbidden_scope` on every control route. |
| C9 | Never offered to devices: shell commands outside a turn, fork, delete, archive, the environment, and editing saved permission rules. |

## T. The remote terminal (Phase B2)

**Changed** (brief: no terminals). Ray added remote terminals, protected by an
unlock.

| Id | Requirement |
|---|---|
| T1 | A control device may open an interactive terminal in the session's directory: at most 4 per device and 16 per machine. `remote.terminal: false` turns them off, and a `watch` device gets 403. |
| T2 | Routes: `POST`/`GET /v1/sessions/{id}/terminals`, `PUT`/`DELETE /v1/terminals/{pty}`, and an attach upgrade carrying framed data, resize, cursor and exit messages. |
| T3 | With `remote.terminal_unlock` on (the default), every open needs an unlock, checked on the machine. One unlock covers one terminal. `remote.terminal_unlock: false`, set only in the global config, turns it off. |
| T4 | Passkey: the claim may carry `unlock_key: {alg: "ES256", spki}`, a P-256 key in the Secure Enclave or Android Keystore that needs biometrics or the device credential for each signature, separate from the iroh key. It's replaced only by pairing again. |
| T5 | Password: set only at the machine's terminal with `codesmith remote password`, stored only as an Argon2id hash. |
| T6 | An open without an unlock answers 401 `unlock_required` with a 32-byte base64url challenge, `expires_in: 60`, and `methods` listing only the methods set up. With none, the error says how to set one up. |
| T7 | A passkey signature is DER ECDSA P-256 over SHA-256 of the exact bytes in remote.md (`codesmith-terminal-unlock/1`, purpose, machine, device, session, pty or `-`, challenge). |
| T8 | A challenge is single use, expires in 60 s, and is bound to its purpose, device, session and pty. |
| T9 | Five failed unlocks in a row lock terminals for that device for 15 minutes (open question 3). |
| T10 | Reattaching from the device that opened the terminal needs no new unlock. Any other device unlocks with purpose `attach`. |
| T11 | Opening, closing, attaching, unlocks, failures and lockouts are audited. What's typed, passwords and signatures never are. |

## N. Push wake-ups (Phase C)

| Id | Requirement |
|---|---|
| N1 | Push turns on with `/mobile`, and its first run says so. `remote.notify: "connected-only"` turns it off. |
| N2 | `PUT /v1/push {kind, token}` stores the device's opaque token. |
| N3 | When a granted session needs an answer, or a turn ends or fails, and a device has no open connection, the gateway posts a wake-up to `remote.push_endpoint`. |
| N4 | A wake-up never contains content: no titles, code or commands. |
| N5 | **Changed** (brief: a signature over "the JSON without sig"). The body adds a 16-byte `nonce`, and `sig` is ed25519 by the machine key over the exact bytes in remote.md (`codesmith-push/1`, machine, kind, ts, nonce, sealed token). |
| N6 | The push endpoint (the app team's service) must refuse a `ts` more than 5 minutes off and a nonce already seen in that window. |

## X. Protection from the agent

| Id | Requirement |
|---|---|
| X1 | `remote-key`, `remote.db` and everything in the gateway's runtime folder get the built-in secret-file guards, which always ask. |
| X2 | The agent's `codesmith remote …` shell commands always ask, except `id` and `status`. |
| X3 | Pairing can't be confirmed from the agent's shell (P10). |

## U. Audit and management

| Id | Requirement |
|---|---|
| U1 | Every device action is audited (prompt sent, permission reply, abort, config change, undo, grant change, pairing, terminal events), never content. |
| U2 | `/mobile devices`, `/mobile log [DEVICE]`, `/mobile revoke DEVICE`, `/mobile off` and `/mobile all`, and the matching `codesmith remote` subcommands. |

## What CodeSmith needs from the app

| Id | Requirement |
|---|---|
| M1 | Pin the machine's node id from the QR, and dial only that key. |
| M2 | Show the check code from P7 and ask the user to compare it. |
| M3 | Publish link test vectors (the brief's five, plus any new ones), and keep the parser in step with P3. |
| M4 | Ignore event types it doesn't know, and handle `reset` by reloading the session. |
| M5 | Resume with `Last-Event-ID` as `<epoch>:<seq>` (S4). |
| M6 | Never send an `Origin` header. |
| M7 | Keep the unlock key separate from the iroh device key, and require biometrics or the device credential for every signature. |
| M8 | Run the push endpoint with the checks in N6, and never put content in a notification. |

## Before building

- Check that iroh 1.5 exists with the API names in the brief, its minimum
  Rust version against ours (1.91), its licence, and what it adds to the
  binary's size and build time.
- Ray answers the open questions in remote.md: the check code length, whether
  remote terminals are plain, and the lockout numbers.
- The app team confirms every **Changed** item above.
