//! `grep`: search file contents with ripgrep's engine. Ported from OpenCode's grep tool.

use super::glob::{walk_files, Matcher, DEFAULT_LIMIT};
use super::{opt_bool, opt_str, opt_u64, str_arg, Ctx, Outcome, Output};
use crate::permission::{Effect, Request};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{sinks::Lossy, BinaryDetection, SearcherBuilder};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MAX_LINE_CHARS: usize = 2_000;

pub fn spec() -> (String, Value) {
    (
        "Search file contents using ripgrep's regular expression syntax or literal text matching. Use it to locate specific code, symbols, or text patterns, and narrow searches with `path` or `include`. Returns matching file paths, line numbers, and line previews.".into(),
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "minLength": 1, "description": "Regular expression or literal text to match in file contents."},
                "path": {"type": "string", "description": "File or directory to search. Defaults to the current working directory."},
                "include": {"type": "string", "description": "Glob pattern to filter files (for example, \"*.js\" or \"*.{ts,tsx}\")"},
                "literal": {"type": "boolean", "description": "Treat `pattern` as exact text instead of a regular expression (default: false)."},
                "caseSensitive": {"type": "boolean", "description": "Use case-sensitive matching (default: true)."},
                "limit": {"type": "integer", "minimum": 1, "description": format!("Maximum number of matching lines to return (default: {DEFAULT_LIMIT})")}
            },
            "required": ["pattern"]
        }),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub line: u64,
    pub text: String,
}

pub struct Query<'a> {
    pub pattern: &'a str,
    pub include: Option<&'a str>,
    pub literal: bool,
    pub case_sensitive: bool,
    pub limit: usize,
}

/// Matches under `root` (a file or directory); plus whether more existed.
/// When `root` is a directory, only files `searchable` accepts are searched;
/// it is offered only regular files the walk found below `root` (the walk
/// does not follow symlinks, and symlinks are not searched).
pub fn search(
    root: &Path,
    q: &Query,
    searchable: &dyn Fn(&Path) -> bool,
) -> Result<(Vec<Hit>, bool), String> {
    let matcher = RegexMatcherBuilder::new()
        .fixed_strings(q.literal)
        .case_insensitive(!q.case_sensitive)
        .build(q.pattern)
        .map_err(|e| format!("Invalid regex pattern: {e}"))?;
    let include = q
        .include
        .map(Matcher::new)
        .transpose()
        .map_err(|e| e.replace("in pattern", "in include"))?;
    let files: Vec<PathBuf> = if root.is_file() {
        vec![root.to_path_buf()]
    } else {
        let mut v: Vec<PathBuf> = walk_files(root, false, &|e| {
            let p = e.path();
            include
                .as_ref()
                .is_none_or(|m| m.is_match(p.strip_prefix(root).unwrap_or(p)))
                .then_some(())
        })
        .into_iter()
        .map(|(p, ())| p)
        .collect();
        v.sort();
        v.retain(|p| searchable(p));
        v
    };
    let mut hits = Vec::new();
    let mut truncated = false;
    // Files are searched on several threads a batch at a time, in order, so
    // the result is what searching them one by one would give; batches grow
    // so that a search that fills its limit early stops early.
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 16);
    let mut start = 0;
    let mut batch = threads * 4;
    'files: while start < files.len() {
        let end = (start + batch).min(files.len());
        for h in search_batch(&files[start..end], &matcher, threads) {
            if hits.len() >= q.limit {
                truncated = true;
                break 'files;
            }
            hits.push(h);
        }
        start = end;
        batch = (batch * 2).min(threads * 256);
    }
    Ok((hits, truncated))
}

