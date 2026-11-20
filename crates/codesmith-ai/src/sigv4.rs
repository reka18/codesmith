//! AWS Signature Version 4, for Amazon Bedrock (after `aws4fetch`, which
//! OpenCode signs Bedrock requests with): the canonical request over the
//! exact method, path, query, headers and body bytes, a string to sign
//! scoped to the date, region and service, and the derived signing key.

use sha2::{Digest, Sha256};

/// AWS credentials for signing. `Debug` leaves the secrets out.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"…")
            .field("session_token", &self.session_token.as_ref().map(|_| "…"))
            .finish()
    }
}

pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// HMAC-SHA256 (RFC 2104).
pub(crate) fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new()
        .chain_update(ipad)
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(opad)
        .chain_update(inner)
        .finalize()
        .into()
}

/// The signing key for a day (`YYYYMMDD`), region and service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// RFC 3986 percent-encoding as SigV4 wants it: everything but the
/// unreserved characters, and `/` too unless `keep_slash`.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The canonical URI of a request path (as sent, already percent-encoded
/// once): repeated slashes collapsed, dot segments resolved, and every
/// segment encoded again, as every service but S3 expects.
fn canonical_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let mut out = String::from("/");
    out.push_str(
        &segments
            .iter()
            .map(|s| uri_encode(s, false))
            .collect::<Vec<_>>()
            .join("/"),
    );
    if path.ends_with('/') && !segments.is_empty() {
        out.push('/');
    }
    out
}

fn canonical_query(url: &reqwest::Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (uri_encode(&k, false), uri_encode(&v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Headers that are never signed: they change in transit or are added by
/// the client after signing.
const UNSIGNED: [&str; 8] = [
    "authorization",
    "content-length",
    "user-agent",
    "connection",
    "expect",
    "presigned-expires",
    "range",
    "x-amzn-trace-id",
];

/// The `host` header the client will send for `url`.
fn host(url: &reqwest::Url) -> String {
    let h = url.host_str().unwrap_or_default();
    match url.port() {
        Some(p) => format!("{h}:{p}"),
        None => h.to_string(),
    }
}

/// The headers to add to a request to sign it: `x-amz-date`, any
/// `x-amz-security-token`, and `authorization`. `headers` are the
/// request's own (every one but [`UNSIGNED`] is signed); `amz_date` is
/// `YYYYMMDDTHHMMSSZ`.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    method: &str,
    url: &reqwest::Url,
    headers: &[(String, String)],
    body: &[u8],
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
) -> Vec<(String, String)> {
    let date = &amz_date[..8.min(amz_date.len())];
    let mut added = vec![("x-amz-date".to_string(), amz_date.to_string())];
    if let Some(t) = creds.session_token.as_deref().filter(|t| !t.is_empty()) {
        added.push(("x-amz-security-token".to_string(), t.to_string()));
    }
    let mut canon: Vec<(String, String)> = vec![("host".into(), host(url))];
    for (k, v) in headers.iter().chain(added.iter()) {
        let k = k.to_ascii_lowercase();
        if UNSIGNED.contains(&k.as_str()) || k == "host" {
            continue;
        }
        let v = v.split_whitespace().collect::<Vec<_>>().join(" ");
        match canon.iter_mut().find(|(n, _)| *n == k) {
            Some(slot) => {
                slot.1.push(',');
                slot.1.push_str(&v);
            }
            None => canon.push((k, v)),
        }
    }
    canon.sort();
    let signed: String = canon
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = canon.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let request = format!(
        "{method}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        canonical_path(url.path()),
        canonical_query(url),
        sha256_hex(body),
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(request.as_bytes())
    );
    let key = signing_key(&creds.secret_access_key, date, region, service);
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    added.push((
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            creds.access_key_id
        ),
    ));
    added
}

/// `YYYYMMDDTHHMMSSZ` for a time.
pub fn amz_date(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// Sign a built request in place (its body must be in memory).
pub fn sign_request(
    req: &mut reqwest::Request,
    creds: &Credentials,
    region: &str,
    service: &str,
    now: std::time::SystemTime,
) {
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let body = req
        .body()
        .and_then(|b| b.as_bytes())
        .unwrap_or_default()
        .to_vec();
    let added = sign(
        req.method().as_str(),
        req.url(),
        &headers,
        &body,
        creds,
        region,
        service,
        &amz_date(now),
    );
    for (k, v) in added {
        if let (Ok(k), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(&v),
        ) {
            req.headers_mut().insert(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // From the AWS Signature Version 4 test suite and documentation.
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    fn example() -> Credentials {
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: SECRET.into(),
            session_token: None,
        }
    }

    fn auth(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> String {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let added = sign(
            method,
            &reqwest::Url::parse(url).unwrap(),
            &headers,
            body,
            &example(),
            "us-east-1",
            "service",
            "20150830T123600Z",
        );
        added
            .into_iter()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1
    }

    #[test]
    fn derives_the_documented_signing_key() {
        assert_eq!(
            hex(&signing_key(SECRET, "20150830", "us-east-1", "iam")),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2.
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block.
        assert_eq!(
            hex(&hmac(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn signs_the_test_suite_vanilla_requests() {
        assert_eq!(
            auth("GET", "https://example.amazonaws.com/", &[], b""),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert!(
            auth("POST", "https://example.amazonaws.com/", &[], b"").ends_with(
                "Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
            )
        );
    }

    #[test]
    fn sorts_query_parameters() {
        assert!(auth(
            "GET",
            "https://example.amazonaws.com/?Param2=value2&Param1=value1",
            &[],
            b""
        )
        .ends_with("Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"));
    }

    #[test]
    fn signs_request_headers_but_not_the_unsignable_ones() {
        let a = auth(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/converse-stream",
            &[
                ("Content-Type", "application/json"),
                ("User-Agent", "codesmith"),
                ("X-Custom", "  a   b "),
            ],
            b"{}",
        );
        assert!(
            a.contains("SignedHeaders=content-type;host;x-amz-date;x-custom,"),
            "{a}"
        );
    }

    #[test]
    fn encodes_path_segments_a_second_time() {
        // A model id's `:` is sent as `%3A` and signed as `%253A`.
        assert_eq!(
            canonical_path("/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse-stream"),
            "/model/anthropic.claude-3-5-sonnet-20240620-v1%253A0/converse-stream"
        );
        assert_eq!(canonical_path("//a/./b/../c/"), "/a/c/");
        assert_eq!(canonical_path(""), "/");
    }

    #[test]
    fn adds_the_session_token_and_signs_it() {
        let mut creds = example();
        creds.session_token = Some("SESSIONTOKEN".into());
        let added = sign(
            "POST",
            &reqwest::Url::parse("https://h.example:8443/p").unwrap(),
            &[],
            b"",
            &creds,
            "eu-west-1",
            "bedrock",
            "20250101T000000Z",
        );
        assert_eq!(
            added[1],
            ("x-amz-security-token".into(), "SESSIONTOKEN".into())
        );
        assert!(added[2]
            .1
            .contains("/20250101/eu-west-1/bedrock/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token,"));
        assert!(!format!("{creds:?}").contains(SECRET));
        assert!(!format!("{creds:?}").contains("SESSIONTOKEN"));
    }

    #[test]
    fn formats_the_amz_date() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_440_938_160);
        assert_eq!(amz_date(t), "20150830T123600Z");
    }
}
