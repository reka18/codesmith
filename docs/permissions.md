# Permissions

Every tool call is checked against an ordered list of rules before it runs.
Each check ends in one of three ways: the call runs, it is refused, or you are
asked.

- [Rules](#rules)
- [Actions and resources](#actions-and-resources)
- [Defaults](#defaults)
- [Shell commands](#shell-commands)
- [Paths outside the project](#paths-outside-the-project)
- [Answering a request](#answering-a-request)
- [Where rules come from](#where-rules-come-from)
- [Unattended runs](#unattended-runs)

## Rules

A rule is `{action, resource, effect}`, and `effect` is `allow`, `ask` or
`deny`:

```jsonc
{
  "permissions": [
    { "action": "shell", "resource": "rm *", "effect": "ask" },
    { "action": "shell", "resource": "git push *", "effect": "deny" },
    { "action": "edit", "resource": "migrations/*", "effect": "deny" },
    { "action": "webfetch", "resource": "*", "effect": "ask" }
  ]
}
```

**The last matching rule wins.** For each request, CodeSmith looks for the
last rule whose `action` and `resource` both match. If no rule matches, you
are asked. When a request covers several resources (a shell line with several
commands, say), any `deny` refuses the whole request, and otherwise any `ask`
asks.

Patterns:

- `*` matches any run of characters, including `/`. `?` matches exactly one
  character.
- A pattern that ends in ` *` also matches the bare command, so `git *`
  matches `git` as well as `git status`.
- On macOS, path resources (`read`, `edit`, `external_directory`) match
  without regard to case, because the default volume ignores case.

`codesmith debug agents` lists every agent with the rules it ends up with.

## Actions and resources

| Action | Resource | Notes |
|---|---|---|
| `read` | the file path | Relative to the project root inside it, absolute outside it. |
| `edit` | the file path | Covers both `write` and `edit`, and files that shell redirects write to. |
| `glob`, `grep` | the pattern | `grep` with a `path` also checks `read` on it. |
| `shell` | each command | See [Shell commands](#shell-commands). |
| `webfetch` | the URL | Redirect targets are checked against `deny` rules too. |
| `websearch` | the query | |
| `subagent` | the subagent id | |
| `skill` | the skill id | |
| `lsp` | the file path | |
| `question`, `todowrite` | `*` | |
| `external_directory` | the path outside the project | See [Paths outside the project](#paths-outside-the-project). |
| `<server>_<tool>` | `*` | An MCP tool, by the name the model sees. See [MCP](mcp.md#permissions). |
| `list_mcp_resources`, `read_mcp_resource` | server, or `server:uri` | See [MCP resources](mcp.md#resources). |
| `automation`, `watch_mcp_event` | operation (`create`, `pause`, ...), or server | See [automations](automations.md#what-the-agent-may-schedule). |
| plugin tools | `*` | By tool name, like any other tool. |

A path rule is checked against every name the path goes by: the path as
written, and its real path with symlinks resolved and the on-disk spelling.
A rule written in either form matches.

A tool is hidden from the model only when its action is denied for every
resource. `{"action": "webfetch", "resource": "*", "effect": "deny"}` removes
`webfetch` entirely. A rule like `{"action": "edit", "resource": "docs/*",
"effect": "allow"}` that takes effect (under `agents.plan.permissions`, say)
keeps `edit` visible even for an agent that denies it elsewhere.

## Defaults

Every agent starts from these rules, in this order:

```jsonc
[
  { "action": "*", "resource": "*", "effect": "allow" },
  { "action": "external_directory", "resource": "*", "effect": "ask" },
  // CodeSmith's own scratch and tool-output folders stay allowed
  { "action": "external_directory", "resource": "<data>/tmp/*", "effect": "allow" },
  { "action": "external_directory", "resource": "<data>/tool-output/*", "effect": "allow" },
  { "action": "read", "resource": "*.env", "effect": "ask" },
  { "action": "read", "resource": "*.env.*", "effect": "ask" },
  { "action": "read", "resource": "*.env.example", "effect": "allow" },
  // reference checkouts are read-only to tools
  { "action": "edit", "resource": "<data>/repos/*", "effect": "deny" },
  { "action": "edit", "resource": "<data>/repos-state/*", "effect": "deny" }
]
```

So by default everything inside the project is allowed, except reading
`.env` files, which asks. Anything outside the project asks. Tools can never
write into the cache of [reference](configuration.md#references) checkouts, where
CodeSmith runs git itself (without hooks, fsmonitor or the `ext` and `file`
transports). Each built-in
agent then adds its own rules on top:

| Agent | Adds |
|---|---|
| `build` | `question` allowed. |
| `plan` | `question` allowed, `edit` denied (no `write`, `edit`, or redirect writes). |
| `general` | `question` and `subagent` denied. |
| `explore` | Everything denied, then `read`, `grep`, `glob`, `webfetch`, `websearch` allowed (`.env` asks), files outside the project allowed, and a list of read-only shell commands (`git diff/show/log/status/...`, `ls`, `cat`, `grep`, `rg`, `find`, ...) allowed, with options that run programs or write files (`find -exec`, `git --output`, `rg --pre`, `sort -o`, and others) denied. |
| `review` | Everything denied, then `read` (`.env` denied), `grep`, `glob` and read-only git commands allowed, and files outside the project denied. |

Shell commands are allowed by default for `build`. If you want to be asked,
add a rule such as `{"action": "shell", "resource": "*", "effect": "ask"}`
and then allow the commands you trust after it.

## Shell commands

A shell line is split into the commands it would run, and **each command is
checked on its own**. `git status && rm -rf build` is checked as both
`git status` and `rm -rf build`. The same goes for commands inside `$(...)`,
backticks, `<(...)`, pipelines, subshells, `{ ...; }` groups, loops,
conditionals, `case` arms, function bodies and unquoted heredocs.

Beyond the commands as written, CodeSmith also checks:

- **Other spellings.** A command is checked with its quotes and leading
  variable assignments removed as well, so `r''m x` and `FOO=1 rm x` are
  checked as `rm x`.
- **Wrappers.** The command that a wrapper runs is checked too:
  `env touch x`, `sudo rm x`, `xargs rm`, `nice`, `timeout`/`gtimeout`,
  `stdbuf`, `ionice`, `chrt`, `taskset`, `setsid`, `flock`, `chroot`,
  `script`, `su -c`, `doas`, `watch`, `parallel`, `caffeinate`, `arch`,
  `find -exec`, and the commands inside a static `sh -c '...'`, `eval '...'`
  or `trap` string. A rule that denies `rm *` also denies those forms. A
  wrapper that starts a shell reading its input (`sudo -s`, `su user`,
  `chroot dir`, `at`) can't be checked command by command.
- **Commands other programs run.** Shell commands given to another program
  are checked the same way: `git -c core.pager='...'`, `git -c
  alias.x='!...'`, `git rebase -x '...'`, `GIT_SSH_COMMAND='...' git fetch`
  (and `PAGER`, `EDITOR`, `GIT_EDITOR` and similar variables), `ssh -o
  ProxyCommand='...'`, `rsync -e '...'`, awk's `system("...")` and pipes,
  and sed's `e` command. `GIT_PAGER=cat` and `GIT_EDITOR=true` ask nothing.
  The command after `ssh host` is checked too, though it runs on the other
  machine.
- **Inline code.** Code in another language on the command line (`python -c
  '...'`, `perl -e`, `ruby -e`, `node -e`, `osascript -e`, a script piped
  or heredoc'd into `python3`, an awk program that runs `system($0)`, sed's
  `s///e`, or an awk/sed program built from shell variables) can't be read
  by the scanner. It's checked as an extra resource, `code:<program> <code>`,
  that a rule like `python *` doesn't match. With the defaults this allows
  it. With an allow-list, you're asked once, and "always" saves
  `code:python3 *`. To refuse inline code, deny `code:*` (or
  `code:python3 *`). Running a script file (`python3 x.py`, `perl x.pl`)
  or a module (`python3 -m pytest`) adds no `code:` resource.
- **Redirects.** A file that a redirect writes to (`>`, `>>`, `&>`, `>|`,
  `<>`), or that a program writes by name (`awk '{print > "f"}'`, `sed 'w
  f'`, `sed -i` and its backup, `find -fprint f`, `flock`'s lock file,
  `script`'s typescript), is
  checked like a `write`: `external_directory` if it is outside the
  project, then `edit` on the path. Under the `plan` agent,
  `echo x > notes.md` is refused because `edit` is denied.
- **Directories.** `cd` and `pushd` targets, and the tool's `workdir`,
  outside the project need `external_directory` approval.

Some commands cannot be analyzed: malformed syntax, a construct the scanner
does not know, a command name computed at run time, `eval` of a variable, or
arithmetic on text. These run only as far as your rules allow commands in
general. If any `shell` rule could deny some command, they are refused. If
one could ask, you are asked. Likewise, a redirect to a path that is only
known at run time runs only as far as the `edit` and `external_directory`
rules allow writes in general.

## Paths outside the project

The project is the nearest folder with `.git` (or `.hg`) above the working directory, or
the working directory itself. Reading, writing or editing a file outside it,
searching there, or `cd`-ing there needs `external_directory` approval first,
and then the tool's own permission. Symlinks are resolved first, so a link
inside the project that points outside it counts as outside.

`external_directory` asks by default. To allow a folder permanently:

```jsonc
{ "permissions": [{ "action": "external_directory", "resource": "/Users/me/src/shared-lib/*", "effect": "allow" }] }
```

## Answering a request

In the [interactive UI](interactive-ui.md#permission-and-question-dialogs), a
permission request opens a dialog. Edits and writes preview their diff.

| Key | Answer |
|---|---|
| **y** | Allow once. |
| **a** | Always allow: allow, and save a rule for the project ([saved approvals](#saved-approvals)). |
| **n**, **Esc** or **Enter** | Reject. |
| type text, then **Enter** | Reject and send what you typed to the model as feedback. |

Enter rejects, so a stray keypress never approves anything. Allowing always
takes a deliberate key. The request is shown with control characters escaped
(`\x1b`, `\u{202e}`) and long runs of spaces or blank lines counted (`␣×700`),
so hidden text can't disguise what you approve. A request too long to fit
says how much is hidden and scrolls with **↑/↓** and **PgUp/PgDn**.

`codesmith run` asks on the terminal with `[y/a/v/N]`. A bare Enter rejects,
and `v` prints a request in full if it was shortened (requests over 2,000
characters are). `--auto` approves every request that no rule denies, and
`serve --auto` does the same for the local server.

"Always" saves rules for the project, so later sessions of it are not
asked again either (see [saved approvals](#saved-approvals)):

- For a shell command, it saves the command's leading words plus `*`:
  `git status *`, `npm run build *`, `cargo test *`. CodeSmith knows how many
  words make up a subcommand for common tools (`git`, `npm run`,
  `docker compose`, `cargo`, `kubectl`, and others).
- For `external_directory`, it saves the enclosing repository (`<repo>/*`,
  the nearest folder above the path with `.git` or `.hg`), as OpenCode does,
  else the folder itself (`<dir>/*`). If that is `/`, your home folder or
  above it, only that file is saved: approving `~/.zshrc` doesn't approve
  all of `$HOME`, even when your home folder is a repository.
- A saved approval never overrides a configured `deny`, a built-in agent's
  own rules or a [policy](#policies).

### Saved approvals

"Always" answers are kept in the session database (OpenCode keeps them per
project too). A Git or Mercurial project's approvals are shared by its
clones and worktrees (the project is named by its first commit); a folder
outside version control has its own. A session without the database (an
in-memory one) keeps them for itself only.

`codesmith permissions` lists them with their ids, `codesmith permissions
edit ID --resource PATTERN` narrows or changes one, and `codesmith
permissions remove ID` drops it; the local server has the same under
[`/permissions/saved`](local-server.md#routes). Running sessions read the
saved approvals at every request, so a change applies at once. To make an
approval part of the project's config instead, add the rule to
`codesmith.json`.

## Where rules come from

An agent's rules are assembled in this order, and the last match wins:

1. the [defaults](#defaults)
2. top-level `permissions` from config, from every config file in
   [load order](configuration.md#files-and-precedence): global first, then
   project files
3. the built-in agent's own rules (see the [table above](#defaults))
4. `agents.<id>.permissions` from config
5. for [unattended runs](#unattended-runs) only, the unattended denies and
   then `automation.permissions`
6. the [session's own rules](#session-rules), which only restrict
7. approvals saved with "always" for the project

Consequences:

- **A built-in agent's own rules only restrict.** CodeSmith evaluates the
  list with and without them and keeps the stricter answer. A top-level
  `{"action": "edit", "resource": "*", "effect": "allow"}` doesn't let `plan`,
  `review` or `explore` edit, and a top-level allow doesn't add commands to
  the read-only agents' shell lists. A top-level `deny` or `ask` still
  reaches them: `{"action": "read", "resource": "secrets/*", "effect":
  "deny"}` applies to `explore` and `review` as well as `build`.
- **Rules for an agent override everything above them.** To let `plan` edit
  docs, add `{"action": "edit", "resource": "docs/*", "effect": "allow"}`
  under `agents.plan.permissions`, not at the top level.
- A project's `codesmith.json` comes after your global config, so **its rules
  override yours**. This is the main reason to read a repository's config
  before you run CodeSmith in it (see the warning in
  [configuration](configuration.md#files-and-precedence)), or to set
  `CODESMITH_DISABLE_PROJECT_CONFIG=1`.

### Session rules

A session can carry rules of its own, set by a client of the
[local server](local-server.md) (`PATCH /sessions/{id} {"permissions":
[...]}`), in the same `{action, resource, effect}` form. A subagent or
other child session starts with its parent's, and a fork copies them.
Like a built-in agent's rules they **only restrict**: a session `deny` or
`ask` applies, but a session `allow` never loosens a configured, built-in
or policy rule. (OpenCode lets a session's rules override the config's;
CodeSmith keeps the configured denies in force.)

A [plugin](plugins.md) can answer a permission request in place of the
user, but it is consulted only for requests that the rules would ask about,
never for ones they deny.

## Policies

`policies` are deny-only statements over the rules, as in OpenCode. A
`tool.use` statement names a permission as `action:resource`:

```jsonc
"policies": [
  { "action": "tool.use", "resource": "shell:*", "effect": "deny" },
  { "action": "tool.use", "resource": "shell:git *", "effect": "allow" },
  { "action": "tool.use", "resource": "shell:git push *", "effect": "deny",
    "message": "Pushing is done in CI" }
]
```

Each resource of a request is matched against the statements, and the last
match decides. A `deny` refuses the request whatever the rules say, before
anyone is asked and over any saved approval, with its `message` (else
"Blocked by configuration policy") as the reason. An `allow` only lifts an
earlier statement's deny: it never grants anything, so with the statements
above `git status` still asks if the rules ask. Compound shell commands are
checked command by command, and paths match without case on macOS, as rules
do. `{"action": "tool.use", "resource": "*", "effect": "deny"}` refuses
every permission.

Statements apply to every agent. Unlike `permissions`, an earlier config
file's statements have the final say: a project's `codesmith.json` cannot
lift a deny in your global config. OpenCode's other policy actions
(`provider.use`, `mcp.use`, `integration.use`, …) belong to its managed
and Console policies and are not read.

## Unattended runs

[Automations](automations.md) run with nobody at the keyboard. Anything that
would ask is refused, and after all of your rules, the agents' included,
`shell`, `edit` (which covers `write`), `webfetch`, `external_directory`
and every MCP tool (each configured server's `<server>_*`, and the resource
tools) are denied, so even an agent configured to allow them can't use them
unattended. Rules under `automation.permissions` apply only to unattended
runs and come after those denies, so they opt back in. This holds wherever
a run goes: the new child session it starts by default, or the session it
continues.

```jsonc
{
  "automation": {
    "permissions": [
      { "action": "shell", "resource": "cargo test*", "effect": "allow" },
      { "action": "edit", "resource": "CHANGELOG.md", "effect": "allow" },
      { "action": "github_create_issue", "resource": "*", "effect": "allow" }
    ]
  }
}
```

Unattended runs can't manage automations at all, whatever the rules say.
Creating, pausing, resuming or removing an automation also has its own rule:
the blanket `allow` doesn't cover it. See [what the agent may schedule](automations.md#what-the-agent-may-schedule).
