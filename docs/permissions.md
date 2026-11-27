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
| `webfetch` | the URL | Redirect targets are checked against `deny` rules too. Code Mode's `fetch` asks the same. |
| `openapi` | `<METHOD> <url>` | A call to an [OpenAPI tool](mcp.md#openapi-tools) in Code Mode. Credentials in the query are left out of the URL. |
| `websearch` | the query | |
| `subagent` | the subagent id | |
| `bus_send` | the recipient's name, or `*` | A message to another session on the [local bus](bus.md). Allowed by default. |
| `bus_receive` | the sender's name | Whether a request from another session starts a turn. Allowed by default; see [the local bus](bus.md#permissions). |
| `skill` | the skill id | |
| `lsp` | the file path | |
| `question`, `todowrite` | `*` | |
| `experimental_image_generate`, `experimental_video_generate` | the model | Ask by default. See [generating images and videos](tools.md#generating-images-and-videos). |
| `codesmith` | `*` | The [`codesmith` tool](tools.md#codesmith). Creating or removing a worktree also asks `edit` on its folder. |
| `external_directory` | the path outside the project | See [Paths outside the project](#paths-outside-the-project). |
| `<server>_<tool>` | `*` | An MCP tool, by the name the model sees. See [MCP](mcp.md#permissions). |
| `list_mcp_resources`, `read_mcp_resource` | server, or `server:uri` | See [MCP resources](mcp.md#resources). |
| `automation`, `watch_mcp_event` | operation (`create`, `pause`, ...), or server | See [automations](automations.md#what-the-agent-may-schedule). |
| `list_mcp_events` | server | Also denied where `watch_mcp_event` is denied on the server. See [MCP events](mcp.md#instructions-notifications-and-events). |
| plugin tools | `*` | By tool name, like any other tool. |

A path rule is checked against every name the path goes by: the path as
written, and its real path with symlinks resolved and the on-disk spelling.
A rule written in either form matches.

A URL (`webfetch`, `fetch`, `openapi`, and the input URLs of image and
video generation) is matched as it is requested: parsed, without its
fragment, with the scheme and host lowercased, a default port dropped,
tabs and newlines removed and `.`/`..` segments resolved. So
`HTTPS://Internal.Corp:443/x` is `https://internal.corp/x` to the rules.
Write URL rules in that form (`https://internal.corp/*`, not
`https://Internal.Corp/*`). Deny rules and policies are also checked
against the URL as written and with its `%XX` escapes decoded, so
`/%73ecret` does not get past a deny for `/secret*`. OpenCode matches the
URL as written.

A URL whose host contains `*` or `?` is refused (rules would read them as
wildcards), and "always" on a URL request saves rules for its origin only:
a saved `webfetch` or `openapi` pattern never has a wildcard in its scheme
or host.

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
  // secret guards (see below): reading these asks, whatever else allows it
  { "action": "read", "resource": "*.env", "effect": "ask" },
  { "action": "read", "resource": "*.env.*", "effect": "ask" },
  { "action": "read", "resource": "*.env.example", "effect": "allow" },
  { "action": "read", "resource": "~/.ssh/*", "effect": "ask" },
  // ... and the other guarded files listed below
  // generating images and videos costs money
  { "action": "experimental_image_generate", "resource": "*", "effect": "ask" },
  { "action": "experimental_video_generate", "resource": "*", "effect": "ask" },
  // reference checkouts are read-only to tools
  { "action": "edit", "resource": "<data>/repos/*", "effect": "deny" },
  { "action": "edit", "resource": "<data>/repos-state/*", "effect": "deny" }
]
```

So by default everything inside the project is allowed, except reading
secret files (below) and generating images or videos, which ask. Anything outside the project asks. Tools can never
write into the cache of [reference](configuration.md#references) checkouts, where
CodeSmith runs git itself (without hooks, fsmonitor or the `ext` and `file`
transports). Each built-in
agent then adds its own rules on top:

| Agent | Adds |
|---|---|
| `build` | `question` allowed. |
| `plan` | `question` allowed, `edit` denied (no `write`, `edit`, `patch`, or redirect writes) except in the Plan directory (`~/.local/share/codesmith/plan`), and `external_directory` allowed, as in OpenCode: it reads, searches and lists folders outside the project without a prompt, but still writes only plan files. A configured `external_directory` rule (deny or ask) still wins, and a configured deny still covers the Plan directory. |
| `general` | `question` and `subagent` denied. |
| `explore` | Everything denied, then `read`, `grep`, `glob`, `webfetch`, `websearch` allowed (`.env` asks), files outside the project allowed, and a list of read-only shell commands (`git diff/show/log/status/...`, `ls`, `cat`, `grep`, `rg`, `find`, ...) allowed, with options that run programs or write files (`find -exec`, `git --output`, `rg --pre`, `sort -o`, and others) denied. |
| `review` | Everything denied, then `read` (`.env` denied), `grep`, `glob` and read-only git commands allowed, and files outside the project denied. |

### Secret files

Every agent, built-in or your own, asks before it reads a file that
usually holds credentials:

- `.env` and `.env.*` files anywhere (not `.env.example`), and
  `.git-credentials` anywhere;
- in the home folder: everything under `~/.ssh`, `~/.aws`, `~/.gnupg` and
  `~/.minisign`, and `~/.netrc`, `~/.config/gh/hosts.yml` (and
  `$XDG_CONFIG_HOME/gh/hosts.yml`), `~/.docker/config.json`,
  `~/.kube/config`, `~/.npmrc` and `~/.pypirc`, and Claude Code's login
  file `~/.claude/.credentials.json` (or the one under
  `$CLAUDE_CONFIG_DIR`);
- CodeSmith's own saved keys, `auth.json` and `mcp-auth.json` (and their
  backups) in the data folder, where a Claude Code token is kept too, the
  global `codesmith.json` or `codesmith.jsonc`, and the per-repo files in
  its `projects/` folder.

Shell commands that read secrets out of the macOS keychain ask the same
way: `security find-generic-password`, `security find-internet-password`,
`security dump-keychain` and `security export`, by any path, inside `sh
-c` or `$(…)` too. The keychain holds Claude Code's login among much else,
and these print it.

These guards are a floor under every other rule. A blanket allow (`read
*`, `* *`, `~/*`), whether in the config, an agent's rules or an
automation's, does not lift them, and neither does a saved approval or an
"always" answer: answering "always" to such a read saves nothing, and the
next read asks again. A deny still denies. To let tools read one of these
files without asking, write an allow rule in your config that names the
path itself, such as `{"action": "read", "resource": "~/.ssh/config",
"effect": "allow"}`, or that is the guard's own pattern
(`~/.ssh/*`); it has to be the last configured rule matching that path.

The guards cover the `read` tool (files and folder listings), `grep`,
`glob` (guarded files are left out of a search from elsewhere, and
searching a guarded folder asks first), files attached to a prompt, and the
same tools run from Code Mode. Paths are judged by where they really are, so
a symlink to `~/.ssh/id_ed25519`, or a path through `..`, asks too. A shell
command that names a guarded file as a word asks for a `read` of it first:
`cat ~/.ssh/id_ed25519`, `cp "$HOME/.aws/credentials" x`, `wc < ~/.netrc`,
`ls ~/.ssh/*`, or a symlink to one. The shell check is best effort: it does
not see a path the command computes (`$(...)`, a variable other than
`HOME`), one hidden behind a glob (`~/.s*h/id`), or files a program opens
on its own (`ssh` reads `~/.ssh`, `aws` reads `~/.aws`). Those are governed
by your `shell` rules alone; deny or ask for the commands you don't want
run. When the project itself is inside a guarded folder (a repository at
`~/.ssh`), its files are named relative to the project and the guards
don't apply to them. Under `--auto`, which answers every question with
"once", guarded reads go through.

Every agent also asks before its shell runs `codesmith models swap` (by
name or by path), however broadly the config allows shell commands; a
`shell` rule under `agents.<id>.permissions` can lift it. That is a
prompt, not a security boundary: see
[swap](configuration.md#swapping-the-servers-model-swap) for denying the
commands a swap runs.

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

A project at `/` or at your home folder (a dotfiles repository, say) is too
broad to count: the working directory is the project's boundary instead, so
`~/.ssh` is still outside it. OpenCode does the same for `/`. A session
started in `/` or in your home folder itself has no boundary at all: every
path outside CodeSmith's scratch folder needs `external_directory` approval.

`external_directory` asks by default (the `plan` and `explore` agents allow
it, and `review` denies it). To allow a folder permanently:

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

In a turn that another session's request started over the
[local bus](bus.md#permissions), the dialog says who asked, and **a** is
replaced by **s**: allow for this session, until it closes, saving nothing.
Another session can never be the reason a rule is saved.

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
project too), keyed by the project root's real path, with symlinks
resolved: a symlink to the folder shares its approvals. Only the folder
decides, never anything read from the repository, since a fork or a
crafted repository can copy its commits, remotes and `.git` file. So a
second clone or a worktree of the same repository starts with none, and so
does a folder outside version control other than this one. A session without the database (an
in-memory one) keeps them for itself only.

`codesmith permissions` lists them with their ids, `codesmith permissions
edit ID --resource PATTERN` narrows or changes one, and `codesmith
permissions remove ID` drops it; the local server has the same under
[`/permissions/saved`](local-server.md#routes). Running sessions read the
saved approvals at every request, so a change applies at once. To make an
approval part of that repository's config instead, add the rule to its
[per-repo file](configuration.md#per-repo-config).

## Where rules come from

An agent's rules are assembled in this order, and the last match wins:

1. the [defaults](#defaults)
2. top-level `permissions` from config, from every config file in
   [load order](configuration.md#files-and-precedence): global first, then
   the matching [per-repo files](configuration.md#per-repo-config), then
   `$CODESMITH_CONFIG` and `$CODESMITH_CONFIG_CONTENT`
3. the built-in agent's own rules (see the [table above](#defaults))
4. `agents.<id>.permissions` from config
5. for [unattended runs](#unattended-runs) only, the unattended denies and
   then `automation.permissions`
6. the [session's own rules](#session-rules), which only restrict
7. approvals saved with "always" for the project, and the rules of a
   [browser attached](#browser-attachments) to the session

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
- **A repository can't add rules.** CodeSmith never reads a repository's
  `codesmith.json[c]` or `.codesmith/` agents, so a cloned repository can't
  allow its own commands. Rules for one repository go in your
  [per-repo file](configuration.md#per-repo-config), which comes after your
  global config, so its rules override the global ones there.
- **A repository's commands still ask.** A command file from the repository
  is only a prompt: any `` !`cmd` `` it expands is checked against your
  rules as the `shell` tool's commands are, and nothing in it runs unless
  every command is allowed.

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

### Browser attachments

A browser attached to a session (OpenCode's browser plugin) grants that
session `browser *` and the `browser` skill, as allows, for as long as the
attachment lasts: there is one per session, attaching again replaces it,
and its rules are revoked when its holder lets it go, when it has been
idle for its time to live (30 minutes by default) and when the registry
is disposed. A replaced attachment's revoke never removes its
replacement's rules, and another session never sees them. They are judged
like saved approvals, so they never override a configured `deny`, a
built-in agent's own `deny`, a [policy](#policies) or a [secret
guard](#secret-files). Opening a file in the browser, as a workspace path
or a `file://` URL, is checked as the read tool checks a file: the path is
resolved (`..` and symlinks followed), a file outside the project needs
`external_directory`, and it needs `read` under its project-relative, as
named and real names, which the attachment's rules don't grant. A
workspace path must not be absolute, contain `.`, `..`, `%`, `#` or `?`, or
lead out of the workspace through a symlink. The browser is handed the
`file://` URL of the file's real path.

A `browser` rule matches a URL as it would be requested, as for
[webfetch](#actions-and-resources): the scheme and host lowercased, a
default port dropped, `.` segments resolved, and trailing dots dropped
from the host, so `browser https://evil.test/* deny` also covers
`HTTPS://EVIL.test.:443/x`.
Its written and percent-decoded forms must pass the deny rules too. A URL
with credentials (`https://user@host/`) is refused, and so is any scheme
but `http`, `https` and `file`.

Because they are judged like saved approvals, they answer an `ask` the way
an "always" approval does: while a browser is attached, a configured
`browser` `ask` (yours, a per-repo file's, or a built-in agent's) is allowed
for that session without a prompt, and asks again once the attachment
ends. To forbid opening a URL even with a browser attached, write `deny`:

```jsonc
{ "action": "browser", "resource": "https://intranet.example/*", "effect": "deny" }
```

The secret guards are not asks of this kind and still ask.

Only `browser` and `skill` rules can be granted this way: the session's
gate ignores a granted rule for any other action, or one whose action is
a pattern such as `*`.

CodeSmith has no browser of its own, so no browser driver is built in:
`codesmith_core::plugin::browser` provides the attachment lifecycle and
the permission checks in front of a driver, and a custom build plugs a
driver in through its `Driver` trait. Without one, nothing opens.

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
file's statements have the final say: a per-repo file cannot lift a deny
in your global config. OpenCode's other policy actions
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
