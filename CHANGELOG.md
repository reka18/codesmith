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
  arguments are still streaming.
- **Tools:** read, write, edit, glob, grep, shell, web fetch, self-hosted
  web search, todos, questions, and image and PDF input.
- **Shell:** background commands that report back when they finish,
  timeouts that move a foreground command to the background, moving a
  turn's commands to the background (**Ctrl+X b**, or the server's
  `background` route), cancelling one command, a per-session environment,
  and live output and shell ids for front ends.
- **Local server:** sessions, turns as Server-Sent Events (including
  `tool_progress` and `tool_output`), a follow stream for turns the server
  starts itself, permissions, questions, undo and redo, and routes for
  jobs, backgrounding, cancelling and the session environment.
- **ACP:** modes, models and an Effort option for variants, plans, diffs
  (replayed when a session is loaded), compaction progress and summaries,
  subagent activity (OpenCode's child-session updates), linked files
  attached as `read` sees them, paged `session/list` with additional
  directories, and background commands that wake an idle session.
- **Permissions:** last-match-wins rules; each command in a shell line is
  checked separately, and paths outside the project ask.
- **Agents:** `build`, a read-only `plan`, `explore` and `general`
  subagents, and user-defined agents.
- **Skills and commands:** `SKILL.md` folders and prompt templates as
  `/commands`.
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
  default.
- **Plugins:** native Rust plugins compiled into your own build.
- **Provider library:** `codesmith-ai` also carries video input, per-part
  media options, per-message effort changes and tool namespaces, which
  sessions do not use yet.
- **Distribution:** signed and notarized macOS binaries (Apple Silicon and
  Intel), static musl Linux binaries (x86_64 and arm64), `install.sh` and a
  Homebrew formula.
