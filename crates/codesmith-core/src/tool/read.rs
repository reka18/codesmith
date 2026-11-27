//! `read`: a file (numbered lines) or a directory (one entry per line).
//! Ported from OpenCode's read tool.

use super::{opt_u64, str_arg, Ctx, Outcome, Output};
use crate::permission::Request;
use serde_json::{json, Value};
use std::path::Path;

pub const MAX_LINES: usize = 2_000;
pub const MAX_LINE_CHARS: usize = 2_000;
pub const MAX_BYTES: usize = 50 * 1024;

pub fn spec() -> (String, Value) {
    (
        "Read the contents of a file or directory. Supports text files, images, and PDFs. Each text line is prefixed by its 1-based line number as <line>: <content>. The prefix is for reference and is not part of the file content. Directory entries are returned one per line. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files.".into(),
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File or directory to read"},
                "offset": {"type": "integer", "description": "The line or directory entry to start reading from (1-based)"},
                "limit": {"type": "integer", "minimum": 1, "description": "The maximum number of lines or directory entries to read (defaults to and capped at 2000)"}
            },
            "required": ["path"]
        }),
    )
}

/// The requested page: `offset` may be negative (lines from the end).
#[derive(Debug, Clone, Copy, Default)]
pub struct Page {
    pub offset: Option<i64>,
    pub limit: Option<usize>,
}

enum Miss {
    NotFound,
    Fail(String),
}

pub fn run(ctx: &Ctx, input: &Value) -> Outcome {
    let path = str_arg(input, "path")?;
    let limit = opt_u64(input, "limit");
    if limit == Some(0) {
        return Err("Limit must be a positive integer".into());
    }
    let page = Page {
        offset: input.get("offset").and_then(Value::as_i64),
        limit: limit.map(|l| l as usize),
    };
    let target = ctx.resolve(path);
    let is_dir = target.absolute.is_dir();
    ctx.authorize_external(&target, is_dir, super::Access::Read)?;
    ask_read(ctx, &target)?;
    match read_target(path, &target, page) {
        Ok(out) => return Ok(out),
        Err(Miss::Fail(e)) => return Err(e),
        Err(Miss::NotFound) => {}
    }
    // A name that differs only in look-alike spaces or quotes, such as the
    // narrow no-break space macOS puts in screenshot names, is read instead.
    // It is a sibling, so no new external_directory approval is needed.
    if let Some(alt) = alternate(&target.absolute) {
        let alt_target = ctx.resolve(&alt.display().to_string());
        ask_read(ctx, &alt_target)?;
        let name = alt.file_name().unwrap_or_default().to_string_lossy();
        match read_target(&sibling(path, &name), &alt_target, page) {
            Ok(out) if !alt.is_dir() => return Ok(out),
            Err(Miss::Fail(e)) => return Err(e),
            _ => {}
        }
    }
    Err(missing(ctx, path, &target.absolute))
}

/// The checks `read` makes before reading a file, for files read on the
/// user's behalf (prompt attachments by path): `external_directory` when it
/// is outside the project, then `read`. Returns the file's absolute path.
pub fn authorize(ctx: &Ctx, path: &str) -> Result<std::path::PathBuf, String> {
    match authorize_any(ctx, path)? {
        (_, true) => Err(format!(
            "Cannot attach a directory: {}",
            ctx.resolve(path).resource
        )),
        (p, false) => Ok(p),
    }
}

/// [`authorize`] for a file or a directory: its absolute path, and
/// whether it is a directory.
pub fn authorize_any(ctx: &Ctx, path: &str) -> Result<(std::path::PathBuf, bool), String> {
    authorize_target(ctx, path).map(|(t, is_dir)| (t.absolute, is_dir))
}

/// [`authorize_any`], keeping the resolved target: its real path is what
/// was checked, under every name it goes by (see [`super::Target`]).
pub(crate) fn authorize_target(ctx: &Ctx, path: &str) -> Result<(super::Target, bool), String> {
    let target = ctx.resolve(path);
    let is_dir = target.absolute.is_dir();
    ctx.authorize_external(&target, is_dir, super::Access::Read)?;
    ask_read(ctx, &target)?;
    Ok((target, is_dir))
}

/// A directory's entries, one per line, folders first (with a trailing
/// `/`) and then by name, at most [`MAX_LINES`] of them.
pub fn directory_listing(dir: &Path) -> Result<String, String> {
    let mut entries: Vec<(bool, String)> = std::fs::read_dir(dir)
        .map_err(|e| format!("Unable to read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let t = e.file_type().ok()?;
            Some((
                t.is_dir(),
                if t.is_dir() { format!("{name}/") } else { name },
            ))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| locale_cmp(&a.1, &b.1)));
    let more = entries.len().saturating_sub(MAX_LINES);
    let mut out: Vec<String> = entries.into_iter().take(MAX_LINES).map(|e| e.1).collect();
    if more > 0 {
        out.push(format!("... and {more} more"));
    }
    Ok(out.join("\n"))
}

pub(crate) fn ask_read(ctx: &Ctx, target: &super::Target) -> Result<(), String> {
    ctx.ask(Request {
        action: "read".into(),
        resources: target.resources.clone(),
        save: vec!["*".into()],
        title: format!("Read {}", target.resource),
        access: None,
    })
}

/// Read a file or directory; `display` names it in the heading. Images and
/// PDFs come back as media for the model to see, named `display`.
fn read_target(display: &str, target: &super::Target, page: Page) -> Result<Output, Miss> {
    let abs = &target.absolute;
    let resource = &target.resource;
    let meta = match std::fs::metadata(abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Miss::NotFound),
        Err(e) => return Err(Miss::Fail(format!("Unable to read {display}: {e}"))),
    };
    if meta.is_dir() {
        if page.offset.is_some_and(|o| o < 0) {
            return Err(Miss::Fail(format!(
                "A negative offset only works for files, and {resource} is a directory"
            )));
        }
        return directory(display, abs, page)
            .map(Output::bounded)
            .map_err(Miss::Fail);
    }
    if !meta.is_file() {
        return Err(Miss::Fail(format!(
            "Path is not a file or directory: {resource}"
        )));
    }
    let bytes = match std::fs::read(abs) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Miss::NotFound),
        Err(e) => return Err(Miss::Fail(format!("Unable to read {display}: {e}"))),
    };
    if let Some(mime) = media(&bytes) {
        let max = crate::media::MAX_INGEST_BYTES;
        if bytes.len() as u64 > max {
            return Err(Miss::Fail(format!(
                "Media exceeds {max} byte ingestion limit: {resource}"
            )));
        }
        let m = codesmith_ai::Media::base64(mime, crate::media::base64(&bytes)).named(display);
        return Ok(Output::media(crate::media::read_note(&m), vec![m]));
    }
    let text = text_page(&bytes, page)
        .ok_or_else(|| Miss::Fail(format!("Cannot read binary file: {resource}")))?;
    Ok(Output::bounded(model_text(display, page.offset, &text)))
}

