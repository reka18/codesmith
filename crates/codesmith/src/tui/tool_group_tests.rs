//! Tool groups in the transcript (`docs/design/tool-groups.md`): the group
//! line drawn for a run of tool calls, its glyph, the transcript cursor's
//! keys and the line it marks. The wording and glyphs themselves are
//! tested in `tool_group`.

use super::app::{App, Entry, Modal, Tool};
use super::bridge::Ev;
use super::diff::Diff;
use super::group::Verbosity;
use super::keymap::Keymap;
use super::theme::{self, ModeSetting, Themes};
use super::tool_group::{self, Glyph};
use super::view::{self, FORGE_FRAME};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use ratatui::text::Line;
use ratatui::Terminal;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

fn call(name: &str, detail: &str, result: Option<(&str, bool)>) -> Entry {
    Entry::Tool(Tool {
        id: format!("{name}-{detail}"),
        name: name.into(),
        detail: detail.into(),
        result: result.map(|(r, e)| (r.to_string(), e)),
        output: result.map(|(r, _)| r.to_string()).unwrap_or_default(),
        ..Default::default()
    })
}

fn ok(name: &str, detail: &str) -> Entry {
    call(name, detail, Some(("ok", false)))
}

fn bad(name: &str, detail: &str, why: &str) -> Entry {
    call(name, detail, Some((why, true)))
}

fn run(name: &str, detail: &str) -> Entry {
    call(name, detail, None)
}

fn edited(file: &str, added: usize, removed: usize) -> Entry {
    let mut e = ok("edit", file);
    if let Entry::Tool(t) = &mut e {
        t.diff = Some(Diff {
            added,
            removed,
            ..Default::default()
        });
    }
    e
}

