//! Claude Code as a model provider (OpenCode's Claude Agent provider,
//! `plugin/provider/claude-agent`).
//!
//! CodeSmith runs the user's own `claude` (found on `PATH`, or set with
//! `$CODESMITH_CLAUDE_PATH`) and speaks to it the stream-json control
//! protocol the Claude Agent SDK speaks: one long-running process per
//! session, Claude Code's own system prompt with CodeSmith's instructions
//! appended, and CodeSmith's tools offered as the tools of an in-process
//! MCP server, so CodeSmith runs every tool call itself, with its own
//! permission checks (see [`tools`]). The login is Claude Code's: CodeSmith
//! never reads, stores or passes a credential for it, and tells Claude Code
//! it is CodeSmith that drives it.
//!
//! Claude Code keeps its own transcript; [`history`] keeps it in step with
//! CodeSmith's history, which stays the source of truth.
//!
//! How many processes run is capped ([`Pool`]): a process belongs to one
//! session for good once it carried a turn, the least recently used idle
//! one is stopped to make room, and a session's process can be started
//! ahead of its turn (see [`registry`]).

pub mod catalog;
pub mod convert;
pub mod history;
pub mod probe;
mod process;
pub mod query;
pub mod registry;
mod session;
pub mod store;
pub mod tools;

use crate::{Error, EventStream, Interaction, Provider, Request, ToolChoice};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub use registry::Registry;

/// The provider's id.
pub const ID: &str = "claude-agent";

/// What to do when Claude Code can't read its login because the macOS
/// keychain it keeps the login in is locked, as it is in an SSH session.
pub const LOCKED_KEYCHAIN: &str = "Claude Code can't read its login: your macOS keychain is locked in this session (as it is over SSH). Start CodeSmith in a terminal to be asked for your Mac password, or run `security unlock-keychain`; if it still fails, run `claude auth login`.";

/// Whether an error from Claude Code says it isn't signed in.
pub fn not_signed_in(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "not logged in",
        "claude auth login",
        "isn't signed in",
        "please run /login",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// Claude Code's sign-in error, or what to do instead when the cause is a
/// locked keychain (`locked` is asked only for a sign-in error).
pub fn login_hint(message: &str, locked: impl FnOnce() -> bool) -> String {
    if not_signed_in(message) && locked() {
        LOCKED_KEYCHAIN.to_string()
    } else {
        message.to_string()
    }
}

/// Whether the default macOS keychain is locked, so Claude Code can't read
/// its login there. Never on other systems.
pub fn keychain_locked() -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        // `show-keychain-info` fails for a locked keychain without asking.
        Command::new("/usr/bin/security")
            .arg("show-keychain-info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Picks the `claude` to run.
pub const PATH_VAR: &str = "CODESMITH_CLAUDE_PATH";

/// What kind of request this is (`primary`, `title`, `generate` or
/// `compaction`), when the interaction does not say.
pub const KIND_HEADER: &str = "x-codesmith-claude-agent-kind";
/// The session a forked session came from, on its primary requests.
pub const FORK_HEADER: &str = "x-codesmith-claude-agent-fork-source";
/// The session a request belongs to.
pub const SESSION_HEADER: &str = "x-codesmith-session-id";

/// Request options CodeSmith's session sets for this provider: the
/// agent's own prompt (its role), CodeSmith's instructions without the
/// date and environment Claude Code writes itself, the tool calls that
/// were stopped rather than run, and the session's folder.
pub const AGENT_OPTION: &str = "claudeCodeAgent";
pub const INSTRUCTIONS_OPTION: &str = "claudeCodeInstructions";
pub const ABORTED_OPTION: &str = "claudeCodeAborted";
pub const DIRECTORY_OPTION: &str = "claudeCodeDirectory";
/// Connection timeouts (milliseconds) from `settings.headerTimeout` and
/// `settings.chunkTimeout`.
pub const HEADER_TIMEOUT_OPTION: &str = "headerTimeout";
pub const CHUNK_TIMEOUT_OPTION: &str = "chunkTimeout";

/// A request's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Title,
    Generate,
    Compaction,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "primary" => Kind::Primary,
            "title" => Kind::Title,
            "generate" => Kind::Generate,
            "compaction" => Kind::Compaction,
            _ => return None,
        })
    }
}

/// A random (version 4) UUID.
pub fn uuid() -> String {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

type Which = Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>;
type Env = Arc<dyn Fn() -> BTreeMap<String, String> + Send + Sync>;

/// Everything that reaches outside CodeSmith: which `claude` to run, the
/// environment it starts from, and where the provider keeps its records.
#[derive(Clone)]
pub struct Io {
    pub which: Which,
    pub env: Env,
    pub storage: PathBuf,
    /// Told of each change to Claude Code's transcripts (for tests).
    pub observe: Option<store::Observer>,
    /// Run a full login check when a chat's start lists models without a
    /// known context window.
    pub recheck: bool,
}

impl std::fmt::Debug for Io {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Io")
            .field("storage", &self.storage)
            .finish()
    }
}

