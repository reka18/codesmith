# Design: tool groups and their glyphs in the transcript

Status: **approved by Ray on 2026-11-27** from the moving preview
(<https://claude.ai/artifact/AVti3PMjhrPd6gPdYtPuau>). Not built yet. The
input field's busy ember and smithing verbs are unchanged and out of scope.

## Why

Today every tool call is its own `⚙ name detail` row, with its output under
it, and a running call shows only a static `…`. A busy turn becomes a long
list where a read, a shell command, a web fetch and a subagent all look the
same while they run.

## Tool groups

- **Grouping:** a run of tool calls with no model text between them is one
  group. Text from the model ends a group, and the next call starts a new
  one.
- **One line per group, worded by what the calls did:** "Ran 2 commands,
  read a file, edited 3 files ›". While the group runs, it reads in the
  present tense ("Running a command, reading a file…") and updates as calls
  arrive.
  - The counts go by kind, in plain words, in the order the kinds first
    appeared.
  - An MCP or plugin tool is named: "called github_create_issue".
- **Collapsed by default.** An open group shows today's per-call rows
  beneath it, with their output and diffs. A live call there animates its own
  glyph, and a finished call shows ✓ or ✗.
- **Never hidden by collapsing.** The group line itself always shows:
  - a failure: "· 1 failed", in the error colour;
  - a permission request or question waiting on you, which takes over the
    line until answered;
  - an edit's diff summary (`+12 −3`) when the group edited files.

## Glyphs

Every glyph is one terminal column, from braille, block and geometric
characters common terminal fonts carry, with no emoji. All glyphs step on the
120 ms clock the input field's ember already uses, so any number of running
groups cost nothing extra.

| Kind | Tools | Animation | Settled |
|---|---|---|---|
| Looking | `read`, `grep`, `glob`, `list` | a dot sweeping around the cell: `⡀ ⠄ ⠂ ⠁ ⠈ ⠐ ⠠ ⢀` | `⠶` |
| Shaping | `edit`, `write`, `patch` | a hammer rising, striking and sparking: `▁ ▂ ▃ ▅ ▇ █ ✶ ▂` (the spark flashes at the strike) | `▆` |
| Commands | `shell` | a turning block: `▖ ▘ ▝ ▗` | `▞` |
| Web | `webfetch`, `websearch` | an orbit: `◜ ◝ ◞ ◟` | `◯` |
| Subagents | `subagent` | a braille spinner, in the agent's colour: `⣾ ⣽ ⣻ ⢿ ⡿ ⣟ ⣯ ⣷` | `⣿` |
| MCP and plugin tools | any other | generated from the tool's name (below) | the sprite's resting form |

- **A running group** animates the glyph of the call running now, glowing
  through the forge's heat colours (cherry red, orange, amber). A light
  theme stops at deep amber, as the ember does.
- **A finished group** settles to its last call's settled glyph: in the
  success colour when every call succeeded, in the error colour when any
  failed.
- **Generated glyphs:** a 2×4 braille sprite from the tool's full name.
  - A hash of the name picks 2 to 4 of the cell's 8 dots and a direction.
  - Each frame steps those dots one place around the cell's ring.
  - The resting form is the starting dots plus the two centre dots.
  - The same name always gives the same sprite, and different names look
    different.
- **`NO_COLOR`:** glyphs still animate, in the muted colour.
- **`tui.animations: false`:** every glyph stays on its first frame, for
  anyone who wants less motion.

## Keys (no mouse)

CodeSmith never captures the mouse, so groups are reached from the keyboard.

| Key | Does |
|---|---|
| `Ctrl+O` | Opens or closes every group at once (`session.toggle.tool_output`, as today). |
| `Ctrl+X ↑` | Enters the transcript cursor on the most recent group (new: `session.tool.select`: the leader key, then ↑). |
| `↑` / `↓` | In the cursor, moves between groups; the view scrolls to follow. |
| `Enter`, `Space`, `→` | Opens the selected group. |
| `←` | Closes it. |
| `Esc`, or typing | Leaves the cursor and returns to the input. |

- While the cursor is active, the footer shows its keys under a
  `TRANSCRIPT` label. The selected group is marked with a bar in the
  primary colour.
- `↑` in an empty prompt still recalls input history: the cursor has its own
  entry key.
- Every key goes through the keymap, so it can be rebound.
- The entry key is `Ctrl+X ↑` rather than `Alt+↑`, which stays the
  previous-tab key (Ray, 2026-11-27).

## Elsewhere

- **`codesmith run`** (stdout text) is unchanged: tool activity still goes to
  stderr, one line per call.
- **ACP and `serve`** clients get the same events as today. Grouping and
  glyphs are only how the terminal UI draws them.
- **Exports** (`/export`, `codesmith export`) keep one entry per call.

## Tests

- **Grouping:**
  - text splits groups;
  - the wording for every mix of kinds, singular and plural, present and
    past tense, MCP names included;
  - order of first appearance.
- **Never hidden:** a failure, a waiting permission or question, and a diff
  summary show on a collapsed group's line.
- **Glyphs:**
  - every frame and settled glyph is exactly one column (`unicode-width`),
    so lines never shift;
  - each kind's sequence;
  - success and failure colours;
  - the subagent colour;
  - `NO_COLOR`;
  - `tui.animations: false`.
- **Generated glyphs:**
  - stable for a name;
  - different for many sampled names (a bounded property test);
  - always a valid braille character;
  - a rotation of their resting dots.
- **Keys:**
  - Ctrl+X ↑ selects the latest group;
  - ↑ and ↓ stop at the ends;
  - Enter, Space and → open; ← closes;
  - Esc and typing leave;
  - Ctrl+O toggles all;
  - the view scrolls to the selection;
  - rebinding works.
- **Render snapshots** for a turn with every kind, collapsed and open, in
  dark and light themes.

## When

Built alongside the OS sandbox, for 1.0.0 (Ray, 2026-11-27).
