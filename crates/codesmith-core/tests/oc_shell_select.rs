//! Ports of OpenCode's shell selection tests (`shell.test.ts`).

use codesmith_core::shell_select::{self as shell, Priority};

#[test]
fn oc_shell_normalizes_shell_names() {
    assert_eq!(shell::name("/bin/bash"), "bash");
    assert_eq!(shell::name("C:/tools/NU.EXE"), "nu");
    assert_eq!(shell::name("C:/tools/PWSH.EXE"), "pwsh");
}

#[test]
fn oc_shell_detects_login_shells() {
    assert!(shell::login("/bin/bash"));
    assert!(!shell::login("C:/tools/pwsh.exe"));
    assert!(shell::login("zsh") && shell::login("/usr/bin/fish"));
    assert!(!shell::login("/usr/bin/env"));
}

#[test]
fn oc_shell_falls_back_when_configured_shell_cannot_be_resolved() {
    for p in [Priority::Config, Priority::Compat] {
        // OpenCode compares with the default for an unset $SHELL: the
        // fallback.
        assert_eq!(
            shell::resolve(p, Some("codesmith-missing-shell")),
            shell::fallback(),
            "{p:?}"
        );
    }
    assert_eq!(
        shell::resolve(Priority::Config, Some("/no/such/shell")),
        shell::fallback()
    );
}

#[test]
fn oc_shell_falls_back_for_terminal_only_shells_when_compatibility_is_required() {
    assert_ne!(
        shell::name(&shell::resolve(Priority::Compat, Some("fish"))),
        "fish"
    );
    assert_ne!(
        shell::name(&shell::resolve(Priority::Compat, Some("nu"))),
        "nu"
    );
    assert!(!shell::compatible("/opt/homebrew/bin/fish"));
}

#[test]
fn oc_shell_rejects_blacklisted_shells_case_insensitively() {
    assert!(!shell::compatible("FISH"));
    assert!(!shell::compatible("C:/tools/NU.EXE"));
    assert_ne!(
        shell::name(&shell::resolve(
            Priority::Compat,
            Some("/usr/local/bin/FISH")
        )),
        "fish"
    );
}

#[cfg(unix)]
#[test]
fn oc_shell_resolves_the_environment_shell_without_retaining_a_removed_executable() {
    // The only test here that changes $SHELL.
    let d = tempfile::tempdir().unwrap();
    let prev = std::env::var_os("SHELL");
    std::env::remove_var("SHELL");
    let fallback = shell::environment(Priority::Config);
    assert_eq!(fallback, shell::fallback());
    let preferred = d.path().join("preferred-shell");
    std::os::unix::fs::symlink("/bin/sh", &preferred).unwrap();
    let preferred = preferred.display().to_string();
    std::env::set_var("SHELL", &preferred);
    assert_eq!(shell::environment(Priority::Config), preferred);
    assert_eq!(shell::resolve(Priority::Config, None), preferred);
    std::fs::remove_file(&preferred).unwrap();
    assert_eq!(shell::environment(Priority::Config), fallback);
    match prev {
        Some(v) => std::env::set_var("SHELL", v),
        None => std::env::remove_var("SHELL"),
    }
}
