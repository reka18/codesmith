//! Autocomplete for the prompt (`/command`, and `@` for files, subagents
//! and skills), fuzzy filtering shared with the pickers, and the project
//! file list.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// What is shown and inserted (without the `/` or `@`).
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Slash,
    File,
    /// A directory argument in shell mode (`!cd src/`).
    Dir,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Popup {
    pub kind: Kind,
    /// Byte offset of the trigger character (`/` or `@`) in the input.
    pub start: usize,
    pub items: Vec<Item>,
    pub sel: usize,
    /// How many of the first items are subagents and skills (`@` names,
    /// not files).
    pub named: usize,
}

/// Most entries a popup holds.
pub const MAX_ITEMS: usize = 50;

/// What, if anything, the text before the cursor asks to complete:
/// `/name` at the very start of the input, or an `@path` word.
pub fn trigger(text: &str, cursor: usize) -> Option<(Kind, usize, &str)> {
    let before = &text[..cursor];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let word = &before[start..];
    if start == 0 && word.starts_with('/') {
        return Some((Kind::Slash, 0, &word[1..]));
    }
    if let Some(q) = word.strip_prefix('@') {
        return Some((Kind::File, start, q));
    }
    None
}

/// Rank `items` against `query` (fuzzy; an empty query keeps the order).
/// `path` tunes the scoring for file paths.
pub fn filter<T, F>(items: &[T], query: &str, key: F, path: bool, limit: usize) -> Vec<usize>
where
    F: Fn(&T) -> &str,
{
    if query.is_empty() {
        return (0..items.len().min(limit)).collect();
    }
    let mut cfg = Config::DEFAULT;
    if path {
        cfg.set_match_paths();
    }
    let mut m = Matcher::new(cfg);
    let pat = Pattern::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let s = key(it);
            pat.score(Utf32Str::new(s, &mut buf), &mut m)
                .map(|sc| (sc, i))
        })
        .collect();
    // Higher score first; shorter labels win ties, then the original order.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(key(&items[a.1]).len().cmp(&key(&items[b.1]).len()))
            .then(a.1.cmp(&b.1))
    });
    scored.into_iter().take(limit).map(|(_, i)| i).collect()
}

/// Most subagents and skills an `@` popup offers once something is typed
/// (OpenCode's).
const MAX_NAMED: usize = 10;

/// Build the popup for the current input, if any. An `@` popup offers the
/// subagents and skills in `named` first, as OpenCode's does, then the
/// files.
pub fn popup(
    text: &str,
    cursor: usize,
    commands: &[Item],
    named: &[Item],
    files: &[String],
) -> Option<Popup> {
    let (kind, start, query) = trigger(text, cursor)?;
    let mut names: Vec<Item> = Vec::new();
    if kind == Kind::File {
        let limit = if query.is_empty() {
            MAX_ITEMS
        } else {
            MAX_NAMED
        };
        names = filter(named, query, |n| n.label.as_str(), false, limit)
            .into_iter()
            .map(|i| named[i].clone())
            .collect();
    }
    let room = MAX_ITEMS.saturating_sub(names.len());
    let items: Vec<Item> = match kind {
        Kind::Slash => filter(commands, query, |c| c.label.as_str(), false, MAX_ITEMS)
            .into_iter()
            .map(|i| commands[i].clone())
            .collect(),
        Kind::Dir => return None,
        Kind::File => names
            .iter()
            .cloned()
            .chain(
                filter(files, query, |f| f.as_str(), true, room)
                    .into_iter()
                    .map(|i| Item {
                        label: files[i].clone(),
                        detail: String::new(),
                    }),
            )
            .collect(),
    };
    if items.is_empty() {
        return None;
    }
    // Nothing to offer when the only match is what is already typed.
    if items.len() == 1 && items[0].label == query {
        return None;
    }
    Some(Popup {
        kind,
        start,
        items,
        sel: 0,
        named: names.len(),
    })
}

/// Files under `root` that git would not ignore (directories end in `/`),
/// sorted, at most `limit`.
pub fn project_files(root: &Path, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = std::collections::BTreeSet::new();
    for e in ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .build()
        .flatten()
    {
        if out.len() >= limit {
            break;
        }
        let Ok(rel) = e.path().strip_prefix(root) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel = rel.to_string_lossy().replace('\\', "/");
        if e.file_type().is_some_and(|t| t.is_dir()) {
            dirs.insert(format!("{rel}/"));
        } else {
            out.push(rel);
        }
    }
    out.extend(dirs);
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(names: &[&str]) -> Vec<Item> {
        names
            .iter()
            .map(|n| Item {
                label: n.to_string(),
                detail: String::new(),
            })
            .collect()
    }

    #[test]
    fn triggers() {
        assert_eq!(trigger("/rev", 4), Some((Kind::Slash, 0, "rev")));
        assert_eq!(trigger("/review HEAD", 12), None, "args are not completed");
        assert_eq!(trigger("x /rev", 6), None, "slash only at the start");
        assert_eq!(trigger("see @src/ma", 11), Some((Kind::File, 4, "src/ma")));
        assert_eq!(trigger("see @src/ma now", 15), None);
        assert_eq!(trigger("see @src now", 8), Some((Kind::File, 4, "src")));
        assert_eq!(trigger("a@b", 3), None, "emails are not mentions");
        assert_eq!(trigger("line\n@x", 7), Some((Kind::File, 5, "x")));
    }

    #[test]
    fn fuzzy_ranking() {
        let files = vec![
            "README.md".to_string(),
            "crates/codesmith/src/tui/app.rs".to_string(),
            "crates/codesmith/src/main.rs".to_string(),
            "src/main.rs".to_string(),
        ];
        let got = filter(&files, "main", |f| f.as_str(), true, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(files[got[0]], "src/main.rs", "shorter path first on a tie");
        let got = filter(&files, "tuiapp", |f| f.as_str(), true, 10);
        assert_eq!(files[got[0]], "crates/codesmith/src/tui/app.rs");
        assert_eq!(filter(&files, "", |f| f.as_str(), true, 2), vec![0, 1]);
        assert!(filter(&files, "zzz", |f| f.as_str(), true, 10).is_empty());
    }

    #[test]
    fn popups() {
        let cmds = items(&["new", "review", "redo", "sessions"]);
        let p = popup("/re", 3, &cmds, &[], &[]).unwrap();
        assert_eq!(p.kind, Kind::Slash);
        assert!(p.items.iter().all(|i| i.label.starts_with('r')));
        assert!(
            popup("/review", 7, &cmds, &[], &[]).is_none(),
            "exact match: done"
        );
        assert!(popup("hello", 5, &cmds, &[], &[]).is_none());
        let files = vec!["a.rs".to_string(), "b/c.rs".to_string()];
        let p = popup("@c", 2, &cmds, &[], &files).unwrap();
        assert_eq!(p.items[0].label, "b/c.rs");
        assert_eq!(p.start, 0);
    }

    #[test]
    fn lists_project_files_respecting_gitignore() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::create_dir_all(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "").unwrap();
        std::fs::write(d.path().join("target/out"), "").unwrap();
        std::fs::write(d.path().join(".gitignore"), "target/\n").unwrap();
        let files = project_files(d.path(), 100);
        assert_eq!(files, vec!["src/", "src/a.rs"]);
    }
}
