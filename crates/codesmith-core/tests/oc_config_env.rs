//! Port of `packages/core/test/config/config.test.ts`'s "excludes claude
//! directories when claude is disabled" (MIT). One test, as it sets the
//! process's environment (this file is a test binary of its own).

use codesmith_core::skill;

/// The Claude Code folders are skill roots unless
/// `CODESMITH_DISABLE_CLAUDE_CODE` is set; the `.agents` folders stay.
#[test]
fn oc_config_excludes_claude_directories_when_claude_is_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let (global, home, project) = (
        tmp.path().join("global"),
        tmp.path().join("home"),
        tmp.path().join("project"),
    );
    std::fs::create_dir_all(&project).unwrap();
    let claude = |roots: &[std::path::PathBuf]| {
        roots
            .iter()
            .filter(|r| r.to_string_lossy().contains(".claude"))
            .cloned()
            .collect::<Vec<_>>()
    };
    std::env::remove_var("CODESMITH_DISABLE_CLAUDE_CODE");
    std::env::remove_var("CODESMITH_DISABLE_CLAUDE_CODE_SKILLS");
    let on = skill::roots(&project, &project, &global, &home, &[]);
    assert_eq!(
        claude(&on),
        [home.join(".claude/skills"), project.join(".claude/skills")]
    );
    std::env::set_var("CODESMITH_DISABLE_CLAUDE_CODE", "1");
    let off = skill::roots(&project, &project, &global, &home, &[]);
    std::env::remove_var("CODESMITH_DISABLE_CLAUDE_CODE");
    assert!(claude(&off).is_empty(), "{off:?}");
    for dir in [home.join(".agents/skills"), project.join(".agents/skills")] {
        assert!(off.contains(&dir), "{dir:?}");
    }
}
