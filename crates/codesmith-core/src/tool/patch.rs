//! The patch format the `patch` tool takes, ported from OpenCode's
//! `@opencode/util/patch`: `*** Begin Patch` … `*** End Patch` around
//! `*** Add File:`, `*** Delete File:` and `*** Update File:` hunks (an
//! update optionally `*** Move to:` another path, then `@@ [context]`
//! chunks of ` `, `-` and `+` lines, optionally `*** End of File`).
//!
//! [`parse`] turns the text into [`Hunk`]s; [`derive`] applies an update's
//! chunks to a file's text with OpenCode's matching tiers (exact, then
//! ignoring trailing whitespace, then surrounding whitespace, then
//! typographic quote, dash and space variants).
//!
//! Strings are trimmed with JavaScript's whitespace set, so the parser
//! accepts exactly what OpenCode's does.

use std::fmt;

/// One file operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<Chunk>,
    },
}

impl Hunk {
    /// The path the hunk names (the source of a move).
    pub fn path(&self) -> &str {
        match self {
            Hunk::Add { path, .. } | Hunk::Delete { path } | Hunk::Update { path, .. } => path,
        }
    }
}

/// One `@@` chunk of an update.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// The text after `@@ `: a line to find before the chunk's lines.
    pub change_context: Option<String>,
    /// `*** End of File` followed the chunk: it matches the file's end.
    pub end_of_file: bool,
}

impl Chunk {
    fn is_empty(&self) -> bool {
        self.old_lines.is_empty() && self.new_lines.is_empty()
    }
}

