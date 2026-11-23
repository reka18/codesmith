//! Unlocking the macOS keychain for Claude Code, without a command to type.
//!
//! Claude Code keeps its login in the login keychain. A desktop session
//! has it unlocked, but an SSH login is a separate security session where
//! it is locked, and only the Mac's password opens it; no program can get
//! around that. So when a Claude Code turn is about to start in such a
//! session, CodeSmith hands the terminal to macOS's own
//! `security unlock-keychain`, which asks for the password itself:
//! CodeSmith never sees, reads or keeps it. Once unlocked, the keychain
//! stays open for that session.

use std::io::Write;

/// What the user is told before macOS asks for the password.
pub const EXPLAIN: &str = "Claude Code keeps its login in your macOS keychain, which is locked in this SSH session.\nType your Mac login password to unlock it. macOS asks for it; CodeSmith never sees it.\nPress Enter on an empty password to skip.";

/// How many passwords are asked for before giving up.
pub const TRIES: usize = 3;

/// Whether a turn on `provider` needs the keychain unlocked first.
pub fn needed(provider: &str, locked: impl FnOnce() -> bool) -> bool {
    provider == codesmith_ai::claude_agent::ID && locked()
}

/// Ask for the password up to [`TRIES`] times with `ask` (one run of
/// `security unlock-keychain`, true when it succeeded), until `locked`
/// says the keychain is open. Whether it is open now.
pub fn unlock_with(
    out: &mut dyn Write,
    mut ask: impl FnMut() -> bool,
    mut locked: impl FnMut() -> bool,
) -> bool {
    let _ = writeln!(out, "\n{EXPLAIN}\n");
    for n in 1..=TRIES {
        let ok = ask();
        if ok && !locked() {
            let _ = writeln!(out, "Keychain unlocked.");
            return true;
        }
        if !ok && n < TRIES {
            let _ = writeln!(out, "That didn't unlock it.");
        }
    }
    let _ = writeln!(
        out,
        "The keychain is still locked; Claude Code can't sign in until it is."
    );
    false
}

/// Run `security unlock-keychain` on this terminal. Ctrl-C ends it, not
/// CodeSmith: CodeSmith ignores the interrupt while it runs, and the child
/// takes the default action back.
#[cfg(target_os = "macos")]
fn ask_macos() -> bool {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new("/usr/bin/security");
    cmd.arg("unlock-keychain");
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    // SAFETY: swapping our own SIGINT disposition and putting it back.
    let before = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let status = cmd.status();
    unsafe {
        libc::signal(libc::SIGINT, before);
    }
    status.map(|s| s.success()).unwrap_or(false)
}

/// Unlock the keychain on this terminal, which must not be in raw mode.
/// Whether it is open now. Never asks anything off macOS.
pub fn unlock() -> bool {
    #[cfg(target_os = "macos")]
    {
        unlock_with(
            &mut std::io::stderr(),
            ask_macos,
            codesmith_ai::claude_agent::keychain_locked,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn only_claude_code_turns_need_the_keychain() {
        assert!(needed("claude-agent", || true));
        assert!(!needed("claude-agent", || false));
        let asked = Cell::new(false);
        assert!(!needed("anthropic", || {
            asked.set(true);
            true
        }));
        assert!(
            !asked.get(),
            "the keychain is not checked for other providers"
        );
    }

    #[test]
    fn the_first_right_password_unlocks() {
        let mut out = Vec::new();
        let tries = Cell::new(0);
        let open = unlock_with(
            &mut out,
            || {
                tries.set(tries.get() + 1);
                true
            },
            || false,
        );
        assert!(open);
        assert_eq!(tries.get(), 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("CodeSmith never sees it"), "{text}");
        assert!(text.contains("Keychain unlocked."), "{text}");
    }

    #[test]
    fn wrong_passwords_are_asked_again_up_to_the_limit() {
        let mut out = Vec::new();
        let tries = Cell::new(0);
        let open = unlock_with(
            &mut out,
            || {
                tries.set(tries.get() + 1);
                tries.get() == 2
            },
            || false,
        );
        assert!(open);
        assert_eq!(tries.get(), 2);

        let mut out = Vec::new();
        let tries = Cell::new(0);
        assert!(!unlock_with(
            &mut out,
            || {
                tries.set(tries.get() + 1);
                false
            },
            || true,
        ));
        assert_eq!(tries.get(), TRIES);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("That didn't unlock it.").count(), TRIES - 1);
        assert!(text.contains("still locked"), "{text}");
    }

    #[test]
    fn success_that_leaves_the_keychain_locked_is_not_trusted() {
        let mut out = Vec::new();
        let tries = Cell::new(0);
        assert!(!unlock_with(
            &mut out,
            || {
                tries.set(tries.get() + 1);
                true
            },
            || true,
        ));
        assert_eq!(tries.get(), TRIES);
    }
}