/// `$CODESMITH_CLAUDE_PATH`, else `claude` on `PATH`.
pub fn which(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(p) = env.get(PATH_VAR).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let path = env.get("PATH")?;
    std::env::split_paths(path)
        .map(|d| d.join("claude"))
        .find(|p| is_executable(p))
}

fn is_executable(p: &std::path::Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.is_file() && m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        m.is_file()
    }
}

/// The process environment.
pub fn process_env() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

/// Where the provider keeps its records: `$XDG_DATA_HOME/codesmith/
/// claude-agent`, else `~/.local/share/codesmith/claude-agent` (beside
/// CodeSmith's own data).
pub fn storage_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/share")
        })
        .join("codesmith")
        .join(ID)
}

impl Io {
    /// The user's own `claude`, environment and CodeSmith data folder.
    pub fn real() -> Io {
        Io {
            which: Arc::new(|| which(&process_env())),
            env: Arc::new(process_env),
            storage: storage_dir(),
            observe: None,
            recheck: true,
        }
    }

    pub fn transcripts(&self) -> store::Transcripts {
        store::Transcripts {
            observe: self.observe.clone(),
            ..store::Transcripts::for_env(&(self.env)())
        }
    }
}

/// How long things may take.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// A session's process (and a spare) stops after this long idle.
    pub idle: Duration,
    /// `claude` must start within this.
    pub startup: Duration,
    /// How long a request waits for a process slot while every process
    /// is busy, before it fails.
    pub queue: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            idle: Duration::from_secs(5 * 60),
            startup: Duration::from_secs(30),
            queue: Duration::from_secs(120),
        }
    }
}

/// How many `claude` processes a registry runs, and which it starts
/// before they are needed (`claude_agent` in the global config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    /// Start a session's process as its turn begins, before the request
    /// is ready, so the request does not wait for Claude Code to start.
    pub prewarm: bool,
    /// Never-used processes kept ready for new sessions. Claude Code takes
    /// its system prompt and tools when it starts, so a spare is started
    /// like the session whose new process left room for it, and serves
    /// only a new session launched the same way (the model and effort may
    /// differ).
    pub spare: usize,
    /// The most processes at once: sessions', spares, side calls and
    /// login checks together.
    pub max_processes: usize,
}

impl Default for Pool {
    fn default() -> Self {
        Pool {
            prewarm: false,
            spare: 0,
            max_processes: DEFAULT_MAX_PROCESSES,
        }
    }
}

/// [`Pool::max_processes`] when not configured.
pub const DEFAULT_MAX_PROCESSES: usize = 4;

/// The highest [`Pool::max_processes`] takes.
pub const MAX_PROCESSES: usize = process::MAX_PROCESSES;

impl Pool {
    /// Within bounds: 1 to [`MAX_PROCESSES`] processes, and spares that
    /// leave room for at least one session's.
    pub fn clamped(self) -> Pool {
        let max_processes = self.max_processes.clamp(1, MAX_PROCESSES);
        Pool {
            prewarm: self.prewarm,
            spare: self.spare.min(max_processes - 1),
            max_processes,
        }
    }
}

static GLOBAL: OnceLock<Registry> = OnceLock::new();

/// The registry CodeSmith's requests use: the user's `claude`, for the
/// life of the process.
pub fn global() -> &'static Registry {
    GLOBAL.get_or_init(|| Registry::new(Io::real(), Timing::default()))
}

