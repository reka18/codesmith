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

See [CLI](cli.md#codesmith) for every flag.

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
  hides it.
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
| **ctrl+c** | Clear the prompt (copy the selection, when text is selected). |
| **shift+←/→/↑/↓**, **shift+home/end**, **alt+shift+←/→**, **ctrl+shift+a/e**, **super+a** | Select text in the prompt (by character, line, to the start or end, by word, to the line's ends, all). |
| **leader e** | Edit the prompt in `$VISUAL` or `$EDITOR` (`/editor` too). |

Selected text in the prompt is highlighted. **ctrl+c** (or **super+c**,
where the terminal passes it on) copies it to the clipboard through the
terminal (`OSC 52`, NUL characters dropped), and it stays selected; typing
or a paste replaces it, **backspace** or **delete** deletes it, **esc**
drops it, and any other key drops it before doing what it does.

The mouse has no effect in CodeSmith other than the terminal's own: the UI
never turns on mouse reporting, so the terminal selects, copies and pastes
with the mouse as it does anywhere else (a paste arrives as typed text).
There are no clicks, drags or wheel scrolling in the UI, and no
`terminal.copy` or mouse setting; OpenCode's copy-on-select is the
terminal's.

The prompt history is kept across restarts, as OpenCode keeps it: the
newest 50 prompts, shared by every tab and folder, in
`~/.local/share/codesmith/prompt-history.jsonl` (one JSON entry a line, in
OpenCode's shape). A recalled prompt brings back the images and files it
attached, when they are still there. Each tab keeps its own unsent prompt
while another is shown.

Pastes go in at the cursor. A long paste (three lines or more, or over
150 characters) shows as a `[Pasted ~N lines]` chip, as in OpenCode, and
is sent in full while the chip is whole in the prompt; pasting the same
text again right after its chip expands it. `tui.prompt.paste: "full"`
(or `app.toggle.paste_summary`, unbound by default, from the command
palette) pastes in whole. **super+z** or **ctrl+-** (`input.undo`) undoes
an edit in the prompt and **super+shift+z** or **ctrl+.** (`input.redo`)
redoes it; a run of typing is one step, and an undone chip or `@file`
mention is sent as before. The same image pasted twice keeps its label
and is attached once. *Stash prompt* (`prompt.stash`, unbound by default;
in the command palette) puts the prompt aside and *Pop stashed prompt*
(`prompt.stash.pop`) brings the latest back; the stash keeps 50 prompts
across restarts, in `prompt-stash.jsonl` beside the history. The external editor gets the terminal while
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
| **ctrl+x**, then a key | OpenCode's leader keys: **m** models, **a** agents, **l** sessions, **n** new session (in a new tab), CodeSmith's **o** [open menu](#projects-worktrees-and-other-folders), **w** close the tab, **1**…**9**, **0** go to tab 1 to 10, **c** compact, **u** undo, **r** redo, **q** quit, and CodeSmith's **h** help, **b** background and **shift+←/→** move the tab. The input box's title shows `ctrl+x …` while it waits (2 seconds). |

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
  `input.delete.to.line.end/start`, `input.delete.line`, `input.undo`,
  `input.redo`, the `input.select.*` commands, `prompt.history.previous/next`,
  `prompt.clear`, `prompt.editor`, `prompt.stash`, `prompt.stash.pop`,
  `app.toggle.paste_summary`.
- **Session:** `app.exit`, `session.interrupt`, `session.background`,
  `command.palette.show`, `agent.cycle`, `agent.cycle.reverse`,
  `agent.list`, `model.list`, `model.cycle_recent` and
  `model.cycle_favorite` (and their `_reverse`), `variant.cycle`,
  `session.list`, `open.menu`, `session.move`, `session.new`,
  `session.copy.id`, `session.child.first`, `mcp.list`, `session.compact`,
  `session.tab.next/previous`, `session.tab.next_unread/previous_unread`,
  `session.tab.close`, `session.tab.reopen`, `session.tab.select.1` to
  `.10`, `session.tab.move.left/right` (CodeSmith's),
  `session.undo`, `session.redo`, `help.show`, `theme.switch`,
  `theme.switch_mode`, `theme.mode.lock`, `session.toggle.thinking`,
  `session.toggle.tool_output` (CodeSmith's), `session.verbosity.cycle`,
  `session.toggle.exploration_grouping`, `session.page.up/down`,
  `session.half.page.up/down`, `session.line.up/down`, `session.first`,
  `session.last`, `session.message.next/previous`,
  `session.message.user.next/previous`, `session.messages_last_user`,
  `terminal.suspend`, `terminal.title.toggle`.
- **Autocomplete:** `prompt.autocomplete.prev/next/select/complete/hide`.
- **Pickers:** `dialog.select.prev/next/page_up/page_down/home/end/submit`,
  and `model.dialog.favorite` in the model picker.
- **Worktrees dialog:** the picker's keys and
  `dialog.move_session.new/move/delete/refresh`.
- **Subagent and shell panel:** `composer.subagent.up/down/select/interrupt`,
  `composer.shell.up/down/select/kill`, and `dialog.mcp.toggle` in the MCP
  servers dialog.
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
`write`, `patch`), thoughts, reads, other tools (searches, fetches,
subagents, …) and instruction files; failed calls count, running ones do
not. A Code Mode `execute` call counts the calls its program finished
(`+ 2 reads, 1 tool`) rather than itself, and itself as one tool when its
program called none (or, in a continued session, whose calls were not
stored).

When a read brings in the `AGENTS.md` (or `CLAUDE.md`) files of the
folders above it, the load is a row of its own: adjacent loads are one
`◈ Instructions: 2 files` row (inside the run's summary at `low`, where
they count as `2 instructions`, and open at `high`), with each file below
it (`↳ Loaded src/AGENTS.md`) when opened. **ctrl+o** opens
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
| `/open` (`/projects`) | Open a recent session or project from any folder, or one of a project's worktrees. See [projects, worktrees and other folders](#projects-worktrees-and-other-folders). |
| `/worktrees` | Open, make or remove one of the project's worktrees, or move the session to one. |
| `/fork` | Fork the session: all of it, or before a prompt you pick, which comes back in the input (after anything typed there). The fork opens in a tab of its own. See [sessions](sessions.md#forking). |
| `/undo`, `/redo` | Undo or redo the latest turn's file changes. See [sessions](sessions.md#undo-and-redo). |
| `/agent [id]` | Switch agent, or pick one. As in OpenCode, an agent takes the model (and variant) it last had in this session, else its configured model, else the session's. |
| `/provider [name]` | Switch provider, or pick one; a provider that needs a key or sign-in opens the sign-in picker on it. |
| `/model [provider/model]` | Switch model, or pick one of the current provider's. |
| `/variant [name]` | Switch to the model's next [variant](configuration.md#provider-options-and-variants), or to the one named; `/variant default` drops it. |
| `/compact` | Summarize older history now. See [compaction](configuration.md#compaction). |
| `/login` | [Sign in](#signing-in-to-providers) to a provider, or sign out. |
| `/export` (**Ctrl+X x**) | [Export the session](#exporting-a-session) as Markdown or JSON: copy it or save it to a file. |
| `/clear` | Clear the screen. The session is kept. |
| `/thinking` | Show or hide reasoning. |
| `/editor` | Edit the prompt in `$VISUAL` or `$EDITOR`. |
| `/diff` | Open or close the [diff viewer and review panel](#the-diff-viewer-and-review-panel). |
| `/verbosity [low\|medium\|high]` | Cycle the transcript's [verbosity](#verbosity), or set it. |
| `/themes [name\|dark\|light\|system]` | Pick a [theme](#themes) (`/theme` too), or set its mode. |
| `/help` | Keys and commands. |
| `/exit` (`/quit`, `/q`) | Quit. |
| `/init` | Write or improve `AGENTS.md` for the project. |
| `/automation [request]` | Ask the model to schedule or change [automations](automations.md) ("every morning at 9 check CI"). |
| `/goal <objective>` | Keep working until a reviewer finds the objective met; `/goal pause`, `resume`, `clear`, or `/goal` alone to see it. The status line shows `◎ goal` while one is set (`◎ goal paused`, `◎ goal met`). See [the goal plugin](plugins.md#the-goal-plugin). |
| `/calibrate`, `/review [target]` | [Code review](code-review.md). |
| `/<name> [args]` | Run a [custom command](skills-and-commands.md#commands) or an [MCP prompt](mcp.md#prompts) (`/server:prompt`). |

Commands that change the session wait until the current turn is finished.
Press **Esc** first to interrupt it. `/new` and `/sessions` do not wait
while tabs are on: the running turn goes on in its tab.

## Exporting a session

`/export` (`session.export`, **<leader>x**) opens OpenCode's export dialog:
**Export as** Markdown or JSON; for Markdown, **Include thinking** (on when
the transcript shows thinking) and **Include tools**; for JSON, **Sanitize
sensitive data**; then **Copy** or **Save**. Tab and Shift+Tab (or the
arrows) move the focus, Enter or Space selects, Esc closes. The keyboard
does it all; the mouse does nothing in the dialog.

- **Copy** puts the export on the clipboard through the terminal (`OSC 52`),
  as copying a selection does.
- **Save** asks for the file, offering `session-<id>.md` (or `.json`) in the
  working folder; type another path (relative to the working folder, or
  `~/…`), Ctrl+U clears it. The file is created owner-only (0600) and never
  replaces anything without asking: a file already there (or a link) is
  replaced only after you press `y`, and no folder is created. Once saved,
  `c` copies its path.

What each form leaves out: the Markdown transcript has the prompts and
replies, thinking and tool calls as chosen, URL query strings in tool input
and output replaced with `?…`, control characters dropped, and images and
files only named. Raw JSON is the stored session, as `codesmith session
export` writes it. Sanitized JSON keeps the session's shape (record kinds,
roles, ids, tool names, model and agent, times and token counts) and
replaces every text, tool input and output, reasoning, file change, path
and title with a `[redacted:…]` marker; it still imports. Every form has the
credentials CodeSmith holds (provider keys and tokens, their key variables'
values, MCP headers and environment) replaced with `[redacted:credential]`.
A secret CodeSmith does not know of, say one a command printed, is not
recognized: use sanitized JSON, or leave tools out, for a transcript you
share.

## Shell commands

A prompt that starts with `!` runs the rest as a shell command, as you, as
OpenCode's shell mode does: no permission prompt and no model call. It runs
in the session's folder (and [environment](local-server.md#session-environment)),
is drawn as a shell row (Ctrl+O shows its output), and its output and exit
code join the conversation, so the model sees them with your next message.
Esc leaves it running; it is recorded when it ends.
The prompt is labelled `SHELL` while it holds one. A command that cannot
run at all says why in a toast and comes back in the prompt to fix.

The words after the command complete to directories as you type, as
OpenCode completes a location: relative to the session's folder, or after
`~/`, `/` or `../` from there (`..` and `~` themselves are offered too);
dot directories show once you type the `.`. **tab** takes the highlighted
one.

## Pickers

`/provider`, `/model`, `/agent`, `/sessions` and `/themes` without arguments
open a picker (and so do **ctrl+x m**, **a** and **l**). Type to filter.
**↑/↓**, **ctrl+p/n** or **tab** move, **pgup/pgdn** jump, **home/end** go to
the ends, **enter** chooses, and **esc** closes.

The **provider picker** lists every provider with where it stands: `ready`
(with where its key or login comes from, and how many models it lists),
`local` (a server on this machine, with the models it listed, or "not
answering"), or `sign in`. The current provider comes first, then the ready
ones, the local ones, and the rest. Choosing a ready provider switches to the
model you used there last, else its newest; choosing one that needs a key or
login opens the [sign-in picker](#signing-in-to-providers) on it.

The **model picker** lists the current provider's models, newest first
(favourites and recent ones ahead of the rest), and ends with a row that
opens the provider picker. For a hosted provider the list is the one its own
API gives your account ([kept for a day](configuration.md#model-lists-from-the-provider)),
for a [local server](configuration.md#local-model-discovery-ollama-lm-studio-vllm-llamacpp)
the models it serves, and for Claude Code the models it offers. To use any
other model, type `/model provider/model`.

For a provider with a [`swap`](configuration.md#swapping-the-servers-model-swap)
setting (a server that holds one model at a time), the picker also lists the
models the server can load, after the others, each with "loads it on the
server, which takes several minutes". The list comes from the provider's
`list` command, run in the background each time the picker opens (the
models it reported last time show at once). Choosing one, while no turn is
running, starts its `use` command and opens a progress view with the
command's output, escape sequences and control characters taken out. When
it succeeds, the model is selected; when it fails, the view shows why (for
tblink, how many bytes short the memory is) and the current model stays.
**esc** hides the view; the swap goes on, and a notice says how it ended.
Only one swap runs at a time, and only you start one: the model, `run`,
automations and the server API cannot. While the server loads, it serves
nothing, so a turn sent to it meanwhile fails. ctrl+f does not make such an
entry a favourite until it is loaded.

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

## MCP servers

*List MCP servers* (`mcp.list`, unbound by default; in the command
palette) shows each configured [MCP server](mcp.md) with its status:
`Connected ✓`, `Disabled ○`, `Failed !` or `Sign in required →`. **Enter**
or **Space** (`dialog.mcp.toggle`) on one that needs a sign-in starts its
OAuth login, as `codesmith mcp auth` does: the browser opens and the
dialog waits, then comes back to the list, and the server reconnects
with its new sign-in (its tools are offered from the next model step). On one that failed it shows the error;
**i** then opens a new session with a prompt to investigate it, to edit
and send.

## Signing in to providers

`/login` opens the sign-in picker: every provider with where its login
stands (`signed in · …` with a dot, `key from $VAR`, `key in config`, `not
signed in`). Type to filter, **↑/↓** to move, **Enter** to choose, **Esc**
to go back or close. A provider's menu lists its ways in, first the usual
one, then *Open the API key page* where one is known, then *Sign out* when
a login is saved:

- **A browser login** opens the browser and waits. The dialog shows the
  URL (to open by hand or on another machine) and a paste box: paste the
  URL the browser ends up on and press **Enter**. A paste that belongs to
  another sign-in is refused and the login keeps waiting. **Esc** stops it.
- **A device login** shows the code to enter and the page to enter it on.
- **A key** is typed or pasted into a masked field (one `•` a character);
  the key is saved to `auth.json` and is never drawn, put in the prompt or
  history, or shown to the model.
- **Claude Pro/Max: use your Claude Code login** runs the `claude-agent`
  check (no key is stored) and shows Claude Code's version and models, or
  what to run when it is missing or signed out.

When a provider's browser or device login has no client id configured,
the dialog shows where to register an OAuth app and the redirect URL to
give it, then asks for the client id. Everything drawn has its control
characters escaped. The picker is keyboard-only. See
[signing in](getting-started.md#signing-in).

## Autocomplete and `@` mentions

Typing `/` lists commands: built-in, custom, and MCP prompts. Typing `@`
lists the [skills](skills-and-commands.md) and subagents first, as
OpenCode does (at most ten once something is typed), then the project
files, matched fuzzily and respecting `.gitignore`. Before anything is
typed after `@`, the files mentioned most and most lately come first
(OpenCode's frecency, kept in `frecency.jsonl`). **↑/↓** choose, **Tab**
completes, **Enter** runs the command or inserts the name or file, and
**Esc** closes the list.

A skill's `@id` in a message loads the skill with it. A subagent's `@name`
stays in the message for the model, which can hand the work to that
subagent.

Each `@path` in a message that names a file is attached to the message as a
`read` result, as in OpenCode, so the model sees it without a round trip. A
directory is attached as its listing, and an image, PDF or video as itself.
As in OpenCode, `@path#12-20` attaches lines 12 to 20, and `@path#12` (or
`@path#12-`) from line 12 on; a `#` that is part of the file's name stays
part of it (`@report#2026.md#3`).

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

### The subagent and shell panel

**↓** on an empty prompt (`session.child.first`, also *Toggle subagent
picker* in the command palette) opens OpenCode's panel of the session's
subagents and background commands; **←**/**→** switch between its
**Subagents** and **Shell** tabs, **Esc** or **ctrl+c** close it.

- **Subagents** lists the running subagents, the ones they ran nested
  under them (`├─`, `└─`); **ctrl+a** shows the finished ones instead (the
  choice is kept until the UI exits). **↑**/**↓** move
  (`composer.subagent.up`, `.down`; up from the first row closes the
  panel), **Enter** (`composer.subagent.select`) opens the subagent's
  session, and **ctrl+d** (`composer.subagent.interrupt`) interrupts it: a
  subagent runs inside its turn, so this interrupts the turn.
- **Shell** lists the running background commands, one line each.
  **Enter** (`composer.shell.select`) opens a command's output and
  **ctrl+d** (`composer.shell.kill`) stops it.

The output view follows the command's output as it comes (its latest
64 KiB, with escape sequences taken out; earlier output is said to be
left out), with its status (`Running`, `Exited · code 0`, `Killed`, `Timed
out`). **↑**/**↓**, **PageUp**/**PageDown** and **Home** scroll back, which
stops following; **End** or scrolling back down to the end follows again.
**c** copies the command and **o** the output (`OSC 52`); **Esc** goes
back to the panel. Output redirected to files is not shown.

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
they are on by default except inside the herdr multiplexer. With no mouse, there are no clicks, drags or tab menus.

## Projects, worktrees and other folders

As OpenCode's open menu does, `/open` (**ctrl+x o**, `open.menu`) lists
the recent sessions from every folder, then the projects they belong to:
a project is a Git repository's main checkout, and the UI's own folder is
always one (marked `●`). Each session shows its project (with the
worktree or subfolder it is in, `OpenCode · dashboard`) and how long ago
it was used. Sessions waiting for you come first, marked `?` (a question)
or `!` (a permission request); `⋯` marks one with a turn running and `▪`
one open in a tab. Up to eight recent sessions show with nothing typed;
typing filters sessions and projects, and a whole session ID finds that
session however old it is. The list shows at once what it listed last
time while it is read again.

**alt+↑/↓** jump between the sessions and the projects. **→** on a Git
project (marked `→`) lists its worktrees, its main one first and the UI's
next; **ctrl+n** there makes a new one (a name is optional; blank picks a
random one) and opens it, and **←** or **esc** goes back to the projects.
**enter** opens what is highlighted: a session is continued, a project or
worktree gets a new session.

`/worktrees` (`session.move`, no key by default) lists the project's
worktrees, the UI's first. **enter** opens one with a new session;
**ctrl+m** (or **ctrl+g**, for terminals that send ctrl+m as enter) moves
the session there and continues it; **ctrl+a** makes a new worktree;
**ctrl+d**, pressed twice, removes the one highlighted (not the main one,
nor the one the UI is in; one with uncommitted changes is refused); and
**ctrl+r** reads the list again, keeping the filter. A session whose
folder is gone opens this dialog with `Session location unavailable`,
and the folder chosen (or a worktree made) becomes the session's.

The UI works in one folder at a time, with that folder's configuration,
tabs and files. A session or folder elsewhere closes it and starts it
again there, once no turn is running (interrupt it first). `/new` then
goes back to the folder CodeSmith was started in, as OpenCode's
`session.new_location: "launch"` does; with `tui.session.new_location:
"inherit"` it stays in the folder opened last (see
[configuration](configuration.md#the-terminal-ui-tui)). Sessions are listed
from the store, so the menu works without Git; worktrees need it.

## The terminal title, notifications and exit

As in OpenCode, the UI keeps the terminal window's title on the shown
session's: `CS | <title>` (cut to 40 characters), or `CodeSmith` until the
session has one. `tui.terminal.title: false` leaves the title alone, and
the `terminal.title.toggle` command (unbound by default) switches it off
and on. The title is cleared on exit, and when the terminal hangs up
(`SIGHUP`) the UI closes as on exit.

Short messages (a copied session ID or link, a turn that failed in another
tab) show as toasts at the top right, as in OpenCode, for five seconds; one
shows at a time, with `+N more` waiting behind it. A toast about another
session than the shown one (or its parent or subagents) carries that
session's title. A toast over the cursor hides it.

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
- **Question:** the `question` tool's questions (and an MCP server's
  requests for input) open as a form, as OpenCode's: a tab for each
  question and a **Submit** tab with a summary of the answers. **←**/**→**
  (or **h**/**l**, **Tab**/**Shift+Tab**) move between tabs, **↑**/**↓** (or
  **j**/**k**) between choices, a number picks one, and **Enter**
  (`dialog.prompt.submit`) picks the highlighted one. Where several may be
  picked, **Space** or Enter ticks them (`[✓]`). The last row, **Type your
  own answer**, takes your own text: start typing on it, or press Enter,
  and Enter again to keep it (pasting on a choice question opens it too).
  Free-form questions are a text field. **Esc** closes an answer you are
  typing, or dismisses the form; **ctrl+c** clears the typed answer first.
  A link to open (an MCP server's) says when it is opened (Enter) or copied
  (**c**), and must be confirmed with Enter before the form submits. A
  question that comes while a picker or the help is open waits until it
  closes.
