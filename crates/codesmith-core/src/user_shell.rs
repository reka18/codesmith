//! Shell commands the user runs from the prompt (`!command`), as OpenCode's
//! `Session.shell` runs them: as the user, with no permission check and no
//! model call. The command runs in the session's directory and environment
//! (see [`crate::tool::shell::start_user`]); once it ends, the session
//! records what the model will see of it with the next prompt, a
//! completion notification in OpenCode's words ([`notification`]). A
//! command that cannot start is recorded too ([`failed`]).
//!
//! OpenCode also projects a shell message for its UI; CodeSmith's front
//! ends show the notification itself, read back with [`parse`].

use crate::tool::shell::{self, Ending, UserRun};

/// How the notification opens (OpenCode's `ShellResult.userNotification`).
pub const PREFIX: &str = "The following shell command was executed by the user:";

/// How a user command came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Completed,
    Cancelled,
    Error,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Completed => "completed",
            State::Cancelled => "cancelled",
            State::Error => "error",
        }
    }
}

/// What a session's user command came to.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    pub id: String,
    pub command: String,
    pub state: State,
    /// The exit code, when it exited.
    pub exit: Option<i32>,
    /// What it printed (stdout and stderr together), as the model sees it.
    pub output: String,
    pub truncated: bool,
    /// The notification text recorded in the session.
    pub text: String,
}

impl Ran {
    /// Whether it failed: cancelled, or ended other than with exit 0.
    pub fn failed(&self) -> bool {
        self.state != State::Completed || self.exit != Some(0)
    }
}

/// The notification for a finished command: its output and, unless it
/// exited 0, how it ended.
pub fn notification(run: &UserRun) -> String {
    let state = if run.cancelled {
        State::Cancelled
    } else {
        State::Completed
    };
    let output = if run.truncated {
        run.output.clone()
    } else {
        run.output
            .strip_suffix('\n')
            .unwrap_or(&run.output)
            .to_string()
    };
    let status = if run.cancelled {
        Some("Cancelled".to_string())
    } else {
        match &run.ending {
            // As OpenCode's: the timeout itself is not said.
            Some(e) => shell::notice(e, 0),
            None => Some("Exit code: unknown".into()),
        }
    };
    let text: Vec<String> = [Some(output), status]
        .into_iter()
        .flatten()
        .filter(|t| !t.is_empty())
        .collect();
    format!(
        "{PREFIX}\n<shell id=\"{}\" state=\"{}\" command=\"{}\">\n{}\n</shell>",
        run.id,
        state.as_str(),
        shell::attribute(&run.command),
        shell::neutralize_frame(&text.join("\n\n"))
    )
}

/// The notification for a command that could not start.
pub fn failed(command: &str, error: &str) -> String {
    format!("User shell command failed to start:\n{command}\n\n{error}")
}

/// What became of `run`, with its notification `text`.
pub fn ran(run: &UserRun, text: String) -> Ran {
    let exit = match &run.ending {
        Some(Ending::Exited(c)) => Some(*c),
        Some(Ending::TimedOut(inner)) => match inner.as_deref() {
            Some(Ending::Exited(c)) => Some(*c),
            _ => None,
        },
        _ => None,
    };
    Ran {
        id: run.id.clone(),
        command: run.command.clone(),
        state: if run.cancelled {
            State::Cancelled
        } else {
            State::Completed
        },
        exit: if run.cancelled { None } else { exit },
        output: run.output.clone(),
        truncated: run.truncated,
        text,
    }
}

/// A user command as a front end shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub command: String,
    pub state: State,
    /// The output and how it ended, or why it did not start.
    pub text: String,
    /// It did not exit 0.
    pub failed: bool,
}

/// The user command a recorded message's text is the notification of.
pub fn parse(text: &str) -> Option<Shown> {
    if let Some(rest) = text.strip_prefix("User shell command failed to start:\n") {
        let (command, error) = rest.split_once("\n\n").unwrap_or((rest, ""));
        return Some(Shown {
            command: command.to_string(),
            state: State::Error,
            text: error.to_string(),
            failed: true,
        });
    }
    let rest = text.strip_prefix(PREFIX)?.strip_prefix('\n')?;
    let rest = rest.strip_prefix("<shell id=\"")?;
    let (_, rest) = rest.split_once("\" state=\"")?;
    let (state, rest) = rest.split_once("\" command=\"")?;
    let (command, body) = rest.split_once("\">\n")?;
    let body = body.strip_suffix("\n</shell>").unwrap_or(body);
    let state = match state {
        "cancelled" => State::Cancelled,
        "error" => State::Error,
        _ => State::Completed,
    };
    let unescape = |s: &str| {
        s.replace("&#10;", "\n")
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
    };
    let failed = state != State::Completed
        || body
            .rsplit("\n\n")
            .next()
            .is_some_and(|l| l.starts_with("Exit code:") || l.contains("\nExit code:"));
    Some(Shown {
        command: unescape(command),
        state,
        text: body.replace("&lt;", "<"),
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(output: &str, ending: Option<Ending>, cancelled: bool) -> UserRun {
        UserRun {
            id: "shell_1".into(),
            command: "say \"hi\"".into(),
            ending,
            cancelled,
            output: output.into(),
            truncated: false,
            timeout_ms: 0,
        }
    }

    #[test]
    fn notifications_read_back() {
        let ok = notification(&run("hi\n", Some(Ending::Exited(0)), false));
        assert_eq!(
            ok,
            format!("{PREFIX}\n<shell id=\"shell_1\" state=\"completed\" command=\"say &quot;hi&quot;\">\nhi\n</shell>")
        );
        let shown = parse(&ok).unwrap();
        assert_eq!(shown.command, "say \"hi\"");
        assert_eq!(shown.text, "hi");
        assert!(!shown.failed);

        let bad = notification(&run("no\n", Some(Ending::Exited(7)), false));
        assert!(bad.ends_with("no\n\nExit code: 7\n</shell>"), "{bad}");
        assert!(parse(&bad).unwrap().failed);

        let gone = notification(&run("", None, true));
        let shown = parse(&gone).unwrap();
        assert_eq!(
            (shown.state, shown.text.as_str()),
            (State::Cancelled, "Cancelled")
        );

        let shown = parse(&failed("ls", "Working directory does not exist: /x")).unwrap();
        assert_eq!(shown.state, State::Error);
        assert_eq!(shown.command, "ls");
        assert_eq!(parse("just text"), None);
    }
}
