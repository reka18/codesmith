# Requirements: remote access from a phone

Status: **agreed with the CodeSmith Mobile team on 2026-11-26, with Ray's
answers to the open questions recorded.** Nothing here is built yet, and
nothing will be until Ray approves building. The app team confirmed every **Changed** item and
added the requirements marked **(app)**, which the phones already depend on.

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
| P7 | **Changed** (brief: the code's last four characters). The check code shown on both screens is derived from SHA-256 over `"codesmith-pair-check/1\n" + code + "\n" + machine id + "\n" + device id` (ids lower-case hex). It has 12 digits (Ray, 2026-11-26): the first 8 bytes as a big-endian u64, mod 10^12, zero-padded, shown `ddd ddd ddd ddd`. It must match the app's vectors (P11). |
| P11 | CodeSmith's link builder and check code pass the app's `transport/fixtures/pairing.json`: every valid link parses and round-trips, and every check-code vector matches For example:<br>• code `pn00cOpwBxfLmPR62H1EaQ`, machine `aa980150be84faa3788d7d170f02984f084bcaeb6871fe1fe7805ff2505a375c`, device `f8820d751e90d88eca3b848440efc98ae226f1552a6fb5c76c7ad09c96832dc6` gives `779 852 318 371`;<br>• the same code and machine with device `2862abb1…2dc6` give `425 748 398 457`;<br>• code `0123456789abcdef_-xy`, device `65b2276e47` followed by 54 zeros, gives `203 664 825 594`.<br>The full ids are in pairing.json. |
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
| S12 | **(app)** `GET /v1/sessions` items include at least `id`, `title`, `agent`, `model`, `directory` and `busy` (a JSON boolean), so a phone can check whether a session is busy without downloading its transcript. `GET /v1/sessions/{id}` has `busy` as a JSON boolean and `messages` in serve's record format: a role, then parts with a type and data (text as a string or `{text}`; `tool_call {id, name, input}`; `tool_result {id, name, content, is_error}`). |
| S13 | **(app)** Events carry at least these fields: `text.delta`, `reasoning.delta`, `tool_start {id, name, input}`, `tool_output {id, delta}`, `tool_end {id, output, error}`, `permission {id, action, resources[], title}`, `notice.message`, `retry.error`, and `done.error` (null or absent when clean). Every boolean is a JSON boolean, never a string. |
| S14 | **(app)** No event's serialised size exceeds 1 MiB. A larger `tool_output` delta or `tool_end` output is cut with a truncation marker, and the full text can be fetched later. (The phones end a stream on a line over 8 MiB or an event over 16 MiB.) |
| S15 | **(app)** `prompt_resolved.id` is the same id as the `permission` or `question` event it resolves. |
| S16 | **(app)** `GET /v1/machine` includes `features`, a list of what this machine supports, such as `session_reply`, `terminal` and `push`. The phones hide what isn't listed. |

### Reconnecting mid-turn

Agreed with the app team on 2026-11-27; it needs Ray's approval. These
rules let a phone that reconnects during a turn rebuild it exactly.

| Id | Requirement |
|---|---|
| S17 | **(app)** `seq` rises per session across turns, and `session` is set on every event, including the session's own. |
| S18 | **(app)** `GET /v1/sessions/{id}` returns `epoch`, `events_seq` (the last seq whose effect is in the response) and `turn: {id, start_seq} \| null` for the running turn. The owner builds the response and reads `events_seq` under the same lock that appends events, so no event is both in the snapshot and after `events_seq`, or in neither. Stored history covers finished turns, so a running turn's events all come after `events_seq`. |
| S19 | **(app)** The buffer always keeps every event from the running turn's `turn_start` on, up to **8 MiB** per session. Finished turns keep the S5 limit. To stay under the cap, consecutive `text`, `reasoning` and `tool_output` deltas of one block are merged into one event. It keeps the last seq and says `"merged_from": <first seq>`. Nothing else is ever merged or dropped. A resume seq always falls on an event boundary. |
| S20 | **(app)** A running turn over 8 MiB after merging: a resume gets `reset` with `"reason": "turn_too_large"`. `GET` then also returns `turn.partial` (the turn so far, in serve's message format) and `turn.partial_seq`, to subscribe from. A reconnect therefore never loops on `reset`. |
| S21 | **(app)** A replayed event has the same seq as the original, so a client that skips seqs it has already applied never applies `done` twice. `session_status {busy: false}` (C10) carries a seq too. |
| S22 | The tests check that a client which GETs mid-turn and subscribes from `events_seq` (or `partial_seq`) rebuilds the same transcript as one that watched live. They cut it before `turn_start`, before the first text, mid-text, mid-tool, during a permission prompt, and just before and after `done`. They also cover a turn longer than the S5 ring, an oversized turn, a new epoch, and `done` never applied twice. |

## E. Errors

| Id | Requirement |
|---|---|
| E1 | **(app)** Every `/v1` error body is `{"error": {"code": "...", "message": "..."}}`, never serve's `{"error": "string"}`. |
| E2 | **(app)** The codes are `bad_request` (400), `not_found` (404), `forbidden_scope` (403), `pairing_refused` (403), `busy` (409), `code_expired` (410), `too_large` (413), `rate_limited` (429), `unlock_required` (401), `unlock_failed` (401) and `unlock_locked` (429). New codes may be added; clients treat an unknown code by its status. |
| E3 | **(app)** A reply to a permission request or question that is no longer open (answered elsewhere, or timed out) answers 404 `not_found`. |

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
| C10 | **(app)** When a session actually goes idle, its stream gets `session_status {busy: false}`, also when an abort's `done {error: "aborted"}` was sent before the turn had stopped. (Today serve can send that `done` while the session still reports busy, and sends nothing when it finally stops, so the phones poll.) |
| C9 | Never offered to devices: shell commands outside a turn, fork, delete, archive, the environment, and editing saved permission rules. |

## T. The remote terminal (Phase B2)

**Changed** (brief: no terminals). Ray added remote terminals, protected by an
unlock.

| Id | Requirement |
|---|---|
| T1 | A control device may open an interactive terminal in the session's directory: at most 4 per device and 16 per machine. `remote.terminal: false` turns them off, and a `watch` device gets 403. It's a plain terminal, like serve's session terminals: its shell doesn't run under the session's permission rules, and the unlock is its protection (Ray, 2026-11-26). |
| T2 | Routes: `POST`/`GET /v1/sessions/{id}/terminals`, `PUT`/`DELETE /v1/terminals/{pty}`, and an attach upgrade carrying framed data, resize, cursor and exit messages. |
| T3 | With `remote.terminal_unlock` on (the default), every open needs an unlock, checked on the machine. One unlock covers one terminal. `remote.terminal_unlock: false`, set only in the global config, turns it off. |
| T4 | Passkey: the claim may carry `unlock_key: {alg: "ES256", spki}`, where `spki` is standard base64 with padding of the DER SubjectPublicKeyInfo. It's a P-256 key in the Secure Enclave or Android Keystore that needs biometrics or the device credential for each signature, separate from the iroh key. It's replaced only by pairing again. |
| T5 | Password: set only at the machine's terminal with `codesmith remote password`, stored only as an Argon2id hash. |
| T6 | An open without an unlock answers 401 `{"error": {"code": "unlock_required", "message"}, "challenge": "<base64url, 32 bytes>", "expires_in": 60, "methods": [...]}`, with the challenge fields at the top level and `methods` listing only the methods set up. With none, the message says how to set one up. |
| T6a | A wrong signature or password answers 401 `unlock_failed`. |
| T7 | A passkey signature is DER ECDSA P-256 over SHA-256 of the exact bytes in remote.md (`codesmith-terminal-unlock/1`, purpose, machine, device, session, pty or `-`, challenge). |
| T8 | A challenge is single use, expires in 60 s, and is bound to its purpose, device, session and pty. |
| T9 | Ten failed unlocks in a row lock terminals for that device for 5 minutes (Ray, 2026-11-26). While locked, an open answers 429 `{"error": {"code": "unlock_locked", "message"}, "retry_after": <seconds>}`. |
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
Status as the app team reported it on 2026-11-26.

| Id | Requirement | Status |
|---|---|---|
| M1 | Pin the machine's node id from the QR, and dial only that key. | Done, tested |
| M2 | Show the check code from P7 and ask the user to compare it. | Done for 6 digits; 12 is one change in their shared crate |
| M3 | Publish link and check-code test vectors, and keep the parser in step with P3. | Done: `transport/fixtures/pairing.json` |
| M4 | Ignore event types it doesn't know, and handle `reset` by reloading the session. | Unknown types done; `reset` once S4/S5 exist |
| M5 | Resume with `Last-Event-ID` as an opaque `<epoch>:<seq>` (S4). | Once S4/S5 exist; until then it reloads on reconnect |
| M6 | Never send an `Origin` header. | Done |
| M7 | Keep the unlock key separate from the iroh device key, and require biometrics or the device credential for every signature. | Planned, Phase B2 |
| M8 | Run the push endpoint with the checks in N6, and never put content in a notification. | Planned, Phase C |

## Before building

- Check iroh 1.5.0's API names against the brief, its minimum Rust version
  against ours (1.91; the app team builds it on 1.100), its licence, and what
  it adds to the binary's size and build time (the app's stripped arm64
  Android library with iroh is about 12.6 MB).
- Ray approves building.
