//! Mini mode end to end on a virtual terminal: the bytes it writes are
//! fed to `vt100`, and the screen and its scrollback are checked. The
//! `oc_*` tests port OpenCode's `mini` tests (adapted where CodeSmith's
//! rows read differently); the others cover CodeSmith's own behaviour.

use super::*;
use crate::tui::app::{Entry, Modal, Sub, Tool};
use crate::tui::bridge::Ev;

const COLS: u16 = 60;
const ROWS: u16 = 16;

fn app_with(s: Settings) -> App {
    let mut a = App::new();
    a.show_reasoning = s.thinking;
    a.mini = Some(s);
    a.header.session = "ses_1".into();
    a.header.agent = "build".into();
    a.header.model = "fake/model".into();
    a
}

fn app() -> App {
    app_with(Settings {
        splash: false,
        turn_summary: false,
        ..Settings::default()
    })
}

struct Term {
    mini: Mini<Vec<u8>>,
    cols: u16,
    rows: u16,
}

impl Term {
    fn new() -> Self {
        Self::sized(COLS, ROWS, 0)
    }

    fn sized(cols: u16, rows: u16, row: u16) -> Self {
        Term {
            mini: Mini::new(Vec::new(), cols, rows, row, "~/src/project".into()),
            cols,
            rows,
        }
    }

    fn frame(&mut self, app: &mut App) {
        self.mini.draw_at(app, self.cols, self.rows).unwrap();
    }

    fn bytes(&self) -> &[u8] {
        self.mini.surface().out()
    }

    fn parser(&self) -> vt100::Parser {
        let mut p = vt100::Parser::new(self.rows, self.cols, 2000);
        p.process(self.bytes());
        p
    }

    /// Every row: the scrollback's, then the screen's.
    fn history(&self) -> Vec<String> {
        let mut p = self.parser();
        // `vt100` shows at most a screenful of its scrollback.
        p.set_scrollback(self.rows as usize);
        let back = p.screen().scrollback();
        let mut out = Vec::new();
        for off in (1..=back).rev() {
            p.set_scrollback(off);
            out.push(p.screen().rows(0, self.cols).next().unwrap_or_default());
        }
        p.set_scrollback(0);
        out.extend(p.screen().rows(0, self.cols));
        out
    }

    /// The rows above the footer.
    fn written(&self) -> Vec<String> {
        let mut h = self.history();
        let footer = (self.rows - self.mini.surface().top()) as usize;
        h.truncate(h.len() - footer);
        h
    }

    fn written_text(&self) -> String {
        self.written().join("\n")
    }

    /// The footer's rows.
    fn footer(&self) -> Vec<String> {
        let p = self.parser();
        let mut rows: Vec<String> = p
            .screen()
            .rows(0, self.cols)
            .skip(self.mini.surface().top() as usize)
            .collect();
        while rows.last().is_some_and(|r| r.is_empty()) {
            rows.pop();
        }
        rows
    }
}

fn tool(name: &str, detail: &str, output: Option<&str>) -> Tool {
    Tool {
        id: format!("t-{name}-{detail}"),
        name: name.into(),
        detail: detail.into(),
        result: output.map(|o| (o.lines().next().unwrap_or("").to_string(), false)),
        output: output.unwrap_or("").into(),
        ..Default::default()
    }
}

// mini/footer.test.ts: "tools setting %s controls tool and skill
// transcript" (show and hide; adapted: CodeSmith has no skill rows).
#[test]
fn oc_the_tools_setting_controls_tool_rows_in_the_scrollback() {
    for tools in [false, true] {
        let mut a = app_with(Settings {
            tools,
            splash: false,
            turn_summary: false,
            ..Settings::default()
        });
        a.verbosity = crate::tui::group::Verbosity::Medium;
        a.entries.push(Entry::User("do the thing".into()));
        a.entries.push(Entry::Tool(tool(
            "write",
            "notes.txt",
            Some("Wrote notes.txt"),
        )));
        a.entries.push(Entry::Assistant("all done".into()));
        let mut t = Term::new();
        t.frame(&mut a);
        let text = t.written_text();
        assert!(text.contains("do the thing"), "{text}");
        assert!(text.contains("all done"), "{text}");
        assert_eq!(text.contains("write notes.txt"), tools, "{text}");
    }
}

