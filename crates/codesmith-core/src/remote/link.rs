//! The pairing link, the pairing code and the check code.
//!
//! The link is
//! `codesmith://pair?v=1&n=<node id>&r=<relay URL>&a=<addresses>&m=<machine>&s=<session>&c=<code>`,
//! every value percent-encoded with only RFC 3986's unreserved characters
//! left as they are; `r`, `a` and `s` are optional. [`Link::parse`] reads
//! links as the phone apps do (their published `pairing.json` vectors), so
//! the tests can check that what [`Link::url`] writes reads back the same.

use base64::Engine;
use sha2::{Digest, Sha256};

/// What a pairing link carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The machine's node id, lower-case hex.
    pub node_id: String,
    /// Its home relay, when it has one.
    pub relay: Option<String>,
    /// Direct addresses (`ip:port`, IPv6 in brackets).
    pub addrs: Vec<String>,
    /// The machine's name.
    pub machine: String,
    /// The session the pairing grants, if one.
    pub session: Option<String>,
    /// The pairing code.
    pub code: String,
}

/// Why a link doesn't parse, as the apps' vectors name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    NotPairingUrl,
    UnsupportedVersion,
    Missing,
    Invalid,
}

impl LinkError {
    pub fn name(self) -> &'static str {
        match self {
            LinkError::NotPairingUrl => "notPairingUrl",
            LinkError::UnsupportedVersion => "unsupportedVersion",
            LinkError::Missing => "missing",
            LinkError::Invalid => "invalid",
        }
    }
}

/// `s` percent-encoded, leaving only RFC 3986 unreserved characters.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-decoding; `None` for a bad escape or text that isn't UTF-8.
/// `+` is a plus, not a space.
fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = b.get(i + 1..i + 3)?;
            let h = std::str::from_utf8(h).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A pairing code's characters: base64url without padding.
