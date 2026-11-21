//! `webfetch`: fetch a URL as markdown, text or HTML. Ported from OpenCode's
//! webfetch tool: the URL is checked against the rules as it is requested
//! (parsed and serialized, without its fragment; see [`normalize`]);
//! redirects are followed by hand so every hop is checked against the deny
//! rules; responses are size-capped and must be textual.

use super::{opt_str, opt_u64, str_arg, Ctx, Outcome, Output};
use crate::permission::Request;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

pub const MAX_BYTES: usize = 5 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const MAX_TIMEOUT_SECS: u64 = 120;
pub const MAX_REDIRECTS: usize = 10;

pub fn spec() -> (String, Value) {
    (
        "Fetch content from an HTTP or HTTPS URL and return it as text, markdown, or HTML. Markdown is the default.\n\nUse a more targeted tool when one is available. This tool is read-only. Large text results may be replaced with a preview while the complete output is retained in managed storage.".into(),
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The HTTP or HTTPS URL to fetch content from"},
                "format": {"type": "string", "enum": ["text", "markdown", "html"], "description": "The format to return the content in. Defaults to markdown."},
                "timeout": {"type": "number", "exclusiveMinimum": 0, "maximum": MAX_TIMEOUT_SECS, "description": format!("Optional timeout in seconds (maximum: {MAX_TIMEOUT_SECS})")}
            },
            "required": ["url"]
        }),
    )
}

pub fn accept(format: &str) -> &'static str {
    match format {
        "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        "html" => "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1",
        _ => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
    }
}

pub fn is_html(content_type: &str) -> bool {
    let c = content_type.to_ascii_lowercase();
    c.contains("text/html") || c.contains("application/xhtml")
}

/// Convert a body to the requested format. Non-HTML bodies pass through.
pub fn convert(body: &str, content_type: &str, format: &str) -> String {
    if !is_html(content_type) || format == "html" {
        return body.to_string();
    }
    match format {
        "text" => super::html_markdown::to_text(body),
        _ => super::html_markdown::to_markdown(body),
    }
}

/// The User-Agent sent first; some sites refuse unknown agents, so it reads
/// like a browser while naming CodeSmith (as OpenCode's does).
pub const USER_AGENT: &str =
    "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; CodeSmith-User/1.0; +https://github.com/Skeptic-Dog-Software/codesmith";
/// The honest agent retried when Cloudflare answers with a browser challenge.
pub const RETRY_USER_AGENT: &str = "codesmith";

const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];

/// The media type of a Content-Type header, lowercased.
fn mime_of(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// OpenCode's `Mime.isTextual`.
fn is_textual(mime: &str) -> bool {
    mime.is_empty()
        || mime.starts_with("text/")
        || mime == "application/json"
        || mime.ends_with("+json")
        || mime == "application/xml"
        || mime.ends_with("+xml")
        || mime == "application/javascript"
        || mime == "application/x-javascript"
}

/// Check a URL the way the rules will see it.
pub(crate) fn check_url(url: &reqwest::Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL must use http:// or https://".into());
    }
    // Rules match the URL, so `https://user@evil.com` would slip past a rule
    // for `https://evil.com/*`.
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL must not include credentials".into());
    }
    check_host(url)
}

/// Refuse a host with a `*` or `?` in it (the URL parser accepts `*`):
/// rules read them as wildcards, so "always" for `https://*/x` would save a
/// rule for every host.
pub(crate) fn check_host(url: &reqwest::Url) -> Result<(), String> {
    if url.host_str().is_some_and(|h| h.contains(['*', '?'])) {
        return Err("URL host must not contain * or ?".into());
    }
    Ok(())
}

/// `raw` parsed, without its fragment: the URL a request goes to, and (as
/// a string) the resource the rules match. Rules see the URL as it is
/// requested, not as written: the parser lowercases the scheme and host,
/// drops a default port, strips tabs and newlines and resolves `.`
/// segments, so `HTTPS://Internal.Corp:443/x` is `https://internal.corp/x`
/// to the rules as to the server. OpenCode matches the URL as written.
pub(crate) fn normalize(raw: &str) -> Result<reqwest::Url, url::ParseError> {
    let mut url = reqwest::Url::parse(raw)?;
    url.set_fragment(None);
    Ok(url)
}