fn text_of(l: &Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn lines(a: &App) -> Vec<String> {
    view::transcript(a, 80).iter().map(text_of).collect()
}

fn line_with(a: &App, s: &str) -> Line<'static> {
    view::transcript(a, 80)
        .into_iter()
        .find(|l| text_of(l).contains(s))
        .unwrap_or_else(|| panic!("no line with {s:?}: {:?}", lines(a)))
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn alt_up() -> KeyEvent {
    KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn light() {
    theme::install(Themes::new(None, Some(ModeSetting::Light), None, BTreeMap::new()).palette());
}

fn dark() {
    theme::install(Themes::new(None, Some(ModeSetting::Dark), None, BTreeMap::new()).palette());
}

/// A turn with every kind of call, as the approved preview scripts it.
fn turn() -> App {
    let mut a = App::new();
    a.entries = vec![
        Entry::User("Why does the login page hang?".into()),
        Entry::Assistant("I'll look at the redirect first.".into()),
        ok("shell", "rg -n redirect src/auth"),
        ok("shell", "git log -5"),
        Entry::Assistant("Reading the callback.".into()),
        ok("read", "src/auth/callback.ts"),
        ok("grep", "timeout in src/auth"),
        Entry::Assistant("Fixing it.".into()),
        edited("src/auth/callback.ts", 12, 3),
        edited("src/auth/session.ts", 4, 1),
        ok("webfetch", "docs.example.dev/session"),
        ok("subagent", "explore: find other callers"),
        bad(
            "github_create_issue",
            "Login hangs",
            "token lacks repo scope",
        ),
        Entry::Assistant("Fixed in two files.".into()),
    ];
    a
}

fn sprite(name: &str) -> Glyph {
    Glyph::of(name)
}

// ----- grouping -----

#[test]
fn text_from_the_model_splits_groups() {
    let a = turn();
    let groups = view::tool_groups(&a);
    assert_eq!(
        groups.iter().map(|g| g.key.as_str()).collect::<Vec<_>>(),
        [
            "shell-rg -n redirect src/auth",
            "read-src/auth/callback.ts",
            "edit-src/auth/callback.ts"
        ]
    );
    // A thought does not split a group; text does.
    let mut b = App::new();
    b.entries = vec![
        ok("read", "a"),
        Entry::Reasoning("hmm".into()),
        ok("read", "b"),
        Entry::Assistant("so".into()),
        ok("read", "c"),
    ];
    assert_eq!(view::tool_groups(&b).len(), 2);
    assert!(lines(&b).contains(&"⠶ Read 2 files ›".to_string()));
    // An empty answer between calls does not split them.
    b.entries.insert(4, Entry::Assistant(String::new()));
    b.entries.push(Entry::Assistant("  ".into()));
    b.entries.push(ok("shell", "ls"));
    assert_eq!(view::tool_groups(&b).len(), 2);
}

#[test]
fn a_group_without_calls_keeps_its_summary_row() {
    // Only thoughts: low verbosity's activity summary, as before.
    let mut a = App::new();
    a.entries = vec![
        Entry::Reasoning("Plan".into()),
        Entry::Assistant("Hi".into()),
    ];
    assert!(view::tool_groups(&a).is_empty());
    assert_eq!(lines(&a)[0], "+ 1 thought");
}

#[test]
fn groups_are_drawn_at_low_verbosity_only() {
    let mut a = turn();
    a.verbosity = Verbosity::Medium;
    assert!(view::tool_groups(&a).is_empty());
    assert!(lines(&a).iter().any(|l| l.starts_with("⚙ shell")));
    a.verbosity = Verbosity::High;
    assert!(view::tool_groups(&a).is_empty());
    // And alt+up then does nothing.
    a.on_key(alt_up());
    assert_eq!(a.tool_cursor, None);
}

// ----- render snapshots -----

const COLLAPSED: &[&str] = &[
    "",
    "› Why does the login page hang?",
    "",
    "I'll look at the redirect first.",
    "▞ Ran 2 commands ›",
    "Reading the callback.",
    "⠶ Read a file, ran a search ›",
    "Fixing it.",
    "",
    "Fixed in two files.",
];

fn collapsed() -> Vec<String> {
    let mut v: Vec<String> = COLLAPSED.iter().map(|s| s.to_string()).collect();
    v[8] = format!(
        "{} Edited 2 files, fetched a page, ran a subagent, called github_create_issue · 1 failed +16 −4 ›",
        sprite("github_create_issue").rest
    );
    v.remove(8);
    v.insert(
        8,
        // Cut to the width, keeping what collapsing never hides.
        format!(
            "{} {} · 1 failed +16 −4 ›",
            sprite("github_create_issue").rest,
            view_cut(
                "Edited 2 files, fetched a page, ran a subagent, called github_create_issue",
                76 - 2 - " · 1 failed +16 −4 ›".width()
            )
        ),
    );
    v
}

/// The view's cut: to `n` columns, an ellipsis at the end when cut.
fn view_cut(s: &str, n: usize) -> String {
    if s.width() <= n {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        if out.width() + 1 >= n {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

fn open_snapshot() -> Vec<String> {
    let g = sprite("github_create_issue");
    vec![
        "".into(),
        "› Why does the login page hang?".into(),
        "".into(),
        "I'll look at the redirect first.".into(),
        "▞ Ran 2 commands ⌄".into(),
        "✓ shell rg -n redirect src/auth".into(),
        "  → ok".into(),
        "✓ shell git log -5".into(),
        "  → ok".into(),
        "Reading the callback.".into(),
        "⠶ Read a file, ran a search ⌄".into(),
        "✓ read src/auth/callback.ts".into(),
        "  → ok".into(),
        "✓ grep timeout in src/auth".into(),
        "  → ok".into(),
        "Fixing it.".into(),
        format!(
            "{} {} · 1 failed +16 −4 ⌄",
            g.rest,
            view_cut(
                "Edited 2 files, fetched a page, ran a subagent, called github_create_issue",
                76 - 2 - " · 1 failed +16 −4 ⌄".width()
            )
        ),
        "✓ edit src/auth/callback.ts (+12 -3)".into(),
        "✓ edit src/auth/session.ts (+4 -1)".into(),
        "✓ webfetch docs.example.dev/session".into(),
        "  → ok".into(),
        "✓ subagent explore: find other callers".into(),
        "  → ok".into(),
        "✗ github_create_issue Login hangs".into(),
        "  ✗ token lacks repo scope".into(),
        "".into(),
        "Fixed in two files.".into(),
    ]
}

fn snapshot(a: &App) -> Vec<String> {
    lines(a)
}

#[test]
fn a_turn_with_every_kind_collapsed_and_open_in_dark_and_light() {
    for theme in [dark as fn(), light] {
        theme();
        let mut a = turn();
        let got = snapshot(&a);
        // Markdown paragraphs may add blank rows; compare without them.
        let strip = |v: Vec<String>| -> Vec<String> {
            v.into_iter().filter(|l| !l.trim().is_empty()).collect()
        };
        assert_eq!(strip(got.clone()), strip(collapsed()), "{got:#?}");
        a.on_key(ctrl('o'));
        let got = snapshot(&a);
        assert_eq!(strip(got.clone()), strip(open_snapshot()), "{got:#?}");
        // Every line fits the width.
        for l in view::transcript(&a, 80) {
            assert!(text_of(&l).width() <= 76, "{}", text_of(&l));
        }
    }
    dark();
}

#[test]
fn settled_glyphs_and_marks_take_the_themes_colours() {
    for (theme, mode) in [
        (dark as fn(), ModeSetting::Dark),
        (light, ModeSetting::Light),
    ] {
        theme();
        let p = Themes::new(None, Some(mode), None, BTreeMap::new()).palette();
        let mut a = turn();
        let fg = |l: &Line, i: usize| l.spans[i].style.fg;
        assert_eq!(fg(&line_with(&a, "Ran 2 commands"), 0), Some(p.success));
        let last = line_with(&a, "Edited 2 files");
        assert_eq!(fg(&last, 0), Some(p.error), "a call failed");
        let failed = last
            .spans
            .iter()
            .find(|s| s.content.contains("failed"))
            .unwrap();
        assert_eq!(failed.style.fg, Some(p.error));
        assert_eq!(failed.content, " · 1 failed");
        let plus = last.spans.iter().find(|s| s.content == " +16").unwrap();
        assert_eq!(plus.style.fg, Some(p.diff_added));
        let minus = last.spans.iter().find(|s| s.content == " −4").unwrap();
        assert_eq!(minus.style.fg, Some(p.diff_removed));
        // A finished group's wording is muted.
        assert_eq!(fg(&last, 2), Some(p.muted));
        a.on_key(ctrl('o'));
        assert_eq!(fg(&line_with(&a, "✓ shell git log"), 0), Some(p.success));
        assert_eq!(
            fg(&line_with(&a, "✗ github_create_issue"), 0),
            Some(p.error)
        );
    }
    dark();
}

// ----- never hidden -----

#[test]
fn a_collapsed_line_still_shows_failures_and_the_diff_summary() {
    let mut a = App::new();
    a.entries = vec![
        bad("shell", "make", "exit 2"),
        bad("shell", "make test", "exit 1"),
        edited("a.rs", 3, 0),
        Entry::Assistant("Done".into()),
    ];
    assert_eq!(
        lines(&a)[0],
        "▆ Ran 2 commands, edited a file · 2 failed +3 −0 ›"
    );
    // A failed edit changed nothing, so it adds nothing to the summary.
    let mut a = App::new();
    let mut e = edited("a.rs", 9, 9);
    if let Entry::Tool(t) = &mut e {
        t.result = Some(("no match".into(), true));
    }
    a.entries = vec![e, edited("b.rs", 1, 2)];
    assert_eq!(lines(&a)[0], "▆ Edited 2 files · 1 failed +1 −2 ›");
    // No diff, no failure: neither shows.
    let mut a = App::new();
    a.entries = vec![ok("read", "a")];
    assert_eq!(lines(&a)[0], "⠶ Read a file ›");
}

#[test]
fn a_waiting_permission_takes_over_the_live_line_until_answered() {
    let mut a = App::new();
    a.busy = true;
    a.entries = vec![ok("read", "a"), run("shell", "rm x")];
    let (tx, _rx) = tokio::sync::oneshot::channel();
    a.on_event(Ev::Permission(
        codesmith_core::permission::Request {
            action: "shell".into(),
            resources: vec!["rm x".into()],
            save: vec![],
            title: "$ rm x".into(),
            access: None,
        },
        tx,
    ));
    let l = line_with(&a, "Waiting");
    assert_eq!(text_of(&l), "▖ Waiting for permission: $ rm x ›");
    assert_eq!(l.spans[2].style.fg, Some(theme::current().warning));
    // Answered: the wording is back.
    a.modal = None;
    assert_eq!(lines(&a)[0], "▖ Reading a file, running a command… ›");
    // A finished group never waits.
    a.entries.push(Entry::Assistant("next".into()));
    a.entries.push(run("shell", "y"));
    if let Some(Entry::Tool(t)) = a.entries.get_mut(1) {
        t.result = Some(("ok".into(), false));
    }
    let (tx, _rx) = tokio::sync::oneshot::channel();
    a.on_event(Ev::Permission(
        codesmith_core::permission::Request {
            action: "shell".into(),
            resources: vec![],
            save: vec![],
            title: "\u{1b}[2J y".into(),
            access: None,
        },
        tx,
    ));
    let l = lines(&a);
    assert_eq!(l[0], "▞ Read a file, ran a command ›");
    // The asker's title is shown with its control characters escaped.
    assert!(
        l.last()
            .unwrap()
            .contains("Waiting for permission: \\x1b[2J y"),
        "{l:?}"
    );
}

#[test]
fn a_waiting_question_takes_over_the_live_line() {
    let mut a = App::new();
    a.busy = true;
    a.entries = vec![run("github_ask", "which repo")];
    let (tx, _rx) = tokio::sync::oneshot::channel();
    a.on_event(Ev::Question(
        vec![codesmith_core::question::Prompt {
            question: "Which?".into(),
            header: String::new(),
            options: vec![],
            multiple: false,
        }],
        tx,
    ));
    assert!(matches!(a.modal, Some(Modal::Question { .. })));
    assert_eq!(
        lines(&a)[0],
        format!(
            "{} Waiting for your answer ›",
            sprite("github_ask").frames[0]
        )
    );
}

// ----- live glyphs -----

fn at_tick(a: &mut App, tick: u32) {
    a.busy_since = Some(Instant::now() - FORGE_FRAME * tick - FORGE_FRAME / 2);
}

#[test]
fn a_live_group_animates_the_running_calls_glyph_on_the_forge_clock() {
    dark();
    let mut a = App::new();
    a.busy = true;
    a.entries = vec![ok("shell", "ls"), run("read", "a.rs")];
    let mut widths = Vec::new();
    for t in 0..16 {
        at_tick(&mut a, t);
        let l = &view::transcript(&a, 80)[0];
        assert_eq!(
            l.spans[0].content,
            tool_group::LOOKING[t as usize % 8].to_string(),
            "tick {t}"
        );
        widths.push(text_of(l).width());
        // The running call's row in the open group animates too.
    }
    // The line never shifts as it animates.
    assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    assert!(a.animating(), "a live group keeps the clock ticking");
    // The heat colours: cherry red on the first frame.
    at_tick(&mut a, 0);
    let first = view::transcript(&a, 80)[0].spans[0].style.fg;
    assert_eq!(first, Some(Color::Rgb(0x8b, 0x1a, 0x1a)));
    // Live wording is in the text colour.
    assert_eq!(
        view::transcript(&a, 80)[0].spans[2].style.fg,
        Some(theme::current().text)
    );
    // Opened, the running call's row is led by the same glyph.
    a.on_key(ctrl('o'));
    at_tick(&mut a, 3);
    let l = lines(&a);
    assert_eq!(l[1], "✓ shell ls");
    assert!(l.iter().any(|l| l == "⠁ read a.rs …"), "{l:?}");
}

#[test]
fn a_running_subagent_glows_in_the_agent_colour() {
    let mut a = App::new();
    a.busy = true;
    a.entries = vec![run("subagent", "explore: survey")];
    for t in 0..8 {
        at_tick(&mut a, t);
        let l = &view::transcript(&a, 80)[0];
        assert_eq!(
            l.spans[0].content,
            tool_group::SUBAGENT[t as usize].to_string()
        );
        assert_eq!(l.spans[0].style.fg, Some(theme::current().agent));
    }
}

#[test]
fn animations_off_keeps_every_glyph_on_its_first_frame() {
    let (s, e) = super::settings::load(&json!({"animations": false}));
    assert!(e.is_empty(), "{e:?}");
    assert!(!s.animations);
    let mut a = App::new();
    a.still = !s.animations;
    a.busy = true;
    a.entries = vec![run("edit", "a.rs")];
    let mut seen = std::collections::HashSet::new();
    for t in 0..24 {
        at_tick(&mut a, t);
        assert_eq!(a.frame(), 0);
        let l = &view::transcript(&a, 80)[0];
        seen.insert((l.spans[0].content.to_string(), l.spans[0].style.fg));
    }
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen.iter().all(|(c, _)| c == "▁"));
    // On by default; a wrong value is reported and the default kept.
    assert!(super::settings::load(&serde_json::Value::Null).0.animations);
    let (s, e) = super::settings::load(&json!({"animations": "no"}));
    assert!(s.animations);
    assert_eq!(e, ["tui.animations: must be true or false"]);
    // The busy ember is not this setting's: the frame clock is the turn's.
    let mut b = App::new();
    b.busy = true;
    at_tick(&mut b, 5);
    assert_eq!(b.frame(), 5);
}

#[test]
fn the_animation_setting_follows_the_shown_tab() {
    let mut a = App::new();
    a.still = true;
    let mut b = App::new();
    a.hand_over(&mut b);
    assert!(b.still);
}

// ----- the transcript cursor -----

/// A transcript of `n` groups, each a call then a line of text.
fn many(n: usize) -> App {
    let mut a = App::new();
    for i in 0..n {
        a.entries.push(ok("shell", &format!("step {i}")));
        a.entries.push(Entry::Assistant(format!("after {i}")));
    }
    a.view_size.set((80, 10));
    a
}

fn selected(a: &App) -> Option<&str> {
    a.tool_cursor.as_deref()
}

#[test]
fn alt_up_selects_the_latest_group_and_up_down_stop_at_the_ends() {
    let mut a = many(3);
    a.on_key(alt_up());
    assert_eq!(selected(&a), Some("shell-step 2"));
    a.on_key(key(KeyCode::Down));
    assert_eq!(selected(&a), Some("shell-step 2"), "stops at the newest");
    a.on_key(key(KeyCode::Up));
    a.on_key(key(KeyCode::Up));
    assert_eq!(selected(&a), Some("shell-step 0"));
    a.on_key(key(KeyCode::Up));
    assert_eq!(selected(&a), Some("shell-step 0"), "stops at the oldest");
    a.on_key(key(KeyCode::Down));
    assert_eq!(selected(&a), Some("shell-step 1"));
    // The prompt was not touched.
    assert!(a.input.is_empty());
}

#[test]
fn alt_up_with_no_groups_does_nothing() {
    let mut a = App::new();
    a.entries = vec![Entry::Assistant("hi".into())];
    a.on_key(alt_up());
    assert_eq!(a.tool_cursor, None);
    assert!(a.input.is_empty());
}

#[test]
fn return_space_and_right_open_left_closes() {
    for open in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Right] {
        let mut a = many(2);
        a.on_key(alt_up());
        a.on_key(key(open));
        assert!(a.group_open("shell-step 1"), "{open:?}");
        assert!(!a.group_open("shell-step 0"), "only the selected one");
        assert!(lines(&a).iter().any(|l| l == "✓ shell step 1"), "{open:?}");
        assert!(!lines(&a).iter().any(|l| l == "✓ shell step 0"));
        // Opening again keeps it open.
        a.on_key(key(open));
        assert!(a.group_open("shell-step 1"));
        a.on_key(key(KeyCode::Left));
        assert!(!a.group_open("shell-step 1"));
        assert!(a.tool_cursor.is_some(), "still selected");
        assert!(a.input.is_empty(), "space typed nothing");
    }
}

#[test]
fn escape_leaves_without_interrupting_and_typing_leaves_to_the_input() {
    let mut a = many(2);
    a.busy = true;
    a.on_key(alt_up());
    assert_eq!(a.on_key(key(KeyCode::Esc)), None, "no interrupt");
    assert_eq!(a.tool_cursor, None);
    // Escape again, outside the cursor, interrupts as usual.
    assert_eq!(
        a.on_key(key(KeyCode::Esc)),
        Some(super::app::Action::Interrupt)
    );
    a.busy = false;
    a.on_key(alt_up());
    a.on_key(key(KeyCode::Char('h')));
    assert_eq!(a.tool_cursor, None);
    assert_eq!(a.input.text(), "h", "the key is typed");
    // Other keys leave and do what they do: ctrl+p opens the palette.
    a.on_key(alt_up());
    a.on_key(ctrl('p'));
    assert_eq!(a.tool_cursor, None);
    assert!(matches!(a.modal, Some(Modal::Picker(_))));
}

#[test]
fn ctrl_o_opens_and_closes_every_group_in_and_out_of_the_cursor() {
    let mut a = many(3);
    a.on_key(ctrl('o'));
    assert!(["shell-step 0", "shell-step 1", "shell-step 2"]
        .iter()
        .all(|k| a.group_open(k)));
    a.on_key(alt_up());
    a.on_key(key(KeyCode::Left));
    assert!(!a.group_open("shell-step 2"));
    // Ctrl+O sets them all again, the one closed by hand too.
    a.on_key(ctrl('o'));
    assert!(!a.group_open("shell-step 0") && !a.group_open("shell-step 2"));
    a.on_key(ctrl('o'));
    assert!(a.group_open("shell-step 0") && a.group_open("shell-step 2"));
    assert!(a.tool_cursor.is_some(), "ctrl+o keeps the cursor");
}

#[test]
fn the_view_scrolls_to_follow_the_selection() {
    let mut a = many(30);
    // Each group is 2 rows (its line, then the text), 10 rows shown.
    a.on_key(alt_up());
    assert_eq!(a.scroll, 0, "the latest group is in view");
    for _ in 0..29 {
        a.on_key(key(KeyCode::Up));
        let (rows, total) = view::group_rows(&a, 80);
        let (_, y, _) = rows
            .iter()
            .find(|r| Some(r.0.as_str()) == selected(&a))
            .unwrap()
            .clone();
        let top = total - 10 - a.scroll.min(total - 10);
        assert!(y >= top && y < top + 10, "row {y} out of view at {top}");
    }
    assert_eq!(selected(&a), Some("shell-step 0"));
    assert!(a.scroll > 0);
    // And back down to the newest.
    for _ in 0..29 {
        a.on_key(key(KeyCode::Down));
    }
    let (rows, total) = view::group_rows(&a, 80);
    let y = rows.last().unwrap().1;
    let top = total - 10 - a.scroll;
    assert!(y >= top && y < top + 10);
}

#[test]
fn an_open_group_scrolls_to_show_its_rows() {
    let mut a = many(3);
    a.scroll = 0;
    a.on_key(alt_up());
    a.on_key(key(KeyCode::Up));
    a.on_key(key(KeyCode::Up));
    // Opening the oldest: its rows come into view below its line.
    a.on_key(key(KeyCode::Enter));
    let (rows, total) = view::group_rows(&a, 80);
    let (_, y, h) = rows[0].clone();
    assert_eq!(h, 3, "its line and the call's two rows");
    let top = total.saturating_sub(10 + a.scroll);
    assert!(y >= top && y + h <= top + 10, "{y}+{h} in {top}..");
}

#[test]
fn the_cursor_keys_can_be_rebound() {
    let binds = json!({
        "session.tool.select": "ctrl+alt+t",
        "session.tool.previous": "k",
        "session.tool.next": "j",
        "session.tool.open": "o",
        "session.tool.close": "c",
        "session.tool.exit": "q",
    });
    let k = Keymap::new(binds.as_object().unwrap(), 2000, true);
    assert!(k.warnings.is_empty(), "{:?}", k.warnings);
    let mut a = many(2);
    a.keymap = std::sync::Arc::new(k);
    a.on_key(alt_up());
    assert_eq!(a.tool_cursor, None, "alt+up is no longer bound");
    a.on_key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ));
    assert_eq!(selected(&a), Some("shell-step 1"));
    a.on_key(key(KeyCode::Char('k')));
    assert_eq!(selected(&a), Some("shell-step 0"));
    a.on_key(key(KeyCode::Char('o')));
    assert!(a.group_open("shell-step 0"));
    a.on_key(key(KeyCode::Char('c')));
    assert!(!a.group_open("shell-step 0"));
    a.on_key(key(KeyCode::Char('j')));
    assert_eq!(selected(&a), Some("shell-step 1"));
    // Up is no longer the cursor's: it leaves (and recalls nothing).
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.tool_cursor, None);
    a.on_key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ));
    a.on_key(key(KeyCode::Char('q')));
    assert_eq!(a.tool_cursor, None);
    assert!(a.input.is_empty());
}

