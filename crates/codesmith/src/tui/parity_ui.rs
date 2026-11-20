//! OpenCode's tests of the interactive UI's smaller features, ported:
//! reasoning titles (`tui/test/cli/tui/thinking`), and the groups that
//! follow it in `PARITY.md`. Each `oc_*` test names the OpenCode test it
//! ports; "(adapted)" ones say what differs.

use super::group::reasoning_summary;

// thinking.test.ts: "extracts a leading summary title and leaves markdown body"
#[test]
fn oc_extracts_a_leading_summary_title_and_leaves_the_markdown_body() {
    assert_eq!(
        reasoning_summary("**Continuing Quality Review**\n\nDetails.\n\n**Next section**\n\nMore."),
        (
            Some("Continuing Quality Review"),
            "Details.\n\n**Next section**\n\nMore."
        )
    );
}

// thinking.test.ts: "extracts a completed title before its streamed body arrives"
#[test]
fn oc_extracts_a_completed_title_before_its_streamed_body_arrives() {
    assert_eq!(
        reasoning_summary("**Continuing Quality Review**"),
        (Some("Continuing Quality Review"), "")
    );
}

// thinking.test.ts: "preserves markdown-significant indentation in the extracted body"
#[test]
fn oc_preserves_markdown_significant_indentation_in_the_extracted_body() {
    assert_eq!(
        reasoning_summary("**Continuing Quality Review**\n\n    const value = true\n"),
        (Some("Continuing Quality Review"), "    const value = true")
    );
}

// thinking.test.ts: "does not consume ordinary leading bold content"
#[test]
fn oc_does_not_consume_ordinary_leading_bold_content() {
    assert_eq!(
        reasoning_summary("**Important:** keep this in the body."),
        (None, "**Important:** keep this in the body.")
    );
}

// thinking.test.ts: "leaves content without a leading title in its body"
#[test]
fn oc_leaves_content_without_a_leading_title_in_its_body() {
    assert_eq!(reasoning_summary("Details only."), (None, "Details only."));
}

// The transcript links a mentioned session by its title (session-link.ts
// labels it; the current session and unknown ones stay plain IDs).
#[test]
fn mentioned_sessions_show_their_titles() {
    use super::app::{App, Entry};
    let mut a = App::new();
    a.header.session = "ses_19a4b2c3d4e000000".into();
    a.entries.push(Entry::Assistant(
        "Continued from ses_19a4b2c3d4e123abc; this is ses_19a4b2c3d4e000000.".into(),
    ));
    assert_eq!(
        a.unknown_mentions(),
        vec!["ses_19a4b2c3d4e123abc", "ses_19a4b2c3d4e000000"]
    );
    a.session_labels.insert(
        "ses_19a4b2c3d4e123abc".into(),
        Some("Fix\x1b the build".into()),
    );
    a.session_labels
        .insert("ses_19a4b2c3d4e000000".into(), Some("This one".into()));
    let text: String = super::view::transcript(&a, 120)
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("@Fix\\x1b the build (ses_19a4b2c3d4e123abc)"),
        "{text}"
    );
    assert!(text.contains("this is ses_19a4b2c3d4e000000."), "{text}");
}

