//! Session mentions, as OpenCode's `util/session-mention`: a session ID
//! (`ses_…`), alone or as `@session:ses_…`, in text. Only whole generated
//! IDs count, so one still streaming in, or one inside a path, a URL or a
//! longer word, is not a mention. CodeSmith's IDs have 17 characters after
//! `ses_`; OpenCode's (in sessions imported from it) have 26.
//!
//! The transcript shows a mention of another session as a link titled
//! after it, `@Fix the build (ses_…)` (OpenCode shows the title and opens
//! the session on a click; CodeSmith has no mouse, so the ID stays in view
//! to resume or open it).

use std::borrow::Cow;

const PREFIX: &str = "@session:";

/// One mention: the ID, the matched text and its byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention<'a> {
    pub id: &'a str,
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
}

fn word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The characters a mention may not follow.
fn joins_before(c: char) -> bool {
    word(c) || matches!(c, '/' | ':' | '@' | '-')
}

/// The characters a mention may not precede.
fn joins_after(c: char) -> bool {
    word(c) || matches!(c, '/' | '-')
}

/// Every mention in `text`, in order.
pub fn session_mentions(text: &str) -> Vec<Mention<'_>> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("ses_") {
        let i = from + at;
        from = i + 4;
        let rest = &text[i + 4..];
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        if len != 17 && len != 26 {
            continue;
        }
        let end = i + 4 + len;
        if text[end..].chars().next().is_some_and(joins_after) {
            continue;
        }
        let before = |at: usize| text[..at].chars().next_back();
        let start = if text[..i].ends_with(PREFIX) {
            i - PREFIX.len()
        } else {
            i
        };
        if before(start).is_some_and(joins_before) {
            continue;
        }
        out.push(Mention {
            id: &text[i..end],
            text: &text[start..end],
            start,
            end,
        });
        from = end;
    }
    out
}

/// The session `text` is exactly a mention of, if it is one.
pub fn parse_session_mention(text: &str) -> Option<&str> {
    match session_mentions(text).first() {
        Some(m) if m.text == text => Some(m.id),
        _ => None,
    }
}

/// The token that mentions session `id` (nothing writes one yet).
#[cfg(test)]
pub fn serialize_session_mention(id: &str) -> String {
    format!("{PREFIX}{id}")
}

/// Text with backslashes before the characters Markdown would act on.
fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.push(' ');
            continue;
        }
        if "\\`*_[]()<>#!|~&".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Assistant Markdown with each mention of a session `label` knows (the
/// current, an archived or a missing session it does not) made a link
/// titled after it. Fenced code is left alone, and a code span only when
/// it is not exactly a mention, so a command such as
/// `` `codesmith -s ses_…` `` stays as it is.
pub fn link_sessions<'a>(text: &'a str, label: &dyn Fn(&str) -> Option<String>) -> Cow<'a, str> {
    if !text.contains("ses_") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 32);
    let mut changed = false;
    let mut fence: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(&t[..3]);
            out.push_str(line);
            continue;
        }
        changed |= link_line(line, label, &mut out);
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// One line outside fenced code: its code spans kept, mentions linked.
fn link_line(line: &str, label: &dyn Fn(&str) -> Option<String>, out: &mut String) -> bool {
    let mut changed = false;
    let mut rest = line;
    while !rest.is_empty() {
        let Some(tick) = rest.find('`') else {
            changed |= link_plain(rest, label, out);
            break;
        };
        changed |= link_plain(&rest[..tick], label, out);
        let after = &rest[tick..];
        let ticks = after.bytes().take_while(|b| *b == b'`').count();
        let close = after[ticks..].find(&after[..ticks]).map(|c| c + ticks);
        let Some(close) = close else {
            out.push_str(after);
            break;
        };
        let span = &after[..close + ticks];
        let inner = after[ticks..close].trim();
        match parse_session_mention(inner).and_then(|id| Some((id, label(id)?))) {
            Some((id, l)) => {
                out.push_str(&format!("[@{}]({id})", escape_markdown(&l)));
                changed = true;
            }
            None => out.push_str(span),
        }
        rest = &after[close + ticks..];
    }
    changed
}

fn link_plain(s: &str, label: &dyn Fn(&str) -> Option<String>, out: &mut String) -> bool {
    let mut at = 0;
    let mut changed = false;
    for m in session_mentions(s) {
        let Some(l) = label(m.id) else {
            continue;
        };
        out.push_str(&s[at..m.start]);
        out.push_str(&format!("[@{}]({})", escape_markdown(&l), m.id));
        at = m.end;
        changed = true;
    }
    out.push_str(&s[at..]);
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "ses_0123456789abcdefghijklmnop";

    fn ids(text: &str) -> Vec<&str> {
        session_mentions(text).iter().map(|m| m.id).collect()
    }

    // session-mention.test.ts: "session tokens round-trip independently of the title"
    #[test]
    fn oc_session_tokens_round_trip_independently_of_the_title() {
        assert_eq!(serialize_session_mention(ID), format!("@session:{ID}"));
        assert_eq!(parse_session_mention(&format!("@session:{ID}")), Some(ID));
        assert_eq!(parse_session_mention(ID), Some(ID));
        assert_eq!(ids(&format!("See @session:{ID}, then {ID}.")), vec![ID, ID]);
    }

    // session-mention.test.ts: "does not parse malformed or embedded IDs: %s" (7 cases)
    #[test]
    fn oc_does_not_parse_malformed_or_embedded_ids() {
        for text in [
            "ses_123".to_string(),
            format!("@session:{ID}extra"),
            format!("x{ID}"),
            format!("{ID}/path"),
            format!("https://host/{ID}"),
            format!("@other:{ID}"),
            format!("{ID}_x"),
        ] {
            assert_eq!(parse_session_mention(&text), None, "{text}");
            assert!(session_mentions(&text).is_empty(), "{text}");
        }
    }

    #[test]
    fn codesmith_ids_are_mentions_too() {
        let id = "ses_19a4b2c3d4e123abc";
        assert_eq!(parse_session_mention(id), Some(id));
        // Still streaming in: not yet a whole ID.
        assert_eq!(parse_session_mention(&id[..12]), None);
    }

    #[test]
    fn mentions_of_known_sessions_become_links_outside_code() {
        let other = "ses_19a4b2c3d4e123abc";
        let label = |id: &str| (id == other).then(|| "Fix *the* build".to_string());
        let text = format!(
            "See {other} and {ID}.\n`codesmith -s {other}` and `{other}`\n```\n{other}\n```\n"
        );
        assert_eq!(
            link_sessions(&text, &label),
            format!(
                "See [@Fix \\*the\\* build]({other}) and {ID}.\n`codesmith -s {other}` and [@Fix \\*the\\* build]({other})\n```\n{other}\n```\n"
            )
        );
        assert!(matches!(
            link_sessions("no mentions", &label),
            Cow::Borrowed(_)
        ));
    }
}