fn code_ok(c: &str) -> bool {
    (16..=128).contains(&c.len())
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A session id's characters.
fn session_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// `s` without control characters or bidirectional controls, which could
/// make a name show as something else in a terminal.
pub fn printable(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c,
                    '\u{200e}' | '\u{200f}' | '\u{061c}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

impl Link {
    /// The `codesmith://pair?…` link.
    pub fn url(&self) -> String {
        let mut q = vec![("v", "1".to_string()), ("n", self.node_id.clone())];
        if let Some(r) = self.relay.as_ref().filter(|r| !r.is_empty()) {
            q.push(("r", r.clone()));
        }
        if !self.addrs.is_empty() {
            q.push(("a", self.addrs.join(",")));
        }
        q.push(("m", self.machine.clone()));
        if let Some(s) = self.session.as_ref().filter(|s| !s.is_empty()) {
            q.push(("s", s.clone()));
        }
        q.push(("c", self.code.clone()));
        let query: Vec<String> = q
            .into_iter()
            .map(|(k, v)| format!("{k}={}", encode(&v)))
            .collect();
        format!("codesmith://pair?{}", query.join("&"))
    }

    /// Read a link as the phone apps do.
    pub fn parse(text: &str) -> Result<Link, LinkError> {
        let t = text.trim();
        let t = t.split('#').next().unwrap_or("");
        let Some((head, query)) = t.split_once('?') else {
            return Err(LinkError::NotPairingUrl);
        };
        let head = head.to_ascii_lowercase();
        if head != "codesmith://pair" && head != "codesmith://pair/" {
            return Err(LinkError::NotPairingUrl);
        }
        let mut fields: std::collections::HashMap<&str, String> = Default::default();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if !["v", "n", "r", "a", "m", "s", "c"].contains(&k) {
                continue;
            }
            let v = decode(v).ok_or(LinkError::Invalid)?;
            if fields.insert(k, v).is_some() {
                return Err(LinkError::Invalid);
            }
        }
        let get = |k: &str| fields.get(k).map(|s| s.as_str()).filter(|s| !s.is_empty());
        match get("v") {
            None => return Err(LinkError::Missing),
            Some("1") => {}
            Some(_) => return Err(LinkError::UnsupportedVersion),
        }
        let node_id = get("n").ok_or(LinkError::Missing)?.to_ascii_lowercase();
        if !super::key::is_node_id(&node_id) {
            return Err(LinkError::Invalid);
        }
        let machine = printable(get("m").ok_or(LinkError::Missing)?);
        if machine.is_empty() {
            return Err(LinkError::Missing);
        }
        let code = get("c").ok_or(LinkError::Missing)?.to_string();
        if !code_ok(&code) {
            return Err(LinkError::Invalid);
        }
        let relay = match get("r") {
            None => None,
            Some(r) => {
                let u = url::Url::parse(r).map_err(|_| LinkError::Invalid)?;
                if !matches!(u.scheme(), "http" | "https") || u.host().is_none() {
                    return Err(LinkError::Invalid);
                }
                Some(r.to_string())
            }
        };
        let addrs = match get("a") {
            None => vec![],
            Some(a) => a
                .split(',')
                .map(|s| {
                    s.parse::<std::net::SocketAddr>()
                        .map(|a| a.to_string())
                        .map_err(|_| LinkError::Invalid)
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        let session = match get("s") {
            None => None,
            Some(s) if session_ok(s) => Some(s.to_string()),
            Some(_) => return Err(LinkError::Invalid),
        };
        Ok(Link {
            node_id,
            relay,
            addrs,
            machine,
            session,
            code,
        })
    }
}

/// A new pairing code: 16 bytes from the OS RNG, as base64url without
/// padding (22 characters).
pub fn new_code() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The 12-digit check code both screens show, as `ddd ddd ddd ddd`: the
/// first 8 bytes of SHA-256 over
/// `"codesmith-pair-check/1\n" + code + "\n" + machine + "\n" + device`
/// (node ids lower-cased), as a big-endian integer mod 10^12.
pub fn check_code(code: &str, machine: &str, device: &str) -> String {
    let msg = format!(
        "codesmith-pair-check/1\n{code}\n{}\n{}",
        machine.to_ascii_lowercase(),
        device.to_ascii_lowercase()
    );
    let h = Sha256::digest(msg.as_bytes());
    let mut first = [0u8; 8];
    first.copy_from_slice(&h[..8]);
    let n = u64::from_be_bytes(first) % 1_000_000_000_000;
    let d = format!("{n:012}");
    format!("{} {} {} {}", &d[0..3], &d[3..6], &d[6..9], &d[9..12])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const MACHINE: &str = "aa980150be84faa3788d7d170f02984f084bcaeb6871fe1fe7805ff2505a375c";

    /// The app team's `transport/fixtures/pairing.json` (link vectors), as
    /// published with the 12-digit check codes of P11.
    fn fixture() -> Value {
        serde_json::from_str(include_str!("testdata/pairing.json")).unwrap()
    }

    #[test]
    fn the_check_code_matches_the_apps_vectors() {
        assert_eq!(
            check_code(
                "pn00cOpwBxfLmPR62H1EaQ",
                MACHINE,
                "f8820d751e90d88eca3b848440efc98ae226f1552a6fb5c76c7ad09c96832dc6"
            ),
            "779 852 318 371"
        );
        assert_eq!(
            check_code(
                "pn00cOpwBxfLmPR62H1EaQ",
                MACHINE,
                "2862abb12e90d88eca3b848440efc98ae226f1552a6fb5c76c7ad09c96832dc6"
            ),
            "425 748 398 457"
        );
        assert_eq!(
            check_code(
                "0123456789abcdef_-xy",
                MACHINE,
                &format!("65b2276e47{}", "0".repeat(54))
            ),
            "203 664 825 594"
        );
        for v in fixture()["checkCodes"].as_array().unwrap() {
            let got = check_code(
                v["code"].as_str().unwrap(),
                v["machine"].as_str().unwrap(),
                v["device"].as_str().unwrap(),
            );
            assert_eq!(got, v["expect"].as_str().unwrap(), "{v}");
        }
    }

    #[test]
    fn the_check_code_ignores_the_case_of_node_ids() {
        let d = "f8820d751e90d88eca3b848440efc98ae226f1552a6fb5c76c7ad09c96832dc6";
        assert_eq!(
            check_code(
                "pn00cOpwBxfLmPR62H1EaQ",
                &MACHINE.to_uppercase(),
                &d.to_uppercase()
            ),
            check_code("pn00cOpwBxfLmPR62H1EaQ", MACHINE, d)
        );
    }

    #[test]
    fn every_valid_vector_parses_and_round_trips() {
        let f = fixture();
        for v in f["valid"].as_array().unwrap() {
            let url = v["url"].as_str().unwrap();
            let l = Link::parse(url).unwrap_or_else(|e| panic!("{url}: {e:?}"));
            let e = &v["expect"];
            assert_eq!(l.node_id, e["nodeId"].as_str().unwrap(), "{url}");
            assert_eq!(l.relay.as_deref(), e["relayUrl"].as_str(), "{url}");
            let addrs: Vec<&str> = e["addrs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            assert_eq!(l.addrs, addrs, "{url}");
            assert_eq!(l.machine, e["machineName"].as_str().unwrap(), "{url}");
            assert_eq!(l.session.as_deref(), e["session"].as_str(), "{url}");
            assert_eq!(l.code, e["code"].as_str().unwrap(), "{url}");
            // What the builder writes from it reads back the same.
            let again = l.url();
            assert_eq!(Link::parse(&again).unwrap(), l, "{url} -> {again}");
            assert!(again.starts_with("codesmith://pair?v=1&n="), "{again}");
        }
    }

    #[test]
    fn every_invalid_vector_is_refused_for_its_reason() {
        let f = fixture();
        for v in f["invalid"].as_array().unwrap() {
            let url = v["url"].as_str().unwrap();
            let got = Link::parse(url).map(|_| ()).map_err(LinkError::name);
            assert_eq!(got, Err(v["error"].as_str().unwrap()), "{url}");
        }
    }

    #[test]
    fn the_builder_uses_unreserved_characters_only() {
        let l = Link {
            node_id: MACHINE.into(),
            relay: Some("https://relay-us.codesmith.skepticdogsoftware.com/".into()),
            addrs: vec!["100.72.137.44:61823".into(), "[2001:db8::20]:51820".into()],
            machine: "Ray’s Mac+Pro & co".into(),
            session: Some("ses_1a21547aca5e297de".into()),
            code: new_code(),
        };
        let url = l.url();
        let query = url.split_once('?').unwrap().1;
        for part in query.split('&') {
            let (_, v) = part.split_once('=').unwrap();
            assert!(
                v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~%".contains(&b)),
                "{v}"
            );
        }
        assert!(
            url.contains("&m=Ray%E2%80%99s%20Mac%2BPro%20%26%20co&"),
            "{url}"
        );
        assert!(
            url.contains("&a=100.72.137.44%3A61823%2C%5B2001%3Adb8%3A%3A20%5D%3A51820&"),
            "{url}"
        );
        assert_eq!(Link::parse(&url).unwrap(), l);
        // Optional values left out.
        let bare = Link {
            relay: None,
            addrs: vec![],
            session: None,
            ..l.clone()
        };
        let u = bare.url();
        assert!(
            !u.contains("&r=") && !u.contains("&a=") && !u.contains("&s="),
            "{u}"
        );
        assert_eq!(Link::parse(&u).unwrap(), bare);
    }

    #[test]
    fn codes_are_22_base64url_characters_and_differ() {
        let a = new_code();
        let b = new_code();
        assert_eq!(a.len(), 22);
        assert!(code_ok(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn names_lose_control_and_bidi_characters() {
        assert_eq!(
            printable("Ray\u{1b}[2J's\u{7} \u{202e}enohP"),
            "Ray[2J's enohP"
        );
        assert_eq!(printable("a\u{2066}b\u{200f}c\nd"), "abcd");
    }

    /// Bounded property test: any link the builder writes, from any
    /// printable machine name and valid parts, parses back to itself.
    #[test]
    fn any_built_link_round_trips() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let pool: Vec<char> = "aZ09 -._~!*'();:@&=+$,/?#[]%\"<>\\^`{|}’é漢🙂"
            .chars()
            .collect();
        for _ in 0..500 {
            let len = rng.gen_range(1..20);
            let machine: String = (0..len)
                .map(|_| pool[rng.gen_range(0..pool.len())])
                .collect();
            let machine = printable(&machine);
            if machine.is_empty() {
                continue;
            }
            let addrs = (0..rng.gen_range(0..4))
                .map(|i| {
                    if rng.gen_bool(0.5) {
                        format!("10.0.0.{i}:{}", rng.gen_range(1..65535))
                    } else {
                        format!("[fd00::{}]:{}", i + 1, rng.gen_range(1..65535))
                    }
                })
                .collect();
            let node: String = (0..64)
                .map(|_| char::from_digit(rng.gen_range(0..16), 16).unwrap())
                .collect();
            let l = Link {
                node_id: node,
                relay: rng
                    .gen_bool(0.5)
                    .then(|| "https://relay.example.com/".to_string()),
                addrs,
                machine,
                session: rng
                    .gen_bool(0.5)
                    .then(|| format!("ses_{}", rng.gen::<u32>())),
                code: new_code(),
            };
            assert_eq!(Link::parse(&l.url()).unwrap(), l, "{}", l.url());
        }
    }
}
