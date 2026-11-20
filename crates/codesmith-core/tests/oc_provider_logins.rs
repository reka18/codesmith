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
    // Adapted: the crossed callback is refused, and the login keeps
    // waiting rather than end on it.
    assert_still_waiting(poe::finish(&client, first, None, |_| {}, WAIT), &crossed).await;
    // Cancelling the other login closes its listener.
    let next_cb = redirect(&next.url, &[]);
    drop(next);
    assert!(browse(&next_cb).await.is_err());
    assert!(srv.seen().is_empty());
}

/// Not in OpenCode: a callback with another state is answered 400 and the
/// login keeps waiting (whatever sent it cannot end the login), so `login`
/// is still running after `to` is refused.
async fn assert_still_waiting<T: std::fmt::Debug>(
    login: impl std::future::Future<Output = T>,
    to: &str,
) {
    let probe = async {
        let (status, page) = browse(to).await.unwrap();
        assert_eq!(status, 400);
        assert!(page.contains("Invalid OAuth state"), "{page}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    tokio::select! {
        done = login => panic!("the login ended on a wrong state: {done:?}"),
        () = probe => {}
    }
}

/// Adapted: OpenCode ends the login on a wrong state; here it keeps waiting
/// (see [`assert_still_waiting`]).
#[tokio::test]
async fn oc_poe_rejects_invalid_or_denied_callbacks() {
    let srv = server().await;
    let login = poe::start(&poe_endpoints(&srv)).await.unwrap();
    let to = redirect(&login.url, &[("state", "")]);
    let client = reqwest::Client::new();
    assert_still_waiting(poe::finish(&client, login, None, |_| {}, WAIT), &to).await;
    let cases: &[(&[(&str, &str)], &str)] = &[
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
    assert_still_waiting(snowflake::finish(&client, login, None, WAIT), &to).await;
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

/// Adapted: OpenCode ends the login on a post with another state; here
/// such a post is refused and the login keeps waiting (another local
/// program could otherwise end it), and the state is checked before the
/// error, so a post that is not this login's cannot choose its error.
#[tokio::test]
async fn oc_digitalocean_rejects_invalid_callbacks_and_releases_the_listener() {
    let cases = [
        (json!({}), "Missing access token"),
        (
            json!({"error": "access_denied", "error_description": "Denied"}),
            "Denied",
        ),
        (
            json!({"error": "access_denied", "error_description": ""}),
            "access_denied",
        ),
    ];
    for (mut body, message) in cases {
        let srv = server().await;
        let e = do_endpoints(&srv);
        let login = digitalocean::start(&e).await.unwrap();
        let state = params(&login.url)["state"].clone();
        let redirect = login.redirect.clone();
        let port = url::Url::parse(&redirect).unwrap().port().unwrap();
        let finish = tokio::spawn(digitalocean::finish(login, WAIT));
        // Another state, or none, or not JSON: refused, and still waiting.
        let wrong = [
            json!({"access_token": "token", "state": "wrong"}).to_string(),
            json!({"error": "access_denied", "error_description": "Forged"}).to_string(),
            "not-json".to_string(),
        ];
        for w in wrong {
            let (code, text) = do_post(&redirect, &w).await;
            assert_eq!(code, 400, "{w}");
            assert!(text.contains("Invalid OAuth state"), "{text}");
        }
        assert!(!finish.is_finished());
        body["state"] = json!(state);
        assert_eq!(do_post(&redirect, &body.to_string()).await.0, 400, "{body}");
        assert_eq!(finish.await.unwrap().unwrap_err().to_string(), message);
        digitalocean::start(&digitalocean::Endpoints { port, ..e })
            .await
            .unwrap();
    }
}

/// Not in OpenCode: the redirect names `localhost`, so the listener holds
/// the port on `[::1]` too (where IPv6 loopback exists), and a login whose
/// port is taken there fails rather than leave the token to whoever has it.
/// A connection that sends nothing holds up no other.
#[tokio::test]
async fn digitalocean_holds_both_loopbacks_and_outlasts_a_silent_connection() {
    let srv = server().await;
    let e = do_endpoints(&srv);
    let v6 = std::net::TcpListener::bind("[::1]:0").ok();
    let login = digitalocean::start(&e).await.unwrap();
    let redirect = login.redirect.clone();
    let state = params(&login.url)["state"].clone();
    let port = url::Url::parse(&redirect).unwrap().port().unwrap();
    if v6.is_some() {
        assert!(std::net::TcpListener::bind(("::1", port)).is_err());
    }
    assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_err());
    let finish = tokio::spawn(digitalocean::finish(login, WAIT));
    let _silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let body = json!({"access_token": "do-token", "state": state}).to_string();
    let posted = tokio::time::timeout(Duration::from_secs(3), do_post(&redirect, &body))
        .await
        .expect("a silent connection held up the login");
    assert_eq!(posted.0, 200);
    assert_eq!(finish.await.unwrap().unwrap()["access"], "do-token");

    // Someone else on [::1] at the port: the login does not start.
    if let Some(taken) = v6 {
        let port = taken.local_addr().unwrap().port();
        let err = digitalocean::start(&digitalocean::Endpoints { port, ..e })
            .await
            .err()
            .expect("started with [::1] taken");
        assert!(err.to_string().contains("already in use"), "{err}");
    }
}

/// Not in OpenCode: a connection that never sends its request holds up
/// no other, and a wrong-state callback leaves the login to finish.
#[tokio::test]
async fn an_oauth_login_outlasts_silent_and_wrong_state_connections() {
    let srv = server().await;
    srv.push(reply(200, json!({"api_key": "poe-key"}).to_string()));
    let login = poe::start(&poe_endpoints(&srv)).await.unwrap();
    let port = url::Url::parse(&params(&login.url)["redirect_uri"])
        .unwrap()
        .port()
        .unwrap();
    let good = redirect(&login.url, &[]);
    let wrong = redirect(&login.url, &[("state", "other")]);
    let client = reqwest::Client::new();
    let finish = poe::finish(&client, login, None, |_| {}, WAIT);
    let browser = async {
        let _silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        assert_eq!(browse(&wrong).await.unwrap().0, 400);
        tokio::time::timeout(Duration::from_secs(3), browse(&good))
            .await
            .expect("a silent connection held up the login")
            .unwrap()
    };
    let (saved, page) = tokio::join!(finish, browser);
    assert_eq!(page.0, 200);
    assert_eq!(saved.unwrap()["access"], "poe-key");
}

// plugin/provider-gitlab-oauth.test.ts

use codesmith_core::providers::gitlab;

fn gitlab_endpoints(srv: &Server, client: &str) -> gitlab::Endpoints {
    gitlab::Endpoints {
        instance: srv.url.clone(),
        client_id: client.into(),
        port: 0,
    }
}

fn renewal() -> Reply {
    reply(
        200,
        json!({"access_token": "renewed-access", "refresh_token": "renewed-refresh", "expires_in": 3600}).to_string(),
    )
}

fn expired_gitlab(instance: &str, client: Option<&str>) -> Value {
    let mut metadata = json!({"instanceUrl": instance});
    if let Some(c) = client {
        metadata["clientID"] = c.into();
    }
    json!({"type": "oauth", "method": "pkce", "access": "stale-access", "refresh": "stored-refresh", "expires": 1, "metadata": metadata})
}

#[test]
fn oc_gitlab_oauth_is_registered_alongside_the_other_provider_plugins() {
    assert_eq!(gitlab::LABEL, "Login with GitLab (OAuth)");
    assert_eq!(
        gitlab::redirect_uri(gitlab::PORT),
        "http://127.0.0.1:8080/callback"
    );
}

#[test]
fn oc_gitlab_oauth_registers_pkce_oauth_alongside_the_generic_key_env_methods_from_models_dev_plugin(
) {
    // The instance defaults to GITLAB_INSTANCE_URL, then gitlab.com.
    assert_eq!(
        gitlab::normalize_instance(None, &env(&[])).unwrap(),
        "https://gitlab.com"
    );
    assert_eq!(
        gitlab::normalize_instance(
            None,
            &env(&[("GITLAB_INSTANCE_URL", "https://git.example/")])
        )
        .unwrap(),
        "https://git.example"
    );
    assert!(gitlab::normalize_instance(Some("ftp://x"), &env(&[])).is_err());
}

#[tokio::test]
async fn oc_gitlab_oauth_exchanges_a_pkce_code_for_a_native_gitlab_oauth_credential() {
    let srv = server().await;
    let login = gitlab::start(&gitlab_endpoints(&srv, "my-gitlab-app"))
        .await
        .unwrap();
    let url = login.url.clone();
    assert!(url.starts_with(&format!("{}/oauth/authorize?", srv.url)));
    let p = params(&url);
    assert_eq!(p["client_id"], "my-gitlab-app");
    assert_eq!(p["response_type"], "code");
    assert_eq!(p["scope"], "api");
    assert_eq!(p["code_challenge_method"], "S256");
    assert!(p["redirect_uri"].starts_with("http://127.0.0.1:"));
    assert!(p["redirect_uri"].ends_with("/callback"));
    srv.push(reply(
        200,
        json!({"access_token": "access-token", "refresh_token": "refresh-token"}).to_string(),
    ));
    let now = oauth::now_ms();
    let client = reqwest::Client::new();
    let to = redirect(&url, &[]);
    let (saved, page) = tokio::join!(gitlab::finish(&client, login, None, WAIT), browse(&to));
    assert_eq!(page.unwrap().0, 200);
    let saved = saved.unwrap();
    let seen = srv.seen();
    assert_eq!(seen[0].path, "/oauth/token");
    assert!(seen[0].headers["content-type"].contains("application/x-www-form-urlencoded"));
    let form = seen[0].form();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["client_id"], "my-gitlab-app");
    assert_eq!(form["code"], "auth-code");
    assert_eq!(form["redirect_uri"], p["redirect_uri"]);
    assert_eq!(
        codesmith_core::mcp::oauth::challenge(&form["code_verifier"]),
        p["code_challenge"]
    );
    assert_eq!(saved["access"], "access-token");
    assert_eq!(saved["refresh"], "refresh-token");
    assert_eq!(
        saved["metadata"],
        json!({"instanceUrl": srv.url, "clientID": "my-gitlab-app"})
    );
    let exp = saved["expires"].as_u64().unwrap();
    assert!(exp >= now + 7_200_000 && exp <= oauth::now_ms() + 7_200_000);
}

#[tokio::test]
async fn oc_gitlab_oauth_uses_the_answered_self_managed_instance_url_including_a_relative_url_root()
{
    assert_eq!(
        gitlab::normalize_instance(Some("https://example.com/gitlab/"), &env(&[])).unwrap(),
        "https://example.com/gitlab"
    );
    let srv = server().await;
    let mut e = gitlab_endpoints(&srv, "self-managed-client");
    e.instance = format!("{}/gitlab", srv.url);
    let login = gitlab::start(&e).await.unwrap();
    assert!(login
        .url
        .starts_with(&format!("{}/gitlab/oauth/authorize?", srv.url)));
    srv.push(reply(
        200,
        json!({"access_token": "a", "refresh_token": "r", "expires_in": 3600}).to_string(),
    ));
    let client = reqwest::Client::new();
    let to = redirect(&login.url, &[]);
    let (saved, _) = tokio::join!(gitlab::finish(&client, login, None, WAIT), browse(&to));
    assert_eq!(srv.seen()[0].path, "/gitlab/oauth/token");
    assert_eq!(
        saved.unwrap()["metadata"],
        json!({"instanceUrl": format!("{}/gitlab", srv.url), "clientID": "self-managed-client"})
    );
}

#[test]
fn oc_gitlab_oauth_fails_early_for_self_managed_instances_without_gitlab_oauth_client_id() {
    let e = gitlab::client_id(
        None,
        &Default::default(),
        &env(&[]),
        "https://gitlab.example.com",
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("GITLAB_OAUTH_CLIENT_ID"), "{e}");
    assert!(e.contains("gitlab.example.com"), "{e}");
    assert!(e.contains("http://127.0.0.1:8080/callback"), "{e}");
}

#[tokio::test]
async fn oc_gitlab_oauth_refreshes_an_oauth_credential_using_its_stored_instance_url_and_includes_redirect_uri(
) {
    let srv = server().await;
    srv.push(renewal());
    let login = expired_gitlab(&format!("{}/self", srv.url), Some("app-1"));
    let now = oauth::now_ms();
    let r = gitlab::refresh(&reqwest::Client::new(), &login, &env(&[]), 8080)
        .await
        .unwrap();
    assert_eq!(r["access"], "renewed-access");
    assert_eq!(r["refresh"], "renewed-refresh");
    assert!(r["expires"].as_u64().unwrap() >= now + 3_600_000);
    assert_eq!(
        r["metadata"],
        json!({"instanceUrl": format!("{}/self", srv.url), "clientID": "app-1"})
    );
    let seen = srv.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/self/oauth/token");
    let form = seen[0].form();
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "stored-refresh");
    assert_eq!(form["client_id"], "app-1");
    assert_eq!(form["redirect_uri"], "http://127.0.0.1:8080/callback");
}

