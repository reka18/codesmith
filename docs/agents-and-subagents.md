# Agents and subagents

An agent is a system prompt, an optional model, a step limit and a set of
[permission rules](permissions.md). **Primary** agents are the ones you talk
to. **Subagents** are the ones the model delegates to, through the
`subagent` tool.

## Built-in agents

| Agent | Mode | What it is |
|---|---|---|
| `build` | primary | The default. Uses every tool your rules allow. |
| `plan` | primary | Read-only planning: investigates and proposes changes without editing. `edit` and `write` are denied, and so are shell redirects that write files. |
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
| `model` | Model as `provider/model`, otherwise the session's model. |
| `steps` | The most model calls per turn (default 200). |
| `disabled` | `true` removes the agent. |
| `permissions` | Rules added after the agent's built-in ones. |

A new agent starts from the [default rules](permissions.md#defaults), and
its own rules come after. Top-level `permissions` come after every agent's
rules, so they win over them. See
[where rules come from](permissions.md#where-rules-come-from).

## Subagents

When an agent has subagents it may call, the model gets a `subagent` tool. It
lists every agent with mode `subagent` or `all`, with its description. Each
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
