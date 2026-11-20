//! Timing benchmarks for large repositories and long sessions.
//!
//! `cargo bench -p codesmith-core --bench perf` builds a synthetic tree
//! (`CODESMITH_PERF_FILES` files, default 20000) in a temporary directory, or
//! uses an existing one named by `CODESMITH_PERF_TREE`. Pass a name filter as
//! the first argument to run only matching benchmarks. Each benchmark prints
//! the minimum and median of its runs.

use codesmith_ai::{Message, Part, Role, Usage};
use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{base_rules, Effect, Fixed, Gate, Reply, Rule};
use codesmith_core::snapshot::Snapshots;
use codesmith_core::store::{Info, Record, Store};
use codesmith_core::tool::{self, Ctx};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn report(name: &str, mut times: Vec<Duration>, note: &str) {
    times.sort();
    let min = times[0];
    let med = times[times.len() / 2];
    println!(
        "{name:<34} min {:>10.2} ms   median {:>10.2} ms   ({} runs){}",
        min.as_secs_f64() * 1e3,
        med.as_secs_f64() * 1e3,
        times.len(),
        if note.is_empty() {
            String::new()
        } else {
            format!("   {note}")
        }
    );
}

/// Runs per benchmark: `runs`, capped by `CODESMITH_PERF_RUNS` when set.
fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> (Vec<Duration>, T) {
    let runs = std::env::var("CODESMITH_PERF_RUNS")
        .ok()
        .and_then(|r| r.parse().ok())
        .map_or(runs, |cap: usize| runs.min(cap.max(1)));
    let mut times = Vec::new();
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        let v = f();
        times.push(t.elapsed());
        last = Some(v);
    }
    (times, last.unwrap())
}

const WORDS: [&str; 12] = [
    "alpha", "beta", "gamma", "delta", "render", "parse", "token", "buffer", "index", "cache",
    "sigma", "omega",
];

/// A nested tree of `n` small files, an ignored build directory and a
/// `.gitignore`, committed to git so snapshots can borrow its objects.
fn make_tree(root: &Path, n: usize) {
    std::fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
    let mut seed: u64 = 7;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for i in 0..n {
        let d = root.join(format!(
            "pkg{:02}/mod{:02}/sub{:03}",
            i % 20,
            (i / 20) % 25,
            i / 5000
        ));
        std::fs::create_dir_all(&d).unwrap();
        let ext = [".rs", ".ts", ".py", ".md", ".txt"][(rnd() % 5) as usize];
        let mut body = String::new();
        for _ in 0..(5 + rnd() % 40) {
            for _ in 0..8 {
                body.push_str(WORDS[(rnd() % 12) as usize]);
                body.push(' ');
            }
            body.push('\n');
        }
        if rnd() % 100 == 0 {
            body.push_str("needle_marker found here\n");
        }
        std::fs::write(d.join(format!("f{i}{ext}")), body).unwrap();
    }
    let b = root.join("build/out");
    std::fs::create_dir_all(&b).unwrap();
    for i in 0..2000 {
        std::fs::write(b.join(format!("o{i}.o")), "x\n").unwrap();
    }
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.email=b@b",
        "-c",
        "user.name=b",
        "commit",
        "-qm",
        "init",
    ]);
}

/// Rules a configured project might have: a few path denies and asks.
fn rich_rules() -> Vec<Rule> {
    let mut r = base_rules();
    for (res, eff) in [
        ("secrets/*", Effect::Deny),
        ("*.pem", Effect::Deny),
        ("*.key", Effect::Ask),
        ("pkg19/mod24/*", Effect::Ask),
        ("**/private/**", Effect::Deny),
        ("*/.ssh/*", Effect::Deny),
        ("vendor/*", Effect::Allow),
        ("*.lock", Effect::Allow),
    ] {
        r.push(Rule::new("read", res, eff));
    }
    for res in ["*.rs", "src/*", "docs/*", "*.md"] {
        r.push(Rule::new("edit", res, Effect::Ask));
    }
    r
}

fn ctx<'a>(root: &Path, gate: &'a Gate) -> Ctx<'a> {
    Ctx {
        cwd: root.to_path_buf(),
        root: root.to_path_buf(),
        gate,
        shell: "/bin/sh".into(),
        session_id: "bench".into(),
        limits: ToolOutputConfig::default(),
        client: reqwest::Client::new(),
        jobs: None,
        websearch: None,
        format: None,
        lsp: None,
        progress: None,
    }
}

fn gate(rules: Vec<Rule>) -> Gate {
    Gate::new(rules, Arc::new(Fixed(Reply::Once)))
}

