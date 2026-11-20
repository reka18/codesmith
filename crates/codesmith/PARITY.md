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
  `dialog-select`, `app-lifecycle`, `mini/prompt.shared`,
  `mini/runtime.queue`).
- `src/tui/theme/tests.rs`: themes, their V1 migration, mode selection and
  the system theme (`tui/test/theme*`, `cli/tui/theme-mode`,
  `context/theme`).
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
- `src/acp.rs`: the `oc_acp_*` tests, with the ACP tests not ported listed
  above its test module.

## Summary

| Package / area | Tests examined | Ported (already passing) | Ported after a fix | Gaps: missing feature | Gaps: by design or n/a | Not ported yet |
|---|---:|---:|---:|---:|---:|---:|
| cli | 269 | 37 | 15 | 52 | 165 | 0 |
| cli: ACP | 105 | 40 | 0 | 22 | 22 | 21 |
| tui: main UI | 1250 | 13 | 98 | 610 | 516 | 13 |
| tui: mini mode | 425 | 3 | 3 | 417 | 2 | 0 |
| server | 123 | 0 | 0 | 0 | 123 | 0 |
| codemode | 1666 | 647 | 179 | 39 | 586 | 215 |
| plugin | 38 | 0 | 0 | 0 | 38 | 0 |
| util | 140 | 2 | 0 | 17 | 111 | 10 |
| session-ui | 621 | 0 | 0 | 0 | 621 | 0 |
| **Total** | **4637** | **742** | **295** | **1157** | **2184** | **259** |

