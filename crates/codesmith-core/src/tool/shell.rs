//! `shell`: run a command and return its combined output. Ported from
//! OpenCode's shell tool, including background commands: a command started
//! with `background: true`, or a foreground command that outlives its
//! timeout, keeps running with its output streaming to a file, and the
//! session is told when it finishes.

use super::{opt_bool, opt_str, opt_u64, str_arg, Ctx, Outcome, Output};
use crate::config::ToolOutputConfig;
use crate::permission::Request;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;

pub const BACKGROUND_INSTRUCTION: &str = "You will be notified automatically when the command finishes. The notification will include the command's output. Unless the user explicitly asks otherwise, DO NOT poll for completion, even if you need the final result to continue. Repeatedly sleeping and reading or searching the output file is polling, not useful work. You may read the current output if it lets you do useful work now, but do not repeatedly check it while waiting for the command to finish. Keep working on anything that does not depend on the result. If you have nothing else to do, end your response; you will be resumed automatically when the command finishes.";

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
        "Commands still running after their timeout move to the background.".into(),
        "Background commands return immediately, and you will be notified when they complete.".into(),
        "The current session ID is available in the CODESMITH_SESSION_ID environment variable.".into(),
    ]
    .join(" ");
    (
        description,
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command string to execute"},
                "workdir": {"type": "string", "description": "Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead."},
                "timeout": {"type": "integer", "minimum": 0, "description": format!("Optional timeout in milliseconds (default {DEFAULT_TIMEOUT_MS}). When it expires, a foreground command moves to the background and a background command is stopped.")},
                "background": {"type": "boolean", "description": "Run the command in the background and return immediately (useful for dev servers and long-running builds). You do not need to use '&' at the end of the command when using this parameter. You will be notified when it completes. DO NOT poll for completion."}
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

/// A command running (or finished) in the background.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub command: String,
    pub file: PathBuf,
    pub pid: Option<u32>,
    pub timeout_ms: u64,
    pub ending: Option<Ending>,
    pub notified: bool,
}

/// The background commands of one session.
#[derive(Clone, Default)]
pub struct Jobs {
    inner: Arc<Mutex<Vec<Job>>>,
    wake: Arc<tokio::sync::Notify>,
    next: Arc<AtomicU32>,
}

impl Jobs {
    fn next_id(&self) -> String {
        format!("shell_{}", self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn add(&self, job: Job) {
        self.inner.lock().unwrap().push(job);
    }

    fn finish(&self, id: &str, ending: Ending) {
        if let Some(j) = self.inner.lock().unwrap().iter_mut().find(|j| j.id == id) {
            j.ending = Some(ending);
        }
        self.wake.notify_waiters();
    }

    /// Jobs still running or finished but not yet reported.
    pub fn running_or_unreported(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.ending.is_none() || !j.notified)
            .count()
    }

    pub fn running(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.ending.is_none())
            .count()
    }

    /// Finished jobs not yet reported, marked as reported.
    pub fn take_finished(&self) -> Vec<Job> {
        let mut out = Vec::new();
        for j in self.inner.lock().unwrap().iter_mut() {
            if j.ending.is_some() && !j.notified {
                j.notified = true;
                out.push(j.clone());
            }
        }
        out
    }