#[test]
fn up_in_an_empty_prompt_still_recalls_history() {
    let mut a = many(2);
    a.history = vec!["fix the login".into()];
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.input.text(), "fix the login");
    assert_eq!(a.tool_cursor, None);
}

#[test]
fn the_cursor_leaves_when_its_group_goes() {
    let mut a = many(2);
    a.on_key(alt_up());
    a.on_key(key(KeyCode::Enter));
    // The session read again: the same calls, so the same groups.
    let same = a.entries.clone();
    a.set_entries(same);
    assert_eq!(selected(&a), Some("shell-step 1"));
    assert!(a.group_open("shell-step 1"));
    // Another session: the group is gone, and so is the cursor.
    a.set_entries(vec![Entry::Assistant("new session".into())]);
    assert_eq!(a.tool_cursor, None);
    // A stale key (entries replaced behind its back) leaves on the next move.
    let mut a = many(2);
    a.on_key(alt_up());
    a.entries.clear();
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.tool_cursor, None);
}

// ----- drawn -----

fn screen(a: &App, w: u16, h: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| view::draw(f, a)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
        .collect()
}

#[test]
fn the_selected_group_has_a_bar_and_the_status_line_its_keys() {
    let mut a = many(3);
    let rows = screen(&a, 80, 16);
    assert!(!rows.iter().any(|r| r.contains('▌')));
    assert!(!rows.iter().any(|r| r.contains("TRANSCRIPT")));
    a.on_key(alt_up());
    a.on_key(key(KeyCode::Up));
    let rows = screen(&a, 80, 16);
    let bar: Vec<&String> = rows.iter().filter(|r| r.starts_with('▌')).collect();
    assert_eq!(bar.len(), 1, "{rows:#?}");
    assert!(bar[0].contains("Ran a command ›"), "{}", bar[0]);
    let status = rows.last().unwrap();
    assert!(status.contains("TRANSCRIPT"), "{status}");
    for k in [
        "↑/↓ move",
        "enter/→ open",
        "← close",
        "esc back to input",
        "ctrl+o all",
    ] {
        assert!(status.contains(k), "{k}: {status}");
    }
    // The bar is in the primary colour.
    let mut t = Terminal::new(TestBackend::new(80, 16)).unwrap();
    t.draw(|f| view::draw(f, &a)).unwrap();
    let buf = t.backend().buffer().clone();
    let y = rows.iter().position(|r| r.starts_with('▌')).unwrap() as u16;
    assert_eq!(buf[(0, y)].fg, theme::current().primary);
    // Leaving takes both away.
    a.on_key(key(KeyCode::Esc));
    let rows = screen(&a, 80, 16);
    assert!(!rows
        .iter()
        .any(|r| r.contains('▌') || r.contains("TRANSCRIPT")));
}

