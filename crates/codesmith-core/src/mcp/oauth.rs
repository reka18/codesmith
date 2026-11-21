//! OAuth for remote MCP servers (MCP authorization spec): protected-resource
//! and authorization-server discovery, dynamic client registration, the
//! authorization-code flow with PKCE through the browser and a loopback
//! callback (or a redirect pasted into the terminal), and refresh.
//! Discovery, documents and tokens go only to https URLs (or http to
//! localhost) and follow no redirects; every response read is capped.
//! Tokens live in the data dir's `mcp-auth.json`
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
    /// Loopback port for the browser callback (default 19876, or the port
    /// of `redirectUri`).
    pub port: Option<u16>,
    /// The redirect URL to register and send instead of the loopback
    /// callback, for an OAuth app registered with a fixed one. The login
    /// listens on its path (and loopback port); a redirect the browser on
    /// another machine reaches can be pasted into the terminal.
    pub redirect_uri: Option<String>,
    /// An authorization server metadata document (RFC 8414) to use instead
    /// of discovery, for a server that publishes no protected-resource
    /// metadata.
    pub auth_server_metadata_url: Option<String>,
    /// A client ID metadata document you host (an `https` URL), used as the
    /// client ID with an authorization server that accepts such documents
    /// from public clients, instead of registering a client.
    pub client_metadata_url: Option<String>,
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

/// A server answered 401 (or 403 asking for more scope); carries its
/// `WWW-Authenticate` header.
#[derive(Debug)]
pub struct Unauthorized(pub Option<String>);

impl Unauthorized {
    /// The server wants a token with more scope (RFC 6750 `insufficient_scope`):
    /// refreshing the one it has will not help, only a new login.
    pub fn insufficient_scope(&self) -> bool {
        self.0
            .as_deref()
            .and_then(|h| auth_param(h, "error"))
            .is_some_and(|e| e == "insufficient_scope")
    }
}

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.insufficient_scope() {
            write!(f, "the server needs a login with more scope")
        } else {
            write!(f, "the server requires authorization")
        }
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
    remove_locked(path, server)
}

