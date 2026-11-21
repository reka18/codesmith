# Terminal parity with OpenCode

CodeSmith's command line, interactive UI, local server and Code Mode are
pinned to OpenCode's by porting the tests of OpenCode's terminal-facing
packages (MIT) where CodeSmith has the behaviour, as
[`codesmith-ai/PARITY.md`](../codesmith-ai/PARITY.md) does for the provider
layer. This file is the per-test inventory: every OpenCode test in
`packages/cli` (ACP included), `tui`, `server`, `codemode`, `plugin`,
`util` and `session-ui` was examined and put in one bucket.
`packages/core` is inventoried separately, in `crates/codesmith-core/PARITY.md`. The
app, desktop, gui, ui, web, cloud and other browser or hosted packages are
not applicable to a terminal agent and were not examined.

- **ported**: CodeSmith already behaved this way; the test is ported (an
  `oc_*` test, or an existing test that now cites the OpenCode test).
- **ported after a fix**: CodeSmith was wrong or missing something cheap; it
  was fixed and the test ported.
- **gap: missing feature**: the test needs a feature CodeSmith lacks; these
  make up the [roadmap](#roadmap).
- **gap: by design or n/a**: CodeSmith differs on purpose (a local
  in-process agent with no hosted services, native Rust plugins, a ratatui
  UI, Boa for Code Mode, a smaller local API), or the test is about
  OpenCode internals (OpenTUI and Solid, Effect layers, Bun bundling, the
  web UI) with no CodeSmith counterpart.
- **deferred**: the test belongs to a feature parked off the main branch;
  see [deferred](#deferred).
- **not ported yet**: CodeSmith has the behaviour but the test is not
  ported, either because the code it would drive belongs to work in progress
  elsewhere or because its assertions are computed rather than literal; see
  [below](#not-ported-yet).

Parametrized tests (`test.each`, `describe.each`, loops over case tables)
count once per case. A test counts as ported only when all of its
assertions are; "(adapted)" in the lists marks a test rephrased for
CodeSmith (its terminal UI, byte offsets rather than OpenTUI cells, its own
command names), and a dropped half is named.

Where the tests live:

- `src/cli_parity.rs`: flags, help and argument tests (`cli/test/flag`,
  `auth`, `import-export`, `mini`, `debug-*`).
- `src/tui/parity.rs`: the prompt editor and history, pickers, autocomplete
  triggers, pasted files and attachment labels (`tui/test/prompt*`,
  `dialog-select`, `app-lifecycle`; also the shared-code cases of
  `mini/prompt.shared` and `mini/runtime.queue`, which are counted with
  mini mode as [deferred](#deferred)).
- `src/tui/theme/tests.rs`: themes, their V1 migration, mode selection and
  the system theme (`tui/test/theme*`, `cli/tui/theme-mode`,
  `context/theme`).
- `src/tui/group.rs` and `src/tui/parity_groups.rs`: the grouped
  transcript and its verbosity levels (`cli/tui/session-verbosity`,
  `grouping-tree`, `session-grouping`, `session-rows`, `group-view`,
  `session-activity-anchor`, `session-backfill-disclosure`,
  `session-mount-budget`, and the grouping settings of `config-v2`).
- `src/tui/parity_diff.rs`, `src/tui/diff_tree.rs` and
  `src/tui/diff_source.rs`: the diff viewer and review panel
  (`cli/tui/diff-viewer`, `diff-viewer-file-tree`, `diff-viewer-image`,
  `feature-plugins/diff-viewer-file-tree-utils`, `session-panel`, `panel`,
  and the diff settings of `config-v2`).
- `src/tui/parity_tabs.rs`: session tabs (`context/session-tabs-model`,
  `context/session-tabs`, `component/session-tabs-status`, the vertical
  tabs of `app-lifecycle` and the tab settings of `config-v2`);
  `tests/e2e/tui.rs` runs tabs in a pty (`oc_keeps_each_visited_session_open`,
  background tabs and their permission requests).
- `src/tui/parity_browser.rs`: the open menu and the worktrees dialog
  (`cli/tui/dialog-open`, `cli/tui/prompt-move`, `cli/tui/dialog-session-list`
  and the session location setting of `config-v2`).
- `src/tui/parity_ui.rs`: the UI's smaller features (reasoning titles,
  `cli/tui/thinking`, and the groups listed with their tests below);
  `src/tui/mention.rs`: session mentions (`util/src/session-mention`).
- `src/tui/parity_keys.rs`: key bindings and the UI's settings
  (`tui/test/config`, `config-v2`, `keymap`, `cli/tui/dialog-prompt`,
  `cli/tui/reasoning-body`, `cli/test/config`).
- `tests/e2e/oc_cli.rs`: tests that need the binary (`debug paths`, piped
  input, session export and import, `auth list` and `logout`, the session
  environment, XDG folders).
- `codesmith-core/tests/oc_stats.rs`: the `stats` card (`cli/test/stats`).
- `codesmith-core/tests/oc_codemode.rs` with
  `tests/fixtures/oc_codemode.json`: 1137 assertions from
  `packages/codemode/test` (the programs and the values they expect,
  extracted as text from the test files; Test262-derived cases under the BSD
  license in `fixtures/LICENSE.test262`, web-platform-tests-derived ones
  under `fixtures/LICENSE.wpt`), run through Code Mode on Boa. 1026 pass;
  the other 111 are listed in the test with their reason, and the test
  fails if any other case fails or a listed one starts passing.
- `codesmith-core/src/codemode/signature.rs`: the JSON Schema cases of
  `codemode/test/signature` (the Effect Schema ones have no counterpart);
  `codemode/search.rs` and `codemode/web.rs`: catalog search, nested tool
  paths and their errors (`tool-paths`, `enumeration`), and the web
  globals beyond the extracted programs.
- `src/upgrade/tests.rs`: `codesmith upgrade` and update checks
  (`cli/test/upgrade`, `updater-install`, `src/services/updater`), against
  a local fixture server.
- `codesmith-core/src/codemode/openapi/tests.rs`: OpenAPI tools
  (`codemode/test/openapi`), with OpenCode's two spec fixtures.
- `src/acp.rs`: the `oc_acp_*` tests, with the ACP tests not ported listed
  above its test module.
- `src/tui/parity_form.rs`: the question form (`tui/test/util/form`,
  `cli/tui/form`); `src/tui/form.rs` is the form itself.
- `src/tui/parity_prompt.rs`: paste chips, prompt undo, attachment labels
  (`tui/test/prompt-paste`, `prompt/attachment`) and the prompt groups after
  them.
- `src/tui/parity_panel.rs`: the subagent and shell panel and the output
  viewer (`cli/tui/composer-keymap`, `util/session`,
  `session-subagent-navigation`, `component/dialog-shell-output`).

## Summary

| Package / area | Tests examined | Ported (already passing) | Ported after a fix | Gaps: missing feature | Gaps: by design or n/a | Not ported yet | Deferred |
|---|---:|---:|---:|---:|---:|---:|---:|
| cli | 269 | 40 | 51 | 2 | 176 | 0 | 0 |
| cli: ACP | 105 | 55 | 4 | 0 | 25 | 21 | 0 |
| tui: main UI | 1247 | 15 | 502 | 41 | 676 | 13 | 0 |
| tui: mini mode | 428 | 0 | 0 | 0 | 0 | 0 | 428 |
| server | 123 | 0 | 0 | 0 | 123 | 0 | 0 |
| codemode | 1666 | 647 | 218 | 0 | 586 | 215 | 0 |
| plugin | 38 | 0 | 0 | 0 | 38 | 0 | 0 |
| util | 140 | 2 | 17 | 0 | 111 | 10 | 0 |
| session-ui | 621 | 0 | 0 | 0 | 621 | 0 | 0 |
| **Total** | **4637** | **759** | **792** | **43** | **2356** | **259** | **428** |

## Fixes made while porting

- **OpenAPI tools** (`codemode/test/openapi`): CodeSmith had no OpenAPI
  support. Specs named in config (`openapi.<name>`) now become Code Mode
  tools, `tools.<name>.<operation>(input)`, generated and called as
  OpenCode's `OpenAPI.fromSpec` does; each call asks the `openapi`
  permission, and credentials come from config and go only to
  `base_url`'s origin (see `docs/mcp.md`).
- **Worktree names need `=`** (`cli/test/flag`): `-w`/`--worktree` took the
  next word as the worktree's name, so `codesmith run -w "fix it"` (the
  form `docs/sessions.md` showed) made a worktree named `fix it` and sent no
  message. As in OpenCode, the name is now `--worktree=NAME` or `-w=NAME`,
  and a separate word is part of the message.
- **The server token reached shell commands** (`cli/test/env`): a
  session's commands inherited `$CODESMITH_SERVER_TOKEN`, so a command could
  drive the local server. It is now removed from their environment.
- **History lost the draft** (`prompt/history-composer`): stepping up into
  the history and back down past the newest entry left an empty prompt; it
  now gives back what was typed.
- **Moving the cursor left the history** (`prompt-offset`): End, Home or an
  arrow on a recalled entry dropped the place in the history, so the next
  Down did nothing; only editing leaves it now.
- **Pasted escape sequences** (`prompt-paste`): a paste kept terminal escape
  sequences (CSI, OSC); they are dropped, other control characters kept.
- **Ctrl+C in a picker** (`dialog-select`): it closed the picker even with a
  filter typed; it clears the filter first.
- **Pasted file paths** (`prompt/local-attachment`): a single unquoted path
  with spaces was split into words and pasted as text, and a URI-list paste
  with a `# comment` line (as some terminals send for dropped files) was not
  recognized. The whole paste is tried as one path first, and comment lines
  are skipped.
- **The server token reached other children** (`cli/test/env`, extended):
  after shell commands, local MCP servers, language servers, formatters,
  git and the worktree start command still inherited
  `$CODESMITH_SERVER_TOKEN`. `codesmith` now takes it out of its own
  environment at startup, before any thread starts (`serve` keeps the
  value), and every spawn removes it again. Code Mode's child already
  starts from an empty environment.
- **Built-in command names and arguments** (`mini/prompt.shared`): `/quit
  now` quit, and `/Quit` or `/EXIT` were unknown commands. As in OpenCode,
  the UI's built-in commands ignore case and surrounding space, and those
  that take no argument (`/exit`, `/quit`, `/q`, `/new`, `/compact`,
  `/help`, `/clear`, `/thinking`, `/sessions`, `/undo`, `/redo`) are only
  themselves without one; `/agent`, `/model` and `/variant` keep theirs.
  Vim's `:q` quits too, as OpenCode's `isExitCommand` allows. Custom
  commands still match their name exactly, as in OpenCode. One difference
  stays: OpenCode sends `/quit now` to the model as a prompt (it sends any
  slash text that is not a command), while CodeSmith reports `/quit takes no
  arguments`, as it reports unknown commands rather than sending them.
- **Compact token counts and truncation** (`util/locale`): `999 950`
  tokens showed as `1000K`; as in OpenCode, counts that round to a thousand
  thousands are millions (`1M`; CodeSmith still drops a trailing `.0`, so
  OpenCode's `1.0M` is `1M`). Cutting text to width 0 gave `…` rather than
  nothing, and cutting could split a grapheme (an emoji family, a letter
  with a combining mark); it now keeps whole graphemes.
- **Calendar month labels** (`cli/test/stats`): the calendar labelled a
  partial first month (`Apr` for a range starting on April 29). As in
  OpenCode's `activityCalendar`, a week now belongs to the month of its
  Thursday, kept inside the range, and a label is drawn only where it fits
  in its month's weeks, or for the last month.
- **Code Mode runs programs in strict mode** (`codemode/test/parity`,
  `descriptors`, `stdlib`): programs ran as sloppy-mode async arrows, so
  writes to frozen, sealed or read-only properties were silently ignored,
  plain calls got the global object as `this`, and so did the program
  itself. They now run as strict-mode async functions, as OpenCode applies
  those checks; a program that repeats a parameter name (OpenCode accepts
  it, strict mode rejects it while parsing, before anything runs) runs in
  sloppy mode. 12 more assertions pass, completing 11 tests: 9 of the 15
  strict-mode ones, and 2 counted with OpenCode's restrictions (array
  `length` is not configurable; `Uint8Array` writes out of range are
  ignored). One difference this adds: top-level `arguments` is the
  wrapper's empty arguments object where OpenCode throws a
  `ReferenceError`.
- **Themes** (`tui/test/theme*`, `cli/tui/theme-mode`, `context/theme`):
  the UI drew in fixed terminal colors. It now has OpenCode's themes: its
  V2 documents (a `base` of semantic tokens over `light` and `dark` hue
  scales, validated, mode-selected, state fallbacks expanded, references
  resolved, dialog surfaces), its V1 format migrated as it loads, its 33
  built-in themes (MIT, attributed), `themes/*.json` in the config folders,
  and `system` from the terminal's palette; the mode follows the
  terminal's background (OSC 11) unless pinned, a theme without the
  requested mode shows its other one, and an invalid theme falls back to
  `opencode`. `/themes` picks one. Every color the UI draws comes from it,
  syntect's code highlighting included.
- **Key bindings and TUI settings** (`tui/test/config`, `config-v2`,
  `keymap`, `cli/tui/dialog-prompt`, `cli/tui/reasoning-body`,
  `cli/test/config`): keys were fixed. They now go through OpenCode's
  keymap: its command ids, descriptions and defaults, the leader key and
  its timeout, sequences, `none`/`false`, binding objects, first-format
  names (an id set directly wins), and `ctrl+z` taking undo where there is
  no suspend; set in `tui.keybinds`, with `tui.leader`, `tui.cursor` and
  `tui.session.thinking`/`markdown`, read in OpenCode's first `tui.json`
  shape too. Following OpenCode's defaults, Home and End now scroll the
  transcript, Ctrl+T cycles the model variant, Ctrl+U deletes to the line
  start and Ctrl+Z suspends; CodeSmith keeps Tab and Shift+Tab for agents,
  Ctrl+O, Ctrl+X h and b, Ctrl+End and Tab in pickers as its own defaults.
  The help and every key hint show the bindings in effect.

- **The grouped transcript** (`cli/tui/session-verbosity` and the rest of
  its roadmap group): every tool call and thought had its own row. As in
  OpenCode, `tui.session.verbosity` (`low`, the default, `medium`, `high`)
  and `tui.session.grouping` now group them: at `low` each run of calls and
  thoughts is one summary row (`+ 3 commands, 1 edit, 2 thoughts`, `+
  Running command…`), at `medium` and `high` adjacent reads and searches
  are an `Explored` row (open at `high`), and hidden thoughts a `Thought`
  row. `session.verbosity.cycle`, `session.toggle.exploration_grouping` and
  `/verbosity` switch them. OpenCode opens one group with a click; CodeSmith
  has no mouse, so ctrl+o (its tool output key) opens them all. A running
  command's output and a running subagent's row stay in view below their
  collapsed group, as OpenCode keeps a call waiting for permission there.
  The level is not written back to the config (the UI never writes it).

- **The diff viewer and review panel** (`cli/tui/diff-viewer` and the
  rest of its roadmap group): there was none. `/diff` (or `diff.open`)
  now opens OpenCode's viewer, docked beside the session from 108 columns
  (`pane.focus.left/right` move the keys between the two) or full screen
  (`f`, remembered). Its sources are OpenCode's: `All` (the branch against
  its base, found from its reflog or the default branch, never guessed),
  `Committed`, `Uncommitted` and `Last turn` (from the snapshots undo
  keeps); the chooser (`d`) also picks the base among the branches, kept
  per branch until the UI exits. Files are cards with a file tree beside
  them (full screen, 88 columns and up), split or unified patches (`v`,
  split from a 100-column pane) with syntax colors, hunk (`[`/`]`) and file
  (`n`/`p`) navigation, review marks (`m`) with the count in the header,
  single-file view (`s`), sticky file titles, OpenCode's shortcuts dialog
  (`?`), and every key rebindable under its `diff.*` id. Only the rows in
  view are drawn, so a large diff costs what a small one does (0.1 ms a
  frame). Review marks stay in the viewer: OpenCode sends them nowhere,
  `/review` included. Differences: the layout choices are remembered beside
  the config rather than written to it (the UI never writes the config);
  OpenCode's mouse actions (its file menu, clicked folders and source
  label) and image previews have no counterpart; while a permission or
  question waits, `y` answers it once the prompt has the keys (a stray
  Enter rejects in CodeSmith, as before); TypeScript is highlighted as
  JavaScript (syntect has no TypeScript), and files over 5000 lines are
  shown without colors.
- **Question forms** (`cli/tui/form`, `util/form`): questions were asked
  one at a time with typed option numbers. They are now OpenCode's form: a
  tab per field and a Submit tab with a compact review, choices with a
  cursor and number keys, multi-select checkboxes, a custom answer row with
  its editor (typing or pasting on it opens it), text and number fields
  with OpenCode's validation messages, and external links to open or copy
  and acknowledge. The question tool's prompts become OpenCode's `q0`, `q1`,
  … fields; MCP elicitations reach the UI as whole forms (a `Questioner`
  may now take a form, and front ends that do not still get questions). A
  question that arrives while a picker or the help is open waits behind it.
  Differences: the form is the centred question dialog rather than the
  composer's place, and ctrl+v is the terminal's own paste.
- **Paste chips and prompt undo** (`prompt-paste`, `prompt/attachment`):
  long pastes went into the prompt whole, there was no undo in the
  prompt, and the same image pasted twice became two attachments. Long
  pastes (three lines, or over 150 characters) are now `[Pasted ~N lines]`
  chips sent in full while whole (`tui.prompt.paste: "full"` and
  `app.toggle.paste_summary` turn them off), `input.undo` and `input.redo`
  undo and redo prompt edits, and an image pasted again keeps its label.
- **Subagent and shell panel, output viewer** (`composer-keymap`,
  `dialog-shell-output`, `util/session`): background commands and
  subagents could only be seen in their transcript rows. OpenCode's
  composer panel now lists them (↓ on an empty prompt, or
  `session.child.first`): subagents nested as `sessionFamily` draws them,
  with the inactive filter, opening a subagent's session; background
  commands one line each, killed with `composer.shell.kill`; and the output
  viewer, which follows a command's output file (its latest 64 KiB from a
  whole character, escapes out), scrolls, copies the command or output, and
  says when the file cannot be read or is gone. Differences: ↓ opens the
  panel only when it has something to list; interrupting a subagent
  interrupts its turn (subagents run inside it); no mouse.
- **Toasts** (`cli/tui/toast`, `plugin-toast`): the failures OpenCode shows
  as session-scoped toasts were worked out and dropped, and copies said so
  in the transcript. Toasts now show at the top right, queued one at a
  time (`+N more`) for five seconds; one about a session outside the shown
  one's family is titled with it, and one over the cursor hides it.
- **Models by agent** (`context/local-selection`): switching agents kept
  the session's model. Each agent now takes the model and variant it last
  had in the session, else its configured model.
- **`@file` line ranges** (`prompt/parse`): `@file#12-20` named no file.
  It now attaches those lines, parsed as OpenCode's `parseFileLineRange`.
- **Shell mode label and failures** (`prompt/traits`,
  `prompt-shell-failure`): a `!command` prompt looked like any other, and
  one that could not run left only a failed row. The prompt is now labelled
  `SHELL` (OpenCode's prompt traits, which also say what the prompt
  captures), and a command that cannot run toasts and comes back in the
  prompt.
- **Goal in the prompt footer** (`feature-plugins/prompt-footer`): a
  session's `/goal` showed only through `/goal`. The status line now leads
  with `◎ goal` (muted), `◎ goal paused` (warning) or `◎ goal met`, kept
  current from the goal plugin's changes.