#[test]
fn a_selection_scrolled_out_of_view_draws_no_bar() {
    let mut a = many(20);
    a.on_key(alt_up());
    // Scrolled away by hand: the selected line is below the view.
    a.scroll = 30;
    let rows = screen(&a, 80, 14);
    assert!(!rows.iter().any(|r| r.starts_with('▌')), "{rows:#?}");
}

#[test]
fn the_help_lists_the_tool_group_keys() {
    let h = super::app::help_text(&Keymap::default());
    for want in [
        "alt+↑",
        "select the latest tool group",
        "with a tool group selected: open it",
        "with a tool group selected: back to the input",
    ] {
        assert!(h.contains(want), "{want}");
    }
}

// ----- old sessions -----

#[test]
fn a_stored_session_replays_into_groups() {
    use codesmith_ai::{Message, Part, Role};
    let call = |id: &str, name: &str, input: serde_json::Value| Part::ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        meta: None,
    };
    let result = |id: &str, name: &str, content: &str, is_error: bool| Part::ToolResult {
        id: id.into(),
        name: name.into(),
        content: content.into(),
        is_error,
        media: vec![],
    };
    let messages = vec![
        Message::user("Look around"),
        Message {
            role: Role::Assistant,
            parts: vec![
                call("c1", "bash", json!({"command": "ls"})),
                call("c2", "read", json!({"filePath": "a.rs"})),
            ],
        },
        Message {
            role: Role::User,
            parts: vec![
                result("c1", "bash", "a.rs", false),
                result("c2", "read", "fn main() {}", false),
            ],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                Part::Text("Found it.".into()),
                call("c3", "linear_search", json!({"q": "x"})),
            ],
        },
        Message {
            role: Role::User,
            parts: vec![result("c3", "linear_search", "denied", true)],
        },
    ];
    let mut a = App::new();
    a.set_entries(super::app::replay(&messages));
    let groups = view::tool_groups(&a);
    assert_eq!(groups.len(), 2);
    let l = lines(&a);
    assert!(
        l.contains(&"⠶ Ran a command, read a file ›".to_string()),
        "{l:?}"
    );
    assert!(
        l.contains(&format!(
            "{} Called linear_search · 1 failed ›",
            sprite("linear_search").rest
        )),
        "{l:?}"
    );
    // A call the store never got a result for (the session stopped) is
    // not live once the session is idle.
    let mut b = App::new();
    b.set_entries(super::app::replay(&messages[..2]));
    assert_eq!(lines(&b)[3], "⠶ Ran a command, read a file ›");
}