    /// Wait until a job finishes (returns at once if one already has, or if none are running).
    pub async fn wait_any(&self) {
        loop {
            let notified = self.wake.notified();
            {
                let jobs = self.inner.lock().unwrap();
                if jobs.iter().any(|j| j.ending.is_some() && !j.notified)
                    || !jobs.iter().any(|j| j.ending.is_none())
                {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Watch a spawned child until it exits (or `stop_after` passes).
    fn watch(&self, id: String, mut child: tokio::process::Child, stop_after: Option<Duration>) {
        let jobs = self.clone();
        tokio::spawn(async move {
            let ending = match stop_after {
                Some(d) => match tokio::time::timeout(d, child.wait()).await {
                    Ok(Ok(status)) => ending_of(status),
                    Ok(Err(_)) => Ending::Exited(-1),
                    Err(_) => {
                        kill_group(&mut child).await;
                        Ending::TimedOut
                    }
                },
                None => child
                    .wait()
                    .await
                    .map(ending_of)
                    .unwrap_or(Ending::Exited(-1)),
            };
            jobs.finish(&id, ending);
        });
    }
}

/// The message the session sends when a background command finishes
/// (OpenCode's `ShellResult.notification`).
pub fn notification(job: &Job, limits: ToolOutputConfig) -> String {
    let output = std::fs::read(&job.file)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let ending = job.ending.clone().unwrap_or(Ending::Exited(-1));
    let text = super::truncate(
        &render(&output, &ending, job.timeout_ms),
        limits,
        &crate::paths::tool_output_dir(),
    );
    format!(
        "<shell id=\"{}\" state=\"completed\" command=\"{}\">\n{text}\n</shell>",
        job.id,
        job.command.replace('"', "&quot;")
    )
}

fn background_text(job: &Job, timed_out: Option<u64>) -> String {
    let mut lines = vec![format!(
        "Command moved to the background (shell ID: {}).",
        job.id
    )];
    if let Some(pid) = job.pid {
        lines.push(format!("PID: {pid}"));
    }
    lines.push(format!("Output is streaming to: {}", job.file.display()));
    if let Some(ms) = timed_out {
        lines.push(String::new());
        lines.push(format!(
            "The command did not finish within its timeout of {} seconds and was moved to the background. Do not start another copy while it is running.",
            ms as f64 / 1000.0
        ));
    }
    lines.push(String::new());
    lines.push(BACKGROUND_INSTRUCTION.into());
    lines.join("\n")
}

pub async fn run(ctx: &Ctx<'_>, input: &Value) -> Outcome {
    let command = str_arg(input, "command")?;
    let background = opt_bool(input, "background").unwrap_or(false);
    let timeout = opt_u64(input, "timeout");
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
    let jobs = ctx.jobs.clone().unwrap_or_default();
    let id = jobs.next_id();
    let out_dir = crate::paths::tool_output_dir();
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("Unable to create {}: {e}", out_dir.display()))?;
    let file = out_dir.join(format!(
        "{}_{id}_{}.log",
        ctx.session_id,
        super::unique_id()
    ));
    let mut child = spawn(&ctx.shell, command, &dir, &ctx.session_id, &file)?;
    let mut job = Job {
        id: id.clone(),
        command: command.to_string(),
        file: file.clone(),
        pid: child.id(),
        timeout_ms: timeout.unwrap_or(0),
        ending: None,
        notified: false,
    };
    if background {
        if ctx.jobs.is_none() {
            return Err(
                "Background commands are not available here. Run the command in the foreground."
                    .into(),
            );
        }
        jobs.add(job.clone());
        jobs.watch(id, child, timeout.map(Duration::from_millis));
        return Ok(Output::bounded(background_text(&job, None)));
    }
    let limit = timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    match tokio::time::timeout(Duration::from_millis(limit), child.wait()).await {
        Ok(status) => {
            let ending = status
                .map(ending_of)
                .map_err(|e| format!("Command failed: {e}"))?;
            let out = std::fs::read(&file)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let _ = std::fs::remove_file(&file);
            Ok(Output::text(render(&out, &ending, limit)))
        }
        Err(_) if ctx.jobs.is_some() => {
            job.timeout_ms = limit;
            jobs.add(job.clone());
            jobs.watch(id, child, None);
            Ok(Output::bounded(background_text(&job, Some(limit))))
        }
        Err(_) => {
            kill_group(&mut child).await;
            let out = std::fs::read(&file)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let _ = std::fs::remove_file(&file);
            Ok(Output::text(render(&out, &Ending::TimedOut, limit)))
        }
    }
}

/// Start `command` under `shell -c` with stdout and stderr appended to `file`.
fn spawn(
    shell: &str,
    command: &str,
    dir: &Path,
    session: &str,
    file: &Path,
) -> Result<tokio::process::Child, String> {
    let out = std::fs::File::create(file)
        .map_err(|e| format!("Unable to create {}: {e}", file.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut sc = crate::plugin::ShellCommand {
        session_id: session.to_string(),
        cwd: dir.to_path_buf(),
        command: command.to_string(),
        env: Default::default(),
    };
    crate::plugin::active().shell_before(&mut sc);
    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg("-c")
        .arg(&sc.command)
        .current_dir(dir)
        .envs(&sc.env)
        .env("CODESMITH_SESSION_ID", session)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.spawn()
        .map_err(|e| format!("Unable to run {shell}: {e}"))
}

fn ending_of(status: std::process::ExitStatus) -> Ending {
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
            &json!({"command": "pwd; echo $CODESMITH_SESSION_ID", "workdir": "sub"}),
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
    async fn timeout_moves_a_command_to_the_background() {
        let env = Env::new();
        let ctx = env.ctx();
        let jobs = ctx.jobs.clone().unwrap();
        let start = std::time::Instant::now();
        let out = run(
            &ctx,
            &json!({"command": "echo started; sleep 1; echo finished", "timeout": 200}),
        )
        .await
        .unwrap()
        .content;
        assert!(
            start.elapsed() < Duration::from_millis(900),
            "returned at the timeout"
        );
        assert!(
            out.starts_with("Command moved to the background (shell ID: shell_1).")
                && out.contains("timeout of 0.2 seconds"),
            "{out}"
        );
        assert_eq!(jobs.running(), 1);
        jobs.wait_any().await;
        let done = jobs.take_finished();
        assert_eq!(done.len(), 1);
        let note = notification(&done[0], ToolOutputConfig::default());
        assert!(
            note.starts_with("<shell id=\"shell_1\" state=\"completed\""),
            "{note}"
        );
        assert!(
            note.contains("started\nfinished"),
            "the command kept running: {note}"
        );
        assert!(jobs.take_finished().is_empty(), "reported once");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn background_commands_return_immediately_and_respect_their_timeout() {
        let env = Env::new();
        let ctx = env.ctx();
        let jobs = ctx.jobs.clone().unwrap();
        let out = run(
            &ctx,
            &json!({"command": "sleep 30", "background": true, "timeout": 300}),
        )
        .await
        .unwrap()
        .content;
        assert!(
            out.contains("shell ID: shell_1") && out.contains("DO NOT poll"),
            "{out}"
        );
        jobs.wait_any().await;
        let done = jobs.take_finished();
        assert_eq!(done[0].ending, Some(Ending::TimedOut));
        assert!(notification(&done[0], ToolOutputConfig::default())
            .contains("Command timed out after 0.3 seconds"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_job_registry_timeouts_kill() {
        let env = Env::new();
        let mut ctx = env.ctx();
        ctx.jobs = None;
        let out = run(
            &ctx,
            &json!({"command": "echo started; sleep 30", "timeout": 300}),
        )
        .await
        .unwrap()
        .content;
        assert!(
            out.starts_with("started") && out.contains("Command timed out after 0.3 seconds"),
            "{out}"
        );
        assert!(run(&ctx, &json!({"command": "true", "background": true}))
            .await
            .unwrap_err()
            .contains("not available"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn declined_commands_do_not_run() {
        let env = Env::with_reply(Reply::Reject(None));
        assert_eq!(
            run(&env.ctx(), &json!({"command": "touch x"}))
                .await
                .unwrap_err(),
            "The user declined this request."
        );
        assert!(!env.root().join("x").exists());
    }

    #[test]
    fn wait_any_returns_when_nothing_runs() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(Jobs::default().wait_any());
    }
}