- **Prompt stash and path frecency** (`prompt/jsonl`, `prompt/history`):
  `prompt.stash` and `prompt.stash.pop` did nothing, and `@` listed files
  in name order. The stash now keeps 50 prompts in `prompt-stash.jsonl`,
  `frecency.jsonl` records each mentioned file and orders the `@` list
  before anything is typed, both parsed as OpenCode's (bad lines skipped,
  latest kept); a prompt restored after a fork follows the typed one.
- **MCP servers dialog** (`cli/tui/dialog-mcp`): `mcp.list` did nothing.
  It now lists the servers with OpenCode's status labels, starts the OAuth
  login of one that needs a sign-in (Enter or Space) and waits for it, and
  shows a failed one's error with `i` to open a new session drafting
  OpenCode's investigation prompt. A server signed in here reconnects
  right away (`Mcp::restart`), and its tools are offered from the next
  step.
- **A compaction that fails before it starts** (`cli/test/acp/translate`):
  a `/compact` with nothing to compact sent ACP clients nothing. Core now
  ends such a compaction without starting it, and the ACP sink opens and
  fails one for a client that renders `compaction_update`, or sends the
  failed marker alone to one that does not.
- **A replayed compaction with no summary** (`cli/test/acp/translate`): a
  provider's native compaction replayed with an empty summary text; it now
  clears the client's summary (`summary: null`), as a live one does.
- **Stats tables in a narrow terminal** (`cli/test/stats`): the model and
  tool tables kept their 68-column rows; below 68 columns each row is now
  two lines, the name and then its numbers, as in OpenCode. The width is
  the terminal's when `COLUMNS` is not set.
- **`mcp auth` without a name** (`cli/test/mcp-auth-choices`): the name
  was required. It is now optional: in a terminal CodeSmith lists the
  remote servers that take an OAuth sign-in and asks for one; without a
  terminal it fails before connecting, asking for the name.
- **`run --archived` archived only at the end** (`cli/test/run/archived`):
  a run that was stopped part way left its session in the lists. The
  session is now marked archived before its prompt runs (the mark is
  dropped if the run never stores the session).
- **Editor-supplied HTTP MCP servers used saved OAuth logins**
  (`cli/test/acp/session`): an `http` server in `mcpServers` was
  configured with OAuth on, so CodeSmith's saved login for that name and
  URL went with its requests. As in OpenCode (`oauth: false`), the
  editor's own headers are its credentials now.

## Differences found but not fixed

None: those the inventory found are fixed above.

## Ported tests

Code Mode's ported tests are the passing cases of `oc_codemode.rs`, by
OpenCode file: Test262 adaptations of arrays, strings, dates, generators,
promises, `for await`, labels, hoisting, lexical scope, Set methods, JSON
callbacks and `groupBy`; and `parity`, `stdlib`, `callbacks`, `classes`,
`accessors`, `descriptors`, `errors`, `enumeration` and `new-expression`.

