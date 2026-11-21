//! A diff between chosen prompts of a session, by turn: OpenCode's
//! `SessionDiff.turn`. A turn is one prompt and the work it started (a
//! CodeSmith session takes no steers or queued prompts, so each prompt is
//! its own busy period). The range from prompt `from` through prompt `to`
//! compares the first start snapshot recorded in those turns with the last
//! end snapshot, so edits made between the turns (while the session was
//! idle) are in it too. Prompts are named by their record sequence number,
//! as `codesmith session messages` lists them; ones an undo hid are gone.

use crate::snapshot::Snapshots;
use crate::store::{hidden_ranges, Entry, Record, Store};
use codesmith_ai::{Part, Role};

/// The trees a range compares: `to` is `None` for the working copy (a turn
/// still running).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub from: String,
    pub to: Option<String>,
}

/// Why a range was refused (OpenCode's `MessageNotFoundError` and
/// `TurnRangeError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeError {
    /// No visible message of this session has that sequence number.
    NotFound { seq: i64 },
    /// `field` (`from` or `to`) names a message that is not a prompt.
    NotPrompt { field: &'static str, seq: i64 },
    /// `to` comes before `from`.
    Precedes { from: i64, to: i64 },
}

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RangeError::NotFound { seq } => write!(f, "no message {seq} in this session"),
            RangeError::NotPrompt { field, seq } => {
                write!(f, "--{field}: message {seq} is not a prompt")
            }
            RangeError::Precedes { from, to } => {
                write!(f, "--to: message {to} comes before message {from}")
            }
        }
    }
}

impl std::error::Error for RangeError {}

/// Whether a stored message is a prompt the user wrote (not the engine's
/// notes, nor tool results), as forks count them.
fn is_prompt(m: &codesmith_ai::Message) -> bool {
    m.role == Role::User
        && !crate::runner::is_engine(m)
        && m.parts.iter().any(|p| matches!(p, Part::Text(_)))
        && !m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }))
}

/// The trees for the turns from prompt `from` (default: the latest)
/// through prompt `to` (default: `from`). `live` says the session is
/// running its latest turn now: a range reaching it compares against the
/// working copy. `Ok(None)`: nothing to compare (no prompt yet, or no
/// start or end snapshot recorded in the range).
pub fn range(
    entries: &[Entry],
    from: Option<i64>,
    to: Option<i64>,
    live: bool,
) -> Result<Option<Range>, RangeError> {
    let hidden = hidden_ranges(entries);
    let visible = |seq: i64| !hidden.iter().any(|(f, t)| seq >= *f && seq < *t);
    let shown: Vec<&Entry> = entries.iter().filter(|e| visible(e.seq)).collect();
    let prompts: Vec<i64> = shown
        .iter()
        .filter_map(|e| match &e.record {
            Record::Message { message } if is_prompt(message) => Some(e.seq),
            _ => None,
        })
        .collect();
    let resolve = |field: &'static str, seq: i64| -> Result<usize, RangeError> {
        if let Some(i) = prompts.iter().position(|p| *p == seq) {
            return Ok(i);
        }
        let message = shown
            .iter()
            .any(|e| e.seq == seq && matches!(e.record, Record::Message { .. }));
        Err(if message {
            RangeError::NotPrompt { field, seq }
        } else {
            RangeError::NotFound { seq }
        })
    };
    let first = match from {
        Some(seq) => resolve("from", seq)?,
        None => match prompts.len() {
            0 => return Ok(None),
            n => n - 1,
        },
    };
    let last = match to {
        Some(seq) => resolve("to", seq)?,
        None => first,
    };
    if last < first {
        return Err(RangeError::Precedes {
            from: prompts[first],
            to: prompts[last],
        });
    }
    // Turn k runs from its prompt to the next one; its start snapshot is
    // recorded just before its prompt, after the turn before's records.
    let before = |k: usize| if k == 0 { i64::MIN } else { prompts[k - 1] };
    let after = |k: usize| prompts.get(k + 1).copied().unwrap_or(i64::MAX);
    let start = (first..=last).find_map(|k| {
        shown
            .iter()
            .filter(|e| e.seq > before(k) && e.seq < prompts[k])
            .filter_map(|e| match &e.record {
                Record::Snapshot { tree } => Some(tree.clone()),
                _ => None,
            })
            .next_back()
    });
    let Some(start) = start else {
        return Ok(None);
    };
    if live && last + 1 == prompts.len() {
        return Ok(Some(Range {
            from: start,
            to: None,
        }));
    }
    // The last end recorded in the range: a turn stopped before its end
    // snapshot falls back to an earlier one.
    let end = shown
        .iter()
        .filter(|e| e.seq > prompts[first] && e.seq < after(last))
        .filter_map(|e| match &e.record {
            Record::TurnEnd { tree } => Some(tree.clone()),
            _ => None,
        })
        .next_back();
    Ok(end.map(|to| Range {
        from: start,
        to: Some(to),
    }))
}

