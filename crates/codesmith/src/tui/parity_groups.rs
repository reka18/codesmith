//! OpenCode's grouped transcript and verbosity tests, ported:
//! `tui/test/cli/tui/session-verbosity`, `session-grouping`, `session-rows`,
//! `group-view`, `tui/test/session-activity-anchor`,
//! `session-backfill-disclosure`, `session-mount-budget` and the grouping
//! settings of `tui/test/config-v2` (the grouping engine's own tests are in
//! `group.rs`). Each `oc_*` test names the OpenCode test it ports;
//! "(adapted)" ones say what differs, and a dropped half is named. OpenCode
//! opens one group with a click; CodeSmith has no mouse, and the tool output
//! key (Ctrl+O) opens every group.

use super::app::{App, Entry, Tool};
use super::bridge::Ev;
use super::group::{GroupKind, Grouping, Node, Verbosity};
use super::settings;
use super::view;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use serde_json::json;

fn app(v: Verbosity) -> App {
    let mut a = App::new();
    a.verbosity = v;
    a
}

fn done(name: &str, detail: &str, output: &str) -> Entry {
    Entry::Tool(Tool {
        id: format!("{name}-{detail}"),
        name: name.into(),
        detail: detail.into(),
        result: Some((output.lines().next().unwrap_or("").into(), false)),
        output: output.into(),
        ..Default::default()
    })
}

fn failed(name: &str, detail: &str, output: &str) -> Entry {
    let mut e = done(name, detail, output);
    if let Entry::Tool(t) = &mut e {
        t.result = Some((output.into(), true));
    }
    e
}

fn running(name: &str, detail: &str) -> Entry {
    Entry::Tool(Tool {
        id: format!("{name}-{detail}"),
        name: name.into(),
        detail: detail.into(),
        ..Default::default()
    })
}