// mini/footer.test.ts: "reasoning in scrollback follows session.markdown
// %s" (rendered and source).
#[test]
fn oc_reasoning_in_the_scrollback_follows_session_markdown() {
    for source in [false, true] {
        let mut a = app();
        a.reasoning_source = source;
        a.entries.push(Entry::Reasoning(
            "| step | risk |\n| --- | --- |\n| check | low |\n\nDone.".into(),
        ));
        a.entries.push(Entry::Assistant("ok".into()));
        let mut t = Term::new();
        t.frame(&mut a);
        let rows = t.written();
        let row = rows
            .iter()
            .find(|r| r.contains("check"))
            .expect("the table row");
        assert_eq!(row.contains('|'), source, "{row}");
    }
}

// mini/footer.test.ts: "startup preserves the terminal prompt row from
// cursor row %s" (1, 5 and 24; adapted: what the shell printed stays, and
// the footer's cursor row holds still across frames).
#[test]
fn oc_startup_preserves_the_terminal_prompt_row() {
    for row in [1u16, 5, 24] {
        let mut t = Term::sized(60, 30, row);
        // The shell's own rows, above where the cursor was.
        let mut shell = Vec::new();
        for i in 0..row {
            shell.extend_from_slice(format!("shell line {i}\r\n").as_bytes());
        }
        t.mini.surface_mut().out_mut().extend_from_slice(&shell);
        let mut a = app_with(Settings::default());
        t.frame(&mut a);
        let first = t.parser().screen().cursor_position();
        t.frame(&mut a);
        assert_eq!(t.parser().screen().cursor_position(), first);
        let h = t.history();
        for i in 0..row {
            assert!(h.contains(&format!("shell line {i}")), "row {row}: {h:?}");
        }
        let banner = h.iter().position(|r| r.contains("cs mini")).unwrap();
        let last_shell = h
            .iter()
            .position(|r| r == &format!("shell line {}", row - 1))
            .unwrap();
        assert!(banner > last_shell);
    }
}

// mini/splash.test.ts: "%s commits one scrollback snapshot without
// reflow on resize" (entry and exit; adapted: with replay off a resize
// writes nothing again).
#[test]
fn oc_banners_are_written_once() {
    let mut a = app_with(Settings {
        replay: false,
        ..Settings::default()
    });
    a.header.title = Some("Review mini layout".into());
    let mut t = Term::new();
    t.frame(&mut a);
    t.frame(&mut a);
    t.cols = 80;
    t.frame(&mut a);
    t.mini.close(&mut a).unwrap();
    let h = t.history();
    assert_eq!(h.iter().filter(|r| r.contains("cs mini")).count(), 1);
    let n = h
        .iter()
        .filter(|r| r.contains("Continue codesmith mini -s ses_1"))
        .count();
    assert_eq!(n, 1, "{h:?}");
    assert!(h.iter().any(|r| r == "Session  Review mini layout"));
}

// mini/scrollback.surface.test.ts: "turn summary starts at the left edge".
#[test]
fn oc_the_turn_summary_starts_at_the_left_edge() {
    let mut a = app_with(Settings {
        splash: false,
        ..Settings::default()
    });
    a.entries.push(Entry::User("hi".into()));
    a.busy = true;
    let mut t = Term::new();
    t.frame(&mut a);
    a.entries.push(Entry::Assistant("Hello.".into()));
    a.busy = false;
    t.frame(&mut a);
    let rows = t.written();
    let summary = rows
        .iter()
        .find(|r| r.contains("fake/model"))
        .expect("summary");
    assert!(
        summary.starts_with("build \u{b7} fake/model \u{b7} "),
        "{summary}"
    );
    let hello = rows.iter().position(|r| r.contains("Hello.")).unwrap();
    let at = rows.iter().position(|r| r.contains("fake/model")).unwrap();
    assert!(at > hello);
    // A turn summary is not written twice.
    t.frame(&mut a);
    assert_eq!(
        t.written()
            .iter()
            .filter(|r| r.contains("fake/model"))
            .count(),
        1
    );
}

