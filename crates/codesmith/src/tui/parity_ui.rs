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
