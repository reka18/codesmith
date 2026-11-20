# The interactive UI

Run `codesmith` in a project to open the terminal UI.

```sh
codesmith                          # new session
codesmith -c                       # continue the latest session in this folder
codesmith -s ses_01J...            # continue a specific session
codesmith --prompt "fix the failing test"   # send a first message right away
codesmith -m ollama/qwen3-coder --agent plan
codesmith -w                       # work in a new git worktree (see sessions.md)
codesmith --auto                   # approve every request no rule denies
```

See [CLI](cli.md#codesmith) for every flag. `codesmith mini` opens the same
UI in [mini mode](#mini-mode), which writes the transcript into the
terminal's own scrollback.

## The screen

- **Transcript.** Replies render as Markdown: headings, bold and italic,
  inline code, links, lists and task lists, quotes, tables, and fenced code
  blocks with syntax highlighting. `edit` and `write` calls show a colored
  diff. Long tool output is collapsed to a few lines. A running shell
  command shows the last lines it printed (marked `│`) until its result
  replaces them. Images and files a tool returned, and images a model
  generates (Gemini image models), are listed by type and size (`▣`).
  A `subagent` call's row follows its subagent, as OpenCode's does
  (`│ General Subagent — <description>`): while it runs, its latest three
  tool calls and a status line (`running · shell · 4 tool calls`); once it
  ends, `✓` (or `✗`, `■` when interrupted) and how it ended. **ctrl+o**
  shows its latest 20 calls and the text it wrote last. A permission prompt
  from a subagent says which one asks (`asked by the general subagent ·
  <description>`).
- **Groups.** Tool calls and thoughts are grouped as OpenCode groups them,
  by the [verbosity](#verbosity) level. At `low`, the default, each run of
  tool calls and thoughts between two replies is one row that sums it up
  (`+ 3 commands, 1 edit, 2 thoughts, 4 reads, 1 tool`, or `+ Running
  command…` until something finished). At `medium` and `high`, calls stand
  alone except adjacent reads and searches (`→ Explored: 2 reads, 1
  search`, `✱ Exploring` while they run), which `high` shows open. While
  reasoning is hidden, adjacent thoughts are one row (`+ Thought: <title>
  · 2 steps`, `Thinking` while they stream). A running command's latest
  output and a running subagent's row stay in view below their collapsed
  group, then fold into its summary. **ctrl+o** opens every group (and
  shows tool output in full), and closes them again. Questions are never
  grouped.
- **Turn footers.** After each turn, OpenCode's footer: the agent, the
  model, how long the turn took and its throughput (`Build · openai/gpt-5
  · 12.3s · 48.2 tok/s`). The throughput counts output and reasoning
  tokens over the time the model spent streaming them, so tool time and
  the wait for the first token are left out; `tui.session.tps: false`
  hides it. Mini mode adds the throughput to its turn summary.
  With `tui.debug.turn_tokens`, a `Tokens` row follows it (`+ Tokens: 2
  steps · latest: 1,234 new · 5,000 cached · 6,234 total`), flagging
  likely cache busts: a step that read fewer cached tokens than the one
  before it on the same model (OpenAI's one- to two-bucket moves aside).
  **ctrl+o** opens the per-step table.
- **Tabs.** The open [session tabs](#session-tabs) along the top: each
  one's title, its status, and `+ New session` for a new one.
- **Header.** The agent, model, session id and title.
- **Status line.** As OpenCode's prompt footer, first what runs beside
  the turn (`1 subagent · 2 shells`: a running subagent and the
  [background commands](#background-commands)), then context in use (the last call's tokens, and the share of
  the model's `limit.context` when it is configured), the session's cost when
  the model has a `cost`, and token totals. The input box's title names
  the agent and the keys for agents and the command palette.

## Keys

These are the defaults, which are OpenCode's (with a few kept from
earlier CodeSmith, noted below); every one can be changed (see [key
bindings](#key-bindings)), and `/help` lists the bindings in effect.

### Editing the prompt

| Key | Action |
|---|---|
| **enter** | Send. |
| **shift+enter**, **ctrl+enter**, **alt+enter**, **ctrl+j** | New line. |
| **←/→**, **ctrl+b/f** | Move by character. |
| **alt+←/→**, **ctrl+←/→**, **alt+b/f** | Move by word. |
| **ctrl+a/e**, **alt+a/e** | Start or end of the line. |
| **↑/↓** | Move between lines; at the first or last line, step through earlier messages. |
| **backspace**, **delete**, **ctrl+d** | Delete a character. |
| **ctrl+w**, **alt+backspace**, **ctrl+backspace** | Delete the previous word. |
| **alt+d**, **alt+delete**, **ctrl+delete** | Delete the next word. |
| **ctrl+k** / **ctrl+u** | Delete to the end / the start of the line. |
| **ctrl+shift+d** | Delete the line. |
| **ctrl+c** | Clear the prompt. |
| **leader e** | Edit the prompt in `$VISUAL` or `$EDITOR` (`/editor` too). |

The prompt history is kept across restarts, as OpenCode keeps it: the
newest 50 prompts, shared by every tab and folder, in
`~/.local/share/codesmith/prompt-history.jsonl` (one JSON entry a line, in
OpenCode's shape). A recalled prompt brings back the images and files it
attached, when they are still there. Each tab keeps its own unsent prompt
while another is shown.

Pastes go in at the cursor. The external editor gets the terminal while
it runs (`$EDITOR` may carry arguments, as `code --wait`); what it saves
comes back as the prompt, the one newline editors add after a single line
dropped, and the UI takes the terminal back however the editor ended.

### Everything else

| Key | Action |
|---|---|
| **tab** / **shift+tab** | Next or previous primary agent (`build`, `plan`, your own). CodeSmith's: OpenCode has shift+tab for the next one. |
| **esc** | Interrupt the running turn; when idle, clear the prompt. |
| **ctrl+c** | Interrupt a running turn; on an empty prompt, quit; otherwise clear the prompt. |
| **ctrl+d** on an empty prompt, **ctrl+x q** | Quit. |
| **ctrl+o** | Expand or collapse all tool output, subagent activity and [groups](#verbosity) (CodeSmith's). |
| **ctrl+t** | Next model [variant](configuration.md#provider-options-and-variants) (as `/variant`). |
| **pgup/pgdn**, **ctrl+alt+b/f** | Scroll the transcript a page. |
| **ctrl+alt+u/d**, **ctrl+alt+y/e** | Scroll half a page, or a line. |
| **home**, **ctrl+g** | Scroll to the top. |
| **end**, **ctrl+end**, **ctrl+alt+g** | Follow the bottom again. |
| **alt+end** | Jump to your last prompt. |
| **ctrl+b** while a command runs | Move the running commands to the background. |
| **ctrl+z** | Suspend to the shell (Unix); `fg` returns. |
| **alt+↓** / **alt+↑**, **ctrl+tab** / **ctrl+shift+tab** | Next or previous [session tab](#session-tabs). |
| **alt+shift+↓** / **alt+shift+↑** | Next or previous tab that is unread or waiting for you. |
| **ctrl+shift+t**, **ctrl+x shift+t** | Reopen the last closed tab (the second is CodeSmith's: most terminals send ctrl+shift+t as ctrl+t). |
| **ctrl+x**, then a key | OpenCode's leader keys: **m** models, **a** agents, **l** sessions, **n** new session (in a new tab), **w** close the tab, **1**…**9**, **0** go to tab 1 to 10, **c** compact, **u** undo, **r** redo, **q** quit, and CodeSmith's **h** help, **b** background and **shift+←/→** move the tab. The input box's title shows `ctrl+x …` while it waits (2 seconds). |

Reasoning has no key by default (OpenCode's `session.toggle.thinking`);
`/thinking` shows or hides it. Nor have the verbosity and grouping
commands (`session.verbosity.cycle`, `session.toggle.exploration_grouping`);
`/verbosity` cycles the level. As in OpenCode, jumping between messages
is unbound by default: `session.message.next` and `.previous` move to the
next or previous prompt or answer, `session.message.user.next` and
`.previous` to prompts only, scrolling it to the top of the transcript
(with blank room below the last one when needed).

### Key bindings

Bindings are set in [`tui.keybinds`](configuration.md#the-terminal-ui-tui)
by OpenCode's command ids, so an OpenCode keymap carries over:

```jsonc
{
  "tui": {
    "keybinds": {
      "leader": "ctrl+o",                  // the <leader> key
      "session.list": "<leader>s,ctrl+l",  // alternatives, comma-separated
      "session.toggle.thinking": "ctrl+r",
      "agent.cycle.reverse": "none",       // or false: unbound
      "prompt.paste": { "key": "ctrl+v" }  // a binding object
    }
  }
}
```

A binding is `"none"` or `false`, a string of alternatives, a key object
(`{"name": "x", "ctrl": true}`), a binding object (`{"key": "ctrl+x"}`) or a
list of them. Keys are written `ctrl+`, `alt+` (or `meta+`), `shift+` and
`super+` before a character or a name (`return`/`enter`, `escape`/`esc`,
`tab`, `space`, `backspace`, `delete`, `up`, `pageup`/`pgup`, `home`, `f2`,
…); `<leader>m` is the leader key then `m`, and `gg` or `ctrl+x ctrl+s` are
sequences. OpenCode's first-format names (`session_list`,
`messages_page_up`, …) are accepted too.

The commands CodeSmith runs, by where they apply:

- **Prompt:** `input.submit`, `input.newline`, `input.move.left/right/up/down`,
  `input.word.forward/backward`, `input.line.home/end`,
  `input.visual.line.home/end`, `input.buffer.home/end`, `input.backspace`,
  `input.delete`, `input.delete.word.forward/backward`,
  `input.delete.to.line.end/start`, `input.delete.line`,
  `prompt.history.previous/next`, `prompt.clear`.
- **Session:** `app.exit`, `session.interrupt`, `session.background`,
  `agent.cycle`, `agent.cycle.reverse`, `agent.list`, `model.list`,
  `variant.cycle`, `session.list`, `session.new`, `session.compact`,
  `session.tab.next/previous`, `session.tab.next_unread/previous_unread`,
  `session.tab.close`, `session.tab.reopen`, `session.tab.select.1` to
  `.10`, `session.tab.move.left/right` (CodeSmith's),
  `session.undo`, `session.redo`, `help.show`, `theme.switch`,
  `theme.switch_mode`, `theme.mode.lock`, `session.toggle.thinking`,
  `session.toggle.tool_output` (CodeSmith's), `session.verbosity.cycle`,
  `session.toggle.exploration_grouping`, `session.page.up/down`,
  `session.half.page.up/down`, `session.line.up/down`, `session.first`,
  `session.last`, `terminal.suspend`.
- **Autocomplete:** `prompt.autocomplete.prev/next/select/complete/hide`.
- **Pickers:** `dialog.select.prev/next/page_up/page_down/home/end/submit`.
- **Questions:** `dialog.prompt.submit`.
- **Permission dialog:** `permission.allow.once` (**y**),
  `permission.allow.always` (**a**), `permission.reject` (**n**), CodeSmith's.

- **Diff viewer:** `diff.open` (unbound by default), `diff.close`,
  `diff.down/up`, `diff.page.down/up`, `diff.half_page.down/up`,
  `diff.first/last`, `diff.next_hunk/previous_hunk`,
  `diff.next_file/previous_file`, `diff.mark_reviewed`,
  `diff.toggle_file_tree`, `diff.single_patch`, `diff.switch_source`,
  `diff.toggle_view`, `diff.toggle_fullscreen`, `diff.help`, and
  `pane.focus.left/right`. See [the diff viewer](#the-diff-viewer-and-review-panel).

OpenCode's other commands (the terminal pane, tab history, …) keep their
ids and defaults, so a shared config loads, but do nothing here. When a key is
bound to more than one command in the same place, a command whose binding
you configured goes first, then the order above; two configured bindings
that clash, or one that starts another (so the longer one never runs), are
reported when the UI opens, as are unknown commands and keys.

## Verbosity

How much tool activity the transcript shows is OpenCode's
`session.verbosity` level, set in [`tui`](configuration.md#the-terminal-ui-tui)
and cycled with `/verbosity` (or `session.verbosity.cycle` when bound):

| Level | Thoughts (reasoning hidden) | Thoughts (shown) | Reads and searches | Other calls, output and diffs |
|---|---|---|---|---|
| `low` (default) | counted in the run's summary row | counted in the summary row | counted in the summary row | counted in the summary row |
| `medium` | one `Thought` row per run | in full | one `Explored` row per run | each on its own row, output and diffs collapsed |
| `high` | one `Thought` row per run | in full | an `Explored` row, open | each on its own row, output and diffs collapsed |

The summary counts finished work by kind: commands, edits (`edit`,
`write`, `patch`), thoughts, reads, and other tools (searches, fetches,
subagents, …); failed calls count, running ones do not. **ctrl+o** opens
every group at any level and shows output and diffs in full.
`session.grouping` set to `none` (or `session.toggle.exploration_grouping`)
draws reads and searches one by one at `medium` and `high`. The level and
grouping changed in the UI last for the run; the config sets them for the
next one. A continued session's transcript is grouped the same way.

## The diff viewer and review panel

`/diff` (or `diff.open`, when bound) opens OpenCode's diff viewer on the
session's folder; `/diff` again closes it. On a terminal 108 columns or
wider it is docked on the right half, beside the session, with the keys
going to it; **ctrl+x ←** gives them back to the prompt and **ctrl+x →**
to the viewer (`pane.focus.left/right`). **f** shows it full screen (and
back), which is remembered for next time; on a narrower terminal it is
always full screen.

What it compares (**d** chooses):

| Source | What it shows |
|---|---|
| All | The branch's commits and local changes (untracked files too), against where it left its base branch (`· vs main`). |
| Committed | The branch's commits only. |
| Uncommitted | Local changes, against `HEAD` (`· vs HEAD`). |
| Last turn | The files the session's last finished turn changed, from the snapshots undo keeps; it follows each new turn. |

The base is found as OpenCode finds it: the branch the current one was
created from (its reflog), or the default branch when that is the one
checked out. When neither tells, the viewer asks you to choose rather than
guess: the chooser's **Base** row lists local and remote branches (type to
filter). The source and the base chosen for a branch are kept until the
UI exits. `tui.diffs.source` sets the source it opens on.

Each changed file is a card: its path and `+added -removed`, then its
hunks with line numbers, in the theme's diff colors with syntax
highlighting (files of more than 5000 lines are shown without it). Wide enough (a 100-column patch pane), modified files are
shown side by side; **v** switches split and unified. Full screen at 88
columns or more, a file tree lists the files beside the patches (folders
first, single-folder chains on one row, `A`/`M`/`D` or `✓`); **b** hides
it. Binary files and images have a note instead of a patch (images are not
previewed in the terminal); git's patch text is read up to 10 MB in all,
and untracked files only when they are text files inside the project,
under 1 MB, not through a symlink.

| Key | Action |
|---|---|
| **j**/**k**, **↓**/**↑** | Scroll a line. |
| **ctrl+d**/**ctrl+u** | Half a page. |
| **pgdn**/**pgup**, **ctrl+f**/**ctrl+b** | A page. |
| **gg**, **home** / **G**, **end** | The start or the end. |
| **]** / **[** | The next or previous hunk. |
| **n**, **alt+↓** / **p**, **alt+↑** | The next or previous file (opening its folder in the tree). |
| **m** | Mark the file in view reviewed (its card folds to its title), or unmark it. |
| **s** | One file at a time; reviewing moves on to the next. |
| **v** | Split or unified. |
| **b** | Show or hide the file tree. |
| **d** | Choose the source or the base. |
| **f** | Full screen or docked. |
| **?** | The viewer's shortcuts, with the bindings in effect. |
| **esc**, **q**, **ctrl+c** | Close the viewer. |

The review count (`2/5`) is in the header. Review marks belong to the
viewer: they are cleared when its files change (another source, a new
turn) or it closes, and, as in OpenCode, they are not sent to the model or
to `/review`. A file's title stays at the top while its card is in view.
Only the rows in view are drawn, so a diff of thousands of lines scrolls as
fast as a small one. While a permission or question waits, the docked
viewer keeps its keys (focus the prompt to answer); full screen, its last
row says so.

The layout chosen in the viewer (tree, single file, split or unified, full
screen) is remembered across runs until `tui.diffs` changes; see
[`tui.diffs`](configuration.md#the-terminal-ui-tui). OpenCode's mouse
actions (clicking folders and the source label, the right-click file menu,
image previews) have no counterpart.

## Themes

The UI draws in a theme: OpenCode's `opencode` theme by default, or one of
its others (`tokyonight`, `catppuccin`, `gruvbox`, `nord`, `dracula`,
`rosepine`, `github`, `solarized`, `one-dark`, `everforest`, …, all MIT,
from OpenCode), or `system`, built from the terminal's own sixteen colors on
the terminal's background. Every color follows it: the transcript,
Markdown, code highlighting, diffs, the status line, the input box, popups
and dialogs.

`/themes` (or `/theme`) opens a picker that previews each theme as you move;
**enter** keeps it, **esc** goes back. `/themes <name>` switches directly.
The choice is remembered across runs; set a default in
[`tui.theme`](configuration.md#the-terminal-ui-tui).

Each theme has a light and/or a dark mode. The mode follows the terminal's
background, which CodeSmith asks the terminal for as it starts (OSC 11, as
OpenCode does); if the terminal does not answer, it is dark. `/themes
dark` or `/themes light` pins a mode, `/themes system` follows the terminal
again, and `tui.theme.mode` sets it in config. A theme with only one mode
shows that one, and the pinned mode comes back with the next theme that
has it.

Your own themes go in `themes/<name>.json` in the global config folder
(`~/.config/codesmith/themes/`) or a project's `.codesmith/themes/`;
a project's override the global ones and the built-ins of the same name.
Both of OpenCode's theme formats load: the current one (a `base` of
semantic tokens, and `light`/`dark` modes with their hue scales, see
OpenCode's `opencode.json`) and the first one (`defs` and a flat `theme`
of named colors with `{"dark", "light"}` pairs), which is migrated as it
loads, as OpenCode migrates it. A theme that does not load is reported
and `opencode` is used.

## Slash commands

| Command | What it does |
|---|---|
| `/new` | Start a new session, in a tab of its own. |
| `/sessions` | Pick a session in this folder to continue; it opens in a tab (or its tab is shown). |
| `/fork` | Fork the session: all of it, or before a prompt you pick, which comes back in the input. The fork opens in a tab of its own. See [sessions](sessions.md#forking). |
| `/undo`, `/redo` | Undo or redo the latest turn's file changes. See [sessions](sessions.md#undo-and-redo). |
| `/agent [id]` | Switch agent, or pick one. |
| `/model [provider/model]` | Switch model, or pick one. |
| `/variant [name]` | Switch to the model's next [variant](configuration.md#provider-options-and-variants), or to the one named; `/variant default` drops it. |
| `/compact` | Summarize older history now. See [compaction](configuration.md#compaction). |
| `/clear` | Clear the screen. The session is kept. |
| `/thinking` | Show or hide reasoning. |
| `/editor` | Edit the prompt in `$VISUAL` or `$EDITOR`. |
| `/diff` | Open or close the [diff viewer and review panel](#the-diff-viewer-and-review-panel). |
| `/verbosity [low\|medium\|high]` | Cycle the transcript's [verbosity](#verbosity), or set it. |
| `/themes [name\|dark\|light\|system]` | Pick a [theme](#themes) (`/theme` too), or set its mode. |
| `/help` | Keys and commands. |
| `/exit` (`/quit`, `/q`) | Quit. |
| `/init` | Write or improve `AGENTS.md` for the project. |
| `/goal <objective>` | Keep working until a reviewer finds the objective met; `/goal pause`, `resume`, `clear`, or `/goal` alone to see it. See [the goal plugin](plugins.md#the-goal-plugin). |
| `/calibrate`, `/review [target]` | [Code review](code-review.md). |
| `/<name> [args]` | Run a [custom command](skills-and-commands.md#commands) or an [MCP prompt](mcp.md#prompts) (`/server:prompt`). |

Commands that change the session wait until the current turn is finished.
Press **Esc** first to interrupt it. `/new` and `/sessions` do not wait
while tabs are on: the running turn goes on in its tab.

## Shell commands

A prompt that starts with `!` runs the rest as a shell command, as you, as
OpenCode's shell mode does: no permission prompt and no model call. It runs
in the session's folder (and [environment](local-server.md#session-environment)),
is drawn as a shell row (Ctrl+O shows its output), and its output and exit
code join the conversation, so the model sees them with your next message.
Esc leaves it running; it is recorded when it ends.

## Pickers

`/model`, `/agent`, `/sessions` and `/themes` without arguments open a
picker (and so do **ctrl+x m**, **a** and **l**). Type to filter. **↑/↓**,
**ctrl+p/n** or **tab** move, **pgup/pgdn** jump, **home/end** go to the
ends, **enter** chooses, and **esc** closes. The model picker
lists the models configured under `providers`, plus the current one. To use
any other model, type `/model provider/model`.

**ctrl+p** opens the command palette, as in OpenCode: every UI command
with its keys, and the slash commands, filtered as you type; **enter**
runs the one highlighted. It is also how to reach commands that have no
key by default, such as `Copy session ID` (which puts the session's ID on
the clipboard through the terminal, `OSC 52`).

As in OpenCode, the model picker lists your favourite models first (`★`),
then the ones used recently, then the rest; **ctrl+f** adds the
highlighted model to the favourites or takes it off. **F2** and
**shift+F2** step through the ten models used last
(`model.cycle_recent`), and `model.cycle_favorite` (unbound) through the
favourites. Each model's chosen [variant](configuration.md#provider-options-and-variants)
is remembered and comes back when you pick it again. They are kept in
`~/.local/share/codesmith/model.json`, in OpenCode's shape.

## Autocomplete and `@` mentions

Typing `/` lists commands: built-in, custom, and MCP prompts. Typing `@`
lists project files, matched fuzzily and respecting `.gitignore`. **↑/↓**
choose, **Tab** completes, **Enter** runs the command or inserts the file,
and **Esc** closes the list.

Each `@path` in a message that names a file is attached to the message as a
`read` result, as in OpenCode, so the model sees it without a round trip. A
directory is attached as its listing, and an image, PDF or video as itself.

## Attaching images and PDFs

Paste the path of an image, PDF or video (`.mp4`, `.mov`, `.webm`;
dragging a file into most terminals pastes its path). The path is replaced
by an `[Image 1]`, `[PDF 1]` or `[Video 1]` placeholder, and the file is attached when you send the prompt. Delete the
placeholder to drop it. See [images and PDFs](tools.md#images-and-pdfs) for
the formats and limits.

## Background commands

A turn ends when the model answers, even while shell commands it started
with `background: true` still run; the status line shows `1 background
command running`. When one finishes while the UI is idle, the session wakes
up by itself: a `background command shell_1 finished (…)` notice appears and
the model carries on with the result as a new turn, which **Esc** interrupts
like any other.

A prompt you send before that goes first: the wait is cancelled and your
turn reports the finished command (or the next one does, if it is still
running), so each result reaches the model once. The commands keep running
when you interrupt a later turn, switch with `/agent`, `/model`, `/variant`
or **Tab**, and across `/undo` and `/redo`; the reloaded session reports
them. They keep running in a tab that is not shown, which resumes by itself
when one finishes; closing the tab stops them, and so does quitting. With
tabs off, leaving the session with `/new` or `/sessions` (for a different
one) stops them. See [background commands](sessions.md#background-commands).

## Session tabs

As in OpenCode, sessions open as tabs: `/new` (**ctrl+x n**) starts one in
a tab of its own, and a session picked in `/sessions` opens in a tab, or
shows its tab when it is open. Every tab's session runs on its own: a turn,
its [background commands](#background-commands) and the resumes they bring
go on in a tab that is not shown, each tab keeps its prompt draft, scroll
position and diff viewer, and the prompt history and display settings are
shared.

The strip along the top shows each tab's title after an indicator:

| Indicator | Meaning |
|---|---|
| `⠋` | A turn is running. |
| `!` | A permission request waits for you. |
| `?` | A question waits for you. |
| `•` | Its turn ended since you last looked (red when it failed). |
| `+` | A new session nothing was done in yet. |

`tui.tabs.indicators: "numbers"` shows each tab's number instead. A
permission request or question from a tab that is not shown also says so
in the shown tab (`Tab "Build" needs your permission: alt+shift+↓ goes to
it.`), and **alt+shift+↓** goes to the next tab that is unread or waiting.
When the tabs do not fit, the strip shows a window around the shown one,
with `‹3` and `2›` counting the tabs hidden either side.

**alt+↓**/**alt+↑** (or **ctrl+tab**, in terminals that send it) cycle the
tabs, **ctrl+x 1** to **0** go to tab 1 to 10, and **ctrl+x shift+←/→**
move the shown tab. **ctrl+x w** closes it, stopping its turn and its
background commands; the tab shown before it comes back (else its right or
left neighbour), and closing the last one leaves a new session.
**ctrl+x shift+t** (or **ctrl+shift+t**) reopens closed tabs, newest
first, where they were. A new
session left with nothing done or typed in it closes by itself, as
OpenCode's new session page does.

The open tabs are kept for the next start, one set per folder
(`tui.tabs.scope: "global"` shares one set); a kept tab loads its session
when it is first shown, and is marked `•` when the session changed since
you last looked at it. With `tui.tabs.layout: "vertical"`, the tabs are a
column on the left, each with the project (and the branch, in a worktree)
under its title, when the terminal is at least 106 columns wide and the
diff viewer is closed; narrower, they are the strip. `tui.tabs.mode: "off"`
turns tabs off (see [configuration](configuration.md#session-tabs-tuitabs));
they are on by default except inside the herdr multiplexer, and mini mode
has none. With no mouse, there are no clicks, drags or tab menus.

## The terminal title, notifications and exit

As in OpenCode, the UI keeps the terminal window's title on the shown
session's: `CS | <title>` (cut to 40 characters), or `CodeSmith` until the
session has one. `tui.terminal.title: false` leaves the title alone, and
the `terminal.title.toggle` command (unbound by default) switches it off
and on. The title is cleared on exit, and when the terminal hangs up
(`SIGHUP`) the UI closes as on exit.

With `tui.attention` (see
[configuration](configuration.md#notifications-and-sounds-tuiattention)),
the UI tells you when a session needs you: a permission request, a
question, a turn that finished or failed, in any tab. A system
notification (`OSC 9`, or `OSC 777` in VTE terminals, foot and urxvt;
passed through tmux) is shown while the terminal is out of focus, and a
sound plays whatever the focus: the terminal's bell, or a sound file you
configure. A subagent finishing only sounds. Each request notifies once,
and a failed turn does not also notify that it is done. Notification text
is stripped of escape sequences and control characters.

In terminals that show progress reports (Ghostty, Windows Terminal,
ConEmu), the tab or taskbar shows when a session is working and when one
waits for you. OpenCode reports each session separately through OpenTUI's
program status records; CodeSmith reports the summary with `OSC 9;4`.

On exit the UI prints the session and the command that continues it:

```
  Session   Fix the failing test
  Continue  codesmith -s ses_19a4b2c3d4e123abc
```

## Permission and question dialogs

Permission requests and the agent's questions open as dialogs:

- **Permission:** **y** allows once, **a** always allows (saved for the project),
  and **n**, **Esc** or **Enter** reject. Type first, and Enter rejects with
  your text as feedback to the model. Edits and writes show a diff preview.
  See [answering a request](permissions.md#answering-a-request).
- **Question:** the `question` tool asks multiple-choice or free-form
  questions, one at a time. Type an option's number (several, separated by
  commas or spaces, when more than one may be picked) or your own answer, and
  press **Enter**. **Esc** dismisses the questions.

## Mini mode

`codesmith mini` is OpenCode's split-footer interface (`opencode mini`): it
stays on the terminal's normal screen, never the alternate one. Each part of
the transcript is written into the terminal's scrollback once it is
finished, so it stays after the UI exits and can be scrolled, searched,
selected and copied with the terminal's own tools; a small live footer at
the bottom holds the prompt, the running turn and any dialog.

```sh
codesmith mini                         # new session
codesmith mini -c                      # continue the latest session here
codesmith mini -s ses_01J... --no-replay
git diff | codesmith mini --prompt "review this"   # piped input, then --prompt, is the first message
```

It takes `-c`, `-s`, `-m`, `--agent`, `--prompt` and `--auto` as `codesmith`
does, plus `--replay` / `--no-replay` and `--replay-limit N` (see below).
It needs a terminal for its output; piped stdin becomes the first prompt
(before `--prompt`, a line apart) and keys are then read from the terminal.

- **What is written, and when.** The transcript is cut into the same rows
  as the full UI: an entry, or a group of tool calls and thoughts at the
  current [verbosity](#verbosity). A row is written when it is finished
  (the turn is over, or a later row has started and none of its calls is
  still running), rendered once with the same Markdown, diff and group
  renderers and the theme, wrapped at the terminal's width. A streamed
  answer is written a block at a time: up to the last blank line outside a
  code fence, so code blocks and tables are held until they are whole.
  Prompts, notices and errors are written at once. Text rows are a blank
  row apart from what is around them; tool rows follow one another. Once
  written, a row does not change (ctrl+o, `/verbosity` and `/thinking`
  apply to what is written after).
- **The footer.** From the top: what is not written yet (the answer's
  unfinished block, running calls with their command output as it prints,
  a running subagent's row), autocomplete, the prompt box, and a status
  line: a spinner, `working`, the interrupt key, then as room allows the
  agent, model, context in use, cost and the help key. The status line
  admits those facts one by one in a fixed order and stops at the first
  that does not fit, so it does not jump about as the terminal is resized
  (OpenCode's `footerStatuslinePolicy`). The footer starts on the row the
  cursor was on when mini mode opened, so what the shell printed before
  stays; it grows upward only once it reaches the bottom.
- **Dialogs.** Permission requests, the agent's questions and pickers
  (`/model`, `/sessions`, …) take the prompt's place in the footer, with the
  same keys as the [full UI's dialogs](#permission-and-question-dialogs).
  Once answered they are gone; the call they asked about is written with
  its result. The diff viewer (`/diff`) is full screen and borrows the
  alternate screen while it is open; the footer comes back as it was.
- **Keys.** Every binding is the full UI's, through the keymap. `app.clear`
  (**ctrl+l**) clears the screen without losing anything: the rows on it
  scroll up into the scrollback. Scrolling keys do nothing here; the
  terminal scrolls its own history. **ctrl+z** suspends; the footer starts
  again at the bottom when the UI is continued.
- **Banners.** `▪ cs mini v0.1.0 · ~/src/project` as the session opens
  (shortened to fit: the folder's name, the version, then more of its
  path), and on exit the session's title and the command that resumes it
  (`Continue codesmith mini -s ses_…`), which replaces the full UI's exit
  line. After each turn a summary row says which agent and model ran it
  and for how long (`Build · anthropic/claude-sonnet-4-5 · 12.3s`).
- **Resume and resize.** With replay on (the default), a continued
  session's newest 200 prompts and answers are written as the UI opens,
  and when the terminal is resized the screen and its scrollback are
  cleared and the transcript written again at the new width (terminals
  reflow lines differently, and some not at all). `--no-replay` or
  `tui.mini.replay: false` keeps what is there: nothing is written on
  resume, and after a resize the footer moves to the bottom rows.
  `--replay-limit` or `tui.mini.replay_limit` changes the 200.
- **Terminals.** Each frame is one write wrapped in synchronized output
  (`CSI ? 2026 h` … `l`), which terminals that support it show at once and
  others ignore; footer rows that did not change are not written again.
  Rows are written without trailing spaces, so copied text is clean.
  `tui.mini.mono` writes monochrome ASCII for terminals (or logs) that want
  it: no colors, and box and list symbols as ASCII.

What the scrollback shows is set under [`tui.mini`](configuration.md#mini-mode-tuimini),
with OpenCode's defaults: reasoning shown, tool calls not (the footer
shows them while they run), and a turn summary. Three presets set the
knobs together, as OpenCode's verbosity slider does: `/verbosity quiet`
(answers and prompts only, no footer details), `/verbosity default`, and
`/verbosity everything` (tool calls with their shell output too);
`/verbosity up` and `/verbosity down` step between them. `/verbosity`
with no argument, or `low`/`medium`/`high`, still sets how tool rows are
grouped when they are shown.

