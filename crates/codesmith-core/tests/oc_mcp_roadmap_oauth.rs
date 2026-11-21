//! Ports of OpenCode's `mcp-oauth.test.ts` (MIT) for the MCP OAuth Login
//! Options roadmap group, with the security and edge cases around them.
//! The authorization server and MCP endpoint are played by scripted local
//! HTTP servers on loopback; the browser by an HTTP client following the
//! authorization URL to the loopback callback, or by lines pasted into the
//! login. Tokens are saved in a temporary store, and the user's home and
//! data folders are kept out (`support::isolate`). Waits are bounded by
//! deadlines over whole loops; nothing sleeps a fixed time.

mod support;

use codesmith_core::mcp::oauth::{self, Entry};
use codesmith_core::mcp::{Mcp, McpConfig, Status};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// The longest any one test waits for something to happen.
const DEADLINE: Duration = Duration::from_secs(30);

// --- A scripted HTTP server ---

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
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
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

enum Resp {
    Json(u16, Value, Vec<(String, String)>),
    Raw(u16, Vec<u8>, Vec<(String, String)>),
    Empty(u16, Vec<(String, String)>),
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
                    headers: lines
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                        .collect(),
                    body,
                };
                log.lock().unwrap().push(req.clone());
                let (status, mut headers, body) = match handler(&req, &base) {
                    Resp::Json(c, v, h) => (c, h, v.to_string().into_bytes()),
                    Resp::Raw(c, b, h) => (c, h, b),
                    Resp::Empty(c, h) => (c, h, vec![]),
                };
                if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                {
                    headers.push(("Content-Type".into(), "application/json".into()));
                }
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let mut bytes = out.into_bytes();
                bytes.extend_from_slice(&body);
                let _ = s.write_all(&bytes).await;
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

fn query(u: &reqwest::Url) -> BTreeMap<String, String> {
    u.query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Authorization server metadata at `base` with `extra` fields.
fn metadata(base: &str, extra: &Value) -> Value {
    let mut m = json!({
        "issuer": base, "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"), "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"]
    });
    for (k, v) in extra.as_object().unwrap() {
        m[k] = v.clone();
    }
    m
}