/// The forms of a URL checked against the deny rules and policies before
/// it is asked or requested: as requested ([`normalize`]), percent-decoded
/// (a server reads `/%73ecret` as `/secret`) and, when given, as written
/// without its fragment. Allowing goes by the requested form alone.
pub(crate) fn deny_forms(written: Option<&str>, url: &reqwest::Url) -> Vec<String> {
    let requested = url.to_string();
    let mut out = vec![requested.clone()];
    let decoded = percent_decoded(&requested);
    let written = written.map(|w| w.split('#').next().unwrap_or(w).to_string());
    for form in [Some(decoded), written].into_iter().flatten() {
        if !out.contains(&form) {
            out.push(form);
        }
    }
    out
}

/// `s` with each `%XX` escape decoded (invalid UTF-8 is replaced).
pub(crate) fn percent_decoded(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The rules "always allow" saves for `url`: its origin, with or without a
/// path. Narrower than OpenCode's `*` (any URL).
pub(crate) fn save_patterns(url: &reqwest::Url) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    vec![origin.clone(), format!("{origin}/*")]
}

/// Check `raw` for a `webfetch` request and ask the permission for it:
/// every form of it must pass the deny rules ([`deny_forms`]) and the
/// requested form is asked. The URL to request.
pub(crate) fn authorize(ctx: &Ctx<'_>, raw: &str) -> Result<reqwest::Url, String> {
    let shown = raw.split('#').next().unwrap_or(raw);
    let url = normalize(raw).map_err(|e| format!("Invalid URL {shown:?}: {e}"))?;
    check_url(&url)?;
    ctx.check_not_denied("webfetch", &deny_forms(Some(raw), &url))?;
    ctx.ask(Request {
        action: "webfetch".into(),
        resources: vec![url.to_string()],
        save: save_patterns(&url),
        title: format!("Fetch {url}"),
    })?;
    Ok(url)
}

/// The client webfetch uses. It never follows redirects itself: each hop is
/// followed by hand so its target is checked against the rules (a client
/// with reqwest's default policy would follow up to ten hops unchecked).
fn client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| "Unable to create the HTTP client".to_string())
}

enum Fetched {
    Body { content_type: String, body: Vec<u8> },
    Challenge,
}

/// Fetch `url`, following redirects by hand so each target is checked
/// against the rules first (deny only; the user approved the request).
async fn fetch(
    ctx: &Ctx<'_>,
    url: &str,
    format: &str,
    user_agent: &str,
) -> Result<Fetched, String> {
    let accept = accept(format);
    let resp = get_following(ctx, url, |rb| {
        rb.header("User-Agent", user_agent)
            .header("Accept", accept)
            .header("Accept-Language", "en-US,en;q=0.9")
    })
    .await?;
    let status = resp.status().as_u16();
    if status == 403
        && resp
            .headers()
            .get("cf-mitigated")
            .is_some_and(|v| v == "challenge")
    {
        return Ok(Fetched::Challenge);
    }
    if !resp.status().is_success() {
        return Err(format!("Unable to fetch {url}: HTTP {status}"));
    }
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mime = mime_of(&content_type);
    if mime.starts_with("image/") && mime != "image/svg+xml" && mime != "image/vnd.fastbidsheet" {
        return Err(format!("Unsupported fetched image content type: {mime}"));
    }
    if !is_textual(&mime) {
        return Err(format!("Unsupported fetched file content type: {mime}"));
    }
    let too_large = || format!("Response too large (exceeds {MAX_BYTES} byte limit)");
    if resp.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Unable to fetch {url}: {e}"))?;
        if body.len() + chunk.len() > MAX_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    // A textual type over binary bytes is refused rather than garbled.
    if crate::tool::read::is_binary(&body) {
        return Err(format!(
            "Unable to fetch {url}: the response is binary ({content_type})"
        ));
    }
    Ok(Fetched::Body { content_type, body })
}

