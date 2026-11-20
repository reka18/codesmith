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
    a.start_turn();
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
    // A `!command` the user ran shows with its output, whatever the
    // settings.
    let mut a = app_with(Settings {
        tools: false,
        shell_output: false,
        splash: false,
        turn_summary: false,
        ..Settings::default()
    });
    let mut direct = tool("shell", "pwd", Some("/tmp/demo"));
    direct.id = "user-shell-1".into();
    a.entries.push(Entry::Tool(direct));
    let mut t = Term::new();
    t.frame(&mut a);
    let text = t.written_text();
    assert!(text.contains("pwd") && text.contains("/tmp/demo"), "{text}");
}

// mini/runtime.queue.test.ts: "shell mode does not emit a turn duration
// summary": a `!command` is no model turn.
#[test]
fn oc_shell_mode_writes_no_turn_summary() {
    let mut a = app_with(Settings {
        splash: false,
        ..Settings::default()
    });
    // The runner marks a shell run busy without starting a turn.
    a.busy = true;
    let mut t = Term::new();
    t.frame(&mut a);
    let mut direct = tool("shell", "ls", Some("a.txt"));
    direct.id = "user-shell-1".into();
    a.entries.push(Entry::Tool(direct));
    a.busy = false;
    t.frame(&mut a);
    let text = t.written_text();
    assert!(text.contains("a.txt"), "{text}");
    assert!(!text.contains("fake/model"), "{text}");
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

// ---- Pending prompts (`mini::queue`) ----

fn press(a: &mut App, code: ratatui::crossterm::event::KeyCode) -> Option<crate::tui::app::Action> {
    a.on_key(ratatui::crossterm::event::KeyEvent::new(
        code,
        ratatui::crossterm::event::KeyModifiers::NONE,
    ))
}

fn ctrl(a: &mut App, c: char) -> Option<crate::tui::app::Action> {
    a.on_key(ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char(c),
        ratatui::crossterm::event::KeyModifiers::CONTROL,
    ))
}

fn type_text(a: &mut App, s: &str) {
    for c in s.chars() {
        press(a, ratatui::crossterm::event::KeyCode::Char(c));
    }
}

/// Type `s` and press enter.
fn send(a: &mut App, s: &str) -> Option<crate::tui::app::Action> {
    type_text(a, s);
    press(a, ratatui::crossterm::event::KeyCode::Enter)
}

fn queued(a: &App) -> Vec<&str> {
    a.queue.items.iter().map(|q| q.text.as_str()).collect()
}

/// `ctrl+x q`: the pending-prompt menu.
fn open_queue(a: &mut App) {
    ctrl(a, 'x');
    press(a, ratatui::crossterm::event::KeyCode::Char('q'));
}

// mini/runtime.queue.test.ts: "durably admits in-flight follow-ups in
// submission order" (adapted: CodeSmith queues every follow-up; it has no
// steering) and "runs and queues image-only prompts without optimistic
// image rows": nothing reaches the scrollback until a prompt is sent.
#[test]
fn oc_follow_ups_wait_in_submission_order() {
    let mut a = app();
    a.busy = true;
    assert_eq!(send(&mut a, "two"), None);
    assert_eq!(send(&mut a, "three"), None);
    // A pasted image alone still queues, its file with it.
    a.attachments.push(crate::tui::attach::Pending {
        label: "[Image 1]".into(),
        path: "/tmp/i.png".into(),
    });
    a.input.set("[Image 1]");
    assert_eq!(
        press(&mut a, ratatui::crossterm::event::KeyCode::Enter),
        None
    );
    assert_eq!(queued(&a), ["two", "three", "[Image 1]"]);
    assert_eq!(a.queue.items[2].files.len(), 1);
    assert!(a.attachments.is_empty());
    assert!(a.input.is_empty());
    let mut t = Term::new();
    t.frame(&mut a);
    assert!(!t.written_text().contains("two"), "{}", t.written_text());
    // They go in order once the turn is over.
    a.busy = false;
    assert_eq!(a.queue.next().unwrap().text, "two");
    assert_eq!(a.queue.next().unwrap().text, "three");
}

