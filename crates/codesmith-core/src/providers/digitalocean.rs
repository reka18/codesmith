//! DigitalOcean (after OpenCode's `DigitalOceanPlugin`): the Gradient
//! serverless inference API (`https://inference.do-ai.run/v1`) with a model
//! access key (`DIGITALOCEAN_ACCESS_TOKEN` or a saved key), or a browser
//! login (DigitalOcean OAuth, implicit grant, scopes `genai:read
//! inference:query`) whose token also lists the account's inference
//! routers, offered as `digitalocean/router:<name>` models.
//!
//! The implicit grant returns the token in the redirect's fragment, which
//! never reaches a server, so the callback page posts it back to the local
//! listener (`/auth/token`) itself. The login runs as an OAuth application
//! you register with DigitalOcean (redirect URI
//! `http://localhost:1456/auth/callback`); CodeSmith ships none: give its
//! client id with `--client-id`, `settings.clientId` or
//! `CODESMITH_DIGITALOCEAN_CLIENT_ID`.

use super::{set, Input, Route};
use anyhow::{anyhow, Result};
use codesmith_ai::{Auth, Protocol};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const ID: &str = "digitalocean";
pub const METHOD: &str = "browser";
pub const LABEL: &str = "Login with DigitalOcean";
pub const CLIENT_ID_VAR: &str = "CODESMITH_DIGITALOCEAN_CLIENT_ID";
pub const PORT: u16 = 1456;
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const TOKEN_PATH: &str = "/auth/token";
pub const BASE_URL: &str = "https://inference.do-ai.run/v1";
pub const SCOPE: &str = "genai:read inference:query";
/// A token without a lifetime lasts 30 days.
const DEFAULT_LIFE: u64 = 60 * 60 * 24 * 30;

/// DigitalOcean's endpoints (overridable in tests).
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub client_id: String,
    pub authorize: String,
    pub routers: String,
    /// The callback port (DigitalOcean matches the registered redirect).
    pub port: u16,
}

impl Endpoints {
    pub fn digitalocean(client_id: &str) -> Endpoints {
        Endpoints {
            client_id: client_id.to_string(),
            authorize: "https://cloud.digitalocean.com/v1/oauth/authorize".into(),
            routers: "https://api.digitalocean.com/v2/gen-ai/models/routers".into(),
            port: PORT,
        }
    }
}

pub fn client_id(
    flag: Option<&str>,
    settings: &super::Settings,
    env: &super::Env,
) -> Result<String> {
    flag.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| set(&settings.client_id).map(str::to_string))
        .or_else(|| env(CLIENT_ID_VAR))
        .ok_or_else(|| {
            anyhow!(
                "DigitalOcean's browser login needs the client id of an OAuth application you register with DigitalOcean, and CodeSmith ships none.\n\
                 Register one at https://cloud.digitalocean.com/account/api/applications with the callback URL http://localhost:{PORT}{CALLBACK_PATH}, \
                 and pass its client id with --client-id, settings.clientId on the digitalocean provider, or {CLIENT_ID_VAR}.\n\
                 Or skip the login: create a model access key and run `codesmith auth login digitalocean`, or set DIGITALOCEAN_ACCESS_TOKEN."
            )
        })
}

/// The page the redirect lands on: it reads the token from the fragment
/// and posts it to the listener.
fn bootstrap_page() -> String {
    format!(
        r#"<!doctype html><meta charset=utf-8><title>CodeSmith</title>
<p id=m style="font-family:system-ui;margin:3em">Finishing the DigitalOcean sign-in…</p>
<script>
(function(){{
  var p=new URLSearchParams(window.location.hash.slice(1)),b={{}};
  p.forEach(function(v,k){{b[k]=v}});
  fetch("{TOKEN_PATH}",{{method:"POST",headers:{{"Content-Type":"application/json"}},body:JSON.stringify(b)}})
    .then(function(r){{return r.json().then(function(j){{return [r.ok,j]}})}})
    .then(function(x){{document.getElementById("m").textContent=x[0]?"Authorization successful. DigitalOcean is connected to CodeSmith; you can close this tab.":"Authorization failed: "+(x[1].error||"")}})
    .catch(function(e){{document.getElementById("m").textContent="Authorization failed: "+e}});
}})();
</script>"#
    )
}

