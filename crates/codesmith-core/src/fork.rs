//! Session forking, as OpenCode's `Session.fork` does it: a new top-level
//! session with a copy of another's settled history, through its latest
//! message or up to (not including) a chosen one, that then goes its own way.
//!
//! A message's id in CodeSmith's log is its record sequence number, so a
//! boundary is one (see [`crate::store::Store::fork_at`]). The copy keeps
//! the parent's sequence numbers, so checkpoints and snapshots still line
//! up, and leaves out what was undone, the parent's step and tool
//! statistics (a fork starts with no usage, as OpenCode's does) and a
//! running step: an assistant message whose tool calls have no results.
//! Compactions it reaches, checkpoints and native windows
//! ([`Record::Native`]), are copied as OpenCode copies settled compaction
//! rows: a native window still replays only on the model that made it.
//! Before its first prompt the fork tells the model it is one
//! ([`NOTICE`], OpenCode's fork-notice plugin).

use crate::store::{Entry, Record};
use codesmith_ai::{Part, Role};

/// What the model is told before a fork's first prompt (OpenCode's text).
pub const NOTICE: &str = "<fork-notice>
This conversation is a fork of another conversation, copied up to the previous message. The original conversation keeps going on its own and may be active right now, as may other forks of it.
- Work started before the fork belongs to the original: background subagents, background shells, schedules, and follow-ups promised there. Their results and notifications go to the original, never here, and you can't continue those subagents. Don't wait for them, redo them, or report their results as yours; if it matters, check what already happened (files, git, PRs, CI).
- Files, repositories, branches and external services are shared with the original. Before changing something the original was working on (its uncommitted changes, branches, worktrees, PRs, deploys, messages), check its current state, and put work that diverges from it on a new branch or worktree.
- Otherwise work normally: new tasks need no extra checks, branches or questions.
- If it's unclear whether this fork or the original should do something, ask.
</fork-notice>";

/// Session meta key: the notice is still to be given.
pub const NOTICE_META: &str = "fork_notice";

/// Session meta key: where the fork came from, as [`Origin`] JSON.
pub const ORIGIN_META: &str = "fork";

/// Where a fork was cut, by the parent's record sequence numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "seq", rename_all = "snake_case")]
pub enum Boundary {
    /// Up to and including this message.
    Through(i64),
    /// Up to, not including, this message.
    Before(i64),
}

/// The session a fork was copied from and where.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Origin {
    pub session: String,
    pub boundary: Boundary,
}

/// Forking a session with no messages (OpenCode's `ForkEmptyError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Empty {
    pub session: String,
}

impl std::fmt::Display for Empty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session {} has no messages to fork", self.session)
    }
}

impl std::error::Error for Empty {}

/// The boundary is not one of the session's messages (OpenCode's
/// `MessageNotFoundError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageNotFound {
    pub session: String,
    pub seq: i64,
}

impl std::fmt::Display for MessageNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session {} has no message {}", self.session, self.seq)
    }
}

impl std::error::Error for MessageNotFound {}

/// A fork's title: the parent's with ` (fork #n)` counted up; untitled
/// stays untitled.
pub fn title(parent: Option<&str>) -> Option<String> {
    let t = parent?;
    if let Some(rest) = t.strip_suffix(')') {
        if let Some((base, n)) = rest.rsplit_once(" (fork #") {
            if let Ok(n) = n.parse::<u64>() {
                if !base.is_empty() {
                    return Some(format!("{base} (fork #{})", n + 1));
                }
            }
        }
    }
    Some(format!("{t} (fork #1)"))
}

/// The visible messages of a log, with their sequence numbers.
pub fn messages(entries: &[Entry]) -> Vec<(i64, &codesmith_ai::Message)> {
    let hidden = crate::store::hidden_ranges(entries);
    entries
        .iter()
        .filter(|e| !hidden.iter().any(|(f, t)| e.seq >= *f && e.seq < *t))
        .filter_map(|e| match &e.record {
            Record::Message { message } => Some((e.seq, message)),
            _ => None,
        })
        .collect()
}

