//! A port of OpenCode's `file-access.test.ts` (MIT) from the porting
//! backlog: one `external_directory` request for several paths outside the
//! project. Its own test binary, with `HOME` and the data folder set to
//! temporary folders before it runs.

use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{Asker, Effect, Gate, Reply, Request, Rule};
use codesmith_core::tool::Ctx;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recorder(Mutex<Vec<Request>>);

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.0.lock().unwrap().push(req.clone());
        Reply::Once
    }
}

fn ctx<'a>(root: &Path, gate: &'a Gate) -> Ctx<'a> {
    Ctx {
        cwd: root.to_path_buf(),
        root: root.to_path_buf(),
        gate,
        shell: "/bin/sh".into(),
        session_id: "test".into(),
        limits: ToolOutputConfig::default(),
        client: reqwest::Client::new(),
        jobs: None,
        websearch: None,
        format: None,
        lsp: None,
        progress: None,
    }
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

// file-access.test.ts: "batches external resources in first-seen order and
// preserves broader repository saves" (adapted: the folders the paths are
// in, as the shell tool batches them; no request metadata)
#[test]
fn oc_file_access_batches_external_resources_in_first_seen_order() {
    let base = tempfile::tempdir().unwrap();
    let base_path = base.path().canonicalize().unwrap();
    std::env::set_var("HOME", base_path.join("home"));
    std::env::set_var("XDG_DATA_HOME", base_path.join("data"));
    let project = base_path.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let external = tempfile::tempdir().unwrap();
    let ext = external.path().canonicalize().unwrap();
    let (git, hg) = (ext.join("git"), ext.join("hg"));
    std::fs::create_dir_all(git.join(".git")).unwrap();
    std::fs::create_dir_all(git.join("nested")).unwrap();
    std::fs::create_dir_all(hg.join(".hg")).unwrap();
    std::fs::create_dir_all(hg.join("nested")).unwrap();
    let rec = Arc::new(Recorder::default());
    let gate = Gate::new(vec![Rule::new("read", "*", Effect::Allow)], rec.clone());
    let c = ctx(&project, &gate);
    let first: PathBuf = git.join("nested");
    let dirs = [
        first.clone(),
        project.clone(),
        git.join("nested"),
        hg.join("nested"),
        first,
    ];
    c.authorize_external_dirs(&dirs, codesmith_core::tool::Access::Read)
        .unwrap();
    {
        let asked = rec.0.lock().unwrap();
        assert_eq!(asked.len(), 1, "one request for every folder");
        assert_eq!(asked[0].action, "external_directory");
        assert_eq!(
            asked[0].resources,
            [
                format!("{}/*", s(&git.join("nested"))),
                format!("{}/*", s(&hg.join("nested")))
            ]
        );
        // "Always" would save each enclosing repository.
        assert_eq!(
            asked[0].save,
            [format!("{}/*", s(&git)), format!("{}/*", s(&hg))]
        );
    }
    // Nothing outside: no request.
    c.authorize_external_dirs(
        std::slice::from_ref(&project),
        codesmith_core::tool::Access::Read,
    )
    .unwrap();
    assert_eq!(rec.0.lock().unwrap().len(), 1);
    // Approved once, so asked again.
    c.authorize_external_dirs(&[git.join("nested")], codesmith_core::tool::Access::Read)
        .unwrap();
    let asked = rec.0.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert_eq!(
        asked[1].resources,
        [format!("{}/*", s(&git.join("nested")))]
    );
}
