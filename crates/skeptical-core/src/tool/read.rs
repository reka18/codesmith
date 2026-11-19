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
        "Read the contents of a file or directory. Supports text files. Each text line is prefixed by its 1-based line number as <line>: <content>. The prefix is for reference and is not part of the file content. Directory entries are returned one per line. Use offset and limit to read large files or directories in sections. Prefer one larger read over many small slices, and use grep to find specific content in large files.".into(),
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File or directory to read"},
                "offset": {"type": "integer", "minimum": 1, "description": "The line or directory entry to start reading from (1-based)"},
                "limit": {"type": "integer", "minimum": 1, "description": "The maximum number of lines or directory entries to read (defaults to and capped at 2000)"}
            },
            "required": ["path"]
        }),
    )
}

pub fn run(ctx: &Ctx, input: &Value) -> Outcome {
    let path = str_arg(input, "path")?;
    let offset = opt_u64(input, "offset").map(|o| o.max(1) as usize);
    if opt_u64(input, "limit") == Some(0) {
        return Err("Limit must be a positive integer".into());
    }
    let limit = opt_u64(input, "limit")
        .map(|l| (l as usize).min(MAX_LINES))
        .unwrap_or(MAX_LINES);
    let target = ctx.resolve(path);
    let is_dir = target.absolute.is_dir();
    ctx.authorize_external(&target, is_dir)?;
    ctx.ask(Request {
        action: "read".into(),
        resources: vec![target.resource.clone()],
        save: vec!["*".into()],
        title: format!("Read {}", target.resource),
    })?;
    if !target.absolute.exists() {
        return Err(missing(path, &target.absolute));
    }
    if is_dir {
        return directory(path, &target.absolute, offset, limit);
    }
    let bytes =
        std::fs::read(&target.absolute).map_err(|e| format!("Unable to read {path}: {e}"))?;
    if is_media(&target.absolute) {
        return Err(format!(
            "Cannot read {path}: images and PDFs are not supported yet"
        ));
    }
    if is_binary(&bytes) {
        return Err(format!("Cannot read binary file: {path}"));
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(Output::bounded(file(path, &text, offset, limit)))
}

fn is_media(p: &Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "pdf"
    )
}

/// NUL in the first 8 KiB means binary, as git and ripgrep decide.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Format a page of a text file the way OpenCode's `toModelContent` does.
pub fn file(path: &str, text: &str, offset: Option<usize>, limit: usize) -> String {
    let all: Vec<&str> = if text.is_empty() {
        vec![]
    } else {
        let t = text.strip_suffix('\n').unwrap_or(text);
        t.split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect()
    };
    let total = all.len();
    let start = offset.unwrap_or(1);
    let mut shown = Vec::new();
    let mut bytes = 0;
    let mut next = None;
    for (i, line) in all.iter().enumerate().skip(start - 1) {
        if shown.len() >= limit || bytes > MAX_BYTES {
            next = Some(i + 1);
            break;
        }
        let line: String = if line.chars().count() > MAX_LINE_CHARS {
            format!(
                "{}… (line truncated)",
                line.chars().take(MAX_LINE_CHARS).collect::<String>()
            )
        } else {
            line.to_string()
        };
        bytes += line.len() + 1;
        shown.push(line);
    }
    let first = start.min(total.max(1));
    let header = if shown.is_empty() {
        let mut h = format!("Read file {path}, 0 lines");
        if offset.is_some() && start > total {
            h.push_str(&format!(
                " (offset {start} is past the end; the file has {total} {})",
                if total == 1 { "line" } else { "lines" }
            ));
        }
        h
    } else {
        format!(
            "Read file {path}, lines {first}-{}",
            start + shown.len() - 1
        )
    };
    let mut out = vec![header];
    for (i, l) in shown.iter().enumerate() {
        out.push(format!("{}: {l}", start + i));
    }
    if let Some(n) = next {
        out.push(format!(
            "[Output truncated. Continue reading with offset: {n}]"
        ));
    }
    out.join("\n")
}

fn directory(path: &str, dir: &Path, offset: Option<usize>, limit: usize) -> Outcome {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("Unable to read {path}: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                format!("{name}/")
            } else {
                name
            }
        })
        .collect();
    names.sort();
    let start = offset.unwrap_or(1);
    let page: Vec<&String> = names.iter().skip(start - 1).take(limit).collect();
    let mut out = vec![if page.is_empty() {
        format!("Read directory {path}, 0 entries")
    } else {
        format!(
            "Read directory {path}, entries {start}-{}",
            start + page.len() - 1
        )
    }];
    out.extend(page.iter().map(|s| s.to_string()));
    if start - 1 + page.len() < names.len() {
        out.push(format!(
            "[Output truncated. Continue reading with offset: {}]",
            start + page.len()
        ));
    }
    Ok(Output::bounded(out.join("\n")))
}

/// "File not found" with up to three similar names from the nearest existing folder.
fn missing(input: &str, absolute: &Path) -> String {
    let mut child = absolute.to_path_buf();
    let mut parent = absolute.parent().map(Path::to_path_buf);
    let base = absolute
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut msg = vec![format!("File not found: {input}")];
    while let Some(p) = parent.clone() {
        if p.is_dir() {
            let names: Vec<String> = std::fs::read_dir(&p)
                .map(|r| {
                    r.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            let wanted = child
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(base.clone());
            let sugg: Vec<String> = similar(&names, &wanted)
                .into_iter()
                .map(|n| p.join(n).display().to_string())
                .collect();
            if child != absolute {
                msg.push(format!(
                    "The folder {} does not exist. The nearest folder that does is {}.",
                    child.display(),
                    p.display()
                ));
            }
            if !sugg.is_empty() {
                msg.push(format!("Did you mean one of these?\n{}", sugg.join("\n")));
            }
            break;
        }
        child = p.clone();
        parent = p.parent().map(Path::to_path_buf);
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
    ranked.sort();
    ranked.into_iter().take(3).map(|r| r.2).collect()
}

#[cfg(test)]
mod tests {
    use super::super::testing::Env;
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
        assert_eq!(
            file("f", "a\nb\n", Some(5), 10),
            "Read file f, 0 lines (offset 5 is past the end; the file has 2 lines)"
        );
        assert_eq!(
            file("f", "a\n", Some(3), 10),
            "Read file f, 0 lines (offset 3 is past the end; the file has 1 line)"
        );
    }

    #[test]
    fn long_lines_are_cut() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let out = file("f", &long, None, MAX_LINES);
        assert!(out.ends_with("… (line truncated)"));
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
        env.write("img.png", "x");
        let ctx = env.ctx();
        assert_eq!(
            run(&ctx, &json!({"path": "bin", "limit": 0})).unwrap_err(),
            "Limit must be a positive integer"
        );
        assert!(run(&ctx, &json!({"path": "bin"}))
            .unwrap_err()
            .starts_with("Cannot read binary file"));
        assert!(run(&ctx, &json!({"path": "img.png"}))
            .unwrap_err()
            .contains("not supported yet"));
    }
}