/// The media type attached natively, detected from the bytes (see
/// `media::detect`).
pub fn media(b: &[u8]) -> Option<&'static str> {
    crate::media::detect(b)
}

/// NUL in the first 8 KiB means binary, as git and ripgrep decide.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

const TAIL_LINES: usize = 100;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// A page of text lines (OpenCode's `TextContent`).
#[derive(Debug, PartialEq)]
pub struct Text {
    pub lines: Vec<String>,
    /// Line number of the first line.
    pub offset: usize,
    /// The line to continue from, when the page stops before the end.
    pub next: Option<usize>,
}

/// Cut a line longer than `MAX_LINE_CHARS` UTF-16 units, as OpenCode counts.
fn clamp(line: &str) -> String {
    if line.encode_utf16().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let mut units = 0;
    let head: String = line
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= MAX_LINE_CHARS
        })
        .collect();
    format!("{head}... (line truncated to {MAX_LINE_CHARS} chars)")
}

fn decode_line(b: &[u8]) -> String {
    let text = String::from_utf8_lossy(b);
    clamp(text.strip_suffix('\r').unwrap_or(&text))
}

/// Select a page of lines from a file's bytes, the way OpenCode's
/// `ReadToolFileSystem.read` does. `None` means the page touches binary data.
pub fn text_page(bytes: &[u8], page: Page) -> Option<Text> {
    let body = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    let paged = bytes.len() > MAX_BYTES || page.offset.is_some() || page.limit.is_some();
    if !paged {
        if bytes.contains(&0) {
            return None;
        }
        let pieces: Vec<&[u8]> = body.split(|b| *b == b'\n').collect();
        let count = pieces.len() - usize::from(pieces.last().is_some_and(|p| p.is_empty()));
        if count <= MAX_LINES {
            let lines: Vec<String> = pieces[..count].iter().map(|l| decode_line(l)).collect();
            let size = lines.iter().map(String::len).sum::<usize>() + pieces.len() - 1;
            if size <= MAX_BYTES {
                return Some(Text {
                    lines,
                    offset: 1,
                    next: None,
                });
            }
        }
    }
    // Physical lines as (start, end without newline, end with newline).
    let mut ranges = Vec::new();
    let mut start = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            ranges.push((start, i, i + 1));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        ranges.push((start, bytes.len(), bytes.len()));
    }
    let total = ranges.len() as i64;
    let requested = match page.offset {
        None | Some(0) => 1,
        Some(o) => o,
    };
    let limit = page.limit.unwrap_or(MAX_LINES).min(MAX_LINES);
    let tail = requested > total;
    let mut from = if requested > 0 && !tail {
        requested
    } else {
        let back = if tail {
            -(page.limit.unwrap_or(TAIL_LINES).min(TAIL_LINES) as i64)
        } else {
            requested
        };
        (total + 1 + back).max(1)
    } as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut size = 0;
    let mut next = None;
    let mut consumed = 0;
    for line in from..=ranges.len() {
        if !tail && (lines.len() >= limit || size >= MAX_BYTES) {
            next = Some(line);
            break;
        }
        let (s, e, end) = ranges[line - 1];
        let raw = &bytes[s..e];
        let raw = if s == 0 {
            raw.strip_prefix(UTF8_BOM).unwrap_or(raw)
        } else {
            raw
        };
        let text = decode_line(raw);
        let cost = text.len() + usize::from(!lines.is_empty());
        if !tail && size + cost > MAX_BYTES {
            next = Some(line);
            break;
        }
        lines.push(text);
        size += cost;
        consumed = end;
    }
    if tail {
        while size > MAX_BYTES && !lines.is_empty() {
            size = size.saturating_sub(lines.remove(0).len() + 1);
            from += 1;
        }
        consumed = bytes.len();
    }
    if bytes
        .iter()
        .position(|b| *b == 0)
        .is_some_and(|n| n < consumed)
    {
        return None;
    }
    Some(Text {
        lines,
        offset: from,
        next,
    })
}

/// Format a page of a text file the way OpenCode's `toModelContent` does.
pub fn model_text(path: &str, offset: Option<i64>, t: &Text) -> String {
    let start = t.offset;
    let total = start + t.lines.len() - 1;
    let mut header = if t.lines.is_empty() {
        format!("Read file {path}, 0 lines")
    } else {
        format!("Read file {path}, lines {start}-{total}")
    };
    // A page that starts before the requested offset is the file's tail.
    if let Some(o) = offset.filter(|o| (start as i64) < *o) {
        header.push_str(&format!(
            " (offset {o} is past the end; the file has {total} {})",
            if total == 1 { "line" } else { "lines" }
        ));
    }
    let mut out = vec![header];
    for (i, l) in t.lines.iter().enumerate() {
        out.push(format!("{}: {l}", start + i));
    }
    if let Some(n) = t.next {
        out.push(format!(
            "[Output truncated. Continue reading with offset: {n}]"
        ));
    }
    out.join("\n")
}

/// Format a page of a text file (whole-file text in, model text out).
#[cfg(test)]
fn file(path: &str, text: &str, offset: Option<usize>, limit: usize) -> String {
    let page = Page {
        offset: offset.map(|o| o as i64),
        limit: Some(limit),
    };
    let t = text_page(text.as_bytes(), page).unwrap_or(Text {
        lines: vec![],
        offset: 1,
        next: None,
    });
    model_text(path, page.offset, &t)
}

/// Approximates JavaScript's `localeCompare`: case-insensitive first, then
/// lowercase before uppercase.
fn locale_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| b.cmp(a))
}

fn directory(path: &str, dir: &Path, page: Page) -> Result<String, String> {
    let mut entries: Vec<(bool, String)> = std::fs::read_dir(dir)
        .map_err(|e| format!("Unable to read {path}: {e}"))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let t = e.file_type().ok()?;
            if t.is_dir() {
                Some((true, format!("{name}/")))
            } else if t.is_file() || t.is_symlink() {
                Some((false, name))
            } else {
                None
            }
        })
        .collect();
    // Folders first, then by name.
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| locale_cmp(&a.1, &b.1)));
    let start = page.offset.filter(|o| *o > 0).unwrap_or(1) as usize;
    let limit = page.limit.unwrap_or(MAX_LINES).min(MAX_LINES);
    let shown: Vec<&String> = entries
        .iter()
        .skip(start - 1)
        .take(limit)
        .map(|e| &e.1)
        .collect();
    let mut out = vec![if shown.is_empty() {
        format!("Read directory {path}, 0 entries")
    } else {
        format!(
            "Read directory {path}, entries {start}-{}",
            start + shown.len() - 1
        )
    }];
    out.extend(shown.iter().map(|s| s.to_string()));
    if start - 1 + shown.len() < entries.len() {
        out.push(format!(
            "[Output truncated. Continue reading with offset: {}]",
            start + shown.len()
        ));
    }
    Ok(out.join("\n"))
}

