//! What a request to a Bifrost gateway carries beyond its protocol's own
//! headers, and what its response headers say back.
//!
//! Every HTTP request to the `bifrost` provider gets a fresh UUID v4 in
//! `x-request-id`, a resend after a renewed credential included: Bifrost
//! drops a request whose id it has already logged, so an id is never
//! reused. The response names the request as Bifrost logged it
//! (`x-bifrost-request-id`) and, after routing or a fallback, the
//! provider and model that actually ran it
//! (`x-bifrost-routing-info-provider`, `x-bifrost-routing-info-model`):
//! [`served`] reads them, falling back to the id that was sent when an
//! older gateway leaves the header out.

use crate::{Provider, Served};

/// The provider id whose requests are stamped.
pub const ID: &str = "bifrost";

/// The header a virtual key goes in.
pub const KEY_HEADER: &str = "x-bf-vk";

/// The id sent with a request.
pub const REQUEST_ID: &str = "x-request-id";

/// The session a request belongs to, for Bifrost's agent views.
pub const CORRELATION_ID: &str = "x-bf-agent-correlation-id";

/// CodeSmith's own session header, which the correlation id copies.
const SESSION_HEADER: &str = "x-codesmith-session-id";

pub const RESPONSE_ID: &str = "x-bifrost-request-id";
pub const ROUTED_PROVIDER: &str = "x-bifrost-routing-info-provider";
pub const ROUTED_MODEL: &str = "x-bifrost-routing-info-model";

/// The `x-request-id` a response's request was sent with, kept on the
/// response.
#[derive(Debug, Clone)]
pub(crate) struct SentId(pub String);

/// Whether `p` is a Bifrost gateway.
pub fn is(p: &Provider) -> bool {
    p.name == ID
}

/// A random (version 4) UUID.
pub fn uuid_v4() -> String {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    if ring::rand::SystemRandom::new().fill(&mut b).is_err() {
        // No system randomness: the time and a counter still never repeat
        // within the process.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        b[..8].copy_from_slice(&t.to_le_bytes());
        b[8..].copy_from_slice(&N.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Give a request to `p` a fresh `x-request-id` (replacing any) and the
/// session's correlation id, when `p` is a Bifrost gateway: the id sent.
pub(crate) fn stamp(p: &Provider, req: &mut reqwest::Request) -> Option<String> {
    if !is(p) {
        return None;
    }
    let id = uuid_v4();
    let h = req.headers_mut();
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&id) {
        h.insert(REQUEST_ID, v);
    }
    if !h.contains_key(CORRELATION_ID) {
        if let Some(s) = h.get(SESSION_HEADER).cloned() {
            h.insert(CORRELATION_ID, s);
        }
    }
    Some(id)
}

/// A header's value, when it is short printable text.
fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    let v = resp.headers().get(name)?.to_str().ok()?.trim();
    (!v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control)).then(|| v.to_string())
}

/// What a Bifrost response's headers say about its request; `None` for
/// any other provider.
pub(crate) fn served(p: &Provider, resp: &reqwest::Response) -> Option<Served> {
    if !is(p) {
        return None;
    }
    let sent = resp.extensions().get::<SentId>().map(|s| s.0.clone());
    Some(Served {
        request_id: header(resp, RESPONSE_ID).or(sent),
        provider: header(resp, ROUTED_PROVIDER),
        model: header(resp, ROUTED_MODEL),
    })
}

/// `stream` with the [`crate::Event::Served`] of `resp` first, when it
/// has one.
pub(crate) fn with_served(
    served: Option<Served>,
    stream: crate::EventStream,
) -> crate::EventStream {
    use futures_util::StreamExt;
    match served {
        Some(s) => futures_util::stream::once(async move { Ok(crate::Event::Served(s)) })
            .chain(stream)
            .boxed(),
        None => stream,
    }
}

