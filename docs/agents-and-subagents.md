# Agents and subagents

An agent is a system prompt, an optional model, a step limit and a set of
[permission rules](permissions.md). **Primary** agents are the ones you talk
to. **Subagents** are the ones the model delegates to, through the
`subagent` tool.

## Built-in agents

| Agent | Mode | What it is |
|---|---|---|
| `build` | primary | The default. Uses every tool your rules allow. |
| `plan` | primary | Read-only planning: investigates and proposes changes without editing the project. It may write plan files only in the Plan directory (`~/.local/share/codesmith/plan`); `edit`, `write` and `patch` are denied everywhere else, and so are shell redirects that write files. As in OpenCode, it reaches folders outside the project without an `external_directory` prompt, to read code it plans around; a configured `external_directory` deny or ask still applies. See [the plan plugin](plugins.md#the-plan-plugin). |
| `general` | subagent | General-purpose work in parallel. Can't ask questions or start subagents. |
| `explore` | subagent | Fast, read-only codebase search: `read`, `grep`, `glob`, web tools and read-only shell commands. |
| `review` | subagent | The read-only reviewer used by [`/review` and `/calibrate`](code-review.md). |

The exact rules for each are in [permissions](permissions.md#defaults).

Switch primary agents with **Tab**/**Shift+Tab** or `/agent` in the
[UI](interactive-ui.md), or with `--agent` on the command line. Set
`default_agent` to start with a different one.

## Configuring agents

`agents` in config overrides a built-in agent, field by field, or adds a new
one:

```jsonc
{
  "agents": {
    "build": { "model": "anthropic/claude-sonnet-4-5", "steps": 50 },
    "plan": { "disabled": true },
    "reviewer": {
      "description": "Reviews code without editing it",
      "mode": "primary",
      "system": "You review code. Never edit files.",
      "permissions": [{ "action": "edit", "resource": "*", "effect": "deny" }]
    },
    "docs-writer": {
      "description": "Writes and updates documentation. Use for README and docs/ changes.",
      "mode": "subagent",
      "model": "ollama/qwen3-coder",
      "permissions": [
        { "action": "edit", "resource": "*", "effect": "deny" },
        { "action": "edit", "resource": "docs/*", "effect": "allow" }
      ]
    }
  }
}
```

| Field | Meaning |
|---|---|
| `description` | What the agent is for. For a subagent, this is what the model reads when deciding whether to delegate to it. |
| `mode` | `primary` (the default for new agents), `subagent`, or `all` (both). |
| `system` | The system prompt, replacing the default one. |
| `model` | Model as `provider/model` (or `provider/model#variant`), or `{"providerID": "...", "model": "...", "variant": "..."}`; otherwise the session's model. |
| `steps` | The most model calls per turn (default 200). |
| `hidden` | `true` leaves a subagent out of the list the `subagent` tool shows, and a primary agent out of agent cycling; it is never the default agent, but can still be chosen by name. |
| `color` | A `#rrggbb` color. Anything else (such as an OpenCode theme name) is ignored. |
| `request` | Extra `headers` and `body` fields for the agent's model requests, over the model's own: `{"body": {"temperature": 0.2, "top_p": 0.9}}`. |
| `disabled` | `true` removes the agent. |
| `permissions` | Rules added after the agent's built-in ones. |

A new agent starts from the [default rules](permissions.md#defaults). Then
come the top-level `permissions`, the built-in agent's own rules (which only
restrict) and the agent's own `permissions`, which win over everything
before them. See [where rules come from](permissions.md#where-rules-come-from).

When more than one config file defines the same agent, later files win
field by field: `permissions` add up, and `request` maps merge key by key.

`default_agent` must name a primary agent that is not hidden; otherwise
CodeSmith starts with `build`, or the first such agent if `build` is
disabled.

## Agent files

An agent can also be a Markdown file: the body is its system prompt and
the header holds the same fields as config. CodeSmith reads them from the
global config folder (`~/.config/codesmith/`) and from each `.codesmith/`
folder from the project root down to the working directory:

- `agents/**/*.md` (or `agent/`): the agent's id is its path below the
  folder without `.md`, so `agents/team/helper.md` is `team/helper`.
- `modes/*.md` (or `mode/`): always primary agents.

```markdown
---
description: Reviews changes for correctness. Use after an edit.
mode: subagent
model: anthropic/claude-sonnet-4-5
request:
  body:
    temperature: 0.1
permissions:
  - action: edit
    resource: "*"
    effect: deny
---
You review code. Report problems with file and line; never edit.
```

Each folder is a config layer: it sits right after the config file next
to it (the global `codesmith.json`, or that folder's
`.codesmith/codesmith.json`), so a later layer still overrides it field by
field. Empty files, and files whose header does not parse or has a field of
the wrong type, are skipped.

OpenCode's older agent format is read too: a file with fields such as
`temperature`, `top_p`, `tools`, `permission`, `prompt` or `maxSteps`, and
none of `permissions`, `request`, `system` or `disabled`, is converted:

| Old field | Becomes |
|---|---|
| `temperature`, `top_p`, `options`, unknown fields | `request.body` fields |
| `tools: {write: false}` | rules: `write` and `patch` are `edit`; `true` allows, `false` denies |
| `permission: {"*": allow, bash: ask}` | rules in the order written: `bash` is `shell`, `task` is `subagent`, `write`/`patch`/`apply_patch` are `edit`; a map value sets per-resource rules |
| `maxSteps` | `steps` (when `steps` is not set) |
| `disable` | `disabled` |
| `model` and `variant` | `model: provider/model#variant` (a model with its own `#variant` keeps it) |
| a color name | `#aaaaaa` |

The body is the prompt in both formats; a header `prompt` is ignored.

## Model prompts

An agent without a `system` prompt gets one chosen by the model's id (as in
config or the catalog): GPT models get OpenCode's GPT prompt (`astra`
models their own), Kimi, Arcee Trinity and Meta Muse models get theirs
(Muse's names the model by its configured or catalog `name`), Claude models
get the default prompt with a short appendix, and the rest get the default.
`codesmith debug prompt -m provider/model` prints it.

## Subagents

When an agent has subagents it may call, the model gets a `subagent` tool. It
lists every agent with mode `subagent` or `all` that is not `hidden`, with
its description (a hidden one can still be named). Each
call runs a task in a child session with that agent and returns its final
answer. The child session is stored like any other, and
`codesmith session delete` removes it along with its parent.

Calls are checked with the `subagent` action and the subagent's id, so
`{"action": "subagent", "resource": "general", "effect": "deny"}` keeps an
agent from delegating to `general`.

By default **subagents cannot start subagents of their own**.
`subagent_depth` (default `1`) sets how many levels may nest:

```jsonc
{ "subagent_depth": 2 }   // the main session's subagents may start one more level
```

The built-in `general` and `explore` subagents also deny `subagent` in their
own rules, so raising the depth matters only for your own subagents, or for
ones whose rules you change.

Permission requests from a subagent are asked of you like any other, and the
subagent's own rules apply.
