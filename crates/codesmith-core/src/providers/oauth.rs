//! The browser half of provider OAuth logins (after OpenCode's provider
//! `authorize` flows): PKCE, a loopback listener for the redirect, and the
//! checks every provider makes on it. The provider modules build the
//! authorization URL and exchange the code.
//!
//! The listener answers the browser only once the code has been exchanged
//! (so the page says whether the login worked), answers any later callback
//! with 409, and closes when the login is dropped. A redirect pasted into
//! the terminal (the browser on another machine) completes the login the
//! same way. A callback or paste that belongs to another login (a wrong
//! state) is refused and the login keeps waiting, so another local program
//! cannot end it; each connection is read in its own task with a short
//! timeout, so a silent one stalls nothing.

use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The query of a callback.
pub type Params = BTreeMap<String, String>;

/// A PKCE verifier, its S256 challenge, and a state.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn new() -> Pkce {
        let (verifier, challenge) = crate::mcp::oauth::pkce();
        let state = crate::mcp::oauth::pkce().0[..43].to_string();
        Pkce {
            verifier,
            challenge,
            state,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Pkce::new()
    }
}

/// The error of a callback whose state is not the login's.
pub const INVALID_STATE: &str = "Invalid OAuth state";

/// How long a callback connection may take to send its request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The checks of a callback: the state, then an error, then the code.
/// `detail` keeps a provider's error description unless it repeats the
/// state.
pub fn check(params: &Params, state: &str, provider: &str) -> Result<String, String> {
    if params.get("state").map(String::as_str) != Some(state) {
        return Err(INVALID_STATE.into());
    }
    if params.contains_key("error") {
        let detail = params
            .get("error_description")
            .filter(|d| !d.is_empty())
            .or(params.get("error").filter(|d| !d.is_empty()))
            .cloned()
            .unwrap_or_else(|| "Authorization denied".into());
        return Err(if detail.contains(state) {
            format!("{provider} authorization failed")
        } else {
            detail
        });
    }
    match params.get("code").map(|c| c.trim()) {
        Some(c) if !c.is_empty() => Ok(c.to_string()),
        _ => Err("Missing authorization code".into()),
    }
}

/// The page the browser is shown.
pub fn page(provider: &str, error: Option<&str>) -> String {
    let body = match error {
        None => format!(
            "Authorization successful. {} is connected to CodeSmith; you can close this tab.",
            html_escape(provider)
        ),
        Some(e) => format!("Authorization failed: {}", html_escape(e)),
    };
    format!("<!doctype html><meta charset=utf-8><title>CodeSmith</title><p style=\"font-family:system-ui;margin:3em\">{body}</p>")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn reply(mut sock: TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
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

/// The method and target of a request.
async fn request_line(sock: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = vec![0u8; 8192];
    let mut have = 0;
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut buf[have..]))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            break;
        }
        have += n;
        if buf[..have].windows(4).any(|w| w == b"\r\n\r\n") || have == buf.len() {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf[..have]).to_string();
    let mut parts = text.lines().next()?.split_whitespace();
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

fn query(target: &str) -> Option<(String, Params)> {
    let u = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    Some((
        u.path().to_string(),
        u.query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
    ))
}

/// Listen on loopback `port` (0 for any free port) on both `127.0.0.1` and
/// `[::1]`, for a redirect to `http://localhost:<port>`: a browser may
/// resolve `localhost` to either, so leaving one free would let another
/// local user listen there and take the redirect. Fails when either is in
/// use; a machine without IPv6 loopback gets the IPv4 listener alone (no
/// one can listen on `[::1]` there either). A redirect to `127.0.0.1`
/// needs only [`TcpListener::bind`] on it.
pub async fn bind_localhost(port: u16) -> std::io::Result<Vec<TcpListener>> {
    let mut tries = if port == 0 { 10 } else { 1 };
    loop {
        let v4 = TcpListener::bind(("127.0.0.1", port)).await?;
        let got = v4.local_addr()?.port();
        match TcpListener::bind(("::1", got)).await {
            Ok(v6) => return Ok(vec![v4, v6]),
            Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => return Ok(vec![v4]),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries > 1 => {
                tries -= 1;
                drop(v4);
            }
            Err(e) => return Err(e),
        }
    }
}