/// The boundary for forking `entries` (of session `session`) before
/// `before`, or through its latest message.
pub fn boundary(session: &str, entries: &[Entry], before: Option<i64>) -> anyhow::Result<Boundary> {
    let list = messages(entries);
    match before {
        Some(seq) if list.iter().any(|(s, _)| *s == seq) => Ok(Boundary::Before(seq)),
        Some(seq) => Err(MessageNotFound {
            session: session.into(),
            seq,
        }
        .into()),
        None => match list.last() {
            Some((seq, _)) => Ok(Boundary::Through(*seq)),
            None => Err(Empty {
                session: session.into(),
            }
            .into()),
        },
    }
}

/// The records a fork at `boundary` copies, settled ones only.
pub fn copied(entries: &[Entry], boundary: Boundary) -> Vec<Entry> {
    let end = match boundary {
        Boundary::Through(s) => s + 1,
        Boundary::Before(s) => s,
    };
    let hidden = crate::store::hidden_ranges(entries);
    let mut out: Vec<Entry> = entries
        .iter()
        .filter(|e| e.seq < end)
        .filter(|e| !hidden.iter().any(|(f, t)| e.seq >= *f && e.seq < *t))
        .filter(|e| {
            !matches!(
                e.record,
                Record::Revert { .. }
                    | Record::Redo { .. }
                    | Record::Step { .. }
                    | Record::Tool { .. }
                    | Record::Unknown
            )
        })
        .cloned()
        .collect();
    // A step still running (its calls have no results yet) stays with the parent.
    if let Some(i) = out
        .iter()
        .rposition(|e| matches!(e.record, Record::Message { .. }))
    {
        if let Record::Message { message } = &out[i].record {
            let calls = message.role == Role::Assistant
                && message
                    .parts
                    .iter()
                    .any(|p| matches!(p, Part::ToolCall { .. }));
            if calls {
                out.truncate(i);
            }
        }
    }
    // Metadata a plugin's prompt hook kept with a copied message goes with
    // it, though it was written just after the boundary message; that of a
    // message left behind does not.
    let kept: std::collections::HashSet<i64> = out
        .iter()
        .filter(|e| matches!(e.record, Record::Message { .. }))
        .map(|e| e.seq)
        .collect();
    out.retain(|e| !matches!(&e.record, Record::Metadata { seq, .. } if !kept.contains(seq)));
    for e in entries.iter().filter(|e| e.seq >= end) {
        if matches!(&e.record, Record::Metadata { seq, .. } if kept.contains(seq)) {
            out.push(e.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_prompt_metadata_with_its_message() {
        let e = |seq, record| Entry {
            seq,
            created: 0,
            record,
        };
        let user = |t: &str| Record::Message {
            message: codesmith_ai::Message::user(t),
        };
        let meta = |seq| Record::Metadata {
            seq,
            metadata: serde_json::json!({"tag": seq}),
        };
        let entries = vec![
            e(1, user("first")),
            e(2, meta(1)),
            e(3, user("second")),
            e(4, meta(3)),
        ];
        // Through the last message: its metadata, written after it, too.
        let all = copied(&entries, Boundary::Through(3));
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3, 4]);
        // Before the second: only the first message's.
        let first = copied(&entries, Boundary::Before(3));
        assert_eq!(first.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn titles_count_forks() {
        assert_eq!(title(None), None);
        assert_eq!(title(Some("Parent")).as_deref(), Some("Parent (fork #1)"));
        assert_eq!(
            title(Some("Parent (fork #1)")).as_deref(),
            Some("Parent (fork #2)")
        );
        assert_eq!(
            title(Some("Parent (fork #x)")).as_deref(),
            Some("Parent (fork #x) (fork #1)")
        );
    }
}
