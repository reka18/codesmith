//! The local bus's wire protocol: one JSON object per line, at most
//! [`MAX_FRAME`] bytes. A client's first line is a [`Hello`] (or a
//! [`Request::Shutdown`]); after the broker's welcome it sends
//! [`Envelope`]s, each with an id the broker's [`Reply`] carries back, and
//! the broker also pushes [`Frame::Deliver`] whenever a message arrives.
//!
//! `Hello` and `Shutdown` never change shape, so a newer CodeSmith can
//! always ask an older broker to make way, whatever else changed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped by any change an older side can't read.
pub const PROTOCOL: u32 = 1;
/// The longest line either side sends or accepts.
pub const MAX_FRAME: usize = 256 * 1024;
/// The longest message body.
pub const MAX_BODY: usize = 64 * 1024;
/// A request sent from a turn that a chain of this many requests started
/// is refused.
pub const MAX_HOPS: u32 = 4;
/// The longest session name.
pub const MAX_NAME: usize = 48;

/// What a broker publishes in its registration file (0600).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub pid: u32,
    pub protocol: u32,
    pub version: String,
    pub socket: String,
    pub token: String,
    /// Unix milliseconds.
    pub started: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Request,
    Progress,
    Response,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Request => "request",
            Kind::Progress => "progress",
            Kind::Response => "response",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "request" => Some(Kind::Request),
            "progress" => Some(Kind::Progress),
            "response" => Some(Kind::Response),
            _ => None,
        }
    }
}

/// What joins: a session, a relay carrying one tailbus message, or an
/// observer (`codesmith bus peers`) that only looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Session,
    Relay,
    Observer,
}

/// A member as it introduces itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub role: Role,
    /// The session id (a relay's or observer's own, made up).
    pub session: String,
    /// The name asked for with `/bus name`; otherwise the broker picks one
    /// from `title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub busy: bool,
    /// Whether requests are delivered to it (`codesmith run` takes only
    /// replies to what it sent; requests wait for the session to open).
    #[serde(default)]
    pub accepts: bool,
}

/// A live session as peers see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub session: String,
    pub project: String,
    pub agent: String,
    pub model: String,
    pub busy: bool,
    pub accepts: bool,
    pub paused: bool,
    /// Unix milliseconds.
    pub joined: i64,
    /// The name it's shared to relays as, if it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
}

