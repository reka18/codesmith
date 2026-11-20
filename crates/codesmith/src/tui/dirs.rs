//! Directory completion for shell mode (a `!command` prompt), as
//! OpenCode's `directory-completion` completes a location: the word after
//! the command (never the command itself) completes to directories,
//! relative to the working directory, to `~` or to `/`, with `..` and
//! `~` offered as they are. Dot directories show only once a `.` is typed.

use std::path::{Component, Path, PathBuf};

/// Where a query searches, what it keeps in front, and what it filters by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    pub directory: PathBuf,
    pub prefix: String,
    pub query: String,
}

/// `path` with `.` and `..` resolved, without touching the disk.
fn resolve(base: &Path, rel: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in base.join(rel).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

fn search_in(query: &str, root: &Path, prefix: &str) -> Search {
    match query.rfind('/') {
        None => Search {
            directory: root.to_path_buf(),
            prefix: prefix.into(),
            query: query.into(),
        },
        Some(sep) => {
            let parent = &query[..=sep];
            Search {
                directory: resolve(root, parent),
                prefix: format!("{prefix}{parent}"),
                query: query[sep + 1..].into(),
            }
        }
    }
}

/// Only `..` segments: `..`, `../..`.
fn only_parents(q: &str) -> bool {
    !q.is_empty() && q.split('/').all(|s| s == "..")
}

pub fn search(query: &str, directory: &Path, home: &Path) -> Search {
    if query == "~" {
        return Search {
            directory: home.to_path_buf(),
            prefix: "~/".into(),
            query: String::new(),
        };
    }
    if let Some(rest) = query.strip_prefix("~/") {
        return search_in(rest, home, "~/");
    }
    if only_parents(query) {
        return Search {
            directory: resolve(directory, query),
            prefix: format!("{query}/"),
            query: String::new(),
        };
    }
    if let Some(rest) = query.strip_prefix('/') {
        return search_in(rest, Path::new("/"), "/");
    }
    search_in(query, directory, "")
}

/// A listed directory (`name/`) as the value to insert.
pub fn result_value(directory: &str, s: &Search) -> String {
    format!("{}{}", s.prefix, directory.trim_start_matches(['/', '\\']))
}

/// The query itself, offered first when it names a whole root (`..`, `~`).
pub fn exact_value(value: &str, s: &Search) -> Option<String> {
    (!value.is_empty() && !s.prefix.is_empty() && s.query.is_empty()).then(|| value.to_string())
}

/// Whether directory `name` matches what is typed of it.
pub fn matches(name: &str, query: &str) -> bool {
    let v = name.trim_start_matches(['/', '\\']);
    if query.is_empty() && v.starts_with('.') {
        return false;
    }
    v.to_lowercase().starts_with(&query.to_lowercase())
}

/// A directory as shown in recents: under home as `~/…`.
pub fn recent_value(directory: &Path, home: &Path) -> String {
    match directory.strip_prefix(home) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => directory.display().to_string(),
    }
}

/// Most directories one completion lists.
const LIMIT: usize = 200;

/// The directories `query` completes to: (value, absolute path).
pub fn complete(query: &str, directory: &Path, home: &Path) -> Vec<(String, PathBuf)> {
    let s = search(query, directory, home);
    let mut out: Vec<(String, PathBuf)> = exact_value(query, &s)
        .map(|v| (v, s.directory.clone()))
        .into_iter()
        .collect();
    let mut names: Vec<String> = std::fs::read_dir(&s.directory)
        .into_iter()
        .flatten()
        .flatten()
        .take(LIMIT * 4)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| matches(n, &s.query))
        .collect();
    names.sort();
    names.truncate(LIMIT);
    out.extend(names.into_iter().map(|n| {
        let abs = s.directory.join(&n);
        (result_value(&format!("{n}/"), &s), abs)
    }));
    out
}

