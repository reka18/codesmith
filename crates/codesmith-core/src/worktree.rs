//! Git worktrees for isolated sessions, as in OpenCode's `--worktree`:
//! `git worktree add --detach` under `<data>/worktree/<project>/<slug>`,
//! then an optional start command.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `worktree` in codesmith.json.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorktreeConfig {
    /// Parent folder for new worktrees (default `<data>/worktree/<project>`).
    pub directory: Option<String>,
    /// Shell command run in each new worktree (e.g. installing dependencies).
    /// `$CODESMITH_WORKTREE_BASE` and `$CODESMITH_WORKTREE_PATH` are set.
    pub start: Option<String>,
}

const ADJECTIVES: [&str; 29] = [
    "brave", "calm", "clever", "cosmic", "crisp", "curious", "eager", "gentle", "glowing", "happy",
    "hidden", "jolly", "kind", "lucky", "mighty", "misty", "neon", "nimble", "playful", "proud",
    "quick", "quiet", "shiny", "silent", "stellar", "sunny", "swift", "tidy", "witty",
];
const NOUNS: [&str; 31] = [
    "cabin", "cactus", "canyon", "circuit", "comet", "eagle", "engine", "falcon", "forest",
    "garden", "harbor", "island", "knight", "lagoon", "meadow", "moon", "mountain", "nebula",
    "orchid", "otter", "panda", "pixel", "planet", "river", "rocket", "sailor", "squid", "star",
    "tiger", "wizard", "wolf",
];

/// A name like `swift-otter`.
pub fn slug() -> String {
    use rand::seq::SliceRandom;
    let mut r = rand::thread_rng();
    format!(
        "{}-{}",
        ADJECTIVES.choose(&mut r).unwrap(),
        NOUNS.choose(&mut r).unwrap()
    )
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .env_remove(crate::SERVER_TOKEN_VAR)
        .args(args)
        .current_dir(dir)
        .output()
        .context("running git")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A stable id for the project: its first commit, shared by every clone and
/// worktree of it (falls back to the top-level path for repos without commits).
pub fn project_id(root: &Path) -> String {
    if let Ok(r) = git(root, &["rev-list", "--max-parents=0", "HEAD"]) {
        if let Some(first) = r.lines().min() {
            return first.chars().take(12).collect();
        }
    }
    use sha2::Digest;
    let h = sha2::Sha256::digest(root.display().to_string().as_bytes());
    h.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// The main worktree of the repository containing `dir`.
pub fn main_worktree(dir: &Path) -> Result<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .map_err(|_| anyhow!("{} is not in a git repository", dir.display()))?;
    list(dir)?
        .into_iter()
        .find(|e| e.main)
        .map(|e| e.directory)
        .ok_or_else(|| anyhow!("no main worktree"))
}

pub fn base_dir(cfg: &WorktreeConfig, root: &Path) -> PathBuf {
    match cfg.directory.as_deref().filter(|d| !d.is_empty()) {
        Some(d) if Path::new(d).is_absolute() => PathBuf::from(d),
        Some(d) => root.join(d),
        None => crate::paths::data_dir()
            .join("worktree")
            .join(project_id(root)),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub directory: PathBuf,
    pub main: bool,
    pub head: Option<String>,
    pub branch: Option<String>,
}

pub fn list(dir: &Path) -> Result<Vec<Entry>> {
    let out = git(dir, &["worktree", "list", "--porcelain"])?;
    let mut v: Vec<Entry> = Vec::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            v.push(Entry {
                directory: PathBuf::from(p),
                main: v.is_empty(),
                head: None,
                branch: None,
            });
        } else if let (Some(e), Some(h)) = (v.last_mut(), line.strip_prefix("HEAD ")) {
            e.head = Some(h.chars().take(7).collect());
        } else if let (Some(e), Some(b)) = (v.last_mut(), line.strip_prefix("branch ")) {
            e.branch = Some(b.trim_start_matches("refs/heads/").to_string());
        }
    }
    Ok(v)
}

#[derive(Debug, Clone, Default)]
pub struct Create {
    pub name: Option<String>,
    /// Commit or branch to start from (default HEAD).
    pub from: Option<String>,
    /// Create this new branch instead of a detached HEAD.
    pub branch: Option<String>,
}

