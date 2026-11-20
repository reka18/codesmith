//! The project's version control (after OpenCode's `project.ts` and
//! `plugin/vcs/hg.ts`): the nearest `.git` or `.hg` marker above a
//! directory names the project, Git winning where a folder has both, and a
//! Mercurial project's working copy and named branch can be reviewed.
//!
//! Every `hg` command runs with `HGPLAIN=1`, no input and the server token
//! removed; a failing command reads as no output.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The version control of a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Git,
    Hg,
}

/// A resolved project: its directory, version control and id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub directory: PathBuf,
    pub vcs: Option<Kind>,
    /// Git: the first commit (see [`crate::worktree::project_id`]);
    /// Mercurial: the first root changeset; otherwise, or when the
    /// metadata cannot be read, [`GLOBAL`].
    pub id: String,
}

/// The id of a project whose identity cannot be read (OpenCode's
/// `Project.ID.global`).
pub const GLOBAL: &str = "global";

/// The nearest ancestor of `dir` (inclusive) with a `.git` (directory or
/// file) or a `.hg` directory, and which. Git wins in a folder with both.
pub fn marker(dir: &Path) -> Option<(PathBuf, Kind)> {
    dir.ancestors().find_map(|d| {
        if d.join(".git").exists() {
            Some((d.to_path_buf(), Kind::Git))
        } else if d.join(".hg").is_dir() {
            Some((d.to_path_buf(), Kind::Hg))
        } else {
            None
        }
    })
}

/// [`marker`], when the repository it found is one this user controls
/// (see [`crate::tool::trusted_repository`]): a `.git` or `.hg` another
/// user planted in a shared ancestor such as `/tmp` does not make that
/// ancestor the project.
pub fn trusted_marker(dir: &Path) -> Option<(PathBuf, Kind)> {
    marker(dir).filter(|(root, _)| crate::tool::trusted_repository(root))
}

/// The project `dir` belongs to: its trusted repository, else `dir`
/// itself.
pub fn resolve(dir: &Path) -> Project {
    match trusted_marker(dir) {
        Some((directory, Kind::Git)) => Project {
            id: crate::worktree::project_id(&directory),
            directory,
            vcs: Some(Kind::Git),
        },
        Some((directory, Kind::Hg)) => Project {
            id: hg_root(&directory).unwrap_or_else(|| GLOBAL.into()),
            directory,
            vcs: Some(Kind::Hg),
        },
        None => Project {
            directory: dir.to_path_buf(),
            vcs: None,
            id: GLOBAL.into(),
        },
    }
}

/// Run `hg` in `root`: its output, or `None` when it fails or is missing.
fn hg(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("hg")
        .args(args)
        .current_dir(root)
        .env("HGPLAIN", "1")
        .env_remove(crate::SERVER_TOKEN_VAR)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The first of the repository's root changesets.
fn hg_root(root: &Path) -> Option<String> {
    let out = hg(root, &["log", "-r", "roots(all())", "-T", "{node}\n"])?;
    let mut nodes: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    nodes.sort();
    nodes.first().map(|n| n.to_string())
}

/// What a review diffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The working copy against its parent.
    Working,
    /// The named branch against where it left `default`.
    Branch,
    /// The last commit.
    Committed,
}

/// A changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub file: String,
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
    /// `added`, `deleted` or `modified`.
    pub status: &'static str,
}

fn kind(code: &str) -> &'static str {
    match code {
        "A" | "?" => "added",
        "R" | "!" => "deleted",
        _ => "modified",
    }
}

/// A Mercurial working copy's named branch.
pub fn hg_branch(root: &Path) -> Option<String> {
    hg(root, &["branch"])
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}

/// The changed files of a Mercurial project in `mode`, sorted by path.
/// Committed reviews and explicit bases are refused before `hg` runs.
pub fn hg_diff(root: &Path, mode: Mode, base: Option<&str>) -> Result<Vec<FileDiff>> {
    if mode == Mode::Committed || base.is_some() {
        return Err(anyhow!(
            "The Mercurial provider does not support committed reviews or explicit bases"
        ));
    }
    if mode == Mode::Working {
        return Ok(diff_against(root, None));
    }
    if hg_branch(root).filter(|b| b != "default").is_none() {
        return Ok(vec![]);
    }
    let Some(ancestor) = hg(root, &["log", "-r", "ancestor(., default)", "-T", "{node}"])
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
    else {
        return Ok(vec![]);
    };
    Ok(diff_against(root, Some(&ancestor)))
}