// mini/scrollback.surface.test.ts: "renders plain errors with one blank
// line before and after the error block".
#[test]
fn oc_errors_have_one_blank_row_before_and_after() {
    let mut a = app();
    a.entries.push(Entry::Assistant("Trying.".into()));
    a.entries.push(Entry::Error("the provider failed".into()));
    a.entries.push(Entry::Notice("retry later".into()));
    let mut t = Term::new();
    t.frame(&mut a);
    let rows = t.written();
    let e = rows
        .iter()
        .position(|r| r.contains("the provider failed"))
        .unwrap();
    assert_eq!(rows[e - 1], "");
    assert_ne!(rows[e - 2], "");
    assert_eq!(rows[e + 1], "");
    assert!(rows[e + 2].contains("retry later"));
}

// mini/scrollback.surface.test.ts: "inserts spacers for new visible
// groups" (adapted: a text row and a run of tool rows are a blank row
// apart; tool rows that follow one another are not).
#[test]
fn oc_spacers_come_between_new_visible_groups() {
    let mut a = app_with(Settings {
        tools: true,
        splash: false,
        turn_summary: false,
        ..Settings::default()
    });
    a.verbosity = crate::tui::group::Verbosity::Medium;
    a.entries.push(Entry::Assistant("Looking.".into()));
    a.entries
        .push(Entry::Tool(tool("write", "a.txt", Some("ok"))));
    a.entries
        .push(Entry::Tool(tool("write", "b.txt", Some("ok"))));
    a.entries.push(Entry::Assistant("Done.".into()));
    let mut t = Term::new();
    t.frame(&mut a);
    let rows = t.written();
    let at = |s: &str| rows.iter().position(|r| r.contains(s)).unwrap();
    let (first, second) = (at("a.txt"), at("b.txt"));
    assert_eq!(rows[first - 1], "");
    assert!(second > first);
    assert!(rows[first + 1..second].iter().all(|r| !r.is_empty()));
    assert_eq!(rows[at("Done.") - 1], "");
    // Never two blank rows in a row.
    assert!(!rows.windows(2).any(|w| w[0].is_empty() && w[1].is_empty()));
}

// mini/scrollback.surface.test.ts: "markdown written in several commits
// keeps its blank rows on the terminal".
#[test]
fn oc_markdown_written_in_several_commits_keeps_its_blank_rows() {
    let mut a = app();
    a.busy = true;
    let mut t = Term::new();
    a.entries
        .push(Entry::Assistant("First paragraph.\n\nSecond".into()));
    t.frame(&mut a);
    assert!(t.written_text().contains("First paragraph."));
    assert!(!t.written_text().contains("Second"));
    // The rest is live in the footer meanwhile.
    assert!(t.footer().iter().any(|r| r.contains("Second")));
    if let Some(Entry::Assistant(s)) = a.entries.last_mut() {
        s.push_str(" paragraph.\n\nThird paragraph.");
    }
    t.frame(&mut a);
    a.busy = false;
    t.frame(&mut a);
    let rows = t.written();
    let at = |s: &str| rows.iter().position(|r| r.contains(s)).unwrap();
    assert_eq!(at("Second paragraph.") - at("First paragraph."), 2);
    assert_eq!(at("Third paragraph.") - at("Second paragraph."), 2);
    assert_eq!(rows[at("Second paragraph.") - 1], "");
    assert_eq!(
        rows.iter()
            .filter(|r| r.contains("First paragraph."))
            .count(),
        1
    );
}