/// GET `url` (already checked and approved), following redirects by hand:
/// each target must pass [`check_url`] and no rule may deny fetching it.
/// `build` adds headers to each hop's request. The final, non-redirect
/// response is returned whatever its status.
pub(crate) async fn get_following(
    ctx: &Ctx<'_>,
    url: &str,
    build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, String> {
    let mut current = reqwest::Url::parse(url).map_err(|e| format!("Invalid URL {url:?}: {e}"))?;
    let mut redirects = 0;
    loop {
        let resp = build(client()?.get(current.clone()))
            .send()
            .await
            .map_err(|e| format!("Unable to fetch {url}: {e}"))?;
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let (true, Some(location)) = (REDIRECTS.contains(&status), location) else {
            return Ok(resp);
        };
        if redirects >= MAX_REDIRECTS {
            return Err(format!("Too many redirects (more than {MAX_REDIRECTS})"));
        }
        let mut next = current
            .join(&location)
            .map_err(|e| format!("Invalid redirect location {location:?}: {e}"))?;
        check_url(&next)?;
        next.set_fragment(None);
        ctx.check_not_denied("webfetch", &deny_forms(None, &next))?;
        current = next;
        redirects += 1;
    }
}

pub async fn run(ctx: &Ctx<'_>, input: &Value) -> Outcome {
    let raw = str_arg(input, "url")?;
    let format = opt_str(input, "format").unwrap_or("markdown");
    if !matches!(format, "text" | "markdown" | "html") {
        return Err(format!(
            "Invalid format {format:?}: use text, markdown or html"
        ));
    }
    let timeout = input
        .get("timeout")
        .and_then(Value::as_f64)
        .or_else(|| opt_u64(input, "timeout").map(|t| t as f64))
        .unwrap_or(DEFAULT_TIMEOUT_SECS as f64);
    if timeout <= 0.0 || timeout > MAX_TIMEOUT_SECS as f64 {
        return Err(format!(
            "Timeout must be between 0 and {MAX_TIMEOUT_SECS} seconds"
        ));
    }
    let parsed = authorize(ctx, raw)?;
    let url = parsed.as_str();
    let fetched = tokio::time::timeout(Duration::from_secs_f64(timeout), async {
        match fetch(ctx, url, format, USER_AGENT).await? {
            Fetched::Challenge => fetch(ctx, url, format, RETRY_USER_AGENT).await,
            done => Ok(done),
        }
    })
    .await
    .map_err(|_| "Request timed out".to_string())??;
    let (content_type, body) = match fetched {
        Fetched::Body { content_type, body } => (content_type, body),
        Fetched::Challenge => {
            return Err(format!(
                "Unable to fetch {url}: the site requires a browser challenge (Cloudflare)"
            ))
        }
    };
    let text = String::from_utf8_lossy(&body);
    Ok(Output::text(convert(&text, &content_type, format)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accept_headers() {
        assert!(accept("markdown").starts_with("text/markdown;q=1.0"));
        assert!(accept("text").starts_with("text/plain;q=1.0"));
        assert!(accept("html").starts_with("text/html;q=1.0"));
    }

    #[test]
    fn html_conversion() {
        let html = "<html><head><title>T</title><style>p{}</style></head><body><h1>Hello</h1><p>One &amp; two</p><script>evil()</script><ul><li>a</li><li>b</li></ul></body></html>";
        let md = convert(html, "text/html; charset=utf-8", "markdown");
        assert!(
            md.contains("# Hello")
                && md.contains("One & two")
                && !md.contains("evil")
                && !md.contains("p{}"),
            "{md}"
        );
        let text = convert(html, "text/html", "text");
        assert!(
            text.contains("Hello")
                && text.contains("One & two")
                && !text.contains('<')
                && !text.contains("evil"),
            "{text}"
        );
        assert_eq!(convert(html, "text/html", "html"), html);
        assert_eq!(
            convert("{\"a\":1}", "application/json", "markdown"),
            "{\"a\":1}"
        );
    }

    #[test]
    fn url_validation() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(check_url(&url("https://example.com/x")).is_ok());
        assert_eq!(
            check_url(&url("file:///etc/passwd")).unwrap_err(),
            "URL must use http:// or https://"
        );
        assert_eq!(
            check_url(&url("https://a:b@example.com/")).unwrap_err(),
            "URL must not include credentials"
        );
        for wild in ["https://*github.com/x", "https://*/x", "http://a*.b:81/"] {
            assert_eq!(
                check_url(&url(wild)).unwrap_err(),
                "URL host must not contain * or ?",
                "{wild}"
            );
        }
        let saves = |s: &str| save_patterns(&normalize(s).unwrap());
        assert_eq!(
            saves("https://Example.com:8443/a?b"),
            ["https://example.com:8443", "https://example.com:8443/*"]
        );
        assert_eq!(
            saves("HTTPS://Example.COM:443/a"),
            ["https://example.com", "https://example.com/*"]
        );
    }

    /// The rules see the URL as it is requested, whatever the spelling.
    #[test]
    fn urls_are_matched_as_requested() {
        let n = |s: &str| normalize(s).unwrap().to_string();
        assert_eq!(n("HTTPS://INTERNAL.CORP/x#top"), "https://internal.corp/x");
        assert_eq!(n("https://internal.corp:443/x"), "https://internal.corp/x");
        assert_eq!(n("http://internal.corp:80/x"), "http://internal.corp/x");
        assert_eq!(n("https://inter\tnal.co\nrp/x"), "https://internal.corp/x");
        assert_eq!(n("https://internal.corp/a/../x"), "https://internal.corp/x");
        assert_eq!(n("https://internal.corp"), "https://internal.corp/");
        let url = normalize("https://internal.corp/%73ecret#f").unwrap();
        assert_eq!(
            deny_forms(Some("HTTPS://internal.corp/%73ecret#f"), &url),
            [
                "https://internal.corp/%73ecret",
                "https://internal.corp/secret",
                "HTTPS://internal.corp/%73ecret",
            ]
        );
        assert_eq!(percent_decoded("a%2fb%zz%4"), "a/b%zz%4");
    }

    // ---------------------------------------------------------------------
    // Ported from OpenCode's packages/core/test/tool-webfetch.test.ts
    // ("WebFetchTool helpers" input cases and "WebFetchTool registration",
    // MIT). The converter cases live in html_markdown.rs. Requests go to a
    // local raw-TCP server, never the internet (a denied redirect to
    // 169.254.169.254 is refused before connecting).
    //
    // Intentional differences:
    // - "Always" saves the URL's origin (`https://host` and `https://host/*`)
    //   instead of `*` (every URL).
    // - The rules match the URL as requested (parsed and serialized), not
    //   as written; its written and percent-decoded forms must also pass
    //   the deny rules.
    // - The User-Agent names CodeSmith; the Cloudflare retry sends
    //   `codesmith`.
    // - A non-2xx answer reads "Unable to fetch <url>: HTTP <status>", and
    //   the result is the text only (no structured `contentType` output).
    // - `text` keeps block boundaries as line breaks (see html_markdown.rs).
    // ---------------------------------------------------------------------
    use super::super::testing::Env;
    use crate::permission::{Effect, Rule};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Debug)]
    struct Seen {
        path: String,
        headers: BTreeMap<String, String>,
    }

    enum Reply {
        Send {
            status: u16,
            headers: Vec<(&'static str, String)>,
            body: Vec<u8>,
            length: bool,
        },
        Stall,
    }

    fn ok(content_type: &str, body: &str) -> Reply {
        Reply::Send {
            status: 200,
            headers: vec![("content-type", content_type.into())],
            body: body.as_bytes().to_vec(),
            length: true,
        }
    }

    fn status(code: u16, headers: Vec<(&'static str, String)>) -> Reply {
        Reply::Send {
            status: code,
            headers,
            body: Vec::new(),
            length: true,
        }
    }

    type Handler = Arc<dyn Fn(&Seen, usize) -> Reply + Send + Sync>;

    /// A local HTTP server answering with `handler`; returns its base URL
    /// and the requests it received.
    async fn serve(handler: Handler) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen_log = log.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    return;
                };
                let log = seen_log.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let mut lines = text.split("\r\n");
                    let path = lines
                        .next()
                        .and_then(|l| l.split(' ').nth(1))
                        .unwrap_or("")
                        .to_string();
                    let headers = lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                        .collect();
                    let seen = Seen { path, headers };
                    let n = {
                        let mut log = log.lock().unwrap();
                        log.push(seen.clone());
                        log.len()
                    };
                    match handler(&seen, n) {
                        Reply::Stall => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        Reply::Send {
                            status,
                            headers,
                            body,
                            length,
                        } => {
                            let mut head = format!("HTTP/1.1 {status} X\r\nconnection: close\r\n");
                            for (k, v) in headers {
                                head.push_str(&format!("{k}: {v}\r\n"));
                            }
                            if length {
                                head.push_str(&format!("content-length: {}\r\n", body.len()));
                            }
                            head.push_str("\r\n");
                            let _ = s.write_all(head.as_bytes()).await;
                            let _ = s.write_all(&body).await;
                            let _ = s.shutdown().await;
                        }
                    }
                });
            }
        });
        (base, log)
    }

    fn hello() -> Handler {
        Arc::new(|_, _| ok("text/plain", "hello"))
    }

    async fn fetch_url(env: &Env, input: Value) -> Outcome {
        run(&env.ctx(), &input).await
    }

    #[test]
    fn oc_webfetch_defaults_format_and_rejects_invalid_timeouts() {
        let (_, schema) = spec();
        assert_eq!(schema["required"], json!(["url"]));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let env = Env::new();
            for timeout in [json!(0), json!(MAX_TIMEOUT_SECS + 1)] {
                let err = fetch_url(
                    &env,
                    json!({"url": "https://example.com", "timeout": timeout}),
                )
                .await
                .unwrap_err();
                assert!(err.starts_with("Timeout must be between"), "{err}");
            }
        });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_fetches_a_hostname_url_without_rewriting_it() {
        let (base, log) = serve(hello()).await;
        let url = format!("{base}/public");
        let (env, rec) = Env::recording(vec![]);
        assert!(crate::tool::ALL.iter().any(|t| t.name() == "webfetch"));
        let out = fetch_url(&env, json!({"url": url, "format": "text", "timeout": 4}))
            .await
            .unwrap();
        assert_eq!(out.content, "hello");
        let asked = rec.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].action, "webfetch");
        assert_eq!(asked[0].resources, std::slice::from_ref(&url));
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].path, "/public");
        let h = &seen[0].headers;
        assert_eq!(
            h["accept"],
            "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1"
        );
        assert_eq!(h["accept-language"], "en-US,en;q=0.9");
        assert_eq!(h["user-agent"], USER_AGENT);
        assert!(!h.contains_key("sec-fetch-mode"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_accepts_localhost_urls_with_the_same_check() {
        let (base, log) = serve(hello()).await;
        let url = format!("{}/private", base.replace("127.0.0.1", "localhost"));
        let (env, rec) = Env::recording(vec![]);
        let out = fetch_url(&env, json!({"url": url, "format": "text"}))
            .await
            .unwrap();
        assert_eq!(out.content, "hello");
        assert_eq!(rec.requests(), [("webfetch".to_string(), vec![url])]);
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_follows_redirects_checking_targets_without_asking() {
        let (base, log) = serve(Arc::new(|seen, _| {
            if seen.path == "/redirect" {
                status(302, vec![("location", "/target".into())])
            } else {
                ok("text/plain", "redirected")
            }
        }))
        .await;
        let url = format!("{base}/redirect");
        let (env, rec) = Env::recording(vec![]);
        let out = fetch_url(&env, json!({"url": url, "format": "text"}))
            .await
            .unwrap();
        assert_eq!(out.content, "redirected");
        assert_eq!(rec.requests(), [("webfetch".to_string(), vec![url])]);
        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen.iter().map(|s| s.path.as_str()).collect::<Vec<_>>(),
            ["/redirect", "/target"]
        );
        for s in &seen {
            assert_eq!(s.headers["accept"], accept("text"));
            assert_eq!(s.headers["accept-language"], "en-US,en;q=0.9");
            assert_eq!(s.headers["user-agent"], USER_AGENT);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_stops_at_a_denied_redirect_target() {
        let target = "http://169.254.169.254/latest/meta-data";
        let (base, log) = serve(Arc::new(move |_, _| {
            status(302, vec![("location", target.into())])
        }))
        .await;
        let url = format!("{base}/start");
        let (env, _) = Env::recording(vec![
            Rule::new("webfetch", "*", Effect::Allow),
            Rule::new("webfetch", target, Effect::Deny),
        ]);
        let err = fetch_url(&env, json!({"url": url, "format": "text"}))
            .await
            .unwrap_err();
        assert!(
            err.starts_with("Permission denied") && err.contains("webfetch"),
            "{err}"
        );
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_matches_the_url_as_requested_and_rejects_credentials() {
        let (base, _) = serve(hello()).await;
        let (env, rec) = Env::recording(vec![]);
        let written = base.replace("127.0.0.1", "LocalHost");
        fetch_url(
            &env,
            json!({"url": format!("{written}#top"), "format": "text"}),
        )
        .await
        .unwrap();
        let requested = format!("{}/", base.replace("127.0.0.1", "localhost"));
        assert_eq!(rec.requests(), [("webfetch".to_string(), vec![requested])]);
        let (env, rec) = Env::recording(vec![]);
        assert_eq!(
            fetch_url(
                &env,
                json!({"url": "https://user@example.com/", "format": "text"})
            )
            .await
            .unwrap_err(),
            "URL must not include credentials"
        );
        assert!(rec.requests().is_empty());
    }

    /// A deny rule or policy for a URL holds however the URL is spelled:
    /// case, tabs in the host, percent-encoding or dot segments.
    #[tokio::test(flavor = "multi_thread")]
    async fn deny_rules_and_policies_match_the_url_as_requested() {
        use crate::permission::{Policy, PolicyEffect};
        let (base, log) = serve(hello()).await;
        let port = base.rsplit(':').next().unwrap().to_string();
        let spellings = [
            base.replace("http://", "HTTP://") + "/secret",
            format!("http://127.0.\t0.1:{port}/secret"),
            format!("{base}/%73ecret"),
            format!("{base}/x/../secret"),
        ];
        let policy = Policy::tool(PolicyEffect::Deny, &format!("webfetch:{base}/secret*"))
            .rule()
            .unwrap();
        for rules in [
            vec![
                Rule::new("webfetch", "*", Effect::Allow),
                Rule::new("webfetch", &format!("{base}/secret*"), Effect::Deny),
            ],
            vec![Rule::new("webfetch", "*", Effect::Allow), policy],
        ] {
            for url in &spellings {
                let (env, rec) = Env::recording(rules.clone());
                let err = fetch_url(&env, json!({"url": url, "format": "text"}))
                    .await
                    .unwrap_err();
                assert!(err.starts_with("Permission denied"), "{url}: {err}");
                assert!(rec.requests().is_empty());
            }
        }
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_rejects_non_http_redirects_and_endless_chains() {
        let (base, _) = serve(Arc::new(|_, _| {
            status(301, vec![("location", "file:///etc/passwd".into())])
        }))
        .await;
        let env = Env::new();
        assert_eq!(
            fetch_url(&env, json!({"url": format!("{base}/a"), "format": "text"}))
                .await
                .unwrap_err(),
            "URL must use http:// or https://"
        );
        let (base, log) = serve(Arc::new(|seen, _| {
            status(307, vec![("location", format!("{}x", seen.path))])
        }))
        .await;
        assert_eq!(
            fetch_url(&env, json!({"url": format!("{base}/a"), "format": "text"}))
                .await
                .unwrap_err(),
            format!("Too many redirects (more than {MAX_REDIRECTS})")
        );
        assert_eq!(log.lock().unwrap().len(), MAX_REDIRECTS + 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_rejects_non_http_schemes_before_permission() {
        let (env, rec) = Env::recording(vec![]);
        assert_eq!(
            fetch_url(&env, json!({"url": "file:///etc/passwd", "format": "text"}))
                .await
                .unwrap_err(),
            "URL must use http:// or https://"
        );
        assert!(rec.requests().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_converts_html_to_markdown_and_text() {
        let (base, _) = serve(Arc::new(|_, _| {
            ok(
                "text/html; charset=utf-8",
                "<h1>Hello</h1><p>world</p><script>bad()</script>",
            )
        }))
        .await;
        let env = Env::new();
        let md = fetch_url(&env, json!({"url": base, "format": "markdown"}))
            .await
            .unwrap();
        assert_eq!(md.content, "# Hello\n\nworld");
        let text = fetch_url(&env, json!({"url": base, "format": "text"}))
            .await
            .unwrap();
        assert_eq!(text.content, "Hello\nworld");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_converts_deeply_nested_html() {
        let page = format!(
            "{}content{}",
            "<div>".repeat(10_000),
            "</div>".repeat(10_000)
        );
        let (base, _) = serve(Arc::new(move |_, _| ok("text/html", &page))).await;
        let env = Env::new();
        let out = fetch_url(
            &env,
            json!({"url": format!("{base}/deep-html"), "format": "markdown"}),
        )
        .await
        .unwrap();
        assert_eq!(out.content, "content");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_rejects_declared_and_streamed_oversized_bodies() {
        let too_large = format!("Response too large (exceeds {MAX_BYTES} byte limit)");
        let (base, _) = serve(Arc::new(|_, _| Reply::Send {
            status: 200,
            headers: vec![
                ("content-type", "text/plain".into()),
                ("content-length", (MAX_BYTES + 1).to_string()),
            ],
            body: b"small".to_vec(),
            length: false,
        }))
        .await;
        let env = Env::new();
        assert_eq!(
            fetch_url(
                &env,
                json!({"url": format!("{base}/declared"), "format": "text"})
            )
            .await
            .unwrap_err(),
            too_large
        );
        let (base, _) = serve(Arc::new(|_, _| Reply::Send {
            status: 200,
            headers: vec![("content-type", "text/plain".into())],
            body: vec![b'x'; MAX_BYTES + 1],
            length: false,
        }))
        .await;
        assert_eq!(
            fetch_url(
                &env,
                json!({"url": format!("{base}/streamed"), "format": "text"})
            )
            .await
            .unwrap_err(),
            too_large
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_keeps_images_and_files_unsupported() {
        let (base, _) = serve(Arc::new(|seen, _| {
            if seen.path == "/image" {
                ok("image/png", "png")
            } else {
                ok("application/pdf", "pdf")
            }
        }))
        .await;
        let env = Env::new();
        assert_eq!(
            fetch_url(
                &env,
                json!({"url": format!("{base}/image"), "format": "html"})
            )
            .await
            .unwrap_err(),
            "Unsupported fetched image content type: image/png"
        );
        assert_eq!(
            fetch_url(
                &env,
                json!({"url": format!("{base}/file"), "format": "html"})
            )
            .await
            .unwrap_err(),
            "Unsupported fetched file content type: application/pdf"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_retries_cloudflare_challenges_with_an_honest_agent() {
        let (base, log) = serve(Arc::new(|_, n| {
            if n == 1 {
                Reply::Send {
                    status: 403,
                    headers: vec![("cf-mitigated", "challenge".into())],
                    body: b"challenge".to_vec(),
                    length: true,
                }
            } else {
                ok("text/plain", "ok")
            }
        }))
        .await;
        let env = Env::new();
        let out = fetch_url(&env, json!({"url": base, "format": "text"}))
            .await
            .unwrap();
        assert_eq!(out.content, "ok");
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].headers["user-agent"], USER_AGENT);
        assert_eq!(seen[1].headers["user-agent"], RETRY_USER_AGENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_does_not_retry_ordinary_403s() {
        let (base, log) = serve(Arc::new(|_, _| status(403, vec![]))).await;
        let url = format!("{base}/forbidden");
        let env = Env::new();
        assert_eq!(
            fetch_url(&env, json!({"url": url, "format": "text"}))
                .await
                .unwrap_err(),
            format!("Unable to fetch {url}: HTTP 403")
        );
        let seen = log.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].headers["user-agent"], USER_AGENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_webfetch_times_out_stalled_requests() {
        let (base, _) = serve(Arc::new(|_, _| Reply::Stall)).await;
        let env = Env::new();
        assert_eq!(
            fetch_url(
                &env,
                json!({"url": format!("{base}/slow"), "format": "text", "timeout": 1})
            )
            .await
            .unwrap_err(),
            "Request timed out"
        );
    }
}
