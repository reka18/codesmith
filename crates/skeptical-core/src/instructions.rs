//! Project instructions: `AGENTS.md` files (with `CLAUDE.md` as a fallback),
//! the global one in the config dir, and files listed under `instructions`.

use std::path::{Path, PathBuf};

fn claude_fallback() -> bool {
    std::env::var_os("SKEPTICAL_DISABLE_CLAUDE_CODE").is_none()
}

/// The instruction file in `dir`: `AGENTS.md`, else `CLAUDE.md`.
fn in_dir(dir: &Path) -> Option<PathBuf> {
    let agents = dir.join("AGENTS.md");
    if agents.is_file() {
        return Some(agents);
    }
    let claude = dir.join("CLAUDE.md");
    (claude_fallback() && claude.is_file()).then_some(claude)
}

/// Instruction files in load order: global, then root down to `cwd`, then
/// configured paths and globs (relative to `root`).
pub fn discover(cwd: &Path, root: &Path, global_dir: &Path, configured: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(g) = in_dir(global_dir) {
        out.push(g);
    }
    let mut dirs: Vec<&Path> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(root))
        .collect();
    dirs.reverse();
    for d in dirs {
        if let Some(f) = in_dir(d) {
            out.push(f);
        }
    }
    for pattern in configured {
        let expanded = match pattern.strip_prefix("~/") {
            Some(rest) => dirs::home_dir()
                .unwrap_or_default()
                .join(rest)
                .display()
                .to_string(),
            None if Path::new(pattern).is_absolute() => pattern.clone(),
            None => root.join(pattern).display().to_string(),
        };
        let mut matched: Vec<PathBuf> = match globset::Glob::new(&expanded) {
            Ok(g) if pattern.contains(['*', '?', '[']) => {
                let m = g.compile_matcher();
                ignore::WalkBuilder::new(root)
                    .build()
                    .flatten()
                    .map(|e| e.into_path())
                    .filter(|p| p.is_file() && m.is_match(p))
                    .collect()
            }
            _ => vec![PathBuf::from(&expanded)]
                .into_iter()
                .filter(|p| p.is_file())
                .collect(),
        };
        matched.sort();
        for m in matched {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
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

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("repo/pkg/src")).unwrap();
        std::fs::create_dir_all(root.join("global")).unwrap();
        (d, root)
    }

    #[test]
    fn walks_from_root_to_cwd_with_claude_fallback() {
        let (_d, r) = setup();
        let repo = r.join("repo");
        std::fs::write(repo.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "ignored: AGENTS.md wins").unwrap();
        std::fs::write(repo.join("pkg/CLAUDE.md"), "pkg rules").unwrap();
        std::fs::write(r.join("global/AGENTS.md"), "global rules").unwrap();
        let files = discover(&repo.join("pkg/src"), &repo, &r.join("global"), &[]);
        assert_eq!(
            files,
            vec![
                r.join("global/AGENTS.md"),
                repo.join("AGENTS.md"),
                repo.join("pkg/CLAUDE.md")
            ]
        );
        let text = render(&files);
        assert!(text.starts_with(&format!(
            "Instructions from: {}\nglobal rules",
            r.join("global/AGENTS.md").display()
        )));
        assert!(text.contains("pkg rules") && !text.contains("ignored"));
    }

    #[test]
    fn configured_files_and_globs() {
        let (_d, r) = setup();
        let repo = r.join("repo");
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/a.md"), "a").unwrap();
        std::fs::write(repo.join("docs/b.md"), "b").unwrap();
        std::fs::write(repo.join("STYLE.md"), "style").unwrap();
        let files = discover(
            &repo,
            &repo,
            &r.join("global"),
            &["STYLE.md".into(), "docs/*.md".into(), "missing.md".into()],
        );
        assert_eq!(
            files,
            vec![
                repo.join("STYLE.md"),
                repo.join("docs/a.md"),
                repo.join("docs/b.md")
            ]
        );
    }

    #[test]
    fn nothing_found() {
        let (_d, r) = setup();
        assert!(discover(&r.join("repo"), &r.join("repo"), &r.join("global"), &[]).is_empty());
        assert_eq!(render(&[]), "");
    }
}