/// An authorization server (and MCP endpoint answering 401) at one origin,
/// whose metadata carries `extra` and whose token endpoint answers `token`.
fn authorization_server(extra: Value, token: fn(&Req) -> Resp) -> Handler {
    Arc::new(
        move |r: &Req, base: &str| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => ok(metadata(base, &extra)),
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

/// A login running on its own task: its authorization URL, the notices it
/// gave about pastes, a way to paste, and its result.
struct Login {
    auth: reqwest::Url,
    notices: mpsc::UnboundedReceiver<String>,
    paste: mpsc::Sender<String>,
    done: tokio::task::JoinHandle<anyhow::Result<Entry>>,
}

impl Login {
    fn q(&self) -> BTreeMap<String, String> {
        query(&self.auth)
    }
    fn redirect(&self) -> reqwest::Url {
        reqwest::Url::parse(&self.q()["redirect_uri"]).unwrap()
    }
    /// The redirect with `params` and the login's state (or `state`).
    fn callback(&self, state: Option<&str>, params: &[(&str, &str)]) -> reqwest::Url {
        let mut u = self.redirect();
        u.query_pairs_mut()
            .append_pair("state", state.unwrap_or(&self.q()["state"]));
        for (k, v) in params {
            u.query_pairs_mut().append_pair(k, v);
        }
        u
    }
    async fn notice(&mut self) -> String {
        tokio::time::timeout(DEADLINE, self.notices.recv())
            .await
            .expect("a notice in time")
            .expect("the login is still running")
    }
    async fn result(self) -> anyhow::Result<Entry> {
        tokio::time::timeout(DEADLINE, self.done)
            .await
            .expect("the login ended in time")
            .unwrap()
    }
}

/// Start a login for `url` with the `oauth` setting (its port 0 unless set).
async fn start_at(path: &Path, server: &str, url: &str, mut setting: Value) -> Login {
    if setting.get("port").is_none() && setting.get("redirectUri").is_none() {
        setting["port"] = json!(0);
    }
    let setting: oauth::Setting = serde_json::from_value(setting).unwrap();
    let (auth_tx, mut auth_rx) = mpsc::unbounded_channel::<String>();
    let (notice_tx, notices) = mpsc::unbounded_channel::<String>();
    let (paste, pastes) = mpsc::channel::<String>(8);
    let (path, server, url) = (path.to_path_buf(), server.to_string(), url.to_string());
    let done = tokio::spawn(async move {
        oauth::login_with(
            &reqwest::Client::new(),
            &path,
            &server,
            &url,
            Some(&setting),
            move |u| {
                let _ = auth_tx.send(u.to_string());
            },
            Some(pastes),
            move |n| {
                let _ = notice_tx.send(n.to_string());
            },
        )
        .await
    });
    let auth = tokio::time::timeout(DEADLINE, auth_rx.recv()).await;
    let auth = match auth {
        Ok(Some(a)) => reqwest::Url::parse(&a).unwrap(),
        // It ended before opening the browser: hand back its error.
        _ => {
            let e = tokio::time::timeout(DEADLINE, done)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            panic!("the login ended before opening the browser: {e:#}");
        }
    };
    Login {
        auth,
        notices,
        paste,
        done,
    }
}

async fn start(path: &Path, url: &str, setting: Value) -> Login {
    start_at(path, "srv", url, setting).await
}

/// A login expected to fail before it opens the browser.
async fn refused(path: &Path, url: &str, setting: Value) -> String {
    let setting: oauth::Setting = serde_json::from_value(setting).unwrap();
    let opened = Arc::new(Mutex::new(false));
    let o = opened.clone();
    let e = tokio::time::timeout(
        DEADLINE,
        oauth::login(
            &reqwest::Client::new(),
            path,
            "srv",
            url,
            Some(&setting),
            move |_| *o.lock().unwrap() = true,
        ),
    )
    .await
    .expect("the login ended in time")
    .unwrap_err();
    assert!(!*opened.lock().unwrap(), "nothing was opened");
    format!("{e:#}")
}

// mcp-oauth.test.ts: "completes from a pasted redirect URL and keeps waiting
// after a wrong paste" (adapted: a paste is also checked against the
// redirect address, and the notices are CodeSmith's)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_completes_from_a_pasted_redirect_url_and_keeps_waiting_after_a_wrong_paste() {
    let (_d, path) = store();
    let server = serve(authorization_server(json!({}), tokens)).await;
    let url = format!("{}/mcp", server.origin);
    let mut login = start(&path, &url, json!({"clientId": "client"})).await;

    let other = login.callback(Some("another-sign-in"), &[("code", "pasted")]);
    login.paste.send(other.to_string()).await.unwrap();
    assert_eq!(
        login.notice().await,
        "This URL belongs to a different sign-in"
    );
    login.paste.send("pasted".into()).await.unwrap();
    assert_eq!(
        login.notice().await,
        "Paste the full URL from the browser's address bar"
    );
    // The right state at another address is not this login's redirect.
    let mut elsewhere = login.callback(None, &[("code", "pasted")]);
    elsewhere.set_host(Some("attacker.example")).unwrap();
    login.paste.send(elsewhere.to_string()).await.unwrap();
    assert_eq!(
        login.notice().await,
        "This URL is not this sign-in's redirect address"
    );
    assert!(server.posts("/token").is_empty(), "nothing redeemed yet");

    let right = login.callback(None, &[("code", "pasted")]);
    login.paste.send(format!(" {right}\n")).await.unwrap();
    let entry = login.result().await.unwrap();
    assert_eq!(entry.access_token, "access");
    let posts = server.posts("/token");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].form()["code"], "pasted");
    assert_eq!(
        posts[0].form()["redirect_uri"],
        right.as_str().split('?').next().unwrap()
    );
}

// Edge cases of pasting: empty and huge lines, a URL without a code, and a
// code in another script all keep the login waiting or complete it as sent.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_paste_edge_cases() {
    let (_d, path) = store();
    let server = serve(authorization_server(json!({}), tokens)).await;
    let url = format!("{}/mcp", server.origin);
    let mut login = start(&path, &url, json!({"clientId": "client"})).await;
    for line in ["", "   \n", &"x".repeat(200_000)] {
        login.paste.send(line.to_string()).await.unwrap();
        assert_eq!(
            login.notice().await,
            "Paste the full URL from the browser's address bar"
        );
    }
    login
        .paste
        .send(login.callback(None, &[]).to_string())
        .await
        .unwrap();
    assert_eq!(
        login.notice().await,
        "The URL does not contain an authorization code"
    );
    let code = "κωδικός-✓-🦀";
    let right = login.callback(None, &[("code", code)]);
    login.paste.send(right.to_string()).await.unwrap();
    login.result().await.unwrap();
    assert_eq!(server.posts("/token")[0].form()["code"], code);
}

