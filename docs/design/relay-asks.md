# Design: headless runs that ask a human through the agent that sent them

Status: **plan for approval.** Nothing here is built yet. The CodeSmith side
is below. The tailbus side is to be agreed with tailbus-dev.

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
2. If tailbus started the run with a way to ask back (below), CodeSmith sends
   the request to the **requester**, the agent that sent the message that
   started the run.
3. The requester shows it to its human and returns the human's answer.
4. The run waits, then carries on with the answer. With no answer within
   the bridge's ask timeout, a permission is refused and a question set is
   dismissed. Either way the run carries on as it would after the same
   answer in a terminal.

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
                "multiple": false}],
 "expires_in": 600}
```

- **Questions use the `question` tool's own shape,** so the requester can
  show them in whatever carousel it has: a terminal question dialog, the
  phone app's question cards, or another agent's own question tool. Every
  question also takes a free-text answer, as in the terminal.
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
- **Read unless write was asked.** Outside-folder permissions carry an
  `access` of `read` or `write` ([Reads and writes outside the
  project](#reads-and-writes-outside-the-project)). An answer grants only
  the access the request named.
- **Never relayed:**
  - the always-ask guards (secret files, keychain reads);
  - password prompts, which have their own design ([askpass.md](askpass.md));
  - anything the rules `deny`.

  These stay refused in a headless run.
- **Only the requester can answer.** tailbus authenticates the sender, and an
  answer from any other agent is ignored and audited.
- **Audited:** every relayed request and answer, with who answered and how
  long it took. Never file contents.
- **Off unless turned on.** The bridge's own config sets `relay_asks: true`.
  Without it, a bridge keeps refusing.
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
| CodeSmith | A relay asker for `codesmith run`, used when tailbus passes an ask channel. It covers permission prompts and the `question` tool (which becomes available in a relaying run), the request and answer shapes above, the limits, the audit, and the bridge setting. Also the read/write split, rules with `access`, the dialog, and reference reads. |
| tailbus | An ask channel for a bridge run: for example a Unix socket path in an environment variable, authenticated to that run. It carries a request to the requester as a tailbus message, and the answer back. It also needs a way for agents to see and answer these requests (like pipe questions), and the instruction to human-facing agents: show the request to the human, pass back their answer, never answer on their own. |
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
- **Off by default:** a run without `relay_asks` refuses as today.
- **Limits:** one open request at a time and the per-run cap.
- **Read and write:**
  - each command is classified;
  - a read rule allows reads and not writes;
  - references are readable without a prompt, and writing to them asks;
  - the dialog's three answers and "always" at each level.

## Open questions

1. Should the ask timeout default to the bridge's own run timeout, or to a
   shorter fixed time, such as 10 minutes?
2. Should a relayed request be allowed to offer "read and write" for a
   folder, or only "read" until a separate write request is made?
