//! `codesmith run` on Bifrost, headless, with only the global config and
//! the environment: the key's model list fetched as the run starts, a
//! model outside it refused, the saved list when the gateway can't list,
//! a fresh request id per call (retries too), and the usage log's billed
//! and estimated cost, request id, routed model and run total.

#[path = "../../../codesmith-core/tests/bifrost_gateway/mod.rs"]
mod gateway;

use crate::harness::{finish, Env, Out};
use gateway::{Gateway, State};
use serde_json::{json, Value};

const KEY: &str = "sk-bf-e2e-test-key-7781";

fn priced(id: &str) -> Value {
    json!({"id": id, "name": id, "context_length": 100000, "max_output_tokens": 8000,
           "pricing": {"prompt": "0.000002", "completion": "0.00001"}})
}

async fn start(models: Vec<Value>) -> Gateway {
    let g = Gateway::start(State {
        tokens: (100, 20, 0, 0),
        ..Default::default()
    })
    .await;
    g.set_models(KEY, models);
    g
}

/// The global config names only the model; the gateway and key come
/// from the environment.
fn world(model: &str) -> Env {
    let e = Env::new();
    std::fs::write(
        e.config_dir.join("codesmith.json"),
        json!({"model": model, "permissions": []}).to_string(),
    )
    .unwrap();
    e
}

async fn run(e: &Env, g: &Gateway, args: &[&str]) -> Out {
    let mut c = e.command(args);
    c.env("BIFROST_BASE_URL", format!("{}/v1", g.url))
        .env("BIFROST_API_KEY", KEY);
    finish(c.spawn().unwrap()).await
}

