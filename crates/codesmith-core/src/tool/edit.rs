//! `edit`: exact find-and-replace. Ported from OpenCode's edit tool,
//! including its matching tiers, line-ending preservation and miss hints.
//! All offsets here are `char` indices, so normalization (which maps one
//! char to one char) never shifts them.

use super::{opt_bool, str_arg, Ctx, Outcome, Output};
use crate::permission::Request;
use serde_json::{json, Value};

pub fn spec() -> (String, Value) {
    (
        "Edit the contents of a file by finding and replacing exact text. When editing text from Read output, preserve the exact indentation (tabs or spaces) and omit the line-number prefix, such as `1: `. Never include the prefix in oldString or newString. Write line breaks as \\n; the file's existing CRLF or LF line endings are matched and preserved. To change line endings themselves, make oldString and newString differ only in \\r; that edit applies to the exact bytes. If there is no exact match, the edit retries while ignoring typographic quote, dash, and space variants, then trailing whitespace on each line. The edit fails if oldString is not found. By default, oldString must identify a UNIQUE location. Multiple matches FAIL unless replaceAll is true. Add more surrounding context to disambiguate, or set replaceAll to true to replace every occurrence. Use replaceAll when the change should apply to every occurrence, such as renaming a variable.".into(),
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit"},
                "oldString": {"type": "string", "description": "Exact text to find and replace"},
                "newString": {"type": "string", "description": "Text to replace oldString with (must differ from oldString)"},
                "replaceAll": {"type": "boolean", "description": "Whether to replace every occurrence of oldString. When false, oldString must match exactly once. Defaults to false."}
            },
            "required": ["path", "oldString", "newString"]
        }),
    )
}

fn norm(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c => c,
    }
}

fn normalize(s: &[char]) -> Vec<char> {
    s.iter().map(|c| norm(*c)).collect()
}

fn norm_str(s: &str) -> String {
    s.chars().map(norm).collect()
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn string(c: &[char]) -> String {
    c.iter().collect()
}

type Span = (usize, usize);

/// Non-overlapping occurrences of `search`, left to right.
fn find(content: &[char], search: &[char]) -> Vec<Span> {
    let mut out = Vec::new();
    if search.is_empty() || search.len() > content.len() {
        return out;
    }
    let mut i = 0;
    while i + search.len() <= content.len() {
        if content[i..i + search.len()] == *search {
            out.push((i, i + search.len()));
            i += search.len();
        } else {
            i += 1;
        }
    }
    out
}

struct Line {
    start: usize,
    end: usize,
    text: Vec<char>,
    content_end: usize,
    newline: bool,
}

fn split_lines(view: &[char]) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < view.len() {
        let nl = view[start..].iter().position(|c| *c == '\n');
        let (text_end, end, newline) = match nl {
            Some(p) => (start + p, start + p + 1, true),
            None => (view.len(), view.len(), false),
        };
        let text = view[start..text_end].to_vec();
        let content_end = text_end - usize::from(text.last() == Some(&'\r'));
        out.push(Line {
            start,
            end,
            text,
            content_end,
            newline,
        });
        start = end;
    }
    out
}

fn trim_end(c: &[char]) -> &[char] {
    let mut e = c.len();
    while e > 0 && c[e - 1].is_whitespace() {
        e -= 1;
    }
    &c[..e]
}

/// Whole-line matches ignoring trailing whitespace and typographic variants.
fn find_lines(view: &[char], search: &[char]) -> Vec<Span> {
    let trailing = search.last() == Some(&'\n');
    let mut expected: Vec<&[char]> = search.split(|c| *c == '\n').collect();
    if trailing {
        expected.pop();
    }
    if expected.is_empty() {
        return vec![];
    }
    let lines = split_lines(view);
    let mut out: Vec<Span> = Vec::new();
    for i in 0..lines.len() {
        if i + expected.len() > lines.len() {
            break;
        }
        let actual = &lines[i..i + expected.len()];
        if !actual
            .iter()
            .zip(&expected)
            .all(|(a, e)| normalize(trim_end(&a.text)) == normalize(trim_end(e)))
        {
            continue;
        }
        let last = actual.last().unwrap();
        if trailing && !last.newline {
            continue;
        }
        let cand = (
            lines[i].start,
            if trailing { last.end } else { last.content_end },
        );
        if !out.iter().any(|m| m.1 > cand.0 && m.0 < cand.1) {
            out.push(cand);
        }
    }
    out
}

/// The dominant line ending in `text`, or `fallback` when tied.
fn line_ending<'a>(text: &str, fallback: &'a str) -> &'a str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf == lf {
        fallback
    } else if crlf > lf {
        "\r\n"
    } else {
        "\n"
    }
}

