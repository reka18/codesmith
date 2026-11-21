//! Ports of OpenCode's `mcp-oauth.test.ts` (MIT) from the porting backlog,
//! against an authorization server and MCP endpoint played by a scripted
//! local HTTP server. The browser is played by an HTTP client that follows
//! the authorization URL's redirect to the loopback callback. Tokens are
//! saved in a temporary store, and the user's home and data folders are
//! kept out (`support::isolate`).
//!
//! Adapted throughout: CodeSmith refreshes a saved token shortly before it
//! expires (within a minute), rather than after the server refuses it.

mod support;

use codesmith_core::mcp::oauth::{self, Entry};
use codesmith_core::mcp::{Mcp, McpConfig, Status};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request the server saw.
#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Req {
    fn form(&self) -> BTreeMap<String, String> {
        reqwest::Url::parse(&format!("http://x/?{}", self.body))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }
}

/// What to answer: a status, headers and a body, or nothing at all.
enum Resp {
    Json(u16, Value, Vec<(String, String)>),
    Empty(u16, Vec<(String, String)>),
    Hang,
}

fn ok(v: Value) -> Resp {
    Resp::Json(200, v, vec![])
}

fn not_found() -> Resp {
    Resp::Empty(404, vec![])
}

type Handler = Arc<dyn Fn(&Req, &str) -> Resp + Send + Sync>;

struct Server {
    origin: String,
    seen: Arc<Mutex<Vec<Req>>>,
}

impl Server {
    fn requests(&self) -> Vec<Req> {
        self.seen.lock().unwrap().clone()
    }
    fn posts(&self, path: &str) -> Vec<Req> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == path)
            .collect()
    }
}

/// A local HTTP server answering with `handler(request, origin)`.
async fn serve(handler: Handler) -> Server {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", l.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Req>>> = Arc::default();
    let (log, base) = (seen.clone(), origin.clone());
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let (log, handler, base) = (log.clone(), handler.clone(), base.clone());
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
                            let body =
                                String::from_utf8_lossy(&buf[i + 4..i + 4 + len]).to_string();
                            break (text[..i].to_string(), body);
                        }
                    }
                };
                let mut lines = head.lines();
                let first: Vec<&str> = lines.next().unwrap_or("").split_whitespace().collect();
                let u = reqwest::Url::parse(&format!("http://x{}", first.get(1).unwrap_or(&"/")))
                    .unwrap();
                let req = Req {
                    method: first.first().unwrap_or(&"").to_string(),
                    path: u.path().to_string(),
                    query: u
                        .query_pairs()
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .collect(),
                    headers: lines
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                        .collect(),
                    body,
                };
                log.lock().unwrap().push(req.clone());
                let (status, headers, body) = match handler(&req, &base) {
                    Resp::Json(c, v, h) => {
                        let mut h = h;
                        h.push(("Content-Type".into(), "application/json".into()));
                        (c, h, v.to_string())
                    }
                    Resp::Empty(c, h) => (c, h, String::new()),
                    Resp::Hang => {
                        // Never answers; the connection stays until the
                        // client goes away.
                        let _ = s.read(&mut tmp).await;
                        return;
                    }
                };
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&body);
                let _ = s.write_all(out.as_bytes()).await;
            });
        }
    });
    Server { origin, seen }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn store() -> (tempfile::TempDir, PathBuf) {
    support::isolate();
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("mcp-auth.json");
    (d, p)
}

/// A saved login for `url` whose token refreshes at `token_endpoint`.
fn saved(path: &Path, url: &str, token_endpoint: &str, expires_at: Option<u64>) {
    oauth::save(
        path,
        "srv",
        &Entry {
            server_url: url.into(),
            client_id: "client".into(),
            client_secret: None,
            token_endpoint: token_endpoint.into(),
            access_token: "a1".into(),
            refresh_token: Some("r1".into()),
            expires_at,
        },
    )
    .unwrap();
}

