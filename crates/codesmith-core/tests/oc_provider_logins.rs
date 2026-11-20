//! Provider browser logins (OpenCode's `plugin/provider-poe.test.ts`,
//! `provider-gitlab-oauth.test.ts`, `provider-snowflake-cortex.test.ts` and
//! `provider-digitalocean.test.ts`), against local servers standing in for
//! the providers' OAuth endpoints; the "browser" is a plain HTTP client.

use codesmith_core::providers::{self, oauth, poe, Input};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn env(pairs: &[(&str, &str)]) -> providers::Env {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Arc::new(move |k| m.get(k).cloned())
}

/// One answer of the fixture server; `gate` holds it until released.
struct Reply {
    status: u16,
    body: String,
    gate: Option<tokio::sync::oneshot::Receiver<()>>,
    started: Option<tokio::sync::oneshot::Sender<()>>,
}

fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        body: body.into(),
        gate: None,
        started: None,
    }
}

/// A recorded request: method, path, headers (lowercase names) and body.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

impl Seen {
    fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

#[derive(Clone)]
struct Server {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    replies: Arc<Mutex<Vec<Reply>>>,
}

impl Server {
    fn push(&self, r: Reply) {
        self.replies.lock().unwrap().push(r);
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn read_request(s: &mut tokio::net::TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 65536];
    loop {
        let n = s.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(i) = text.find("\r\n\r\n") {
            let len = text[..i]
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if buf.len() >= i + 4 + len {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(Seen {
        method,
        path,
        headers,
        body: body.to_string(),
    })
}

/// A local server answering requests with the queued replies, in order.
async fn server() -> Server {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let srv = Server {
        url: format!("http://{}", l.local_addr().unwrap()),
        seen: Default::default(),
        replies: Default::default(),
    };
    let s2 = srv.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let srv = s2.clone();
            tokio::spawn(async move {
                let Some(req) = read_request(&mut s).await else {
                    return;
                };
                srv.seen.lock().unwrap().push(req);
                let r = {
                    let mut q = srv.replies.lock().unwrap();
                    (!q.is_empty()).then(|| q.remove(0))
                };
                let Some(mut r) = r else {
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 500 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                    return;
                };
                if let Some(t) = r.started.take() {
                    let _ = t.send(());
                }
                if let Some(g) = r.gate.take() {
                    let _ = g.await;
                }
                let resp = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    r.status,
                    r.body.len(),
                    r.body
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    srv
}

/// The browser following a redirect: status and page.
async fn browse(url: &str) -> reqwest::Result<(u16, String)> {
    let r = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await?;
    Ok((r.status().as_u16(), r.text().await.unwrap_or_default()))
}

fn params(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// The redirect a provider would send the browser to.
fn redirect(auth_url: &str, extra: &[(&str, &str)]) -> String {
    let p = params(auth_url);
    let mut u = url::Url::parse(&p["redirect_uri"]).unwrap();
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("state", &p["state"]);
        q.append_pair("code", "auth-code");
    }
    let mut pairs: Vec<(String, String)> = u.query_pairs().into_owned().collect();
    for (k, v) in extra {
        pairs.retain(|(pk, _)| pk != k);
        pairs.push((k.to_string(), v.to_string()));
    }
    u.query_pairs_mut().clear().extend_pairs(pairs);
    u.to_string()
}

fn input<'a>(
    provider: &'a str,
    s: &'a codesmith_core::config::ProviderSettings,
    stored: Option<Value>,
    key: Option<&str>,
    e: providers::Env,
) -> Input<'a> {
    Input {
        provider_id: provider,
        settings: s,
        configured_url: s.base_url.as_deref(),
        configured_protocol: None,
        package: None,
        catalog_url: None,
        model_id: "test-model",
        api_key: key.map(str::to_string),
        stored,
        env: e,
    }
}

const WAIT: Duration = Duration::from_secs(30);

// plugin/provider-poe.test.ts

fn poe_endpoints(srv: &Server) -> poe::Endpoints {
    poe::Endpoints {
        client_id: "my-poe-client".into(),
        authorize: "https://poe.com/oauth/authorize".into(),
        token: format!("{}/token", srv.url),
        issuer: "https://poe.com".into(),
    }
}

#[tokio::test]
async fn oc_poe_registers_poe_browser_oauth_alongside_generic_key_and_environment_methods_without_fetching(
) {
    let p = providers::builtin("poe").unwrap();
    assert_eq!(p.env, ["POE_API_KEY"]);
    assert_eq!(poe::LABEL, "Login with Poe (browser)");
    let srv = server().await;
    let login = poe::start(&poe_endpoints(&srv)).await.unwrap();
    assert!(login.url.starts_with("https://poe.com/oauth/authorize?"));
    assert!(srv.seen().is_empty(), "starting a login fetches nothing");
    // CodeSmith ships no Poe OAuth client: the login asks for one.
    let e = poe::client_id(None, &Default::default(), &env(&[]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("CODESMITH_POE_CLIENT_ID"), "{e}");
    assert!(e.contains("POE_API_KEY"), "{e}");
    assert_eq!(
        poe::client_id(
            None,
            &Default::default(),
            &env(&[("CODESMITH_POE_CLIENT_ID", "mine")])
        )
        .unwrap(),
        "mine"
    );
}

#[tokio::test]
async fn oc_poe_exchanges_a_pkce_code_for_a_native_poe_credential() {
    // An expiry in seconds, null, or none at all.
    for (expiry, give) in [
        (json!(3600), true),
        (Value::Null, true),
        (Value::Null, false),
    ] {
        let srv = server().await;
        let e = poe_endpoints(&srv);
        let login = poe::start(&e).await.unwrap();
        let url = login.url.clone();
        let p = params(&url);
        assert_eq!(p["response_type"], "code");
        assert_eq!(p["client_id"], "my-poe-client");
        assert_eq!(p["scope"], "apikey:create");
        assert_eq!(p["code_challenge_method"], "S256");
        assert!(!p["state"].is_empty());
        let cb = url::Url::parse(&p["redirect_uri"]).unwrap();
        assert_eq!(cb.host_str(), Some("127.0.0.1"));
        assert_eq!(cb.path(), "/callback");
        let body = if give {
            json!({"api_key": " poe-key ", "api_key_expires_in": expiry})
        } else {
            json!({"api_key": " poe-key "})
        };
        srv.push(reply(200, body.to_string()));
        let extra: &[(&str, &str)] = if expiry.is_null() {
            &[]
        } else {
            &[("iss", "https://poe.com")]
        };
        let to = redirect(&url, extra);
        let before = oauth::now_ms();
        let client = reqwest::Client::new();
        let (done, page) =
            tokio::join!(poe::finish(&client, login, None, |_| {}, WAIT), browse(&to));
        let saved = done.unwrap();
        let (status, page) = page.unwrap();
        assert_eq!(status, 200);
        assert!(page.contains("Authorization successful"), "{page}");
        let seen = srv.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            (seen[0].method.as_str(), seen[0].path.as_str()),
            ("POST", "/token")
        );
        assert!(seen[0].headers["content-type"].contains("application/x-www-form-urlencoded"));
        let form = seen[0].form();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["client_id"], "my-poe-client");
        assert_eq!(form["code"], "auth-code");
        assert_eq!(form["redirect_uri"], p["redirect_uri"]);
        assert_eq!(
            codesmith_core::mcp::oauth::challenge(&form["code_verifier"]),
            p["code_challenge"]
        );
        assert_eq!(saved["access"], "poe-key");
        assert_eq!(saved["refresh"], "");
        let expires = saved["expires"].as_u64().unwrap();
        if expiry.is_null() {
            assert_eq!(expires, poe::NEVER);
        } else {
            assert!(expires >= before + 3_600_000 && expires <= oauth::now_ms() + 3_600_000);
        }
        // The saved key is the provider's bearer key.
        let s = Default::default();
        let r = providers::route(&input("poe", &s, Some(saved), None, env(&[])))
            .unwrap()
            .unwrap();
        assert_eq!(r.base_url, "https://api.poe.com/v1");
        assert_eq!(r.api_key.as_deref(), Some("poe-key"));
        assert_eq!(r.protocol, codesmith_ai::Protocol::OpenaiChat);
    }
}

#[tokio::test]
async fn oc_poe_completes_a_login_from_a_pasted_redirect_url_and_keeps_waiting_after_a_wrong_paste()
{
    let srv = server().await;
    let login = poe::start(&poe_endpoints(&srv)).await.unwrap();
    let good = redirect(&login.url, &[]);
    let foreign = redirect(&login.url, &[("state", "another-login")]);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let wrong = Arc::new(Mutex::new(Vec::<String>::new()));
    let w = wrong.clone();
    srv.push(reply(
        200,
        json!({"api_key": "poe-key", "api_key_expires_in": 3600}).to_string(),
    ));
    let client = reqwest::Client::new();
    let wait = poe::finish(
        &client,
        login,
        Some(rx),
        move |m| w.lock().unwrap().push(m.to_string()),
        WAIT,
    );
    let paste = async {
        tx.send(foreign).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            *wrong.lock().unwrap(),
            ["This URL belongs to a different sign-in"]
        );
        tx.send(good).await.unwrap();
    };
    let (saved, ()) = tokio::join!(wait, paste);
    assert_eq!(saved.unwrap()["access"], "poe-key");
    assert_eq!(srv.seen()[0].form()["code"], "auth-code");
}

#[tokio::test]
async fn oc_poe_isolates_overlapping_login_attempts_and_closes_cancelled_listeners() {
    let srv = server().await;
    let e = poe_endpoints(&srv);
    let first = poe::start(&e).await.unwrap();
    let next = poe::start(&e).await.unwrap();
    let (p1, p2) = (params(&first.url), params(&next.url));
    assert_ne!(p1["redirect_uri"], p2["redirect_uri"]);
    assert_ne!(p1["code_challenge"], p2["code_challenge"]);
    assert_ne!(p1["state"], p2["state"]);
    let crossed = redirect(&first.url, &[("state", p2["state"].as_str())]);
    let client = reqwest::Client::new();
    let (done, page) = tokio::join!(
        poe::finish(&client, first, None, |_| {}, WAIT),
        browse(&crossed)
    );
    assert_eq!(page.unwrap().0, 400);
    assert_eq!(done.unwrap_err().to_string(), "Invalid OAuth state");
    // Cancelling the other login closes its listener.
    let next_cb = redirect(&next.url, &[]);
    drop(next);
    assert!(browse(&next_cb).await.is_err());
    assert!(srv.seen().is_empty());
}

#[tokio::test]
async fn oc_poe_rejects_invalid_or_denied_callbacks() {
    let cases: &[(&[(&str, &str)], &str)] = &[
        (&[("state", "")], "Invalid OAuth state"),
        (
            &[("iss", "https://other.example"), ("error", "access_denied")],
            "Invalid OAuth issuer",
        ),
        (&[("iss", "https://poe.com/")], "Invalid OAuth issuer"),
        (&[("iss", "")], "Invalid OAuth issuer"),
        (&[("code", "")], "Missing authorization code"),
        (
            &[
                ("error", "access_denied"),
                ("error_description", "User declined access"),
            ],
            "User declined access",
        ),
    ];
    for (extra, message) in cases {
        let srv = server().await;
        let login = poe::start(&poe_endpoints(&srv)).await.unwrap();
        let to = redirect(&login.url, extra);
        let client = reqwest::Client::new();
        let (done, page) =
            tokio::join!(poe::finish(&client, login, None, |_| {}, WAIT), browse(&to));
        let (status, page) = page.unwrap();
        assert_eq!(status, 400, "{extra:?}");
        assert!(page.contains("Authorization failed"), "{page}");
        assert_eq!(done.unwrap_err().to_string(), *message, "{extra:?}");
        assert!(srv.seen().is_empty());
    }
}

#[tokio::test]
async fn oc_poe_preserves_the_active_connection_after_a_failed_token_exchange() {
    let cases: Vec<(u16, &str, &str)> = vec![
        (
            400,
            r#"{"error":"invalid_grant","error_description":"Code expired"}"#,
            "Poe token exchange failed: Code expired",
        ),
        (
            400,
            r#"{"error":"invalid_grant"}"#,
            "Poe token exchange failed: invalid_grant",
        ),
        (502, "Bad gateway", "Poe token exchange failed (502)"),
        (
            400,
            r#"{"error_description":"Rejected auth-code"}"#,
            "Poe token exchange failed (400)",
        ),
        (200, "{}", "Invalid Poe token response"),
        (200, "{", "Invalid Poe token response"),
        (200, r#"{"api_key":"   "}"#, "Invalid Poe token response"),
        (
            200,
            r#"{"api_key":"poe-key","api_key_expires_in":-1}"#,
            "Invalid Poe token response",
        ),
        (
            200,
            r#"{"api_key":"poe-key","api_key_expires_in":1.5}"#,
            "Invalid Poe token response",
        ),
        (
            200,
            r#"{"api_key":"poe-key","api_key_expires_in":1e309}"#,
            "Invalid Poe token response",
        ),
        (
            200,
            r#"{"api_key":"poe-key","api_key_expires_in":8640000000000}"#,
            "Invalid Poe API key expiry",
        ),
    ];
    for (status, body, message) in cases {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        codesmith_core::auth::set_in(&file, "poe", "previous-key").unwrap();
        let srv = server().await;
        let login = poe::start(&poe_endpoints(&srv)).await.unwrap();
        let to = redirect(&login.url, &[]);
        srv.push(reply(status, body));
        let client = reqwest::Client::new();
        let (done, page) =
            tokio::join!(poe::finish(&client, login, None, |_| {}, WAIT), browse(&to));
        let (code, page) = page.unwrap();
        assert_eq!(code, 400, "{body}");
        assert!(page.contains(message), "{body}: {page}");
        assert_eq!(done.unwrap_err().to_string(), message, "{body}");
        assert_eq!(
            codesmith_core::auth::key_in(&file, "poe").as_deref(),
            Some("previous-key")
        );
    }
}

#[tokio::test]
async fn oc_poe_waits_for_the_token_exchange_and_consumes_the_callback_once() {
    for cancel in [false, true] {
        let srv = server().await;
        let login = poe::start(&poe_endpoints(&srv)).await.unwrap();
        let to = redirect(&login.url, &[]);
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        srv.push(Reply {
            status: 200,
            body: json!({"api_key": "poe-key"}).to_string(),
            gate: Some(gate),
            started: Some(started_tx),
        });
        let client = reqwest::Client::new();
        let finish =
            tokio::spawn(async move { poe::finish(&client, login, None, |_| {}, WAIT).await });
        let page = tokio::spawn({
            let to = to.clone();
            async move { browse(&to).await }
        });
        started.await.unwrap();
        // A second callback while the first is exchanged is refused.
        assert_eq!(browse(&to).await.unwrap().0, 409);
        assert_eq!(srv.seen().len(), 1);
        if cancel {
            finish.abort();
            let _ = release.send(());
            assert!(finish.await.is_err(), "cancelled");
            assert!(
                page.await.unwrap().is_err(),
                "the browser's request is dropped"
            );
            continue;
        }
        let _ = release.send(());
        let saved = finish.await.unwrap().unwrap();
        assert_eq!(saved["access"], "poe-key");
        let (status, page) = page.await.unwrap().unwrap();
        assert_eq!(status, 200);
        assert!(page.contains("Authorization successful"));
    }
}

#[test]
fn oc_poe_keeps_near_expiry_keys_usable_and_requires_a_new_login_after_expiry() {
    let now = oauth::now_ms();
    let saved = json!({"type": "oauth", "method": "browser", "access": "poe-key", "refresh": "", "expires": now + 120_000});
    assert_eq!(
        poe::stored_key(Some(&saved), now).unwrap().as_deref(),
        Some("poe-key")
    );
    let e = poe::stored_key(Some(&saved), now + 120_000)
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with("Poe API key expired. Log in with Poe again"),
        "{e}"
    );
    // A key (saved or from POE_API_KEY) is used instead.
    let s = Default::default();
    let r = providers::route(&input(
        "poe",
        &s,
        Some(json!({"type": "api", "key": "manual-key"})),
        Some("manual-key"),
        env(&[]),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(r.api_key.as_deref(), Some("manual-key"));
}

// plugin/provider-snowflake-cortex.test.ts

use codesmith_ai::Authorizer;
use codesmith_core::providers::snowflake;

fn snowflake_endpoints(srv: &Server) -> snowflake::Endpoints {
    snowflake::Endpoints {
        account: "myorg-myaccount".into(),
        issuer: srv.url.clone(),
        client_id: snowflake::LOCAL_APPLICATION.into(),
    }
}

/// The `authorization` header the authorizer puts on a request.
async fn authorized(a: &dyn Authorizer) -> String {
    let client = reqwest::Client::new();
    let mut req = client.get("http://127.0.0.1:9/x").build().unwrap();
    a.authorize(&client, &mut req).await.unwrap();
    req.headers()["authorization"].to_str().unwrap().to_string()
}

const ENDPOINT: &str = "https://myorg-myaccount.snowflakecomputing.com";

#[tokio::test]
async fn oc_snowflake_browser_oauth_supplies_the_native_endpoint_token_and_refreshes_a_rejected_token_once(
) {
    assert_eq!(
        snowflake::normalize_account(&format!("{ENDPOINT}///")),
        "myorg-myaccount"
    );
    let srv = server().await;
    let e = snowflake_endpoints(&srv);
    let login = snowflake::start(&e, Some("My Role")).await.unwrap();
    let url = login.url.clone();
    assert!(url.starts_with(&format!("{}/oauth/authorize?", srv.url)));
    let p = params(&url);
    assert_eq!(p["scope"], "refresh_token session:role-encoded:My%20Role");
    assert_eq!(p["client_id"], "LOCAL_APPLICATION");
    srv.push(reply(
        200,
        json!({"access_token": "access", "refresh_token": "refresh", "expires_in": 3600})
            .to_string(),
    ));
    let client = reqwest::Client::new();
    let to = redirect(&url, &[]);
    let (saved, page) = tokio::join!(snowflake::finish(&client, login, None, WAIT), browse(&to));
    let saved = saved.unwrap();
    assert_eq!(page.unwrap().0, 200);
    let seen = srv.seen();
    assert_eq!(seen[0].path, "/oauth/token-request");
    use base64::Engine;
    assert_eq!(
        seen[0].headers["authorization"],
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("LOCAL_APPLICATION:LOCAL_APPLICATION")
        )
    );
    let form = seen[0].form();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "auth-code");
    assert_eq!(form["redirect_uri"], p["redirect_uri"]);
    assert_eq!(
        codesmith_core::mcp::oauth::challenge(&form["code_verifier"]),
        p["code_challenge"]
    );
    assert_eq!(saved["metadata"]["account"], "myorg-myaccount");

    // The login authorizes requests; a rejected token is refreshed once,
    // keeping the refresh token when the answer has none.
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    let a = snowflake::Refreshing::new(saved, e, file.clone());
    assert_eq!(authorized(&a).await, "Bearer access");
    srv.push(reply(
        200,
        json!({"access_token": "renewed", "refresh_token": "", "expires_in": 3600}).to_string(),
    ));
    assert!(a.rejected(&client).await);
    let refresh = srv.seen()[1].form();
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "refresh");
    assert_eq!(authorized(&a).await, "Bearer renewed");
    let kept = codesmith_core::auth::entry_in(&file, "snowflake-cortex").unwrap();
    assert_eq!(kept["access"], "renewed");
    assert_eq!(kept["refresh"], "refresh");
    // The route for the saved login is the account's Cortex API.
    let s = Default::default();
    let r = providers::route(&input("snowflake-cortex", &s, Some(kept), None, env(&[])))
        .unwrap()
        .unwrap();
    assert_eq!(r.base_url, format!("{ENDPOINT}/api/v2/cortex/v1"));
    assert!(matches!(r.auth, codesmith_ai::Auth::Dynamic(_)));
}

#[test]
fn oc_snowflake_manual_pat_uses_its_account_and_native_compatibility_rather_than_an_sdk_request_rewrite(
) {
    assert_eq!(snowflake::key_form().label, "Paste PAT or bearer token");
    assert_eq!(snowflake::key_form().fields[0].key, "account");
    let mut c: codesmith_core::config::Config = serde_json::from_value(json!({
        "providers": {"snowflake-cortex": {
            "protocol": "openai-chat",
            "settings": {"account": format!("{ENDPOINT}/"), "token": "pat"},
        }}
    }))
    .unwrap();
    c.models_dev = Some(Arc::new(codesmith_core::catalog::ModelsDev::fixed(
        (*codesmith_core::catalog::bundled()).clone(),
    )));
    let m = c
        .resolve_model("snowflake-cortex/claude-sonnet-4-6")
        .unwrap();
    assert_eq!(m.provider.base_url, format!("{ENDPOINT}/api/v2/cortex/v1"));
    assert_eq!(m.provider.api_key.as_deref(), Some("pat"));
    assert_eq!(m.provider.auth, codesmith_ai::Auth::Bearer);
    assert_eq!(
        m.provider.compat.max_tokens_field.as_deref(),
        Some("max_completion_tokens")
    );
    assert_eq!(m.options.str("cacheFormat"), Some("anthropic"));
}

#[tokio::test]
async fn oc_snowflake_rejects_a_mismatched_callback_state_before_token_exchange() {
    let srv = server().await;
    let login = snowflake::start(&snowflake_endpoints(&srv), None)
        .await
        .unwrap();
    assert_eq!(params(&login.url)["scope"], "refresh_token");
    let to = redirect(&login.url, &[("state", "wrong"), ("code", "forged")]);
    let client = reqwest::Client::new();
    let (done, page) = tokio::join!(snowflake::finish(&client, login, None, WAIT), browse(&to));
    assert_eq!(page.unwrap().0, 400);
    assert_eq!(done.unwrap_err().to_string(), "Invalid OAuth state");
    assert!(srv.seen().is_empty());
}

#[test]
fn oc_snowflake_uses_environment_tokens_rather_than_the_account_identifier_and_bypasses_expired_stored_oauth(
) {
    let e = env(&[
        (
            "SNOWFLAKE_ACCOUNT",
            "https://myorg-myaccount.snowflakecomputing.com/",
        ),
        ("SNOWFLAKE_CORTEX_TOKEN", "env-token"),
        ("SNOWFLAKE_CORTEX_PAT", "env-pat"),
    ]);
    let s = Default::default();
    let expired = json!({"type": "oauth", "method": "browser", "access": "expired", "refresh": "revoked-refresh", "expires": 1, "metadata": {"account": "other-account"}});
    for stored in [None, Some(expired)] {
        let r = providers::route(&input("snowflake-cortex", &s, stored, None, e.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(r.base_url, format!("{ENDPOINT}/api/v2/cortex/v1"));
        assert_eq!(r.api_key.as_deref(), Some("env-token"));
        assert_eq!(r.auth, codesmith_ai::Auth::Bearer);
    }
}

#[test]
fn oc_snowflake_requires_a_token_separately_from_the_account_and_supports_environment_pats() {
    let s = Default::default();
    let e = providers::route(&input(
        "snowflake-cortex",
        &s,
        None,
        None,
        env(&[("SNOWFLAKE_ACCOUNT", "myorg-myaccount")]),
    ))
    .unwrap_err()
    .to_string();
    assert!(e.contains("needs a token"), "{e}");
    let r = providers::route(&input(
        "snowflake-cortex",
        &s,
        None,
        None,
        env(&[
            ("SNOWFLAKE_ACCOUNT", "myorg-myaccount"),
            ("SNOWFLAKE_CORTEX_PAT", "env-pat"),
        ]),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(r.api_key.as_deref(), Some("env-pat"));
}

#[tokio::test]
async fn oc_snowflake_refreshes_expired_oauth_during_native_model_resolution_and_persists_rotated_credentials(
) {
    let srv = server().await;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    let expired = json!({"type": "oauth", "method": "browser", "access": "expired", "refresh": "refresh", "expires": 1, "metadata": {"account": "myorg-myaccount"}});
    let a = snowflake::Refreshing::new(expired, snowflake_endpoints(&srv), file.clone());
    srv.push(reply(
        200,
        json!({"access_token": "renewed", "refresh_token": "rotated-refresh", "expires_in": 3600})
            .to_string(),
    ));
    assert_eq!(authorized(&a).await, "Bearer renewed");
    let seen = srv.seen();
    assert_eq!(seen[0].path, "/oauth/token-request");
    assert_eq!(
        seen[0].form(),
        [
            ("grant_type", "refresh_token"),
            ("refresh_token", "refresh"),
            ("client_id", "LOCAL_APPLICATION"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect::<HashMap<_, _>>()
    );
    let kept = codesmith_core::auth::entry_in(&file, "snowflake-cortex").unwrap();
    assert_eq!(kept["access"], "renewed");
    assert_eq!(kept["refresh"], "rotated-refresh");
    assert_eq!(kept["metadata"]["account"], "myorg-myaccount");
    // Still fresh: no second refresh.
    assert_eq!(authorized(&a).await, "Bearer renewed");
    assert_eq!(srv.seen().len(), 1);
}

// plugin/provider-digitalocean.test.ts

use codesmith_core::providers::{digitalocean, live};

fn do_endpoints(srv: &Server) -> digitalocean::Endpoints {
    digitalocean::Endpoints {
        client_id: "my-do-app".into(),
        authorize: "https://cloud.digitalocean.com/v1/oauth/authorize".into(),
        routers: format!("{}/v2/gen-ai/models/routers", srv.url),
        port: 0,
    }
}

fn do_login(access: &str, expires: u64) -> Value {
    json!({"type": "oauth", "method": "browser", "access": access, "refresh": "", "expires": expires})
}

#[test]
fn oc_digitalocean_is_registered_alongside_the_other_provider_plugins() {
    let p = providers::builtin("digitalocean").unwrap();
    assert_eq!(p.env, ["DIGITALOCEAN_ACCESS_TOKEN"]);
}

#[test]
fn oc_digitalocean_registers_browser_oauth_alongside_generic_key_and_environment_methods() {
    assert_eq!(digitalocean::LABEL, "Login with DigitalOcean");
    let e = digitalocean::client_id(None, &Default::default(), &env(&[]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("CODESMITH_DIGITALOCEAN_CLIENT_ID"), "{e}");
    assert!(e.contains("http://localhost:1456/auth/callback"), "{e}");
    // A key works without a login.
    let s = Default::default();
    let r = providers::route(&input("digitalocean", &s, None, Some("do-key"), env(&[])))
        .unwrap()
        .unwrap();
    assert_eq!(r.base_url, "https://inference.do-ai.run/v1");
    assert_eq!(r.api_key.as_deref(), Some("do-key"));
}

#[tokio::test]
async fn oc_digitalocean_loads_routers_for_the_signed_in_account() {
    let srv = server().await;
    srv.push(reply(
        200,
        json!({"model_routers": [{"name": "beta"}, {"name": "alpha"}]}).to_string(),
    ));
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("live.json");
    let login = do_login("do-token", oauth::now_ms() + 3_600_000);
    let names =
        digitalocean::refresh_routers(&reqwest::Client::new(), &do_endpoints(&srv), &login, &file)
            .await
            .unwrap();
    assert_eq!(names, ["alpha", "beta"]);
    assert_eq!(srv.seen()[0].headers["authorization"], "Bearer do-token");
    let models = live::derive(
        "digitalocean",
        &live::load_in(&file, "digitalocean").unwrap(),
        &[],
    );
    let m = &models[0];
    assert_eq!(m.id, "router:alpha");
    assert_eq!(m.name, "alpha");
    assert_eq!(m.family.as_deref(), Some("digitalocean-inference-routers"));
    assert!(m.capabilities.tools);
    assert_eq!((m.limit.context, m.limit.output), (128_000, 8_192));
    // The saved login is the provider's credential.
    let s = Default::default();
    let r = providers::route(&input("digitalocean", &s, Some(login), None, env(&[])))
        .unwrap()
        .unwrap();
    assert_eq!(r.api_key.as_deref(), Some("do-token"));
}

#[tokio::test]
async fn oc_digitalocean_loads_nothing_and_logs_nothing_at_startup_when_the_access_token_has_expired(
) {
    let srv = server().await;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("live.json");
    let expired = do_login("old", 1);
    assert!(digitalocean::refresh_routers(
        &reqwest::Client::new(),
        &do_endpoints(&srv),
        &expired,
        &file
    )
    .await
    .is_err());
    assert!(srv.seen().is_empty());
    assert!(live::load_in(&file, "digitalocean").is_none());
    let s = Default::default();
    let e = providers::route(&input("digitalocean", &s, Some(expired), None, env(&[])))
        .unwrap_err()
        .to_string();
    assert!(e.contains("expired"), "{e}");
}

#[tokio::test]
async fn oc_digitalocean_loads_on_credential_changes_and_clears_the_previous_accounts_routers() {
    let srv = server().await;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("live.json");
    let later = oauth::now_ms() + 3_600_000;
    srv.push(reply(
        200,
        json!({"model_routers": [{"name": "alpha"}]}).to_string(),
    ));
    srv.push(reply(
        200,
        json!({"model_routers": [{"name": "gamma"}]}).to_string(),
    ));
    let c = reqwest::Client::new();
    digitalocean::refresh_routers(&c, &do_endpoints(&srv), &do_login("first", later), &file)
        .await
        .unwrap();
    digitalocean::refresh_routers(&c, &do_endpoints(&srv), &do_login("second", later), &file)
        .await
        .unwrap();
    let ids: Vec<String> = live::derive(
        "digitalocean",
        &live::load_in(&file, "digitalocean").unwrap(),
        &[],
    )
    .into_iter()
    .map(|m| m.id)
    .collect();
    assert_eq!(ids, ["router:gamma"]);
}

#[tokio::test]
async fn oc_digitalocean_shows_a_returning_accounts_cached_routers_at_once() {
    // A kept list serves at once, without asking DigitalOcean.
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("live.json");
    live::save_in(
        &file,
        "digitalocean",
        "acct",
        "",
        vec![json!({"name": "alpha"})],
    )
    .unwrap();
    let kept = live::load_in(&file, "digitalocean").unwrap();
    assert_eq!(
        live::derive("digitalocean", &kept, &[])[0].id,
        "router:alpha"
    );
    // A catalog model of the same id wins.
    let mut catalog = live::blank("digitalocean", "router:alpha");
    catalog.name = "catalog".into();
    assert!(live::derive("digitalocean", &kept, &[catalog]).is_empty());
}

async fn do_post(redirect: &str, body: &str) -> (u16, String) {
    let u = url::Url::parse(redirect).unwrap();
    let token = format!(
        "http://127.0.0.1:{}{}",
        u.port().unwrap(),
        digitalocean::TOKEN_PATH
    );
    let r = reqwest::Client::new()
        .post(token)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

#[tokio::test]
async fn oc_digitalocean_completes_browser_oauth_independently_of_router_discovery_and_releases_the_listener(
) {
    let srv = server().await;
    srv.push(reply(503, "unavailable"));
    let e = do_endpoints(&srv);
    let login = digitalocean::start(&e).await.unwrap();
    let p = params(&login.url);
    assert!(login
        .url
        .starts_with("https://cloud.digitalocean.com/v1/oauth/authorize?"));
    assert_eq!(p["response_type"], "token");
    assert_eq!(p["client_id"], "my-do-app");
    assert_eq!(p["scope"], "genai:read inference:query");
    assert!(p["redirect_uri"].ends_with("/auth/callback"));
    assert!(p["redirect_uri"].starts_with("http://localhost:"));
    let state = p["state"].clone();
    let redirect = login.redirect.clone();
    let port = url::Url::parse(&redirect).unwrap().port().unwrap();
    let now = oauth::now_ms();
    let finish = tokio::spawn(digitalocean::finish(login, WAIT));
    let page = browse(&format!("http://127.0.0.1:{port}/auth/callback"))
        .await
        .unwrap();
    assert!(page.1.contains("/auth/token"));
    let body =
        json!({"access_token": "do-token", "expires_in": "3600", "state": state}).to_string();
    assert_eq!(do_post(&redirect, &body).await.0, 200);
    let saved = finish.await.unwrap().unwrap();
    assert_eq!(saved["access"], "do-token");
    assert_eq!(saved["refresh"], "");
    assert!(saved.get("metadata").is_none());
    let exp = saved["expires"].as_u64().unwrap();
    assert!(exp >= now + 3_600_000 && exp <= oauth::now_ms() + 3_600_000);
    // Router discovery failing does not undo the login.
    let d = tempfile::tempdir().unwrap();
    assert!(digitalocean::refresh_routers(
        &reqwest::Client::new(),
        &e,
        &saved,
        &d.path().join("l.json")
    )
    .await
    .is_err());
    // The listener was released: the port can be listened on again.
    let again = digitalocean::start(&digitalocean::Endpoints { port, ..e.clone() })
        .await
        .unwrap();
    assert_ne!(params(&again.url)["state"], state);
}

#[tokio::test]
async fn oc_digitalocean_rejects_invalid_callbacks_and_releases_the_listener() {
    let cases = [
        (
            json!({"access_token": "token", "state": "wrong"}).to_string(),
            "Invalid OAuth state",
        ),
        ("{}".to_string(), "Missing access token"),
        ("not-json".to_string(), "Missing access token"),
        (
            json!({"error": "access_denied", "error_description": "Denied"}).to_string(),
            "Denied",
        ),
        (
            json!({"error": "access_denied", "error_description": ""}).to_string(),
            "access_denied",
        ),
    ];
    for (body, message) in cases {
        let srv = server().await;
        let e = do_endpoints(&srv);
        let login = digitalocean::start(&e).await.unwrap();
        let redirect = login.redirect.clone();
        let port = url::Url::parse(&redirect).unwrap().port().unwrap();
        let finish = tokio::spawn(digitalocean::finish(login, WAIT));
        assert_eq!(do_post(&redirect, &body).await.0, 400, "{body}");
        assert_eq!(finish.await.unwrap().unwrap_err().to_string(), message);
        digitalocean::start(&digitalocean::Endpoints { port, ..e })
            .await
            .unwrap();
    }
}
