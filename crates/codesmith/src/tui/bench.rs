//! Timing benchmarks for the TUI, run with
//! `cargo test --release -p codesmith tui::bench -- --ignored --nocapture`.
//! `CODESMITH_PERF_TREE` names a large tree for the `@` completion index.

use super::app::{App, Entry, Tool};
use super::{complete, view};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::time::{Duration, Instant};

fn report(name: &str, mut times: Vec<Duration>, note: &str) {
    times.sort();
    println!(
        "{name:<34} min {:>10.2} ms   median {:>10.2} ms   ({} runs)   {note}",
        times[0].as_secs_f64() * 1e3,
        times[times.len() / 2].as_secs_f64() * 1e3,
        times.len()
    );
}

fn time(runs: usize, mut f: impl FnMut()) -> Vec<Duration> {
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect()
}

/// A session of `n` transcript entries: prompts, markdown answers (half of
/// all entries) and tools.
fn long_session(n: usize) -> App {
    let mut a = App::new();
    for i in 0..n {
        a.entries.push(match i % 4 {
            0 => Entry::User(format!("Please look at module {i} and fix the failing test in it; the error mentions a missing field.")),
            1 => Entry::Assistant(format!(
                "## Step {i}\n\nI found the problem in `src/mod{i}.rs`: the **builder** skips a field.\n\n- first point about it\n- second point, longer, so that it wraps across the line at narrower widths\n\n```rust\nfn f{i}() -> u32 {{\n    {i}\n}}\n```\n"
            )),
            2 => Entry::Tool(Tool {
                id: format!("t{i}"),
                name: "shell".into(),
                detail: "cargo test".into(),
                result: Some(("running 3 tests".into(), false)),
                output: (0..30).map(|l| format!("test line {l}\n")).collect(),
                ..Default::default()
            }),
            _ => Entry::Assistant(format!("Done with {i}; the test passes now.")),
        });
    }
    a
}

