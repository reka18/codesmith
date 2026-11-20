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
//! Deliberate differences: configured URLs are not fetched, the files are
//! read once per session (no change or removal notices mid-session, so
//! `instruction-state.test.ts` has no counterpart), and trailing whitespace
//! of each file is trimmed.

use std::path::{Path, PathBuf};

fn claude_fallback() -> bool {
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
    let global = global_dir.join("AGENTS.md");
    let claude_global = home.join(".claude/CLAUDE.md");
    if global.is_file() {
        add(global);
    } else if claude && claude_global.is_file() {
        add(claude_global);
    }
    let stop = if cwd.starts_with(home) { home } else { root };
    for d in cwd.ancestors() {
        if let Some(f) = in_dir(d) {
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
