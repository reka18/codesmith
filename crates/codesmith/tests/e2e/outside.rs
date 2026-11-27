//! A headless `codesmith run` (as a bridge runs it: the default rules,
//! nobody to answer) whose model works in a repository beside the project
//! through the shell: `git -C`, `rm`, `perl -i`, `cp`. Nothing changes
//! there, and each refusal tells the model why.

use crate::harness::Env;
use crate::model::{self, Model, Reply, Script, M};
use crate::need_git;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if std::fs::symlink_metadata(&p).unwrap().is_dir() {
                stack.push(p);
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap_or_default()));
            }
        }
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_cannot_work_in_a_sibling_repository_through_the_shell() {
    need_git!();
    let e = Env::new();
    // The sibling: `<root>/project-stack`, next to `<root>/project`.
    let sibling = e.root.join("project-stack");
    std::fs::create_dir_all(&sibling).unwrap();
    e.git_in(&sibling, &["init", "-q", "-b", "main"]);
    std::fs::write(sibling.join("f"), "one\n").unwrap();
    e.git_in(&sibling, &["add", "f"]);
    e.git_in(&sibling, &["commit", "-q", "--no-gpg-sign", "-m", "first"]);
    std::fs::write(sibling.join("f"), "two\n").unwrap();
    e.write("a", "a\n");
    e.commit("a");
    let s = sibling.display().to_string();
    let commands = [
        format!("git -C {s} commit -am x"),
        format!("git -C {s} checkout -b y"),
        format!("rm {s}/f"),
        format!("perl -i -pe 's/two/three/' {s}/f"),
        format!("cp a {s}/b"),
        // The same reached by a relative path.
        "git -C ../project-stack commit -am x".to_string(),
        "rm ../project-stack/f".to_string(),
    ];
    let work = M::tools().and(M::prompt("tidy the stack"));
    let mut script = Script::new().titled("Tidy the stack");
    for (n, c) in commands.iter().enumerate() {
        script = script.then(
            &format!("step{n}"),
            work.clone().and(M::step(n)),
            Reply::call("shell", json!({"command": c, "description": "work"})),
        );
    }
    let script = script.then(
        "done",
        work.and(M::step(commands.len())),
        Reply::text("I could not touch the sibling."),
    );
    let m = Model::start(script).await;
    e.config(&m.url, json!({}));
    let before = tree(&sibling);

    let out = e.run(&["run", "--format", "json", "tidy the stack"]).await;
    out.ok();
    let results: Vec<Value> = out
        .events()
        .into_iter()
        .filter(|e| e["type"] == "tool_result")
        .collect();
    assert_eq!(results.len(), commands.len(), "{}", out.both());
    for (r, c) in results.iter().zip(&commands) {
        assert_eq!(r["is_error"], true, "{c}: {r}");
        let text = r["output"].as_str().unwrap();
        assert!(
            text.contains("outside the project") || text.contains("external_directory"),
            "{c}: {text}"
        );
    }
    assert_eq!(tree(&sibling), before, "the sibling changed");
    assert_eq!(std::fs::read_to_string(sibling.join("f")).unwrap(), "two\n");
    assert!(!sibling.join("b").exists());
    // The model saw why each one was refused.
    let seen = model::results(&m.by("done")[0]);
    assert_eq!(seen.len(), commands.len());
    for s in &seen {
        assert!(s.contains("outside the project"), "{s}");
    }
    assert_eq!(out.text(), "I could not touch the sibling.");
    m.assert_done();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_run_still_works_inside_its_own_project() {
    need_git!();
    let e = Env::new();
    e.write("f", "two\n");
    e.write("a", "a\n");
    e.commit("files");
    let commands = [
        "git -C . checkout -q -b y",
        "perl -i -pe 's/two/three/' f",
        "cp a b",
        "git -C . add -A",
        "git -C . commit -q --no-gpg-sign -m x",
        "rm b",
    ];
    let work = M::tools().and(M::prompt("tidy"));
    let mut script = Script::new().titled("Tidy");
    for (n, c) in commands.iter().enumerate() {
        script = script.then(
            &format!("step{n}"),
            work.clone().and(M::step(n)),
            Reply::call("shell", json!({"command": c, "description": "work"})),
        );
    }
    let script = script.then(
        "done",
        work.and(M::step(commands.len())),
        Reply::text("Done."),
    );
    let m = Model::start(script).await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "--format", "json", "tidy"]).await;
    out.ok();
    for r in out
        .events()
        .into_iter()
        .filter(|e| e["type"] == "tool_result")
    {
        assert_eq!(r["is_error"], false, "{r}");
    }
    assert_eq!(e.read("f"), "three\n");
    assert!(!e.path("b").exists());
    assert_eq!(e.git(&["rev-parse", "--abbrev-ref", "HEAD"]), "y");
    m.assert_done();
}
