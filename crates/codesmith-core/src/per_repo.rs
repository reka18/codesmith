//! Per-repo config: `projects/*.json[c]` in CodeSmith's global config
//! folder, each naming the repositories it applies to with `path`.
//!
//! A repository is someone else's input, so CodeSmith never reads config
//! from inside one. Settings for one repository live in your own folder
//! instead:
//!
//! ```jsonc
//! // ~/.config/codesmith/projects/app.jsonc
//! { "path": "~/src/app", "model": "anthropic/claude-sonnet-4-5" }
//! ```
//!
//! - `path` is a string or a list of strings: absolute paths or globs, `~`
//!   expanded. It is matched against the project root (the nearest folder
//!   with `.git` or `.hg`, see [`crate::config::project_root`]) after
//!   symlinks are resolved on both sides. A git remote never matches.
//! - In a glob, `*`, `?` and `[...]` stay within one folder name and `**`
//!   crosses folders.
//! - Every matching file applies, least specific first (see [`Rank`]), so
//!   the most specific wins. Equally specific files apply in file-name
//!   order, with a warning.
//! - `projects/<name>/` beside `<name>.json[c]` holds that file's `agents/`,
//!   `modes/`, `commands/` and `themes/`, applied wherever the file matches.
//! - A matching file or folder that someone else could change (group or
//!   world writable, or owned by another user) is refused with an error
//!   naming it.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// The folder in the global config folder that holds per-repo files.
pub const DIR: &str = "projects";

/// How specifically a `path` names a project root, compared field by field:
/// an exact path beats any glob, then a longer literal start (the text
/// before the first `*`, `?`, `[` or `{`) beats a shorter one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    pub exact: bool,
    pub literal: usize,
}

/// A per-repo file that matches the project root.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// The file (`projects/<name>.json[c]`).
    pub file: PathBuf,
    /// Its folder (`projects/<name>/`), when there is one.
    pub dir: Option<PathBuf>,
    /// The `path` entry that matched most specifically, as written.
    pub pattern: String,
    pub rank: Rank,
}

/// The per-repo files that apply to one project root, in the order they
/// apply, and anything worth warning about.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    pub matches: Vec<Match>,
    pub warnings: Vec<String>,
}

/// Whether `s` holds glob syntax.
fn is_glob(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// `~` and `~/rest` with the home folder; anything else as written.
fn expand(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// How `pattern` (as written in `file`) matches `root` (already resolved):
/// `None` when it doesn't.
pub fn rank(pattern: &str, root: &Path, home: &Path) -> Result<Option<Rank>> {
    let trimmed = pattern.trim();
    let trimmed = if trimmed.len() > 1 {
        trimmed.trim_end_matches('/')
    } else {
        trimmed
    };
    if trimmed.is_empty() {
        bail!("an empty `path`");
    }
    let expanded = expand(trimmed, home);
    if !expanded.is_absolute() {
        bail!("`path` {pattern:?} is not absolute (start it with / or ~)");
    }
    if expanded
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        bail!("`path` {pattern:?} may not contain `..`");
    }
    let text = expanded.to_string_lossy().into_owned();
    if !is_glob(&text) {
        let exact = real(&expanded);
        return Ok((exact == root).then_some(Rank {
            exact: true,
            literal: exact.as_os_str().len(),
        }));
    }
    // The folders before the first one with glob syntax are resolved like
    // the root is, so `/tmp/*` matches a root under `/private/tmp`.
    let mut prefix = PathBuf::new();
    let mut rest: Vec<String> = Vec::new();
    for c in expanded.components() {
        let s = c.as_os_str().to_string_lossy().into_owned();
        if rest.is_empty() && !is_glob(&s) {
            prefix.push(c.as_os_str());
        } else {
            rest.push(s);
        }
    }
    let prefix = real(&prefix);
    let mut full = prefix.to_string_lossy().into_owned();
    for r in &rest {
        if !full.ends_with('/') {
            full.push('/');
        }
        full.push_str(r);
    }
    let literal = full.find(['*', '?', '[', '{']).unwrap_or(full.len());
    let glob = globset::GlobBuilder::new(&full)
        .literal_separator(true)
        .build()
        .with_context(|| format!("`path` {pattern:?} is not a valid glob"))?
        .compile_matcher();
    Ok(glob.is_match(root).then_some(Rank {
        exact: false,
        literal,
    }))
}

/// The `path` entries of a per-repo document.
fn patterns(v: &Value) -> Result<Vec<String>> {
    match v.get("path") {
        None => bail!("it has no `path` naming the repositories it applies to"),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(a)) if !a.is_empty() => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("`path` must be a string or a list of strings"))
            })
            .collect(),
        Some(_) => bail!("`path` must be a string or a non-empty list of strings"),
    }
}