/// The next connection on any of `listeners`.
pub async fn accept_any(listeners: &[TcpListener]) -> std::io::Result<TcpStream> {
    let accepts = listeners.iter().map(|l| Box::pin(l.accept()));
    let (got, _, _) = futures_util::future::select_all(accepts).await;
    got.map(|(sock, _)| sock)
}

/// A loopback listener waiting for one login's redirect.
pub struct Callback {
    listener: Option<TcpListener>,
    path: String,
    pub redirect: String,
    provider: String,
    drain: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Callback {
    fn drop(&mut self) {
        if let Some(d) = self.drain.take() {
            d.abort();
        }
    }
}

/// A callback that passed the checks, whose browser waits for the result.
pub struct Received {
    pub code: String,
    pub params: Params,
    browser: Option<TcpStream>,
    provider: String,
}

impl Received {
    /// Tell the browser how the login ended.
    pub async fn finish(mut self, result: Result<(), &str>) {
        if let Some(sock) = self.browser.take() {
            match result {
                Ok(()) => reply(sock, 200, &page(&self.provider, None)).await,
                Err(e) => reply(sock, 400, &page(&self.provider, Some(e))).await,
            }
        }
    }
}

impl Callback {
    /// Listen on `127.0.0.1:<port>` (0 for any free port) for `path`.
    pub async fn bind(provider: &str, port: u16, path: &str) -> Result<Callback> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AddrInUse => anyhow!(
                    "The {provider} login needs port {port} for its browser callback, and another program is using it. Stop that program, or register the OAuth application with another redirect port, and try again."
                ),
                _ => anyhow::Error::from(e),
            })
            .with_context(|| format!("listening for the {provider} browser callback"))?;
        let port = listener.local_addr()?.port();
        Ok(Callback {
            listener: Some(listener),
            path: path.to_string(),
            redirect: format!("http://127.0.0.1:{port}{path}"),
            provider: provider.to_string(),
            drain: None,
        })
    }

    /// Wait for the redirect (or a pasted one), checked with `check`, for
    /// at most `timeout`. A failed check is answered at once and ends the
    /// login, except a wrong state: that request (from another login, or
    /// another program) is answered 400 and the login keeps waiting, and a
    /// paste that belongs to another login is reported through
    /// `wrong_paste`. Each connection is read on its own, with
    /// [`READ_TIMEOUT`] to send its request, so a silent one holds up none.
    pub async fn wait(
        &mut self,
        check: impl Fn(&Params) -> Result<String, String>,
        mut pasted: Option<tokio::sync::mpsc::Receiver<String>>,
        wrong_paste: impl Fn(&str),
        timeout: Duration,
    ) -> Result<Received> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| anyhow!("the login already ended"))?;
        let deadline = tokio::time::Instant::now() + timeout;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(TcpStream, Params)>(4);
        let mut readers = tokio::task::JoinSet::new();
        let out = loop {
            let paste = async {
                match pasted.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    break Err(anyhow!("timed out waiting for the browser ({} minutes)", timeout.as_secs() / 60));
                }
                text = paste => {
                    let Some(text) = text else { pasted = None; continue };
                    let Some(params) = pasted_params(&text) else {
                        wrong_paste("That is not the URL the browser was sent to");
                        continue;
                    };
                    match check(&params) {
                        Err(e) if e == INVALID_STATE => {
                            wrong_paste("This URL belongs to a different sign-in");
                            continue;
                        }
                        Err(e) => break Err(anyhow!(e)),
                        Ok(code) => break Ok(Received { code, params, browser: None, provider: self.provider.clone() }),
                    }
                }
                Some((sock, params)) = rx.recv() => {
                    match check(&params) {
                        Err(e) if e == INVALID_STATE => {
                            let page = page(&self.provider, Some(&e));
                            readers.spawn(async move { reply(sock, 400, &page).await });
                            continue;
                        }
                        Err(e) => {
                            reply(sock, 400, &page(&self.provider, Some(&e))).await;
                            break Err(anyhow!(e));
                        }
                        Ok(code) => break Ok(Received { code, params, browser: Some(sock), provider: self.provider.clone() }),
                    }
                }
                acc = listener.accept() => {
                    let Ok((mut sock, _)) = acc else { continue };
                    let (tx, path) = (tx.clone(), self.path.clone());
                    readers.spawn(async move {
                        let Ok(Some((method, target))) =
                            tokio::time::timeout(READ_TIMEOUT, request_line(&mut sock)).await
                        else {
                            return;
                        };
                        match query(&target) {
                            Some((p, params)) if method == "GET" && p == path => {
                                let _ = tx.send((sock, params)).await;
                            }
                            _ => reply(sock, 404, "").await,
                        }
                    });
                }
            }
        };
        readers.abort_all();
        // Later callbacks are refused until the login is dropped.
        self.drain = Some(tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(READ_TIMEOUT, request_line(&mut sock)).await;
                    reply(sock, 409, "OAuth callback already received").await;
                });
            }
        }));
        out
    }
}

