//! Project instructions: `AGENTS.md` files (with `CLAUDE.md` as a fallback),
//! the global one in the config dir, and files listed under `instructions`.
//!
//! Ported from OpenCode's instruction discovery and pinned to it by the
//! `oc_instructions_*` tests (from `packages/core/test/instruction-discovery.test.ts`).
//! Load order, each file once:
//!   1. the global `<config dir>/AGENTS.md`, else `~/.claude/CLAUDE.md`
//!   2. from the working directory up, nearest first, each folder's
//!      `AGENTS.md`, else its `CLAUDE.md`; the walk ends at the home folder
//!      when the working directory is inside it, else at the project root
//!   3. each configured path or glob in order (matches sorted): `~/` is the
//!      home folder, a relative one is looked for in every folder from the
//!      working directory up to the project root
//!
//! The files are read again before each model step ([`source`]): a file
//! that changes, appears or goes away mid-session reaches the model as an
//! update (a diff when that is shorter), not as a new system prompt (see
//! [`crate::instruction_state`]).
//!
//! Deliberate differences: configured URLs are not fetched, and trailing
//! whitespace of each file is trimmed.

use crate::instruction_state::{self as is, Definition, Observed};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub(crate) fn claude_fallback() -> bool {
    std::env::var_os("CODESMITH_DISABLE_CLAUDE_CODE").is_none()
}

/// Instruction files in load order (see the module docs).
pub fn discover(cwd: &Path, root: &Path, global_dir: &Path, configured: &[String]) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    discover_in(cwd, root, global_dir, &home, configured)
}

/// [`discover`] with an explicit home folder.
pub fn discover_in(
    cwd: &Path,
    root: &Path,
    global_dir: &Path,
    home: &Path,
    configured: &[String],
) -> Vec<PathBuf> {
    walk(cwd, root, global_dir, home, configured, claude_fallback())
}

fn walk(
    cwd: &Path,
    root: &Path,
    global_dir: &Path,
    home: &Path,
    configured: &[String],
    claude: bool,
) -> Vec<PathBuf> {
    let in_dir = |dir: &Path| -> Option<PathBuf> {
        let agents = dir.join("AGENTS.md");
        if agents.is_file() {
            return Some(agents);
        }
        let claude_md = dir.join("CLAUDE.md");
        (claude && claude_md.is_file()).then_some(claude_md)
    };
    let mut out = Vec::new();
    let mut seen = Vec::new();
    let mut add = |p: PathBuf| {
        let key = p.canonicalize().unwrap_or_else(|_| p.clone());
        if !seen.contains(&key) {
            seen.push(key);
            out.push(p);
        }
    };
    let global_on = !crate::config::global_disabled();
    let global = global_dir.join("AGENTS.md");
    let claude_global = home.join(".claude/CLAUDE.md");
    if global_on && global.is_file() {
        add(global);
    } else if global_on && claude && claude_global.is_file() {
        add(claude_global);
    }
    let stop = if cwd.starts_with(home) { home } else { root };
    for d in cwd.ancestors() {
        // The home and global folders are global sources.
        let skip = !global_on && crate::config::global_source(d, global_dir, home);
        if let Some(f) = in_dir(d).filter(|_| !skip) {
            add(f);
        }
        if d == stop || !d.starts_with(stop) {
            break;
        }
    }
    for pattern in configured {
        if pattern.starts_with("http://") || pattern.starts_with("https://") {
            continue;
        }
        let expanded = match pattern.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(pattern),
        };
        let mut matched = if expanded.is_absolute() {
            glob_files(&expanded)
        } else {
            let mut m = Vec::new();
            for d in cwd.ancestors() {
                m.extend(glob_files(&d.join(&expanded)));
                if d == root || !d.starts_with(root) {
                    break;
                }
            }
            m
        };
        matched.sort();
        for m in matched {
            add(m);
        }
    }
    out
}