// ----- negative, edge and stress -----

#[test]
fn a_tool_name_with_control_characters_is_shown_escaped() {
    let mut a = App::new();
    a.entries = vec![ok("evil\u{1b}]0;pwn\u{7}\nname", "x")];
    let l = &lines(&a)[0];
    assert!(
        !l.contains('\u{1b}') && !l.contains('\u{7}') && !l.contains('\n'),
        "{l:?}"
    );
    assert!(l.contains("Called evil\\x1b]0;pwn\\x07 name"), "{l:?}");
}

#[test]
fn group_lines_fit_any_width_and_never_panic() {
    let mut a = turn();
    a.busy = true;
    a.entries.push(run(
        "an_extremely_long_mcp_tool_name_that_goes_on_and_on",
        "x",
    ));
    for w in 0..120u16 {
        for l in view::transcript(&a, w) {
            let _ = text_of(&l);
        }
        let _ = screen(&a, w.max(1), 12);
    }
    // At a narrow width the line keeps its glyph, failures and chevron.
    let mut b = App::new();
    b.entries = vec![bad("shell", "a", "x")];
    let l = text_of(&view::transcript(&b, 24)[0]);
    assert!(l.starts_with('▞') && l.ends_with(" · 1 failed ›"), "{l}");
    assert!(l.width() <= 20, "{l}");
}

#[test]
fn many_groups_stay_cheap_to_find_and_move_through() {
    let mut a = many(2000);
    let start = Instant::now();
    assert_eq!(view::tool_groups(&a).len(), 2000);
    a.on_key(alt_up());
    for _ in 0..50 {
        a.on_key(key(KeyCode::Up));
    }
    assert_eq!(selected(&a), Some("shell-step 1949"));
    // Generous: catches a quadratic walk, not a slow machine.
    assert!(start.elapsed().as_secs() < 30, "{:?}", start.elapsed());
}

#[test]
fn group_keys_are_unique_for_distinct_first_calls() {
    let a = many(100);
    let keys: std::collections::HashSet<String> =
        view::tool_groups(&a).into_iter().map(|g| g.key).collect();
    assert_eq!(keys.len(), 100);
}
