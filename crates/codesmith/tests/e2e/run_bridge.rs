//! `codesmith run` as the headless agent of a tailbus bridge: the message
//! on stdin alone, a session database of the bridge's choosing
//! (`CODESMITH_DB`), `--effort`, `--tools` / `--no-tools`, `--no-mcp` and
//! `--usage-log`.

use crate::harness::Env;
use crate::model::{self, Model, Reply, Script, M};
use serde_json::{json, Value};

fn answer(prompt: &str, text: &str) -> Script {
    Script::new().always("answer", M::prompt(prompt), Reply::text(text))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_message_can_come_on_stdin_alone() {
    let e = Env::new();
    let m = Model::start(answer("review this please", "Looks fine.")).await;
    e.config(&m.url, json!({}));
    let out = e
        .run_with_stdin(&["run", "--format", "json"], "review this please\n")
        .await;
    out.ok();
    assert_eq!(out.text(), "Looks fine.");
    // An empty stdin is still refused.
    let out = e.run_with_stdin(&["run"], "  \n").await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("no message"), "{}", out.both());
}

#[tokio::test(flavor = "multi_thread")]
async fn effort_picks_the_models_variant() {
    let e = Env::new();
    let m = Model::start(answer("think", "Done.")).await;
    e.config(
        &m.url,
        json!({"providers": {"fake": {
            "protocol": "openai-chat",
            "settings": {"baseURL": m.url, "apiKey": "test-key"},
            "models": {"m": {
                "limit": {"context": 100000},
                "variants": {
                    "low": {"reasoningEffort": "low"},
                    "high": {"reasoningEffort": "high"}
                }
            }}
        }}}),
    );
    let out = e
        .run(&["run", "--format", "json", "--effort", "high", "think"])
        .await;
    out.ok();
    let steps = m.steps();
    let step = steps.iter().find(|b| model::prompt(b) == "think").unwrap();
    assert_eq!(step["reasoning_effort"], "high", "{step}");

    // `default` is the model without a variant.
    let out = e
        .run(&["run", "--format", "json", "--effort", "default", "think"])
        .await;
    out.ok();
    let steps = m.steps();
    let last = steps.iter().rfind(|b| model::prompt(b) == "think").unwrap();
    assert!(last.get("reasoning_effort").is_none(), "{last}");

    // A level the model doesn't have is refused before anything is sent.
    let before = m.steps().len();
    let out = e.run(&["run", "--effort", "max", "think"]).await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("no effort \"max\""), "{}", out.both());
    assert_eq!(m.steps().len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn effort_on_a_model_without_levels_is_refused() {
    let e = Env::new();
    let m = Model::start(answer("think", "Done.")).await;
    e.config(
        &m.url,
        json!({"providers": {"fake": {
            "protocol": "openai-chat",
            "settings": {"baseURL": m.url, "apiKey": "test-key"},
            "models": {"m": {
                "limit": {"context": 100000},
                "capabilities": {"reasoning": {"supported": false}}
            }}
        }}}),
    );
    let out = e.run(&["run", "--effort", "high", "think"]).await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert!(out.stderr.contains("no effort levels"), "{}", out.both());
    assert!(m.steps().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_offers_only_the_named_tools_and_denies_the_rest() {
    let e = Env::new();
    e.write("notes.txt", "hello\n");
    let script = Script::new()
        .then(
            "try shell",
            M::prompt("look").and(M::step(0)),
            Reply::call(
                "shell",
                json!({"command": "touch pwned", "description": "x"}),
            ),
        )
        .then(
            "read",
            M::prompt("look").and(M::step(1)),
            Reply::call("read", json!({"path": "notes.txt"})),
        )
        .then(
            "answer",
            M::prompt("look").and(M::step(2)),
            Reply::text("It says hello."),
        );
    let m = Model::start(script).await;
    // Even a configured allow for shell can't lift the narrowing.
    e.config(
        &m.url,
        json!({"permissions": [{"action": "shell", "resource": "*", "effect": "allow"}]}),
    );
    let out = e
        .run(&["run", "--format", "json", "--tools", "read,grep", "look"])
        .await;
    out.ok();
    let steps = m.steps();
    let mut offered = model::tools(&steps[0]);
    offered.sort();
    assert!(
        offered
            .iter()
            .all(|t| t == "read" || t == "grep" || t == "execute"),
        "{offered:?}"
    );
    assert!(offered.contains(&"read".to_string()), "{offered:?}");
    assert!(!e.path("pwned").exists(), "shell never ran");
    let results = model::results(&steps[1]);
    assert!(
        results[0].contains("No tool named \"shell\""),
        "{results:?}"
    );
    assert!(model::results(&steps[2])[1].contains("hello"));
    assert_eq!(out.text(), "It says hello.");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_tool_is_refused_before_anything_is_sent() {
    let e = Env::new();
    let m = Model::start(answer("look", "x")).await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "--tools", "read,rm -rf", "look"]).await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("unknown tool"), "{}", out.both());
    assert!(m.steps().is_empty());
    // --tools and --no-tools together make no sense.
    let out = e
        .run(&["run", "--tools", "read", "--no-tools", "look"])
        .await;
    assert_ne!(out.code, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_tools_offers_nothing() {
    let e = Env::new();
    let m = Model::start(answer("hi", "Hello.")).await;
    e.config(&m.url, json!({}));
    let out = e
        .run(&["run", "--format", "json", "--no-tools", "--no-mcp", "hi"])
        .await;
    out.ok();
    // The agent step offers no tools: the request has none (Model::steps
    // keeps only requests that offer some).
    assert!(
        m.steps().is_empty(),
        "{:?}",
        m.steps().iter().map(model::tools).collect::<Vec<_>>()
    );
    // The agent step and the title request both reached the model.
    let answered = m.by("answer");
    assert!(!answered.is_empty());
    assert!(answered.iter().all(|b| model::tools(b).is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_mcp_connects_no_server() {
    let e = Env::new();
    let m = Model::start(answer("hi", "Hello.")).await;
    // A server that can't start would warn if it were tried.
    e.config(
        &m.url,
        json!({"mcp": {"servers": {"broken": {"type": "local", "command": ["/nonexistent/mcp-server"]}}}}),
    );
    let out = e.run(&["run", "--no-mcp", "hi"]).await;
    out.ok();
    assert!(!out.stderr.contains("broken"), "{}", out.both());
    let out = e.run(&["run", "hi"]).await;
    out.ok();
    assert!(out.stderr.contains("broken"), "the control: {}", out.both());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_usage_log_has_a_line_per_step_and_tool_call() {
    let e = Env::new();
    e.write("notes.txt", "hello\n");
    let script = Script::new()
        .then(
            "read",
            M::prompt("look").and(M::step(0)),
            Reply::call("read", json!({"path": "notes.txt"})),
        )
        .then(
            "answer",
            M::prompt("look").and(M::step(1)),
            Reply::text("hello"),
        );
    let m = Model::start(script).await;
    e.config(&m.url, json!({}));
    let log = e.path("bridge/session/usage.jsonl");
    let out = e
        .run(&[
            "run",
            "--format",
            "json",
            "--usage-log",
            log.to_str().unwrap(),
            "look",
        ])
        .await;
    out.ok();
    let sid = out.done()["session"].as_str().unwrap().to_string();
    let lines: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let steps: Vec<&Value> = lines.iter().filter(|l| l["type"] == "step").collect();
    assert_eq!(steps.len(), 2, "{lines:?}");
    for s in &steps {
        assert_eq!(s["model"], "fake/m");
        assert_eq!(s["session"], sid.as_str());
        assert_eq!(s["input"], 100);
        assert_eq!(s["output"], 10);
    }
    let tools: Vec<&Value> = lines.iter().filter(|l| l["type"] == "tool").collect();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "read");
    assert_eq!(tools[0]["path"], "notes.txt");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&log).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // A second run appends.
    let m2 = Model::start(answer("again", "ok")).await;
    e.config(&m2.url, json!({}));
    e.run(&["run", "--usage-log", log.to_str().unwrap(), "again"])
        .await
        .ok();
    let n = std::fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(n, lines.len() + 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_usage_log_refuses_a_symlink() {
    let e = Env::new();
    let m = Model::start(answer("look", "x")).await;
    e.config(&m.url, json!({}));
    let target = e.path("victim.txt");
    std::fs::write(&target, "keep\n").unwrap();
    let link = e.path("usage.jsonl");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let out = e
        .run(&["run", "--usage-log", link.to_str().unwrap(), "look"])
        .await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("usage log"), "{}", out.both());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep\n");
    assert!(m.steps().is_empty(), "refused before the model is asked");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_keeps_sessions_in_its_own_database_and_continues_them() {
    let e = Env::new();
    let script = Script::new()
        .always("first", M::prompt("remember 7"), Reply::text("Noted."))
        .always("second", M::prompt("what number"), Reply::text("7."));
    let m = Model::start(script).await;
    e.config(&m.url, json!({}));
    let dir = e.path("bridge/sender-a");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("codesmith.db");
    let run = |args: &[&str], input: &str| {
        let mut c = e.command(args);
        c.env("CODESMITH_DB", &db)
            .stdin(std::process::Stdio::piped());
        let input = input.to_string();
        async move {
            use tokio::io::AsyncWriteExt;
            let mut child = c.spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(input.as_bytes()).await.unwrap();
            drop(stdin);
            child.wait_with_output().await.unwrap()
        }
    };
    let first = run(&["run", "--format", "json"], "remember 7").await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run(&["run", "--format", "json", "--continue"], "what number").await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    // The second run saw the first exchange.
    let steps = m.steps();
    let last = steps
        .iter()
        .rfind(|b| model::prompt(b) == "what number")
        .unwrap();
    let all: String = model::messages(last).iter().map(model::content).collect();
    assert!(
        all.contains("remember 7") && all.contains("Noted."),
        "{all}"
    );
    assert!(db.exists());
    // The user's own database never saw these sessions.
    assert!(e.store().list(None, true).unwrap_or_default().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_with_no_key_fails_at_once_and_says_how_to_give_it_one() {
    let e = Env::new();
    // No config naming a server: a built-in provider signed in to with a key.
    let started = std::time::Instant::now();
    let out = e
        .run_with_stdin(
            &[
                "run",
                "-m",
                "fireworks/accounts/fireworks/models/kimi-k3p1-pro",
            ],
            "say hi",
        )
        .await;
    assert_ne!(out.code, Some(0));
    assert!(
        out.stderr.contains("No API key for Fireworks AI"),
        "{}",
        out.both()
    );
    assert!(out.stderr.contains("FIREWORKS_API_KEY"), "{}", out.both());
    // The world's HOME isn't the account's, as under a bridge: the login
    // goes in that home with `--home`, run as you in your own shell.
    assert!(
        out.stderr.contains(&format!(
            "codesmith auth login fireworks --home {}",
            e.home.display()
        )),
        "{}",
        out.both()
    );
    assert!(!out.stderr.contains("HOME="), "{}", out.both());
    assert!(
        out.stderr.contains("codesmith bridge init"),
        "{}",
        out.both()
    );
    assert!(out.stderr.contains("auth.json"), "{}", out.both());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "it waited"
    );
    // The run goes ahead with a key (to a local model server standing in
    // for Fireworks, so nothing leaves the machine).
    let m = Model::start(answer("hi", "Hello.")).await;
    e.config(
        &m.url,
        json!({"model": "fireworks/kimi", "providers": {"fireworks": {
            "settings": {"baseURL": m.url},
            "models": {"kimi": {"limit": {"context": 100000}}}
        }}}),
    );
    let mut c = e.command(&["run", "hi"]);
    c.env("FIREWORKS_API_KEY", "fw-test");
    let o = tokio::time::timeout(std::time::Duration::from_secs(60), c.output())
        .await
        .unwrap()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "Hello.");
    // (A server on this machine needs no key, so the check leaves it
    // alone: the run above would have passed without FIREWORKS_API_KEY.)
}
