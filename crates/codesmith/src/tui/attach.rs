//! `@path` mentions in a prompt. Like OpenCode, each mentioned file is
//! attached to the message as if the Read tool had been called on it
//! (directories get a listing), so the model sees the contents without a
//! round trip.

use std::path::Path;

/// Lines attached per file, as the Read tool's default page.
const MAX_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

/// Starts the attachments in a stored user message (used to hide them
/// when a session is replayed).
pub const MARKER: &str = "\n\nCalled the Read tool with the following input: ";

/// The `@path` words of `text` that name an existing file or directory
/// under `cwd`, in order, without duplicates.
pub fn mentions(text: &str, cwd: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let Some(p) = word.strip_prefix('@') else {
            continue;
        };
        // Trailing punctuation is not part of the path ("see @a.rs, then").
        let p = p.trim_end_matches([',', '.', ';', ':', ')', '!', '?', '"', '\'']);
        if p.is_empty() || out.iter().any(|o| o == p) {
            continue;
        }
        if cwd.join(p).exists() {
            out.push(p.to_string());
        }
    }
    out
}

fn listing(dir: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    let mut n = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                        n.push('/');
                    }
                    n
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.join("\n")
}

/// The Read tool's output for a whole file (its first page).
fn read_text(path: &str, text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let total = lines.len();
    let shown = total.min(MAX_LINES);
    let mut out = vec![if total == 0 {
        format!("Read file {path}, 0 lines")
    } else {
        format!("Read file {path}, lines 1-{shown}")
    }];
    for (i, l) in lines.iter().take(shown).enumerate() {
        let l = l.strip_suffix('\r').unwrap_or(l);
        let l: String = if l.chars().count() > MAX_LINE_CHARS {
            l.chars().take(MAX_LINE_CHARS).collect::<String>() + "..."
        } else {
            l.to_string()
        };
        out.push(format!("{}: {l}", i + 1));
    }
    if total > shown {
        out.push(format!(
            "[Output truncated. Continue reading with offset: {}]",
            shown + 1
        ));
    }
    out.join("\n")
}

/// The prompt as sent: the text followed by each mentioned file's contents.
/// Returns the text unchanged when nothing is mentioned.
pub fn expand(text: &str, cwd: &Path) -> (String, Vec<String>) {
    let found = mentions(text, cwd);
    let mut out = text.to_string();
    for m in &found {
        let abs = cwd.join(m);
        let shown = abs.display().to_string();
        let body = if abs.is_dir() {
            listing(&abs)
        } else {
            match std::fs::read(&abs) {
                Ok(bytes) if bytes.contains(&0) => {
                    format!("{m} is a binary file; its contents are not shown.")
                }
                Ok(bytes) => read_text(m, &String::from_utf8_lossy(&bytes)),
                Err(e) => format!("Unable to read {m}: {e}"),
            }
        };
        out.push_str(MARKER);
        out.push_str(&serde_json::json!({ "filePath": shown }).to_string());
        out.push('\n');
        out.push_str(&body);
    }
    (out, found)
}

/// The text the user typed, without attachments.
pub fn typed(stored: &str) -> &str {
    stored.split(MARKER).next().unwrap_or(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentioned_files_are_attached() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        let text = "explain @src/a.rs, and @src and @missing.rs";
        assert_eq!(mentions(text, d.path()), vec!["src/a.rs", "src"]);
        let (sent, files) = expand(text, d.path());
        assert_eq!(files.len(), 2);
        assert!(sent.starts_with(text));
        assert!(
            sent.contains("Read file src/a.rs, lines 1-1\n1: fn a() {}"),
            "{sent}"
        );
        assert!(sent.contains("\"filePath\""));
        assert!(sent.ends_with("a.rs"), "directory listing last: {sent}");
        assert_eq!(typed(&sent), text);
        let (plain, none) = expand("no mentions", d.path());
        assert_eq!(plain, "no mentions");
        assert!(none.is_empty());
    }
}
