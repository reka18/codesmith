//! Shell commands that reach a folder outside the project through an
//! option (`git -C`, `make -C`, `GIT_DIR=`...) or write a file there by
//! name (`rm`, `cp`, `perl -i`...) need `external_directory` approval,
//! like `cd`. Refused when nobody can answer, refused under a deny, and
//! the same commands inside the project ask nothing new. Each request
//! says whether the command only reads there.

use super::super::testing::{Env, Recorder};
use super::*;
use crate::permission::{base_rules, Effect, Rule};
use std::path::{Path, PathBuf};
use std::sync::Arc;

async fn sh(ctx: &Ctx<'_>, command: &str) -> Outcome {
    run(ctx, &serde_json::json!({ "command": command })).await
}

fn git(dir: &Path, args: &[&str]) {
    let out = crate::git::command(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_COUNT")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A repository with a committed file `f`, an archive and a patch.
fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("f"), "a\n").unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["add", "f"]);
    git(dir, &["commit", "-qm", "first"]);
}

/// Every file under `dir` and its bytes, `.git` included.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            if meta.is_dir() {
                stack.push(p);
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
            }
        }
    }
    out.sort();
    out
}

/// A project (the env's root, with `a`, `a.tar` and `x.diff` in it) and a
/// repository beside it, removed on drop.
struct World {
    env: Env,
    rec: Arc<Recorder>,
    sibling: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.sibling);
    }
}

fn world(rules: Vec<Rule>) -> World {
    let (env, rec) = Env::recording(rules);
    let root = env.root();
    std::fs::write(root.join("a"), "a\n").unwrap();
    std::fs::write(root.join("x.diff"), "").unwrap();
    let sibling = PathBuf::from(format!("{}-sib", root.display()));
    repo(&sibling);
    World { env, rec, sibling }
}

impl World {
    /// The sibling as `cmd` names it: `{S}` absolute, `{R}` relative.
    fn fill(&self, template: &str, abs: &Path, rel: &str) -> String {
        let _ = self;
        template
            .replace("{S}", &abs.display().to_string())
            .replace("{R}", rel)
    }
    fn outside(&self, template: &str) -> String {
        let name = self.sibling.file_name().unwrap().to_str().unwrap();
        self.fill(template, &self.sibling, &format!("../{name}"))
    }
}

/// Commands that change something in the folder they name.
const WRITES: &[&str] = &[
    "git -C {S} commit -am x",
    "git -C {R} checkout -b y",
    "git -C {S} reset --hard HEAD",
    "git -C {S} cherry-pick -n HEAD",
    "git -C {S} rebase --onto HEAD HEAD",
    "git -C {S} add -A",
    "git -C {S} rm f",
    "git -C {S} mv f g",
    "git -C {S} apply x.diff",
    "git -C {S} am x.diff",
    "git -C {S} merge HEAD",
    "git -C {S} stash",
    "git -C {S} clean -fdx",
    "git -C {S} restore f",
    "git -C {S} switch -c z",
    "git -C {S} worktree add w",
    "git -C . -C {R} commit -am x",
    "GIT_DIR={S}/.git GIT_WORK_TREE={S} git commit -am x",
    "GIT_DIR={R}/.git git reset --hard",
    "export GIT_DIR={S}/.git; git add -A",
    "git --git-dir={S}/.git --work-tree={S} add -A",
    "git --work-tree {R} checkout -- f",
    "git clone {S} {S}/clone",
    "git worktree add {R}-wt",
    "git apply --directory={R} x.diff",
    "env -C {S} rm f",
    "env --chdir={S} touch n",
    "sudo -D {S} rm f",
    "make -C {S}",
    "tar -xf a.tar -C {S}",
    "tar -cf {S}/out.tar a",
    "npm --prefix {S} install",
    "cargo --manifest-path {S}/Cargo.toml build",
    "rm {S}/f",
    "rm -rf {R}/f",
    "unlink {S}/f",
    "perl -i -pe 's/a/b/' {S}/f",
    "perl -pi.bak -e 's/a/b/' {R}/f",
    "ruby -i -pe 'x' {S}/f",
    "cp a {S}/b",
    "cp -t {S} a",
    "mv {S}/f g",
    "mv a {R}/a",
    "ln -s a {S}/l",
    "touch {S}/n",
    "mkdir {S}/d",
    "rmdir {S}/d",
    "truncate -s 0 {S}/f",
    "install a {S}/b",
    "chmod 600 {S}/f",
    "echo x | tee {S}/f",
    "patch {S}/f x.diff",
    "dd if=a of={S}/f",
    "unzip a.zip -d {S}",
    "curl -o {S}/f file:///dev/null",
    "curl -so {R}/f file:///dev/null",
    "wget -O {S}/f http://127.0.0.1:9/",
    "wget -P {S} http://127.0.0.1:9/",
    "gzip {S}/f",
    "rsync a {S}/b",
    "sed -i 's/a/b/' {S}/f",
    "awk '{print > \"{S}/f\"}' a",
    "find {S} -delete",
    "find {S} -name f -exec rm {} +",
    "find . -fprint {S}/f",
    "flock {S}/lock true",
    "script -q {S}/ts true",
    "echo x > {S}/f",
    "echo x >> {R}/f",
    "python3 -c \"open('{S}/f','w').write('x')\"",
    "sh -c 'rm {S}/f'",
    "(cd {S} && bash -c 'rm f')",
    "nice rm {S}/f",
    "timeout 5 rm {S}/f",
    "true && git -C {S} commit -am x",
];

