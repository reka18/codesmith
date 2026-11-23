//! Which providers have been tested with CodeSmith, and the warning shown
//! for the rest. CodeSmith ships many providers; only these have been run
//! end to end by the project. Everyone else's are marked untested, with a
//! request to confirm one that works (an issue) or fix one that doesn't (a
//! pull request).
//!
//! To mark a provider verified, add its id to [`VERIFIED`] in a pull
//! request that links the issue confirming it.

/// The providers tested with CodeSmith: Anthropic (API key), Claude Code
/// (Claude Pro/Max) and Fireworks.
pub const VERIFIED: &[&str] = &["anthropic", "claude-agent", "fireworks"];

/// The project's repository.
pub const REPO: &str = "https://github.com/reka18/codesmith";

/// The tag shown beside an untested provider.
pub const TAG: &str = "untested";

/// Whether `pid` has been tested with CodeSmith.
pub fn verified(pid: &str) -> bool {
    VERIFIED.contains(&pid)
}

/// The link to open an issue confirming `pid` works, with the template and
/// a title filled in.
pub fn issue_url(pid: &str) -> String {
    format!(
        "{REPO}/issues/new?template=provider-verified.md&title={}",
        encode(&format!("Provider verified: {pid}"))
    )
}

/// The warning for an untested provider: `None` for a verified one.
pub fn warning(pid: &str, name: &str) -> Option<String> {
    if verified(pid) {
        return None;
    }
    Some(format!(
        "{name} hasn't been tested with CodeSmith yet, so it may not work as expected. \
If it works for you, please confirm it in an issue: {}. \
If it doesn't, a pull request that fixes it, following the project's patterns, \
is welcome: see {REPO}/blob/main/CONTRIBUTING.md.",
        issue_url(pid)
    ))
}

/// Percent-encode everything but unreserved characters (RFC 3986), so any
/// provider id, a custom one too, makes a safe query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_tested_providers_are_verified() {
        for p in ["anthropic", "claude-agent", "fireworks"] {
            assert!(verified(p), "{p}");
            assert_eq!(warning(p, p), None);
        }
        for p in [
            "openai",
            "chatgpt",
            "google",
            "openrouter",
            "groq",
            "deepseek",
            "mistral",
            "xai",
            "together",
            "cerebras",
            "poe",
            "copilot",
            "ollama",
            "lmstudio",
            "my-custom",
        ] {
            assert!(!verified(p), "{p}");
            let w = warning(p, "Some Name").unwrap();
            assert!(w.starts_with("Some Name hasn't been tested"), "{w}");
            assert!(w.contains(&issue_url(p)), "{w}");
            assert!(w.contains("CONTRIBUTING.md"), "{w}");
        }
    }

    #[test]
    fn near_misses_are_not_verified() {
        for p in [
            "",
            "Anthropic",
            "anthropic ",
            " fireworks",
            "claude",
            "fireworks-ai",
            "claude-agent/x",
        ] {
            assert!(!verified(p), "{p:?}");
        }
    }

    #[test]
    fn the_issue_link_names_the_template_and_the_provider() {
        assert_eq!(
            issue_url("groq"),
            "https://github.com/reka18/codesmith/issues/new?template=provider-verified.md&title=Provider%20verified%3A%20groq"
        );
    }

    #[test]
    fn any_provider_id_makes_a_safe_link() {
        let u = issue_url("a b&c=d#e?f/g\u{e9}\n");
        let query = u.split_once("&title=").unwrap().1;
        assert!(
            query
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.~%".contains(&b)),
            "{query}"
        );
        assert!(query.ends_with("%C3%A9%0A"), "{query}");
    }

    #[test]
    fn encoding_is_reversible_for_many_inputs() {
        // A bounded property check: decoding the encoding gives back the input.
        let mut seed: u32 = 0x9e37_79b9;
        for _ in 0..2000 {
            let len = (seed % 24) as usize;
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                bytes.push((seed % 128) as u8);
            }
            let s = String::from_utf8(bytes).unwrap();
            let e = encode(&s);
            let mut back = Vec::new();
            let mut it = e.bytes();
            while let Some(b) = it.next() {
                if b == b'%' {
                    let h = [it.next().unwrap(), it.next().unwrap()];
                    back.push(u8::from_str_radix(std::str::from_utf8(&h).unwrap(), 16).unwrap());
                } else {
                    back.push(b);
                }
            }
            assert_eq!(back, s.as_bytes());
        }
    }
}