// mini/scrollback.surface.test.ts: "holds markdown code blocks until
// final commit and keeps newline ownership".
#[test]
fn oc_code_blocks_are_held_until_they_close() {
    let mut a = app();
    a.busy = true;
    let mut t = Term::new();
    a.entries.push(Entry::Assistant(
        "Here:\n\n```rust\nfn a() {}\n\nfn b() {}\n".into(),
    ));
    t.frame(&mut a);
    let text = t.written_text();
    assert!(text.contains("Here:"));
    assert!(
        !text.contains("fn a"),
        "an open code block is not written: {text}"
    );
    if let Some(Entry::Assistant(s)) = a.entries.last_mut() {
        s.push_str("```\n\nAfter it.");
    }
    t.frame(&mut a);
    let text = t.written_text();
    assert!(text.contains("fn a") && text.contains("fn b"), "{text}");
    assert!(!text.contains("After it."));
    a.busy = false;
    t.frame(&mut a);
    let rows = t.written();
    assert_eq!(rows.iter().filter(|r| r.contains("fn a")).count(), 1);
    assert!(rows.iter().any(|r| r.contains("After it.")));
}

// mini/scrollback.surface.test.ts: "finalizes markdown tables for
// streamed and coalesced input".
#[test]
fn oc_tables_render_the_same_streamed_or_whole() {
    let table = "| name | size |\n| --- | --- |\n| a.rs | 12 |\n| b.rs | 7 |\n\nThat is all.";
    let whole = {
        let mut a = app();
        a.entries.push(Entry::Assistant(table.into()));
        let mut t = Term::new();
        t.frame(&mut a);
        t.written()
    };
    let streamed = {
        let mut a = app();
        a.busy = true;
        a.entries.push(Entry::Assistant(String::new()));
        let mut t = Term::new();
        for chunk in table.as_bytes().chunks(7) {
            if let Some(Entry::Assistant(s)) = a.entries.last_mut() {
                s.push_str(std::str::from_utf8(chunk).unwrap());
            }
            t.frame(&mut a);
        }
        a.busy = false;
        t.frame(&mut a);
        t.written()
    };
    assert_eq!(whole, streamed);
    assert!(whole.iter().any(|r| r.contains("a.rs") && r.contains('│')));
}

// mini/entry.body.test.ts: "keeps running subagent tool state out of
// scrollback".
#[test]
fn oc_a_running_subagent_stays_out_of_the_scrollback() {
    let mut a = app_with(Settings {
        tools: true,
        splash: false,
        turn_summary: false,
        ..Settings::default()
    });
    a.verbosity = crate::tui::group::Verbosity::Medium;
    a.busy = true;
    a.entries.push(Entry::User("explore it".into()));
    let mut call = tool("subagent", "Find the adder", None);
    let mut sub = Sub::default();
    sub.agent = "explore".into();
    sub.title = "Find the adder".into();
    call.sub = Some(Box::new(sub));
    a.entries.push(Entry::Tool(call));
    let mut t = Term::new();
    t.frame(&mut a);
    assert!(!t.written_text().contains("Find the adder"));
    assert!(
        t.footer().iter().any(|r| r.contains("Find the adder")),
        "the subagent's row is live in the footer: {:?}",
        t.footer()
    );
    if let Some(Entry::Tool(c)) = a.entries.last_mut() {
        c.result = Some(("found it".into(), false));
        c.output = "found it".into();
    }
    a.entries.push(Entry::Assistant("It is in math.rs.".into()));
    t.frame(&mut a);
    assert!(t.written_text().contains("Find the adder"));
}

// mini/entry.body.test.ts: "hides shell tool output but not direct shell
// output" (adapted: `shell_output` decides whether a finished command's
// output is written under its row).
#[test]
fn oc_shell_output_shows_only_when_asked() {
    for show in [false, true] {
        let mut a = app_with(Settings {
            tools: true,
            shell_output: show,
            splash: false,
            turn_summary: false,
            ..Settings::default()
        });
        a.verbosity = crate::tui::group::Verbosity::Medium;
        a.entries.push(Entry::Tool(tool(
            "shell",
            "cargo test",
            Some("test result: ok"),
        )));
        a.entries.push(Entry::Assistant("Tests pass.".into()));
        let mut t = Term::new();
        t.frame(&mut a);
        let text = t.written_text();
        assert!(text.contains("cargo test"), "{text}");
        assert_eq!(text.contains("test result: ok"), show, "{text}");
        // The output is still the session's.
        let Entry::Tool(s) = &a.entries[0] else {
            panic!()
        };
        assert_eq!(s.output, "test result: ok");
    }
}