// A pasted redirect carrying an error ends the login, as the browser's
// would, and its description reaches the terminal without control
// characters.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_a_pasted_error_ends_the_login_with_a_clean_message() {
    let (_d, path) = store();
    let server = serve(authorization_server(json!({}), tokens)).await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({"clientId": "client"})).await;
    let bad = login.callback(
        None,
        &[
            ("error", "access_denied"),
            ("error_description", "\u{1b}[2Jno\u{202e}pe\u{7}"),
        ],
    );
    login.paste.send(bad.to_string()).await.unwrap();
    let e = format!("{:#}", login.result().await.unwrap_err());
    assert!(e.contains("access_denied"), "{e}");
    assert!(e.contains("nope"), "{e}");
    assert!(
        !e.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "{e:?}"
    );
    assert!(server.posts("/token").is_empty());
}

// mcp-oauth.test.ts: "reports a rejected token exchange on the callback
// page"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_reports_a_rejected_token_exchange_on_the_callback_page() {
    fn refuse(_: &Req) -> Resp {
        Resp::Json(
            400,
            json!({"error": "invalid_client", "error_description": "bad_client_secret"}),
            vec![],
        )
    }
    let (_d, path) = store();
    let server = serve(authorization_server(json!({}), refuse)).await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({"clientId": "client"})).await;
    let page = reqwest::get(login.callback(None, &[("code", "accepted")]))
        .await
        .unwrap();
    assert_eq!(page.status(), 400);
    let text = page.text().await.unwrap();
    assert!(text.contains("Authorization failed"), "{text}");
    assert!(text.contains("bad_client_secret"), "{text}");
    let e = format!("{:#}", login.result().await.unwrap_err());
    assert!(e.contains("bad_client_secret"), "{e}");
    assert_eq!(oauth::load(&path, "srv", &url), None);
}

// The callback page says a login worked only once the code is redeemed and
// saved; the saved file is the owner's alone.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_the_callback_page_reports_success_after_the_exchange() {
    let (_d, path) = store();
    let server = serve(authorization_server(json!({}), tokens)).await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({"clientId": "client"})).await;
    let page = reqwest::get(login.callback(None, &[("code", "accepted")]))
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page
        .text()
        .await
        .unwrap()
        .contains("Authorization successful"));
    login.result().await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

// mcp-oauth.test.ts: "preserves a configured redirect URL and fixed port",
// with "registers dynamically when a custom redirect_uri is configured"
// (even where client metadata documents are accepted)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_preserves_a_configured_redirect_url_and_fixed_port() {
    let (_d, path) = store();
    let server = serve(authorization_server(
        json!({"client_id_metadata_document_supported": true, "token_endpoint_auth_methods_supported": ["none"]}),
        tokens,
    ))
    .await;
    let url = format!("{}/mcp", server.origin);
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let redirect = format!("http://127.0.0.1:{port}/fixed-callback");
    let login = start(
        &path,
        &url,
        json!({"redirectUri": redirect, "clientMetadataUrl": "https://client.example/codesmith.json"}),
    )
    .await;
    assert_eq!(login.q()["redirect_uri"], redirect);
    assert_eq!(login.q()["client_id"], "registered");
    let regs = server.posts("/register");
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].json()["redirect_uris"], json!([redirect]));
    // The listener is on the redirect's port and path.
    let http = reqwest::Client::new();
    let other_path = format!("http://127.0.0.1:{port}{}", oauth::CALLBACK_PATH);
    assert_eq!(http.get(other_path).send().await.unwrap().status(), 404);
    let page = http
        .get(login.callback(None, &[("code", "accepted")]))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    login.result().await.unwrap();
    assert_eq!(server.posts("/token")[0].form()["redirect_uri"], redirect);
}

// mcp-oauth.test.ts: "rejects an invalid redirect URL"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_rejects_an_invalid_redirect_url() {
    let (_d, path) = store();
    let server = serve(authorization_server(json!({}), tokens)).await;
    let url = format!("{}/mcp", server.origin);
    for bad in [
        "not a URL",
        "",
        "javascript:alert(1)",
        "file:///tmp/cb",
        "ftp://127.0.0.1/cb",
    ] {
        let e = refused(&path, &url, json!({"clientId": "c", "redirectUri": bad})).await;
        assert!(e.contains("invalid redirect URL"), "{bad}: {e}");
    }
    assert!(server.requests().is_empty(), "nothing was sent");
}