/// An authorization server and MCP endpoint at one origin: metadata, a
/// registration endpoint and a token endpoint answering with `token`.
fn authorization_server(token: fn(&Req) -> Resp) -> Handler {
    Arc::new(
        move |r: &Req, base: &str| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/.well-known/oauth-protected-resource/mcp") => ok(json!({
                "resource": format!("{base}/mcp"), "authorization_servers": [base], "scopes_supported": ["tools"]
            })),
            ("GET", "/.well-known/oauth-authorization-server") => ok(json!({
                "issuer": base, "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"), "registration_endpoint": format!("{base}/register"),
                "code_challenge_methods_supported": ["S256"]
            })),
            ("POST", "/register") => Resp::Json(201, json!({"client_id": "registered"}), vec![]),
            ("POST", "/token") => token(r),
            ("POST", "/mcp") => Resp::Empty(401, vec![]),
            _ => not_found(),
        },
    )
}

fn tokens(_: &Req) -> Resp {
    ok(
        json!({"access_token": "access", "refresh_token": "refresh", "expires_in": 3600, "token_type": "Bearer"}),
    )
}

/// Play the browser: check the authorization URL, try the callback without
/// the redirect's parameters (it must keep waiting), then follow it.
async fn browse(url: String, seen: Arc<Mutex<Vec<reqwest::Url>>>) {
    let u = reqwest::Url::parse(&url).unwrap();
    seen.lock().unwrap().push(u.clone());
    let q: BTreeMap<String, String> = u
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut callback = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
    let http = reqwest::Client::new();
    let bare = http.get(callback.clone()).send().await.unwrap();
    assert_eq!(
        bare.status(),
        400,
        "a request without parameters is not the redirect"
    );
    let options = http
        .request(reqwest::Method::OPTIONS, callback.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(options.status(), 400);
    callback
        .query_pairs_mut()
        .append_pair("code", "accepted")
        .append_pair("state", &q["state"]);
    assert_eq!(http.get(callback).send().await.unwrap().status(), 200);
}

async fn login(path: &Path, url: &str) -> (anyhow::Result<Entry>, Vec<reqwest::Url>) {
    let seen: Arc<Mutex<Vec<reqwest::Url>>> = Arc::default();
    let s = seen.clone();
    let setting: oauth::Setting = serde_json::from_value(json!({"port": 0})).unwrap();
    let r = oauth::login(
        &reqwest::Client::new(),
        path,
        "srv",
        url,
        Some(&setting),
        move |u| {
            tokio::spawn(browse(u.to_string(), s));
        },
    )
    .await;
    let urls = seen.lock().unwrap().clone();
    (r, urls)
}

// mcp-oauth.test.ts: "completes interactive authorization through the
// loopback callback", with "keeps waiting for the redirect after callback
// requests without OAuth parameters", "generates a loopback redirect URL
// when none is configured", "registers dynamically when the server does not
// accept public clients" and "reuses the client registered by an earlier
// login"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_completes_authorization_through_the_loopback_callback() {
    let (_d, path) = store();
    let server = serve(authorization_server(tokens)).await;
    let url = format!("{}/mcp", server.origin);
    let (entry, urls) = login(&path, &url).await;
    let entry = entry.unwrap();
    assert_eq!(
        (entry.access_token.as_str(), entry.refresh_token.as_deref()),
        ("access", Some("refresh"))
    );
    assert_eq!(oauth::load(&path, "srv", &url), Some(entry.clone()));
    let auth = &urls[0];
    assert_eq!(auth.path(), "/authorize");
    let q: BTreeMap<String, String> = auth
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(q["client_id"], "registered");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["resource"], url);
    assert_eq!(q["scope"], "tools");
    let redirect = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), oauth::CALLBACK_PATH);
    assert_ne!(redirect.port(), Some(0));
    let registrations = server.posts("/register");
    assert_eq!(registrations.len(), 1);
    let reg: Value = serde_json::from_str(&registrations[0].body).unwrap();
    // Both spellings of the loopback redirect are registered (some servers
    // rewrite one to the other); the login is sent with 127.0.0.1.
    let localhost = q["redirect_uri"].replace("127.0.0.1", "localhost");
    assert_eq!(reg["redirect_uris"], json!([q["redirect_uri"], localhost]));
    let token = server.posts("/token")[0].form();
    assert_eq!(token["grant_type"], "authorization_code");
    assert_eq!(token["code"], "accepted");
    assert_eq!(token["redirect_uri"], q["redirect_uri"]);
    assert_eq!(
        oauth::challenge(&token["code_verifier"]),
        q["code_challenge"]
    );

    // A second login reuses the registered client.
    let (again, _) = login(&path, &url).await;
    again.unwrap();
    assert_eq!(server.posts("/register").len(), 1);
}

