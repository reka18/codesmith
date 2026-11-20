//! `codesmith run` and the session subcommands: a multi-step fix, config
//! permission rules, continuing, undo/redo, export/import and stats.

use crate::harness::{parts, tool_results, user_texts, Env};
use crate::model::{self, Model, Reply, Script, M};
use crate::need_git;
use serde_json::{json, Value};

const BUGGY: &str = "echo $(($1 - $2))\n";
const FIXED: &str = "echo $(($1 + $2))\n";
const TEST: &str = "out=$(sh add.sh 2 3)\nif [ \"$out\" = 5 ]; then echo PASS; else echo \"FAIL: got $out\"; exit 1; fi\n";

/// read → edit → run the test → answer.
fn fix_script() -> Script {
    let fix = M::tools().and(M::prompt("fix add.sh"));
    Script::new()
        .titled("Fix add.sh")
        .then(
            "read",
            fix.clone().and(M::step(0)),
            Reply::call("read", json!({"path": "add.sh"})),
        )
        .then(
            "edit",
            fix.clone().and(M::step(1)),
            Reply::text("The operator is wrong.")
                .and_call(
                    "edit",
                    json!({"path": "add.sh", "oldString": "$1 - $2", "newString": "$1 + $2"}),
                )
                .finish("tool_calls"),
        )
        .then(
            "test",
            fix.clone().and(M::step(2)),
            Reply::call(
                "shell",
                json!({"command": "sh test.sh", "description": "Run the test"}),
            ),
        )
        .then(
            "answer",
            fix.and(M::step(3)).and(M::last("PASS")),
            Reply::text("Fixed: add.sh now adds, and test.sh passes."),
        )
}