fn run_tool(rt: &tokio::runtime::Runtime, c: &Ctx, name: &str, input: serde_json::Value) -> String {
    let r = rt.block_on(tool::run(c, name, &input));
    assert!(!r.is_error, "{name}: {}", r.content);
    r.content
}

fn bench_tools(root: &Path, rt: &tokio::runtime::Runtime, filter: &str) {
    for (label, rules) in [("base", base_rules()), ("rules", rich_rules())] {
        let g = gate(rules);
        let c = ctx(root, &g);
        let name = format!("grep tree ({label})");
        if name.contains(filter) {
            let (t, out) = time(5, || {
                run_tool(
                    rt,
                    &c,
                    "grep",
                    json!({"pattern": "needle_marker", "limit": 100000}),
                )
            });
            report(&name, t, out.lines().next().unwrap_or(""));
        }
        let name = format!("glob **/*.rs ({label})");
        if name.contains(filter) {
            let (t, _) = time(5, || {
                run_tool(rt, &c, "glob", json!({"pattern": "**/*.rs", "limit": 100}))
            });
            report(&name, t, "");
        }
    }
    let files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .take(10_000)
        .map(|e| e.into_path())
        .collect();
    let g = gate(rich_rules());
    let c = ctx(root, &g);
    if "resolve 10k paths".contains(filter) {
        let (t, _) = time(5, || {
            for f in &files {
                let t = c.resolve(&f.display().to_string());
                std::hint::black_box(c.read_effect(&t.resources));
            }
        });
        report("resolve 10k paths", t, "resolve + read rules");
    }
    if "read+edit 1k files".contains(filter) {
        let scratch = tempfile::tempdir_in(root).unwrap();
        let mut made = Vec::new();
        for i in 0..1000 {
            let p = scratch.path().join(format!("e{i}.txt"));
            std::fs::write(&p, "hello world\n").unwrap();
            made.push(p);
        }
        let (t, _) = time(3, || {
            for p in &made {
                let p = p.display().to_string();
                run_tool(rt, &c, "read", json!({"path": p}));
                run_tool(
                    rt,
                    &c,
                    "edit",
                    json!({"path": p, "oldString": "hello", "newString": "hallo"}),
                );
                run_tool(
                    rt,
                    &c,
                    "edit",
                    json!({"path": p, "oldString": "hallo", "newString": "hello"}),
                );
            }
        });
        report("read+edit 1k files", t, "1 read + 2 edits each");
    }
}

fn bench_snapshot(root: &Path, base: &Path, filter: &str) {
    if !"snapshot".contains(filter) && !filter.contains("snapshot") {
        return;
    }
    let s = Snapshots::new(root, base).unwrap();
    let (t, _) = time(1, || s.capture().unwrap());
    report("snapshot first capture", t, "");
    let (t, _) = time(5, || s.capture().unwrap());
    report("snapshot capture (unchanged)", t, "");
    let touched = ignore::WalkBuilder::new(root)
        .build()
        .flatten()
        .find(|e| e.file_type().is_some_and(|t| t.is_file()))
        .expect("a file to edit")
        .into_path();
    let orig = std::fs::read(&touched).unwrap();
    let mut i = 0;
    let (t, _) = time(5, || {
        i += 1;
        let mut edited = orig.clone();
        edited.extend_from_slice(format!("\n// edit {i}\n").as_bytes());
        std::fs::write(&touched, edited).unwrap();
        s.capture().unwrap()
    });
    std::fs::write(&touched, orig).unwrap();
    report("snapshot capture (1 file edited)", t, "");
}

fn tool_message(i: usize) -> Vec<Message> {
    let body: String = (0..40)
        .map(|l| format!("line {l} of output {i}\n"))
        .collect();
    vec![
        Message {
            role: Role::Assistant,
            parts: vec![
                Part::text(format!("Looking at file {i}.")),
                Part::ToolCall {
                    id: format!("c{i}"),
                    name: "read".into(),
                    input: json!({"filePath": format!("src/f{i}.rs")}),
                    meta: None,
                },
            ],
        },
        Message {
            role: Role::User,
            parts: vec![Part::ToolResult {
                id: format!("c{i}"),
                name: "read".into(),
                content: body,
                is_error: false,
                media: vec![],
            }],
        },
    ]
}

fn info(id: &str, t: i64) -> Info {
    Info {
        id: id.into(),
        directory: "/work/proj".into(),
        title: None,
        agent: "build".into(),
        model: "anthropic/claude".into(),
        parent_id: None,
        created: t,
        updated: t,
        usage: Usage::default(),
    }
}

