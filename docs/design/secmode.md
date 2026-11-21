# SecMode: running CodeSmith's code-executing tools in a container

Status: design, no code. Scope: "Mode 2". CodeSmith stays on the host and holds every credential. Whatever runs the project's code runs in a locked-down container with the project bind-mounted. Remote workspaces are out of scope.

## 0. Why the name and the config key

The user-facing name is **SecMode**:

- CLI flags: `--secmode` and `--no-secmode`.
- Environment: `CODESMITH_SECMODE=1`.
- TUI: a `/secmode` command and a status-line badge.
- Docs: `docs/secmode.md`.

The config key is a top-level `"secmode": {...}`, not `"sandbox": {"mode": "secmode"}`, for three reasons:

1. SecMode and a later per-command OS sandbox (Seatbelt/Landlock) are layers that run together. A shared `sandbox.mode` would make them look like alternatives.
2. SecMode has its own trust rule: a project may only tighten it (§10). That rule is easier to state and enforce for a whole key.
3. The key matches the name the user sees.

`"sandbox"` is kept free for the OS-sandbox layer.

## 1. Architecture

| Runs on the host (trusted) | Runs in the SecMode container (untrusted) |
|---|---|
| Agent loop, providers, OAuth, MCP auth, keychain | `shell` tool: foreground, background and timed-out-to-background jobs |
| Permission engine, policies, saved approvals | `!cmd` user commands (same `Jobs::spawn_proc` path) |
| File tools `read/write/edit/patch/grep/glob`, hardened (§4) | PTY terminals (`serve`), and the worktree `start` command |
| Snapshots, worktree creation, review and diff git, all hardened | Phase 3: MCP stdio servers from project config, LSP servers, formatters |
| Egress proxy (§5), host-command escape hatch (§7) | The in-container shell daemon (`codesmith __shelld --stdio`) |

**Choke point.** Every command already goes through `Jobs::spawn_proc` (`tool/shell.rs`). When a daemon is set (`Jobs::use_daemon`), it sends `Request::CreatePipe` with `program/args/cwd/env/output_path`, and kills, waits and timeouts go through `Signal/Wait/Terminate`. In SecMode, CodeSmith sets a daemon whose transport reaches into the container, and this applies in the TUI and `run` too, which run commands in-process today. The job model, `Ending::Lost`, adoption and notices are reused unchanged.

**Transport: one multiplexed stdio channel, not a socket.** I tested this. A Unix socket does not cross Docker Desktop's virtiofs bind mount in either direction:

- container connecting to a host socket: `EOPNOTSUPP`;
- host connecting to a container socket: `ECONNREFUSED`.

The daemon's per-request connections, its peer-uid checks and its socket directory therefore cannot be used as they are. Instead:

- The host runs the container with the daemon as its main process, roughly `docker run -i --rm --init … /opt/codesmith/codesmith __shelld --stdio`.
- The daemon speaks the existing frames over stdin/stdout, inside a thin mux envelope `{chan, frame}` (protocol version +1).
- Each current "connection" (a one-shot request, `own`, `wait`, `attach`, `events`) becomes a channel.
- The same mux carries egress-proxy streams (§5) and, later, MCP and LSP stdio.
- Authentication is the channel itself: only the host process holding the attach has it. The token stays in the protocol for defence in depth.

On native Linux a bind-mounted socket would work, but a single transport for every runtime is simpler. Apple `container` has `--publish-socket host:container`, which is noted for later.

**Lifecycle.**

