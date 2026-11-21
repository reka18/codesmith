//! Port of OpenCode's `session-diff.test.ts` (MIT): the diff of the turn
//! containing a prompt, and of ranges across later turns, on real
//! snapshots. Adapted: CodeSmith takes no steers, so every prompt is its own
//! busy period (OpenCode's steered prompt is a turn of its own here), and
//! prompts are named by their record sequence number.

mod support;

use codesmith_core::session::diff::{self, RangeError};
use codesmith_core::snapshot::Snapshots;
use codesmith_core::store::Record;
use serde_json::json;
use support::H;

fn prompts(h: &H) -> Vec<i64> {
    h.store()
        .entries(&h.s.id)
        .unwrap()
        .iter()
        .filter_map(|e| match &e.record {
            Record::Message { message }
                if message.role == codesmith_ai::Role::User
                    && message
                        .parts
                        .iter()
                        .all(|p| matches!(p, codesmith_ai::Part::Text(_))) =>
            {
                Some(e.seq)
            }
            _ => None,
        })
        .collect()
}

fn summary(snaps: &Snapshots, h: &H, from: Option<i64>, to: Option<i64>) -> Vec<(String, char)> {
    summary_live(snaps, h, from, to, false)
}

fn summary_live(
    snaps: &Snapshots,
    h: &H,
    from: Option<i64>,
    to: Option<i64>,
    live: bool,
) -> Vec<(String, char)> {
    let Some((a, b)) = diff::trees(&h.store(), &h.s.id, snaps, from, to, live).unwrap() else {
        return vec![];
    };
    let mut v: Vec<(String, char)> = snaps
        .changes(&a, &b)
        .unwrap()
        .into_iter()
        .map(|c| (c.path, c.status))
        .collect();
    v.sort();
    v
}

fn err(h: &H, snaps: &Snapshots, from: Option<i64>, to: Option<i64>) -> RangeError {
    diff::trees(&h.store(), &h.s.id, snaps, from, to, false)
        .unwrap_err()
        .downcast::<RangeError>()
        .unwrap()
}

async fn write(h: &mut H, path: &str, content: &str) {
    h.prompt(&format!(
        "call write {}",
        json!({"path": path, "content": content})
    ))
    .await;
}

fn files(list: &[(&str, char)]) -> Vec<(String, char)> {
    list.iter().map(|(p, c)| (p.to_string(), *c)).collect()
}

// session-diff.test.ts: "diffs the busy period containing a user message
// and ranges across later turns"
#[tokio::test(flavor = "multi_thread")]
async fn oc_diffs_the_turn_containing_a_prompt_and_ranges_across_later_turns() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let mut h = H::with(json!({"snapshots": true}), None).await;
    let root = h.root();
    for (f, t) in [
        ("first.txt", "first\n"),
        ("second.txt", "second\n"),
        ("manual.txt", "manual\n"),
    ] {
        std::fs::write(root.join(f), t).unwrap();
    }
    let snaps = Snapshots::for_project(&root).unwrap();
    assert_eq!(summary(&snaps, &h, None, None), vec![]);

    write(&mut h, "first.txt", "first edited\n").await;
    // Edits made while idle are no turn's work, but a range spanning them
    // still sees them.
    std::fs::write(root.join("manual.txt"), "manual edited\n").unwrap();
    write(&mut h, "second.txt", "second edited\n").await;
    let p = prompts(&h);
    let (first, second) = (p[0], p[1]);
    assert_eq!(
        summary(&snaps, &h, None, None),
        files(&[("second.txt", 'M')])
    );
    assert_eq!(
        summary(&snaps, &h, Some(first), None),
        files(&[("first.txt", 'M')])
    );

    write(&mut h, "third.txt", "third\n").await;
    write(&mut h, "fourth.txt", "fourth\n").await;
    let p = prompts(&h);
    let (third, fourth) = (p[2], p[3]);
    assert_eq!(
        summary(&snaps, &h, None, None),
        files(&[("fourth.txt", 'A')])
    );
    assert_eq!(
        summary(&snaps, &h, Some(third), Some(fourth)),
        files(&[("fourth.txt", 'A'), ("third.txt", 'A')])
    );
    assert_eq!(
        summary(&snaps, &h, Some(second), None),
        files(&[("second.txt", 'M')])
    );
    assert_eq!(
        summary(&snaps, &h, Some(first), Some(third)),
        files(&[
            ("first.txt", 'M'),
            ("manual.txt", 'M'),
            ("second.txt", 'M'),
            ("third.txt", 'A'),
        ])
    );
    let patch = diff::unified(&h.store(), &h.s.id, &snaps, Some(first), None)
        .unwrap()
        .unwrap();
    assert!(patch.contains("-first\n+first edited\n"), "{patch}");

    // Refused ranges.
    assert_eq!(
        err(&h, &snaps, Some(fourth), Some(second)),
        RangeError::Precedes {
            from: fourth,
            to: second
        }
    );
    let entries = h.store().entries(&h.s.id).unwrap();
    let reply = entries
        .iter()
        .find(|e| {
            e.seq > first
                && matches!(&e.record, Record::Message { message } if message.role == codesmith_ai::Role::Assistant)
        })
        .unwrap()
        .seq;
    assert_eq!(
        err(&h, &snaps, Some(reply), None),
        RangeError::NotPrompt {
            field: "from",
            seq: reply
        }
    );
    assert_eq!(
        err(&h, &snaps, Some(first), Some(reply)),
        RangeError::NotPrompt {
            field: "to",
            seq: reply
        }
    );
    assert_eq!(
        err(&h, &snaps, Some(999_999), None),
        RangeError::NotFound { seq: 999_999 }
    );

    // Only a turn running now compares against the working copy.
    std::fs::remove_file(root.join("manual.txt")).unwrap();
    assert_eq!(
        summary(&snaps, &h, None, None),
        files(&[("fourth.txt", 'A')])
    );
    assert_eq!(
        summary_live(&snaps, &h, None, None, true),
        files(&[("fourth.txt", 'A'), ("manual.txt", 'D')])
    );
    std::fs::write(root.join("manual.txt"), "manual edited\n").unwrap();

    // Undoing removes the later history: the undone turn's prompt is gone.
    codesmith_core::session::undo(&h.store(), &h.s.id, &snaps)
        .unwrap()
        .unwrap();
    assert_eq!(
        summary(&snaps, &h, None, None),
        files(&[("third.txt", 'A')])
    );
    assert_eq!(
        err(&h, &snaps, Some(fourth), None),
        RangeError::NotFound { seq: fourth }
    );
}
