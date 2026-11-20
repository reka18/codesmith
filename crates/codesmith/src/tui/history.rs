//! The prompt history, kept across restarts as OpenCode keeps it: one
//! JSON entry a line in `prompt-history.jsonl` (in CodeSmith's data
//! folder), the newest [`MAX_ENTRIES`] of them, in OpenCode's shape
//! (`text`, `files`, `agents`, `pasted`, and `mode`). Lines that do not
//! parse are skipped and the file is rewritten without them; a prompt is
//! appended unless it repeats the one before (with the same files). All
//! tabs share it.

use super::attach::Pending;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// How many prompts are kept.
pub const MAX_ENTRIES: usize = 50;

/// The history file.
pub fn file() -> PathBuf {
    codesmith_core::paths::data_dir().join("prompt-history.jsonl")
}

/// The entries of a history file: valid lines (an object with a `text`
/// string and a `pasted` list), the newest [`MAX_ENTRIES`].
pub fn parse(text: &str) -> Vec<Value> {
    let mut out: Vec<Value> = text
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["text"].is_string() && v["pasted"].is_array())
        .collect();
    if out.len() > MAX_ENTRIES {
        out.drain(..out.len() - MAX_ENTRIES);
    }
    out
}

/// A prompt as an entry: its text and the files attached to it.
pub fn entry(text: &str, files: &[Pending]) -> Value {
    let files: Vec<Value> = files
        .iter()
        .map(|f| {
            let mut v = json!({"name": f.label, "path": f.path.display().to_string()});
            if let Some(start) = text.find(&f.label) {
                let start = text[..start].chars().count();
                v["mention"] = json!({
                    "start": start,
                    "end": start + f.label.chars().count(),
                    "text": f.label,
                });
            }
            v
        })
        .collect();
    json!({"text": text, "files": files, "agents": [], "pasted": []})
}

/// The same prompt again: text, mode and attachments alike.
pub fn is_duplicate(last: Option<&Value>, next: &Value) -> bool {
    let Some(last) = last else {
        return false;
    };
    let mode = |v: &Value| v["mode"].as_str().unwrap_or("normal").to_string();
    let files = |v: &Value| v.get("files").cloned().unwrap_or_else(|| json!([]));
    last["text"] == next["text"] && mode(last) == mode(next) && files(last) == files(next)
}

/// An entry's text.
pub fn text(v: &Value) -> String {
    v["text"].as_str().unwrap_or_default().to_string()
}

/// An entry's files that still exist, to attach again when it is recalled.
pub fn files(v: &Value) -> Vec<Pending> {
    v["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let label = f["name"].as_str()?.to_string();
            let path = PathBuf::from(f["path"].as_str()?);
            path.exists().then_some(Pending { label, path })
        })
        .collect()
}

/// Add `line` (and a newline) to `path`, creating its folder.
pub fn append_text(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    codesmith_core::paths::ensure_parent(path)?;
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)?.write_all(line.as_bytes())
}

fn lines(entries: &[Value]) -> String {
    entries.iter().map(|e| format!("{e}\n")).collect()
}

/// The history in its file.
#[derive(Debug, Default)]
pub struct Store {
    path: PathBuf,
    pub entries: Vec<Value>,
}

impl Store {
    /// Read `path`, rewriting it when lines were dropped.
    pub fn load(path: PathBuf) -> Store {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let entries = parse(&text);
        if !entries.is_empty() && lines(&entries) != text {
            let _ = codesmith_core::paths::write_private(&path, lines(&entries).as_bytes());
        }
        Store { path, entries }
    }

