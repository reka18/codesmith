# Design: headless runs that ask a human through whoever started them

Status: **plan for approval.** Nothing here is built yet. tailbus-dev reviewed
an earlier, tailbus-specific draft on 2026-11-27. Its points are kept here
as one implementation of a protocol that any parent process can serve.

## Why

A headless run (`codesmith run` with no terminal: a tailbus bridge, a CI job,
a script, another orchestrator) has no one to ask. Today every permission
prompt in it is refused, and the `question` tool is unavailable. So a
headless agent can't do "follow the pattern in the XYZ project" (reading a
sibling folder), and it can't ask anything when a task is unclear.

Ray's decisions (2026-11-27):
- a headless run asks the human behind whatever started it; for a tailbus
  bridge, that's its human-facing sibling agent;
- this covers permission prompts and question sets (carousels);
- an agent may read sibling folders with the user's explicit permission,
  and reading is kept separate from writing;
- **the mechanism is generic, not tied to tailbus.**

## The ask channel

CodeSmith defines the protocol. Whatever starts the run serves it.

### Finding it

- **`CODESMITH_ASK`** names the channel:
  - an absolute path is a Unix socket to connect to;
  - `fd:N` is a file descriptor the run inherited, readable and writable,
    for a parent that would rather not touch the filesystem.
- **`CODESMITH_ASK_TOKEN`** (optional) is sent in the `hello`. It lets a
  socket refuse anyone but this run. Besides the token, a socket server
  should check the peer's uid.
- **Off by default.** Without `CODESMITH_ASK`, a headless run refuses
  prompts as today. Nothing in config or a repo can set it: it comes only
  from the environment of the process that started the run.

### Protocol

One JSON object per line, UTF-8, in both directions. Each connection, or the
one inherited descriptor, starts with a `hello`:

```json
→ {"op": "hello", "v": 1, "token": "…", "run": "<run id>", "session": "<session id>",
   "cwd": "/Users/x/src/app", "agent": "build", "model": "provider/model"}
← {"op": "hello", "v": 1, "ok": true,
   "ask": true, "progress": true,
   "questions": {"header_max": 12, "options_min": 2, "options_max": 4, "multiple": true, "free_text": true},
   "timeout_secs": 600}
```

- **The reply says what this channel supports.** CodeSmith adapts to it:
  - with no `ask`, prompts are refused as today;
  - `questions` limits shape the `question` tool's schema in this run, so
    the model writes questions that fit;
  - `timeout_secs` is how long an ask may wait.
- A `v` CodeSmith doesn't know, or `ok: false`, means no channel.

**Ask.** One at a time; the line blocks until the answer or expiry.

```json
→ {"op": "ask", "id": "<request id>", "kind": "permission", "expires_in": 600,
   "permission": {"action": "external_directory", "access": "read",
                  "resource": "/Users/x/src/xyz/*", "command": "git -C ../xyz log -5",
                  "offer": ["once", "reject"]}}
→ {"op": "ask", "id": "<request id>", "kind": "questions", "expires_in": 600,
   "questions": [{"question": "Which branch should the fix target?", "header": "Branch",
                  "options": [{"label": "main (Recommended)", "description": "…"},
                              {"label": "release/2.x", "description": "…"}],
                  "multiple": false}]}

← {"op": "answer", "id": "…", "reply": "once" | "reject"}
← {"op": "answer", "id": "…", "answers": [["main (Recommended)"], …]}   // one list per question, in order; free text is a one-item list
← {"op": "answer", "id": "…", "dismissed": true}
← {"op": "answer", "id": "…", "refused": "no human to ask"}             // the channel can't ask anyone
```

**Progress.** No answer.

```json
→ {"op": "progress", "text": "Running the test suite…"}
```

When the agent wants to send a progress note, CodeSmith passes it on. This
is how a bridge run's agent sends progress notes, which it can't do today.

