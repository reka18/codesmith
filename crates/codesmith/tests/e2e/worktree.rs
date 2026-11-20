//! `codesmith run -w`: the turn works in a new git worktree and leaves the
//! main checkout alone.

use crate::harness::Env;
use crate::model::{self, Model, Reply, Script, M};
use crate::need_git;
use serde_json::json;
use std::path::PathBuf;

#[tokio::test(flavor = "multi_thread")]
async fn run_in_a_worktree_writes_only_there() {
    need_git!();
    let e = Env::new();
    e.write("app.txt", "v1\n");
    e.commit("app");
    let ask = M::tools().and(M::prompt("bump the version"));
    let m = Model::start(
        Script::new()
            .titled("Bump")
            .then(
                "edit",
                ask.clone().and(M::step(0)),
                Reply::call(
                    "edit",
                    json!({"path": "app.txt", "oldString": "v1", "newString": "v2"}),
                ),
            )
            .then(
                "new",
                ask.clone().and(M::step(1)),
                Reply::call("write", json!({"path": "CHANGES.md", "content": "v2\n"})),
            )
            .then(
                "where",
                ask.clone().and(M::step(2)),
                Reply::call(
                    "shell",
                    json!({"command": "pwd && git status --short", "description": "where"}),
                ),
            )
            .then("done", ask.and(M::step(3)), Reply::text("Bumped.")),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = e
        .run(&["run", "-w", "bump", "--format", "json", "bump the version"])
        .await;
    out.ok();
    let line = out
        .stderr
        .lines()
        .find(|l| l.contains("working in new worktree"))
        .unwrap_or_else(|| panic!("{}", out.both()));
    let wt = PathBuf::from(
        line.split("working in new worktree ")
            .nth(1)
            .unwrap()
            .trim_end_matches("\x1b[0m")
            .trim(),
    );
    assert!(wt.starts_with(e.data.join("codesmith/worktree")), "{wt:?}");
    assert!(wt.ends_with("bump"), "{wt:?}");

    // The worktree has the changes; the main checkout does not.
    assert_eq!(std::fs::read_to_string(wt.join("app.txt")).unwrap(), "v2\n");
    assert!(wt.join("CHANGES.md").exists());
    assert_eq!(e.read("app.txt"), "v1\n");
    assert!(!e.path("CHANGES.md").exists());
    assert_eq!(e.git(&["status", "--short"]), "");
    // The shell ran there, and the model was told where it works.
    let shell = &model::results(&m.by("done")[0])[2];
    let wt_real = wt.canonicalize().unwrap();
    assert!(shell.contains(&wt_real.display().to_string()), "{shell}");
    assert!(
        shell.contains("M app.txt") && shell.contains("?? CHANGES.md"),
        "{shell}"
    );
    assert!(model::system(&m.by("edit")[0]).contains(&wt_real.display().to_string()));
    // The session belongs to the worktree folder.
    let sid = out.done()["session"].as_str().unwrap().to_string();
    let info = e.store().get(&sid).unwrap().unwrap();
    assert_eq!(
        PathBuf::from(&info.directory).canonicalize().unwrap(),
        wt_real
    );
    assert!(e
        .store()
        .latest(&e.project.display().to_string())
        .unwrap()
        .is_none());

    let list = e.run(&["worktree", "list"]).await;
    list.ok();
    assert!(list.stdout.contains("(main)"), "{}", list.both());
    assert!(list.stdout.contains("bump"), "{}", list.both());
    let rm = e.run(&["worktree", "remove", "bump"]).await;
    assert_eq!(
        rm.code,
        Some(1),
        "uncommitted changes keep it: {}",
        rm.both()
    );
    e.run(&["worktree", "remove", "bump", "--force"]).await.ok();
    assert!(!wt.exists());
    m.assert_done();
}
