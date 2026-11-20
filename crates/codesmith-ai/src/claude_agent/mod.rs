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
    /// A session's process stops after this long idle.
    pub idle: Duration,
    /// `claude` must start within this.
    pub startup: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            idle: Duration::from_secs(5 * 60),
            startup: Duration::from_secs(30),
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

/// [`Provider::stream`] for this provider.
pub async fn stream(p: &Provider, req: &Request) -> Result<EventStream, Error> {
    let request = agent_request(p, req)?;
    Ok(stream_with(global(), request))
}