/// A JSON GET of `url` with `p`'s key, configured headers and a fresh
/// request id, within `timeout`: the status and the parsed body. The key
/// never appears in an error.
pub async fn get_json(
    p: &Provider,
    client: &reqwest::Client,
    url: &str,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, crate::Error> {
    let mut rb = client.get(url).timeout(timeout);
    for (k, v) in p.auth_headers(p.headers.clone()) {
        rb = rb.header(k, v);
    }
    let resp = crate::endpoint::execute(p, rb)
        .await
        .map_err(|e| redact_error(e, p.api_key.as_deref()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| redact_error(crate::Error::from(e), p.api_key.as_deref()))?;
    if !status.is_success() {
        return Err(crate::Error::Status {
            status: status.as_u16(),
            body: redact(
                &crate::redact_urls(&text.chars().take(2000).collect::<String>()),
                p.api_key.as_deref(),
            ),
        });
    }
    serde_json::from_str(&text).map_err(|e| crate::Error::Decode(e.to_string()))
}

/// Whether a gateway's `status` and `body` say the route itself isn't
/// served there, rather than anything about the key or the model: a 405;
/// or a 403 or 404 whose body isn't an API error, which every Bifrost
/// refusal of a key or model is (`{"error": {"type": …, "message": …}}`),
/// passed on from an upstream or written by Bifrost's governance. A string
/// `error` (a proxy in front of the gateway), a page that isn't JSON, and
/// Bifrost's own "Route not found" are route refusals.
pub fn is_route_refusal(status: u16, body: &str) -> bool {
    match status {
        405 => true,
        403 | 404 => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(body.trim()) else {
                return true;
            };
            match v.get("error") {
                Some(serde_json::Value::Object(e)) => e
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|m| m.starts_with("Route not found")),
                _ => true,
            }
        }
        _ => false,
    }
}

/// What a gateway's error body says, short: its `error` text or
/// `error.message`, else the body itself, with URLs' queries and `key`
/// taken out.
pub fn gateway_message(body: &str, key: Option<&str>) -> String {
    let v = serde_json::from_str::<serde_json::Value>(body.trim()).ok();
    let said = v
        .as_ref()
        .and_then(|v| {
            v["error"]
                .as_str()
                .or_else(|| v["error"]["message"].as_str())
                .or_else(|| v["message"].as_str())
        })
        .map(str::to_string)
        .unwrap_or_else(|| body.trim().to_string());
    let said: String = said
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(300)
        .collect();
    let said = redact(&crate::redact_urls(&said), key);
    if said.trim().is_empty() {
        "no message".into()
    } else {
        said.trim().to_string()
    }
}

/// `url` as shown in an error: without its query (which may carry
/// configured parameters) or any user info.
pub fn shown_url(url: &reqwest::Url) -> String {
    let mut u = url.clone();
    u.set_query(None);
    u.set_fragment(None);
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.to_string()
}

/// The setting that picks the route of a request to `url` in `protocol`,
/// and what to try.
fn route_hint(protocol: crate::Protocol, url: &reqwest::Url) -> String {
    use crate::Protocol::*;
    let setting = "providers.bifrost.settings";
    match protocol {
        AnthropicMessages => format!(
            "Anthropic models go to <baseURL>/anthropic/v1 unless {setting}.anthropicBaseURL (BIFROST_ANTHROPIC_BASE_URL) is set; set it to where this gateway serves Bifrost's Anthropic API"
        ),
        GoogleGemini => format!(
            "models with protocol \"google-gemini\" go to <baseURL>/genai/v1beta unless {setting}.geminiBaseURL (BIFROST_GEMINI_BASE_URL) is set; set it to where this gateway serves Bifrost's Gemini API"
        ),
        _ if url.path().contains("/openai/v1/") => format!(
            "this is Bifrost's OpenAI integration route, which {setting}.api (BIFROST_API) or the model's own api picks with \"openai\", unless {setting}.openaiBaseURL (BIFROST_OPENAI_BASE_URL) is set; if this gateway serves only the unified API, set api to \"unified\", or set openaiBaseURL to where it serves the OpenAI API (`codesmith debug model` shows where each URL came from)"
        ),
        _ => format!(
            "the route is {setting}.openaiBaseURL (BIFROST_OPENAI_BASE_URL) when that is set, else what {setting}.api (BIFROST_API) picks: \"unified\" (the default) is <baseURL>/v1, \"openai\" is <baseURL>/openai/v1 (`codesmith debug model` shows where each URL came from); this gateway may serve only Bifrost's integration routes: set {setting}.api to \"openai\", or point openaiBaseURL at where it serves the OpenAI API"
        ),
    }
}

