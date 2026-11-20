//! Ports of OpenCode's repository-scoped external approval tests
//! (`file-access-path.test.ts`, `tool-write.test.ts`): "always" for a path
//! outside the project saves its enclosing repository.

use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{Asker, Effect, Gate, Reply, Request, Rule};
use codesmith_core::tool::{self, Ctx};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recorder(Mutex<Vec<Request>>);

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.0.lock().unwrap().push(req.clone());
        Reply::Once
    }
}

struct Env {
    dir: tempfile::TempDir,
    gate: Gate,
    rec: Arc<Recorder>,
}

impl Env {
    fn new() -> Env {
        let rec = Arc::new(Recorder::default());
        Env {
            dir: tempfile::tempdir().unwrap(),
            gate: Gate::new(vec![Rule::new("read", "*", Effect::Allow)], rec.clone()),
            rec,
        }
    }
    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
    async fn call(&self, name: &str, input: serde_json::Value) -> (String, bool) {
        let ctx = Ctx {
            cwd: self.root(),
            root: self.root(),
            gate: &self.gate,
            shell: "/bin/sh".into(),
            session_id: "test".into(),
            limits: ToolOutputConfig::default(),
            client: reqwest::Client::new(),
            jobs: None,
            websearch: None,
            format: None,
            lsp: None,
            progress: None,
        };
        tool::execute(&ctx, name, &input).await
    }
    fn first(&self) -> Request {
        self.rec.0.lock().unwrap()[0].clone()
    }
}

// file-access-path.test.ts

#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_saves_external_approval_at_the_enclosing_repository_on_the_host_filesystem()
{
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let src = outside.join("src");
    std::fs::create_dir(outside.join(".git")).unwrap();
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.txt"), "a\n").unwrap();
    let (out, err) = env
        .call(
            "glob",
            json!({"path": src.display().to_string(), "pattern": "*.txt"}),
        )
        .await;
    assert!(!err, "{out}");
    let req = env.first();
    assert_eq!(req.action, "external_directory");
    assert_eq!(req.resources, [format!("{}/*", src.display())]);
    assert_eq!(req.save, [format!("{}/*", outside.display())]);
}

// tool-write.test.ts

#[tokio::test(flavor = "multi_thread")]
async fn oc_write_saves_external_directory_approval_at_the_nearest_project_directory() {
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let repo = outside.join("repo");
    let nested = repo.join("packages").join("app");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    let target = nested.join("external.txt");
    let (out, err) = env
        .call(
            "write",
            json!({"path": target.display().to_string(), "content": "external"}),
        )
        .await;
    assert!(!err, "{out}");
    let req = env.first();
    assert_eq!(req.action, "external_directory");
    // CodeSmith names the file; OpenCode its folder.
    assert_eq!(req.resources, [target.display().to_string()]);
    assert_eq!(req.save, [format!("{}/*", repo.display())]);
    assert_eq!(
        codesmith_core::permission::always_saves(&req),
        [format!("{}/*", repo.display())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_external_folder_outside_any_repository_saves_itself() {
    let env = Env::new();
    let outside = tempfile::tempdir().unwrap();
    let outside = outside.path().canonicalize().unwrap();
    let target = outside.join("x.txt");
    let (out, err) = env
        .call(
            "write",
            json!({"path": target.display().to_string(), "content": "x"}),
        )
        .await;
    assert!(!err, "{out}");
    assert_eq!(env.first().save, [format!("{}/*", outside.display())]);
}
