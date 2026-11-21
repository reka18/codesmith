//! The one way CodeSmith runs `git` on the host for itself: snapshots,
//! worktrees, `/review`, the diff viewer, the reference cache and project
//! ids. Every such command starts from [`command`] (or [`command_in`],
//! [`remote_command`]); a guard test fails on a `Command::new("git")`
//! anywhere else. Commands the user or the model run through the shell
//! tool are theirs, and are not changed.
//!
//! A repository's own config (`.git/config`, `config.worktree`, and what
//! they include) and its hooks may have been written by the model's
//! tools, or have come with a downloaded project, so nothing there may run
//! a program when CodeSmith runs git:
//!
//! - hooks are off (`core.hooksPath=/dev/null`), and so are
//!   `core.fsmonitor`, the pager (`--no-pager`, `core.pager=cat`) and
//!   external diff (`diff.external=`; diffs also take [`DIFF_SAFETY`],
//!   `--no-ext-diff --no-textconv`);
//! - every setting that names a program — `filter.*.clean`/`smudge`/
//!   `process`, `diff.*.textconv`/`command`, `merge.*.driver`,
//!   `credential.helper`, `core.sshCommand`, `core.askPass`,
//!   `core.gitProxy`, `gpg.program`, `pager.*`, `remote.*.uploadpack`, and
//!   the like ([`DANGEROUS`]) — set in the repository's own config is
//!   overridden by the user's (global or system) value for it, else
//!   neutralized; the overrides go in `GIT_CONFIG_COUNT`, which takes
//!   names and values apart, so no subsection name can confuse them;
//! - `protocol.ext.allow=never` and `protocol.file.allow=user`;
//! - `GIT_TERMINAL_PROMPT=0`, and the repository-location variables git
//!   exports to hooks ([`LOCAL_ENV_VARS`]: `GIT_DIR`, `GIT_INDEX_FILE`,
//!   `GIT_CONFIG_PARAMETERS`, ...) are cleared, so CodeSmith's git always
//!   acts on the directory it was given, even when CodeSmith itself runs
//!   inside a git hook;
//! - local commands also lose `GIT_SSH_COMMAND`, `GIT_SSH`, `GIT_ASKPASS`,
//!   `GIT_EXTERNAL_DIFF` and `GIT_PAGER`, and the credentials
//!   [`crate::child_env`] removes; [`remote_command`] (fetching a
//!   reference) keeps the user's own ssh and askpass variables, which no
//!   repository can set.
//!
//! OpenCode runs its git with `core.fsmonitor=false` and its diffs with
//! `--no-ext-diff`, and nothing more.

use std::path::Path;
use std::process::Command;

/// The program.
pub const GIT: &str = "git";

/// Arguments every diff-producing subcommand (`diff`, `show`, `log -p`)
/// takes after its name.
pub const DIFF_SAFETY: [&str; 2] = ["--no-ext-diff", "--no-textconv"];

/// The variables `git rev-parse --local-env-vars` names (what git exports
/// to a hook about the repository it runs in), and the config-injection
/// variables: inherited, they would point CodeSmith's git at another
/// repository or config.
pub const LOCAL_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_QUARANTINE_PATH",
];

/// Variables a local command loses: programs git would run.
const LOCAL_PROGRAM_VARS: &[&str] = &[
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_PROXY_COMMAND",
];

