//! Refreshable provider logins and xAI's SuperGrok device login, ported
//! from OpenCode's `plugin/provider-xai.test.ts` and `plugin.test.ts` (MIT)
//! against a scripted xAI issuer on loopback. The credential store's own
//! ports are in `src/providers/credentials.rs`. See
//! `crates/codesmith-core/PARITY.md`.

use codesmith_ai::Authorizer;
use codesmith_core::providers::credentials::{self, New};
use codesmith_core::providers::xai;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Issuer {
    /// Each request: path and form.
    seen: Vec<(String, String)>,
    /// Device polls left before approval.
    pending: u32,
}

async fn issuer(state: Arc<Mutex<Issuer>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head, body) = loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len: usize = text[..i]
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            break (text[..i].to_string(), text[i + 4..i + 4 + len].to_string());
                        }
                    }
                };
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, payload) = {
                    let mut st = state.lock().unwrap();
                    st.seen.push((path.clone(), body.clone()));
                    if path.ends_with("/device/code") {
                        (
                            200,
                            json!({"device_code": "dev-1", "user_code": "ABCD", "verification_uri": "https://x.test/device", "interval": 1, "expires_in": 60}),
                        )
                    } else if body.contains("device_code=dev-1") && st.pending > 0 {
                        st.pending -= 1;
                        (400, json!({"error": "authorization_pending"}))
                    } else if body.contains("grant_type=refresh_token") {
                        (
                            200,
                            json!({"access_token": "fresh-access", "refresh_token": "rotated-refresh", "expires_in": 3600}),
                        )
                    } else {
                        (
                            200,
                            json!({"access_token": "first-access", "refresh_token": "first-refresh", "expires_in": 3600}),
                        )
                    }
                };
                let payload = payload.to_string();
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/oauth2")
}

// provider-xai.test.ts: "registers device OAuth and API key methods"
#[test]
fn oc_xai_registers_device_oauth_and_api_key_methods() {
    assert_eq!(
        xai::methods(),
        [
            ("device", "SuperGrok Subscription"),
            ("key", "Manually enter API Key")
        ]
    );
    // CodeSmith ships no OAuth client: without one the login explains.
    let none: codesmith_core::providers::Env = Arc::new(|_: &str| None);
    let e = xai::client_id(None, &Default::default(), &none)
        .unwrap_err()
        .to_string();
    assert!(e.contains("CodeSmith ships none"), "{e}");
    assert!(e.contains(xai::CLIENT_ID_VAR), "{e}");
    let env: codesmith_core::providers::Env =
        Arc::new(|k: &str| (k == xai::CLIENT_ID_VAR).then(|| "mine".to_string()));
    assert_eq!(
        xai::client_id(None, &Default::default(), &env).unwrap(),
        "mine"
    );
    assert_eq!(
        xai::client_id(Some("flag"), &Default::default(), &env).unwrap(),
        "flag"
    );
}

