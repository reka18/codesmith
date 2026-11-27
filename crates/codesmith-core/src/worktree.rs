//! Git worktrees for isolated sessions, as in OpenCode's `--worktree`:
//! `git worktree add --detach` under `<repo>/.worktrees/<slug>` (inside the
//! main checkout, kept out of it by `.git/info/exclude`), then an optional
//! start command. Worktrees made by older versions under
//! `<data>/worktree/<project>` keep working: git tracks them.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `worktree` in codesmith.json.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorktreeConfig {
    /// Parent folder for new worktrees (default `<repo>/.worktrees`).
    pub directory: Option<String>,
    /// Shell command run in each new worktree (e.g. installing dependencies).
    /// `$CODESMITH_WORKTREE_BASE` and `$CODESMITH_WORKTREE_PATH` are set.
    pub start: Option<String>,
}

/// The default parent of new worktrees, inside the main checkout.
pub const NESTED: &str = ".worktrees";

/// The file in a worktree's private git directory that records the commit
/// it was created from, which `remove` counts as its base.
const BASE_FILE: &str = "codesmith-base";

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
    git_raw(dir, args).map(|s| s.trim().to_string())
}

/// `git` output as it is, leading blanks kept (`status --porcelain`).
fn git_raw(dir: &Path, args: &[&str]) -> Result<String> {
    let out = crate::git::command(dir)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

/// The repository's common git directory (the main checkout's `.git`,
/// also from inside a linked worktree), as git itself finds it.
pub fn common_dir(dir: &Path) -> Result<PathBuf> {
    let p = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(|_| anyhow!("{} is not in a git repository", dir.display()))?;
    let p = PathBuf::from(p);
    Ok(p.canonicalize().unwrap_or(p))
}

/// The main worktree of the repository containing `dir`: the folder
/// holding git's common directory when that is a `.git` folder, which is
/// where git keeps it for every checkout made with `git init` or `git
/// clone`. A repository whose git directory lives elsewhere
/// (`--separate-git-dir`) falls back to git's own listing.
pub fn main_worktree(dir: &Path) -> Result<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .map_err(|_| anyhow!("{} is not in a git repository", dir.display()))?;
    let common = common_dir(dir)?;
    if common.file_name().is_some_and(|n| n == ".git") && common.is_dir() {
        if let Some(parent) = common.parent().filter(|p| p.is_dir()) {
            return Ok(parent.to_path_buf());
        }
    }
    list(dir)?
        .into_iter()
        .find(|e| e.main && !e.bare)
        .map(|e| e.directory)
        .ok_or_else(|| anyhow!("no main worktree"))
}

/// Where new worktrees go: `worktree.directory` (absolute, or relative to
/// the main checkout), else `<main>/.worktrees`.
pub fn base_dir(cfg: &WorktreeConfig, root: &Path) -> PathBuf {
    match cfg.directory.as_deref().filter(|d| !d.is_empty()) {
        Some(d) if Path::new(d).is_absolute() => PathBuf::from(d),
        Some(d) => root.join(d),
        None => root.join(NESTED),
    }
}

/// Where CodeSmith 0.6.1 and earlier put new worktrees by default:
/// `<data>/worktree/<project>`. Still searched by name on remove.
pub fn legacy_dir(root: &Path) -> PathBuf {
    crate::paths::data_dir()
        .join("worktree")
        .join(project_id(root))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub directory: PathBuf,
    pub main: bool,
    /// The main entry of a bare repository, which has no checkout.
    pub bare: bool,
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// The repository's worktrees as git tracks them, the main checkout first.
/// Each nested worktree is its own entry: the main checkout's entry is
/// only its own folder.
pub fn list(dir: &Path) -> Result<Vec<Entry>> {
    let out = git(dir, &["worktree", "list", "--porcelain"])?;
    let mut v: Vec<Entry> = Vec::new();
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            v.push(Entry {
                directory: PathBuf::from(p),
                main: v.is_empty(),
                bare: false,
                head: None,
                branch: None,
            });
        } else if let (Some(e), Some(h)) = (v.last_mut(), line.strip_prefix("HEAD ")) {
            e.head = Some(h.chars().take(7).collect());
        } else if let (Some(e), Some(b)) = (v.last_mut(), line.strip_prefix("branch ")) {
            e.branch = Some(b.trim_start_matches("refs/heads/").to_string());
        } else if let (Some(e), "bare") = (v.last_mut(), line) {
            e.bare = true;
        }
    }
    Ok(v)
}