fn project(e: &Env) {
    e.write("add.sh", BUGGY);
    e.write("test.sh", TEST);
    e.commit("add calculator");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fix_from_read_to_answer_then_undo_redo_continue_export_and_stats() {
    need_git!();
    let e = Env::new();
    project(&e);
    let m = Model::start(fix_script().then(
        "recap",
        M::tools().and(M::prompt("what did you change")),
        Reply::text("I changed - to + in add.sh."),
    ))
    .await;
    e.config(&m.url, json!({}));

    let out = e.run(&["run", "--format", "json", "fix add.sh"]).await;
    out.ok();
    assert_eq!(e.read("add.sh"), FIXED);

    // What the front end showed.
    let ev = out.events();
    let calls: Vec<&str> = ev
        .iter()
        .filter(|e| e["type"] == "tool_call")
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(calls, ["read", "edit", "shell"]);
    let results: Vec<&Value> = ev.iter().filter(|e| e["type"] == "tool_result").collect();
    assert!(
        results.iter().all(|r| r["is_error"] == false),
        "{results:?}"
    );
    assert!(results[2]["output"].as_str().unwrap().contains("PASS"));
    assert_eq!(
        out.text(),
        "The operator is wrong.Fixed: add.sh now adds, and test.sh passes."
    );
    let done = out.done();
    assert_eq!(done["error"], Value::Null);
    assert_eq!(done["title"], "Fix add.sh");
    // Four steps and the title call.
    assert_eq!(done["usage"]["input"], 500);
    assert_eq!(done["usage"]["output"], 50);
    let sid = done["session"].as_str().unwrap().to_string();

    // What the model was sent.
    let steps = m.steps();
    assert_eq!(steps.len(), 4);
    let first = &steps[0];
    assert_eq!(model::prompt(first), "fix add.sh");
    assert!(model::system(first).contains(&e.project.display().to_string()));
    for t in ["read", "edit", "write", "shell", "glob", "grep"] {
        assert!(model::tools(first).contains(&t.to_string()), "{t} offered");
    }
    let read = &model::results(&steps[1])[0];
    assert!(read.contains("echo $(($1 - $2))"), "{read}");
    let last = steps.last().unwrap();
    assert_eq!(model::results(last).len(), 3);
    // Only the user's prompt is a user message: no engine nudges.
    let users: Vec<String> = model::messages(last)
        .iter()
        .filter(|m| m["role"] == "user")
        .map(model::content)
        .collect();
    assert_eq!(users, ["fix add.sh"]);

    // What the store holds.
    let db = e.store();
    let info = db.get(&sid).unwrap().unwrap();
    assert_eq!(info.directory, e.project.display().to_string());
    assert_eq!(info.title.as_deref(), Some("Fix add.sh"));
    assert_eq!((info.usage.input, info.usage.output), (500, 50));
    let h = e.history(&sid);
    assert_eq!(user_texts(&h), ["fix add.sh"]);
    assert_eq!(tool_results(&h).len(), 3);
    let kinds = e.record_kinds(&sid);
    // Four steps, then the title call's.
    assert_eq!(kinds.iter().filter(|k| *k == "step").count(), 5);
    assert_eq!(kinds.iter().filter(|k| *k == "tool").count(), 3);
    assert_eq!(kinds.first().map(String::as_str), Some("snapshot"));
    // The turn ends, then the title call (which ran beside it) is counted.
    assert_eq!(
        kinds[kinds.len() - 2..],
        ["turn_end".to_string(), "step".to_string()]
    );

    // diff → undo → redo.
    let diff = e.run(&["session", "diff"]).await;
    diff.ok();
    assert!(
        diff.stdout.contains("+echo $(($1 + $2))"),
        "{}",
        diff.both()
    );
    let undo = e.run(&["session", "undo"]).await;
    undo.ok();
    assert!(
        undo.stdout.contains("restored 1 file(s)") && undo.stdout.contains("reverted  add.sh"),
        "{}",
        undo.both()
    );
    assert_eq!(e.read("add.sh"), BUGGY);
    assert!(e.history(&sid).is_empty(), "the undone turn is hidden");
    let redo = e.run(&["session", "redo", &sid]).await;
    redo.ok();
    assert!(redo.stdout.contains("reapplied add.sh"), "{}", redo.both());
    assert_eq!(e.read("add.sh"), FIXED);
    assert_eq!(user_texts(&e.history(&sid)), ["fix add.sh"]);

    // Continue the same session: the model sees the whole first turn.
    let more = e
        .run(&["run", "-c", "--format", "json", "what did you change?"])
        .await;
    more.ok();
    assert_eq!(more.done()["session"], sid.as_str());
    assert_eq!(more.text(), "I changed - to + in add.sh.");
    let recap = &m.by("recap")[0];
    let roles: Vec<&str> = model::messages(recap)
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        [
            "system",
            "user",
            "assistant",
            "tool",
            "assistant",
            "tool",
            "assistant",
            "tool",
            "assistant",
            "user"
        ]
    );
    assert_eq!(
        user_texts(&e.history(&sid)),
        ["fix add.sh", "what did you change?"]
    );
    let listed = e.run(&["session", "list", "--json"]).await;
    listed.ok();
    let list: Value = serde_json::from_str(&listed.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], sid.as_str());

    // Export, then import into a fresh world.
    let exported = e.run(&["session", "export", &sid]).await;
    exported.ok();
    let ex: Value = serde_json::from_str(&exported.stdout).unwrap();
    assert_eq!(ex["info"]["id"], sid.as_str());
    let other = Env::new();
    let imported = other
        .run_with_stdin(&["session", "import", "-"], &exported.stdout)
        .await;
    imported.ok();
    assert!(imported.stdout.contains(&format!("Imported {sid}")));
    assert_eq!(other.history(&sid), e.history(&sid));
    let again = other.run(&["session", "export", &sid]).await;
    let ex2: Value = serde_json::from_str(&again.stdout).unwrap();
    assert_eq!(ex2["entries"], ex["entries"]);

    // Stats over both runs.
    let stats = e.run(&["stats", "--all", "--json"]).await;
    stats.ok();
    let s: Value = serde_json::from_str(&stats.stdout).unwrap();
    assert_stats(&s);
    m.assert_done();
}