#[tokio::test]
async fn oc_gitlab_oauth_shares_one_token_exchange_between_concurrent_refreshes_of_the_same_credential(
) {
    let srv = server().await;
    let (release, gate) = tokio::sync::oneshot::channel();
    let mut r = renewal();
    r.gate = Some(gate);
    srv.push(r);
    let mut login = expired_gitlab(&srv.url, Some("app-1"));
    login["refresh"] = "shared-refresh".into();
    let client = reqwest::Client::new();
    let e = env(&[]);
    let all =
        futures_util::future::join_all((0..3).map(|_| gitlab::refresh(&client, &login, &e, 8080)));
    let go = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = release.send(());
    };
    let (out, ()) = tokio::join!(all, go);
    let access: Vec<String> = out
        .into_iter()
        .map(|r| r.unwrap()["access"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(access, ["renewed-access"; 3]);
    assert_eq!(srv.seen().len(), 1);
}

#[tokio::test]
async fn oc_gitlab_oauth_refreshes_credentials_without_a_client_id_only_with_gitlab_oauth_client_id(
) {
    // CodeSmith never refreshes with another project's application: a
    // login without one needs GITLAB_OAUTH_CLIENT_ID (or a new login).
    let srv = server().await;
    let login = expired_gitlab(&srv.url, None);
    let e = gitlab::refresh(&reqwest::Client::new(), &login, &env(&[]), 8080)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("GITLAB_OAUTH_CLIENT_ID"), "{e}");
    assert!(srv.seen().is_empty());
    srv.push(renewal());
    let mut login = login;
    login["refresh"] = "other-refresh".into();
    let r = gitlab::refresh(
        &reqwest::Client::new(),
        &login,
        &env(&[("GITLAB_OAUTH_CLIENT_ID", "env-app")]),
        8080,
    )
    .await
    .unwrap();
    assert_eq!(r["metadata"]["clientID"], "env-app");
    assert_eq!(srv.seen()[0].form()["client_id"], "env-app");
}

