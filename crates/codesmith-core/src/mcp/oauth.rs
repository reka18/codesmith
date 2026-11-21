//! OAuth for remote MCP servers (MCP authorization spec): protected-resource
//! and authorization-server discovery, dynamic client registration, the
//! authorization-code flow with PKCE through the browser and a loopback
//! callback, and refresh. Tokens live in the data dir's `mcp-auth.json`
//! (owner-only), keyed by server name and bound to the server URL.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const DEFAULT_PORT: u16 = 19876;
pub const CALLBACK_PATH: &str = "/mcp/oauth/callback";

/// `oauth` on a remote server: `false` turns OAuth off; an object pre-sets the client.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Setting {
    Enabled(bool),
    Client(ClientConfig),
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct ClientConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    /// Loopback port for the browser callback (default 19876).
    pub port: Option<u16>,
}

impl Setting {
    pub fn enabled(s: Option<&Setting>) -> bool {
        !matches!(s, Some(Setting::Enabled(false)))
    }
    fn client(s: Option<&Setting>) -> ClientConfig {
        match s {
            Some(Setting::Client(c)) => c.clone(),
            _ => ClientConfig::default(),
        }
    }
}

/// A server answered 401; carries its `WWW-Authenticate` header.
#[derive(Debug)]
pub struct Unauthorized(pub Option<String>);

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server requires authorization")
    }
}
impl std::error::Error for Unauthorized {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub server_url: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub token_endpoint: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn store_path() -> std::path::PathBuf {
    crate::paths::data_dir().join("mcp-auth.json")
}

fn read_all(path: &Path) -> BTreeMap<String, Entry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_all(path: &Path, all: &BTreeMap<String, Entry>) -> Result<()> {
    crate::paths::write_private(path, serde_json::to_string_pretty(all)?.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// The saved entry for a server, if it was issued for this URL.
pub fn load(path: &Path, server: &str, url: &str) -> Option<Entry> {
    read_all(path)
        .remove(server)
        .filter(|e| e.server_url == url)
}

/// Save under the file lock (see [`crate::auth::FileLock`]); a file that
/// does not parse is kept (and copied to `.bak`) rather than replaced.
pub fn save(path: &Path, server: &str, e: &Entry) -> Result<()> {
    let _lock = crate::auth::FileLock::acquire(path)?;
    save_locked(path, server, e)
}

fn save_locked(path: &Path, server: &str, e: &Entry) -> Result<()> {
    let mut all: BTreeMap<String, Entry> = crate::auth::read_for_update(path)?;
    all.insert(server.to_string(), e.clone());
    write_all(path, &all)
}

pub fn remove(path: &Path, server: &str) -> Result<bool> {
    let _lock = crate::auth::FileLock::acquire(path)?;
    let mut all: BTreeMap<String, Entry> = crate::auth::read_for_update(path)?;
    let had = all.remove(server).is_some();
    if had {
        write_all(path, &all)?;
    }
    Ok(had)
}

/// `key="value"` parameters from a `WWW-Authenticate: Bearer ...` header.
pub fn auth_param(header: &str, key: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(
        r#"(?i)\b{}\s*=\s*(?:"([^"]*)"|([^\s,]+))"#,
        regex::escape(key)
    ))
    .ok()?;
    let c = re.captures(header)?;
    c.get(1).or(c.get(2)).map(|m| m.as_str().to_string())
}

/// RFC 8414 / RFC 9728 well-known URLs, path-aware first.
pub fn well_known(base: &Url, suffix: &str) -> Vec<String> {
    let origin = base.origin().ascii_serialization();
    let path = base.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{suffix}{path}"));
    }
    out.push(format!("{origin}/.well-known/{suffix}"));
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    pub authorization: String,
    pub token: String,
    pub registration: Option<String>,
    pub scopes: Vec<String>,
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    let r = client
        .get(url)
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", super::PROTOCOL_VERSION)
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json().await.ok()
}

/// Find the authorization server for an MCP server URL.
pub async fn discover(
    client: &reqwest::Client,
    server_url: &str,
    www_auth: Option<&str>,
) -> Result<Endpoints> {
    let server = Url::parse(server_url).context("invalid server URL")?;
    let mut prm_urls: Vec<String> = www_auth
        .and_then(|h| auth_param(h, "resource_metadata"))
        .into_iter()
        .collect();
    prm_urls.extend(well_known(&server, "oauth-protected-resource"));
    let mut scopes: Vec<String> = www_auth
        .and_then(|h| auth_param(h, "scope"))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let mut issuer: Option<Url> = None;
    for u in prm_urls {
        if let Some(v) = get_json(client, &u).await {
            if let Some(i) = v["authorization_servers"].get(0).and_then(Value::as_str) {
                issuer = Url::parse(i).ok();
                if scopes.is_empty() {
                    scopes = v["scopes_supported"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect();
                }
                break;
            }
        }
    }
    // Servers from before protected-resource metadata act as their own authorization server.
    let issuer =
        issuer.unwrap_or_else(|| Url::parse(&server.origin().ascii_serialization()).unwrap());
    let mut candidates = well_known(&issuer, "oauth-authorization-server");
    candidates.extend(well_known(&issuer, "openid-configuration"));
    candidates.push(format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    ));
    for u in candidates {
        if let Some(v) = get_json(client, &u).await {
            if let (Some(a), Some(t)) = (
                v["authorization_endpoint"].as_str(),
                v["token_endpoint"].as_str(),
            ) {
                if scopes.is_empty() {
                    scopes = v["scopes_supported"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect();
                }
                return Ok(Endpoints {
                    authorization: a.into(),
                    token: t.into(),
                    registration: v["registration_endpoint"].as_str().map(str::to_string),
                    scopes,
                });
            }
        }
    }
    let origin = issuer.origin().ascii_serialization();
    Ok(Endpoints {
        authorization: format!("{origin}/authorize"),
        token: format!("{origin}/token"),
        registration: Some(format!("{origin}/register")),
        scopes,
    })
}

/// PKCE verifier and its S256 challenge.
pub fn pkce() -> (String, String) {
    use rand::Rng;
    const CH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    let verifier: String = (0..64)
        .map(|_| CH[rng.gen_range(0..CH.len())] as char)
        .collect();
    (verifier.clone(), challenge(&verifier))
}

pub fn challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

async fn register(
    client: &reqwest::Client,
    endpoint: &str,
    redirect: &str,
) -> Result<(String, Option<String>)> {
    let body = serde_json::json!({
        "client_name": "CodeSmith",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let r = client
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .context("registering with the authorization server")?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "client registration failed (HTTP {}): {v}",
            status.as_u16()
        ));
    }
    let id = v["client_id"]
        .as_str()
        .ok_or_else(|| anyhow!("registration returned no client_id"))?;
    Ok((
        id.to_string(),
        v["client_secret"].as_str().map(str::to_string),
    ))
}

async fn token_request(
    client: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, &str)],
) -> Result<Value> {
    let body: String = form
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    let r = client
        .post(endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .context("contacting the token endpoint")?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or_default();
    if !status.is_success() || v.get("access_token").is_none() {
        return Err(anyhow!(
            "token request failed (HTTP {}): {}",
            status.as_u16(),
            v.get("error_description").or(v.get("error")).unwrap_or(&v)
        ));
    }
    Ok(v)
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn apply_tokens(e: &mut Entry, v: &Value) {
    e.access_token = v["access_token"].as_str().unwrap_or("").to_string();
    if let Some(r) = v["refresh_token"].as_str() {
        e.refresh_token = Some(r.to_string());
    }
    e.expires_at = v["expires_in"].as_u64().map(|s| now() + s);
}

/// A valid access token for the server: the saved one, refreshed if it is
/// about to expire. `None` when nothing usable is saved.
pub async fn token(
    client: &reqwest::Client,
    path: &Path,
    server: &str,
    url: &str,
) -> Option<String> {
    let e = load(path, server, url)?;
    if e.expires_at.is_none_or(|t| t > now() + 60) {
        return Some(e.access_token);
    }
    // Refresh under the file lock, and look again once it is ours: another
    // process may have refreshed meanwhile, and with rotating refresh tokens
    // the old one no longer works.
    let lock_path = path.to_path_buf();
    let _lock = tokio::task::spawn_blocking(move || crate::auth::FileLock::acquire(&lock_path))
        .await
        .ok()?
        .ok()?;
    let mut e = load(path, server, url)?;
    if e.expires_at.is_some_and(|t| t <= now() + 60) {
        let rt = e.refresh_token.clone()?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", rt.as_str()),
            ("client_id", e.client_id.as_str()),
            ("resource", url),
        ];
        if let Some(s) = e.client_secret.as_deref() {
            form.push(("client_secret", s));
        }
        let v = token_request(client, &e.token_endpoint, &form).await.ok()?;
        apply_tokens(&mut e, &v);
        let _ = save_locked(path, server, &e);
    }
    Some(e.access_token)
}

/// Wait for one browser callback on the listener; returns its query parameters.
async fn callback(listener: tokio::net::TcpListener) -> Result<BTreeMap<String, String>> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let target = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        let Ok(u) = Url::parse(&format!("http://localhost{target}")) else {
            continue;
        };
        if u.path() != CALLBACK_PATH {
            let _ = sock
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let q: BTreeMap<String, String> = u
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let msg = if q.contains_key("code") {
            "CodeSmith is authorized. You can close this tab."
        } else {
            "Authorization failed. You can close this tab and check the terminal."
        };
        let html = format!("<!doctype html><meta charset=utf-8><title>CodeSmith</title><p style=\"font-family:system-ui;margin:3em\">{msg}</p>");
        let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len()).as_bytes()).await;
        return Ok(q);
    }
}

