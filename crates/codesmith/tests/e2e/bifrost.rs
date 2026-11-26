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

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_on_the_responses_api_and_on_gemini() {
    let g = start(vec![
        priced("openai/test-gpt-1"),
        priced("gemini/test-gemini-1"),
    ])
    .await;
    g.with(|s| {
        s.routed = Some(("gemini".into(), "test-gemini-1".into()));
        s.quota = vec![budget(2.0, "r"), budget(2.5, "r")];
    });
    let e = Env::new();
    std::fs::write(
        e.config_dir.join("codesmith.json"),
        json!({"model": "bifrost/openai/test-gpt-1", "permissions": [],
               "providers": {"bifrost": {"models": {
                   "openai/test-gpt-1": {"protocol": "openai-responses"},
                   "gemini/test-gemini-1": {"protocol": "google-gemini"}}}}})
        .to_string(),
    )
    .unwrap();
    for (spec, text, path) in [
        ("bifrost/openai/test-gpt-1", "Hello there.", "/v1/responses"),
        (
            "bifrost/gemini/test-gemini-1",
            "Hello from Gemini.",
            "/genai/v1beta/models/gemini/test-gemini-1:streamGenerateContent",
        ),
    ] {
        let log = e.path(&format!("{}.jsonl", spec.replace('/', "_")));
        let out = run(
            &e,
            &g,
            &[
                "run",
                "-m",
                spec,
                "--format",
                "json",
                "--usage-log",
                log.to_str().unwrap(),
                "hi",
            ],
        )
        .await;
        out.ok();
        assert_eq!(out.text(), text, "{spec}");
        // The step's own request, and the session title's: each its own id.
        let seen = g.seen();
        let sent: Vec<String> = seen
            .iter()
            .filter(|s| s.path == path)
            .map(|s| {
                assert_eq!(s.header("x-bf-vk"), Some(KEY));
                format!("bf-{}", s.header("x-request-id").unwrap())
            })
            .collect();
        assert!(!sent.is_empty(), "{spec}: {seen:?}");
        let lines = log_lines(&log);
        let step = lines.iter().find(|l| l["type"] == "step").unwrap();
        assert!(
            sent.iter().any(|id| step["request_id"] == id.as_str()),
            "{spec}: {step} {sent:?}"
        );
        assert_eq!(step["served_model"], "gemini/test-gemini-1", "{spec}");
        // 100 × $2/M + 20 × $10/M on the routed model's gateway rates.
        let est = step["usd_estimated"].as_f64().unwrap();
        assert!((est - 0.0004).abs() < 1e-12, "{spec}: {step}");
        assert!(
            lines.iter().any(|l| l["type"] == "run"),
            "{spec}: {lines:?}"
        );
        assert!(!out.both().contains(KEY));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gateway_down_with_no_saved_list_refuses_the_model() {
    let g = start(vec![priced("openai/test-gpt-1")]).await;
    let url = g.url.clone();
    g.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let e = world("bifrost/openai/test-gpt-1");
    let out = run(&e, &g, &["run", "hi"]).await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    for part in [
        format!(
            "the Bifrost gateway at {url} couldn't be reached to list the models this key allows"
        ),
        "there's no saved list yet".to_string(),
        "check the URL, the key and the gateway, then try again".to_string(),
    ] {
        assert!(out.stderr.contains(&part), "{part}:\n{}", out.both());
    }
    assert!(!out.both().contains(KEY));
}

/// Against a real Bifrost built from source (`CODESMITH_BIFROST_BIN`),
/// with the fake gateway as its OpenAI and Anthropic upstream: the key's
/// filtered list, both APIs, the request id and routed model, and the
/// quota. Not run by default: `cargo test -p codesmith --test e2e
/// bifrost::real -- --ignored`. `CODESMITH_BIFROST_DATASHEET` names a
/// copy of Bifrost's pricing datasheet to serve it locally, as a gateway
/// won't start without one.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn real_bifrost_end_to_end() {
    let Some(bin) = std::env::var_os("CODESMITH_BIFROST_BIN") else {
        eprintln!("skipped: set CODESMITH_BIFROST_BIN to a bifrost-http binary");
        return;
    };
    const ALPHA: &str = "sk-bf-real-e2e-alpha";
    const BRAVO: &str = "sk-bf-real-e2e-bravo";
    let up = Gateway::start(State {
        tokens: (100, 20, 0, 0),
        ..Default::default()
    })
    .await;
    up.set_models(
        "",
        vec![
            json!({"id": "test-gpt-1", "object": "model", "created": 1767225600, "owned_by": "test"}),
            json!({"id": "test-gpt-2", "object": "model", "created": 1767225600, "owned_by": "test"}),
            json!({"id": "claude-test-9-1", "type": "model", "display_name": "Claude Test",
                   "created_at": "2026-01-01T00:00:00Z"}),
        ],
    );
    let datasheet = std::env::var_os("CODESMITH_BIFROST_DATASHEET")
        .map(|p| std::fs::read_to_string(p).expect("the datasheet"));
    let pricing = datasheet.is_some();
    up.with(|s| s.datasheet = datasheet);
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let provider = |models: &[&str]| {
        json!({"keys": [{"name": "upstream", "value": "upstream-test-key", "models": models, "weight": 1}],
               "network_config": {"base_url": up.url, "default_request_timeout_in_seconds": 30}})
    };
    let vk = |id: &str, value: &str, openai: &[&str], anthropic: &[&str]| {
        json!({"id": id, "name": id, "value": value, "is_active": true, "provider_configs": [
            {"provider": "openai", "allowed_models": openai, "key_ids": ["*"], "weight": 1.0},
            {"provider": "anthropic", "allowed_models": anthropic, "key_ids": ["*"], "weight": 1.0}]})
    };
    let config = json!({
        "client": {"enable_logging": true, "allowed_origins": ["*"]},
        "config_store": {"enabled": true, "type": "sqlite", "config": {"path": dir.path().join("config.db")}},
        "logs_store": {"enabled": true, "type": "sqlite", "config": {"path": dir.path().join("logs.db")}},
        "providers": {
            "openai": provider(&["*"]),
            "anthropic": provider(&["*"]),
        },
        "governance": {
            "virtual_keys": [
                vk("vk-alpha", ALPHA, &["test-gpt-1"], &["claude-test-9-1"]),
                vk("vk-bravo", BRAVO, &["test-gpt-2"], &[]),
            ],
            "budgets": [{"id": "bud-alpha", "max_limit": 100.0, "reset_duration": "1M",
                         "virtual_key_id": "vk-alpha"}]
        }
    });
    let mut config = config;
    if pricing {
        config["framework"] = json!({"pricing": {"pricing_url": format!("{}/datasheet", up.url)}});
    }
    std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
    let mut gw = tokio::process::Command::new(bin)
        .args([
            "-app-dir",
            dir.path().to_str().unwrap(),
            "-host",
            "127.0.0.1",
            "-port",
            &port.to_string(),
            "-log-level",
            "warn",
        ])
        .stdout(std::fs::File::create(dir.path().join("out.log")).unwrap())
        .stderr(std::fs::File::create(dir.path().join("err.log")).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let mut up_ok = false;
    // Bifrost takes a while to start (its pricing sync).
    for _ in 0..360 {
        if client
            .get(format!("{base}/health"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            up_ok = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        up_ok,
        "bifrost didn't start:\n{}",
        std::fs::read_to_string(dir.path().join("err.log")).unwrap_or_default()
    );

    let e = world("bifrost/openai/test-gpt-1");
    let run_with = |key: &'static str, args: Vec<String>| {
        let mut c = e.command(&args.iter().map(String::as_str).collect::<Vec<_>>());
        c.env("BIFROST_BASE_URL", &base).env("BIFROST_API_KEY", key);
        async move { finish(c.spawn().unwrap()).await }
    };
    // Each key's own list.
    let a = run_with(ALPHA, vec!["models".into(), "bifrost".into()]).await;
    a.ok();
    let b = run_with(BRAVO, vec!["models".into(), "bifrost".into()]).await;
    b.ok();
    eprintln!("alpha lists:\n{}\nbravo lists:\n{}", a.stdout, b.stdout);
    assert!(
        a.stdout.contains("bifrost/openai/test-gpt-1"),
        "{}",
        a.both()
    );
    assert!(
        !a.stdout.contains("bifrost/openai/test-gpt-2"),
        "{}",
        a.both()
    );
    assert!(
        b.stdout.contains("bifrost/openai/test-gpt-2"),
        "{}",
        b.both()
    );
    assert!(
        !b.stdout.contains("bifrost/openai/test-gpt-1"),
        "{}",
        b.both()
    );

    // Chat, with the usage log.
    let log = e.path("usage.jsonl");
    let out = run_with(
        ALPHA,
        vec![
            "run".into(),
            "--format".into(),
            "json".into(),
            "--usage-log".into(),
            log.display().to_string(),
            "hi".into(),
        ],
    )
    .await;
    out.ok();
    assert_eq!(out.text(), "Hello there.");
    let lines = log_lines(&log);
    eprintln!("usage log: {lines:?}");
    let step = lines.iter().find(|l| l["type"] == "step").unwrap();
    assert!(
        step["request_id"].as_str().is_some_and(|s| !s.is_empty()),
        "{step}"
    );
    assert_eq!(step["served_model"], "openai/test-gpt-1", "{step}");
    let to_up = up.seen();
    let chat = to_up
        .iter()
        .find(|s| s.path == "/v1/chat/completions")
        .expect("reached the upstream");
    assert_eq!(chat.body["model"], "test-gpt-1");
    assert!(
        chat.header("x-bf-vk").is_none(),
        "the virtual key stays at the gateway"
    );

    // The Messages route.
    let out = run_with(
        ALPHA,
        vec![
            "run".into(),
            "-m".into(),
            "bifrost/anthropic/claude-test-9-1".into(),
            "--format".into(),
            "json".into(),
            "hello".into(),
        ],
    )
    .await;
    eprintln!("anthropic run: {}", out.both());
    out.ok();
    assert_eq!(out.text(), "Hello from Claude.");

    // A model this key may not use.
    let out = run_with(
        BRAVO,
        vec![
            "run".into(),
            "-m".into(),
            "bifrost/openai/test-gpt-1".into(),
            "hi".into(),
        ],
    )
    .await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    let _ = gw.kill().await;
}