#[tokio::test]
async fn oc_gitlab_oauth_refreshes_only_with_the_recorded_client_id_when_the_credential_has_one() {
    let srv = server().await;
    srv.push(renewal());
    let mut login = expired_gitlab(&srv.url, Some("recorded-app"));
    login["refresh"] = "recorded-refresh".into();
    gitlab::refresh(
        &reqwest::Client::new(),
        &login,
        &env(&[("GITLAB_OAUTH_CLIENT_ID", "env-app")]),
        8080,
    )
    .await
    .unwrap();
    assert_eq!(srv.seen()[0].form()["client_id"], "recorded-app");
}

#[tokio::test]
async fn oc_gitlab_oauth_reports_a_revoked_refresh_token_with_a_sign_in_again_hint_instead_of_authorization_code_hints(
) {
    let srv = server().await;
    srv.push(reply(400, json!({"error": "invalid_grant"}).to_string()));
    let mut login = expired_gitlab(&srv.url, Some("app-1"));
    login["refresh"] = "revoked-refresh".into();
    let e = gitlab::refresh(&reqwest::Client::new(), &login, &env(&[]), 8080)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("refresh token was revoked, expired"), "{e}");
    assert!(e.contains("Sign in to GitLab again"), "{e}");
    assert!(!e.contains("authorization code"), "{e}");
}