/// Whether an exclude-file line already leaves out the folder `rel`
/// (`.worktrees`, `.worktrees/`, `/.worktrees/`, `**/.worktrees`).
fn excludes(line: &str, rel: &str) -> bool {
    let l = line.trim();
    if l.is_empty() || l.starts_with('#') || l.starts_with('!') {
        return false;
    }
    let l = l.strip_prefix("**/").unwrap_or(l);
    let l = l.strip_prefix('/').unwrap_or(l);
    let l = l.strip_suffix('/').unwrap_or(l);
    l == rel.trim_matches('/')
}

/// Add the folder `rel` (relative to the repository root) to the exclude
/// file `file` unless a line already covers it. Creates the file and its
/// folder when missing, and starts a new line when the file does not end
/// with one. Returns whether it added the line.
pub fn add_exclude(file: &Path, rel: &str) -> Result<bool> {
    let rel = rel.trim_matches('/');
    let text = match std::fs::read(file) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow!(e).context(format!("reading {}", file.display()))),
    };
    if text.lines().any(|l| excludes(l, rel)) {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let line = if rel.contains('/') {
        format!("/{rel}/")
    } else {
        format!("{rel}/")
    };
    let sep = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .and_then(|mut f| f.write_all(format!("{sep}{line}\n").as_bytes()))
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(true)
}

/// Keep `parent` out of the main checkout `main` when it is inside it:
/// its path goes into the repository's `info/exclude` (never
/// `.gitignore`). Returns whether a line was added.
fn exclude_inside(main: &Path, parent: &Path) -> Result<bool> {
    let real_main = main.canonicalize().unwrap_or_else(|_| main.to_path_buf());
    let parent = crate::tool::normalize(parent);
    let rel = parent
        .strip_prefix(&real_main)
        .or_else(|_| parent.strip_prefix(main))
        .map(Path::to_path_buf);
    let Ok(rel) = rel else {
        return Ok(false);
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    if rel.is_empty() {
        return Ok(false);
    }
    let file = common_dir(main)?.join("info").join("exclude");
    add_exclude(&file, &rel)
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
    exclude_inside(&main, &parent)?;
    std::fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
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
    let base = git(
        &toplevel,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{from}^{{commit}}"),
        ],
    )
    .ok();
    match &opts.branch {
        Some(b) => git(&toplevel, &["worktree", "add", "-b", b, "--", &d, &from])?,
        None => git(&toplevel, &["worktree", "add", "--detach", "--", &d, &from])?,
    };
    let dir = dir.canonicalize().unwrap_or(dir);
    if let (Some(base), Ok(own)) = (base, git(&dir, &["rev-parse", "--absolute-git-dir"])) {
        // Best effort: without it, `remove` measures against the main
        // checkout's HEAD.
        let _ = std::fs::write(Path::new(&own).join(BASE_FILE), format!("{base}\n"));
    }
    copy_local_config(&toplevel, &dir)?;
    if let Some(cmd) = cfg
        .start
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        let mut bash = Command::new("bash");
        crate::child_env::scrub(&mut bash);
        let status = bash
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

/// Why a worktree cannot be removed without `--force`: files it has not
/// committed, or commits that are on no remote branch and not on its base
/// (the commit it was created from, else the main checkout's HEAD), which
/// removing a detached worktree would lose.
pub fn unsaved_work(dir: &Path, main: &Path) -> Result<Option<String>> {
    let d = dir.display();
    let status = git_raw(
        dir,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )?;
    let files: Vec<&str> = status.lines().filter(|l| !l.is_empty()).collect();
    if !files.is_empty() {
        let shown: Vec<&str> = files
            .iter()
            .take(5)
            .map(|l| l.get(3..).unwrap_or(l))
            .collect();
        let more = if files.len() > shown.len() {
            format!(" and {} more", files.len() - shown.len())
        } else {
            String::new()
        };
        return Ok(Some(format!(
            "{d} has uncommitted changes ({}{more}); commit or discard them, or pass --force to remove it anyway",
            shown.join(", ")
        )));
    }
    if git(dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
        return Ok(None);
    }
    let mut bases: Vec<String> = Vec::new();
    let recorded = git(dir, &["rev-parse", "--absolute-git-dir"])
        .ok()
        .and_then(|g| std::fs::read_to_string(Path::new(&g).join(BASE_FILE)).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit()))
        .filter(|s| git(dir, &["cat-file", "-e", &format!("{s}^{{commit}}")]).is_ok());
    let described = match &recorded {
        Some(b) => format!("its base {}", &b[..7]),
        None => "the main checkout's HEAD".to_string(),
    };
    bases.extend(recorded);
    if let Ok(h) = git(main, &["rev-parse", "--verify", "--quiet", "HEAD"]) {
        bases.push(h);
    }
    let mut args = vec!["rev-list", "--max-count=101", "HEAD", "--not", "--remotes"];
    args.extend(bases.iter().map(String::as_str));
    let out = git(dir, &args)?;
    let commits: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    if commits.is_empty() {
        return Ok(None);
    }
    let count = if commits.len() > 100 {
        "more than 100".to_string()
    } else {
        commits.len().to_string()
    };
    let noun = if commits.len() == 1 {
        "commit"
    } else {
        "commits"
    };
    Ok(Some(format!(
        "{d} has {count} {noun} not on any remote branch or on {described}; push them, or pass --force to remove it anyway"
    )))
}

