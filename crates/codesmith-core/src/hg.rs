//! The one way CodeSmith runs Mercurial on the host for itself: a
//! Mercurial project's id, its named branch and the working copy and
//! branch reviews ([`crate::vcs`]). Every such command starts from
//! [`command`]; a guard test (in [`crate::git`]) fails on a
//! `Command::new("hg")` anywhere else. Commands the user or the model run
//! through the shell tool are theirs, and are not changed.
//!
//! A repository's `.hg/hgrc` (and `hgrc-not-shared`, a share source's
//! `hgrc`, and every file they `%include`) may have been written by the
//! model's tools, or have come with a downloaded project. Read, it could
//! run a program through `[hooks]` (`pre-status`, ...), `[extensions]`
//! (a Python file in the repository), `[ui] merge`, `[merge-tools]`,
//! `[pager]`, `[extdiff]`, `[ui] ssh` or an `[alias]`. So:
//!
//! - `HGRCSKIPREPO=1`: Mercurial reads no repository config at all, the
//!   project's or a nested subrepository's. It is Mercurial 5.7's; an
//!   older `hg` ([`MIN_VERSION`]) would ignore it, so CodeSmith runs no
//!   Mercurial command there (an operation reads as no output);
//! - `HGRCPATH` is the user's own global config files only (`~/.hgrc`,
//!   `$XDG_CONFIG_HOME/hg/hgrc`), whatever the parent set, so the user's
//!   extensions (a repository that needs `largefiles`, say) keep working;
//! - `HGPLAIN=1`: no aliases, `[defaults]`, colour, pager or other
//!   output-changing user settings;
//! - every hook the remaining config defines is set empty (which
//!   disables it), and an extension it loads from a relative path (which
//!   Mercurial resolves against the repository) is disabled with `!`;
//!   both are read by one `hg config hooks extensions`, cached while the
//!   config files are unchanged ([`user_overrides`]);
//! - `--noninteractive`, `--pager never`, `ui.interactive=false`,
//!   `ui.paginate=false`, `pager.pager=`, `ui.merge=internal:fail`,
//!   `ui.ssh=false`, `ui.editor=false`, `extensions.extdiff=!`, and git
//!   and Subversion subrepositories refused;
//! - every inherited `HG*` variable is cleared but the encoding ones:
//!   `HGRCPATH`, `HGPLAINEXCEPT`, `HGUSER`, `HGEDITOR`, `HGMERGE`, and
//!   what Mercurial exports to a hook (`HG_PENDING`, which would read
//!   another repository's pending transaction, `HG_NODE`, ...), so
//!   CodeSmith's Mercurial acts on the repository it was given even when
//!   CodeSmith runs inside a Mercurial hook. The repository is named with
//!   `-R`, and the credentials [`crate::child_env`] removes are removed.
//!
//! OpenCode runs its Mercurial with `HGPLAIN=1` and nothing more.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// The program.
pub const HG: &str = "hg";

/// The first Mercurial that honours `HGRCSKIPREPO`.
pub const MIN_VERSION: (u32, u32) = (5, 7);

/// Inherited variables kept: they only say how to decode output.
const KEEP: &[&str] = &["HGENCODING", "HGENCODINGMODE", "HGENCODINGAMBIGUOUS"];

/// The settings fixed on every command, whatever any config says.
pub const FIXED: &[(&str, &str)] = &[
    ("ui.interactive", "false"),
    ("ui.paginate", "false"),
    ("pager.pager", ""),
    ("ui.merge", "internal:fail"),
    ("ui.ssh", "false"),
    ("ui.editor", "false"),
    ("extensions.extdiff", "!"),
    ("subrepos.git:allowed", "false"),
    ("subrepos.svn:allowed", "false"),
];

/// The user's global config files Mercurial would read (its
/// `userrcpath`), those that exist, as `HGRCPATH` lists them. A path with
/// the list separator in it is left out.
fn user_rc_files() -> Vec<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .filter(|h| h.is_absolute());
    let mut files = Vec::new();
    if cfg!(windows) {
        if let Some(h) = &home {
            files.push(h.join("mercurial.ini"));
            files.push(h.join(".hgrc"));
        }
    } else {
        if let Some(h) = &home {
            files.push(h.join(".hgrc"));
        }
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        if let Some(c) = config_home {
            files.push(c.join("hg").join("hgrc"));
        }
    }
    let sep = if cfg!(windows) { ';' } else { ':' };
    files
        .into_iter()
        .filter(|f| f.is_file() && f.to_str().is_some_and(|s| !s.contains(sep)))
        .collect()
}

fn rc_path(files: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    files
        .iter()
        .filter_map(|f| f.to_str())
        .collect::<Vec<_>>()
        .join(sep)
}