/// For each `after` line, the `before` line it keeps (LCS) or replaces.
fn align_lines(before: &[String], after: &[String]) -> Vec<Option<usize>> {
    let w = after.len() + 1;
    let mut t = vec![0u32; (before.len() + 1) * w];
    for i in (0..before.len()).rev() {
        for j in (0..after.len()).rev() {
            t[i * w + j] = if before[i] == after[j] {
                t[(i + 1) * w + j + 1] + 1
            } else {
                t[(i + 1) * w + j].max(t[i * w + j + 1])
            };
        }
    }
    let mut result = vec![None; after.len()];
    let (mut gb, mut ga): (Vec<usize>, Vec<usize>) = (vec![], vec![]);
    let flush = |gb: &mut Vec<usize>, ga: &mut Vec<usize>, result: &mut Vec<Option<usize>>| {
        for (k, j) in ga.iter().enumerate() {
            result[*j] = gb.get(k).copied();
        }
        gb.clear();
        ga.clear();
    };
    let (mut i, mut j) = (0, 0);
    while i < before.len() && j < after.len() {
        if before[i] == after[j] {
            flush(&mut gb, &mut ga, &mut result);
            result[j] = Some(i);
            i += 1;
            j += 1;
            continue;
        }
        if t[(i + 1) * w + j] >= t[i * w + j + 1] {
            gb.push(i);
            i += 1;
        } else {
            ga.push(j);
            j += 1;
        }
    }
    while i < before.len() {
        gb.push(i);
        i += 1;
    }
    while j < after.len() {
        ga.push(j);
        j += 1;
    }
    flush(&mut gb, &mut ga, &mut result);
    result
}

fn line_key(l: &str) -> String {
    norm_str(l.trim_end())
}

/// Join the replacement's lines with endings taken from the matched region.
fn with_region_endings(region: &str, replacement: &str, fallback: &str) -> String {
    let re = regex::Regex::new(r"\r?\n").unwrap();
    let before: Vec<String> = re.split(region).map(line_key).collect();
    let endings: Vec<&str> = re.find_iter(region).map(|m| m.as_str()).collect();
    let after: Vec<&str> = replacement.split('\n').collect();
    let keys: Vec<String> = after.iter().map(|l| line_key(l)).collect();
    let src = align_lines(&before, &keys);
    let mut out = String::new();
    for (k, line) in after.iter().enumerate() {
        out.push_str(line);
        if k + 1 < after.len() {
            out.push_str(
                src[k]
                    .and_then(|i| endings.get(i).copied())
                    .unwrap_or(fallback),
            );
        }
    }
    out
}

fn count_before(positions: &[usize], offset: usize) -> usize {
    positions.partition_point(|p| *p < offset)
}

#[derive(Debug, PartialEq)]
pub enum EditError {
    NotFound,
    Ambiguous(usize),
    Unchanged { endings_only: bool },
}

/// Apply an edit to `source`. Returns the new text and the number of matches.
pub fn apply_edit(
    source: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, usize), EditError> {
    let src = chars(source);
    let endings_only = old.replace('\r', "") == new.replace('\r', "");
    let (matches, replacer): (Vec<Span>, Box<dyn Fn(Span) -> String>) = if endings_only {
        (find(&src, &chars(old)), Box::new(|_| new.to_string()))
    } else {
        let view_s = source.replace("\r\n", "\n");
        let view = chars(&view_s);
        let mut removed = Vec::new();
        let mut k = 0;
        for (i, w) in src.windows(2).enumerate() {
            if w == ['\r', '\n'] {
                removed.push(i - k);
                k += 1;
            }
        }
        let search = chars(&old.replace("\r\n", "\n"));
        let replacement = new.replace("\r\n", "\n");
        let file_ending = line_ending(source, "\n").to_string();
        let exact = find(&view, &search);
        let unicode = if exact.is_empty() {
            find(&normalize(&view), &normalize(&search))
        } else {
            vec![]
        };
        let trailing = if exact.is_empty() && unicode.is_empty() {
            find_lines(&view, &search)
        } else {
            vec![]
        };
        let chosen = if !exact.is_empty() {
            exact
        } else if !unicode.is_empty() {
            unicode
        } else {
            trailing
        };
        let mapped = chosen
            .into_iter()
            .map(|(s, e)| (s + count_before(&removed, s), e + count_before(&removed, e)))
            .collect();
        let src2 = src.clone();
        (
            mapped,
            Box::new(move |(s, e)| {
                let region = string(&src2[s..e]);
                let fb = line_ending(&region, &file_ending).to_string();
                with_region_endings(&region, &replacement, &fb)
            }),
        )
    };
    let n = matches.len();
    if n == 0 {
        return Err(EditError::NotFound);
    }
    if n > 1 && !replace_all {
        return Err(EditError::Ambiguous(n));
    }
    let chosen: Vec<Span> = if replace_all {
        matches
    } else {
        matches.into_iter().take(1).collect()
    };
    let mut out = src.clone();
    for m in chosen.into_iter().rev() {
        let rep = chars(&replacer(m));
        out.splice(m.0..m.1, rep);
    }
    let result = string(&out);
    if result == source {
        return Err(EditError::Unchanged { endings_only });
    }
    Ok((result, n))
}

const HINT_LINES: usize = 12;
const HINT_LINE_CHARS: usize = 200;

