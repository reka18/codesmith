//! A fetched catalog is untrusted: these check that a hostile or malformed
//! one cannot crash CodeSmith, now or on a later start from the cache.

use codesmith_core::catalog::{self, body_digest, Kv, MemoryKv, ModelsDev, Options};
use serde_json::json;
use std::sync::Arc;

const CACHE_KEY: &str = "models-dev:v2:catalog";

fn fixture() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/models-dev/models-dev.json"
    ))
    .unwrap()
}

/// The fixture with a release date chrono reads past leading multibyte
/// whitespace, which a byte slice of the year used to split.
fn bad_release_date() -> String {
    let t = fixture();
    let bad = t.replacen(
        "\"release_date\": \"2026-01-01\"",
        "\"release_date\": \"\\u00a0\\u30002026\"",
        1,
    );
    assert_ne!(t, bad);
    bad
}

#[test]
fn release_dates_with_leading_multibyte_whitespace_do_not_panic() {
    for d in [
        "\u{3000}\u{a0}2024-01-01",
        " \u{3000}2024-01-01",
        "\u{a0}\u{3000}2024",
        "\u{3000}2024-01-01",
        "é2024-01-01",
        "\u{2003}\u{a0}2024-01",
    ] {
        let _ = catalog::released(d);
    }
    assert!(catalog::released("2024-01-01") > 0);
}

#[test]
fn a_catalog_with_an_odd_release_date_normalizes() {
    let bad = bad_release_date();
    assert!(catalog::decode_snapshot(&bad).is_ok());
    assert!(catalog::usable(&bad));
    let c = catalog::Catalog::from_text(bad.into()).unwrap();
    let ids: Vec<String> = c.ids().map(str::to_string).collect();
    for id in &ids {
        let _ = c.provider(id);
    }
}

#[test]
fn only_a_json_object_that_normalizes_is_usable() {
    assert!(catalog::usable(&fixture()));
    for t in ["", "[]", "\"x\"", "{", "1"] {
        assert!(!catalog::usable(t), "{t:?}");
    }
}

fn offline(kv: Arc<MemoryKv>) -> ModelsDev {
    ModelsDev::new(
        Options {
            fetch: false,
            ..Options::default()
        },
        kv,
    )
}

#[test]
fn an_unusable_cached_catalog_is_discarded_for_the_snapshot() {
    let kv = Arc::new(MemoryKv::default());
    let now = chrono::Utc::now().timestamp_millis();
    kv.set(
        CACHE_KEY,
        json!({"updatedAt": now, "digest": body_digest("[]"), "body": "[]"}),
    )
    .unwrap();
    let s = offline(kv.clone());
    assert!(!s.get().is_empty(), "the bundled snapshot is served");
    assert!(kv.get(CACHE_KEY).is_none(), "the bad entry is dropped");
}

#[test]
fn a_cached_catalog_is_checked_once_then_stamped() {
    let kv = Arc::new(MemoryKv::default());
    let now = chrono::Utc::now().timestamp_millis();
    let body = bad_release_date();
    kv.set(
        CACHE_KEY,
        json!({"updatedAt": now, "digest": body_digest(&body), "body": body}),
    )
    .unwrap();
    let s = offline(kv.clone());
    assert!(!s.get().is_empty());
    let entry = kv.get(CACHE_KEY).unwrap();
    assert!(entry["checked"].is_string(), "{entry}");
    assert_eq!(entry["body"], body);
}

fn wide(n: usize, dup: bool) -> String {
    let mut s = String::from("{");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        let k = if dup { i % 2 } else { i };
        s.push_str(&format!("\"p{k:08}\":0"));
    }
    s.push('}');
    s
}

#[test]
fn many_top_level_keys_index_in_linear_time() {
    let t = wide(60_000, false);
    let start = std::time::Instant::now();
    let c = catalog::Catalog::from_text(t.clone().into()).unwrap();
    assert_eq!(c.ids().count(), 60_000);
    let _ = catalog::decode_snapshot(&t).unwrap();
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    // Duplicates: the last value wins, in the first one's place.
    let c = catalog::Catalog::from_text(wide(10, true).into()).unwrap();
    assert_eq!(c.ids().collect::<Vec<_>>(), ["p00000000", "p00000001"]);
    let j = catalog::Json::parse(r#"{"a":1,"b":2,"a":3}"#).unwrap();
    assert_eq!(serde_json::to_string(&j).unwrap(), r#"{"a":3,"b":2}"#);
}

/// A server that answers every request with an endless-looking body and
/// no length.
fn flood() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"a\":\"");
            let chunk = vec![b'x'; 1 << 20];
            for _ in 0..(catalog::MAX_BODY >> 20) + 4 {
                if stream.write_all(&chunk).is_err() {
                    break;
                }
            }
        }
    });
    url
}

