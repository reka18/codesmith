//! Timing benchmarks for large repositories and long sessions.
//!
//! `cargo bench -p codesmith-core --bench perf` builds a synthetic tree
//! (`CODESMITH_PERF_FILES` files, default 20000) in a temporary directory, or
//! uses an existing one named by `CODESMITH_PERF_TREE`. Pass a name filter as
//! the first argument to run only matching benchmarks. Each benchmark prints
//! the minimum and median of its runs.

use codesmith_ai::{Message, Part, Role, Usage};
use codesmith_core::approvals::Book;
use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{
    base_rules, Effect, Fixed, Gate, Policy, PolicyEffect, Reply, Rule,
};
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
    // As a session runs it: "always" answers kept in the store beside it.
    let name = "grep tree (saved approvals)";
    if name.contains(filter) {
        let book = tempfile::tempdir().unwrap();
        let mut g = gate(rich_rules());
        g.set_durable(
            Arc::new(Book::open(&book.path().join("sessions.db")).unwrap()),
            "bench".into(),
        );
        let c = ctx(root, &g);
        let (t, out) = time(5, || {
            run_tool(
                rt,
                &c,
                "grep",
                json!({"pattern": "needle_marker", "limit": 100000}),
            )
        });
        report(name, t, out.lines().next().unwrap_or(""));
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
/// steps and tool timings. (Each append is its own transaction.)
fn fill_session(db: &Store, id: &str, records: usize, t0: i64) {
    db.create(&info(id, t0)).unwrap();
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
    let needs_tree = ["grep", "glob", "resolve", "read", "snapshot", "watch"]
        .iter()
        .any(|k| k.contains(filter.as_str()) || filter.contains(k));
    if needs_tree {
        let root = tree(scratch.path());
        bench_tools(&root, &rt, &filter);
        bench_snapshot(&root, &scratch.path().join("snap"), &filter);
        bench_watch(&root, &rt, &filter);
    }
    bench_store(scratch.path(), &filter);
    bench_calls(scratch.path(), &rt, &filter);
    if "turn".contains(filter.as_str()) || filter.contains("turn") {
        bench_turn(&rt);
    }
}

/// Runs of `f` taking at least about 50 ms, to time calls of microseconds.
fn per_call(runs: usize, mut f: impl FnMut()) -> Vec<Duration> {
    let t = Instant::now();
    f();
    let one = t.elapsed().max(Duration::from_nanos(50));
    let n = (Duration::from_millis(50).as_nanos() / one.as_nanos()).clamp(1, 100_000) as u32;
    time(runs, || {
        for _ in 0..n {
            f();
        }
    })
    .0
    .into_iter()
    .map(|t| t / n)
    .collect()
}

/// Like [`report`], in microseconds per call.
fn report_us(name: &str, mut times: Vec<Duration>, note: &str) {
    times.sort();
    println!(
        "{name:<34} min {:>10.2} us   median {:>10.2} us   ({} runs){}",
        times[0].as_secs_f64() * 1e6,
        times[times.len() / 2].as_secs_f64() * 1e6,
        times.len(),
        if note.is_empty() {
            String::new()
        } else {
            format!("   {note}")
        }
    );
}

/// What each tool call pays before it runs: the shell scan, the input
/// schema check, permission rules and webfetch's URL checks.
fn bench_calls(dir: &Path, rt: &tokio::runtime::Runtime, filter: &str) {
    use codesmith_core::tool::input_schema::Cache;
    use codesmith_core::tool::shell_scan;
    let want = |name: &str| name.contains(filter) || filter.contains("call");
    let cwd = Path::new("/project");
    let typical = "git status && cargo test -p codesmith-core -- --nocapture 2>&1 | tail -n 50";
    if want("shell scan (typical)") {
        let t = per_call(10, || {
            std::hint::black_box(shell_scan::analyze(typical, cwd).unwrap());
        });
        report_us("shell scan (typical)", t, "");
    }
    // Twenty stages with precommands, printf formats, arithmetic,
    // substitutions and redirects.
    let long: String = (0..20)
        .map(|i| match i % 5 {
            0 => format!("env FOO={i} sudo -u nobody grep -rn \"pat{i}\" src/m{i}"),
            1 => format!("printf '%s:%d\\n' \"$(basename f{i})\" $((i + {i}))"),
            2 => format!("awk '{{print $1, $2}}' > out{i}.txt"),
            3 => format!("sed -e 's/a{i}/b/g' -e \"s/$X/y/\" 2>/dev/null"),
            _ => "xargs -n1 command time nice -n 5 wc -l".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" | ");
    if want("shell scan (20-stage pipeline)") {
        shell_scan::analyze(&long, cwd).unwrap();
        let t = per_call(10, || {
            std::hint::black_box(shell_scan::analyze(&long, cwd).unwrap());
        });
        report_us(
            "shell scan (20-stage pipeline)",
            t,
            &format!("{} bytes", long.len()),
        );
    }
    let edit = codesmith_core::tool::Tool::Edit.spec("/bin/sh").parameters;
    let input = json!({
        "path": "/project/src/lib.rs",
        "oldString": "fn main() {\n    println!(\"hello\");\n}\n",
        "newString": "fn main() {\n    println!(\"hello, world\");\n}\n",
        "replaceAll": false
    });
    if want("schema check (cached)") {
        let cache = Cache::default();
        let t = per_call(10, || {
            std::hint::black_box(cache.check("edit", &edit, &input).unwrap());
        });
        report_us("schema check (cached)", t, "edit's schema");
    }
    if want("schema check (cold)") {
        let t = per_call(10, || {
            let cache = Cache::default();
            std::hint::black_box(cache.check("edit", &edit, &input).unwrap());
        });
        report_us("schema check (cold)", t, "compiles it too");
    }
    // A configured session: base and project rules, rules the session
    // was given, saved approvals and policy statements.
    let mut rules = rich_rules();
    for i in 0..10 {
        rules.push(Rule::new(
            "shell",
            &format!("npm run task{i} *"),
            Effect::Allow,
        ));
    }
    rules.push(Rule::new("shell", "git *", Effect::Allow));
    rules.push(Rule::new("shell", "rm -rf *", Effect::Deny));
    for (e, r) in [
        (PolicyEffect::Deny, "shell:curl *"),
        (PolicyEffect::Deny, "webfetch:https://internal.example/*"),
        (PolicyEffect::Allow, "shell:curl https://ok.example/*"),
        (PolicyEffect::Deny, "edit:*.env"),
    ] {
        rules.extend(Policy::tool(e, r).rule());
    }
    let book = Arc::new(Book::open(&dir.join("approvals.db")).unwrap());
    book.add("bench", "shell", &["make *".into(), "cargo build *".into()])
        .unwrap();
    for (label, durable) in [("", false), (", saved approvals", true)] {
        let mut g = gate(rules.clone());
        if durable {
            g.set_durable(book.clone(), "bench".into());
        }
        let name = format!("permission check{label}");
        if want(&name) {
            let shell = vec!["git status".to_string(), "cargo test -p x".to_string()];
            let t = per_call(10, || {
                std::hint::black_box(g.allows("shell", &shell));
            });
            report_us(&name, t, "2 commands, 40 rules + 4 policies");
        }
    }
    if want("webfetch html to markdown") {
        // A documentation page: headings, prose, quotes and 40 code blocks.
        let page: String = (0..40)
            .map(|i| {
                format!(
                    "<h2>Section {i}</h2><p>Some <b>prose</b> about <a href=\"/x{i}\">item {i}</a>,   \n</p>\
                     <blockquote><p>A quoted note {i}.</p></blockquote>\
                     <pre><code class=\"language-rust\">fn f{i}() -> u32 {{\n    {i}\n}}\n</code></pre>\
                     <ul><li>one</li><li>two</li></ul>\n\n\n"
                )
            })
            .collect();
        let page = format!("<html><body>{page}</body></html>");
        let t = per_call(10, || {
            std::hint::black_box(codesmith_core::tool::html_markdown::to_markdown(&page));
        });
        report_us(
            "webfetch html to markdown",
            t,
            &format!("{} KB, 40 code blocks", page.len() / 1024),
        );
    }
    if want("webfetch authorize") {
        let g = gate(rules.clone());
        let c = ctx(dir, &g);
        let t = rt.block_on(async {
            per_call(10, || {
                std::hint::black_box(
                    tool::webfetch_authorize(
                        &c,
                        "HTTPS://Docs.Example.com:443/guide/%7Euser/a%20b?q=1#frag",
                    )
                    .unwrap(),
                );
            })
        });
        report_us("webfetch authorize", t, "normalize, deny forms, ask");
    }
}

/// One full turn through the scripted model on loopback: a prompt, a
/// `read` call and the answer after it, with the store on.
fn bench_turn(rt: &tokio::runtime::Runtime) {
    rt.block_on(async {
        let mut t = scripted::start(scripted::Setup {
            store: true,
            ..Default::default()
        })
        .await;
        std::fs::write(t.path("a.txt"), "hello\n".repeat(200)).unwrap();
        let p = t.path("a.txt").display().to_string();
        let mut times = Vec::new();
        for i in 0..20 {
            t.push([
                scripted::call(&format!("c{i}"), "read", json!({"filePath": p})),
                scripted::text("Read it."),
            ]);
            let start = Instant::now();
            t.prompt("Read a.txt").await.unwrap();
            times.push(start.elapsed());
        }
        report(
            "turn: prompt, read, answer",
            times,
            &format!("{} messages at the end", t.s.messages.len()),
        );
    });
}

#[path = "../tests/scripted/mod.rs"]
#[allow(unused_imports)]
mod scripted;

/// CPU time used by this process so far.
#[cfg(unix)]
fn cpu_time() -> Duration {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |v: libc::timeval| Duration::new(v.tv_sec as u64, v.tv_usec as u32 * 1000);
    t(u.ru_utime) + t(u.ru_stime)
}

#[cfg(not(unix))]
fn cpu_time() -> Duration {
    Duration::ZERO
}

/// Watching the whole tree as the language-server forwarder does: how long
/// the native watch takes to start, the CPU it uses while nothing changes
/// (`CODESMITH_PERF_IDLE` seconds, default 10), and a burst of 1,000
/// writes.
fn bench_watch(root: &Path, rt: &tokio::runtime::Runtime, filter: &str) {
    use codesmith_core::watch::native::{Mode, Notify};
    use codesmith_core::watch::{default_log, WatchInput, Watcher};
    if !"watch".contains(filter) && !filter.contains("watch") {
        return;
    }
    let idle = std::env::var("CODESMITH_PERF_IDLE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10u64);
    // The platform's own mode (per directory only on inotify).
    let mode = Mode::platform();
    {
        rt.block_on(async {
            let native = Arc::new(Notify::with(mode, default_log()));
            let watcher = Watcher::new(native.clone());
            let (tx, rx) = tokio::sync::oneshot::channel();
            let mut updates = watcher.subscribe(
                WatchInput::Directory {
                    path: root.to_path_buf(),
                    ignore: vec![],
                },
                Some(Box::pin(async move {
                    let _ = tx.send(());
                })),
            );
            let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = seen.clone();
            let t = Instant::now();
            let consumer = tokio::spawn(async move {
                while updates.recv().await.is_some() {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
            rx.await.unwrap();
            let started = t.elapsed();
            let (dirs, _) = native.counts();
            report(
                &format!("watch: start ({mode:?})"),
                vec![started],
                &format!("{dirs} directories watched"),
            );

            let cpu = cpu_time();
            let wall = Instant::now();
            tokio::time::sleep(Duration::from_secs(idle)).await;
            let used = cpu_time() - cpu;
            println!(
                "{:<34} {:.3} ms CPU over {:.0} s idle ({:.4}% of a core)",
                format!("watch: idle ({mode:?})"),
                used.as_secs_f64() * 1e3,
                wall.elapsed().as_secs_f64(),
                100.0 * used.as_secs_f64() / wall.elapsed().as_secs_f64()
            );

            let dir = root.join("watch-bench-burst");
            std::fs::create_dir_all(&dir).unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            seen.store(0, std::sync::atomic::Ordering::Relaxed);
            let t = Instant::now();
            for i in 0..1000 {
                std::fs::write(dir.join(format!("burst{i}.tmp")), "x").unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while seen.load(std::sync::atomic::Ordering::Relaxed) < 1000
                && Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            report(
                &format!("watch: 1k writes seen ({mode:?})"),
                vec![t.elapsed()],
                &format!(
                    "{} updates",
                    seen.load(std::sync::atomic::Ordering::Relaxed)
                ),
            );
            let _ = std::fs::remove_dir_all(&dir);
            watcher.shutdown();
            let _ = consumer.await;
        });
    }
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
