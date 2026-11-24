//! Whether a folder is a good place to start a session.
//!
//! A session's project is the repository above the folder it starts in
//! ([`crate::config::project_root`]), so starting in a subfolder of a
//! repository is fine. Outside any repository the project is the folder
//! itself: the model sees everything under it, and undo snapshots all of
//! it. That's worth a warning for a loose folder (`~/Downloads`), and a
//! stronger one for the home folder or `/`, which are never snapshotted
//! because snapshotting them takes far too long.
//!
//! The home folder is both `$HOME` and the account's home from the user
//! database: a tailbus bridge sets `HOME` to its own folder, and the
//! account's home must still count as home.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where a session would start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// In a repository (at its top or in a subfolder).
    Repo,
    /// Outside any repository: this folder is the project.
    Loose(PathBuf),
    /// The home folder or `/` (or a repository there, such as dotfiles
    /// in `~/.git`).
    Broad(PathBuf),
}

/// The home folders: `$HOME` and the account's own, each as given and
/// through its real path.
pub fn homes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for h in dirs::home_dir()
        .into_iter()
        .chain(crate::paths::account_home())
    {
        if let Ok(real) = h.canonicalize() {
            if !out.contains(&real) {
                out.push(real);
            }
        }
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

/// Whether `dir` is `/` or a home folder: too broad to be a project.
pub fn is_broad(dir: &Path) -> bool {
    is_broad_in(dir, &homes())
}

fn is_broad_in(dir: &Path, homes: &[PathBuf]) -> bool {
    let real = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    [dir, real.as_path()]
        .iter()
        .any(|d| d.parent().is_none() || homes.iter().any(|h| h == d))
}

/// Where a session started in `cwd` would be.
pub fn place(cwd: &Path) -> Place {
    place_in(cwd, &homes())
}

/// [`place`] with the home folders given.
pub fn place_in(cwd: &Path, homes: &[PathBuf]) -> Place {
    let root = crate::config::project_root(cwd);
    let real = root.canonicalize().unwrap_or(root);
    if is_broad_in(&real, homes) {
        Place::Broad(real)
    } else if crate::vcs::trusted_marker(cwd).is_some() {
        Place::Repo
    } else {
        Place::Loose(real)
    }
}

/// What to tell someone about to start in `place` (nothing for a repo).
pub fn warning(place: &Place) -> Option<String> {
    match place {
        Place::Repo => None,
        Place::Loose(d) => Some(format!(
            "{} isn't in a git repository. CodeSmith would see everything under it, \
             and undo would snapshot all of it.",
            d.display()
        )),
        Place::Broad(d) if d.parent().is_none() => Some(
            "This is the root folder, /. CodeSmith would see your whole machine, \
             and undo can't snapshot it."
                .to_string(),
        ),
        Place::Broad(d) => Some(format!(
            "{} is your home folder. CodeSmith would see everything in it, \
             and undo can't snapshot it.",
            d.display()
        )),
    }
}

/// Loose folders someone said yes to, so they aren't asked again. The
/// home folder and `/` are never kept here.
pub struct Accepted {
    path: PathBuf,
}

impl Accepted {
    pub fn new(path: PathBuf) -> Accepted {
        Accepted { path }
    }

    /// `folders.json` in the state folder.
    pub fn default_store() -> Accepted {
        Accepted::new(crate::paths::state_dir().join("folders.json"))
    }

    fn load(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("accepted").and_then(|a| a.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    pub fn has(&self, dir: &Path) -> bool {
        let key = dir.display().to_string();
        self.load().contains(&key)
    }

    /// Remember a yes for `dir`. A broad folder is refused.
    pub fn add(&self, dir: &Path) -> Result<()> {
        if is_broad(dir) {
            anyhow::bail!("the home folder and / are asked about every time");
        }
        let mut all = self.load();
        let key = dir.display().to_string();
        if all.contains(&key) {
            return Ok(());
        }
        all.push(key);
        let body = serde_json::to_string_pretty(&serde_json::json!({ "accepted": all }))?;
        let parent = self.path.parent().context("no folder for folders.json")?;
        std::fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".folders.json.{}", std::process::id()));
        {
            use std::io::Write;
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let mut f = o.open(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real(p: &Path) -> PathBuf {
        p.canonicalize().unwrap()
    }

    #[test]
    fn a_repo_and_its_subfolders_are_fine() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("repo/.git")).unwrap();
        std::fs::create_dir_all(d.path().join("repo/src/deep")).unwrap();
        assert_eq!(place_in(&d.path().join("repo"), &[]), Place::Repo);
        assert_eq!(place_in(&d.path().join("repo/src/deep"), &[]), Place::Repo);
    }

    #[test]
    fn a_mercurial_repo_counts() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("repo/.hg")).unwrap();
        std::fs::create_dir_all(d.path().join("repo/a")).unwrap();
        assert_eq!(place_in(&d.path().join("repo/a"), &[]), Place::Repo);
    }

    #[test]
    fn a_loose_folder_is_loose() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("loose/sub")).unwrap();
        assert_eq!(
            place_in(&d.path().join("loose/sub"), &[]),
            Place::Loose(real(&d.path().join("loose/sub")))
        );
        assert!(warning(&place_in(&d.path().join("loose"), &[]))
            .unwrap()
            .contains("isn't in a git repository"));
    }

    #[test]
    fn root_is_broad() {
        assert_eq!(place_in(Path::new("/"), &[]), Place::Broad("/".into()));
        assert!(warning(&Place::Broad("/".into()))
            .unwrap()
            .contains("root folder"));
    }

    #[test]
    fn a_home_folder_is_broad_even_as_a_repo() {
        let d = tempfile::tempdir().unwrap();
        let home = real(d.path()).join("me");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        std::fs::create_dir_all(home.join("notes")).unwrap();
        let homes = vec![home.clone()];
        assert_eq!(place_in(&home, &homes), Place::Broad(home.clone()));
        // A loose subfolder whose repository is the home's dotfiles one.
        assert_eq!(
            place_in(&home.join("notes"), &homes),
            Place::Broad(home.clone())
        );
        assert!(warning(&Place::Broad(home))
            .unwrap()
            .contains("home folder"));
    }

    #[test]
    fn a_folder_inside_home_is_not_home() {
        let d = tempfile::tempdir().unwrap();
        let home = real(d.path());
        std::fs::create_dir_all(home.join("Downloads")).unwrap();
        assert_eq!(
            place_in(&home.join("Downloads"), std::slice::from_ref(&home)),
            Place::Loose(home.join("Downloads"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_home_is_home() {
        let d = tempfile::tempdir().unwrap();
        let home = real(d.path()).join("me");
        std::fs::create_dir_all(&home).unwrap();
        let link = real(d.path()).join("link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert_eq!(
            place_in(&link, std::slice::from_ref(&home)),
            Place::Broad(home)
        );
    }

    #[test]
    fn the_account_home_counts_whatever_home_says() {
        // `homes()` always includes the account's home from the user
        // database, which `$HOME` can't change.
        if let Some(acct) = crate::paths::account_home() {
            let acct = acct.canonicalize().unwrap_or(acct);
            assert!(homes().contains(&acct));
            assert!(is_broad(&acct));
        }
    }

    #[test]
    fn accepted_folders_are_remembered_privately() {
        let d = tempfile::tempdir().unwrap();
        let store = Accepted::new(d.path().join("state/folders.json"));
        let loose = real(d.path());
        assert!(!store.has(&loose));
        store.add(&loose).unwrap();
        store.add(&loose).unwrap();
        assert!(store.has(&loose));
        assert!(!store.has(&loose.join("other")));
        let body = std::fs::read_to_string(d.path().join("state/folders.json")).unwrap();
        assert_eq!(body.matches(&loose.display().to_string()).count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join("state/folders.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn root_and_home_are_never_remembered() {
        let d = tempfile::tempdir().unwrap();
        let store = Accepted::new(d.path().join("folders.json"));
        assert!(store.add(Path::new("/")).is_err());
        if let Some(h) = crate::paths::account_home() {
            assert!(store.add(&h).is_err());
        }
        assert!(!d.path().join("folders.json").exists());
    }

    #[test]
    fn a_damaged_store_reads_as_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("folders.json");
        std::fs::write(&p, "{not json").unwrap();
        let store = Accepted::new(p);
        assert!(!store.has(d.path()));
        store.add(&real(d.path())).unwrap();
        assert!(store.has(&real(d.path())));
    }
}