/// Run the browser login for a server and save its tokens. `open` receives
/// the authorization URL (it opens a browser, or a test follows it).
pub async fn login(
    client: &reqwest::Client,
    path: &Path,
    server: &str,
    url: &str,
    setting: Option<&Setting>,
    open: impl FnOnce(&str),
) -> Result<Entry> {
    let cfg = Setting::client(setting);
    let www = match client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}))
        .send()
        .await
    {
        Ok(r) if r.status() == 401 => r
            .headers()
            .get("www-authenticate")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string),
        _ => None,
    };
    let ep = discover(client, url, www.as_deref()).await?;
    let port = cfg.port.unwrap_or(DEFAULT_PORT);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("listening for the browser callback on 127.0.0.1:{port}"))?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let (client_id, client_secret) = match (cfg.client_id.clone(), load(path, server, url)) {
        (Some(id), _) => (id, cfg.client_secret.clone()),
        (None, Some(prev)) if !prev.client_id.is_empty() && prev.token_endpoint == ep.token => {
            (prev.client_id, prev.client_secret)
        }
        (None, _) => {
            let reg = ep.registration.as_deref().ok_or_else(|| anyhow!("the server offers no client registration; set mcp.servers.{server}.oauth.clientId"))?;
            register(client, reg, &redirect).await?
        }
    };
    let (verifier, chal) = pkce();
    let state: String = pkce().0[..32].to_string();
    let scope = cfg.scope.clone().unwrap_or_else(|| ep.scopes.join(" "));
    let mut auth = Url::parse(&ep.authorization).context("invalid authorization endpoint")?;
    check_browser_url(&auth)?;
    {
        let mut q = auth.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &chal)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", url);
        if !scope.is_empty() {
            q.append_pair("scope", &scope);
        }
    }
    let wait = tokio::spawn(callback(listener));
    open(auth.as_str());
    let q = tokio::time::timeout(Duration::from_secs(300), wait)
        .await
        .map_err(|_| anyhow!("timed out waiting for the browser (5 minutes)"))???;
    if q.get("state") != Some(&state) {
        return Err(anyhow!("the callback's state did not match; try again"));
    }
    let code = match (q.get("code"), q.get("error")) {
        (Some(c), _) => c.clone(),
        (None, Some(e)) => {
            return Err(anyhow!(
                "authorization denied: {e} {}",
                q.get("error_description").cloned().unwrap_or_default()
            ))
        }
        _ => return Err(anyhow!("the callback carried no code")),
    };
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.as_str()),
        ("resource", url),
    ];
    if let Some(s) = client_secret.as_deref() {
        form.push(("client_secret", s));
    }
    let v = token_request(client, &ep.token, &form).await?;
    let mut e = Entry {
        server_url: url.to_string(),
        client_id,
        client_secret,
        token_endpoint: ep.token,
        ..Default::default()
    };
    apply_tokens(&mut e, &v);
    save(path, server, &e)?;
    Ok(e)
}