/// The query of a pasted redirect URL (or of a bare query string).
pub fn pasted_params(text: &str) -> Option<Params> {
    let t = text.trim();
    let u = url::Url::parse(t).ok().or_else(|| {
        url::Url::parse(&format!("http://127.0.0.1/?{}", t.trim_start_matches('?'))).ok()
    })?;
    let params: Params = u
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    (!params.is_empty()).then_some(params)
}

/// Lines typed in the terminal while a login waits, for pasted redirects.
pub fn stdin_lines() -> tokio::sync::mpsc::Receiver<String> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            if tx.blocking_send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });
    rx
}

/// A form POST to a token endpoint: the JSON answer, or the status and
/// body of a failure.
pub async fn post_form(
    client: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
    headers: &[(&str, String)],
) -> Result<std::result::Result<serde_json::Value, (u16, String)>> {
    let mut rb = client
        .post(url)
        .header("accept", "application/json")
        .form(form)
        .timeout(Duration::from_secs(30));
    for (k, v) in headers {
        rb = rb.header(*k, v);
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| anyhow!("{}", codesmith_ai::Error::from(e)))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Ok(Err((status, text)));
    }
    Ok(Ok(
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
    ))
}

/// The provider's description of a failed token request, unless it repeats
/// one of the secrets sent.
pub fn error_detail(body: &str, secrets: &[&str]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let d = v["error_description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or(v["message"].as_str().filter(|s| !s.is_empty()))
        .or(v["error"].as_str().filter(|s| !s.is_empty()))?;
    (!secrets.iter().any(|s| !s.is_empty() && d.contains(s))).then(|| d.to_string())
}

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // oauth-page.test.ts: "escapes bootstrap options embedded in the inline
    // script" (adapted: the page has no script; what it embeds is escaped)
    #[test]
    fn oc_oauth_page_escapes_what_it_embeds() {
        let ok = page(r#"xAI</script><script>alert("provider")</script>"#, None);
        assert!(!ok.contains("<script>"), "{ok}");
        assert!(ok.contains("xAI&lt;/script&gt;&lt;script&gt;alert(&quot;provider&quot;)"));
        let failed = page("xai", Some("</p><img src=x onerror='1'>"));
        assert!(!failed.contains("<img"), "{failed}");
        assert!(failed.contains("&lt;img src=x onerror=&#39;1&#39;&gt;"));
    }
}