/// A hint for an oldString miss: newString already present, or the closest lines.
pub fn miss_hint(source: &str, old: &str, new: &str) -> Option<String> {
    let view = source.replace("\r\n", "\n");
    let replacement = new.replace("\r\n", "\n");
    if replacement.trim().chars().count() >= 20 && norm_str(&view).contains(&norm_str(&replacement))
    {
        return Some(
            "newString is already in the file, so this edit may already be applied.".into(),
        );
    }
    let ws = regex::Regex::new(r"\s+").unwrap();
    let key = |l: &str| ws.replace_all(&norm_str(l), " ").trim().to_string();
    let distinct = |l: &str| l.chars().any(char::is_alphanumeric);
    let lines: Vec<&str> = view.split('\n').collect();
    let search_owned = old.replace("\r\n", "\n");
    let search: Vec<&str> = search_owned.split('\n').collect();
    let mut positions: std::collections::HashMap<String, Vec<isize>> = Default::default();
    for (i, l) in lines.iter().enumerate() {
        let k = key(l);
        if distinct(&k) {
            positions.entry(k).or_default().push(i as isize);
        }
    }
    let mut votes: Vec<(isize, usize)> = Vec::new();
    let mut wanted = 0;
    for (off, l) in search.iter().enumerate() {
        let k = key(l);
        if !distinct(&k) {
            continue;
        }
        wanted += 1;
        for idx in positions.get(&k).into_iter().flatten() {
            let s = idx - off as isize;
            match votes.iter_mut().find(|v| v.0 == s) {
                Some(v) => v.1 += 1,
                None => votes.push((s, 1)),
            }
        }
    }
    let (start, count) = votes.iter().fold(
        (0isize, 0usize),
        |best, v| if v.1 > best.1 { *v } else { best },
    );
    if count == 0 || count * 2 < wanted {
        return None;
    }
    let matched: Vec<usize> = search
        .iter()
        .enumerate()
        .filter_map(|(off, l)| {
            let k = key(l);
            let idx = start + off as isize;
            (!k.is_empty()
                && idx >= 0
                && (idx as usize) < lines.len()
                && key(lines[idx as usize]) == k)
                .then_some(idx as usize)
        })
        .collect();
    let from = *matched.first()?;
    let to = (matched.last()? + 1).min(from + HINT_LINES);
    let mut out = vec!["The closest lines in the file:".to_string()];
    for (k, l) in lines[from..to].iter().enumerate() {
        let shown = if l.chars().count() > HINT_LINE_CHARS {
            format!("{}…", l.chars().take(HINT_LINE_CHARS).collect::<String>())
        } else {
            l.to_string()
        };
        out.push(format!("{}: {shown}", from + k + 1));
    }
    Some(out.join("\n"))
}

const INVALID_UTF8: &str = "file is not valid UTF-8. It was left unchanged because rewriting it as text would replace every byte that is not UTF-8 with U+FFFD; change it with a tool that preserves its bytes, or convert it to UTF-8 first";

/// Split leading UTF-8 BOMs off `text` (OpenCode's `Bom.split`).
fn split_bom(text: &str) -> (bool, &str) {
    let stripped = text.trim_start_matches('\u{feff}');
    (stripped.len() != text.len(), stripped)
}

/// One lock per edited file, so concurrent edits to it apply in turn
/// (OpenCode's `fileMutation.withLock`).
fn file_lock(path: &std::path::Path) -> std::sync::Arc<std::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static LOCKS: OnceLock<Mutex<HashMap<std::path::PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(key)
        .or_default()
        .clone()
}

pub fn run(ctx: &Ctx, input: &Value) -> Outcome {
    let path = str_arg(input, "path")?;
    let old = str_arg(input, "oldString")?;
    let new = str_arg(input, "newString")?;
    let replace_all = opt_bool(input, "replaceAll").unwrap_or(false);
    if old == new {
        return Err("No changes to apply: oldString and newString are identical.".into());
    }
    if old.is_empty() {
        return Err("oldString must not be empty. Use write to create or overwrite a file.".into());
    }
    let target = ctx.resolve(path);
    ctx.authorize_external(&target, false)?;
    let lock = file_lock(&target.absolute);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if target.absolute.is_dir() {
        return Err(format!("Path is a directory, not a file: {path}"));
    }
    let bytes = match std::fs::read(&target.absolute) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("File not found: {path}"))
        }
        Err(e) => return Err(format!("Unable to edit {path}: {e}")),
    };
    let decoded =
        String::from_utf8(bytes).map_err(|_| format!("Unable to edit {path}: {INVALID_UTF8}"))?;
    // Match and edit the text after the BOM; it is written back below.
    let (had_bom, source) = split_bom(&decoded);
    let result = apply_edit(source, old, new, replace_all);
    // Ask before reporting whether oldString matched, so a denied edit
    // does not disclose the file's contents.
    let (removed, added) = (super::count_lines(old), super::count_lines(new));
    ctx.ask(Request {
        action: "edit".into(),
        resources: vec![target.resource.clone()],
        save: vec!["*".into()],
        title: format!("Edit {} (-{removed} +{added} lines)", target.resource),
    })?;
    let (updated, n) = match result {
        Ok(r) => r,
        Err(EditError::NotFound) => {
            let readable = ctx
                .gate
                .allows("read", std::slice::from_ref(&target.resource));
            let hint = if readable {
                miss_hint(source, old, new)
            } else {
                None
            };
            return Err(format!(
                "Could not find oldString in {path}. It must match exactly, including whitespace and indentation.{}",
                hint.map(|h| format!("\n\n{h}")).unwrap_or_default()
            ));
        }
        Err(EditError::Ambiguous(n)) => {
            return Err(format!("Found {n} matches for oldString, but expected exactly one. Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence."));
        }
        Err(EditError::Unchanged { endings_only: true }) => {
            return Err(format!(
                "No changes to apply: {path} already has those line endings."
            ));
        }
        Err(EditError::Unchanged {
            endings_only: false,
        }) => {
            return Err(format!("No changes to apply: the edit leaves {path} unchanged. Line endings follow the file; to change them, write \\r explicitly in oldString or newString."));
        }
    };
    let (new_bom, text) = split_bom(&updated);
    let written = if had_bom || new_bom {
        format!("\u{feff}{text}")
    } else {
        text.to_string()
    };
    std::fs::write(&target.absolute, &written)
        .map_err(|e| format!("Unable to edit {path}: {e}"))?;
    let note = ctx.format(&target.absolute, &written);
    let diagnostics = ctx.diagnostics(&target.absolute, false);
    Ok(Output::text(format!(
        "Edited {} ({n} replacement{}){note}{diagnostics}",
        target.resource,
        if n == 1 { "" } else { "s" }
    )))
}