/// Create a worktree of the repository containing `cwd`. Returns the
/// worktree and the folder matching `cwd` inside it.
pub fn create(cfg: &WorktreeConfig, cwd: &Path, opts: &Create) -> Result<(PathBuf, PathBuf)> {
    let toplevel = PathBuf::from(
        git(cwd, &["rev-parse", "--show-toplevel"])
            .map_err(|_| anyhow!("{} is not in a git repository", cwd.display()))?,
    );
    let main = main_worktree(cwd)?;
    let parent = base_dir(cfg, &main);
    std::fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    let name = opts
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(slug);
    if name.contains('/') || name.starts_with('.') {
        return Err(anyhow!(
            "worktree names cannot contain '/' or start with '.'"
        ));
    }
    let mut dir = parent.join(&name);
    let mut n = 1;
    while dir.exists() {
        n += 1;
        if n > 10 {
            return Err(anyhow!("{} already exists", parent.join(&name).display()));
        }
        dir = parent.join(format!("{name}-{n}"));
    }
    let d = dir.display().to_string();
    let from = opts.from.clone().unwrap_or_else(|| "HEAD".into());
    match &opts.branch {
        Some(b) => git(&toplevel, &["worktree", "add", "-b", b, "--", &d, &from])?,
        None => git(&toplevel, &["worktree", "add", "--detach", "--", &d, &from])?,
    };
    let dir = dir.canonicalize().unwrap_or(dir);
    copy_local_config(&toplevel, &dir)?;
    if let Some(cmd) = cfg
        .start
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        let status = Command::new("bash")
            .env_remove(crate::SERVER_TOKEN_VAR)
            .args(["-lc", cmd])
            .current_dir(&dir)
            .env("CODESMITH_WORKTREE_BASE", &toplevel)
            .env("CODESMITH_WORKTREE_PATH", &dir)
            .stdin(std::process::Stdio::null())
            .status()
            .context("running the worktree start command")?;
        if !status.success() {
            return Err(anyhow!(
                "the worktree start command failed ({status}); the worktree is at {}",
                dir.display()
            ));
        }
    }
    // Keep the caller's subfolder when the worktree has it.
    let top = toplevel.canonicalize().unwrap_or(toplevel);
    let here = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let sub = here
        .strip_prefix(&top)
        .ok()
        .map(|s| dir.join(s))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| dir.clone());
    Ok((dir, sub))
}

/// Whether a repo-relative path is CodeSmith configuration.
fn is_config(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    crate::config::NAMES.contains(&name) || path.split('/').any(|c| c == ".codesmith")
}

