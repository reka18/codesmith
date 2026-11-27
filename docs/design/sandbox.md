# Design: an operating-system sandbox for shell commands

Status: **plan for approval.** Nothing here is built yet. Ray asked for it
on 2026-11-27, for macOS and Linux, **on by default: you opt out, never
in.**

## Why

CodeSmith decides whether a shell command may run by analysing its text: the
programs it names, their options, redirects, `cd`s. That's precise and
explains itself, but it can only check what it understands. The
`git -C ../other` and `rm ../other/f` gaps (PR #42) were exactly that: options
and programs the analysis didn't know. An interpreter script that computes a
path, or a program nobody listed, can still write wherever you can.

A sandbox closes the class rather than each case. The kernel enforces where
a command may write and what it may connect to, whatever the command line
looks like. Codex CLI (Seatbelt on macOS, Landlock and seccomp on Linux) and
Claude Code (Seatbelt and bubblewrap) do the same.

The command analysis stays. It still decides what to ask about and explains
why. The sandbox is the floor under it: something the analysis misses fails
at the kernel instead of succeeding.

## What it covers

- **Sandboxed:** every command the agent runs through `shell`, including
  background jobs, and the processes they start.
- **Not sandboxed:**
  - your own terminals (the TUI's, `serve`'s and the phone's remote
    terminal, which are yours, not the agent's);
  - language servers, formatters and MCP servers, which you configure
    yourself (a later phase could add them);
  - CodeSmith's own processes.

## The policy

### Files

| | Default |
|---|---|
| **Write** | The project folder, nested worktrees (`.worktrees/`) included; the session's own temp folder (`TMPDIR` is set to it); CodeSmith's scratch and tool-output folders; and folders you approved for writing (an `external_directory` rule with write access, or an answer in the dialog), for that command. |
| **Read** | Everywhere, except the protected paths below. Reading outside the project is still governed by permissions; the sandbox only adds the protected list. |
| **Never read or written** | The secret-file guards' paths: `~/.ssh` (keys; `known_hosts` and `config` stay readable), `~/.aws`, `~/.gnupg` private keys, `~/.netrc`, `~/.docker/config.json`, `~/.kube/config`, the keychains, CodeSmith's `auth.json`, `mcp-auth.json`, `remote-key`, `remote.db` and its runtime folders. Also anything a `deny` rule names. |

Git, SSH and GPG still work, through their agents rather than their key
files:
- connecting to `SSH_AUTH_SOCK` and the GPG agent's socket is allowed;
- `git commit -S`, `git push` over SSH and `ssh` itself work as long as the
  key is in an agent.

A command that needs a protected file fails, and the error says why.

### Network

| Mode | What commands may reach |
|---|---|
| `all` | Anything, as today. |
| `allowlist` | Hosts on a list, through a local proxy CodeSmith runs (HTTP and HTTPS by `CONNECT`, plus SOCKS5 for other TCP). Everything else is refused. |
| `none` | Nothing but local Unix sockets (agents, the shell daemon). |

- `HTTP(S)_PROXY`, `ALL_PROXY` and `NO_PROXY` are set to the proxy in
  `allowlist`. A program that ignores them can't connect directly, because
  the sandbox only allows the proxy's port.
- The default list covers package registries and source hosts (crates.io,
  npm, PyPI, Go's proxy, GitHub, GitLab), your configured providers and MCP
  URLs, and `sandbox.network.allow`.
- A connection to a host not on the list asks, like any permission, with
  once, always (saved per project) or reject. A headless run refuses it,
  or asks over the ask channel ([relay-asks.md](relay-asks.md)) when there
  is one.

### When a command needs more

A command the analysis already let through, that then needs something the
sandbox refuses, fails with the sandbox's error. CodeSmith reads that error
and offers the matching permission: for example "write to ../other". If you
allow it, the command runs again with that one grant added, for that run
only, or saved if you answered "always". A headless run without an ask
channel just reports the error.

## Settings

**On by default, opt-out.** Every session sandboxes its commands wherever
the machine supports it, in the terminal and headless alike, with no setting
needed. Turning any of it off is a deliberate choice in your own config:
`"sandbox": {"enabled": false}` for everything, `"network": "all"` for the
network alone, or `"write": [...]` for one more folder. CodeSmith says once
per session when the sandbox is off, so it's never off unnoticed.

Global config only (and per-repo files in the global folder). Never from a
repo:

```jsonc
"sandbox": {
  "enabled": true,              // the default; false opts out
  "network": "allowlist",       // "all" | "allowlist" | "none"
  "allow": ["internal.example"],// extra hosts for "allowlist"
  "write": ["~/scratch"],       // extra folders commands may always write
  "unsupported": "warn"         // "warn" | "refuse": when this machine can't sandbox
}
```

- **Headless runs** (`codesmith run`, bridges, automations) use
  `unsupported: "refuse"` unless set otherwise: a headless agent never runs
  unsandboxed by surprise.
- `codesmith debug sandbox` prints the policy a command in this folder would
  get, and whether this machine supports it.

## How

### macOS: Seatbelt

- Each command runs under `/usr/bin/sandbox-exec -p <profile>`. The profile
  is generated per command in Seatbelt's profile language: deny by default
  for writes and network, allow reads except the protected paths, allow
  writes to the folders above, allow outbound only to the proxy's port in
  `allowlist`, and allow the agent sockets.
- `sandbox-exec` is marked deprecated but is still shipped and used by
  Chrome, Codex CLI and Claude Code. If Apple removes it, CodeSmith treats
  the machine as unsupported (the `unsupported` setting applies) rather than
  running unsandboxed silently.
- Paths are canonical (`/private/var` and so on), and the profile is built
  with escaping CodeSmith controls, never from command text.

### Linux: Landlock, seccomp, namespaces

- **Files: Landlock** (kernel 5.13+, no root, no extra binary), applied in
  the child before `exec`:
  - read everywhere except the protected paths;
  - write only to the folders above.

  Landlock can't deny a path inside an allowed tree. So the protected
  paths, which are all outside the project, are left out of the read tree
  by building it from the top-level folders minus those.
- **Network:**
  - `none`: a seccomp filter refuses `socket()` for every family but Unix.
  - `allowlist` on kernel 6.7+: Landlock's TCP rules allow connecting only
    to the proxy's port.
  - `allowlist` on older kernels: a new network namespace (`unshare`, with
    an unprivileged user namespace) with only loopback, plus a small relay
    into the proxy.
  - Where neither works, `allowlist` is unsupported on that machine.
- **Process:** `PR_SET_NO_NEW_PRIVS` is set, so setuid binaries (`sudo`)
  can't gain privileges inside the sandbox. `sudo` therefore needs the
  askpass path ([askpass.md](askpass.md)) to run unsandboxed. That's a
  deliberate, visible step: the askpass prompt shows the command, and
  approving it runs that one `sudo` outside the sandbox.
- **Unsupported:** no Landlock (old kernel, disabled LSM), WSL1, some
  containers. `codesmith debug sandbox` says which.

## Compatibility

Things that should keep working, with tests:
- builds and tests that write to `target/`, `node_modules`, `.venv` and the
  like, inside the project;
- caches outside it: `~/.cargo/registry`, `~/.npm`, `~/.cache/pip` and
  `~/go/pkg`. These are on a default write list
  (`sandbox.write` defaults), since package managers need them. Docs say how
  to remove them.
- `git` commit, signing via the agent, push and fetch over SSH and HTTPS;
- Docker through its socket (asks once, since it's root-equivalent);
- `ssh` with an agent; the shell daemon's PTYs.

## Order

1. **macOS:** files and network `all`/`none`, with the failure-to-prompt
   step, `debug sandbox` and the settings.
2. **Linux:** Landlock files, seccomp `none`, `no_new_privs`.
3. **The proxy and `allowlist`** on both, plus Landlock TCP rules and the
   namespace fallback.
4. **Later:** language servers, formatters and MCP servers.

## Tests

- **The escape tests:** commands that the analysis can't see write outside
  the project, and each fails in the sandbox on both systems, even with
  every permission rule allowing it:
  - an interpreter script computing a path;
  - an unknown program;
  - `git -C` with a computed folder;
  - a symlink planted inside the project pointing out;
  - a hard link;
  - `mv` of the project folder itself.
- **Protected paths:** reads of each are refused. `git commit -S` and
  `ssh` through agents still work.
- **Network:**
  - `none` refuses TCP and UDP;
  - `allowlist` allows listed hosts and refuses others, including a direct
    connection that ignores the proxy;
  - an unlisted host asks, and is refused headless.
- **Failure to prompt:** a refused write offers the right permission, and
  re-runs with only that grant.
- **Unsupported machines:** `warn` runs unsandboxed with a notice; `refuse`
  refuses; headless defaults to refuse.
- **Compatibility:** the list above, on both systems.
- **Repo config can't touch it:** a repo can't set `sandbox.*`, and per-repo
  files in the global folder can.
- **On by default:** a session with no `sandbox` settings at all is
  sandboxed, in the terminal and headless. Each opt-out (`enabled: false`,
  `network: "all"`) works and gets the once-per-session notice.

## Open questions

1. **The default network mode:** `allowlist` (as drafted) or `all` to start,
   tightening later?
2. **Package caches outside the project** (`~/.cargo`, `~/.npm`, …): writable
   by default, as drafted, or ask the first time?
3. **Interactive sessions on an unsupported machine:** `warn` (as drafted)
   or `refuse`, like headless?