// mini/footer.responsive.test.tsx: "ctrl+l clears the split-footer screen
// without wiping scrollback".
#[test]
fn oc_clearing_the_screen_keeps_the_scrollback() {
    let mut a = app();
    a.entries.push(Entry::Assistant("kept in history".into()));
    let mut t = Term::new();
    t.frame(&mut a);
    t.mini.clear();
    t.frame(&mut a);
    let p = t.parser();
    let screen: Vec<String> = p.screen().rows(0, COLS).collect();
    assert!(!screen.iter().any(|r| r.contains("kept in history")));
    assert_eq!(t.mini.surface().top(), 0);
    assert!(t.history().iter().any(|r| r.contains("kept in history")));
}

// mini/footer.test.ts: "production footer preserves wrapped input and
// status on resize (mono=%s)" (adapted: the draft and the status line are
// in the footer at both widths).
#[test]
fn oc_the_footer_keeps_the_draft_and_status_across_resizes() {
    for mono in [false, true] {
        the_footer_keeps_the_draft_and_status(mono);
    }
}

fn the_footer_keeps_the_draft_and_status(mono: bool) {
    let mut a = app_with(Settings {
        mono,
        splash: false,
        ..Settings::default()
    });
    a.input
        .set("a draft long enough to wrap onto a second row of the prompt box");
    let mut t = Term::new();
    t.frame(&mut a);
    let joined = |t: &Term| t.footer().join(" ");
    assert!(joined(&t).contains("a draft"));
    assert!(joined(&t).contains("fake/model"));
    t.cols = 40;
    t.frame(&mut a);
    assert!(joined(&t).contains("a draft"));
    assert!(joined(&t).contains("prompt box"), "{:?}", t.footer());
    assert!(t.footer().iter().any(|r| r.contains("build")));
}

#[test]
fn a_permission_prompt_waits_in_the_footer() {
    let mut a = app();
    a.entries.push(Entry::User("delete it".into()));
    a.busy = true;
    a.entries
        .push(Entry::Tool(tool("shell", "rm victim.txt", None)));
    let (tx, _rx) = tokio::sync::oneshot::channel();
    a.on_event(Ev::Permission(
        codesmith_core::permission::Request {
            action: "shell".into(),
            resources: vec!["rm victim.txt".into()],
            save: vec!["rm *".into()],
            title: "rm victim.txt".into(),
        },
        tx,
    ));
    let mut t = Term::new();
    t.frame(&mut a);
    let f = t.footer().join("\n");
    assert!(f.contains("Permission required"), "{f}");
    assert!(f.contains("allow once"), "{f}");
    assert!(f.contains("rm victim.txt"));
    // The prompt is above, in the scrollback; the call waits in the footer.
    assert!(t.written_text().contains("delete it"));
    assert!(!t.written_text().contains("rm victim.txt"));
    assert!(matches!(a.modal, Some(Modal::Permission { .. })));
}

#[test]
fn a_question_waits_in_the_footer() {
    let mut a = app();
    a.busy = true;
    let (tx, _rx) = tokio::sync::oneshot::channel();
    a.on_event(Ev::Question(
        vec![codesmith_core::question::Prompt {
            question: "Which database?".into(),
            header: "DB".into(),
            options: vec![
                codesmith_core::question::Choice {
                    label: "Postgres".into(),
                    description: "the usual".into(),
                },
                codesmith_core::question::Choice {
                    label: "SQLite".into(),
                    description: String::new(),
                },
            ],
            multiple: false,
        }],
        tx,
    ));
    let mut t = Term::new();
    t.frame(&mut a);
    let f = t.footer().join("\n");
    assert!(f.contains("Which database?"), "{f}");
    assert!(f.contains("1. Postgres") && f.contains("2. SQLite"), "{f}");
}

