//! Claude Code tokens from `claude setup-token`: a one-year token for the
//! user's Claude subscription that Claude Code reads from
//! `CLAUDE_CODE_OAUTH_TOKEN` instead of its keychain login, so it works
//! where the keychain is locked (an SSH session). It can only make model
//! requests.
//!
//! CodeSmith runs `claude setup-token` on the user's terminal under
//! `script`, which records the session to a file in a private folder, and
//! takes the token from between the lines Claude Code prints around it.
//! The record is removed at once. When no token can be found there, the
//! user pastes it instead.

use std::path::Path;

/// The variable Claude Code reads the token from.
pub const VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// How long a token lasts, as Claude Code says when it makes one.
pub const LIFETIME_MS: u64 = 365 * 24 * 60 * 60 * 1000;

/// Warn this long before a token expires.
pub const WARN_BEFORE_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// The line Claude Code prints before the token.
const BEFORE: &str = "Your OAuth token";
/// The line Claude Code prints after it.
const AFTER: &str = "Store this token securely";

const MIN_LEN: usize = 40;
const MAX_LEN: usize = 512;

/// `s` without terminal escape sequences (colours, cursor moves, titles).
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one final byte in @..~.
            Some('[') => {
                for d in chars.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ST.
            Some(']') => {
                while let Some(d) = chars.next() {
                    if d == '\x07' {
                        break;
                    }
                    if d == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// A token as typed or pasted: whitespace and line breaks (a wrapped
/// paste) removed, then checked to look like one.
pub fn validate(input: &str) -> Result<String, String> {
    let t: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if t.is_empty() {
        return Err("No token: paste the token `claude setup-token` printed.".into());
    }
    if !t.starts_with("sk-ant-") {
        return Err("That isn't a Claude Code token: it starts with sk-ant-.".into());
    }
    if !t.chars().all(token_char) {
        return Err("That isn't a Claude Code token: it has characters a token doesn't.".into());
    }
    if !(MIN_LEN..=MAX_LEN).contains(&t.len()) {
        return Err("That isn't a whole Claude Code token: check it was copied in full.".into());
    }
    Ok(t)
}

/// The token in a record of `claude setup-token`'s session: the lines
/// between the one naming it and the one after it, joined (the terminal
/// may have wrapped it), when that is a valid token.
pub fn parse(record: &str) -> Option<String> {
    let text = strip_escapes(record).replace('\r', "\n");
    // The last run, so an earlier failed attempt in the same record is
    // passed over.
    let start = text.rfind(BEFORE)?;
    let after_label = &text[start..];
    let body_start = after_label.find('\n')? + 1;
    let body = &after_label[body_start..];
    let end = body.find(AFTER)?;
    let joined: String = body[..end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    validate(&joined).ok()
}

/// `claude setup-token` run under `script`, recording to `record`, with
/// the terminal handed to it. `None` where there is no `script` to use.
pub fn command(claude: &Path, record: &Path) -> Option<std::process::Command> {
    #[cfg(target_os = "macos")]
    {
        let mut c = std::process::Command::new("/usr/bin/script");
        c.arg("-q").arg(record).arg(claude).arg("setup-token");
        Some(c)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let script = ["/usr/bin/script", "/bin/script"]
            .into_iter()
            .find(|p| Path::new(p).exists())?;
        // util-linux takes the command as one shell string.
        let quoted = format!(
            "'{}' setup-token",
            claude.display().to_string().replace('\'', r"'\''")
        );
        let mut c = std::process::Command::new(script);
        c.arg("-q").arg("-e").arg("-c").arg(quoted).arg(record);
        Some(c)
    }
    #[cfg(not(unix))]
    {
        let _ = (claude, record);
        None
    }
}

/// When a token made at `created_ms` expires.
pub fn expires(created_ms: u64) -> u64 {
    created_ms.saturating_add(LIFETIME_MS)
}

/// Whether a token made at `created_ms` is within [`WARN_BEFORE_MS`] of
/// expiring (or past it) at `now_ms`.
pub fn expiring(created_ms: u64, now_ms: u64) -> bool {
    expires(created_ms).saturating_sub(WARN_BEFORE_MS) <= now_ms
}

#[cfg(test)]
#[path = "token_tests.rs"]
mod tests;