| OpenCode file | Test | Status | CodeSmith test |
|---|---|---|---|
| `codemode/test/openapi` | covers a representative API from generation through execution | after a fix | oc_covers_a_representative_api_from_generation_through_execution (adapted: fields listed in name order) |
| `codemode/test/openapi` | generates supported operations and reports unsupported transports | after a fix | oc_generates_supported_operations_and_reports_unsupported_transports (adapted: skipped operations compared as a set) |
| `codemode/test/openapi` | preserves operation path sanitization and collision handling | after a fix | oc_preserves_operation_path_sanitization_and_collision_handling |
| `codemode/test/openapi` | does not reserve names for unsupported operations between duplicate operation IDs | after a fix | oc_does_not_reserve_names_for_unsupported_operations_between_duplicate_operation_ids |
| `codemode/test/openapi` | synthesizes flat operation IDs from methods and paths | after a fix | oc_synthesizes_flat_operation_ids_from_methods_and_paths |
| `codemode/test/openapi` | lets operation parameters override matching path parameters | after a fix | oc_lets_operation_parameters_override_matching_path_parameters |
| `codemode/test/openapi` | normalizes OpenAPI 3.0 schemas with Effect | after a fix | oc_normalizes_openapi_3_0_schemas |
| `codemode/test/openapi` | preserves schema-local definitions alongside component definitions | after a fix | oc_preserves_schema_local_definitions_alongside_component_definitions (adapted: a tool carries only the components it reaches) |
| `codemode/test/openapi` | projects read-only and write-only properties by schema direction | after a fix | oc_projects_read_only_and_write_only_properties_by_schema_direction (adapted: name order; the flattened request reaches no component) |
| `codemode/test/openapi` | projects directional annotations through local refs and allOf composition | after a fix | oc_projects_directional_annotations_through_local_refs_and_all_of_composition |
| `codemode/test/openapi` | honors declarations that are siblings of a $ref | after a fix | oc_honors_declarations_that_are_siblings_of_a_ref |
| `codemode/test/openapi` | honors directional declarations on intermediate reference hops | after a fix | oc_honors_directional_declarations_on_intermediate_reference_hops |
| `codemode/test/openapi` | projects cyclic component references without hanging | after a fix | oc_projects_cyclic_component_references_without_hanging (adapted: name order) |
| `codemode/test/openapi` | projects diamond-shaped reference graphs in linear time | after a fix | oc_projects_diamond_shaped_reference_graphs_in_linear_time |
| `codemode/test/openapi` | resolves hiding through reference cycles regardless of evaluation order | after a fix | oc_resolves_hiding_through_reference_cycles_regardless_of_evaluation_order |
| `codemode/test/openapi` | keeps not, if, and contains subschemas unprojected | after a fix | oc_keeps_not_if_and_contains_subschemas_unprojected |
| `codemode/test/openapi` | does not hide properties whose direction is declared only in anyOf or oneOf alternatives | after a fix | oc_does_not_hide_properties_whose_direction_is_declared_only_in_any_of_or_one_of_alternatives |
| `codemode/test/openapi` | does not misresolve shadowed local $defs when flattening body fields | after a fix | oc_does_not_misresolve_shadowed_local_defs_when_flattening_body_fields |
| `codemode/test/openapi` | projects directional annotations inside parameter schemas | after a fix | oc_projects_directional_annotations_inside_parameter_schemas |
| `codemode/test/openapi` | ignores inherited directional annotations | after a fix | oc_ignores_inherited_directional_annotations (adapted: JSON has no inherited keys; a property named `readOnly` does not hide its schema) |
| `codemode/test/openapi` | cleans required properties across allOf branches | after a fix | oc_cleans_required_properties_across_all_of_branches |
| `codemode/test/openapi` | keeps directional schemas model-facing while preserving runtime pass-through | after a fix | oc_keeps_directional_schemas_model_facing_while_preserving_runtime_pass_through |
| `codemode/test/openapi` | exposes generated operations through CodeMode discovery | after a fix | oc_exposes_generated_operations_through_code_mode_discovery |
| `codemode/test/openapi` | serializes supported simple and form parameter shapes | after a fix | oc_serializes_supported_simple_and_form_parameter_shapes (adapted: `[{}]` for `[undefined]`) |
| `codemode/test/openapi` | preserves ordered exploded and deep-object query parameters | after a fix | oc_preserves_ordered_exploded_and_deep_object_query_parameters (adapted: an object's keys in name order) |
| `codemode/test/openapi` | skips unsupported parameter encodings and malformed security | after a fix | oc_skips_unsupported_parameter_encodings_and_malformed_security |
| `codemode/test/openapi` | fails closed on prototype-named missing security schemes | after a fix | oc_fails_closed_on_prototype_named_missing_security_schemes |
| `codemode/test/openapi` | resolves bearer authentication without exposing it as input | after a fix | oc_resolves_bearer_authentication_without_exposing_it_as_input |
| `codemode/test/openapi` | applies authentication carriers without prototype or collision loss | after a fix | oc_applies_authentication_carriers_without_prototype_or_collision_loss |
| `codemode/test/openapi` | honors server precedence and rejects ambiguous base URLs | after a fix | oc_honors_server_precedence_and_rejects_ambiguous_base_urls (adapted: the operation's URL is checked, not requested) |
| `codemode/test/openapi` | resolves chained response refs before detecting unsupported transports | after a fix | oc_resolves_chained_response_refs_before_detecting_unsupported_transports |
| `codemode/test/openapi` | resolves response schemas before detecting binary output | after a fix | oc_resolves_response_schemas_before_detecting_binary_output |
| `codemode/test/openapi` | validates composite parameters before resolving auth | after a fix | oc_validates_composite_parameters_before_resolving_auth (adapted: `{value: {}}` for `{value: undefined}`) |
| `codemode/test/openapi` | preserves JSON media types and rejects unencodable bodies | after a fix | oc_preserves_json_media_types (adapted: JSON cannot hold the cyclic body; that half is dropped) |
| `codemode/test/openapi` | rejects oversized and malformed JSON responses | after a fix | oc_rejects_oversized_and_malformed_json_responses (adapted: a 10 MiB cap) |
| `codemode/test/openapi` | keeps non-JSON responses raw and unions every success output | after a fix | oc_keeps_non_json_responses_raw_and_unions_every_success_output |
| `codemode/test/openapi` | fails missing required parameters before auth and network | after a fix | oc_fails_missing_required_parameters_before_auth_and_network |
| `codemode/test/openapi` | prefixes cross-location collisions and reconstructs the HTTP request | after a fix | oc_prefixes_cross_location_collisions_and_reconstructs_the_http_request (adapted: the tools under a namespace; name order) |
| `codemode/test/openapi` | keeps bodies nested when flattening would lose schema semantics | after a fix | oc_keeps_bodies_nested_when_flattening_would_lose_schema_semantics |
| `cli/test/env` | session environment omits server credentials | after a fix | oc_shell_commands_do_not_see_the_server_token; also local_mcp_servers_do_not_see_the_server_token (e2e), local_mcp_servers_do_not_inherit_the_server_token, language_servers_do_not_inherit_the_server_token |
| `cli/test/flag` | parses $args (3 cases) | after a fix | oc_flag_parses_an_optional_worktree_value |
| `cli/test/flag` | renders the value as optional in help | after a fix | oc_flag_renders_the_value_as_optional_in_help |
| `tui/test/prompt-offset` | history navigation moves the cursor to the end of wide text | after a fix | oc_history_navigation_moves_the_cursor_to_the_end_of_wide_text (fix: cursor moves keep the history place) |
| `tui/test/prompt-paste` | the prompt keeps pasted control characters and drops escape sequences | after a fix | oc_paste_keeps_control_characters_and_drops_escape_sequences |
| `tui/test/cli/tui/dialog-select` | ctrl+c clears a dialog filter before closing the dialog | after a fix | oc_ctrl_c_clears_a_picker_filter_before_closing_it |
| `tui/test/prompt/history-composer` | down past the newest entry restores the draft text and shell mode | after a fix | oc_down_past_the_newest_entry_restores_the_draft |
| `tui/test/prompt/local-attachment` | parses multi-file drops from POSIX, URI-list, and Windows terminals | after a fix | oc_multi_file_drops_..., oc_a_single_image_path_..., oc_quoted_paths_... |
| `tui/test/prompt/local-attachment` | resolves a single image path before splitting spaces | after a fix | oc_multi_file_drops_..., oc_a_single_image_path_..., oc_quoted_paths_... |
| `tui/test/prompt/local-attachment` | resolves quoted paths and URI lists as ordered attachments | after a fix | oc_multi_file_drops_..., oc_a_single_image_path_..., oc_quoted_paths_... |
| `tui/test/util/locale` | truncates text from the right by terminal width | after a fix | oc_cut_truncates_from_the_right_by_terminal_width |
| `tui/test/util/locale` | takes whole graphemes within terminal width | after a fix | oc_take_keeps_whole_graphemes_within_terminal_width (adapted) |
| `tui/test/util/locale` | rolls the compact form over at the rounding boundary | after a fix | oc_token_counts_roll_over_at_the_rounding_boundary (adapted: `1M` for `1.0M`) |
| `tui/test/theme/v2/resolve` | orders light hues dark-to-light and dark hues light-to-dark | after a fix | oc_orders_light_hues_dark_to_light_and_dark_hues_light_to_dark |
| `tui/test/theme/v2/resolve` | resolves complete one-mode documents in the available mode | after a fix | oc_resolves_complete_one_mode_documents_in_the_available_mode |
| `tui/test/theme/v2/resolve` | rejects theme documents without a mode | after a fix | oc_rejects_theme_documents_without_a_mode |
| `tui/test/theme/v2/resolve` | validates and resolves categorical hues in configured order | after a fix | oc_validates_and_resolves_categorical_hues_in_configured_order |
| `tui/test/theme/v2/resolve` | resolves arbitrary hue names across aliases, categorical colors, and token references | after a fix | oc_resolves_arbitrary_hue_names_across_aliases_categorical_colors_and_token_references |
| `tui/test/theme/v2/resolve` | validates hue relationships in every provided mode while parsing | after a fix | oc_validates_hue_relationships_in_every_provided_mode_while_parsing |
| `tui/test/theme/v2/resolve` | generates syntax with one categorical hue | after a fix | oc_generates_syntax_with_one_categorical_hue (adapted: a syntect theme) |
| `tui/test/theme/v2/resolve` | rejects incomplete themes instead of merging defaults | after a fix | oc_rejects_incomplete_themes_instead_of_merging_defaults |
| `tui/test/theme/v2/resolve` | resolves independent definitions and hue aliases | after a fix | oc_resolves_independent_definitions_and_hue_aliases |
| `tui/test/theme/v2/resolve` | resolves base hue aliases and rejects circular hue aliases | after a fix | oc_resolves_base_hue_aliases_and_rejects_circular_hue_aliases |
| `tui/test/theme/v2/resolve` | steps by hue source when adjacent colors have equal values | after a fix | oc_steps_by_hue_source_when_adjacent_colors_have_equal_values |
| `tui/test/theme/v2/resolve` | resolves complete light and dark definitions independently | after a fix | oc_resolves_complete_light_and_dark_definitions_independently |
| `tui/test/theme/v2/resolve` | expands structural state fallbacks within a complete theme | after a fix | oc_expands_structural_state_fallbacks_within_a_complete_theme |
| `tui/test/theme/v2/resolve` | resolves matched action variants and states | after a fix | oc_resolves_matched_action_variants_and_states |
| `tui/test/theme/v2/resolve` | defaults faint text to muted unless a theme sets it | after a fix | oc_defaults_faint_text_to_muted_unless_a_theme_sets_it |
| `tui/test/theme/v2/resolve` | resolves dialog surfaces from direct colors | after a fix | oc_resolves_dialog_surfaces_from_direct_colors |
| `tui/test/theme/v2/resolve` | resolves transparent colors | after a fix | oc_resolves_transparent_colors |
| `tui/test/theme/v2/resolve` | reports theme decoding failures as native errors | after a fix | oc_reports_theme_decoding_failures_as_native_errors |
| `tui/test/theme/v2/resolve` | surface overrides rewire references and reset action states from their base | after a fix | oc_surface_overrides_rewire_references_and_reset_action_states_from_their_base |
| `tui/test/theme/v2/resolve` | dialog footer overrides layer on the dialog surface | after a fix | oc_dialog_footer_overrides_layer_on_the_dialog_surface |
| `tui/test/theme/v2/resolve` | rejects missing and circular references | after a fix | oc_rejects_missing_and_circular_references |
| `tui/test/theme/v2/resolve` | validates complete hues, resolved groups, and hue-only syntax | after a fix | oc_validates_complete_hues_resolved_groups_and_hue_only_syntax |
| `tui/test/theme/v2/v1-migrate` | migrates resolved V1 modes into V2 tokens | after a fix | oc_migrates_resolved_v1_modes_into_v2_tokens |
| `tui/test/theme/v2/v1-migrate` | references generated hues from matching token colors | after a fix | oc_references_generated_hues_from_matching_token_colors |
| `tui/test/theme/v2/v1-migrate` | infers and emits only chromatic hues represented by V1 colors | after a fix | oc_infers_and_emits_only_chromatic_hues_represented_by_v1_colors |
| `tui/test/theme/v2/v1-migrate` | orders categorical hues by V1 semantic color mapping | after a fix | oc_orders_categorical_hues_by_v1_semantic_color_mapping |
| `tui/test/theme/v2/v1-migrate` | gives accent and primary ownership of their inferred hues | after a fix | oc_gives_accent_and_primary_ownership_of_their_inferred_hues |
| `tui/test/theme/v2/v1-migrate` | uses the semantic neutral hue when V1 categorical colors are ambiguous | after a fix | oc_uses_the_semantic_neutral_hue_when_v1_categorical_colors_are_ambiguous |
| `tui/test/theme/v2/v1-migrate` | builds and extrapolates gray from V1 surfaces and text without using menus or borders | after a fix | oc_builds_and_extrapolates_gray_from_v1_surfaces_and_text_without_using_menus_or_borders |
| `tui/test/theme/v2/v1-migrate` | uses the base text reference for primary actions on transparent backgrounds | after a fix | oc_uses_the_base_text_reference_for_primary_actions_on_transparent_backgrounds |
| `tui/test/theme/v2/v1-migrate` | retains V1 circular reference errors | after a fix | oc_retains_v1_circular_reference_errors |
| `tui/test/theme/v2/v1-migrate` | migrates every built-in V1 theme in its supported modes | after a fix | oc_migrates_every_built_in_v1_theme_in_its_supported_modes |
| `tui/test/theme/v2/v1-migrate` | collapses identical V1 backgrounds when both variants infer one mode | after a fix | oc_collapses_identical_v1_backgrounds_when_both_variants_infer_one_mode |
| `tui/test/theme/v2/v1-migrate` | gives dialogs an opaque background when V1 panels are see-through | after a fix | oc_gives_dialogs_an_opaque_background_when_v1_panels_are_see_through |
| `tui/test/theme/v2/v1-migrate` | keeps both modes when a shared background has different contrast | after a fix | oc_keeps_both_modes_when_a_shared_background_has_different_contrast |
| `tui/test/theme` | rejects unrecognized theme structures | after a fix | oc_rejects_unrecognized_theme_structures |
| `tui/test/theme` | registers opencode as a native V2 theme | after a fix | oc_registers_opencode_as_a_native_v2_theme |
| `tui/test/theme` | keeps the experimental opencode2 theme out of the global theme list | after a fix | oc_keeps_the_experimental_opencode2_theme_out_of_the_global_theme_list |
| `tui/test/theme` | detects V1 themes from their theme field and caches migrations | after a fix | oc_detects_v1_themes_from_their_theme_field_and_caches_migrations (adapted: no cache, parsing is pure) |
| `tui/test/theme` | decodes native V2 themes lazily once | after a fix | oc_decodes_native_v2_themes_lazily_once (adapted: no cache) |
| `tui/test/theme` | rejects invalid V2 themes when parsing | after a fix | oc_rejects_invalid_v2_themes_when_parsing |
| `tui/test/theme` | rejects invalid V1 themes when parsing | after a fix | oc_rejects_invalid_v1_themes_when_parsing |
| `tui/test/theme` | replacement sources receive independent parse caches | after a fix | oc_replacement_sources_receive_independent_parse_caches (adapted: no cache) |
| `tui/test/theme` | hasTheme checks theme presence | after a fix | oc_has_theme_checks_theme_presence |
| `tui/test/theme` | resolveTheme rejects circular color refs | after a fix | oc_resolve_theme_rejects_circular_color_refs |
| `tui/test/theme` | resolveTheme preserves full theme numeric color and marker semantics | after a fix | oc_resolve_theme_preserves_full_theme_numeric_color_and_marker_semantics (adapted: no color intent) |
| `tui/test/theme` | terminalMode derives mode from refreshed background | after a fix | oc_terminal_mode_derives_mode_from_refreshed_background |
| `tui/test/theme` | terminalMode does not derive mode from ANSI slot zero | after a fix | oc_terminal_mode_does_not_derive_mode_from_ansi_slot_zero |
| `tui/test/theme` | custom theme precedence follows directory order | after a fix | oc_custom_theme_precedence_follows_directory_order |
| `tui/test/theme` | theme directories include global config before project directories | after a fix | oc_theme_directories_include_global_config_before_project_directories |
| `tui/test/cli/tui/theme-mode` | uses an available mode while retaining the pinned preference | after a fix | oc_uses_an_available_mode_while_retaining_the_pinned_preference |
| `tui/test/cli/tui/theme-mode` | falls back to OpenCode when configured V2 theme %s is invalid (3 cases) | after a fix | oc_falls_back_to_opencode_when_configured_v2_theme_*_is_invalid |
| `tui/test/cli/tui/theme-mode` | dialog surfaces are absolute and can be inherited through the theme context | after a fix | oc_dialog_surfaces_are_absolute_and_can_be_inherited_through_the_theme_context |
| `tui/test/context/theme` | the system theme follows a palette reply that arrives after the query | after a fix | oc_the_system_theme_follows_a_palette_reply_that_arrives_after_the_query (adapted) |
| `tui/test/theme/v2/select` | selects complete light and dark themes independently | after a fix | oc_selects_complete_light_and_dark_themes_independently |
| `tui/test/theme/v2/select` | selects the available mode when the requested mode is missing | after a fix | oc_selects_the_available_mode_when_the_requested_mode_is_missing |
| `tui/test/theme/v2/component` | provides reactive properties, states, surfaces, and color operations | after a fix | oc_provides_reactive_properties_states_surfaces_and_color_operations (adapted: no signals) |
| `tui/test/config-v2` | resolves nested config and keybind defaults | after a fix | oc_resolves_nested_config_and_keybind_defaults (adapted: the scroll, diffs, debug and session halves are dropped) |
| `tui/test/config-v2` | uses command IDs as keybind keys | after a fix | oc_uses_command_ids_as_keybind_keys |
| `tui/test/config-v2` | preserves current navigation defaults | after a fix | oc_preserves_current_navigation_defaults (adapted: ctrl+x shift+t reopens a tab too; the open menu is ctrl+x o, as ctrl+o is CodeSmith's tool output key) |
| `tui/test/cli/tui/dialog-open` | selecting an unhydrated session preserves its location | after a fix | oc_selecting_an_unhydrated_session_preserves_its_location (adapted: a session elsewhere restarts the UI in its folder) |
| `tui/test/cli/tui/dialog-open` | finds and opens an exact child session ID outside the recent list | after a fix | oc_finds_and_opens_an_exact_child_session_id_outside_the_recent_list |
| `tui/test/cli/tui/dialog-open` | shows the current project and opens its root | after a fix | oc_shows_the_current_project_and_opens_its_root |
| `tui/test/cli/tui/dialog-open` | shows only canonical projects while retaining sessions from other directories | after a fix | oc_shows_only_canonical_projects_while_retaining_sessions_from_other_directories |
| `tui/test/cli/tui/dialog-open` | keeps nested Git session directories out of projects and in their session footer | after a fix | oc_keeps_nested_git_session_directories_out_of_projects_and_in_their_session_footer |
| `tui/test/cli/tui/dialog-open` | loads Git worktrees only when drilling into the canonical project | after a fix | oc_loads_git_worktrees_only_when_drilling_into_the_canonical_project |
| `tui/test/cli/tui/dialog-open` | does not show or trigger worktree navigation for non-Git and global directories | after a fix | oc_does_not_show_or_trigger_worktree_navigation_for_non_git_and_global_directories |
| `tui/test/cli/tui/dialog-open` | does not show the previous project's worktrees while loading another project | after a fix | oc_does_not_show_the_previous_projects_worktrees_while_loading_another_project (adapted: projects come from the sessions, so alt+down passes the project's session first) |
| `tui/test/cli/tui/dialog-open` | creates a worktree named '%s' by project and opens its local directory (2 cases) | after a fix | oc_creates_a_worktree_named_by_project_and_opens_its_local_directory (adapted: the mouse selection half is dropped) |
| `tui/test/cli/tui/dialog-open` | shows projects while sessions refresh and preserves the selected project | after a fix | oc_shows_projects_while_sessions_refresh_and_preserves_the_selected_project (adapted: the projects are the last list's while it refreshes) |
| `tui/test/cli/tui/dialog-open` | keeps a filtered selection visible after refresh with query reset %s (2 cases) | after a fix | oc_keeps_a_filtered_selection_visible_after_refresh_with_query_reset (adapted: after the reset, the top row is the first project's session) |
| `tui/test/cli/tui/dialog-open` | sessions remain selectable while projects are still loading | after a fix | oc_sessions_remain_selectable_while_projects_are_still_loading (adapted: one read brings both) |
| `tui/test/cli/tui/dialog-open` | shows hydrated sessions immediately without waiting for either read | after a fix | oc_shows_hydrated_sessions_immediately_without_waiting_for_either_read |
| `tui/test/cli/tui/dialog-open` | keeps the previous recent list usable when reopening fails to refresh | after a fix | oc_keeps_the_previous_recent_list_usable_when_reopening_fails_to_refresh |
| `tui/test/cli/tui/dialog-open` | shows an initial loading shell instead of reporting an empty list | after a fix | oc_shows_an_initial_loading_shell_instead_of_reporting_an_empty_list |
| `tui/test/cli/tui/dialog-open` | reports both refresh failures while keeping hydrated sessions usable | after a fix | oc_reports_both_refresh_failures_while_keeping_hydrated_sessions_usable |
| `tui/test/cli/tui/dialog-open` | option arrows jump between sections | after a fix | oc_option_arrows_jump_between_sections |
| `tui/test/cli/tui/dialog-open` | option arrows stay in the only visible section | after a fix | oc_option_arrows_stay_in_the_only_visible_section |
| `tui/test/cli/tui/dialog-open` | surfaces sessions awaiting permissions or questions at the top with attention icons | after a fix | oc_surfaces_sessions_awaiting_permissions_or_questions_at_the_top_with_attention_icons (adapted: the parent session is the one asking) |
| `tui/test/cli/tui/prompt-move` | passes the current project and uses server worktree defaults for $name (5 cases) | after a fix | oc_passes_the_current_project_and_uses_server_worktree_defaults (adapted: the project is found on disk from the folder) |
| `tui/test/cli/tui/prompt-move` | uses Home's selected location instead of launch in $name (2 cases) | after a fix | oc_uses_homes_selected_location_instead_of_launch (adapted: the launch folder is never read) |
| `tui/test/cli/tui/prompt-move` | selecting a workspace opens Home without moving a session (home=%s) (2 cases) | after a fix | oc_selecting_a_workspace_opens_home_without_moving_a_session |
| `tui/test/cli/tui/prompt-move` | removal sends project ownership and the destination without a configuration location | after a fix | oc_removal_sends_project_ownership_and_the_destination_without_a_configuration_location |
| `tui/test/cli/tui/prompt-move` | refresh explicitly scans the project and preserves the worktree filter | after a fix | oc_refresh_explicitly_scans_the_project_and_preserves_the_worktree_filter |
| `tui/test/cli/tui/prompt-move` | Ctrl+M moves only an existing session (home=%s) (2 cases) | after a fix | oc_ctrl_m_moves_only_an_existing_session |
| `tui/test/cli/tui/prompt-move` | choosing a directory recovers the session when its location is unavailable | after a fix | oc_choosing_a_directory_recovers_the_session_when_its_location_is_unavailable |
| `tui/test/cli/tui/prompt-move` | creating a worktree recovers the session without reading its removed location | after a fix | oc_creating_a_worktree_recovers_the_session_without_reading_its_removed_location |
| `tui/test/cli/tui/prompt-move` | failed recovery does not navigate away from the session | after a fix | oc_failed_recovery_does_not_navigate_away_from_the_session (adapted: the store move fails and the session stays) |
| `tui/test/cli/tui/prompt-move` | does not create from another clone when $name lookup fails (2 cases) | after a fix | oc_does_not_create_from_another_clone_when_lookup_fails (adapted: Git fails to make the worktree) |
| `tui/test/cli/tui/prompt-move` | does not open creation when the selected Home location lookup fails | after a fix | oc_does_not_open_creation_when_the_selected_home_location_lookup_fails |
| `tui/test/cli/tui/dialog-session-list` | scopes sessions to the active session location | after a fix | oc_scopes_sessions_to_the_active_session_location |
| `tui/test/config-v2` | shows the new session location default in settings | after a fix | oc_shows_the_new_session_location_default_in_settings |
| `tui/test/config-v2` | preserves migrated v1 keybind defaults | after a fix | oc_preserves_migrated_v1_keybind_defaults |
| `tui/test/config-v2` | accepts every v2-only named command ID | after a fix | oc_accepts_every_v2_only_named_command_id |
| `tui/test/config-v2` | centralizes named command defaults and resolves explicit none | after a fix | oc_centralizes_named_command_defaults_and_resolves_explicit_none |
| `tui/test/config-v2` | retired diff tree keybinds remain accepted but have no default bindings | after a fix | oc_retired_diff_tree_keybinds_remain_accepted_but_have_no_default_bindings |
| `tui/test/config-v2` | rejects orphaned keybind definitions | after a fix | oc_rejects_orphaned_keybind_definitions |
| `tui/test/config-v2` | uses ctrl+z for input undo when terminal suspend is unavailable | after a fix | oc_uses_ctrl_z_for_input_undo_when_terminal_suspend_is_unavailable |
| `tui/test/config` | validates config constraints | after a fix | oc_validates_config_constraints (adapted: the attention, prompt size, scroll and diff style halves are dropped) |
| `tui/test/config` | resolves host-neutral defaults | after a fix | oc_resolves_host_neutral_defaults (adapted: the attention and mouse halves are dropped) |
| `tui/test/config` | resolves overrides without mutating input | after a fix | oc_resolves_overrides_without_mutating_input (adapted: the attention and mouse halves are dropped) |
| `tui/test/config` | resolves a session move keybind | after a fix | oc_resolves_a_session_move_keybind |
| `tui/test/config` | resolves message navigation defaults | after a fix | oc_resolves_message_navigation_defaults (adapted: OpenCode's current table adds kphome) |
| `tui/test/config` | reserves home and end for navigation | after a fix | oc_reserves_home_and_end_for_navigation |
| `tui/test/config` | opens the subagent picker with down | after a fix | oc_opens_the_subagent_picker_with_down |
| `tui/test/config` | navigates session tabs with option arrows | after a fix | oc_navigates_session_tabs_with_option_arrows |
| `tui/test/config` | preserves pinned session bindings alongside tab bindings | after a fix | oc_preserves_pinned_session_bindings_alongside_tab_bindings |
| `tui/test/config` | disables suspend and assigns ctrl+z to undo when unsupported | after a fix | oc_disables_suspend_and_assigns_ctrl_z_to_undo_when_unsupported |
| `tui/test/config` | preserves an explicit undo binding when suspend is unsupported | after a fix | oc_preserves_an_explicit_undo_binding_when_suspend_is_unsupported |
| `cli/test/config` | merges inline CLI config content over the global config | after a fix | oc_merges_inline_cli_config_content_over_the_global_config (adapted: tui in codesmith.json; the tabs, plugins and update halves are dropped) |
| `cli/test/config` | migrates tui and kv config into cli.json | after a fix | oc_migrates_tui_and_kv_config_into_cli_json (adapted: no kv.json, no file written) |
| `cli/test/config` | preserves legacy cursor settings | after a fix | oc_preserves_legacy_cursor_settings |
| `tui/test/context/session-tabs-model` | appends the branch to the project detail | after a fix | oc_appends_the_branch_to_the_project_detail |
| `tui/test/context/session-tabs-model` | labels tabs by ordinal | after a fix | oc_labels_tabs_by_ordinal |
| `tui/test/context/session-tabs-model` | moves a tab to a clamped index and returns the same tabs for no-ops | after a fix | oc_moves_a_tab_to_a_clamped_index_and_returns_the_same_tabs_for_no_ops (adapted: a no-op returns `false`) |
| `tui/test/context/session-tabs-model` | overflow markers reserve room for a gap beside their digits | after a fix | oc_overflow_markers_reserve_room_for_a_gap_beside_their_digits |
| `tui/test/context/session-tabs-model` | opens each session once and refreshes its title | after a fix | oc_opens_each_session_once_and_refreshes_its_title |
| `tui/test/context/session-tabs-model` | selects the right tab then the left tab after closing | after a fix | oc_selects_the_right_tab_then_the_left_tab_after_closing |
| `tui/test/context/session-tabs-model` | closing an unknown session returns the same tabs reference | after a fix | oc_closing_an_unknown_session_returns_the_same_tabs_reference (adapted: it returns `false`) |
| `tui/test/context/session-tabs-model` | closing an already removed tab picks the neighbour at its last index | after a fix | oc_closing_an_already_removed_tab_picks_the_neighbour_at_its_last_index |
| `tui/test/context/session-tabs-model` | cycles through a filtered tab set in either direction | after a fix | oc_cycles_through_a_filtered_tab_set_in_either_direction |
| `tui/test/context/session-tabs-model` | cycles to the nearest matching tab from an unmatched active tab | after a fix | oc_cycles_to_the_nearest_matching_tab_from_an_unmatched_active_tab |
| `tui/test/context/session-tabs-model` | moves backward and forward through selection history | after a fix | oc_moves_backward_and_forward_through_selection_history |
| `tui/test/context/session-tabs-model` | truncates forward history after a new selection | after a fix | oc_truncates_forward_history_after_a_new_selection |
| `tui/test/context/session-tabs-model` | skips closed tabs and duplicate entries for the active tab | after a fix | oc_skips_closed_tabs_and_duplicate_entries_for_the_active_tab |
| `tui/test/context/session-tabs-model` | drops the oldest history entries beyond the limit | after a fix | oc_drops_the_oldest_history_entries_beyond_the_limit |
| `tui/test/context/session-tabs-model` | returns to the latest history entry when no tab is active | after a fix | oc_returns_to_the_latest_history_entry_when_no_tab_is_active |
| `tui/test/context/session-tabs-model` | returns to the previous selected open tab after closing the active tab | after a fix | oc_returns_to_the_previous_selected_open_tab_after_closing_the_active_tab |
| `tui/test/context/session-tabs-model` | reopens the most recently closed tab at its original position | after a fix | oc_reopens_the_most_recently_closed_tab_at_its_original_position |
| `tui/test/context/session-tabs-model` | reopens a selected closed tab without consuming newer entries | after a fix | oc_reopens_a_selected_closed_tab_without_consuming_newer_entries |
| `tui/test/context/session-tabs-model` | skips and consumes closed entries that are already open | after a fix | oc_skips_and_consumes_closed_entries_that_are_already_open |
| `tui/test/context/session-tabs-model` | clamps restored positions and keeps one entry per session | after a fix | oc_clamps_restored_positions_and_keeps_one_entry_per_session |
| `tui/test/context/session-tabs-model` | expands the active tab and keeps inactive widths equal | after a fix | oc_expands_the_active_tab_and_keeps_inactive_widths_equal |
| `tui/test/context/session-tabs-model` | reserves an active tab slot for the new session page | after a fix | oc_reserves_an_active_tab_slot_for_the_new_session_page |
| `tui/test/context/session-tabs-model` | keeps the visible tab window stable on the new session page | after a fix | oc_keeps_the_visible_tab_window_stable_on_the_new_session_page |
| `tui/test/context/session-tabs-model` | only swaps old and new active width inside a sticky window | after a fix | oc_only_swaps_old_and_new_active_width_inside_a_sticky_window |
| `tui/test/context/session-tabs-model` | shares roomy width equally without changing widths on selection | after a fix | oc_shares_roomy_width_equally_without_changing_widths_on_selection |
| `tui/test/context/session-tabs-model` | caps a single tab instead of stretching it across the terminal | after a fix | oc_caps_a_single_tab_instead_of_stretching_it_across_the_terminal |
| `tui/test/context/session-tabs-model` | fills roomy space equally below the maximum width | after a fix | oc_fills_roomy_space_equally_below_the_maximum_width |
| `tui/test/context/session-tabs-model` | expands only the active tab under compact pressure | after a fix | oc_expands_only_the_active_tab_under_compact_pressure |
| `tui/test/context/session-tabs-model` | moves the window only after selection crosses its edge | after a fix | oc_moves_the_window_only_after_selection_crosses_its_edge |
| `tui/test/context/session-tabs` | loads persisted tab metadata concurrently on connect | after a fix | oc_loads_persisted_tab_metadata (adapted: the kept tab list, per folder or shared, in OpenCode's file shape) |
| `tui/test/context/session-tabs` | keeps each visited session open | after a fix | oc_keeps_each_visited_session_open (e2e) |
| `tui/test/context/session-tabs` | tracks a temporary new session tab across close and creation | after a fix | oc_keeps_each_visited_session_open (e2e; adapted: keys rather than routes) |
| `tui/test/context/session-tabs` | keeps scroll anchors for open session tabs | after a fix | oc_keeps_scroll_anchors_for_open_session_tabs (adapted: each tab's app keeps its scroll and draft) |
| `tui/test/context/session-tabs` | derives unread state from server session times | after a fix | oc_derives_unread_state_from_session_times (adapted: the store's update time against when the tab was shown); a_background_tab_keeps_its_commands_and_resumes_by_itself (e2e) |
| `tui/test/context/session-tabs` | marks unread failed sessions with error styling | after a fix | oc_marks_unread_failed_sessions_with_error_styling |
| `tui/test/context/session-tabs` | distinguishes family questions and permissions without clearing them on selection | after a fix | oc_distinguishes_family_questions_and_permissions_without_clearing_them_on_selection (adapted: subagents ask through their session's dialog); a_background_tabs_permission_request_is_surfaced (e2e) |
| `tui/test/component/session-tabs-status` | ${orientation} tabs replace ordinals with status without moving titles and keep context menu actions (2 cases) | after a fix | oc_horizontal_tabs_replace_ordinals_with_status_without_moving_titles, oc_vertical_tabs_replace_ordinals_with_status_without_moving_titles (adapted: no mouse, so no context menu) |
| `tui/test/config-v2` | validates the session tabs setting | after a fix | oc_validates_the_session_tabs_setting (adapted: the prompt and session halves are dropped) |
| `tui/test/config-v2` | resolves automatic tabs from the terminal environment | after a fix | oc_resolves_automatic_tabs_from_the_terminal_environment |
| `tui/test/config-v2` | shows resolved tab defaults in settings | after a fix | oc_shows_resolved_tab_defaults_in_settings (adapted: no settings dialog) |
| `tui/test/app-lifecycle` | vertical session tabs switch to horizontal below readable content width | after a fix | oc_vertical_session_tabs_switch_to_horizontal_below_readable_content_width (adapted: frames drawn at 120 and 100 columns) |
| `cli/test/config` | migrates legacy keybind names in an existing cli.json | after a fix | oc_migrates_legacy_keybind_names_in_an_existing_cli_json (adapted: read without rewriting) |
| `cli/test/config` | uses migrated keybinds when persistence fails | after a fix | oc_uses_migrated_keybinds_when_persistence_fails (adapted: never persisted) |
| `cli/test/config` | preserves the effective value when migrating duplicate legacy keybinds | after a fix | oc_preserves_the_effective_value_when_migrating_duplicate_legacy_keybinds |
| `cli/test/config` | migrates and updates the effective duplicate top-level keybinds | after a fix | oc_migrates_and_updates_the_effective_duplicate_top_level_keybinds (the update half is dropped) |
| `cli/test/config` | updates effective duplicate canonical keybinds | after a fix | oc_updates_effective_duplicate_canonical_keybinds (the update half is dropped) |
| `cli/test/config` | removes orphaned keybinds without deleting trailing comments | after a fix | oc_removes_orphaned_keybinds_without_deleting_trailing_comments |
| `tui/test/keymap` | legacy page key aliases compile as page keys | after a fix | oc_legacy_page_key_aliases_compile_as_page_keys |
| `tui/test/keymap` | formats navigation keys as arrows | after a fix | oc_formats_navigation_keys_as_arrows |
| `tui/test/keymap` | returns every formatted command shortcut | after a fix | oc_returns_every_formatted_command_shortcut (adapted: a configured binding) |
| `tui/test/cli/tui/dialog-prompt` | dialog prompt submit wins when return is also input newline | after a fix | oc_dialog_prompt_submit_wins_when_return_is_also_input_newline (adapted: the question dialog) |
| `tui/test/cli/tui/dialog-prompt` | alt return inserts a newline with default keybinds | after a fix | oc_alt_return_inserts_a_newline_with_default_keybinds (adapted: the prompt) |
| `tui/test/cli/tui/dialog-prompt` | dialog prompt submit can be rebound separately from input submit | after a fix | oc_dialog_prompt_submit_can_be_rebound_separately_from_input_submit (adapted: the question dialog) |
| `tui/test/cli/tui/dialog-prompt` | dialog prompt submit can be disabled | after a fix | oc_dialog_prompt_submit_can_be_disabled (adapted: the question dialog) |
| `tui/test/cli/tui/dialog-prompt` | dialog prompt renders block cursor and survives cursor hot reloads | after a fix | oc_dialog_prompt_renders_block_cursor_and_survives_cursor_hot_reloads (adapted: set at start; the hot-reload half is dropped) |
| `tui/test/cli/tui/reasoning-body` | reasoning follows the markdown setting, also when it changes | after a fix | oc_reasoning_follows_the_markdown_setting_also_when_it_changes (adapted) |
| `tui/test/cli/tui/session-verbosity` | reasoning and exploration group at every level; other tools stand alone | after a fix | oc_reasoning_and_exploration_group_at_every_level_other_tools_stand_alone |
| `tui/test/cli/tui/session-verbosity` | medium and high add web tools to exploration and group instruction loads | after a fix | oc_medium_and_high_add_web_tools_to_exploration (adapted: the instruction half is dropped; the transcript does not show instruction loads) |
| `tui/test/cli/tui/session-verbosity` | low wraps tools, thoughts and instruction loads in activity; text and other messages break it | after a fix | oc_low_wraps_tools_and_thoughts_in_activity_text_breaks_it (adapted: the instruction half is dropped) |
| `tui/test/cli/tui/session-verbosity` | low hydration nests subgroups inside one activity group per run | after a fix | oc_low_hydration_nests_subgroups_inside_one_activity_group_per_run (adapted: no instruction load) |
| `tui/test/cli/tui/session-verbosity` | medium hydration keeps production shape plus instruction groups | after a fix | oc_medium_hydration_keeps_production_shape (adapted: the instruction group is dropped) |
| `tui/test/cli/tui/session-verbosity` | live appends under low merge into the open activity group | after a fix | oc_live_appends_under_low_merge_into_the_open_activity_group (adapted: through session events) |
| `tui/test/cli/tui/session-verbosity` | permission-blocked tools inside an activity group are tracked as pending without reordering | after a fix | oc_permission_blocked_tools_inside_an_activity_group_are_tracked_as_pending_without_reordering (adapted: a running call stays in place and in view) |
| `tui/test/cli/tui/session-verbosity` | activity summary counts finished work by category | after a fix | oc_activity_summary_counts_finished_work_by_category (adapted: no instructions to count) |
| `tui/test/cli/tui/session-verbosity` | running work is active but not counted, and failed tools are counted | after a fix | oc_running_work_is_active_but_not_counted_and_failed_tools_are_counted |
| `tui/test/cli/tui/session-verbosity` | activity summary counts distinct instruction files and skips redacted thoughts | after a fix | oc_activity_summary_skips_redacted_thoughts (adapted: the instruction-file half is dropped) |
| `tui/test/cli/tui/session-verbosity` | questions stand alone at every level and end an activity run | after a fix | oc_questions_stand_alone_at_every_level, oc_questions_stand_alone_at_every_level_and_end_an_activity_run |
| `tui/test/cli/tui/session-verbosity` | a thought still streaming counts as finished once a later row closes its group | after a fix | oc_a_thought_still_streaming_counts_as_finished_once_a_later_row_closes_its_group |
| `tui/test/cli/tui/session-verbosity` | until something finishes, the label is the first running item's status | after a fix | oc_until_something_finishes_the_label_is_the_first_running_items_status (both modules) |
| `tui/test/cli/tui/session-verbosity` | busy labels depend only on the tool and whether it is still being prepared | after a fix | oc_busy_labels_depend_only_on_the_tool_and_whether_it_is_still_being_prepared |
| `tui/test/cli/tui/session-verbosity` | a finished execute with no nested calls counts as one tool | after a fix | oc_a_finished_execute_with_no_nested_calls_counts_as_one_tool |
| `tui/test/cli/tui/grouping-tree` | groups adjacent entries by path and counts leaves, not wrappers | after a fix | oc_groups_adjacent_entries_by_path_and_counts_leaves_not_wrappers |
| `tui/test/cli/tui/grouping-tree` | a direct child breaks a subgroup without ending the outer group | after a fix | oc_a_direct_child_breaks_a_subgroup_without_ending_the_outer_group |
| `tui/test/cli/tui/session-grouping` | a pending tool does not hide a later tool reusing its call ID in another message | after a fix | oc_a_pending_tool_does_not_hide_a_later_tool_reusing_its_call_id_in_another_message (adapted: rows are entries, not ids) |
| `tui/test/cli/tui/session-rows` | groups exploration parts across assistant messages until a delimiter | after a fix | oc_groups_exploration_parts_across_assistant_messages_until_a_delimiter (adapted: at medium) |
| `tui/test/cli/tui/session-rows` | keeps non-exploration tools as individual part rows | after a fix | oc_keeps_non_exploration_tools_as_individual_part_rows (adapted: at medium) |
| `tui/test/cli/tui/session-rows` | groups adjacent reasoning parts until a visible boundary | after a fix | oc_groups_adjacent_reasoning_parts_until_a_visible_boundary (adapted: at medium) |
| `tui/test/cli/tui/session-rows` | groups across empty assistant reasoning parts | after a fix | oc_groups_across_empty_assistant_reasoning_parts (adapted: at medium) |
| `tui/test/cli/tui/session-rows` | completes exploration groups when another row follows | after a fix | oc_completes_exploration_groups_when_another_row_follows (adapted: the turn's end completes the last one) |
| `tui/test/cli/tui/group-view` | retains nested expansion state and registers exact headers and parts | after a fix | oc_retains_nested_expansion_state_and_registers_exact_headers_and_parts (adapted: ctrl+o opens every group; no anchors) |
| `tui/test/cli/tui/group-view` | a low activity group with nothing finished stays collapsed behind a status | after a fix | oc_a_low_activity_group_with_nothing_finished_stays_collapsed_behind_a_status |
| `tui/test/cli/tui/group-view` | failed low activity uses a disclosure icon and keeps details expandable | after a fix | oc_failed_low_activity_uses_a_disclosure_icon_and_keeps_details_expandable (adapted: ctrl+o rather than a click) |
| `tui/test/session-activity-anchor` | a low activity summary that ends the transcript opens like v2 at the bottom | after a fix | oc_a_low_activity_summary_that_ends_the_transcript_opens_like_v2_at_the_bottom (adapted: ctrl+o) |
| `tui/test/session-backfill-disclosure` | clicking a backfilled thought group toggles it | after a fix | oc_clicking_a_backfilled_thought_group_toggles_it (adapted: a replayed session, ctrl+o) |
| `tui/test/session-mount-budget` | an expanded group spends the mounting budget that older rows used | after a fix | oc_an_expanded_group_spends_the_mounting_budget_that_older_rows_used (adapted: only the rows in view are laid out) |
| `tui/test/config-v2` | shows transcript verbosity in session settings | after a fix | oc_shows_transcript_verbosity_in_session_settings (adapted: no settings dialog; the `tui` setting) |
| `tui/test/config-v2` | names tool grouping explicitly in settings | after a fix | oc_names_tool_grouping_explicitly_in_settings (adapted: the `tui` setting) |
| `tui/test/cli/tui/diff-viewer` | closing the diff viewer returns to the route it opened from | after a fix | oc_closing_the_diff_viewer_returns_to_the_route_it_opened_from (adapted: back to the session) |
| `tui/test/cli/tui/diff-viewer` | standalone full-screen diff does not expose panel-only toggling at %i columns | after a fix | oc_standalone_full_screen_diff_does_not_expose_panel_only_toggling_at_80_columns (adapted: the 80-column case; the 160-column standalone route is by design) |
| `tui/test/cli/tui/diff-viewer` | ctrl+c closes the diff viewer without exiting the application | after a fix | oc_ctrl_c_closes_the_diff_viewer_without_exiting_the_application |
| `tui/test/cli/tui/diff-viewer` | shows an error instead of an empty diff when loading fails | after a fix | oc_shows_an_error_instead_of_an_empty_diff_when_loading_fails |
| `tui/test/cli/tui/diff-viewer` | base lookup is lazy until the viewer opens | after a fix | oc_base_lookup_is_lazy_until_the_viewer_opens |
| `tui/test/cli/tui/diff-viewer` | the source chooser selects all three scopes directly at %i columns (2 cases) | after a fix | oc_the_source_chooser_selects_all_three_scopes_directly_at_50_columns, …_at_160_columns |
| `tui/test/cli/tui/diff-viewer` | reopening remembers the last source choice instead of the initial %s setting (3 cases) | after a fix | oc_reopening_remembers_the_last_source_choice_instead_of_the_initial_branch_setting, …_committed_setting, …_working_setting |
| `tui/test/cli/tui/diff-viewer` | the turn source diffs the session's last turn without resolving a base | after a fix | oc_the_turn_source_diffs_the_sessions_last_turn_without_resolving_a_base |
| `tui/test/cli/tui/diff-viewer` | the turn source refreshes only when the session finishes a turn | after a fix | oc_the_turn_source_refreshes_only_when_the_session_finishes_a_turn |
| `tui/test/cli/tui/diff-viewer` | an empty or failing turn diff uses the viewer's empty and error states | after a fix | oc_an_empty_or_failing_turn_diff_uses_the_viewers_empty_and_error_states |
| `tui/test/cli/tui/diff-viewer` | keeps scope, base, and review count on one row with a selectable base at %i columns (3 cases) | after a fix | oc_keeps_scope_base_and_review_count_on_one_row_with_a_selectable_base_at_50_columns, …_80_columns, …_160_columns (adapted: the chooser opens by its key) |
| `tui/test/cli/tui/diff-viewer` | long comparison bases cannot hide the source or review count at %i columns (4 cases) | after a fix | oc_long_comparison_bases_cannot_hide_the_source_or_review_count_at_50_columns, …_80_columns, …_100_columns, …_160_columns |
| `tui/test/cli/tui/diff-viewer` | opening the source chooser from initial Uncommitted does not resolve a branch base | after a fix | oc_opening_the_source_chooser_from_initial_uncommitted_does_not_resolve_a_branch_base |
| `tui/test/cli/tui/diff-viewer` | choosing a base remembers it and refreshes only the affected %s comparison (3 cases) | after a fix | oc_choosing_a_base_remembers_it_and_refreshes_only_the_affected_branch_comparison, …_committed_comparison, …_working_comparison (adapted: the picker filters the listed branches) |
| `tui/test/cli/tui/diff-viewer` | an ambiguous base never requests a guessed %s diff (2 cases) | after a fix | oc_an_ambiguous_base_never_requests_a_guessed_branch_diff, …_committed_diff |
| `tui/test/cli/tui/diff-viewer` | base and scope choices survive reopening but not a new TUI instance | after a fix | oc_base_and_scope_choices_survive_reopening_but_not_a_new_tui_instance |
| `tui/test/cli/tui/diff-viewer` | base choices are isolated by branch within the same location | after a fix | oc_base_choices_are_isolated_by_branch_within_the_same_location (adapted: the branch is read as the viewer opens) |
| `tui/test/cli/tui/diff-viewer` | an invalid comparison reports an error and allows another base choice | after a fix | oc_an_invalid_comparison_reports_an_error_and_allows_another_base_choice |
| `tui/test/cli/tui/diff-viewer` | base search failures are visible without changing the diff | after a fix | oc_base_search_failures_are_visible_without_changing_the_diff |
| `tui/test/cli/tui/diff-viewer` | a late base lookup cannot overwrite an in-memory base choice | after a fix | oc_a_late_base_lookup_cannot_overwrite_an_in_memory_base_choice |
| `tui/test/cli/tui/diff-viewer` | dismissing the base picker leaves the comparison unchanged | after a fix | oc_dismissing_the_base_picker_leaves_the_comparison_unchanged |
| `tui/test/cli/tui/diff-viewer` | tree, single-file, and layout preferences do not refetch base or diff | after a fix | oc_tree_single_file_and_layout_preferences_do_not_refetch_base_or_diff (adapted: remembered beside the config, not written to it) |
| `tui/test/cli/tui/diff-viewer` | the remapped source chooser preserves its current selection marker | after a fix | oc_the_remapped_source_chooser_preserves_its_current_selection_marker |
| `tui/test/cli/tui/diff-viewer` | pending and failed scopes never render the previous scope's files | after a fix | oc_pending_and_failed_scopes_never_render_the_previous_scopes_files |
| `tui/test/cli/tui/diff-viewer` | late diff responses cannot replace the selected scope | after a fix | oc_late_diff_responses_cannot_replace_the_selected_scope |
| `tui/test/cli/tui/diff-viewer` | switching to uncommitted does not wait for or display a late base result | after a fix | oc_switching_to_uncommitted_does_not_wait_for_or_display_a_late_base_result |
| `tui/test/cli/tui/diff-viewer` | missing base metadata preserves provider branch review (has branch changes: %s) (2 cases) | after a fix | oc_missing_base_metadata_preserves_provider_branch_review_has_branch_changes_false, …_true |
| `tui/test/cli/tui/diff-viewer` | unresolved branch comparisons report errors without switching to working | after a fix | oc_unresolved_branch_comparisons_report_errors_without_switching_to_working |
| `tui/test/cli/tui/diff-viewer` | base lookup failures are errors rather than missing-base fallbacks | after a fix | oc_base_lookup_failures_are_errors_rather_than_missing_base_fallbacks |
| `tui/test/cli/tui/diff-viewer` | committed images never read the working-tree image (patch: %s) (2 cases) | after a fix | oc_committed_images_never_read_the_working_tree_image_patch_undefined, …_patch_binary (adapted: no image is read at all) |
| `tui/test/cli/tui/diff-viewer` | brackets navigate diff hunks | after a fix | oc_brackets_navigate_diff_hunks |
| `tui/test/cli/tui/diff-viewer` | disabled diff keybinds have no component fallbacks | after a fix | oc_disabled_diff_keybinds_have_no_component_fallbacks |
| `tui/test/cli/tui/diff-viewer` | %s files use full width even in split view (2 cases) | after a fix | oc_added_files_use_full_width_even_in_split_view, oc_deleted_files_use_full_width_even_in_split_view |
| `tui/test/cli/tui/diff-viewer` | adapts navigation and modified patches at %i columns (2 cases) | after a fix | oc_adapts_navigation_and_modified_patches_at_80_columns, …_160_columns |
| `tui/test/cli/tui/diff-viewer` | the first file title sits below full-height top padding with a blank row below | after a fix | oc_the_first_file_title_sits_below_full_height_top_padding_with_a_blank_row_below |
| `tui/test/cli/tui/diff-viewer` | shared pane edges align headings and stay fixed at %i columns (2 cases) | after a fix | oc_shared_pane_edges_align_headings_and_stay_fixed_at_100_columns, …_160_columns |
| `tui/test/cli/tui/diff-viewer` | the pane edge matches the visible reviewed card in %s mode (2 cases) | after a fix | oc_the_pane_edge_matches_the_visible_reviewed_card_in_dark_mode, …_light_mode (adapted: the default theme's colors) |
| `tui/test/cli/tui/diff-viewer` | the sidebar holds a compact help hint and the patch pane reaches the screen bottom | after a fix | oc_the_sidebar_holds_a_compact_help_hint_and_the_patch_pane_reaches_the_screen_bottom (adapted: no right-click row) |
| `tui/test/cli/tui/diff-viewer` | compact gutter help leaves the full patch viewport available when the sidebar is hidden | after a fix | oc_compact_gutter_help_leaves_the_full_patch_viewport_available_when_the_sidebar_is_hidden (adapted: `?` rather than a click; `esc close`) |
| `tui/test/cli/tui/diff-viewer` | full screen keeps its tree down to 88 columns and uses the compact layout below | after a fix | oc_full_screen_keeps_its_tree_down_to_88_columns_and_uses_the_compact_layout_below |
| `tui/test/cli/tui/diff-viewer` | compact help scrolls at a narrow size and reflects customized bindings | after a fix | oc_compact_help_scrolls_at_a_narrow_size_and_reflects_customized_bindings |
| `tui/test/cli/tui/diff-viewer` | the viewer opens help from $key with shift=$shift (3 cases) | after a fix | oc_the_viewer_opens_help_from_question_mark_with_shift_false, …_shift_true, oc_the_viewer_opens_help_from_slash_with_shift_true |
| `tui/test/cli/tui/diff-viewer` | file titles stick to the viewport and hand off while scrolling at %i columns (2 cases) | after a fix | oc_file_titles_stick_to_the_viewport_and_hand_off_while_scrolling_at_80_columns, …_160_columns |
| `tui/test/cli/tui/diff-viewer` | split diff filler keeps the full card background | after a fix | oc_split_diff_filler_keeps_the_full_card_background |
| `tui/test/cli/tui/diff-viewer` | reviewing collapses a %s body and unmarking reopens it (3 cases) | after a fix | oc_reviewing_collapses_a_patch_body_and_unmarking_reopens_it, …_image_body_…, …_fallback_body_… (adapted: the image card has a note, not a preview) |
| `tui/test/cli/tui/diff-viewer` | reviewing after scrolling targets the visible file rather than the initial file | after a fix | oc_reviewing_after_scrolling_targets_the_visible_file_rather_than_the_initial_file |
| `tui/test/cli/tui/diff-viewer` | single-file review advances, but reopening and the last file stay put at %i columns (2 cases) | after a fix | oc_single_file_review_advances_but_reopening_and_the_last_file_stay_put_at_80_columns, …_160_columns |
| `tui/test/cli/tui/diff-viewer` | Alt+arrows navigate files rather than session tabs (after Tab: %s) (2 cases) | after a fix | oc_alt_arrows_navigate_files_rather_than_session_tabs_after_tab_false, …_true (adapted: with tabs on, the focused viewer keeps its keys) |
| `tui/test/cli/tui/diff-viewer` | pads $file fallback messages inside the card without reading images (2 cases) | after a fix | oc_pads_bun_lock_fallback_messages_inside_the_card_without_reading_images, oc_pads_assets_old_png_fallback_messages_inside_the_card_without_reading_images |
| `tui/test/cli/tui/diff-viewer` | Ctrl+D and Ctrl+U scroll the patch viewport by half a page, without closing | after a fix | oc_ctrl_d_and_ctrl_u_scroll_the_patch_viewport_by_half_a_page_without_closing |
| `tui/test/cli/tui/diff-viewer` | Tab and file clicks never redirect scrolling keys into the tree at %i rows (2 cases) | after a fix | oc_tab_and_file_clicks_never_redirect_scrolling_keys_into_the_tree_at_20_rows, …_32_rows (adapted: no clicks) |
| `tui/test/cli/tui/diff-viewer` | folders are mouse controlled and n/p reveal files inside collapsed folders | after a fix | oc_folders_are_mouse_controlled_and_n_p_reveal_files_inside_collapsed_folders (adapted: the folder is closed directly) |
| `tui/test/cli/tui/diff-viewer` | n/p jump between files and gg/G always navigate the diff | after a fix | oc_n_p_jump_between_files_and_gg_g_always_navigate_the_diff |
| `tui/test/cli/tui/diff-viewer` | virtualizes a large added file at %i columns without losing its end (2 cases) | after a fix | oc_virtualizes_a_large_added_file_at_80_columns_without_losing_its_end, …_160_columns_… (adapted: only the rows in view are drawn; nothing is split) |
| `tui/test/cli/tui/diff-viewer` | keeps the line-number gutter the same width across virtual chunks | after a fix | oc_keeps_the_line_number_gutter_the_same_width_across_virtual_chunks |
| `tui/test/cli/tui/diff-viewer` | highlights virtual chunks with whole-file syntax context | after a fix | oc_highlights_virtual_chunks_with_whole_file_syntax_context (adapted: TypeScript is highlighted as JavaScript) |
| `tui/test/cli/tui/diff-viewer` | file navigation and review still work after a virtualized patch | after a fix | oc_file_navigation_and_review_still_work_after_a_virtualized_patch |
| `tui/test/cli/tui/diff-viewer` | the sidebar source picker switches VCS sources at %i columns (2 cases) | after a fix | oc_the_sidebar_source_picker_switches_vcs_sources_at_100_columns, …_160_columns (adapted: the chooser opens by its key) |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | builds a nested tree with deduplicated directories and file indexes | after a fix | oc_builds_a_nested_tree_with_deduplicated_directories_and_file_indexes |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | sorts directories before files and alphabetically within each group | after a fix | oc_sorts_directories_before_files_and_alphabetically_within_each_group |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | sorts root-level files without creating directories | after a fix | oc_sorts_root_level_files_without_creating_directories |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | collapses unary directory chains while flattening | after a fix | oc_collapses_unary_directory_chains_while_flattening |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | does not collapse a directory into a file row | after a fix | oc_does_not_collapse_a_directory_into_a_file_row |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | stops collapsing at branches | after a fix | oc_stops_collapsing_at_branches |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | keeps same directory names under different parents separate | after a fix | oc_keeps_same_directory_names_under_different_parents_separate |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | flattens all-expanded rows depth-first with depths and file references | after a fix | oc_flattens_all_expanded_rows_depth_first_with_depths_and_file_references |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | collapses expanded unary children under the first visible directory id | after a fix | oc_collapses_expanded_unary_children_under_the_first_visible_directory_id |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | flattens only expanded directory descendants when expansion is provided | after a fix | oc_flattens_only_expanded_directory_descendants_when_expansion_is_provided |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | finds the parent directories to expand for a patch file | after a fix | oc_finds_the_parent_directories_to_expand_for_a_patch_file |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | prefers the selected file when choosing the single patch file | after a fix | oc_prefers_the_selected_file_when_choosing_the_single_patch_file |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | orders patches by the flattened file tree order | after a fix | oc_orders_patches_by_the_flattened_file_tree_order |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | shows the diff viewer file tree only when enabled and files exist | after a fix | oc_shows_the_diff_viewer_file_tree_only_when_enabled_and_files_exist |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | moves patch selection through the ordered patch file indexes | after a fix | oc_moves_patch_selection_through_the_ordered_patch_file_indexes |
| `tui/test/feature-plugins/diff-viewer-file-tree-utils` | toggles only selected directory expansion | after a fix | oc_toggles_only_selected_directory_expansion |
| `tui/test/cli/tui/diff-viewer-file-tree` | defaults to text-line file icons and triangle folders with straight rails | after a fix | oc_defaults_to_text_line_file_icons_and_triangle_folders_with_straight_rails (adapted: the viewer's tree, titled by its source) |
| `tui/test/cli/tui/diff-viewer-file-tree` | %s layout uses two-cell horizontal sidebar padding | after a fix | oc_tree_layout_uses_two_cell_horizontal_sidebar_padding (adapted: the tree layout; the list layout is by design) |
| `tui/test/cli/tui/diff-viewer-file-tree` | full top padding keeps the heading one row down in %s mode (2 cases) | after a fix | oc_full_top_padding_keeps_the_heading_one_row_down_in_dark_mode, …_light_mode |
| `tui/test/cli/tui/diff-viewer-file-tree` | does not render text markers for selected files | after a fix | oc_does_not_render_text_markers_for_selected_files |
| `tui/test/cli/tui/diff-viewer-file-tree` | renders collapsed and expanded directory rows | after a fix | oc_renders_collapsed_and_expanded_directory_rows |
| `tui/test/cli/tui/diff-viewer-file-tree` | keeps rows quiet: straight rails, single status letters, no hooks or dots | after a fix | oc_keeps_rows_quiet_straight_rails_single_status_letters_no_hooks_or_dots |
| `tui/test/cli/tui/diff-viewer-file-tree` | narrow collapsed chains drop whole leading segments | after a fix | oc_narrow_collapsed_chains_drop_whole_leading_segments (adapted: the chain's truncation at that width) |
| `tui/test/cli/tui/diff-viewer-image` | recognizes only supported image file extensions | after a fix | oc_recognizes_only_supported_image_file_extensions |
| `tui/test/session-panel` | /diff focuses the production session panel without sending typing to the prompt | after a fix | oc_diff_focuses_the_production_session_panel_without_sending_typing_to_the_prompt |
| `tui/test/session-panel` | repeated fullscreen toggles retain the viewer, transcript, scroll, and reviewed files | after a fix | oc_repeated_fullscreen_toggles_retain_the_viewer_transcript_scroll_and_reviewed_files |
| `tui/test/session-panel` | the diff panel reopens in the presentation last chosen with the fullscreen toggle | after a fix | oc_the_diff_panel_reopens_in_the_presentation_last_chosen_with_the_fullscreen_toggle |
| `tui/test/session-panel` | docked tree commands leave the full-screen tree preference unchanged | after a fix | oc_docked_tree_commands_leave_the_full_screen_tree_preference_unchanged |
| `tui/test/session-panel` | a panel opened at %i columns can split after growing without remounting (2 cases) | after a fix | oc_a_panel_opened_at_60_columns_can_split_after_growing_without_remounting, …_80_columns_… |
| `tui/test/session-panel` | narrow terminals show the panel full screen, then compact, and dock again without remounting | after a fix | oc_narrow_terminals_show_the_panel_full_screen_then_compact_and_dock_again_without_remounting |
| `tui/test/session-panel` | panel focus, scrolling, and fullscreen use customized keys | after a fix | oc_panel_focus_scrolling_and_fullscreen_use_customized_keys |
| `tui/test/session-panel` | disabled panel keys (%s) have no behavioral or hint fallbacks (2 cases) | after a fix | oc_disabled_panel_keys_none_have_no_behavioral_or_hint_fallbacks, …_false_… (adapted: the prompt click is dropped) |
| `tui/test/session-panel` | Enter in the diff panel cannot approve a pending permission in either presentation | after a fix | oc_enter_in_the_diff_panel_cannot_approve_a_pending_permission_in_either_presentation (adapted: `y` approves in CodeSmith) |
| `tui/test/session-panel` | a form cannot capture diff keys (form already pending: %s) (2 cases) | after a fix | oc_a_form_cannot_capture_diff_keys_form_already_pending_true, …_false (adapted: a question) |
| `tui/test/panel` | presentation changes preserve the selected panel identity | after a fix | oc_presentation_changes_preserve_the_selected_panel_identity (adapted: `/diff` closes an open viewer) |
| `tui/test/panel` | narrow geometry overrides presentation without discarding the user's choice | after a fix | oc_narrow_geometry_overrides_presentation_without_discarding_the_users_choice |
| `tui/test/config-v2` | validates the explicit diff source defaults | after a fix | oc_validates_the_explicit_diff_source_defaults (adapted: the `tui.diffs` setting) |
| `tui/test/cli/tui/thinking` | extracts a leading summary title and leaves markdown body | after a fix | oc_extracts_a_leading_summary_title_and_leaves_the_markdown_body |
| `tui/test/cli/tui/thinking` | extracts a completed title before its streamed body arrives | after a fix | oc_extracts_a_completed_title_before_its_streamed_body_arrives |
| `tui/test/cli/tui/thinking` | preserves markdown-significant indentation in the extracted body | after a fix | oc_preserves_markdown_significant_indentation_in_the_extracted_body |
| `tui/test/cli/tui/thinking` | does not consume ordinary leading bold content | after a fix | oc_does_not_consume_ordinary_leading_bold_content |
| `tui/test/cli/tui/thinking` | leaves content without a leading title in its body | after a fix | oc_leaves_content_without_a_leading_title_in_its_body |
| `util/src/session-mention` | session tokens round-trip independently of the title | after a fix | oc_session_tokens_round_trip_independently_of_the_title (`tui/mention.rs`; adapted: CodeSmith's 17-character IDs are mentions too) |
| `util/src/session-mention` | does not parse malformed or embedded IDs: %s (7 cases) | after a fix | oc_does_not_parse_malformed_or_embedded_ids |
| `tui/test/app-lifecycle` | boots on the simulation renderer with the terminal title enabled | after a fix | oc_the_terminal_title_follows_the_session_and_the_exit_prints_the_summary (e2e; adapted: in a pty) |
| `tui/test/app-lifecycle` | SIGHUP clears title and disposes scoped resources once | after a fix | oc_sighup_clears_the_title_and_closes_the_ui (e2e; adapted: the UI closes as on exit) |
| `tui/test/app-lifecycle` | session lifecycle updates the terminal title and prints the epilogue after cleanup | after a fix | oc_the_terminal_title_follows_the_session_and_the_exit_prints_the_summary (e2e) |
| `tui/test/app-lifecycle` | session title generated while an untitled session is loading remains visible | after a fix | oc_the_terminal_title_follows_the_session_and_the_exit_prints_the_summary (e2e; adapted: the title comes with the turn that generated it and the window title never goes back) |
| `tui/test/app-lifecycle` | automatic rename refreshes the displayed title before settling, even without a renamed event | after a fix | oc_the_terminal_title_follows_the_session_and_the_exit_prints_the_summary (e2e; adapted: the header and window title take the title from the turn's end, no rename event) |
| `tui/test/util/presentation` | prints nothing without a session or update | after a fix | oc_prints_nothing_without_a_session_or_update |
| `tui/test/util/presentation` | formats session continuation summary | after a fix | oc_formats_the_session_continuation_summary |
| `tui/test/util/presentation` | tells the user to start OpenCode again after an update | after a fix | oc_tells_the_user_to_start_codesmith_again_after_an_update (adapted: no update check yet, so nothing passes a version) |
| `tui/test/util/presentation` | keeps the session continuation after an update | after a fix | oc_keeps_the_session_continuation_after_an_update |
| `tui/test/context/program-status` | reports a blocked session on the root and its own record | after a fix | oc_reports_a_blocked_session_on_the_root_and_its_own_record (adapted: the root goes out as `OSC 9;4`, see below) |
| `tui/test/context/program-status` | summarizes several pending sessions and clears resolved ones | after a fix | oc_summarizes_several_pending_sessions_and_clears_resolved_ones |
| `tui/test/context/program-status` | keeps record IDs within the protocol's name rules | after a fix | oc_keeps_record_ids_within_the_protocols_name_rules |
| `tui/test/context/program-status` | clips long titles to the protocol byte limit | after a fix | oc_clips_long_titles_to_the_protocol_byte_limit |
| `tui/test/cli/cmd/tui/notifications` | shows execution failures as session-scoped toasts without needing an assistant message | after a fix | oc_shows_execution_failures_as_session_scoped_toasts (`tui/attention.rs`; adapted: a usage limit stands for OpenCode's free and Go limits, and the UI shows the failure as its error row) |
| `tui/test/cli/cmd/tui/notifications` | notifies for form and permission requests with blurred notifications and always-on sounds | after a fix | oc_notifies_for_forms_and_permissions (adapted: a question is OpenCode's form) |
| `tui/test/cli/cmd/tui/notifications` | notifies for global forms once the TUI can render them | after a fix | oc_notifies_for_global_forms |
| `tui/test/cli/cmd/tui/notifications` | dedupes pending forms and permissions until they are resolved | after a fix | oc_dedupes_pending_forms_and_permissions_until_resolved |
| `tui/test/cli/cmd/tui/notifications` | notifies for terminal lifecycle events even when attached after execution started | after a fix | oc_notifies_for_lifecycle_events_attached_after_start |
| `tui/test/cli/cmd/tui/notifications` | uses sound-only notifications and subagent_done sound for subagent sessions | after a fix | oc_subagents_only_sound |
| `tui/test/cli/cmd/tui/notifications` | plays done only once none of the session's direct children is running | after a fix | oc_plays_done_once_no_direct_child_runs |
| `tui/test/cli/cmd/tui/notifications` | notifies session errors once and suppresses the following idle done notification | after a fix | oc_notifies_errors_once_and_suppresses_the_done |
| `tui/test/config` | defines attention sound names | after a fix | oc_defines_attention_sound_names |
| `tui/test/editor` | rejects when the external editor cannot start | after a fix | oc_rejects_when_the_external_editor_cannot_start (`tui/external.rs`) |
| `tui/test/editor` | returns the edited prompt and resumes the renderer | after a fix | oc_returns_the_edited_prompt_and_resumes_the_renderer; also the_external_editor_edits_the_prompt (e2e) |
| `tui/test/editor` | says which variable to set when no external editor is configured | after a fix | oc_says_which_variable_to_set_when_no_editor_is_configured |
| `tui/test/editor` | normalizes a single trailing editor newline for one-line prompts | after a fix | oc_normalizes_a_single_trailing_editor_newline_for_one_line_prompts |
| `tui/test/editor` | preserves multiline prompts that end with a newline | after a fix | oc_preserves_multiline_prompts_that_end_with_a_newline |
| `tui/test/cli/tui/message-navigation` | adds only enough slack to align the selected message | after a fix | oc_adds_only_enough_slack_to_align_the_selected_message (`tui/navigation.rs`) |
| `tui/test/cli/tui/message-navigation` | finds the next user message without stopping at an assistant message | after a fix | oc_finds_the_next_user_message_without_stopping_at_an_answer |
| `tui/test/cli/tui/message-navigation` | finds the previous user message without stopping at an assistant message | after a fix | oc_finds_the_previous_user_message_without_stopping_at_an_answer |
| `tui/test/cli/tui/message-navigation` | preserves navigation across both user and assistant messages | after a fix | oc_preserves_navigation_across_prompts_and_answers |
| `tui/test/cli/tui/message-navigation` | uses the selected message when the viewport is too tall to scroll | after a fix | oc_uses_the_selected_message_when_the_viewport_cannot_scroll |
| `tui/test/cli/tui/message-navigation` | stops at the first and last selected user message | after a fix | oc_stops_at_the_first_and_last_selected_user_message |
| `tui/test/cli/tui/message-navigation` | keeps the logical boundary when layout temporarily moves it outside the viewport | after a fix | oc_keeps_the_logical_boundary_when_layout_moves_it |
| `tui/test/cli/tui/message-navigation` | stops at the first and last message | after a fix | oc_stops_at_the_first_and_last_message |
| `util/src/session-turn` | measures turn duration from the user prompt across assistant steps | after a fix | oc_measures_turn_duration_from_the_prompt_across_steps (`tui/turn.rs`) |
| `util/src/session-turn` | measures decode throughput from the first streamed token | after a fix | oc_measures_decode_throughput_from_the_first_streamed_token |
| `util/src/session-turn` | falls back to the request window for steps recorded without a first token | after a fix | oc_falls_back_to_the_request_window_without_a_first_token |
| `util/src/session-turn` | measures request throughput including reasoning without tool time | after a fix | oc_measures_request_throughput_including_reasoning_without_tool_time |
| `util/src/session-turn` | omits turn throughput when a stream boundary is unavailable | after a fix | oc_omits_turn_throughput_without_a_stream_boundary |
| `util/src/session-turn` | measures historical footers without later inputs or inc (2 cases) | after a fix | oc_measures_historical_footers_without_later_inputs_or_incomplete_steps |
| `util/src/session-turn` | preserves missing-anchor fallbacks without including the absent assistant's tokens | after a fix | oc_preserves_missing_anchor_fallbacks |
| `util/src/session-turn` | measures a marker-era turn from its first prompt across steers | after a fix | oc_measures_a_marker_era_turn_from_its_first_prompt_across_steers |
| `tui/test/config-v2` | shows the TPS default in session settings | after a fix | oc_shows_the_tps_default_in_session_settings (adapted: `tui.session.tps`) |
| `tui/test/cli/tui/session-rows` | filters OpenAI cache quantization from cache reuse drops | after a fix | oc_filters_openai_cache_quantization_from_cache_reuse_drops (`tui/usage.rs`) |
| `tui/test/cli/tui/session-rows` | compares cache reuse only for the same model | after a fix | oc_compares_cache_reuse_only_for_the_same_model |
| `tui/test/cli/tui/session-rows` | carries model identity with the cross-turn cache baseline | after a fix | oc_carries_model_identity_with_the_cross_turn_cache_baseline |
| `tui/test/cli/tui/session-rows` | resets the cross-turn cache baseline after compaction | after a fix | oc_resets_the_cross_turn_cache_baseline_after_compaction |
| `tui/test/cli/tui/session-rows` | closes turn usage on the idle marker so steered steps share one footer | after a fix | oc_closes_turn_usage_on_the_idle_marker (adapted: the usage rows; the transcript rows are CodeSmith's own) |
| `tui/test/prompt/draft-stash` | tab-keyed drafts stay on the tab they were written in | after a fix | oc_tab_keyed_drafts_stay_on_their_tab (adapted: each tab's app keeps its prompt) |
| `tui/test/prompt/draft-stash` | each tab keeps its own draft, including home | after a fix | oc_each_tab_keeps_its_own_draft (adapted) |
| `tui/test/prompt/draft-stash` | a newer draft for the same slot replaces the older one | after a fix | oc_a_newer_draft_replaces_the_older_one (adapted) |
| `tui/test/prompt/history-provider` | shares prompt history across sessions and the home composer | after a fix | oc_shares_prompt_history_across_sessions (adapted: tabs hand the history over) |
| `tui/test/prompt/history-provider` | keeps legacy unscoped history on the home composer | after a fix | oc_keeps_legacy_unscoped_history (adapted: the file as it loads) |
| `tui/test/prompt/history-provider` | reset clears browsing position and live draft, and move returns a deep copy | after a fix | oc_reset_clears_the_browsing_position_and_draft (adapted: no pasted-text parts) |
| `tui/test/prompt/history` | recovers valid JSONL entries around corruption | after a fix | oc_recovers_valid_jsonl_entries_around_corruption (`tui/history.rs`) |
| `tui/test/prompt/history` | ignores the legacy parts shape | after a fix | oc_ignores_the_legacy_parts_shape |
| `tui/test/prompt/history` | retains only the newest entries | after a fix | oc_retains_only_the_newest_entries |
| `tui/test/prompt/history` | does not dedupe entries with different attachments | after a fix | oc_does_not_dedupe_entries_with_different_attachments |
| `tui/test/prompt/history` | preserves duplicate attachment mentions for prompt restoration | after a fix | oc_preserves_duplicate_attachment_mentions |
| `tui/test/prompt/persistence` | persistence creates parent directories and supports text, append, and JSON | after a fix | oc_persistence_creates_parent_directories_and_appends (adapted: the history's writes) |
| `tui/test/model-preference` | repairs known model preferences and preserves unrelated fields | after a fix | oc_repairs_known_model_preferences_and_preserves_unrelated_fields (`tui/models.rs`) |
| `tui/test/model-preference` | atomically serializes model preference updates | after a fix | oc_atomically_serializes_model_preference_updates (adapted: threads under the repository's lock) |
| `tui/test/context/local-selection` | cycles all recent models in a stable order in both directions | after a fix | oc_cycles_all_recent_models_in_a_stable_order_in_both_directions (adapted: the cycle over the recents) |
| `tui/test/context/local` | moves a model to the front, deduplicates, and limits recents | after a fix | oc_moves_a_model_to_the_front_deduplicates_and_limits_recents |
| `tui/test/copy-session-id` | Copy session ID from Ctrl+P (%s) (3 cases) | after a fix | oc_copy_session_id_from_ctrl_p (adapted: `OSC 52` reports no failure, and a UI with no session stands for home) |
| `tui/test/feature-plugins/prompt-footer` | prompt footer separates simultaneous subagent, shell, and usage status | after a fix | oc_prompt_footer_separates_subagent_shell_and_usage_status (adapted: no clicks; the status line leads with it) |
| `tui/test/feature-plugins/prompt-footer` | prompt footer can hide details | after a fix | oc_prompt_footer_can_hide_details |
| `tui/test/prompt/autocomplete` | lists parents and siblings without changing location | after a fix | oc_lists_parents_and_siblings_without_changing_location (`tui/dirs.rs`; adapted: the folder read directly) |
| `tui/test/prompt/autocomplete` | resolves home and nested sibling completions against the current location | after a fix | oc_resolves_home_and_nested_sibling_completions (adapted) |
| `tui/test/prompt/autocomplete` | starts %j after the command separator (4 cases) | after a fix | oc_starts_after_the_command_separator (adapted: the arguments of a `!command`, CodeSmith having no `/cd`) |
| `tui/test/prompt/autocomplete` | does not complete the command token | after a fix | oc_does_not_complete_the_command_token (adapted) |
| `tui/test/prompt/autocomplete` | searches from home after a home prefix | after a fix | oc_searches_from_home_after_a_home_prefix |
| `tui/test/prompt/autocomplete` | searches from parent prefixes | after a fix | oc_searches_from_parent_prefixes |
| `tui/test/prompt/autocomplete` | keeps ordinary searches rooted at the current directory | after a fix | oc_keeps_ordinary_searches_rooted_at_the_current_directory |
| `tui/test/prompt/autocomplete` | preserves explicit roots | after a fix | oc_preserves_explicit_roots |
| `tui/test/prompt/autocomplete` | includes complete explicit roots | after a fix | oc_includes_complete_explicit_roots |
| `tui/test/prompt/autocomplete` | omits incomplete and implicit roots | after a fix | oc_omits_incomplete_and_implicit_roots |
| `tui/test/prompt/autocomplete` | hides dot directories for an empty component | after a fix | oc_hides_dot_directories_for_an_empty_component |
| `tui/test/prompt/autocomplete` | shows dot directories when explicitly filtered | after a fix | oc_shows_dot_directories_when_explicitly_filtered |
| `tui/test/prompt/autocomplete` | abbreviates home paths | after a fix | oc_abbreviates_home_paths (the popup's detail) |
| `tui/test/prompt/autocomplete` | keeps paths outside home absolute | after a fix | oc_keeps_paths_outside_home_absolute |
| `cli/test/auth` | registers authentication commands | already passing | oc_auth_registers_its_commands (adapted) |
| `cli/test/auth` | lists stored and environment connections | already passing | oc_auth_list_shows_stored_and_environment_keys (adapted) |
| `cli/test/auth` | removes a selected stored credential | already passing | oc_auth_logout_removes_a_stored_key |
| `cli/test/auth` | requires a target outside an interactive terminal, including through acp --login | already passing | oc_auth_login_requires_a_target (the acp --login half is a gap) |
| `cli/test/debug-config` | is included in troubleshooting help | already passing | oc_debug_paths_and_config_are_in_the_debug_help |
| `cli/test/debug-paths` | is included in troubleshooting help | already passing | oc_debug_paths_and_config_are_in_the_debug_help |
| `cli/test/debug-paths` | prints resolved global paths without starting a server | already passing | oc_debug_paths_prints_the_resolved_folders |
| `cli/test/flag` | parses $args (9 cases) | already passing | oc_flag_parses_an_optional_worktree_value |
| `cli/test/flag` | leaves the next flag and its value alone | already passing | oc_flag_leaves_the_next_flag_..., oc_flag_rejects_repeated_flags |
| `cli/test/flag` | rejects repeated flags: $args (3 cases) | already passing | oc_flag_leaves_the_next_flag_..., oc_flag_rejects_repeated_flags |
| `cli/test/import-export` | export is raw by default and supports explicit sanitization | already passing | oc_export_* / oc_import_* (the sanitizing export half is a gap) |
| `cli/test/import-export` | export requires a session outside an interactive terminal | already passing | oc_export_* / oc_import_* (the sanitizing export half is a gap) |
| `cli/test/import-export` | export reports a missing session without a stack trace | already passing | oc_export_* / oc_import_* (the sanitizing export half is a gap) |
| `cli/test/import-export` | import sends the file unchanged to the resolved location | already passing | oc_export_* / oc_import_* (the sanitizing export half is a gap) |
| `cli/test/import-export` | import reports an existing session without a stack trace | already passing | oc_export_* / oc_import_* (the sanitizing export half is a gap) |
| `cli/test/mini` | uses piped stdin as the initial prompt | already passing | oc_piped_stdin_alone_is_the_prompt |
| `cli/test/mini` | merges non-interactive argument and stdin input | already passing | run_cli.rs piped_stdin_attached_files_and_titles |
| `cli/test/mini` | parses model variants from the model reference | already passing | oc_model_references_carry_nested_ids_and_variants |
| `cli/test/mini` | keeps option-like prompt text after the argument separator | already passing | oc_option_like_prompt_text_after_the_separator_is_the_message |
| `cli/test/run/archived` | run --archived archives the new session before subscribing to its prompt | after a fix | oc_run_archived_archives_the_new_session_before_its_prompt (e2e; adapted: the model call fails rather than the event stream) |
| `cli/test/mcp-auth-choices` | offers only OAuth-capable MCP servers by their server identity | after a fix | oc_mcp_auth_offers_only_oauth_capable_servers_by_their_server_identity (adapted: the hint comes from saved tokens, not a live status) |
| `cli/test/mcp-auth-choices` | mcp auth accepts an optional server name and rejects no-name noninteractive calls before connecting | after a fix | oc_mcp_auth_accepts_an_optional_server_name, oc_mcp_auth_without_a_name_needs_a_terminal (e2e; adapted: the error is on stderr) |
| `cli/test/session-target` | creates a worktree from the caller's checkout | already passing | oc_creates_a_worktree_from_the_callers_checkout (codesmith-core worktree.rs) |
| `cli/test/session-target` | keeps the caller's subfolder in the new worktree | already passing | oc_keeps_the_callers_subfolder_in_the_new_worktree (codesmith-core worktree.rs) |
| `cli/test/session-target` | uses the worktree root when the subfolder is not in the worktree | already passing | oc_uses_the_worktree_root_when_the_subfolder_is_not_in_the_worktree (codesmith-core worktree.rs) |
| `cli/test/stats` | renders shared intensity levels and partial month labels | after a fix | oc_stats_labels_only_the_months_a_partial_range_shows (codesmith-core/tests/oc_stats.rs) |
| `tui/test/util/form` | initializes configured and custom defaults | after a fix | oc_initializes_configured_and_custom_defaults |
| `tui/test/util/form` | validates every supported field constraint | after a fix | oc_validates_every_supported_field_constraint (adapted: a string for NaN, which JSON lacks) |
| `tui/test/util/form` | shares field classification, rows, selection, and display | after a fix | oc_shares_field_classification_rows_selection_and_display |
| `tui/test/util/form` | updates multiselects without mutating their source | after a fix | oc_updates_multiselects_without_mutating_their_source |
| `tui/test/cli/tui/form` | restores the composer when terminal-form revalidation fails | after a fix | oc_restores_the_composer_when_terminal_form_revalidation_fails (adapted: the asking turn went away) |
| `tui/test/cli/tui/form` | restores the composer after a terminal ${status} reply (2 cases) | after a fix | oc_restores_the_composer_after_a_terminal_reply (adapted) |
| `tui/test/cli/tui/form` | restores the composer after a terminal ${status} cancellation (2 cases) | after a fix | oc_restores_the_composer_after_a_terminal_cancellation (adapted) |
| `tui/test/cli/tui/form` | requires explicit acknowledgement before submitting an external field | after a fix | oc_requires_explicit_acknowledgement_before_submitting_an_external_field (the URL is copied with `OSC 52`) |
| `tui/test/cli/tui/form` | includes external acknowledgements in progress | after a fix | oc_includes_external_acknowledgements_in_progress |
| `tui/test/cli/tui/form` | shows a compact confirmation summary | after a fix | oc_shows_a_compact_confirmation_summary |
| `tui/test/cli/tui/form` | confirmation fits wrapped answers before offering scroll | after a fix | oc_confirmation_fits_wrapped_answers_before_offering_scroll (adapted: the dialog is centred) |
| `tui/test/cli/tui/form` | pasting on a custom choice opens its editor without submitting | after a fix | oc_pasting_on_a_custom_choice_opens_its_editor_without_submitting |
| `tui/test/cli/tui/form` | pasting in an active custom editor inserts at the cursor | after a fix | oc_pasting_in_an_active_custom_editor_inserts_at_the_cursor |
| `tui/test/cli/tui/form` | clipboard shortcut opens a custom choice editor without submitting | after a fix | oc_clipboard_shortcut_opens_a_custom_choice_editor_without_submitting (adapted: the terminal pastes on ctrl+v) |
| `tui/test/cli/tui/form` | typing a custom multiselect answer selects it before commit | after a fix | oc_typing_a_custom_multiselect_answer_selects_it_before_commit |
| `tui/test/cli/tui/form` | up leaves a custom editor only from its first visual line | after a fix | oc_up_leaves_a_custom_editor_only_from_its_first_visual_line (adapted: the first line) |
| `tui/test/cli/tui/form` | typing on the highlighted custom option opens it without losing burst input | after a fix | oc_typing_on_the_highlighted_custom_option_opens_it_without_losing_burst_input |
| `tui/test/cli/tui/form` | enter on an empty custom multiselect option clearly enters editing | after a fix | oc_enter_on_an_empty_custom_multiselect_option_clearly_enters_editing |
| `tui/test/cli/tui/form` | defers multiselect validation until submission | after a fix | oc_defers_multiselect_validation_until_submission |
| `tui/test/cli/tui/form` | submits an optional empty multiselect as an omitted answer | after a fix | oc_submits_an_optional_empty_multiselect_as_an_omitted_answer |
| `tui/test/cli/tui/form` | typing a custom single-select answer selects it without submitting | after a fix | oc_typing_a_custom_single_select_answer_selects_it_without_submitting |
| `tui/test/cli/tui/form` | committing a custom multiselect answer keeps one editable custom row | after a fix | oc_committing_a_custom_multiselect_answer_keeps_one_editable_custom_row |
| `tui/test/cli/tui/form` | text fields retain default paste behavior | after a fix | oc_text_fields_retain_default_paste_behavior |
| `tui/test/cli/tui/form` | ctrl+c clears a text field before cancelling its form | after a fix | oc_ctrl_c_clears_a_text_field_before_cancelling_its_form |
| `tui/test/cli/tui/form` | pasting on a choice without custom answers does not open an editor | after a fix | oc_pasting_on_a_choice_without_custom_answers_does_not_open_an_editor |
| `tui/test/cli/tui/form` | space toggles the selected multiselect option | after a fix | oc_space_toggles_the_selected_multiselect_option |
| `tui/test/cli/tui/form` | keeps a visible space between a multiselect marker and wrapped label | after a fix | oc_keeps_a_visible_space_between_a_multiselect_marker_and_wrapped_label |
| `tui/test/cli/tui/form` | space activates the custom multiselect option | after a fix | oc_space_activates_the_custom_multiselect_option |
| `tui/test/cli/tui/form` | space does not select a single-choice option | after a fix | oc_space_does_not_select_a_single_choice_option |
| `tui/test/cli/tui/form` | arriving choice question keeps focus with an open rename dialog until it closes | after a fix | oc_arriving_choice_question_keeps_focus_with_an_open_dialog_until_it_closes (adapted: a filtered picker for the rename dialog, which CodeSmith lacks) |
| `tui/test/cli/tui/form` | arriving text question does not steal editor focus from an open rename dialog | after a fix | oc_arriving_text_question_does_not_steal_editor_focus_from_an_open_dialog (adapted) |
| `tui/test/cli/tui/form` | escape closes an open command palette instead of cancelling an arriving question | after a fix | oc_escape_closes_an_open_command_palette_instead_of_cancelling_an_arriving_question |
| `tui/test/prompt-paste` | undo submits the restored %s paste chip (2 cases) | after a fix | oc_undo_submits_the_restored_paste_chip (adapted: the prompt is sent trimmed) |
| `tui/test/prompt-paste` | a pasted image that reuses a wide label gets a chip as wide as the label | after a fix | oc_a_pasted_image_that_reuses_a_label_keeps_it (adapted: no OpenTUI cell widths; the label is reused) |
| `tui/test/prompt-paste` | %s grows the prompt in the first frame that shows it (2 cases) | after a fix | oc_text_grows_the_prompt_in_the_first_frame_that_shows_it (adapted: no server route appends to the prompt; a restored prompt) |
| `tui/test/prompt-paste` | undo submits the restored %s chip with its attachment (files) | after a fix | oc_undo_submits_the_restored_file_chip_with_its_attachment |
| `tui/test/prompt/attachment` | deduplicates identical inline images while preserving other attachments | after a fix | oc_deduplicates_identical_inline_images_while_preserving_other_attachments (adapted: pasted paths) |
| `tui/test/prompt/attachment` | reuses labels for identical image data | after a fix | oc_reuses_labels_for_identical_image_data (adapted: the same path) |
| `tui/test/prompt/attachment` | does not reuse a label when attachment metadata differs | after a fix | oc_does_not_reuse_a_label_when_attachment_metadata_differs (adapted: another name) |
| `tui/test/component/dialog-shell-output` | shell output opens, follows, scrolls, and survives exit at %s columns (2 cases) | after a fix | oc_shell_output_opens_follows_scrolls_and_survives_exit (adapted: the file is read, not a server; no request counts) |
| `tui/test/component/dialog-shell-output` | empty output explains redirection, retries errors, and preserves output after removal | after a fix | oc_empty_output_explains_redirection_retries_errors_and_preserves_output_after_removal (adapted: a directory in the file's place fails the read) |
| `tui/test/component/dialog-shell-output` | mouse-wheel scrolling pauses and resumes output following (2 cases) | after a fix | oc_scrolling_pauses_and_resumes_output_following (adapted: ↑ and ↓ for the wheel) |
| `tui/test/component/dialog-shell-output` | large captures open at a bounded tail and clicking a shell opens the viewer | after a fix | oc_large_captures_open_at_a_bounded_tail (adapted: Enter for the click) |
| `tui/test/component/dialog-shell-output` | new designs show footer actions instead of the legacy hint line | after a fix | oc_the_viewer_shows_footer_actions |
| `tui/test/component/dialog-shell-output` | the retained 64 KiB tail starts on a whole character | after a fix | oc_the_retained_64_kib_tail_starts_on_a_whole_character |
| `tui/test/session-subagent-navigation` | Home and End navigate a subagent transcript | after a fix | oc_home_and_end_navigate_a_subagent_transcript (adapted: a subagent's session opens as any session) |
| `tui/test/util/session` | flattens nested subagents from any session in the family | after a fix | oc_flattens_nested_subagents_from_any_session_in_the_family |
| `tui/test/cli/tui/composer-keymap` | disabled subagent bindings have no component fallbacks | after a fix | oc_disabled_subagent_bindings_have_no_component_fallbacks (adapted: no dispatch by id; the default binding) |
| `tui/test/cli/tui/composer-keymap` | the inactive subagent filter only changes with its toggle | after a fix | oc_the_inactive_subagent_filter_only_changes_with_its_toggle (adapted: opening a subagent closes the panel; reopened, it keeps the filter) |
| `tui/test/cli/tui/composer-keymap` | disabled shell bindings have no component fallbacks | after a fix | oc_disabled_shell_bindings_have_no_component_fallbacks (adapted) |
| `tui/test/cli/tui/composer-keymap` | composer tabs switch by mouse and keyboard | after a fix | oc_composer_tabs_switch_by_keyboard (adapted: no mouse) |
| `tui/test/cli/tui/composer-keymap` | shell list shows one line per command | after a fix | oc_shell_list_shows_one_line_per_command |
| `tui/test/cli/tui/composer-keymap` | configured composer bindings work with a focused textarea | after a fix | oc_configured_composer_bindings_work_with_a_draft (adapted: a draft in the prompt) |
| `tui/test/cli/tui/composer-keymap` | ctrl+c closes the active composer | after a fix | oc_ctrl_c_closes_the_active_composer |
| `tui/test/cli/tui/composer-keymap` | shell output respects a configured binding with a focused textarea | after a fix | oc_shell_output_respects_a_configured_binding |
| `tui/test/cli/tui/toast` | toast hides only the terminal cursor it covers, until dismissed | after a fix | oc_toast_hides_only_the_terminal_cursor_it_covers_until_dismissed (adapted: the prompt's cursor) |
| `tui/test/plugin-toast` | toast for %s's family shows as-is while %s is open (3 cases) | after a fix | oc_toast_for_a_family_shows_as_is |
| `tui/test/context/local-selection` | switching agents restores their model and variant within the session | after a fix | oc_switching_agents_restores_their_model_and_variant_within_the_session (adapted: the runner's selection memory) |
| `tui/test/context/local-selection` | agent and model drafts are isolated across sessions and survive navigation | after a fix | oc_agent_and_model_drafts_are_isolated_across_sessions (adapted: the agent is saved with the session) |
| `tui/test/context/local-selection` | retains an unavailable saved session model instead of selecting a fallback | after a fix | oc_retains_an_unavailable_saved_session_model (adapted) |
| `tui/test/prompt/parse` | preserves file line-range parsing semantics | after a fix | oc_preserves_file_line_range_parsing_semantics |
| `tui/test/prompt/traits` | normal mode without autocomplete only captures tab | after a fix | oc_normal_mode_without_autocomplete_only_captures_tab |
| `tui/test/prompt/traits` | normal mode with autocomplete captures navigation keys | after a fix | oc_normal_mode_with_autocomplete_captures_navigation_keys |
| `tui/test/prompt/traits` | shell mode disables capture and labels the prompt without suspending | after a fix | oc_shell_mode_disables_capture_and_labels_the_prompt_without_suspending |
| `tui/test/prompt-shell-failure` | a failed shell command in an existing session toasts and restores the composer | after a fix | oc_a_failed_shell_command_toasts_and_restores_the_prompt (adapted: the runner's failure path) |
| `tui/test/feature-plugins/prompt-footer` | prompt footer shows the session's goal without its objective and opens it on click | after a fix | oc_prompt_footer_shows_the_sessions_goal_without_its_objective (adapted: no mouse, so the click half is dropped) |
| `tui/test/prompt/jsonl` | stash JSONL skips corruption and retains newest entries | after a fix | oc_stash_jsonl_skips_corruption_and_retains_newest_entries |
| `tui/test/prompt/jsonl` | frecency JSONL skips corruption, keeps latest path state, and limits entries | after a fix | oc_frecency_jsonl_skips_corruption_keeps_latest_path_state_and_limits_entries |
| `tui/test/prompt/history` | appends a prompt and shifts ranges by the %s offset (2 cases) | after a fix | oc_appends_a_prompt_after_the_typed_one (adapted: parts follow labels, not offsets) |
| `tui/test/cli/tui/dialog-mcp` | starts OAuth with %s for an MCP server requiring authentication (2 cases) | after a fix | oc_starts_oauth_for_an_mcp_server_requiring_authentication |
| `tui/test/cli/tui/dialog-mcp` | returns to the MCP server list after OAuth completes | after a fix | oc_returns_to_the_mcp_server_list_after_oauth_completes |
| `tui/test/cli/tui/dialog-mcp` | opens an investigation draft for a failed MCP server at its originating location | after a fix | oc_opens_an_investigation_draft_for_a_failed_mcp_server (adapted: one location) |
| `cli/test/stats` | keeps the default card shareable | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | renders only requested detail tables | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | shows when detail tables omit rows | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | uses the OpenCode palette in color mode | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | renders a concise empty state | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | labels activity when terminal width truncates the requested range | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | uses compact layouts in narrow terminals | after a fix | oc_stats_uses_compact_layouts_in_narrow_terminals (codesmith-core/tests/oc_stats.rs; adapted: output tokens include reasoning) |
| `cli/test/run/noninteractive` | waits for a background shell and streams the reply it wakes | already passing | run_cli.rs run_waits_for_background_commands_and_resumes_the_model |
| `cli/test/run/noninteractive` | cancelling while waiting on background work stops the Session and this prompt's jobs | already passing | run_cli.rs interrupting_a_run_stops_its_background_server (adapted) |
| `cli/test/acp/additional-directories.subprocess` | tools read files in an additional directory without an external directory ask | already passing | oc_acp_additional_directories |
| `cli/test/acp/additional-directories` | rejects %p before creating a session (4 cases) | already passing | oc_acp_additional_directories |
| `cli/test/acp/additional-directories` | load and resume replace ACP grants and keep other session rules and metadata | already passing | oc_acp_load_and_resume_replace_acp_grants_and_keep_other_session_rules_and_metadata (adapted: grants are kept beside the session's own rules) |
| `cli/test/acp/additional-directories` | forks replace inherited grants with the requested list | already passing | oc_acp_forks_replace_inherited_grants_with_the_requested_list |
| `cli/test/acp/background-turn.subprocess` | streams turns woken by background shells and keeps the session usable | already passing | oc_acp_background_shell_wakes_the_session |
| `cli/test/acp/catalog` | switches model, effort, and mode against the warm catalog | already passing | oc_acp_config_options_switch_model_and_mode |
| `cli/test/acp/compaction` | streams a /compact turn's summary and completes it with the full summary | already passing | oc_acp_compaction_updates (adapted: reported once done) |
| `cli/test/acp/config-option` | builds effort option from variants and falls back to default when current variant is invalid | already passing | oc_acp_effort_option_shape, oc_acp_config_options_switch_model_and_mode |
| `cli/test/acp/config-option` | parses the model selection %s, preferring exact slash-cont (5 cases) | already passing | oc_acp_effort_option_shape, oc_acp_config_options_switch_model_and_mode |
| `cli/test/acp/elicitation` | cancels forms from unsupported clients, unknown flows, and credential-looking fields | already passing | oc_acp_question_tool_through_elicitation, oc_acp_elicitation_mapping |
| `cli/test/acp/elicitation` | answers a question form through elicitation and continues the turn | already passing | oc_acp_question_tool_through_elicitation, oc_acp_elicitation_mapping |
| `cli/test/acp/elicitation` | cancels the form when the client declines, cancels, fails, or sends a wrong value type | already passing | oc_acp_question_tool_through_elicitation, oc_acp_elicitation_mapping |
| `cli/test/acp/permission` | forwards allow-once and allow-always selections to the server | already passing | oc_acp_tool_calls_and_permission_selections, oc_acp_edits_report_full_file_diffs |
| `cli/test/acp/permission` | previews added files as new and deleted files as empty | already passing | oc_acp_tool_calls_and_permission_selections, oc_acp_edits_report_full_file_diffs |
| `cli/test/acp/prompt-content.subprocess` | accepts embedded text resource, inline and remote image, and file resource link prompt content | already passing | oc_acp_* media prompt test (adapted: no audio) |
| `cli/test/acp/prompt` | streams an admitted turn and resolves with usage after its terminal event | already passing | oc_acp_prompt_streams_..., oc_acp_cancel_*, oc_acp_close_and_delete_settle_the_active_turn |
| `cli/test/acp/prompt` | reports full-file diffs for a completed edit and patch | already passing | oc_acp_prompt_streams_..., oc_acp_cancel_*, oc_acp_close_and_delete_settle_the_active_turn |
| `cli/test/acp/prompt` | session/cancel forwards the server's wind-down before resolving cancelled | already passing | oc_acp_prompt_streams_..., oc_acp_cancel_*, oc_acp_close_and_delete_settle_the_active_turn |
| `cli/test/acp/prompt` | stops waiting for a wind-down that never ends and fails the tools left running | already passing | oc_acp_prompt_streams_..., oc_acp_cancel_*, oc_acp_close_and_delete_settle_the_active_turn |
| `cli/test/acp/prompt` | $/cancel_request on the prompt request cancels the turn like session/cancel | already passing | oc_acp_prompt_streams_..., oc_acp_cancel_*, oc_acp_close_and_delete_settle_the_active_turn |
| `cli/test/acp/prompt` | %s settles the active turn before responding and deta (2 cases) | already passing | oc_acp_prompt_streams_..., oc_acp_cancel_*, oc_acp_close_and_delete_settle_the_active_turn |
| `cli/test/acp/replay` | replays user, text, reasoning, and tool messages in order on session/load | already passing | oc_acp_replay_shape |
| `cli/test/acp/session` | initialize advertises capabilities, negotiates the version, and offers terminal auth only when asked | already passing | oc_acp_initialize_..., oc_acp_load_replays_..., oc_acp_close_keeps_..., oc_acp_rejects_... |
| `cli/test/acp/session` | loads with paginated replay while resume and fork do not replay | already passing | oc_acp_initialize_..., oc_acp_load_replays_..., oc_acp_close_keeps_..., oc_acp_rejects_... |
| `cli/test/acp/session` | publishes a session's commands after the response that attaches it | already passing | oc_acp_initialize_..., oc_acp_load_replays_..., oc_acp_close_keeps_..., oc_acp_rejects_... |
| `cli/test/acp/session` | converts MCP configs and deduplicates registrations per session and config | after a fix | content_and_servers (`oauth: false` for HTTP servers), editor_supplied_mcp_servers_tools_go_through_the_permission_rules (adapted: one Mcp per open session, so a resume replaces the session's servers) |
| `cli/test/acp/session` | rejects relative cwds, MCP-over-ACP, and SSE servers before creating or loading a session | already passing | oc_acp_initialize_..., oc_acp_load_replays_..., oc_acp_close_keeps_..., oc_acp_rejects_... |
| `cli/test/acp/session` | close removes a new session that was never prompted and keeps every other session | already passing | oc_acp_initialize_..., oc_acp_load_replays_..., oc_acp_close_keeps_..., oc_acp_rejects_... |
| `cli/test/acp/translate` | streams text and reasoning with one message ID per reasoning ordinal | already passing | oc_acp_prompt_streams_message_ids_and_resolves_with_usage, oc_acp_replay_shape |
| `cli/test/acp/translate` | ends a length-limited step with max_tokens | already passing | oc_acp_prompt_streams_message_ids_and_resolves_with_usage, oc_acp_replay_shape |
| `cli/test/acp/translate` | sums usage across steps and reports thought and cache tokens | already passing | oc_acp_prompt_streams_message_ids_and_resolves_with_usage, oc_acp_replay_shape |
| `cli/test/acp/translate` | replays a completed compaction as one terminal update | already passing | oc_acp_compaction_updates |
| `cli/test/acp/compaction` | treats a null compaction capability as unsupported | already passing | oc_acp_compaction_updates |
| `cli/test/acp/translate` | cancels the compaction a cancelled turn left open | already passing | oc_acp_cancels_the_compaction_a_cancelled_turn_left_open |
| `cli/test/acp/translate` | streams a compaction's summary and completes it with the full summary | already passing | oc_acp_streams_a_compactions_summary_and_completes_it_with_the_full_summary |
| `cli/test/acp/translate` | clears the summary of a compaction that completes without a readable one | already passing | oc_acp_clears_the_summary_of_a_compaction_that_completes_without_a_readable_one |
| `cli/test/acp/translate` | fails a compaction with its error and clears the partial summary | already passing | oc_acp_fails_a_compaction_with_its_error_and_clears_the_partial_summary |
| `cli/test/acp/translate` | keeps core's own compaction failure messages | already passing | oc_acp_keeps_cores_own_compaction_failure_messages |
| `cli/test/acp/translate` | reports a compaction defect as a generic failure | already passing | oc_acp_reports_a_compaction_defect_as_a_generic_failure |
| `cli/test/acp/translate` | reports an interrupted compaction as cancelled | already passing | oc_acp_reports_an_interrupted_or_aborted_compaction_as_cancelled (adapted: an interrupt cancels the turn) |
| `cli/test/acp/translate` | reports an aborted compaction as cancelled | already passing | oc_acp_reports_an_interrupted_or_aborted_compaction_as_cancelled (adapted: an abort cancels the turn) |
| `cli/test/acp/translate` | opens a compaction that fails before it starts before settling it | after a fix | oc_acp_opens_a_compaction_that_fails_before_it_starts_before_settling_it |
| `cli/test/acp/translate` | drops summary chunks outside an open compaction | already passing | oc_acp_drops_summary_chunks_outside_an_open_compaction |
| `cli/test/acp/translate` | drops a compaction end without a start | already passing | oc_acp_drops_a_compaction_end_without_a_start |
| `cli/test/acp/translate` | marks a compaction without forwarding its summary when the client lacks the capability | already passing | oc_acp_marks_a_compaction_without_forwarding_its_summary_when_the_client_lacks_the_capability |
| `cli/test/acp/translate` | marks a failed compaction with the full error | already passing | oc_acp_marks_a_failed_compaction_with_the_full_error (adapted: CodeSmith's `compaction.failed` error with the full message) |
| `cli/test/acp/translate` | marks a compaction that fails before it starts without opening it | after a fix | oc_acp_marks_a_compaction_that_fails_before_it_starts_without_opening_it |
| `cli/test/acp/translate` | replays a compaction without a summary as cleared | after a fix | oc_acp_compaction_failure_shapes (the replayed checkpoint goes through the same update) |
| `cli/test/acp/translate` | replays a compaction as a marker when the client lacks the capability | already passing | oc_acp_compaction_updates |
| `cli/test/acp/translate` | replays reasoning with the same message IDs as live reasoning ordinals | already passing | oc_acp_prompt_streams_message_ids_and_resolves_with_usage, oc_acp_replay_shape |
| `cli/test/acp/turn` | streams resumed turns after background shells finish | already passing | oc_acp_background_shell_wakes_the_session |
| `cli/src/services/updater` | reads update policy from JSONC | after a fix | oc_reads_update_policy_from_jsonc |
| `cli/src/services/updater` | maps the v1 update policy | after a fix | oc_maps_the_v1_update_policy |
| `cli/src/services/updater` | reports every available release | after a fix | oc_reports_every_available_release |
| `cli/src/services/updater` | automatically installs every available release when enabled | after a fix | oc_automatically_installs_every_available_release_when_enabled (`auto` is accepted but CodeSmith only notifies) |
| `cli/src/services/updater` | skips when updates are disabled | after a fix | oc_skips_when_updates_are_disabled |
| `cli/src/services/updater` | reports up-to-date only when versions match | after a fix | oc_reports_up_to_date_only_when_versions_match |
| `cli/src/services/updater` | reports when latest is lower (rollback) | after a fix | oc_reports_when_latest_is_lower |
| `cli/src/services/updater` | accepts strict release version variants | after a fix | oc_accepts_strict_release_version_variants |
| `cli/src/services/updater` | preserves strict validity | after a fix | oc_preserves_strict_validity |
| `cli/src/services/updater` | handles numeric limits without losing precision | after a fix | oc_handles_numeric_limits_without_losing_precision |
| `cli/src/services/updater` | preserves equality for oversized numeric prerelease identifiers | after a fix | oc_preserves_equality_for_oversized_numeric_prerelease_identifiers |
| `cli/src/services/updater` | rejects versions longer than semver's limit before trimming | after a fix | oc_rejects_versions_longer_than_semvers_limit_before_trimming |
| `cli/test/upgrade` | is registered in root help and documents its options | after a fix | oc_upgrade_is_registered_in_root_help_and_documents_its_options (adapted: clap's `[TARGET]`) |
| `cli/test/upgrade` | detects the installation method and resolves the latest version | after a fix | oc_upgrade_detects_the_installation_method_and_resolves_the_latest_version |
| `cli/test/upgrade` | resolves the latest version for an explicit method without detection | after a fix | oc_upgrade_resolves_the_latest_version_for_an_explicit_method_without_detection (adapted: `--method binary`) |
| `cli/test/upgrade` | accepts an explicit version and method without detection or a version lookup | after a fix | oc_upgrade_accepts_an_explicit_version_and_method_without_detection_or_a_lookup |
| `cli/test/upgrade` | accepts the short method flag and an explicit major upgrade | after a fix | oc_upgrade_accepts_the_short_method_flag_and_an_explicit_major_upgrade |
| `cli/test/upgrade` | skips the already installed version | after a fix | oc_upgrade_skips_the_already_installed_version |
| `cli/test/upgrade` | requires an explicit method when detection fails | after a fix | oc_upgrade_requires_an_explicit_method_when_detection_fails |
| `cli/test/upgrade` | rejects unsupported methods before attempting an upgrade | after a fix | oc_upgrade_rejects_unsupported_methods_before_attempting_an_upgrade |
| `cli/test/upgrade` | reports version lookup failures without installing | after a fix | oc_upgrade_reports_version_lookup_failures_without_installing |
| `cli/test/upgrade` | reports installation failures with a nonzero exit code | after a fix | oc_upgrade_reports_installation_failures_with_a_nonzero_exit_code |
| `cli/test/updater-install` | ${method} installs the explicit V2 package version without a leading v | after a fix | oc_installs_the_explicit_version_without_a_leading_v (adapted: the release tarball) |
| `cli/test/updater-install` | npm upgrades fail when the running copy keeps its old version | after a fix | oc_upgrades_fail_when_the_new_binary_reports_another_version (adapted: the downloaded binary must report the version before it replaces anything) |
| `cli/test/updater-install` | ${manager.label} installs print the ${manager.name} command instead of updating in place | after a fix | oc_package_manager_installs_print_their_command_instead_of_updating_in_place (adapted: Homebrew and cargo) |
| `cli/test/updater-install` | bun isolates and removes its install cache after exit ${exitCode} | after a fix | oc_cleans_its_directory_after_success_and_failure (adapted: the download folder) |
| `cli/test/updater-install` | curl uses the V2 installer and cleans its directory: ${failure} | after a fix | oc_cleans_its_directory_after_success_and_failure (adapted: success, a failed download, a bad checksum) |
| `cli/test/updater-install` | invalid version targets never execute a command or create a cache | after a fix | oc_invalid_version_targets_never_make_a_request_or_create_a_folder |
| `cli/test/updater-install` | install failures expose stderr and process errors do not report success | after a fix | oc_install_failures_say_what_failed (adapted: HTTP status, unlisted tarball) |
| `cli/test/updater-install` | method detection identifies ${method ?? "an unknown installation"} using the V2 package | after a fix | oc_method_detection_identifies_the_installation (adapted: by path) |
| `cli/test/updater-install` | Homebrew Core installs check and upgrade the Core formula | after a fix | oc_package_manager_installs_print_their_command_instead_of_updating_in_place (adapted: CodeSmith names `brew upgrade` instead of running it) |
| `cli/test/updater-install` | Homebrew tap installs of ${formula} use the published tap formula | after a fix | oc_package_manager_installs_print_their_command_instead_of_updating_in_place (adapted: the reka18/tap formula) |
| `tui/test/app-lifecycle` | ctrl+c dismisses autocomplete and shell mode before exiting | already passing | oc_ctrl_c_dismisses_autocomplete_before_exiting (adapted: no shell mode) |
| `tui/test/util/model` | splits provider from a nested model identifier | already passing | oc_model_references_carry_nested_ids_and_variants |
| `tui/test/util/model` | includes the selected variant in model refs | already passing | oc_model_references_carry_nested_ids_and_variants |
| `tui/test/cli/tui/dialog-select` | shows no-match and still closes after a flat filter goes empty | already passing | oc_a_picker_with_no_matches_still_closes |
| `tui/test/prompt/attachment` | numbers PDFs independently from images | already passing | oc_pdfs_are_numbered_independently_from_images |
| `tui/test/prompt/attachment` | does not reuse numbers after an earlier attachment is removed | already passing | oc_attachment_numbers_are_not_reused_after_one_is_removed |
| `tui/test/prompt/display` | places the %s trigger where the editor shows it | already passing | oc_the_mention_trigger_is_found_after_any_separator (adapted) |
| `tui/test/prompt/display` | ignores triggers inside a token or before whitespace | already passing | oc_mention_and_slash_triggers_ignore_tokens |
| `tui/test/prompt/history-composer` | clearing a recalled entry drops the draft so down stays empty | already passing | oc_clearing_a_recalled_entry_drops_the_draft |
| `tui/test/prompt/history-provider` | down rejects at the newest history item with an empty prompt | already passing | oc_down_at_the_newest_entry_with_an_empty_prompt_does_nothing |
| `tui/test/prompt/history` | dedupes only identical consecutive entries | already passing | oc_prompt_history_skips_blanks_and_dedupes_consecutive_prompts |
| `tui/test/prompt/local-attachment` | rejects unbounded and malformed multi-file drops | already passing | oc_unbounded_and_malformed_drops_are_text, oc_unsupported_or_incomplete_drops_are_plain_text |
| `tui/test/prompt/local-attachment` | falls back to plain text for unsupported or incomplete drops | already passing | oc_unbounded_and_malformed_drops_are_text, oc_unsupported_or_incomplete_drops_are_plain_text |
| `util/test/global-roots` | uses XDG overrides | already passing | oc_paths_use_xdg_overrides |
| `util/test/global-roots` | empty XDG overrides use home directory defaults | already passing | oc_empty_xdg_overrides_use_the_home_defaults |
| `tui/test/feature-plugins/sidebar-context` | sidebar omits context before usage is available | already passing | oc_the_context_shows_once_usage_is_available (adapted: the status line holds the context) |
| `tui/test/feature-plugins/sidebar-context` | sidebar shows available context usage | already passing | oc_the_context_shows_once_usage_is_available (adapted) |
## Roadmap

Missing features, largest first, with the OpenCode tests that would come
with each (every test for groups up to 30 cases; by file, with examples,
for larger ones). Mini mode is not here: it is [deferred](#deferred), and
its remaining work is tracked in
[#1](https://github.com/reka18/codesmith/issues/1).

### tui: mouse selection and copy (32)

- `tui/test/util/selection` (8): e.g. "copy writes selected text without clearing the highlight"; "copy-on-select ignores a later non-drag release"
- `tui/test/util/selection-keys` (7): e.g. "terminal selections retain repeated copies until Escape or typing dismisses them"; "copy-on-select forwards Ctrl+C without recopying the selection"
- `tui/test/ui/session-link` (6): e.g. "session links in markdown"; "a wrapped title opens its session from either row, and plain text does not"
- `tui/test/clipboard` (5): e.g. "adapts OpenTUI image and text reads"; "uses all available routes but skips the process host remotely"
- `tui/test/cli/tui/dialog-select` (2): e.g. "Escape clears text selection before invoking a custom back action"; "ctrl+c clears a dialog text selection before closing the dialog"
- `tui/test/util/selection-copy-on-select` (2): e.g. "copy-on-select preserves multi-clicks at column $column"
- `tui/test/ui/link` (1): e.g. "plain clicks on any row of a wrapped Markdown link resolve the full URL"
- `tui/test/config-v2` (1): e.g. "validates terminal copy behavior"

### tui: per-agent and per-session model selection, the rest (2)

- `tui/test/context/local-selection`: uses the last configured model and variant ahead of recents (the object form of `model` with a variant)
- `tui/test/context/local-selection`: a manual agent switch supersedes the CLI agent after its commit (no runner-level test harness yet)

### cli: SSH askpass for git prompts (2)

- `cli/src/ssh-askpass`: the executable askpass branch returns only the response, without CLI output
- `cli/src/ssh-askpass`: a response split inside a multi-byte character is written intact

### sessions: fork (1)

- `tui/test/session-fork-pending`: choosing a row in /fork shows the pending view until the fork finishes

### tui: @agent and skill mentions (3)

- `tui/test/prompt-offset`: an agent mention after a tab replaces only the mention
- `tui/test/prompt-paste`: undo submits the restored %s chip with its attachment (agents, skills: 2 cases)

### tui: export dialog (1)

- `tui/test/component/dialog-export-options`: tab and shift+tab move a visible focus through the export options

### tui: instruction loads in the transcript (1)

- `tui/test/cli/tui/group-view`: expanded instructions stay adjacent to their summary and each other (the instruction halves of four `session-verbosity` tests are dropped too)

### tui: Code Mode calls in activity summaries (1)

- `tui/test/cli/tui/session-verbosity`: execute counts its finished nested calls instead of itself

## By design or not applicable

| Group | Kind | Cases | Files | Why |
|---|---|---:|---:|---|
| codemode: interpreter and host API | by design | 425 | 18 | OpenCode's own VM (bytecode, suspend/resume, snapshots) and Effect host API; CodeSmith runs Boa; assertions on OpenCode's scheduler traces and host gates |
| session-ui: markdown | by design | 207 | 16 | the web renderer (streaming worker, caching, images); the TUI renders Markdown itself |
| session-ui: review | by design | 144 | 13 | the web diff and review viewer |
| plugins: JavaScript plugins | by design | 142 | 15 | plugins are native Rust, compiled in; server plugins; plugin key layers |
| server: hosted features and internals | by design | 83 | 34 | experimental, hosted, PTY, VCS, workerd or Effect server internals |
| codemode: Effect tools and host diagnostics | by design | 60 | 6 | Effect Schema signatures and decoding (CodeSmith's tools are JSON Schema), diagnostic kinds and suggestion fields, extension globals, `Date` keys in UTC; object fields list in name order (CodeSmith's JSON keeps keys sorted) |
| server: managed service | by design | 55 | 6 | QR codes for pairing; no CORS configuration; browsers are refused; no background server service, pairing or remote access |
| tui: hosted catalog | by design | 54 | 11 | hosted plans and usage limits; integration accounts; integrations, favourites and the hosted model catalog; release highlights, hosted plans or onboarding |
| cli: client/server frontends | by design | 48 | 7 | run and the UI drive the session in-process, not through a server; run reads the session in-process, not by reconciling server events; sessions are resolved locally, no server locations; subagents run inside the turn |
| codemode: Boa departures | by design | 39 | 15 | Boa 0.22 departs from the specification here |
| tui: client/server state | by design | 39 | 11 | the UI runs the session in-process (and no event stream moves a session the open menu has not read yet) |
| codemode: OpenCode restrictions and result conversion | by design | 28 | 10 | opaque references, cycles, typed arrays, errors as {name,message}, awaited result promises, UTC dates |
| server: auth and transport | by design | 28 | 7 | a bearer token instead of Basic auth, pairing and CORS; browsers are refused |
| codemode: error wording | by design | 25 | 5 | Boa's messages; OpenCode's error kinds, locations and suggestions |
| cli: integrations | by design | 24 | 4 | hosted, OAuth and command integrations through a server; integration login URLs; plugin-defined login forms; CodeSmith saves API keys; the /connect integration catalog |
| tui: transcript rows | by design | 22 | 4 | CodeSmith's tool rows are its own (`⚙ name detail`), not OpenCode's inline tool copy and snapshots; no synthetic-message, retry-footer or compaction-barrier rows; a turn's error is one error row |; a running command's row keeps a 64 KiB tail (the output viewer shows the rest) rather than an expanded 1 MiB one, and a stopped call shows its result as any call's
| codemode: strict-mode checks | by design | 6 | 3 | Boa's strict mode words frozen-write errors differently, rejects early what OpenCode accepts (`delete` of a variable, a parameter named `arguments`), and misses two read-only writes |
| acp: child sessions | by design | 13 | 3 | subagents run inside the parent turn; subagents run inside the parent turn; no steering |
| server: session API | by design | 12 | 6 | CodeSmith's local API is smaller (sessions, turns, permissions, jobs, environment); this route or field is not part of it |
| tui: session tabs: mouse, motion and server sync | by design | 33 | 7 | no mouse: clicks, middle-click closing, drags, the tab menu and its recently closed list, opening a tab in the background, the compact rail reached by narrowing the column; no motion: width seeding, the completion reveal and running pulses; one UI running its sessions in-process: no server viewed times, terminal focus or acknowledgement retries, no other UI sharing the list, no session moves or per-tab locations and branches, no queued prompt pulses, new sessions start in the UI's folder; subagents have no transcript of their own; no home page footer |
| tui: attachments | by design | 10 | 2 | attachments are read on send by path; SVG is not media; no remote server; every attachment has its label in the prompt (no mentionless ones to keep), and the transcript shows a prompt's labels, not image cards to deduplicate |
| tui: dialogs | by design | 6 | 3 | error, update and variant dialogs (variants are /variant and the model id) |
| tui: new designs experiment | by design | 6 | 3 | OpenCode's `opencode2` theme and its V1 migration exist only behind its `new-designs` experiment |
| cli: stats | by design | 4 | 1 | statistics are computed locally (always with tool counts), no server request |
| acp: separate server | by design | 4 | 3 | no separate server or plugin activation; no server defects to wrap; no shared event stream |
| util: titles | by design | 4 | 1 | CodeSmith keeps untitled sessions untitled until a title is generated |
| tui: diff viewer: mouse, images and routes | by design | 33 | 5 | no mouse: OpenCode's right-click file menu, clicked folders and source label; no image previews in the terminal; the viewer is always a session's panel (no standalone route, session tabs or plugin to disable, one kind of panel); the file tree's unused list layout, and its loading and empty states, which the viewer never shows |
| tui: terminal pane | by design | 3 | 1 | no terminal pane whose palette follows the theme; commands run through the shell tool |
| tui: toasts and completion notices | by design | 6 | 2 | no mouse: a toast is not clicked to run its action or dismissed early, nor paused while hovered; completion notices are not clicked (subagents run inside the turn, so no background subagent sends one) |
| tui: settings dialog | by design | 1 | 1 | no settings dialog to search from the palette: the UI never writes the config; settings it can change are commands (`app.toggle.paste_summary`, the theme mode), which the palette lists |
| tui: group disclosure | by design | 3 | 1 | no mouse: ctrl+o opens every group, so the rows above an opened group move when scrolled up |
| server: API command | by design | 3 | 1 | no `api` command for a server |
| tui: model notices | by design | 3 | 1 | model switch notices are worded differently |
| codemode: limits | by design | 3 | 2 | 400 nested calls, 64 MiB buffers |
| cli: TUI config writes | by design | 2 | 1 | no JSON schema is published, and the UI never writes its config: the theme picked in the UI is kept in a state file |
| cli: debug config | by design | 2 | 1 | debug config prints a summary without secret values |
| acp: auth | by design | 2 | 1 | provider 401/403 map to -32000; no auth-required server event |
| tui: scrolling | by design | 2 | 1 | sending always follows the bottom; no setting |
| tui: stats | by design | 2 | 2 | stats is a CLI command, not a TUI view |
| tui: history | by design | 2 | 1 | history steps at the first/last line, not only at offset 0 |
| acp: shared sessions | by design | 1 | 1 | no other client shares a session's asks |
| acp: resource links | by design | 1 | 1 | links are referenced in the text |
| acp: patch tool | by design | 1 | 1 | CodeSmith has no patch tool |
| tui: keys | by design | 1 | 1 | Tab and Shift+Tab both cycle agents (documented) |
| tui: permission dialog | by design | 1 | 1 | the dialog shows the request title itself |
| tui: interrupt | by design | 1 | 1 | Esc interrupts at once; no armed state |
| tui: slash commands | by design | 2 | 2 | commands complete only at the start of the prompt; one parser for both UIs, where any whitespace (`\r` too) ends a command's name |
| tui: OpenTUI internals | n/a | 272 | 41 | OpenTUI cell offsets; Solid config and keymap contexts, command object identity and keymap modes, developer tools; Solid batching and renderer internals; layout, virtualization, reactive data or event-stream internals; renderer lifecycle, caching and server sync; select widget layout and reconciliation; widgets, animation and layout helpers |
| session-ui: web UI | n/a | 270 | 29 | web and desktop UI state machines |
| util: helpers | n/a | 101 | 16 | TypeScript helpers with no CodeSmith counterpart (or covered by Rust std/crates) |
| tui: paged history and anchors | n/a | 48 | 6 | the whole session is in memory and the visible tail is regrouped each frame: no history pages to load, merge at a seam or split, no row anchors or ids to restore, no reactive footer indexes |
| tui: patch chunking | n/a | 4 | 2 | OpenTUI draws a large added file in chunks (`splitAddedPatch`, its size threshold); CodeSmith draws only the rows in view and never splits a patch |
| tui: prompt parts | n/a | 21 | 3 | OpenCode's prompt part model and media refs |
| cli: runtime internals | n/a | 16 | 7 | Bun/Node runtime, bundling or web UI |
| cli: cli.json | n/a | 7 | 1 | cli.json schema, migration and file locking |
| cli: packaging | n/a | 3 | 1 | single-executable asset archive |
| cli: self-update through package managers | n/a | 11 | 2 | npm, pnpm, bun, yarn, Vite+, WinGet and Node distributions (their detection, prefixes, caches and commands), and `vp` as a method: CodeSmith ships GitHub release tarballs and Homebrew, and never runs a package manager |
| cli: migrations | n/a | 1 | 1 | credential import from older versions |
### Code Mode on Boa
| acp: compaction replay | by design | 3 | 1 | CodeSmith's history keeps completed compactions only: a failed, cancelled or running compaction leaves nothing to replay |

Of the 1137 Code Mode assertions run, 111 fail on purpose, by reason (the
test names each one):

| Reason | Assertions | Bucket |
|---|---:|---|
| OpenCode's own restrictions and result conversion (opaque function and iterator references, cycles, typed arrays at the boundary, errors returned as `{ name, message }`, promises awaited inside results, deterministic UTC dates and `en-US` formatting, empty programs) | 43 | by design |
| Boa 0.22 departs from the specification or lacks a recent addition (Set methods on Set-likes, `Error.isError`, the RegExp `d` flag, `substr`, `super` in arrows, some TDZ and property-order cases; one labeled `continue` loops forever) | 41 | by design (engine) |
| Strict-mode checks where Boa's strict mode still differs from OpenCode's (wording of frozen-write errors, an early `SyntaxError` for `delete` of a variable or a parameter named `arguments`, no `TypeError` writing a named function expression's name or in one frozen-array case) | 6 | by design |
| Boa's error wording | 16 | by design |
| CodeSmith's limits (400 nested calls where OpenCode allows 10000, 64 MiB buffers) | 4 | by design |
| Depends on the time zone (passes in UTC, fails in local time), so either outcome is accepted | 1 | by design |

Programs run in strict mode, as OpenCode applies strict-mode checks; one
that repeats a parameter name, which OpenCode accepts and strict mode
rejects as it parses, runs in sloppy mode instead.

## Deferred

| Group | Cases | Where |
|---|---:|---|
| tui: mini mode (slim mode) — parked on feature/slim-mode, see #1 | 428 | every `tui/test/mini/*` case, ported or not, and the mini settings case of `tui/test/config-v2` |

`codesmith mini` (OpenCode's split-footer mode, to be renamed slim mode)
lives on the `feature/slim-mode` branch, with its tests, until it is
finished ([#1](https://github.com/reka18/codesmith/issues/1)). The 428
cases were: 72 ported (3 already passing, 69 after a fix), the mini
settings case of `config-v2` (after a fix), 323 missing features and 32 by
design (mini themes and its permission prompt). Six of the ported ones
(`mini/prompt.shared` and `mini/runtime.queue`: blank and repeated history
entries, empty prompts, the exit, new-session and compact commands) drive
code the full UI shares, so their tests stay in `src/tui/parity.rs`; they
are counted here with the rest of mini mode.

## Not ported yet

### codemode: language (computed assertions) (215)

assertions built at run time or calling tools; not extracted some of its assertions are ported; the rest are computed

- `codemode/test/string-search-test262` (90): e.g. "(90 more)"
- `codemode/test/parity` (28): e.g. "Error values and instanceof > typeof Error is function; an unknown instanceof right-hand s"; "Object.freeze, seal, and preventExtensions > primitives pass through, and Object.assign ho"
- `codemode/test/stdlib` (26): e.g. "CodeMode values at intra-CodeMode checkpoints > Array.from rejects invalid receivers and g"; "CodeMode values at intra-CodeMode checkpoints > Object enumeration rejects invalid receive"
- `codemode/test/classes` (24): e.g. "class inheritance > derived constructor return rules"; "classes extending Error > every error type can be subclassed"
- `codemode/test/accessors` (13): e.g. "reads call the getter > a getter can return a promise, and a long getter is time sliced"; "reads call the getter > a getter that throws is caught in the caller"
- `codemode/test/promise-test262` (8): e.g. "(8 more)"
- `codemode/test/callbacks` (6): e.g. "built-in method references as callbacks > promise reactions accept built-in references"; "still-rejected callables get the wrap hint > JSON callbacks use the unified callback gate"
- `codemode/test/object-coercion-test262` (4): e.g. "(4 more)"
- `codemode/test/for-await-test262` (3): e.g. "(3 more)"
- `codemode/test/string-date-methods-test262` (3): e.g. "(3 more)"
- `codemode/test/new-expression` (2): e.g. "new on a non-constructible callee > non-callable values are not constructors"; "new on a non-constructible callee > user-defined functions explain the documented gap"
- `codemode/test/constructor-test262` (1): e.g. "(1 more)"
- `codemode/test/descriptors` (1): e.g. "(1 more)"
- `codemode/test/enumeration` (1): e.g. "for...in > non-object values enumerate like JS: strings by index, everything else nothing"
- `codemode/test/generator-test262` (1): e.g. "(1 more)"
- `codemode/test/labeled-control-test262` (1): e.g. "(1 more)"
- `codemode/test/promise` (1): e.g. "Promise.resolve / Promise.reject > resolve wraps plain values and passes promises through"
- `codemode/test/string-core-test262` (1): e.g. "(1 more)"
- `codemode/test/string-regexp-test262` (1): e.g. "(1 more)"

### acp (left to the ACP work) (21)

behaviour exists; no test cites it not cited by a test oc_acp_elicitation_mapping covers forms, not this flow stdin EOF ends the loop; not cited by a test

- `cli/test/acp/lifecycle.subprocess`: stdin EOF exits cleanly
- `cli/test/acp/prompt`: routes slash commands and compact through their session endpoints
- `cli/test/acp/prompt`: session/cancel during prompt setup or before admission returns cancelled
- `cli/test/acp/translate`: ends a normal turn with end_turn
- `cli/test/acp/translate`: ends a content-filtered step with refusal
- `cli/test/acp/translate`: ends a content-filter failure with refusal and no error
- `cli/test/acp/translate`: ends a server-side interruption with cancelled
- `cli/test/acp/translate`: ends an aborted execution with cancelled and no error
- `cli/test/acp/translate`: fails other execution failures as service failures
- `cli/test/acp/translate`: counts a failed step's tokens and clears its error when the next step starts
- `cli/test/acp/translate`: streams a tool call from pending through progress to completion
- `cli/test/acp/translate`: fails a tool call with its partial content and error
- `cli/test/acp/translate`: locates the file of an edit tool
- `cli/test/acp/translate`: locates the file of a write tool
- `cli/test/acp/translate`: keeps a background shell pending after its turn ends
- `cli/test/acp/translate`: keeps a background shell pending until its queued notice is delivered
- `cli/test/acp/translate`: settles a background shell once its notice is delivered
- `cli/test/acp/translate`: settles a background shell whose notice is cancelled
- `cli/test/acp/translate`: settles a shell that exits before its call's result arrives
- `cli/test/acp/translate`: leaves a user's shell command, whose notice waits for the next prompt, out of pending work
- `cli/test/acp/turn`: cancels unsupported session forms so execution can continue

### tui: text truncation (12)

file paths and middle truncation, which view.rs does not do as OpenCode does

- `tui/test/ui/file-path`: keeps the full path when it fits
- `tui/test/ui/file-path`: adds nearest parent segments from right to left
- `tui/test/ui/file-path`: preserves the working directory and branch suffix
- `tui/test/ui/file-path`: uses remaining width for part of a long parent segment
- `tui/test/ui/file-path`: preserves the extension when the basename must shrink
- `tui/test/ui/file-path`: preserves the input separator
- `tui/test/ui/file-path`: does not treat a backslash in a POSIX filename as a separator
- `tui/test/ui/file-path`: preserves absolute roots
- `tui/test/ui/file-path`: measures terminal columns without splitting graphemes
- `tui/test/ui/file-path`: never exceeds the requested width
- `tui/test/util/locale`: truncates text in the middle without overflowing small widths
- `tui/test/util/path-format`: formats relative, home, and foreign paths

### util: activity calendar (10)

CodeSmith draws the calendar in stats.rs (private helpers)

- `util/src/activity-calendar`: aligns Monday-first weeks and excludes cells outside the end-exclusive range
- `util/src/activity-calendar`: clips to the latest weeks and keeps empty days distinct from out-of-range cells
- `util/src/activity-calendar`: caps long histories at 53 weeks
- `util/src/activity-calendar`: returns month positions and spans for partial first and last months
- `util/src/activity-calendar`: places month labels across a year boundary
- `util/src/activity-calendar`: keeps exactly seven distinct cells per week across DST and leap days
- `util/src/activity-calendar`: uses the highest intensity for a single distinct positive count
- `util/src/activity-calendar`: ranks distinct activity counts in four levels
- `util/src/activity-calendar`: keeps intensity relative to the requested range when older weeks are clipped
- `util/src/activity-calendar`: returns an empty calendar for an empty range

### tui: tool output (1)

CodeSmith collapses output; the line budget lives in view.rs

- `tui/test/cli/tui/collapse-tool-output`: limits command input and output to the same line budget