fn lines(a: &App) -> Vec<String> {
    view::transcript(a, 80)
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn has(a: &App, s: &str) -> bool {
    lines(a).iter().any(|l| l.contains(s))
}

/// Ctrl+O: the tool output key.
fn ctrl_o(a: &mut App) {
    a.on_key(KeyEvent {
        code: KeyCode::Char('o'),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
}

fn kind(k: GroupKind) -> &'static str {
    match k {
        GroupKind::Activity => "activity",
        GroupKind::Reasoning => "reasoning",
        GroupKind::Exploration => "exploration",
        GroupKind::Instructions => "instructions",
    }
}

fn name(a: &App, n: &Node<usize, GroupKind>) -> String {
    match n {
        Node::Group { kind: k, size, .. } => format!("{}:{size}", kind(*k)),
        Node::Entry(i) => match &a.entries[*i] {
            Entry::User(_) => "user".into(),
            Entry::Assistant(_) => "text".into(),
            Entry::Reasoning(_) => "thought".into(),
            Entry::Tool(t) => t.name.clone(),
            _ => "other".into(),
        },
    }
}

/// The transcript's rows: `activity:6`, `user`, `shell`, …
fn shape(a: &App) -> Vec<String> {
    view::rows(a).iter().map(|n| name(a, n)).collect()
}

fn children(a: &App, n: &Node<usize, GroupKind>) -> Vec<String> {
    match n {
        Node::Group { children, .. } => children.iter().map(|c| name(a, c)).collect(),
        Node::Entry(_) => vec![],
    }
}

// ----- tui/test/cli/tui/session-verbosity -----------------------------------

fn instructions(paths: &[&str]) -> Entry {
    Entry::Instructions(paths.iter().map(std::path::PathBuf::from).collect())
}

// (adapted) No footer row; the run is a tool group, worded by what its
// calls did (CodeSmith's tool groups).
#[test]
fn oc_low_hydration_nests_subgroups_inside_one_activity_group_per_run() {
    let mut a = app(Verbosity::Low);
    a.entries = vec![
        Entry::User("Go".into()),
        Entry::Reasoning("Plan".into()),
        done("read", "a.rs", "Read a.rs"),
        done("grep", "x", "1 match"),
        instructions(&["AGENTS.md"]),
        done("shell", "ls", "a.rs"),
        done("read", "b.rs", "Read b.rs"),
        Entry::Assistant("Done".into()),
    ];
    assert_eq!(shape(&a), ["user", "activity:6", "text"]);
    let rows = view::rows(&a);
    assert_eq!(
        children(&a, &rows[1]),
        [
            "reasoning:1",
            "exploration:2",
            "instructions:1",
            "shell",
            "exploration:1"
        ]
    );
    assert!(
        has(&a, "⠶ Read 2 files, ran a search, ran a command ›"),
        "{:?}",
        lines(&a)
    );
}

#[test]
fn oc_medium_hydration_keeps_production_shape_plus_instruction_groups() {
    let mut a = app(Verbosity::Medium);
    a.entries = vec![
        done("read", "a.rs", "Read a.rs"),
        done("webfetch", "https://x", "Fetched"),
        instructions(&["AGENTS.md"]),
        instructions(&["packages/tui/AGENTS.md"]),
        done("shell", "ls", "a.rs"),
    ];
    assert_eq!(shape(&a), ["exploration:2", "instructions:2", "shell"]);
}

// (adapted) Through the session's events, worded as a tool group.
#[test]
fn oc_live_appends_under_low_merge_into_the_open_activity_group() {
    let mut a = app(Verbosity::Low);
    a.busy = true;
    a.on_event(Ev::Reasoning("Thinking it over".into()));
    for (id, name) in [("r1", "read"), ("sh", "shell"), ("r2", "read")] {
        a.on_event(Ev::ToolStart {
            id: id.into(),
            name: name.into(),
            detail: id.into(),
            diff: None,
        });
        a.on_event(Ev::ToolEnd {
            id: id.into(),
            name: name.into(),
            output: "ok".into(),
            error: false,
        });
    }
    assert_eq!(shape(&a), ["activity:4"]);
    assert!(has(&a, "Read 2 files, ran a command ›"), "{:?}", lines(&a));
    a.on_event(Ev::Text("Done".into()));
    assert_eq!(shape(&a), ["activity:4", "text"]);
}

// (adapted) A call waiting for permission is still running: the group
// reads in the present tense with the command's glyph, the call keeps its
// place, and its command output stays in view below the group's line.
#[test]
fn oc_permission_blocked_tools_inside_an_activity_group_are_tracked_as_pending_without_reordering()
{
    let mut a = app(Verbosity::Low);
    a.busy = true;
    let mut sh = running("shell", "cargo build");
    if let Entry::Tool(t) = &mut sh {
        t.live = "Compiling a\n".into();
    }
    a.entries = vec![
        done("read", "a.rs", "Read"),
        sh,
        done("read", "b.rs", "Read"),
    ];
    let rows = view::rows(&a);
    assert_eq!(
        children(&a, &rows[0]),
        ["exploration:1", "shell", "exploration:1"]
    );
    let l = lines(&a);
    assert_eq!(l[0], "▖ Reading 2 files, running a command… ›");
    assert!(l.iter().any(|l| l == "▖ shell cargo build …"), "{l:?}");
    assert!(l.iter().any(|l| l.contains("│ Compiling a")), "{l:?}");
}

#[test]
fn oc_questions_stand_alone_at_every_level_and_end_an_activity_run() {
    let mut a = app(Verbosity::Low);
    a.entries = vec![
        done("read", "a.rs", "Read"),
        running("question", "Which?"),
        done("read", "b.rs", "Read"),
    ];
    assert_eq!(shape(&a), ["activity:1", "question", "activity:1"]);
}

// (adapted) A tool group names every call while it runs, with the glyph
// of the latest.
#[test]
fn oc_until_something_finishes_the_label_is_the_first_running_items_status() {
    let mut a = app(Verbosity::Low);
    a.busy = true;
    a.entries = vec![running("edit", "a.rs"), running("shell", "ls")];
    assert_eq!(lines(&a), ["▖ Editing a file, running a command… ›"]);
}

// ----- tui/test/cli/tui/session-grouping ------------------------------------

// (adapted) CodeSmith's rows are entries, not ids: a call still running
// does not hide a later one with its call id. Worded as a tool group.
#[test]
fn oc_a_pending_tool_does_not_hide_a_later_tool_reusing_its_call_id_in_another_message() {
    let mut a = app(Verbosity::Low);
    a.busy = true;
    let mut later = done("read", "b.rs", "Read");
    if let Entry::Tool(t) = &mut later {
        t.id = "call-reused".into();
    }
    let mut blocked = running("read", "a.rs");
    if let Entry::Tool(t) = &mut blocked {
        t.id = "call-reused".into();
    }
    a.entries = vec![blocked, later];
    assert_eq!(lines(&a), ["⡀ Reading 2 files… ›"]);
    a.on_event(Ev::ToolEnd {
        id: "call-reused".into(),
        name: "read".into(),
        output: "Read".into(),
        error: false,
    });
    assert_eq!(lines(&a), ["⠶ Read 2 files ›"]);
}

/// "execute counts its finished nested calls instead of itself", drawn:
/// the call's progress rows reach its row, and the tool group's line
/// words its nested calls in place of it.
#[test]
fn oc_execute_counts_its_finished_nested_calls_in_the_activity_row() {
    let mut a = app(Verbosity::Low);
    a.busy = true;
    a.on_event(Ev::ToolStart {
        id: "x".into(),
        name: "execute".into(),
        detail: String::new(),
        diff: None,
    });
    let sprite = super::tool_group::Glyph::of("execute");
    assert_eq!(
        lines(&a),
        [format!("{} Calling execute… ›", sprite.frames[0])]
    );
    let calls = |rows: serde_json::Value| Ev::ToolCalls {
        id: "x".into(),
        calls: super::group::nested_calls(&json!({ "toolCalls": rows })).unwrap(),
    };
    a.on_event(calls(json!([
        {"tool": "read", "status": "completed"},
        {"tool": "custom.slow", "status": "running"},
    ])));
    assert_eq!(
        lines(&a),
        [format!(
            "{} Reading a file, calling custom.slow… ›",
            sprite.frames[0]
        )]
    );
    a.on_event(calls(json!([
        {"tool": "read", "status": "completed"},
        {"tool": "read", "status": "completed"},
        {"tool": "custom.lookup", "status": "error"},
        {"tool": "custom.slow", "status": "running"},
    ])));
    a.on_event(Ev::ToolEnd {
        id: "x".into(),
        name: "execute".into(),
        output: "done".into(),
        error: false,
    });
    assert_eq!(
        lines(&a),
        [format!(
            "{} Read 2 files, called custom.lookup, called custom.slow ›",
            sprite.rest
        )]
    );
}

// ----- tui/test/cli/tui/session-rows ----------------------------------------

// (adapted) At medium, where exploration groups are top-level rows.
#[test]
fn oc_groups_exploration_parts_across_assistant_messages_until_a_delimiter() {
    let mut a = app(Verbosity::Medium);
    a.entries = vec![
        Entry::User("Explore".into()),
        Entry::Assistant("Looking".into()),
        running("read", "a"),
        running("glob", "*"),
        running("grep", "x"),
        Entry::Assistant("Done".into()),
    ];
    assert_eq!(shape(&a), ["user", "text", "exploration:3", "text"]);
    assert!(has(&a, "→ Explored: 1 read, 2 searches"), "{:?}", lines(&a));
}

#[test]
fn oc_keeps_non_exploration_tools_as_individual_part_rows() {
    let mut a = app(Verbosity::Medium);
    a.busy = true;
    a.entries = vec![
        running("read", "a"),
        running("bash", "ls"),
        running("grep", "x"),
    ];
    assert_eq!(shape(&a), ["exploration:1", "bash", "exploration:1"]);
    let l = lines(&a);
    assert_eq!(l[0], "→ Explored: 1 read", "a later row completes it");
    assert_eq!(l.last().unwrap(), "✱ Exploring: 1 search");
}

#[test]
fn oc_groups_adjacent_reasoning_parts_until_a_visible_boundary() {
    let mut a = app(Verbosity::Medium);
    a.busy = true;
    a.entries = vec![
        Entry::Reasoning("First".into()),
        Entry::Reasoning("Second".into()),
        Entry::Assistant("Visible".into()),
        Entry::Reasoning("Third".into()),
    ];
    assert_eq!(shape(&a), ["reasoning:2", "text", "reasoning:1"]);
    let l = lines(&a);
    assert_eq!(l[0], "+ Thought · 2 steps");
    assert_eq!(l.last().unwrap(), "+ Thinking", "still streaming");
}

#[test]
fn oc_groups_across_empty_assistant_reasoning_parts() {
    let mut a = app(Verbosity::Medium);
    a.entries = vec![
        Entry::Reasoning("Looking".into()),
        running("read", "a"),
        Entry::Reasoning("".into()),
        running("grep", "x"),
    ];
    assert_eq!(shape(&a), ["reasoning:1", "exploration:2"]);
}

// (adapted) The turn ending completes the last group, as OpenCode's
// assistant footer row does.
#[test]
fn oc_completes_exploration_groups_when_another_row_follows() {
    let mut a = app(Verbosity::Medium);
    a.busy = true;
    a.entries = vec![
        running("read", "a"),
        Entry::User("Continue".into()),
        running("grep", "x"),
    ];
    let l = lines(&a);
    assert_eq!(l[0], "→ Explored: 1 read");
    assert_eq!(l.last().unwrap(), "✱ Exploring: 1 search");
    a.busy = false;
    assert_eq!(lines(&a).last().unwrap(), "→ Explored: 1 search");
}

// ----- tui/test/cli/tui/group-view ------------------------------------------

// (adapted) Ctrl+O opens the exploration group and the thoughts at once;
// the anchor registrations are OpenTUI's.
#[test]
fn oc_retains_nested_expansion_state_and_registers_exact_headers_and_parts() {
    let mut a = app(Verbosity::Medium);
    a.entries = vec![done("read", "a", "Target A"), done("read", "b", "Target B")];
    assert_eq!(lines(&a), ["→ Explored: 2 reads"]);
    ctrl_o(&mut a);
    let l = lines(&a);
    assert_eq!(l[0], "→ Explored: 2 reads");
    let (ra, rb) = (
        l.iter().position(|l| l == "⚙ read a").unwrap(),
        l.iter().position(|l| l == "⚙ read b").unwrap(),
    );
    assert!(rb > ra, "{l:?}");
    ctrl_o(&mut a);
    assert!(!has(&a, "Target B"));
    a.entries = vec![
        Entry::Reasoning("**Reset title**\n\nReset thought body".into()),
        Entry::Reasoning("**Reset title**\n\nReset thought body".into()),
    ];
    assert_eq!(lines(&a), ["+ Thought: Reset title · 2 steps"]);
    ctrl_o(&mut a);
    assert_eq!(lines(&a)[0], "- Thought · 2 steps");
    assert!(has(&a, "Reset thought body"));
}

// (adapted) Worded as a tool group.
#[test]
fn oc_a_low_activity_group_with_nothing_finished_stays_collapsed_behind_a_status() {
    let mut a = app(Verbosity::Low);
    a.busy = true;
    a.entries = vec![running("shell", "one"), running("shell", "two")];
    assert_eq!(lines(&a), ["▖ Running 2 commands… ›"]);
}

// (adapted) A tool group's chevron is its disclosure icon, and its line
// says how many calls failed.
#[test]
fn oc_failed_low_activity_uses_a_disclosure_icon_and_keeps_details_expandable() {
    let mut a = app(Verbosity::Low);
    a.entries = vec![failed("shell", "make", "command failed")];
    assert_eq!(lines(&a), ["▞ Ran a command · 1 failed ›"]);
    ctrl_o(&mut a);
    let l = lines(&a);
    assert_eq!(l[0], "▞ Ran a command · 1 failed ⌄");
    assert_eq!(l[1], "✗ shell make");
    assert!(has(&a, "command failed"), "{l:?}");
}

// (adapted) Ctrl+O opens the group (no click); a load's rows say "↳
// Loaded", relative to the session's folder.
#[test]
fn oc_expanded_instructions_stay_adjacent_to_their_summary_and_each_other() {
    let mut a = app(Verbosity::Medium);
    a.cwd = "/p".into();
    a.entries = vec![
        instructions(&["/p/AGENTS.md"]),
        instructions(&["/p/packages/tui/AGENTS.md"]),
    ];
    assert_eq!(lines(&a), ["◈ Instructions: 2 files"]);
    assert!(!has(&a, "Loaded AGENTS.md"));
    ctrl_o(&mut a);
    assert_eq!(
        lines(&a),
        [
            "◈ Instructions: 2 files",
            "  ↳ Loaded AGENTS.md",
            "  ↳ Loaded packages/tui/AGENTS.md"
        ]
    );
    // High opens it by default; a file loaded twice counts once.
    let mut a = app(Verbosity::High);
    a.entries = vec![
        instructions(&["AGENTS.md"]),
        instructions(&["AGENTS.md", "src/AGENTS.md"]),
    ];
    assert_eq!(lines(&a)[0], "◈ Instructions: 2 files");
    assert_eq!(lines(&a).len(), 4);
}

/// Instruction loads come from the session as it runs, and from the
/// stored history when it is continued; a path's control characters are
/// shown as escapes.
#[test]
fn instruction_loads_reach_the_transcript_live_and_replayed() {
    use codesmith_ai::Message;
    let mut a = app(Verbosity::Medium);
    a.on_event(Ev::Instructions(vec!["/p/sub/AGENTS.md".into()]));
    assert_eq!(lines(&a), ["◈ Instructions: 1 file"]);
    a.entries = super::app::replay(&[Message::user(
        "Instructions from: /p/sub/AGENTS.md\nbe nice\n\nInstructions from: /p/AGENTS.md\nok",
    )]);
    assert_eq!(lines(&a), ["◈ Instructions: 2 files"]);
    a.verbosity = Verbosity::High;
    a.entries = vec![instructions(&["/p/a\u{1b}[2J/AGENTS.md"])];
    assert!(has(&a, "Loaded /p/a\\x1b[2J/AGENTS.md"), "{:?}", lines(&a));
}

// ----- tui/test/session-activity-anchor --------------------------------------

// (adapted) Following the bottom, the opened details end the transcript
// and the tool group's line moves up above them.
#[test]
fn oc_a_low_activity_summary_that_ends_the_transcript_opens_like_v2_at_the_bottom() {
    let mut a = app(Verbosity::Low);
    a.entries = vec![
        Entry::User("Running prompt".into()),
        done("shell", "echo step", "running step 0 output"),
    ];
    let before = view::transcript_tail(&a, 80, 5);
    let at = |l: &[ratatui::text::Line], s: &str| {
        l.iter().position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .contains(s)
        })
    };
    assert_eq!(at(&before, "Ran a command ›"), Some(3));
    ctrl_o(&mut a);
    let after = view::transcript_tail(&a, 80, 5);
    let row = at(&after, "Ran a command ⌄").unwrap();
    assert!(row < 3);
    assert!(at(&after[row..], "running step 0 output").is_some());
    ctrl_o(&mut a);
    assert_eq!(
        at(&view::transcript_tail(&a, 80, 5), "Ran a command ›"),
        Some(3)
    );
}

