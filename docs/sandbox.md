# Sandbox

Every shell command the agent runs goes through two checks. The first is
CodeSmith's own analysis of the command line (see
[Permissions](permissions.md#shell-commands)): it decides what to ask you
about and explains why. The second is an operating-system sandbox under the
command: the kernel refuses a write outside the folders the command may
write, whatever the command line looks like. A script that computes a path,
a program CodeSmith doesn't know, or `git -C` with a folder only known at run
time can't write outside the project, even when every permission rule
allows it.

The sandbox is on by default. You can turn it off in your own config, but a
repository can't.

## What's sandboxed

- Every command the agent runs with the `shell` tool, in the foreground or
  the background, and every process it starts.
- Commands run in the shell daemon of `codesmith serve` the same way.

Not sandboxed:

- your own commands: `!command` at the prompt, and the terminals of the
  TUI, `serve` and a paired phone;
- language servers, formatters and MCP servers, which you configure
  yourself;
- CodeSmith itself.

## What a command may do

### Write

Only to:

- the project folder (the repository root, or the working folder when that
  isn't in a repository), nested worktrees in `.worktrees/` included;
- for a linked git worktree, its git folders in the main checkout, so
  `git commit` works there;
- the session's own temp folder, which `TMPDIR` points to
  (`codesmith-sandbox-<uid>-<session>` in the system temp folder);
- CodeSmith's scratch folder and the session's tool-output folder;
- the package caches outside the project: Cargo's registry and git
  checkouts (and its lock files), npm's cache, pip's, Go's module and build
  caches, each where its usual variable (`CARGO_HOME`, `npm_config_cache`,
  `PIP_CACHE_DIR`, `GOMODCACHE`, `GOCACHE`) puts it, and `CARGO_TARGET_DIR`
  when it is set. Rustup's toolchains (`~/.rustup`) aren't among them: a
  toolchain runs outside the sandbox later, so a command mustn't be able to
  change one. When a project's `rust-toolchain.toml` needs a new toolchain,
  run `rustup` yourself, or add `~/.rustup` to `sandbox.write`;
- GPG's `trustdb.gpg` and `random_seed`, which `gpg` opens for writing even
  to sign;
- the folders in `sandbox.write`;
- folders outside the project you approved for writing: a folder the
  command names that you allowed (`external_directory`), a folder an
  `external_directory` allow rule or an "always" answer names, and a folder
  you allowed when the sandbox refused a write (below).

An approval for a folder you gave for reading, such as an "always" answer to
`ls ../other`, also lets commands write there: the permission rules don't
tell reading from writing apart. Rules for `/`, your home folder or anything
above it, or a wildcard (`external_directory: {"*": "allow"}`), never become
write folders.

### Read

Everywhere, except the protected paths. Reading outside the project is still
governed by the permission rules as before; the sandbox only adds the
protected list, which is never read or written:

- `~/.ssh` (except `known_hosts`, `config`, `config.d/` and public keys
  `*.pub`, which stay readable);
- `~/.aws`, `~/.minisign`, `~/.netrc`, `~/.config/gh/hosts.yml`,
  `~/.docker/config.json`, `~/.kube/config`, `~/.npmrc`, `~/.pypirc`, Claude
  Code's `~/.claude/.credentials.json`;
- GPG's private keys and revocation certificates
  (`~/.gnupg/private-keys-v1.d`, `secring.gpg`, `openpgp-revocs.d`); the
  public keyring and trust database stay readable;
- on macOS, the keychain files (`~/Library/Keychains`,
  `/Library/Keychains`);
- CodeSmith's own keys and runtime files: `auth.json`, `mcp-auth.json`, the
  global config files and per-repo files (which can hold provider keys), the
  remote-access key and database, and the shell daemon's and the local bus's
  runtime folders (which hold their tokens);
- anything a `read` deny rule names by an absolute path.

The list comes from the secret-file guards (see
[Permissions](permissions.md#secret-files)). Guards on a file name anywhere,
such as `*.env` and `.git-credentials`, stay with the permission rules: the
sandbox can't refuse them without refusing every project's `.env`.

### Agents

`ssh`, `git` over SSH and `git commit -S` work through their agents rather
than their key files. A command may connect to `SSH_AUTH_SOCK` and to GPG's
agent socket, whatever the network setting. A key must be in the agent: an
agent the command would start itself runs in the sandbox too, and can't read
the key files.

### Network

| `sandbox.network` | What commands may reach |
|---|---|
| `"all"` (the default) | Anything. |
| `"none"` | Local Unix sockets only (agents, the shell daemon, a test's own sockets). TCP and UDP are refused. |
| `"allowlist"` | Not available yet: it needs a proxy CodeSmith doesn't have yet. Commands are refused with a message saying so. |

### Privileges

A command can't gain privileges: on Linux `no_new_privs` is set, and on
macOS the sandbox refuses to run a setuid program. So `sudo` can't work
inside the sandbox. CodeSmith refuses a command that runs `sudo` (or `doas`)
before it starts, and adds the same note when one fails because a script
inside it ran `sudo`. A way to enter your password for one command, which
then runs that command outside the sandbox, is planned
([design/askpass.md](design/askpass.md)). Until then, run it in your own
terminal. (`/bin/ps`, setuid on macOS, runs outside the sandbox so it can
list processes; it only prints.)

## When the sandbox refuses a write

A command that fails because the sandbox refused a write outside the folders
it may write fails with the operating system's error (`Operation not
permitted` on macOS, `Permission denied` on Linux). CodeSmith reads that
error and offers to allow writing to that folder:

```
$ make install (the sandbox refused a write to /opt/tool/bin/tool; allow writing to /opt/tool/bin and run it again)
```

- **Once** runs the command again, with that one folder added, for that run.
- **Always** does the same and saves the folder, as any `external_directory`
  approval is saved (for its repository, or the folder), so later commands
  may write there from the start.
- **Reject** returns the failed command's output, with a line saying what
  the sandbox refused.

This is asked even where a rule allows the folder, so a rule allowing
everything can't turn a refused write into a silent second run. A deny rule
still refuses. CodeSmith offers one folder per command, and never `/`, your
home folder or above it: add a narrower folder to `sandbox.write` instead.
A headless run with nobody to ask reports the error. A background command
isn't run again: its notification shows the error.

A command that reads a protected path fails the same way, and the output
says the sandbox protects that path.

## Settings

In your global config (`~/.config/codesmith/codesmith.json`), or a per-repo
file in the global folder's `projects/` (see
[Configuration](configuration.md)). Never in a repository: CodeSmith reads no
config file from a repository, so a repository can't turn the sandbox off or
widen it.

```jsonc
"sandbox": {
  "enabled": true,          // the default; false turns the sandbox off
  "network": "all",         // the default; "none" allows Unix sockets only
  "write": ["~/scratch"],   // extra folders commands may always write
  "caches": true,           // the default; false drops the package caches
  "unsupported": "refuse"   // the default; "run" runs commands anyway where the sandbox can't work
}
```

`allow` (hosts for `allowlist`) is accepted and unused until the proxy is
built.

### Opting out

- `"sandbox": {"enabled": false}` turns it off. Commands then write
  anywhere you can.
- `"caches": false` removes the package caches from the write list; a
  command that needs one then fails, and you're offered its folder.
- `"write": [...]` adds folders without turning anything off.

CodeSmith says once per session, as a notice in the transcript (on stderr,
or as a `notice` event with `--format json`, in a headless run), when
commands run without the sandbox, so it's never off unnoticed.

## Machines that can't sandbox

macOS needs `/usr/bin/sandbox-exec` (every release ships it; it fails inside
another sandbox). Linux needs Landlock (kernel 5.13 or later with Landlock
enabled) and seccomp. Without them, for example on an old kernel, under WSL1,
or in a container whose seccomp profile hides Landlock, CodeSmith refuses to
run the agent's commands, in the terminal and headless alike:

```
Not run: this machine can't sandbox shell commands (the kernel has no Landlock …),
and CodeSmith doesn't run the agent's commands without one. To run them anyway,
add "sandbox": {"unsupported": "run"} to your config.
```

With `"unsupported": "run"`, commands run without the sandbox, with the
once-per-session notice.

## `codesmith debug sandbox`

Prints whether this machine can sandbox commands (and why not), what happens
to commands with the config here, and the policy a command in this folder
would get: the network mode, `TMPDIR`, the write folders, the protected paths
and the agent sockets. `--profile` prints the macOS Seatbelt profile and its
parameters too. Folders an approval adds for one command aren't shown.

## Troubleshooting

- **A build writes outside the project.** Allow the folder when asked
  ("always" keeps it), or add it to `sandbox.write`.
- **`mktemp` fails on macOS.** Without a template, macOS's `mktemp` ignores
  `TMPDIR` and uses the per-user temp folder, which the sandbox refuses like
  any other folder. `mktemp "$TMPDIR/x.XXXXXX"` works; so does allowing the
  folder when asked.
- **`xcrun: error: couldn't create cache file …/xcrun_db-…`** on macOS.
  Xcode's tool shims (`/usr/bin/git`, `make`, `clang` and others) keep a
  cache of tool paths in the per-user temp folder; the sandbox refuses it,
  and they run anyway. The cache stays unwritable on purpose: an entry
  planted there could point a later `xcrun`, outside the sandbox, at
  another program. CodeSmith doesn't offer that folder for it.
- **Something writes to `/tmp`.** Programs that follow `TMPDIR` use the
  session's temp folder; one that writes to `/tmp` by name is refused, and
  you're offered `/tmp` for that run.
- **`git commit -S` or `ssh` asks for a passphrase or can't find the key.**
  The key must be in the agent (`ssh-add`, or a GPG agent already running
  with the key unlocked). The sandbox protects the key files themselves.
- **A tool needs a keychain item.** Keychain files are protected; access
  through the macOS security service follows the keychain's own access
  rules, and `security find-generic-password` and the like ask first (see
  [Permissions](permissions.md#secret-files)).
- **Python's multiprocessing or something else needs `/dev/shm` on
  Linux.** It isn't writable by default; add `"/dev/shm"` to `sandbox.write`.
- **Commands are refused on Linux.** `codesmith debug sandbox` says why;
  in a container, Docker's default seccomp profile allows Landlock on recent
  versions, but some runtimes hide it.
- **The sandbox is in the way of something legitimate you can't fix with a
  folder.** `"sandbox": {"enabled": false}` turns it off; CodeSmith tells you
  once per session that it's off.

## Limits

- On Linux, Landlock can only allow, so reads are granted on the paths that
  cover everything but the protected ones, worked out when the command
  starts. A file created later directly in a folder that holds a protected
  path (your home folder, for one) can't be read by a command that started
  before it existed. Writes are refused inside a protected folder the same
  way.
- The network mode is all or nothing for now; host allowlists come with the
  proxy.
