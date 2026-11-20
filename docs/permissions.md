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
"effect": "allow"}` keeps `edit` visible even for an agent that denies it
elsewhere.

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
  { "action": "read", "resource": "*.env.example", "effect": "allow" }
]
```

So by default everything inside the project is allowed, except reading
`.env` files, which asks. Anything outside the project asks. Each built-in
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
  `env touch x`, `sudo rm x`, `xargs rm`, `nice`, `timeout`, `find -exec`,
  and the commands inside a static `sh -c '...'`, `eval '...'` or `trap`
  string. A rule that denies `rm *` also denies those forms.
- **Redirects.** A file that a redirect writes to (`>`, `>>`, `&>`, `>|`,
  `<>`) is checked like a `write`: `external_directory` if it is outside the
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

The project is the nearest folder with `.git` above the working directory, or
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
| **a** | Always allow: allow, and save a rule for the rest of the session. |
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

"Always" saves rules only for the current session:

- For a shell command, it saves the command's leading words plus `*`:
  `git status *`, `npm run build *`, `cargo test *`. CodeSmith knows how many
  words make up a subcommand for common tools (`git`, `npm run`,
  `docker compose`, `cargo`, `kubectl`, and others).
- For `external_directory`, it saves the folder (`<dir>/*`). If the folder is
  `/`, your home folder or above it, only that file is saved: approving
  `~/.zshrc` doesn't approve all of `$HOME`.
- A saved approval never overrides a configured `deny`.

To make an approval permanent, add the rule to your config.

## Where rules come from

An agent's rules are assembled in this order, and the last match wins:

1. the [defaults](#defaults) and the built-in agent's own rules
2. `agents.<id>.permissions` from config
3. top-level `permissions` from config, from every config file in
   [load order](configuration.md#files-and-precedence): global first, then
   project files
4. approvals saved with "always" during the session

Two consequences follow:

- Top-level rules override per-agent rules, the built-in ones included. A
  top-level `{"action": "edit", "resource": "*", "effect": "allow"}` lets
  `plan` (and the read-only `review` agent) edit. Prefer narrow top-level
  allows, and put agent-specific allows under `agents.<id>.permissions`.
- A project's `codesmith.json` comes after your global config, so **its rules
  override yours**. This is the main reason to read a repository's config
  before you run CodeSmith in it (see the warning in
  [configuration](configuration.md#files-and-precedence)), or to set
  `CODESMITH_DISABLE_PROJECT_CONFIG=1`.

A [plugin](plugins.md) can answer a permission request in place of the
user, but it is consulted only for requests that the rules would ask about,
never for ones they deny.

## Unattended runs

[Automations](automations.md) run with nobody at the keyboard. Anything that
would ask is refused, and on top of your rules, `shell`, `edit` (which
covers `write`), `webfetch` and `external_directory` are denied. Rules under
`automation.permissions` apply only to unattended runs and come after those
denies, so they opt back in:

```jsonc
{
  "automation": {
    "permissions": [
      { "action": "shell", "resource": "cargo test*", "effect": "allow" },
      { "action": "edit", "resource": "CHANGELOG.md", "effect": "allow" }
    ]
  }
}
```

Creating an automation also has its own rule: the blanket `allow` doesn't
cover it. See [what the agent may schedule](automations.md#what-the-agent-may-schedule).