#[test]
fn running_shell_output_is_live_in_the_footer() {
    let mut a = app();
    a.busy = true;
    let mut run = tool("shell", "make", None);
    run.live = "compiling a\ncompiling b\n".into();
    a.entries.push(Entry::Tool(run));
    let mut t = Term::new();
    t.frame(&mut a);
    assert!(t.footer().iter().any(|r| r.contains("compiling b")));
    assert!(!t.written_text().contains("compiling"));
}

#[test]
fn a_resize_writes_the_history_again_at_the_new_width() {
    let mut a = app();
    let long = "word ".repeat(30);
    a.entries.push(Entry::Assistant(long.trim().into()));
    let mut t = Term::new();
    t.frame(&mut a);
    let narrow = t.written().iter().filter(|r| r.contains("word")).count();
    t.cols = 30;
    t.frame(&mut a);
    let rows = t.written();
    let wide = rows.iter().filter(|r| r.contains("word")).count();
    assert!(wide > narrow, "{rows:?}");
    assert!(rows.iter().all(|r| r.chars().count() <= 30));
    // The screen was cleared first: nothing is there twice.
    let joined: String = rows.concat();
    assert_eq!(joined.matches("word").count(), 30);
}

#[test]
fn a_resumed_session_replays_its_newest_messages() {
    let mut a = app_with(Settings {
        splash: false,
        turn_summary: false,
        replay_limit: 2,
        ..Settings::default()
    });
    a.set_entries(vec![
        Entry::User("first question".into()),
        Entry::Assistant("first answer".into()),
        Entry::User("second question".into()),
        Entry::Assistant("second answer".into()),
    ]);
    let mut t = Term::new();
    t.frame(&mut a);
    let text = t.written_text();
    assert!(!text.contains("first"), "{text}");
    assert!(text.contains("second question") && text.contains("second answer"));
    // With replay off, none of it.
    let mut a = app_with(Settings {
        splash: false,
        replay: false,
        ..Settings::default()
    });
    a.set_entries(vec![Entry::User("old".into())]);
    let mut t = Term::new();
    t.frame(&mut a);
    assert!(!t.written_text().contains("old"));
}

#[test]
fn an_interrupted_turn_keeps_what_was_on_view() {
    let mut a = app();
    a.busy = true;
    a.entries.push(Entry::User("go".into()));
    a.entries.push(Entry::Assistant("Half an ans".into()));
    let mut t = Term::new();
    t.frame(&mut a);
    assert!(!t.written_text().contains("Half an ans"));
    // The session reloads from the store: the same session, its entries
    // replaced.
    a.busy = false;
    a.set_entries(vec![Entry::User("go".into())]);
    a.notice("interrupted");
    t.frame(&mut a);
    let text = t.written_text();
    assert!(text.contains("Half an ans"), "{text}");
    assert_eq!(text.matches("go").count(), 1, "{text}");
    assert!(text.contains("interrupted"));
}

#[test]
fn a_todo_list_in_the_scrollback_stays_when_a_new_one_comes() {
    use codesmith_core::tool::todo::{Status, Todo};
    let todo = |c: &str| Todo {
        content: c.into(),
        status: Status::Pending,
    };
    let mut a = app();
    a.busy = true;
    a.on_event(Ev::Todos(vec![todo("one")]));
    a.entries.push(Entry::Assistant("Working.".into()));
    a.entries.push(Entry::User("next".into()));
    let mut t = Term::new();
    t.frame(&mut a);
    assert!(t.written_text().contains("one"));
    a.on_event(Ev::Todos(vec![todo("two")]));
    assert!(
        matches!(a.entries[0], Entry::Todos(_)),
        "the written list stays"
    );
    a.busy = false;
    t.frame(&mut a);
    let text = t.written_text();
    assert_eq!(text.matches("○ one").count(), 1, "{text}");
    assert!(text.contains("○ two"));
}

