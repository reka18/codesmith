//! Which shell runs commands (after OpenCode's `shell/select.ts`): the
//! configured `shell`, else `$SHELL`, else the platform's default (Zsh on
//! macOS; Bash on `PATH`, else `/bin/sh`, elsewhere). A shell that cannot be
//! found falls back, and so do the terminal-only shells (Fish, Nushell) when
//! a command needs a POSIX-compatible one, as the `shell` tool does.
//! Terminals take any shell. `$SHELL` is read afresh each time, so a
//! removed shell is not kept.

use std::path::Path;

/// What a command needs of its shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Any shell the user chose (terminals).
    Config,
    /// One that runs POSIX `-c` scripts (the `shell` tool).
    Compat,
}

/// A shell's name: its file name, lower-cased, without `.exe`.
pub fn name(file: &str) -> String {
    let base = Path::new(file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file)
        .to_ascii_lowercase();
    match base.strip_suffix(".exe") {
        Some(b) => b.to_string(),
        None => base,
    }
}

/// Whether a shell takes `-l` to start as a login shell.
pub fn login(file: &str) -> bool {
    matches!(
        name(file).as_str(),
        "bash" | "dash" | "fish" | "ksh" | "sh" | "zsh"
    )
}

/// Whether a shell runs POSIX `-c` scripts (Fish and Nushell do not).
pub fn compatible(file: &str) -> bool {
    !matches!(name(file).as_str(), "fish" | "nu")
}

/// `file` as a shell that exists: an absolute path that is a file, or a
/// bare name found on `PATH`.
pub fn executable(file: &str) -> Option<String> {
    let p = Path::new(file);
    if p.is_absolute() {
        return p.is_file().then(|| file.to_string());
    }
    if file.contains('/') {
        return None;
    }
    crate::format::which(file).map(|p| p.display().to_string())
}

/// The platform's default shell.
pub fn fallback() -> String {
    if cfg!(target_os = "macos") {
        return "/bin/zsh".into();
    }
    crate::format::which("bash")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/bin/sh".into())
}

fn select(file: Option<&str>, priority: Priority) -> String {
    if let Some(f) = file.filter(|f| !f.is_empty()) {
        if priority == Priority::Config || compatible(f) {
            if let Some(s) = executable(f) {
                return s;
            }
        }
    }
    fallback()
}

/// The shell `$SHELL` names, if it can be used, else the fallback.
pub fn environment(priority: Priority) -> String {
    select(std::env::var("SHELL").ok().as_deref(), priority)
}

/// The shell for `priority`: the configured one if it can be used, else
/// [`environment`]'s.
pub fn resolve(priority: Priority, configured: Option<&str>) -> String {
    match configured.filter(|c| !c.is_empty()) {
        Some(c) => select(Some(c), priority),
        None => environment(priority),
    }
}