// mcp-oauth.test.ts: "rejects an authorization endpoint that is not http or
// https"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_rejects_an_authorization_endpoint_that_is_not_http_or_https() {
    let (_d, path) = store();
    let server = serve(Arc::new(|r: &Req, base: &str| match r.path.as_str() {
        "/.well-known/oauth-authorization-server" => ok(json!({
            "issuer": base, "authorization_endpoint": "file:///etc/passwd",
            "token_endpoint": format!("{base}/token"), "registration_endpoint": format!("{base}/register")
        })),
        "/register" => Resp::Json(201, json!({"client_id": "c"}), vec![]),
        _ => not_found(),
    }))
    .await;
    let setting: oauth::Setting = serde_json::from_value(json!({"port": 0})).unwrap();
    let opened = Arc::new(Mutex::new(false));
    let o = opened.clone();
    let e = oauth::login(
        &reqwest::Client::new(),
        &path,
        "srv",
        &format!("{}/mcp", server.origin),
        Some(&setting),
        move |_| *o.lock().unwrap() = true,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("only https"), "{e}");
    assert!(!*opened.lock().unwrap(), "nothing was opened");
    for u in ["javascript:alert(1)", "ftp://auth.example/authorize"] {
        assert!(oauth::check_browser_url(&reqwest::Url::parse(u).unwrap()).is_err());
    }
}

// mcp-oauth.test.ts: "finds resource metadata through the 401 header when
// the well-known path is not served"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_finds_resource_metadata_through_the_401_header() {
    let server = serve(Arc::new(|r: &Req, base: &str| match r.path.as_str() {
        "/metadata/resource" => ok(json!({"resource": format!("{base}/mcp"), "authorization_servers": [format!("{base}/issuer")]})),
        "/.well-known/oauth-authorization-server/issuer" => ok(json!({
            "issuer": format!("{base}/issuer"), "authorization_endpoint": format!("{base}/issuer/authorize"),
            "token_endpoint": format!("{base}/issuer/token")
        })),
        _ => not_found(),
    }))
    .await;
    let header = format!(
        "Bearer resource_metadata=\"{}/metadata/resource\"",
        server.origin
    );
    let ep = oauth::discover(
        &reqwest::Client::new(),
        &format!("{}/mcp", server.origin),
        Some(&header),
    )
    .await
    .unwrap();
    assert_eq!(ep.token, format!("{}/issuer/token", server.origin));
    assert_eq!(
        ep.authorization,
        format!("{}/issuer/authorize", server.origin)
    );
}

// mcp-oauth.test.ts: "sends the configured URL when the server publishes no
// metadata" (adapted: the endpoints fall back to the server's origin, and
// the configured URL is the resource asked for)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_falls_back_to_the_origin_when_the_server_publishes_no_metadata() {
    let (_d, path) = store();
    let server = serve(Arc::new(|r: &Req, _: &str| match r.path.as_str() {
        "/register" => Resp::Json(201, json!({"client_id": "c"}), vec![]),
        "/token" => tokens(r),
        _ => not_found(),
    }))
    .await;
    let url = format!("{}/mcp?codemode=false", server.origin);
    let ep = oauth::discover(&reqwest::Client::new(), &url, None)
        .await
        .unwrap();
    assert_eq!(ep.authorization, format!("{}/authorize", server.origin));
    assert_eq!(ep.token, format!("{}/token", server.origin));
    let (entry, urls) = login(&path, &url).await;
    entry.unwrap();
    let q: BTreeMap<String, String> = urls[0]
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(q["resource"], url);
    assert_eq!(server.posts("/token")[0].form()["resource"], url);
}

