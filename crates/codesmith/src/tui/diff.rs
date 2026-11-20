//! Line diffs for `edit` and `write` calls, shown in the transcript and in
//! the permission dialog. Computed from the tool input (and the file as it
//! is before the tool runs), so no help from the tools is needed.

use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Context,
    Add,
    Del,
    /// Unchanged lines left out between hunks.
    Gap,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub kind: Kind,
    /// Line number in the old (Del, Context) or new (Add) file.
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Diff {
    pub lines: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
}

/// Lines of context kept around each change.
const CONTEXT: usize = 2;

/// Diff `old` against `new`; `first` is the file line number of their first line.
pub fn diff(old: &str, new: &str, first: usize) -> Diff {
    let td = TextDiff::from_lines(old, new);
    let mut out = Diff::default();
    for (gi, group) in td.grouped_ops(CONTEXT).iter().enumerate() {
        if gi > 0 {
            out.lines.push(DiffLine {
                kind: Kind::Gap,
                line: 0,
                text: String::new(),
            });
        }
        for op in group {
            for ch in td.iter_changes(op) {
                let text = ch.value().trim_end_matches(['\n', '\r']).to_string();
                let (kind, idx) = match ch.tag() {
                    ChangeTag::Equal => (Kind::Context, ch.old_index()),
                    ChangeTag::Delete => {
                        out.removed += 1;
                        (Kind::Del, ch.old_index())
                    }
                    ChangeTag::Insert => {
                        out.added += 1;
                        (Kind::Add, ch.new_index())
                    }
                };
                out.lines.push(DiffLine {
                    kind,
                    line: idx.unwrap_or(0) + first,
                    text,
                });
            }
        }
    }
    out
}

/// The change an `edit` or `write` call is about to make, if it can be told.
pub fn for_tool(name: &str, input: &Value, cwd: &Path) -> Option<Diff> {
    let name = name.rsplit(" › ").next().unwrap_or(name);
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    let path = cwd.join(s("path")?);
    match name {
        "edit" => {
            let (old, new) = (s("oldString")?, s("newString")?);
            // Number lines from where the text sits in the file, when found.
            let first = std::fs::read_to_string(&path)
                .ok()
                .and_then(|src| src.find(old).map(|i| src[..i].matches('\n').count() + 1))
                .unwrap_or(1);
            let mut old = old.to_string();
            let mut new = new.to_string();
            if !old.ends_with('\n') {
                old.push('\n');
            }
            if !new.ends_with('\n') {
                new.push('\n');
            }
            Some(diff(&old, &new, first))
        }
        "write" => {
            let before = std::fs::read_to_string(&path).unwrap_or_default();
            Some(diff(&before, s("content")?, 1))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kinds(d: &Diff) -> Vec<(Kind, usize, &str)> {
        d.lines
            .iter()
            .map(|l| (l.kind, l.line, l.text.as_str()))
            .collect()
    }

    #[test]
    fn hunks_with_context_and_gaps() {
        let old: String = (1..=10).map(|i| format!("{i}\n")).collect();
        let new = old.replace("2\n", "two\n").replace("9\n", "");
        let d = diff(&old, &new, 1);
        assert_eq!((d.added, d.removed), (1, 2));
        assert_eq!(
            kinds(&d),
            vec![
                (Kind::Context, 1, "1"),
                (Kind::Del, 2, "2"),
                (Kind::Add, 2, "two"),
                (Kind::Context, 3, "3"),
                (Kind::Context, 4, "4"),
                (Kind::Gap, 0, ""),
                (Kind::Context, 7, "7"),
                (Kind::Context, 8, "8"),
                (Kind::Del, 9, "9"),
                (Kind::Context, 10, "10"),
            ]
        );
    }

    #[test]
    fn edit_and_write_inputs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let e = for_tool(
            "edit",
            &json!({"path": "a.rs", "oldString": "fn b() {}", "newString": "fn b() { 1 }"}),
            d.path(),
        )
        .unwrap();
        assert_eq!(
            kinds(&e),
            vec![(Kind::Del, 2, "fn b() {}"), (Kind::Add, 2, "fn b() { 1 }")]
        );
        let w = for_tool(
            "build › write",
            &json!({"path": "new.txt", "content": "x\ny\n"}),
            d.path(),
        )
        .unwrap();
        assert_eq!((w.added, w.removed), (2, 0));
        assert!(for_tool("read", &json!({"path": "a.rs"}), d.path()).is_none());
    }
}
