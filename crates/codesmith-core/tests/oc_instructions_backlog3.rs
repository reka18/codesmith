//! A port of OpenCode's `instruction-discovery.test.ts` (MIT) that sets
//! `CODESMITH_DISABLE_PROJECT_CONFIG`, so it has a test binary of its own.

use codesmith_core::instructions::discover_in;
use std::path::Path;
use std::sync::Mutex;

/// The tests here set and read `CODESMITH_DISABLE_PROJECT_CONFIG`.
static ENV: Mutex<()> = Mutex::new(());

fn write(p: &Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

// instruction-discovery.test.ts: "canonicalizes boundaries and honors
// project opt-out" (adapted: the files found, not the folder walks asked
// for; the opt-out is `$CODESMITH_DISABLE_PROJECT_CONFIG`, as OpenCode's is
// `OPENCODE_DISABLE_PROJECT_CONFIG`)
#[test]
fn oc_instructions_canonicalize_boundaries_and_honor_project_opt_out() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let (home, global) = (base.join("home"), base.join("global"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&global).unwrap();
    let repo = base.join("repo");
    write(&repo.join("AGENTS.md"), "repo");
    write(&base.join("AGENTS.md"), "above the project");
    let outside = base.join("outside");
    write(&outside.join("AGENTS.md"), "outside");

    // `/repo/` is the project `/repo`: its file, and nothing above it.
    let slashed = Path::new(&format!("{}/", repo.display())).to_path_buf();
    assert_eq!(
        discover_in(&slashed, &repo, &global, &home, &[]),
        [repo.join("AGENTS.md")]
    );
    // A folder outside the project is not walked.
    assert_eq!(
        discover_in(&outside, &repo, &global, &home, &[]),
        [] as [std::path::PathBuf; 0]
    );
    // Project config turned off: no project file is read; the global one is.
    write(&global.join("AGENTS.md"), "global");
    std::env::set_var("CODESMITH_DISABLE_PROJECT_CONFIG", "1");
    let off = discover_in(&repo, &repo, &global, &home, &[]);
    std::env::remove_var("CODESMITH_DISABLE_PROJECT_CONFIG");
    assert_eq!(off, [global.join("AGENTS.md")]);
    assert_eq!(
        discover_in(&repo, &repo, &global, &home, &[]),
        [global.join("AGENTS.md"), repo.join("AGENTS.md")]
    );
}

// Regression for the opt-out: the project's files, walked from a nested
// folder too, are left out whatever the variable is set to (as project
// config is), and the configured files still load.
#[test]
fn oc_instructions_project_opt_out_skips_every_project_file() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let (home, global) = (base.join("home"), base.join("global"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&global).unwrap();
    let repo = base.join("repo");
    let nested = repo.join("pkg/ünï cødé");
    write(&repo.join("AGENTS.md"), "repo");
    write(&nested.join("CLAUDE.md"), "nested");
    write(&repo.join("docs/extra.md"), "configured");
    let configured = [repo.join("docs/extra.md").display().to_string()];
    for value in ["1", "", "0"] {
        std::env::set_var("CODESMITH_DISABLE_PROJECT_CONFIG", value);
        let found = discover_in(&nested, &repo, &global, &home, &configured);
        std::env::remove_var("CODESMITH_DISABLE_PROJECT_CONFIG");
        assert_eq!(found, [repo.join("docs/extra.md")], "{value:?}");
    }
    assert_eq!(
        discover_in(&nested, &repo, &global, &home, &configured),
        [
            nested.join("CLAUDE.md"),
            repo.join("AGENTS.md"),
            repo.join("docs/extra.md")
        ]
    );
}

// Security review 8, finding 3: nested instructions (`nearby`) are project
// files too, so turning project config off gives none, whatever the
// variable is set to, for a plain discovery and after a read step alike.
#[test]
fn nested_instructions_honor_the_project_opt_out() {
    use codesmith_ai::{Message, Part, Role};
    use codesmith_core::nearby;
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let d = tempfile::tempdir().unwrap();
    let repo = d.path().canonicalize().unwrap().join("repo");
    let nested = repo.join("pkg/ünï cødé");
    write(&nested.join("AGENTS.md"), "nested rules");
    write(&nested.join("file.txt"), "x");
    let file = nested.join("file.txt");
    let all = |_: &[String]| true;
    let call = Message {
        role: Role::Assistant,
        parts: vec![Part::tool_call(
            "c1",
            "read",
            serde_json::json!({"path": file.display().to_string()}),
        )],
    };
    let result = Message {
        role: Role::User,
        parts: vec![Part::ToolResult {
            id: "c1".into(),
            name: "read".into(),
            content: "x".into(),
            is_error: false,
            media: vec![],
        }],
    };
    let step = || nearby::after_step(&repo, &repo, &call, &result, &[], &all);
    // On: the nested file is found and given.
    assert_eq!(
        nearby::discover(&repo, &repo, &file, &all),
        [nested.join("AGENTS.md")]
    );
    assert_eq!(step().len(), 1);
    assert!(step()[0].contains("nested rules"));
    for value in ["1", "", "0", "true"] {
        std::env::set_var("CODESMITH_DISABLE_PROJECT_CONFIG", value);
        let found = nearby::discover(&repo, &repo, &file, &all);
        let given = step();
        std::env::remove_var("CODESMITH_DISABLE_PROJECT_CONFIG");
        assert!(found.is_empty(), "{value:?}: {found:?}");
        assert!(given.is_empty(), "{value:?}: {given:?}");
    }
    // And back on.
    assert_eq!(step().len(), 1);
}