// ----- tui/test/session-backfill-disclosure ----------------------------------

// (adapted) A thought group of a stored session, replayed, opens with
// Ctrl+O.
#[test]
fn oc_clicking_a_backfilled_thought_group_toggles_it() {
    use codesmith_ai::{Message, Part, Role};
    let mut a = app(Verbosity::Medium);
    a.entries = super::app::replay(&[
        Message::user("Think"),
        Message {
            role: Role::Assistant,
            parts: vec![
                Part::Reasoning("Backfilled thought".into()),
                Part::Text("Answer".into()),
            ],
        },
    ]);
    assert!(has(&a, "+ Thought") && !has(&a, "Backfilled thought"));
    ctrl_o(&mut a);
    assert!(has(&a, "- Thought") && has(&a, "Backfilled thought"));
    ctrl_o(&mut a);
    assert!(!has(&a, "Backfilled thought"));
}

// ----- tui/test/session-mount-budget -----------------------------------------

// (adapted) Only the rows reaching into view are laid out: an open group
// at the end fills the view in place of the older rows.
#[test]
fn oc_an_expanded_group_spends_the_mounting_budget_that_older_rows_used() {
    let mut a = app(Verbosity::Low);
    for i in 0..20 {
        a.entries.push(Entry::User(format!("older prompt {i}")));
    }
    let output: String = (0..30).map(|i| format!("line {i}\n")).collect();
    a.entries.push(done("shell", "seq 30", &output));
    let text = |a: &App| -> String {
        view::transcript_tail(a, 80, 10)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect()
    };
    assert!(text(&a).contains("older prompt 19"));
    ctrl_o(&mut a);
    let t = text(&a);
    assert!(!t.contains("older prompt"), "{t}");
    assert!(t.contains("line 29"), "{t}");
}