/// One message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub kind: Kind,
    /// Sender's session id and name.
    pub from: String,
    pub from_name: String,
    /// Recipient's session id and name.
    pub to: String,
    pub to_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub body: String,
    /// Unix milliseconds.
    pub sent: i64,
    #[serde(default)]
    pub hop: u32,
    /// The sender's project folder.
    #[serde(default)]
    pub project: String,
    /// Where a relayed request came from (`tailbus: crucible@stable-mac`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub op: String,
    pub protocol: u32,
    pub version: String,
    pub token: String,
    pub member: Member,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Change what peers see.
    Update {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        busy: Option<bool>,
    },
    Send {
        to: String,
        kind: Kind,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        #[serde(default)]
        hop: u32,
    },
    Peers,
    /// Messages to this session not yet handled; an observer names one.
    Inbox {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// Mark a message to this session handled.
    Ack {
        id: String,
    },
    /// Stop or start taking new requests.
    Pause {
        on: bool,
    },
    /// Open this session to relays as `name`.
    Share {
        name: String,
        /// Where the sharing process keeps its config, keys and sessions
        /// ([`SHARE_ENV`]), so a relay's headless run continues the
        /// session as it would.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        env: std::collections::BTreeMap<String, String>,
    },
    Unshare,
    /// Relay only: carry a message to the session shared as `share`. `key`
    /// (the tailbus message id) makes a repeat find the first.
    Relay {
        share: String,
        body: String,
        via: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Stop the broker (a newer CodeSmith taking over). Sent as a first
    /// line with `token`, or later on a welcomed connection.
    Shutdown {
        #[serde(default)]
        token: String,
        #[serde(default)]
        version: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub rid: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub rid: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the broker sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    Welcome {
        name: String,
        version: String,
        protocol: u32,
    },
    Reply(Reply),
    Deliver {
        message: Message,
    },
    /// The connection is refused or ends; why.
    Bye {
        reason: String,
    },
}

/// What answering a relay says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relayed {
    /// The request's id, when it was delivered or queued for a live session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Whether the shared session is running now.
    pub live: bool,
    pub session: String,
    pub project: String,
    pub name: String,
    /// Where the sharing process kept its folders ([`SHARE_ENV`]).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}

/// The variables a share records: where CodeSmith keeps config, keys and
/// sessions. Only these, and only absolute paths, are kept or applied.
pub const SHARE_ENV: [&str; 6] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "CODESMITH_DB",
];

/// `env` cut to [`SHARE_ENV`] names with absolute values.
pub fn share_env(
    env: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    env.iter()
        .filter(|(k, v)| {
            SHARE_ENV.contains(&k.as_str())
                && std::path::Path::new(v.as_str()).is_absolute()
                && !v.contains('\0')
                && v.len() < 4096
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Unix milliseconds now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A new message id: a ULID (48 bits of milliseconds, 80 random bits, in
/// Crockford base 32), so ids sort in time order.
pub fn new_id() -> String {
    use rand::RngCore;
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let ms = now_ms().max(0) as u128 & ((1u128 << 48) - 1);
    let mut r = [0u8; 10];
    rand::thread_rng().fill_bytes(&mut r);
    let mut v = ms << 80;
    for (i, b) in r.iter().enumerate() {
        v |= (*b as u128) << (72 - 8 * i);
    }
    (0..26)
        .rev()
        .map(|i| ALPHABET[((v >> (5 * i)) & 31) as usize] as char)
        .collect()
}

/// Whether `name` is a session name: lowercase letters, digits and
/// dashes, starting with a letter, at most [`MAX_NAME`].
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// A session name from a title: lowercase words joined by dashes, cut at
/// a word boundary to fit; `session` when nothing is left.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    for word in title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let w = word.to_ascii_lowercase();
        let extra = if out.is_empty() { w.len() } else { w.len() + 1 };
        if out.len() + extra > 32 {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(&w);
    }
    let trimmed = out.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
    if trimmed.is_empty() {
        "session".into()
    } else {
        trimmed.to_string()
    }
}

/// Whether version `a` is newer than `b` (dotted numbers; anything that
/// isn't a number counts as 0).
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(a) > parse(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_ulids_in_time_order() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id();
        assert_eq!(a.len(), 26);
        assert!(a < b, "{a} {b}");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
        let many: std::collections::HashSet<String> = (0..2000).map(|_| new_id()).collect();
        assert_eq!(many.len(), 2000);
    }

    #[test]
    fn names_are_plain() {
        for ok in ["reviewer", "fix-login", "a1", "x-2"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "Reviewer",
            "-x",
            "x-",
            "a--b",
            "1abc",
            "a b",
            "a/b",
            "a.b",
            "é",
            &"a".repeat(MAX_NAME + 1),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn slugs_are_valid_names() {
        assert_eq!(slug("Fix the login page"), "fix-the-login-page");
        assert_eq!(slug("  ##  "), "session");
        assert_eq!(slug("2026 roadmap"), "roadmap");
        assert_eq!(slug("Ünïcode only"), "n-code-only");
        let long = slug("one two three four five six seven eight nine ten");
        assert!(long.len() <= 32, "{long}");
        // Bounded property: any title gives a valid name.
        let alphabet = ['a', 'Z', '9', ' ', '-', '_', '.', 'é', '/', '#'];
        let mut seed = 7u64;
        for _ in 0..3000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (seed >> 33) as usize % 60;
            let t: String = (0..len)
                .map(|i| alphabet[((seed >> (i % 50)) as usize + i) % alphabet.len()])
                .collect();
            let s = slug(&t);
            assert!(valid_name(&s), "{t:?} -> {s:?}");
        }
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("0.5.0", "0.4.10"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.4.1", "0.4.1"));
        assert!(!newer("0.4.0", "0.4.1"));
        assert!(newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn frames_round_trip() {
        let m = Message {
            id: new_id(),
            kind: Kind::Request,
            from: "s1".into(),
            from_name: "a".into(),
            to: "s2".into(),
            to_name: "b".into(),
            reply_to: None,
            body: "hi\nthere \u{202e}".into(),
            sent: 1,
            hop: 2,
            project: "/p".into(),
            via: Some("tailbus: x@y".into()),
        };
        let f = Frame::Deliver { message: m.clone() };
        let line = serde_json::to_string(&f).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(serde_json::from_str::<Frame>(&line).unwrap(), f);
        let e = Envelope {
            rid: 9,
            request: Request::Send {
                to: "b".into(),
                kind: Kind::Response,
                body: "ok".into(),
                reply_to: Some(m.id.clone()),
                hop: 0,
            },
        };
        let line = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Envelope>(&line).unwrap(), e);
    }
}