/// Refuse `path` when someone other than you could change what it holds:
/// it must be yours and not writable by group or others. A symlink must be
/// yours too, and is judged by what it leads to. (`trust.json` was checked
/// the same way before it went away.)
pub fn check_own(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        let link = std::fs::symlink_metadata(path)
            .with_context(|| format!("reading {}", path.display()))?;
        if link.file_type().is_symlink() && link.uid() != me {
            bail!(
                "refusing {}: the symlink is owned by another user",
                path.display()
            );
        }
        let meta =
            std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
        if meta.uid() != me {
            bail!(
                "refusing {}: it is owned by another user, and per-repo config must be yours",
                path.display()
            );
        }
        if meta.permissions().mode() & 0o022 != 0 {
            bail!(
                "refusing {}: others can write it (group or world); `chmod go-w` it to use it",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// [`check_own`] for a folder and everything in it (to a sane depth).
fn check_tree(dir: &Path, depth: usize) -> Result<()> {
    check_own(dir)?;
    if depth == 0 {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            check_tree(&p, depth - 1)?;
        } else {
            check_own(&p)?;
        }
    }
    Ok(())
}

/// The per-repo file names in `dir`, in file-name order.
fn files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|x| x == "json" || x == "jsonc")
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    out.sort();
    out
}

/// Read a per-repo file's `path` without filling in `{env:}` or reading any
/// `{file:}`.
fn read_patterns(file: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(file)?;
    let base = file.parent().unwrap_or(Path::new("."));
    let v = crate::config::parse_as(&text, base, crate::config::Fill::Literal)
        .or_else(|_| crate::config::parse_as(&text, base, crate::config::Fill::Env))?;
    patterns(&v)
}

/// The per-repo files in `global_dir` that apply to project root `root`,
/// least specific first. A matching file, its folder or the `projects`
/// folder that others could change is an error naming it, as is a file
/// whose `path` is missing or malformed.
pub fn find(global_dir: &Path, root: &Path) -> Result<Found> {
    find_in(global_dir, root, &dirs::home_dir().unwrap_or_default())
}

/// [`find`] with `~` meaning `home`.
pub fn find_in(global_dir: &Path, root: &Path, home: &Path) -> Result<Found> {
    let projects = global_dir.join(DIR);
    if !projects.is_dir() {
        return Ok(Found::default());
    }
    let root = real(root);
    let mut matches: Vec<Match> = Vec::new();
    for file in files(&projects) {
        let pats = read_patterns(&file).with_context(|| format!("in {}", file.display()))?;
        let mut best: Option<(Rank, String)> = None;
        for p in pats {
            let r = rank(&p, &root, home).with_context(|| format!("in {}", file.display()))?;
            if let Some(r) = r {
                if best.as_ref().is_none_or(|(b, _)| r > *b) {
                    best = Some((r, p));
                }
            }
        }
        let Some((rank, pattern)) = best else {
            continue;
        };
        let dir = file.with_extension("");
        let dir = dir.is_dir().then_some(dir);
        matches.push(Match {
            file,
            dir,
            pattern,
            rank,
        });
    }
    if matches.is_empty() {
        return Ok(Found::default());
    }
    check_own(&projects)?;
    for m in &matches {
        check_own(&m.file)?;
        if let Some(d) = &m.dir {
            check_tree(d, 6)?;
        }
    }
    // Stable: equally specific files stay in file-name order.
    matches.sort_by_key(|m| m.rank);
    let mut warnings = Vec::new();
    for pair in matches.windows(2) {
        if pair[0].rank == pair[1].rank {
            warnings.push(format!(
                "{} and {} match {} equally specifically ({:?} and {:?}); they apply in file-name order, so {} wins",
                pair[0].file.display(),
                pair[1].file.display(),
                root.display(),
                pair[0].pattern,
                pair[1].pattern,
                pair[1].file.display(),
            ));
        }
    }
    Ok(Found { matches, warnings })
}