/// The numbers `stats --json` reports after the fix (four steps, three
/// tools, the title call) and the recap (one step), priced at $1/$2 per
/// million tokens.
fn assert_stats(s: &Value) {
    assert_eq!(
        (&s["sessions"], &s["subagents"], &s["prompts"], &s["steps"]),
        (&json!(1), &json!(0), &json!(2), &json!(6)),
        "{s:#}"
    );
    assert_eq!(
        (&s["tokens"]["input"], &s["tokens"]["output"]),
        (&json!(600), &json!(60))
    );
    assert!(
        (s["cost"].as_f64().unwrap() - 0.00072).abs() < 1e-9,
        "{s:#}"
    );
    assert_eq!(s["models"][0]["model"], "fake/m");
    assert_eq!(s["models"][0]["steps"], 6);
    let mut tools: Vec<(String, u64, u64)> = s["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["calls"].as_u64().unwrap(),
                t["failed"].as_u64().unwrap(),
            )
        })
        .collect();
    tools.sort();
    assert_eq!(
        tools,
        [
            ("edit".into(), 1, 0),
            ("read".into(), 1, 0),
            ("shell".into(), 1, 0)
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn config_permission_rules_allow_deny_and_ask() {
    need_git!();
    let e = Env::new();
    e.write("build/out.txt", "artifact\n");
    let clean = M::tools().and(M::prompt("clean up"));
    let m = Model::start(
        Script::new()
            .titled("Clean up")
            .then(
                "compound",
                clean.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "touch made.txt && rm -rf build", "description": "clean"}),
                ),
            )
            .then(
                "ask",
                clean.clone().and(M::step(1)),
                Reply::call(
                    "shell",
                    json!({"command": "touch asked.txt", "description": "mark"}),
                ),
            )
            .then(
                "write-denied",
                clean.clone().and(M::step(2)),
                Reply::call("write", json!({"path": "secret/key.txt", "content": "k"})),
            )
            .then(
                "done",
                clean.clone().and(M::step(3)),
                Reply::text("Cleaned what I was allowed to."),
            )
            .then(
                "ask-no-tty",
                M::tools().and(M::prompt("mark again")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "touch again.txt", "description": "mark"}),
                ),
            )
            .then(
                "explain",
                M::tools().and(M::prompt("mark again")).and(M::step(1)),
                Reply::text("I need approval: re-run with --auto."),
            ),
    )
    .await;
    e.config(
        &m.url,
        json!({"permissions": [
            {"action": "shell", "resource": "rm *", "effect": "deny"},
            {"action": "shell", "resource": "touch asked.txt", "effect": "ask"},
            {"action": "shell", "resource": "touch again.txt", "effect": "ask"},
            {"action": "edit", "resource": "secret/*", "effect": "deny"}
        ]}),
    );

    let out = e
        .run(&["run", "--auto", "--format", "json", "clean up"])
        .await;
    out.ok();
    let results: Vec<Value> = out
        .events()
        .into_iter()
        .filter(|e| e["type"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 3);
    // The whole compound command is refused when one part is denied.
    assert_eq!(results[0]["is_error"], true);
    assert!(
        results[0]["output"]
            .as_str()
            .unwrap()
            .contains("rm -rf build"),
        "{results:?}"
    );
    assert!(e.path("build/out.txt").exists());
    assert!(!e.path("made.txt").exists(), "no part of it ran");
    // `ask` with --auto: approved.
    assert_eq!(results[1]["is_error"], false, "{results:?}");
    assert!(e.path("asked.txt").exists());
    // A denied path for write.
    assert_eq!(results[2]["is_error"], true);
    assert!(!e.path("secret/key.txt").exists());
    assert_eq!(out.text(), "Cleaned what I was allowed to.");
    // The model saw the refusals as tool errors and went on.
    let last = &m.by("done")[0];
    let seen = model::results(last);
    assert!(seen[0].contains("Permission denied"), "{seen:?}");
    assert!(seen[2].contains("Permission denied"), "{seen:?}");

    // `ask` without --auto and without a terminal: nothing runs, and the
    // model is told why (a rejection with feedback, so the turn goes on).
    let out = e.run(&["run", "--format", "json", "mark again"]).await;
    out.ok();
    assert!(!e.path("again.txt").exists());
    let ev = out.events();
    let result = ev.iter().find(|e| e["type"] == "tool_result").unwrap();
    assert_eq!(result["is_error"], true);
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("Re-run with --auto"),
        "{}",
        out.both()
    );
    assert!(model::results(&m.by("explain")[0])[0].contains("No terminal is available"));
    assert_eq!(out.text(), "I need approval: re-run with --auto.");
    m.assert_done();
    let sid = out.done()["session"].as_str().unwrap().to_string();
    let h = e.history(&sid);
    let r = &tool_results(&h)[0];
    assert_eq!(r["is_error"], true);
    assert_eq!(parts(&h[0], "text")[0], "mark again");
}

// OpenCode: mini.test.ts, "merges non-interactive argument and stdin input".
#[tokio::test(flavor = "multi_thread")]
async fn piped_stdin_attached_files_and_titles() {
    let e = Env::new();
    e.write("notes.txt", "remember the milk\n");
    let m = Model::start(
        Script::new().titled("Notes").then(
            "answer",
            M::tools()
                .and(M::prompt("summarize"))
                .and(M::prompt("from stdin"))
                .and(M::prompt("remember the milk")),
            Reply::text("Milk."),
        ),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = e
        .run_with_stdin(
            &["run", "--title", "Mine", "-f", "notes.txt", "summarize"],
            "from stdin\n",
        )
        .await;
    out.ok();
    assert_eq!(out.stdout.trim(), "Milk.");
    assert!(out.stderr.contains("Build · fake/m"), "{}", out.both());
    m.assert_done();
    assert!(m.by("title").is_empty(), "--title skips the title request");
    let list = e.store().list(None, false).unwrap();
    assert_eq!(list[0].title.as_deref(), Some("Mine"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_commands_and_missing_sessions_fail_cleanly() {
    let e = Env::new();
    let m = Model::start(Script::new()).await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "/nope"]).await;
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("unknown command /nope"),
        "{}",
        out.both()
    );
    let out = e.run(&["run", "-c", "hello"]).await;
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("no session to continue"),
        "{}",
        out.both()
    );
    assert!(m.seen().is_empty());
}