/// The trees of [`range`] for stored session `id`, with the working copy
/// captured when the range reaches a running turn.
pub fn trees(
    db: &Store,
    id: &str,
    snaps: &Snapshots,
    from: Option<i64>,
    to: Option<i64>,
    live: bool,
) -> anyhow::Result<Option<(String, String)>> {
    let entries = db.entries(id)?;
    let Some(r) = range(&entries, from, to, live)? else {
        return Ok(None);
    };
    let to = match r.to {
        Some(t) => t,
        None => snaps.capture()?,
    };
    Ok(Some((r.from, to)))
}

/// The unified diff of a [`range`] of stored session `id` (`None`: nothing
/// to compare).
pub fn unified(
    db: &Store,
    id: &str,
    snaps: &Snapshots,
    from: Option<i64>,
    to: Option<i64>,
) -> anyhow::Result<Option<String>> {
    trees(db, id, snaps, from, to, false)?
        .map(|(a, b)| snaps.diff(&a, &b))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesmith_ai::Message;

    fn e(seq: i64, record: Record) -> Entry {
        Entry {
            seq,
            created: 0,
            record,
        }
    }
    fn prompt(seq: i64, text: &str) -> Entry {
        e(
            seq,
            Record::Message {
                message: Message::user(text),
            },
        )
    }
    fn reply(seq: i64) -> Entry {
        e(
            seq,
            Record::Message {
                message: Message {
                    role: Role::Assistant,
                    parts: vec![Part::text("ok")],
                },
            },
        )
    }
    fn snap(seq: i64, t: &str) -> Entry {
        e(seq, Record::Snapshot { tree: t.into() })
    }
    fn end(seq: i64, t: &str) -> Entry {
        e(seq, Record::TurnEnd { tree: t.into() })
    }
    fn r(from: &str, to: &str) -> Option<Range> {
        Some(Range {
            from: from.into(),
            to: Some(to.into()),
        })
    }

    /// Three turns: s1 p2 a3 e4 | s5 p6 a7 e8 | s9 p10 a11 e12.
    fn three() -> Vec<Entry> {
        vec![
            snap(1, "s1"),
            prompt(2, "one"),
            reply(3),
            end(4, "e1"),
            snap(5, "s2"),
            prompt(6, "two"),
            reply(7),
            end(8, "e2"),
            snap(9, "s3"),
            prompt(10, "three"),
            reply(11),
            end(12, "e3"),
        ]
    }

    #[test]
    fn turns_and_ranges() {
        let t = three();
        assert_eq!(range(&t, None, None, false), Ok(r("s3", "e3")));
        assert_eq!(range(&t, Some(2), None, false), Ok(r("s1", "e1")));
        assert_eq!(range(&t, Some(2), Some(6), false), Ok(r("s1", "e2")));
        assert_eq!(range(&t, Some(6), Some(10), false), Ok(r("s2", "e3")));
        assert_eq!(range(&t, Some(10), Some(10), false), Ok(r("s3", "e3")));
    }

    #[test]
    fn refusals() {
        let t = three();
        assert_eq!(
            range(&t, Some(6), Some(2), false),
            Err(RangeError::Precedes { from: 6, to: 2 })
        );
        assert_eq!(
            range(&t, Some(3), None, false),
            Err(RangeError::NotPrompt {
                field: "from",
                seq: 3
            })
        );
        assert_eq!(
            range(&t, Some(2), Some(7), false),
            Err(RangeError::NotPrompt {
                field: "to",
                seq: 7
            })
        );
        for seq in [0, 1, 4, 99, -1, i64::MAX, i64::MIN] {
            assert_eq!(
                range(&t, Some(seq), None, false),
                Err(RangeError::NotFound { seq }),
                "{seq}"
            );
        }
    }

    #[test]
    fn empty_and_unrecorded() {
        assert_eq!(range(&[], None, None, false), Ok(None));
        // No snapshots at all (snapshots off).
        let t = vec![prompt(1, "a"), reply(2)];
        assert_eq!(range(&t, None, None, false), Ok(None));
        assert_eq!(range(&t, None, None, true), Ok(None));
        // A start but no end yet: nothing recorded, unless it runs now.
        let t = vec![snap(1, "s"), prompt(2, "a")];
        assert_eq!(range(&t, None, None, false), Ok(None));
        assert_eq!(
            range(&t, None, None, true),
            Ok(Some(Range {
                from: "s".into(),
                to: None
            }))
        );
    }

    #[test]
    fn a_turn_without_its_own_end_falls_back_to_an_earlier_end() {
        let mut t = three();
        t.retain(|x| x.seq != 12);
        assert_eq!(range(&t, Some(6), Some(10), false), Ok(r("s2", "e2")));
        // Alone, the turn has no end of its own.
        assert_eq!(range(&t, None, None, false), Ok(None));
        // Live, an earlier turn's range keeps its recorded end.
        assert_eq!(range(&t, Some(2), Some(6), true), Ok(r("s1", "e2")));
    }

    #[test]
    fn a_turn_without_a_start_uses_the_next_recorded_one() {
        let mut t = three();
        t.retain(|x| x.seq != 1);
        assert_eq!(range(&t, Some(2), None, false), Ok(None));
        assert_eq!(range(&t, Some(2), Some(6), false), Ok(r("s2", "e2")));
    }

    #[test]
    fn a_later_end_tree_of_the_same_turn_counts() {
        // A continued turn records a second end.
        let mut t = three();
        t.push(end(13, "e3b"));
        assert_eq!(range(&t, None, None, false), Ok(r("s3", "e3b")));
    }

    #[test]
    fn undone_turns_are_gone() {
        let mut t = three();
        // Undo of the third turn hides 9..14.
        t.push(e(
            14,
            Record::Revert {
                from_seq: 9,
                after: "x".into(),
            },
        ));
        assert_eq!(range(&t, None, None, false), Ok(r("s2", "e2")));
        assert_eq!(
            range(&t, Some(10), None, false),
            Err(RangeError::NotFound { seq: 10 })
        );
        // Redone, it is back.
        t.push(e(15, Record::Redo { revert_seq: 14 }));
        assert_eq!(range(&t, None, None, false), Ok(r("s3", "e3")));
    }

    #[test]
    fn engine_notes_and_tool_results_are_not_prompts() {
        let mut t = three();
        t.insert(
            3,
            e(
                3,
                Record::Message {
                    message: Message {
                        role: Role::User,
                        parts: vec![Part::ToolResult {
                            id: "c".into(),
                            name: "read".into(),
                            content: "x".into(),
                            is_error: false,
                            media: vec![],
                        }],
                    },
                },
            ),
        );
        t.retain(|x| !(x.seq == 3 && matches!(&x.record, Record::Message { message } if message.role == Role::Assistant)));
        assert_eq!(
            range(&t, Some(3), None, false),
            Err(RangeError::NotPrompt {
                field: "from",
                seq: 3
            })
        );
        // Unicode, control characters and a huge prompt are prompts all the same.
        let huge = "x".repeat(1 << 20);
        for text in ["é ✓ 漢字", "a\u{0}b\u{1b}[31m", huge.as_str()] {
            let t = vec![snap(1, "s"), prompt(2, text), end(3, "e")];
            assert_eq!(range(&t, Some(2), None, false), Ok(r("s", "e")));
        }
    }
}