// mcp-oauth.test.ts: "uses configured authorization server metadata when
// the resource publishes none" and "refreshes through configured
// authorization server metadata" (adapted: the refresh after the server
// refused the token at connect)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_uses_and_refreshes_through_configured_authorization_server_metadata() {
    let (_d, path) = store();
    let issuer = serve(authorization_server(json!({}), tokens)).await;
    // The resource publishes no metadata and refuses everything.
    let resource = serve(Arc::new(|r: &Req, _: &str| match r.path.as_str() {
        "/mcp" => Resp::Empty(401, vec![]),
        _ => not_found(),
    }))
    .await;
    let url = format!("{}/mcp", resource.origin);
    let meta = format!("{}/.well-known/oauth-authorization-server", issuer.origin);
    let login = start(
        &path,
        &url,
        json!({"clientId": "client", "authServerMetadataUrl": meta}),
    )
    .await;
    assert_eq!(
        login.auth.origin().ascii_serialization(),
        issuer.origin,
        "the configured server, not the resource's origin"
    );
    assert_eq!(login.auth.path(), "/authorize");
    assert_eq!(login.q()["resource"], url);
    reqwest::get(login.callback(None, &[("code", "accepted")]))
        .await
        .unwrap();
    let e = login.result().await.unwrap();
    assert_eq!(e.token_endpoint, format!("{}/token", issuer.origin));
    assert!(
        resource
            .requests()
            .iter()
            .all(|r| !r.path.starts_with("/.well-known")),
        "no discovery at the resource"
    );
}

// A configured metadata document must answer, over https or loopback.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_configured_metadata_must_load_from_a_secure_url() {
    let (_d, path) = store();
    let resource = serve(Arc::new(|_: &Req, _: &str| not_found())).await;
    let url = format!("{}/mcp", resource.origin);
    let missing = format!("{}/missing", resource.origin);
    let e = refused(
        &path,
        &url,
        json!({"clientId": "c", "authServerMetadataUrl": missing}),
    )
    .await;
    assert!(
        e.contains("could not load the authorization server metadata"),
        "{e}"
    );
    let e = refused(
        &path,
        &url,
        json!({"clientId": "c", "authServerMetadataUrl": "http://auth.example/.well-known/oauth-authorization-server"}),
    )
    .await;
    assert!(e.contains("OAuth needs https"), "{e}");
}

// mcp-oauth.test.ts: "requests offline_access without forcing a consent
// prompt", with the cases around it
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_requests_offline_access_without_forcing_a_consent_prompt() {
    let (_d, path) = store();
    let offering = serve(authorization_server(
        json!({"scopes_supported": ["read", "offline_access"]}),
        tokens,
    ))
    .await;
    let url = format!("{}/mcp", offering.origin);
    let scope_of = |l: &Login| (l.q().get("scope").cloned(), l.q().contains_key("prompt"));
    let l = start(&path, &url, json!({"clientId": "client", "scope": "read"})).await;
    assert_eq!(scope_of(&l), (Some("read offline_access".into()), false));
    let l = start(
        &path,
        &url,
        json!({"clientId": "client", "scope": "offline_access read"}),
    )
    .await;
    assert_eq!(
        scope_of(&l),
        (Some("offline_access read".into()), false),
        "not twice"
    );
    let l = start(&path, &url, json!({"clientId": "client"})).await;
    assert_eq!(scope_of(&l), (Some("read offline_access".into()), false));
    // A server that does not offer it is not asked for it.
    let plain = serve(authorization_server(
        json!({"scopes_supported": ["read"]}),
        tokens,
    ))
    .await;
    let url = format!("{}/mcp", plain.origin);
    let l = start(&path, &url, json!({"clientId": "client", "scope": "read"})).await;
    assert_eq!(scope_of(&l), (Some("read".into()), false));
}

// mcp-oauth.test.ts: "forwards iss from the redirect so issuer-advertising
// servers can complete" (adapted: CodeSmith checks the iss itself, RFC 9207)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_accepts_the_iss_of_an_issuer_advertising_server() {
    let (_d, path) = store();
    let server = serve(authorization_server(
        json!({"authorization_response_iss_parameter_supported": true}),
        tokens,
    ))
    .await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({"clientId": "client"})).await;
    let page = reqwest::get(login.callback(None, &[("code", "accepted"), ("iss", &server.origin)]))
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(login.result().await.unwrap().access_token, "access");
}