/// The non-blank rows of terminal text, without its colors.
fn rows(text: Option<String>) -> Vec<String> {
    super::app::strip_escapes(&text.unwrap_or_default())
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

// presentation.test.ts: "prints nothing without a session or update"
#[test]
fn oc_prints_nothing_without_a_session_or_update() {
    assert_eq!(super::term::epilogue(None, "codesmith", None), None);
}

// presentation.test.ts: "formats session continuation summary"
#[test]
fn oc_formats_the_session_continuation_summary() {
    let text = super::term::epilogue(Some(("A session", "ses_123")), "codesmith", None);
    assert_eq!(
        rows(text),
        vec!["  Session   A session", "  Continue  codesmith -s ses_123"]
    );
}

// presentation.test.ts: "tells the user to start OpenCode again after an update"
#[test]
fn oc_tells_the_user_to_start_codesmith_again_after_an_update() {
    let text = super::term::epilogue(None, "codesmith", Some("2.0.0"));
    assert_eq!(rows(text), vec!["  Run codesmith to use 2.0.0"]);
}

// presentation.test.ts: "keeps the session continuation after an update"
#[test]
fn oc_keeps_the_session_continuation_after_an_update() {
    let text = super::term::epilogue(Some(("A session", "ses_123")), "codesmith", Some("2.0.0"));
    assert_eq!(
        rows(text),
        vec![
            "  Run codesmith to use 2.0.0",
            "  Session   A session",
            "  Continue  codesmith -s ses_123"
        ]
    );
}

#[test]
fn the_exit_summary_strips_control_characters_from_the_title() {
    let text =
        super::term::epilogue(Some(("Fix\x1b]0;x\x07\nit", "ses_1")), "codesmith", None).unwrap();
    assert!(
        !text.contains('\x07') && !text.contains("\x1b]"),
        "{text:?}"
    );
}

mod program_status {
    use super::super::term::{reports, Kind, Pending, PendingSession, ProgramStatus, Status};

    fn s(id: &str, title: Option<&str>, state: Pending) -> PendingSession {
        PendingSession {
            id: id.into(),
            title: title.map(str::to_string),
            state,
        }
    }

    fn blocked(kind: Kind, message: &str) -> Pending {
        Pending::Blocked {
            kind,
            message: Some(message.into()),
        }
    }

    // program-status.test.ts: "reports a blocked session on the root and its own record"
    #[test]
    fn oc_reports_a_blocked_session_on_the_root_and_its_own_record() {
        let mut p = ProgramStatus::default();
        assert_eq!(
            p.update(&[]),
            vec![Status {
                state: "idle",
                app: Some("codesmith"),
                ..Default::default()
            }]
        );
        let one = s(
            "ses_a",
            Some("Fix\nbuild"),
            blocked(Kind::Permission, "Allow bash?"),
        );
        let rec = |id: Option<&str>| Status {
            state: "blocked",
            kind: Some("permission"),
            id: id.map(str::to_string),
            app: Some("codesmith"),
            title: Some("Fix build".into()),
            message: Some("Allow bash?".into()),
        };
        assert_eq!(p.update(&[one]), vec![rec(None), rec(Some("ses_a"))]);
    }

    // program-status.test.ts: "summarizes several pending sessions and clears resolved ones"
    #[test]
    fn oc_summarizes_several_pending_sessions_and_clears_resolved_ones() {
        let mut p = ProgramStatus::default();
        let first = p.update(&[
            s("ses_a", None, blocked(Kind::Permission, "Allow bash?")),
            s("ses_b", None, blocked(Kind::Question, "Pick one")),
            s("ses_c", None, Pending::Working),
        ]);
        assert_eq!(
            first[0],
            Status {
                state: "blocked",
                app: Some("codesmith"),
                message: Some("2 sessions need input".into()),
                ..Default::default()
            }
        );
        let clear = |id: &str| Status {
            state: "clear",
            id: Some(id.into()),
            ..Default::default()
        };
        assert_eq!(
            p.update(&[s("ses_c", None, Pending::Working)]),
            vec![
                clear("ses_a"),
                clear("ses_b"),
                Status {
                    state: "working",
                    app: Some("codesmith"),
                    ..Default::default()
                }
            ]
        );
        assert_eq!(
            p.close(),
            Status {
                state: "clear",
                ..Default::default()
            }
        );
    }

    // program-status.test.ts: "keeps record IDs within the protocol's name rules"
    #[test]
    fn oc_keeps_record_ids_within_the_protocols_name_rules() {
        let ids = [
            "ses_a".to_string(),
            format!("ses_panel_{}", "f".repeat(36)),
            "ses with spaces".to_string(),
        ];
        let sessions: Vec<PendingSession> =
            ids.iter().map(|id| s(id, None, Pending::Working)).collect();
        let records = reports(&sessions);
        for (id, _) in &records[1..] {
            assert!(
                (1..=32).contains(&id.len())
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b)),
                "{id}"
            );
        }
        assert!(records.iter().any(|(id, _)| id == "ses_a"));
        assert_eq!(records.len(), 4);
    }

    // program-status.test.ts: "clips long titles to the protocol byte limit"
    #[test]
    fn oc_clips_long_titles_to_the_protocol_byte_limit() {
        let mut p = ProgramStatus::default();
        let reports = p.update(&[s("ses_a", Some(&"é".repeat(200)), Pending::Working)]);
        let title = reports[0].title.clone().unwrap();
        assert!(title.len() <= 192);
        assert!(title.ends_with("é…"), "{title}");
    }
}