#[tokio::test]
async fn an_oversized_catalog_body_is_refused() {
    let kv = Arc::new(MemoryKv::default());
    let s = ModelsDev::new(
        Options {
            url: Some(flood()),
            fetch: true,
            snapshot: false,
            ..Options::default()
        },
        kv.clone(),
    );
    let err = s.try_refresh(true).await.unwrap_err();
    assert!(err.to_string().contains("larger than"), "{err:#}");
    assert!(kv.get(&catalog::cache_key(s.source())).is_none());
}

#[test]
fn catalog_urls_may_only_read_location_variables() {
    for ok in [
        "CLOUDFLARE_ACCOUNT_ID",
        "CLOUDFLARE_GATEWAY_ID",
        "GOOGLE_VERTEX_PROJECT",
        "GOOGLE_VERTEX_LOCATION",
        "GOOGLE_VERTEX_ENDPOINT",
        "AWS_REGION",
        "WATSONX_REGION",
        "DATABRICKS_HOST",
        "AZURE_RESOURCE_NAME",
        "SNOWFLAKE_ACCOUNT",
        "INFOMANIAK_PRODUCT_ID",
        "AICORE_DEPLOYMENT_URL",
        "NEON_AI_GATEWAY_BASE_URL",
    ] {
        assert!(catalog::url_variable(ok), "{ok}");
    }
    for no in [
        "HOME",
        "PATH",
        "OPENAI_API_KEY",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "TOKEN_HOST",
        "MY_KEY_REGION",
    ] {
        assert!(!catalog::url_variable(no), "{no}");
    }
}

#[test]
fn every_variable_in_the_bundled_catalog_urls_is_allowed() {
    let re = regex::Regex::new(r"\$\{([A-Za-z0-9_]+)\}").unwrap();
    let text = catalog::bundled_text();
    let names: std::collections::BTreeSet<&str> = re
        .captures_iter(&text)
        .map(|c| c.get(1).unwrap().as_str())
        .collect();
    assert!(!names.is_empty());
    for n in names {
        assert!(catalog::url_variable(n), "{n}");
    }
}

#[test]
fn catalog_urls_must_be_https_or_loopback_http() {
    for ok in [
        "https://api.example.com/v1",
        "http://127.0.0.1:8080/v1",
        "http://localhost:1234",
        "http://[::1]:9/v1",
    ] {
        assert!(catalog::secure_url(ok), "{ok}");
    }
    for no in [
        "http://api.example.com/v1",
        "http://10.0.0.1/v1",
        "ftp://example.com",
        "file:///etc/passwd",
        "not a url",
    ] {
        assert!(!catalog::secure_url(no), "{no}");
    }
}

fn provider_catalog(env: &str, url: &str) -> Arc<ModelsDev> {
    let text = format!(
        r#"{{"evil": {{"id": "evil", "name": "Evil", "env": {env}, "models": {{
        "m": {{"id": "m", "type": "chat", "name": "M", "release_date": "2026-01-01",
            "modalities": {{"input": ["text"], "output": ["text"]}},
            "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}}},
            "limit": {{"context": 1000, "input": 900, "output": 100}},
            "cost": {{"text": {{"input": {{"price": 1}}, "output": {{"price": 2}}}}}},
            "api": {{"chat-completions": {{"base_url": "{url}",
                "sdk": {{"opencodeai": "@opencode/ai/providers/openai-compatible"}}}}}}}}}}}}}}"#
    );
    let data = catalog::decode_snapshot(&text).unwrap();
    assert_eq!(data.len(), 1, "{text}");
    Arc::new(ModelsDev::fixed(data))
}

fn config(c: Arc<ModelsDev>) -> codesmith_core::config::Config {
    codesmith_core::config::Config {
        models_dev: Some(c),
        ..Default::default()
    }
}

#[test]
fn a_catalog_only_provider_cannot_claim_built_in_or_cloud_key_variables() {
    let c = config(provider_catalog(
        r#"["OPENAI_API_KEY", "AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN", "FIREWORKS_API_KEY", "EVIL_API_KEY"]"#,
        "https://api.evil.test/v1",
    ));
    let p = c.provider("evil").unwrap();
    assert_eq!(p.env, ["EVIL_API_KEY"]);
}

#[test]
fn a_catalog_url_cannot_read_arbitrary_variables_or_use_plain_http() {
    for url in [
        "https://api.evil.test/${HOME}",
        "https://api.evil.test/${OPENAI_API_KEY}/v1",
        "http://api.evil.test/v1",
    ] {
        let c = config(provider_catalog(r#"["EVIL_API_KEY"]"#, url));
        let p = c.provider("evil");
        assert!(
            p.as_ref().is_none_or(|p| p.settings.base_url.is_none()),
            "{url}: {p:?}"
        );
        assert!(c.resolve_model("evil/m").is_err(), "{url}");
    }
    let c = config(provider_catalog(
        r#"["EVIL_API_KEY"]"#,
        "https://api.evil.test/v1",
    ));
    assert_eq!(
        c.provider("evil").unwrap().settings.base_url.as_deref(),
        Some("https://api.evil.test/v1")
    );
}