/// Only web URLs may be handed to the browser: `https`, or plain `http` to a
/// loopback host. The authorization endpoint comes from server metadata, and
/// `open`/`xdg-open` would happily launch `file:` or custom-scheme URLs.
pub fn check_browser_url(u: &Url) -> Result<()> {
    let host = u.host_str().unwrap_or("");
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match u.scheme() {
        "https" if !host.is_empty() => Ok(()),
        "http" if loopback => Ok(()),
        s => Err(anyhow!(
            "refusing to open authorization URL {u}: only https (or http to localhost) is allowed, not {s}"
        )),
    }
}

/// Open a URL in the default browser (best effort). URLs that fail
/// [`check_browser_url`] are not opened.
pub fn open_browser(url: &str) {
    if Url::parse(url)
        .map_err(anyhow::Error::from)
        .and_then(|u| check_browser_url(&u))
        .is_err()
    {
        return;
    }
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .env_remove(crate::SERVER_TOKEN_VAR)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_urls_are_opened() {
        let ok = |u: &str| check_browser_url(&Url::parse(u).unwrap()).is_ok();
        assert!(ok("https://auth.example/authorize?x=1"));
        assert!(ok("http://127.0.0.1:9000/authorize"));
        assert!(ok("http://localhost/authorize"));
        assert!(ok("http://[::1]:8080/a"));
        assert!(
            !ok("http://auth.example/authorize"),
            "plain http off loopback"
        );
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("javascript:alert(1)"));
        assert!(!ok("x-custom-app://run?cmd=1"));
        assert!(!ok("smb://share/x"));
    }

    #[test]
    fn settings() {
        let off: Setting = serde_json::from_value(serde_json::json!(false)).unwrap();
        assert!(!Setting::enabled(Some(&off)));
        assert!(Setting::enabled(None));
        let c: Setting =
            serde_json::from_value(serde_json::json!({"clientId": "abc", "scope": "read"}))
                .unwrap();
        assert_eq!(Setting::client(Some(&c)).client_id.as_deref(), Some("abc"));
    }

    #[test]
    fn www_authenticate_params() {
        let h = r#"Bearer error="invalid_token", resource_metadata="https://mcp.example/.well-known/oauth-protected-resource", scope=files:read"#;
        assert_eq!(
            auth_param(h, "resource_metadata").as_deref(),
            Some("https://mcp.example/.well-known/oauth-protected-resource")
        );
        assert_eq!(auth_param(h, "scope").as_deref(), Some("files:read"));
        assert_eq!(auth_param(h, "realm"), None);
    }

    #[test]
    fn well_known_urls() {
        let u = Url::parse("https://a.example/tenant/mcp").unwrap();
        assert_eq!(
            well_known(&u, "oauth-protected-resource"),
            [
                "https://a.example/.well-known/oauth-protected-resource/tenant/mcp",
                "https://a.example/.well-known/oauth-protected-resource"
            ]
        );
        assert_eq!(
            well_known(&Url::parse("https://a.example").unwrap(), "x"),
            ["https://a.example/.well-known/x"]
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc7636() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let (v, c) = pkce();
        assert_eq!(v.len(), 64);
        assert_eq!(challenge(&v), c);
    }

    #[test]
    fn store_is_bound_to_the_url() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("mcp-auth.json");
        let e = Entry {
            server_url: "https://a/mcp".into(),
            client_id: "c".into(),
            access_token: "t".into(),
            ..Default::default()
        };
        save(&p, "a", &e).unwrap();
        assert_eq!(load(&p, "a", "https://a/mcp"), Some(e));
        assert_eq!(
            load(&p, "a", "https://other/mcp"),
            None,
            "a token is never sent to a different URL"
        );
        assert!(remove(&p, "a").unwrap());
        assert_eq!(load(&p, "a", "https://a/mcp"), None);
    }

    #[test]
    fn a_corrupt_store_is_kept_and_concurrent_saves_are_kept() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("mcp-auth.json");
        let entry = |i: usize| Entry {
            server_url: format!("https://s{i}/mcp"),
            access_token: "t".into(),
            ..Default::default()
        };
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || save(&p, &format!("s{i}"), &entry(i)).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        #[cfg(unix)]
        assert_eq!(read_all(&p).len(), 8);
        std::fs::write(&p, "{not json").unwrap();
        let e = save(&p, "x", &entry(9)).unwrap_err().to_string();
        assert!(e.contains("mcp-auth.json.bak"), "{e}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{not json");
        assert!(remove(&p, "s1").is_err());
    }

    #[test]
    fn form_encoding() {
        assert_eq!(enc("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }
}