// Jumping between prompts scrolls the one jumped to to the top, adding
// room below the last one.
#[test]
fn jumping_puts_the_prompt_at_the_top() {
    use super::app::{App, Entry};
    let mut a = App::new();
    for i in 0..3 {
        a.entries.push(Entry::User(format!("prompt {i}")));
        a.entries.push(Entry::Assistant(
            (0..20).map(|l| format!("line {l}\n\n")).collect(),
        ));
    }
    a.view_size.set((80, 10));
    let top = |a: &App| {
        let mut lines = super::view::transcript(a, 80);
        lines.extend(std::iter::repeat_n(Default::default(), a.nav_slack));
        let start = lines.len().saturating_sub(10 + a.scroll);
        lines[start..]
            .iter()
            .map(|l: &ratatui::text::Line| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
    };
    // From the bottom, back to the last prompt, then the one before.
    a.jump(false, true);
    assert!(top(&a).contains("prompt 2"), "{}", top(&a));
    // The last answer is short: blank room lets the prompt reach the top.
    a.entries.push(Entry::User("prompt 3".into()));
    a.entries.push(Entry::Assistant("short".into()));
    a.end_navigation_for_tests();
    a.jump(false, true);
    assert!(top(&a).contains("prompt 2"), "{}", top(&a));
    a.jump(true, true);
    assert!(top(&a).contains("prompt 3"), "{}", top(&a));
    assert!(a.nav_slack > 0);
    a.jump(false, true);
    a.jump(false, true);
    assert!(top(&a).contains("prompt 1"), "{}", top(&a));
    a.jump(true, true);
    a.jump(true, true);
    assert!(top(&a).contains("prompt 3"), "{}", top(&a));
    // Past the last: stays.
    a.jump(true, true);
    assert!(top(&a).contains("prompt 3"), "{}", top(&a));
}

// config-v2.test.tsx: "shows the TPS default in session settings"
// (adapted: `tui.session.tps`, on unless set false).
#[test]
fn oc_shows_the_tps_default_in_session_settings() {
    use super::settings;
    assert!(settings::Settings::default().tps);
    let (s, errors) = settings::load(&serde_json::json!({"session": {"tps": false}}));
    assert!(!s.tps && errors.is_empty(), "{errors:?}");
    let (_, errors) = settings::load(&serde_json::json!({"session": {"tps": "yes"}}));
    assert_eq!(errors, ["tui.session.tps: must be true or false"]);
}

// A turn ends with OpenCode's footer: agent, model, duration, throughput.
#[test]
fn a_turn_ends_with_its_footer() {
    use super::app::{App, Entry};
    use super::bridge::Ev;
    let mut a = App::new();
    a.tps = true;
    a.header.agent = "build".into();
    a.header.model = "fake/m".into();
    a.start_turn();
    a.on_event(Ev::Text("hi".into()));
    std::thread::sleep(std::time::Duration::from_millis(5));
    a.on_event(Ev::StepEnd(codesmith_ai::Usage {
        output: 50,
        ..Default::default()
    }));
    a.busy = false;
    a.end_turn(false);
    let Some(Entry::Footer(f)) = a.entries.last() else {
        panic!("{:?}", a.entries.last());
    };
    assert_eq!((f.agent.as_str(), f.model.as_str()), ("build", "fake/m"));
    assert!(f.tps.is_some_and(|t| t > 0.0), "{f:?}");
    let text: String = super::view::transcript(&a, 80)
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("Build · fake/m · "), "{text}");
    assert!(text.contains(" tok/s"), "{text}");
    // A shell command with no model step leaves none.
    let n = a.entries.len();
    a.start_turn();
    a.end_turn(false);
    assert_eq!(a.entries.len(), n);
}

