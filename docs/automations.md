# Automations

Automations run a prompt, or a Code Mode program, later or on a schedule,
with nobody at the keyboard, as in OpenCode. Ask the agent ("check the build
every 10 minutes", "remind me at 17:00 to ...") and it uses its `automation`
tool, or use the CLI:

```sh
codesmith automation add --name standup --cron "0 9 * * 1-5" --tz Europe/Paris \
  --prompt "Summarize yesterday's commits in three bullets"
codesmith automation add --name tidy --every "1 day" --agent explore --prompt "List stale TODOs"
codesmith automation add --name once --at "in 30 minutes" --session ses_... --prompt "Check the deploy"
codesmith automation add --name ping --every "1 hour" --session ses_... --in-session --prompt "Any news?"
codesmith automation add --name tickets --event tracker:ticket.created \
  --event-args '{"queue": "ops"}' --prompt "Triage the new ticket"
codesmith automation list [--all] | pause ID | resume ID | remove ID | log ID
```

## Triggers

Each automation has exactly one trigger:

| Flag | Runs |
|---|---|
| `--at T` | Once. `"in 30 minutes"`, a local wall-clock time such as `"2026-10-27T09:00"`, or an RFC 3339 time. |
| `--every I` | On an interval: `"90 seconds"`, `"10 minutes"`, `"2 hours"`, `"1 day"`; from a minute to 10 years. |
| `--cron C` | On a five-field cron schedule, such as `"0 9 * * 1-5"`. |
| `--event SERVER:EVENT` | Once for each event an MCP server pushes. See [MCP events](#mcp-events). |

`--tz` sets an IANA time zone for `--cron` and `--at` (default: local time).

## Actions

- **A prompt** (`--prompt`) runs, on each run, in one of:
  - **a new child session of the session that created it** (the default).
    The child inherits that session's agent, model and directory, lists
    under it in the session list, and is titled
    `<automation name> — <time>`. The creating session itself is left
    alone. From the CLI, `--session ID` names the creating session
    (`--agent` picks another agent, on that agent's default model).
  - **the creating session itself**, continuing its conversation: ask the
    agent for that (the tool's `target: "session"`), or pass
    `--session ID --in-session`. See
    [Prompts sent to an existing session](#prompts-sent-to-an-existing-session).
  - **a new standalone session** with `--agent` (default `build`), when the
    CLI is given no `--session`. The agent can also pick a new session with
    another agent (the tool's `agent`), which is a child of its session too.
- **A Code Mode program** (`--code-file FILE`, with optional `--args JSON`)
  runs over the MCP tools, like [Code Mode](mcp.md#code-mode). It gets
  `input = {state, now, runs, args}` and may return
  `{state, done, send: {prompt, to: {subagent}}}`, to keep state between
  runs, finish itself, or hand work to a new session with that agent.

Each run's outcome, session and answer are kept for `automation log ID`.

Automations created before child sessions became the default keep the
target they were stored with: one created to continue a session still
continues it. Nothing is migrated; to switch one over, remove it and create
it again. A child target records the agent and model when the automation
is created, so later changes to the creating session don't change its
runs. If the creating session is deleted, runs go on in new sessions
without a parent.

### Prompts sent to an existing session

A session runs one turn at a time, across every CodeSmith process that
shares the session database: a turn holds a lease on its session while it
runs. When a timed run's session is busy (you are in the middle of a turn in
the UI, say), the run is **deferred**: its last outcome reads `deferred` and
it is tried again on the next tick, about 15 seconds later. An event's run,
or a Code Mode program's `send`, waits for the turn to finish instead, since
it can't be repeated. Your own turn likewise waits while an automation's
turn runs in the session.

A session you have open picks up what an automation added the next time you
send it a message, so the model sees the automation's prompt and answer.
The UI's transcript shows them once you reopen the session. Undo in that
session undoes the newest turn, which may be the automation's.

## Who runs them

Automations are stored with your sessions. Whichever CodeSmith is open runs
them: the UI, `serve` or `acp`. Set `"automations": false` to keep a process
from running them. A dedicated runner is also available:

```sh
codesmith automation serve      # run in the foreground; keep it alive with launchd or systemd
codesmith automation run-due    # fire whatever is due once and exit (for cron)
```

A lease in the database keeps two processes from firing the same run.

## Safety

An automation is a way for a prompt-injected model to come back later, so
automations are kept on a short leash.

### What the agent may schedule

- **Creating an automation asks you first**, and so does watching an MCP
  server's events, even though agents allow everything else by default. The
  request shows what you are approving: the name, the trigger, where the
  prompt goes (a new child session of this one on each run, with its agent
  and model, which is the default; this session itself; or a new session
  with which agent), and the prompt itself, with control and invisible characters spelled out and
  anything past 4000 characters cut with a "(N more chars)" note. An event
  watch's request names the server, the event and its arguments.
- **Pausing, resuming and removing ask too**, showing the automation's
  trigger and prompt, so an injected model can't quietly restart a job you
  paused or delete one you rely on.
- The blanket `allow` doesn't count for any of these. Only a rule that names
  the action itself skips the question:
  `{"action": "automation", "resource": "*", "effect": "allow"}` for every
  operation, a resource of `create`, `pause`, `resume` or `remove` for one,
  or `{"action": "watch_mcp_event", "resource": "tracker", "effect": "allow"}`
  for one server's events.
- **A job the agent creates runs only as that session's own agent, or as a
  read-only subagent** (`explore`, `review`). The `plan` agent can't
  schedule work for `build`. Use `codesmith automation add --agent ...` to
  pick any agent yourself.

### Permissions for unattended runs

Runs are unattended. They follow your permission rules, and anything that
would ask is refused. On top of that, **`shell`, `edit` (and `write`),
`webfetch`, `external_directory` and MCP tools are denied**, because a run
may act on untrusted input such as an MCP event, and MCP tools (chat,
issues, mail) are the easiest way to send data out. The MCP denies cover
every tool of every configured server (`<server>_*`), the
`list_mcp_resources` and `read_mcp_resource` tools, and Code Mode: denied
tools aren't offered to a Code Mode program, so a run's model gets no
`execute` tool and a Code Mode automation sees only the tools you opted in.

Rules under `automation.permissions` apply only to unattended runs, and come
after those denies, so they opt back in:

```jsonc
{
  "automation": {
    "permissions": [
      { "action": "shell", "resource": "cargo test*", "effect": "allow" },
      { "action": "edit", "resource": "CHANGELOG.md", "effect": "allow" },
      // One MCP tool, directly or from Code Mode; "github_*" for the server.
      { "action": "github_create_issue", "resource": "*", "effect": "allow" }
    ]
  }
}
```

`{"action": "shell", "resource": "*", "effect": "allow"}` gives runs the
whole shell back.

Unattended runs can't manage automations: the `automation` tool and event
watches are denied in them whatever the rules, `automation.permissions`
included, so a run can't schedule more work, resume a paused job or remove
one. See [permissions](permissions.md#unattended-runs).

## MCP events

`--event server:event` runs once for each event an MCP server pushes, for
servers that offer MCP Events (the draft `io.modelcontextprotocol/events`
extension, as in OpenCode):

- The server must be connected and advertise the event for push delivery,
  and `--event-args` must match the event's input schema.
- Each run's prompt carries the event (`{id, timestamp, data}`) quoted as
  untrusted data. A program gets it as `input.event`.
- The watch saves the server's cursor after every message (for an event,
  when its run starts), so pausing and resuming, a restart, or another
  process taking over picks up where it left off without running an event
  twice. A stream that breaks is reopened up to three times.
- A watch that cannot go on is paused, with the reason shown in
  `automation list`. That happens for a refused or ended subscription, a
  server that can't replay from the saved cursor, an event that doesn't match
  its schema, a run that can't start (its config, session or agent is
  gone), a failed Code Mode program, or a `watch_mcp_event` rule that
  denies the server. A prompt whose turn fails (the model returned an
  error, say) is logged as `failed` and the watch goes on, as in OpenCode.
