//! What the diff viewer shows, as OpenCode's `vcs.diff`, `vcs.base`,
//! `vcs.branch.list` and `session.diff` produce it: the project's changes
//! against a base branch (`All`: the branch's commits and local changes;
//! `Committed`: its commits only), its uncommitted changes against `HEAD`,
//! or the files the session's last turn changed (from the snapshot store
//! that undo uses).
//!
//! Every read is bounded. Git's patch output is read up to a total cap and
//! the files past it keep no patch; untracked files are read only when they
//! are regular text files inside the project, under the size cap the edit
//! previews use, never through a symlink.

use codesmith_core::snapshot::Snapshots;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// Lines of context around each change (OpenCode's `VCS_DIFF_CONTEXT_LINES`).
pub const CONTEXT: u32 = 12;
/// The most patch text one comparison reads (OpenCode's
/// `MAX_TOTAL_PATCH_BYTES`).
pub const MAX_TOTAL: usize = 10_000_000;
/// The most untracked files one comparison lists.
const MAX_UNTRACKED: usize = 2000;
/// The largest untracked file read for its patch.
const UNTRACKED_MAX: usize = codesmith_core::tool::apply_patch::PREVIEW_MAX;
/// The most branches the base picker lists.
const MAX_BRANCHES: usize = 1000;

/// Where the changes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// The branch's commits and local changes, against its base.
    Branch,
    /// The branch's commits only.
    Committed,
    /// Local changes against `HEAD`.
    Working,
    /// The session's last turn.
    Turn,
}

impl Source {
    pub const ALL: [Source; 4] = [
        Source::Branch,
        Source::Committed,
        Source::Working,
        Source::Turn,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Source::Branch => "branch",
            Source::Committed => "committed",
            Source::Working => "working",
            Source::Turn => "turn",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        Source::ALL.into_iter().find(|m| m.key() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Source::Branch => "All",
            Source::Committed => "Committed",
            Source::Working => "Uncommitted",
            Source::Turn => "Last turn",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Source::Branch => "Branch + local changes",
            Source::Committed => "Branch commits only",
            Source::Working => "Local changes only",
            Source::Turn => "Latest session turn",
        }
    }

    /// Whether the comparison needs a base branch.
    pub fn needs_base(self) -> bool {
        matches!(self, Source::Branch | Source::Committed)
    }
}

/// The branch a comparison starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// As shown (`v2`, `release`).
    pub name: String,
    /// As given to git (`refs/heads/v2`).
    pub reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

/// One line of a hunk, with its numbers in the old and new file (0 where
/// it has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchLine {
    pub kind: LineKind,
    pub old: u32,
    pub new: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub lines: Vec<PatchLine>,
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub file: String,
    pub status: Status,
    pub additions: usize,
    pub deletions: usize,
    /// Empty when there is no patch to show (a binary file, one past the
    /// size cap).
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// A file from a unified patch (`--- a/…`, `+++ b/…`, `@@` hunks); the
    /// counts are the patch's own.
    pub fn from_patch(file: &str, status: Status, patch: &str) -> FileDiff {
        let hunks = parse_hunks(patch);
        let count = |k: LineKind| {
            hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.kind == k)
                .count()
        };
        FileDiff {
            file: file.to_string(),
            status,
            additions: count(LineKind::Add),
            deletions: count(LineKind::Del),
            hunks,
        }
    }
}

fn hunk_start(s: &str) -> Option<u32> {
    s.split(',').next()?.parse().ok()
}