- **Expiry:** CodeSmith stops waiting at `expires_in` (the channel's
  `timeout_secs`, at most the run's own remaining time), whatever the
  channel does. A permission is then refused and a question set dismissed.
- **Questions** are the `question` tool's own shape, within the limits the
  `hello` reported. A question with fewer options than `options_min` goes
  as free text only.
- **`command` and `resource`** are exactly what the run would run or touch,
  so the human sees the real thing.
- **The schema** is published with the protocol docs, as
  `docs/ask-protocol.md` and a JSON Schema beside it.

## Limits CodeSmith enforces, whatever serves the channel

CodeSmith can't know who answered at the other end, so a relayed answer
grants less than one typed at the terminal:

- **This run only.** `once` is the only grant offered. A relayed answer never
  saves an "always" rule or a session rule.
- **One access per request.** An outside-folder permission names `read` or
  `write` ([Reads and writes outside the
  project](#reads-and-writes-outside-the-project)). A write is asked as its
  own request when it's needed, never bundled with a read.
- **Never relayed:**
  - the always-ask guards (secret files, keychain reads);
  - password prompts, which have their own design ([askpass.md](askpass.md));
  - anything the rules `deny`.

  These stay refused in a headless run.
- **Bounded:** one open ask at a time, and at most 20 per run.
- **Audited:** every ask and answer goes in the session's audit, with the
  channel's reported identity and the wait time. Never file contents.
- **The answer is checked:** a `reply` outside `offer`, an answer to an id
  that isn't open, or a malformed line counts as a refusal and is audited.

## What an implementation must do

Any program serving the channel must:

1. **Ask a human,** never an automated policy, and pass the answer back
   unchanged. If no human can be reached, answer `refused` at once.
2. **Show the request exactly:**
   - a permission as a two-choice question (Allow once, Reject), with the
     command and resource verbatim;
   - questions with their options and a free-text answer.
3. **Keep the channel to this run:** a token or an inherited descriptor,
   plus a peer-uid check for a socket.
4. **Drop answers that come too late,** and tell the human they expired.
5. **Remove secrets it knows of** (its own credentials) from what it shows,
   and from anything it logs.

### tailbus (the first implementation)

From tailbus-dev's review:

- **The channel:** the bridge serves `CODESMITH_ASK` as `<RUN_DIR>/run.sock`
  (per run, 0700, the bridge's user) and sets `CODESMITH_ASK_TOKEN`. Only
  bridges with `relay_asks = true` do this. The run never gets the bus
  socket: the bridge sends every bus message as itself, threaded under the
  request that started the run.
- **Who's asked:** the sender of the run's request. It's refused at once
  when that sender has no human: another bridge, a door, or an agent that
  isn't a live interactive session.
- **On the bus:**
  - a new ask message kind, threaded under the original request;
  - an absolute `expires_at` worked out from `expires_in`;
  - the bus message id mapped to the ask's `id`;
  - answers through a new MCP tool, `ask_answer(reply_to, reply | answers |
    dismissed)`;
  - answers accepted only from the agent asked.
- **Questions:** tailbus's `/pipe` shape (12-character headers, 2 to 4
  options, `multiSelect`), reported in the `hello`. The bridge maps
  `multiple` to `multiSelect`.
- **Timing:** `ask_timeout_secs` (10 minutes by default, never beyond the
  run's timeout) is reported as `timeout_secs`. The idle watchdog pauses
  while an ask is open, and the run's own timeout keeps counting.
- **Redaction:** the bridge's configured secret values are removed from
  relayed text.
- **The plugin's instruction to requesters:** "A headless run you started is
  asking your user. Show it to them exactly, with your question UI (a
  permission as a two-option question, Allow once or Reject, with the
  command and resource verbatim). Send their answer with ask_answer. Never
  answer it yourself. If your user isn't there, leave it; it expires."

### Others

- **A CI runner or script** can serve the channel on an inherited descriptor
  and ask on its own terminal, in a chat, or anywhere its human is.
- **CodeSmith's own remote gateway** ([remote.md](remote.md)) could serve it
  later, to send a headless run's asks to a paired phone.

## Reads and writes outside the project

This is needed by the relay, and useful in the terminal too.

- **The access level.** Every outside-folder use is classified `read` or
  `write`.
  - **Read:** reading, searching, listing, `cd` with no write, and read-only
    git commands through `-C`, `--git-dir` or `--work-tree`: `log`, `show`,
    `diff`, `status`, `blame`, `grep`, `ls-files`, `rev-parse`, `cat-file`,
    and branch or tag listing.
  - **Write:** anything that changes a file, a ref or the index.
  - **Unknown:** a command the scanner can't analyse counts as a write.
- **Rules:** `{"action": "external_directory", "resource": "~/src/xyz/*",
  "effect": "allow", "access": "read"}`. Without `access`, a rule covers
  both, as today.
- **The terminal dialog** for an outside folder offers read for this session
  (the default), read and write, or no. "Always" saves the chosen level,
  keyed by the project's folder path.
- **References in your own config** (global or per-repo) can be read
  without asking, since you named them yourself. Writes there still ask.
  This gives any headless run "follow the pattern in XYZ" with no channel
  at all: name XYZ under `references` in that repo's per-repo file.

## Who builds what

| Side | Work |
|---|---|
| CodeSmith | The protocol and its docs and schema. The client in `codesmith run`: `hello`, `ask`, `progress`, adapting the `question` tool to the channel's limits, and enforcing every limit above, including the audit. The read/write split, rules with `access`, the dialog, and reference reads. A small reference server for tests. |
| tailbus | Serving the channel as above, for bridges with `relay_asks = true`. |
| Others | Whatever serves the channel, following "What an implementation must do". |

## Tests (CodeSmith)

- **The channel:**
  - a socket path and `fd:N` both work;
  - the token is sent;
  - an unknown version, `ok: false` or no channel refuses as today;
  - nothing in config or a repo can set `CODESMITH_ASK`.
- **`hello` limits:**
  - the `question` schema follows the reported header and option limits;
  - a question below `options_min` goes as free text;
  - `ask: false` refuses permissions.
- **A relayed permission:**
  - `once` lets the call run;
  - `reject`, `refused`, a timeout, a reply outside `offer`, a wrong id and a
    malformed line each refuse;
  - nothing is saved;
  - an identical call later asks again;
  - a read answer never allows a write, and a write is asked separately.
- **Relayed questions:**
  - answers reach the model exactly as typed answers do;
  - free text works;
  - dismissed and timed-out questions act as a dismiss;
  - multiple choice returns several answers.
- **Never relayed:** secret-file reads, keychain commands and password
  prompts stay refused, and denies stay denied.
- **Limits:** one open ask at a time, the per-run cap, and expiry when the
  channel never answers.
- **Progress:** notes go out without waiting.
- **Read and write:**
  - each command is classified;
  - a read rule allows reads and not writes;
  - references are readable without asking, and writing to them asks;
  - the dialog's three answers and "always" at each level.

## Decided

1. **The ask timeout:** reported by the channel (tailbus: 10 minutes by
   default), never beyond the run's remaining time.
2. **Access:** a relayed request asks for one access. A write is asked
   separately, because a human can say yes to a read more safely than to a
   write they haven't seen.
3. **Transport-neutral:** CodeSmith defines the channel, and tailbus is one
   implementation of it.
