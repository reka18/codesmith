//! The `codesmith` tool (OpenCode's `opencode` tools), ported from
//! `tool-opencode.test.ts` and `tool-image-generate.test.ts` (MIT): its
//! model listing by type, and the project's worktrees. OpenCode calls them
//! through Code Mode as `tools.opencode.*`; CodeSmith offers one tool whose
//! `action` picks the operation. See `crates/codesmith-core/PARITY.md`.

mod media_harness;

use codesmith_core::plugin::codesmith::{run, Check, Permit};
use codesmith_core::plugin::ToolContext;
use media_harness::*;
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn ctx(config: codesmith_core::config::Config, dir: &Path, model: &str) -> ToolContext {
    ToolContext {
        cwd: dir.to_path_buf(),
        root: dir.to_path_buf(),
        session_id: "ses_codesmith_tool".into(),
        model: model.into(),
        config: Arc::new(config),
    }
}

/// A permission check: action, resources and what "always" saves.
type Seen = (String, Vec<String>, Vec<String>);

/// Records each permission check; answers with `reject` when set.
#[derive(Default)]
struct Approvals {
    seen: Mutex<Vec<Seen>>,
    reject: Mutex<Option<String>>,
}

fn check(a: &Arc<Approvals>) -> Arc<Check> {
    let a = a.clone();
    Arc::new(move |p: Permit| {
        a.seen
            .lock()
            .unwrap()
            .push((p.action.clone(), p.resources.clone(), p.save.clone()));
        let r = a.reject.lock().unwrap().clone();
        Box::pin(async move { r.map_or(Ok(()), Err) })
    })
}

fn ids(v: &Value) -> Vec<String> {
    v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["models"].as_array().unwrap().clone())
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

// tool-image-generate.test.ts: "lists image models through the models tool type filter"
#[tokio::test(flavor = "multi_thread")]
async fn oc_codesmith_lists_image_models_through_the_models_type_filter() {
    let base = "http://127.0.0.1:9";
    let painter = model(
        "painter",
        "image",
        "@opencode/ai/providers/openai/images",
        &format!("{base}/v1"),
        &["text"],
        20,
    );
    let chat = model(
        "chat",
        "chat",
        "@opencode/ai/providers/openai-compatible",
        &format!("{base}/v1"),
        &["text"],
        10,
    );
    let config = config(
        vec![provider("test", "", vec![painter, chat])],
        &["test"],
        json!({}),
    );
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(config, dir.path(), "test/chat");
    let a = Arc::new(Approvals::default());
    let models = |input: Value| {
        let c = &c;
        let k = check(&a);
        async move { ids(&run(c, &input, k.as_ref()).await.unwrap()) }
    };
    assert_eq!(
        models(json!({"action": "models", "type": "image"})).await,
        ["test/painter"]
    );
    let chat = models(json!({"action": "models"})).await;
    assert!(!chat.contains(&"test/painter".to_string()), "{chat:?}");
    assert!(chat.contains(&"test/chat".to_string()), "{chat:?}");
    assert!(a.seen.lock().unwrap().is_empty(), "listing asks nothing");
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

// tool-opencode.test.ts: "creates, lists, and removes worktrees for the plugin's project"
#[tokio::test(flavor = "multi_thread")]
async fn oc_codesmith_creates_lists_and_removes_worktrees() {
    isolate();
    let repo = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let r = repo.path().canonicalize().unwrap();
    for a in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "t@t"],
        &["config", "user.name", "t"],
        &["config", "commit.gpgsign", "false"],
    ] {
        git(&r, a);
    }
    std::fs::write(r.join("a.txt"), "a").unwrap();
    git(&r, &["add", "."]);
    git(&r, &["commit", "-q", "-m", "first"]);
    let config: codesmith_core::config::Config =
        serde_json::from_value(json!({"worktree": {"directory": parent.path()}})).unwrap();
    let c = ctx(config, &r, "test/alpha");
    let a = Arc::new(Approvals::default());
    let k = check(&a);
    let call = |input: Value| {
        let (c, k) = (&c, k.clone());
        async move { run(c, &input, k.as_ref()).await }
    };

    let created = call(json!({"action": "worktree_create", "name": " task "}))
        .await
        .unwrap();
    let directory = created["directory"].as_str().unwrap().to_string();
    assert!(directory.ends_with("/task"), "{directory}");
    assert!(Path::new(&directory).is_dir());
    let listed = call(json!({"action": "worktree_list"})).await.unwrap();
    assert!(
        listed["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["directory"] == directory.as_str() && w["main"] == false),
        "{listed}"
    );

    // The project checkout is not a removable worktree.
    let main = r.display().to_string();
    assert_eq!(
        call(json!({"action": "worktree_remove", "directory": main}))
            .await
            .unwrap_err(),
        format!("Unable to remove {main}: it is not a worktree of this project. Use worktree_list to see the project's worktrees.")
    );
    assert!(r.is_dir());

    // A dirty worktree is kept, and forcing is left to the model.
    std::fs::write(Path::new(&directory).join("draft.txt"), "draft").unwrap();
    assert_eq!(
        call(json!({"action": "worktree_remove", "directory": directory}))
            .await
            .unwrap_err(),
        format!("Unable to remove worktree {directory}: it has modified or untracked files.")
    );
    assert!(Path::new(&directory).join("draft.txt").exists());
    std::fs::remove_file(Path::new(&directory).join("draft.txt")).unwrap();

    // Removal needs the approval editing files there needs; the user's
    // correction is what the model sees.
    a.seen.lock().unwrap().clear();
    *a.reject.lock().unwrap() = Some("Keep this worktree".into());
    assert_eq!(
        call(json!({"action": "worktree_remove", "directory": directory}))
            .await
            .unwrap_err(),
        "Keep this worktree"
    );
    assert!(Path::new(&directory).join(".git").exists());
    *a.reject.lock().unwrap() = None;

    // A relative path resolves from the working directory.
    let rel = pathdiff(&r, Path::new(&directory));
    assert_eq!(
        call(json!({"action": "worktree_remove", "directory": rel}))
            .await
            .unwrap(),
        json!({"directory": directory, "removed": true})
    );
    assert!(!Path::new(&directory).exists());
    let seen = a.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            (
                "edit".to_string(),
                vec![directory.clone()],
                vec!["*".to_string()]
            );
            2
        ]
    );
    let listed = call(json!({"action": "worktree_list"})).await.unwrap();
    assert!(!listed.to_string().contains(&directory));
}

/// `to` relative to `from` (both absolute).
fn pathdiff(from: &Path, to: &Path) -> String {
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = to.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut out = std::path::PathBuf::new();
    for _ in common..f.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c);
    }
    out.display().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn codesmith_tool_rejects_unknown_actions_and_types() {
    let dir = tempfile::tempdir().unwrap();
    let c = ctx(Default::default(), dir.path(), "x/y");
    let a = Arc::new(Approvals::default());
    let k = check(&a);
    assert!(run(&c, &json!({}), k.as_ref())
        .await
        .unwrap_err()
        .contains("needs an action"));
    assert!(run(&c, &json!({"action": "set_model"}), k.as_ref())
        .await
        .unwrap_err()
        .contains("Unknown action"));
    assert!(run(
        &c,
        &json!({"action": "models", "type": "audio"}),
        k.as_ref()
    )
    .await
    .unwrap_err()
    .contains("Unknown model type"));
}
