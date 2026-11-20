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
- **Command palette:** ctrl+p lists the UI's commands and slash
  commands to filter and run, `Copy session ID` among them.
- **Message navigation:** OpenCode's `session.message.*` commands jump
  to the next or previous prompt or answer; alt+end to the last prompt.
- **External editor:** `<leader>e` or `/editor` edits the prompt in
  `$VISUAL` or `$EDITOR`, handing it the terminal and taking it back.
- **Mini mode:** `codesmith mini` is OpenCode's split-footer interface:
  finished transcript rows are written once into the terminal's own
  scrollback (searchable and selectable after exit) while a live footer
  holds the prompt, the running turn and permission, question and picker
  dialogs. Streamed answers are written a block at a time; resizes and
  resumes replay the history (`tui.mini.replay`, `replay_limit`); entry
  and exit banners, turn summaries, `tui.mini` knobs with OpenCode's
  quiet/default/everything presets, monochrome output, and synchronized
  output frames.
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
