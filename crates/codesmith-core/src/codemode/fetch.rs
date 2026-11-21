//! The `fetch` global in Code Mode, as OpenCode's `codemode/fetch.ts`
//! provides it: programs make HTTP requests with the same checks as the
//! `webfetch` tool.
//!
//! - The URL, parsed and without its fragment, must be `http(s)` with no
//!   credentials in it, and is asked as the `webfetch` permission in the
//!   form it is requested in (see
//!   [`webfetch::normalize`](crate::tool::webfetch::normalize); OpenCode
//!   matches it as written); an "always" answer covers its origin
//!   (OpenCode saves `*`, any URL).
//! - Redirects are followed by hand: each target must pass the same URL
//!   checks and no rule may deny fetching it. A redirect to another origin
//!   drops the `authorization`, `cookie` and `proxy-authorization`
//!   headers, as a browser does.
//! - CodeSmith adds no credentials of its own and keeps no cookies.
//! - A response is read whole, up to the webfetch limit
//!   ([`webfetch::MAX_BYTES`](crate::tool::webfetch::MAX_BYTES)), within
//!   [`TIMEOUT`].
//!
//! The program side is `fetch.js`: it encodes the request, calls
//! [`TOOL`] through the same channel as tool calls, and wraps the result
//! in a `Response`.

use crate::permission::{Denied, Gate, Request};
use crate::tool::webfetch;
use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

/// The name `fetch.js` calls the host by. No tool can have it.
pub const TOOL: &str = "\u{1}fetch";
/// The permission a request asks (the webfetch tool's).
pub const ACTION: &str = "webfetch";
/// How long a request may take, redirects and body included (OpenCode's).
pub const TIMEOUT: Duration = Duration::from_secs(30);
const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];
/// Methods `fetch` refuses, as the Fetch standard does.
const FORBIDDEN_METHODS: [&str; 3] = ["CONNECT", "TRACE", "TRACK"];
/// Request headers a program cannot set: the client frames the request.
const FRAMING: [&str; 9] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "expect",
];
/// Request headers dropped when a redirect leaves the origin.
const CROSS_ORIGIN_DROPPED: [&str; 3] = ["authorization", "cookie", "proxy-authorization"];
/// Request headers dropped when a redirect turns the request into a GET.
const BODY_HEADERS: [&str; 5] = [
    "content-type",
    "content-length",
    "content-encoding",
    "content-language",
    "content-location",
];

/// What a progress row shows for a call: its method and URL.
pub fn display(input: &Value) -> Value {
    json!({
        "method": input["method"].as_str().unwrap_or("GET"),
        "url": input["url"].as_str().unwrap_or(""),
    })
}

/// The client: no redirects (followed by hand), no cookie store.
fn client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
                .user_agent("codesmith")
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| "Unable to create the HTTP client".to_string())
}

