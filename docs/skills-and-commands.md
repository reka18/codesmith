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
3. every entry under `skills` in config, in order: a folder (relative to
   the project root) of skill folders or holding a `SKILL.md` itself, or a
   [catalog URL](#downloaded-skills)

Set `$CODESMITH_DISABLE_CLAUDE_CODE_SKILLS` (or `$CODESMITH_DISABLE_CLAUDE_CODE`)
to skip the `.claude/skills` folders. Loading a skill is checked with the
`skill` action and the skill's id.

The folders are read again before each model request. A skill added,
changed or removed mid-session is announced to the model in a short update
(only the skills that changed), and the list in the system prompt stays as
it was, so the cached prompt prefix does too; see [instructions that
change](configuration.md#when-instructions-change).

### Mentioning a skill

`@id` in a prompt (at its start or after a space) loads skill `id` with
that prompt, as OpenCode's skill mentions do: the skill's content goes to the
model in the prompt's message, before your text, once however often you
mention it. A word that is also a file or folder in the working directory is
a [file mention](interactive-ui.md#autocomplete-and--mentions) instead. A
skill the `skill` rules deny refuses the prompt: nothing is sent or saved.

### Downloaded skills

An `http://` or `https://` entry in `skills` names a catalog. CodeSmith
fetches `<url>/index.json`, which lists its skills:

```json
{ "skills": [{ "name": "deploy", "version": "3", "files": ["SKILL.md", "references/guide.md"] }] }
```

and downloads each skill's files from `<url>/<name>/<file>` into the cache
folder (`~/.cache/codesmith/skills/`), once per process. A skill whose name
is not a single path segment, or that lists a file outside its folder (an
absolute path, `..`, or another site), is skipped before anything is
fetched; a skill needs a `SKILL.md`. Without a `version`, only missing
files are downloaded. A new `version` downloads every file afresh and
replaces the cached copy only when all of them arrived, so a failed update
keeps the old one and files the new version no longer lists are removed. A
catalog that cannot be reached adds no skills, and the others still load.

### Built-in skills

CodeSmith ships two skills, as OpenCode does: `codesmith`, an index of this
guide for questions about CodeSmith itself, and `report`, which drafts a
GitHub issue with the run's diagnostics (version, OS, terminal, shell) and
files it with `gh` once you confirm. They are off by default, so they add
nothing to a session's prompt; turn them on with

```json
{ "builtin_skills": true }
```

A skill of your own with the same id replaces the built-in one.

With `builtin_skills` on, every agent also gets built-in `shell` rules that
ask before `gh issue create` and `gh issue comment` (also with options
before `issue`, such as `gh --repo o/r issue create`), so `report` cannot
file or comment on an issue without your answer whatever the model does.
Like a built-in agent's own rules they only restrict: an `allow` in your
config does not lift them, while a `deny` still applies. They do not cover
other ways of reaching the GitHub API (`gh api`, `curl`).

## Commands

Commands are prompt templates. Run them as `/name args` in the
[UI](interactive-ui.md#slash-commands), or as `codesmith run /name args`.
`codesmith commands` lists them all.

Put each one in a Markdown file in a `commands/` (or `command/`) folder
under the global config folder (`~/.config/codesmith/`) or any `.codesmith/`
folder from the project root down to the working directory. A nested file
is a nested name: `commands/git/sync.md` runs as `/git/sync`. Claude Code's
`~/.claude/commands/` and `.claude/commands/` (top-level `<name>.md` files)
are read too, below everything else.

```markdown
---
description: Explain a file in one sentence
agent: plan
model: ollama/qwen3-coder
---
Read $1 and explain what it does in exactly one sentence.
```

Or define them inline in config:

```jsonc
{
  "commands": {
    "test-fix": { "template": "Run the tests and fix the first failure. Focus on $ARGUMENTS.", "description": "fix a failing test" }
  }
}
```

Command folders and config files are layers in the same
[order](configuration.md#files-and-precedence): a `.codesmith/commands/`
folder comes right after that folder's `.codesmith/codesmith.json`, the
global folder after the global config. A later definition of a command,
inline or file, replaces an earlier one whole.

In a template, `$ARGUMENTS` is the whole argument string, and `$1`, `$2`,
… are single arguments (quotes group words; the highest one used takes the
rest). A template with neither gets the arguments appended. Then each
`` !`command` `` is replaced by what the command prints (output and errors),
run with your shell in the session's folder before the prompt is sent:

```markdown
---
description: Review what changed
---
Review these changes for $ARGUMENTS:

!`git diff --stat`
```

These commands are yours, so they run without asking; a template from a
project you don't trust can run anything, as a shell script in it could.

`agent` and `model` switch the session to that agent or model for the
command. A command runs in a **child session** instead when it sets
`subagent: true` (OpenCode's older `subtask: true` too) or names an agent
whose mode is `subagent`: the child uses the command's agent (else the
session's) and model (else that agent's, else the session's), titled with
the command's `description`, and its report joins the session's history
without switching the session's agent or model. `subagent: false` keeps a
subagent-mode agent's command in the session. Unlike OpenCode, the command
waits for the child to finish.

Built-in commands:

| Command | What it does |
|---|---|
| `/init` | Write or improve `AGENTS.md` for the project, guided. |
| `/automation [request]` | Schedule, list, pause, resume or remove [automations](automations.md): the request goes to the model with how to use the `automation` tool. |
| `/calibrate` | Choose the project's review dimensions. See [code review](code-review.md). |
| `/review [target]` | Review changes. See [code review](code-review.md). |

A custom command with the same name replaces a built-in one. MCP servers can
add commands too, named `/<server>:<prompt>` ([MCP prompts](mcp.md#prompts)),
and so can [plugins](plugins.md), whose commands run as code.
