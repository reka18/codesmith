//! What `/review` looks at: the working tree, a commit, a range, a branch
//! or a GitHub pull request, turned into a diff plus context.

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .env_remove(crate::SERVER_TOKEN_VAR)
        .args(args)
        .current_dir(root)
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

fn gh(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("gh")
        .env_remove(crate::SERVER_TOKEN_VAR)
        .args(args)
        .current_dir(root)
        .output()
        .context("running gh (the GitHub CLI) for a pull request")?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// CodeSmith's own review settings are not part of the change under review.
const EXCLUDE: &str = ":(exclude).codesmith/review.json";

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// e.g. "uncommitted changes", "commit 1a2b3c4", "pull request #12".
    pub label: String,
    pub diff: String,
    pub files: Vec<String>,
    /// Commit messages or the PR title and description.
    pub context: String,
    /// A command a reviewer can run to see the change itself.
    pub command: String,
    pub pr: Option<u64>,
}

/// Parsed `/review` arguments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Args {
    pub target: Option<String>,
    pub comment: bool,
}

pub fn parse_args(s: &str) -> Args {
    let mut a = Args::default();
    let mut rest = Vec::new();
    for w in s.split_whitespace() {
        match w {
            "--comment" => a.comment = true,
            w => rest.push(w),
        }
    }
    let t = rest.join(" ");
    a.target = (!t.is_empty()).then_some(t);
    a
}

/// A PR number from `#12`, `pr 12`, `12`-with-gh or a pull URL.
pub fn pr_number(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(i) = s.find("/pull/") {
        return s[i + 6..]
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok();
    }
    let s = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("pr "))
        .or_else(|| s.strip_prefix("PR "))
        .unwrap_or(s);
    s.trim().parse().ok()
}

/// Files a unified diff touches (new paths; deleted files keep the old one).
pub fn diff_files(diff: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        // "a/x b/y"; paths with spaces split on " b/".
        let path = rest
            .rsplit_once(" b/")
            .map(|(_, b)| b)
            .unwrap_or(rest)
            .to_string();
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn untracked_diff(root: &Path) -> Result<String> {
    let mut out = String::new();
    for f in git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            ".",
            EXCLUDE,
        ],
    )?
    .lines()
    .filter(|l| !l.is_empty())
    {
        let o = Command::new("git")
            .env_remove(crate::SERVER_TOKEN_VAR)
            .args(["diff", "--no-index", "--", "/dev/null", f])
            .current_dir(root)
            .output()?;
        out.push_str(&String::from_utf8_lossy(&o.stdout));
    }
    Ok(out)
}

fn default_base(root: &Path) -> Option<String> {
    if let Ok(r) = git(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        return Some(r.trim().to_string());
    }
    ["main", "master", "origin/main", "origin/master"]
        .into_iter()
        .find(|b| git(root, &["rev-parse", "--verify", "--quiet", b]).is_ok())
        .map(str::to_string)
}

fn is_branch(root: &Path, name: &str) -> bool {
    git(
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .is_ok()
        || git(
            root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{name}"),
            ],
        )
        .is_ok()
}