/// Adapted: a wrong state leaves the login waiting (see
/// [`assert_still_waiting`]).
#[tokio::test]
async fn oc_gitlab_oauth_rejects_invalid_or_denied_callbacks() {
    let srv = server().await;
    let login = gitlab::start(&gitlab_endpoints(&srv, "app")).await.unwrap();
    let to = redirect(&login.url, &[("state", "")]);
    let client = reqwest::Client::new();
    assert_still_waiting(gitlab::finish(&client, login, None, WAIT), &to).await;
    let cases: &[(&[(&str, &str)], &str)] = &[
        (&[("code", "")], "Missing authorization code"),
        (
            &[
                ("error", "access_denied"),
                ("error_description", "User declined access"),
            ],
            "User declined access",
        ),
        (&[("error", "access_denied")], "access_denied"),
    ];
    for (extra, message) in cases {
        let srv = server().await;
        let login = gitlab::start(&gitlab_endpoints(&srv, "app")).await.unwrap();
        let to = redirect(&login.url, extra);
        let client = reqwest::Client::new();
        let (done, page) = tokio::join!(gitlab::finish(&client, login, None, WAIT), browse(&to));
        let (status, page) = page.unwrap();
        assert_eq!(status, 400);
        assert!(page.contains("Authorization failed"));
        assert_eq!(done.unwrap_err().to_string(), *message);
        assert!(srv.seen().is_empty());
    }
}