// With `tui.debug.turn_tokens`, a turn's footer has its usage row, which
// flags a step that read fewer cached tokens than the turn before.
#[test]
fn turn_usage_rows_flag_likely_cache_busts() {
    use super::app::{App, Entry};
    use super::bridge::Ev;
    let mut a = App::new();
    a.turn_tokens = true;
    a.header.agent = "build".into();
    a.header.model = "anthropic/claude".into();
    for read in [10_000, 4_000] {
        a.start_turn();
        a.on_event(Ev::Text("hi".into()));
        a.on_event(Ev::StepEnd(codesmith_ai::Usage {
            input: 100,
            output: 20,
            cache_read: read,
            ..Default::default()
        }));
        a.busy = false;
        a.end_turn(false);
    }
    let footers: Vec<&super::app::Footer> = a
        .entries
        .iter()
        .filter_map(|e| match e {
            Entry::Footer(f) => Some(f),
            _ => None,
        })
        .collect();
    assert_eq!(footers[0].tokens[0].drop, None);
    assert_eq!(footers[1].tokens[0].drop, Some(6_000));
    let text: String = super::view::transcript(&a, 120)
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("+ Tokens: 1 step · latest: 120 new · 4,000 cached · 4,120 total · ! 1 likely cache bust"),
        "{text}"
    );
}

mod history {
    use super::super::app::App;
    use super::super::history::{self, Store};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn send(a: &mut App, s: &str) {
        a.input.set(s);
        a.on_key(key(KeyCode::Enter));
    }

    fn up(a: &mut App) -> String {
        a.on_key(key(KeyCode::Up));
        a.input.text().to_string()
    }

    fn down(a: &mut App) -> String {
        a.on_key(key(KeyCode::Down));
        a.input.text().to_string()
    }

    /// Switch from `shown` to `other`, as the tabs do.
    fn switch(shown: &mut App, other: &mut App) {
        shown.hand_over(other);
        std::mem::swap(shown, other);
    }

    // history-provider.test.tsx: "shares prompt history across sessions and
    // the home composer" (adapted: tabs hand the history to the shown one)
    #[test]
    fn oc_shares_prompt_history_across_sessions() {
        let (mut a, mut b) = (App::new(), App::new());
        send(&mut a, "a-one");
        switch(&mut a, &mut b);
        send(&mut a, "b-one");
        switch(&mut a, &mut b);
        send(&mut a, "a-two");
        assert_eq!(up(&mut a), "a-two");
        assert_eq!(up(&mut a), "b-one");
        assert_eq!(up(&mut a), "a-one");
    }