/// The hunks of a unified patch. Text before the first `@@` (headers) is
/// skipped, and so are `\ No newline at end of file` markers.
pub fn parse_hunks(patch: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut old, mut new) = (0u32, 0u32);
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@ -") {
            let mut parts = rest.split(' ');
            let o = parts.next().and_then(hunk_start);
            let n = parts
                .next()
                .and_then(|p| p.strip_prefix('+'))
                .and_then(hunk_start);
            if let (Some(o), Some(n)) = (o, n) {
                // A side that starts at 0 is empty; its first line is 1.
                old = o.max(1);
                new = n.max(1);
                hunks.push(Hunk { lines: vec![] });
                continue;
            }
        }
        let Some(h) = hunks.last_mut() else { continue };
        let text = line
            .get(1..)
            .unwrap_or("")
            .trim_end_matches('\r')
            .to_string();
        let kind = match line.as_bytes().first() {
            Some(b'+') => LineKind::Add,
            Some(b'-') => LineKind::Del,
            Some(b' ') | None => LineKind::Context,
            _ => continue,
        };
        let (o, n) = match kind {
            LineKind::Context => {
                old += 1;
                new += 1;
                (old - 1, new - 1)
            }
            LineKind::Add => {
                new += 1;
                (0, new - 1)
            }
            LineKind::Del => {
                old += 1;
                (old - 1, 0)
            }
        };
        h.lines.push(PatchLine {
            kind,
            old: o,
            new: n,
            text,
        });
    }
    hunks.retain(|h| !h.lines.is_empty());
    hunks
}

/// Whether OpenCode previews `file` as an image.
pub fn is_image(file: &str) -> bool {
    let Some((_, ext)) = file.rsplit_once('.') else {
        return false;
    };
    if file.ends_with('/') || ext.contains('/') {
        return false;
    }
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif"
    )
}

// ----- git ------------------------------------------------------------------

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir)
        .env_remove(codesmith_core::SERVER_TOKEN_VAR)
        // Reading must not take the index lock a running turn may need.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"]);
    c
}

/// Run and return stdout, or stderr as the error.
fn run(mut c: Command) -> Result<String, String> {
    let out = c
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ok(mut c: Command) -> Option<String> {
    let out = c.stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// At most `cap` bytes of stdout; whether there was more.
fn run_bounded(mut c: Command, cap: usize) -> Result<(String, bool), String> {
    let mut child = c
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("running git: {e}"))?;
    let mut bytes = Vec::new();
    let out = child.stdout.take().expect("piped");
    let read = out.take(cap as u64 + 1).read_to_end(&mut bytes);
    let more = bytes.len() > cap;
    if more {
        bytes.truncate(cap);
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| format!("running git: {e}"))?;
    read.map_err(|e| format!("reading git: {e}"))?;
    if !more && !status.success() {
        return Err("git diff failed".into());
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), more))
}

fn lines(c: Command) -> Vec<String> {
    ok(c)
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn has_head(dir: &Path) -> bool {
    let mut c = git(dir);
    c.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
    ok(c).is_some()
}

/// The branch checked out, if any.
pub fn current_branch(dir: &Path) -> Option<String> {
    let mut c = git(dir);
    c.args(["symbolic-ref", "--quiet", "--short", "HEAD"]);
    ok(c).filter(|s| !s.is_empty())
}

fn resolve(dir: &Path, reference: &str) -> Option<String> {
    let mut c = git(dir);
    c.args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{reference}^{{commit}}"));
    ok(c).filter(|s| !s.is_empty())
}

fn ancestor(dir: &Path, commit: &str, reference: &str) -> bool {
    if commit.len() < 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let mut c = git(dir);
    c.args(["merge-base", "--is-ancestor", commit, reference]);
    ok(c).is_some()
}

fn primary_remote(dir: &Path) -> Option<String> {
    let mut c = git(dir);
    c.arg("remote");
    let list = lines(c);
    let has = |r: &str| list.iter().any(|l| l == r);
    if has("origin") {
        return Some("origin".into());
    }
    if list.len() != 1 && has("upstream") {
        return Some("upstream".into());
    }
    list.first().cloned()
}

/// The repository's default branch: the primary remote's `HEAD`, else
/// `init.defaultBranch`, `main` or `master` when it exists.
fn default_branch(dir: &Path) -> Option<Base> {
    if let Some(remote) = primary_remote(dir) {
        let mut c = git(dir);
        c.args(["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")]);
        if let Some(head) = ok(c) {
            let reference = head.trim_start_matches("refs/remotes/").to_string();
            if let Some(name) = reference.strip_prefix(&format!("{remote}/")) {
                if !name.is_empty() {
                    return Some(Base {
                        name: name.to_string(),
                        reference,
                    });
                }
            }
        }
    }
    let mut c = git(dir);
    c.args(["for-each-ref", "--format=%(refname:lstrip=2)", "refs/heads"]);
    let list = lines(c);
    let mut c = git(dir);
    c.args(["config", "init.defaultBranch"]);
    let configured = ok(c).unwrap_or_default();
    let found = [configured.as_str(), "main", "master"]
        .into_iter()
        .find(|n| !n.is_empty() && list.iter().any(|l| l == n))
        .map(|n| Base {
            name: n.to_string(),
            reference: n.to_string(),
        });
    found
}