// An iss naming another issuer (a mix-up), or none from a server that says
// it sends one, ends the login without redeeming the code, whether the
// browser brings it or it is pasted.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_refuses_a_wrong_or_missing_iss() {
    let (_d, path) = store();
    let advertising = serve(authorization_server(
        json!({"authorization_response_iss_parameter_supported": true}),
        tokens,
    ))
    .await;
    let url = format!("{}/mcp", advertising.origin);
    let login = start(&path, &url, json!({"clientId": "client"})).await;
    let page =
        reqwest::get(login.callback(None, &[("code", "c"), ("iss", "https://evil.example")]))
            .await
            .unwrap();
    assert_eq!(page.status(), 400);
    let e = format!("{:#}", login.result().await.unwrap_err());
    assert!(e.contains("another issuer"), "{e}");
    let login = start(&path, &url, json!({"clientId": "client"})).await;
    reqwest::get(login.callback(None, &[("code", "c")]))
        .await
        .unwrap();
    let e = format!("{:#}", login.result().await.unwrap_err());
    assert!(e.contains("did not name its issuer"), "{e}");
    // A server that does not advertise it is still held to an iss it sends.
    let quiet = serve(authorization_server(json!({}), tokens)).await;
    let url = format!("{}/mcp", quiet.origin);
    let login = start(&path, &url, json!({"clientId": "client"})).await;
    let pasted = login.callback(None, &[("code", "c"), ("iss", "https://evil.example/")]);
    login.paste.send(pasted.to_string()).await.unwrap();
    let e = format!("{:#}", login.result().await.unwrap_err());
    assert!(e.contains("another issuer"), "{e}");
    assert!(advertising.posts("/token").is_empty());
    assert!(quiet.posts("/token").is_empty());
}

// mcp-oauth.test.ts: "uses the client metadata document when the server
// supports public CIMD clients" (adapted: the document is one the user
// hosts and configures; CodeSmith publishes none and uses no vendor's)
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_uses_a_configured_client_metadata_document_with_public_cimd_servers() {
    let (_d, path) = store();
    let doc = "https://client.example/codesmith.json";
    let server = serve(authorization_server(
        json!({"client_id_metadata_document_supported": true, "token_endpoint_auth_methods_supported": ["none"]}),
        tokens,
    ))
    .await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({"clientMetadataUrl": doc})).await;
    assert_eq!(login.q()["client_id"], doc);
    reqwest::get(login.callback(None, &[("code", "accepted")]))
        .await
        .unwrap();
    let e = login.result().await.unwrap();
    assert_eq!(e.client_id, doc);
    assert!(server.posts("/register").is_empty());
    assert_eq!(server.posts("/token")[0].form()["client_id"], doc);

    // Without a document of the user's, the client is registered.
    let (_d2, path2) = store();
    let login = start(&path2, &url, json!({})).await;
    assert_eq!(login.q()["client_id"], "registered");
}

// A server that takes documents only from confidential clients gets a
// registered client; a document URL that is not https is refused.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_registers_when_the_server_takes_no_public_documents() {
    let (_d, path) = store();
    let server = serve(authorization_server(
        json!({"client_id_metadata_document_supported": true, "token_endpoint_auth_methods_supported": ["client_secret_post"]}),
        tokens,
    ))
    .await;
    let url = format!("{}/mcp", server.origin);
    let login = start(
        &path,
        &url,
        json!({"clientMetadataUrl": "https://client.example/c.json"}),
    )
    .await;
    assert_eq!(login.q()["client_id"], "registered");
    assert_eq!(server.posts("/register").len(), 1);
    for bad in [
        "http://client.example/c.json",
        "https://client.example",
        "nope",
    ] {
        let e = refused(&path, &url, json!({"clientMetadataUrl": bad})).await;
        assert!(e.contains("invalid clientMetadataUrl"), "{bad}: {e}");
    }
}

// mcp-oauth.test.ts: "redeems the code with the localhost redirect a server
// sent the browser back to"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_redeems_with_the_localhost_redirect_the_browser_came_back_to() {
    let (_d, path) = store();
    let server = serve(authorization_server(json!({}), tokens)).await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({})).await;
    let redirect = login.redirect();
    let port = redirect.port().unwrap();
    let cb = login.callback(None, &[("code", "accepted")]);
    // The server rewrote 127.0.0.1 to localhost; the browser reaches the
    // same listener.
    let page = reqwest::Client::new()
        .get(cb)
        .header("Host", format!("localhost:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    login.result().await.unwrap();
    let path_ = oauth::CALLBACK_PATH;
    assert_eq!(
        server.posts("/register")[0].json()["redirect_uris"],
        json!([
            format!("http://127.0.0.1:{port}{path_}"),
            format!("http://localhost:{port}{path_}")
        ])
    );
    assert_eq!(
        server.posts("/token")[0].form()["redirect_uri"],
        format!("http://localhost:{port}{path_}")
    );
}

/// An MCP endpoint at `/mcp` taking only `Bearer a2`, and a token endpoint
/// at `/token` answering with `token`.
fn guarded(token: fn(&Req) -> Resp) -> Handler {
    Arc::new(
        move |r: &Req, _: &str| match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/token") => token(r),
            ("POST", "/mcp") => {
                if r.headers.get("authorization").map(String::as_str) != Some("Bearer a2") {
                    return Resp::Empty(401, vec![]);
                }
                let m = r.json();
                let Some(id) = m.get("id").cloned() else {
                    return Resp::Empty(202, vec![]);
                };
                let result = match m["method"].as_str() {
                    Some("initialize") => {
                        json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "g", "version": "1"}})
                    }
                    Some("tools/list") => {
                        json!({"tools": [{"name": "whoami", "inputSchema": {"type": "object"}}]})
                    }
                    _ => json!({}),
                };
                ok(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
            _ => not_found(),
        },
    )
}