#[tokio::test]
async fn oc_gitlab_oauth_preserves_the_active_connection_after_a_failed_token_exchange() {
    let cases = [
        (
            400,
            json!({"error": "invalid_grant", "error_description": "Code expired"}).to_string(),
            "Code expired",
        ),
        (
            400,
            json!({"error": "invalid_grant"}).to_string(),
            "client_id used:",
        ),
        (502, "Bad gateway".to_string(), "HTTP 502"),
    ];
    for (status, body, contains) in cases {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("auth.json");
        codesmith_core::auth::set_in(&file, "gitlab", "previous-key").unwrap();
        let srv = server().await;
        let login = gitlab::start(&gitlab_endpoints(&srv, "app")).await.unwrap();
        srv.push(reply(status, body));
        let to = redirect(&login.url, &[]);
        let client = reqwest::Client::new();
        let (done, page) = tokio::join!(gitlab::finish(&client, login, None, WAIT), browse(&to));
        let (code, page) = page.unwrap();
        assert_eq!(code, 400);
        assert!(page.contains(contains), "{page}");
        assert!(done.unwrap_err().to_string().contains(contains));
        assert_eq!(
            codesmith_core::auth::key_in(&file, "gitlab").as_deref(),
            Some("previous-key")
        );
    }
}

#[tokio::test]
async fn oc_gitlab_oauth_reports_eaddrinuse_with_a_clear_message_when_the_callback_port_is_occupied(
) {
    let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = blocker.local_addr().unwrap().port();
    let srv = server().await;
    let e = gitlab::Endpoints {
        port,
        ..gitlab_endpoints(&srv, "app")
    };
    let err = gitlab::start(&e).await.err().unwrap().to_string();
    assert!(err.contains(&format!("port {port}")), "{err}");
    assert!(err.contains("already in use"), "{err}");
}

#[tokio::test]
async fn oc_gitlab_oauth_honors_gitlab_oauth_client_id_for_self_managed_applications() {
    let id = gitlab::client_id(
        None,
        &Default::default(),
        &env(&[("GITLAB_OAUTH_CLIENT_ID", " custom-client-id ")]),
        "https://gitlab.com",
    )
    .unwrap();
    assert_eq!(id, "custom-client-id");
    let srv = server().await;
    let login = gitlab::start(&gitlab_endpoints(&srv, &id)).await.unwrap();
    assert_eq!(params(&login.url)["client_id"], "custom-client-id");
    srv.push(reply(
        200,
        json!({"access_token": "a", "refresh_token": "r", "expires_in": 3600}).to_string(),
    ));
    let client = reqwest::Client::new();
    let to = redirect(&login.url, &[]);
    let (done, _) = tokio::join!(gitlab::finish(&client, login, None, WAIT), browse(&to));
    done.unwrap();
    assert_eq!(srv.seen()[0].form()["client_id"], "custom-client-id");
}
