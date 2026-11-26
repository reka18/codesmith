# Changelog

All notable changes to CodeSmith are listed here. Versions follow
[Semantic Versioning](https://semver.org/).

## Unreleased

- **A Bifrost model is refused when the gateway can't list the key's
  models and none is saved yet**, with an error naming the gateway's URL,
  instead of running unchecked.
- **Bifrost's Responses and Gemini APIs** are reached with a model's
  `protocol` setting (`<base>/v1/responses`,
  `<base>/genai/v1beta/models/…`), with the gateway's request id and
  routed model on Gemini streams too, and Gemini's effort levels sent as
  `thinkingConfig`.
- **Bifrost is a built-in provider.** `bifrost` reaches models through a
  [Bifrost](https://github.com/maximhq/bifrost) gateway with a virtual
  key (`BIFROST_BASE_URL` and `BIFROST_API_KEY`, or `settings.baseURL`
  and `settings.apiKey`), sent in `x-bf-vk`. Model ids are Bifrost's
  (`bifrost/openai/<model>`), passed through unchanged. An id starting
  with `anthropic/` goes to Bifrost's Anthropic Messages route; every
  other one to its unified Chat API. The model list is the one the
  gateway gives your key, fetched afresh as each session starts and kept
  under a hash of the key for when the gateway can't be reached; a model
  outside it is refused. Each model offers the effort levels it has
  reached directly. Every request carries a fresh `x-request-id`, and
  `codesmith bridge init` offers Bifrost. See `docs/configuration.md`.
- **A model's `protocol` setting** picks the protocol it speaks, in place
  of its provider's.
- **`--usage-log` step lines gain `usd_estimated`**, CodeSmith's estimate
  from the token counts and the model's base rates (its configured
  `cost`, then a Bifrost gateway's prices, then the catalog), and on
  Bifrost `request_id` and `served_model`. `usd` is now only a cost the
  provider reported billing, above 0: a context-tier price CodeSmith
  worked out no longer appears as `usd`. A run on Bifrost ends with a
  `type: "run"` line, `usd_quota`, what the key's budget moved by.
- **`codesmith bridge init NAME` sets up a tailbus bridge.** It asks in
  the terminal: headless (`codesmith run` for each message) or a relay to
  a session shared with `/bus share`; for a headless bridge, where it
  works, a provider (untested ones tagged), a sign-in made for the bridge
  alone (a key, pasted from the key page it offers or taken from an
  environment variable you name; a Claude Code token for `claude-agent`;
  or the provider's OAuth), a model, and an effort from that model's own
  levels. It writes the bridge's folder and a 0600 key file for its
  `secrets`, tries the bridge exactly as tailbus runs it, and prints the
  `[bridges.NAME]` table. It adds the table to tailbus's config and runs
  `tailbus bridge install` only after a yes, never changes a bridge
  that's already there, and never prints a key. See `docs/tailbus.md`.
- **`codesmith auth login PROVIDER --home DIR`** saves a login in another
  CodeSmith home's auth store, such as a bridge's, reading that home's
  config rather than yours or a project's. **`--to-file PATH`** writes
  the key, or a Claude Code token, to a 0600 file of its own for a
  bridge's `secrets`, and prints the line that reads it. A symlink there
  is refused, and an existing file is replaced only after a yes.
- **The sign-in picker ends with "For a bridge or another home…"**, in
  `/login` and in `codesmith auth login`: it leads to `bridge init`,
  `--home` or `--to-file`.
- `/bus share NAME` points at `codesmith bridge init`, and a bridge's run
  with no key now suggests `codesmith auth login PROVIDER --home DIR`
  instead of a `HOME=` prefix.

## 0.5.1 — 2026-11-25

0.5.0 was tagged but never released; its changes ship here.

- **A queued Claude Code request starts when a process frees up.** At the
  process cap, a request could miss the moment another session finished and
  wait until it timed out.
- **Stopping a command on Linux signals only that command.** CodeSmith ran
  `/bin/kill -TERM -<group>`, which procps's `kill` misreads. On Ubuntu
  24.04 and Debian 12 it keeps only the group's first digit, so the command
  ignored the stop, and a group starting with 1 signalled every process you
  own. On Ubuntu 22.04 it signalled CodeSmith's own process group, so a stop
  could end CodeSmith itself. CodeSmith now signals the group itself.
- **The local bus: sessions on one machine work together.** Every
  CodeSmith session you have open joins a bus that only you can reach: no
  network listener, nothing hosted. A session's model can list the others
  (`bus_peers`), send one a request (`bus_send`), and wait for the answer
  (`bus_wait`). A request starts a turn in the receiving session, under that
  session's own permission rules; a prompt it raises says who asked and
  offers allow once or allow for this session, never "always". `/bus` shows
  and names the session, `codesmith bus peers` and `inbox` look from the
  shell. A small broker starts with the first session and exits a minute
  after the last. See `docs/bus.md`.
- **tailbus can reach a CodeSmith session you have open.** `/bus share
  reviewer`, plus a tailbus bridge that runs `codesmith bus relay --to
  reviewer`, lets a Claude Code session (or any tailbus agent) send that
  session work and get its answer back. With `--offline run`, a closed
  session is continued headless. See `docs/tailbus.md`.
- **`codesmith run` works as a headless agent for other programs.** New
  flags: `--effort LEVEL`; `--tools LIST` and `--no-tools`, which offer only
  the named built-in tools and deny every other one, MCP and plugin tools
  included, past any configured rule; `--no-mcp`; and `--usage-log PATH`,
  one JSON line per model step and tool call while the run goes. The
  message can come on stdin alone, as before. These are what a tailbus
  bridge needs to run CodeSmith the way it runs pi or Claude Code.
- **A run with no API key stops at once and says how to give one.**
  Instead of a quiet wait and then the provider's 401, `codesmith run`
  names the missing variable and `codesmith auth login`, before anything
  is sent.
- **Starting outside a repository warns first.** The UI and `codesmith
  run` ask `Start here anyway?` at a terminal when the folder isn't in a
  repository, and remember a yes for that folder. Your home folder and `/`
  are asked about every time, and with no terminal (a tailbus bridge, a
  script) they're refused instead of starting. `--any-folder` skips the
  question; `"repo_warning": false` in your own config quiets it for other
  folders.
- **Fixed: a run with its own `HOME` could snapshot your whole home
  folder.** Undo never snapshots the home folder, but CodeSmith found it
  from `$HOME`, so a run started in your home with `HOME` set elsewhere (as
  a tailbus bridge does) copied all of it into a snapshot first, and seemed
  to hang. The home folder is now also your account's, from the user
  database.
- **The README leads with what keeps you safe**, and describes CodeSmith
  on its own terms.
- **Fixed: commands ignored the signal that stops them when CodeSmith was
  started with it ignored.** Some launchers and CI runners start programs
  with SIGTERM ignored, and every command CodeSmith ran inherited that, so
  stopping one waited out the grace period and killed it. Commands and
  terminals now start with SIGTERM, SIGINT and SIGQUIT at their defaults
  (SIGHUP is left alone, so `nohup` still works).
- **Fixed: `codesmith upgrade` could fail on Linux with "Text file busy"**
  when another thread started a process just as the new binary was written.
  It now tries again for up to a second.
- **The input box's title says what tab does and stands out.** "tab
  agents" is now "tab → Plan" (or "→ Build"), and the title is drawn in the
  text colour, apart from the placeholder and the usage line.
- **Homebrew, the built-in `codesmith` skill and the crate describe
  CodeSmith on its own terms.**
- **CI runs on GitHub Actions** for every pull request and push to `main`,
  on Linux (x86_64 and arm64) and macOS. Releases are still built and
  signed locally.

## 0.4.1 — 2026-11-23

- **Fixed: `/login`'s *Open the API key page* for Fireworks opened a page
  that no longer exists.** It now opens
  `https://app.fireworks.ai/settings/users/api-keys`. Anthropic's entry
  points to its new console address (`platform.claude.com`), and Poe's to
  `poe.com/api/keys`.
- **Releases check every outside link first.** `codesmith debug links`
  lists each page CodeSmith shows or opens (API key pages, setup and help
  pages), and `--check` requests them: a page that is gone stops a release,
  and one that moved permanently prints its new address. A test keeps the
  list in step with the code. The web fetch tool's user agent now names
  the project's current address.

## 0.4.0 — 2026-11-23

- **Untested providers are marked.** Only Anthropic, Claude Code
  (`claude-agent`) and Fireworks have been tested with CodeSmith. Every
  other provider, custom and local ones too, is tagged `untested` in
  `/provider`, `/login`, `codesmith auth login` and `codesmith auth list`,
  and CodeSmith warns the first time a run uses one. The warning asks
  anyone it works for to confirm it in an issue (a new *Provider verified*
  template), and anyone it doesn't to send a fix; see the new
  `CONTRIBUTING.md`.
- **Claude Code tokens: Claude Code works over SSH with no password.**
  `/login`, then Claude Pro/Max, then *Claude Code token*: CodeSmith runs
  `claude setup-token`, saves the token it prints (owner-only, never shown)
  and hands it to Claude Code, so it no longer needs its keychain login,
  which an SSH session can't open. The token lasts a year and can only
  send requests; CodeSmith warns before it expires, and Sign out removes
  it. `codesmith auth login claude-agent --token` does the same from the
  command line. Without a token, Claude Code's "Not logged in" over SSH now
  says the keychain is locked and points to the token.
- **Agents can't read the keychain or Claude Code's login unasked.** Shell
  commands that print keychain secrets (`security find-generic-password`
  and the like) and reads of `~/.claude/.credentials.json` now ask, like
  other secret files, and "always" doesn't lift it.
- **`/effort` picks the reasoning effort.** It lists the current model's
  effort levels with the provider's default first, and `/effort high` sets
  one directly. It is offered only for models that have levels, and the
  levels come from the provider's API where it lists them: Anthropic's
  `capabilities.effort` and OpenRouter's `reasoning.supported_efforts`
  replace the catalog's guesses, so a model gets exactly the levels it
  accepts. A model the API says doesn't reason (Anthropic, OpenRouter,
  Gemini) offers none.
- **Installs end with a forge banner.** `install.sh`, Homebrew (as the
  formula's caveats) and `codesmith upgrade` finish with the same
  "CodeSmith forged successfully" box: CODE in fire colours, SMITH in
  mithril. Any failure of `install.sh` or `codesmith upgrade` ends with
  "CodeSmith forge failed" instead, in cooled embers and ash, after the
  error. Both are plain under `NO_COLOR` or off a terminal, and `install.sh`
  waits for Enter only when run from a terminal (not under `CI` or
  `CODESMITH_NO_PROMPT`); an interrupt shows no banner.

## 0.3.0 — 2026-11-22

- **`/provider` switches provider; `/model` picks within it.** The
  provider picker lists every provider as `ready`, `local` or `sign in`;
  choosing a ready one switches to the model you used there last, else its
  newest, and choosing one that needs a key opens the sign-in picker on
  it. The model picker now lists only the current provider's models,
  newest first, with a row to switch provider.
- **Model lists come from each provider's API.** Anthropic, OpenAI,
  Gemini, OpenRouter, Groq, DeepSeek, Mistral, xAI, Together, Fireworks
  and Cerebras are asked for the models your key can use when a picker
  opens and the kept list is over a day old (never while idle), so new
  models appear without a CodeSmith update and can be run. Claude Code's
  list refreshes the same way. Lists are kept per key fingerprint, never
  the key; a failed request keeps the previous list.

## 0.2.0 — 2026-11-22

- **The busy indicator is a forge.** While idle, the input box title shows
  a cold steel hammer. When a turn starts, the hammer heats over 1.5 s
  through dark blue and purple to crimson, orange and white, then becomes
  an ember that flares and cools (`· • ✦ ✶ ✷ ✸`) along steel's heat
  colours, while a hot band runs along a rotating smithing verb
  (Forging, Tempering, Quenching, …). Light themes use a darker ramp;
  `NO_COLOR` keeps the motion without the colour. A busy tab shows `✶`.
- **Model specs are forgiving, and checked before anything is sent.**
  Spaces around the provider, model and variant are dropped
  (`anthropic/ opus` is `anthropic/opus`), providers and models are found
  ignoring case and `-`/`.`/`_` differences, and a partial name means the
  newest model containing it (`opus` is the newest `claude-*opus*`; a name
  alone is looked for among every runnable model). A spec naming no known
  model, or one that matches several equally new ones, is refused with the
  closest matches instead of being sent; providers without a model list
  and servers on this machine still take any model.
- **A signed-in Claude Code counts as available**, so a fresh machine
  defaults to its newest model rather than a local server that may not be
  running.
- **Signing in selects the provider.** After `/login` or
  `codesmith auth login`, when no model was chosen (by `-m`, `/model`, the
  agent or codesmith.json), the session switches to the provider's newest
  model and it is saved as `model` in the global codesmith.json (comments
  kept). A model a login chose may be changed by a later login; one you
  set is never overridden.
- **Claude without an Anthropic key runs through Claude Code.**
  `anthropic/<model>` with no Anthropic key while Claude Code is signed in
  runs the same model as `claude-agent/<model>`, or, without one exact
  equivalent, is refused with the one-line fix.
- **Provider errors lead with the fix.** A missing or rejected key names
  the login command (and, for Anthropic with Claude Code signed in, the
  `claude-agent/…` model to switch to); a local server that refuses the
  connection is named with its URL. The provider's own error follows.

## 0.1.0 — 2026-11-21

First public release.

- **Claude Agent processes are capped, and can start ahead of a turn.**
  The global `claude_agent` setting (`max_processes`, `prewarm`, `spare`;
  ignored in project config) bounds every `claude` CodeSmith runs, 4 by
  default where it was 32: at the cap a spare, then the least recently
  used idle session's process stops (it resumes next time); a running or
  parked turn is never stopped, and a request over the cap waits, failing
  after two minutes. `prewarm` (off by default) starts a session's own
  process as its turn begins; `spare` (0 by default) keeps never-used
  processes, each started like the session that made room for it, for new
  sessions launched the same way. A process still serves one session for
  good once it carried a turn, and each session keeps its own Claude
  session, history control, model and effort. A process's slot is now
  given back only once it has exited. See
  [Claude Code processes](docs/configuration.md#claude-code-processes-claude_agent).
- **One place to sign in:** `/login` in the UI, and `codesmith auth login`
  with no provider in a terminal, list every provider with where its login
  stands and run the right login for it: the browser OAuth login (with a
  paste box for the URL the browser ends up on, for SSH or another
  machine), a device code, a masked API key (offering the provider's key
  page where it is well known), an AWS profile, a Vertex project, the
  Azure CLI, or Claude Pro/Max through the existing Claude Code login
  check, plus signing out of a saved login. A login with no client id
  configured says where to register an OAuth app and its redirect URL,
  then asks for the client id; none is built in. Keys and pasted URLs are
  never drawn or sent to the model. Without a terminal, `auth login`
  prints the providers and how to sign in to each, and fails without
  asking. See [signing in](docs/getting-started.md#signing-in).
- **A key read without echo is no longer echoed when typed the moment the
  prompt appears:** `auth login` turns echo off before showing the prompt.
- **A trailing dot in a URL host no longer dodges a deny.** `https://host./` resolves to the same server as `https://host/` but parsed as a distinct host, so a `webfetch`/`fetch`/OpenAPI deny on the host could be bypassed; the dot is now stripped before allow/deny matching.
- **Browser attachments** (OpenCode's browser plugin): a browser attached
  to a session grants it `browser *` and the `browser` skill while the
  attachment lasts. Replacing, dropping, idling out (30 minutes) or
  disposing it revokes them, and a replaced attachment's revoke never
  takes its replacement's. The rules never lift a configured, built-in or
  policy deny or a secret guard, and reach no other session. No browser
  driver is built in; a custom build plugs one in. See
  [permissions](docs/permissions.md#browser-attachments).
- **A browser opens a workspace file only after the read tool's checks:**
  `open_file` resolves the path as `read` does and asks `read` under its
  project-relative, as-named and real names, so `secrets/./key` or
  `secrets//key` meet a `secrets/*` deny, a symlink out of the workspace
  is refused, names with `%`, `#` or `?` are refused, and the browser is
  handed the percent-encoded file:// URL of the real path.
- **A file:// URL a browser opens is checked as a file read:** `open`
  turns it into a path and checks it as `read` does, so a URL to `~/.ssh`
  asks for the outside folder and meets the secret guard, a `.env` asks,
  and `file:///w/../../etc/passwd` is checked as `/etc/passwd`, where it
  used to need only `browser`, which an attachment grants. `open` now
  takes the project root.
- **`browser` rules see a URL as it would be requested:** `open` parses
  an http(s) URL as webfetch does (scheme and host lowercased, default
  port dropped, `.` segments resolved) and drops trailing dots from the
  host, so `HTTPS://EVIL.test.:443/x` meets a `browser
  https://evil.test/*` deny, and its written and percent-decoded forms
  must pass the denies too. A URL with credentials is refused, as are
  schemes other than http, https and file. The browser is handed the URL
  the rules saw.
- **Documented: a browser attachment answers a `browser` ask** as an
  "always" approval would, for the attached session only; write `deny`
  to forbid a URL even with a browser attached.
- **A grant source can grant only `browser` and `skill`:** the gate keeps
  a granted rule only when its action is one of those, named literally,
  so a source that hands it `* * allow` or `webfetch * allow` grants
  nothing.
- **GitLab Duo is not supported, by decision.** It is a GitLab-subscription
  backend reached through GitLab's unpublished Duo workflow and
  agentic-chat SDK protocols; porting it would mean reimplementing a
  proprietary protocol with no spec, for a narrow audience. Open an issue
  to request it.
- **`codesmith upgrade` never downgrades unasked:** with no version named,
  it refuses when the latest release is older than the running copy, and
  the update notice stays quiet. Name the version to roll back.
- **Bidi and zero-width characters are stripped** from swap progress
  lines, swap errors and exported Markdown transcripts, along with the
  other Unicode format characters, so text can't read backwards or hide
  characters.
- **MCP OAuth ignores forged callbacks:** a browser callback whose `state`
  doesn't match the login is answered 400 and the login keeps waiting,
  instead of failing, so a stray or forged request can't cancel it.
- **Swap names can't look like remote paths:** a model name with both `@`
  and `:`, a `:` followed by `/`, or a one-letter head before `:` (`x:y`)
  is refused. Tags like `qwen:7b` still work.
- **Starting a model swap from the shell asks:** every agent asks before
  its shell runs `codesmith models swap`, even with a broad `shell` allow.
  The swap docs now say plainly that this and the session check are not a
  security boundary, and show `shell` deny rules for the swap commands.
- **A swap selects only a listed model:** when a swap's `use` command
  reports a `model_id` that is neither the requested model nor one `list`
  offered, CodeSmith keeps the requested model and shows a warning.
- **Model swap output is bounded:** `swap` commands' output is read in
  chunks; a line is kept up to 2000 characters (the rest, up to its
  newline, is dropped), each stream up to 8 MiB, and a `list` that prints
  more than 1 MiB is refused instead of filling memory. A JSON listing on
  one long line now parses: it is read from the whole output, not from a
  cut line.
- **`CODESMITH_DISABLE_PROJECT_CONFIG` covers nested instructions:** with
  project config off, reading a file no longer brings in the `AGENTS.md`
  or `CLAUDE.md` files of the folders around it.
- **Release signing checks the draft against the build:** `release.sh`
  writes `BUILT-SHA256SUMS`, a manifest of the tarballs it built (and of
  each macOS tarball's files), uploads it with the draft and prints its
  SHA-256. `sign-macos.sh` and `sign-release.sh` take that hash
  (`CODESMITH_BUILT_SUMS_SHA256`) and refuse a draft whose tarballs don't
  match the manifest. `sign-macos.sh` extracts only the binary, and refuses
  tarballs with links or members outside their folder; `sign-release.sh`
  also requires the macOS binaries' `TeamIdentifier` to be
  `CODESMITH_TEAM_ID`. See [RELEASING.md](RELEASING.md).
- **Secret files ask, for every agent:** reading `.env` files,
  `.git-credentials`, `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.minisign`,
  `~/.netrc`, the GitHub CLI's `hosts.yml`, Docker and Kubernetes
  credentials, `~/.npmrc`, `~/.pypirc`, and CodeSmith's own `auth.json`,
  `mcp-auth.json` and global config file now asks, under `plan` too (which
  still reads other outside files without asking, as in OpenCode). A
  blanket allow, a saved approval or "always" no longer lifts these asks;
  only a configured allow naming the path does. `grep`, `glob`, folder
  listings, Code Mode's built-ins and shell commands that name such a file
  (`cat ~/.ssh/id_ed25519`) ask too. See
  [Permissions](docs/permissions.md#secret-files).
- **`/automation [request]`**, as in OpenCode: hands the request to the
  model with how to manage automations through the `automation` tool. It
  is only a prompt, so creating or changing one still asks (or follows the
  rules) as the tool does.
- **The tools an automation's program calls** are read from it when it is
  created, as in OpenCode: `automation add --code-file` lists them and
  refuses a program whose calls can't be read (a computed `tools[name]`,
  an alias of `tools`), that calls an unknown tool, `shell` or
  `automation`, or whose tools (or running it at all) would be denied or
  ask in an unattended run. Its runs are offered only those tools, checked
  again before each run.
- **Fixed:** an event watch whose run failed was paused twice, by the run
  and again by the watch once its stream closed, so resuming it in between
  could be undone. It is paused once, after the watch has stopped.
- **Automation expiry:** `codesmith automation add --expires "in 8 hours"`
  (and the tool's `expires`) stops an automation at a time, as in OpenCode.
  No slot after it runs, including one that only comes up after downtime,
  a restart or a backoff; resuming an expired one finishes it; an event
  watch ends at its expiry. `automation list` shows it.
- **Automation backoff:** a prompt whose handoff fails is retried five
  minutes later, as in OpenCode, rather than a minute later (a program's
  still waits a minute). A run handed off a minute or more after its slot,
  such as that retry, is logged as `late`.
- **Commands no longer see your credentials:** shell tool commands (local
  and in the shell daemon), background jobs, `!cmd`, command-template
  interpolation, MCP stdio servers, language servers, formatters and the
  worktree start command lose provider keys, configured key variables,
  `{env:}` credentials and anything named like a key, token, secret or
  password. Pass one through on purpose with `shell.env.pass` in the
  global config file; a project config cannot. OpenCode passes the whole
  environment.
- **Hardened host git:** every git command CodeSmith runs for itself goes
  through one builder: no hooks, fsmonitor, pager, external diff or
  textconv, and a repository's own program-running settings (filters,
  credential helpers, `core.sshCommand`, ...) are replaced by yours or
  turned off. Inherited `GIT_DIR`, `GIT_INDEX_FILE` and the other
  repository variables are cleared. A test fails on any new raw
  `git` call.
- **Project config trust:** a project's `codesmith.json[c]` or
  `.codesmith/codesmith.json[c]` that uses `{env:}` or `{file:}`, sets a
  provider URL, header, credential or host setting, an MCP server's URL,
  headers or command, an OpenAPI API, a web search or catalog URL, a
  program (`shell` path, `worktree.start`, a language server's or
  formatter's `command`), or reads files outside the project is asked
  about once per project, at a terminal. The answer is kept per project
  with what it covered (owner-only `trust.json` beside the session
  database) and asked again only when the config asks for more. Until a
  project is trusted (and always without a terminal, in `acp`, `serve`
  and automations) its `{env:}` and `{file:}` are empty and the rest is
  ignored in favour of the global config, with a one-line notice. A project
  that only picks models, agents or rules is never asked. `codesmith trust`
  trusts, distrusts, shows, lists and revokes. OpenCode trusts project
  config as written.
- **MCP Code Mode defaults for known servers:** without `codemode` set,
  Executor's tools are offered directly (it is a Code Mode server itself),
  and PostHog is asked for its plain tools (`x-posthog-mcp-mode: tools`,
  unless the URL or a header picks a mode), as OpenCode does.
- **`list_mcp_events` tool:** while a connected server offers MCP Events,
  the model can list the events one server pushes, to use as an
  automation's trigger. It asks `list_mcp_events` for the server, is denied
  wherever `watch_mcp_event` is denied on it, and is not offered where
  watching events is denied everywhere (unattended runs).
- **MCP events over HTTP:** an `events/stream` request (and the other
  events requests) to a Streamable HTTP server names its event in an
  `Mcp-Name` header, base64-encoded when it is not plain ASCII.
- **MCP resource blobs:** binary content in MCP tool results and resource
  reads that is not an image or PDF is no longer only noted: text blobs are
  decoded, and other blobs (and audio) are saved to an owner-only file in
  the session's tool-output folder, under a name CodeSmith picks, with the
  result naming the file.
- **MCP session recovery:** a remote server that forgot its session (it
  answers 404 to a request carrying the `Mcp-Session-Id`) is initialized
  again and the tool call or resource read is sent once more.
- **MCP output schemas:** a tool's `outputSchema` is kept (across paginated
  tool lists) and shown as its result type in the Code Mode catalog, and a
  program calling it gets the call's `structuredContent` as its value.
- **MCP OAuth login options:** `codesmith mcp auth` takes a redirect URL
  pasted into the terminal (the browser on another machine); one that is not
  the login's redirect address or carries another state is refused and the
  login keeps waiting. New `oauth` settings: `redirectUri` (a fixed redirect,
  registered dynamically), `authServerMetadataUrl` (metadata instead of
  discovery, also used to refresh) and `clientMetadataUrl` (a client ID
  metadata document you host, used with servers that accept them from public
  clients). The login asks for `offline_access` where offered, checks the
  redirect's `iss` against the issuer (RFC 9207), registers and redeems the
  `localhost` spelling of the loopback redirect, and its browser page reports
  a token exchange the server refused. A connection whose saved token is
  refused refreshes it once and retries; a `403` asking for more scope shows
  the server as needing a login, and the next login asks for that scope.
  OAuth requests go only to `https` (or loopback `http`) URLs, follow no
  redirects, read at most 1 MiB, and refuse metadata naming another issuer;
  server-sent error text is shown without control characters.
- **Automation runs survive restarts:** a run is saved as it goes (claimed,
  a Code Mode program's result, the prompt handed to its session, then the
  outcome), and the process holding the scheduler lease next picks up one
  that was left in the middle. A program is never run twice: one that was
  interrupted fails and counts as a strike, and a saved result is handed
  off. A prompt that was handed off but not answered is continued once.
  Resumed turns are set up like first attempts: unattended, every question
  refused, held to the directory's rules and the creating session's
  recorded rules. Retries reuse the run's session and prompt, which now
  have fixed ids.
- **Automation failures:** three failed Code Mode runs in a row pause the
  automation, with the reason, and a handoff that can't be written is
  retried a minute later (three failures pause it). An invalid target or a
  policy that refuses the run pauses it at once, with nothing committed. A
  program's `send.to.subagent` is checked against the creating session's
  policy as OpenCode does: unknown, primary-only and hidden agents, the
  subagent depth limit, `subagent` rules that deny or would ask, and agents
  an automation created there couldn't run as are refused. A run's detail,
  error and pause reason are kept up to 32 KiB.
- **At-least-once MCP events:** an event watch's cursor now moves when the
  run's prompt is handed off, not when the run starts, as in OpenCode. An
  event whose handoff is refused, or whose process stops first, is replayed;
  replays are skipped by event id. Pausing a watch in the middle of a run
  now lets that run finish.
- `session::Options` takes an optional fixed `id`; a stored session with it
  is adopted.

- **Export dialog:** `/export` (`session.export`, `<leader>x`, which did
  nothing before) exports the session as OpenCode's dialog does: a Markdown
  transcript (thinking and tools optional) or the JSON, raw or sanitized,
  copied through the terminal or saved to a file you name. A saved file is
  created 0600 and never replaces one without a `y`.
  `codesmith session export --sanitize` writes the sanitized JSON (every
  text, tool input and output, path and title replaced with a marker; it
  still imports). Every export, raw ones included, has the credentials
  CodeSmith holds replaced with `[redacted:credential]`.
- **ChatGPT and OpenAI requests:** the `openai` provider (a key or a
  ChatGPT sign-in) is sent no output limit on session steps and compaction
  summaries, as OpenCode sends none; the ChatGPT backend rejected one. A
  ChatGPT sign-in turns `effortUpdates` off, so a mid-session effort switch
  sends the new effort for the whole request instead of in-place updates
  its backend does not take. WebSocket selection stays an explicit
  `transport` policy (the built-in providers' defaults), with tests pinning
  it. No client id is built in: it stays yours or issued by the
  registration opt-in.
- **OpenAI verbosity defaults:** known OpenAI Responses models (GPT-5,
  5.1 to 5.6, mini, nano and GPT-6, on OpenAI, Azure, Bedrock Mantle and
  Cloudflare's OpenAI passthrough) are sent `text.verbosity: "low"` unless a
  `textVerbosity` is configured (`null` sends none), as OpenCode's
  verbosity plugin does.
- **Session diff ranges:** `codesmith session diff --from N [--to M]` shows
  what the turns of prompts `N` through `M` changed (start snapshot of the
  first against the last recorded end), as OpenCode's session diff does by
  busy period. `codesmith_core::session::diff` has the range for other
  callers; a turn still running can be compared against the working copy.
- **Code Mode rejection warnings:** a promise a Code Mode program rejects
  and never awaits or handles is reported in the result under `Warnings:`
  (`Unhandled rejection from an un-awaited promise: Error: boom`), as
  OpenCode reports it, instead of being dropped. The program still
  succeeds; at most ten warnings are listed, each bounded.
- **Default model fallback:** with no model chosen (no `-m`, agent model or
  `"model"`), a session runs the newest available chat model that takes and
  makes text, as OpenCode does, instead of refusing to start: the newest by
  catalog release date among configured providers, providers with a key and
  local servers. A configured `model` is still used as given.
- **`disabled_providers`:** OpenCode's config key. A disabled provider's
  models are not listed or picked, a model naming one is refused, and a
  default `model` on one is passed over for the newest available model.
  Every config file's list counts.
- **Plugin tool renaming:** a plugin's `tool_before` hook can send a call
  to another tool by setting `call.name`, as OpenCode's can. The call runs
  as the new tool, its input checked against that tool's schema and through
  that tool's own permission checks; a name that is not offered that step
  (denied outright, removed by a `context` hook, unknown) runs nothing and
  the model is told `Tool is not available for this request: NAME`. Inside a
  Code Mode program the new tool must be one of the program's own built-ins
  or MCP tools. MCP tools called from a Code Mode program now get the
  per-call hooks too, as built-ins there already did.
- **Signed releases:** the CodeSmith minisign release key
  (`RWTFKe6qhvSs57s1pw5x6aJRK0jdZr42tg1NgNp3a9rAJ3HersFsRoMV`) is pinned in
  `codesmith upgrade` and `install.sh`, so from this release on an upgrade
  refuses a release whose `SHA256SUMS.minisig` is missing or doesn't
  verify. Builds from before this upgrade into it on checksums once.
  Signing is two steps on two Macs: `scripts/sign-macos.sh` signs and
  notarizes the macOS binaries and no longer runs minisign, then
  `scripts/sign-release.sh TAG` checks the draft's final `SHA256SUMS`
  against its tarballs, signs it and uploads the signature (RELEASING.md).
- **Shell commands without a terminal:** commands the `shell` tool and
  `!command` run start a session of their own (as OpenCode's do), with no
  controlling terminal, so a prompt that opens `/dev/tty` (git or ssh
  asking for a password, `sudo`, `read < /dev/tty`) fails at once instead
  of stopping the command as a background job of the UI's terminal or
  drawing over the UI. Stopping a command still stops everything it
  started. Unless already set, `GIT_TERMINAL_PROMPT=0`,
  `SSH_ASKPASS_REQUIRE=never`, `GCM_INTERACTIVE=never` and
  `DEBIAN_FRONTEND=noninteractive` are set for those commands.
- **ChatGPT agent registration opt-in:** `codesmith auth login openai
  --oauth --register`, or `settings.register: true` on the `openai`
  provider, signs in with OpenAI's agent registration as OpenCode does
  (`dynamic_agent_client`, agent name CodeSmith), so OpenAI issues you a
  client of your own. The issued client is saved with the login and used
  to refresh it. OpenAI may reject the registration; the sign-in says so,
  and a refused one says what to use instead. Nothing is registered
  without the opt-in: `dynamic_agent_client` given as a client id (flag,
  `settings.clientId` or `CODESMITH_CHATGPT_CLIENT_ID`) no longer registers
  and is refused, naming the two opt-ins.
- **Plan agent outside the project:** the `plan` agent reaches folders
  outside the project (reads, searches, listings) without an
  `external_directory` prompt, as OpenCode's plan agent does. It still
  writes only plan files in the Plan directory. A configured
  `external_directory` deny or ask, and policies, still win.
- **Model swap:** a provider can have a `swap` setting: a `list` and a `use` command that
  list the models its server can load and load one, for servers that hold
  one model at a time (such as a two-Mac oMLX cluster run by tblink). The
  model picker lists those models and loads one when chosen, with a
  progress view, selecting it once it is loaded; `codesmith models
  PROVIDER` lists them and `codesmith models swap PROVIDER MODEL` loads one
  from a terminal. The commands run without a shell, take only a name the
  server listed, are read only from the global config file, and only a
  person can start a swap: it is not a tool, and `run`, automations,
  `serve`, ACP and CodeSmith's own shells cannot.
- Git's and ssh's passphrase and password prompts stay unrelayed, unlike
  OpenCode's askpass helper: the answer could reach the model, which runs
  the commands that ask. The troubleshooting guide says how to avoid the
  prompts (`ssh-agent`, a git credential helper).
- The transcript's activity summary counts the tool calls a Code Mode
  `execute` program finished (`+ 2 reads, 1 tool`) instead of the
  `execute` call itself, as OpenCode does, and shows the instruction files
  a read brought in: adjacent loads are one `◈ Instructions: 2 files` row
  (counted as `2 instructions` at the `low` verbosity), live and in a
  continued session.
- Code Mode's `console` shows values as OpenCode's Code Mode does instead
  of as JSON: getters, `toJSON` and proxy traps are no longer run to log a
  value (an accessor shows as `[Getter]`), a `Map`, `Set` or error shows its
  contents (`Map(1) [["a",1]]`, `Error: boom`), class instances carry
  their class name, and cycles and deep nesting are marked. `console.table`,
  `console.dir` and `console.debug` are added, and `warn`, `error` and
  `debug` lines start with `[warn]`, `[error]` and `[debug]`. A logged
  line is cut at 100,000 characters.
- The check that keeps a public host from redirecting `webfetch`, Code
  Mode `fetch`, OpenAPI calls and image input downloads to a local or
  private address now covers the connection itself: the target's name was
  looked up for the check and again to connect, so a name that answered
  with a public address and then a private one (DNS rebinding) got
  through. A redirect hop from a public host now resolves the name once,
  refuses it unless every address is public, and connects to those.
- jdtls's workspace data moved from a fixed folder in the shared temporary
  directory to `~/.cache/codesmith/jdtls/`, created owner-only (a symlink
  or another user's folder there is refused), and the external editor's
  prompt file is in a new owner-only folder with a random name: another
  local user could create or link the old paths first.
- A Code Mode automation's MCP tool calls are held to the creating
  session's own permission rules, as its prompt runs are: a deny set in
  that session was not applied to the program's calls.
- An automation keeps the creating session's own permission rules with it,
  recorded when it is created (and, for one created earlier, at its next
  run while that session exists), and every run is held to them. Once that
  session is deleted, runs go on in a session of their own with those
  rules, rather than unparented without them. Only an automation whose
  rules were never recorded and whose creator is gone is refused, with an
  error saying so, and paused.
- **Signed releases:** each release's `SHA256SUMS` is signed with minisign
  on the signing Mac (`SHA256SUMS.minisig`, made by the new
  `scripts/sign-release.sh`, which `scripts/sign-macos.sh` runs).
  `codesmith upgrade` verifies the signature, and its trusted comment naming
  the version asked for, against the public keys compiled in from
  `release_key.pub`, before it uses any checksum; a missing or bad signature,
  or one for another version, refuses the upgrade, with no override.
  `install.sh` checks the same signature when `minisign` is installed.
  Until the first release key is generated, the key file is a placeholder
  and upgrades check `SHA256SUMS` only, with a warning that releases aren't
  signed yet; `scripts/release.sh --draft-upload` refuses to upload a
  release then. Upgrades trusted `SHA256SUMS` from the same release, so
  anyone able to replace a release's files could ship a bad binary with a
  matching checksum.
- A project root of `/` or the home folder no longer puts every path inside
  the project: the working directory is the boundary then (OpenCode does
  this for `/`), and a session working in `/` or `~` itself has none, so
  reading `~/.ssh` there asks for `external_directory` first.
- **Context-tier pricing:** a step whose prompt passes one of the catalog's
  context tiers (such as over 200k tokens) is priced at that tier, as
  OpenCode prices it, for the session cost and `codesmith stats`. Every step
  was priced at the default price, and a model with only tiered prices at
  none.
- **Tool input validation:** a call's input is checked against the tool's
  JSON Schema after repair, as OpenCode checks it. Input that does not fit
  never reaches the tool: the call fails with the issues (up to five) and
  the arguments, for the model to call again. Built-in, MCP and plugin
  tools, and calls from Code Mode programs. Each schema is compiled once.
- An interrupted turn (**Esc**) keeps the text and reasoning the model had
  streamed, as a failed stream does: they are saved at once and replayed in
  the next turn, without the step's unfinished calls or provider state.
  They were lost.
- Plugins' `tool_before` and `tool_after` hooks run for each `fetch` (as
  `webfetch`) and OpenAPI call (as `openapi`) inside `execute`, as they do
  for built-ins: a veto refuses the call and `tool_after` can redact it.
- With `builtin_skills` on, `gh issue create` and `gh issue comment` always
  ask, through built-in rules a configured allow cannot lift, so the
  `report` skill's confirmation no longer rests on the model.
- Code Mode `fetch` and OpenAPI calls ask the permission again when a
  redirect leaves the origin (a 307 or 308 keeps the method and body), and
  `webfetch`, `fetch`, OpenAPI and image input downloads refuse a redirect
  from a public host to a local or private address.
- URL permission rules (`webfetch`, Code Mode `fetch`, `openapi`, image
  and video input URLs) match the URL as it is requested, not as written:
  `HTTPS://Internal.Corp:443/x`, a tab in the host or a `%XX` escape no
  longer gets past a deny rule or policy. Write URL rules with a lowercase
  host and no default port.
- A URL with `*` or `?` in its host is refused, and "always" never saves a
  `webfetch` or `openapi` rule with a wildcard in its origin: approving
  `https://*github.com/x` saved a rule that allowed almost every URL.
- `scripts/ci-linux.sh` runs local CI (fmt, clippy, tests, shellcheck) in a
  Linux container, arm64 or `--amd64`; the pre-push hook adds it with
  `CODESMITH_CI_LINUX=1`.
- **Shell permission fixes:** arithmetic the scan did not see is flagged
  as dynamic now: Zsh evaluates the operands of `shift`, `exit`, `return`,
  `break`, `continue`, `repeat` and the numeric conversions of `printf`
  and `print -f` (however the options are written: `print -rf %d`,
  `print -f%d`) as arithmetic, and Bash and Zsh a value assigned to `RANDOM`, `OPTIND` (and
  Zsh's `SECONDS`, `LINES`...), where an array subscript runs its command
  substitutions, so `shift 'a[$(rm -rf x)]'` ran `rm` but was checked as
  `shift`. A declaration (`export NULLCMD=rm`, `typeset ...`) is checked as
  a plain assignment is.
- **Shell permission fixes:** Bash 5.3's `${ cmd; }` and `${| cmd; }`
  command substitutions were read as parameter expansions, so the command
  inside was never checked (`echo "${ rm -rf x; }"` passed as `echo`); they
  are now refused as unanalyzable. An ANSI-C NUL (`$'curl\0z'`, which Bash
  runs as `curl`) and `\cX` escapes are refused too. A group's descriptor
  duplication (`(make) 2>&1`) no longer asks about `2>&1`, and
  `> # comment` is refused as the shell refuses it.
- Compaction checks the summary template by its heading lines: a reply
  that only mentions `## Objective` is not a summary, and a second reply
  without the template fails the compaction instead of replacing the
  history. The skill list leaves out the skills the agent's rules deny.
- **Permission fix:** a path rule written from the home folder
  (`"read": "~/.ssh/*"`) never matched, since paths are checked as
  absolute paths, so such a deny did nothing; it matches now.
- A tool path of a bare `~` is the home folder (it was a file named `~`
  in the working folder).
- A title written over OpenAI Responses leaves out the model's
  commentary, keeping only its final answer.
- A configured skill folder `~/x` is looked for in the home folder, not
  under the project.
- A prompt of attachments alone no longer carries an empty text part
  (sent to the Responses API as an empty `input_text`).
- Undo survives a snapshot capture that was killed: a stale `index.lock`
  or a corrupt index no longer breaks every later capture and undo, and a
  restore that cannot remove one file still restores the others.
- A turn's request fits its output limit to the room the prompt leaves in
  the context window (less 1%), and a configured output limit is capped at
  256k like a catalog one.
- An agent whose `system` prompt is empty gets the default prompt rather
  than none; a session started in `dir/` lists with `dir`; an unreachable
  provider host is named in the error ("Can't reach host. Check the network
  connection.").
- Mini mode moved out of the main branch to `feature/slim-mode` (#1).
- `codesmith upgrade` (also `codesmith update`): upgrades a copy installed
  from a release tarball to the latest or a given release, checking the
  tarball against `SHA256SUMS` and the new binary's `--version` before it
  renames it over the old one. Homebrew, `cargo install`, source-checkout
  and system-package copies are never replaced; it names the package
  manager's command instead. `--check` only reports.
- An update check, off by default (OpenCode's is on): `"update": "notify"`
  in the global config checks at most once a day with one plain GET and
  prints a line after the UI exits.
- `scripts/release.sh` builds, smoke-tests and packages all four release
  targets on one Apple Silicon Mac (static musl Linux builds through
  cargo-zigbuild), and with `--draft-upload` creates the draft release.
- **OpenAPI tools in Code Mode:** OpenAPI 3.x specs named in config
  (`openapi.<name>`) become `tools.<name>.<operation>(input)` inside
  `execute`. Each call asks the `openapi` permission ("always" covers the
  method on that origin); credentials come from config or `{env:}` and go
  only to `base_url`'s origin, and are dropped once a redirect leaves it.
- **`fetch` in Code Mode:** programs have OpenCode's `fetch` global, with
  the webfetch tool's checks (permission, redirect deny checks, no
  credentials, a 5 MiB cap). Under `full_codemode`, which hides `webfetch`,
  programs could not fetch at all before; a model offered no `execute`
  now keeps `webfetch`.
- **Client-attached MCP servers:** a client embedding the core can attach
  an MCP server it hosts over its own connection (`Mcp::attach`), scoped
  to the attachment it holds. Attaching under a name already attached
  stacks, and the connection below takes over when the top one closes.
  Attached tools go through the same permission rules, plugin hooks and
  Code Mode as a configured server's, and each call carries its session's
  ID. Configured servers can be added, removed and reconnected at runtime
  (`Mcp::add`, `Mcp::remove`, `Mcp::restart`).
- The TUI MCP dialog connects a server as soon as you sign in to it,
  without a restart.
- **Freeform tools:** for models that accept them (`capabilities.freeform`),
  `execute` takes its program and `patch` its patch as raw text, sent as
  custom tools on OpenAI Responses and Chat Completions (with OpenCode's
  patch grammar for `patch`). Calls and history keep the same input object,
  and past calls replay the way each tool is declared now. Inside Code Mode
  a freeform tool takes its string directly.
- ACP: an editor-supplied HTTP MCP server no longer uses CodeSmith's saved
  MCP OAuth logins. The headers the editor sends are its credentials, as
  in OpenCode.
- The open menu (`/open`, **ctrl+x o**) opens recent sessions and
  projects from any folder and a project's worktrees, where new ones can be
  made; `/worktrees` opens, makes and removes worktrees and moves the
  session to one. A session or folder elsewhere restarts the UI there;
  `tui.session.new_location` chooses where `/new` goes back to.
- Typing `@` offers the skills and subagents before the files, as
  OpenCode does.
- Text in the prompt can be selected from the keyboard (shift and a
  movement key) and copied with ctrl+c through the terminal (`OSC 52`);
  typing replaces it. Copies drop NUL characters. The mouse stays the
  terminal's: the UI never captures it.
- **Plan mode plugin:** the `plan` agent writes plan files in a Plan
  directory (`~/.local/share/codesmith/plan`) and nowhere else, and a
  blocked write names that directory. Plan mode is announced by enter and
  leave reminders saved in the conversation, reconciled with the history
  before each request, instead of in the system prompt. A new built-in
  `plan` plugin; `"plugins": {"plan": false}` turns it off.
- **The `codesmith` tool:** the model can search the chat, image and video
  models it can use (grouped by provider, own provider first, newest of
  each family, in pages) and list, create and remove the project's git
  worktrees. Creating or removing a worktree asks the same `edit` approval
  writing there does. It never changes configuration; `"plugins":
  {"codesmith": false}` turns it off.
- **GitHub Copilot Auto:** a session on `github-copilot/auto` asks
  Copilot's Auto router which model serves it and keeps the pick (and its
  session token, sent only with that model's requests) until the token
  nears expiry, the session is compacted, or the account changes; the
  account's chat fallback serves while the router fails.
- **Provider credentials:** several saved credentials per provider, one
  active (the others kept in `auth.json` beside it), and OAuth logins
  refreshed once per expiry for every request that needs them, keeping a
  login another process refreshed meanwhile.
- **xAI SuperGrok login:** `codesmith auth login xai --oauth` signs in
  through xAI's device flow with an OAuth client you register (CodeSmith
  ships none); the login is refreshed with that client.
- **Sign in with ChatGPT:** `codesmith auth login openai --oauth` lets a
  ChatGPT subscription pay for OpenAI requests (OpenAI token sharing),
  with an OAuth client of your own or one registered for you through
  OpenAI's agent registration; CodeSmith ships none. The ID token is
  verified; requests go over HTTP to ChatGPT's models only, with plan
  context caps, summary compaction, and no retries for usage limits.
- `codesmith stats`: the calendar shades each day by how its count ranks
  among the range's distinct counts, as OpenCode does, rather than by its
  share of the busiest day, and shows at most the latest 53 weeks.
- Long paths in the open menu, the worktrees dialog and the diff viewer are
  cut as OpenCode cuts them (`…/src/ui/dialog-se….tsx`), keeping the root,
  the nearest folders and the extension; long `@` and directory
  completions keep both ends.
- ACP: a `session/cancel` sent while a prompt is still being set up
  cancels its turn; it was lost, and the turn ran on. A cancel that came
  just as the turn asked a permission no longer hangs, and a turn cancelled
  as it started no longer keeps the session's turn lock, which made the next
  prompt wait 30 seconds.
- The config's `model` also takes OpenCode's object form,
  `{"providerID", "model", "variant"}`, as agents' and commands' models do.

## 0.1.0

First release. CodeSmith is a Rust port of OpenCode's terminal agent; this
release covers its feature set, checked against OpenCode by ported tests and
replays of its recorded provider traffic.

- **Providers:** Anthropic, OpenAI, Gemini, Azure and other hosted
  providers, plus local servers (Ollama, LM Studio, llama.cpp, vLLM, oMLX or
  any OpenAI-compatible endpoint). Provider options such as reasoning effort
  and thinking budgets, with named variants (`provider/model#high`).
  Finish reasons are normalized across providers (a content-filter stop
  names the policy area), and usage includes reasoning tokens.
- **Hosted tools:** provider-run web search, web fetch, code execution,
  file search and image generation, turned on with the `hostedTools`
  option, stored with the session and replayed to the provider that ran
  them.
- **Responses over WebSocket:** session steps go over one socket per
  session, sending only new input after each response. On by default for
  OpenAI, xAI (on Responses) and Azure's v1 API; `settings.transport`
  chooses per provider, and failures fall back to HTTP.
- **Provider state:** Responses message and tool call item ids and phases,
  Anthropic signatures and redacted thinking, and Gemini thought signatures
  (on text too) are kept and sent back. Compatibility overrides cover
  `maxTokensField`, `streamUsage`, `requireReasoning`, `reasoningField`,
  `requireAssistantAfterTool`, `requireSignature` and
  `requireFinishReason`.
- **Stream validation:** malformed stream events from any protocol fail
  the step as invalid output. A Chat stream that ends without a finish
  reason is retried unless `requireFinishReason` is `false`, and a Gemini
  response that drops past its last checkpoint is retried whole.
- **Interfaces:** an interactive terminal UI, `codesmith run` for one-shot
  and scripted use, ACP for editors such as Zed, and a token-protected local
  HTTP server. Tool calls appear in the UI and over ACP while their
  arguments are still streaming. The UI follows a running subagent in its
  call's row and names it in its permission prompts.
- **Themes and key bindings:** OpenCode's themes (its 33 built-in ones,
  theme files in either of its formats, and `system` from the terminal's
  colors) in light or dark mode following the terminal, picked with
  `/themes`; OpenCode's keymap, its command ids, leader key and defaults,
  rebound in `tui.keybinds`.
- **Grouped transcript:** OpenCode's verbosity levels
  (`tui.session.verbosity`, `/verbosity`): at `low` each run of tool calls
  and thoughts is one summary row, at `medium` and `high` reads and
  searches are grouped; ctrl+o opens the groups.
- **Diff viewer and review panel:** `/diff` opens OpenCode's viewer,
  docked beside the session or full screen, on the branch against its
  base, its commits, the uncommitted changes or the session's last turn:
  a file tree, split or unified patches with syntax colors, hunk and file
  navigation, review marks and single-file review, all on rebindable
  `diff.*` keys.
- **Session tabs:** as in OpenCode, sessions open as tabs (`/new`, a
  session picked in `/sessions`), each running its own turn, background
  commands and wake-ups while another is shown. The strip (or, with
  `tui.tabs.layout: vertical`, a column) shows each tab's status: running,
  waiting for a permission or an answer, or unread (failed in red).
  Keys cycle, select, close, reopen and move tabs, and go to the next tab
  waiting for you; a background permission request is announced in the
  shown tab. Open tabs are kept across restarts per folder (or globally),
  and `tui.tabs` takes OpenCode's `mode`, `scope`, `layout` and
  `indicators`. Switching sessions no longer stops the old one's
  background commands; closing its tab does.
- **Terminal title, notifications and exit summary:** the window title
  follows the shown session (`tui.terminal.title`); `tui.attention` shows
  system notifications (`OSC 9`/`777`) and plays sounds when a permission,
  a question, a finished or failed turn needs you, in any tab; terminals
  that read progress reports show when a session works or waits; on exit
  the session and the command that continues it are printed. Mentions of
  other sessions (`ses_…`) in replies are shown with their titles.
- **Turn footers:** each turn ends with its agent, model, duration and
  throughput, measured as OpenCode measures them (`tui.session.tps`);
  `tui.debug.turn_tokens` adds each turn's token usage and flags likely
  cache busts.
- **Prompt history:** kept across restarts in OpenCode's
  `prompt-history.jsonl` shape (the newest 50, shared by all tabs, with
  their attachments).
- **Model favourites and recents:** the model picker lists favourites
  (ctrl+f) and recent models first; F2 steps through the recent ones, and
  each model's variant is remembered, in OpenCode's `model.json`.
- **Shell mode label and failures:** a `!command` prompt is labelled
  `SHELL`, and one that cannot run comes back in the prompt with a toast.
- **Shell mode completion:** a `!command`'s arguments complete to
  directories, as OpenCode completes locations.
- **Question forms:** questions and MCP input requests open as OpenCode's
  form: tabs per question, choices with a cursor, multi-select checkboxes,
  a "Type your own answer" row, text and number fields, links to
  acknowledge, and a review before submitting.
- **Paste chips and prompt undo:** long pastes show as `[Pasted ~N
  lines]` chips (`tui.prompt.paste`), and super+z / super+shift+z undo and
  redo edits in the prompt.
- **Subagent and shell panel:** ↓ on an empty prompt lists the session's
  subagents (nested, with a finished filter) and background commands, and
  opens a command's live output, as OpenCode's composer panel does.
- **Toasts:** copied IDs and links, and failures in other tabs, show as
  OpenCode's toasts at the top right.
- **Models by agent:** switching agents brings back the model each had in
  the session, else the agent's configured one.
- **Line ranges:** `@file#12-20` attaches those lines of the file.
- **Goal in the status line:** `◎ goal`, `◎ goal paused` or `◎ goal met`
  while the session has a `/goal`.
- **Prompt stash and file frecency:** `prompt.stash` and
  `prompt.stash.pop` put a prompt aside and bring it back; `@` lists the
  files mentioned most and lately first.
- **MCP servers dialog:** `mcp.list` lists the MCP servers and their status,
  signs in to one that needs OAuth, and drafts an investigation of one that
  failed.
- **Command palette:** ctrl+p lists the UI's commands and slash
  commands to filter and run, `Copy session ID` among them.
- **Message navigation:** OpenCode's `session.message.*` commands jump
  to the next or previous prompt or answer; alt+end to the last prompt.
- **External editor:** `<leader>e` or `/editor` edits the prompt in
  `$VISUAL` or `$EDITOR`, handing it the terminal and taking it back.
- **Tools:** read, write, edit, glob, grep, shell, web fetch, self-hosted
  web search, todos, questions, and image, PDF and video input (video only
  for models that take it).
- **Shell:** background commands that report back when they finish,
  timeouts that move a foreground command to the background, moving a
  turn's commands to the background (**Ctrl+X b**, or the server's
  `background` route), cancelling one command, a per-session environment,
  and live output and shell ids for front ends.
- **Local server:** sessions, turns as Server-Sent Events (including
  `tool_progress`, `tool_output`, subagent activity as `child_start`,
  `child_text`, `child_reasoning` and `child_end`, and generated media's
  metadata as `media`), a follow stream for turns the server
  starts itself, permissions, questions, undo and redo, and routes for
  jobs, backgrounding, cancelling and the session environment, and
  `filesystem.changed` events for the branch metadata and files a client
  watches (`/fs/watch`).
- **ACP:** modes, models and an Effort option for variants, plans, diffs
  (replayed when a session is loaded), compaction progress and summaries,
  subagent activity (OpenCode's child-session updates), linked files
  attached as `read` sees them, paged `session/list` with additional
  directories, and background commands that wake an idle session.
- **Permissions:** last-match-wins rules; each command in a shell line is
  checked separately, and paths outside the project ask.
- **Agents:** `build`, a read-only `plan`, `explore` and `general`
  subagents, and user-defined agents, in config or as Markdown agent files
  (`agents/**/*.md`, `modes/*.md`, OpenCode's older format included), with
  hidden agents, per-agent request headers and body fields (`temperature`,
  `top_p`), and OpenCode's model-lab prompts for GPT, GPT Astra, Claude,
  Kimi, Trinity and Muse models.
- **Skills and commands:** `SKILL.md` folders, skills downloaded from
  catalog URLs, and prompt templates as `/commands` (nested `commands/**`
  files, `$ARGUMENTS`/`$1` arguments, `` !`cmd` `` output, and commands that
  run in a child session); plugins can add commands that run as code.
- **Instructions that change:** instruction files, skills, MCP server
  instructions, the Code Mode catalog and the date are read again before
  each request; a change reaches the model as a short update in place,
  and the system prompt (and its cached prefix) stays the same until a
  compaction or an undo that removes an update.
- **References:** other folders and Git repositories named under
  `references`, listed to the model when they have a description; Git
  checkouts are kept under the data folder and refreshed daily with `git`.
- **Sessions:** stored locally in SQLite, with continue, per-turn undo and
  redo, export and import, automatic compaction, usage and cost statistics,
  and git worktrees.
- **Code review:** `/calibrate` and `/review`.
- **MCP:** local and remote servers, OAuth, Code Mode, resources, prompts and
  events.
- **Automations:** scheduled and MCP-event-driven runs. An automation made
  from a session runs in a new child session of it each time; `--in-session`
  (CLI) or target `session` (tool) continues the creating session instead.
- **Language servers and formatters:** OpenCode's built-in lists, off by
  default. Files changed outside the agent reach the language servers.
- **File watching:** FSEvents, inotify or ReadDirectoryChangesW, started
  only when something uses it, honouring `.gitignore` and `watcher.ignore`;
  `CODESMITH_DISABLE_FILEWATCHER` turns it off.
- **Plugins:** native Rust plugins compiled into your own build, with
  OpenCode's hooks on prompts, the model context, raw HTTP requests and
  WebSocket frames, and going idle, and a host API for sessions, their
  instruction entries, compactions, permission checks and plugin storage.
  The goal plugin is built in: `/goal <objective>` keeps a session working
  until a reviewer finds the objective met.
- **Session correlation:** model requests carry the session's correlation
  headers, and a subagent's share its parent's cache affinity and prompt
  cache key.
- **Provider library:** `codesmith-ai` also carries per-part media options,
  per-message effort changes and tool namespaces, which sessions do not use
  yet.
- **Distribution:** signed and notarized macOS binaries (Apple Silicon and
  Intel), static musl Linux binaries (x86_64 and arm64), `install.sh` and a
  Homebrew formula.
