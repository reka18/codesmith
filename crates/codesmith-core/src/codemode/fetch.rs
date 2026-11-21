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
//!   is asked as the `webfetch` permission like the first URL (it may keep
//!   the method and body), and drops the `authorization`, `cookie` and
//!   `proxy-authorization` headers, as a browser does. OpenCode asks only
//!   for the first URL. A public host may not redirect to a local or
//!   private address.
//! - CodeSmith adds no credentials of its own and keeps no cookies.
//! - A response is read whole, up to the webfetch limit
//!   ([`webfetch::MAX_BYTES`](crate::tool::webfetch::MAX_BYTES)), within
//!   [`TIMEOUT`] (time spent answering a redirect's question not counted).
//!
//! The program side is `fetch.js`: it encodes the request, calls
//! [`TOOL`] through the same channel as tool calls, and wraps the result
//! in a `Response`.

use crate::permission::{Denied, Gate, Request};
use crate::tool::webfetch::{self, Reach};
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

/// The client for `reach`: no redirects (followed by hand), no cookie
/// store.
fn client(reach: Reach) -> Result<&'static reqwest::Client, String> {
    static CLIENTS: [std::sync::OnceLock<Option<reqwest::Client>>; 2] =
        [const { std::sync::OnceLock::new() }; 2];
    webfetch::shared_client(&CLIENTS, reach, || {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .user_agent("codesmith")
    })
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
    ask(gate, &asked, &url)?;
    send(gate, parsed, req, &url).await
}

/// Ask `gate` for `req` (the user may be prompted) about `url`.
fn ask(gate: &Gate, req: &Request, url: &str) -> Result<(), String> {
    tokio::task::block_in_place(|| gate.assert(req)).map_err(|e| match e {
        // The feedback is the user talking to the model: it is the message.
        Denied::Declined { feedback: Some(fb) } => {
            format!("The user rejected fetching {url}: {fb}")
        }
        e => e.to_string(),
    })
}

/// Send `req` to `current`, following redirects. The request may take
/// [`TIMEOUT`], not counting the time the user takes to answer a redirect's
/// question.
async fn send(
    gate: &Gate,
    mut current: url::Url,
    mut req: Req,
    first: &str,
) -> Result<Value, String> {
    let mut reach = Reach::Any;
    let mut deadline = tokio::time::Instant::now() + TIMEOUT;
    let timed_out = || {
        format!(
            "fetch {first} timed out after {} seconds.",
            TIMEOUT.as_secs()
        )
    };
    let mut redirects = 0;
    let resp = loop {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| format!("'{}' is not a valid HTTP method.", req.method))?;
        let mut rb = client(reach)?.request(method, current.clone());
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        if let Some(b) = &req.body {
            rb = rb.body(b.clone());
        }
        let resp = tokio::time::timeout_at(deadline, rb.send())
            .await
            .map_err(|_| timed_out())?
            .map_err(|e| {
                webfetch::refusal(&e)
                    .unwrap_or_else(|| format!("fetch failed: {}", e.without_url()))
            })?;
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let (true, Some(location)) = (REDIRECTS.contains(&status), location) else {
            break resp;
        };
        let from_local = reach == Reach::Any
            && tokio::time::timeout_at(deadline, webfetch::answered_locally(&resp, &current))
                .await
                .map_err(|_| timed_out())?;
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
        reach = tokio::time::timeout_at(
            deadline,
            webfetch::check_redirect_target(&current, from_local, &next),
        )
        .await
        .map_err(|_| timed_out())??;
        req = redirected(req, status, &current, &next);
        if next.origin() != current.origin() {
            // Another origin is a request the user has not approved: it
            // may carry the program's method, body and headers (307/308).
            let shown = next.to_string();
            let asked = Request {
                action: ACTION.into(),
                resources: vec![shown.clone()],
                save: webfetch::save_patterns(&next),
                title: format!(
                    "Fetch {shown} ({}, redirected from {})",
                    req.method,
                    current.origin().ascii_serialization()
                ),
            };
            let started = tokio::time::Instant::now();
            ask(gate, &asked, &shown)?;
            deadline += started.elapsed();
        }
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
    while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .map_err(|_| timed_out())?
    {
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

/// A response as plugins' `tool_after` hook sees it: the JSON of
/// [`run`]'s value, with a UTF-8 body as text (`"bodyText"`) instead of
/// base64, so a hook can find and redact what the response says.
pub fn hook_text(v: &Value) -> String {
    let mut v = v.clone();
    let text = v["body"]
        .as_str()
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|b| String::from_utf8(b).ok());
    if let (Some(text), Some(o)) = (text, v.as_object_mut()) {
        o.remove("body");
        o.insert("bodyText".into(), Value::String(text));
    }
    v.to_string()
}

/// [`run`]'s value back from what `tool_after` left of [`hook_text`].
pub fn from_hook_text(text: &str) -> Result<Value, String> {
    let mut v: Value = serde_json::from_str(text)
        .map_err(|_| "fetch: a plugin left the response unreadable.".to_string())?;
    if let Some(o) = v.as_object_mut() {
        if let Some(Value::String(t)) = o.remove("bodyText") {
            o.insert(
                "body".into(),
                Value::String(base64::engine::general_purpose::STANDARD.encode(t)),
            );
        }
    }
    if !v["body"].is_string() || !v["status"].is_u64() {
        return Err("fetch: a plugin left the response unreadable.".into());
    }
    Ok(v)
}

/// A call's value as Code Mode's tool text.
pub fn result_text(v: &Value) -> String {
    format!("{}{v}", super::JSON_RESULT)
}

#[cfg(test)]
mod tests;
