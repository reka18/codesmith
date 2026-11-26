# Design: passwords for commands, from the terminal and the phone

Status: **plan for approval.** Nothing here is built yet.

When a command the agent runs needs a password (`sudo`, an `ssh` key's
passphrase, `git` over HTTPS), CodeSmith asks you for it. You type it in the
terminal, or on a paired phone. It goes to that program and nowhere else.

## The promise, and its limit

**The model never sees the password.** It's never in the transcript, a tool's
output, a log, a usage line, the event stream, a push, the session store or
an export.

**The password is never kept.** It isn't saved, cached or offered for
"remember". Every prompt asks again.

**The limit:** the agent's commands run as you, and so does CodeSmith. A
hostile command can do anything you can do, so nothing makes theft by such a
command impossible. The design below makes it hard and visible:
- the password goes only to the real program that asked for it;
- it's encrypted for that one helper process;
- every prompt shows exactly what will run.

The docs will say so plainly.

## What it covers

| Program | How it asks | Kind |
|---|---|---|
| `sudo` | `SUDO_ASKPASS` with `-A` | secret |
| `ssh`, `scp`, `sftp`, `git` over SSH | `SSH_ASKPASS`, `SSH_ASKPASS_REQUIRE=force` | secret: a passphrase or password; confirm: a host key or a security-key touch |
| `git` over HTTPS | `GIT_ASKPASS` | plain: the username; secret: the password or token |

`pkexec`, `su`, `doas` and programs that read `/dev/tty` themselves aren't
covered. Agent commands have no terminal, so these fail as they do today, and
CodeSmith says why.

## Threats

| Threat | Defence |
|---|---|
| The model, by any route | The password never enters the session. Only an encrypted copy crosses CodeSmith's own processes ([Sealing](#sealing)). |
| A hostile command fakes a request to steal the password | Process checks: the password goes only to a helper started by the real `sudo`, `ssh` or `git`, inside the command that asked. The request needs that command's single-use token. The prompt shows the real program's arguments. |
| A hostile command hides a `sudo` inside a script | The prompt shows the arguments `sudo` will run, read from the process itself, beside the agent's command. |
| The agent reads the helper's memory or pipe | The helper can't be debugged or dumped, and it lives only for the one request. |
| `sudo`'s grace period gives later commands root silently | Every `sudo` ignores cached credentials (`-k`), and CodeSmith removes the timestamp (`sudo -K`) when each command ends. |
| The gateway, a relay or the network | The phone encrypts to the helper's key. The gateway and the relay forward only ciphertext, inside iroh's TLS. |
| A phone that's unlocked but in the wrong hands | Every secret prompt on the phone needs a passkey signature (Face ID, Touch ID or the device credential). The Mac checks it. |
| Over-the-shoulder, screenshots, keyboards | Masked fields, with keyboard learning and autofill off. Android blocks screenshots. iOS can't, but blanks its secure field in screenshots and hides the prompt's details while the screen is captured. |
| Replay | A request id, an expiry, and a passkey signature over both. |

## How it works

### Wiring (CodeSmith)

Every command the agent runs gets these, from the session, never from config
a project can set:

- `SUDO_ASKPASS`, `SSH_ASKPASS` and `GIT_ASKPASS` pointing at
  `codesmith __askpass`, plus `SSH_ASKPASS_REQUIRE=force`.
- `CODESMITH_ASKPASS_SOCKET`: the session's askpass socket, in its 0700
  runtime folder.
- `CODESMITH_ASKPASS_TOKEN`: 32 random bytes, valid only while this command
  runs.
- A `sudo` wrapper first on `PATH`. It runs the real `sudo` (found as a
  root-owned setuid file) with `-A -k` added. A command calling
  `/usr/bin/sudo` directly still has no terminal. If it uses the askpass at
  all, the process checks below still apply, and `sudo -K` at the end clears
  any timestamp it left.

### A request