/// The one file in the same folder whose name matches ignoring look-alike
/// characters (OpenCode's `alternatePath`).
fn alternate(abs: &Path) -> Option<std::path::PathBuf> {
    let base = canonical(&abs.file_name()?.to_string_lossy());
    let dir = abs.parent()?;
    let matches: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| canonical(&e.file_name().to_string_lossy()) == base)
        .map(|e| e.path())
        .collect();
    match matches.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// `name` in the same folder as `input`, written the way `input` was
/// (Node's `join(dirname(input), name)`).
fn sibling(input: &str, name: &str) -> String {
    match input.trim_end_matches('/').rfind('/') {
        Some(0) => format!("/{name}"),
        Some(i) => format!("{}/{name}", &input[..i]),
        None => name.to_string(),
    }
}

fn basename(input: &str) -> &str {
    let t = input.trim_end_matches('/');
    t.rsplit('/').next().unwrap_or(t)
}

enum Listing {
    Found(Vec<String>),
    Missing,
    Unknown,
}

fn listing(dir: &Path) -> Listing {
    match std::fs::read_dir(dir) {
        Ok(r) => Listing::Found(
            r.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Listing::Missing,
        Err(_) => Listing::Unknown,
    }
}

fn did_you_mean(suggestions: &[String]) -> Option<String> {
    (!suggestions.is_empty()).then(|| {
        format!(
            "Did you mean one of these?\n{}",
            suggestions
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        )
    })
}

/// A path written from the wrong root, such as `/Users/me/src/a.ts` for a
/// project at `/Users/me/repo`, usually means `<project>/src/a.ts`
/// (OpenCode's `reroot`).
fn reroot(ctx: &Ctx, input: &str, abs: &Path) -> Vec<String> {
    let root = &ctx.cwd;
    let Some(up) = root.parent() else {
        return vec![];
    };
    if abs.starts_with(root) || !abs.starts_with(up) {
        return vec![];
    }
    let Ok(rest) = abs.strip_prefix(up) else {
        return vec![];
    };
    let candidate = root.join(rest);
    if !candidate.exists() {
        return vec![];
    }
    let t = ctx.resolve(&candidate.display().to_string());
    if !ctx.gate.allows("read", &t.resources) {
        return vec![];
    }
    vec![if input.starts_with('/') || input.starts_with("~/") {
        candidate.display().to_string()
    } else {
        t.resource
    }]
}

/// "File not found" with suggestions: a re-rooted path, close names in the
/// same folder, or the first missing folder and its nearest existing parent.
fn missing(ctx: &Ctx, input: &str, absolute: &Path) -> String {
    let mut msg = vec![format!("File not found: {input}")];
    let moved = reroot(ctx, input, absolute);
    let Some(folder) = absolute.parent() else {
        msg.extend(did_you_mean(&moved));
        return msg.join("\n\n");
    };
    match listing(folder) {
        Listing::Found(names) => {
            let mut s = moved;
            s.extend(
                similar(&names, basename(input))
                    .into_iter()
                    .map(|n| sibling(input, &n)),
            );
            msg.extend(did_you_mean(&s));
        }
        Listing::Unknown => msg.extend(did_you_mean(&moved)),
        Listing::Missing => {
            let mut child = folder.to_path_buf();
            while let Some(parent) = child.parent().map(Path::to_path_buf) {
                match listing(&parent) {
                    Listing::Unknown => break,
                    Listing::Missing => child = parent,
                    Listing::Found(names) => {
                        let wanted = child
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let mut s = moved;
                        s.extend(
                            similar(&names, &wanted)
                                .into_iter()
                                .map(|n| parent.join(n).display().to_string()),
                        );
                        msg.push(format!(
                            "The folder {} does not exist. The nearest folder that does is {}.",
                            child.display(),
                            parent.display()
                        ));
                        msg.extend(did_you_mean(&s));
                        return msg.join("\n\n");
                    }
                }
            }
            msg.extend(did_you_mean(&moved));
        }
    }
    msg.join("\n\n")
}

fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

fn canonical(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\u{00a0}' | '\u{202f}' => ' ',
            '\u{2018}' | '\u{2019}' => '\'',
            c => c,
        })
        .collect()
}

pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let v = (prev[j] + 1)
                .min(cur[j - 1] + 1)
                .min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
            cur.push(v);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Up to three names close to `requested`, best first (OpenCode's `similar`).
pub fn similar(names: &[String], requested: &str) -> Vec<String> {
    let name = requested.to_lowercase();
    let wanted = stem(&name).to_string();
    let typos = if wanted.chars().count() < 3 {
        0
    } else if wanted.chars().count() < 5 {
        1
    } else {
        2
    };
    let tier = |cand: &str| -> Option<u8> {
        if cand == name {
            return Some(1);
        }
        if canonical(cand) == canonical(&name) {
            return None;
        }
        let found = stem(cand);
        if found == wanted {
            return Some(2);
        }
        if found.chars().count().min(wanted.chars().count()) >= 3
            && (cand.contains(&name)
                || name.contains(cand)
                || found.contains(&wanted)
                || wanted.contains(found))
        {
            return Some(3);
        }
        let len_diff =
            (found.chars().count() as isize - wanted.chars().count() as isize).unsigned_abs();
        if typos > 0 && len_diff <= typos && edit_distance(found, &wanted) <= typos {
            return Some(4);
        }
        None
    };
    let mut ranked: Vec<(u8, usize, String)> = names
        .iter()
        .filter(|n| n.as_str() != requested)
        .filter_map(|n| {
            let lower = n.to_lowercase();
            tier(&lower).map(|t| (t, edit_distance(&lower, &name), n.clone()))
        })
        .collect();
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then_with(|| locale_cmp(&a.2, &b.2))
    });
    ranked.into_iter().take(3).map(|r| r.2).collect()
}

#[cfg(test)]
mod tests {
    use super::super::testing::Env;
    use super::super::Tool;
    use super::*;
    use crate::permission::Reply;

    #[test]
    fn numbered_lines() {
        assert_eq!(
            file("a.txt", "one\ntwo\n", None, MAX_LINES),
            "Read file a.txt, lines 1-2\n1: one\n2: two"
        );
        assert_eq!(
            file("a.txt", "one\r\ntwo", None, MAX_LINES),
            "Read file a.txt, lines 1-2\n1: one\n2: two"
        );
    }

    #[test]
    fn empty_file() {
        assert_eq!(file("e", "", None, MAX_LINES), "Read file e, 0 lines");
    }

    #[test]
    fn offset_and_limit() {
        let t: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        assert_eq!(file("f", &t, Some(4), 2), "Read file f, lines 4-5\n4: l4\n5: l5\n[Output truncated. Continue reading with offset: 6]");
        assert_eq!(
            file("f", &t, Some(9), 5),
            "Read file f, lines 9-10\n9: l9\n10: l10"
        );
    }

    #[test]
    fn offset_past_end() {
        // Past the end, the file's tail comes back (OpenCode's tail page).
        assert_eq!(
            file("f", "a\nb\n", Some(5), 10),
            "Read file f, lines 1-2 (offset 5 is past the end; the file has 2 lines)\n1: a\n2: b"
        );
        assert_eq!(
            file("f", "a\n", Some(3), 10),
            "Read file f, lines 1-1 (offset 3 is past the end; the file has 1 line)\n1: a"
        );
        assert_eq!(
            file("f", "", Some(3), 10),
            "Read file f, 0 lines (offset 3 is past the end; the file has 0 lines)"
        );
    }