// mcp-oauth.test.ts: "tries the next metadata URL when one answers 502"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_tries_the_next_metadata_url_when_one_answers_502() {
    let server = serve(Arc::new(|r: &Req, base: &str| match r.path.as_str() {
        "/.well-known/oauth-protected-resource/mcp" => Resp::Empty(502, vec![]),
        "/.well-known/oauth-protected-resource" => {
            ok(json!({"resource": format!("{base}/mcp"), "authorization_servers": [format!("{base}/as")]}))
        }
        "/.well-known/oauth-authorization-server/as" => Resp::Empty(502, vec![]),
        "/.well-known/openid-configuration/as" => ok(json!({
            "issuer": format!("{base}/as"), "authorization_endpoint": format!("{base}/as/authorize"),
            "token_endpoint": format!("{base}/as/token")
        })),
        _ => not_found(),
    }))
    .await;
    let ep = oauth::discover(
        &reqwest::Client::new(),
        &format!("{}/mcp", server.origin),
        None,
    )
    .await
    .unwrap();
    assert_eq!(ep.token, format!("{}/as/token", server.origin));
}

fn refreshed(_: &Req) -> Resp {
    ok(
        json!({"access_token": "a2", "refresh_token": "r2", "expires_in": 3600, "token_type": "Bearer"}),
    )
}

async fn token_server(token: fn(&Req) -> Resp) -> Server {
    serve(Arc::new(move |r: &Req, _: &str| {
        match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/token") => token(r),
            _ => not_found(),
        }
    }))
    .await
}

// mcp-oauth.test.ts: "refreshes a token that expires within 30 seconds with
// the saved discovery before sending it" (adapted: within a minute)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_refreshes_a_token_about_to_expire_with_the_saved_endpoint() {
    let (_d, path) = store();
    let server = token_server(refreshed).await;
    let url = "https://mcp.example/mcp";
    saved(
        &path,
        url,
        &format!("{}/token", server.origin),
        Some(now() + 30),
    );
    let http = reqwest::Client::new();
    assert_eq!(
        oauth::token(&http, &path, "srv", url).await.as_deref(),
        Some("a2")
    );
    let form = server.posts("/token")[0].form();
    assert_eq!(
        (
            form["grant_type"].as_str(),
            form["refresh_token"].as_str(),
            form["resource"].as_str()
        ),
        ("refresh_token", "r1", url)
    );
    let e = oauth::load(&path, "srv", url).unwrap();
    assert_eq!(
        (e.access_token.as_str(), e.refresh_token.as_deref()),
        ("a2", Some("r2"))
    );
}

// mcp-oauth.test.ts: "leaves tokens with time left, without a lifetime, or
// saved without discovery to the 401" (adapted: a login is always saved
// with its token endpoint)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_leaves_tokens_with_time_left_or_without_a_lifetime_alone() {
    let server = token_server(refreshed).await;
    let url = "https://mcp.example/mcp";
    for expires in [Some(now() + 600), None] {
        let (_d, path) = store();
        saved(&path, url, &format!("{}/token", server.origin), expires);
        let tok = oauth::token(&reqwest::Client::new(), &path, "srv", url).await;
        assert_eq!(tok.as_deref(), Some("a1"), "{expires:?}");
    }
    assert!(server.requests().is_empty());
}

fn unavailable(_: &Req) -> Resp {
    Resp::Json(503, json!({"error": "temporarily_unavailable"}), vec![])
}

fn invalid_grant(_: &Req) -> Resp {
    Resp::Json(400, json!({"error": "invalid_grant"}), vec![])
}

// mcp-oauth.test.ts: "sends the current token when the refresh fails" and
// "fails without signing out when the token endpoint answers 503"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_sends_the_current_token_and_keeps_the_login_when_the_refresh_fails() {
    let (_d, path) = store();
    let server = token_server(unavailable).await;
    let url = "https://mcp.example/mcp";
    saved(
        &path,
        url,
        &format!("{}/token", server.origin),
        Some(now() + 10),
    );
    let tok = oauth::token(&reqwest::Client::new(), &path, "srv", url).await;
    assert_eq!(tok.as_deref(), Some("a1"));
    assert_eq!(server.posts("/token").len(), 1);
    let e = oauth::load(&path, "srv", url).unwrap();
    assert_eq!(
        (e.access_token.as_str(), e.refresh_token.as_deref()),
        ("a1", Some("r1"))
    );
}