/// The efforts Claude Code takes.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// A request as the registry takes it.
pub fn agent_request(p: &Provider, req: &Request) -> Result<registry::Request, Error> {
    let kind = header(req, KIND_HEADER)
        .and_then(Kind::parse)
        .unwrap_or(match req.interaction {
            Interaction::Agent | Interaction::Subagent => Kind::Primary,
            Interaction::Background => Kind::Title,
            Interaction::Compaction => Kind::Compaction,
        });
    let session = header(req, SESSION_HEADER).map(str::to_string);
    let primary = kind == Kind::Primary;
    let branch = matches!(kind, Kind::Generate | Kind::Compaction);
    let flat = crate::flatten_namespaces(req);
    let tools = match (&req.tool_choice, primary || branch) {
        (Some(ToolChoice::None), _) | (_, false) => Vec::new(),
        _ => flat.tools.clone(),
    };
    let o = &req.options;
    let effort = match o.get("effort") {
        None | Some(Value::Null) => None,
        Some(Value::String(e)) if EFFORTS.contains(&e.as_str()) => Some(e.clone()),
        Some(_) => {
            return Err(Error::Invalid(
                "Claude Agent effort must be low, medium, high, xhigh or max".into(),
            ))
        }
    };
    let system = if primary || branch {
        // CodeSmith's own base prompt is replaced by Claude Code's; the
        // session passes its instructions apart from it.
        query::preset(o.str(AGENT_OPTION), o.str(INSTRUCTIONS_OPTION))
    } else {
        query::utility(&req.system)
    };
    let connection = query::connection_env(
        Some(p.base_url.as_str()),
        &p.headers,
        &req.body,
        o.int(HEADER_TIMEOUT_OPTION).map(|n| n.max(0) as u64),
        o.int(CHUNK_TIMEOUT_OPTION).map(|n| n.max(0) as u64),
    );
    Ok(registry::Request {
        session_id: if primary { session.clone() } else { None },
        source_id: if primary {
            header(req, FORK_HEADER).map(str::to_string)
        } else if branch {
            session
        } else {
            None
        },
        model: req.model.clone(),
        effort,
        max_output: req.max_tokens,
        connection,
        directory: o.str(DIRECTORY_OPTION).map(PathBuf::from),
        system,
        prompt: history::from_messages(&req.messages),
        tools,
        aborted: o
            .strings(ABORTED_OPTION)
            .unwrap_or_default()
            .into_iter()
            .collect::<HashSet<_>>(),
    })
}

/// Stream a request through `registry`.
pub fn stream_with(registry: &Registry, request: registry::Request) -> EventStream {
    use futures_util::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    let registry = registry.clone();
    let tools = request.tools.clone();
    tokio::spawn(async move {
        let mut converter = convert::Converter::new(&tools);
        let events = tx.clone();
        let mut emit = move |m: &Value| {
            for e in converter.push(m) {
                let _ = events.send(e);
            }
        };
        if let Err(e) = registry.run(request, &mut emit, cancelled).await {
            if e != registry::CANCELLED {
                let _ = tx.send(Err(convert::failure(e)));
            }
        }
    });
    // Dropping the stream cancels the request: the turn is interrupted and
    // drained in the background before the session takes another.
    struct Guard(tokio::sync::watch::Sender<bool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.send(true);
        }
    }
    let guard = Guard(cancel);
    futures_util::stream::unfold((rx, guard), |(mut rx, guard)| async move {
        rx.recv().await.map(|e| (e, (rx, guard)))
    })
    .boxed()
}

/// Start the process a request's session will use, in the background
/// (see [`Registry::prewarm`]); nothing when prewarming is off.
pub fn prewarm(p: &Provider, req: &Request) {
    let registry = global();
    if !registry.pool().prewarm {
        return;
    }
    if let Ok(request) = agent_request(p, req) {
        registry.prewarm(request);
    }
}

/// [`Provider::stream`] for this provider.
pub async fn stream(p: &Provider, req: &Request) -> Result<EventStream, Error> {
    let request = agent_request(p, req)?;
    Ok(stream_with(global(), request))
}

#[cfg(test)]
mod keychain_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn sign_in_errors_are_recognised_in_any_case() {
        for m in [
            "Not logged in · Run `claude auth login` in a terminal",
            "NOT LOGGED IN",
            "Claude Code isn't signed in. Run …",
            "Invalid API key · Please run /login",
        ] {
            assert!(not_signed_in(m), "{m}");
        }
        for m in ["Anthropic is overloaded (529).", "rate limited", ""] {
            assert!(!not_signed_in(m), "{m}");
        }
    }

    #[test]
    fn a_locked_keychain_replaces_the_sign_in_advice() {
        let got = login_hint(
            "Not logged in · Run `claude auth login` in a terminal",
            || true,
        );
        assert_eq!(got, LOCKED_KEYCHAIN);
        assert!(got.contains("security unlock-keychain"));
        // Signing in again is still named, for a login that is really gone.
        assert!(got.contains("claude auth login"));
    }

    #[test]
    fn an_unlocked_keychain_keeps_claude_codes_message() {
        let m = "Not logged in · Run `claude auth login` in a terminal";
        assert_eq!(login_hint(m, || false), m);
    }

    #[test]
    fn the_keychain_is_asked_only_about_sign_in_errors() {
        let asked = Cell::new(false);
        let m = "Anthropic returned HTTP 500.";
        assert_eq!(
            login_hint(m, || {
                asked.set(true);
                true
            }),
            m
        );
        assert!(!asked.get(), "no `security` run for other errors");
    }

    #[test]
    fn checking_the_keychain_never_prompts_or_hangs() {
        // The answer depends on the session (locked over SSH); it must come
        // back at once either way, with no dialog.
        let started = std::time::Instant::now();
        let _ = keychain_locked();
        assert!(started.elapsed() < Duration::from_secs(10));
        #[cfg(not(target_os = "macos"))]
        assert!(!keychain_locked());
    }
}