/// A branch a reflog's `Created from` names, as a full ref.
fn named_ref(dir: &Path, input: &str) -> Option<Base> {
    if input == "HEAD"
        || input.ends_with("/HEAD")
        || input.contains(['~', '^', ':', '@', '{', '}', ' ', '\t'])
    {
        return None;
    }
    let mut c = git(dir);
    c.args([
        "rev-parse",
        "--symbolic-full-name",
        "--verify",
        "--quiet",
        "--end-of-options",
        input,
    ]);
    let reference = ok(c)?;
    let valid = reference.starts_with("refs/heads/") || reference.starts_with("refs/remotes/");
    if !valid || reference.ends_with("/HEAD") || resolve(dir, &reference).is_none() {
        return None;
    }
    let name = match reference.strip_prefix("refs/heads/") {
        Some(n) => n.to_string(),
        None => {
            let rest = reference.trim_start_matches("refs/remotes/");
            rest.split_once('/').map_or(rest, |(_, n)| n).to_string()
        }
    };
    Some(Base { name, reference })
}

/// The base a branch is compared with, as OpenCode's `vcs.base` finds it:
/// the branch it was created from (its reflog), or the default branch when
/// that is the one checked out. `Ok(None)` when there is no commit yet;
/// an error when the base is ambiguous (choose one), never a guess.
pub fn base(dir: &Path) -> Result<Option<Base>, String> {
    if !has_head(dir) {
        return Ok(None);
    }
    let current = current_branch(dir).ok_or("Choose a review base")?;
    let mut c = git(dir);
    c.args(["reflog", "show", "--max-count=256", "--format=%H%x00%gs"])
        .arg(format!("refs/heads/{current}"));
    let history: Vec<(String, String)> = lines(c)
        .into_iter()
        .filter_map(|l| {
            let (h, m) = l.split_once('\0')?;
            Some((h.to_string(), m.to_string()))
        })
        .collect();
    let renamed = history
        .iter()
        .any(|(_, m)| m.starts_with("Branch: renamed "));
    let creation = (!renamed)
        .then(|| {
            history
                .iter()
                .find(|(_, m)| m.starts_with("branch: Created from "))
        })
        .flatten();
    if let Some((commit, message)) = creation {
        let origin = &message["branch: Created from ".len()..];
        if let Some(candidate) = named_ref(dir, origin) {
            if candidate.name != current
                && ancestor(dir, commit, "HEAD")
                && ancestor(dir, commit, &candidate.reference)
            {
                return Ok(Some(candidate));
            }
        }
    }
    let root = default_branch(dir).filter(|r| r.name == current);
    let Some(root) = root else {
        return Err("Choose a review base".into());
    };
    let candidate =
        named_ref(dir, &root.reference).ok_or("The default review base is unavailable")?;
    Ok(Some(Base {
        name: root.name,
        reference: candidate.reference,
    }))
}

