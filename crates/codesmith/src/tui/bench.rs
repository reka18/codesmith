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
            std::hint::black_box(complete::popup(&text, text.len(), &cmds, &files));
        });
        report(&format!("@ query {q:?}"), t, "");
    }
}
