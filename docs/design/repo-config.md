# Design: a repo's config lives in CodeSmith's global folder

Status: **plan; Ray's decisions of 2026-11-26 recorded, building not yet
approved.** Nothing here is built yet.

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
3. **Repo files are ignored, with an offer to import.** A config file found
   in a repo is never applied. CodeSmith says so once and offers
   `codesmith config import`.
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
  `~/.config/codesmith/projects/<name>/agents/`, `commands/` and `themes/`.
  `<name>` is the file's own name without `.json[c]`, and the folder
  applies wherever its file matches.
- **Load order, later winning:**
  1. the global config;
  2. the matching per-repo files, least specific first;
  3. `$CODESMITH_CONFIG`;
  4. `$CODESMITH_CONFIG_CONTENT`.

  `permissions` and `instructions` still append, so a per-repo file's rules
  come after the global ones.
- **No trust prompt.** These files are yours, in your own folder, so
  nothing in them is asked about. A per-repo file that others can write
  (group or world write, or another owner) is refused with an error, as
  `trust.json` is today.

## What still comes from the repo

| From the repo | Loads? |
|---|---|
| `codesmith.json[c]`, `.codesmith/codesmith.json[c]` | **no** (offered for import) |
| `.codesmith/agents/`, `.codesmith/modes/` | **no**: agents carry permissions and models, so they're config (offered for import) |
| `.codesmith/themes/` | **no** (offered for import) |
| `AGENTS.md`, `CLAUDE.md` and the other instructions files | yes, as now |
| skills (`.codesmith/skills/`, `.agents/skills/`, `.claude/skills/`) | yes, as now |
| commands (`.codesmith/commands/`, `.claude/commands/`) | yes, as now. A command's own `model` or `agent` choice is honoured only if that agent exists in your config, and any shell it expands still goes through permissions. |
| `.codesmith/review.json` (CodeSmith writes this for code review) | yes: it's CodeSmith's own state file, not settings. Check it holds nothing that changes permissions or where requests go. |

## The import

The first time a session starts in a repo that has one of the ignored
files, CodeSmith shows a one-line notice:

> This repo has CodeSmith config in `./codesmith.json` and
> `.codesmith/agents/`. It isn't used; `codesmith config import` can copy
> it into your own config after showing it.

`codesmith config import [DIR]`:
- shows each file's contents, and highlights what reaches out: URLs,
  `{env:}` and `{file:}`, programs and allow rules;
- asks once;
- on a yes, writes `~/.config/codesmith/projects/<repo folder name>.jsonc`
  with `path` set to that repo, and copies agent and theme files into its
  folder;
- never overwrites an existing per-repo file without a second yes, and
  never writes outside the global folder.

The notice is shown once per repo and file set, kept with the per-repo
answers in the data folder. `codesmith config import --dismiss` stops it
for that repo.

## What goes away

- **The project trust prompt** and `codesmith trust` go away, because
  nothing the repo writes is ever settings.
  - `trust.json` is read once more, at upgrade: for each project trusted
    there, the notice above is shown with the import offer.
  - Then the file is left in place, unused. `codesmith trust` prints that
    it's gone and points to `codesmith config import`.
- **`$CODESMITH_DISABLE_PROJECT_CONFIG`** now only turns off the repo's
  instructions, skills and commands, the parts still read. Per-repo files
  in the global folder are your config and keep loading.

## Upgrading

This changes what a repo with its own `codesmith.json` gets.
- **Before:** its model, agents and rules applied.
- **After:** they don't, until imported.

The release notes say so first, with the import command. Teams that check
in a shared config get the same notice and import it once per machine.

## Tests

- **Never read:** a repo's `codesmith.json`, `.codesmith/codesmith.json`,
  agents, modes and themes, each with an `allow` rule, a provider URL, an
  MCP server and `{env:}`. None of it applies, the key isn't sent there,
  and the notice names each file.
- **Still loaded:** a repo's `AGENTS.md`, skills and commands. A command
  naming an agent that exists only in the repo falls back to the
  session's agent.
- **Matching:**
  - an exact path, a glob, a list;
  - the more specific path winning;
  - equal paths in name order, with a warning;
  - `~` expansion, and a symlinked repo matched by its target;
  - a clone in another folder not matching;
  - a git remote never matching.
- **Order:** global, then per-repo, then `$CODESMITH_CONFIG` and the
  content variable, with permissions appending.
- **Ownership:** a per-repo file writable by others is refused, with an
  error naming it.
- **Import:**
  - shows everything;
  - highlights what reaches out;
  - a no writes nothing;
  - a yes writes the file with `path` and the agent files;
  - an existing file needs a second yes;
  - `--dismiss` stops the notice.
- **Upgrade:** a project trusted in `trust.json` gets the notice once.
  `codesmith trust` prints the pointer.

## Open questions

1. Should a repo's commands still be allowed to pick a model? As drafted,
   a command's `model` is honoured, and its `agent` only if that agent is
   in your config.
2. Is `codesmith config import` the right name, or should it live under
   `codesmith trust` for familiarity during the change?