// mini/runtime.queue.test.ts: "keeps prompts submitted after an in-flight
// /compact behind the compaction barrier", "treats /new as a local
// session command" and "treats %s as a close command" (/exit, /quit, /q,
// :q): `/compact` and custom commands wait their turn; the UI's own
// commands act at once.
#[test]
fn oc_compact_waits_its_turn_and_local_commands_act_at_once() {
    use crate::tui::app::Action;
    let mut a = app();
    let slash = |name: &str| {
        Some(Action::Slash {
            name: name.into(),
            args: String::new(),
        })
    };
    // Idle, `/compact` is the local command.
    assert_eq!(send(&mut a, "/compact"), slash("compact"));
    a.busy = true;
    assert_eq!(send(&mut a, "/compact"), None);
    assert_eq!(send(&mut a, "later"), None);
    assert_eq!(queued(&a), ["/compact", "later"]);
    assert_eq!(send(&mut a, "/new"), slash("new"));
    for (cmd, name) in [
        ("/exit", "exit"),
        ("/quit", "quit"),
        ("/q", "q"),
        (":q", "quit"),
    ] {
        assert_eq!(send(&mut a, cmd), slash(name), "{cmd}");
    }
    assert_eq!(queued(&a), ["/compact", "later"]);
}

// mini/runtime.queue.test.ts: "shell mode submits /exit as a shell
// command" and "shell mode submits /new instead of creating a session"
// (adapted: shell mode is a `!` prompt).
#[test]
fn oc_shell_mode_sends_slash_text_as_a_command() {
    use crate::tui::app::Action;
    let mut a = app();
    assert_eq!(
        send(&mut a, "!/exit"),
        Some(Action::Submit("!/exit".into()))
    );
    assert_eq!(send(&mut a, "!/new"), Some(Action::Submit("!/new".into())));
    assert!(!a.quit);
    a.busy = true;
    assert_eq!(send(&mut a, "!/new"), None);
    assert_eq!(queued(&a), ["!/new"]);
}

// mini/footer.view.test.tsx: "direct footer toggles and deletes pending
// %s prompts" (queue; adapted: enter sends the prompt next instead of
// steering it) and "direct footer counts queued and steering work while
// running".
#[test]
fn oc_the_footer_lists_and_deletes_pending_prompts() {
    let mut a = app();
    a.busy = true;
    send(&mut a, "follow up");
    send(&mut a, "another one");
    let mut t = Term::sized(100, ROWS, 0);
    t.frame(&mut a);
    let status = t.footer().last().cloned().unwrap();
    assert!(status.contains("ctrl+x q 2 pending"), "{status}");
    // Down does not open it.
    press(&mut a, ratatui::crossterm::event::KeyCode::Down);
    t.frame(&mut a);
    assert!(!t.footer().join("\n").contains("Pending prompts"));
    open_queue(&mut a);
    assert!(!a.quit, "ctrl+x q opens the menu while prompts wait");
    t.frame(&mut a);
    let f = t.footer().join("\n");
    assert!(f.contains("Pending prompts"), "{f}");
    assert!(f.contains("follow up") && f.contains("queued"), "{f}");
    assert!(
        f.contains("enter next · ctrl+d delete · ctrl+u undo"),
        "{f}"
    );
    assert!(!f.contains('┌') && !f.contains('┃'), "{f}");
    // Enter sends the second one first.
    press(&mut a, ratatui::crossterm::event::KeyCode::Down);
    press(&mut a, ratatui::crossterm::event::KeyCode::Enter);
    assert_eq!(queued(&a), ["another one", "follow up"]);
    assert!(a.queue.menu.is_none());
    open_queue(&mut a);
    ctrl(&mut a, 'd');
    assert_eq!(queued(&a), ["follow up"]);
    ctrl(&mut a, 'd');
    assert!(a.queue.is_empty());
    t.frame(&mut a);
    let f = t.footer().join("\n");
    assert!(
        !f.contains("Pending prompts") && !f.contains("pending"),
        "{f}"
    );
    // With nothing waiting, `ctrl+x q` is the exit binding again.
    open_queue(&mut a);
    assert!(a.quit);
}