fn rotate(_: &Req) -> Resp {
    ok(
        json!({"access_token": "a2", "refresh_token": "r2", "expires_in": 3600, "token_type": "Bearer"}),
    )
}

/// A saved login (`a1`/`r1`, an hour left) for `name` at `url`.
fn signed_in(name: &str, url: &str, token_endpoint: &str) {
    oauth::save(
        &oauth::store_path(),
        name,
        &Entry {
            server_url: url.into(),
            client_id: "client".into(),
            token_endpoint: token_endpoint.into(),
            access_token: "a1".into(),
            refresh_token: Some("r1".into()),
            expires_at: Some(now() + 3600),
            ..Default::default()
        },
    )
    .unwrap();
}

fn remote(name: &str, url: &str) -> McpConfig {
    serde_json::from_value(json!({"servers": {name: {"type": "remote", "url": url}}})).unwrap()
}

// mcp-oauth.test.ts: "refreshes a refused token and retries with the new
// one"
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_refreshes_a_refused_token_and_retries_with_the_new_one() {
    support::isolate();
    let server = serve(guarded(rotate)).await;
    let url = format!("{}/mcp", server.origin);
    let name = "refused-retry";
    signed_in(name, &url, &format!("{}/token", server.origin));
    let mcp = Mcp::connect(&remote(name, &url), Path::new("/")).await;
    assert_eq!(mcp.statuses()[name], Status::Connected { tools: 1 });
    let refreshes = server.posts("/token");
    assert_eq!(refreshes.len(), 1);
    let form = refreshes[0].form();
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "r1");
    assert_eq!(form["resource"], url);
    let e = oauth::load(&oauth::store_path(), name, &url).unwrap();
    assert_eq!(
        (e.access_token.as_str(), e.refresh_token.as_deref()),
        ("a2", Some("r2"))
    );
}

// A refresh that does not help is tried once: the server is reported as
// needing a login, and a refused grant signs it out.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_a_refused_token_is_refreshed_once() {
    fn unavailable(_: &Req) -> Resp {
        Resp::Raw(503, b"upstream unavailable".to_vec(), vec![])
    }
    fn invalid(_: &Req) -> Resp {
        Resp::Json(400, json!({"error": "invalid_grant"}), vec![])
    }
    // The new token is refused too.
    fn useless(_: &Req) -> Resp {
        ok(json!({"access_token": "a3", "refresh_token": "r3", "expires_in": 3600}))
    }
    support::isolate();
    for (name, token, kept) in [
        ("refresh-unavailable", unavailable as fn(&Req) -> Resp, true),
        ("refresh-invalid", invalid, false),
        ("refresh-useless", useless, true),
    ] {
        let server = serve(guarded(token)).await;
        let url = format!("{}/mcp", server.origin);
        signed_in(name, &url, &format!("{}/token", server.origin));
        let mcp = Mcp::connect(&remote(name, &url), Path::new("/")).await;
        assert_eq!(mcp.statuses()[name], Status::NeedsAuth, "{name}");
        assert_eq!(server.posts("/token").len(), 1, "{name}: one refresh");
        assert_eq!(
            server.posts("/mcp").len(),
            if name == "refresh-useless" { 2 } else { 1 },
            "{name}"
        );
        assert_eq!(
            oauth::load(&oauth::store_path(), name, &url).is_some(),
            kept,
            "{name}"
        );
    }
}