async fn reply(mut sock: TcpStream, status: u16, kind: &str, body: &str) {
    let _ = sock
        .write_all(
            format!(
                "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
    let _ = sock.shutdown().await;
}

/// The method, path and body of a request.
async fn read(sock: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 16384];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(i) = text.find("\r\n\r\n") {
            let len = text[..i]
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buf.len() >= i + 4 + len || buf.len() > 1 << 20 {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut first = head.lines().next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?;
    let path = target.split('?').next().unwrap_or(target).to_string();
    Some((method, path, body.to_string()))
}

/// What the callback page posted: the token and its lifetime, or why not.
pub fn check(body: &str, state: &str) -> Result<(String, u64), String> {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let s = |k: &str| v[k].as_str().filter(|x| !x.is_empty()).map(str::to_string);
    if let Some(e) = s("error_description").or_else(|| s("error")) {
        return Err(e);
    }
    let Some(access) = s("access_token") else {
        return Err("Missing access token".into());
    };
    if s("state").as_deref() != Some(state) {
        return Err("Invalid OAuth state".into());
    }
    let life = s("expires_in")
        .and_then(|e| e.parse::<u64>().ok())
        .filter(|e| *e > 0)
        .unwrap_or(DEFAULT_LIFE);
    Ok((access, life))
}

/// A started login: open `url`, then [`finish`].
pub struct Login {
    pub url: String,
    pub redirect: String,
    listener: TcpListener,
    state: String,
}

pub async fn start(e: &Endpoints) -> Result<Login> {
    let listener = TcpListener::bind(("127.0.0.1", e.port)).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::AddrInUse {
            anyhow!(
                "DigitalOcean login needs local port {}, but it is already in use. Stop the process using that port and try again.",
                e.port
            )
        } else {
            anyhow::Error::from(err)
        }
    })?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://localhost:{port}{CALLBACK_PATH}");
    let state = super::oauth::Pkce::new().state;
    let mut url = url::Url::parse(&e.authorize)?;
    url.query_pairs_mut()
        .append_pair("response_type", "token")
        .append_pair("client_id", &e.client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", SCOPE)
        .append_pair("state", &state);
    Ok(Login {
        url: url.to_string(),
        redirect,
        listener,
        state,
    })
}

/// Serve the callback page until it posts the token; the listener closes
/// when this returns.
pub async fn finish(login: Login, timeout: Duration) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (mut sock, _) = tokio::time::timeout_at(deadline, login.listener.accept())
            .await
            .map_err(|_| anyhow!("timed out waiting for the browser"))??;
        let Some((method, path, body)) = read(&mut sock).await else {
            continue;
        };
        match (method.as_str(), path.as_str()) {
            ("GET", CALLBACK_PATH) => {
                reply(sock, 200, "text/html; charset=utf-8", &bootstrap_page()).await
            }
            ("POST", TOKEN_PATH) => {
                return match check(&body, &login.state) {
                    Ok((access, life)) => {
                        reply(
                            sock,
                            200,
                            "application/json",
                            &json!({"ok": true}).to_string(),
                        )
                        .await;
                        Ok(json!({
                            "type": "oauth",
                            "method": METHOD,
                            "access": access,
                            "refresh": "",
                            "expires": super::oauth::now_ms() + life * 1000,
                        }))
                    }
                    Err(e) => {
                        reply(
                            sock,
                            400,
                            "application/json",
                            &json!({"error": e}).to_string(),
                        )
                        .await;
                        Err(anyhow!(e))
                    }
                };
            }
            _ => reply(sock, 404, "text/plain", "Not found").await,
        }
    }
}

/// The whole browser login, saved to `file`, then the account's routers
/// listed (a failure there does not undo the login).
pub async fn login(
    client: &reqwest::Client,
    file: &std::path::Path,
    e: &Endpoints,
    open: impl FnOnce(&str),
) -> Result<Option<String>> {
    let l = start(e).await?;
    open(&l.url);
    let entry = finish(l, Duration::from_secs(600)).await?;
    crate::auth::set_entry_in(file, ID, entry.clone())?;
    Ok(
        match refresh_routers(client, e, &entry, &super::live::file()).await {
            Ok(n) => Some(format!("{} inference routers", n.len())),
            Err(_) => None,
        },
    )
}

