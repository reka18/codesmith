//! The models.dev catalog through the binary: listing, resolution, and the
//! background refresh against a local stand-in for the catalog server.

use crate::harness::*;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ACME: &str = r#"{"acme": {"id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "models": {
    "acme-1": {"id": "acme-1", "type": "chat", "name": "Acme One", "release_date": "2026-01-01",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": true, "effort": ["low", "high"]}},
        "limit": {"context": 128000, "output": 8192},
        "api": {"chat-completions": {"base_url": "https://api.acme.test/v1",
            "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible"}}}}
}}}"#;

/// Request lines and `User-Agent`s, in order.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// Serves `body` for every request and records each request line and
/// `User-Agent`.
fn catalog_server(body: &'static str) -> (String, Seen) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut ua = String::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("user-agent:") {
                    ua = v.trim().to_string();
                }
            }
            log.lock().unwrap().push((line.trim().to_string(), ua));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn models_lists_catalog_models_and_keyed_catalog_providers() {
    let e = Env::new();
    let out = run_env(&e, &["models"], &[("MOONSHOT_API_KEY", "k")]).await;
    out.ok();
    // Built-in providers list the bundled catalog's models.
    assert!(
        out.stdout
            .lines()
            .any(|l| l.starts_with("anthropic/claude-")),
        "{}",
        out.both()
    );
    assert!(!out.stdout.contains("anthropic/*"), "{}", out.both());
    // A catalog provider with a key is listed too; one without is not.
    assert!(
        out.stdout
            .lines()
            .any(|l| l.starts_with("moonshotai/kimi-")),
        "{}",
        out.both()
    );
    assert!(!out.stdout.contains("deepinfra/"), "{}", out.both());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_file_supplies_providers_limits_and_variants() {
    let e = Env::new();
    let file = e.root.join("catalog.json");
    std::fs::write(&file, ACME).unwrap();
    let path = file.display().to_string();
    let env = [
        ("CODESMITH_MODELS_PATH", path.as_str()),
        ("ACME_API_KEY", "k"),
    ];
    let out = run_env(&e, &["models"], &env).await;
    out.ok();
    assert!(
        out.stdout.lines().any(|l| l == "acme/acme-1"),
        "{}",
        out.both()
    );
    let out = run_env(&e, &["debug", "model", "acme/acme-1#high"], &env).await;
    out.ok();
    for want in [
        "https://api.acme.test/v1",
        "context:  128000",
        "output:   8192",
        "api key:  set",
        "* high",
    ] {
        assert!(out.stdout.contains(want), "{want}\n{}", out.both());
    }
}

/// Run `codesmith acp` (a long-running session front end) with `env` until
/// `done` says so or `wait` passes; the catalog server's requests.
async fn acp_requests(
    e: &Env,
    env: &[(&str, &str)],
    wait: Duration,
    done: impl Fn(&Env) -> bool,
    seen: &Seen,
) -> Vec<(String, String)> {
    let mut c = e.command(&["acp"]);
    c.env_remove("CODESMITH_DISABLE_MODELS_FETCH")
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    let deadline = std::time::Instant::now() + wait;
    while std::time::Instant::now() < deadline && !done(e) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill().await;
    let out = seen.lock().unwrap().clone();
    out
}

/// The cached catalog body, once written.
fn cached(e: &Env) -> Option<String> {
    let dir = e.home.join(".cache/codesmith/models");
    let entry = std::fs::read_dir(dir).ok()?.flatten().find(|f| {
        let n = f.file_name().to_string_lossy().into_owned();
        n.starts_with("models-dev-v2-catalog") && !n.ends_with("-meta.json")
    })?;
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(entry.path()).ok()?).ok()?;
    v["body"].as_str().map(str::to_string)
}

fn write_config(e: &Env, cfg: serde_json::Value) {
    std::fs::write(
        e.config_dir.join("codesmith.json"),
        serde_json::to_string(&cfg).unwrap(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_make_no_catalog_request_unless_fetching_is_turned_on() {
    let e = Env::new();
    let (url, seen) = catalog_server(ACME);
    write_config(
        &e,
        serde_json::json!({"catalog": {"url": url, "fetch": false}}),
    );
    let got = acp_requests(&e, &[], Duration::from_secs(3), |_| false, &seen).await;
    assert!(got.is_empty(), "{got:?}");
    assert!(cached(&e).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_refresh_the_catalog_in_the_background_when_config_turns_it_on() {
    let e = Env::new();
    let (url, seen) = catalog_server(ACME);
    write_config(
        &e,
        serde_json::json!({"catalog": {"fetch": true, "url": url}}),
    );
    let got = acp_requests(
        &e,
        &[],
        Duration::from_secs(30),
        |e| cached(e).is_some(),
        &seen,
    )
    .await;
    assert_eq!(cached(&e).as_deref(), Some(ACME));
    // A mirror is asked for the v2 catalog first; a generic User-Agent.
    assert_eq!(
        got[0].0,
        "GET /experimental/v2.0/api.json?type=chat,decision,image,video HTTP/1.1"
    );
    assert!(got[0].1.starts_with("codesmith/"), "{:?}", got[0]);
    assert!(!got[0].1.contains("tester"), "{:?}", got[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_refresh_the_catalog_with_codesmith_models_fetch() {
    let e = Env::new();
    let (url, seen) = catalog_server(ACME);
    let got = acp_requests(
        &e,
        &[
            ("CODESMITH_MODELS_FETCH", "1"),
            ("CODESMITH_MODELS_URL", &url),
        ],
        Duration::from_secs(30),
        |e| cached(e).is_some(),
        &seen,
    )
    .await;
    assert!(!got.is_empty());
    assert_eq!(cached(&e).as_deref(), Some(ACME));
}

#[tokio::test(flavor = "multi_thread")]
async fn models_refresh_updates_the_cache_on_demand() {
    let e = Env::new();
    let (url, seen) = catalog_server(ACME);
    // Forbidden outright by CODESMITH_DISABLE_MODELS_FETCH (the harness sets it).
    let out = run_env(
        &e,
        &["models", "--refresh"],
        &[("CODESMITH_MODELS_URL", &url)],
    )
    .await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert!(
        out.stderr.contains("CODESMITH_DISABLE_MODELS_FETCH"),
        "{}",
        out.both()
    );
    assert!(seen.lock().unwrap().is_empty());
    // Allowed otherwise, even with background fetching off.
    let mut c = e.command(&["models", "--refresh"]);
    c.env_remove("CODESMITH_DISABLE_MODELS_FETCH")
        .env("CODESMITH_MODELS_URL", &url)
        .env("CODESMITH_MODELS_FETCH", "0")
        .env("ACME_API_KEY", "k");
    let o = tokio::time::timeout(TIMEOUT, c.output())
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("Updated the model catalog"), "{stderr}");
    assert!(stdout.lines().any(|l| l == "acme/acme-1"), "{stdout}");
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(cached(&e).as_deref(), Some(ACME));
}

async fn run_env(e: &Env, args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut c = e.command(args);
    c.envs(env.iter().copied());
    let o = tokio::time::timeout(TIMEOUT, c.output())
        .await
        .expect("timed out")
        .unwrap();
    Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}
