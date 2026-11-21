//! Ports of OpenCode's global-source toggle tests (`config/config.test.ts`,
//! `global: false`): `$CODESMITH_DISABLE_GLOBAL_CONFIG` turns off the
//! global config folder and the home folder's `.claude` and `.agents`
//! however they are reached. The tests here are the only ones in this
//! binary that set the variable, one at a time.

use codesmith_core::config::{self, Layer, Sources};
use std::path::Path;
use std::sync::Mutex;

static ENV: Mutex<()> = Mutex::new(());

fn with_global_disabled<T>(off: bool, f: impl FnOnce() -> T) -> T {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    if off {
        std::env::set_var("CODESMITH_DISABLE_GLOBAL_CONFIG", "1");
    } else {
        std::env::remove_var("CODESMITH_DISABLE_GLOBAL_CONFIG");
    }
    let out = f();
    std::env::remove_var("CODESMITH_DISABLE_GLOBAL_CONFIG");
    out
}

fn sources(global: &Path) -> Sources {
    Sources {
        global_dir: global.to_path_buf(),
        file: None,
        content: None,
        project: true,
        trust: codesmith_core::trust::Mode::All,
    }
}

#[test]
fn oc_config_excludes_home_level_claude_and_agents_directories_when_global_is_disabled() {
    let d = tempfile::tempdir().unwrap();
    let t = d.path().canonicalize().unwrap();
    let global = t.join("global");
    let home = global.join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    for sub in [".claude/skills/a", ".agents/skills/b", ".claude/commands"] {
        std::fs::create_dir_all(home.join(sub)).unwrap();
    }
    std::fs::write(
        home.join(".claude/skills/a/SKILL.md"),
        "---\nname: a\ndescription: A\n---\nA",
    )
    .unwrap();
    std::fs::write(
        home.join(".agents/skills/b/SKILL.md"),
        "---\nname: b\ndescription: B\n---\nB",
    )
    .unwrap();
    std::fs::write(home.join("AGENTS.md"), "home rules").unwrap();
    // The project root is the home folder, so the walk reaches it.
    let roots = |off| {
        with_global_disabled(off, || {
            (
                codesmith_core::skill::roots(&project, &home, &global, &home, &[]),
                codesmith_core::command::roots(&project, &home, &global, &home),
                codesmith_core::instructions::discover_in(&project, &home, &global, &home, &[]),
            )
        })
    };
    let home_level = |v: &[std::path::PathBuf]| {
        v.iter()
            .filter(|p| {
                p.starts_with(home.join(".claude"))
                    || p.starts_with(home.join(".agents"))
                    || *p == &home.join("AGENTS.md")
            })
            .count()
    };
    let (skills, commands, instructions) = roots(false);
    // The fixture is real: with global sources on, the walk finds them.
    assert!(home_level(&skills) >= 2, "{skills:?}");
    assert!(home_level(&commands) >= 1);
    assert_eq!(home_level(&instructions), 1);
    let (skills, commands, instructions) = roots(true);
    assert_eq!(home_level(&skills), 0, "{skills:?}");
    assert_eq!(home_level(&commands), 0, "{commands:?}");
    assert_eq!(home_level(&instructions), 0, "{instructions:?}");
}

#[test]
fn oc_config_excludes_global_config_reached_through_the_project_walk_when_global_is_disabled() {
    let d = tempfile::tempdir().unwrap();
    let t = d.path().canonicalize().unwrap();
    let global = t.join("global");
    let project = global.join("plugins").join("demo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        global.join("codesmith.json"),
        r#"{"shell": "global-sentinel"}"#,
    )
    .unwrap();
    // The project root is the global folder's parent, so the walk passes
    // through the global folder.
    std::fs::create_dir(t.join(".git")).unwrap();
    let shell = |off| {
        with_global_disabled(off, || {
            config::load_from(&project, &sources(&global))
                .unwrap()
                .config
                .shell
        })
    };
    assert_eq!(shell(false).as_deref(), Some("global-sentinel"));
    assert_eq!(shell(true), None);
}

#[cfg(unix)]
#[test]
fn oc_config_excludes_missing_files_under_symlinked_global_roots_when_global_is_disabled() {
    let d = tempfile::tempdir().unwrap();
    let t = d.path().canonicalize().unwrap();
    let global = t.join("global");
    let link = t.join("link");
    std::fs::create_dir_all(global.join("plugins").join("demo")).unwrap();
    std::os::unix::fs::symlink(&global, &link).unwrap();
    let project = link.join("plugins").join("demo");
    std::fs::create_dir(link.join(".git")).unwrap();
    let layers = with_global_disabled(true, || config::layers(&project, &link, &sources(&global)));
    assert!(!layers.is_empty());
    let global_level: Vec<&Layer> = layers
        .iter()
        .filter(|l| {
            let p = match l {
                Layer::File(p) => p.parent().unwrap(),
                Layer::Dir(p) => p.as_path(),
            };
            p == link || p == global
        })
        .collect();
    assert!(global_level.is_empty(), "{global_level:?}");
}
