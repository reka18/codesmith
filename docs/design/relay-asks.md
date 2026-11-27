# Design: headless runs that ask a human through the agent that sent them

Status: **plan for approval, reviewed by tailbus-dev on 2026-11-27.**
Nothing here is built yet, on either side.

## Why

A headless run (`codesmith run` started by a tailbus bridge) has no one at a
terminal. Today every permission prompt in it is refused, and the `question`
tool is unavailable. So a bridge can't do "follow the pattern in the XYZ
project" (reading a sibling folder), and it can't ask anything when a task
is unclear.

Ray's decisions (2026-11-27):
- a headless bridge asks its human-facing sibling agent, the one whose
  request started the run, which asks the human;
- that covers permission prompts and question sets (carousels);
- an agent may read sibling folders with the user's explicit permission;
  reading is kept separate from writing.

## The flow

1. A bridge run reaches a permission prompt or a `question` call.
2. If tailbus started the run with a run channel (below), CodeSmith sends
   the request through it. The bridge forwards it to the **requester**, the
   agent that sent the message that started the run.
   - The bridge refuses at once, without waiting, when the requester has no
     human: another bridge, a door, or an agent that isn't a live
     interactive session.
3. The requester shows it to its human and returns the human's answer.
4. The run waits, then carries on with the answer. With no answer within
   the ask timeout, a permission is refused and a question set is
   dismissed. Either way the run carries on as it would after the same
   answer in a terminal.
   - The timeout is `ask_timeout_secs`, 10 minutes by default, set per
     bridge and never beyond the run's remaining timeout.
   - The bridge's idle watchdog pauses while an ask is open; the run's own
     timeout keeps counting.
   - An answer that arrives after expiry is dropped, and the requester is
     told it expired.

### The run channel (tailbus)

- **Where:** the bridge creates `<RUN_DIR>/run.sock`. `RUN_DIR` is per run,
  mode 0700, owned by the bridge's user. The path is passed in
  `TAILBUS_RUN_SOCKET`, and as `{RUN_SOCKET}` in args.
- **Auth:** a per-run random token in `TAILBUS_RUN_TOKEN`, sent as the first
  line of each connection, plus a peer-uid check. The socket exists only
  while the run is alive.
- **No bus access for the run:** it never gets the bus socket and never acts
  as an agent. The bridge sends every bus message as itself, threaded under
  the request that started the run.
- **Protocol:** one JSON object per line.
  - `{"op": "ask", …}` (the request below) blocks on that connection until
    the answer line or expiry.
  - `{"op": "progress", "text": "…"}` returns at once and becomes a progress
    note under the request. This also fixes a bridge run's agent being
    unable to send progress notes.
- **The switch:** `relay_asks = true` in the bridge's tailbus table. The
  environment variables are present only then. CodeSmith relays only when it
  sees the channel, so it needs no setting of its own.
- **On the bus:**
  - the ask goes out as a new kind of request, threaded under the
    requester's original request;
  - its expiry is an absolute `expires_at`, worked out by the bridge from
    `expires_in`, because a message can sit queued;
  - the bus message's id is the ask's id there, and the bridge maps it to
    CodeSmith's `id`;
  - requesters answer with a new MCP tool, `ask_answer(reply_to, reply |
    answers | dismissed)`;
  - the bus accepts an answer only from the agent the ask was sent to.
- **Secrets:** the bridge removes its configured secret values from the
  relayed text (`command`, `resource`, questions), as it already does for
  logs and stderr.

### What a request carries

```json
{"v": 1, "kind": "permission", "run": "<run id>", "id": "<request id>",
 "agent": "build", "cwd": "/Users/x/src/app",
 "permission": {"action": "external_directory", "access": "read",
                "resource": "/Users/x/src/xyz/*", "command": "git -C ../xyz log -5",
                "offer": ["once", "reject"]},
 "expires_in": 600}
```

```json
{"v": 1, "kind": "questions", "run": "<run id>", "id": "<request id>",
 "questions": [{"question": "Which branch should the fix target?", "header": "Branch",
                "options": [{"label": "main (Recommended)", "description": "…"},
                            {"label": "release/2.x", "description": "…"}],
                "multiSelect": false}],
 "expires_in": 600}
```

- **Questions use tailbus's question shape,** the one `/pipe` uses: `question`,
  a `header` of at most 12 characters, 2 to 4 `options` with `label` and
  `description`, and `multiSelect`. The requester can show them in whatever
  carousel it has: a terminal question dialog, the phone app's question
  cards, or another agent's own question tool. Every question also takes a
  free-text answer, as in the terminal.
  - In a relaying run, CodeSmith's `question` tool advertises those limits in
    its schema, so the model writes questions that fit.
  - The tool's `multiple` is sent as `multiSelect`.
  - A question with 0 or 1 options goes as free text only.
