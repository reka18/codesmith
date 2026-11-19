//! `shell`: run a command and return its combined output. Ported from
//! OpenCode's shell tool; background commands arrive with sessions (phase 2).

use super::{opt_str, opt_u64, str_arg, Ctx, Outcome, Output};
use crate::permission::Request;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;

fn os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}

pub fn spec(shell: &str) -> (String, Value) {
    let description = [
        "Execute a shell command and return its output.".to_string(),
        format!("Commands run on {} using {shell}.", os()),
        "Quote file paths containing spaces or special characters.".into(),
        "Prefer dedicated tools over shell commands when possible.".into(),
        "When output is large, the full result is saved to a file and a truncated preview is returned.".into(),
        "Rely on automatic truncation unless filtering the output is more useful.".into(),
        "Commands still running when their timeout expires are stopped.".into(),
        "The current session ID is available in the SKEPTICAL_SESSION_ID environment variable.".into(),
    ]
    .join(" ");
    (
        description,
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command string to execute"},
                "workdir": {"type": "string", "description": "Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead."},
                "timeout": {"type": "integer", "minimum": 0, "description": format!("Optional timeout in milliseconds (default {DEFAULT_TIMEOUT_MS}). When it expires, the command is stopped.")}
            },
            "required": ["command"]
        }),
    )
}

/// The rule saved by "always allow": the command's first word, any arguments.
pub fn save_pattern(command: &str) -> String {
    match command.split_whitespace().next() {
        Some(first) => format!("{first} *"),
        None => "*".into(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ending {
    Exited(i32),
    Signaled(i32),
    TimedOut,
}

/// The notice after the output, as OpenCode's `ShellResult.notice` words it.
pub fn notice(ending: &Ending, timeout_ms: u64) -> Option<String> {
    match ending {
        Ending::Exited(0) => None,
        Ending::Exited(code) => Some(format!("Exit code: {code}")),
        Ending::Signaled(sig) => Some(format!("Exit code: {} ({})", 128 + sig, signal_name(*sig))),
        Ending::TimedOut => Some(format!(
            "Command timed out after {} seconds. If it needs longer and isn't waiting for input, rerun with a larger timeout.\nExit code: unknown",
            timeout_ms as f64 / 1000.0
        )),
    }
}

fn signal_name(sig: i32) -> &'static str {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => "signal",
    }
}

pub fn render(output: &str, ending: &Ending, timeout_ms: u64) -> String {
    let body = if output.is_empty() {
        "(no output)"
    } else {
        output.strip_suffix('\n').unwrap_or(output)
    };
    match notice(ending, timeout_ms) {
        Some(n) => format!("{body}\n\n{n}"),
        None => body.to_string(),
    }
}

pub async fn run(ctx: &Ctx<'_>, input: &Value) -> Outcome {
    let command = str_arg(input, "command")?;
    if input.get("background").and_then(Value::as_bool) == Some(true) {
        return Err("Background commands are not supported yet. Run the command in the foreground with a suitable timeout.".into());
    }
    let timeout_ms = opt_u64(input, "timeout").unwrap_or(DEFAULT_TIMEOUT_MS);
    let dir = match opt_str(input, "workdir") {
        Some(w) => {
            let t = ctx.resolve(w);
            ctx.authorize_external(&t, true)?;
            if !t.absolute.is_dir() {
                return Err(format!("Working directory does not exist: {w}"));
            }
            t.absolute
        }
        None => ctx.cwd.clone(),
    };
    ctx.ask(Request {
        action: "shell".into(),
        resources: vec![command.to_string()],
        save: vec![save_pattern(command)],
        title: format!("$ {command}"),
    })?;
    let (out, ending) = execute(&ctx.shell, command, &dir, &ctx.session_id, timeout_ms).await?;
    Ok(Output::text(render(&out, &ending, timeout_ms)))
}

/// Run `command` under `shell -c`, merging stdout and stderr in arrival order.
pub async fn execute(
    shell: &str,
    command: &str,
    dir: &std::path::Path,
    session: &str,
    timeout_ms: u64,
) -> Result<(String, Ending), String> {
    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg("-c")
        .arg(command)
        .current_dir(dir)
        .env("SKEPTICAL_SESSION_ID", session)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Unable to run {shell}: {e}"))?;
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let pump = |mut r: Box<dyn tokio::io::AsyncRead + Unpin + Send>, buf: Arc<Mutex<Vec<u8>>>| {
        tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = r.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                buf.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        })
    };
    let a = pump(Box::new(child.stdout.take().unwrap()), buf.clone());
    let b = pump(Box::new(child.stderr.take().unwrap()), buf.clone());
    let ending = match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
        Ok(Ok(status)) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                match (status.code(), status.signal()) {
                    (Some(c), _) => Ending::Exited(c),
                    (None, Some(s)) => Ending::Signaled(s),
                    _ => Ending::Exited(-1),
                }
            }
            #[cfg(not(unix))]
            Ending::Exited(status.code().unwrap_or(-1))
        }
        Ok(Err(e)) => return Err(format!("Command failed: {e}")),
        Err(_) => {
            kill_group(&mut child).await;
            Ending::TimedOut
        }
    };
    // Children that inherited the pipes may keep them open; don't wait forever.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = a.await;
        let _ = b.await;
    })
    .await;
    let bytes = buf.lock().unwrap().clone();
    Ok((String::from_utf8_lossy(&bytes).into_owned(), ending))
}