- **One container per project per CodeSmith process**, keyed by the project id (as worktrees already use it) and a hash of the effective SecMode config. Sessions of that project share it.
- **Started lazily** on the first command, the way the daemon is started on demand today.
- **Stopped by EOF.** It runs with `--rm`, and `StdinOnce` closes the daemon's stdin when the host exits or crashes. The daemon then applies its existing "owner connection closed, stop everything" rule (SIGTERM to process groups, then SIGKILL after 3 s) and exits, which removes the container.
- **Orphan sweep.** Containers carry the labels `codesmith.secmode=<project>` and `codesmith.owner=<pid>:<start-time>`. At startup, CodeSmith sweeps labelled containers whose owner is dead, as `server::sweep` does for runtime directories.
- **Crash recovery.** If the container dies, its running jobs end as `Lost` and the next command starts a fresh container. If the daemon stops answering (for example a fork bomb has used up the pids), the host runs `docker kill` after 5 s, and that path is tested.
- **Serve handoff (Phase 3).** The container is detached (`docker run -d`) and each server re-attaches the mux with `docker exec -i`, so commands survive a server restart as they do now.

**Startup latency** (measured on Docker Desktop 29.8, Apple Silicon, image already present):

| Operation | Time |
|---|---|
| `docker run --rm … true`, hardened flags | ≈115 ms |
| `docker run -d` (create and start) | ≈105 ms |
| `docker exec true` | ≈42 ms, still 44 ms while a fork bomb was saturating the pids limit |
| `docker kill` + `rm` | ≈60 ms |

Targets:

- first command of a session ≤ 400 ms, excluding any image pull or build, which is shown as progress;
- each later command ≤ 10 ms over the mux, so no `docker exec` per command;
- restart after a kill ≤ 300 ms.

## 2. Runtimes

- **Docker Desktop, OrbStack, Colima, and Podman on macOS.** All containers run in **one shared Linux VM**, and the VM's file server runs as your macOS user. That has four consequences:
  - **A container escape is a VM escape.** A kernel bug gives root in the VM, which means every other container and **every folder shared into the VM**. Docker Desktop shares `/Users` by default, so in practice that is your home folder, even though SecMode mounts only the project.
  - **Uid checks inside a bind mount mean nothing.** Tested: uids 501, 1000, 65534 and root without capabilities could all read a host file with mode 0600. Every file they created belonged to the host user on the host, and setuid bits were dropped.
  - **Bridge networking reaches the host's loopback.** Tested: a bridge container fetched a server bound to the host's `127.0.0.1` through `host.docker.internal` (HTTP 200). That includes `codesmith serve`, Ollama and oMLX. So SecMode never uses bridge networking.
  - Mitigations: `secmode doctor` warns when VM file sharing covers the home folder and recommends narrowing it to the code folders. Enhanced Container Isolation helps where it is licensed.
- **Apple `container` (v1.0, macOS 26).** It runs **one lightweight VM per container** (Virtualization.framework), so an escape lands in a VM that holds only this project's mounts. Its documented flags have `--user`, `--cpus`, `--memory`, `--read-only`, `--tmpfs`, `--cap-drop`, `--init`, `--publish-socket` and `--no-dns`. They have **no pids limit** (the VM's own kernel bounds a fork bomb) and **no documented `--network none`**. Memory ballooning is only partial: memory the guest frees is not returned to macOS. I could not test it, because it isn't installed.
- **Native Linux namespaces** (Docker or Podman on a Linux host): there is no VM, so an escape reaches the host kernel. Prefer **rootless Podman** (`--userns=keep-id`) or rootless Docker, so that an escape lands as your unprivileged user. Rootful Docker runs with `--user $(id -u):$(id -g)`. A built-in `bwrap`-style launcher is not planned.

**Detection order** (`"runtime": "auto"`):

- macOS: `docker` (any context, including OrbStack and Colima), then `podman`. `apple` is opt-in until the Phase 3 checks pass (§12, question 1).
- Linux: rootless `podman`, then rootless `docker`, then rootful `docker` with a warning.

A runtime counts as found only if `info` answers within 2 s. A driver is a small trait over each runtime's CLI (`create/attach/kill/inspect/ps --filter label`). Docker and Podman share one implementation.

**Hardened flags** (from `scripts/ci-linux.sh`, plus more):

```
--init --rm -i --user <uid>:<gid> --read-only --cap-drop ALL
--security-opt no-new-privileges --network none
--cpus N --memory M --memory-swap M --pids-limit P --ulimit nofile=4096
--tmpfs /tmp:rw,nosuid,nodev,size=1g --tmpfs /home/codesmith:rw,nosuid,nodev,size=256m
```

