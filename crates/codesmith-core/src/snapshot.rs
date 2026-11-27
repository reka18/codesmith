//! File snapshots for undo/redo, ported from OpenCode's design: a private git
//! repository per project (in codesmith's data dir, never the project's own
//! `.git`) records the working tree as git trees. Capturing is `add -A` +
//! `write-tree`; restoring checks files out of a tree and removes files that
//! did not exist in it. `.gitignore` is respected, so build output is skipped.
//!
//! Undo restores only the paths a turn changed (between its start and end
//! snapshots), as OpenCode's revert restores its steps' files, so edits made
//! after the turn survive. A turn with no end snapshot (stopped mid-way, or
//! recorded by an older CodeSmith) restores every change since it began.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Snapshots {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

/// One changed path between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `A` added, `M` modified, `D` deleted, `T` type changed.
    pub status: char,
    pub path: String,
}

fn stable_hash(s: &str) -> String {
    // FNV-1a: stable across runs and Rust versions (unlike DefaultHasher).
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

impl Snapshots {
    /// Snapshots for a project root, stored under `base`. `None` for roots too
    /// broad to snapshot (the home directory or `/`).
    pub fn new(root: &Path, base: &Path) -> Option<Snapshots> {
        // `$HOME` and the account's home: a bridge's `HOME` doesn't make
        // the real home folder snapshottable.
        if crate::folder::is_broad(root) {
            return None;
        }
        Some(Snapshots {
            git_dir: base.join(stable_hash(&root.display().to_string())),
            work_tree: root.to_path_buf(),
        })
    }

    pub fn for_project(root: &Path) -> Option<Snapshots> {
        Self::new(root, &crate::paths::data_dir().join("snapshot"))
    }

    /// A `git` command on the snapshot repository, set up as every
    /// snapshot command is (hardened by [`crate::git::command_in`], its
    /// own git dir and index, the project as the work tree, no system
    /// config). The diff viewer runs its own diff of two trees with it,
    /// reading the output in bounded pieces.
    pub fn command(&self) -> Command {
        let mut c = crate::git::command_in(&self.work_tree, &self.git_dir, &self.work_tree);
        c.args([
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.quotepath=false",
            "-c",
            "core.safecrlf=false",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1");
        c
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        self.git_with(args, None)
    }

    /// `git` with its index at `index` (the repository's own when `None`).
    fn git_with(&self, args: &[&str], index: Option<&Path>) -> Result<String> {
        let mut c = self.command();
        if let Some(i) = index {
            c.env("GIT_INDEX_FILE", i);
        }
        let out = c
            .args(args)
            .output()
            .context("running git (is it installed?)")?;
        if !out.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn ensure(&self) -> Result<()> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.git_dir)?;
        let parent = self.git_dir.parent().unwrap_or(&self.git_dir);
        let out = crate::git::command(parent)
            .arg("init")
            .arg("--quiet")
            .arg("--bare")
            .arg(&self.git_dir)
            .output()
            .context("running git init")?;
        if !out.status.success() {
            return Err(anyhow!(
                "git init failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        self.git(&["config", "gc.auto", "0"])?;
        self.git(&["config", "core.bare", "false"])?;
        // Borrow the project's own objects when it is a git repo, so most
        // files are already stored.
        let objects = self.work_tree.join(".git/objects");
        if objects.is_dir() {
            let info = self.git_dir.join("objects/info");
            std::fs::create_dir_all(&info)?;
            std::fs::write(info.join("alternates"), format!("{}\n", objects.display()))?;
        }
        Ok(())
    }

    /// A fresh index file name of this process, next to the repository's.
    fn temporary_index(&self) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.git_dir
            .join(format!("index.codesmith-{}-{n}", std::process::id()))
    }

    /// Record the working tree; returns its tree hash. The index is
    /// written as a copy, then renamed over the repository's (OpenCode's
    /// temporary index): processes never contend on `index.lock`, a stale
    /// lock or a writer killed mid-write cannot block later captures, and a
    /// corrupt index is rebuilt from the files.
    pub fn capture(&self) -> Result<String> {
        self.ensure()?;
        let index = self.git_dir.join("index");
        let tmp = self.temporary_index();
        let copied = copy_index(&index, &tmp);
        let mut tree = self.capture_into(&tmp);
        if tree.is_err() && copied {
            // An unreadable index: start over from the files.
            let _ = std::fs::remove_file(&tmp);
            tree = self.capture_into(&tmp);
        }
        match tree {
            Ok(t) => {
                if std::fs::rename(&tmp, &index).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
                Ok(t)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Folders inside the work tree that hold other checkouts: the
    /// `.worktrees` folder and every linked worktree of the project's
    /// repository under the work tree. Snapshots leave them out (a nested
    /// worktree is its own project, with its own snapshots), so undo never
    /// touches their files.
    fn nested(&self) -> Vec<String> {
        let root = self
            .work_tree
            .canonicalize()
            .unwrap_or_else(|_| self.work_tree.clone());
        let mut out: Vec<String> = Vec::new();
        if root.join(crate::worktree::NESTED).is_dir() {
            out.push(crate::worktree::NESTED.to_string());
        }
        let listed = crate::git::command(&root)
            .args(["worktree", "list", "--porcelain"])
            .output();
        if let Some(o) = listed.ok().filter(|o| o.status.success()) {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let Some(p) = line.strip_prefix("worktree ") else {
                    continue;
                };
                let p = Path::new(p);
                let p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                let Ok(rel) = p.strip_prefix(&root) else {
                    continue;
                };
                let rel = rel.to_string_lossy().replace('\\', "/");
                let covered = out
                    .iter()
                    .any(|o| rel == *o || rel.starts_with(&format!("{o}/")));
                if !rel.is_empty() && !covered {
                    out.push(rel);
                }
            }
        }
        out
    }

    fn capture_into(&self, index: &Path) -> Result<String> {
        let nested = self.nested();
        let mut add: Vec<String> = ["add", "--all", "--", "."].map(String::from).to_vec();
        if !nested.is_empty() {
            // Out of the index too: an older capture may hold one.
            let mut rm: Vec<String> = ["rm", "--cached", "-r", "-q", "--ignore-unmatch", "--"]
                .map(String::from)
                .to_vec();
            rm.extend(nested.iter().map(|n| format!(":(literal){n}")));
            let rm: Vec<&str> = rm.iter().map(String::as_str).collect();
            self.git_with(&rm, Some(index))?;
            add.extend(nested.iter().map(|n| format!(":(exclude,literal){n}")));
        }
        let add: Vec<&str> = add.iter().map(String::as_str).collect();
        self.git_with(&add, Some(index))?;
        Ok(self
            .git_with(&["write-tree"], Some(index))?
            .trim()
            .to_string())
    }

    /// Paths that differ between two trees.
    ///
    /// Another checkout inside the work tree (a gitlink an older capture
    /// recorded, or a path under a nested worktree) is never a change:
    /// undo leaves nested worktrees alone.
    pub fn changes(&self, from: &str, to: &str) -> Result<Vec<Change>> {
        let out = self.git(&[
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "--raw",
            "-z",
            from,
            to,
        ])?;
        let nested = self.nested();
        let mut parts = out.split('\0').filter(|s| !s.is_empty());
        let mut changes = Vec::new();
        // `:old-mode new-mode old new status`, then the path.
        while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
            let fields: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            let gitlink = fields.iter().take(2).any(|m| *m == "160000");
            let inside = nested
                .iter()
                .any(|n| path == n || path.starts_with(&format!("{n}/")));
            if gitlink || inside {
                continue;
            }
            changes.push(Change {
                status: fields.get(4).and_then(|s| s.chars().next()).unwrap_or('M'),
                path: path.to_string(),
            });
        }
        Ok(changes)
    }

    /// A unified diff between two trees.
    pub fn diff(&self, from: &str, to: &str) -> Result<String> {
        let [ext, textconv] = crate::git::DIFF_SAFETY;
        self.git(&[
            "diff",
            ext,
            textconv,
            "--no-renames",
            "--no-color",
            from,
            to,
        ])
    }

    /// Make the working tree match `tree` for every path changed since, and
    /// return those changes (relative to `tree`).
    pub fn restore(&self, tree: &str) -> Result<Vec<Change>> {
        self.restore_only(tree, None)
    }

    /// Like `restore`, but only for `paths` when given: files changed since
    /// for other reasons (the user's own edits) are left alone.
    pub fn restore_only(&self, tree: &str, paths: Option<&[String]>) -> Result<Vec<Change>> {
        let now = self.capture()?;
        let mut changes = self.changes(tree, &now)?;
        if let Some(paths) = paths {
            changes.retain(|c| paths.contains(&c.path));
        }
        let (added, rest): (Vec<&Change>, Vec<&Change>) =
            changes.iter().partition(|c| c.status == 'A');
        // One path that cannot be restored does not stop the others; the
        // first failure is reported once they are done.
        let mut failed: Option<anyhow::Error> = None;
        for c in &added {
            let p = self.work_tree.join(&c.path);
            if p.is_file() || p.is_symlink() {
                if let Err(e) = std::fs::remove_file(&p) {
                    failed.get_or_insert(anyhow!(e).context(format!("removing {}", p.display())));
                    continue;
                }
            }
            remove_empty_parents(&p, &self.work_tree);
        }
        // Checked out through a temporary index, so a stale lock on the
        // repository's own cannot block it.
        let index = self.temporary_index();
        for chunk in rest.chunks(200) {
            let mut args = vec!["checkout", tree, "--"];
            let paths: Vec<String> = chunk
                .iter()
                .map(|c| format!(":(literal){}", c.path))
                .collect();
            args.extend(paths.iter().map(String::as_str));
            if let Err(e) = self.git_with(&args, Some(&index)) {
                failed.get_or_insert(e);
            }
        }
        let _ = std::fs::remove_file(&index);
        match failed {
            Some(e) => Err(e),
            None => Ok(changes),
        }
    }
}

/// Copy an index, keeping its modification time: Git compares it with its
/// entries' to tell racily clean files.
fn copy_index(from: &Path, to: &Path) -> bool {
    if std::fs::copy(from, to).is_err() {
        return false;
    }
    if let Ok(t) = std::fs::metadata(from).and_then(|m| m.modified()) {
        let _ = std::fs::File::options()
            .write(true)
            .open(to)
            .and_then(|f| f.set_modified(t));
    }
    true
}

fn remove_empty_parents(file: &Path, stop: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == stop || !d.starts_with(stop) || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, Snapshots) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let snaps = Snapshots::new(&root, &d.path().join("snaps")).unwrap();
        (d, root, snaps)
    }

    fn git_available() -> bool {
        crate::git::fixture()
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn capture_change_restore() {
        if !git_available() {
            return;
        }
        let (_d, root, s) = setup();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("keep.txt"), "same\n").unwrap();
        std::fs::create_dir_all(root.join("gone")).unwrap();
        std::fs::write(root.join("gone/b.txt"), "b\n").unwrap();
        let before = s.capture().unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::remove_file(root.join("gone/b.txt")).unwrap();
        std::fs::create_dir_all(root.join("new/dir")).unwrap();
        std::fs::write(root.join("new/dir/c.txt"), "c\n").unwrap();
        let after = s.capture().unwrap();
        let mut ch = s.changes(&before, &after).unwrap();
        ch.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            ch,
            vec![
                Change {
                    status: 'M',
                    path: "a.txt".into()
                },
                Change {
                    status: 'D',
                    path: "gone/b.txt".into()
                },
                Change {
                    status: 'A',
                    path: "new/dir/c.txt".into()
                },
            ]
        );
        assert!(s.diff(&before, &after).unwrap().contains("-one\n+two"));
        s.restore(&before).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("gone/b.txt")).unwrap(),
            "b\n"
        );
        assert!(
            !root.join("new").exists(),
            "added files and their empty folders are removed"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("keep.txt")).unwrap(),
            "same\n"
        );
        // and forward again (redo)
        s.restore(&after).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "two\n"
        );
        assert!(root.join("new/dir/c.txt").exists());
        assert!(!root.join("gone/b.txt").exists());
    }

    #[test]
    fn gitignored_files_are_not_touched() {
        if !git_available() {
            return;
        }
        let (_d, root, s) = setup();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let before = s.capture().unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/out.bin"), "built").unwrap();
        s.restore(&before).unwrap();
        assert!(root.join("target/out.bin").exists());
    }

    #[test]
    fn unchanged_tree_is_stable() {
        if !git_available() {
            return;
        }
        let (_d, root, s) = setup();
        std::fs::write(root.join("x"), "x").unwrap();
        assert_eq!(s.capture().unwrap(), s.capture().unwrap());
    }

    #[test]
    fn project_git_repo_is_never_touched() {
        if !git_available() {
            return;
        }
        let (_d, root, s) = setup();
        crate::git::fixture()
            .arg("init")
            .arg("--quiet")
            .arg(&root)
            .output()
            .unwrap();
        std::fs::write(root.join("f"), "1").unwrap();
        s.capture().unwrap();
        let status = crate::git::fixture()
            .arg("-C")
            .arg(&root)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&status.stdout).trim(),
            "?? f",
            "the project's index is untouched"
        );
    }

    #[test]
    fn broad_roots_are_skipped() {
        let base = Path::new("/tmp/x");
        assert!(Snapshots::new(Path::new("/"), base).is_none());
        if let Some(h) = dirs::home_dir() {
            assert!(Snapshots::new(&h, base).is_none());
        }
        // The account's home, whatever `$HOME` says (a bridge sets its own).
        if let Some(h) = crate::paths::account_home() {
            assert!(Snapshots::new(&h, base).is_none());
        }
        assert_eq!(stable_hash("/a"), stable_hash("/a"));
        assert_ne!(stable_hash("/a"), stable_hash("/b"));
    }

    fn fixture_git(dir: &Path, args: &[&str]) {
        let out = crate::git::fixture()
            .arg("-C")
            .arg(dir)
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
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A project that is a repository with a nested worktree at
    /// `.worktrees/w` (made as CodeSmith makes it) and one in a folder of
    /// another name.
    fn with_nested() -> (tempfile::TempDir, PathBuf, Snapshots, PathBuf, PathBuf) {
        let (d, root, s) = setup();
        fixture_git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "main\n").unwrap();
        fixture_git(&root, &["add", "a.txt"]);
        fixture_git(&root, &["commit", "-q", "-m", "a"]);
        let (wt, _) = crate::worktree::create(
            &crate::worktree::WorktreeConfig::default(),
            &root,
            &crate::worktree::Create {
                name: Some("w".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let other = root.join("trees/other");
        fixture_git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &other.display().to_string(),
            ],
        );
        (d, root, s, wt, other)
    }

    #[test]
    fn nested_worktrees_are_never_captured_or_restored() {
        if !git_available() {
            return;
        }
        let (_d, root, s, wt, other) = with_nested();
        assert_eq!(
            s.nested(),
            vec![".worktrees".to_string(), "trees/other".to_string()]
        );
        let before = s.capture().unwrap();
        let listed = s.git(&["ls-tree", "-r", "--name-only", &before]).unwrap();
        assert_eq!(
            listed.trim(),
            "a.txt",
            "only the main checkout's files: {listed}"
        );
        // A turn changes files in the main checkout and in both worktrees,
        // and commits in one (which moves what a gitlink would record).
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        std::fs::write(wt.join("a.txt"), "worktree work\n").unwrap();
        std::fs::write(wt.join("new.txt"), "new in worktree\n").unwrap();
        std::fs::write(other.join("a.txt"), "other work\n").unwrap();
        fixture_git(&wt, &["commit", "-q", "-am", "wt"]);
        let after = s.capture().unwrap();
        let ch = s.changes(&before, &after).unwrap();
        assert_eq!(
            ch,
            vec![Change {
                status: 'M',
                path: "a.txt".into()
            }]
        );
        // Undo restores the main checkout's file and nothing nested.
        let restored = s.restore(&before).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "main\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).unwrap(),
            "worktree work\n"
        );
        assert!(wt.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(other.join("a.txt")).unwrap(),
            "other work\n"
        );
        // Undoing everything since the worktree existed leaves it alone too.
        s.restore_only(
            &before,
            Some(&[".worktrees/w".into(), ".worktrees/w/new.txt".into()]),
        )
        .unwrap();
        assert!(wt.join("new.txt").exists());
    }

    #[test]
    fn a_gitlink_an_older_capture_recorded_is_dropped_and_never_restored() {
        if !git_available() {
            return;
        }
        let (_d, root, s, wt, _other) = with_nested();
        // What captures did before: `add --all` records each nested
        // worktree as a gitlink.
        s.ensure().unwrap();
        let mut c = s.command();
        c.args(["add", "--all", "--", "."]);
        assert!(c.output().unwrap().status.success());
        let old = s.git(&["write-tree"]).unwrap().trim().to_string();
        let tree = s.git(&["ls-tree", "-r", &old]).unwrap();
        assert!(tree.contains("160000"), "{tree}");
        fixture_git(
            &wt,
            &["commit", "-q", "--allow-empty", "-m", "moves the gitlink"],
        );
        let now = s.capture().unwrap();
        assert!(!s.git(&["ls-tree", "-r", &now]).unwrap().contains("160000"));
        assert!(s.changes(&old, &now).unwrap().is_empty());
        s.restore(&old).unwrap();
        let head = crate::git::fixture()
            .arg("-C")
            .arg(&wt)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "moves the gitlink"
        );
        let _ = root;
    }

    #[test]
    fn a_session_in_a_nested_worktree_snapshots_that_worktree() {
        if !git_available() {
            return;
        }
        let (d, _root, _s, wt, _other) = with_nested();
        let s = Snapshots::new(&wt, &d.path().join("wt-snaps")).unwrap();
        assert!(s.nested().is_empty(), "the main checkout is not inside it");
        let before = s.capture().unwrap();
        std::fs::write(wt.join("a.txt"), "edited\n").unwrap();
        s.restore(&before).unwrap();
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "main\n");
    }
}