// ----- tui/test/config-v2 -----------------------------------------------------

// (adapted) CodeSmith has no settings dialog: `session.verbosity` and
// `session.grouping` are read from `tui`, with OpenCode's values and
// defaults, and a wrong value is reported.
#[test]
fn oc_shows_transcript_verbosity_in_session_settings() {
    let (s, e) = settings::load(&serde_json::Value::Null);
    assert!(e.is_empty());
    assert_eq!(s.verbosity, Verbosity::Low);
    for v in Verbosity::ALL {
        let (s, e) = settings::load(&json!({"session": {"verbosity": v.key()}}));
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(s.verbosity, v);
    }
    let (s, e) = settings::load(&json!({"session": {"verbosity": "loud"}}));
    assert_eq!(s.verbosity, Verbosity::Low);
    assert_eq!(
        e,
        ["tui.session.verbosity: must be \"low\", \"medium\" or \"high\""]
    );
}

#[test]
fn oc_names_tool_grouping_explicitly_in_settings() {
    let (s, _) = settings::load(&serde_json::Value::Null);
    assert_eq!(s.grouping, Grouping::Auto);
    let (s, e) = settings::load(&json!({"session": {"grouping": "none"}}));
    assert!(e.is_empty());
    assert_eq!(s.grouping, Grouping::None);
    let (_, e) = settings::load(&json!({"session": {"grouping": "off"}}));
    assert_eq!(e, ["tui.session.grouping: must be \"auto\" or \"none\""]);
}