There is no `--privileged`, no `--cap-add`, and no device, socket, home-folder or SSH-agent mount.

## 3. Images

**The default image** is `ghcr.io/reka18/codesmith-secmode:<version>`, pinned by digest inside the host binary. It is Debian slim with bash, git, curl, ca-certificates, build-essential, python3 and an `open`/`xdg-open` shim (§7). It contains **no CodeSmith binary**. The static `aarch64`/`x86_64-unknown-linux-musl` release build, the same version as the host, is mounted read-only at `/opt/codesmith`:

- It is downloaded once into `<data>/secmode/bin/<version>-<arch>`, and its sha256 is pinned in the host binary.
- Development builds name a local binary with the global-only `secmode.daemon_binary` setting (built with `cargo zigbuild`).

**The project's toolchain**, in order of preference:

1. `secmode.image`, set to `name@sha256:…`.
2. `secmode.build`, set to `{"dockerfile": "…", "context": "…"}`. It is built with `--network none` unless the user approves network for the build.
3. `secmode.devcontainer`, set to a path to a `devcontainer.json`. Only `image` and `build.dockerfile/context/args` are read. `runArgs`, `mounts`, `capAdd`, `privileged`, `features`, `forwardPorts` and `remoteUser` are **ignored, with a warning naming each one**, because they would widen the box.

The image a project proposes is **asked about once** (`secmode_image <ref>`). The answer and the resolved digest go into `<state>/secmode/<project>.lock`, and a changed digest asks again.

**Caches** are named volumes per project and ecosystem (`codesmith-sm-<project>-cargo`, `-npm`, `-pip`, `-go`). They are mounted at `CARGO_HOME`, `npm_config_cache`, `PIP_CACHE_DIR` and `GOMODCACHE`. They are never shared between projects, so one project cannot poison another's cache. `codesmith secmode prune` removes them.

**Offline.** Network `none` needs nothing. Images and caches are local. A missing image fails with a pull hint and never falls back to the host.

## 4. Filesystem

**Mounts** (absolute paths are the same inside as on the host, which I call path identity):

| Mount | Mode | Notes |
|---|---|---|
| project root (or the worktree), at **the same absolute path** | rw | No path translation anywhere: tool output, `workdir`, error text and git's `gitdir:` pointers all agree. |
| `<main>/.git` (for a worktree outside the project) | rw | Worktrees live under `<data>/worktree/<project>/<slug>`. Their `.git` file names `<main>/.git/worktrees/<slug>`, so the common dir is mounted at the same path too. |
| `.git/config`, `.git/hooks/`, `.git/info/`, `config.worktree` | **ro** sub-mounts | The container cannot plant hooks, `core.fsmonitor`, `core.pager` or filter drivers for host git to run. |
| `<data>/tool-output/<session>/` | rw | The daemon writes job output there with `O_NOFOLLOW`, and the host tails it. |
| `/opt/codesmith` | ro | The daemon binary. |
| `/tmp`, `$HOME=/home/codesmith` | tmpfs | A generated read-only `.gitconfig` that sets only `user.name/email`. |
| caches | named volumes | §3 |
| `secmode.mounts` | as configured | Global config only, or approved by an ask (§6). |
| `.env`, `.env.*` (`secmode.mask`) | `/dev/null` bound over each match | This matches the host's `read *.env → ask`. The masks are computed at container start. |

Never mounted: the home folder, `~/.ssh`, the SSH agent, the Docker socket, `/var/run`, the host's `/etc`, keychains, `~/.config/codesmith`.

**Environment.** Today `command_env` passes **the host's whole environment** to commands. That includes any `*_API_KEY` set in the shell. In SecMode the environment is an **allowlist**:

