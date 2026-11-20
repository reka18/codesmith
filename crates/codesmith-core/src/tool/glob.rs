//! `glob`: find files by glob pattern, respecting `.gitignore` like ripgrep.
//! Ported from OpenCode's glob tool.

use super::{opt_bool, opt_str, opt_u64, str_arg, Ctx, Outcome, Output};
use crate::permission::Request;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const DEFAULT_LIMIT: usize = 100;

pub fn spec() -> (String, Value) {
    (
        "Search file paths using a glob pattern (examples: \"**/*.ts\", \"src/**/*.tsx\").".into(),
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match files against"},
                "path": {"type": "string", "description": "Directory to search. Defaults to the current working directory."},
                "hidden": {"type": "boolean", "description": "Include hidden files and directories (default: false)."},
                "limit": {"type": "integer", "minimum": 1, "description": format!("Maximum number of matching files to return (default: {DEFAULT_LIMIT})")}
            },
            "required": ["pattern"]
        }),
    )
}

/// A matcher with ripgrep's `--glob` semantics: a pattern without `/` matches
/// a file name anywhere; with `/` it matches the path relative to the root.
pub struct Matcher {
    set: globset::GlobMatcher,
    by_name: bool,
}

impl Matcher {
    pub fn new(pattern: &str) -> Result<Self, String> {
        let p = pattern.trim_start_matches("./");
        let by_name = !p.contains('/');
        let glob = globset::GlobBuilder::new(p)
            .literal_separator(!by_name)
            .backslash_escape(true)
            .build()
            .map_err(|e| format!("Invalid glob in pattern {e}"))?;
        Ok(Matcher {
            set: glob.compile_matcher(),
            by_name,
        })
    }

    pub fn is_match(&self, rel: &Path) -> bool {
        if self.by_name {
            rel.file_name()
                .map(|n| self.set.is_match(n))
                .unwrap_or(false)
        } else {
            self.set.is_match(rel)
        }
    }
}

pub fn walker(root: &Path, hidden: bool) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(!hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
}

/// Matching files under `root` that `listable` accepts, newest first, at
/// most `limit`; plus whether more existed.
pub fn search(
    root: &Path,
    pattern: &str,
    hidden: bool,
    limit: usize,
    listable: &dyn Fn(&Path) -> bool,
) -> Result<(Vec<PathBuf>, bool), String> {
    let m = Matcher::new(pattern)?;
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in walker(root, hidden).flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if m.is_match(rel) && listable(entry.path()) {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|md| md.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            found.push((mtime, entry.path().to_path_buf()));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let truncated = found.len() > limit;
    Ok((
        found.into_iter().take(limit).map(|f| f.1).collect(),
        truncated,
    ))
}

pub fn format(paths: &[String], truncated: bool) -> String {
    let mut lines: Vec<String> = if paths.is_empty() {
        vec!["No files found".into()]
    } else {
        paths.to_vec()
    };
    if truncated {
        lines.push(String::new());
        lines.push(format!("(Results are truncated: showing first {} results. Consider using a more specific path or pattern.)", paths.len()));
    }
    lines.join("\n")
}

pub fn run(ctx: &Ctx, input: &Value) -> Outcome {
    let pattern = str_arg(input, "pattern")?;
    let path = opt_str(input, "path");
    let hidden = opt_bool(input, "hidden").unwrap_or(false);
    let limit = opt_u64(input, "limit")
        .map(|l| l.max(1) as usize)
        .unwrap_or(DEFAULT_LIMIT);
    let target = ctx.resolve(path.unwrap_or("."));
    ctx.authorize_external(&target, true)?;
    ctx.ask(Request {
        action: "glob".into(),
        resources: vec![pattern.to_string()],
        save: vec!["*".into()],
        title: format!("Find files matching {pattern}"),
    })?;
    // The read rules apply to the directory searched, and files they deny
    // are left out. (OpenCode checks only `glob` and `external_directory`.)
    if path.is_some() {
        ctx.check_not_denied("read", &target.resources)?;
    }
    if !target.absolute.is_dir() {
        return Err(format!(
            "Search path is not a directory: {}",
            path.unwrap_or(".")
        ));
    }
    let listable = |p: &Path| {
        let t = ctx.resolve(&p.display().to_string());
        ctx.check_not_denied("read", &t.resources).is_ok()
    };
    let (found, truncated) = search(&target.absolute, pattern, hidden, limit, &listable)?;
    let shown: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
    Ok(Output::bounded(format(&shown, truncated)))
}

#[cfg(test)]
mod tests {
    use super::super::testing::Env;
    use super::*;

    #[test]
    fn name_vs_path_patterns() {
        let m = Matcher::new("*.rs").unwrap();
        assert!(m.is_match(Path::new("a/b/c.rs")));
        assert!(!m.is_match(Path::new("a/b/c.ts")));
        let m = Matcher::new("src/*.rs").unwrap();
        assert!(m.is_match(Path::new("src/a.rs")));
        assert!(!m.is_match(Path::new("src/x/a.rs")));
        let m = Matcher::new("src/**/*.rs").unwrap();
        assert!(m.is_match(Path::new("src/x/y/a.rs")));
        assert!(Matcher::new("./src/*.rs")
            .unwrap()
            .is_match(Path::new("src/a.rs")));
        assert!(Matcher::new("*.{ts,tsx}")
            .unwrap()
            .is_match(Path::new("x/a.tsx")));
    }

    #[test]
    fn invalid_glob() {
        assert!(Matcher::new("a[").is_err());
    }

    #[test]
    fn formatting() {
        assert_eq!(format(&[], false), "No files found");
        assert_eq!(format(&["a".into()], true), "a\n\n(Results are truncated: showing first 1 results. Consider using a more specific path or pattern.)");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn respects_gitignore_and_hidden() {
        let env = Env::new();
        env.write(".gitignore", "target/\n");
        env.write("src/a.rs", "");
        env.write("target/b.rs", "");
        env.write(".hidden/c.rs", "");
        let ctx = env.ctx();
        let out = run(&ctx, &json!({"pattern": "*.rs"})).unwrap().content;
        assert!(
            out.ends_with("src/a.rs") && !out.contains("target") && !out.contains(".hidden"),
            "{out}"
        );
        let out = run(&ctx, &json!({"pattern": "*.rs", "hidden": true}))
            .unwrap()
            .content;
        assert!(
            out.contains(".hidden/c.rs") && !out.contains("target"),
            "{out}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn limit_and_bad_path() {
        let env = Env::new();
        for i in 0..5 {
            env.write(&format!("f{i}.txt"), "");
        }
        let ctx = env.ctx();
        let out = run(&ctx, &json!({"pattern": "*.txt", "limit": 2}))
            .unwrap()
            .content;
        assert_eq!(out.lines().filter(|l| l.ends_with(".txt")).count(), 2);
        assert!(out.contains("Results are truncated"));
        env.write("file", "");
        assert_eq!(
            run(&ctx, &json!({"pattern": "*", "path": "file"})).unwrap_err(),
            "Search path is not a directory: file"
        );
        assert_eq!(
            run(&ctx, &json!({"pattern": "*.none"})).unwrap().content,
            "No files found"
        );
    }
}
