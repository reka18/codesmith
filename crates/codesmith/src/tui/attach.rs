//! `@path` mentions and pasted file paths in a prompt. Like OpenCode, each
//! mentioned file is attached to the message as if the Read tool had been
//! called on it (directories get a listing, images and PDFs are attached as
//! media), so the model sees the contents without a round trip. Pasting the
//! path of an image or PDF (dragging it into the terminal) inserts an
//! `[Image 1]` placeholder whose file is attached when the prompt is sent.

use codesmith_ai::Media;
use codesmith_core::media::ImageLimits;
use std::path::{Path, PathBuf};

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

/// A prompt ready to send: the text with mentioned files' contents, the
/// images and PDFs to attach, and the mentions that were found.
#[derive(Debug, Default)]
pub struct Expanded {
    pub text: String,
    pub media: Vec<Media>,
    pub files: Vec<String>,
}

/// The prompt as sent: the text followed by each mentioned file's contents;
/// mentioned images, PDFs and videos are attached as media (and say so in the text).
/// Returns the text unchanged when nothing is mentioned.
pub fn expand(text: &str, cwd: &Path, limits: &ImageLimits) -> Expanded {
    let found = mentions(text, cwd);
    let mut out = text.to_string();
    let mut media = Vec::new();
    for m in &found {
        let abs = cwd.join(m);
        let attached = if abs.is_dir() {
            Ok(None)
        } else {
            codesmith_core::media::attachment(&abs, m, limits)
        };
        out.push_str(&attachment(&abs, m, attached, &mut media));
    }
    Expanded {
        text: out,
        media,
        files: found,
    }
}

/// What a prompt says about the file `abs` attached to it as if the Read
/// tool had been called on it (`m` names it): `attached` is the file as
/// media (pushed to `media`), `Ok(None)` to read it as text, or why it
/// cannot be.
pub fn attachment(
    abs: &Path,
    m: &str,
    attached: Result<Option<Media>, String>,
    media: &mut Vec<Media>,
) -> String {
    let body = if let Err(e) = attached {
        e
    } else if abs.is_dir() {
        listing(abs)
    } else if let Ok(Some(file)) = attached {
        let note = codesmith_core::media::read_note(&file);
        media.push(file);
        note.to_string()
    } else {
        match std::fs::read(abs) {
            Ok(bytes) if bytes.contains(&0) => {
                format!("{m} is a binary file; its contents are not shown.")
            }
            Ok(bytes) => read_text(m, &String::from_utf8_lossy(&bytes)),
            Err(e) => format!("Unable to read {m}: {e}"),
        }
    };
    let shown = abs.display().to_string();
    format!(
        "{MARKER}{}\n{body}",
        serde_json::json!({ "filePath": shown })
    )
}

/// A file waiting to be attached, shown in the prompt as `label`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub label: String,
    pub path: PathBuf,
}

/// Images, PDFs and video, by extension, as OpenCode decides for pasted
/// paths (it takes no video there).
fn is_media_path(p: &Path) -> bool {
    let ext = p
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "pdf" | "mp4" | "mov" | "webm"
    )
}

/// A `file://` URL as a path (local host only).
pub fn file_url(s: &str) -> Option<String> {
    let rest = s.strip_prefix("file://")?;
    let path = match rest.strip_prefix("localhost") {
        Some(p) => p,
        None if rest.starts_with('/') => rest,
        None => return None,
    };
    let mut out = Vec::new();
    let b = path.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok()?;
            let v = u8::from_str_radix(hex, 16).ok()?;
            if v == b'/' || v == b'\\' {
                return None;
            }
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Split pasted text into paths: whitespace separates them, quotes and
/// backslash escapes keep spaces (what terminals paste for dropped files),
/// and `file://` URLs are decoded. `None` for unbalanced quotes or more
/// than 32 paths.
fn split_paths(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    let push = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(file_url(cur).unwrap_or_else(|| cur.clone()));
            cur.clear();
        }
    };
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') if chars.peek().is_some() => cur.push(chars.next().unwrap()),
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '\\') if chars.peek().is_some() => cur.push(chars.next().unwrap()),
            (None, c) if c.is_whitespace() => push(&mut cur, &mut out),
            (None, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    push(&mut cur, &mut out);
    (out.len() <= 32).then_some(out)
}

/// The image and PDF files a paste names, resolved against `cwd`, when the
/// paste is nothing but such paths; `None` means it is ordinary text.
pub fn pasted_files(text: &str, cwd: &Path) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() || text.starts_with("http://") || text.starts_with("https://") {
        return None;
    }
    let paths = split_paths(text)?;
    let files: Vec<PathBuf> = paths.iter().map(|p| cwd.join(p)).collect();
    (!files.is_empty() && files.iter().all(|f| f.is_file() && is_media_path(f))).then_some(files)
}

/// The placeholder for the next attachment of `path`'s kind: `[Image N]`,
/// `[PDF N]` or `[Video N]`, numbered after those already pending.
pub fn label(pending: &[Pending], path: &Path) -> String {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let kind = match ext.as_str() {
        "pdf" => "PDF",
        "mp4" | "mov" | "webm" => "Video",
        _ => "Image",
    };
    let n = pending
        .iter()
        .filter(|p| p.label.starts_with(&format!("[{kind} ")))
        .count();
    format!("[{kind} {}]", n + 1)
}