pub fn resolve(root: &Path, target: Option<&str>) -> Result<Target> {
    if let Some((dir, crate::vcs::Kind::Hg)) = crate::vcs::marker(root) {
        return resolve_hg(&dir, target);
    }
    git(root, &["rev-parse", "--git-dir"])
        .map_err(|_| anyhow!("{} is not a git repository", root.display()))?;
    let t = match target.map(str::trim).filter(|t| !t.is_empty()) {
        None => {
            let status = git(root, &["status", "--porcelain"])?;
            if !status.trim().is_empty() {
                let has_head = git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok();
                let mut diff = if has_head {
                    git(root, &["diff", "HEAD", "--", ".", EXCLUDE])?
                } else {
                    git(root, &["diff", "--cached", "--", ".", EXCLUDE])?
                };
                diff.push_str(&untracked_diff(root)?);
                Target {
                    label: "uncommitted changes".into(),
                    files: diff_files(&diff),
                    diff,
                    context: String::new(),
                    command: "git diff HEAD (and `git status` for new files)".into(),
                    pr: None,
                }
            } else {
                let base = default_base(root);
                let mb = base
                    .as_deref()
                    .and_then(|b| git(root, &["merge-base", b, "HEAD"]).ok())
                    .map(|s| s.trim().to_string());
                let head = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
                match (base, mb) {
                    (Some(b), Some(mb)) if mb != head => {
                        let range = format!("{mb}..HEAD");
                        let diff = git(root, &["diff", &range, "--", ".", EXCLUDE])?;
                        let context = git(root, &["log", "--format=%h %s%n%b", &range])?;
                        Target {
                            label: format!("this branch against {b}"),
                            files: diff_files(&diff),
                            diff,
                            context,
                            command: format!("git diff {mb}..HEAD"),
                            pr: None,
                        }
                    }
                    _ => commit(root, "HEAD")?,
                }
            }
        }
        Some(t)
            if pr_number(t).is_some()
                && (t.starts_with('#')
                    || t.to_lowercase().starts_with("pr")
                    || t.contains("/pull/")
                    || !is_commitish(root, t)) =>
        {
            let n = pr_number(t).unwrap();
            let info: serde_json::Value = serde_json::from_str(&gh(
                root,
                &[
                    "pr",
                    "view",
                    &n.to_string(),
                    "--json",
                    "title,body,state,isDraft,baseRefName,headRefName",
                ],
            )?)?;
            if info["state"] != "OPEN" {
                return Err(anyhow!(
                    "pull request #{n} is {}; only open pull requests are reviewed",
                    info["state"].as_str().unwrap_or("not open").to_lowercase()
                ));
            }
            let diff = gh(root, &["pr", "diff", &n.to_string()])?;
            let mut context = format!(
                "Pull request title: {}\n\n{}",
                info["title"].as_str().unwrap_or(""),
                info["body"].as_str().unwrap_or("").trim()
            );
            if info["isDraft"] == true {
                context.push_str("\n\n(This pull request is a draft.)");
            }
            Target {
                label: format!(
                    "pull request #{n} ({} → {})",
                    info["headRefName"].as_str().unwrap_or("?"),
                    info["baseRefName"].as_str().unwrap_or("?")
                ),
                files: diff_files(&diff),
                diff,
                context,
                command: format!("gh pr diff {n}"),
                pr: Some(n),
            }
        }
        Some(t) if t.contains("..") => {
            let diff = git(root, &["diff", t, "--", ".", EXCLUDE])?;
            let context = git(
                root,
                &["log", "--format=%h %s%n%b", &t.replace("...", "..")],
            )
            .unwrap_or_default();
            Target {
                label: format!("range {t}"),
                files: diff_files(&diff),
                diff,
                context,
                command: format!("git diff {t}"),
                pr: None,
            }
        }
        Some(t) if is_branch(root, t) => {
            let range = format!("{t}...HEAD");
            let diff = git(root, &["diff", &range, "--", ".", EXCLUDE])?;
            let context = git(root, &["log", "--format=%h %s%n%b", &format!("{t}..HEAD")])
                .unwrap_or_default();
            Target {
                label: format!("this branch against {t}"),
                files: diff_files(&diff),
                diff,
                context,
                command: format!("git diff {range}"),
                pr: None,
            }
        }
        Some(t) => commit(root, t)?,
    };
    if t.diff.trim().is_empty() {
        return Err(anyhow!("nothing to review in {}", t.label));
    }
    Ok(t)
}

/// A Mercurial project's review: the working copy, else the named branch
/// against `default`. Commits, ranges and bases are Git's.
fn resolve_hg(root: &Path, target: Option<&str>) -> Result<Target> {
    use crate::vcs::{hg_branch, hg_diff, Mode};
    let base = target.map(str::trim).filter(|t| !t.is_empty());
    if base.is_some() {
        hg_diff(root, Mode::Branch, base)?;
    }
    let join =
        |files: &[crate::vcs::FileDiff]| files.iter().map(|f| f.patch.as_str()).collect::<String>();
    let working = hg_diff(root, Mode::Working, None)?;
    let t = if !working.is_empty() {
        let diff = join(&working);
        Target {
            label: "uncommitted changes".into(),
            files: working.iter().map(|f| f.file.clone()).collect(),
            diff,
            context: String::new(),
            command: "hg diff --git (and `hg status` for new and missing files)".into(),
            pr: None,
        }
    } else {
        let branch = hg_diff(root, Mode::Branch, None)?;
        let name = hg_branch(root).unwrap_or_else(|| "default".into());
        Target {
            label: format!("branch {name} against default"),
            files: branch.iter().map(|f| f.file.clone()).collect(),
            diff: join(&branch),
            context: String::new(),
            command: "hg diff --git -r \"ancestor(., default)\"".into(),
            pr: None,
        }
    };
    if t.diff.trim().is_empty() {
        return Err(anyhow!("nothing to review in {}", t.label));
    }
    Ok(t)
}