/// In a `!command` prompt, the argument word before the cursor: its byte
/// start and text. Not the command itself.
pub fn shell_argument(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = &text[..cursor];
    let body = before.strip_prefix('!')?;
    let lead = body.len() - body.trim_start().len();
    let rest = &body[lead..];
    // The command token must be finished: a separator follows it.
    let sep = rest.find(char::is_whitespace)?;
    let after = &rest[sep..];
    let start = before.len() - after.rsplit(char::is_whitespace).next().map_or(0, str::len);
    Some((start, &before[start..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(d: &str, prefix: &str, query: &str) -> Search {
        Search {
            directory: d.into(),
            prefix: prefix.into(),
            query: query.into(),
        }
    }

    // autocomplete.test.ts: "lists parents and siblings without changing location"
    #[test]
    fn oc_lists_parents_and_siblings_without_changing_location() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["current", "sibling", ".hidden"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        std::fs::write(root.join("README.md"), "x").unwrap();
        let cwd = root.join("current");
        assert_eq!(
            complete("..", &cwd, Path::new("/home/user")),
            vec![
                ("..".to_string(), root.clone()),
                ("../current/".to_string(), root.join("current")),
                ("../sibling/".to_string(), root.join("sibling")),
            ]
        );
        assert_eq!(
            complete("../sib", &cwd, Path::new("/home/user")),
            vec![("../sibling/".to_string(), root.join("sibling"))]
        );
    }

    // autocomplete.test.ts: "resolves home and nested sibling completions against the current location"
    #[test]
    fn oc_resolves_home_and_nested_sibling_completions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("sibling/src")).unwrap();
        std::fs::create_dir(root.join("current")).unwrap();
        let (cwd, home) = (root.join("current"), root.join("sibling"));
        assert_eq!(
            complete("../sibling/s", &cwd, &home),
            vec![("../sibling/src/".to_string(), root.join("sibling/src"))]
        );
        assert_eq!(
            complete("~", &cwd, &home),
            vec![
                ("~".to_string(), home.clone()),
                ("~/src/".to_string(), home.join("src"))
            ]
        );
    }

    // autocomplete.test.ts: "starts %j after the command separator" (4
    // cases; adapted: the word after a `!command`'s command)
    #[test]
    fn oc_starts_after_the_command_separator() {
        assert_eq!(shell_argument("!cd ", 4), Some((4, "")));
        assert_eq!(shell_argument("!cd src", 7), Some((4, "src")));
        assert_eq!(shell_argument("!chdir src", 10), Some((7, "src")));
        let t = "!cd\t中文";
        assert_eq!(shell_argument(t, t.len()), Some((4, "中文")));
    }

    // autocomplete.test.ts: "does not complete the command token"
    #[test]
    fn oc_does_not_complete_the_command_token() {
        assert_eq!(shell_argument("!cd", 3), None);
        assert_eq!(shell_argument("/cd ", 4), None);
    }

    // autocomplete.test.ts: "searches from home after a home prefix"
    #[test]
    fn oc_searches_from_home_after_a_home_prefix() {
        let (p, h) = (Path::new("/project"), Path::new("/home/user"));
        assert_eq!(search("~", p, h), s("/home/user", "~/", ""));
        assert_eq!(search("~/pro", p, h), s("/home/user", "~/", "pro"));
        assert_eq!(
            search("~/projects/open", p, h),
            s("/home/user/projects", "~/projects/", "open")
        );
    }

    // autocomplete.test.ts: "searches from parent prefixes"
    #[test]
    fn oc_searches_from_parent_prefixes() {
        let h = Path::new("/home/user");
        assert_eq!(
            search("..", Path::new("/project/src"), h),
            s("/project", "../", "")
        );
        assert_eq!(
            search("../../pac", Path::new("/project/src/lib"), h),
            s("/project", "../../", "pac")
        );
        assert_eq!(
            search("../../..", Path::new("/project/src/lib"), h),
            s("/", "../../../", "")
        );
    }

    // autocomplete.test.ts: "keeps ordinary searches rooted at the current directory"
    #[test]
    fn oc_keeps_ordinary_searches_rooted_at_the_current_directory() {
        let (p, h) = (Path::new("/project"), Path::new("/home/user"));
        assert_eq!(search("src", p, h), s("/project", "", "src"));
        assert_eq!(
            search("packages/core", p, h),
            s("/project/packages", "packages/", "core")
        );
        assert_eq!(search("/root/pro", p, h), s("/root", "/root/", "pro"));
    }

    // autocomplete.test.ts: "preserves explicit roots"
    #[test]
    fn oc_preserves_explicit_roots() {
        let h = Path::new("/home/user");
        assert_eq!(
            result_value("projects/", &search("~/", Path::new("/project"), h)),
            "~/projects/"
        );
        assert_eq!(
            result_value("src/", &search("../", Path::new("/project/pkg"), h)),
            "../src/"
        );
    }

    // autocomplete.test.ts: "includes complete explicit roots"
    #[test]
    fn oc_includes_complete_explicit_roots() {
        let h = Path::new("/home/user");
        let pkg = Path::new("/project/pkg");
        assert_eq!(
            exact_value("../..", &search("../..", pkg, h)).as_deref(),
            Some("../..")
        );
        let p = Path::new("/project");
        assert_eq!(exact_value("~", &search("~", p, h)).as_deref(), Some("~"));
    }

    // autocomplete.test.ts: "omits incomplete and implicit roots"
    #[test]
    fn oc_omits_incomplete_and_implicit_roots() {
        let (p, h) = (Path::new("/project"), Path::new("/home/user"));
        assert_eq!(exact_value("../../src", &search("../../src", p, h)), None);
        assert_eq!(exact_value("", &search("", p, h)), None);
    }

    // autocomplete.test.ts: "hides dot directories for an empty component"
    #[test]
    fn oc_hides_dot_directories_for_an_empty_component() {
        assert!(matches("src/", ""));
        assert!(!matches(".git/", ""));
    }

    // autocomplete.test.ts: "shows dot directories when explicitly filtered"
    #[test]
    fn oc_shows_dot_directories_when_explicitly_filtered() {
        assert!(matches(".git/", "."));
        assert!(matches(".github/", ".gi"));
        assert!(!matches(".zed/", ".gi"));
    }

    // autocomplete.test.ts: "abbreviates home paths"
    #[test]
    fn oc_abbreviates_home_paths() {
        let h = Path::new("/home/user");
        assert_eq!(recent_value(Path::new("/home/user"), h), "~");
        assert_eq!(
            recent_value(Path::new("/home/user/projects/opencode"), h),
            "~/projects/opencode"
        );
    }

    // autocomplete.test.ts: "keeps paths outside home absolute"
    #[test]
    fn oc_keeps_paths_outside_home_absolute() {
        assert_eq!(
            recent_value(Path::new("/project/recent"), Path::new("/home/user")),
            "/project/recent"
        );
    }
}