// A token another process refreshed meanwhile is used rather than spending
// the refresh token again.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_a_refused_token_uses_one_another_process_saved() {
    let (_d, path) = store();
    let server = serve(guarded(rotate)).await;
    let url = format!("{}/mcp", server.origin);
    oauth::save(
        &path,
        "srv",
        &Entry {
            server_url: url.clone(),
            client_id: "c".into(),
            token_endpoint: format!("{}/token", server.origin),
            access_token: "a2".into(),
            refresh_token: Some("r2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let got = oauth::refresh_refused(&path, "srv", &url, "a1", Duration::from_secs(5)).await;
    assert_eq!(got.as_deref(), Some("a2"));
    assert!(server.posts("/token").is_empty());
}

// mcp-oauth.test.ts: "reports needs_auth when the server asks for more
// scope", and the next login asks for that scope
#[tokio::test(flavor = "multi_thread")]
async fn oc_mcp_oauth_reports_needs_auth_when_the_server_asks_for_more_scope() {
    let (_d, path) = store();
    let server = serve(Arc::new(|r: &Req, base: &str| {
        match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/mcp") => Resp::Empty(
                403,
                vec![(
                    "WWW-Authenticate".into(),
                    r#"Bearer error="insufficient_scope", scope="admin files:read""#.into(),
                )],
            ),
            ("GET", "/.well-known/oauth-authorization-server") => ok(metadata(base, &json!({}))),
            ("POST", "/token") => rotate(r),
            _ => not_found(),
        }
    }))
    .await;
    let url = format!("{}/mcp", server.origin);
    let name = "more-scope";
    signed_in(name, &url, &format!("{}/token", server.origin));
    let mcp = Mcp::connect(&remote(name, &url), Path::new("/")).await;
    assert_eq!(mcp.statuses()[name], Status::NeedsAuth);
    assert!(server.posts("/token").is_empty(), "a refresh adds no scope");
    // Logging in again (with the saved token, so the server says what it
    // wants) asks for the scope it named.
    oauth::save(
        &path,
        "srv",
        &oauth::load(&oauth::store_path(), name, &url).unwrap(),
    )
    .unwrap();
    let login = start(&path, &url, json!({"clientId": "client"})).await;
    assert_eq!(login.q()["scope"], "admin files:read");
    let probe = &server.posts("/mcp")[1];
    assert_eq!(probe.headers["authorization"], "Bearer a1");
}

// A 403 that is not about scope is a failure, not a login matter.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_a_plain_403_is_a_failure() {
    support::isolate();
    let server = serve(Arc::new(|_: &Req, _: &str| Resp::Empty(403, vec![]))).await;
    let url = format!("{}/mcp", server.origin);
    let mcp = Mcp::connect(&remote("plain-403", &url), Path::new("/")).await;
    assert!(
        matches!(&mcp.statuses()["plain-403"], Status::Failed(e) if e.contains("403")),
        "{:?}",
        mcp.statuses()
    );
}

// --- Where OAuth requests may go ---

// Discovery that points at a plain-http authorization server off loopback
// is refused before anything is sent there or opened.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_refuses_an_insecure_authorization_server() {
    let (_d, path) = store();
    let server = serve(Arc::new(|r: &Req, base: &str| match r.path.as_str() {
        "/.well-known/oauth-protected-resource/mcp" => ok(json!({
            "resource": format!("{base}/mcp"), "authorization_servers": ["http://auth.example"]
        })),
        _ => not_found(),
    }))
    .await;
    let url = format!("{}/mcp", server.origin);
    let e = refused(&path, &url, json!({"clientId": "c"})).await;
    assert!(e.contains("OAuth needs https"), "{e}");
    // Endpoints in metadata are held to the same rule.
    let insecure = serve(Arc::new(|r: &Req, base: &str| match r.path.as_str() {
        "/.well-known/oauth-authorization-server" => ok(metadata(
            base,
            &json!({"token_endpoint": "http://tokens.example/token"}),
        )),
        _ => not_found(),
    }))
    .await;
    let url = format!("{}/mcp", insecure.origin);
    let e = refused(&path, &url, json!({"clientId": "c"})).await;
    assert!(e.contains("token endpoint"), "{e}");
    // So is the MCP server itself.
    let e = refused(&path, "http://mcp.example/mcp", json!({"clientId": "c"})).await;
    assert!(e.contains("OAuth needs https"), "{e}");
}

// Metadata naming another issuer than the one it was discovered for is
// refused, so one server's document cannot stand in for another's.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_refuses_metadata_for_another_issuer() {
    let (_d, path) = store();
    let server = serve(Arc::new(|r: &Req, base: &str| match r.path.as_str() {
        "/.well-known/oauth-authorization-server" => ok(metadata(
            base,
            &json!({"issuer": "https://someone-else.example"}),
        )),
        _ => not_found(),
    }))
    .await;
    let url = format!("{}/mcp", server.origin);
    let e = refused(&path, &url, json!({"clientId": "c"})).await;
    assert!(e.contains("names another issuer"), "{e}");
    assert!(server.posts("/token").is_empty());
}