    // history-provider.test.tsx: "keeps legacy unscoped history on the home
    // composer" (adapted: the history file as it loads)
    #[test]
    fn oc_keeps_legacy_unscoped_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt-history.jsonl");
        std::fs::write(
            &path,
            "{\"text\":\"legacy\",\"files\":[],\"agents\":[],\"pasted\":[]}\n",
        )
        .unwrap();
        let store = Store::load(path);
        let mut a = App::new();
        a.history = store.entries.iter().map(history::text).collect();
        assert_eq!(up(&mut a), "legacy");
        assert_eq!(down(&mut a), "");
    }

    // history-provider.test.tsx: "reset clears browsing position and live
    // draft, and move returns a deep copy" (adapted: editing a recalled
    // prompt leaves the history, which keeps the entry as it was)
    #[test]
    fn oc_reset_clears_the_browsing_position_and_draft() {
        let mut a = App::new();
        send(&mut a, "[Pasted ~3 lines] ");
        a.input.set("unsent draft");
        assert_eq!(up(&mut a), "[Pasted ~3 lines]");
        a.input.insert('!');
        a.on_key(key(KeyCode::Char('x')));
        // Down moves within the edited text: no draft comes back.
        assert_eq!(down(&mut a), "[Pasted ~3 lines]!x");
        a.input.set("fresh draft");
        assert_eq!(up(&mut a), "[Pasted ~3 lines]");
        assert_eq!(down(&mut a), "fresh draft");
    }

    // draft-stash.test.ts: "tab-keyed drafts stay on the tab they were
    // written in" (adapted: each tab's app keeps its prompt)
    #[test]
    fn oc_tab_keyed_drafts_stay_on_their_tab() {
        let (mut one, mut two) = (App::new(), App::new());
        two.input.set("notes for session two");
        switch(&mut two, &mut one);
        assert_eq!(two.input.text(), "");
        switch(&mut two, &mut one);
        assert_eq!(two.input.text(), "notes for session two");
    }

    // draft-stash.test.ts: "each tab keeps its own draft, including home"
    #[test]
    fn oc_each_tab_keeps_its_own_draft() {
        let (mut one, mut home) = (App::new(), App::new());
        one.input.set("DRAFT-ONE");
        home.input.set("draft on home");
        switch(&mut one, &mut home);
        assert_eq!(one.input.text(), "draft on home");
        switch(&mut one, &mut home);
        assert_eq!(one.input.text(), "DRAFT-ONE");
    }

    // draft-stash.test.ts: "a newer draft for the same slot replaces the older one"
    #[test]
    fn oc_a_newer_draft_replaces_the_older_one() {
        let (mut a, mut b) = (App::new(), App::new());
        a.input.set("first");
        switch(&mut a, &mut b);
        switch(&mut a, &mut b);
        a.input.set("second");
        switch(&mut a, &mut b);
        switch(&mut a, &mut b);
        assert_eq!(a.input.text(), "second");
    }

    // A recalled prompt brings back the files it attached.
    #[test]
    fn a_recalled_prompt_attaches_its_files_again() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        std::fs::write(&png, b"x").unwrap();
        let mut a = App::new();
        a.attachments.push(super::super::attach::Pending {
            label: "[Image 1]".into(),
            path: png.clone(),
        });
        send(&mut a, "describe [Image 1]");
        let kept = a.history_new.pop().unwrap();
        assert_eq!(history::files(&kept)[0].path, png);
        a.attachments.clear();
        assert_eq!(up(&mut a), "describe [Image 1]");
        assert_eq!(a.attachments.len(), 1);
    }
}

// ctrl+f in the model picker asks to toggle the highlighted model's
// favourite; elsewhere it does nothing.
#[test]
fn ctrl_f_in_the_model_picker_toggles_a_favorite() {
    use super::app::{Action, App, Modal, Pick, PickItem, Picker};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let item = |id: &str| PickItem {
        id: id.into(),
        label: id.into(),
        detail: String::new(),
        current: false,
    };
    let mut a = App::new();
    let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
    a.modal = Some(Modal::Picker(Picker::new(
        Pick::Model,
        "Models",
        vec![item("openai/gpt-5"), item("anthropic/claude")],
    )));
    assert_eq!(
        a.on_key(ctrl_f),
        Some(Action::Favorite("openai/gpt-5".into()))
    );
    assert!(a.modal.is_some());
    a.modal = Some(Modal::Picker(Picker::new(
        Pick::Agent,
        "Agents",
        vec![item("build")],
    )));
    assert_eq!(a.on_key(ctrl_f), None);
    assert!(a.modal.is_some());
}

