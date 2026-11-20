//! How `codesmith run` handles a step cut short: output-limit
//! continuations, the step limit's forced summary, retries after transient
//! failures, quota errors that are not retried, and content filtering.

use crate::harness::{parts, user_texts, Env};
use crate::model::{self, Model, Reply, Script, M};
use codesmith::core::runner;
use serde_json::{json, Value};

fn events_of<'a>(ev: &'a [Value], kind: &str) -> Vec<&'a Value> {
    ev.iter().filter(|e| e["type"] == kind).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn output_limit_continues_then_answers_and_gives_up_after_two() {
    let e = Env::new();
    let essay = M::tools().and(M::prompt("write the essay"));
    let nudged = M::last(runner::CONTINUE_AFTER_OUTPUT_LIMIT);
    let m = Model::start(
        Script::new()
            .titled("Essay")
            .then(
                "cut",
                essay.clone().and(nudged.clone().not()),
                Reply::text("Part one,")
                    .and_reasoning("more to say")
                    .finish("length"),
            )
            .then(
                "rest",
                essay.and(nudged.clone()),
                Reply::text(" part two.").usage(150, 20),
            )
            .then(
                "never-ends",
                M::tools()
                    .and(M::prompt("ramble"))
                    .and(nudged.clone().not()),
                Reply::text("a").finish("length"),
            )
            .always(
                "still-cut",
                M::tools().and(M::prompt("ramble")).and(nudged),
                Reply::text("b").finish("length"),
            ),
    )
    .await;
    e.config(&m.url, json!({}));

    let out = e.run(&["run", "--format", "json", "write the essay"]).await;
    out.ok();
    assert_eq!(out.text(), "Part one, part two.");
    let ev = out.events();
    let steps: Vec<&str> = events_of(&ev, "step")
        .iter()
        .map(|s| s["reason"].as_str().unwrap())
        .collect();
    assert_eq!(steps, ["length", "stop"]);
    assert_eq!(out.done()["error"], Value::Null);
    // Both steps and the title call.
    assert_eq!(out.done()["usage"]["input"], 350);

    // The model got the nudge after its own truncated answer.
    let rest = &m.by("rest")[0];
    let msgs = model::messages(rest);
    let n = msgs.len();
    assert_eq!(msgs[n - 2]["role"], "assistant");
    assert_eq!(model::content(&msgs[n - 2]), "Part one,");
    assert_eq!(
        model::content(&msgs[n - 1]),
        runner::CONTINUE_AFTER_OUTPUT_LIMIT
    );

    // Stats count the user's prompt once: the nudge is not a prompt.
    let sid = out.done()["session"].as_str().unwrap().to_string();
    let h = e.history(&sid);
    assert_eq!(user_texts(&h)[0], "write the essay");
    let stats = e.run(&["stats", "--all", "--json"]).await;
    stats.ok();
    let s: Value = serde_json::from_str(&stats.stdout).unwrap();
    assert_eq!(s["prompts"], 1, "{s:#}");

    // Three truncated responses in a row end the turn with an error.
    let out = e.run(&["run", "--format", "json", "ramble"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert_eq!(out.done()["error"], runner::OUTPUT_LIMIT_EXHAUSTED);
    assert_eq!(out.text(), "abb");
    assert_eq!(m.by("never-ends").len() + m.by("still-cut").len(), 3);
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_step_limit_forces_a_text_summary() {
    let e = Env::new();
    let work = M::tools().and(M::prompt("keep working"));
    let fin = M::last(runner::MAX_STEPS_PROMPT);
    let m = Model::start(
        Script::new()
            .titled("Work")
            .then(
                "first",
                work.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "echo one", "description": "one"}),
                ),
            )
            .then(
                "summary",
                work.and(M::step(1)).and(fin.clone()),
                Reply::text("Maximum steps reached. Ran echo one."),
            )
            .then(
                "defy-first",
                M::tools().and(M::prompt("defy")).and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "echo two", "description": "two"}),
                ),
            )
            .then(
                "defiant",
                M::tools().and(M::prompt("defy")).and(fin),
                Reply::call(
                    "shell",
                    json!({"command": "touch defied.txt", "description": "x"}),
                ),
            ),
    )
    .await;
    e.config(&m.url, json!({"agents": {"build": {"steps": 2}}}));

    let out = e.run(&["run", "--format", "json", "keep working"]).await;
    out.ok();
    assert_eq!(out.text(), "Maximum steps reached. Ran echo one.");
    let sent = &m.by("summary")[0];
    // Tools stay defined (for the cached prefix) but are switched off.
    assert_eq!(sent["tool_choice"], "none");
    assert!(!model::tools(sent).is_empty());
    let first = &m.by("first")[0];
    assert!(first.get("tool_choice").is_none());
    // The instruction is sent, not stored.
    let sid = out.done()["session"].as_str().unwrap().to_string();
    assert_eq!(user_texts(&e.history(&sid)), ["keep working"]);

    // A tool called on the final step anyway is not run.
    let out = e.run(&["run", "--format", "json", "defy"]).await;
    out.ok();
    let ev = out.events();
    let r = events_of(&ev, "tool_result");
    assert_eq!(r[1]["output"], runner::TOOLS_DISABLED);
    assert_eq!(r[1]["is_error"], true);
    assert!(!e.path("defied.txt").exists());
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_503_is_retried_then_succeeds() {
    let e = Env::new();
    let ask = M::tools().and(M::prompt("hello"));
    let m = Model::start(
        Script::new()
            .titled("Hello")
            .then(
                "busy",
                ask.clone(),
                Reply::status(503, r#"{"error": {"message": "overloaded, try again"}}"#),
            )
            .then("ok", ask, Reply::text("Hi there.")),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "--format", "json", "hello"]).await;
    out.ok();
    let ev = out.events();
    let retries = events_of(&ev, "retry");
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["attempt"], 1);
    assert!(retries[0]["error"].as_str().unwrap().contains("503"));
    assert_eq!(out.text(), "Hi there.");
    let (a, b) = (&m.by("busy")[0], &m.by("ok")[0]);
    assert_eq!(a, b, "the retry resends the same request");
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_stream_is_retried_and_only_the_full_answer_is_kept() {
    let e = Env::new();
    let ask = M::tools().and(M::prompt("hello"));
    let m = Model::start(
        Script::new()
            .titled("Hello")
            .then("dropped", ask.clone(), Reply::text("Hi th").then_drop())
            .then("ok", ask, Reply::text("Hi there.")),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "--format", "json", "hello"]).await;
    out.ok();
    let ev = out.events();
    assert_eq!(events_of(&ev, "retry").len(), 1, "{}", out.both());
    let sid = out.done()["session"].as_str().unwrap().to_string();
    let h = e.history(&sid);
    let answers: Vec<&Value> = h
        .iter()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| parts(m, "text"))
        .collect();
    assert_eq!(answers, ["Hi there."]);
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quota_429_is_not_retried() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Q").then(
        "quota",
        M::tools(),
        Reply::status(
            429,
            r#"{"error": {"message": "You exceeded your current quota", "type": "insufficient_quota", "code": "insufficient_quota"}}"#,
        ),
    ))
    .await;
    e.config(&m.url, json!({}));
    let started = std::time::Instant::now();
    let out = e.run(&["run", "--format", "json", "hello"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let ev = out.events();
    assert!(events_of(&ev, "retry").is_empty());
    let err = out.done()["error"].as_str().unwrap().to_string();
    assert!(err.contains("429") && err.contains("quota"), "{err}");
    assert_eq!(m.steps().len(), 1);
    m.assert_done();
    // The prompt is kept, so the session can be continued.
    let sid = out.done()["session"].as_str().unwrap().to_string();
    assert_eq!(user_texts(&e.history(&sid)), ["hello"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_content_filter_stops_the_turn_and_keeps_what_was_shown() {
    let e = Env::new();
    let m = Model::start(
        Script::new().titled("F").then(
            "filtered",
            M::tools(),
            Reply::text("I started to")
                .and_call("shell", json!({"command": "touch filtered.txt"}))
                .finish("content_filter"),
        ),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "--format", "json", "do it"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert_eq!(out.done()["error"], runner::CONTENT_FILTERED);
    assert_eq!(out.text(), "I started to");
    assert!(
        !e.path("filtered.txt").exists(),
        "the blocked call never ran"
    );
    let sid = out.done()["session"].as_str().unwrap().to_string();
    let h = e.history(&sid);
    let last = h.last().unwrap();
    assert_eq!(last["role"], "assistant");
    assert_eq!(parts(last, "text"), [&json!("I started to")]);
    assert!(parts(last, "tool_call").is_empty());
    m.assert_done();
}
