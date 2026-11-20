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
        let home = dirs::home_dir();
        if root.parent().is_none() || Some(root) == home.as_deref() {
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

    fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .env_remove(crate::SERVER_TOKEN_VAR)
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.quotepath=false",
                "-c",
                "core.safecrlf=false",
            ])
            .args(args)
            .current_dir(&self.work_tree)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
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
        let out = Command::new("git")
            .env_remove(crate::SERVER_TOKEN_VAR)
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

    /// Record the working tree; returns its tree hash.
    pub fn capture(&self) -> Result<String> {
        self.ensure()?;
        self.git(&["add", "--all", "--", "."])?;
        Ok(self.git(&["write-tree"])?.trim().to_string())
    }

    /// Paths that differ between two trees.
    pub fn changes(&self, from: &str, to: &str) -> Result<Vec<Change>> {
        let out = self.git(&["diff", "--no-renames", "--name-status", "-z", from, to])?;
        let mut parts = out.split('\0').filter(|s| !s.is_empty());
        let mut changes = Vec::new();
        while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
            changes.push(Change {
                status: status.chars().next().unwrap_or('M'),
                path: path.to_string(),
            });
        }
        Ok(changes)
    }

    /// A unified diff between two trees.
    pub fn diff(&self, from: &str, to: &str) -> Result<String> {
        self.git(&["diff", "--no-renames", "--no-color", from, to])
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
        for c in &added {
            let p = self.work_tree.join(&c.path);
            if p.is_file() || p.is_symlink() {
                std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
            }
            remove_empty_parents(&p, &self.work_tree);
        }
        for chunk in rest.chunks(200) {
            let mut args = vec!["checkout", tree, "--"];
            let paths: Vec<String> = chunk
                .iter()
                .map(|c| format!(":(literal){}", c.path))
                .collect();
            args.extend(paths.iter().map(String::as_str));
            self.git(&args)?;
        }
        Ok(changes)
    }
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
        Command::new("git")
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
        Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(&root)
            .output()
            .unwrap();
        std::fs::write(root.join("f"), "1").unwrap();
        s.capture().unwrap();
        let status = Command::new("git")
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
        assert_eq!(stable_hash("/a"), stable_hash("/a"));
        assert_ne!(stable_hash("/a"), stable_hash("/b"));
    }
}