#[cfg(test)]
mod tests {
    use super::super::testing::Env;
    use super::super::Tool;
    use super::*;

    fn ok(src: &str, old: &str, new: &str) -> String {
        apply_edit(src, old, new, false).unwrap().0
    }

    #[test]
    fn exact_unique_match() {
        assert_eq!(ok("a = 1\nb = 2\n", "b = 2", "b = 3"), "a = 1\nb = 3\n");
    }

    #[test]
    fn ambiguous_and_replace_all() {
        assert_eq!(
            apply_edit("x x x", "x", "y", false),
            Err(EditError::Ambiguous(3))
        );
        assert_eq!(
            apply_edit("x x x", "x", "y", true).unwrap(),
            ("y y y".into(), 3)
        );
    }

    #[test]
    fn not_found() {
        assert_eq!(
            apply_edit("abc", "zzz", "y", false),
            Err(EditError::NotFound)
        );
    }

    #[test]
    fn typographic_variants_match() {
        assert_eq!(
            ok("say “hi” — it’s\n", "say \"hi\" - it's", "said"),
            "said\n"
        );
    }

    #[test]
    fn trailing_whitespace_tier() {
        assert_eq!(
            ok(
                "fn a() {   \n  body\n}\n",
                "fn a() {\n  body\n}",
                "fn b() {}"
            ),
            "fn b() {}\n"
        );
    }

    #[test]
    fn trailing_newline_search_needs_a_newline() {
        assert_eq!(ok("a  \nb\n", "a\n", "z\n"), "z\nb\n");
    }

    #[test]
    fn crlf_file_keeps_crlf() {
        assert_eq!(
            ok("one\r\ntwo\r\nthree\r\n", "two\nthree", "2\n3\nfour"),
            "one\r\n2\r\n3\r\nfour\r\n"
        );
    }

    #[test]
    fn mixed_endings_keep_unchanged_lines() {
        let src = "a\r\nb\nc\r\n";
        assert_eq!(ok(src, "a\nb\nc", "a\nB\nc"), "a\r\nB\nc\r\n");
    }

    #[test]
    fn endings_only_edit_applies_to_bytes() {
        assert_eq!(
            apply_edit("a\r\nb", "a\r\nb", "a\nb", false).unwrap().0,
            "a\nb"
        );
        assert_eq!(
            apply_edit("a\nb", "a\r\nb", "a\nb", false),
            Err(EditError::NotFound)
        );
    }

    #[test]
    fn unchanged_detection() {
        assert_eq!(
            apply_edit("a\r\nb\r\n", "a\nb", "a\r\nb", false),
            Err(EditError::NotFound)
        );
        assert_eq!(
            apply_edit("x  \n", "x\n", "x  \n", false),
            Err(EditError::Unchanged {
                endings_only: false
            })
        );
    }

    #[test]
    fn multibyte_text_is_safe() {
        assert_eq!(ok("héllo wörld ✓\n", "wörld", "world"), "héllo world ✓\n");
        assert_eq!(ok("“q” ✓ x\n", "\"q\" ✓", "q"), "q x\n");
    }

    #[test]
    fn line_ending_votes() {
        assert_eq!(line_ending("a\r\nb\r\nc\n", "\n"), "\r\n");
        assert_eq!(line_ending("a\nb", "\r\n"), "\n");
        assert_eq!(line_ending("ab", "\r\n"), "\r\n");
    }