fn log_lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn budget(usage: f64, reset: &str) -> (u16, Value) {
    (
        200,
        json!({"virtual_key_name": "test", "budgets": [{"id": "b1", "max_limit": 50.0,
            "reset_duration": "1M", "last_reset": reset, "current_usage": usage, "per_model_usage": []}]}),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_lists_the_keys_models_and_logs_billed_estimated_and_run_costs() {
    let g = start(vec![
        priced("openai/test-gpt-1"),
        priced("openai/test-gpt-1-mini"),
    ])
    .await;
    g.with(|s| {
        s.routed = Some(("openai".into(), "test-gpt-1-mini".into()));
        s.cost = Some(json!({"input_cost": 0.001, "output_cost": 0.003, "total_cost": 0.004}));
        s.quota = vec![
            budget(1.0, "2026-11-01T00:00:00Z"),
            budget(1.25, "2026-11-01T00:00:00Z"),
        ];
    });
    let e = world("bifrost/openai/test-gpt-1");
    let log = e.path("usage.jsonl");
    let out = run(
        &e,
        &g,
        &[
            "run",
            "--format",
            "json",
            "--usage-log",
            log.to_str().unwrap(),
            "hi",
        ],
    )
    .await;
    out.ok();
    assert_eq!(out.text(), "Hello there.");
    let seen = g.seen();
    let paths: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
    let list = paths
        .iter()
        .position(|p| *p == "/v1/models")
        .expect("listed");
    let chat = paths
        .iter()
        .position(|p| *p == "/v1/chat/completions")
        .expect("asked");
    assert!(
        list < chat,
        "the list is fetched as the run starts: {paths:?}"
    );
    let req = &seen[chat];
    assert_eq!(req.header("x-bf-vk"), Some(KEY));
    assert_eq!(req.body["model"], "openai/test-gpt-1");
    let sent = req.header("x-request-id").unwrap();

    let lines = log_lines(&log);
    let step = lines.iter().find(|l| l["type"] == "step").unwrap();
    assert_eq!(step["usd"], 0.004, "{step}");
    // Priced on the routed model's gateway rates: 100 × $2/M + 20 × $10/M.
    let est = step["usd_estimated"].as_f64().unwrap();
    assert!((est - 0.0004).abs() < 1e-12, "{step}");
    assert_eq!(step["request_id"], format!("bf-{sent}"));
    assert_eq!(step["served_model"], "openai/test-gpt-1-mini");
    let total = lines
        .iter()
        .find(|l| l["type"] == "run")
        .expect("a run line");
    assert!(
        (total["usd_quota"].as_f64().unwrap() - 0.25).abs() < 1e-12,
        "{total}"
    );
    assert_eq!(total["budget"], "b1");
    assert!(seen
        .iter()
        .filter(|s| s.path.ends_with("/quota"))
        .all(|s| s.header("x-bf-vk") == Some(KEY)));
    // The key is nowhere in what the run printed or wrote.
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(!out.both().contains(KEY) && !text.contains(KEY));
    let kept = std::fs::read_to_string(e.data.join("codesmith/live-models.json")).unwrap();
    assert!(!kept.contains(KEY));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_outside_the_keys_list_is_refused_before_anything_is_sent() {
    let g = start(vec![priced("openai/test-gpt-1")]).await;
    let e = world("bifrost/groq/not-for-this-key");
    let out = run(&e, &g, &["run", "hi"]).await;
    assert_ne!(out.code, Some(0));
    assert!(
        out.stderr.contains("isn't among the 1 models"),
        "{}",
        out.both()
    );
    assert!(!g.seen().iter().any(|s| s.path.contains("chat")));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_saved_list_stands_in_when_the_gateway_cant_list_and_the_run_says_so() {
    let g = start(vec![priced("openai/test-gpt-1")]).await;
    let e = world("bifrost/openai/test-gpt-1");
    run(&e, &g, &["run", "hi"]).await.ok();
    g.with(|s| s.list_status = Some(503));
    let out = run(&e, &g, &["run", "--format", "json", "again"]).await;
    out.ok();
    let notice = out
        .events()
        .into_iter()
        .find(|v| v["type"] == "notice")
        .unwrap_or_else(|| panic!("a notice:\n{}", out.both()));
    assert!(
        notice["message"]
            .as_str()
            .unwrap()
            .contains("using the list saved"),
        "{notice}"
    );
    // `codesmith models bifrost` falls back the same way, with a warning.
    let out = run(&e, &g, &["models", "bifrost"]).await;
    out.ok();
    assert!(
        out.stdout.contains("bifrost/openai/test-gpt-1"),
        "{}",
        out.both()
    );
    assert!(out.stderr.contains("saved for this key"), "{}", out.both());
    // A changed list is seen by the next run.
    g.with(|s| s.list_status = None);
    g.set_models(
        KEY,
        vec![priced("openai/test-gpt-1"), priced("mistral/new-model")],
    );
    let out = run(&e, &g, &["models", "bifrost"]).await;
    out.ok();
    assert!(
        out.stdout.contains("bifrost/mistral/new-model"),
        "{}",
        out.both()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_call_gets_a_new_request_id() {
    let g = start(vec![priced("openai/test-gpt-1")]).await;
    g.with(|s| s.fail_first = 1);
    let e = world("bifrost/openai/test-gpt-1");
    let out = run(&e, &g, &["run", "--format", "json", "hi"]).await;
    out.ok();
    let ids: Vec<String> = g
        .seen()
        .iter()
        .filter(|s| s.path == "/v1/chat/completions")
        .map(|s| s.header("x-request-id").unwrap().to_string())
        .collect();
    // The failed call, its retry, and the session title's call.
    assert!(ids.len() >= 2, "{}", out.both());
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "{ids:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_run_line_without_a_budget_after_a_reset_or_when_the_read_fails() {
    for quota in [
        vec![(200, json!({"budgets": []}))],
        vec![
            budget(5.0, "2026-11-01T00:00:00Z"),
            budget(0.1, "2026-11-02T00:00:00Z"),
        ],
        vec![],
        vec![(500, json!({"error": "down"}))],
    ] {
        let g = start(vec![priced("openai/test-gpt-1")]).await;
        g.with(|s| s.quota = quota.clone());
        let e = world("bifrost/openai/test-gpt-1");
        let log = e.path("usage.jsonl");
        run(&e, &g, &["run", "--usage-log", log.to_str().unwrap(), "hi"])
            .await
            .ok();
        let lines = log_lines(&log);
        assert!(lines.iter().any(|l| l["type"] == "step"));
        assert!(
            !lines.iter().any(|l| l["type"] == "run"),
            "{quota:?}: {lines:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_quota_setting_turns_the_reads_off() {
    let g = start(vec![priced("openai/test-gpt-1")]).await;
    g.with(|s| s.quota = vec![budget(1.0, "r"), budget(2.0, "r")]);
    let e = Env::new();
    std::fs::write(
        e.config_dir.join("codesmith.json"),
        json!({"model": "bifrost/openai/test-gpt-1", "permissions": [],
               "providers": {"bifrost": {"settings": {"quota": false}}}})
        .to_string(),
    )
    .unwrap();
    let log = e.path("usage.jsonl");
    run(&e, &g, &["run", "--usage-log", log.to_str().unwrap(), "hi"])
        .await
        .ok();
    assert!(!g.seen().iter().any(|s| s.path.ends_with("/quota")));
    assert!(!log_lines(&log).iter().any(|l| l["type"] == "run"));
}
