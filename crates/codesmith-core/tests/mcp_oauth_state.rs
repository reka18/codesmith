//! Security review 8, finding 8: an OAuth callback with the wrong `state`
//! (a stale tab, or a page that found the loopback port) used to end the
//! login with an error. It is now answered 400 and the login keeps waiting
//! for the real redirect. A scripted local server plays the authorization
//! server, and an HTTP client plays the browser.

mod support;

use codesmith_core::mcp::oauth;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// An authorization server and MCP endpoint at one origin; the token
/// requests' bodies are kept.
async fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let tokens: Arc<Mutex<Vec<String>>> = Arc::default();
    let (origin, log) = (base.clone(), tokens.clone());
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let (base, log) = (origin.clone(), log.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
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
                            break (
                                text[..i].to_string(),
                                String::from_utf8_lossy(&buf[i + 4..i + 4 + len]).to_string(),
                            );
                        }
                    }
                };
                let first: Vec<&str> = head
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .collect();
                let path = first
                    .get(1)
                    .unwrap_or(&"/")
                    .split('?')
                    .next()
                    .unwrap()
                    .to_string();
                let (status, body) = match (first[0], path.as_str()) {
                    ("GET", "/.well-known/oauth-authorization-server") => (
                        200,
                        json!({
                            "issuer": base, "authorization_endpoint": format!("{base}/authorize"),
                            "token_endpoint": format!("{base}/token"),
                            "registration_endpoint": format!("{base}/register"),
                            "code_challenge_methods_supported": ["S256"]
                        })
                        .to_string(),
                    ),
                    ("POST", "/register") => (201, json!({"client_id": "registered"}).to_string()),
                    ("POST", "/token") => {
                        log.lock().unwrap().push(body);
                        (
                            200,
                            json!({"access_token": "access", "token_type": "Bearer", "expires_in": 3600})
                                .to_string(),
                        )
                    }
                    ("POST", "/mcp") => (401, String::new()),
                    _ => (404, String::new()),
                };
                let out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(out.as_bytes()).await;
            });
        }
    });
    (base, tokens)
}

/// Play the browser: send each of `forged` (extra query pairs) to the
/// callback first, recording the statuses, then the real redirect.
async fn browse(
    url: String,
    forged: Vec<Vec<(&'static str, String)>>,
    deny: bool,
    statuses: Arc<Mutex<Vec<u16>>>,
) {
    let u = reqwest::Url::parse(&url).unwrap();
    let q: BTreeMap<String, String> = u
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let callback = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
    let http = reqwest::Client::new();
    for pairs in forged {
        let mut c = callback.clone();
        for (k, v) in &pairs {
            c.query_pairs_mut().append_pair(k, v);
        }
        let s = http.get(c).send().await.unwrap().status().as_u16();
        statuses.lock().unwrap().push(s);
    }
    let mut c = callback.clone();
    if deny {
        c.query_pairs_mut().append_pair("error", "access_denied");
    } else {
        c.query_pairs_mut().append_pair("code", "accepted");
    }
    c.query_pairs_mut().append_pair("state", &q["state"]);
    let s = http.get(c).send().await.unwrap().status().as_u16();
    statuses.lock().unwrap().push(s);
}

async fn login(
    path: &Path,
    url: &str,
    forged: Vec<Vec<(&'static str, String)>>,
    deny: bool,
) -> (anyhow::Result<oauth::Entry>, Vec<u16>) {
    let statuses: Arc<Mutex<Vec<u16>>> = Arc::default();
    let s = statuses.clone();
    let task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::default();
    let t = task.clone();
    let setting: oauth::Setting = serde_json::from_value(json!({"port": 0})).unwrap();
    let r = oauth::login(
        &reqwest::Client::new(),
        path,
        "srv",
        url,
        Some(&setting),
        move |u| {
            *t.lock().unwrap() = Some(tokio::spawn(browse(u.to_string(), forged, deny, s)));
        },
    )
    .await;
    // The browser reads its last answer after the login has its redirect.
    let browser = task.lock().unwrap().take();
    if let Some(b) = browser {
        b.await.unwrap();
    }
    let st = statuses.lock().unwrap().clone();
    (r, st)
}

fn pairs(v: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
    v.iter().map(|(k, v)| (*k, v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callback_with_the_wrong_state_is_refused_and_the_login_goes_on() {
    support::isolate();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("mcp-auth.json");
    let (base, tokens) = serve().await;
    let url = format!("{base}/mcp");
    let forged = vec![
        pairs(&[("code", "evil"), ("state", "wrong")]),
        pairs(&[("code", "evil")]),
        pairs(&[("code", "evil"), ("state", "")]),
        pairs(&[("error", "access_denied"), ("state", "wrong")]),
        pairs(&[("code", "evil"), ("state", "ünïcødé\u{202e}")]),
    ];
    let n = forged.len();
    let (r, statuses) = login(&path, &url, forged, false).await;
    let entry = r.unwrap();
    assert_eq!(entry.access_token, "access");
    assert_eq!(statuses.len(), n + 1, "{statuses:?}");
    assert!(statuses[..n].iter().all(|s| *s == 400), "{statuses:?}");
    assert_eq!(statuses[n], 200);
    // Only the real code was exchanged.
    let t = tokens.lock().unwrap().clone();
    assert_eq!(t.len(), 1, "{t:?}");
    assert!(
        t[0].contains("code=accepted") && !t[0].contains("evil"),
        "{t:?}"
    );
    assert_eq!(oauth::load(&path, "srv", &url), Some(entry));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_right_state_still_completes_and_a_matching_error_still_ends_the_login() {
    support::isolate();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("mcp-auth.json");
    let (base, _) = serve().await;
    let url = format!("{base}/mcp");
    let (r, statuses) = login(&path, &url, vec![], false).await;
    r.unwrap();
    assert_eq!(statuses, [200]);
    // The user declining, on the real redirect, still ends it.
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("mcp-auth.json");
    let forged = vec![pairs(&[("error", "x"), ("state", "wrong")])];
    let (r, statuses) = login(&path, &url, forged, true).await;
    let e = r.unwrap_err().to_string();
    assert!(e.contains("authorization denied: access_denied"), "{e}");
    // Both answered 400: the forged one, and the real one with its error
    // page.
    assert_eq!(statuses, [400, 400]);
    assert_eq!(oauth::load(&path, "srv", &url), None);
}
