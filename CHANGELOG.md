# Changelog

All notable changes to CodeSmith are listed here. Versions follow
[Semantic Versioning](https://semver.org/).

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
- **Plugins:** native Rust plugins compiled into your own build.
- **Provider library:** `codesmith-ai` also carries per-part media options,
  per-message effort changes and tool namespaces, which sessions do not use
  yet.
- **Distribution:** signed and notarized macOS binaries (Apple Silicon and
  Intel), static musl Linux binaries (x86_64 and arm64), `install.sh` and a
  Homebrew formula.