// mini/scrollback.surface.test.ts: "renders monochrome scrollback as
// ASCII markdown" (adapted: CodeSmith's renderer, with symbols as ASCII
// and no colors).
#[test]
fn oc_monochrome_scrollback_is_ascii() {
    let mut a = app_with(Settings {
        mono: true,
        tools: true,
        ..Settings::default()
    });
    a.verbosity = crate::tui::group::Verbosity::Medium;
    a.entries.push(Entry::User("list them".into()));
    a.entries.push(Entry::Assistant(
        "# Files\n\n- one\n- two\n\n| a | b |\n| --- | --- |\n| 1 | 2 |".into(),
    ));
    a.entries
        .push(Entry::Tool(tool("write", "x.txt", Some("ok"))));
    let mut t = Term::new();
    t.frame(&mut a);
    t.mini.close(&mut a).unwrap();
    for r in t.history() {
        assert!(r.is_ascii(), "{r:?}");
    }
    let p = t.parser();
    for y in 0..ROWS {
        for x in 0..COLS {
            let c = p.screen().cell(y, x).unwrap();
            assert_eq!(c.fgcolor(), vt100::Color::Default);
        }
    }
}

#[test]
fn mini_mode_never_uses_the_alternate_screen_and_synchronizes_frames() {
    let mut a = app_with(Settings::default());
    a.entries.push(Entry::User("hi".into()));
    let mut t = Term::new();
    t.frame(&mut a);
    t.frame(&mut a);
    t.mini.close(&mut a).unwrap();
    let s = String::from_utf8_lossy(t.bytes());
    assert!(!s.contains("\x1b[?1049h") && !s.contains("\x1b[?47h"));
    assert_eq!(
        s.matches("\x1b[?2026h").count(),
        s.matches("\x1b[?2026l").count()
    );
    assert!(s.starts_with("\x1b[?2026h"));
}

// mini/footer.test.ts: "command menu uses its full height on first open
// (mono=%s)" (adapted: a picker gets its rows in the footer when it opens).
#[test]
fn oc_a_picker_uses_its_full_height_when_it_opens() {
    for mono in [false, true] {
        a_picker_uses_its_full_height(mono);
    }
}

fn a_picker_uses_its_full_height(mono: bool) {
    use crate::tui::app::{Pick, PickItem, Picker};
    let mut a = app_with(Settings {
        mono,
        splash: false,
        ..Settings::default()
    });
    let items: Vec<PickItem> = (0..6)
        .map(|i| PickItem {
            id: format!("m{i}"),
            label: format!("model {i}"),
            detail: String::new(),
            current: false,
        })
        .collect();
    a.modal = Some(Modal::Picker(Picker::new(Pick::Model, "Models", items)));
    let mut t = Term::sized(60, 30, 0);
    t.frame(&mut a);
    let f = t.footer().join("\n");
    for i in 0..6 {
        assert!(f.contains(&format!("model {i}")), "{f}");
    }
}

#[test]
fn the_status_line_shows_the_spinner_and_interrupt_key_while_working() {
    let mut a = app();
    a.busy = true;
    a.entries.push(Entry::User("go".into()));
    let mut t = Term::new();
    t.frame(&mut a);
    let status = t.footer().last().cloned().unwrap();
    assert!(status.contains("working"), "{:?}", t.footer());
    assert!(status.contains("interrupt"), "{status}");
    a.busy = false;
    t.frame(&mut a);
    let status = t.footer().last().cloned().unwrap();
    assert!(!status.contains("working"), "{status}");
    assert!(status.contains("build"));
}

// mini/stream.test.ts: "delivers commits before ordered footer updates":
// in a frame, the rows written to the scrollback come before the footer
// drawn under them.
#[test]
fn oc_commits_come_before_the_footer_in_a_frame() {
    let mut a = app();
    let mut t = Term::new();
    t.frame(&mut a);
    let before = t.bytes().len();
    a.entries.push(Entry::Notice("a finished row".into()));
    a.input.set("typed draft");
    t.frame(&mut a);
    let frame = String::from_utf8_lossy(&t.bytes()[before..]).to_string();
    let row = frame.find("a finished row").expect("the row");
    let draft = frame.find("typed draft").expect("the footer");
    assert!(row < draft);
    assert_eq!(
        frame.matches("\x1b[?2026h").count(),
        1,
        "one frame, one write"
    );
}