1. `sudo` (or `ssh`, or `git`) runs `codesmith __askpass "<its prompt>"`.
2. The helper hardens itself:
   - `PR_SET_DUMPABLE 0` on Linux, `PT_DENY_ATTACH` on macOS (the release
     binary also has the hardened runtime);
   - no core dumps;
   - its memory locked where it can be.

   It then makes a one-time X25519 key pair.
3. It connects to the socket and sends the token, the kind (secret, plain or
   confirm), the program's prompt text and its public key.
4. **The session checks the peer** before showing anything:
   - its uid is yours;
   - its parent process is the real program: for `sudo`, a root-owned
     setuid binary at its usual path; for `ssh` and `git`, the binaries on
     the command's `PATH`, resolved and recorded when the command started;
   - that program is inside the process group of the command that holds the
     token;
   - the token is live, with at most 3 requests per command and 10 a minute
     per session.

   Anything else is refused, and the audit log notes it.
5. **The session reads what will run** from the parent process: `sudo`'s
   arguments, or `ssh`'s host. It shows the prompt to you (below) with the
   request id, an expiry of 2 minutes and the helper's public key.
6. The first answer wins, from the terminal or a phone. It arrives sealed to
   the helper's key ([Sealing](#sealing)). The session passes it to the
   helper unread.