/// Matches in `files`, in file order, searched on up to `threads` threads.
fn search_batch(files: &[PathBuf], matcher: &grep_regex::RegexMatcher, threads: usize) -> Vec<Hit> {
    let per = files.len().div_ceil(threads.max(1)).max(1);
    if files.len() <= per {
        return search_files(files, matcher);
    }
    std::thread::scope(|s| {
        let parts: Vec<_> = files
            .chunks(per)
            .map(|chunk| s.spawn(move || search_files(chunk, matcher)))
            .collect();
        parts
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

fn search_files(files: &[PathBuf], matcher: &grep_regex::RegexMatcher) -> Vec<Hit> {
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();
    let mut hits = Vec::new();
    for f in files {
        let _ = searcher.search_path(
            matcher,
            f,
            Lossy(|line, text| {
                let t = text.trim_end_matches(['\n', '\r']);
                let t = if t.chars().count() > MAX_LINE_CHARS {
                    format!("{}…", t.chars().take(MAX_LINE_CHARS).collect::<String>())
                } else {
                    t.to_string()
                };
                hits.push(Hit {
                    path: f.clone(),
                    line,
                    text: t,
                });
                Ok(true)
            }),
        );
    }
    hits
}

pub fn format(hits: &[Hit], truncated: bool) -> String {
    let mut lines = vec![if hits.is_empty() {
        "No matches found".to_string()
    } else {
        format!("Found {} matches", hits.len())
    }];
    let mut current: Option<&Path> = None;
    for h in hits {
        if current != Some(h.path.as_path()) {
            if current.is_some() {
                lines.push(String::new());
            }
            current = Some(&h.path);
            lines.push(format!("{}:", h.path.display()));
        }
        lines.push(format!("  Line {}: {}", h.line, h.text));
    }
    if truncated {
        lines.push(String::new());
        lines.push(format!("(Results are truncated: showing first {} results. Consider using a more specific path or pattern.)", hits.len()));
    }
    lines.join("\n")
}

pub fn run(ctx: &Ctx, input: &Value) -> Outcome {
    let pattern = str_arg(input, "pattern")?;
    if pattern.is_empty() {
        return Err("Pattern must not be empty".into());
    }
    let path = opt_str(input, "path");
    let target = ctx.resolve(path.unwrap_or("."));
    ctx.authorize_external(&target, target.absolute.is_dir(), super::Access::Read)?;
    ctx.ask(Request {
        action: "grep".into(),
        resources: vec![pattern.to_string()],
        save: vec!["*".into()],
        title: format!("Search for {pattern}"),
        access: None,
    })?;
    // Grep shows file contents, so the read rules apply: a file named by
    // `path` is approved like a read, and a directory a rule denies is not
    // searched. (OpenCode checks only `grep` and `external_directory`.)
    if path.is_some() || ctx.gate.guarded("read", &target.resources) {
        if target.absolute.is_file() || ctx.gate.guarded("read", &target.resources) {
            ctx.ask(Request {
                action: "read".into(),
                resources: target.resources.clone(),
                save: vec!["*".into()],
                title: format!("Read {}", target.resource),
                access: None,
            })?;
        } else {
            ctx.check_not_denied("read", &target.resources)?;
        }
    }
    if !target.absolute.exists() {
        return Err(format!(
            "Search path does not exist: {}",
            path.unwrap_or(".")
        ));
    }
    let q = Query {
        pattern,
        include: opt_str(input, "include"),
        literal: opt_bool(input, "literal").unwrap_or(false),
        case_sensitive: opt_bool(input, "caseSensitive").unwrap_or(true),
        limit: opt_u64(input, "limit")
            .map(|l| l.max(1) as usize)
            .unwrap_or(DEFAULT_LIMIT),
    };
    // Searching a directory skips files the read rules deny, and those they
    // would ask about, which are named so the model can grep them by path.
    let skipped = std::cell::RefCell::new(Vec::new());
    let roots = ctx.roots();
    // The project's saved approvals, read once for the whole search.
    let durable = ctx.gate.durable_now();
    // `search` offers only regular files its walk found, never symlinks.
    let searchable = |p: &Path| {
        let t = ctx.resolve_walked(&roots, &target, p, false);
        match ctx.read_effect_given(&t.resources, &durable) {
            Effect::Allow => true,
            Effect::Ask => {
                skipped.borrow_mut().push(t.resource);
                false
            }
            Effect::Deny => false,
        }
    };
    let (hits, truncated) = search(&target.absolute, &q, &searchable)?;
    let mut out = format(&hits, truncated);
    let skipped = skipped.into_inner();
    if !skipped.is_empty() {
        out.push_str(&skipped_note(&skipped));
    }
    Ok(Output::bounded(out))
}

/// Names the files a directory search left out because reading them asks.
fn skipped_note(skipped: &[String]) -> String {
    const SHOWN: usize = 10;
    let mut names = skipped[..skipped.len().min(SHOWN)].join(", ");
    if skipped.len() > SHOWN {
        names.push_str(&format!(" and {} more", skipped.len() - SHOWN));
    }
    format!(
        "\n\n(Not searched, because reading them needs approval: {names}. To search one, pass it as `path`.)"
    )
}

#[cfg(test)]
mod tests {
    use super::super::testing::Env;
    use super::*;

    fn setup() -> Env {
        let env = Env::new();
        env.write("src/a.rs", "fn main() {}\nlet Foo = 1;\n");
        env.write("src/b.ts", "const foo = 2;\n");
        env.write("bin.dat", "foo\0bar");
        env
    }

    /// A search reads the project's saved approvals once: files they
    /// allow are searched, and an approval saved (or removed) between two
    /// searches applies to the next one.
    #[tokio::test(flavor = "multi_thread")]
    async fn saved_approvals_apply_to_the_next_search() {
        use crate::permission::{base_rules, Effect, Fixed, Gate, Reply, Rule};
        let mut env = setup();
        env.write("notes/n.txt", "foo in a note\n");
        let data = tempfile::tempdir().unwrap();
        let book = std::sync::Arc::new(
            crate::approvals::Book::open(&data.path().join("sessions.db")).unwrap(),
        );
        let mut rules = base_rules();
        rules.push(Rule::new("read", "*.txt", Effect::Ask));
        env.gate = Gate::new(rules, std::sync::Arc::new(Fixed(Reply::Once)));
        env.gate.set_durable(book.clone(), "p".into());
        let search = |env: &Env| run(&env.ctx(), &json!({"pattern": "foo"})).unwrap().content;
        let first = search(&env);
        assert!(
            first.starts_with("Found 1 matches") && first.contains("needs approval: notes/n.txt"),
            "{first}"
        );
        let saved = book.add("p", "read", &["*.txt".into()]).unwrap();
        let out = search(&env);
        assert!(
            out.starts_with("Found 2 matches")
                && out.contains("Line 1: foo in a note")
                && !out.contains("needs approval"),
            "{out}"
        );
        book.remove(&saved[0].id).unwrap();
        assert_eq!(search(&env), first);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn regex_grouped_output() {
        let env = setup();
        let out = run(&env.ctx(), &json!({"pattern": "fo+"})).unwrap().content;
        assert!(out.starts_with("Found 1 matches\n"), "{out}");
        assert!(
            out.ends_with("src/b.ts:\n  Line 1: const foo = 2;"),
            "{out}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn case_insensitive_include_and_literal() {
        let env = setup();
        let ctx = env.ctx();
        let out = run(&ctx, &json!({"pattern": "foo", "caseSensitive": false}))
            .unwrap()
            .content;
        assert!(out.starts_with("Found 2 matches"), "{out}");
        let out = run(
            &ctx,
            &json!({"pattern": "foo", "caseSensitive": false, "include": "*.rs"}),
        )
        .unwrap()
        .content;
        assert!(
            out.starts_with("Found 1 matches") && out.contains("Line 2: let Foo = 1;"),
            "{out}"
        );
        let out = run(&ctx, &json!({"pattern": "main()", "literal": true}))
            .unwrap()
            .content;
        assert!(out.contains("Line 1: fn main() {}"), "{out}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn binary_files_are_skipped() {
        let env = setup();
        let out = run(&env.ctx(), &json!({"pattern": "bar"})).unwrap().content;
        assert_eq!(out, "No matches found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_file_and_limit() {
        let env = Env::new();
        let body: String = (0..10).map(|i| format!("hit {i}\n")).collect();
        env.write("many.txt", &body);
        let ctx = env.ctx();
        let out = run(
            &ctx,
            &json!({"pattern": "hit", "path": "many.txt", "limit": 3}),
        )
        .unwrap()
        .content;
        assert!(out.starts_with("Found 3 matches"), "{out}");
        assert!(out.contains("Results are truncated"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn errors() {
        let env = setup();
        let ctx = env.ctx();
        assert!(run(&ctx, &json!({"pattern": "("}))
            .unwrap_err()
            .starts_with("Invalid regex pattern"));
        assert!(run(&ctx, &json!({"pattern": "x", "include": "a["}))
            .unwrap_err()
            .starts_with("Invalid glob in include"));
        assert_eq!(
            run(&ctx, &json!({"pattern": ""})).unwrap_err(),
            "Pattern must not be empty"
        );
    }
}