// ----- CodeSmith's own -----------------------------------------------------------

// Without grouping, reads and searches are drawn one by one; the
// verbosity and grouping commands switch them in place.
#[test]
fn grouping_and_verbosity_commands_switch_the_transcript() {
    let mut a = app(Verbosity::Medium);
    a.entries = vec![done("read", "a.rs", "Read a.rs"), done("grep", "x", "1")];
    assert_eq!(lines(&a)[0], "→ Explored: 1 read, 1 search");
    a.grouping = Grouping::None;
    assert_eq!(lines(&a)[0], "⚙ read a.rs");
    a.grouping = Grouping::Auto;
    a.verbosity = Verbosity::High;
    let l = lines(&a);
    assert_eq!(l[0], "→ Explored: 1 read, 1 search");
    assert!(l.contains(&"⚙ read a.rs".to_string()), "high opens it");
    a.verbosity = Verbosity::Low;
    // At low they are one tool group.
    assert_eq!(lines(&a), ["⠶ Read a file, ran a search ›"]);
}

// A subagent's row stays in view below its collapsed group while it
// runs, then folds into the group's line.
#[test]
fn a_running_subagent_stays_in_view_below_its_summary() {
    let mut a = app(Verbosity::Low);
    a.busy = true;
    a.entries.push(done("read", "a.rs", "Read"));
    a.on_event(Ev::ToolStart {
        id: "task".into(),
        name: "subagent".into(),
        detail: "explore: survey".into(),
        diff: None,
    });
    a.on_event(Ev::ChildStart {
        id: "ses_child".into(),
        title: "Survey".into(),
        agent: "explore".into(),
    });
    let l = lines(&a);
    assert_eq!(l[0], "⣾ Reading a file, running a subagent… ›");
    assert!(
        l.iter().any(|l| l.contains("⣾ Explore Subagent — Survey")),
        "{l:?}"
    );
    a.on_event(Ev::ChildEnd {
        id: "ses_child".into(),
        status: "completed".into(),
        error: None,
    });
    a.on_event(Ev::ToolEnd {
        id: "task".into(),
        name: "subagent".into(),
        output: "done".into(),
        error: false,
    });
    assert_eq!(lines(&a), ["⣿ Read a file, ran a subagent ›"]);
}

