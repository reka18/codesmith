# tailbus

[tailbus](https://github.com/reka18/tailbus) carries messages between AI
agents on your tailnet. A tailbus **bridge** answers an agent's messages
by running a command once per message. CodeSmith can be that command in two
ways:

- [**A relay to a session you have open**](#a-session-you-have-open): a
  Claude Code session sends to `reviewer@your-mac`, and the CodeSmith
  session you shared as `reviewer` does the work, with you watching, and
  answers.
- [**A headless agent**](#a-headless-agent): every message gets a fresh
  `codesmith run`, the way tailbus runs pi or Claude Code.

## A session you have open

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

## A headless agent

`codesmith run` takes the message on stdin and prints its answer, which is
all a bridge needs. These options make it a good headless agent. `--effort`,
`--tools`, `--no-tools`, `--no-mcp` and `--usage-log` are new after 0.4.1:
check yours with `codesmith --version` and `codesmith run --help`.

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

### Signing in, the model and the effort

A run under a bridge sees none of your own sign-ins: only `HOME`, `PATH`,
`TMPDIR` and what the bridge's table sets. Give it a provider one of these
ways:

- **A key in `secrets`** (best): `FIREWORKS_API_KEY = "/Users/you/.secrets/fireworks"`
  reads the key from that file (yours, `chmod 600`) for each run and passes
  it as the provider's usual variable (`FIREWORKS_API_KEY`,
  `ANTHROPIC_API_KEY`, `MOONSHOT_API_KEY`, …). A new key needs no restart.
- **A key saved in the bridge's home**, once:
  `HOME=/Users/you/agent-home/home codesmith auth login fireworks`. It goes
  in that home's `auth.json`, apart from yours.
- **Claude Code** (`claude-agent`): save a `claude setup-token` token in the
  bridge's home the same way (`codesmith auth login claude-agent`), and put
  `claude` on the bridge's `path`.

The model is `-m {MODEL}`: the bridge's `model`, or one a request picks from
`options.models` (anything else is refused before the run starts). Without
one, it's `"model"` in the bridge home's `codesmith.json`. The effort is
`--effort {EFFORT}` from `options.efforts`, or fixed as the model's variant
(`provider/model#high`). A level the model doesn't have is refused before
any call; `codesmith debug model PROVIDER/MODEL` lists its levels.

### Trying it by hand

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
