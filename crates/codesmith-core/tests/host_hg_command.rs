//! How CodeSmith's own Mercurial is started, seen by a stand-in `hg` on
//! `PATH` that records its arguments and environment: this runs whether
//! or not Mercurial is installed. The parent's `HGRCPATH`, `HGUSER`,
//! `HG_PENDING` and the like are gone, the repository's config is
//! skipped, the user's hooks and repository-relative extensions are
//! disabled, and the user's config is listed once until it changes.
//!
//! Nothing here reads the user's own config: HOME and XDG_CONFIG_HOME
//! point into a temporary folder.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Call {
    args: Vec<String>,
    env: Vec<(String, String)>,
}

fn calls(log: &Path) -> Vec<Call> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut out: Vec<Call> = Vec::new();
    for line in text.lines() {
        if let Some(args) = line.strip_prefix("ARGS\t") {
            out.push(Call {
                args: args.split('\t').map(str::to_string).collect(),
                env: Vec::new(),
            });
        } else if let Some(kv) = line.strip_prefix("ENV ") {
            if let (Some(c), Some((k, v))) = (out.last_mut(), kv.split_once('=')) {
                c.env.push((k.to_string(), v.to_string()));
            }
        }
    }
    out
}

impl Call {
    fn env(&self, k: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
    fn has_pair(&self, a: &str, b: &str) -> bool {
        self.args.windows(2).any(|w| w[0] == a && w[1] == b)
    }
    fn is_probe(&self) -> bool {
        self.args.iter().any(|a| a == "config")
    }
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".hgrc"), "[ui]\nusername = t\n").unwrap();
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = base.join("calls");
    let fake = bin.join("hg");
    std::fs::write(
        &fake,
        format!(
            r#"#!/bin/sh
log='{log}'
{{ printf 'ARGS'; for a in "$@"; do printf '\t%s' "$a"; done; printf '\n'
   env | sed 's/^/ENV /'; }} >> "$log"
for a in "$@"; do
  case "$a" in
    --version) echo "Mercurial Distributed SCM (version 6.9.1)"; exit 0;;
    config) printf 'hooks.pre-status=touch evil\nhooks.priority.pre-status=1\nextensions.evil=evil.py\nextensions.rebase=\n'; exit 0;;
    branch) echo feature; exit 0;;
  esac
done
exit 0
"#,
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = base.join("proj");
    std::fs::create_dir_all(root.join(".hg")).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    // SAFETY: the only test in this binary, before it runs anything.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("HGRCPATH", base.join("planted.rc"));
        std::env::set_var("HGUSER", "planted");
        std::env::set_var("HGPLAINEXCEPT", "alias,pager");
        std::env::set_var("HGMERGE", "planted");
        std::env::set_var("HG_PENDING", base.join("decoy"));
        std::env::set_var("HGENCODING", "utf-8");
    }
    (tmp, home, root, log)
}

#[test]
fn codesmiths_hg_skips_the_repository_config_and_the_inherited_environment() {
    let (_tmp, home, root, log) = setup();

    assert_eq!(
        codesmith_core::vcs::hg_branch(&root).as_deref(),
        Some("feature")
    );
    let all = calls(&log);
    let run = all.last().unwrap();
    assert_eq!(run.args.last().map(String::as_str), Some("branch"));
    assert_eq!(run.env("HGPLAIN"), Some("1"));
    assert_eq!(run.env("HGRCSKIPREPO"), Some("1"));
    let rc = home.join(".hgrc");
    assert_eq!(run.env("HGRCPATH"), rc.to_str(), "only the user's own hgrc");
    for gone in ["HGUSER", "HGPLAINEXCEPT", "HGMERGE", "HG_PENDING"] {
        assert_eq!(run.env(gone), None, "{gone} was inherited");
    }
    assert_eq!(run.env("HGENCODING"), Some("utf-8"));
    assert!(run.args.iter().any(|a| a == "--noninteractive"));
    assert!(run.has_pair("--pager", "never"));
    assert!(run.has_pair("-R", root.to_str().unwrap()));
    for c in [
        "ui.interactive=false",
        "ui.merge=internal:fail",
        "pager.pager=",
        "ui.ssh=false",
        "hooks.pre-status=",
        "extensions.evil=!",
    ] {
        assert!(
            run.has_pair("--config", c),
            "missing --config {c}: {:?}",
            run.args
        );
    }
    assert!(!run.args.iter().any(|a| a.starts_with("extensions.rebase")));
    assert!(!run.args.iter().any(|a| a.starts_with("hooks.priority")));

    // The probes ran outside the repository, under the same environment.
    let probes: Vec<&Call> = all.iter().filter(|c| c.is_probe()).collect();
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].env("HGRCSKIPREPO"), Some("1"));
    assert!(!probes[0].args.iter().any(|a| a == "-R"));

    // The user's config is listed again only once it changes.
    codesmith_core::vcs::hg_branch(&root);
    codesmith_core::vcs::hg_status(&root);
    assert_eq!(calls(&log).iter().filter(|c| c.is_probe()).count(), 1);
    std::fs::write(&rc, "[ui]\nusername = someone else\n").unwrap();
    codesmith_core::vcs::hg_branch(&root);
    assert_eq!(calls(&log).iter().filter(|c| c.is_probe()).count(), 2);
}
