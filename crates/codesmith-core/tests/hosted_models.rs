//! Hosted providers' model lists, end to end against a local fake API: the
//! request each protocol sends (path and credentials), what is kept, and
//! that a failed fetch keeps the list from before.

use codesmith_core::config::Config;
use codesmith_core::providers::{hosted, live};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// What the fake API saw: the request line and the headers, lower-cased.
#[derive(Debug, Clone)]
struct Seen {
    target: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A fake API answering every request with `status` and `body`.
async fn serve(status: u16, body: Value) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let body = body.to_string();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut s, _)) = l.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = s.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let mut lines = text.lines();
            let target = lines
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            let headers = lines
                .take_while(|l| !l.is_empty())
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                .collect();
            log.lock().unwrap().push(Seen { target, headers });
            let reason = if status == 200 { "OK" } else { "Error" };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    (origin, seen)
}

fn config(pid: &str, base: &str, key: &str) -> Config {
    serde_json::from_value(json!({"providers": {
        pid: {"settings": {"baseURL": base, "apiKey": key}}
    }}))
    .unwrap()
}

async fn refresh(cfg: &Config, pid: &str, path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let m = cfg.resolve_model(&format!("{pid}/_"))?;
    hosted::refresh(&reqwest::Client::new(), path, pid, &m.provider).await
}

#[tokio::test]
async fn anthropic_is_asked_for_its_whole_list_with_its_key_header() {
    let (origin, seen) = serve(
        200,
        json!({"data": [
            {"type": "model", "id": "claude-future-9", "display_name": "Claude Future 9",
             "created_at": "2026-10-01T00:00:00Z"}
        ], "has_more": false}),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live-models.json");
    let cfg = config("anthropic", &format!("{origin}/v1"), "sk-ant-test");
    let ids = refresh(&cfg, "anthropic", &path).await.unwrap();
    assert_eq!(ids, ["claude-future-9"]);

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].target, "/v1/models?limit=1000");
    assert_eq!(seen[0].header("x-api-key"), Some("sk-ant-test"));
    assert!(seen[0].header("anthropic-version").is_some());

    let kept = live::load_in(&path, "anthropic").unwrap();
    assert_eq!(kept["account"], hosted::account(Some("sk-ant-test")));
    let models = live::derive("anthropic", &kept, &[]);
    assert_eq!(models[0].name, "Claude Future 9");
    assert!(models[0].time.released > 0);
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("sk-ant-test"),
        "the key is never kept"
    );
}

#[tokio::test]
async fn openai_compatible_providers_send_a_bearer_key_and_keep_chat_models() {
    let (origin, seen) = serve(
        200,
        json!({"object": "list", "data": [
            {"id": "llama-9-70b", "context_window": 131072},
            {"id": "whisper-large-v3"}
        ]}),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live-models.json");
    let cfg = config("groq", &format!("{origin}/openai/v1"), "gsk-test");
    let ids = refresh(&cfg, "groq", &path).await.unwrap();
    assert_eq!(ids, ["llama-9-70b"], "speech models are kept but not chat");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].target, "/openai/v1/models");
    assert_eq!(seen[0].header("authorization"), Some("Bearer gsk-test"));
}

#[tokio::test]
async fn gemini_is_asked_with_a_page_size() {
    let (origin, seen) = serve(
        200,
        json!({"models": [{"name": "models/gemini-9-pro", "inputTokenLimit": 2000000,
                           "supportedGenerationMethods": ["generateContent"]}]}),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live-models.json");
    let cfg = config("google", &format!("{origin}/v1beta"), "AIza-test");
    let ids = refresh(&cfg, "google", &path).await.unwrap();
    assert_eq!(ids, ["gemini-9-pro"]);
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen[0].target.starts_with("/v1beta/models?pageSize=1000"),
        "{}",
        seen[0].target
    );
    let keyed = seen[0].header("x-goog-api-key") == Some("AIza-test")
        || seen[0].target.contains("key=AIza-test");
    assert!(keyed, "the key goes with the request: {seen:?}");
}

#[tokio::test]
async fn a_rejected_key_keeps_the_list_from_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live-models.json");
    let before = vec![json!({"id": "kept-model", "kind": "chat"})];
    live::save_in(&path, "mistral", &hosted::account(Some("old")), "", before).unwrap();

    let (origin, _) = serve(401, json!({"error": {"message": "invalid key"}})).await;
    let cfg = config("mistral", &format!("{origin}/v1"), "bad-key");
    let err = refresh(&cfg, "mistral", &path).await.unwrap_err();
    assert!(format!("{err:#}").contains("401"), "{err:#}");
    let kept = live::load_in(&path, "mistral").unwrap();
    assert_eq!(kept["models"][0]["id"], "kept-model");
}

#[tokio::test]
async fn an_empty_answer_keeps_the_list_from_before() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live-models.json");
    let before = vec![json!({"id": "kept-model", "kind": "chat"})];
    live::save_in(&path, "deepseek", &hosted::account(Some("k")), "", before).unwrap();
    let (origin, _) = serve(200, json!({"data": []})).await;
    let cfg = config("deepseek", &format!("{origin}/v1"), "k");
    assert!(refresh(&cfg, "deepseek", &path).await.is_err());
    assert_eq!(
        live::load_in(&path, "deepseek").unwrap()["models"][0]["id"],
        "kept-model"
    );
}

#[tokio::test]
async fn a_list_kept_for_another_key_is_not_used() {
    let cfg = config("xai", "https://api.x.ai/v1", "new-key");
    assert_eq!(
        cfg.live_account("xai"),
        Some(hosted::account(Some("new-key")))
    );
    let kept =
        json!({"account": hosted::account(Some("old-key")), "fetched": u64::MAX / 2, "models": []});
    assert!(hosted::stale(
        Some(&kept),
        &cfg.live_account("xai").unwrap(),
        0
    ));
}