/// Commands that only read in the folder they name.
const READS: &[&str] = &[
    "git -C {S} log --oneline",
    "git -C {S} status",
    "git -C {S} diff",
    "git -C {S} show HEAD",
    "git -C {S} blame f",
    "git -C {S} grep a",
    "git -C {S} ls-files",
    "git -C {S} rev-parse HEAD",
    "git -C {S} cat-file -p HEAD",
    "git -C {S} branch -a",
    "git -C {S} tag -l",
    "git --git-dir={S}/.git log",
    "GIT_DIR={S}/.git git status",
    "cd {S} && ls && git log | head -1",
];

fn asked_external(rec: &Recorder) -> Vec<crate::permission::Request> {
    rec.asked
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.action == "external_directory")
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_targets_are_refused_when_nobody_can_answer() {
    for template in WRITES.iter().chain(READS) {
        // A headless run: every question is refused.
        let w = world(base_rules());
        w.rec.deny_action("external_directory");
        let command = w.outside(template);
        let before = snapshot(&w.sibling);
        let result = sh(&w.env.ctx(), &command).await;
        assert!(result.is_err(), "{command} ran: {result:?}");
        let asked = asked_external(&w.rec);
        assert!(!asked.is_empty(), "{command}: {:?}", w.rec.requests());
        assert!(
            w.rec.actions().iter().all(|a| a != "shell"),
            "{command} reached the shell rules first"
        );
        assert_eq!(
            snapshot(&w.sibling),
            before,
            "{command} changed the sibling"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_targets_are_refused_under_a_deny() {
    for template in WRITES.iter().chain(READS) {
        let mut rules = base_rules();
        rules.push(Rule::new("external_directory", "*", Effect::Deny));
        let w = world(rules);
        let command = w.outside(template);
        let before = snapshot(&w.sibling);
        let err = sh(&w.env.ctx(), &command).await.unwrap_err();
        assert!(err.contains("Permission denied"), "{command}: {err}");
        assert!(asked_external(&w.rec).is_empty(), "{command}");
        assert_eq!(
            snapshot(&w.sibling),
            before,
            "{command} changed the sibling"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_request_says_whether_the_command_only_reads() {
    for (templates, want) in [(WRITES, Access::Write), (READS, Access::Read)] {
        for template in templates {
            let w = world(base_rules());
            w.rec.deny_action("external_directory");
            let command = w.outside(template);
            let _ = sh(&w.env.ctx(), &command).await;
            let asked = asked_external(&w.rec);
            assert_eq!(asked[0].access, Some(want), "{command}");
        }
    }
    // A target only known at run time counts as a write.
    let w = world(base_rules());
    w.rec.deny_action("external_directory");
    let _ = sh(&w.env.ctx(), "rm -rf \"$X\"").await;
    assert_eq!(asked_external(&w.rec)[0].access, Some(Access::Write));
    // A computed folder with only reading commands still reads.
    let w = world(base_rules());
    w.rec.deny_action("external_directory");
    let _ = sh(&w.env.ctx(), "git -C \"$D\" log").await;
    assert_eq!(asked_external(&w.rec)[0].access, Some(Access::Read));
}

#[tokio::test(flavor = "multi_thread")]
async fn approved_outside_targets_run() {
    let w = world(base_rules());
    let command = w.outside("git -C {S} checkout -q -b approved && rm {S}/f");
    let out = sh(&w.env.ctx(), &command).await.unwrap();
    assert!(!w.sibling.join("f").exists(), "{}", out.content);
    let asked = asked_external(&w.rec);
    assert!(!asked.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_commands_inside_the_project_ask_nothing_new() {
    for template in WRITES.iter().chain(READS) {
        if ["sudo", "npm", "cargo"]
            .iter()
            .any(|p| template.starts_with(p))
        {
            continue; // would run sudo, or reach a registry
        }
        let w = world(base_rules());
        let inner = w.env.root().join("inner");
        repo(&inner);
        let command = w.fill(template, &inner, "inner");
        let _ = sh(&w.env.ctx(), &command).await;
        let asked = asked_external(&w.rec);
        assert!(
            asked.is_empty(),
            "{command} asked {:?}",
            asked.iter().map(|r| &r.resources).collect::<Vec<_>>()
        );
    }
    // And they still do what they say.
    let w = world(base_rules());
    let inner = w.env.root().join("inner");
    repo(&inner);
    sh(&w.env.ctx(), "rm inner/f && git -C inner checkout -q -b y")
        .await
        .unwrap();
    assert!(!inner.join("f").exists());
    assert!(asked_external(&w.rec).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_symlink_into_the_outside_folder_is_outside() {
    for template in [
        "git -C link commit -am x",
        "rm link/f",
        "perl -i -pe 's/a/b/' link/f",
        "cp a link/b",
        "GIT_DIR=link/.git git add -A",
        "make -C link",
    ] {
        let w = world(base_rules());
        w.rec.deny_action("external_directory");
        std::os::unix::fs::symlink(&w.sibling, w.env.root().join("link")).unwrap();
        let before = snapshot(&w.sibling);
        let result = sh(&w.env.ctx(), template).await;
        assert!(result.is_err(), "{template}");
        assert!(!asked_external(&w.rec).is_empty(), "{template}");
        assert_eq!(snapshot(&w.sibling), before, "{template}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn consecutive_cds_are_followed() {
    // From a subfolder of the project, `cd ..` stays inside and `cd ../x`
    // after it goes out.
    let (env, rec) = Env::recording(base_rules());
    rec.deny_action("external_directory");
    let root = env.root();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let sibling = PathBuf::from(format!("{}-sib", root.display()));
    repo(&sibling);
    let name = sibling.file_name().unwrap().to_str().unwrap();
    let mut ctx = env.ctx();
    ctx.cwd = root.join("sub");
    for command in [
        format!("cd .. && cd ../{name} && rm f"),
        format!("cd .. && git -C ../{name} commit -am x"),
        format!("cd ..; rm ../{name}/f"),
    ] {
        let result = sh(&ctx, &command).await;
        assert!(result.is_err(), "{command}");
        assert!(sibling.join("f").exists(), "{command}");
    }
    assert!(rec
        .asked
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.action != "shell"));
    let _ = std::fs::remove_dir_all(&sibling);
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_writes_only_known_at_run_time_are_refused_headless() {
    for command in [
        "rm -rf \"$TARGET\"",
        "ls .. | xargs rm",
        "patch -p0 < x.diff",
        "git -C \"$D\" commit -am x",
        "cp a \"$(echo ..)/x\"",
    ] {
        let w = world(base_rules());
        w.rec.deny_action("external_directory");
        let before = snapshot(&w.sibling);
        assert!(sh(&w.env.ctx(), command).await.is_err(), "{command}");
        assert_eq!(snapshot(&w.sibling), before, "{command}");
    }
}