/// Why a patch does not parse. The messages are OpenCode's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The first (`first == true`) or last line is not the marker.
    Boundary { first: bool },
    InvalidHunk {
        line: String,
        line_number: usize,
        reason: Option<String>,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Boundary { first: true } => {
                f.write_str("The first line of the patch must be '*** Begin Patch'")
            }
            ParseError::Boundary { first: false } => {
                f.write_str("The last line of the patch must be '*** End Patch'")
            }
            ParseError::InvalidHunk {
                line_number,
                reason: Some(reason),
                ..
            } => write!(f, "Invalid hunk at line {line_number}: {reason}"),
            ParseError::InvalidHunk {
                line, line_number, ..
            } => write!(
                f,
                "Invalid hunk at line {line_number}: '{line}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// JavaScript's `\s` (what `String.prototype.trim` strips).
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

fn trim_end(s: &str) -> &str {
    s.trim_end_matches(js_space)
}

const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF: &str = "*** End of File";
const END: &str = "*** End Patch";
const ENVIRONMENT: &str = "*** Environment ID:";

fn invalid(line: &str, index: usize, reason: Option<String>) -> ParseError {
    ParseError::InvalidHunk {
        line: line.to_string(),
        line_number: index + 1,
        reason,
    }
}

/// Parse a patch into its hunks, in order.
pub fn parse(patch_text: &str) -> Result<Vec<Hunk>, ParseError> {
    let stripped = strip_heredoc(trim(patch_text));
    let lines: Vec<&str> = stripped
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    if trim(lines[0]) != "*** Begin Patch" {
        return Err(ParseError::Boundary { first: true });
    }
    let begin = 0;
    let end = lines.len() - 1;
    if trim(lines[end]) != END || begin >= end {
        return Err(ParseError::Boundary { first: false });
    }
    let mut hunks = Vec::new();
    let mut index = begin + 1;
    while index < end {
        let header = trim(lines[index]);
        if index == begin + 1
            && header.starts_with(ENVIRONMENT)
            && !trim(&header[ENVIRONMENT.len()..]).is_empty()
        {
            index += 1;
            continue;
        }
        if let Some(rest) = header.strip_prefix(ADD) {
            let path = trim(rest).to_string();
            let (contents, next) = parse_add(&lines, index + 1, end, &path)?;
            hunks.push(Hunk::Add { path, contents });
            index = next;
            continue;
        }
        if let Some(rest) = header.strip_prefix(DELETE) {
            let path = trim(rest).to_string();
            if index + 1 < end {
                let next = trim(lines[index + 1]);
                if !is_boundary(next) {
                    let reason = (!next.starts_with("*** ")).then(|| {
                        format!(
                            "Unexpected line after Delete File '{path}': '{next}'. Delete hunks do not contain body lines"
                        )
                    });
                    return Err(invalid(next, index + 1, reason));
                }
            }
            hunks.push(Hunk::Delete { path });
            index += 1;
            continue;
        }
        if let Some(rest) = header.strip_prefix(UPDATE) {
            let path = trim(rest).to_string();
            let mut next = index + 1;
            let mut move_path = None;
            while lines.get(next).is_some_and(|l| trim_end(l) == EOF) {
                next += 1;
            }
            if let Some(line) = lines.get(next).map(|l| trim_end(l)) {
                if line == "*** Move to:" || line.starts_with(MOVE) {
                    let dest = trim(line.get(MOVE.len()..).unwrap_or(""));
                    if dest.is_empty() {
                        return Err(invalid(
                            trim(lines[next]),
                            next,
                            Some(format!("Move destination for '{path}' must not be empty")),
                        ));
                    }
                    move_path = Some(dest.to_string());
                    next += 1;
                }
            }
            let (chunks, next) = parse_update(&lines, next, end, &path, index)?;
            hunks.push(Hunk::Update {
                path,
                move_path,
                chunks,
            });
            index = next;
            continue;
        }
        return Err(invalid(header, index, None));
    }
    Ok(hunks)
}

fn parse_add(
    lines: &[&str],
    start: usize,
    end: usize,
    path: &str,
) -> Result<(String, usize), ParseError> {
    let mut content = Vec::new();
    let mut index = start;
    while index < end && !is_boundary(trim(lines[index])) {
        let Some(rest) = lines[index].strip_prefix('+') else {
            let line = trim(lines[index]);
            return Err(invalid(
                line,
                index,
                Some(format!(
                    "Invalid Add File line for '{path}': expected a line starting with '+', got '{line}'"
                )),
            ));
        };
        content.push(rest);
        index += 1;
    }
    Ok((content.join("\n"), index))
}

fn unexpected(line: &str) -> String {
    format!("Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)")
}

fn expected_marker(line: &str) -> String {
    format!("Expected update hunk to start with a @@ context marker, got: '{line}'")
}

fn is_marker(line: &str) -> bool {
    line == "@@" || line.starts_with("@@ ")
}

fn parse_update(
    lines: &[&str],
    start: usize,
    end: usize,
    path: &str,
    hunk: usize,
) -> Result<(Vec<Chunk>, usize), ParseError> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut index = start;
    let mut after_eof = false;
    while index < end {
        let line = lines[index];
        let update_line = trim_end(line);
        if after_eof {
            if update_line.is_empty() {
                index += 1;
                continue;
            }
            if is_marker(update_line) {
                after_eof = false;
            } else if is_boundary(update_line) {
                break;
            } else {
                return Err(invalid(line, index, Some(expected_marker(line))));
            }
        }
        if update_line == EOF {
            if let Some(chunk) = chunks.last_mut() {
                if chunk.is_empty() {
                    return Err(invalid(
                        update_line,
                        index,
                        Some("Update hunk does not contain any lines".into()),
                    ));
                }
                chunk.end_of_file = true;
                after_eof = true;
            }
            index += 1;
            continue;
        }
        if is_boundary(update_line) {
            break;
        }
        if is_marker(update_line) {
            if chunks.last().is_some_and(Chunk::is_empty) {
                return Err(invalid(line, index, Some(unexpected(line))));
            }
            chunks.push(Chunk {
                change_context: (update_line != "@@").then(|| update_line[3..].to_string()),
                ..Chunk::default()
            });
            index += 1;
            continue;
        }
        if chunks.is_empty() {
            chunks.push(Chunk::default());
        }
        let chunk = chunks.last_mut().expect("pushed above");
        if line.is_empty() {
            chunk.old_lines.push(String::new());
            chunk.new_lines.push(String::new());
        } else if let Some(rest) = line.strip_prefix(' ') {
            chunk.old_lines.push(rest.to_string());
            chunk.new_lines.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('-') {
            chunk.old_lines.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('+') {
            chunk.new_lines.push(rest.to_string());
        } else {
            let reason = if chunk.is_empty() {
                unexpected(line)
            } else {
                expected_marker(line)
            };
            return Err(invalid(line, index, Some(reason)));
        }
        index += 1;
    }
    if chunks.is_empty() {
        return Err(invalid(
            trim(lines[hunk]),
            hunk,
            Some(format!("Update file hunk for path '{path}' is empty")),
        ));
    }
    if chunks.last().is_some_and(Chunk::is_empty) {
        let line = trim(lines[index]);
        let reason = if line == END {
            "Update hunk does not contain any lines".to_string()
        } else {
            unexpected(line)
        };
        return Err(invalid(line, index, Some(reason)));
    }
    Ok((chunks, index))
}

fn is_boundary(line: &str) -> bool {
    line == END || line.starts_with(ADD) || line.starts_with(DELETE) || line.starts_with(UPDATE)
}

/// The body of a heredoc wrapper (`cat <<'EOF'` … `EOF`) models sometimes
/// put around the patch, as OpenCode's
/// `/^(?:cat\s+)?<<(['"]?)(\w+)\1\s*\n([\s\S]*?)\n\2\s*$/` finds it, or
/// the text unchanged.
fn strip_heredoc(input: &str) -> &str {
    heredoc_body(input).unwrap_or(input)
}

fn heredoc_body(input: &str) -> Option<&str> {
    let mut rest = input;
    if let Some(after) = rest.strip_prefix("cat") {
        let spaced = after.trim_start_matches(js_space);
        if spaced.len() < after.len() {
            rest = spaced;
        }
    }
    let rest = rest.strip_prefix("<<")?;
    let (quote, rest) = match rest.chars().next() {
        Some(q @ ('\'' | '"')) => (Some(q), &rest[1..]),
        _ => (None, rest),
    };
    let word_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if word_len == 0 {
        return None;
    }
    let (tag, mut rest) = rest.split_at(word_len);
    if let Some(q) = quote {
        rest = rest.strip_prefix(q)?;
    }
    // `\s*\n`: the body starts after a line break within the whitespace
    // run, the last one first (the regex's greedy `\s*`).
    let space = rest.len() - rest.trim_start_matches(js_space).len();
    let starts: Vec<usize> = rest[..space]
        .char_indices()
        .filter(|(_, c)| *c == '\n')
        .map(|(i, _)| i + 1)
        .rev()
        .collect();
    let closing = format!("\n{tag}");
    for start in starts {
        // `([\s\S]*?)\n\2\s*$`: the shortest body that the closing tag
        // and only whitespace follow (empty when the tag comes at once).
        let mut from = start;
        while let Some(i) = rest[from..].find(&closing) {
            let at = from + i;
            if rest[at + closing.len()..].chars().all(js_space) {
                return Some(&rest[start..at]);
            }
            from = at + 1;
        }
    }
    None
}

/// An update's result: the text (without a BOM) and whether to write one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUpdate {
    pub content: String,
    pub bom: bool,
}

/// Split leading BOMs off `text` (OpenCode's `Bom.split`).
pub fn split_bom(text: &str) -> (bool, &str) {
    let stripped = text.trim_start_matches('\u{feff}');
    (stripped.len() != text.len(), stripped)
}

/// `text` with one BOM when `bom` (OpenCode's `Bom.join`).
pub fn join_bom(text: &str, bom: bool) -> String {
    let (_, text) = split_bom(text);
    if bom {
        format!("\u{feff}{text}")
    } else {
        text.to_string()
    }
}

/// Apply an update's chunks to `original`; `path` names the file in errors.
pub fn derive(path: &str, chunks: &[Chunk], original: &str) -> Result<FileUpdate, String> {
    let (had_bom, text) = split_bom(original);
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut replacements = compute_replacements(&lines, path, chunks)?;
    // Apply from the end so earlier offsets stay valid; equal offsets keep
    // their order (a stable sort, then reversed).
    replacements.reverse();
    for (start, remove, insert) in replacements {
        lines.splice(start..start + remove, insert);
    }
    if lines.last().is_none_or(|l| !l.is_empty()) {
        lines.push(String::new());
    }
    let joined = lines.join("\n");
    let (next_bom, content) = split_bom(&joined);
    Ok(FileUpdate {
        content: content.to_string(),
        bom: had_bom || next_bom,
    })
}

type Replacement = (usize, usize, Vec<String>);

fn compute_replacements(
    lines: &[String],
    path: &str,
    chunks: &[Chunk],
) -> Result<Vec<Replacement>, String> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index = 0;
    for chunk in chunks {
        if let Some(context) = chunk.change_context.as_deref().filter(|c| !c.is_empty()) {
            let found = seek(lines, &[context.to_string()], line_index, false)
                .ok_or_else(|| format!("Failed to find context '{context}' in {path}"))?;
            line_index = found + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push((lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }
        let mut old_lines: &[String] = &chunk.old_lines;
        let mut new_lines: &[String] = &chunk.new_lines;
        let mut found = seek(lines, old_lines, line_index, chunk.end_of_file);
        if found.is_none() && old_lines.last().is_some_and(String::is_empty) {
            old_lines = &old_lines[..old_lines.len() - 1];
            if new_lines.last().is_some_and(String::is_empty) {
                new_lines = &new_lines[..new_lines.len() - 1];
            }
            found = seek(lines, old_lines, line_index, chunk.end_of_file);
        }
        let Some(found) = found else {
            if chunk.old_lines.iter().all(String::is_empty) {
                let expected = if chunk.old_lines.len() == 1 {
                    "an expected blank line".to_string()
                } else {
                    format!("{} consecutive blank lines", chunk.old_lines.len())
                };
                return Err(format!("Failed to find {expected} in {path}"));
            }
            return Err(format!(
                "Failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            ));
        };
        replacements.push((found, old_lines.len(), new_lines.to_vec()));
        line_index = found + old_lines.len();
    }
    replacements.sort_by_key(|r| r.0);
    Ok(replacements)
}

type Compare = fn(&str, &str) -> bool;

const TIERS: [Compare; 4] = [
    |a, b| a == b,
    |a, b| trim_end(a) == trim_end(b),
    |a, b| trim(a) == trim(b),
    |a, b| normalize(trim(a)) == normalize(trim(b)),
];

fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() || pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    let matches = |offset: usize, compare: Compare| {
        pattern
            .iter()
            .enumerate()
            .all(|(i, p)| compare(&lines[offset + i], p))
    };
    if eof {
        if last < start {
            return None;
        }
        return TIERS
            .iter()
            .any(|compare| matches(last, *compare))
            .then_some(last);
    }
    TIERS
        .iter()
        .find_map(|compare| (start..=last).find(|offset| matches(*offset, *compare)))
}

/// Typographic quotes, dashes and spaces as their ASCII forms.
fn normalize(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}
