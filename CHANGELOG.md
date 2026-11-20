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
- **Interfaces:** an interactive terminal UI, `codesmith run` for one-shot
  and scripted use, ACP for editors such as Zed, and a token-protected local
  HTTP server. Tool calls appear in the UI and over ACP while their
  arguments are still streaming.
- **Tools:** read, write, edit, glob, grep, shell (with background commands),
  web fetch, self-hosted web search, todos, questions, and image and PDF
  input.
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
- **Distribution:** signed and notarized macOS binaries (Apple Silicon and
  Intel), static musl Linux binaries (x86_64 and arm64), `install.sh` and a
  Homebrew formula.