- **Answers:**
  - `{"id", "reply": "once" | "reject"}` for a permission;
  - `{"id", "answers": [["main (Recommended)"], …]}` for questions, one list
    per question in order (free text is a one-item list);
  - `{"id", "dismissed": true}` to dismiss.
- **The command and resource** are exactly what the run would run or touch,
  so the human sees the real thing.

## Limits on a relayed answer

CodeSmith can't prove a human answered on the far side, so a relayed answer
grants less than one typed at the terminal:

- **This run only.** `once` is the only grant offered. A relayed answer never
  saves an "always" rule or a session rule.
- **Read only, unless the request is for a write.** Outside-folder
  permissions carry an `access` of `read` or `write` ([Reads and writes
  outside the project](#reads-and-writes-outside-the-project)).
  - A relayed request names exactly one access, and never offers "read and
    write" together.
  - A write is asked as its own request when it's needed, so the human sees
    the write they're approving.
- **Never relayed:**
  - the always-ask guards (secret files, keychain reads);
  - password prompts, which have their own design ([askpass.md](askpass.md));
  - anything the rules `deny`.

  These stay refused in a headless run.
- **Only the requester can answer.** tailbus authenticates the sender, and an
  answer from any other agent is ignored and audited.
- **Audited:** every relayed request and answer, with who answered and how
  long it took. Never file contents.
- **Off unless turned on.** Only a run started with the run channel (tailbus's
  `relay_asks = true`) relays. Without it, a bridge keeps refusing.
- **Bounded:** at most one open request per run at a time, and at most 20
  per run.

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
  This is how a bridge gets "follow the pattern in XYZ" without any relay:
  name XYZ under `references` in that repo's per-repo file.

## Who builds what

| Side | Work |
|---|---|
| CodeSmith | A relay asker for `codesmith run`, used when the run channel is present. It covers permission prompts and the `question` tool (which becomes available in a relaying run, with tailbus's question limits), the request and answer shapes above, the limits and the audit. It also sends progress notes through `progress`. Plus the read/write split, rules with `access`, the dialog, and reference reads. |
| tailbus | The run channel and the bus side, as in [The run channel](#the-run-channel-tailbus): the socket and token, `ask` and `progress`, `relay_asks`, the no-human refusal, `expires_at`, the ask message kind, `ask_answer`, requester-only answers, redaction, `ask_timeout_secs` and the idle-watchdog pause. Also the plugin's instruction: "A headless run you started is asking your user. Show it to them exactly, with your question UI (a permission as a two-option question, Allow once or Reject, with the command and resource verbatim). Send their answer with ask_answer. Never answer it yourself. If your user isn't there, leave it; it expires." |
| Requesting agents | Show permission prompts and question sets to their human, using their own question UI, and relay the answer unchanged. |

## Tests (CodeSmith)

- **A relayed permission:**
  - `once` lets the call run;
  - `reject`, a timeout and an answer from another agent each refuse;
  - nothing is saved;
  - a second identical call in the same run asks again;
  - a `read` answer doesn't allow a write.
- **Relayed questions:**
  - answers reach the model exactly as typed answers do;
  - free text works;
  - dismissal and timeout act as a dismiss;
  - a multiple-choice question returns several answers.
- **Never relayed:** secret-file reads, keychain commands and password
  prompts stay refused. Denies stay denied.
- **Off by default:** a run without the run channel refuses as today.
- **The channel:**
  - a wrong token or peer uid is refused;
  - an `ask` blocks until its answer or expiry;
  - `progress` returns at once;
  - a requester with no human is refused at once, and the run carries on as
    after a refusal;
  - an expired answer is dropped.
- **Question limits:** in a relaying run, the `question` schema caps headers
  at 12 characters and options at 2 to 4. `multiple` is sent as
  `multiSelect`. A question with 0 or 1 options goes as free text.
- **One access per relayed request:** a write is asked separately, and a read
  answer never allows a write.
- **Limits:** one open request at a time and the per-run cap.
- **Read and write:**
  - each command is classified;
  - a read rule allows reads and not writes;
  - references are readable without a prompt, and writing to them asks;
  - the dialog's three answers and "always" at each level.

## Decided with tailbus-dev

1. **The ask timeout:** a fixed default of 10 minutes (`ask_timeout_secs`),
   never beyond the run's remaining timeout.
2. **Access:** a relayed request offers only one access. A write is asked
   separately, because a human can say yes to a read more safely than to a
   write they haven't seen.