/// A `hg` with the environment cleanup and the fixed settings, in `dir`,
/// reading only `files` for config.
fn bare(dir: &Path, files: &[PathBuf]) -> Command {
    let mut c = Command::new(HG);
    for (k, _) in std::env::vars_os() {
        if let Some(k) = k.to_str() {
            if k.starts_with("HG") && !KEEP.contains(&k) {
                c.env_remove(k);
            }
        }
    }
    crate::child_env::scrub(&mut c);
    c.env_remove(crate::SERVER_TOKEN_VAR)
        .env("HGPLAIN", "1")
        .env("HGRCSKIPREPO", "1")
        .env("HGRCPATH", rc_path(files))
        .current_dir(dir)
        .stdin(Stdio::null())
        .args(["--noninteractive", "--pager", "never"]);
    for (k, v) in FIXED {
        c.arg("--config").arg(format!("{k}={v}"));
    }
    c
}

/// A folder no repository can be planted in, for commands that need none.
fn neutral_dir() -> &'static Path {
    Path::new("/")
}

/// `X.Y` of `hg --version` output.
fn parse_version(out: &str) -> Option<(u32, u32)> {
    let rest = &out[out.find("version ")? + "version ".len()..];
    let mut parts = rest.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// The installed Mercurial's version, asked once.
fn version() -> Option<(u32, u32)> {
    static V: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *V.get_or_init(|| {
        let out = bare(neutral_dir(), &user_rc_files())
            .args(["--version", "-q"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| parse_version(&String::from_utf8_lossy(&out.stdout)))
            .flatten()
    })
}

/// [`user_overrides`] from `hg config hooks extensions` output.
fn overrides_from(listing: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in listing.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if let Some(name) = key.strip_prefix("hooks.") {
            // `priority.NAME` and `tonative.NAME` are a hook's options.
            if !name.is_empty() && !name.starts_with("priority.") && !name.starts_with("tonative.")
            {
                out.push((key.to_string(), String::new()));
            }
        } else if let Some(name) = key.strip_prefix("extensions.") {
            let v = value.trim();
            let from_path = !v.is_empty() && !v.starts_with('!');
            let anchored = Path::new(v).is_absolute() || v.starts_with("~/");
            if !name.is_empty() && from_path && !anchored {
                out.push((key.to_string(), "!".to_string()));
            }
        }
    }
    out
}

/// What identifies a config file's content without reading it.
type Stamp = Vec<(PathBuf, Option<(SystemTime, u64)>)>;

/// `--config` overrides, as `section.name` and value.
type Overrides = Vec<(String, String)>;

fn stamp(files: &[PathBuf]) -> Stamp {
    files
        .iter()
        .map(|f| {
            let m = std::fs::metadata(f).ok();
            (
                f.clone(),
                m.and_then(|m| Some((m.modified().ok()?, m.len()))),
            )
        })
        .collect()
}

/// The overrides that disable every hook the user's own config defines
/// and every extension it loads from a relative path. The listing is
/// kept while the same files have the same modification times and sizes:
/// it is the user's config, which no repository can reach (a file it
/// `%include`s is not watched).
fn user_overrides(files: &[PathBuf]) -> Vec<(String, String)> {
    static CACHE: Mutex<Option<(Stamp, Overrides)>> = Mutex::new(None);
    let key = stamp(files);
    if let Some((k, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *k == key {
            return v.clone();
        }
    }
    let listing = bare(neutral_dir(), files)
        .args(["config", "hooks", "extensions"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let v = overrides_from(&listing);
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, v.clone()));
    v
}

/// A hardened `hg` command for the repository at `root`, run there.
/// Callers add the subcommand and its arguments. `None` when Mercurial is
/// missing or older than [`MIN_VERSION`], which cannot be kept from
/// reading the repository's config.
pub fn command(root: &Path) -> Option<Command> {
    if version()? < MIN_VERSION {
        return None;
    }
    let files = user_rc_files();
    let mut c = bare(root, &files);
    for (k, v) in user_overrides(&files) {
        c.arg("--config").arg(format!("{k}={v}"));
    }
    c.arg("-R").arg(root);
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(
            parse_version("Mercurial Distributed SCM (version 6.8.1)\n"),
            Some((6, 8))
        );
        assert_eq!(parse_version("Mercurial (version 5.7rc0+20)"), Some((5, 7)));
        assert_eq!(parse_version("Mercurial (version 4.9)"), Some((4, 9)));
        assert_eq!(parse_version("Mercurial (version 7)"), Some((7, 0)));
        assert_eq!(parse_version("nonsense"), None);
        assert!(parse_version("Mercurial (version 5.6.1)").unwrap() < MIN_VERSION);
    }

    #[test]
    fn user_hooks_and_relative_extensions_are_disabled() {
        let listing = concat!(
            "hooks.pre-status=touch x\n",
            "hooks.post-commit.notify=python:hk.run\n",
            "hooks.priority.pre-status=10\n",
            "extensions.rebase=\n",
            "extensions.off=!\n",
            "extensions.abs=/opt/ext/a.py\n",
            "extensions.home=~/ext/h.py\n",
            "extensions.rel=ext/evil.py\n",
            "extensions.var=$PWD/evil.py\n",
            "ui.username=nope\n",
        );
        let pairs: Vec<(String, String)> = overrides_from(listing);
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("hooks.pre-status", ""),
                ("hooks.post-commit.notify", ""),
                ("extensions.rel", "!"),
                ("extensions.var", "!"),
            ]
        );
    }
}
