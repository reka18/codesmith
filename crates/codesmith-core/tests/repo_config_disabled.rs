//! `$CODESMITH_DISABLE_PROJECT_CONFIG` turns off what is still read from a
//! repository (its instructions files, skills and commands) but not your
//! per-repo files in the global folder. It sets the variable, so it has a
//! test binary of its own.

use codesmith_core::config::{self, Sources};
use serde_json::json;
use std::path::Path;

fn write(p: &Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[test]
fn the_opt_out_turns_off_repo_instructions_skills_and_commands_but_not_per_repo_files() {
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let (home, global, repo) = (base.join("home"), base.join("global"), base.join("repo"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    write(&repo.join("AGENTS.md"), "repo instructions");
    write(
        &repo.join(".codesmith/commands/repo-cmd.md"),
        "From the repo",
    );
    write(&repo.join(".claude/commands/claude-cmd.md"), "From .claude");
    for dir in [".codesmith/skills", ".agents/skills", ".claude/skills"] {
        write(
            &repo.join(dir).join("s/SKILL.md"),
            "---\nname: s\ndescription: a repo skill\n---\nS",
        );
    }
    write(
        &global.join("projects/repo.json"),
        &json!({"path": repo, "model": "per/repo", "commands": {"mine": {"template": "Mine"}}})
            .to_string(),
    );
    write(
        &global.join("projects/repo/commands/folder-cmd.md"),
        "Folder",
    );

    // As `Sources::from_env` builds them, with the global folder here.
    let sources = |project| Sources {
        global_dir: global.clone(),
        file: None,
        content: None,
        project,
        per_repo: true,
    };
    let skills = || codesmith_core::skill::roots(&repo, &repo, &global, &home, &[]);
    let commands = || codesmith_core::command::roots(&repo, &repo, &global, &home);
    let instructions =
        || codesmith_core::instructions::discover_in(&repo, &repo, &global, &home, &[]);

    // On: everything the repo still offers loads, and the per-repo file.
    assert!(!config::project_disabled());
    let on = config::load_from(&repo, &sources(!config::project_disabled())).unwrap();
    assert!(on.config.commands.contains_key("repo-cmd"));
    assert!(on.config.commands.contains_key("mine"));
    assert!(on.config.commands.contains_key("folder-cmd"));
    assert_eq!(on.config.model.as_deref(), Some("per/repo"));
    assert!(skills().iter().any(|p| p.starts_with(&repo)));
    assert!(commands().iter().any(|p| p.starts_with(&repo)));
    assert_eq!(instructions(), [repo.join("AGENTS.md")]);

    // Off.
    std::env::set_var("CODESMITH_DISABLE_PROJECT_CONFIG", "1");
    assert!(config::project_disabled());
    let off = config::load_from(&repo, &sources(!config::project_disabled())).unwrap();
    let skill_roots = skills();
    let command_roots = commands();
    let instruction_files = instructions();
    std::env::remove_var("CODESMITH_DISABLE_PROJECT_CONFIG");
    assert!(!off.config.commands.contains_key("repo-cmd"));
    assert!(
        skill_roots.iter().all(|p| !p.starts_with(&repo)),
        "{skill_roots:?}"
    );
    assert!(
        command_roots.iter().all(|p| !p.starts_with(&repo)),
        "{command_roots:?}"
    );
    assert!(instruction_files.is_empty(), "{instruction_files:?}");
    // Your per-repo file and its folder still apply.
    assert_eq!(off.config.model.as_deref(), Some("per/repo"));
    assert!(off.config.commands.contains_key("mine"));
    assert!(off.config.commands.contains_key("folder-cmd"));
    assert_eq!(off.repo.len(), 1);
}