    #[test]
    fn long_lines_are_cut() {
        let long = "x".repeat(MAX_LINE_CHARS + 500);
        let out = file("f", &long, None, MAX_LINES);
        assert!(out.ends_with("... (line truncated to 2000 chars)"));
        assert!(out.len() < long.len());
    }

    #[test]
    fn byte_cap_pages_the_file() {
        let line = "y".repeat(1000);
        let t: String = (0..200).map(|_| format!("{line}\n")).collect();
        let out = file("f", &t, None, MAX_LINES);
        assert!(out.contains("[Output truncated. Continue reading with offset: "));
        assert!(out.len() < 60 * 1024);
    }

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary("héllo".as_bytes()));
    }

    #[test]
    fn similar_names() {
        let names: Vec<String> = ["config.rs", "Config.toml", "main.rs", "lib.rs", "confg.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let s = similar(&names, "config.ts");
        assert!(s.contains(&"config.rs".to_string()), "{s:?}");
        assert!(similar(&names, "zzz.rs").is_empty());
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_files_and_directories() {
        let env = Env::new();
        env.write("src/a.rs", "fn a() {}\n");
        env.write("src/b.rs", "");
        let ctx = env.ctx();
        assert_eq!(
            run(&ctx, &json!({"path": "src/a.rs"})).unwrap().content,
            "Read file src/a.rs, lines 1-1\n1: fn a() {}"
        );
        assert_eq!(
            run(&ctx, &json!({"path": "src"})).unwrap().content,
            "Read directory src, entries 1-2\na.rs\nb.rs"
        );
        assert_eq!(
            run(&ctx, &json!({"path": "."})).unwrap().content,
            "Read directory ., entries 1-1\nsrc/"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_file_suggests() {
        let env = Env::new();
        env.write("src/config.rs", "");
        let e = run(&env.ctx(), &json!({"path": "src/confg.rs"})).unwrap_err();
        assert!(e.starts_with("File not found: src/confg.rs"), "{e}");
        assert!(e.contains("config.rs"), "{e}");
        let e = run(&env.ctx(), &json!({"path": "nope/dir/x.rs"})).unwrap_err();
        assert!(
            e.contains("does not exist. The nearest folder that does is"),
            "{e}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn env_files_need_approval() {
        let env = Env::with_reply(Reply::Reject(None));
        env.write(".env", "SECRET=1");
        let e = run(&env.ctx(), &json!({"path": ".env"})).unwrap_err();
        assert_eq!(e, "The user declined this request.");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_zero_limit_binary_and_media() {
        let env = Env::new();
        let p = env.root().join("bin");
        std::fs::write(&p, b"\0\x01").unwrap();
        std::fs::write(env.root().join("img.png"), b"\x89PNG\r\n\x1a\n\0\0").unwrap();
        let ctx = env.ctx();
        assert_eq!(
            run(&ctx, &json!({"path": "bin", "limit": 0})).unwrap_err(),
            "Limit must be a positive integer"
        );
        assert!(run(&ctx, &json!({"path": "bin"}))
            .unwrap_err()
            .starts_with("Cannot read binary file"));
        let img = run(&ctx, &json!({"path": "img.png"})).unwrap();
        assert_eq!(img.content, "Image read successfully");
        assert_eq!(img.media[0].mime, "image/png");
    }

    // ---------------------------------------------------------------------
    // Ported from OpenCode's packages/core/test/tool-read.test.ts (MIT).
    // One `oc_read_*` test per OpenCode case, in the same order.
    //
    // Intentional differences:
    // - Media is detected by its bytes and returned as `Output.media`; the
    //   session fits images to the limits afterwards (`media::fit_tool_media`,
    //   OpenCode's Tool.execute step), so the image cases run both. Cases
    //   that call Image.Service directly are in `media.rs`. There is no
    //   structured `output` (file URI) beside the model content.
    // - No Code Mode catalog placement and no structured `output`/`metadata`:
    //   the model-facing text is asserted.
    // - No virtual `skill://` filesystem: CodeSmith skills are folders on disk,
    //   so `skill://` paths are ordinary missing paths.
    // - Permission plumbing: a `Recorder` asker observes requests; rejections
    //   read "The user declined this request."; external_directory requests
    //   name the file (with a `<dir>/*` save rule) rather than `<dir>/*`.
    // - Folder-name canonicalization skips Unicode NFC (no normalization
    //   crate); look-alike spaces and quotes are still folded.
    // - OpenCode's whole-file path keeps a trailing `\r` on each line; CodeSmith
    //   strips it everywhere, matching OpenCode's paged path and edit's LF view.
    // - Environment-level read counters and injected defects are replaced by
    //   real filesystem conditions (a socket, an unreadable file).
    // ---------------------------------------------------------------------

    use super::super::testing::Recorder;
    use std::sync::Arc;

    const PIXEL: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    fn b64(s: &str) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(s).unwrap()
    }

    fn rec_env() -> (Env, Arc<Recorder>) {
        Env::recording(vec![])
    }

    fn read_path(ctx: &Ctx, path: &str) -> Outcome {
        run(ctx, &json!({"path": path}))
    }

    /// The file is read as media named `name`, with the model text OpenCode sends.
    fn assert_media(bytes: &[u8], name: &str, mime: &str) -> Output {
        let env = Env::new();
        std::fs::write(env.root().join(name), bytes).unwrap();
        let out = read_path(&env.ctx(), name).unwrap();
        let text = if mime == "application/pdf" {
            "PDF read successfully"
        } else {
            "Image read successfully"
        };
        assert_eq!(out.content, text);
        assert_eq!(
            out.media,
            vec![codesmith_ai::Media::base64(mime, crate::media::base64(bytes)).named(name)]
        );
        out
    }

    /// Read `bytes` as `name`, then fit its images as the session does.
    fn read_fitted(
        bytes: &[u8],
        name: &str,
        limits: crate::media::ImageLimits,
    ) -> (String, Vec<codesmith_ai::Media>) {
        let env = Env::new();
        std::fs::write(env.root().join(name), bytes).unwrap();
        let out = read_path(&env.ctx(), name).unwrap();
        let (media, notes) = crate::media::fit_tool_media(out.media, &limits);
        (
            std::iter::once(out.content)
                .chain(notes)
                .collect::<Vec<_>>()
                .join("\n"),
            media,
        )
    }

    /// A white 16x1 PNG (OpenCode's "wide" fixture).
    fn wide_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(16, 1, image::Rgba([255, 255, 255, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_registers_authorizes_and_reads() {
        let (env, rec) = rec_env();
        env.write("README.md", "hello");
        assert!(Tool::Read
            .spec("/bin/sh")
            .description
            .contains("Each text line is prefixed by its 1-based line number"));
        assert_eq!(
            read_path(&env.ctx(), "README.md").unwrap().content,
            "Read file README.md, lines 1-1\n1: hello"
        );
        let asked = rec.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            (
                asked[0].action.as_str(),
                asked[0].resources.clone(),
                asked[0].save.clone()
            ),
            ("read", vec!["README.md".to_string()], vec!["*".to_string()])
        );
    }

    #[test]
    fn oc_read_code_mode_placement() {
        // CodeSmith has no full_codemode catalog; the direct tool keeps the
        // line-number description OpenCode shows when read is demoted to direct.
        let spec = Tool::Read.spec("/bin/sh");
        assert_eq!(spec.name, "read");
        assert!(spec
            .description
            .contains("Each text line is prefixed by its 1-based line number"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_rejects_zero_limit_before_reading() {
        let (env, rec) = rec_env();
        env.write("README.md", "hello");
        assert_eq!(
            run(&env.ctx(), &json!({"path": "README.md", "limit": 0})).unwrap_err(),
            "Limit must be a positive integer"
        );
        assert!(rec.actions().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_asks_external_directory_before_external_read() {
        let (env, rec) = rec_env();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().canonicalize().unwrap().join("notes.txt");
        std::fs::write(&external, "hello").unwrap();
        let abs = external.display().to_string();
        assert!(read_path(&env.ctx(), &abs).is_ok());
        assert_eq!(
            rec.requests(),
            vec![
                ("external_directory".to_string(), vec![abs.clone()]),
                ("read".to_string(), vec![abs]),
            ]
        );
        let save = rec.asked.lock().unwrap()[0].save.clone();
        assert_eq!(
            save,
            vec![format!(
                "{}/*",
                outside.path().canonicalize().unwrap().display()
            )]
        );
    }

    #[test]
    fn oc_read_small_png_as_media() {
        let out = assert_media(&b64(PIXEL), "pixel.png", "image/png");
        assert!(out.bounded);
        // Reading again gives the same media (nothing is cached or moved).
        assert_media(&b64(PIXEL), "pixel.png", "image/png");
    }

    #[test]
    fn oc_read_png_above_text_limit_as_media() {
        let img = image::RgbaImage::from_fn(256, 256, |x, y| {
            let i = (y * 256 + x) * 4;
            let c = |ch: u32| (((i + ch) * 73 + ((i + ch) >> 3)) % 256) as u8;
            image::Rgba([c(0), c(1), c(2), c(3)])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();
        assert!(crate::media::base64(&png).len() > MAX_BYTES);
        assert_media(&png, "large.png", "image/png");
        // Small enough PNGs pass the default limits untouched.
        let (_, media) = read_fitted(&png, "large.png", Default::default());
        assert_eq!(media[0].inline(), Some(crate::media::base64(&png).as_str()));
    }

    #[test]
    fn oc_read_original_image_when_resizer_unavailable() {
        // CodeSmith always has its resizer; a fitting image is sent as read.
        let (text, media) = read_fitted(&b64(PIXEL), "pixel.png", Default::default());
        assert_eq!(text, "Image read successfully");
        assert_eq!(media[0].inline(), Some(PIXEL));
    }

    #[test]
    fn oc_read_drops_undecodable_image_data() {
        let (text, media) = read_fitted(&b64("iVBORw0KGgo="), "truncated.png", Default::default());
        assert_eq!(
            text,
            "Image read successfully\n[1 image omitted: could not be decoded.]"
        );
        assert!(media.is_empty());
    }

    #[test]
    fn oc_read_drops_oversized_images_without_resizing() {
        let limits = crate::media::ImageLimits {
            auto_resize: false,
            max_width: 4,
            ..Default::default()
        };
        let (text, media) = read_fitted(&wide_png(), "wide.png", limits);
        assert_eq!(
            text,
            "Image read successfully\n[1 image omitted: could not be resized below the image size limit.]"
        );
        assert!(media.is_empty());
    }

    #[test]
    fn oc_read_resizes_images_to_configured_dimensions() {
        let limits = crate::media::ImageLimits {
            max_width: 4,
            ..Default::default()
        };
        let (_, media) = read_fitted(&wide_png(), "wide.png", limits);
        let bytes = b64(media[0].inline().unwrap());
        let img = image::load_from_memory(&bytes).unwrap();
        assert!(img.width() <= 4 && img.height() <= 2_000);
    }

    #[test]
    fn oc_read_large_opaque_png_screenshots_as_jpeg() {
        // The transcode itself is `media::tests::oc_image_stores_large_opaque_png_screenshots_as_jpeg`;
        // a JPEG photo is read as JPEG media.
        assert_media(b"\xff\xd8\xff\xe0\0\x10JFIF", "photo.jpg", "image/jpeg");
    }

    #[test]
    fn oc_read_keeps_fitting_pngs() {
        let (_, media) = read_fitted(&b64(PIXEL), "image.png", Default::default());
        assert_eq!(media[0].inline(), Some(PIXEL));
    }

    #[test]
    fn oc_read_png_candidates_at_base64_limit() {
        // See `media::tests::oc_image_accepts_png_candidates_at_the_exact_base64_limit_…`.
        let limits = crate::media::ImageLimits {
            max_width: 4,
            max_base64_bytes: 1_024,
            ..Default::default()
        };
        let (_, media) = read_fitted(&wide_png(), "wide.png", limits);
        assert_eq!(media[0].mime, "image/png");
        assert!(media[0].inline_len() <= 1_024);
    }

    #[test]
    fn oc_read_drops_images_that_cannot_fit_max_bytes() {
        let limits = crate::media::ImageLimits {
            max_base64_bytes: 1,
            ..Default::default()
        };
        let (text, media) = read_fitted(&b64(PIXEL), "pixel.png", limits);
        assert_eq!(
            text,
            "Image read successfully\n[1 image omitted: could not be resized below the image size limit.]"
        );
        assert!(media.is_empty());
    }

    #[test]
    fn oc_read_image_despite_misleading_extension() {
        // Detected by content, so a PNG named .bin is media, not binary.
        assert_media(&b64(PIXEL), "pixel.bin", "image/png");
        assert_media(b"GIF89a\x01\0\x01\0", "anim.dat", "image/gif");
        assert_media(b"RIFF\x10\0\0\0WEBPVP8 ", "pic.data", "image/webp");
    }

    #[test]
    fn oc_read_pdfs_as_media() {
        assert_media(&b64("JVBERi0xLjcK"), "document.pdf", "application/pdf");
    }

    #[test]
    fn oc_read_real_image_and_pdf_files() {
        assert_media(&b64(PIXEL), "pixel.png", "image/png");
        assert_media(b"%PDF-1.7\ncontent", "document.pdf", "application/pdf");
        // A media extension alone does not make text media.
        let env = Env::new();
        env.write("notes.png", "plain text");
        let out = read_path(&env.ctx(), "notes.png").unwrap();
        assert_eq!(out.content, "Read file notes.png, lines 1-1\n1: plain text");
        assert!(out.media.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_binary_failure_returned_to_model() {
        let env = Env::new();
        std::fs::write(env.root().join("archive.dat"), "one\narchive\u{0}\n").unwrap();
        let ctx = env.ctx();
        assert_eq!(
            run(
                &ctx,
                &json!({"path": "archive.dat", "offset": 2, "limit": 1})
            )
            .unwrap_err(),
            "Cannot read binary file: archive.dat"
        );
        // A page that stops before the NUL is still text, as in OpenCode.
        assert_eq!(
            run(&ctx, &json!({"path": "archive.dat", "offset": 1, "limit": 1}))
                .unwrap()
                .content,
            "Read file archive.dat, lines 1-1\n1: one\n[Output truncated. Continue reading with offset: 2]"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_preserves_actionable_failure_messages() {
        let env = Env::new();
        let _sock = std::os::unix::net::UnixListener::bind(env.root().join("target")).unwrap();
        assert_eq!(
            read_path(&env.ctx(), "target").unwrap_err(),
            "Path is not a file or directory: target"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_unexpected_filesystem_failures() {
        use std::os::unix::fs::PermissionsExt;
        let env = Env::new();
        let p = env.write("README.md", "hello");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&p).is_ok() {
            return; // running as root
        }
        let e = read_path(&env.ctx(), "README.md").unwrap_err();
        assert!(e.starts_with("Unable to read README.md: "), "{e}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_denied_permission_does_not_read() {
        let (env, rec) = rec_env();
        rec.deny_action("read");
        env.write("README.md", "hello");
        assert_eq!(
            read_path(&env.ctx(), "README.md").unwrap_err(),
            "The user declined this request."
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_missing_paths_are_tool_failures() {
        let (env, rec) = rec_env();
        for n in [
            "__missing_read_target__.txt.bak",
            "copy___missing_read_target__.txt",
            "old___missing_read_target__.txt",
            "other___missing_read_target__.txt",
            "unrelated.txt",
        ] {
            env.write(n, "x\n");
        }
        let missing = "__missing_read_target__.txt";
        assert_eq!(
            read_path(&env.ctx(), missing).unwrap_err(),
            format!("File not found: {missing}\n\nDid you mean one of these?\n__missing_read_target__.txt.bak\nold___missing_read_target__.txt\ncopy___missing_read_target__.txt")
        );
        assert_eq!(
            rec.requests(),
            vec![("read".to_string(), vec![missing.to_string()])]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_names_first_missing_folder_and_nearest_parent() {
        let env = Env::new();
        let root = env.root();
        std::fs::create_dir(root.join("__missing_read_folder__-old")).unwrap();
        let requested = "__missing_read_folder__/src/file.ts";
        assert_eq!(
            read_path(&env.ctx(), requested).unwrap_err(),
            [
                format!("File not found: {requested}"),
                format!(
                    "The folder {} does not exist. The nearest folder that does is {}.",
                    root.join("__missing_read_folder__").display(),
                    root.display()
                ),
                format!(
                    "Did you mean one of these?\n{}",
                    root.join("__missing_read_folder__-old").display()
                ),
            ]
            .join("\n\n")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_trusts_the_filesystem_about_folders() {
        let env = Env::new();
        let folder = env.root().join("__only_in_workspace__");
        env.write("__only_in_workspace__/file.tsx", "x\n");
        let requested = folder.join("file.ts").display().to_string();
        assert_eq!(
            read_path(&env.ctx(), &requested).unwrap_err(),
            format!(
                "File not found: {requested}\n\nDid you mean one of these?\n{}",
                folder.join("file.tsx").display()
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_suggests_close_names_from_same_folder() {
        let env = Env::new();
        for f in [
            "typo/config.ts",
            "typo/db.ts",
            "ext/config.yaml",
            "case/README.md",
            "short/db.ts",
            "short/io.ts",
            "short/api.ts",
            "short/build.ts",
        ] {
            env.write(f, "x\n");
        }
        let ctx = env.ctx();
        assert_eq!(
            read_path(&ctx, "typo/conifg.ts").unwrap_err(),
            "File not found: typo/conifg.ts\n\nDid you mean one of these?\ntypo/config.ts"
        );
        assert_eq!(
            read_path(&ctx, "ext/config.yml").unwrap_err(),
            "File not found: ext/config.yml\n\nDid you mean one of these?\next/config.yaml"
        );
        assert_eq!(
            read_path(&ctx, "short/ui.ts").unwrap_err(),
            "File not found: short/ui.ts"
        );
        // Case-insensitive filesystems, such as macOS by default, simply read the file.
        match read_path(&ctx, "case/readme.md") {
            Ok(_) => {}
            Err(e) => assert_eq!(
                e,
                "File not found: case/readme.md\n\nDid you mean one of these?\ncase/README.md"
            ),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_ranks_suggestions_and_shows_three() {
        let names: Vec<String> = ["acnfig.ts", "cconfig.ts", "config.json", "Config.ts"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Checked directly: a case-insensitive filesystem would read Config.ts.
        assert_eq!(
            similar(&names, "config.ts"),
            vec!["Config.ts", "config.json", "cconfig.ts"]
        );
        let env = Env::new();
        for n in &names[..3] {
            env.write(n, "x\n");
        }
        assert_eq!(
            read_path(&env.ctx(), "config.ts").unwrap_err(),
            "File not found: config.ts\n\nDid you mean one of these?\nconfig.json\ncconfig.ts\nacnfig.ts"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_suggests_path_from_parent_of_location_first() {
        let env = Env::new();
        let tmp = env.root();
        let repo = tmp.join("repo");
        env.write("repo/src/a.ts", "x\n");
        let mut ctx = env.ctx();
        ctx.cwd = repo.clone();
        ctx.root = repo.clone();
        let requested = tmp.join("src").join("a.ts").display().to_string();
        assert_eq!(
            read_path(&ctx, &requested).unwrap_err(),
            format!(
                "File not found: {requested}\n\nThe folder {} does not exist. The nearest folder that does is {}.\n\nDid you mean one of these?\n{}",
                tmp.join("src").display(),
                tmp.display(),
                repo.join("src/a.ts").display()
            )
        );
        env.write("src/a.tsx", "x\n");
        assert_eq!(
            read_path(&ctx, &requested).unwrap_err(),
            format!(
                "File not found: {requested}\n\nDid you mean one of these?\n{}\n{}",
                repo.join("src/a.ts").display(),
                tmp.join("src/a.tsx").display()
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_suggests_close_folder_names() {
        let env = Env::new();
        env.write("lib/x.ts", "x\n");
        let root = env.root();
        assert_eq!(
            read_path(&env.ctx(), "libs/x.ts").unwrap_err(),
            format!(
                "File not found: libs/x.ts\n\nThe folder {} does not exist. The nearest folder that does is {}.\n\nDid you mean one of these?\n{}",
                root.join("libs").display(),
                root.display(),
                root.join("lib").display()
            )
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_recovers_file_whose_nonbreaking_spaces_differ() {
        let (env, rec) = rec_env();
        let requested = "Screenshot 2026-08-27 3.15 PM.png";
        let recovered = "Screenshot 2026-08-27 3.15\u{202f}PM.png";
        env.write(recovered, "hello");
        assert_eq!(
            read_path(&env.ctx(), requested).unwrap().content,
            format!("Read file {recovered}, lines 1-1\n1: hello")
        );
        assert_eq!(
            rec.requests(),
            vec![
                ("read".to_string(), vec![requested.to_string()]),
                ("read".to_string(), vec![recovered.to_string()]),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_recovers_external_name_without_repeating_directory_approval() {
        let (env, rec) = rec_env();
        let outside = tempfile::tempdir().unwrap();
        let dir = outside.path().canonicalize().unwrap();
        let requested = dir.join("report final.txt").display().to_string();
        let recovered = dir.join("report\u{202f}final.txt");
        std::fs::write(&recovered, "hello").unwrap();
        assert!(read_path(&env.ctx(), &requested).is_ok());
        assert_eq!(
            rec.requests(),
            vec![
                ("external_directory".to_string(), vec![requested.clone()]),
                ("read".to_string(), vec![requested]),
                ("read".to_string(), vec![recovered.display().to_string()]),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_recovered_name_denied_by_own_rules() {
        let (env, rec) = rec_env();
        let recovered = "report\u{202f}final.txt";
        rec.deny_resource(recovered);
        env.write(recovered, "hello");
        let requested = env.root().join("report final.txt").display().to_string();
        assert_eq!(
            read_path(&env.ctx(), &requested).unwrap_err(),
            "The user declined this request."
        );
        assert_eq!(
            rec.requests(),
            vec![
                ("read".to_string(), vec!["report final.txt".to_string()]),
                ("read".to_string(), vec![recovered.to_string()]),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_does_not_recover_ambiguous_files() {
        let (env, rec) = rec_env();
        env.write("report\u{00a0}final.txt", "x\n");
        env.write("report\u{202f}final.txt", "x\n");
        assert_eq!(
            read_path(&env.ctx(), "report final.txt").unwrap_err(),
            "File not found: report final.txt"
        );
        assert_eq!(rec.actions().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_does_not_recover_a_directory() {
        let (env, rec) = rec_env();
        std::fs::create_dir(env.root().join("report\u{00a0}final")).unwrap();
        assert_eq!(
            read_path(&env.ctx(), "report final").unwrap_err(),
            "File not found: report final"
        );
        assert_eq!(rec.actions().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_lists_bounded_directory_page() {
        let (env, rec) = rec_env();
        std::fs::create_dir_all(env.root().join("src/assets")).unwrap();
        std::fs::create_dir_all(env.root().join("src/components")).unwrap();
        for n in ["index.ts", "z1.ts", "z2.ts"] {
            env.write(&format!("src/{n}"), "x\n");
        }
        assert_eq!(
            run(&env.ctx(), &json!({"path": "src", "offset": 2, "limit": 2}))
                .unwrap()
                .content,
            "Read directory src, entries 2-3\ncomponents/\nindex.ts\n[Output truncated. Continue reading with offset: 4]"
        );
        assert_eq!(
            rec.requests(),
            vec![("read".to_string(), vec!["src".to_string()])]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_zero_directory_offset_heading() {
        let env = Env::new();
        env.write("src/index.ts", "x\n");
        env.write("src/zzz.ts", "x\n");
        assert_eq!(
            run(&env.ctx(), &json!({"path": "src", "offset": 0, "limit": 1}))
                .unwrap()
                .content,
            "Read directory src, entries 1-1\nindex.ts\n[Output truncated. Continue reading with offset: 2]"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_denied_directory_is_not_listed() {
        let (env, rec) = rec_env();
        rec.deny_action("read");
        std::fs::create_dir(env.root().join("src")).unwrap();
        assert_eq!(
            read_path(&env.ctx(), "src").unwrap_err(),
            "The user declined this request."
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_pagination_with_continuation() {
        let env = Env::new();
        env.write("large.txt", "a\nhello\nc\n");
        assert_eq!(
            run(&env.ctx(), &json!({"path": "large.txt", "offset": 2, "limit": 1}))
                .unwrap()
                .content,
            "Read file large.txt, lines 2-2\n2: hello\n[Output truncated. Continue reading with offset: 3]"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_negative_offset_shows_real_line_numbers() {
        let env = Env::new();
        let lines: Vec<String> = (0..13)
            .map(|i| match i {
                8 => "nine".to_string(),
                9 => "ten".to_string(),
                _ => format!("line {}", i + 1),
            })
            .collect();
        env.write("log.txt", &lines.join("\n"));
        assert_eq!(
            run(&env.ctx(), &json!({"path": "log.txt", "offset": -5, "limit": 2}))
                .unwrap()
                .content,
            "Read file log.txt, lines 9-10\n9: nine\n10: ten\n[Output truncated. Continue reading with offset: 11]"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_negative_directory_offset_error() {
        let env = Env::new();
        std::fs::create_dir(env.root().join("src")).unwrap();
        assert_eq!(
            run(&env.ctx(), &json!({"path": "src", "offset": -5})).unwrap_err(),
            "A negative offset only works for files, and src is a directory"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_read_virtual_skill_paths() {
        // CodeSmith skills live on disk and the skill tool names their folder,
        // so a skill:// path is not special and reads as missing.
        let env = Env::new();
        for bad in ["skill://guide/references/style.md", "skill://guide/"] {
            let e = read_path(&env.ctx(), bad).unwrap_err();
            assert!(e.starts_with(&format!("File not found: {bad}")), "{e}");
        }
    }

    #[test]
    fn bom_and_byte_budget_paging() {
        // Whole-file reads drop a leading BOM (TextDecoder's default).
        assert_eq!(
            file("b", "\u{feff}hi\n", None, MAX_LINES),
            "Read file b, lines 1-1\n1: hi"
        );
        let t = text_page("\u{feff}hi\n".as_bytes(), Page::default()).unwrap();
        assert_eq!(t.lines, vec!["hi"]);
        // The byte budget stops before a line that would exceed it.
        let line = "y".repeat(1000);
        let big: String = (0..100).map(|_| format!("{line}\n")).collect();
        let t = text_page(big.as_bytes(), Page::default()).unwrap();
        assert_eq!(t.lines.len(), 51);
        assert_eq!(t.next, Some(52));
    }

    // --- Ports of tool-read-filesystem.test.ts from PARITY.md's backlog ---

    fn page(bytes: &[u8], offset: Option<i64>, limit: Option<usize>) -> Option<Text> {
        text_page(bytes, Page { offset, limit })
    }

    fn read_text(env: &Env, name: &str, input: Value) -> String {
        let mut input = input;
        input["path"] = json!(name);
        run(&env.ctx(), &input).unwrap().content
    }

    #[test]
    fn oc_read_applies_the_default_line_limit_at_physical_line_boundaries() {
        let prefix = "x\r\n".repeat(1_999);
        for ending in ["last", "last\n", "last\r\n", "\n"] {
            let t = page(format!("{prefix}{ending}").as_bytes(), None, None).unwrap();
            assert_eq!(
                (t.offset, t.lines.len(), t.next),
                (1, 2_000, None),
                "{ending:?}"
            );
            assert!(t.lines[..1_999].iter().all(|l| l == "x"));
        }
        let bytes = format!("{prefix}\r\nlast");
        let t = page(bytes.as_bytes(), None, None).unwrap();
        assert_eq!((t.lines.len(), t.next), (2_000, Some(2_001)));
        assert!(model_text("dense.txt", None, &t)
            .ends_with("2000: \n[Output truncated. Continue reading with offset: 2001]"));
        let t = page(bytes.as_bytes(), Some(2_001), None).unwrap();
        assert_eq!(
            (t.offset, t.lines.as_slice(), t.next),
            (2_001, &["last".to_string()][..], None)
        );
    }

    #[test]
    fn oc_read_applies_the_byte_limit_to_the_returned_content() {
        let exact = format!("{}x\n", "é".repeat(799)).repeat(32);
        assert_eq!(exact.len(), MAX_BYTES);
        let t = page(exact.as_bytes(), None, None).unwrap();
        assert_eq!((t.lines.len(), t.next), (32, None));
        // Each truncation marker takes more bytes than the character it replaces.
        let long = format!("{}\n", "x".repeat(2_001)).repeat(25) + &"é".repeat(500);
        let t = page(long.as_bytes(), None, None).unwrap();
        assert_eq!((t.offset, t.lines.len(), t.next), (1, 25, Some(26)));
        assert!(t.lines.join("\n").len() <= MAX_BYTES);
    }

    #[test]
    fn oc_read_reads_extensionless_and_renamed_gifs_as_images() {
        for (version, name) in [("GIF87a", "pixel"), ("GIF89a", "pixel.txt")] {
            let mut bytes = version.as_bytes().to_vec();
            bytes.extend(
                [
                    0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff,
                    0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44,
                    0x01, 0x00, 0x3b,
                ]
                .iter(),
            );
            assert_media(&bytes, name, "image/gif");
        }
    }

    #[test]
    fn oc_read_returns_a_listing_for_a_directory() {
        let env = Env::new();
        std::fs::create_dir(env.root().join("folder")).unwrap();
        env.write("file.txt", "hello");
        assert_eq!(
            read_text(&env, ".", json!({})),
            "Read directory ., entries 1-2\nfolder/\nfile.txt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn oc_read_lists_unresolved_symlinks_including_broken_and_escaping_links() {
        let env = Env::new();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(env.root().join("folder")).unwrap();
        env.write("file.txt", "hello");
        std::os::unix::fs::symlink(outside.path().join("target.txt"), env.root().join("escape"))
            .unwrap();
        std::os::unix::fs::symlink(env.root().join("missing.txt"), env.root().join("broken"))
            .unwrap();
        assert_eq!(
            read_text(&env, ".", json!({})),
            "Read directory ., entries 1-4\nfolder/\nbroken\nescape\nfile.txt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn oc_read_reads_a_symlinked_directory_as_a_listing() {
        let env = Env::new();
        env.write("target/file.txt", "hello");
        std::os::unix::fs::symlink(env.root().join("target"), env.root().join("link")).unwrap();
        assert_eq!(
            read_text(&env, "link", json!({})),
            "Read directory link, entries 1-1\nfile.txt"
        );
    }

    #[test]
    fn oc_read_enforces_line_and_byte_budgets_with_continuation_offsets() {
        let lines: Vec<String> = (0..2_001).map(|i| i.to_string()).collect();
        let t = page(lines.join("\n").as_bytes(), None, Some(2_000)).unwrap();
        assert_eq!((t.lines.len(), t.next), (2_000, Some(2_001)));
        let bytes = vec!["a".repeat(2_000); 200].join("\n");
        let t = page(bytes.as_bytes(), None, None).unwrap();
        assert_eq!((t.lines.len(), t.next), (25, Some(26)));
        assert!(t.lines.join("\n").len() <= MAX_BYTES);
    }

    #[test]
    fn oc_read_sorts_and_pages_directory_entries() {
        let env = Env::new();
        std::fs::create_dir_all(env.root().join("d/z")).unwrap();
        std::fs::create_dir_all(env.root().join("d/a")).unwrap();
        env.write("d/b.txt", "");
        assert_eq!(
            read_text(&env, "d", json!({"offset": 2, "limit": 1})),
            "Read directory d, entries 2-2\nz/\n[Output truncated. Continue reading with offset: 3]"
        );
    }

    #[test]
    fn oc_read_stops_checking_for_null_bytes_after_the_requested_page() {
        let t = page(b"one\n\0", None, Some(1)).unwrap();
        assert_eq!(
            (t.lines, t.offset, t.next),
            (vec!["one".to_string()], 1, Some(2))
        );
    }

    #[test]
    fn oc_read_ignores_a_nul_after_a_page_but_rejects_it_for_a_past_end_offset() {
        let bytes = format!("{}\nselected\n\0\ntail", "x".repeat(256 * 1024));
        for offset in [2, -3] {
            let t = page(bytes.as_bytes(), Some(offset), Some(1)).unwrap();
            assert_eq!(
                (t.lines, t.offset, t.next),
                (vec!["selected".to_string()], 2, Some(3)),
                "{offset}"
            );
        }
        assert!(page(bytes.as_bytes(), Some(5), Some(1)).is_none());
    }

    /// Adapted: only a file's leading byte order mark is dropped; one at
    /// the start of a later line is text, at any offset (OpenCode's chunked
    /// decoder also drops it where a chunk starts).
    #[test]
    fn oc_read_keeps_a_byte_order_mark_that_is_not_at_the_start_of_the_file() {
        let bytes = format!("{}\n\u{feff}selected", "x".repeat(256 * 1024 - 2));
        for offset in [3, -1] {
            let t = page(bytes.as_bytes(), Some(offset), Some(1)).unwrap();
            assert_eq!(
                (t.lines, t.offset),
                (vec!["\u{feff}selected".to_string()], 2),
                "{offset}"
            );
        }
    }

    #[test]
    fn oc_read_checks_skipped_lines_for_null_bytes() {
        assert!(page(b"one\0\ntwo\n", Some(2), Some(1)).is_none());
    }

    #[test]
    fn oc_read_reads_page_two_after_a_long_first_line() {
        for first in [300 * 1024, 256 * 1024 - 1] {
            let bytes = format!("{}\nsecond\n", "a".repeat(first));
            let t = page(bytes.as_bytes(), Some(2), Some(1)).unwrap();
            assert_eq!(
                (t.lines, t.offset, t.next),
                (vec!["second".to_string()], 2, None),
                "{first}"
            );
        }
    }

    #[test]
    fn oc_read_preserves_the_media_ingestion_limit_message() {
        let env = Env::new();
        let path = env.root().join("oversized.png");
        std::fs::write(&path, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(crate::media::MAX_INGEST_BYTES + 1)
            .unwrap();
        assert_eq!(
            read_path(&env.ctx(), "oversized.png").unwrap_err(),
            format!(
                "Media exceeds {} byte ingestion limit: oversized.png",
                crate::media::MAX_INGEST_BYTES
            )
        );
    }
}