// The verbosity and grouping commands have no keys by default, as in
// OpenCode; bound, they switch the transcript.
#[test]
fn the_verbosity_and_grouping_commands_can_be_bound() {
    let tui = json!({"keybinds": {
        "session.verbosity.cycle": "ctrl+alt+v",
        "session.toggle.exploration_grouping": "ctrl+alt+g",
    }});
    let (s, e) = settings::load(&tui);
    assert!(e.is_empty(), "{e:?}");
    let k = super::keymap::Keymap::new(&s.keybinds, s.leader_timeout, true);
    assert!(k.warnings.is_empty(), "{:?}", k.warnings);
    let mut a = App::new();
    a.keymap = std::sync::Arc::new(k);
    let press = |a: &mut App, c: char| {
        a.on_key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
    };
    press(&mut a, 'v');
    assert_eq!(a.verbosity, Verbosity::Medium);
    press(&mut a, 'v');
    press(&mut a, 'v');
    assert_eq!(a.verbosity, Verbosity::Low);
    press(&mut a, 'g');
    assert_eq!(a.grouping, Grouping::None);
    assert!(a.input.is_empty());
    let unbound = super::keymap::Keymap::new(&Default::default(), 2000, true);
    assert!(unbound.list("session.verbosity.cycle").is_empty());
}