#[test]
#[ignore]
fn bench_transcript() {
    let mut a = long_session(2000);
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    // The first frame renders every message's markdown.
    let t = time(1, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui first frame (2k entries)", t, "");
    let t = time(50, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame (2k entries)", t, "");
    a.verbosity = super::group::Verbosity::Medium;
    let t = time(50, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame at medium verbosity", t, "");
    a.verbosity = super::group::Verbosity::Low;
    a.scroll = 500;
    let t = time(50, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame scrolled 500 lines", t, "");
    a.scroll = 0;
    // Eight session tabs: the strip above the transcript, then the column.
    let strip = |vertical: bool| super::tabs::Strip {
        items: (0..8)
            .map(|i| super::tabs::Item {
                title: format!("Session number {i} with a long title"),
                status: super::tabs::Status {
                    busy: i % 3 == 0,
                    ..Default::default()
                },
                new: false,
            })
            .collect(),
        active: Some(3),
        vertical,
        detail: "codesmith ⎇ par/tui-tabs".into(),
        ..Default::default()
    };
    a.tabs = Some(strip(false));
    let t = time(50, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame with 8 tabs", t, "");
    a.tabs = Some(strip(true));
    let mut wide = Terminal::new(TestBackend::new(160, 40)).unwrap();
    wide.draw(|f| view::draw(f, &a)).unwrap();
    let t = time(50, || {
        wide.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame with 8 vertical tabs", t, "160 columns");
    a.tabs = None;
    // Streaming: the last answer grows every frame.
    a.entries.push(Entry::Assistant(String::new()));
    let t = time(50, || {
        if let Some(Entry::Assistant(s)) = a.entries.last_mut() {
            s.push_str("more streamed text ");
        }
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui streaming frame (2k entries)", t, "");
    // A running command's output grows every frame (its row keeps the
    // latest 64 KiB, of which it shows a tail).
    a.entries.push(Entry::Tool(Tool {
        id: "live".into(),
        name: "shell".into(),
        detail: "cargo build".into(),
        ..Default::default()
    }));
    let mut n = 0;
    let t = time(50, || {
        let chunk: String = (0..40)
            .map(|_| {
                n += 1;
                format!("\x1b[32m   Compiling\x1b[0m crate{n} v0.1.0 (/src/crate{n})\n")
            })
            .collect();
        a.on_event(super::bridge::Ev::ToolOutput {
            id: "live".into(),
            delta: chunk,
        });
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui live output frame (2k entries)", t, "");
    // A subagent runs: every frame it starts and ends a call and writes.
    use super::bridge::Ev;
    a.on_event(Ev::ToolStart {
        id: "task".into(),
        name: "subagent".into(),
        detail: "explore: survey".into(),
        diff: None,
    });
    a.on_event(Ev::ChildStart {
        id: "ses_child".into(),
        title: "Survey the crate".into(),
        agent: "explore".into(),
    });
    let mut n = 0;
    let t = time(50, || {
        n += 1;
        a.on_event(Ev::ToolStart {
            id: format!("c{n}"),
            name: "explore › read".into(),
            detail: format!("src/mod{n}.rs"),
            diff: None,
        });
        a.on_event(Ev::ToolEnd {
            id: format!("c{n}"),
            name: "explore › read".into(),
            output: format!("Read file src/mod{n}.rs, lines 1-200"),
            error: false,
        });
        a.on_event(Ev::ChildText(format!("Module {n} wires the builder. ")));
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui subagent frame (2k entries)", t, "");
    a.expand_tools = true;
    let t = time(50, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui subagent frame expanded", t, "");
    a.expand_tools = false;
    let mut w = 80;
    let t = time(10, || {
        w = if w == 80 { 100 } else { 80 };
        let mut term = Terminal::new(TestBackend::new(w, 40)).unwrap();
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame after resize", t, "");
}

/// Turns of a prompt, thoughts and a long run of reads, searches and
/// commands, then an answer: low verbosity sums each run up on one row.
#[test]
#[ignore]
fn bench_grouped_transcript() {
    let mut a = App::new();
    for turn in 0..100 {
        a.entries
            .push(Entry::User(format!("Turn {turn}: fix the build")));
        a.entries.push(Entry::Reasoning(format!(
            "**Looking at turn {turn}**\n\nThe build fails in a module."
        )));
        for i in 0..20 {
            let name = ["read", "grep", "shell", "read", "edit"][i % 5];
            a.entries.push(Entry::Tool(Tool {
                id: format!("t{turn}-{i}"),
                name: name.into(),
                detail: format!("src/mod{i}.rs"),
                result: Some(("ok".into(), false)),
                output: (0..10).map(|l| format!("line {l}\n")).collect(),
                ..Default::default()
            }));
        }
        a.entries.push(Entry::Assistant(format!(
            "Fixed turn {turn}; the build passes."
        )));
    }
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    term.draw(|f| view::draw(f, &a)).unwrap();
    for v in super::group::Verbosity::ALL {
        a.verbosity = v;
        let t = time(50, || {
            term.draw(|f| view::draw(f, &a)).unwrap();
        });
        report(
            &format!("tui grouped frame ({})", v.key()),
            t,
            "2.3k entries",
        );
    }
    a.verbosity = super::group::Verbosity::Low;
    a.expand_tools = true;
    let t = time(50, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui grouped frame (low, expanded)", t, "");
}

#[test]
#[ignore]
fn bench_completion() {
    let root = std::env::var_os("CODESMITH_PERF_TREE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let mut files = Vec::new();
    let t = time(3, || files = complete::project_files(&root, 20_000));
    report(
        "@ index build (20k cap)",
        t,
        &format!("{} entries", files.len()),
    );
    let cmds = vec![];
    for q in ["m", "pkg1", "sub0f3rs", "zzzz"] {
        let text = format!("look at @{q}");
        let t = time(20, || {
            std::hint::black_box(complete::popup(&text, text.len(), &cmds, &[], &files));
        });
        report(&format!("@ query {q:?}"), t, "");
    }
}

/// The diff viewer on 200 changed files and a 7500-line new one: frames
/// while scrolling, in split and unified view, docked beside a long
/// session and full screen.
#[test]
#[ignore]
fn bench_diff_viewer() {
    use super::diff_source::{FileDiff, Source, Status};
    use super::diff_view::{Prefs, Response, Viewer};
    let patch: String = (0..3)
        .map(|h| {
            let at = h * 40 + 1;
            format!(
                "@@ -{at},5 +{at},5 @@\n fn a{h}() {{\n-    let x = {h};\n+    let x = {h} + 1;\n     x\n }}\n // end\n"
            )
        })
        .collect();
    let mut files: Vec<FileDiff> = (0..200)
        .map(|i| {
            FileDiff::from_patch(
                &format!("src/m{}/f{i}.rs", i % 12),
                Status::Modified,
                &patch,
            )
        })
        .collect();
    let big: String = (0..7500)
        .map(|i| format!("+    let value_{i} = compute({i}); // row {i}\n"))
        .collect();
    files.push(FileDiff::from_patch(
        "src/big.rs",
        Status::Added,
        &format!("@@ -0,0 +1,7500 @@\n{big}"),
    ));
    let mut a = long_session(2000);
    let open = |a: &mut App, fullscreen: bool| {
        let prefs = Prefs {
            fullscreen,
            ..Prefs::default()
        };
        let mut v = Viewer::new("/repo".into(), Some("main"), Source::Working, prefs);
        v.reload(&a.diff_memory);
        let seq = v.requests.last().unwrap().seq;
        v.answer(Response::Diff {
            seq,
            lookup: None,
            base: None,
            files: Ok(Some(files.clone())),
        });
        a.diff = Some(v);
    };
    open(&mut a, true);
    let mut term = Terminal::new(TestBackend::new(160, 40)).unwrap();
    let t = time(1, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report(
        "diff first frame (201 files)",
        t,
        "lays out and colors the rows in view",
    );
    let t = time(100, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("diff frame (split, tree)", t, "");
    let mut n = 0;
    let t = time(100, || {
        n += 7;
        a.diff.as_mut().unwrap().scroll = n;
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report(
        "diff frame while scrolling",
        t,
        "new rows colored each frame",
    );
    let v = a.diff.as_mut().unwrap();
    v.view_override = Some(super::diff_view::View::Unified);
    let t = time(100, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("diff frame (unified, relaid once)", t, "");
    // Deep in the big file: its colors are worked out down to there once.
    let g = a.diff.as_ref().unwrap().now();
    let end = a.diff.as_ref().unwrap().max_scroll(&g);
    a.diff.as_mut().unwrap().scroll = end;
    let t = time(1, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report(
        "diff jump to the end of 7.5k lines",
        t,
        "colors the whole file once",
    );
    let t = time(100, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("diff frame at the end", t, "");
    open(&mut a, false);
    term.draw(|f| view::draw(f, &a)).unwrap();
    let t = time(100, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("diff docked beside 2k entries", t, "both panes");
}

/// The open menu over a short session: 50 recent sessions in 10 projects,
/// with and without a filter typed, and the worktrees of one.
#[test]
#[ignore]
fn bench_open_menu() {
    use super::browser::{Browser, Context, Response, SessionRow};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;
    let mut a = long_session(40);
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let t = time(200, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame, no menu (40 entries)", t, "");
    let now = codesmith_core::store::now();
    let sessions: Vec<SessionRow> = (0..50)
        .map(|i| {
            let root = PathBuf::from(format!("/work/project-{}", i % 10));
            SessionRow {
                id: format!("ses_{i:017x}"),
                title: Some(format!("Fix the failing test in module {i}")),
                dir: root.join(format!("packages/pkg{i}")),
                root: Some(root),
                updated: now - i * 60_000,
                parent: None,
            }
        })
        .collect();
    let ctx = Context {
        cwd: PathBuf::from("/work/project-0"),
        project: Some(PathBuf::from("/work/project-0")),
        ..Default::default()
    };
    let mut b = Browser::open(ctx, Vec::new());
    b.answer(Response::Recent(Ok(sessions)));
    a.browser = Some(Box::new(b));
    let t = time(200, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame, open menu", t, "50 sessions, 10 projects");
    let km = a.keymap.clone();
    if let Some(b) = a.browser.as_mut() {
        for c in "module 4".chars() {
            b.key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &km);
        }
    }
    let t = time(200, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame, open menu filtered", t, "\"module 4\"");
    let root = PathBuf::from("/work/project-0");
    let worktrees: Vec<PathBuf> = std::iter::once(root.clone())
        .chain((0..20).map(|i| PathBuf::from(format!("/work/project-0-wt{i}"))))
        .collect();
    let ctx = Context {
        cwd: root.clone(),
        project: Some(root.clone()),
        session: Some("ses_1".into()),
        ..Default::default()
    };
    let mut b = Browser::worktrees(ctx).unwrap();
    b.answer(Response::Worktrees(root, Ok(worktrees)));
    a.browser = Some(Box::new(b));
    let t = time(200, || {
        term.draw(|f| view::draw(f, &a)).unwrap();
    });
    report("tui frame, worktrees dialog", t, "21 worktrees");
}
