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
| Over-the-shoulder, screenshots, keyboards | Masked fields. The phone blocks screenshots and turns off keyboard learning and autofill. |
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

The answer is sealed with HPKE (RFC 9180, base mode, DHKEM X25519 +
HKDF-SHA256 + ChaCha20-Poly1305) to the helper's one-time key. The
associated data is
`"codesmith-askpass/1\n" + request id + "\n" + kind + "\n" + SHA-256 of the
program's arguments`, so a sealed answer opens only for the request it was
made for.

- **The terminal** seals in the TUI process as you press Enter. The
  plaintext lives only in the input buffer, which is zeroized at once.
- **A phone** seals on the phone. Plaintext exists only on the phone and in
  the helper.

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

- **Event:** `secret_prompt {id, kind, program, argv, command, cwd, expires,
  hpke_pk}`, on the session's stream and in the open prompts sent on
  connect. It never carries a secret.
- **Push:** a wake-up of kind `attention`, with no content, as for any
  prompt.
- **Answer:** `POST /v1/sessions/{id}/secrets/{rid}` with
  `{sealed, unlock: {method: "passkey", signature}}`. The signature is the
  unlock key's ES256 signature over these exact bytes:

  ```
  codesmith-secret-answer/1
  <machine node id>
  <device node id>
  <session id>
  <request id>
  <SHA-256 of the sealed bytes, hex>
  ```

  The Mac checks the signature against the unlock key from pairing, then
  forwards the sealed bytes. Only `passkey` is accepted. Typing the Mac's
  remote password to send another password would add nothing.
- **Replies:** 404 when the request isn't open (answered, refused or
  expired), 403 `forbidden_scope` for a `watch` device, 401 `unlock_failed`
  for a bad signature. A refusal is `{refuse: true}` with a signature.
- **App requirements:**
  - a masked secure field (iOS `isSecureTextEntry`, Android
    `TYPE_TEXT_VARIATION_PASSWORD` with `FLAG_SECURE` on the screen);
  - no suggestions, autofill, logging or analytics;
  - the plaintext cleared as soon as it's sealed;
  - a screen showing the program, its arguments and the session;
  - the answer sealed on the device;
  - nothing kept after sending.
- **Settings:**
  - `remote.secret_prompts`, global config only, default `true`. `false`
    keeps password prompts terminal-only.
  - Off on a machine whose devices have no unlock key.

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
- **Sealing:** a sealed answer for one request doesn't open for another. A
  changed byte fails. The gateway only ever holds ciphertext.
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