/// Append a session of about `records` records: prompts, tool exchanges,
/// steps and tool timings.
fn fill_session(db: &Store, id: &str, records: usize, t0: i64) {
    db.create(&info(id, t0)).unwrap();
    db.with_conn(|c| Ok(c.execute_batch("BEGIN")?)).unwrap();
    let mut n = 0;
    let mut i = 0;
    while n < records {
        let t = t0 + (n as i64) * 60_000;
        if i % 10 == 0 {
            db.append_at(
                id,
                &Record::Message {
                    message: Message::user(format!("please fix bug {i}")),
                },
                t,
            )
            .unwrap();
            n += 1;
        }
        for m in tool_message(i) {
            db.append_at(id, &Record::Message { message: m }, t)
                .unwrap();
        }
        db.append_at(
            id,
            &Record::Step {
                model: "anthropic/claude".into(),
                usage: Usage {
                    input: 1000,
                    output: 100,
                    cache_read: 500,
                    cache_write: 0,
                    ..Default::default()
                },
            },
            t,
        )
        .unwrap();
        db.append_at(
            id,
            &Record::Tool {
                name: "read".into(),
                ms: (i % 97) as u64,
                error: i % 13 == 0,
            },
            t,
        )
        .unwrap();
        n += 4;
        i += 1;
    }
    db.with_conn(|c| Ok(c.execute_batch("COMMIT")?)).unwrap();
}

fn bench_store(dir: &Path, filter: &str) {
    if "store".contains(filter) || filter.contains("store") || filter.contains("history") {
        let db = Store::open(&dir.join("one.db")).unwrap();
        fill_session(&db, "s", 5000, 1_700_000_000_000);
        let (t, n) = time(5, || db.entries("s").unwrap().len());
        report("store entries (5k records)", t, &format!("{n} entries"));
        let (t, n) = time(5, || db.history("s").unwrap().len());
        report("store history (5k records)", t, &format!("{n} messages"));
        let (t, _) = time(5, || {
            let e = db.entries("s").unwrap();
            std::hint::black_box(codesmith_core::store::undo_target(&e));
            std::hint::black_box(codesmith_core::store::history_with_seqs(&e));
        });
        report("store resume (entries+undo)", t, "");
        let mut i = 0;
        let (t, _) = time(200, || {
            i += 1;
            db.append(
                "s",
                &Record::Message {
                    message: Message::user(format!("more {i}")),
                },
            )
            .unwrap()
        });
        report("store append (one record)", t, "");
        let h = db.history("s").unwrap();
        let (t, v) = time(20, || codesmith_core::compaction::estimate(&h));
        report(
            "estimate history",
            t,
            &format!("{} messages, {v} tokens", h.len()),
        );
    }
    if "stats".contains(filter) || filter.contains("stats") {
        let db = Store::open(&dir.join("many.db")).unwrap();
        for s in 0..500 {
            fill_session(
                &db,
                &format!("s{s:03}"),
                100,
                1_700_000_000_000 + s * 3_600_000,
            );
        }
        let q = codesmith_core::stats::Query {
            from: None,
            to: i64::MAX,
            directory: None,
        };
        let (t, s) = time(3, || {
            codesmith_core::stats::compute(&db, &q, &|_| None).unwrap()
        });
        report(
            "stats (500 sessions x 100)",
            t,
            &format!("{} prompts, {} steps", s.prompts, s.steps),
        );
    }
}

fn main() {
    let filter = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .unwrap_or_default();
    let scratch = tempfile::tempdir().unwrap();
    // Keep the session's private dirs out of the user's data directory.
    std::env::set_var("XDG_DATA_HOME", scratch.path().join("data"));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let needs_tree = ["grep", "glob", "resolve", "read", "snapshot"]
        .iter()
        .any(|k| k.contains(filter.as_str()) || filter.contains(k));
    if needs_tree {
        let root = tree(scratch.path());
        bench_tools(&root, &rt, &filter);
        bench_snapshot(&root, &scratch.path().join("snap"), &filter);
    }
    bench_store(scratch.path(), &filter);
}

/// The tree to search: `CODESMITH_PERF_TREE`, or a new one under `scratch`.
fn tree(scratch: &Path) -> PathBuf {
    let root = match std::env::var_os("CODESMITH_PERF_TREE") {
        Some(t) => std::fs::canonicalize(t).unwrap(),
        None => {
            let n = std::env::var("CODESMITH_PERF_FILES")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(20_000);
            let r = scratch.canonicalize().unwrap().join("tree");
            std::fs::create_dir_all(&r).unwrap();
            let t = Instant::now();
            make_tree(&r, n);
            println!("built a {n}-file tree in {:.1}s", t.elapsed().as_secs_f64());
            r
        }
    };
    println!("tree: {}", root.display());
    root
}
