//! Which paths are inside the project (OpenCode's `file-access-path.test.ts`,
//! MIT): a project root of `/` or the home folder is no boundary, so paths
//! outside the working directory still need `external_directory`. Its own
//! test binary, with `HOME` (and the data folder in it) set to a temporary
//! folder before any test runs.

use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{Asker, Effect, Gate, Reply, Request, Rule};
use codesmith_core::tool::{self, Ctx};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The fake home folder, set once for the whole binary.
fn home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("codesmith-boundary-{}", std::process::id()));
        let home = base.join("home");
        std::fs::create_dir_all(home.join("work")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(base.join("data")).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_DATA_HOME", base.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", base.join("config"));
        home.canonicalize().unwrap()
    })
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Request>>);

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.0.lock().unwrap().push(req.clone());
        Reply::Once
    }
}

fn ctx<'a>(cwd: &Path, root: &Path, gate: &'a Gate) -> Ctx<'a> {
    Ctx {
        cwd: cwd.to_path_buf(),
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

fn gate() -> (Gate, Arc<Recorder>) {
    let rec = Arc::new(Recorder::default());
    (
        Gate::new(vec![Rule::new("read", "*", Effect::Allow)], rec.clone()),
        rec,
    )
}

/// A project folder outside the fake home.
fn project() -> (tempfile::TempDir, PathBuf) {
    home();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("active")).unwrap();
    (dir, root)
}

// file-access-path.test.ts: "does not treat a filesystem-root project
// sentinel as an internal boundary". The working directory is the boundary
// instead.
#[test]
fn oc_file_access_does_not_treat_a_filesystem_root_project_as_an_internal_boundary() {
    let (_dir, root) = project();
    let (gate, _) = gate();
    let active = root.join("active");
    let c = ctx(&active, Path::new("/"), &gate);
    assert_eq!(c.boundary(), Some(active.clone()));
    assert!(c.resolve("../outside.txt").external);
    assert!(c.resolve("/etc/hosts").external);
    assert!(!c.resolve("inside.txt").external);
}

/// Decided (safer than OpenCode): the home folder is no boundary either, so
/// a project root at `~` (a dotfiles repository, a session started there)
/// does not put `~/.ssh` inside the project.
#[test]
fn oc_file_access_treats_a_home_folder_project_as_no_boundary() {
    let home = home();
    let (gate, _) = gate();
    let work = home.join("work");
    let c = ctx(&work, home, &gate);
    assert_eq!(c.boundary(), Some(work.clone()));
    assert!(c.resolve("~/.ssh/id_ed25519").external);
    assert!(c.resolve("../.ssh/config").external);
    assert!(!c.resolve("notes.txt").external);
}

/// Decided (safer than OpenCode, whose Location `/` contains everything): a
/// session working in `/` or the home folder has no boundary, and every
/// path but the scratch folder needs `external_directory`.
#[test]
fn oc_file_access_gives_a_session_working_in_root_or_home_no_boundary() {
    let home = home();
    let (gate, _) = gate();
    for dir in [Path::new("/"), home] {
        let c = ctx(dir, dir, &gate);
        assert_eq!(c.boundary(), None, "{}", dir.display());
        assert!(c.resolve("/etc/hosts").external);
        assert!(
            c.resolve(&home.join("notes.txt").display().to_string())
                .external
        );
        let scratch = codesmith_core::paths::tmp_dir().join("x.txt");
        assert!(!c.resolve(&scratch.display().to_string()).external);
    }
    // An ordinary project keeps its root as the boundary.
    let (_dir, root) = project();
    let c = ctx(&root.join("active"), &root, &gate);
    assert_eq!(c.boundary(), Some(root.clone()));
    assert!(!c.resolve("../README.md").external);
}

/// Through a tool: a read outside the working directory of a `/`-rooted
/// session asks `external_directory` first.
#[tokio::test(flavor = "multi_thread")]
async fn oc_file_access_asks_before_a_read_outside_the_working_directory_of_a_root_project() {
    let (_dir, root) = project();
    std::fs::write(root.join("outside.txt"), "outside\n").unwrap();
    let (gate, rec) = gate();
    let c = ctx(&root.join("active"), Path::new("/"), &gate);
    let (out, err) = tool::execute(&c, "read", &json!({"path": "../outside.txt"})).await;
    assert!(!err, "{out}");
    let asked = rec.0.lock().unwrap().clone();
    assert_eq!(asked[0].action, "external_directory", "{asked:?}");
    assert_eq!(
        asked[0].resources,
        [root.join("outside.txt").display().to_string()]
    );
}