## Fixes made while porting

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
| `tui/test/mini/prompt.shared` | recognizes exit commands | after a fix | oc_exit_commands_ignore_case_and_space_but_not_arguments |
| `tui/test/mini/prompt.shared` | recognizes the new-session command | after a fix | oc_the_new_session_command_ignores_case_and_space_but_not_arguments |
| `tui/test/mini/prompt.shared` | recognizes only the compact command | after a fix | oc_only_the_compact_command_compacts |
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
| `tui/test/config-v2` | resolves nested config and keybind defaults | after a fix | oc_resolves_nested_config_and_keybind_defaults (adapted: the scroll, diffs, debug, tabs and session halves are dropped) |
| `tui/test/config-v2` | uses command IDs as keybind keys | after a fix | oc_uses_command_ids_as_keybind_keys |
| `tui/test/config-v2` | preserves current navigation defaults | after a fix | oc_preserves_current_navigation_defaults |
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
| `cli/test/stats` | renders shared intensity levels and partial month labels | after a fix | oc_stats_labels_only_the_months_a_partial_range_shows (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | keeps the default card shareable | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | renders only requested detail tables | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | shows when detail tables omit rows | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | uses the OpenCode palette in color mode | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | renders a concise empty state | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/stats` | labels activity when terminal width truncates the requested range | already passing | oc_stats_* (codesmith-core/tests/oc_stats.rs) |
| `cli/test/run/noninteractive` | waits for a background shell and streams the reply it wakes | already passing | run_cli.rs run_waits_for_background_commands_and_resumes_the_model |
| `cli/test/run/noninteractive` | cancelling while waiting on background work stops the Session and this prompt's jobs | already passing | run_cli.rs interrupting_a_run_stops_its_background_server (adapted) |
| `cli/test/acp/additional-directories.subprocess` | tools read files in an additional directory without an external directory ask | already passing | oc_acp_additional_directories |
| `cli/test/acp/additional-directories` | rejects %p before creating a session (4 cases) | already passing | oc_acp_additional_directories |
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
| `cli/test/acp/session` | converts MCP configs and deduplicates registrations per session and config | already passing | oc_acp_initialize_..., oc_acp_load_replays_..., oc_acp_close_keeps_..., oc_acp_rejects_... |
| `cli/test/acp/session` | rejects relative cwds, MCP-over-ACP, and SSE servers before creating or loading a session | already passing | oc_acp_initialize_..., oc_acp_load_replays_..., oc_acp_close_keeps_..., oc_acp_rejects_... |
| `cli/test/acp/session` | close removes a new session that was never prompted and keeps every other session | already passing | oc_acp_initialize_..., oc_acp_load_replays_..., oc_acp_close_keeps_..., oc_acp_rejects_... |
| `cli/test/acp/translate` | streams text and reasoning with one message ID per reasoning ordinal | already passing | oc_acp_prompt_streams_message_ids_and_resolves_with_usage, oc_acp_replay_shape |
| `cli/test/acp/translate` | ends a length-limited step with max_tokens | already passing | oc_acp_prompt_streams_message_ids_and_resolves_with_usage, oc_acp_replay_shape |
| `cli/test/acp/translate` | sums usage across steps and reports thought and cache tokens | already passing | oc_acp_prompt_streams_message_ids_and_resolves_with_usage, oc_acp_replay_shape |
| `cli/test/acp/translate` | replays a completed compaction as one terminal update | already passing | oc_acp_compaction_updates |
| `cli/test/acp/translate` | replays reasoning with the same message IDs as live reasoning ordinals | already passing | oc_acp_prompt_streams_message_ids_and_resolves_with_usage, oc_acp_replay_shape |
| `cli/test/acp/turn` | streams resumed turns after background shells finish | already passing | oc_acp_background_shell_wakes_the_session |
| `tui/test/app-lifecycle` | ctrl+c dismisses autocomplete and shell mode before exiting | already passing | oc_ctrl_c_dismisses_autocomplete_before_exiting (adapted: no shell mode) |
| `tui/test/util/model` | splits provider from a nested model identifier | already passing | oc_model_references_carry_nested_ids_and_variants |
| `tui/test/util/model` | includes the selected variant in model refs | already passing | oc_model_references_carry_nested_ids_and_variants |
| `tui/test/mini/prompt.shared` | filters blank prompts and dedupes consecutive history | already passing | oc_prompt_history_skips_blanks_and_dedupes_consecutive_prompts |
| `tui/test/mini/prompt.shared` | push ignores blanks and dedupes only the latest item | already passing | oc_prompt_history_skips_blanks_and_dedupes_consecutive_prompts |
| `tui/test/mini/runtime.queue` | ignores empty prompts | already passing | oc_empty_prompts_are_not_sent |
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
## Roadmap

Missing features, largest first, with the OpenCode tests that would come
with each (every test for groups up to 30 cases; by file, with examples,
for larger ones).

### tui: mini (inline scrollback) mode (418)

- `tui/test/mini/footer.view` (81): e.g. "mini pastes, previews, submits, and recalls an image ($"; "mini waits for image paste before submitting and drops a paste after editing the draft"
- `tui/test/mini/stream-v2.transport` (70): e.g. "cancels the first local %s after attaching while busy"; "renders user content once (%s first)"
- `tui/test/mini/blocker.responsive` (39): e.g. "roomy question forms retain the established spacing and inline descriptions"; "complete questions and distinct option details remain r"
- `tui/test/mini/splash` (26): e.g. "entry renders metadata with distinct foreground roles (%o)"; "entry progressively admits the basename, whole version, an"
- `tui/test/mini/scrollback.surface` (25): e.g. "turn summary starts at the left edge"; "theme swaps restyle active reasoning without resetting the stream"
- `tui/test/mini/theme` (25): e.g. "theme lookup does not allocate resources after renderer shutdown"; "preserves %s monochrome terminal defaults"
- `tui/test/mini/footer` (23): e.g. "coalesces progress only within the same message and tool state"; "tools setting %s controls tool and skill transcript"
- `tui/test/mini/runtime.queue` (21): e.g. "runs and queues image-only prompts without optimistic image rows"; "treats %s as a close command"
- `tui/test/mini/entry.body` (19): e.g. "renders a failed direct shell as an error instead of completed success"; "renders assistant, reasoning, and user entries in their display formats"
- `tui/test/mini/footer.responsive` (15): e.g. "production footer confirms exit with a visible menu hint a"; "production statusline restores identity and trailing me"
- `tui/test/mini/scrollback.image` (12): e.g. "fits a $name image at the left edge and commits only its f"; "reserves wrapped caption rows and allows caption-only output in a short terminal"
- `tui/test/mini/footer.width` (8): e.g. "progressive layouts fit and preserve information at every"; "identity admission cannot separate the selected variant from its model"
- `tui/test/mini/permission.shared` (7): e.g. "replies immediately for allow once"; "requires confirmation for allow always"
- `tui/test/mini/runtime` (6): e.g. "resolves the default model reactively without blocking catalog startup"; "routes form responses to their owners with global location and local settlement"
- `tui/test/mini/session.shared` (6): e.g. "builds user prompts from projected text and attachments"; "leaves attachment sources undefined when projected mentions are absent"
- `tui/test/mini/prompt.editor` (5): e.g. "strips the local /editor command from the initial editor text"; "realigns file and agent parts after external editing"
- `tui/test/mini/tool` (5): e.g. "uses V2 shell output without the model-facing status"; "normalizes only persisted tool aliases into current fields"
- `tui/test/mini/verbosity` (5): e.g. "default Mini settings match the default verbosity preset"; "verbosity presets leave splash, spinner, and mono alone"
- `tui/test/mini/variant.shared` (4): e.g. "prefers cli then session then saved variants"; "cycles through variants and back to default"
- `tui/test/mini/runtime.boot` (3): e.g. "falls back to default tui keymap config when config load fails"; "preserves shared config while resolving independent Mini defaults"
- `tui/test/mini/catalog.shared` (2): e.g. "loads visible project references from the current reference catalog"; "merges current providers and models into the footer catalog shape"
- `tui/test/mini/footer-keymap` (2): e.g. "configured subagent key updates its hint and action"; "disabled subagent interrupt has no component fallback"
- `tui/test/mini/footer.menu` (2): e.g. "footer menu scrolls before the selected row hits the bottom edge"; "footer menu scrolls before the selected row hits the top edge"
- `tui/test/mini/form.shared` (2): e.g. "builds every supported answer and preserves owner location"; "rejects invalid and deliberately unsupported shapes"
- `tui/test/mini/prompt.shared` (2): e.g. "appends and shifts part ranges by the %s offset"
- `tui/test/mini/stream` (2): e.g. "defaults status patches to running phase"; "delivers commits before ordered footer updates"
- `tui/test/config-v2` (1): e.g. "validates mini replay and work spinner settings"

### tui: diff viewer and review panel (164)

- `tui/test/cli/tui/diff-viewer` (104): e.g. "closing the diff viewer returns to the route it opened from"; "standalone full-screen diff does not expose panel-only tog"
- `tui/test/feature-plugins/diff-viewer-file-tree-utils` (16): e.g. "builds a nested tree with deduplicated directories and file indexes"; "sorts directories before files and alphabetically within each group"
- `tui/test/session-panel` (16): e.g. "/diff focuses the production session panel without sending typing to the prompt"; "repeated fullscreen toggles retain the viewer, transcript, scroll, and reviewed files"
- `tui/test/cli/tui/diff-viewer-file-tree` (13): e.g. "defaults to text-line file icons and triangle folders with straight rails"; "keeps loading and error quiet while rendering an empty settled state"
- `tui/test/cli/tui/diff-viewer-image` (7): e.g. "recognizes only supported image file extensions"; "renders a real image at $width columns in $mode mode"
- `tui/test/panel` (4): e.g. "presentation changes preserve the selected panel identity"; "narrow geometry overrides presentation without discarding the user's choice"
- `tui/test/util/added-patch` (3): e.g. "splits a complete new-file patch into independently numbered chunks"; "retains a missing-final-newline marker only on the last chunk"
- `tui/test/config-v2` (1): e.g. "validates the explicit diff source defaults"

### tui: grouped transcript and verbosity levels (109)

- `tui/test/cli/tui/session-rows` (22): e.g. "indexed tail footer calculations do not subscribe to an unrelated history prefix"; "filters OpenAI cache quantization from cache reuse drops"
- `tui/test/session-history-interactions` (17): e.g. "pending history preserves the reader: $mode, $width col"
- `tui/test/session-home` (17): e.g. "Home loads a stable, bounded beginning (%s)"
- `tui/test/cli/tui/session-verbosity` (16): e.g. "reasoning and exploration group at every level; other tools stand alone"; "medium and high add web tools to exploration and group instruction loads"
- `tui/test/cli/tui/inline-tool-wrap-snapshot` (12): e.g. "falls back for unknown tool names"; "replaces pending copy when a tool fails before completion"
- `tui/test/cli/tui/grouping-tree` (6): e.g. "groups adjacent entries by path and counts leaves, not wrappers"; "a direct child breaks a subgroup without ending the outer group"
- `tui/test/config-v2` (2): e.g. "shows transcript verbosity in session settings"; "names tool grouping explicitly in settings"
- `tui/test/cli/tui/group-view` (4): e.g. "retains nested expansion state and registers exact headers and parts"; "expanded instructions stay adjacent to their summary and each other"
- `tui/test/session-activity-anchor` (4): e.g. "low activity details open below a summary that keeps it"; "a low activity summary that ends the transcript opens like v2 at the bottom"
- `tui/test/session-group-navigation` (4): e.g. "restores exact part/group anchors at width %s with %s t"
- `tui/test/session-history-reveal` (2): e.g. "scrolling up into older history never flashes: $width c"
- `tui/test/cli/tui/session-grouping` (1): e.g. "a pending tool does not hide a later tool reusing its call ID in another message"
- `tui/test/session-backfill-disclosure` (1): e.g. "clicking a backfilled thought group toggles it"
- `tui/test/session-mount-budget` (1): e.g. "an expanded group spends the mounting budget that older rows used"

### tui: session tabs (75)

- `tui/test/context/session-tabs-model` (34): e.g. "appends the branch to the project detail"; "labels tabs by ordinal"
- `tui/test/context/session-tabs` (25): e.g. "loads persisted tab metadata concurrently on connect"; "loads VCS metadata for each persisted tab location"
- `tui/test/component/session-tabs-mouse` (4): e.g. "releasing a transcript selection over tab controls does not activate them"; "middle-click closes a session tab without selecting it"
- `tui/test/component/session-tabs-status` (4): e.g. "${orientation} tabs replace ordinals with status without moving titles and keep context me"; "vertical running tab pulse spans the full %i-column tab"
- `tui/test/config-v2` (3): e.g. "validates the session tabs setting"; "resolves automatic tabs from the terminal environment"
- `tui/test/app-lifecycle` (2): e.g. "vertical session tabs switch to horizontal below readable content width"; "narrow vertical session tabs collapse to a compact rail with the terminal"
- `tui/test/feature-plugins/home-footer` (2): e.g. "keeps failure labels readable at the minimum supported width"; "adds secondary hints as space becomes available"
- `tui/test/component/session-tabs-rail` (1): e.g. "compact rail renders and controls session tabs"

### cli: self-update (upgrade command, update checks) (43)

- `cli/test/updater-install` (20): e.g. "${method} installs the explicit V2 package version without a leading v"; "npm upgrades fail when the running copy keeps its old version"
- `cli/src/services/updater` (12): e.g. "reads update policy from JSONC"; "maps the v1 update policy"
- `cli/test/upgrade` (11): e.g. "is registered in root help and documents its options"; "detects the installation method and resolves the latest version"

### tui: project, worktree and session browser (43)

- `tui/test/cli/tui/dialog-open` (22): e.g. "selecting an unhydrated session preserves its location"; "finds and opens an exact child session ID outside the recent list"
- `tui/test/cli/tui/prompt-move` (19): e.g. "passes the current project and uses server worktree defaul"; "uses Home's selected location instead of launch in $name"
- `tui/test/cli/tui/dialog-session-list` (1): e.g. "scopes sessions to the active session location"
- `tui/test/config-v2` (1): e.g. "shows the new session location default in settings"

### codemode: OpenAPI tools (39)

- `codemode/test/openapi` (39): e.g. "(all 39)"

### tui: rich question forms (multi-select, custom answers) (34)

- `tui/test/cli/tui/form` (30): e.g. "restores the composer when terminal-form revalidation fails"; "restores the composer after a terminal ${status} reply"
- `tui/test/util/form` (4): e.g. "initializes configured and custom defaults"; "validates every supported field constraint"

### tui: mouse selection and copy (32)

- `tui/test/util/selection` (8): e.g. "copy writes selected text without clearing the highlight"; "copy-on-select ignores a later non-drag release"
- `tui/test/util/selection-keys` (7): e.g. "terminal selections retain repeated copies until Escape or typing dismisses them"; "copy-on-select forwards Ctrl+C without recopying the selection"
- `tui/test/ui/session-link` (6): e.g. "session links in markdown"; "a wrapped title opens its session from either row, and plain text does not"
- `tui/test/clipboard` (5): e.g. "adapts OpenTUI image and text reads"; "uses all available routes but skips the process host remotely"
- `tui/test/cli/tui/dialog-select` (2): e.g. "Escape clears text selection before invoking a custom back action"; "ctrl+c clears a dialog text selection before closing the dialog"
- `tui/test/util/selection-copy-on-select` (2): e.g. "copy-on-select preserves multi-clicks at column $column"
- `tui/test/ui/link` (1): e.g. "plain clicks on any row of a wrapped Markdown link resolve the full URL"
- `tui/test/config-v2` (1): e.g. "validates terminal copy behavior"

### tui: shell mode (!) (21)

- `tui/test/prompt-shell-failure`: a failed shell command in an existing session toasts and restores the composer
- `tui/test/prompt/autocomplete`: lists parents and siblings without changing location
- `tui/test/prompt/autocomplete`: resolves home and nested sibling completions against the current location
- `tui/test/prompt/autocomplete`: starts %j after the command separator (4 cases)
- `tui/test/prompt/autocomplete`: does not complete the command token
- `tui/test/prompt/autocomplete`: searches from home after a home prefix
- `tui/test/prompt/autocomplete`: searches from parent prefixes
- `tui/test/prompt/autocomplete`: keeps ordinary searches rooted at the current directory
- `tui/test/prompt/autocomplete`: preserves explicit roots
- `tui/test/prompt/autocomplete`: includes complete explicit roots
- `tui/test/prompt/autocomplete`: omits incomplete and implicit roots
- `tui/test/prompt/autocomplete`: hides dot directories for an empty component
- `tui/test/prompt/autocomplete`: shows dot directories when explicitly filtered
- `tui/test/prompt/autocomplete`: abbreviates home paths
- `tui/test/prompt/autocomplete`: keeps paths outside home absolute
- `tui/test/prompt/traits`: normal mode without autocomplete only captures tab
- `tui/test/prompt/traits`: normal mode with autocomplete captures navigation keys
- `tui/test/prompt/traits`: shell mode disables capture and labels the prompt without suspending

### acp: streamed compaction (20)

- `cli/test/acp/compaction`: treats a null compaction capability as unsupported
- `cli/test/acp/translate`: cancels the compaction a cancelled turn left open
- `cli/test/acp/translate`: streams a compaction's summary and completes it with the full summary
- `cli/test/acp/translate`: clears the summary of a compaction that completes without a readable one
- `cli/test/acp/translate`: fails a compaction with its error and clears the partial summary
- `cli/test/acp/translate`: keeps core's own compaction failure messages
- `cli/test/acp/translate`: reports a compaction defect as a generic failure
- `cli/test/acp/translate`: reports an interrupted compaction as cancelled
- `cli/test/acp/translate`: reports an aborted compaction as cancelled
- `cli/test/acp/translate`: opens a compaction that fails before it starts before settling it
- `cli/test/acp/translate`: drops summary chunks outside an open compaction
- `cli/test/acp/translate`: drops a compaction end without a start
- `cli/test/acp/translate`: marks a compaction without forwarding its summary when the client lacks the capability
- `cli/test/acp/translate`: marks a failed compaction with the full error
- `cli/test/acp/translate`: marks a compaction that fails before it starts without opening it
- `cli/test/acp/translate`: replays a compaction without a summary as cleared
- `cli/test/acp/translate`: replays a failed compaction with its error
- `cli/test/acp/translate`: replays a cancelled compaction as cancelled
- `cli/test/acp/translate`: skips a running compaction on replay
- `cli/test/acp/translate`: replays a compaction as a marker when the client lacks the capability

### tui: notifications (toasts, sounds, completion notices) (19)

- `tui/test/completion-notices`: shell completion notices do not navigate at width %s (2 cases)
- `tui/test/completion-notices`: subagent completion notices navigate to the child session (2 cases)
- `tui/test/plugin-toast`: toast for %s's family shows as-is while %s is open (3 cases)
- `tui/test/cli/cmd/tui/notifications`: shows execution failures as session-scoped toasts without needing an assistant message
- `tui/test/cli/cmd/tui/notifications`: notifies for form and permission requests with blurred notifications and always-on sounds
- `tui/test/cli/cmd/tui/notifications`: notifies for global forms once the TUI can render them
- `tui/test/cli/cmd/tui/notifications`: dedupes pending forms and permissions until they are resolved
- `tui/test/cli/cmd/tui/notifications`: notifies for terminal lifecycle events even when attached after execution started
- `tui/test/cli/cmd/tui/notifications`: uses sound-only notifications and subagent_done sound for subagent sessions
- `tui/test/cli/cmd/tui/notifications`: plays done only once none of the session's direct children is running
- `tui/test/cli/cmd/tui/notifications`: notifies session errors once and suppresses the following idle done notification
- `tui/test/cli/tui/toast`: activation runs an action and keeps a queued toast paused
- `tui/test/cli/tui/toast`: activation dismisses a toast without an action
- `tui/test/cli/tui/toast`: toast hides only the terminal cursor it covers, until dismissed
- `tui/test/config`: defines attention sound names

### tui: persistent prompt history and drafts (16)

- `tui/test/prompt/draft-stash`: tab-keyed drafts stay on the tab they were written in
- `tui/test/prompt/draft-stash`: each tab keeps its own draft, including home
- `tui/test/prompt/draft-stash`: a newer draft for the same slot replaces the older one
- `tui/test/prompt/history-provider`: shares prompt history across sessions and the home composer
- `tui/test/prompt/history-provider`: keeps legacy unscoped history on the home composer
- `tui/test/prompt/history-provider`: reset clears browsing position and live draft, and move returns a deep copy
- `tui/test/prompt/history`: recovers valid JSONL entries around corruption
- `tui/test/prompt/history`: ignores the legacy parts shape
- `tui/test/prompt/history`: retains only the newest entries
- `tui/test/prompt/history`: does not dedupe entries with different attachments
- `tui/test/prompt/history`: preserves duplicate attachment mentions for prompt restoration
- `tui/test/prompt/history`: appends a prompt and shifts ranges by the %s offset (2 cases)
- `tui/test/prompt/jsonl`: stash JSONL skips corruption and retains newest entries
- `tui/test/prompt/jsonl`: frecency JSONL skips corruption, keeps latest path state, and limits entries
- `tui/test/prompt/persistence`: persistence creates parent directories and supports text, append, and JSON

### tui: paste chips and prompt undo (14)

- `tui/test/prompt-paste`: undo submits the restored %s paste chip (2 cases)
- `tui/test/prompt-paste`: a pasted image that reuses a wide label gets a chip as wide as the label
- `tui/test/prompt-paste`: %s grows the prompt in the first frame that shows it (2 cases)
- `tui/test/prompt-paste`: undo submits the restored %s chip with its attachment (3 cases)
- `tui/test/prompt/attachment`: deduplicates identical inline images while preserving other attachments
- `tui/test/prompt/attachment`: reuses labels for identical image data
- `tui/test/prompt/attachment`: does not reuse a label when attachment metadata differs
- `tui/test/prompt/attachment`: preserves mentionless attachments when tracked mentions are synchronized
- `tui/test/prompt/attachment`: deduplicates visible inline image cards without dropping durable references
- `tui/test/prompt/attachment`: deduplicates visible image cards the server sent as media refs by their ref

### tui: model favourites and recents (12)

- `tui/test/model-preference`: repairs known model preferences and preserves unrelated fields
- `tui/test/model-preference`: atomically serializes model preference updates
- `tui/test/context/local-selection`: cycles all recent models in a stable order in both directions
- `tui/test/context/local-selection`: uses the last configured model and variant ahead of recents
- `tui/test/context/local-selection`: switching agents restores their model and variant within the session
- `tui/test/context/local-selection`: agent and model drafts are isolated across sessions and survive navigation
- `tui/test/context/local-selection`: retains an unavailable saved session model instead of selecting a fallback
- `tui/test/context/local-selection`: new-session fallback still skips unavailable CLI and recent models
- `tui/test/context/local-selection`: a manual agent switch supersedes the CLI agent after its commit
- `tui/test/context/local-selection`: a late inactive-agent acknowledgment preserves its choice after the active agent commits
- `tui/test/context/local-selection`: same-model agent switches clear drafts without a model acknowledgment
- `tui/test/context/local`: moves a model to the front, deduplicates, and limits recents

### tui: background command output viewer (10)

- `tui/test/session-shell-output`: an expanded background shell keeps its 1 MiB tail on a whole character
- `tui/test/session-shell-stopped`: a call the user stopped shows its output once, clipped like a finished call's, then the notice once
- `tui/test/component/dialog-shell-output`: shell output opens, follows, scrolls, and survives exit at (2 cases)
- `tui/test/component/dialog-shell-output`: empty output explains redirection, retries errors, and preserves output after removal
- `tui/test/component/dialog-shell-output`: mouse-wheel scrolling pauses and resumes output following (2 cases)
- `tui/test/component/dialog-shell-output`: large captures open at a bounded tail and clicking a shell opens the viewer
- `tui/test/component/dialog-shell-output`: new designs show footer actions instead of the legacy hint line
- `tui/test/component/dialog-shell-output`: the retained 64 KiB tail starts on a whole character

### tui: subagent and shell panels (10)

- `tui/test/session-subagent-navigation`: Home and End navigate a subagent transcript
- `tui/test/util/session`: flattens nested subagents from any session in the family
- `tui/test/cli/tui/composer-keymap`: disabled subagent bindings have no component fallbacks
- `tui/test/cli/tui/composer-keymap`: the inactive subagent filter only changes with its toggle
- `tui/test/cli/tui/composer-keymap`: disabled shell bindings have no component fallbacks
- `tui/test/cli/tui/composer-keymap`: composer tabs switch by mouse and keyboard
- `tui/test/cli/tui/composer-keymap`: shell list shows one line per command
- `tui/test/cli/tui/composer-keymap`: configured composer bindings work with a focused textarea
- `tui/test/cli/tui/composer-keymap`: ctrl+c closes the active composer
- `tui/test/cli/tui/composer-keymap`: shell output respects a configured binding with a focused textarea

### tui: turn duration and throughput (10)

- `util/src/session-turn`: measures turn duration from the user prompt across assistant steps
- `util/src/session-turn`: measures decode throughput from the first streamed token
- `util/src/session-turn`: falls back to the request window for steps recorded without a first token
- `util/src/session-turn`: measures request throughput including reasoning without tool time
- `util/src/session-turn`: omits turn throughput when a stream boundary is unavailable
- `util/src/session-turn`: measures historical footers without later inputs or inc (2 cases)
- `util/src/session-turn`: preserves missing-anchor fallbacks without including the absent assistant's tokens
- `util/src/session-turn`: measures a marker-era turn from its first prompt across steers
- `tui/test/config-v2`: shows the TPS default in session settings

### tui: @session mentions (8)

- `util/src/session-mention`: session tokens round-trip independently of the title
- `util/src/session-mention`: does not parse malformed or embedded IDs: %s (7 cases)

### tui: jump between messages (8)

- `tui/test/cli/tui/message-navigation`: adds only enough slack to align the selected message
- `tui/test/cli/tui/message-navigation`: finds the next user message without stopping at an assistant message
- `tui/test/cli/tui/message-navigation`: finds the previous user message without stopping at an assistant message
- `tui/test/cli/tui/message-navigation`: preserves navigation across both user and assistant messages
- `tui/test/cli/tui/message-navigation`: uses the selected message when the viewport is too tall to scroll
- `tui/test/cli/tui/message-navigation`: stops at the first and last selected user message
- `tui/test/cli/tui/message-navigation`: keeps the logical boundary when layout temporarily moves it outside the viewport
- `tui/test/cli/tui/message-navigation`: stops at the first and last message

### tui: external editor (5)

- `tui/test/editor`: rejects when the external editor cannot start
- `tui/test/editor`: returns the edited prompt and resumes the renderer
- `tui/test/editor`: says which variable to set when no external editor is configured
- `tui/test/editor`: normalizes a single trailing editor newline for one-line prompts
- `tui/test/editor`: preserves multiline prompts that end with a newline

### tui: reasoning titles (5)

- `tui/test/cli/tui/thinking`: extracts a leading summary title and leaves markdown body
- `tui/test/cli/tui/thinking`: extracts a completed title before its streamed body arrives
- `tui/test/cli/tui/thinking`: preserves markdown-significant indentation in the extracted body
- `tui/test/cli/tui/thinking`: does not consume ordinary leading bold content
- `tui/test/cli/tui/thinking`: leaves content without a leading title in its body

### tui: sidebar and footer status (5)

- `tui/test/feature-plugins/prompt-footer`: prompt footer separates simultaneous subagent, shell, and usage status
- `tui/test/feature-plugins/prompt-footer`: prompt footer can hide details
- `tui/test/feature-plugins/prompt-footer`: prompt footer shows the session's goal without its objective and opens it on click
- `tui/test/feature-plugins/sidebar-context`: sidebar omits context before usage is available
- `tui/test/feature-plugins/sidebar-context`: sidebar shows available context usage

### tui: terminal title (5)

- `tui/test/app-lifecycle`: boots on the simulation renderer with the terminal title enabled
- `tui/test/app-lifecycle`: SIGHUP clears title and disposes scoped resources once
- `tui/test/app-lifecycle`: session lifecycle updates the terminal title and prints the epilogue after cleanup
- `tui/test/app-lifecycle`: session title generated while an untitled session is loading remains visible
- `tui/test/app-lifecycle`: automatic rename refreshes the displayed title before settling, even without a renamed event

### tui: MCP dialog (status, OAuth) (4)

- `tui/test/cli/tui/dialog-mcp`: starts OAuth with %s for an MCP server requiring authentic (2 cases)
- `tui/test/cli/tui/dialog-mcp`: returns to the MCP server list after OAuth completes
- `tui/test/cli/tui/dialog-mcp`: opens an investigation draft for a failed MCP server at its originating location

### tui: command palette (4)

- `tui/test/copy-session-id`: Copy session ID from Ctrl+P (%s) (3 cases)
- `tui/test/cli/tui/command-palette`: searches settings globally and opens the matching setting

### tui: exit summary with the resume command (4)

- `tui/test/util/presentation`: prints nothing without a session or update
- `tui/test/util/presentation`: formats session continuation summary
- `tui/test/util/presentation`: tells the user to start OpenCode again after an update
- `tui/test/util/presentation`: keeps the session continuation after an update

### tui: terminal status reporting (4)

- `tui/test/context/program-status`: reports a blocked session on the root and its own record
- `tui/test/context/program-status`: summarizes several pending sessions and clears resolved ones
- `tui/test/context/program-status`: keeps record IDs within the protocol's name rules
- `tui/test/context/program-status`: clips long titles to the protocol byte limit

### cli: worktree from a subfolder (3)

- `cli/test/session-target`: creates a worktree from the caller's checkout
- `cli/test/session-target`: keeps the caller's subfolder in the new worktree
- `cli/test/session-target`: uses the worktree root when the subfolder is not in the worktree

### acp: additional directories on load, resume and fork (2)

- `cli/test/acp/additional-directories`: load and resume replace ACP grants and keep other session rules and metadata
- `cli/test/acp/additional-directories`: forks replace inherited grants with the requested list

### cli: SSH askpass for git prompts (2)

- `cli/src/ssh-askpass`: the executable askpass branch returns only the response, without CLI output
- `cli/src/ssh-askpass`: a response split inside a multi-byte character is written intact

### cli: interactive pickers for auth targets (2)

- `cli/test/mcp-auth-choices`: offers only OAuth-capable MCP servers by their server identity
- `cli/test/mcp-auth-choices`: mcp auth accepts an optional server name and rejects no-name noninteractive calls before connecting

### tui: @file line ranges (2)

- `tui/test/prompt/parse`: preserves file line-range parsing semantics
- `tui/test/prompt/parse`: keeps frontend-specific slash separators

### cli: stats narrow layout (1)

- `cli/test/stats`: uses compact layouts in narrow terminals

### sessions: archiving (1)

- `cli/test/run/archived`: run --archived archives the new session before subscribing to its prompt

### sessions: fork (1)

- `tui/test/session-fork-pending`: choosing a row in /fork shows the pending view until the fork finishes

### tui: @agent mentions (1)

- `tui/test/prompt-offset`: an agent mention after a tab replaces only the mention

### tui: export dialog (1)

- `tui/test/component/dialog-export-options`: tab and shift+tab move a visible focus through the export options

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
| tui: client/server state | by design | 38 | 10 | the UI runs the session in-process |
| codemode: OpenCode restrictions and result conversion | by design | 28 | 10 | opaque references, cycles, typed arrays, errors as {name,message}, awaited result promises, UTC dates |
| server: auth and transport | by design | 28 | 7 | a bearer token instead of Basic auth, pairing and CORS; browsers are refused |
| codemode: error wording | by design | 25 | 5 | Boa's messages; OpenCode's error kinds, locations and suggestions |
| cli: integrations | by design | 24 | 4 | hosted, OAuth and command integrations through a server; integration login URLs; plugin-defined login forms; CodeSmith saves API keys; the /connect integration catalog |
| codemode: strict-mode checks | by design | 6 | 3 | Boa's strict mode words frozen-write errors differently, rejects early what OpenCode accepts (`delete` of a variable, a parameter named `arguments`), and misses two read-only writes |
| acp: child sessions | by design | 13 | 3 | subagents run inside the parent turn; subagents run inside the parent turn; no steering |
| server: session API | by design | 12 | 6 | CodeSmith's local API is smaller (sessions, turns, permissions, jobs, environment); this route or field is not part of it |
| tui: attachments | by design | 7 | 1 | attachments are read on send by path; SVG is not media; no remote server |
| tui: dialogs | by design | 6 | 3 | error, update and variant dialogs (variants are /variant and the model id) |
| tui: new designs experiment | by design | 6 | 3 | OpenCode's `opencode2` theme and its V1 migration exist only behind its `new-designs` experiment |
| cli: stats | by design | 4 | 1 | statistics are computed locally (always with tool counts), no server request |
| acp: separate server | by design | 4 | 3 | no separate server or plugin activation; no server defects to wrap; no shared event stream |
| util: titles | by design | 4 | 1 | CodeSmith keeps untitled sessions untitled until a title is generated |
| tui: terminal pane | by design | 3 | 1 | no terminal pane whose palette follows the theme; commands run through the shell tool |
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
| tui: slash commands | by design | 1 | 1 | commands complete only at the start of the prompt |
| tui: OpenTUI internals | n/a | 272 | 41 | OpenTUI cell offsets; Solid config and keymap contexts, command object identity and keymap modes, developer tools; Solid batching and renderer internals; layout, virtualization, reactive data or event-stream internals; renderer lifecycle, caching and server sync; select widget layout and reconciliation; widgets, animation and layout helpers |
| session-ui: web UI | n/a | 270 | 29 | web and desktop UI state machines |
| util: helpers | n/a | 101 | 16 | TypeScript helpers with no CodeSmith counterpart (or covered by Rust std/crates) |
| tui: prompt parts | n/a | 21 | 3 | OpenCode's prompt part model and media refs |
| cli: runtime internals | n/a | 16 | 7 | Bun/Node runtime, bundling or web UI |
| cli: cli.json | n/a | 7 | 1 | cli.json schema, migration and file locking |
| cli: packaging | n/a | 3 | 1 | single-executable asset archive |
| cli: migrations | n/a | 1 | 1 | credential import from older versions |
### Code Mode on Boa

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