    /// Keep a submitted prompt.
    pub fn append(&mut self, entry: Value) {
        if is_duplicate(self.entries.last(), &entry) {
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(..self.entries.len() - MAX_ENTRIES);
            let _ =
                codesmith_core::paths::write_private(&self.path, lines(&self.entries).as_bytes());
            return;
        }
        let last = self.entries.last().expect("just pushed");
        let _ = append_text(&self.path, &format!("{last}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(text: &str) -> Value {
        json!({"text": text, "files": [], "agents": [], "pasted": []})
    }

    // history.test.ts: "recovers valid JSONL entries around corruption"
    #[test]
    fn oc_recovers_valid_jsonl_entries_around_corruption() {
        let control = e("a\u{1b}[31mb\u{c}c\0d\r\n");
        let text = format!("{control}\nnot-json\n{}\n", e("two"));
        assert_eq!(parse(&text), vec![control, e("two")]);
    }

    // history.test.ts: "ignores the legacy parts shape"
    #[test]
    fn oc_ignores_the_legacy_parts_shape() {
        assert!(parse(r#"{"input": "old", "parts": []}"#).is_empty());
    }

    // history.test.ts: "retains only the newest entries"
    #[test]
    fn oc_retains_only_the_newest_entries() {
        let text: Vec<String> = (0..MAX_ENTRIES + 5)
            .map(|i| e(&i.to_string()).to_string())
            .collect();
        let got = parse(&text.join("\n"));
        assert_eq!(got.len(), MAX_ENTRIES);
        assert_eq!(got[0]["text"], "5");
    }

    // history.test.ts: "does not dedupe entries with different attachments"
    #[test]
    fn oc_does_not_dedupe_entries_with_different_attachments() {
        let a = entry(
            "describe this",
            &[Pending {
                label: "a.png".into(),
                path: "/tmp/a.png".into(),
            }],
        );
        let b = entry(
            "describe this",
            &[Pending {
                label: "b.png".into(),
                path: "/tmp/b.png".into(),
            }],
        );
        assert!(!is_duplicate(Some(&a), &b));
        assert!(is_duplicate(Some(&a), &a.clone()));
        assert!(!is_duplicate(None, &a));
        let shell = json!({"text": "ls", "files": [], "agents": [], "pasted": [], "mode": "shell"});
        assert!(!is_duplicate(Some(&e("ls")), &shell));
    }

    // history.test.ts: "preserves duplicate attachment mentions for prompt restoration"
    #[test]
    fn oc_preserves_duplicate_attachment_mentions() {
        let value = json!({
            "text": "[Image 1] [Image 1]",
            "files": [
                {"name": "clipboard", "uri": "data:image/png;base64,AAA", "mention": {"start": 0, "end": 9, "text": "[Image 1]"}},
                {"name": "clipboard", "uri": "data:image/png;base64,AAA", "mention": {"start": 10, "end": 19, "text": "[Image 1]"}},
            ],
            "agents": [],
            "pasted": [],
        });
        assert_eq!(parse(&value.to_string()), vec![value]);
    }

    // persistence.test.ts: "persistence creates parent directories and
    // supports text, append, and JSON" (adapted: the history's own writes)
    #[test]
    fn oc_persistence_creates_parent_directories_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.jsonl");
        append_text(&path, "one\n").unwrap();
        append_text(&path, "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        let json = dir.path().join("other").join("state.json");
        codesmith_core::paths::write_private(&json, br#"{"value":1}"#).unwrap();
        codesmith_core::paths::write_private(&json, br#"{"value":2}"#).unwrap();
        assert_eq!(std::fs::read_to_string(&json).unwrap(), r#"{"value":2}"#);
    }

    #[test]
    fn the_store_heals_its_file_dedupes_and_trims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt-history.jsonl");
        std::fs::write(&path, format!("{}\nbroken\n", e("legacy"))).unwrap();
        let mut s = Store::load(path.clone());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\n", e("legacy"))
        );
        s.append(e("next"));
        s.append(e("next"));
        assert_eq!(parse(&std::fs::read_to_string(&path).unwrap()).len(), 2);
        for i in 0..MAX_ENTRIES {
            s.append(e(&i.to_string()));
        }
        let kept = parse(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(kept.len(), MAX_ENTRIES);
        assert_eq!(kept[0]["text"], "0");
    }
}