/// Remove `path` from a per-repo document, and warn about each setting
/// only the global `codesmith.json[c]` may set: a provider's `swap`,
/// `claude_agent`, `shell.env.pass`, `update` and `autoupdate` (they run
/// commands, change process caps, pass secrets on or turn on network
/// requests). [`crate::config::load_from`] drops the first three as it does
/// from every source but the global file; the last two are only ever read
/// from the global file.
pub fn strip(v: &mut Value, file: &Path, warnings: &mut Vec<String>) {
    let Some(o) = v.as_object_mut() else {
        return;
    };
    o.remove("path");
    let mut note = |what: String| {
        warnings.push(format!(
            "{}: {what} is read only from the global codesmith.json, so it is ignored here",
            file.display()
        ))
    };
    if let Some(Value::Object(providers)) = o.get("providers") {
        for (id, p) in providers {
            if p.get("swap").is_some() {
                note(format!("providers.{id}.swap"));
            }
        }
    }
    if o.contains_key("claude_agent") {
        note("claude_agent".into());
    }
    for key in ["update", "autoupdate"] {
        if o.remove(key).is_some() {
            note(key.to_string());
        }
    }
    if o.get("shell")
        .and_then(|s| s.get("env"))
        .and_then(|e| e.get("pass"))
        .is_some()
    {
        note("shell.env.pass".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(p: &str, root: &str) -> Option<Rank> {
        rank(p, Path::new(root), Path::new("/home/me")).unwrap()
    }

    #[test]
    fn exact_paths_glob_and_tilde() {
        assert!(r("/nonexistent/src/app", "/nonexistent/src/app").is_some_and(|r| r.exact));
        assert!(r("/nonexistent/src/app/", "/nonexistent/src/app").is_some());
        assert_eq!(r("/nonexistent/src/app", "/nonexistent/src/app2"), None);
        assert!(r("~/src/app", "/home/me/src/app").is_some_and(|r| r.exact));
        assert!(r("~/src/*", "/home/me/src/app").is_some_and(|r| !r.exact));
        // `*` stays in one folder; `**` crosses.
        assert_eq!(r("~/src/*", "/home/me/src/a/b"), None);
        assert!(r("~/src/**", "/home/me/src/a/b").is_some());
        assert!(r("~/src/app-?", "/home/me/src/app-1").is_some());
        assert!(r("~/src/{a,b}", "/home/me/src/b").is_some());
    }

    #[test]
    fn exact_beats_glob_and_longer_literal_beats_shorter() {
        let exact = r("/w/src/app", "/w/src/app").unwrap();
        let near = r("/w/src/ap*", "/w/src/app").unwrap();
        let far = r("/w/*", "/w/src/app");
        assert_eq!(far, None);
        let far = r("/w/**", "/w/src/app").unwrap();
        assert!(exact > near && near > far, "{exact:?} {near:?} {far:?}");
    }

    #[test]
    fn bad_paths_are_errors() {
        for p in ["", "src/app", "./app", "~user/app", "/a/../b", "/a/[b"] {
            assert!(
                rank(p, Path::new("/a/b"), Path::new("/h")).is_err(),
                "{p:?} was accepted"
            );
        }
        for v in [
            serde_json::json!({}),
            serde_json::json!({"path": 1}),
            serde_json::json!({"path": []}),
            serde_json::json!({"path": ["/a", 2]}),
        ] {
            assert!(patterns(&v).is_err(), "{v}");
        }
    }

    #[test]
    fn strip_removes_path_and_warns_about_global_only_settings() {
        let mut v = serde_json::json!({
            "path": "/a",
            "model": "x/y",
            "providers": {"p": {"swap": {"use": ["x"]}, "settings": {}}},
            "claude_agent": {"spare": 3},
            "update": "notify",
            "autoupdate": true,
            "shell": {"path": "/bin/sh", "env": {"pass": ["TOKEN"], "remove": ["X"]}}
        });
        let mut w = Vec::new();
        strip(&mut v, Path::new("/c/projects/a.jsonc"), &mut w);
        // `path`, `update` and `autoupdate` go here; `load_from` drops
        // the rest as it does from every source but the global file.
        assert!(
            v.get("path").is_none() && v.get("update").is_none() && v.get("autoupdate").is_none()
        );
        assert_eq!(v["model"], "x/y");
        assert_eq!(w.len(), 5, "{w:?}");
        assert!(w.iter().all(|w| w.starts_with("/c/projects/a.jsonc: ")));
    }

    /// Bounded property: for any project-root-like path, its exact path
    /// matches as exact, a glob over its last folder name matches less
    /// specifically, and a sibling never matches the exact path.
    #[test]
    fn property_exact_and_glob_ranks() {
        let names = ["a", "app", "a-b", "x.y", "deep", "Z9"];
        for (i, n) in names.iter().enumerate() {
            for m in &names[i..] {
                let root = format!("/nonexistent/{n}/{m}");
                let exact = r(&root, &root).unwrap();
                let glob = r(&format!("/nonexistent/{n}/*"), &root).unwrap();
                let wide = r("/nonexistent/**", &root).unwrap();
                assert!(exact.exact && !glob.exact && exact > glob && glob > wide);
                assert_eq!(r(&format!("{root}x"), &root), None);
                assert_eq!(r(&root, &format!("{root}x")), None);
            }
        }
    }
}