/// Regression: processes opening a new session database at the same moment
/// (the UI and its automation scheduler on first run) failed with
/// "database is locked", and the UI then ran without saving its session.
#[tokio::test(flavor = "multi_thread")]
async fn processes_opening_a_new_database_together_all_succeed() {
    for _ in 0..6 {
        let e = Env::new();
        let runs: Vec<_> = (0..6)
            .map(|_| e.command(&["session", "list", "--json"]).spawn().unwrap())
            .collect();
        for r in runs {
            let out = r.wait_with_output().await.unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// As OpenCode's `run`: the prompt's background commands are waited for,
/// and the model is resumed with each result before the run exits.
/// OpenCode: run/noninteractive.test.ts, "waits for a background shell and
/// streams the reply it wakes".
#[tokio::test(flavor = "multi_thread")]
async fn run_waits_for_background_commands_and_resumes_the_model() {
    let e = Env::new();
    let ask = M::tools().and(M::prompt("build it"));
    let notified = M::last("<shell id=\"shell_1\"");
    let m = Model::start(
        Script::new()
            .titled("Build")
            .then(
                "start",
                ask.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "sleep 1; echo built-ok", "description": "build", "background": true}),
                ),
            )
            .then(
                "answer",
                ask.clone().and(M::step(1)).and(notified.clone().not()),
                Reply::text("The build is running."),
            )
            .always(
                "resumed",
                M::tools().and(notified),
                Reply::text(" It finished."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = e
        .run(&["run", "--auto", "--format", "json", "build it"])
        .await;
    out.ok();
    assert_eq!(
        out.text(),
        "The build is running. It finished.",
        "{}",
        out.both()
    );
    let resumed = &m.by("resumed")[0];
    let last = model::messages(resumed).last().cloned().unwrap();
    assert!(model::content(&last).contains("built-ok"), "{last}");
}

/// Fixed: Ctrl-C on a run waiting for a background server left the server
/// running (it is in its own process group, which never got the signal).
/// Now the run stops it and exits; before the fix the turn itself never
/// ended while the command ran, in `run` and in the TUI and server.
/// OpenCode: run/noninteractive.test.ts, "cancelling while waiting on
/// background work stops the Session and this prompt's jobs" (adapted: no
/// subagent jobs, which run inside the turn here).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn interrupting_a_run_stops_its_background_server() {
    let e = Env::new();
    let pid_file = e.path("server.pid");
    let command = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
    let m = Model::start(
        Script::new()
            .titled("Bg")
            .then(
                "start",
                M::tools()
                    .and(M::prompt("start the server"))
                    .and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": command, "description": "server", "background": true}),
                ),
            )
            .always(
                "said",
                M::tools()
                    .and(M::prompt("start the server"))
                    .and(M::step(0).not()),
                Reply::text("The server is running in the background."),
            ),
    )
    .await;
    e.config(&m.url, json!({}));
    let child = e
        .command(&["run", "--auto", "start the server"])
        .spawn()
        .unwrap();
    let mut server = String::new();
    for _ in 0..300 {
        server = std::fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !server.is_empty() && !m.by("said").is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!server.is_empty(), "the server never started");
    // Let the run settle into waiting.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let alive = |pid: &str| {
        std::process::Command::new("/bin/kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(
        alive(&server),
        "the run should still be waiting on the server"
    );
    let pid = child.id().unwrap().to_string();
    std::process::Command::new("/bin/kill")
        .args(["-INT", &pid])
        .status()
        .unwrap();
    let t = std::time::Instant::now();
    let out = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output())
        .await
        .expect("the run did not exit after Ctrl-C")
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let gone = !alive(&server);
    let _ = std::process::Command::new("/bin/kill")
        .args(["-9", &server])
        .status();
    assert!(t.elapsed() < std::time::Duration::from_secs(10), "{stderr}");
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("waiting for 1 background command: shell_1"),
        "{stderr}"
    );
    assert!(stderr.contains("interrupted"), "{stderr}");
    assert!(gone, "the server {server} outlived the run:\n{stderr}");
}