/// Local and remote branches, most recent commit first.
pub fn branches(dir: &Path) -> Result<Vec<String>, String> {
    let mut c = git(dir);
    c.args([
        "for-each-ref",
        "--sort=refname",
        "--sort=-committerdate",
        "--format=%(refname:lstrip=2)",
        "refs/heads",
        "refs/remotes",
    ]);
    Ok(run(c)?
        .lines()
        .filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
        .take(MAX_BRANCHES)
        .map(String::from)
        .collect())
}

/// Changed paths (status letter, path) between `from` and `to` (the
/// working tree when `None`), with the patches the command prints for
/// them, in the same order.
fn changes(
    mut base: impl FnMut() -> Command,
    range: &[&str],
    scope: &[String],
) -> Result<Vec<FileDiff>, String> {
    let mut c = base();
    c.args([
        "diff",
        "--no-renames",
        "--no-ext-diff",
        "--name-status",
        "-z",
    ])
    .args(range)
    .arg("--")
    .args(scope);
    let listed = run(c)?;
    let mut parts = listed.split('\0').filter(|s| !s.is_empty());
    let mut files = Vec::new();
    while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
        let status = match status.chars().next() {
            Some('A') => Status::Added,
            Some('D') => Status::Deleted,
            _ => Status::Modified,
        };
        files.push((status, path.to_string()));
    }
    let mut c = base();
    c.args([
        "diff",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
    ])
    .arg(format!("-U{CONTEXT}"))
    .args(range)
    .arg("--")
    .args(scope);
    let (text, cut) = run_bounded(c, MAX_TOTAL)?;
    let mut sections: Vec<&str> = Vec::new();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("diff --git ") {
        let after = &rest[start + 1..];
        let end = after
            .find("\ndiff --git ")
            .map_or(rest.len(), |e| start + 1 + e + 1);
        sections.push(&rest[start..end]);
        rest = &rest[end..];
    }
    // The last section may be cut short by the cap: it keeps no patch.
    let complete = sections.len().saturating_sub(usize::from(cut));
    let mut next = 0;
    Ok(files
        .into_iter()
        .map(|(status, path)| {
            let header = |s: &str| {
                s.lines()
                    .next()
                    .is_some_and(|h| h.ends_with(&path) || h.contains(&format!(" b/{path}")))
            };
            let patch = sections[next..complete]
                .iter()
                .position(|s| header(s))
                .map(|i| {
                    next += i + 1;
                    sections[next - 1]
                });
            match patch {
                Some(p) => FileDiff::from_patch(&path, status, p),
                None => FileDiff {
                    file: path,
                    status,
                    additions: 0,
                    deletions: 0,
                    hunks: vec![],
                },
            }
        })
        .collect())
}

/// A new file's patch, read as the edit previews read files: a regular
/// text file inside `root`, under the size cap, never through a symlink.
fn untracked(root: &Path, file: &str) -> FileDiff {
    use codesmith_core::tool::{peek_text, real_path, Peek};
    let path = root.join(file);
    let inside = real_path(&path).starts_with(real_path(root));
    let plain = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
    let text = match (inside && plain).then(|| peek_text(&path, UNTRACKED_MAX)) {
        Some(Peek::Text(t)) => Some(t),
        _ => None,
    };
    let Some(text) = text else {
        return FileDiff {
            file: file.to_string(),
            status: Status::Added,
            additions: 0,
            deletions: 0,
            hunks: vec![],
        };
    };
    let lines: Vec<PatchLine> = text
        .lines()
        .enumerate()
        .map(|(i, l)| PatchLine {
            kind: LineKind::Add,
            old: 0,
            new: i as u32 + 1,
            text: l.trim_end_matches('\r').to_string(),
        })
        .collect();
    FileDiff {
        file: file.to_string(),
        status: Status::Added,
        additions: lines.len(),
        deletions: 0,
        hunks: if lines.is_empty() {
            vec![]
        } else {
            vec![Hunk { lines }]
        },
    }
}