7. The helper opens it, writes the password to its stdout (which goes to
   `sudo`, not to the command's output) and exits. Esc, "no", or the expiry
   makes it exit with an error, and `sudo` fails as if nothing was typed.
8. When the command ends, the token is revoked and `sudo -K` runs.

### Sealing

The answer is sealed with HPKE (RFC 9180, base mode, DHKEM(X25519,
HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305) to the helper's one-time key.
CodeSmith uses the `hpke` crate (rust-hpke), and so does the app's shared
Rust transport crate, so both ends run the same implementation.

| Part | Value |
|---|---|
| `info` | the fixed bytes `codesmith-askpass/1` |
| AAD | `"codesmith-askpass/1\n" + request id + "\n" + kind + "\n" + argv_sha256`, UTF-8 |
| `argv_sha256` | computed by the Mac only: SHA-256 over each argument's raw bytes followed by one NUL byte (terminated, so `[]` and `[""]` differ), as lower-case hex. The phone copies it from the event into the AAD as-is and never recomputes it. |
| Plaintext | UTF-8, no trailing newline (the helper adds the newline `sudo` and `ssh` expect), at most 4 KiB. A `secret` is the password, a `plain` is the text typed (a username), a `confirm` is `yes`. A "no" is a refusal, never sealed. |
| `sealed` | base64url without padding of `enc` (32 bytes) followed by the ciphertext |

A sealed answer therefore opens only for the request it was made for.

- **The terminal** seals in the TUI process as you press Enter. The
  plaintext lives only in the input buffer, which is zeroized at once.
- **A phone** seals on the phone. Plaintext exists only on the phone and in
  the helper.
- **Test vectors** ship in this document before the phone phase is built,
  made with rust-hpke: key pair, plaintext, `info`, AAD, `enc` and
  ciphertext, for each kind, plus a check of RFC 9180's A.2 vectors for
  this suite (DHKEM(X25519), HKDF-SHA256, ChaCha20-Poly1305).

### In the terminal

A modal outside the transcript:

```
 sudo wants your password
 to run:   apt-get install -y libssl-dev
 because:  the agent's command  `make deps`  in ~/src/app
 [••••••••        ]          Enter send · Esc refuse     1:54
```

- Typing is never echoed, and the modal isn't in history, undo, the
  transcript, export or a screenshot of the transcript.
- Input is ignored for 300 ms after the prompt appears or changes, as for
  permission prompts.
- A `confirm` kind (an ssh host key) shows the program's text with
  yes and no buttons. A `plain` kind (a git username) is a normal field.
- In a session with no terminal (`codesmith run`, a bridge, ACP without a
  client prompt), a request is refused at once with a clear error, unless a
  phone with control is paired (below).

### On the phone

This needs remote access ([remote.md](remote.md)) through Phase B, and a
device with `control` on the session.

- **Event:** `secret_prompt`, on the session's stream and in the open
  prompts sent on connect. It never carries a secret.

  ```json
  {"type": "secret_prompt", "id": "<request id>", "kind": "secret|plain|confirm",
   "prompt": "[sudo] password for ray:", "program": "sudo",
   "argv": ["apt-get", "install", "-y", "libssl-dev"], "argv_sha256": "<hex>",
   "command": "make deps", "cwd": "/Users/ray/src/app",
   "session_title": "fix-the-build", "expires_in": 120, "hpke_pk": "<base64url, 32 bytes>"}
  ```

  - `prompt` is the program's own text, such as `sudo`'s line or `ssh`'s
    host-key fingerprint. A `confirm` can't be shown without it.
  - `argv` is for display only: strings, with any non-UTF-8 bytes escaped.
    `argv_sha256` is what's bound into the seal.
  - `cwd` is always absolute.
  - `expires_in` is seconds from sending, so the phone counts down from when
    it receives the event, whatever its clock says.
- **Who gets it:** only devices with `control` on the session and an unlock
  key. A device without a key never gets the event, rather than showing a
  prompt it can't answer.
- **Closing it:** `prompt_resolved {id, kind: "secret"}` when it's answered
  elsewhere, refused or expired. The same `id` is used, as in S15 of the
  remote requirements.
- **Push:** a wake-up of kind `attention`, with no content, as for any
  prompt.
- **Answer:** `POST /v1/sessions/{id}/secrets/{rid}` with
  `{sealed, unlock: {method: "passkey", signature}}`. The signature is the
  unlock key's ES256 signature, DER-encoded and base64 as for the terminal
  unlock (T4/T7), over these exact bytes (UTF-8, `\n` between lines, no
  trailing newline):

  ```
  codesmith-secret-answer/1
  <machine node id>
  <device node id>
  <session id>
  <request id>
  <SHA-256 of the decoded sealed bytes, lower-case hex>
  ```

  A refusal is `{refuse: true, unlock: {method: "passkey", signature}}`,
  signed over the same lines without the last one, headed
  `codesmith-secret-refuse/1`.

  The Mac checks the signature against the unlock key from pairing, then
  forwards the sealed bytes. Only `passkey` is accepted. Typing the Mac's
  remote password to send another password would add nothing.
- **Replies:**

  | Status | When | Request |
  |---|---|---|
  | 200 | sent or refused | closed |
  | 400 `bad_request` | `sealed` is malformed or the wrong length | stays open |
  | 401 `unlock_failed` | a bad signature | stays open: one more try (a cancelled biometric, say); the second failure refuses it |
  | 403 `forbidden_scope` | a `watch` device | stays open |
  | 404 `not_found` | not open: answered, refused or expired | — |
- **App requirements:**
  - a masked secure field (iOS `isSecureTextEntry`, Android
    `TYPE_TEXT_VARIATION_PASSWORD`) with no suggestions, autofill, logging
    or analytics;
  - the answer sealed on the device, in the shared Rust crate;
  - nothing kept after sending;
  - a card that opens a secure sheet, shown like a question: it doesn't
    steal focus while you're typing, and later prompts queue behind it;
  - the sheet showing the prompt text, the program and its arguments, the
    agent's command, the folder and the session title;
  - `argv` never put in a notification, a log or the app's cache, because
    it can hold secrets of its own (`mysql -pPASS`).
- **Limits the app states plainly:**
  - **iOS can't block screenshots.** Its secure field is blanked in
    screenshots and recordings, and the app hides the prompt's details
    while the screen is being captured. Blocking screenshots
    (`FLAG_SECURE`) is Android only.
  - **Clearing the plaintext is best effort.** Swift strings can't be
    zeroed, so iOS keeps the secret in a byte buffer it overwrites after
    sealing. Android uses a `CharArray`, never a `String`.
- **Settings:** `remote.secret_prompts`, global config only, default
  `true`. `false` keeps password prompts terminal-only.

### Touch ID for sudo (macOS, local only)

When you're at the Mac, a fingerprint is better than any password
passthrough. `codesmith setup sudo-touch-id` checks
`/etc/pam.d/sudo_local`, shows the one line it would add
(`auth sufficient pam_tid.so`), and adds it with `sudo` only after a yes.
`sudo` then asks for Touch ID first. Nothing passes through CodeSmith, and
the password prompt above is only the fallback.

If you're on the phone, a Touch ID dialog on the Mac's screen can't be
answered. CodeSmith tells the phone it's waiting for Touch ID on the Mac,
then shows the password prompt once `sudo` falls back. Open question 2 asks
whether to detect this and skip Touch ID.

## Permissions and audit

- **Always asks:** a command containing `sudo` always asks for approval,
  whatever the saved rules or an "always" answer say, as keychain reads do.
  Approving the command and typing the password are two separate steps.
- **Audit:** every request, refusal, answer and failed check goes in the
  audit log: the kind, the program, a hash of the arguments, the session,
  the device or terminal, and the outcome. Never the secret, never the
  sealed bytes.
- **Settings:** `askpass.enabled` (default `true`) and `askpass.programs`
  (default all three), from the global config only.

## Order

1. **The terminal:** `__askpass`, the wiring and the `sudo` wrapper, the
   process checks, sealing, the modal, `sudo -k`/`-K`, permissions and
   audit, for `sudo`, `ssh` and `git`.
2. **Touch ID setup** on macOS.
3. **The phone,** with remote access Phase B: the event, the route, the
   signature, and the app's screen.

## Tests

- **Never seen:** a fake `sudo` that prints its stdin. The password reaches
  it, and doesn't appear in the tool output, the transcript, the store, the
  usage log, an event or an export.
- **Process checks:** each of these is refused, and none gets a prompt
  shown:
  - a request from the agent's own command, not the real `sudo`;
  - a wrong, expired or reused token;
  - a parent outside the command's process group;
  - another uid.
- **What will run:** the prompt shows `sudo`'s real arguments, including
  when the `sudo` is hidden inside a script.
- **Grace period:** a second `sudo` in the same command, and one in the next
  command, both ask again.
- **Sealing:**
  - a sealed answer for one request doesn't open for another, and a changed
    byte fails;
  - the gateway only ever holds ciphertext;
  - CodeSmith passes the published vectors and RFC 9180's A.2 vectors;
  - `argv_sha256` tells `[]` from `[""]` and keeps non-UTF-8 bytes exact.
- **Replies:** a bad signature leaves one more try, then refuses. A
  malformed `sealed` gets 400 and leaves the request open. A device without
  an unlock key never gets the event.
- **The phone:**
  - the right signature sends;
  - a replayed or altered signature fails;
  - another device's signature fails;
  - a `watch` device gets 403;
  - an expired request gets 404;
  - with `remote.secret_prompts: false`, nothing is offered;
  - the push has no content.
- **First answer wins** between the terminal and the phone, and the other
  side closes its prompt.
- **No terminal:** `codesmith run` with no phone refuses at once, with the
  error.
- **Hardening:** an attempt to attach to the helper or dump its core fails,
  on Linux and macOS.
- **Rate limits** and the audit lines.

## Open questions

1. Should `remote.secret_prompts` default to `true` (as drafted, because you
   want it on the phone), or be opt-in?
2. With Touch ID set up and you on the phone, should CodeSmith skip Touch ID
   for that `sudo` (`sudo` can't be told to; it would need a separate PAM
   service for agent commands), or wait for the fallback as drafted?
3. Should a command that calls the real `sudo` directly, bypassing the
   wrapper, be refused outright rather than allowed through the checks?