/// The access token of a saved browser login while it lasts.
pub fn stored_token(stored: Option<&Value>, now: u64) -> Result<Option<String>> {
    let Some(s) = stored.filter(|s| s["type"] == "oauth" && s["method"] == METHOD) else {
        return Ok(None);
    };
    let expires = s["expires"].as_u64().unwrap_or(0);
    if expires != 0 && expires <= now {
        return Err(anyhow!(
            "the DigitalOcean login has expired: log in again with `codesmith auth login digitalocean --oauth`"
        ));
    }
    Ok(s["access"].as_str().map(str::to_string))
}

/// The account's inference routers, sorted by name.
pub async fn routers(client: &reqwest::Client, e: &Endpoints, token: &str) -> Result<Vec<String>> {
    let resp = client
        .get(&e.routers)
        .bearer_auth(token)
        .header("accept", "application/json")
        .header("user-agent", codesmith_ai::copilot::user_agent())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|err| {
            anyhow!(
                "listing DigitalOcean routers: {}",
                codesmith_ai::Error::from(err)
            )
        })?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "listing DigitalOcean routers: HTTP {}",
            resp.status().as_u16()
        ));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|_| anyhow!("listing DigitalOcean routers: not JSON"))?;
    let list = v["model_routers"]
        .as_array()
        .ok_or_else(|| anyhow!("listing DigitalOcean routers: no model_routers"))?;
    let mut names = list
        .iter()
        .map(|r| {
            r["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .ok_or(())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| anyhow!("listing DigitalOcean routers: a router has no name"))?;
    names.sort();
    Ok(names)
}

/// List and keep the routers of a saved login (nothing is asked with an
/// expired one).
pub async fn refresh_routers(
    client: &reqwest::Client,
    e: &Endpoints,
    login: &Value,
    path: &std::path::Path,
) -> Result<Vec<String>> {
    let token = stored_token(Some(login), super::oauth::now_ms())?
        .ok_or_else(|| anyhow!("DigitalOcean's routers need a browser login: `codesmith auth login digitalocean --oauth`"))?;
    let names = routers(client, e, &token).await?;
    let raw = names.iter().map(|n| json!({"name": n})).collect();
    super::live::save_in(path, ID, &super::live::fingerprint(&token), "", raw)?;
    Ok(names)
}

/// Routers as models (`router:<name>`), unless the catalog has the id.
pub fn derive(raw: &[Value], templates: &[crate::catalog::Model]) -> Vec<crate::catalog::Model> {
    raw.iter()
        .filter_map(|r| r["name"].as_str())
        .map(|name| format!("router:{name}"))
        .filter(|id| !templates.iter().any(|m| &m.id == id))
        .map(|id| {
            let mut m = super::live::blank(ID, &id);
            m.name = id.trim_start_matches("router:").to_string();
            m.family = Some("digitalocean-inference-routers".into());
            m.package = Some("@opencode/ai/providers/digitalocean".into());
            m.settings = Some(crate::catalog::Settings {
                base_url: Some(BASE_URL.into()),
                provider: None,
            });
            m.limit = crate::catalog::Limit {
                context: 128_000,
                input: None,
                output: 8_192,
            };
            m
        })
        .collect()
}

pub fn route(input: &Input) -> Result<Route> {
    let api_key = match &input.api_key {
        Some(k) => Some(k.clone()),
        None => stored_token(input.stored.as_ref(), super::oauth::now_ms())?,
    };
    let protocol = input.configured_protocol.unwrap_or_else(|| {
        input
            .package
            .and_then(crate::catalog::protocol)
            .unwrap_or(Protocol::OpenaiChat)
    });
    Ok(Route {
        protocol,
        base_url: input
            .configured_url
            .or(input.catalog_url)
            .unwrap_or(BASE_URL)
            .to_string(),
        auth: Auth::Default,
        api_key,
        headers: vec![],
        model_id: None,
    })
}