/// Load the pending attachments whose placeholder is still in `text`.
pub fn load(pending: &[Pending], text: &str, limits: &ImageLimits) -> Result<Vec<Media>, String> {
    let mut out = Vec::new();
    for p in pending.iter().filter(|p| text.contains(&p.label)) {
        let name = p
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.path.display().to_string());
        match codesmith_core::media::attachment(&p.path, &name, limits)? {
            Some(m) => out.push(m),
            None => return Err(format!("{name} is not an image, PDF or video")),
        }
    }
    Ok(out)
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
        let e = expand(text, d.path(), &ImageLimits::default());
        let (sent, files) = (e.text, e.files);
        assert_eq!(files.len(), 2);
        assert!(sent.starts_with(text));
        assert!(
            sent.contains("Read file src/a.rs, lines 1-1\n1: fn a() {}"),
            "{sent}"
        );
        assert!(sent.contains("\"filePath\""));
        assert!(sent.ends_with("a.rs"), "directory listing last: {sent}");
        assert_eq!(typed(&sent), text);
        let plain = expand("no mentions", d.path(), &ImageLimits::default());
        assert_eq!(plain.text, "no mentions");
        assert!(plain.files.is_empty() && plain.media.is_empty());
    }

    const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    fn pixel(path: &Path) {
        use base64::Engine;
        std::fs::write(
            path,
            base64::engine::general_purpose::STANDARD
                .decode(PIXEL)
                .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn mentioned_images_and_pdfs_are_media() {
        let d = tempfile::tempdir().unwrap();
        pixel(&d.path().join("shot.png"));
        std::fs::write(d.path().join("r.pdf"), b"%PDF-1.7\n").unwrap();
        let e = expand(
            "look at @shot.png and @r.pdf",
            d.path(),
            &ImageLimits::default(),
        );
        assert_eq!(e.media.len(), 2);
        assert_eq!(e.media[0].inline(), Some(PIXEL));
        assert_eq!(e.media[0].filename.as_deref(), Some("shot.png"));
        assert_eq!(e.media[1].mime, "application/pdf");
        assert!(e.text.contains("Image read successfully"));
        assert!(e.text.contains("PDF read successfully"));
        assert_eq!(typed(&e.text), "look at @shot.png and @r.pdf");
    }

    #[test]
    fn pasted_paths_become_attachments() {
        let d = tempfile::tempdir().unwrap();
        pixel(&d.path().join("my shot.png"));
        std::fs::write(d.path().join("r.pdf"), b"%PDF-1.7\n").unwrap();
        std::fs::write(d.path().join("notes.txt"), "x").unwrap();
        let abs = d.path().join("my shot.png");
        // Terminals escape spaces, quote, or paste file URLs.
        for paste in [
            "my\\ shot.png".to_string(),
            "'my shot.png'".to_string(),
            format!("file://{}", abs.display().to_string().replace(' ', "%20")),
        ] {
            assert_eq!(
                pasted_files(&paste, d.path()),
                Some(vec![abs.clone()]),
                "{paste}"
            );
        }
        assert_eq!(
            pasted_files("my\\ shot.png r.pdf", d.path()).unwrap().len(),
            2
        );
        // Not only media files, missing files, URLs and prose stay text.
        assert_eq!(pasted_files("notes.txt", d.path()), None);
        assert_eq!(pasted_files("r.pdf notes.txt", d.path()), None);
        assert_eq!(pasted_files("gone.png", d.path()), None);
        assert_eq!(pasted_files("https://x.test/a.png", d.path()), None);
        assert_eq!(pasted_files("fix the bug", d.path()), None);

        let mut pending = Vec::new();
        for f in ["my shot.png", "r.pdf", "my shot.png"] {
            let path = d.path().join(f);
            pending.push(Pending {
                label: label(&pending, &path),
                path,
            });
        }
        let labels: Vec<&str> = pending.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["[Image 1]", "[PDF 1]", "[Image 2]"]);
        let media = load(
            &pending,
            "compare [Image 1] with [PDF 1]",
            &ImageLimits::default(),
        )
        .unwrap();
        assert_eq!(media.len(), 2, "[Image 2] was deleted from the prompt");
        assert_eq!(media[0].filename.as_deref(), Some("my shot.png"));
    }

    #[test]
    fn videos_are_attached_by_mention_and_paste() {
        let d = tempfile::tempdir().unwrap();
        let mp4 = b"\0\0\0\x18ftypisom\0\0\x02\0isommp41";
        std::fs::write(d.path().join("clip.mp4"), mp4).unwrap();
        let e = expand("watch @clip.mp4", d.path(), &ImageLimits::default());
        assert_eq!(e.media.len(), 1);
        assert_eq!(e.media[0].mime, "video/mp4");
        assert!(e.text.contains("Video read successfully"));

        let path = d.path().join("clip.mp4");
        assert_eq!(pasted_files("clip.mp4", d.path()), Some(vec![path.clone()]));
        let pending = vec![Pending {
            label: label(&[], &path),
            path,
        }];
        assert_eq!(pending[0].label, "[Video 1]");
        let media = load(&pending, "see [Video 1]", &ImageLimits::default()).unwrap();
        assert_eq!(media[0].mime, "video/mp4");
    }
}