// A metadata document that redirects is not followed to where it points.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_follows_no_redirects() {
    let (_d, path) = store();
    let elsewhere = serve(authorization_server(json!({}), tokens)).await;
    let target = format!(
        "{}/.well-known/oauth-authorization-server",
        elsewhere.origin
    );
    let server = serve(Arc::new(move |r: &Req, _: &str| match r.path.as_str() {
        "/.well-known/oauth-authorization-server" => {
            Resp::Empty(302, vec![("Location".into(), target.clone())])
        }
        "/register" => Resp::Json(201, json!({"client_id": "c"}), vec![]),
        _ => not_found(),
    }))
    .await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({})).await;
    // Discovery fell back to the server's own origin.
    assert_eq!(login.auth.origin().ascii_serialization(), server.origin);
    assert!(
        elsewhere.requests().is_empty(),
        "{:?}",
        elsewhere.requests()
    );
}

// Responses are capped: oversized metadata counts as none, and an
// oversized token response fails the login.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_caps_response_sizes() {
    fn huge(_: &Req) -> Resp {
        let pad = "x".repeat(oauth::MAX_DOCUMENT + 1);
        ok(json!({"access_token": "a", "pad": pad}))
    }
    let (_d, path) = store();
    let server = serve(Arc::new(|r: &Req, base: &str| {
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/.well-known/oauth-authorization-server") => {
                let mut m = metadata(base, &json!({}));
                m["authorization_endpoint"] = json!(format!("{base}/big/authorize"));
                m["pad"] = json!("x".repeat(oauth::MAX_DOCUMENT));
                ok(m)
            }
            ("POST", "/token") => huge(r),
            _ => not_found(),
        }
    }))
    .await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({"clientId": "c"})).await;
    assert_eq!(
        login.auth.path(),
        "/authorize",
        "the oversized document was not used"
    );
    reqwest::get(login.callback(None, &[("code", "c")]))
        .await
        .unwrap();
    let e = format!("{:#}", login.result().await.unwrap_err());
    assert!(e.contains("larger than"), "{e}");
}

// What a token endpoint says goes to the terminal without control or
// invisible characters, and cut short.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_oauth_cleans_token_errors_for_the_terminal() {
    fn noisy(_: &Req) -> Resp {
        let d = format!(
            "\u{1b}]0;title\u{7}bad\u{202e}\r\nsecret{}",
            "!".repeat(5000)
        );
        Resp::Json(
            400,
            json!({"error": "invalid_client", "error_description": d}),
            vec![],
        )
    }
    let (_d, path) = store();
    let server = serve(authorization_server(json!({}), noisy)).await;
    let url = format!("{}/mcp", server.origin);
    let login = start(&path, &url, json!({"clientId": "c"})).await;
    reqwest::get(login.callback(None, &[("code", "c")]))
        .await
        .unwrap();
    let e = format!("{:#}", login.result().await.unwrap_err());
    assert!(e.contains("bad"), "{e}");
    assert!(
        !e.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "{e:?}"
    );
    assert!(e.chars().count() < 600, "{}", e.len());
}

#[test]
fn mcp_oauth_secure_urls() {
    let ok = |u: &str| oauth::secure(&reqwest::Url::parse(u).unwrap());
    assert!(ok("https://auth.example/x"));
    assert!(ok("http://127.0.0.1:9/x"));
    assert!(ok("http://localhost/x"));
    assert!(ok("http://[::1]/x"));
    assert!(!ok("http://auth.example/x"));
    assert!(!ok("http://127.0.0.1.nip.io/x"));
    assert!(!ok("ftp://127.0.0.1/x"));
    assert!(!ok("file:///etc/passwd"));
}

#[test]
fn mcp_oauth_clean() {
    assert_eq!(oauth::clean("", 10), "");
    assert_eq!(oauth::clean("a\u{1b}[31mb", 10), "a [31mb");
    assert_eq!(oauth::clean("ab\u{200b}\u{202e}cd", 10), "abcd");
    assert_eq!(oauth::clean("héllo wörld", 5), "héllo…");
    assert_eq!(oauth::clean("a\r\n\tb", 10), "a b");
    assert_eq!(oauth::clean("🦀🦀🦀", 2), "🦀🦀…");
}
