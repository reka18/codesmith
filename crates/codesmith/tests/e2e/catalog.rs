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

#[tokio::test(flavor = "multi_thread")]
async fn sessions_refresh_the_catalog_in_the_background_with_a_generic_user_agent() {
    let e = Env::new();
    let (url, seen) = catalog_server(ACME);
    let mut c = e.command(&["acp"]);
    c.env_remove("CODESMITH_DISABLE_MODELS_FETCH")
        .env("CODESMITH_MODELS_URL", &url)
        .stdin(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    let cache = e.home.join(".cache/codesmith/models");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let written = loop {
        let files: Vec<_> = std::fs::read_dir(&cache)
            .map(|d| d.flatten().map(|f| f.path()).collect())
            .unwrap_or_default();
        let entry = files.iter().find(|f| {
            let n = f.file_name().unwrap().to_string_lossy().into_owned();
            n.starts_with("models-dev-v2-catalog-") && !n.ends_with("-meta.json")
        });
        if let Some(entry) = entry {
            break std::fs::read_to_string(entry).unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no cached catalog in {}",
            cache.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let _ = child.kill().await;
    let cached: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(cached["body"], ACME);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen[0].0,
        "GET /experimental/v2.0/api.json?type=chat,decision,image,video HTTP/1.1"
    );
    assert!(seen[0].1.starts_with("codesmith/"), "{:?}", seen[0]);
    assert!(!seen[0].1.contains("tester"), "{:?}", seen[0]);
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
