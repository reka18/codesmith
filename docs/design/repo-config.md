# Design: a repo's config lives in CodeSmith's global folder

Status: **built.** Ray's decisions of 2026-11-26, with the follow-up that
there is no import. The user-facing description is in
[configuration.md](../configuration.md#per-repo-config).

## Why

Today CodeSmith reads a repository's own `codesmith.json[c]` and
`.codesmith/codesmith.json[c]`. Anything in them that reaches out waits for a
one-time trust prompt: provider URLs and keys, MCP servers, programs,
`{env:}` and `{file:}`. Everything else applies without asking. That
includes `permissions`, so a cloned repository can add `allow` rules for its
own commands. It also includes agents carrying their own permissions and
models.

A repository is someone else's input. Its settings shouldn't decide where
your keys go, what runs, or what the agent may do. So, per Ray: **a repo's
config is defined in CodeSmith's global folder, never inside the repo.**

## Decisions (Ray, 2026-11-26)

1. **All of it.** CodeSmith never reads a repository's `codesmith.json[c]`
   or `.codesmith/codesmith.json[c]`. Every per-repo setting lives in the
   global folder: models, agents, rules, permissions, providers, MCP,
   language servers, formatters, the shell, and the rest.
2. **Matched by folder path.** A per-repo file names the folder (or glob) it
   applies to. A clone in another folder doesn't pick it up by accident.
3. **Repo files are ignored, with no import.** A config file found in a
   repo is never applied, and CodeSmith says nothing about it. Nobody uses
   repo config yet, so there is no import command, no migration and no
   notice: an offer to import would only be another way for a repo to put
   its settings in front of you.
4. **Instructions still load from the repo.** `AGENTS.md`, `CLAUDE.md`, and
   the repo's skills and commands are the project's own docs, read as text
   the model sees, not settings. Permissions still guard what the agent
   does with them. Pi treats them the same way.

## Per-repo files

They live at `~/.config/codesmith/projects/*.json[c]` (under
`$CODESMITH_CONFIG_DIR` or `$XDG_CONFIG_HOME/codesmith`, as the global
config does):

```jsonc
// ~/.config/codesmith/projects/app.jsonc
{
  "path": "~/src/app",              // or a list; globs allowed: "~/src/app-*"
  "model": "anthropic/claude-sonnet-4-5",
  "permissions": [{ "action": "bash", "resource": "make *", "effect": "allow" }],
  "mcp": { "docs": { "url": "https://mcp.example/mcp" } }
}
```

- **Contents:** the same keys as the global config, plus `path`.
- **Matching:** `path` is matched against the project root, the folder
  where `.git` or `.hg` is found, as today.
  - **Several matches:** every matching file applies, the more specific
    path last, so it wins. Two files with equally specific paths apply in
    name order, and `codesmith debug config` warns about them.
  - **Paths only:** `~` is expanded, and paths are compared after resolving
    symlinks. A path that's a symlink into the repo is matched by where it
    leads.
  - **Never by remote:** a git remote never matches, because a repo can
    set its own.
- **Agent, command and theme files for one repo** go in
  `~/.config/codesmith/projects/<name>/agents/`, `modes/`, `commands/` and
  `themes/`.
  `<name>` is the file's own name without `.json[c]`, and the folder
  applies wherever its file matches.
- **Load order, later winning:**
  1. the global config;
  2. the matching per-repo files, least specific first;
  3. `$CODESMITH_CONFIG`;
  4. `$CODESMITH_CONFIG_CONTENT`.

  `permissions` and `instructions` still append, so a per-repo file's rules
  come after the global ones.
- **Specificity.** An exact path beats any glob; between globs, the longer
  literal start (the text before the first `*`, `?`, `[` or `{`) wins. A
  file listing several paths counts its most specific match. In a glob,
  `*` stays within one folder name and `**` crosses folders.
- **Global-only settings stay global-only.** A provider's `swap`,
  `claude_agent`, `shell.env.pass` and `update` are ignored in a per-repo
  file, with a warning in `codesmith debug config`.
- **Worktrees.** A worktree is its own folder and doesn't match its main
  checkout's per-repo file. Following a worktree's `.git` file back to the
  main repository would let a repo (whose `.git` can be a file naming any
  folder) claim your per-repo settings for another repository, so it isn't
  done; add the worktree folder or a glob to `path`.
- **No trust prompt.** These files are yours, in your own folder, so
  nothing in them is asked about. A per-repo file that others can write
  (group or world write, or another owner) is refused with an error, as
  `trust.json` is today.

## What still comes from the repo

| From the repo | Loads? |
|---|---|
| `codesmith.json[c]`, `.codesmith/codesmith.json[c]` | **no** |
| `.codesmith/agents/`, `.codesmith/modes/` | **no**: agents carry permissions and models, so they're config |
| `.codesmith/themes/` | **no** |
| `AGENTS.md`, `CLAUDE.md` and the other instructions files | yes, as now |
| skills (`.codesmith/skills/`, `.agents/skills/`, `.claude/skills/`) | yes, as now |
| commands (`.codesmith/commands/`, `.claude/commands/`) | yes, with the frontmatter rule below. |
| `.codesmith/review.json` (CodeSmith writes this for code review) | yes: it's CodeSmith's own state file, not settings. It holds review dimensions (names, focus text, checks, path globs) and nothing that changes permissions, providers or where requests go. |

**A repo command's frontmatter.** Its `model` is honoured: it can only name
a model your own providers serve. Its `agent` is honoured only when that
agent is in your config (built in, global or per-repo); otherwise the
command runs on the session's agent, with a one-line notice. Any
`` !`cmd` `` it expands is checked against your permission rules as the
`shell` tool's commands are, and nothing in the template runs unless every
command is allowed. (Before, a command's shell ran without a check.)

A skill's header is read for its `name` and `description` alone, so fields
such as `allowed-tools`, `model` or `mcp` do nothing.

## What goes away

- **The project trust prompt** and `codesmith trust` go away, because
  nothing the repo writes is ever settings. The capability audit that
  decided what to ask about goes with them.
  - `trust.json` is never read again. An existing file is left in place,
    unused.
  - `codesmith trust` is an unknown command.
- **`$CODESMITH_DISABLE_PROJECT_CONFIG`** now only turns off the repo's
  instructions, skills and commands, the parts still read. Per-repo files
  in the global folder are your config and keep loading.

## Upgrading

This changes what a repo with its own `codesmith.json` gets.
- **Before:** its model, agents and rules applied.
- **After:** they don't. Settings for it go in a per-repo file.

The release notes say so first.

## Tests

- **Never read:** a repo's `codesmith.json[c]`, `.codesmith/codesmith.json[c]`
  (at the root and in a subfolder), agents, modes and themes, each with an
  allow rule, a provider URL and key, an MCP server, `{env:}` and
  `{file:}`, a language server and a shell. None of it applies, no request
  reaches the URL, no program runs, nothing is read, and nothing is asked:
  in the UI, `codesmith run`, `serve`, ACP and automations.
- **Still loaded:** a repo's `AGENTS.md`, skills and commands. A command
  naming an agent you don't have falls back with a notice; its model is
  kept; its shell asks permission.
- **Matching:** an exact path, a glob, a list, `~`, a symlinked repo, the
  more specific path winning, equal paths with a warning, a clone in
  another folder not matching, a subfolder cwd matching its repo root, no
  match, and a git remote never matching.
- **Order:** global, then per-repo, then `$CODESMITH_CONFIG` and the
  content variable, with permissions appending; per-repo agent, command and
  theme folders.
- **Ownership:** a per-repo file or folder writable by others, or owned by
  another user, is refused with an error naming it.
- **Global-only settings** are refused from per-repo files.
- **`$CODESMITH_DISABLE_PROJECT_CONFIG`** turns off the repo's
  instructions, skills and commands, but not per-repo files.
- **`codesmith trust`** is an unknown command, and `trust.json` is never
  read.

## Open questions

None left from the plan: a repo command's `model` is honoured and its
`agent` only when it's yours, and there is no import command.