    #[test]
    fn alignment_keeps_unchanged_lines() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            align_lines(&s(&["a", "b", "c"]), &s(&["a", "x", "c"])),
            vec![Some(0), Some(1), Some(2)]
        );
        assert_eq!(
            align_lines(&s(&["a", "c"]), &s(&["a", "b", "c"])),
            vec![Some(0), None, Some(1)]
        );
    }

    #[test]
    fn hint_shows_closest_lines() {
        let src = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let h = miss_hint(
            src,
            "fn main() {\n    let x = 2;\n    println!(\"{x}\");",
            "fn main() {}",
        )
        .unwrap();
        assert!(
            h.starts_with("The closest lines in the file:\n1: fn main() {"),
            "{h}"
        );
        assert!(h.contains("3:     println!"), "{h}");
    }

    #[test]
    fn hint_detects_already_applied() {
        let src = "let value = compute_the_thing(42);\n";
        assert_eq!(
            miss_hint(src, "let v = old();", "let value = compute_the_thing(42);").unwrap(),
            "newString is already in the file, so this edit may already be applied."
        );
    }

    #[test]
    fn no_hint_for_unrelated_text() {
        assert_eq!(miss_hint("alpha\nbeta\n", "gamma delta", "x"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tool_messages() {
        let env = Env::new();
        env.write("f.rs", "a\nb\na\n");
        let ctx = env.ctx();
        let e = |v: Value| run(&ctx, &v).unwrap_err();
        assert_eq!(
            e(json!({"path": "f.rs", "oldString": "a", "newString": "a"})),
            "No changes to apply: oldString and newString are identical."
        );
        assert!(
            e(json!({"path": "f.rs", "oldString": "", "newString": "x"}))
                .starts_with("oldString must not be empty")
        );
        assert_eq!(
            e(json!({"path": "nope.rs", "oldString": "a", "newString": "b"})),
            "File not found: nope.rs"
        );
        assert!(
            e(json!({"path": "f.rs", "oldString": "a", "newString": "c"}))
                .starts_with("Found 2 matches")
        );
        assert!(
            e(json!({"path": "f.rs", "oldString": "zzz", "newString": "c"}))
                .starts_with("Could not find oldString in f.rs.")
        );
        let ok = run(
            &ctx,
            &json!({"path": "f.rs", "oldString": "a", "newString": "c", "replaceAll": true}),
        )
        .unwrap();
        assert_eq!(ok.content, "Edited f.rs (2 replacements)");
        assert_eq!(
            std::fs::read_to_string(env.root().join("f.rs")).unwrap(),
            "c\nb\nc\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_utf8_is_refused() {
        let env = Env::new();
        std::fs::write(env.root().join("bad.txt"), [0xff, 0xfe, b'a']).unwrap();
        let e = run(
            &env.ctx(),
            &json!({"path": "bad.txt", "oldString": "a", "newString": "b"}),
        )
        .unwrap_err();
        assert!(e.contains("not valid UTF-8"), "{e}");
    }

    // ---------------------------------------------------------------------
    // Ported from OpenCode's packages/core/test/tool-edit.test.ts (MIT).
    // One `oc_edit_*` test per OpenCode case, in the same order.
    //
    // Intentional differences:
    // - OpenCode returns a structured diff in `metadata.files` and attaches it
    //   to the permission request; CodeSmith returns text only, so the diff
    //   assertions become assertions on the file and the output line.
    // - OpenCode's permission service sees every assert; CodeSmith's gate only
    //   reaches the asker for requests the rules leave at "ask", so these tests
    //   use a `Recorder` asker with no allow rules to observe the order.
    // - A rejected request reads "The user declined this request." instead of
    //   "Permission denied: <action>" (CodeSmith's gate wording).
    // - Tool-definition filtering by deny rules (case 1) and Environment read
    //   counters/hooks (cases 6, 7, 21, 22) are OpenCode service plumbing;
    //   the observable file results are asserted instead.
    // ---------------------------------------------------------------------

    use crate::permission::{Effect, Rule};

    fn call(ctx: &Ctx, v: Value) -> Outcome {
        run(ctx, &v)
    }

    fn read(env: &Env, rel: &str) -> String {
        std::fs::read_to_string(env.root().join(rel)).unwrap()
    }

    /// A formatter for `.txt` files that runs `perl -pi -e <script>`.
    fn perl_formatter(env: &Env, script: &str) -> std::sync::Arc<crate::format::Formatter> {
        use crate::format::{Entry, Formatter, Setting};
        let entry = Entry {
            command: Some(vec![
                "perl".into(),
                "-pi".into(),
                "-e".into(),
                script.into(),
                "$FILE".into(),
            ]),
            extensions: Some(vec![".txt".into()]),
            ..Default::default()
        };
        let setting = Setting::Custom([("test".to_string(), entry)].into_iter().collect());
        std::sync::Arc::new(Formatter::new(Some(&setting), env.root(), env.root()))
    }

    fn allow_read() -> Vec<Rule> {
        vec![Rule::new("read", "*", Effect::Allow)]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_replaces_relative_exact_text_once() {
        let (env, rec) = Env::recording(allow_read());
        env.write("hello.txt", "before\nrest\n");
        assert_eq!(Tool::Edit.spec("/bin/sh").name, "edit");
        let out = call(
            &env.ctx(),
            json!({"path": "hello.txt", "oldString": "before", "newString": "after"}),
        )
        .unwrap();
        assert_eq!(out.content, "Edited hello.txt (1 replacement)");
        assert_eq!(read(&env, "hello.txt"), "after\nrest\n");
        let asked = rec.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].action, "edit");
        assert_eq!(asked[0].resources, vec!["hello.txt".to_string()]);
        assert_eq!(asked[0].save, vec!["*".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_reports_final_formatted_content() {
        let env = Env::new();
        env.write("formatted.txt", "before\n");
        let mut ctx = env.ctx();
        ctx.format = Some(perl_formatter(&env, "s/after/AFTER/"));
        let out = call(
            &ctx,
            json!({"path": "formatted.txt", "oldString": "before", "newString": "after"}),
        )
        .unwrap();
        // OpenCode puts the formatted diff in metadata; CodeSmith tells the model.
        assert!(
            out.content.starts_with(
                "Edited formatted.txt (1 replacement)\n\nThe file was reformatted by test"
            ),
            "{}",
            out.content
        );
        assert_eq!(read(&env, "formatted.txt"), "AFTER\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_accepts_absolute_path_inside_location() {
        let (env, rec) = Env::recording(allow_read());
        let target = env.write("absolute.txt", "before");
        call(
            &env.ctx(),
            json!({"path": target.display().to_string(), "oldString": "before", "newString": "after"}),
        )
        .unwrap();
        assert_eq!(rec.actions(), vec!["edit"]);
        assert_eq!(read(&env, "absolute.txt"), "after");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_external_symlink_target_needs_only_edit_permission() {
        let (env, rec) = Env::recording(allow_read());
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("external.txt");
        std::fs::write(&target, "before").unwrap();
        std::os::unix::fs::symlink(&target, env.root().join("link.txt")).unwrap();
        call(
            &env.ctx(),
            json!({"path": "link.txt", "oldString": "before", "newString": "after"}),
        )
        .unwrap();
        assert_eq!(
            rec.requests(),
            vec![("edit".to_string(), vec!["link.txt".to_string()])]
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_approves_external_absolute_path_before_edit() {
        let (env, rec) = Env::recording(allow_read());
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().canonicalize().unwrap().join("external.txt");
        std::fs::write(&target, "before").unwrap();
        call(
            &env.ctx(),
            json!({"path": target.display().to_string(), "oldString": "before", "newString": "after"}),
        )
        .unwrap();
        assert_eq!(rec.actions(), vec!["external_directory", "edit"]);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_no_write_when_external_or_edit_denied() {
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().canonicalize().unwrap().join("denied.txt");
        std::fs::write(&external, "before").unwrap();
        let input = json!({"path": external.display().to_string(), "oldString": "before", "newString": "after"});

        let (env, rec) = Env::recording(allow_read());
        rec.deny_action("external_directory");
        assert_eq!(
            call(&env.ctx(), input.clone()).unwrap_err(),
            "The user declined this request."
        );
        assert_eq!(rec.actions(), vec!["external_directory"]);

        let (env, rec) = Env::recording(allow_read());
        rec.deny_action("edit");
        assert_eq!(
            call(&env.ctx(), input).unwrap_err(),
            "The user declined this request."
        );
        assert_eq!(rec.actions(), vec!["external_directory", "edit"]);
        assert_eq!(std::fs::read_to_string(&external).unwrap(), "before");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_denied_edit_does_not_disclose_match() {
        let (env, rec) = Env::recording(allow_read());
        rec.deny_action("edit");
        env.write("secret.txt", "secret content");
        let ctx = env.ctx();
        let matching = call(
            &ctx,
            json!({"path": "secret.txt", "oldString": "secret content", "newString": "replacement"}),
        )
        .unwrap_err();
        let missing = call(
            &ctx,
            json!({"path": "secret.txt", "oldString": "not present", "newString": "replacement"}),
        )
        .unwrap_err();
        assert_eq!(matching, "The user declined this request.");
        assert_eq!(missing, matching);
        assert_eq!(rec.actions(), vec!["edit", "edit"]);
        assert_eq!(read(&env, "secret.txt"), "secret content");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_rejects_noop_empty_missing_and_ambiguous() {
        let env = Env::new();
        env.write("matches.txt", "same same");
        let ctx = env.ctx();
        let e = |old: &str, new: &str| {
            call(
                &ctx,
                json!({"path": "matches.txt", "oldString": old, "newString": new}),
            )
            .unwrap_err()
        };
        assert_eq!(
            e("same", "same"),
            "No changes to apply: oldString and newString are identical."
        );
        assert_eq!(
            e("", "after"),
            "oldString must not be empty. Use write to create or overwrite a file."
        );
        assert_eq!(
            e("missing", "after"),
            "Could not find oldString in matches.txt. It must match exactly, including whitespace and indentation."
        );
        assert_eq!(
            e("same", "after"),
            "Found 2 matches for oldString, but expected exactly one. Add more surrounding context to make oldString unique, or set replaceAll to true to replace every occurrence."
        );
        assert_eq!(read(&env, "matches.txt"), "same same");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_miss_points_at_closest_lines_or_applied_new_string() {
        let src = "import { a } from './a'\n\nexport function run() {\n  const value = a()\n  return value + 1\n}\n";
        let missed =
            "Could not find oldString in miss.ts. It must match exactly, including whitespace and indentation.";
        let check = |env: &Env, old: &str, new: &str| {
            call(
                &env.ctx(),
                json!({"path": "miss.ts", "oldString": old, "newString": new}),
            )
            .unwrap_err()
        };
        let env = Env::new();
        env.write("miss.ts", src);
        assert_eq!(
            check(&env, "export function run() {\n\tconst value = a()\n\treturn value + 2\n}", "x"),
            format!("{missed}\n\nThe closest lines in the file:\n3: export function run() {{\n4:   const value = a()\n5:   return value + 1\n6: }}")
        );
        assert_eq!(
            check(&env, "  return value + 2", "  return value + 1"),
            missed
        );
        assert_eq!(
            check(&env, "const value = b()", "  const value = a()\n  return value + 1"),
            format!("{missed}\n\nnewString is already in the file, so this edit may already be applied.")
        );
        assert_eq!(check(&env, "nothing like it", "anything"), missed);
        assert_eq!(
            check(&env, "import { a } from './a'\n\n\n\n\n\n", "x"),
            format!("{missed}\n\nThe closest lines in the file:\n1: import {{ a }} from './a'")
        );
        // Without read permission the hint, which shows file contents, is left out.
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
            gate: crate::permission::Gate::new(
                vec![
                    Rule::new("*", "*", Effect::Allow),
                    Rule::new("read", "*", Effect::Ask),
                ],
                std::sync::Arc::new(crate::permission::Fixed(crate::permission::Reply::Once)),
            ),
        };
        env.write("miss.ts", src);
        assert_eq!(
            check(
                &env,
                "export function run() {\n\tconst value = a()\n\treturn value + 2\n}",
                "x"
            ),
            missed
        );
        assert_eq!(read(&env, "miss.ts"), src);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_missing_file_and_directory_errors() {
        let env = Env::new();
        std::fs::create_dir(env.root().join("src")).unwrap();
        let ctx = env.ctx();
        assert_eq!(
            call(
                &ctx,
                json!({"path": "missing.ts", "oldString": "before", "newString": "after"})
            )
            .unwrap_err(),
            "File not found: missing.ts"
        );
        assert_eq!(
            call(
                &ctx,
                json!({"path": "src", "oldString": "before", "newString": "after"})
            )
            .unwrap_err(),
            "Path is a directory, not a file: src"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_replace_all_replaces_every_occurrence() {
        let env = Env::new();
        env.write("all.txt", "same same same");
        let out = call(
            &env.ctx(),
            json!({"path": "all.txt", "oldString": "same", "newString": "after", "replaceAll": true}),
        )
        .unwrap();
        assert_eq!(out.content, "Edited all.txt (3 replacements)");
        assert_eq!(read(&env, "all.txt"), "after after after");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_normalizes_typography_only_after_exact_fails() {
        let env = Env::new();
        env.write(
            "unicode.txt",
            "exact - match\ncurly “quotes”\nminus − one\nspace\u{00A0}here\nexact − match\n",
        );
        let ctx = env.ctx();
        call(
            &ctx,
            json!({"path": "unicode.txt", "oldString": "curly \"quotes\"\nminus - one\nspace here", "newString": "normalized"}),
        )
        .unwrap();
        call(
            &ctx,
            json!({"path": "unicode.txt", "oldString": "exact - match", "newString": "selected"}),
        )
        .unwrap();
        assert_eq!(
            read(&env, "unicode.txt"),
            "selected\nnormalized\nexact − match\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_ignores_trailing_whitespace_preserving_untouched_lines() {
        let env = Env::new();
        env.write("whitespace.txt", "before  \nmatch  \nnext\t\nafter  \n");
        call(
            &env.ctx(),
            json!({"path": "whitespace.txt", "oldString": "match\nnext", "newString": "changed"}),
        )
        .unwrap();
        assert_eq!(read(&env, "whitespace.txt"), "before  \nchanged\nafter  \n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_non_overlapping_trailing_matches_and_crlf() {
        let env = Env::new();
        env.write("overlap.txt", "a  \na  \na  \n");
        env.write("windows.txt", "a  \r\nb\t\r\n");
        let ctx = env.ctx();
        let out = call(
            &ctx,
            json!({"path": "overlap.txt", "oldString": "a\na", "newString": "x", "replaceAll": true}),
        )
        .unwrap();
        assert_eq!(out.content, "Edited overlap.txt (1 replacement)");
        call(
            &ctx,
            json!({"path": "windows.txt", "oldString": "a\nb", "newString": "x"}),
        )
        .unwrap();
        assert_eq!(read(&env, "overlap.txt"), "x\na  \n");
        assert_eq!(read(&env, "windows.txt"), "x\r\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_preserves_bom_and_crlf() {
        let env = Env::new();
        env.write("windows.txt", "\u{FEFF}before\r\nrest\r\n");
        let mut ctx = env.ctx();
        // A formatter that strips the BOM; the edit puts it back.
        ctx.format = Some(perl_formatter(&env, "s/^\\xEF\\xBB\\xBF//"));
        call(
            &ctx,
            json!({"path": "windows.txt", "oldString": "before\nrest", "newString": "after\nrest"}),
        )
        .unwrap();
        assert_eq!(read(&env, "windows.txt"), "\u{FEFF}after\r\nrest\r\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_refuses_invalid_utf8() {
        let env = Env::new();
        let mut bytes = b"// Jos\xe9\n".to_vec();
        bytes.extend_from_slice(b"int MAX = 10;\n");
        let target = env.root().join("Invoice.java");
        std::fs::write(&target, &bytes).unwrap();
        let e = call(
            &env.ctx(),
            json!({"path": "Invoice.java", "oldString": "MAX = 10", "newString": "MAX = 25"}),
        )
        .unwrap_err();
        assert!(
            e.contains("Unable to edit Invoice.java: file is not valid UTF-8."),
            "{e}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_mixed_endings_keep_each_regions_endings() {
        let env = Env::new();
        env.write("mostly-lf.txt", "head\r\nAAA OLD BBB\ntail\n");
        env.write("mostly-crlf.txt", "one\r\ntwo\r\nthree\nfour\r\n");
        let ctx = env.ctx();
        for (p, old, new) in [
            ("mostly-lf.txt", "AAA OLD BBB", "AAA NEW\nLINE2"),
            ("mostly-lf.txt", "LINE2\ntail", "LINE2\nTAIL"),
            ("mostly-crlf.txt", "o\nthree\nfo", "O\nTHREE\nFO"),
            ("mostly-crlf.txt", "one", "ONE\nUNO"),
        ] {
            call(&ctx, json!({"path": p, "oldString": old, "newString": new})).unwrap();
        }
        assert_eq!(
            read(&env, "mostly-lf.txt"),
            "head\r\nAAA NEW\nLINE2\nTAIL\n"
        );
        assert_eq!(
            read(&env, "mostly-crlf.txt"),
            "ONE\r\nUNO\r\ntwO\r\nTHREE\nFOur\r\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_keeps_endings_of_unchanged_and_in_place_lines() {
        let env = Env::new();
        env.write(
            "store.cs",
            "All(item.Sku);\r\n\r\nFind(sku);\r\n\r\nvoid Add()\n{\n  _items[item.Sku] = item;\n}\r\n",
        );
        call(
            &env.ctx(),
            json!({
                "path": "store.cs",
                "oldString": "All(item.Sku);\n\nFind(sku);\n\nvoid Add()\n{\n  _items[item.Sku] = item;\n}",
                "newString": "All(item.Code);\n\nFind(code);\n\nvoid Add()\n{\n  _items[item.Code] = item;\n}\n\nbool Remove(string code)\n{\n}",
            }),
        )
        .unwrap();
        assert_eq!(
            read(&env, "store.cs"),
            "All(item.Code);\r\n\r\nFind(code);\r\n\r\nvoid Add()\n{\n  _items[item.Code] = item;\n}\r\n\r\nbool Remove(string code)\r\n{\r\n}\r\n"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_carriage_return_only_edits_apply_to_bytes() {
        let env = Env::new();
        env.write("strip.sh", "a\r\nb\r\n");
        env.write("whole.sh", "x\r\ny\r\n");
        env.write("add.bat", "p\nq\n");
        let ctx = env.ctx();
        let out = call(
            &ctx,
            json!({"path": "strip.sh", "oldString": "\r", "newString": "", "replaceAll": true}),
        )
        .unwrap();
        assert_eq!(out.content, "Edited strip.sh (2 replacements)");
        call(
            &ctx,
            json!({"path": "whole.sh", "oldString": "x\r\ny\r\n", "newString": "x\ny\n"}),
        )
        .unwrap();
        call(
            &ctx,
            json!({"path": "add.bat", "oldString": "p\nq", "newString": "p\r\nq"}),
        )
        .unwrap();
        assert!(call(
            &ctx,
            json!({"path": "add.bat", "oldString": "p\nq", "newString": "p\r\nq"}),
        )
        .is_err());
        assert_eq!(read(&env, "strip.sh"), "a\nb\n");
        assert_eq!(read(&env, "whole.sh"), "x\ny\n");
        assert_eq!(read(&env, "add.bat"), "p\r\nq\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_fails_when_edit_leaves_file_unchanged() {
        let env = Env::new();
        env.write("quote.txt", "it’s here\n");
        assert_eq!(
            call(
                &env.ctx(),
                json!({"path": "quote.txt", "oldString": "it's here", "newString": "it’s here"}),
            )
            .unwrap_err(),
            "No changes to apply: the edit leaves quote.txt unchanged. Line endings follow the file; to change them, write \\r explicitly in oldString or newString."
        );
        assert_eq!(read(&env, "quote.txt"), "it’s here\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_serializes_concurrent_edits() {
        let env = Env::new();
        env.write("concurrent.txt", "one\ntwo\n");
        let results: Vec<Outcome> = std::thread::scope(|s| {
            let handles: Vec<_> = [("one", "ONE"), ("two", "TWO")]
                .into_iter()
                .map(|(old, new)| {
                    let env = &env;
                    s.spawn(move || {
                        call(
                            &env.ctx(),
                            json!({"path": "concurrent.txt", "oldString": old, "newString": new}),
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(read(&env, "concurrent.txt"), "ONE\nTWO\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oc_edit_applies_when_content_changes_after_matching() {
        // OpenCode rewrites the file between its read and write through an
        // Environment hook; CodeSmith has no such seam, so this pins the plain
        // read-modify-write and its output.
        let env = Env::new();
        env.write("concurrent.txt", "before\n");
        let out = call(
            &env.ctx(),
            json!({"path": "concurrent.txt", "oldString": "before", "newString": "after"}),
        )
        .unwrap();
        assert_eq!(out.content, "Edited concurrent.txt (1 replacement)");
        assert_eq!(read(&env, "concurrent.txt"), "after\n");
    }
}