async fn kill_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("/bin/kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .status()
            .await;
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::super::testing::Env;
    use super::*;
    use crate::permission::Reply;

    #[test]
    fn notices() {
        assert_eq!(notice(&Ending::Exited(0), 1000), None);
        assert_eq!(notice(&Ending::Exited(2), 1000).unwrap(), "Exit code: 2");
        assert_eq!(
            notice(&Ending::Signaled(9), 1000).unwrap(),
            "Exit code: 137 (SIGKILL)"
        );
        assert!(notice(&Ending::TimedOut, 1500)
            .unwrap()
            .starts_with("Command timed out after 1.5 seconds."));
    }

    #[test]
    fn rendering() {
        assert_eq!(render("", &Ending::Exited(0), 1), "(no output)");
        assert_eq!(render("hi\n", &Ending::Exited(0), 1), "hi");
        assert_eq!(
            render("oops\n", &Ending::Exited(1), 1),
            "oops\n\nExit code: 1"
        );
    }

    #[test]
    fn save_patterns() {
        assert_eq!(save_pattern("git status --short"), "git *");
        assert_eq!(save_pattern("  ls"), "ls *");
        assert_eq!(save_pattern(""), "*");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runs_and_merges_output() {
        let env = Env::new();
        let out = run(
            &env.ctx(),
            &json!({"command": "echo out; echo err 1>&2; exit 3"}),
        )
        .await
        .unwrap()
        .content;
        assert!(
            out.contains("out") && out.contains("err") && out.ends_with("Exit code: 3"),
            "{out}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workdir_and_session_env() {
        let env = Env::new();
        std::fs::create_dir(env.root().join("sub")).unwrap();
        let out = run(
            &env.ctx(),
            &json!({"command": "pwd; echo $SKEPTICAL_SESSION_ID", "workdir": "sub"}),
        )
        .await
        .unwrap()
        .content;
        assert!(out.contains("/sub\ntest"), "{out}");
        assert!(run(
            &env.ctx(),
            &json!({"command": "true", "workdir": "missing"})
        )
        .await
        .unwrap_err()
        .contains("does not exist"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_stops_the_command() {
        let env = Env::new();
        let start = std::time::Instant::now();
        let out = run(
            &env.ctx(),
            &json!({"command": "echo started; sleep 30", "timeout": 300}),
        )
        .await
        .unwrap()
        .content;
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(
            out.starts_with("started") && out.contains("Command timed out after 0.3 seconds"),
            "{out}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn declined_and_background() {
        let env = Env::with_reply(Reply::Reject(None));
        assert_eq!(
            run(&env.ctx(), &json!({"command": "touch x"}))
                .await
                .unwrap_err(),
            "The user declined this request."
        );
        assert!(!env.root().join("x").exists());
        let env = Env::new();
        assert!(run(
            &env.ctx(),
            &json!({"command": "sleep 1", "background": true})
        )
        .await
        .unwrap_err()
        .contains("not supported yet"));
    }
}
