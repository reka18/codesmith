# Skills and commands

## Skills

A skill is a folder with a `SKILL.md`: a header with a `name` and a
`description`, then instructions. The model sees the list of skills and loads
one with the `skill` tool when a task matches the description.

```markdown
---
name: release-notes
description: Write release notes from the git log. Use when asked for a changelog or release notes.
---
1. Run `git log --oneline <last-tag>..HEAD`.
2. Group the commits into Features, Fixes and Internal.
...
```

CodeSmith looks in these folders. Later ones win when two skills have the
same id:

1. `~/.config/codesmith/skills/`, `~/.agents/skills/`, `~/.claude/skills/`
2. in each folder from the project root down to the working directory:
   `.agents/skills/`, `.claude/skills/`, `.codesmith/skills/`
3. every folder listed under `skills` in config (relative to the project
   root)

Set `$CODESMITH_DISABLE_CLAUDE_CODE_SKILLS` (or `$CODESMITH_DISABLE_CLAUDE_CODE`)
to skip the `.claude/skills` folders. Loading a skill is checked with the
`skill` action and the skill's id.

## Commands

Commands are prompt templates. Run them as `/name args` in the
[UI](interactive-ui.md#slash-commands), or as `codesmith run /name args`.
`codesmith commands` lists them all.

Put each one in a Markdown file, `<name>.md`, in any of these folders (later
ones win):

1. `~/.config/codesmith/commands/`, `~/.claude/commands/`
2. in each folder from the project root down to the working directory:
   `.claude/commands/`, `.codesmith/commands/`

```markdown
---
description: Explain a file in one sentence
agent: plan
model: ollama/qwen3-coder
---
Read $1 and explain what it does in exactly one sentence.
```

Or define them inline in config, where they take precedence over files:

```jsonc
{
  "commands": {
    "test-fix": { "template": "Run the tests and fix the first failure. Focus on $ARGUMENTS.", "description": "fix a failing test" }
  }
}
```

In a template, `$ARGUMENTS` is the whole argument string, and `$1` to `$9`
are single arguments. `agent` and `model` switch the session to that agent
or model for the command.

Built-in commands:

| Command | What it does |
|---|---|
| `/init` | Write or improve `AGENTS.md` for the project, guided. |
| `/calibrate` | Choose the project's review dimensions. See [code review](code-review.md). |
| `/review [target]` | Review changes. See [code review](code-review.md). |

A custom command with the same name replaces a built-in one. MCP servers can
add commands too, named `/<server>:<prompt>` ([MCP prompts](mcp.md#prompts)).
