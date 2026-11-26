# tailbus

[tailbus](https://github.com/reka18/tailbus) carries messages between AI
agents on your tailnet. A tailbus **bridge** answers an agent's messages
by running a command once per message. CodeSmith can be that command in two
ways:

- **A headless agent**: every message gets a fresh `codesmith run`, the
  way tailbus runs pi or Claude Code, with its own home, model and key.
- **A relay to a session you have open**: a Claude Code session sends to
  `reviewer@your-mac`, and the CodeSmith session you shared as `reviewer`
  does the work, with you watching, and answers.

`codesmith bridge init NAME` sets up either one. The rest of this page is
what it does, and how to do the same [by hand](#by-hand).

## Setting up a bridge: `codesmith bridge init`

```sh
codesmith bridge init coder
```

It asks, in the terminal:

1. **How it runs**: headless, or a relay to a session you shared with
   `/bus share NAME` (it asks for that name).
2. **The bridge's folder** (default `bridges/NAME` in CodeSmith's data
   folder, or `--dir DIR`): its `home`, `workspace` and `secrets`, each
   readable only by you. A folder that's already there is used only when
   you say so, and what's in it is kept, so running it again picks up
   where the last one left off.
3. For a headless bridge:
   - **Where the agent works**: a repository, or the bridge's own
     `workspace`. Your home folder and `/` are refused, as `codesmith run`
     refuses them.
   - **A provider**, from the providers the bridge's home knows, with
     untested ones tagged as elsewhere.
     For [Bifrost](configuration.md#bifrost) it first asks for the
     gateway's URL, which goes in the bridge home's `codesmith.json`
     (`providers.bifrost.settings.baseURL`); the virtual key then goes
     in `secrets` as `BIFROST_API_KEY`, and the models offered are the
     ones the gateway lists for that key, each with its own levels.
   - **A sign-in made for the bridge**: a key, pasted (it offers the
     provider's key page) or taken from an environment variable you name,
     and never shown; a Claude Code token for `claude-agent` (it runs
     `claude setup-token`, or takes a pasted one); or the provider's own
     OAuth, saved in the bridge home's auth store. A key or token goes in
     a file of its own under `secrets`, created 0600 and never through a
     symlink, which the bridge's `secrets` read. A key that's already
     there is kept unless you say otherwise. Logins that rest on files in
     your own home (Claude Code's own login, an AWS profile, Google's
     Application Default Credentials, the Azure CLI) aren't offered.
   - **A model**, newest first (type part of a name to narrow the list),
     then **an effort** from that model's own levels, or the provider's
     default. The levels become the bridge's `options.efforts`, so a
     request can name only one the model has.
4. **The owner**: who may send it messages, offered from tailbus's config.

Then it **tries the bridge exactly as tailbus will run it**: the same
program and arguments, in the same folder, with tailbus's cleared
environment (`HOME`, `PATH`, `TMPDIR`, a session database of its own)
and the secrets read from their files, and a test message on stdin. It
shows the answer, or why the run failed, with every secret taken out. A
relay's try sends a message to the shared session, so it asks first and
skips it unless you say yes.

Last, it prints the `[bridges.NAME]` table and asks whether to add it to
tailbus's config (`~/.config/tailbus/config.toml`, or where
`$TAILBUS_HOME` or `$XDG_CONFIG_HOME` puts it), and then whether to run
`tailbus bridge install NAME`. Nothing in that file changes without a
yes. A bridge of the same name that's already there is never changed:
replace it by hand. A config that's a symlink, or that isn't valid TOML,
is left alone too. Without a terminal, `bridge init` writes nothing.

The sign-in picker (`/login`, or `codesmith auth login` in a terminal)
ends with **For a bridge or another home…**, which runs `bridge init`,
or one of the two logins below.

### Signing in for a bridge by hand

```sh
codesmith auth login fireworks --home /Users/you/agent-home/home   # into that home's auth store
codesmith auth login fireworks --to-file ~/.secrets/fireworks      # a key file for `secrets`
codesmith auth login claude-agent --to-file ~/.secrets/claude      # a Claude Code token
```

`--home DIR` saves the login in that home's `auth.json`
(`DIR/.local/share/codesmith`), where a run with `HOME=DIR` finds it, and
reads that home's config, not yours or a project's. The folder must
exist and can't be your own home. `--to-file PATH` writes the key, or for
`claude-agent` a Claude Code token, to `PATH` alone (0600), and prints the
`secrets` line that reads it, such as `FIREWORKS_API_KEY = "PATH"`. It
takes only a key or a token: an OAuth login, or a key that needs other
answers, goes in a home with `--home`. An existing file is replaced only
when you say so at a terminal; a symlink never is. Without a provider,
either flag opens the sign-in picker for that home or file.

## By hand

What `bridge init` sets up, done yourself. For a headless agent it writes
a short `kind = "codesmith"` table, which takes its arguments, environment,
options and usage from tailbus's own defaults for CodeSmith; the full table
below spells out what those amount to.

### A session you have open

1. In the CodeSmith session, share it:

   ```
   /bus share reviewer
   ```

2. Give tailbus a bridge that relays to it, in
   `~/.config/tailbus/config.toml`:

   ```toml
   [bridges.reviewer]
   owner = "you@example.com"
   command = "/opt/homebrew/bin/codesmith"
   args = ["bus", "relay", "--to", "reviewer", "--from", "{SENDER}", "--id", "{ID}"]
   working_directory = "/Users/you/agent-home/workspace"
   home = "/Users/you/agent-home/home"
   path = "/opt/homebrew/bin:/usr/bin:/bin"
   timeout_secs = 3600
   ```

3. Start it: `tailbus bridge install reviewer`.

A message to `reviewer@your-mac` now goes through the relay to the session
on your screen. It shows as **⇄ Request from `crucible@stable-mac`**, runs
under that session's own permission rules, and asks you when a rule asks,
offering allow once or allow for this session (see
[permissions](bus.md#permissions)). Its answer is what the relay prints,
which tailbus sends back.

What the relay does:

- It reaches the bus through your user id, whatever `HOME` the bridge gives
  it, so the bridge's `home` stays separate from yours, as tailbus
  requires.
- `--id {ID}` makes a repeat of the same tailbus message (tailbus delivers
  at least once) get the first answer instead of running again.
- `--from {SENDER}` is the name the session sees the request from.
- `--timeout SECONDS` (default 3600) is how long it waits for the answer.
  Keep the bridge's `timeout_secs` at least as long.
- Progress notes the session sends go to the relay's stderr; tailbus
  bridges don't pass them on.

When the shared session isn't running, the relay refuses by default
("reviewer isn't running"). With `--offline run` it continues that session
in a headless `codesmith run` instead, in its project folder and with the
config, keys and session store of the process that shared it. Nobody is
there to answer a permission prompt then, so anything a rule would ask about
is refused. Opening the session later shows the exchange.

Only sessions you shared can be reached, and `/bus unshare` closes one
again. A share outlives the session's process, so a closed session stays
reachable with `--offline run` until you unshare it.

### A headless agent

`codesmith run` takes the message on stdin and prints its answer, which is
all a bridge needs. These options make it a good headless agent. `--effort`,
`--tools`, `--no-tools`, `--no-mcp` and `--usage-log` are new after 0.4.1,
and `bridge init`, `auth login --home` and `--to-file` after 0.5.1: check
yours with `codesmith --version` and `codesmith run --help`.

| Bridge setting | CodeSmith |
|---|---|
| Sessions kept apart (`{SESSION_DIR}`) | `CODESMITH_DB = "{SESSION_DIR}/codesmith.db"` in the bridge's `env`: each run's sessions in its own database. |
| `session = "per-sender"`, `continue_args` | `["--continue"]`: continue the latest session in that database. |
| Model (`{MODEL}`) | `-m {MODEL}` |
| Effort (`effort_args`) | `["--effort", "{EFFORT}"]` |
| Tools (`tools_args`, `no_tools_args`) | `["--tools", "{TOOLS}"]`, and `["--no-tools", "--no-mcp"]` |
| MCP off (`no_mcp_args`) | `["--no-mcp"]` |
| Usage (`[usage]`) | `--usage-log {SESSION_DIR}/usage.jsonl`: one JSON line per model step and tool call. |

```toml
[bridges.codesmith]
owner = "you@example.com"
command = "/opt/homebrew/bin/codesmith"
args = ["run", "-m", "{MODEL}", "--usage-log", "{SESSION_DIR}/usage.jsonl"]
model = "anthropic/claude-sonnet-5-5"
working_directory = "/Users/you/agent-home/workspace"
home = "/Users/you/agent-home/home"
path = "/opt/homebrew/bin:/usr/bin:/bin"
session = "per-sender"
continue_args = ["--continue"]

[bridges.codesmith.env]
CODESMITH_DB = "{SESSION_DIR}/codesmith.db"

[bridges.codesmith.secrets]
ANTHROPIC_API_KEY = "/Users/you/.secrets/anthropic"

[bridges.codesmith.options]
tools = ["read", "grep", "glob", "shell", "edit", "write"]
tools_args = ["--tools", "{TOOLS}"]
no_tools_args = ["--no-tools", "--no-mcp"]
no_mcp_args = ["--no-mcp"]
efforts = ["low", "medium", "high", "max"]
effort_args = ["--effort", "{EFFORT}"]

[bridges.codesmith.usage]
files = "**/usage.jsonl"
when = { type = "step" }
input = "input"
output = "output"
cache_read = "cache_read"
cache_write = "cache_write"
usd = "usd"
model = "model"
```

`--tools` offers only the tools named and denies every other one, MCP and
plugin tools included, past any rule in the bridge home's config. A run with
nobody at a terminal refuses whatever a rule would ask about, so give the
bridge home's `codesmith.json` the rules the agent needs (`{"permissions":
[...]}`), or run with `--auto` to approve what no rule denies.

The bridge's `home` is CodeSmith's home for these runs: its config, keys and
sessions are the bridge's, not yours.

Give the bridge a `working_directory` that's a repository, or a folder of
its own. A run in your home folder or `/` is refused (nobody is there to
confirm it, and undo would have to snapshot all of it), even though the
bridge's `HOME` is another folder. A run in a folder outside any repository
prints a warning on stderr and goes ahead; `"repo_warning": false` in the
bridge home's `codesmith.json` quiets it. A narrowed run (`--tools` or
`--no-tools`) never joins your [local bus](bus.md).

#### Signing in, the model and the effort

A run under a bridge sees none of your own sign-ins: only `HOME`, `PATH`,
`TMPDIR` and what the bridge's table sets. Give it a provider one of these
ways:

- **A key in `secrets`** (best): `FIREWORKS_API_KEY = "/Users/you/.secrets/fireworks"`
  reads the key from that file (yours, `chmod 600`) for each run and passes
  it as the provider's usual variable (`FIREWORKS_API_KEY`,
  `ANTHROPIC_API_KEY`, `MOONSHOT_API_KEY`, …). A new key needs no restart.
  `codesmith auth login fireworks --to-file /Users/you/.secrets/fireworks`
  writes the file.
- **A key saved in the bridge's home**, once:
  `codesmith auth login fireworks --home /Users/you/agent-home/home`. It
  goes in that home's `auth.json`, apart from yours.
- **Claude Code** (`claude-agent`): a `claude setup-token` token, in a
  file for `secrets` (`codesmith auth login claude-agent --to-file PATH`,
  read as `CLAUDE_CODE_OAUTH_TOKEN`) or in the bridge's home
  (`--home DIR`), and `claude` on the bridge's `path`.

The model is `-m {MODEL}`: the bridge's `model`, or one a request picks from
`options.models` (anything else is refused before the run starts). Without
one, it's `"model"` in the bridge home's `codesmith.json`. The effort is
`--effort {EFFORT}` from `options.efforts`, or fixed as the model's variant
(`provider/model#high`). A level the model doesn't have is refused before
any call; `codesmith debug model PROVIDER/MODEL` lists its levels.

#### Trying it by hand

Run what the bridge will run, as it will run it, before installing the
bridge:

```sh
mkdir -p /tmp/bridge-home
# Not in your home folder: a run there with no terminal is refused.
cd /tmp/bridge-home
# Read the key before HOME changes: on the same line, `~` would already be
# the new HOME.
KEY=$(cat /Users/you/.secrets/fireworks)
echo "say hi" | HOME=/tmp/bridge-home FIREWORKS_API_KEY="$KEY" \
  codesmith run -m fireworks/accounts/fireworks/models/kimi-k3p1-pro --effort high
```

With a CodeSmith from before `--effort`, put the effort in the model
instead, quoted so the shell leaves the `#` alone:
`-m 'fireworks/accounts/fireworks/models/kimi-k3p1-pro#high'`.

It should print the model's answer and nothing else on stdout. An error
about a missing key means the bridge needs one of the sign-ins above; an
unknown model or effort is reported before anything is sent.