// mcp-oauth.test.ts: "fails without signing out when the token endpoint is
// unreachable"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_keeps_the_login_when_the_token_endpoint_is_unreachable() {
    let (_d, path) = store();
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", closed.local_addr().unwrap());
    drop(closed);
    let url = "https://mcp.example/mcp";
    saved(&path, url, &endpoint, Some(now() + 10));
    let tok = oauth::token(&reqwest::Client::new(), &path, "srv", url).await;
    assert_eq!(tok.as_deref(), Some("a1"));
    assert!(oauth::load(&path, "srv", url).is_some());
}

// mcp-oauth.test.ts: "signs out when the token endpoint answers
// invalid_grant"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_signs_out_when_the_token_endpoint_answers_invalid_grant() {
    let (_d, path) = store();
    let server = token_server(invalid_grant).await;
    let url = "https://mcp.example/mcp";
    saved(
        &path,
        url,
        &format!("{}/token", server.origin),
        Some(now() + 10),
    );
    assert_eq!(
        oauth::token(&reqwest::Client::new(), &path, "srv", url).await,
        None
    );
    assert_eq!(oauth::load(&path, "srv", url), None, "signed out");
}

// mcp-oauth.test.ts: "keeps the credential when the refresh times out"
// (adapted: the saved token is sent once the time is up)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_keeps_the_credential_when_the_refresh_times_out() {
    let (_d, path) = store();
    let server = serve(Arc::new(|_: &Req, _: &str| Resp::Hang)).await;
    let url = "https://mcp.example/mcp";
    saved(
        &path,
        url,
        &format!("{}/token", server.origin),
        Some(now() + 10),
    );
    let started = Instant::now();
    let tok = oauth::token_within(
        &reqwest::Client::new(),
        &path,
        "srv",
        url,
        Duration::from_millis(300),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(tok.as_deref(), Some("a1"));
    let e = oauth::load(&path, "srv", url).unwrap();
    assert_eq!(
        (e.access_token.as_str(), e.refresh_token.as_deref()),
        ("a1", Some("r1"))
    );
}

// mcp-oauth.test.ts: "sends one refresh when two instances refresh the same
// credential at once" and "uses the stored tokens instead of spending a
// refresh token another instance already spent"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_sends_one_refresh_when_two_refresh_at_once() {
    let (_d, path) = store();
    let server = token_server(refreshed).await;
    let url = "https://mcp.example/mcp";
    saved(
        &path,
        url,
        &format!("{}/token", server.origin),
        Some(now() + 10),
    );
    let http = reqwest::Client::new();
    let (a, b) = tokio::join!(
        oauth::token(&http, &path, "srv", url),
        oauth::token(&http, &path, "srv", url)
    );
    assert_eq!((a.as_deref(), b.as_deref()), (Some("a2"), Some("a2")));
    assert_eq!(server.posts("/token").len(), 1, "one refresh");
    // A later request uses what the refresh saved.
    assert_eq!(
        oauth::token(&http, &path, "srv", url).await.as_deref(),
        Some("a2")
    );
    assert_eq!(server.posts("/token").len(), 1);
}

// mcp-oauth.test.ts: "sends nothing to an authorization server other than
// the one that issued the credential"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_refreshes_only_at_the_server_that_issued_the_credential() {
    let (_d, path) = store();
    let issuer = token_server(refreshed).await;
    let other = serve(authorization_server(refreshed)).await;
    // The MCP server now points at another authorization server.
    let url = format!("{}/mcp", other.origin);
    saved(
        &path,
        &url,
        &format!("{}/token", issuer.origin),
        Some(now() + 10),
    );
    let tok = oauth::token(&reqwest::Client::new(), &path, "srv", &url).await;
    assert_eq!(tok.as_deref(), Some("a2"));
    assert_eq!(issuer.posts("/token").len(), 1);
    assert!(other.requests().is_empty(), "{:?}", other.requests());
}

fn remote(url: &str) -> McpConfig {
    serde_json::from_value(json!({"servers": {"srv": {"type": "remote", "url": url}}})).unwrap()
}

