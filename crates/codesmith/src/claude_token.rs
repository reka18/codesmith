//! Signing Claude Code in with a token from `claude setup-token`, on the
//! user's terminal: CodeSmith runs it (the browser approval is Claude
//! Code's own), takes the token it prints, and saves it in `auth.json`.
//! See [`codesmith_ai::claude_agent::token`].

use anyhow::{anyhow, Context, Result};
use std::io::Read;
use std::path::Path;

/// What is shown before `claude setup-token` starts.
pub const EXPLAIN: &str = "\
Create a Claude Code token

Claude Code now makes a token for your Claude subscription. Approve it in
the browser page it opens (or open the link it shows). CodeSmith saves the
token so Claude Code signs in with it, also over SSH, with no password.

  * The token lasts a year and can only send requests to Claude.
  * It is saved in CodeSmith's auth file, readable only by you.
  * Remove it any time: /login, Claude Pro/Max, Sign out.
";

/// The most of the session record that is read.
const RECORD_LIMIT: u64 = 4 * 1024 * 1024;

/// Run `claude setup-token` on this terminal (not in raw mode) and return
/// the token it printed: `None` when it printed none (cancelled, failed,
/// or a layout CodeSmith could not read, so the user pastes it).
pub fn create_on_terminal() -> Result<Option<String>> {
    use codesmith_ai::claude_agent::{process_env, token, which};
    let claude = which(&process_env()).ok_or_else(|| {
        anyhow!(
            "Claude Code isn't installed: install it, then try again ({})",
            codesmith_ai::claude_agent::catalog::SETUP_URL
        )
    })?;
    // In a new owner-only folder: the record holds the token until it is
    // read, and is removed with the folder right after.
    let dir = codesmith_core::paths::new_private_temp_dir("codesmith-token-")
        .context("creating a folder for Claude Code's output")?;
    let record = dir.join("session");
    let result = (|| {
        let mut cmd = token::command(&claude, &record).ok_or_else(|| {
            anyhow!("this system can't run claude setup-token for you; paste a token instead")
        })?;
        eprintln!("\n{EXPLAIN}");
        run_interruptible(&mut cmd)
            .with_context(|| format!("running {} setup-token", claude.display()))?;
        read_record(&record)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    Ok(result?.as_deref().and_then(token::parse))
}

fn read_record(path: &Path) -> Result<Option<String>> {
    let Ok(f) = std::fs::File::open(path) else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    f.take(RECORD_LIMIT).read_to_end(&mut bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Run `cmd` on this terminal. Ctrl-C ends it, not CodeSmith: CodeSmith
/// ignores the interrupt meanwhile, and the child takes the default back.
pub(crate) fn run_interruptible(
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only an async-signal-safe call between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        // SAFETY: swapping this process's SIGINT disposition and back.
        let before = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let status = cmd.status();
        unsafe {
            libc::signal(libc::SIGINT, before);
        }
        status
    }
    #[cfg(not(unix))]
    {
        cmd.status()
    }
}

/// Save `token` (checked) as Claude Code's credential, and say so.
pub fn save(file: &Path, token: &str) -> Result<String> {
    let now = codesmith_core::providers::oauth::now_ms();
    let t = codesmith_core::providers::claude_agent::save_token_in(file, token, now)?;
    Ok(saved_message(&t))
}

/// What is said once a token is saved.
pub fn saved_message(t: &codesmith_core::providers::claude_agent::Token) -> String {
    let until = chrono::DateTime::from_timestamp_millis(
        codesmith_ai::claude_agent::token::expires(t.created) as i64,
    )
    .map(|d| d.format("%Y-%m-%d").to_string())
    .unwrap_or_else(|| "a year from now".into());
    format!(
        "Saved your Claude Code token (valid until about {until}). Claude Code signs in with it from now on, also over SSH, with no password."
    )
}

/// A warning when the saved token expires within two weeks (or has).
pub fn expiry_warning() -> Option<String> {
    use codesmith_ai::claude_agent::token;
    let t = codesmith_core::providers::claude_agent::token_in(&codesmith_core::paths::auth_file())?;
    let now = codesmith_core::providers::oauth::now_ms();
    if t.created == 0 || !token::expiring(t.created, now) {
        return None;
    }
    let when = chrono::DateTime::from_timestamp_millis(token::expires(t.created) as i64)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    let verb = if token::expires(t.created) <= now {
        "expired"
    } else {
        "expires"
    };
    Some(format!(
        "Your Claude Code token {verb} on {when}. Make a new one: /login, then Claude Pro/Max, then Claude Code token."
    ))
}

/// Restart Claude Code's processes, so the next turn starts with the
/// credential just saved or removed.
pub async fn restart_claude_code() {
    codesmith_ai::claude_agent::global().close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str =
        "sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-xyzAA";

    #[test]
    fn a_saved_token_is_stored_for_claude_code_and_described() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        let msg = save(&file, &format!(" {TOKEN}\n")).unwrap();
        assert!(msg.contains("valid until about"), "{msg}");
        assert!(!msg.contains(TOKEN), "the token is never shown: {msg}");
        let kept = codesmith_core::providers::claude_agent::token_in(&file).unwrap();
        assert_eq!(kept.token, TOKEN);
        assert!(kept.created > 0);
        // Not an API key: nothing else reads it as one.
        assert_eq!(codesmith_core::auth::key_in(&file, "claude-agent"), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "only the owner reads it");
        }
    }

    #[test]
    fn a_bad_token_is_refused_and_nothing_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        assert!(save(&file, "sk-proj-not-a-claude-token").is_err());
        assert!(codesmith_core::providers::claude_agent::token_in(&file).is_none());
    }

    #[test]
    fn a_missing_record_reads_as_no_token() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_record(&dir.path().join("none")).unwrap(), None);
    }

    #[test]
    fn a_huge_record_is_read_only_up_to_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big");
        std::fs::write(&p, vec![b'x'; (RECORD_LIMIT as usize) + 4096]).unwrap();
        assert_eq!(
            read_record(&p).unwrap().unwrap().len(),
            RECORD_LIMIT as usize
        );
    }
}