/// The working copy's changed files: `+`/`-` counts with zero context.
pub fn hg_status(root: &Path) -> Vec<FileDiff> {
    diff_against(root, None)
}

fn diff_against(root: &Path, rev: Option<&str>) -> Vec<FileDiff> {
    let mut args = vec!["status", "-0"];
    if let Some(r) = rev {
        args.extend(["--rev", r]);
    }
    args.push(".");
    let Some(status) = hg(root, &args) else {
        return vec![];
    };
    let mut items: Vec<(String, String)> = status
        .split('\0')
        .filter(|e| e.len() > 2)
        .map(|e| (e[..1].to_string(), e[2..].to_string()))
        .collect();
    items.sort_by(|a, b| a.1.cmp(&b.1));
    let mut dargs = vec!["diff", "--git"];
    if let Some(r) = rev {
        dargs.extend(["-r", r]);
    }
    dargs.push(".");
    let batch = hg(root, &dargs).unwrap_or_default();
    let chunks = chunks_by_file(&batch);
    items
        .into_iter()
        .map(|(code, file)| {
            let patch = match code.as_str() {
                // Untracked: a new file from the working copy.
                "?" => std::fs::read_to_string(root.join(&file))
                    .map(|t| synthesize(&file, "", &t))
                    .unwrap_or_default(),
                // Missing: deleted from the parent's copy.
                "!" => hg(root, &["cat", "-r", rev.unwrap_or("."), "--", &file])
                    .map(|t| synthesize(&file, &t, ""))
                    .unwrap_or_default(),
                _ => chunks.get(&file).cloned().unwrap_or_default(),
            };
            let (additions, deletions) = count(&patch);
            FileDiff {
                status: kind(&code),
                file,
                patch,
                additions,
                deletions,
            }
        })
        .collect()
}

/// A `diff --git` output split by file.
fn chunks_by_file(diff: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let mut current: Option<(String, String)> = None;
    for line in diff.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some((f, p)) = current.take() {
                out.insert(f, p);
            }
            let file = rest
                .trim_end()
                .rsplit_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_default();
            current = Some((file, String::new()));
        }
        if let Some((_, p)) = current.as_mut() {
            p.push_str(line);
        }
    }
    if let Some((f, p)) = current {
        out.insert(f, p);
    }
    out
}

/// A Git-style patch from `before` to `after`.
pub fn synthesize(file: &str, before: &str, after: &str) -> String {
    let lines = |t: &str| -> Vec<String> { t.lines().map(str::to_string).collect() };
    let (old, new) = (lines(before), lines(after));
    let mut p = format!("diff --git a/{file} b/{file}\n");
    if before.is_empty() {
        p.push_str("new file mode 100644\n--- /dev/null\n");
    } else {
        p.push_str(&format!("--- a/{file}\n"));
    }
    if after.is_empty() {
        p.push_str("deleted file mode 100644\n+++ /dev/null\n");
    } else {
        p.push_str(&format!("+++ b/{file}\n"));
    }
    let range = |n: usize| {
        if n == 0 {
            "0,0".to_string()
        } else {
            format!("1,{n}")
        }
    };
    p.push_str(&format!(
        "@@ -{} +{} @@\n",
        range(old.len()),
        range(new.len())
    ));
    for l in &old {
        p.push_str(&format!("-{l}\n"));
    }
    for l in &new {
        p.push_str(&format!("+{l}\n"));
    }
    p
}

fn count(patch: &str) -> (usize, usize) {
    let mut a = 0;
    let mut d = 0;
    for l in patch.lines() {
        if l.starts_with("+++") || l.starts_with("---") {
            continue;
        }
        if l.starts_with('+') {
            a += 1;
        } else if l.starts_with('-') {
            d += 1;
        }
    }
    (a, d)
}