// mcp-oauth.test.ts: "reports needs_auth for an unauthorized server without
// a credential or any OAuth request" and "needs auth without a request when
// the credential was signed out"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_reports_needs_auth_without_a_credential_or_any_oauth_request() {
    support::isolate();
    let server = serve(authorization_server(tokens)).await;
    let url = format!("{}/mcp", server.origin);
    // Signed out: nothing saved in the store connections read.
    let _ = oauth::remove(&oauth::store_path(), "srv");
    let mcp = Mcp::connect(&remote(&url), Path::new("/")).await;
    assert_eq!(mcp.statuses()["srv"], Status::NeedsAuth);
    let seen = server.requests();
    assert!(
        seen.iter().all(|r| r.path == "/mcp"),
        "no OAuth request: {seen:?}"
    );
    assert!(seen
        .iter()
        .all(|r| !r.headers.contains_key("authorization")));
    let _ = seen.first().map(|r| r.query.len());
}

/// Log in, playing a browser that sends the callback `params` (the state
/// replaced when `state` is given).
async fn login_with(
    path: &Path,
    url: &str,
    params: &'static [(&'static str, &'static str)],
    state: Option<&'static str>,
) -> anyhow::Result<Entry> {
    let setting: oauth::Setting = serde_json::from_value(json!({"port": 0})).unwrap();
    oauth::login(
        &reqwest::Client::new(),
        path,
        "srv",
        url,
        Some(&setting),
        move |u| {
            let u = reqwest::Url::parse(u).unwrap();
            tokio::spawn(async move {
                let q: BTreeMap<String, String> = u
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                let mut cb = reqwest::Url::parse(&q["redirect_uri"]).unwrap();
                cb.query_pairs_mut()
                    .append_pair("state", state.unwrap_or(&q["state"]));
                for (k, v) in params {
                    cb.query_pairs_mut().append_pair(k, v);
                }
                let _ = reqwest::get(cb).await;
            });
        },
    )
    .await
}

// Negative cases around the callback fix: a callback carrying an error
// still ends the login (it is the redirect), and one with another state is
// refused rather than redeemed, so a forged redirect cannot sign in.
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_ends_on_an_error_callback_and_refuses_a_forged_state() {
    let (_d, path) = store();
    let server = serve(authorization_server(tokens)).await;
    let url = format!("{}/mcp", server.origin);
    let e = login_with(&path, &url, &[("error", "access_denied")], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("access_denied"), "{e}");
    let e = login_with(&path, &url, &[("code", "accepted")], Some("forged"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("state"), "{e}");
    assert!(server.posts("/token").is_empty(), "no code was redeemed");
    assert_eq!(oauth::load(&path, "srv", &url), None);
}

// Only the authorization server refusing the grant signs out: a server
// error that happens to say invalid_grant, or a refusal of another kind,
// keeps the login and sends its token.
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_signs_out_only_when_the_grant_is_refused() {
    fn server_error(_: &Req) -> Resp {
        Resp::Json(500, json!({"error": "invalid_grant"}), vec![])
    }
    fn other_refusal(_: &Req) -> Resp {
        Resp::Json(400, json!({"error": "invalid_client"}), vec![])
    }
    for token in [server_error as fn(&Req) -> Resp, other_refusal] {
        let (_d, path) = store();
        let server = token_server(token).await;
        let url = "https://mcp.example/mcp";
        saved(
            &path,
            url,
            &format!("{}/token", server.origin),
            Some(now() + 10),
        );
        let tok = oauth::token(&reqwest::Client::new(), &path, "srv", url).await;
        assert_eq!(tok.as_deref(), Some("a1"));
        assert!(oauth::load(&path, "srv", url).is_some());
    }
}

// A login without a refresh token near its expiry sends what it has.
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_sends_a_token_it_cannot_refresh() {
    let (_d, path) = store();
    let url = "https://mcp.example/mcp";
    oauth::save(
        &path,
        "srv",
        &Entry {
            server_url: url.into(),
            client_id: "client".into(),
            token_endpoint: "http://127.0.0.1:9/token".into(),
            access_token: "a1".into(),
            expires_at: Some(now() + 10),
            ..Default::default()
        },
    )
    .unwrap();
    let tok = oauth::token(&reqwest::Client::new(), &path, "srv", url).await;
    assert_eq!(tok.as_deref(), Some("a1"));
}