// provider-xai.test.ts: "migrates browser OAuth credentials to the device method"
#[test]
fn oc_xai_migrates_browser_oauth_credentials_to_the_device_method() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    let original = credentials::create_in(
        &file,
        New {
            provider: "xai".into(),
            label: Some("personal".into()),
            value: json!({"type": "oauth", "method": "browser", "access": "access", "refresh": "refresh", "expires": 123, "metadata": {"account": "account"}}),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(xai::migrate(&file).unwrap(), 1);
    let got = credentials::get_in(&file, &original.id).unwrap();
    assert_eq!(got.label.as_deref(), Some("personal"));
    assert_eq!(
        got.value,
        json!({"type": "oauth", "method": "device", "access": "access", "refresh": "refresh", "expires": 123, "metadata": {"account": "account"}})
    );
    assert_eq!(xai::migrate(&file).unwrap(), 0);
}

// provider-xai.test.ts: "enables xAI Responses WebSockets"
#[test]
fn oc_xai_enables_responses_websockets() {
    let config: codesmith_core::config::Config = serde_json::from_value(json!({
        "providers": {"xai": {"protocol": "openai-responses", "settings": {"baseURL": "https://api.x.ai/v1", "apiKey": "k"}}}
    }))
    .unwrap();
    let m = config.resolve_model("xai/grok-4.6").unwrap();
    assert!(m.websocket.is_some(), "xAI steps go over a WebSocket");
}

#[tokio::test(flavor = "multi_thread")]
async fn xai_device_login_saves_the_login_with_its_client_and_refreshes_at_the_issuer() {
    let state = Arc::new(Mutex::new(Issuer {
        pending: 1,
        ..Default::default()
    }));
    let base = issuer(state.clone()).await;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    let e = xai::Endpoints {
        client_id: "my-client".into(),
        issuer: base.clone(),
        scope: xai::SCOPE.into(),
    };
    let client = reqwest::Client::new();
    let code = xai::start(&client, &e).await.unwrap();
    assert_eq!(code.user_code, "ABCD");
    let login = xai::finish(&client, &e, &code, std::time::Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(login["method"], "device");
    assert_eq!(login["access"], "first-access");
    assert_eq!(login["metadata"]["clientID"], "my-client");
    let seen = state.lock().unwrap().seen.clone();
    assert!(seen[0].1.contains("client_id=my-client"), "{seen:?}");
    assert!(!seen[0].1.contains("grok-cli"), "no other product's scope");
    let none: codesmith_core::providers::Env = Arc::new(|_: &str| None);
    let fresh = xai::refresh(&client, &base, &login, &none).await.unwrap();
    assert_eq!(fresh["access"], "fresh-access");
    assert_eq!(fresh["refresh"], "rotated-refresh");
    assert_eq!(fresh["metadata"]["clientID"], "my-client");
    let last = state.lock().unwrap().seen.last().cloned().unwrap();
    assert_eq!(last.0, "/oauth2/token");
    assert!(
        last.1.contains("client_id=my-client") && last.1.contains("refresh_token=first-refresh")
    );
    // Saved as a new credential; an earlier key is kept.
    codesmith_core::auth::set_in(&file, "xai", "xai-key").unwrap();
    let state2 = Arc::new(Mutex::new(Issuer::default()));
    let base2 = issuer(state2).await;
    xai::login(
        &client,
        &file,
        &xai::Endpoints {
            issuer: base2,
            ..e.clone()
        },
        |_| {},
    )
    .await
    .unwrap();
    let all = credentials::list_in(&file, "xai");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].value["key"], "xai-key");
    assert_eq!(all[1].value["type"], "oauth");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

fn expired(at: &str) -> Value {
    json!({"type": "oauth", "method": "device", "access": "expired-access", "refresh": "fixture-refresh",
        "expires": codesmith_core::providers::oauth::now_ms() + 60_000, "metadata": {"clientID": at}})
}

async fn authorize(a: &xai::Refreshing) -> String {
    let client = reqwest::Client::new();
    let mut req = client.get("https://api.x.ai/v1/models").build().unwrap();
    a.authorize(&client, &mut req).await.unwrap();
    req.headers()["authorization"].to_str().unwrap().to_string()
}

// plugin.test.ts: "refreshes expired OAuth credentials through the context during activation"
#[tokio::test(flavor = "multi_thread")]
async fn oc_refreshes_expired_oauth_credentials_before_they_are_used() {
    let state = Arc::new(Mutex::new(Issuer::default()));
    let base = issuer(state.clone()).await;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    credentials::create_in(
        &file,
        New {
            provider: "xai".into(),
            label: Some("Fixture".into()),
            value: expired("fixture-client"),
            ..Default::default()
        },
    )
    .unwrap();
    let a = xai::Refreshing {
        file: file.clone(),
        issuer: base,
    };
    // Due within five minutes: refreshed once, however many requests need it.
    let (one, two) = tokio::join!(authorize(&a), authorize(&a));
    assert_eq!(one, "Bearer fresh-access");
    assert_eq!(two, "Bearer fresh-access");
    let refreshes = state
        .lock()
        .unwrap()
        .seen
        .iter()
        .filter(|s| s.1.contains("grant_type=refresh_token"))
        .count();
    assert_eq!(refreshes, 1);
    let saved = codesmith_core::auth::entry_in(&file, "xai").unwrap();
    assert_eq!(saved["refresh"], "rotated-refresh");
    assert_eq!(saved["metadata"]["clientID"], "fixture-client");
    assert_eq!(authorize(&a).await, "Bearer fresh-access");
    assert_eq!(
        state.lock().unwrap().seen.len(),
        1,
        "a fresh login is not refreshed"
    );
}

// plugin.test.ts: "activates a saved credential through the context"
#[tokio::test(flavor = "multi_thread")]
async fn oc_activates_a_saved_credential() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    let login = |access: &str| json!({"type": "oauth", "method": "device", "access": access, "refresh": "r", "expires": 0});
    let first = credentials::create_in(
        &file,
        New {
            provider: "xai".into(),
            label: Some("First".into()),
            value: login("first"),
            ..Default::default()
        },
    )
    .unwrap();
    credentials::create_in(
        &file,
        New {
            provider: "xai".into(),
            label: Some("Second".into()),
            value: login("second"),
            ..Default::default()
        },
    )
    .unwrap();
    let a = xai::Refreshing {
        file: file.clone(),
        issuer: "http://127.0.0.1:9/never".into(),
    };
    assert_eq!(authorize(&a).await, "Bearer second");
    assert!(credentials::activate_in(&file, &first.id).unwrap());
    let active = credentials::list_in(&file, "xai").pop().unwrap();
    assert_eq!(
        (active.id, active.label.as_deref()),
        (first.id, Some("First"))
    );
    assert_eq!(authorize(&a).await, "Bearer first");
}