/// The settings (as `git config --get-regexp` spells them: section and
/// key lowercased) that name a program to run.
pub const DANGEROUS: &str = concat!(
    "^(",
    r"core\.(hookspath|fsmonitor|pager|sshcommand|askpass|gitproxy|alternaterefscommand|editor)",
    r"|diff\.external|sequence\.editor|pager\..+|interactive\.difffilter",
    r"|filter\..+\.(clean|smudge|process|required)",
    r"|diff\..+\.(textconv|command)",
    r"|merge\..+\.driver",
    r"|credential\.(.+\.)?helper",
    r"|gpg\.(.+\.)?program",
    r"|remote\..+\.(uploadpack|receivepack|proxy)",
    r"|uploadpack\.packobjectshook",
    ")$"
);

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// The settings fixed on every command, whatever any config says.
fn fixed(file: &str) -> Vec<(String, String)> {
    [
        ("core.hooksPath", null_device()),
        ("core.fsmonitor", "false"),
        ("core.pager", "cat"),
        ("diff.external", ""),
        ("protocol.ext.allow", "never"),
        ("protocol.file.allow", file),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// The value a repository's own setting `key` is replaced with when the
/// user sets none.
fn neutral(key: &str) -> &'static str {
    if key.ends_with(".required") {
        "false"
    } else if key.starts_with("diff.") && key.ends_with(".textconv") {
        // Identity: an empty textconv would make a diff fail.
        "cat"
    } else {
        ""
    }
}

/// Settings the repository set itself, each as the overrides that put the
/// user's own values (or a neutral one) back in force, read by listing
/// every scope's dangerous settings.
fn overrides(
    dir: &Path,
    git_dir: Option<&Path>,
    work_tree: Option<&Path>,
) -> Vec<(String, String)> {
    let mut c = bare(dir, git_dir, work_tree);
    c.env_remove("GIT_CONFIG_COUNT");
    let Ok(out) = c
        .args(["config", "--show-scope", "-z", "--get-regexp", DANGEROUS])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return vec![];
    };
    overrides_from(&String::from_utf8_lossy(&out.stdout))
}

/// [`overrides`] from `git config --show-scope -z --get-regexp` output:
/// `scope NUL key LF value NUL` entries.
fn overrides_from(listing: &str) -> Vec<(String, String)> {
    let mut parts = listing.split('\0');
    let mut entries: Vec<(String, String, String)> = Vec::new();
    while let (Some(scope), Some(kv)) = (parts.next(), parts.next()) {
        if scope.is_empty() {
            break;
        }
        let (k, v) = kv.split_once('\n').unwrap_or((kv, ""));
        entries.push((scope.to_string(), k.to_string(), v.to_string()));
    }
    // `command` is only this module's own `-c` settings: the user's
    // `GIT_CONFIG_PARAMETERS` is cleared.
    let trusted = |s: &str| matches!(s, "system" | "global" | "command");
    let mut keys: Vec<&str> = entries
        .iter()
        .filter(|(s, _, _)| !trusted(s))
        .map(|(_, k, _)| k.as_str())
        .collect();
    keys.sort();
    keys.dedup();
    let mut out = Vec::new();
    for k in keys {
        let mine: Vec<&str> = entries
            .iter()
            .filter(|(s, key, _)| trusted(s) && key == k)
            .map(|(_, _, v)| v.as_str())
            .collect();
        // An empty value resets a list (`credential.helper`); a single
        // value is the last one.
        out.push((k.to_string(), String::new()));
        if mine.is_empty() {
            out.push((k.to_string(), neutral(k).to_string()));
        } else {
            out.extend(mine.into_iter().map(|v| (k.to_string(), v.to_string())));
        }
    }
    out
}

/// A `git` command in `dir` with only the environment cleanup and the
/// fixed `-c` settings, for listing the config itself.
fn bare(dir: &Path, git_dir: Option<&Path>, work_tree: Option<&Path>) -> Command {
    let mut c = Command::new(GIT);
    for v in LOCAL_ENV_VARS {
        c.env_remove(v);
    }
    c.env_remove(crate::SERVER_TOKEN_VAR)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(dir)
        .arg("--no-pager");
    for (k, v) in fixed("user") {
        c.arg("-c").arg(format!("{k}={v}"));
    }
    if let Some(g) = git_dir {
        c.arg("--git-dir").arg(g);
    }
    if let Some(w) = work_tree {
        c.arg("--work-tree").arg(w);
    }
    c
}

fn build(
    dir: &Path,
    git_dir: Option<&Path>,
    work_tree: Option<&Path>,
    file: &str,
    remote: bool,
) -> Command {
    let mut c = bare(dir, git_dir, work_tree);
    if !remote {
        crate::child_env::scrub(&mut c);
        for v in LOCAL_PROGRAM_VARS {
            c.env_remove(v);
        }
    }
    // The repository's overrides, then the fixed settings again (so they
    // win there too, as `-c` does for a git without `GIT_CONFIG_COUNT`).
    let mut all = overrides(dir, git_dir, work_tree);
    all.extend(fixed(file));
    c.env("GIT_CONFIG_COUNT", all.len().to_string());
    for (i, (k, v)) in all.iter().enumerate() {
        c.env(format!("GIT_CONFIG_KEY_{i}"), k);
        c.env(format!("GIT_CONFIG_VALUE_{i}"), v);
    }
    // `-c` from `bare` is file.allow=user; a remote command may change it.
    if file != "user" {
        c.arg("-c").arg(format!("protocol.file.allow={file}"));
    }
    c
}