/// Copy CodeSmith config that git does not track (an uncommitted
/// `codesmith.json`, `.codesmith/` contents) into a new worktree, which is
/// otherwise a clean checkout. Returns the files copied.
pub fn copy_local_config(from: &Path, to: &Path) -> Result<Vec<String>> {
    let mut copied = Vec::new();
    let untracked = git(from, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    let ignored = git(
        from,
        &["ls-files", "--others", "--ignored", "--exclude-standard"],
    )
    .unwrap_or_default();
    for f in untracked
        .lines()
        .chain(ignored.lines())
        .filter(|f| !f.is_empty() && is_config(f))
    {
        let (src, dst) = (from.join(f), to.join(f));
        if !src.is_file() || dst.exists() {
            continue;
        }
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(&src, &dst).with_context(|| format!("copying {f} into the worktree"))?;
        copied.push(f.to_string());
    }
    Ok(copied)
}

/// Remove a linked worktree, by path or by name under the base folder.
pub fn remove(cfg: &WorktreeConfig, cwd: &Path, which: &str, force: bool) -> Result<PathBuf> {
    let main = main_worktree(cwd)?;
    let candidates = [PathBuf::from(which), base_dir(cfg, &main).join(which)];
    let entries = list(cwd)?;
    let target = candidates
        .iter()
        .filter_map(|c| c.canonicalize().ok())
        .find_map(|c| {
            entries
                .iter()
                .find(|e| e.directory.canonicalize().ok().as_ref() == Some(&c))
        })
        .ok_or_else(|| anyhow!("no worktree {which:?}; `codesmith worktree list` shows them"))?;
    if target.main {
        return Err(anyhow!(
            "{} is the main worktree and cannot be removed",
            target.directory.display()
        ));
    }
    let d = target.directory.display().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&d);
    git(&main, &args).map_err(|e| {
        if !force && e.to_string().contains("modified or untracked") {
            anyhow!("{d} has uncommitted changes; pass --force to remove it anyway")
        } else {
            e
        }
    })?;
    Ok(target.directory.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for a in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(r, a).unwrap();
        }
        std::fs::create_dir_all(r.join("pkg/src")).unwrap();
        std::fs::write(r.join("pkg/src/a.txt"), "a").unwrap();
        git(r, &["add", "."]).unwrap();
        git(r, &["commit", "-q", "-m", "first"]).unwrap();
        d
    }

    #[test]
    fn slugs() {
        let s = slug();
        let (a, n) = s.split_once('-').unwrap();
        assert!(ADJECTIVES.contains(&a) && NOUNS.contains(&n), "{s}");
    }

    #[test]
    fn create_list_remove() {
        let d = repo();
        let r = d.path();
        let base = tempfile::tempdir().unwrap();
        let cfg = WorktreeConfig {
            directory: Some(base.path().display().to_string()),
            start: Some(
                "echo started > started.txt; echo \"$CODESMITH_WORKTREE_BASE\" > base.txt".into(),
            ),
        };
        let (wt, sub) = create(
            &cfg,
            &r.join("pkg/src"),
            &Create {
                name: Some("fix-it".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(wt.file_name().unwrap(), "fix-it");
        assert_eq!(sub, wt.join("pkg/src"), "the caller's subfolder is kept");
        assert!(wt.join("pkg/src/a.txt").exists());
        assert!(
            wt.join("started.txt").exists(),
            "start command ran in the worktree"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("base.txt")).unwrap().trim(),
            r.canonicalize().unwrap().display().to_string()
        );

        let (wt2, _) = create(
            &cfg,
            r,
            &Create {
                name: Some("fix-it".into()),
                branch: Some("feature-x".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            wt2.file_name().unwrap(),
            "fix-it-2",
            "taken names get a suffix"
        );
        let l = list(r).unwrap();
        assert_eq!(l.len(), 3);
        assert!(l[0].main && !l[1].main);
        assert_eq!(
            l.iter()
                .find(|e| e.directory.ends_with("fix-it-2"))
                .unwrap()
                .branch
                .as_deref(),
            Some("feature-x")
        );

        std::fs::write(wt.join("pkg/src/a.txt"), "changed").unwrap();
        assert!(remove(&cfg, r, "fix-it", false)
            .unwrap_err()
            .to_string()
            .contains("--force"));
        assert_eq!(remove(&cfg, r, "fix-it", true).unwrap(), wt);
        assert!(!wt.exists());
        assert!(remove(&cfg, r, &r.display().to_string(), false)
            .unwrap_err()
            .to_string()
            .contains("main worktree"));
        assert!(remove(&cfg, r, "nope", false).is_err());
    }

    #[test]
    fn untracked_config_follows_into_new_worktrees() {
        let d = repo();
        let r = d.path();
        std::fs::write(r.join("codesmith.json"), "{}").unwrap();
        std::fs::create_dir_all(r.join(".codesmith/commands")).unwrap();
        std::fs::write(r.join(".codesmith/commands/x.md"), "x").unwrap();
        std::fs::write(r.join("pkg/codesmith.jsonc"), "{}").unwrap();
        std::fs::write(r.join("notes.txt"), "not config").unwrap();
        let base = tempfile::tempdir().unwrap();
        let cfg = WorktreeConfig {
            directory: Some(base.path().display().to_string()),
            start: None,
        };
        let (wt, _) = create(&cfg, r, &Create::default()).unwrap();
        assert!(
            wt.join("codesmith.json").exists()
                && wt.join(".codesmith/commands/x.md").exists()
                && wt.join("pkg/codesmith.jsonc").exists()
        );
        assert!(
            !wt.join("notes.txt").exists(),
            "only CodeSmith config is copied"
        );
    }

    #[test]
    fn project_ids_are_shared_by_worktrees() {
        let d = repo();
        let r = d.path();
        let base = tempfile::tempdir().unwrap();
        let cfg = WorktreeConfig {
            directory: Some(base.path().display().to_string()),
            start: None,
        };
        let (wt, _) = create(&cfg, r, &Create::default()).unwrap();
        assert_eq!(project_id(r), project_id(&wt));
        assert_eq!(
            main_worktree(&wt).unwrap().canonicalize().unwrap(),
            r.canonicalize().unwrap()
        );
        assert!(create(&cfg, Path::new("/"), &Create::default()).is_err());
    }
}
