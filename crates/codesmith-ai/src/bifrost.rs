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