/// Remove a linked worktree, by path or by name under the base folder (or
/// the old default under the data folder). Without `force` it refuses a
/// worktree with uncommitted changes or unpushed commits (see
/// [`unsaved_work`]).
pub fn remove(cfg: &WorktreeConfig, cwd: &Path, which: &str, force: bool) -> Result<PathBuf> {
    let main = main_worktree(cwd)?;
    let candidates = [
        PathBuf::from(which),
        cwd.join(which),
        base_dir(cfg, &main).join(which),
        base_dir(&WorktreeConfig::default(), &main).join(which),
        legacy_dir(&main).join(which),
    ];
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
    if !force {
        if let Some(why) = unsaved_work(&target.directory, &main)? {
            return Err(anyhow!(why));
        }
    }
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

    // OpenCode's `cli/test/session-target.test.ts` (MIT): where `--worktree`
    // starts the session.
    fn named(r: &Path, from: &Path, name: &str) -> (PathBuf, PathBuf, tempfile::TempDir) {
        let base = tempfile::tempdir().unwrap();
        let cfg = WorktreeConfig {
            directory: Some(base.path().display().to_string()),
            start: None,
        };
        let _ = r;
        let (wt, sub) = create(
            &cfg,
            from,
            &Create {
                name: Some(name.into()),
                ..Default::default()
            },
        )
        .unwrap();
        (wt, sub, base)
    }

    #[test]
    fn oc_creates_a_worktree_from_the_callers_checkout() {
        let d = repo();
        let (wt, sub, _base) = named(d.path(), d.path(), "pr-12");
        assert_eq!(wt.file_name().unwrap(), "pr-12");
        assert_eq!(sub, wt);
        assert!(wt.join("pkg/src/a.txt").exists());
    }

    #[test]
    fn oc_keeps_the_callers_subfolder_in_the_new_worktree() {
        let d = repo();
        let (wt, sub, _base) = named(d.path(), &d.path().join("pkg/src"), "pr-12");
        assert_eq!(sub, wt.join("pkg/src"));
    }

    #[test]
    fn oc_uses_the_worktree_root_when_the_subfolder_is_not_in_the_worktree() {
        let d = repo();
        // An untracked folder is not checked out in the new worktree.
        std::fs::create_dir_all(d.path().join("scratch")).unwrap();
        let (wt, sub, _base) = named(d.path(), &d.path().join("scratch"), "pr-12");
        assert_eq!(sub, wt);
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

    fn commit_in(dir: &Path, file: &str, text: &str, msg: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", "--", file]).unwrap();
        git(
            dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", msg],
        )
        .unwrap();
    }

    fn exclude_file(r: &Path) -> PathBuf {
        r.join(".git/info/exclude")
    }

    fn exclude_lines(r: &Path) -> Vec<String> {
        std::fs::read_to_string(exclude_file(r))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn opts(name: &str) -> Create {
        Create {
            name: Some(name.into()),
            ..Default::default()
        }
    }

    #[test]
    fn new_worktrees_go_inside_the_main_checkout_by_default() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        let cfg = WorktreeConfig::default();
        assert_eq!(base_dir(&cfg, &r), r.join(".worktrees"));
        let (wt, sub) = create(&cfg, &r, &opts("task")).unwrap();
        assert_eq!(wt, r.join(".worktrees/task"));
        assert_eq!(sub, wt);
        assert!(wt.join("pkg/src/a.txt").exists());
        assert!(wt.join(".git").is_file(), "a linked worktree's .git file");
        // Excluded through info/exclude, never .gitignore.
        assert!(exclude_lines(&r).contains(&".worktrees/".to_string()));
        assert!(!r.join(".gitignore").exists());
        assert_eq!(
            git(&r, &["status", "--porcelain", "--untracked-files=all"]).unwrap(),
            "",
            "the main checkout does not see the nested worktree"
        );
        assert_eq!(
            git(&r, &["check-ignore", ".worktrees/task/pkg/src/a.txt"]).unwrap(),
            ".worktrees/task/pkg/src/a.txt"
        );
        // Listed as its own entry; the main checkout's entry is its own folder.
        let l = list(&r).unwrap();
        assert_eq!(l.len(), 2);
        assert!(l[0].main && !l[0].bare);
        assert_eq!(l[0].directory.canonicalize().unwrap(), r);
        assert!(!l[1].main);
        assert_eq!(l[1].directory.canonicalize().unwrap(), wt);
        assert_eq!(main_worktree(&wt).unwrap(), r);
        // Removed by name, at the new default.
        assert_eq!(remove(&cfg, &r, "task", false).unwrap(), wt);
        assert!(!wt.exists());
        assert_eq!(list(&r).unwrap().len(), 1);
        // A second create adds no second line.
        create(&cfg, &r, &opts("again")).unwrap();
        let lines = exclude_lines(&r);
        assert_eq!(
            lines.iter().filter(|l| l.contains(".worktrees")).count(),
            1,
            "{lines:?}"
        );
    }

    #[test]
    fn a_worktree_made_from_inside_a_worktree_goes_beside_it_in_the_main_checkout() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        let cfg = WorktreeConfig::default();
        let (first, _) = create(&cfg, &r, &opts("first")).unwrap();
        let (second, sub) = create(&cfg, &first.join("pkg/src"), &opts("second")).unwrap();
        assert_eq!(
            second,
            r.join(".worktrees/second"),
            "not nested in the first"
        );
        assert_eq!(sub, second.join("pkg/src"));
        assert!(!first.join(".worktrees").exists());
        assert_eq!(common_dir(&first).unwrap(), r.join(".git"));
        assert_eq!(
            exclude_lines(&r)
                .iter()
                .filter(|l| l.contains(".worktrees"))
                .count(),
            1
        );
        // Removed from inside the other worktree by name.
        remove(&cfg, &first, "second", false).unwrap();
        assert!(!second.exists());
    }

    #[test]
    fn the_main_checkout_comes_from_gits_common_dir_not_from_the_worktree() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        let (wt, _) = create(&WorktreeConfig::default(), &r, &opts("w")).unwrap();
        // A worktree that plants a `.git` folder of its own deeper down is
        // still a worktree of `r`... but a folder with its own repository is
        // its own main checkout.
        std::fs::create_dir_all(wt.join("pkg/fake/.git")).unwrap();
        assert_eq!(main_worktree(&wt.join("pkg")).unwrap(), r);
        assert!(main_worktree(Path::new("/")).is_err());
        assert!(common_dir(Path::new("/")).is_err());
    }

    #[test]
    fn the_exclude_line_is_added_once_and_respects_what_is_there() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("no/info/yet/exclude");
        // A missing folder and file.
        assert!(add_exclude(&f, ".worktrees").unwrap());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), ".worktrees/\n");
        assert!(!add_exclude(&f, ".worktrees").unwrap(), "idempotent");
        assert!(
            !add_exclude(&f, "/.worktrees/").unwrap(),
            "same folder however written"
        );
        // No trailing newline: the line starts a line of its own.
        std::fs::write(
            &f,
            "# git ls-files --others --exclude-from=.git/info/exclude\n*.log",
        )
        .unwrap();
        assert!(add_exclude(&f, ".worktrees").unwrap());
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "# git ls-files --others --exclude-from=.git/info/exclude\n*.log\n.worktrees/\n"
        );
        // Equivalent lines count; comments, negations and others do not.
        for present in [
            ".worktrees",
            ".worktrees/",
            "/.worktrees",
            "/.worktrees/",
            "  .worktrees/  ",
            "**/.worktrees",
        ] {
            std::fs::write(&f, format!("x\n{present}\n")).unwrap();
            assert!(!add_exclude(&f, ".worktrees").unwrap(), "{present:?}");
            assert_eq!(
                std::fs::read_to_string(&f).unwrap(),
                format!("x\n{present}\n")
            );
        }
        for absent in [
            "#.worktrees",
            "!.worktrees",
            ".worktrees/x",
            ".worktree",
            "a/.worktrees",
            ".worktreesx",
        ] {
            std::fs::write(&f, format!("{absent}\n")).unwrap();
            assert!(add_exclude(&f, ".worktrees").unwrap(), "{absent:?}");
        }
        // A folder deeper in the repository is anchored.
        std::fs::write(&f, "").unwrap();
        assert!(add_exclude(&f, "tmp/wt").unwrap());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "/tmp/wt/\n");
        assert!(!add_exclude(&f, "tmp/wt/").unwrap());
    }

    #[test]
    fn exclude_files_of_any_shape_end_with_one_covering_line() {
        // Bounded property check: for every mix of up to four lines from a
        // pool (equivalents, near misses, comments), with or without a
        // final newline, adding twice keeps the old text as it was, ends
        // with a covering line, and adds at most one.
        let pool = [
            "",
            "# c",
            ".worktrees",
            "/.worktrees/",
            "!.worktrees",
            "*.o",
            ".worktrees/x",
            "target/",
        ];
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("exclude");
        let mut cases = 0;
        for n in 0..=4u32 {
            for combo in 0..(pool.len() as u32).pow(n) {
                let mut c = combo;
                let lines: Vec<&str> = (0..n)
                    .map(|_| {
                        let l = pool[(c % pool.len() as u32) as usize];
                        c /= pool.len() as u32;
                        l
                    })
                    .collect();
                for newline in [false, true] {
                    let mut text = lines.join("\n");
                    if newline && !text.is_empty() {
                        text.push('\n');
                    }
                    std::fs::write(&f, &text).unwrap();
                    let covered = lines.iter().any(|l| excludes(l, ".worktrees"));
                    let added = add_exclude(&f, ".worktrees").unwrap();
                    assert_eq!(added, !covered, "{text:?}");
                    assert!(!add_exclude(&f, ".worktrees").unwrap(), "{text:?}");
                    let after = std::fs::read_to_string(&f).unwrap();
                    assert!(after.starts_with(&text), "{text:?} -> {after:?}");
                    assert!(after.lines().any(|l| excludes(l, ".worktrees")));
                    if added {
                        assert!(
                            after.ends_with("\n.worktrees/\n") || after == ".worktrees/\n",
                            "{after:?}"
                        );
                        assert_eq!(
                            after.len(),
                            text.len()
                                + ".worktrees/\n".len()
                                + usize::from(!text.is_empty() && !text.ends_with('\n'))
                        );
                    } else {
                        assert_eq!(after, text);
                    }
                    cases += 1;
                }
            }
        }
        assert!(cases > 9000, "{cases}");
    }

    #[test]
    fn a_repository_without_an_info_folder_gets_one() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        let _ = std::fs::remove_dir_all(r.join(".git/info"));
        create(&WorktreeConfig::default(), &r, &opts("w")).unwrap();
        assert_eq!(exclude_lines(&r), vec![".worktrees/".to_string()]);
        assert_eq!(git(&r, &["status", "--porcelain"]).unwrap(), "");
    }

    #[test]
    fn worktree_directory_still_overrides_the_default() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        // Absolute, outside the repository: nothing to exclude.
        let outside = tempfile::tempdir().unwrap();
        let out = outside.path().canonicalize().unwrap();
        let cfg = WorktreeConfig {
            directory: Some(out.display().to_string()),
            start: None,
        };
        let (wt, _) = create(&cfg, &r, &opts("o")).unwrap();
        assert_eq!(wt, out.join("o"));
        assert!(exclude_lines(&r).iter().all(|l| !l.contains("worktree")));
        remove(&cfg, &r, "o", false).unwrap();
        // Relative: inside the repository, excluded where it is.
        let cfg = WorktreeConfig {
            directory: Some("build/trees".into()),
            start: None,
        };
        let (wt, _) = create(&cfg, &r, &opts("rel")).unwrap();
        assert_eq!(wt, r.join("build/trees/rel"));
        assert!(exclude_lines(&r).contains(&"/build/trees/".to_string()));
        assert!(!r.join(".worktrees").exists());
        assert_eq!(git(&r, &["status", "--porcelain"]).unwrap(), "");
        remove(&cfg, &r, "rel", false).unwrap();
    }

    #[test]
    fn remove_refuses_uncommitted_work_and_says_so() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        let cfg = WorktreeConfig::default();
        let (wt, _) = create(&cfg, &r, &opts("dirty")).unwrap();
        std::fs::write(wt.join("new.txt"), "untracked").unwrap();
        let e = remove(&cfg, &r, "dirty", false).unwrap_err().to_string();
        assert!(
            e.contains("uncommitted changes") && e.contains("new.txt") && e.contains("--force"),
            "{e}"
        );
        assert!(wt.exists());
        std::fs::remove_file(wt.join("new.txt")).unwrap();
        std::fs::write(wt.join("pkg/src/a.txt"), "edited").unwrap();
        let e = remove(&cfg, &r, "dirty", false).unwrap_err().to_string();
        assert!(
            e.contains("uncommitted changes") && e.contains("pkg/src/a.txt"),
            "{e}"
        );
        assert_eq!(remove(&cfg, &r, "dirty", true).unwrap(), wt);
        assert!(!wt.exists());
    }

    #[test]
    fn remove_refuses_commits_on_no_remote_branch_or_base() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        let cfg = WorktreeConfig::default();
        // A detached worktree with a commit of its own: removing it loses it.
        let (wt, _) = create(&cfg, &r, &opts("detached")).unwrap();
        commit_in(&wt, "b.txt", "b", "work");
        let e = remove(&cfg, &r, "detached", false).unwrap_err().to_string();
        assert!(
            e.contains("1 commit not on any remote branch or on its base") && e.contains("--force"),
            "{e}"
        );
        assert!(!e.contains("uncommitted"), "{e}");
        commit_in(&wt, "c.txt", "c", "more");
        let e = remove(&cfg, &r, "detached", false).unwrap_err().to_string();
        assert!(e.contains("2 commits"), "{e}");
        remove(&cfg, &r, "detached", true).unwrap();
        assert!(!wt.exists());

        // Pushed to a remote: nothing is lost.
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare"]).unwrap();
        git(
            &r,
            &[
                "remote",
                "add",
                "origin",
                &remote.path().display().to_string(),
            ],
        )
        .unwrap();
        let (wt, _) = create(
            &cfg,
            &r,
            &Create {
                name: Some("pushed".into()),
                branch: Some("feature".into()),
                ..Default::default()
            },
        )
        .unwrap();
        commit_in(&wt, "d.txt", "d", "feature work");
        assert!(remove(&cfg, &r, "pushed", false)
            .unwrap_err()
            .to_string()
            .contains("not on any remote branch"));
        git(&wt, &["push", "-q", "origin", "feature"]).unwrap();
        remove(&cfg, &r, "pushed", false).unwrap();

        // Made from a branch ahead of the main checkout: its base holds
        // those commits, so a worktree with none of its own goes quietly.
        git(&r, &["branch", "ahead"]).unwrap();
        let (tmp, _) = create(
            &cfg,
            &r,
            &Create {
                name: Some("tmp".into()),
                from: Some("ahead".into()),
                ..Default::default()
            },
        )
        .unwrap();
        commit_in(&tmp, "e.txt", "e", "ahead work");
        git(&tmp, &["branch", "-f", "ahead", "HEAD"]).unwrap();
        remove(&cfg, &r, "tmp", true).unwrap();
        let (on_base, _) = create(
            &cfg,
            &r,
            &Create {
                name: Some("on-base".into()),
                from: Some("ahead".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(on_base.join("e.txt").exists());
        remove(&cfg, &r, "on-base", false).unwrap();

        // A worktree git made without CodeSmith has no recorded base: the
        // main checkout's HEAD stands in for it.
        let plain = r.join(".worktrees/plain");
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &plain.display().to_string(),
            ],
        )
        .unwrap();
        remove(&cfg, &r, "plain", false).unwrap();
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &plain.display().to_string(),
            ],
        )
        .unwrap();
        commit_in(&plain, "f.txt", "f", "plain work");
        let e = remove(&cfg, &r, "plain", false).unwrap_err().to_string();
        assert!(e.contains("the main checkout's HEAD"), "{e}");
        remove(&cfg, &r, "plain", true).unwrap();
    }

    #[test]
    fn worktrees_in_the_old_data_folder_are_still_listed_and_removed() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        let old = legacy_dir(&r).join("old-one");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        git(
            &r,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &old.display().to_string(),
            ],
        )
        .unwrap();
        let old = old.canonicalize().unwrap();
        let cfg = WorktreeConfig::default();
        let (new, _) = create(&cfg, &r, &opts("new-one")).unwrap();
        let dirs: Vec<PathBuf> = list(&r)
            .unwrap()
            .into_iter()
            .map(|e| e.directory.canonicalize().unwrap())
            .collect();
        assert_eq!(dirs[0], r);
        assert_eq!(dirs.len(), 3);
        assert!(dirs.contains(&old) && dirs.contains(&new), "{dirs:?}");
        // Not moved: removed where it is, by name or by path.
        assert_eq!(
            remove(&cfg, &r, "old-one", false)
                .unwrap()
                .canonicalize()
                .ok(),
            None
        );
        assert!(!old.exists());
        assert_eq!(
            remove(&cfg, &r, &new.display().to_string(), false).unwrap(),
            new
        );
        let _ = std::fs::remove_dir_all(legacy_dir(&r));
    }

    #[test]
    fn the_watcher_skips_nested_worktrees_through_info_exclude() {
        let d = repo();
        let r = d.path().canonicalize().unwrap();
        let ignore = crate::watch::native::Ignore::new(&r, &[]);
        assert!(!ignore.ignored(Path::new(".worktrees/w/a.txt"), false));
        create(&WorktreeConfig::default(), &r, &opts("w")).unwrap();
        let ignore = crate::watch::native::Ignore::new(&r, &[]);
        assert!(ignore.ignored(Path::new(".worktrees"), true));
        assert!(ignore.ignored(Path::new(".worktrees/w/pkg/src/a.txt"), false));
        assert!(!ignore.ignored(Path::new("pkg/src/a.txt"), false));
    }
}