/// A hardened `git` command run in `dir`, for the repository there.
/// Callers add the subcommand and its arguments.
pub fn command(dir: &Path) -> Command {
    build(dir, None, None, "user", false)
}

/// [`command`] for a repository named explicitly: its git dir and work
/// tree (CodeSmith's snapshot repository).
pub fn command_in(dir: &Path, git_dir: &Path, work_tree: &Path) -> Command {
    build(dir, Some(git_dir), Some(work_tree), "user", false)
}

/// A hardened command that may reach a remote (the reference cache): the
/// user's own ssh, askpass and credential settings stay in force, and the
/// `file://` transport is allowed only when `allow_file`.
pub fn remote_command(dir: &Path, allow_file: bool) -> Command {
    let file = if allow_file { "always" } else { "never" };
    build(dir, None, None, file, true)
}

/// Plain git for tests that build repository fixtures: none of the
/// hardening (a fixture may want hooks or filters), but the
/// [`LOCAL_ENV_VARS`] are cleared, so a test run inside a git hook (the
/// pre-push CI run) never writes to the hook's repository.
#[doc(hidden)]
pub fn fixture() -> Command {
    let mut c = Command::new(GIT);
    for v in LOCAL_ENV_VARS {
        c.env_remove(v);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_settings_are_replaced_by_the_users_or_neutralized() {
        let listing = concat!(
            "global\0credential.helper\nosxkeychain\0",
            "local\0credential.helper\n!evil\0",
            "local\0filter.lfs.process\nevil\0",
            "global\0filter.lfs.process\ngit-lfs filter-process\0",
            "local\0filter.x=y.clean\nevil\0",
            "local\0diff.pic.textconv\nevil\0",
            "global\0core.sshcommand\nssh -i k\0",
            "worktree\0core.askpass\nevil\0",
        );
        let o = overrides_from(listing);
        let pairs: Vec<(&str, &str)> = o.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            pairs,
            [
                ("core.askpass", ""),
                ("core.askpass", ""),
                ("credential.helper", ""),
                ("credential.helper", "osxkeychain"),
                ("diff.pic.textconv", ""),
                ("diff.pic.textconv", "cat"),
                ("filter.lfs.process", ""),
                ("filter.lfs.process", "git-lfs filter-process"),
                ("filter.x=y.clean", ""),
                ("filter.x=y.clean", ""),
            ]
        );
        assert!(overrides_from("").is_empty());
    }

    /// A `Command::new("git")` anywhere in the workspace's sources outside
    /// this module bypasses the hardening. Tests build repository fixtures
    /// with [`fixture`]; a raw one must say `git-guard: test fixture` on
    /// the line or the one above, and the test-only files listed here are
    /// fixtures throughout.
    #[test]
    fn every_host_git_command_comes_from_the_hardened_builder() {
        const TEST_ONLY_FILES: &[&str] = &["tui/diff_source_oc.rs", "tui/parity_browser.rs"];
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut offenders = Vec::new();
        let mut stack = vec![crates.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                let name = e.file_name();
                if p.is_dir() {
                    if name != "target" && name != "tests" && name != "benches" {
                        stack.push(p);
                    }
                    continue;
                }
                let Some(rel) = p.to_str() else { continue };
                if !rel.ends_with(".rs") || !rel.contains("/src/") {
                    continue;
                }
                if rel.ends_with("codesmith-core/src/git.rs")
                    || TEST_ONLY_FILES.iter().any(|f| rel.ends_with(f))
                {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap();
                let lines: Vec<&str> = text.lines().collect();
                for (i, l) in lines.iter().enumerate() {
                    let squashed: String = l.chars().filter(|c| !c.is_whitespace()).collect();
                    if !squashed.contains("Command::new(\"git\"") {
                        continue;
                    }
                    let marked = |j: usize| {
                        lines
                            .get(j)
                            .is_some_and(|x| x.contains("git-guard: test fixture"))
                    };
                    if !(marked(i) || (i > 0 && marked(i - 1))) {
                        offenders.push(format!("{rel}:{}", i + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "run host git through codesmith_core::git::command: {offenders:#?}"
        );
    }
}