fn with_untracked(root: &Path, scope: &[String], mut files: Vec<FileDiff>) -> Vec<FileDiff> {
    let mut c = git(root);
    c.args(["ls-files", "--others", "--exclude-standard", "-z", "--"])
        .args(scope);
    let listed = ok(c).unwrap_or_default();
    let size = |d: &FileDiff| -> usize {
        d.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| l.text.len() + 1)
            .sum()
    };
    let mut total: usize = files.iter().map(size).sum();
    for file in listed
        .split('\0')
        .filter(|s| !s.is_empty())
        .take(MAX_UNTRACKED)
    {
        let mut d = untracked(root, file);
        let n = size(&d);
        if total + n > MAX_TOTAL {
            d.hunks.clear();
        } else {
            total += n;
        }
        files.push(d);
    }
    files.sort_by(|a, b| a.file.cmp(&b.file));
    files
}

/// The repository's top folder.
fn toplevel(dir: &Path) -> Result<std::path::PathBuf, String> {
    let mut c = git(dir);
    c.args(["rev-parse", "--show-toplevel"]);
    run(c)
        .map(|s| std::path::PathBuf::from(s.trim()))
        .map_err(|e| {
            if e.is_empty() {
                "not a git repository".into()
            } else {
                e
            }
        })
}

/// The pathspec limiting a comparison to `dir` inside the repository at
/// `top`: none at the top itself.
fn scope_of(top: &Path, dir: &Path) -> Vec<String> {
    let here = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let top = top.canonicalize().unwrap_or_else(|_| top.to_path_buf());
    match here.strip_prefix(&top) {
        Ok(rel) if !rel.as_os_str().is_empty() => {
            vec![format!(":(literal){}", rel.display())]
        }
        _ => vec![],
    }
}

/// The project's changes for `source` (not `Turn`) against `base` (the
/// default branch when `None`), as OpenCode's `vcs.diff` makes them: for
/// the files under `dir`, a subfolder of the repository limiting them.
pub fn vcs_diff(dir: &Path, source: Source, base: Option<&str>) -> Result<Vec<FileDiff>, String> {
    // Paths are relative to the repository's top, as OpenCode lists them.
    let top = toplevel(dir)?;
    let scope = scope_of(&top, dir);
    let dir = top.as_path();
    let cmd = || git(dir);
    if source == Source::Working {
        if has_head(dir) {
            return Ok(with_untracked(
                dir,
                &scope,
                changes(cmd, &["HEAD"], &scope)?,
            ));
        }
        return Ok(with_untracked(
            dir,
            &scope,
            empty_tree_changes(dir, &scope)?,
        ));
    }
    if !has_head(dir) {
        if source == Source::Committed {
            return Ok(vec![]);
        }
        return Ok(with_untracked(
            dir,
            &scope,
            empty_tree_changes(dir, &scope)?,
        ));
    }
    let reference = match base {
        Some(b) => Some(b.to_string()),
        None => default_branch(dir).map(|b| b.reference),
    };
    let Some(reference) = reference else {
        return Err("No review base available".into());
    };
    let merge_base = resolve(dir, &reference).and_then(|commit| {
        let mut c = git(dir);
        c.args(["merge-base", &commit, "HEAD"]);
        ok(c)
    });
    let Some(merge_base) = merge_base.filter(|m| !m.is_empty()) else {
        return Err(format!("No merge base available for {reference}"));
    };
    if source == Source::Committed {
        return changes(cmd, &[&merge_base, "HEAD"], &scope);
    }
    Ok(with_untracked(
        dir,
        &scope,
        changes(cmd, &[&merge_base], &scope)?,
    ))
}

/// Staged files of a repository with no commit yet.
fn empty_tree_changes(dir: &Path, scope: &[String]) -> Result<Vec<FileDiff>, String> {
    let mut c = git(dir);
    c.args(["hash-object", "-t", "tree", "/dev/null"]);
    let empty = ok(c).ok_or("git hash-object failed")?;
    changes(|| git(dir), &[&empty], scope)
}