fn is_commitish(root: &Path, t: &str) -> bool {
    git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{t}^{{commit}}"),
        ],
    )
    .is_ok()
}

fn commit(root: &Path, rev: &str) -> Result<Target> {
    if !is_commitish(root, rev) {
        return Err(anyhow!(
            "{rev:?} is not a commit, branch, range or pull request"
        ));
    }
    let short = git(root, &["rev-parse", "--short", rev])?
        .trim()
        .to_string();
    let diff = git(root, &["show", "--format=", rev, "--", ".", EXCLUDE])?;
    let context = git(root, &["show", "-s", "--format=%s%n%n%b", rev])?;
    Ok(Target {
        label: format!("commit {short}"),
        files: diff_files(&diff),
        diff,
        context: context.trim().to_string(),
        command: format!("git show {short}"),
        pr: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let run = |a: &[&str]| {
            assert!(
                Command::new("git")
                    .args(a)
                    .current_dir(r)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "{a:?}"
            )
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(r.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first"]);
        std::fs::write(r.join("a.txt"), "one\ntwo\n").unwrap();
        run(&["commit", "-q", "-am", "second: add two"]);
        d
    }

    #[test]
    fn args_and_pr_numbers() {
        assert_eq!(parse_args(""), Args::default());
        assert_eq!(
            parse_args("#12 --comment"),
            Args {
                target: Some("#12".into()),
                comment: true
            }
        );
        assert_eq!(pr_number("#12"), Some(12));
        assert_eq!(pr_number("pr 7"), Some(7));
        assert_eq!(pr_number("https://github.com/o/r/pull/34/files"), Some(34));
        assert_eq!(pr_number("main"), None);
    }

    #[test]
    fn diff_file_names() {
        let d = "diff --git a/x.rs b/x.rs\n+++ b/x.rs\ndiff --git a/dir/my file.txt b/dir/my file.txt\n";
        assert_eq!(diff_files(d), ["x.rs", "dir/my file.txt"]);
    }

    #[test]
    fn targets_in_a_repository() {
        let d = repo();
        let r = d.path();
        let t = resolve(r, None).unwrap();
        assert_eq!(
            t.label,
            format!(
                "commit {}",
                git(r, &["rev-parse", "--short", "HEAD"]).unwrap().trim()
            ),
            "a clean tree on the base branch reviews the last commit"
        );
        assert!(t.context.starts_with("second: add two"));
        assert_eq!(t.files, ["a.txt"]);

        std::fs::write(r.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(r.join("new.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(r.join(".codesmith")).unwrap();
        std::fs::write(r.join(".codesmith/review.json"), "{}").unwrap();
        let t = resolve(r, None).unwrap();
        assert_eq!(t.label, "uncommitted changes");
        assert_eq!(t.files, ["a.txt", "new.rs"], "untracked files are included");
        assert!(t.diff.contains("+fn main() {}"));

        assert_eq!(
            resolve(r, Some("HEAD~1")).unwrap().label,
            format!(
                "commit {}",
                git(r, &["rev-parse", "--short", "HEAD~1"]).unwrap().trim()
            )
        );
        assert!(resolve(r, Some("HEAD~1..HEAD"))
            .unwrap()
            .diff
            .contains("+two"));
        assert!(resolve(r, Some("no-such-thing")).is_err());
    }

    #[test]
    fn branches_compare_against_their_base() {
        let d = repo();
        let r = d.path();
        git(r, &["checkout", "-q", "-b", "feature"]).unwrap();
        std::fs::write(r.join("b.txt"), "b\n").unwrap();
        git(r, &["add", "."]).unwrap();
        git(
            r,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "add b"],
        )
        .unwrap();
        let t = resolve(r, None).unwrap();
        assert_eq!(t.label, "this branch against main");
        assert_eq!(t.files, ["b.txt"]);
        assert!(t.context.contains("add b"));
        assert_eq!(resolve(r, Some("main")).unwrap().files, ["b.txt"]);
    }
}