- `PATH` (the image's), `HOME`, `LANG`, `TERM`, `TZ`, the `NONINTERACTIVE_ENV` defaults, `AGENT`, `CODESMITH`, `AI_AGENT`, `CODESMITH_SESSION_ID` and the proxy variables;
- plus names listed in the global `secmode.env`.

A session environment (`Jobs::set_environment`) is filtered the same way.

**File tools stay on the host, with an open-beneath resolver.** Running them in the container was considered and rejected:

- I measured virtiofs at about 4× slower than host-native for small files (3000 creates: 667 ms against 167 ms; a `cat` of all of them: 568 ms against 115 ms) and 13× slower than a volume.
- The host-side permission checks and diff previews would have to be reimplemented in the daemon.

The real risk with the tools on the host is **TOCTOU**. Today the path is canonicalized, checked, then opened by path. A background job in the container can swap a directory for a symlink to `/Users/<u>/.ssh` between the check and the open. Tested: the container can create a link to `/Users/<u>/.ssh/id_ed25519`. Inside the container it resolves to nothing, but on the host it is a real link into the home folder.

So in SecMode every host file operation inside a mounted tree resolves **beneath the root**:

- On Linux, `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`.
- On macOS, a walk with `openat(O_NOFOLLOW|O_DIRECTORY)` from a dirfd of the root, component by component. The final open uses `O_NOFOLLOW`, and `O_NOFOLLOW_ANY` (macOS 11 and later) where it applies.
- Writes create a temporary file with `openat` in the final dirfd and `renameat` it within the same dirfd.
- A symlink found inside the tree is resolved **by the host on purpose**, and its target goes through the existing `external_directory`/`edit` checks before the walk restarts from the target.
- `grep` and `glob` walks never follow symlinks.

**Uid and ownership.** The container runs as `--user <host uid>:<host gid>` on every runtime:

- On Docker Desktop the uid is cosmetic, as tested.
- On Linux it makes the files the container creates belong to the user.
- Rootless Podman uses `--userns=keep-id`.

The image has to tolerate any uid: `$HOME` is a tmpfs and the shared directories are mode 1777.

## 5. Network

The default is **`none`**. Tested under `--network none`:

- DNS fails;
- `1.1.1.1:443`, the gateway and `host.docker.internal` all return "network unreachable";
- only `lo` is up, and a listener on `127.0.0.1` still works.

An `--internal` Docker network behaved the same way.

**Allowlist mode (`"network": {"mode": "proxy", …}`).** Measured with a throwaway prototype:

1. Inside the `--network none` container, the daemon listens on `127.0.0.1:3128` and forwards each connection as a mux channel.
2. The host proxy reads `CONNECT host:port` and checks the allowlist.
3. It resolves the name on the host and refuses private, loopback and link-local answers, then connects to the address it checked.
4. Results:
   - `https://example.com` was allowed (200, 0.11 s);
   - `www.google.com` got a `403` and the client saw "Tunnel connection failed: 403";
   - a direct fetch with no proxy failed at DNS.

The real proxy runs in-process (tokio):

- It reuses webfetch's pinned resolve and private-address refusal, which already covers DNS rebinding and `169.254.169.254`.
- A CONNECT target is matched as `https://host[:port]/` against `webfetch` deny rules and `policies`. Without MITM the path is invisible, which the docs state. Plain-HTTP requests get the full URL checks.
- Allow entries are `host[:port]` patterns (the default port is 443), or presets: `npm`, `crates`, `pypi`, `go`, `github-read`, `debian`.
- The container gets `HTTP(S)_PROXY`, `ALL_PROXY`, `NO_PROXY=localhost,127.0.0.1`, `CARGO_HTTP_PROXY`, `npm_config_proxy` and `PIP_PROXY`. DNS stays off inside the container, because names are resolved by the proxy.
- An unlisted host **asks** (`secmode_network`, resource `host:port`). "Always" saves a user-level approval for the project. Unattended runs deny it.
- A bridge or open network is not offered. Even `"allow": ["*"]` still refuses private addresses, because bridge reaches the host's loopback (§2).

**ssh, git and credential helpers.** The container has no keys, agent, credential helper or `~/.gitconfig`:

- `git push`, `gh` and `ssh` cannot authenticate, and raw TCP (port 22, `git://`) has no route.
- `git fetch` over https from an allowed host works anonymously.
- Authenticated operations are run on the host through §7, or refused with a message that says so.

## 6. Permissions and trust

- **Configured rules, policies and saved denies apply unchanged**, and the shell scanner still runs for every command. SecMode adds a *relaxation layer* that goes right after the built-in defaults, at step 1½ of "Where rules come from". It applies only to requests from the container's tools, so any configured rule after it, whether allow, ask or deny, still wins. It is evaluated like an agent's rules: with and without the layer, and the stricter answer is kept wherever a configured rule matches. **A deny is never weakened.**
- **Asks that SecMode makes unnecessary.** These are default-only `external_directory` asks raised by the shell (a `cd` or `workdir` outside the project, or a redirect to a path outside it). Inside the container those paths are its own tmpfs, or don't exist at all. Writes inside the project are already allowed by default.
- **What stays the same.** All host file tools keep `external_directory`, `.env` reads, `edit` and the `<data>/repos` denies, because they run on the host. A configured `shell: ask` keeps asking.
- **New asks, always asked, never relaxed:**
  - `secmode_network` (an allowlist addition);
  - `secmode_mount` (extra mounts);
  - `secmode_image` (an image the project proposes);
  - `host_exec` (§7);
  - `secmode_off` (turning SecMode off in a session, which only the user can do; the model has no tool for it).
- **Project config under SecMode is restricted** (today "project config is trusted"), following the precedent of `swap`, which is read only from global config:
  - It may only **tighten SecMode** (§10).
  - Its `permissions` act like session rules, which only restrict.
  - `{env:}` and `{file:}` substitution is disabled, because it could put `~/.ssh` into a provider header.
  - Provider and endpoint changes are ignored.
  - Its MCP stdio servers, LSP servers and formatters run in the container, from Phase 3. Until then they are disabled.
  - Remote MCP servers from project config are ignored.

  This matters because the container can write `codesmith.json`, `.codesmith/` and `AGENTS.md` into the project for the next start.
- **Host git is always hardened.** The `repository.rs` `hardening()` flags (`core.hooksPath=/dev/null`, `core.fsmonitor=false`, `protocol.ext.allow=never`) are extended to snapshots, worktrees, review and the TUI's `diff_source`, plus `core.pager=cat`, `diff.external=` and `--no-ext-diff`, with `GIT_CONFIG_NOSYSTEM`. This applies even outside SecMode.
- **Secret-file guards.** The read tool's `.env` asks stay. The container cannot see `.env` files at all, because of the masks. `secmode.mask` is additive from any layer.
- **Unattended runs.** The shell and edit denies are **not** lifted automatically. `automation.permissions` can opt back in, and the docs recommend doing that only with SecMode on.

## 7. Escape hatches

- `secmode.host_commands` (**global config only**), for example `["git push *", "gh pr *", "gh issue *"]`. The shell tool runs a line on the host only when the scanner shows **every** command on it matches a host pattern and nothing is dynamic. It asks `host_exec <command>` first ("always" may be saved), and it runs with the normal host environment and the hardened git. Mixed lines (`npm test && git push`) are refused: *"`git push` runs on the host in SecMode; run it as its own command."*
- **Failure hint.** A command that fails in the container with a known credential error (`Permission denied (publickey)`, `could not read Username`) gets a notice: *"SecMode: no credentials in the container. Add it to `secmode.host_commands` or run it with `/host`."*
- **Browser.** The image's `open`/`xdg-open` shim sends `open <url>` over the mux. The host checks the URL against the `webfetch` rules and asks with `host_exec`.
- **User.** `/host <cmd>` or `!!cmd` runs a command on the host as the user, because the user typed it. A plain `!cmd` runs in the container. `/secmode off` (asks) turns SecMode off for the session, and the badge turns red.

## 8. Limits, timeouts and kill semantics

- **Defaults:** `cpus = min(4, ncpu/2)`, `memory 4g` (swap equal to memory), `pids 1024`, `/tmp 1g`, `nofile 4096`. Tested: 1 GiB allocated under a 256 MiB limit exited 137, and a bash fork bomb under `--pids-limit 64` hit "fork: Resource temporarily unavailable" without disturbing the host. Bind-mount writes cannot be limited, which the docs state.
- **Timeouts** stay on the host (`Jobs::watch`). At a timeout, cancel or `/stop`, the host sends `Signal` to the daemon, which sends SIGTERM to the job's process group (`own_session`), waits 3 s (`KILL_GRACE`), then sends SIGKILL. `stop_all` maps to `Terminate`. Killing the `docker` CLI never kills anything inside, so kills always go through the daemon.
- **A daemon that is starved or OOM-killed** gets a `docker kill` after 5 s without a reply. All running jobs become `Lost` with *"SecMode container was restarted (pids or memory limit)"*.
  - The daemon sets each child's `oom_score_adj` to +500. Raising it needs no privilege, so a build dies before the daemon does.
  - An OOM kill is read from `memory.events` and reported as *"killed: out of memory (limit 4g)"*.
  - A pids exhaustion is reported from `pids.events`.

## 9. Threat model

**What SecMode protects against**, for code from the repository (build scripts, install hooks, tests):

- reading files outside the project, including the home folder, `~/.ssh`, cloud credentials, keychains and CodeSmith's own state;
- getting provider keys or tokens (no host environment, no files);
- writing outside the project, including persistence through LaunchAgents or shell rc files;
- reaching the network, the host's loopback services, or cloud metadata endpoints;
- getting host-side git to run code through hooks, fsmonitor or config;
- weakening CodeSmith through a planted `codesmith.json`;
- exhausting the host's CPU, memory or pids.

**What it does not protect against:**

- the project itself being changed or wiped (rely on snapshots and git);
- exfiltration through allowlisted hosts (`npm publish` and gists stay possible, though without credentials);
- prompt injection in tool output steering the agent, which leaves the asks on host tools and `host_exec` as the line of defence;
- code you later run outside SecMode;
- a malicious or compromised image (mitigated by digest pinning, a default image from the project's own registry, and asking for project images);
- container escapes by kernel bug: on a shared VM this reaches everything shared into the VM, which by default includes `/Users`; with a per-container VM it reaches only that VM; on Linux it reaches the host kernel;
- side channels (cache timing, Spectre-class, and CPU or memory contention between co-tenants);
- anyone else who can use your Docker daemon.

| | Permission-only (today) | SecMode |
|---|---|---|
| A shell command reads `~/.ssh` | Blocked only if the scanner sees the path and a rule asks. `python -c`, `make` and `npm install` hooks are invisible. | It isn't mounted. |
| Provider keys in the environment | Inherited by every command | Allowlisted environment |
| Network | Unrestricted | none, or a proxied allowlist |
| `npm install` postinstall | Runs as you, everywhere | Runs in the box |
| Prompt cost | Many asks, or a blanket allow | Few asks, and the boundary does the work |

## 10. Config surface

```jsonc
"secmode": {
  "enabled": true,                       // or CLI --secmode / env CODESMITH_SECMODE=1
  "runtime": "auto",                     // docker | podman | apple
  "image": "ghcr.io/acme/dev@sha256:…",  // or "build": {...}, or "devcontainer": ".devcontainer/devcontainer.json"
  "limits": { "cpus": 4, "memory": "4g", "pids": 1024, "tmp": "1g" },
  "network": { "mode": "none", "allow": ["crates", "npm", "registry.acme.dev"] },
  "mounts": [{ "source": "~/src/shared-lib", "readonly": true }],   // global only
  "caches": ["cargo", "npm"],
  "mask": [".env", ".env.*"],
  "env": ["RUST_LOG"],                   // global only
  "host_commands": ["git push *", "gh pr *"],   // global only
  "mcp": "container", "lsp": "container", "format": "container"
}
```

**Trust rules.** Global config, `$CODESMITH_CONFIG`, the CLI and the environment are the user's, so they set anything. **A project file may only tighten:**

- It may set `enabled: true` (never false).
- It may set lower `limits` (merged by min).
- It may add `mask` entries.
- It may propose `image`, `build`, `devcontainer` or `network.allow`, each of which asks (§6).
- `runtime`, `mounts`, `env`, `host_commands`, a network `mode` that loosens, and `mcp/lsp/format: "host"` are ignored from project files, and `codesmith debug config` names each one ignored.

**UI:**

- The status line shows `SEC docker · net:none` or `SEC docker · net:3 hosts`, and `SEC off` in red when SecMode is configured but turned off.
- `/secmode` shows status, mounts and the allowlist. `/secmode allow <host>` asks. `/secmode off` asks.
- `codesmith secmode doctor` checks the runtime, VM file sharing and the image digest, then runs the escape suite in about 10 s.
- `codesmith secmode shell` opens a terminal in the container.
- `codesmith secmode prune` removes the caches and stale containers.
- The shell tool's description says "Commands run in a Linux container (SecMode)…", so the model knows.

**Coexisting with a per-command OS sandbox** (`"sandbox"`, designed later):

- Seatbelt/Landlock wraps processes that run on the host: escape-hatch commands, host git, and everything when SecMode is off.
- Inside a Linux container, Landlock can also confine each command (the project read-write, nothing else), as a second wall.
- The badge shows both layers.

## 11. Testing plan

- **Unit tests:**
  - The config merge only restricts. A property test checks that no project layer adds mounts, network or host commands, or turns SecMode off.
  - A property test over random rule lists checks the relaxation layer: a configured deny or ask stays a deny or ask.
  - The beneath-resolver is tested, including a **race test**: a thread flips a directory and a symlink to an outside path 10,000 times while `write`, `edit` and `patch` run, and nothing outside may change.
  - The environment allowlist is tested (a fake `ANTHROPIC_API_KEY` on the host must not appear).
- **Escape and negative suite** (`tests/secmode_escape.rs`, skipped with a notice when no runtime is found):
  - writing outside the project, and to a sibling of the mount;
  - reading `/Users/<u>` and `~/.ssh` (ENOENT);
  - the Docker socket, `/proc/1/root`, and `mount` (EPERM);
  - network: `1.1.1.1`, DNS, `host.docker.internal`, the gateway, `169.254.169.254`; and through the proxy, an allowed host, a denied host, and an allowed name that resolves to `127.0.0.1` or `10.x`;
  - symlink escapes, both absolute and relative, through the read, write and edit tools;
  - a hook, `core.fsmonitor` or `core.pager` planted by the container (with a sentinel file) while the host takes snapshots, diffs and creates a worktree;
  - a planted `codesmith.json` that disables SecMode or adds a mount, which must be ignored;
  - a fork bomb (the host stays responsive, the container restarts, and the next command works);
  - OOM reporting;
  - filling the tmpfs;
  - a timeout that kills the whole group (`sh -c 'sleep 100 & sleep 100'`);
  - `kill -9` of the host process (the container is gone, or swept at the next start).
- **Compatibility:**
  - the existing daemon and shell suites (`oc_shell_daemon.rs`, the shell tool tests) run again with the SecMode transport;
  - cargo, npm, pytest and go projects with presets, and offline;
  - a worktree session;
  - a devcontainer image.
- **Performance** (benchmarks in `benches/`): first-command latency, per-command overhead over the mux (p50 ≤ 10 ms), and `cargo build` with `target/` on the bind mount against a volume.
- **CI** (local, with no GitHub Actions):
  - `scripts/ci-secmode.sh` runs from `scripts/ci.sh` in the pre-push hook when `docker info` answers, with a budget of about 90 s. Otherwise it prints that it was skipped.
  - It cannot run inside `ci-linux.sh`, because that would need the Docker socket, the very mount SecMode forbids.
  - The Linux path (rootless Podman) runs on a Linux box before each release, as an item on the `RELEASING.md` checklist.

## 12. Size, phases, open questions

**Size estimate** (Rust, production code; tests about 2,500 lines more):

| Part | Lines |
|---|---|
| Mux transport and stdio daemon mode | ≈800 |
| Runtime drivers and lifecycle | ≈1,000 |
| Egress proxy | ≈700 |
| Beneath-resolver and file-tool changes | ≈600 |
| Config and trust | ≈400 |
| Permission layer and host escape hatch | ≈500 |
| Git hardening | ≈200 |
| Environment, mounts, masks, caches, images | ≈600 |
| CLI and TUI | ≈400 |
| **Total** | **≈5,200** |

That is about 5 to 6 weeks for one person.

**Phases:**

- **Phase 1, the MVP:**
  - Docker and Podman, with the default image and network `none`;
  - the shell tool, `!cmd` and background jobs through the in-container daemon (`run -i` mux);
  - path-identity mounts, the environment allowlist, and the tool-output mount;
  - the beneath-resolver for the host file tools;
  - read-only `.git` config and hooks, plus host git hardening;
  - restricted project config and the config trust rules;
  - the badge, `doctor`, and the escape suite.

  In the MVP, LSP, formatters and project MCP stdio servers are **disabled**. Examples: rust-analyzer runs `build.rs` and proc macros, and eslint or prettier load plugins from the repo.
- **Phase 2:** the egress proxy with presets and asks; caches; masks; `host_commands`, `/host` and the `open` shim; OOM and pids reporting; PTY terminals; the worktree `start` command in the container.
- **Phase 3:**
  - MCP stdio, LSP and formatters in the container, through a new long-lived stdio-channel request;
  - `build` and `devcontainer` images;
  - serve handoff (a detached container, re-attached with `exec`);
  - the Apple `container` runtime.
- **Phase 4:** the Seatbelt/Landlock layer, as a separate design.

**Open questions, each with a recommendation:**

1. **Should Apple `container` come first in `auto` on macOS?** *Recommendation:* make it opt-in until a test confirms three things: there is a no-network mode or the network is absent, `--publish-socket` or stdio attach is reliable, and the VM's own pids bounding holds up. After that, prefer it on macOS 26 and later, because a per-container VM removes the "all of `/Users`" caveat.
2. **Should file tools stay on the host or go through the container?** *Recommendation:* the host, with the beneath-resolver. It is faster, the checks and previews already exist, and there is only one implementation.
3. **Should the container's lifetime be per session or per project?** *Recommendation:* per project per CodeSmith process, stopped when CodeSmith exits. Add `keep` for idle reuse only if the 115 ms start ever matters.
4. **Where do MCP stdio servers from the *global* config run?** *Recommendation:* on the host by default, since they are the user's tools and often need credentials, with a per-server `"secmode": "container"` opt-in. Servers from project config always run in the container.
5. **What happens to LSP and formatters before Phase 3?** *Recommendation:* turn them off in SecMode, with a one-line notice. Running rust-analyzer on the host defeats the point.
6. **Should SecMode relax the unattended denies?** *Recommendation:* no. The user opts in through `automation.permissions`, and the docs advise doing that only with SecMode on.
7. **Should the default network be `none` or a proxy with presets?** *Recommendation:* `none`. The first blocked connection says *"blocked by SecMode; `/secmode allow registry.npmjs.org`"*.
8. **Should `.git` be writable by the container?** *Recommendation:* yes, so commits and branches work inside, but with config, hooks and info read-only, and host git hardened whatever happens.
9. **How does the Linux daemon binary get there?** *Recommendation:* mount the matching static musl release binary, sha-pinned and cached, rather than baking it into images. That works with any user image.
10. **Should SecMode refuse to run when Docker Desktop shares the whole home folder?** *Recommendation:* warn in `doctor` and once per project; don't refuse, because the default would make SecMode unusable for most people.
11. **Does the strict environment allowlist break workflows?** *Recommendation:* keep it strict, add names in the global `secmode.env`, and in `doctor` list host variables that look like secrets and were dropped.