/// What the session's last finished turn changed: its start snapshot
/// against its last end snapshot (turns undone are skipped).
pub fn turn_diff(
    store: &codesmith_core::store::Store,
    session: &str,
    snaps: &Snapshots,
) -> Result<Vec<FileDiff>, String> {
    use codesmith_core::store::{hidden_ranges, Record};
    let entries = store.entries(session).map_err(|e| format!("{e:#}"))?;
    let hidden = hidden_ranges(&entries);
    let visible = |seq: i64| !hidden.iter().any(|(from, to)| seq >= *from && seq < *to);
    let mut range = None;
    let mut end: Option<String> = None;
    for e in entries.iter().rev() {
        if !visible(e.seq) {
            continue;
        }
        match &e.record {
            Record::TurnEnd { tree } if end.is_none() => end = Some(tree.clone()),
            Record::Snapshot { tree } => {
                if let Some(end) = end.take() {
                    range = Some((tree.clone(), end));
                    break;
                }
            }
            _ => {}
        }
    }
    let Some((from, to)) = range else {
        return Ok(vec![]);
    };
    changes(|| snaps.command(), &[&from, &to], &[])
}

#[cfg(test)]
#[path = "diff_source_oc.rs"]
mod oc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_number_both_sides() {
        let h = parse_hunks("--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -0,0 +1 @@\n+x\n\\ No newline at end of file\n");
        assert_eq!(h.len(), 2);
        let k: Vec<(LineKind, u32, u32)> =
            h[0].lines.iter().map(|l| (l.kind, l.old, l.new)).collect();
        assert_eq!(
            k,
            [
                (LineKind::Context, 1, 1),
                (LineKind::Del, 2, 0),
                (LineKind::Add, 0, 2),
                (LineKind::Context, 3, 3)
            ]
        );
        assert_eq!(h[1].lines.len(), 1);
        assert_eq!(h[1].lines[0].new, 1);
    }

    fn sh(dir: &Path, args: &[&str]) {
        let s = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            s.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&s.stderr)
        );
    }

    /// A repository on `main` with a `feature` branch created from it.
    pub(crate) fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        sh(p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(p.join("gone.txt"), "bye\n").unwrap();
        sh(p, &["add", "."]);
        sh(p, &["commit", "-qm", "init"]);
        sh(p, &["checkout", "-qb", "feature", "main"]);
        std::fs::write(p.join("committed.txt"), "c\n").unwrap();
        sh(p, &["add", "."]);
        sh(p, &["commit", "-qm", "feature"]);
        std::fs::write(p.join("a.txt"), "one\nTWO\n").unwrap();
        std::fs::remove_file(p.join("gone.txt")).unwrap();
        std::fs::write(p.join("untracked.txt"), "u\n").unwrap();
        d
    }

    fn names(files: &[FileDiff]) -> Vec<(&str, Status)> {
        files.iter().map(|f| (f.file.as_str(), f.status)).collect()
    }

    #[test]
    fn sources_compare_what_opencode_compares() {
        let d = repo();
        let p = d.path();
        let b = base(p).unwrap().unwrap();
        assert_eq!(b.name, "main");
        assert_eq!(b.reference, "refs/heads/main");
        let all = vcs_diff(p, Source::Branch, Some(&b.reference)).unwrap();
        assert_eq!(
            names(&all),
            [
                ("a.txt", Status::Modified),
                ("committed.txt", Status::Added),
                ("gone.txt", Status::Deleted),
                ("untracked.txt", Status::Added),
            ]
        );
        let a = &all[0];
        assert_eq!((a.additions, a.deletions), (1, 1));
        let committed = vcs_diff(p, Source::Committed, Some(&b.reference)).unwrap();
        assert_eq!(names(&committed), [("committed.txt", Status::Added)]);
        let working = vcs_diff(p, Source::Working, None).unwrap();
        assert_eq!(
            names(&working),
            [
                ("a.txt", Status::Modified),
                ("gone.txt", Status::Deleted),
                ("untracked.txt", Status::Added),
            ]
        );
        assert_eq!(working[2].hunks[0].lines[0].text, "u");
        assert!(branches(p).unwrap().contains(&"main".to_string()));
        assert!(vcs_diff(p, Source::Branch, Some("nope")).is_err());
    }

    // An ambiguous base is an error, not a guess: a detached HEAD.
    #[test]
    fn an_ambiguous_base_is_an_error() {
        let d = repo();
        sh(d.path(), &["checkout", "-q", "--detach"]);
        assert!(base(d.path()).is_err());
    }

    // Untracked files are read as the previews read them: not through a
    // symlink, and not past the size cap.
    #[test]
    fn untracked_reads_are_bounded() {
        let d = repo();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "SECRET\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), d.path().join("link")).unwrap();
        std::fs::write(d.path().join("big.txt"), "x".repeat(UNTRACKED_MAX + 1)).unwrap();
        let working = vcs_diff(d.path(), Source::Working, None).unwrap();
        let link = working.iter().find(|f| f.file == "link").unwrap();
        assert!(link.hunks.is_empty());
        let big = working.iter().find(|f| f.file == "big.txt").unwrap();
        assert!(big.hunks.is_empty());
    }

    // The last turn: its start snapshot against its end, with turns undone
    // skipped, from the snapshot store undo uses.
    #[test]
    fn the_last_turn_is_its_snapshots_difference() {
        use codesmith_core::store::{Info, Record, Store};
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let snaps = Snapshots::new(&root, &d.path().join("snaps")).unwrap();
        let db = Store::open(&d.path().join("s.db")).unwrap();
        db.create(&Info {
            id: "s".into(),
            directory: root.display().to_string(),
            title: None,
            agent: "build".into(),
            model: "m".into(),
            parent_id: None,
            created: 0,
            updated: 0,
            usage: Default::default(),
        })
        .unwrap();
        assert!(turn_diff(&db, "s", &snaps).unwrap().is_empty());
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        let t0 = snaps.capture().unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        let t1 = snaps.capture().unwrap();
        db.append("s", &Record::Snapshot { tree: t0 }).unwrap();
        db.append("s", &Record::TurnEnd { tree: t1.clone() })
            .unwrap();
        let files = turn_diff(&db, "s", &snaps).unwrap();
        assert_eq!(
            names(&files),
            [("a.txt", Status::Modified), ("b.txt", Status::Added)]
        );
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
        // A second turn replaces the first; undone, the first shows again.
        std::fs::remove_file(root.join("b.txt")).unwrap();
        let t2 = snaps.capture().unwrap();
        let from = db.append("s", &Record::Snapshot { tree: t1 }).unwrap();
        db.append("s", &Record::TurnEnd { tree: t2.clone() })
            .unwrap();
        assert_eq!(
            names(&turn_diff(&db, "s", &snaps).unwrap()),
            [("b.txt", Status::Deleted)]
        );
        db.append(
            "s",
            &Record::Revert {
                from_seq: from,
                after: t2,
            },
        )
        .unwrap();
        assert_eq!(turn_diff(&db, "s", &snaps).unwrap().len(), 2);
    }

    // `tui/test/cli/tui/diff-viewer-image`.
    #[test]
    fn oc_recognizes_only_supported_image_file_extensions() {
        for ext in ["png", "jpg", "jpeg", "webp", "gif", "PNG", "JPEG"] {
            assert!(is_image(&format!("assets/preview.{ext}")), "{ext}");
        }
        for f in [
            "assets/preview.svg",
            "preview.avif",
            "image.png.ts",
            "png",
            "image.png/file",
        ] {
            assert!(!is_image(f), "{f}");
        }
    }
}