/// The error for a request to `url` that the gateway answered as a route
/// it doesn't serve ([`is_route_refusal`]): the URL, the gateway's own
/// message without the key, and the setting that picks the route. It is
/// never retried, on this route or another.
pub fn route_error(p: &Provider, url: &reqwest::Url, status: u16, body: &str) -> crate::Error {
    crate::Error::Invalid(format!(
        "Bifrost: {} answered HTTP {status} ({}): the gateway doesn't serve this route. Nothing else was tried; {}.",
        shown_url(url),
        gateway_message(body, p.api_key.as_deref()),
        route_hint(p.protocol, url),
    ))
}

/// `text` with every copy of `key` taken out.
pub fn redact(text: &str, key: Option<&str>) -> String {
    match key.filter(|k| k.len() >= 4) {
        Some(k) => text.replace(k, "[redacted]"),
        None => text.to_string(),
    }
}

fn redact_error(e: crate::Error, key: Option<&str>) -> crate::Error {
    match e {
        crate::Error::Status { status, body } => crate::Error::Status {
            status,
            body: redact(&body, key),
        },
        other => {
            let shown = other.to_string();
            let clean = redact(&shown, key);
            if clean == shown {
                other
            } else {
                crate::Error::Invalid(clean)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_v4_uuids_and_never_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let id = uuid_v4();
            assert_eq!(id.len(), 36, "{id}");
            let parts: Vec<&str> = id.split('-').collect();
            assert_eq!(
                parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
                [8, 4, 4, 4, 12]
            );
            assert!(parts[2].starts_with('4'), "{id}");
            assert!(matches!(&parts[3][..1], "8" | "9" | "a" | "b"), "{id}");
            assert!(seen.insert(id));
        }
    }

    #[test]
    fn only_a_bifrost_provider_is_stamped() {
        let client = reqwest::Client::new();
        let mut p = Provider {
            name: "openai".into(),
            ..Default::default()
        };
        let mut req = client
            .get("http://127.0.0.1:1/x")
            .header(SESSION_HEADER, "ses_1")
            .build()
            .unwrap();
        assert!(stamp(&p, &mut req).is_none());
        assert!(req.headers().get(REQUEST_ID).is_none());
        p.name = ID.into();
        let a = stamp(&p, &mut req).unwrap();
        assert_eq!(req.headers()[REQUEST_ID], a.as_str());
        assert_eq!(req.headers()[CORRELATION_ID], "ses_1");
        // A second stamp replaces the id rather than adding one.
        let b = stamp(&p, &mut req).unwrap();
        assert_ne!(a, b);
        assert_eq!(req.headers().get_all(REQUEST_ID).iter().count(), 1);
        assert_eq!(req.headers()[REQUEST_ID], b.as_str());
    }

    #[test]
    fn only_an_answer_about_the_route_is_a_route_refusal() {
        let api_error = r#"{"error": {"type": "policy_model_blocked", "message": "blocked"}}"#;
        for (status, body, want) in [
            (405, "Method Not Allowed", true),
            (405, api_error, true),
            (
                403,
                r#"{"error":"This hostname serves inference only. Use the admin hostname for administration."}"#,
                true,
            ),
            (403, "<html>Forbidden</html>", true),
            (403, "", true),
            (403, r#"{"message": "forbidden"}"#, true),
            (403, api_error, false),
            (
                403,
                r#"{"type":"error","error":{"type":"permission_error","message":"no"}}"#,
                false,
            ),
            (
                404,
                r#"{"is_bifrost_error":false,"status_code":404,"error":{"message":"Route not found: /v1/x"}}"#,
                true,
            ),
            (404, "404 page not found", true),
            (
                404,
                r#"{"error":{"message":"The model `x` does not exist","code":"model_not_found"}}"#,
                false,
            ),
            (401, "Unauthorized", false),
            (400, "bad", false),
            (500, "<html>", false),
            (429, "", false),
        ] {
            assert_eq!(is_route_refusal(status, body), want, "{status} {body}");
        }
    }

    #[test]
    fn route_refusal_is_decided_by_the_status_and_the_bodys_error_alone() {
        // Bounded: every status from 100 to 599 over a set of bodies.
        let bodies = [
            "",
            "x",
            "{}",
            "[]",
            "null",
            r#"{"error": null}"#,
            r#"{"error": 3}"#,
            r#"{"error": "s"}"#,
            r#"{"error": {}}"#,
            r#"{"error": {"message": "Route not found: /"}}"#,
            r#"{"error": {"message": "route not found"}}"#,
            "\u{0}\u{ffff}",
        ];
        for status in 100..600u16 {
            for b in bodies {
                let got = is_route_refusal(status, b);
                let object = serde_json::from_str::<serde_json::Value>(b)
                    .ok()
                    .is_some_and(|v| v.get("error").is_some_and(|e| e.is_object()));
                let not_found = b.contains(r#""message": "Route not found"#);
                let want = match status {
                    405 => true,
                    403 | 404 => !object || not_found,
                    _ => false,
                };
                assert_eq!(got, want, "{status} {b:?}");
            }
        }
    }

    #[test]
    fn a_route_error_names_the_url_the_message_and_the_setting_never_the_key() {
        let key = "sk-bf-route-secret-1";
        let mut p = Provider {
            name: ID.into(),
            api_key: Some(key.into()),
            protocol: crate::Protocol::OpenaiChat,
            ..Default::default()
        };
        let url = reqwest::Url::parse(&format!(
            "https://u:{key}@bifrost.example/v1/chat/completions?k={key}#f"
        ))
        .unwrap();
        let e = route_error(&p, &url, 403, &format!(r#"{{"error":"no {key} here"}}"#));
        let m = e.to_string();
        assert!(!e.retryable());
        assert!(!m.contains(key) && !format!("{e:?}").contains(key), "{m}");
        assert!(
            m.contains(
                "https://bifrost.example/v1/chat/completions answered HTTP 403 (no [redacted] here)"
            ),
            "{m}"
        );
        assert!(
            m.contains(r#"set providers.bifrost.settings.api to "openai""#),
            "{m}"
        );
        let url = reqwest::Url::parse("https://bifrost.example/openai/v1/responses").unwrap();
        p.protocol = crate::Protocol::OpenaiResponses;
        let m = route_error(&p, &url, 404, "").to_string();
        assert!(
            m.contains("(no message)") && m.contains(r#"set api to "unified""#),
            "{m}"
        );
        p.protocol = crate::Protocol::AnthropicMessages;
        let m = route_error(&p, &url, 404, "").to_string();
        assert!(m.contains("anthropicBaseURL"), "{m}");
        p.protocol = crate::Protocol::GoogleGemini;
        let m = route_error(&p, &url, 405, "").to_string();
        assert!(m.contains("geminiBaseURL"), "{m}");
        // A long or control-laden message is cut and cleaned.
        let m = gateway_message(&format!("{}\n\u{7}end", "a".repeat(1000)), None);
        assert_eq!(m.chars().count(), 300);
        assert!(!m.chars().any(char::is_control));
    }

    #[test]
    fn the_key_is_taken_out_of_errors() {
        let key = "sk-bf-secret-virtual-key";
        assert_eq!(
            redact(&format!("bad key {key} given"), Some(key)),
            "bad key [redacted] given"
        );
        let e = redact_error(
            crate::Error::Status {
                status: 401,
                body: format!("{{\"error\": \"virtual key {key} not found\"}}"),
            },
            Some(key),
        );
        assert!(!e.to_string().contains(key), "{e}");
        let e = redact_error(crate::Error::Invalid(format!("x {key}")), Some(key));
        assert!(!e.to_string().contains(key), "{e}");
        assert!(!format!("{e:?}").contains(key));
    }
}