struct Req {
    method: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

/// The request after a redirect, as a browser sends it: 303 (but for
/// HEAD), and 301/302 after a POST, become a GET without a body; another
/// origin does not get the request's credentials.
fn redirected(mut r: Req, status: u16, from: &url::Url, to: &url::Url) -> Req {
    let to_get = if status == 303 {
        r.method != "HEAD"
    } else {
        matches!(status, 301 | 302) && r.method == "POST"
    };
    if to_get {
        r.method = "GET".into();
        r.body = None;
        r.headers
            .retain(|(k, _)| !BODY_HEADERS.contains(&k.as_str()));
    }
    if from.origin() != to.origin() {
        r.headers
            .retain(|(k, _)| !CROSS_ORIGIN_DROPPED.contains(&k.as_str()));
    }
    r
}

fn parse_input(input: &Value) -> Result<(String, Req), String> {
    let raw = input["url"]
        .as_str()
        .ok_or("fetch needs a URL.".to_string())?;
    let method = input["method"].as_str().unwrap_or("GET").to_string();
    if method.is_empty() || reqwest::Method::from_bytes(method.as_bytes()).is_err() {
        return Err(format!("'{method}' is not a valid HTTP method."));
    }
    if FORBIDDEN_METHODS.contains(&method.to_ascii_uppercase().as_str()) {
        return Err(format!("'{method}' HTTP method is unsupported."));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for pair in input["headers"].as_array().into_iter().flatten() {
        let (Some(k), Some(v)) = (pair[0].as_str(), pair[1].as_str()) else {
            return Err("fetch headers must be name and value strings.".into());
        };
        let k = k.to_ascii_lowercase();
        if reqwest::header::HeaderName::from_bytes(k.as_bytes()).is_err() {
            return Err(format!("'{k}' is not a valid header name."));
        }
        if reqwest::header::HeaderValue::from_str(v).is_err() {
            return Err(format!("The value of header '{k}' is not valid."));
        }
        if !FRAMING.contains(&k.as_str()) {
            headers.push((k, v.to_string()));
        }
    }
    let body = match &input["body"] {
        Value::Null => None,
        Value::String(b) => Some(
            base64::engine::general_purpose::STANDARD
                .decode(b)
                .map_err(|_| "fetch: the request body is not valid.".to_string())?,
        ),
        _ => return Err("fetch: the request body is not valid.".into()),
    };
    if body.is_some() && matches!(method.as_str(), "GET" | "HEAD") {
        return Err("Request with GET/HEAD method cannot have body.".into());
    }
    Ok((
        raw.to_string(),
        Req {
            method,
            headers,
            body,
        },
    ))
}

/// Run one `fetch` call: the response as `fetch.js` expects it
/// (`status`, `statusText`, `url`, `redirected`, `headers` as pairs, and
/// `body` as base64), or the error the program's promise rejects with.
pub async fn run(gate: &Gate, input: &Value) -> Result<Value, String> {
    let (raw, req) = parse_input(input)?;
    // Rules match the URL as requested (parsed, without the fragment,
    // which is never sent); every form of it must pass the deny rules.
    let written = raw.split('#').next().unwrap_or(&raw);
    let parsed = webfetch::normalize(&raw).map_err(|_| format!("Invalid URL: {written}"))?;
    if parsed.scheme() == "file" {
        return Err("fetch cannot be used for filesystem access.".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("fetch cannot be used with credentials in the URL.".into());
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "fetch cannot be used with {}: URLs.",
            parsed.scheme()
        ));
    }
    webfetch::check_host(&parsed)?;
    let url = parsed.to_string();
    gate.assert_not_denied(ACTION, &webfetch::deny_forms(Some(&raw), &parsed))
        .map_err(|e| e.to_string())?;
    let title = if req.method.eq_ignore_ascii_case("GET") {
        format!("Fetch {url}")
    } else {
        format!("Fetch {url} ({})", req.method)
    };
    let asked = Request {
        action: ACTION.into(),
        resources: vec![url.clone()],
        save: webfetch::save_patterns(&parsed),
        title,
    };
    tokio::task::block_in_place(|| gate.assert(&asked)).map_err(|e| match e {
        // The feedback is the user talking to the model: it is the message.
        Denied::Declined { feedback: Some(fb) } => {
            format!("The user rejected fetching {url}: {fb}")
        }
        e => e.to_string(),
    })?;
    tokio::time::timeout(TIMEOUT, send(gate, parsed, req))
        .await
        .map_err(|_| format!("fetch {url} timed out after {} seconds.", TIMEOUT.as_secs()))?
}

async fn send(gate: &Gate, mut current: url::Url, mut req: Req) -> Result<Value, String> {
    let client = client()?;
    let mut redirects = 0;
    let resp = loop {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| format!("'{}' is not a valid HTTP method.", req.method))?;
        let mut rb = client.request(method, current.clone());
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        if let Some(b) = &req.body {
            rb = rb.body(b.clone());
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| format!("fetch failed: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let (true, Some(location)) = (REDIRECTS.contains(&status), location) else {
            break resp;
        };
        drop(resp);
        if redirects >= webfetch::MAX_REDIRECTS {
            return Err(format!(
                "Too many redirects (more than {})",
                webfetch::MAX_REDIRECTS
            ));
        }
        let mut next = current
            .join(&location)
            .map_err(|_| format!("Invalid redirect location {location:?}"))?;
        if !matches!(next.scheme(), "http" | "https") {
            return Err(format!(
                "fetch cannot follow a redirect to {}: URLs.",
                next.scheme()
            ));
        }
        if !next.username().is_empty() || next.password().is_some() {
            return Err("fetch cannot follow a redirect with credentials in the URL.".into());
        }
        webfetch::check_host(&next)?;
        next.set_fragment(None);
        gate.assert_not_denied(ACTION, &webfetch::deny_forms(None, &next))
            .map_err(|e| e.to_string())?;
        req = redirected(req, status, &current, &next);
        current = next;
        redirects += 1;
    };
    let status = resp.status();
    let headers: Vec<Value> = resp
        .headers()
        .iter()
        .map(|(k, v)| json!([k.as_str(), String::from_utf8_lossy(v.as_bytes())]))
        .collect();
    let too_large = || {
        format!(
            "Response too large (exceeds {} byte limit)",
            webfetch::MAX_BYTES
        )
    };
    if resp
        .content_length()
        .is_some_and(|n| n > webfetch::MAX_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("fetch failed: {}", e.without_url()))?;
        if body.len() + chunk.len() > webfetch::MAX_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(json!({
        "status": status.as_u16(),
        "statusText": status.canonical_reason().unwrap_or(""),
        "url": current.to_string(),
        "redirected": redirects > 0,
        "headers": headers,
        "body": base64::engine::general_purpose::STANDARD.encode(&body),
    }))
}

/// A call's value as Code Mode's tool text.
pub fn result_text(v: &Value) -> String {
    format!("{}{v}", super::JSON_RESULT)
}

#[cfg(test)]
mod tests;