/// Open the palette with ctrl+p, type `filter`, and press Enter.
fn palette(a: &mut super::app::App, filter: &str) -> Option<super::app::Action> {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    a.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    for c in filter.chars() {
        a.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    a.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
}

// copy-session-id.test.tsx: "Copy session ID from Ctrl+P (%s)" (3 cases;
// adapted: `OSC 52` reports no failure, so success and failure are one
// case, and a UI with no session yet stands for home)
#[test]
fn oc_copy_session_id_from_ctrl_p() {
    use super::app::{App, Modal};
    let mut a = App::new();
    a.header.session = "ses_copy_id".into();
    assert_eq!(palette(&mut a, "Copy session ID"), None);
    assert!(a.modal.is_none(), "the palette closes");
    assert_eq!(a.clipboard.as_deref(), Some("ses_copy_id"));
    assert!(
        matches!(a.entries.last(), Some(super::app::Entry::Notice(n)) if n == "Session ID copied to clipboard!")
    );
    assert_eq!(
        super::term::copy("ses_copy_id"),
        "\x1b]52;c;c2VzX2NvcHlfaWQ=\x07"
    );
    // Home: nothing to copy, so the palette does not offer it.
    let mut home = App::new();
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    home.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    let Some(Modal::Picker(p)) = &home.modal else {
        panic!("no palette");
    };
    assert!(!p.items.iter().any(|i| i.id == "session.copy.id"));
    assert!(p.items.iter().any(|i| i.id == "model.list"));
}

// The palette runs UI commands and slash commands.
#[test]
fn the_palette_runs_commands() {
    use super::app::{Action, App};
    let mut a = App::new();
    assert_eq!(
        palette(&mut a, "List available models"),
        Some(Action::Slash {
            name: "model".into(),
            args: String::new()
        })
    );
    assert_eq!(
        palette(&mut a, "/diff"),
        Some(Action::Slash {
            name: "diff".into(),
            args: String::new()
        })
    );
}

mod footer {
    use super::super::view::{prompt_footer, status_text, Footer};

    fn keys(id: &str) -> String {
        match id {
            "session.child.first" => "ctrl+j".into(),
            "command.palette.show" => "ctrl+p".into(),
            "agent.cycle" => "shift+tab".into(),
            _ => String::new(),
        }
    }

    // prompt-footer.test.tsx: "prompt footer separates simultaneous
    // subagent, shell, and usage status" (adapted: no clicks)
    #[test]
    fn oc_prompt_footer_separates_subagent_shell_and_usage_status() {
        let mut f = Footer {
            subagents: 1,
            shells: 1,
            usage: vec!["$1.00".into()],
            session: true,
            show_details: true,
        };
        let (left, right) = prompt_footer(&f, &keys);
        assert_eq!(left, "ctrl+j 1 subagent · 1 shell · $1.00");
        assert_eq!(right, "ctrl+p commands");
        f.subagents = 0;
        assert_eq!(prompt_footer(&f, &keys).0, "ctrl+j 1 shell · $1.00");
    }

    // prompt-footer.test.tsx: "prompt footer can hide details"
    #[test]
    fn oc_prompt_footer_can_hide_details() {
        let mut f = Footer {
            subagents: 0,
            shells: 0,
            usage: vec!["1K (10%)".into(), "$1.00".into()],
            session: true,
            show_details: true,
        };
        let (left, right) = prompt_footer(&f, &keys);
        assert_eq!(
            (left.as_str(), right.as_str()),
            ("1K (10%) · $1.00", "ctrl+p commands")
        );
        f.show_details = false;
        assert_eq!(prompt_footer(&f, &keys), (String::new(), String::new()));
        f.session = false;
        f.show_details = true;
        assert_eq!(prompt_footer(&f, &keys), (String::new(), String::new()));
    }

    // sidebar-context.test.tsx: "sidebar omits context before usage is
    // available" and "sidebar shows available context usage" (adapted: the
    // status line, CodeSmith's place for the context)
    #[test]
    fn oc_the_context_shows_once_usage_is_available() {
        let mut a = super::super::app::App::new();
        assert!(
            !status_text(&a, 0).contains("tokens ·"),
            "{}",
            status_text(&a, 0)
        );
        a.context = 1234;
        assert!(
            status_text(&a, 0).contains("1.2K tokens"),
            "{}",
            status_text(&a, 0)
        );
    }
}