fn remove_locked(path: &Path, server: &str) -> Result<bool> {
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Endpoints {
    pub authorization: String,
    pub token: String,
    pub registration: Option<String>,
    pub scopes: Vec<String>,
    /// The authorization server's issuer identifier: its metadata's
    /// `issuer`, or the URL it was discovered at.
    pub issuer: String,
    /// The server says it puts `iss` in its redirects (RFC 9207), so one
    /// without it is refused.
    pub iss_required: bool,
    /// The server accepts client ID metadata documents from public clients.
    pub cimd: bool,
    /// The server offers `offline_access` (refresh tokens on request).
    pub offline_access: bool,
}

/// The largest metadata document, registration or token response read.
pub const MAX_DOCUMENT: usize = 1 << 20;

/// How long one request to an authorization server may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The client OAuth requests go out on: it follows no redirects, so a
/// metadata document comes only from the URL it was asked at, and a token
/// request (code, verifier, secret) is never re-posted elsewhere.
fn oauth_http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Whether `host` is this machine.
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `https`, or plain `http` to a loopback host: the only URLs OAuth
/// metadata, documents and tokens are fetched from or sent to.
pub fn secure(u: &Url) -> bool {
    let host = u.host_str().unwrap_or("");
    match u.scheme() {
        "https" => !host.is_empty(),
        "http" => is_loopback(host),
        _ => false,
    }
}

fn secure_url(kind: &str, s: &str) -> Result<Url> {
    let u = Url::parse(s).map_err(|e| anyhow!("invalid {kind} {}: {e}", clean(s, 200)))?;
    if !secure(&u) {
        return Err(anyhow!(
            "refusing the {kind} {}: OAuth needs https (or http to localhost)",
            clean(s, 200)
        ));
    }
    Ok(u)
}

/// `s` fit to show in the terminal: control characters (escape sequences
/// included) and invisible formatting characters dropped, cut to `max`
/// characters. For anything a server sent.
pub fn clean(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    for c in s.chars() {
        let invisible =
            matches!(c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xFEFF);
        if invisible {
            continue;
        }
        if n == max {
            out.push('…');
            break;
        }
        if c.is_control() {
            if !out.ends_with(' ') {
                out.push(' ');
                n += 1;
            }
            continue;
        }
        out.push(c);
        n += 1;
    }
    out
}

/// A response body of at most `max` bytes.
async fn read_capped(mut r: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if r.content_length().is_some_and(|n| n > max as u64) {
        return Err(anyhow!("the response is larger than {} KiB", max >> 10));
    }
    let mut out = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        if out.len() + chunk.len() > max {
            return Err(anyhow!("the response is larger than {} KiB", max >> 10));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// A JSON object from `url` (https or loopback, no redirects, capped).
async fn get_json(url: &str) -> Option<Value> {
    let u = Url::parse(url).ok().filter(secure)?;
    let r = oauth_http()
        .get(u)
        .header("Accept", "application/json")
        .header("MCP-Protocol-Version", super::PROTOCOL_VERSION)
        .send()
        .await
        .ok()?;
    if !r.status().is_success() {
        return None;
    }
    let body = read_capped(r, MAX_DOCUMENT).await.ok()?;
    serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(Value::is_object)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

/// An issuer identifier compared as RFC 8414 asks, ignoring a trailing `/`.
fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The endpoints in authorization server metadata `v`. When `expected` (the
/// URL it was discovered from) is given, the document's `issuer` must be it
/// (RFC 8414 3.3), so one server's metadata cannot stand in for another's.
fn from_metadata(v: &Value, expected: Option<&str>, scopes: Vec<String>) -> Result<Endpoints> {
    let (Some(a), Some(t)) = (
        v["authorization_endpoint"].as_str(),
        v["token_endpoint"].as_str(),
    ) else {
        return Err(anyhow!(
            "the authorization server metadata names no authorization and token endpoints"
        ));
    };
    let issuer = match (v["issuer"].as_str(), expected) {
        (Some(i), Some(e)) if !same_issuer(i, e) => {
            return Err(anyhow!(
                "the authorization server metadata at {} names another issuer ({})",
                clean(e, 200),
                clean(i, 200)
            ))
        }
        (Some(i), _) => i.to_string(),
        (None, Some(e)) => e.to_string(),
        (None, None) => Url::parse(t)
            .map(|u| u.origin().ascii_serialization())
            .unwrap_or_default(),
    };
    secure_url("token endpoint", t)?;
    let registration = v["registration_endpoint"]
        .as_str()
        .map(|r| secure_url("registration endpoint", r).map(|_| r.to_string()))
        .transpose()?;
    let supported = strings(&v["scopes_supported"]);
    Ok(Endpoints {
        authorization: a.into(),
        token: t.into(),
        registration,
        offline_access: supported.iter().any(|s| s == "offline_access"),
        scopes: if scopes.is_empty() { supported } else { scopes },
        issuer,
        iss_required: v["authorization_response_iss_parameter_supported"] == true,
        cimd: v["client_id_metadata_document_supported"] == true
            && strings(&v["token_endpoint_auth_methods_supported"])
                .iter()
                .any(|m| m == "none"),
    })
}

/// Find the authorization server for an MCP server URL. Every URL fetched
/// is https (or http to localhost), and none redirects.
pub async fn discover(
    _client: &reqwest::Client,
    server_url: &str,
    www_auth: Option<&str>,
) -> Result<Endpoints> {
    let server = secure_url("MCP server URL", server_url)?;
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
        if let Some(v) = get_json(&u).await {
            if let Some(i) = v["authorization_servers"].get(0).and_then(Value::as_str) {
                issuer = Some(secure_url("authorization server", i)?);
                if scopes.is_empty() {
                    scopes = strings(&v["scopes_supported"]);
                }
                break;
            }
        }
    }
    // Servers from before protected-resource metadata act as their own authorization server.
    let issuer =
        issuer.unwrap_or_else(|| Url::parse(&server.origin().ascii_serialization()).unwrap());
    let expected = issuer.as_str().to_string();
    let mut candidates = well_known(&issuer, "oauth-authorization-server");
    candidates.extend(well_known(&issuer, "openid-configuration"));
    candidates.push(format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    ));
    for u in candidates {
        if let Some(v) = get_json(&u).await {
            if v["authorization_endpoint"].is_string() && v["token_endpoint"].is_string() {
                return from_metadata(&v, Some(&expected), scopes);
            }
        }
    }
    let origin = issuer.origin().ascii_serialization();
    Ok(Endpoints {
        authorization: format!("{origin}/authorize"),
        token: format!("{origin}/token"),
        registration: Some(format!("{origin}/register")),
        scopes,
        issuer: expected,
        ..Default::default()
    })
}

/// The endpoints from a configured metadata document (`authServerMetadataUrl`),
/// which must answer.
pub async fn configured_metadata(url: &str, scopes: Vec<String>) -> Result<Endpoints> {
    secure_url("authorization server metadata URL", url)?;
    let v = get_json(url).await.ok_or_else(|| {
        anyhow!(
            "could not load the authorization server metadata at {}",
            clean(url, 200)
        )
    })?;
    from_metadata(&v, None, scopes)
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

/// An OAuth error response's description for the terminal: its
/// `error_description` or `error`, else the body, cleaned and cut short.
fn describe(v: &Value, raw: &[u8]) -> String {
    let text = v["error_description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or(v["error"].as_str().filter(|s| !s.is_empty()))
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(raw).into_owned());
    clean(&text, 300)
}

async fn register(
    endpoint: &str,
    redirects: &[String],
    scope: Option<&str>,
) -> Result<(String, Option<String>)> {
    let mut body = serde_json::json!({
        "client_name": "CodeSmith",
        "redirect_uris": redirects,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if let Some(s) = scope.filter(|s| !s.is_empty()) {
        body["scope"] = s.into();
    }
    let r = oauth_http()
        .post(secure_url("registration endpoint", endpoint)?)
        .json(&body)
        .send()
        .await
        .context("registering with the authorization server")?;
    let status = r.status();
    let raw = read_capped(r, MAX_DOCUMENT).await?;
    let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "client registration failed (HTTP {}): {}",
            status.as_u16(),
            describe(&v, &raw)
        ));
    }
    let id = v["client_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("registration returned no client_id"))?;
    Ok((
        id.to_string(),
        v["client_secret"].as_str().map(str::to_string),
    ))
}

/// A token endpoint refused the grant (`invalid_grant`): the refresh token
/// is spent or revoked, so the login is over.
#[derive(Debug)]
struct InvalidGrant;

impl std::fmt::Display for InvalidGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the authorization server refused the grant (invalid_grant)")
    }
}

impl std::error::Error for InvalidGrant {}

async fn token_request(endpoint: &str, form: &[(&str, &str)]) -> Result<Value> {
    let body: String = form
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    let r = oauth_http()
        .post(secure_url("token endpoint", endpoint)?)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .context("contacting the token endpoint")?;
    let status = r.status();
    let raw = read_capped(r, MAX_DOCUMENT).await?;
    let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
    if status.is_client_error() && v["error"] == "invalid_grant" {
        return Err(InvalidGrant.into());
    }
    if !status.is_success() || !v["access_token"].as_str().is_some_and(|t| !t.is_empty()) {
        return Err(anyhow!(
            "token request failed (HTTP {}): {}",
            status.as_u16(),
            describe(&v, &raw)
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
/// about to expire. `None` when nothing usable is saved. A refresh that
/// fails leaves the saved one to be sent (the server may still take it),
/// unless the authorization server refused the grant: the login is then
/// removed, so the server is reported as needing one.
pub async fn token(
    client: &reqwest::Client,
    path: &Path,
    server: &str,
    url: &str,
) -> Option<String> {
    token_within(client, path, server, url, REFRESH_TIMEOUT).await
}

/// The longest a refresh may take before the saved token is sent instead.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// How a refresh ended.
enum Refreshed {
    /// New tokens, saved.
    Saved,
    /// The grant was refused; the login is removed.
    SignedOut,
    /// It failed (no refresh token, a server error, a timeout).
    Failed,
}

/// Refresh `e` at the token endpoint that issued it, with the store's lock
/// held: save the new tokens, or remove a login whose grant was refused.
async fn refresh_locked(
    path: &Path,
    server: &str,
    url: &str,
    e: &mut Entry,
    limit: Duration,
) -> Refreshed {
    let Some(rt) = e.refresh_token.clone() else {
        return Refreshed::Failed;
    };
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", rt.as_str()),
        ("client_id", e.client_id.as_str()),
        ("resource", url),
    ];
    if let Some(s) = e.client_secret.as_deref() {
        form.push(("client_secret", s));
    }
    let refreshed = tokio::time::timeout(limit, token_request(&e.token_endpoint, &form))
        .await
        .unwrap_or_else(|_| Err(anyhow!("the token endpoint did not answer in time")));
    match refreshed {
        Ok(v) => {
            apply_tokens(e, &v);
            let _ = save_locked(path, server, e);
            Refreshed::Saved
        }
        Err(err) if err.downcast_ref::<InvalidGrant>().is_some() => {
            let _ = remove_locked(path, server);
            Refreshed::SignedOut
        }
        Err(_) => Refreshed::Failed,
    }
}

async fn lock(path: &Path) -> Option<crate::auth::FileLock> {
    let lock_path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::auth::FileLock::acquire(&lock_path))
        .await
        .ok()?
        .ok()
}

/// [`token`], giving a refresh at most `limit`: a token endpoint that does
/// not answer leaves the saved token to be sent, as one that fails does.
pub async fn token_within(
    _client: &reqwest::Client,
    path: &Path,
    server: &str,
    url: &str,
    limit: Duration,
) -> Option<String> {
    let e = load(path, server, url)?;
    if e.expires_at.is_none_or(|t| t > now() + 60) {
        return Some(e.access_token);
    }
    // Refresh under the file lock, and look again once it is ours: another
    // process may have refreshed meanwhile, and with rotating refresh tokens
    // the old one no longer works.
    let _lock = lock(path).await?;
    let mut e = load(path, server, url)?;
    if e.expires_at.is_some_and(|t| t <= now() + 60) {
        // Nothing to refresh with: the server decides on the saved one.
        if let Refreshed::SignedOut = refresh_locked(path, server, url, &mut e, limit).await {
            return None;
        }
    }
    Some(e.access_token)
}

/// The server refused `sent`, a token with time left: refresh it once and
/// return the new token to retry with. Another process that refreshed
/// meanwhile already saved a newer token, which is returned unspent. `None`
/// when there is nothing better to send (no refresh token, the refresh
/// failed, or the grant was refused and the login removed).
pub async fn refresh_refused(
    path: &Path,
    server: &str,
    url: &str,
    sent: &str,
    limit: Duration,
) -> Option<String> {
    let _lock = lock(path).await?;
    let mut e = load(path, server, url)?;
    if e.access_token != sent {
        return Some(e.access_token);
    }
    match refresh_locked(path, server, url, &mut e, limit).await {
        Refreshed::Saved if e.access_token != sent => Some(e.access_token),
        _ => None,
    }
}

/// How long a login waits for the browser.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a callback connection may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The method, target and `Host` of a request on the callback listener.
async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<(String, String, String)> {
    let mut buf = vec![0u8; 8192];
    let mut have = 0;
    loop {
        let n = sock.read(&mut buf[have..]).await.ok()?;
        if n == 0 {
            break;
        }
        have += n;
        if buf[..have].windows(4).any(|w| w == b"\r\n\r\n") || have == buf.len() {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf[..have]).to_string();
    let mut lines = text.lines();
    let mut parts = lines.next()?.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let host = lines
        .take_while(|l| !l.is_empty())
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("host")
                .then(|| v.trim().to_string())
        })
        .unwrap_or_default();
    Some((method, target, host))
}

async fn reply(mut sock: tokio::net::TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let _ = sock
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
    let _ = sock.shutdown().await;
}

fn page(server: &str, error: Option<&str>) -> String {
    crate::providers::oauth::page(&format!("MCP server {server}"), error)
}

/// The redirect URLs a login accepts, and the one it was sent with.
struct Redirects {
    /// Registered: the configured redirect, or the loopback callback as
    /// `127.0.0.1` and as `localhost` (some servers rewrite one to the other).
    registered: Vec<Url>,
    path: String,
}

impl Redirects {
    /// The registered redirect `u` is (scheme, host, port and path), if any.
    fn matching(&self, u: &Url) -> Option<&Url> {
        self.registered.iter().find(|r| {
            r.scheme() == u.scheme()
                && r.host_str().map(str::to_ascii_lowercase)
                    == u.host_str().map(str::to_ascii_lowercase)
                && r.port_or_known_default() == u.port_or_known_default()
                && r.path() == u.path()
        })
    }

    /// The registered redirect a browser request reached, by its `Host`.
    fn reached(&self, host: &str) -> &Url {
        let wanted = |r: &&Url| {
            let h = match r.port() {
                Some(p) => format!("{}:{p}", r.host_str().unwrap_or("")),
                None => r.host_str().unwrap_or("").to_string(),
            };
            h.eq_ignore_ascii_case(host)
        };
        self.registered
            .iter()
            .find(wanted)
            .unwrap_or(&self.registered[0])
    }
}

/// What a pasted line was: a redirect to redeem, one that ends the login,
/// or something to say why it was not taken (the login keeps waiting).
enum Pasted {
    Code(String, BTreeMap<String, String>, Url),
    Failed(String),
    Ignored(&'static str),
}

/// Read a pasted redirect: it must be a full URL to one of the login's
/// redirects with the login's state, as the browser's redirect must.
fn pasted(text: &str, redirects: &Redirects, state: &str) -> Pasted {
    let Ok(u) = Url::parse(text.trim()) else {
        return Pasted::Ignored("Paste the full URL from the browser's address bar");
    };
    let Some(redirect) = redirects.matching(&u).cloned() else {
        return Pasted::Ignored("This URL is not this sign-in's redirect address");
    };
    let q = query(&u);
    if q.get("state").map(String::as_str) != Some(state) {
        return Pasted::Ignored("This URL belongs to a different sign-in");
    }
    if let Some(e) = callback_error(&q) {
        return Pasted::Failed(e);
    }
    match q.get("code").filter(|c| !c.is_empty()).cloned() {
        Some(c) => Pasted::Code(c, q, redirect),
        None => Pasted::Ignored("The URL does not contain an authorization code"),
    }
}

fn query(u: &Url) -> BTreeMap<String, String> {
    u.query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn callback_error(q: &BTreeMap<String, String>) -> Option<String> {
    let e = q.get("error")?;
    let d = q.get("error_description").cloned().unwrap_or_default();
    let text = format!("{e} {d}");
    Some(format!("authorization denied: {}", clean(text.trim(), 300)))
}

/// RFC 9207: a redirect's `iss` must name the issuer the request went to,
/// and a server that says it sends one must have.
fn check_iss(q: &BTreeMap<String, String>, ep: &Endpoints) -> Result<(), String> {
    match q.get("iss") {
        Some(i) if !same_issuer(i, &ep.issuer) => Err(format!(
            "the redirect came from another issuer ({}), not {}",
            clean(i, 200),
            clean(&ep.issuer, 200)
        )),
        None if ep.iss_required => {
            Err("the redirect did not name its issuer (iss), which this server sends".into())
        }
        _ => Ok(()),
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
    login_with(client, path, server, url, setting, open, None, |_| {}).await
}

/// [`login`], also taking redirect URLs pasted into the terminal (the
/// browser on another machine). A paste that is not this login's redirect,
/// or carries another state, is refused through `notice` and the login
/// keeps waiting. The browser's page reports how the token exchange went.
#[allow(clippy::too_many_arguments)]
pub async fn login_with(
    client: &reqwest::Client,
    path: &Path,
    server: &str,
    url: &str,
    setting: Option<&Setting>,
    open: impl FnOnce(&str),
    mut pastes: Option<tokio::sync::mpsc::Receiver<String>>,
    notice: impl Fn(&str),
) -> Result<Entry> {
    let cfg = Setting::client(setting);
    let configured_redirect = cfg
        .redirect_uri
        .as_deref()
        .map(|r| {
            Url::parse(r)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
                .ok_or_else(|| {
                    anyhow!(
                        "invalid redirect URL {} for {server}: it must be an http or https URL",
                        clean(r, 200)
                    )
                })
        })
        .transpose()?;
    if let Some(m) = &cfg.client_metadata_url {
        let u = Url::parse(m)
            .ok()
            .filter(|u| u.scheme() == "https" && u.path() != "/");
        if u.is_none() {
            return Err(anyhow!(
                "invalid clientMetadataUrl {} for {server}: it must be an https URL with a path",
                clean(m, 200)
            ));
        }
    }
    secure_url("MCP server URL", url)?;
    // The server's 401 (or 403 for more scope) names where its resource
    // metadata lives and which scopes it wants. The saved token is sent, if
    // any, so a server wanting more scope says which; like every OAuth
    // request, the probe follows no redirect.
    let saved = load(path, server, url);
    let mut probe = oauth_http()
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(10))
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}));
    if let Some(e) = &saved {
        probe = probe.bearer_auth(&e.access_token);
    }
    let www = match probe.send().await {
        Ok(r) if r.status() == 401 || r.status() == 403 => r
            .headers()
            .get("www-authenticate")
            .and_then(|h| h.to_str().ok())
            .map(str::to_string),
        _ => None,
    };
    let challenge_scope = www.as_deref().and_then(|h| auth_param(h, "scope"));
    let ep = match &cfg.auth_server_metadata_url {
        Some(m) => configured_metadata(m, vec![]).await?,
        None => discover(client, url, www.as_deref()).await?,
    };
    // The listener: the configured port, else a loopback redirect's own,
    // else the default. Both loopback addresses are taken, so no one else
    // can listen where `localhost` may resolve.
    let redirect_port = configured_redirect
        .as_ref()
        .filter(|u| u.host_str().is_some_and(is_loopback))
        .and_then(Url::port);
    let port = cfg.port.or(redirect_port).unwrap_or(DEFAULT_PORT);
    let listeners = crate::providers::oauth::bind_localhost(port)
        .await
        .with_context(|| format!("listening for the browser callback on 127.0.0.1:{port}"))?;
    let port = listeners[0].local_addr()?.port();
    let redirects = match &configured_redirect {
        Some(r) => Redirects {
            registered: vec![r.clone()],
            path: r.path().to_string(),
        },
        None => Redirects {
            registered: ["127.0.0.1", "localhost"]
                .iter()
                .map(|h| Url::parse(&format!("http://{h}:{port}{CALLBACK_PATH}")).unwrap())
                .collect(),
            path: CALLBACK_PATH.into(),
        },
    };
    let registered: Vec<String> = redirects
        .registered
        .iter()
        .map(|u| u.as_str().to_string())
        .collect();
    let scope = {
        let mut s: Vec<String> = cfg
            .scope
            .clone()
            .or(challenge_scope)
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| ep.scopes.clone());
        // Ask for refresh tokens where the server offers them, without
        // forcing a consent prompt.
        if ep.offline_access && !s.iter().any(|x| x == "offline_access") {
            s.push("offline_access".into());
        }
        s.join(" ")
    };
    // The client: a configured one, the one an earlier login registered
    // with this authorization server, a client metadata document of yours
    // the server accepts (not with a configured redirect, which it would
    // not list), or one registered now.
    let cimd = cfg
        .client_metadata_url
        .clone()
        .filter(|_| ep.cimd && configured_redirect.is_none());
    let previous = saved.filter(|p| {
        !p.client_id.is_empty()
            && p.token_endpoint == ep.token
            && (!p.client_id.contains("://") || Some(&p.client_id) == cimd.as_ref())
    });
    let (client_id, client_secret) = match (cfg.client_id.clone(), previous, cimd) {
        (Some(id), _, _) => (id, cfg.client_secret.clone()),
        (None, Some(prev), _) if configured_redirect.is_none() => {
            (prev.client_id, prev.client_secret)
        }
        (None, _, Some(doc)) => (doc, None),
        (None, _, None) => {
            let reg = ep.registration.as_deref().ok_or_else(|| anyhow!("the server offers no client registration; set mcp.servers.{server}.oauth.clientId"))?;
            register(reg, &registered, cfg.scope.as_deref()).await?
        }
    };
    let (verifier, chal) = pkce();
    let state: String = pkce().0[..43].to_string();
    let mut auth = Url::parse(&ep.authorization).context("invalid authorization endpoint")?;
    check_browser_url(&auth)?;
    {
        let mut q = auth.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &registered[0])
            .append_pair("code_challenge", &chal)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", url);
        if !scope.is_empty() {
            q.append_pair("scope", &scope);
        }
    }
    open(auth.as_str());
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    let (tx, mut rx) =
        tokio::sync::mpsc::channel::<(tokio::net::TcpStream, String, String, Url)>(8);
    let mut readers = tokio::task::JoinSet::new();
    let got = loop {
        let paste = async {
            match pastes.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                break Err(anyhow!("timed out waiting for the browser ({} minutes)", LOGIN_TIMEOUT.as_secs() / 60));
            }
            text = paste => {
                let Some(text) = text else { pastes = None; continue };
                match pasted(&text, &redirects, &state) {
                    Pasted::Ignored(why) => notice(why),
                    Pasted::Failed(e) => break Err(anyhow!(e)),
                    Pasted::Code(code, q, redirect) => match check_iss(&q, &ep) {
                        Err(e) => break Err(anyhow!(e)),
                        Ok(()) => break Ok((code, redirect, None)),
                    },
                }
            }
            Some((sock, method, host, u)) = rx.recv() => {
                let q = query(&u);
                // A request without the redirect's parameters (a prefetch, a
                // probe, a reload of the bare page) is not the redirect: keep
                // waiting.
                if method != "GET" || (!q.contains_key("code") && !q.contains_key("error") && !q.contains_key("state")) {
                    let body = page(server, Some("Missing authorization code"));
                    readers.spawn(async move { reply(sock, 400, &body).await });
                    continue;
                }
                // Another state (a stale tab, or a page that found the
                // port) neither completes nor cancels the login: refuse it
                // and keep waiting for the redirect.
                if q.get("state") != Some(&state) {
                    let body = page(server, Some("the callback's state did not match"));
                    readers.spawn(async move { reply(sock, 400, &body).await });
                    continue;
                }
                let failed = if let Some(e) = callback_error(&q) {
                    Some(e)
                } else if let Err(e) = check_iss(&q, &ep) {
                    Some(e)
                } else if !q.get("code").is_some_and(|c| !c.is_empty()) {
                    Some("the callback carried no code".to_string())
                } else {
                    None
                };
                if let Some(e) = failed {
                    reply(sock, 400, &page(server, Some(&e))).await;
                    break Err(anyhow!(e));
                }
                let redirect = redirects.reached(&host).clone();
                break Ok((q["code"].clone(), redirect, Some(sock)));
            }
            acc = crate::providers::oauth::accept_any(&listeners) => {
                let Ok(mut sock) = acc else { continue };
                let (tx, path) = (tx.clone(), redirects.path.clone());
                readers.spawn(async move {
                    let Ok(Some((method, target, host))) =
                        tokio::time::timeout(READ_TIMEOUT, read_request(&mut sock)).await
                    else {
                        return;
                    };
                    match Url::parse(&format!("http://127.0.0.1{target}")) {
                        Ok(u) if u.path() == path => {
                            let _ = tx.send((sock, method, host, u)).await;
                        }
                        _ => reply(sock, 404, "").await,
                    }
                });
            }
        }
    };
    readers.abort_all();
    drop(listeners);
    let (code, redirect, browser) = got?;
    let redirect = redirect.as_str().to_string();
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
    // The browser's page waits for the exchange, so it never reports a
    // login the server then refused.
    let exchanged = token_request(&ep.token, &form).await.and_then(|v| {
        let mut e = Entry {
            server_url: url.to_string(),
            client_id: client_id.clone(),
            client_secret: client_secret.clone(),
            token_endpoint: ep.token.clone(),
            ..Default::default()
        };
        apply_tokens(&mut e, &v);
        save(path, server, &e)?;
        Ok(e)
    });
    if let Some(sock) = browser {
        match &exchanged {
            Ok(_) => reply(sock, 200, &page(server, None)).await,
            Err(e) => reply(sock, 400, &page(server, Some(&format!("{e:#}")))).await,
        }
    }
    exchanged
}

/// Only web URLs may be handed to the browser: `https`, or plain `http` to a
/// loopback host. The authorization endpoint comes from server metadata, and
/// `open`/`xdg-open` would happily launch `file:` or custom-scheme URLs.
pub fn check_browser_url(u: &Url) -> Result<()> {
    if secure(u) {
        return Ok(());
    }
    Err(anyhow!(
        "refusing to open authorization URL {}: only https (or http to localhost) is allowed, not {}",
        clean(u.as_str(), 300),
        u.scheme()
    ))
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