// mini/footer.view.test.tsx: "undo appends a pending prompt to the
// draft", "undo retains mentioned files when the prompt is sent again"
// and "an undone chip deletion keeps the chip's file".
#[test]
fn oc_undo_takes_a_pending_prompt_back_into_the_draft() {
    use crate::tui::app::Action;
    let mut a = app();
    a.busy = true;
    a.attachments.push(crate::tui::attach::Pending {
        label: "[Image 1]".into(),
        path: "/tmp/i.png".into(),
    });
    send(&mut a, "look at [Image 1]");
    type_text(&mut a, "existing draft");
    open_queue(&mut a);
    ctrl(&mut a, 'u');
    assert!(a.queue.is_empty() && a.queue.menu.is_none());
    assert_eq!(a.input.text(), "existing draft\n\nlook at [Image 1]");
    assert_eq!(a.attachments.len(), 1, "its file comes back");
    let mut t = Term::new();
    t.frame(&mut a);
    assert!(!t.footer().join("\n").contains("Pending prompts"));
    // Undoing the draft's deletion keeps the file.
    ctrl(&mut a, 'u');
    a.on_key(ratatui::crossterm::event::KeyEvent::new(
        ratatui::crossterm::event::KeyCode::Char('z'),
        ratatui::crossterm::event::KeyModifiers::SUPER,
    ));
    assert_eq!(a.input.text(), "existing draft\n\nlook at [Image 1]");
    assert_eq!(a.attachments.len(), 1);
    a.busy = false;
    assert_eq!(
        press(&mut a, ratatui::crossterm::event::KeyCode::Enter),
        Some(Action::Submit("existing draft\n\nlook at [Image 1]".into()))
    );
}

// mini/footer.view.test.tsx: "undo leaves the queue and input alone when
// cancellation fails" (adapted: nothing can fail; Escape closes the menu
// and the prompt stays queued) and "direct footer closes settings with
// ctrl-c instead of arming exit".
#[test]
fn oc_escape_and_ctrl_c_close_the_menu_and_keep_the_queue() {
    let mut a = app();
    a.busy = true;
    send(&mut a, "still queued");
    for close in [
        ratatui::crossterm::event::KeyEvent::from(ratatui::crossterm::event::KeyCode::Esc),
        ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('c'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        ),
    ] {
        open_queue(&mut a);
        assert!(a.queue.menu.is_some());
        a.on_key(close);
        assert!(a.queue.menu.is_none());
        assert!(!a.quit);
        assert!(a.busy, "the turn is not interrupted");
        assert_eq!(queued(&a), ["still queued"]);
    }
}

// mini/footer.view.test.tsx: "direct footer preserves steer and queue
// submission shortcuts while busy" (adapted: both queue) and "direct
// footer rejects local commands submitted with the queue shortcut".
#[test]
fn oc_the_queue_shortcut_queues_and_rejects_local_commands() {
    let mut a = app();
    a.busy = true;
    send(&mut a, "steer now");
    type_text(&mut a, "queue later");
    ctrl(&mut a, 'x');
    press(&mut a, ratatui::crossterm::event::KeyCode::Enter);
    assert_eq!(queued(&a), ["steer now", "queue later"]);
    type_text(&mut a, "/themes ");
    ctrl(&mut a, 'x');
    press(&mut a, ratatui::crossterm::event::KeyCode::Enter);
    assert_eq!(queued(&a), ["steer now", "queue later"]);
    assert!(a
        .entries
        .iter()
        .any(|e| matches!(e, Entry::Notice(n) if n == "this prompt cannot be queued")));
    assert_eq!(a.input.text(), "/themes ");
}

// mini/footer.view.test.tsx: "direct footer steers the oldest queued
// prompt from an empty composer" (adapted: after an interrupted or
// failed turn the queue is held, and enter on an empty prompt lets it go)
// and "direct footer does not steer queued work on a double submit".
#[test]
fn oc_enter_on_an_empty_prompt_lets_a_held_queue_go() {
    let mut a = app();
    a.busy = true;
    send(&mut a, "first");
    send(&mut a, "second");
    a.busy = false;
    assert!(a.queue.hold());
    assert_eq!(a.queue.next(), None, "held");
    let mut t = Term::sized(100, ROWS, 0);
    t.frame(&mut a);
    let status = t.footer().last().cloned().unwrap();
    assert!(status.contains("2 pending (held)"), "{status}");
    // A prompt typed now goes at once, and does not let the queue go.
    assert!(send(&mut a, "now").is_some());
    assert_eq!(a.queue.next(), None);
    assert_eq!(
        press(&mut a, ratatui::crossterm::event::KeyCode::Enter),
        None
    );
    assert_eq!(a.queue.next().unwrap().text, "first");
}

// Untrusted prompt text in the menu is one line with no control
// characters.
#[test]
fn the_pending_prompt_menu_strips_control_characters() {
    let mut a = app();
    a.busy = true;
    a.queue.items.push(super::queue::Queued {
        text: "evil\x1b[2J\x07 text\nmore".into(),
        files: vec![],
    });
    open_queue(&mut a);
    let mut t = Term::new();
    t.frame(&mut a);
    assert!(!String::from_utf8_lossy(t.bytes()).contains("\x1b[2J"));
    let f = t.footer().join("\n");
    assert!(f.contains("evil [2J text more"), "{f}");
}