/// Files matching an absolute path that may contain `*`, `?`, `[` or `**`.
/// `*` stays within one folder; hidden folders are searched too.
fn glob_files(pattern: &Path) -> Vec<PathBuf> {
    let is_glob = |s: &str| s.contains(['*', '?', '[', '{']);
    let text = pattern.to_string_lossy();
    if !is_glob(&text) {
        return if pattern.is_file() {
            vec![pattern.to_path_buf()]
        } else {
            vec![]
        };
    }
    let mut base = PathBuf::new();
    let mut rest: Vec<String> = Vec::new();
    for c in pattern.components() {
        let s = c.as_os_str().to_string_lossy().into_owned();
        if rest.is_empty() && !is_glob(&s) {
            base.push(c);
        } else {
            rest.push(s);
        }
    }
    let Ok(glob) = globset::GlobBuilder::new(&rest.join("/"))
        .literal_separator(true)
        .build()
    else {
        return vec![];
    };
    let m = glob.compile_matcher();
    if !base.is_dir() {
        return vec![];
    }
    ignore::WalkBuilder::new(&base)
        .standard_filters(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.into_path())
        .filter(|p| p.strip_prefix(&base).is_ok_and(|r| m.is_match(r)))
        .collect()
}

/// Render files as `Instructions from: <path>` sections.
pub fn render(files: &[PathBuf]) -> String {
    files
        .iter()
        .filter_map(|f| {
            std::fs::read_to_string(f)
                .ok()
                .map(|t| format!("Instructions from: {}\n{}", f.display(), t.trim_end()))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// One loaded instruction file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    pub content: String,
}

impl File {
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> File {
        File {
            path: path.into(),
            content: content.into(),
        }
    }
}

/// Read discovered files: one that cannot be read (or vanished since it was
/// found, as an editor's save can make it) leaves the whole source
/// unavailable for now, so its stored value stands; none at all is a
/// removal.
pub fn read(files: &[PathBuf]) -> Observed<Vec<File>> {
    let mut out = Vec::new();
    for f in files {
        match std::fs::read_to_string(f) {
            Ok(t) => out.push(File::new(f.display().to_string(), t.trim_end())),
            Err(_) => return Observed::Unavailable,
        }
    }
    if out.is_empty() {
        Observed::Removed
    } else {
        Observed::Value(out)
    }
}

/// `Instructions from: <path>` sections.
pub fn render_files(files: &[File]) -> String {
    files
        .iter()
        .map(|f| format!("Instructions from: {}\n{}", f.path, f.content))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A unified diff of two texts from the first hunk on, three lines of
/// context (OpenCode's `createPatch` sliced at `@@`).
fn patch(path: &str, before: &str, after: &str) -> String {
    let diff = similar::TextDiff::from_lines(before, after);
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    text.find("@@")
        .map(|i| text[i..].trim_end().to_string())
        .unwrap_or_default()
}

fn render_update(previous: &[File], current: &[File]) -> Option<String> {
    let changes = is::diff_by_key(
        previous,
        current,
        |f| f.path.clone(),
        |a, b| a.content != b.content,
    );
    let mut parts: Vec<String> = Vec::new();
    for f in &changes.removed {
        parts.push(format!("The instructions from {} no longer apply.", f.path));
    }
    for f in &changes.added {
        parts.push(format!(
            "New instructions apply from:\n{}",
            render_files(std::slice::from_ref(f))
        ));
    }
    for (before, after) in &changes.changed {
        let diff = format!(
            "The instructions from {} changed. Here's the diff:\n```diff\n{}\n```",
            after.path,
            patch(&after.path, &before.content, &after.content)
        );
        let replacement = format!(
            "The instructions changed:\n{}",
            render_files(std::slice::from_ref(after))
        );
        parts.push(if diff.chars().count() < replacement.chars().count() {
            diff
        } else {
            replacement
        });
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

pub const KEY: &str = "core/instructions";

/// The instruction files as an instruction source.
pub fn source(observed: Observed<Vec<File>>) -> is::List {
    is::make::<Vec<File>>(Definition {
        key: KEY.into(),
        read: Box::new(move || observed.clone()),
        initial: Box::new(|f| render_files(f)),
        changed: Box::new(|a, b| render_update(a, b)),
        removed: Some(Box::new(|_| {
            "Previously loaded instructions no longer apply.".into()
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        (d, root)
    }

    fn write(p: &Path, text: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn section(p: &Path, text: &str) -> String {
        format!("Instructions from: {}\n{text}", p.display())
    }

    /// The home folder for tests that should not see the real one.
    fn nowhere(r: &Path) -> PathBuf {
        r.join("no-home")
    }

    #[test]
    fn oc_instructions_loads_global_and_upward_project_files() {
        let (_d, t) = tmp();
        let (global, home) = (t.join("global"), t.join("home"));
        let shared = home.join("code");
        let project = shared.join("repo");
        let directory = project.join("packages/core");
        fs::create_dir_all(&directory).unwrap();
        write(&t.join("AGENTS.md"), "outside");
        write(&global.join("AGENTS.md"), "global");
        write(&shared.join("AGENTS.md"), "shared");
        write(&project.join("AGENTS.md"), "project");
        write(&directory.join("AGENTS.md"), "package");
        let files = walk(&directory, &project, &global, &home, &[], true);
        assert_eq!(
            render(&files),
            [
                section(&global.join("AGENTS.md"), "global"),
                section(&directory.join("AGENTS.md"), "package"),
                section(&project.join("AGENTS.md"), "project"),
                section(&shared.join("AGENTS.md"), "shared"),
            ]
            .join("\n\n"),
            "nearest first, past the project root up to home, never above it"
        );
    }

    #[test]
    fn oc_instructions_loads_configured_files_globs_and_home_paths() {
        let (_d, t) = tmp();
        let project = t.join("repo");
        let directory = project.join("packages/core");
        fs::create_dir_all(&directory).unwrap();
        let agents = project.join("AGENTS.md");
        let (first, second) = (project.join("rules/a.md"), project.join("rules/b.md"));
        let absolute = t.join("absolute.md");
        let home_file = t.join("home.md");
        let note_top = t.join("notes/guide.md");
        let note_nested = t.join("notes/projects/memory.md");
        for (p, text) in [
            (&agents, "agents"),
            (&first, "first"),
            (&second, "second"),
            (&absolute, "absolute"),
            (&home_file, "home"),
            (&note_top, "note top"),
            (&note_nested, "note nested"),
        ] {
            write(p, text);
        }
        let configured: Vec<String> = [
            "rules/*.md",
            absolute.to_str().unwrap(),
            "~/home.md",
            "~/notes/**/*.md",
            "https://example.test/instructions.md",
            "missing.md",
        ]
        .map(String::from)
        .to_vec();
        let files = walk(
            &directory,
            &project,
            &t.join("global"),
            &t,
            &configured,
            true,
        );
        // OpenCode also appends the fetched URL; CodeSmith fetches nothing.
        assert_eq!(
            files,
            vec![
                agents,
                first,
                second,
                absolute,
                home_file,
                note_top,
                note_nested
            ]
        );
    }

    #[test]
    fn oc_instructions_falls_back_to_claude_md_where_no_agents_md_exists() {
        let (_d, t) = tmp();
        let (global, home) = (t.join("global"), t.join("home"));
        let project = home.join("repo");
        let directory = project.join("packages/core");
        let claude_global = home.join(".claude/CLAUDE.md");
        write(&claude_global, "claude global");
        write(&project.join("AGENTS.md"), "project agents");
        write(&project.join("CLAUDE.md"), "project claude");
        write(&directory.join("CLAUDE.md"), "package claude");
        let load = |claude| render(&walk(&directory, &project, &global, &home, &[], claude));
        assert_eq!(
            load(true),
            [
                section(&claude_global, "claude global"),
                section(&directory.join("CLAUDE.md"), "package claude"),
                section(&project.join("AGENTS.md"), "project agents"),
            ]
            .join("\n\n")
        );
        write(&global.join("AGENTS.md"), "global agents");
        let preferred = load(true);
        assert!(preferred.contains(&section(&global.join("AGENTS.md"), "global agents")));
        assert!(!preferred.contains("claude global"));
        let disabled = load(false);
        assert!(!disabled.contains("CLAUDE.md"));
        assert!(disabled.contains(&section(&project.join("AGENTS.md"), "project agents")));
    }

    #[test]
    fn oc_instructions_keeps_an_empty_agents_md() {
        let (_d, t) = tmp();
        write(&t.join("AGENTS.md"), "");
        let files = walk(&t, &t, &t.join("global"), &nowhere(&t), &[], true);
        assert_eq!(render(&files), section(&t.join("AGENTS.md"), ""));
    }

    #[test]
    fn oc_instructions_finds_a_file_above_the_project_root_inside_home() {
        let (_d, t) = tmp();
        let home = t.join("home");
        let shared = home.join("code");
        let project = shared.join("repo");
        let directory = project.join("core");
        write(&project.join("AGENTS.md"), "project");
        fs::create_dir_all(&directory).unwrap();
        let load = || walk(&directory, &project, &t.join("global"), &home, &[], true);
        assert_eq!(load(), vec![project.join("AGENTS.md")]);
        write(&shared.join("AGENTS.md"), "intermediate");
        assert_eq!(
            load(),
            vec![project.join("AGENTS.md"), shared.join("AGENTS.md")]
        );
    }

    #[test]
    fn oc_instructions_stop_at_the_project_root_outside_home() {
        let (_d, t) = tmp();
        let project = t.join("scratch/repo");
        let directory = project.join("packages/core");
        fs::create_dir_all(&directory).unwrap();
        write(&t.join("scratch/AGENTS.md"), "above the root");
        write(&project.join("packages/AGENTS.md"), "packages");
        let files = walk(
            &directory,
            &project,
            &t.join("global"),
            &t.join("home"),
            &[],
            true,
        );
        assert_eq!(files, vec![project.join("packages/AGENTS.md")]);
    }

    #[test]
    fn oc_instructions_each_file_once_at_its_first_position() {
        let (_d, t) = tmp();
        write(&t.join("AGENTS.md"), "root");
        write(&t.join("docs/a.md"), "a");
        let configured = ["docs/a.md", "AGENTS.md", "docs/*.md"].map(String::from);
        let files = walk(&t, &t, &t.join("global"), &nowhere(&t), &configured, true);
        assert_eq!(files, vec![t.join("AGENTS.md"), t.join("docs/a.md")]);
    }

    #[test]
    fn oc_instructions_relative_patterns_search_up_to_the_project_root() {
        let (_d, t) = tmp();
        let directory = t.join("packages/core");
        write(&directory.join("RULES.md"), "near");
        write(&t.join("RULES.md"), "root");
        write(&t.join(".cursor/rules/style.md"), "hidden folder");
        write(&t.join(".cursor/rules/deep/skip.md"), "too deep for *");
        let configured = ["RULES.md", ".cursor/rules/*.md"].map(String::from);
        let files = walk(
            &directory,
            &t,
            &t.join("global"),
            &nowhere(&t),
            &configured,
            true,
        );
        // Matches of one pattern are sorted, as in OpenCode.
        assert_eq!(
            files,
            vec![
                t.join("RULES.md"),
                directory.join("RULES.md"),
                t.join(".cursor/rules/style.md"),
            ]
        );
    }

    #[test]
    fn nothing_found() {
        let (_d, t) = tmp();
        assert!(walk(&t, &t, &t.join("global"), &nowhere(&t), &[], true).is_empty());
        assert_eq!(render(&[]), "");
    }
}
