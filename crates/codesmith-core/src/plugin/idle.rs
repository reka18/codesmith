//! What the async idle hook sees and may do (OpenCode's session
//! `experimental.idle`): the session about to go idle, its history and the
//! background work still running, a side request with its context, and
//! what to do instead of going idle.

use super::BoxFuture;
use codesmith_ai::Message;

/// Work still running when a session would go idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Background {
    /// One of the session's own background shell commands; its finishing
    /// wakes the session, and the hook runs again then.
    Shell { id: String, command: String },
}

/// Why a [`SessionIdle::generate`] request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerateError {
    /// The request was too large for the model's context: compacting
    /// ([`IdleAction::Compact`]) may help.
    Overflow(String),
    Failed(String),
}

impl std::fmt::Display for GenerateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerateError::Overflow(e) | GenerateError::Failed(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for GenerateError {}

pub(crate) trait Generate: Send + Sync {
    fn generate<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Result<String, GenerateError>>;
}

/// A session that finished its work and would go idle (see
/// [`super::Plugin::session_idle`]).
pub struct SessionIdle<'a> {
    pub session_id: &'a str,
    pub parent_id: Option<&'a str>,
    /// The session's history (model and tool output among it: untrusted).
    pub messages: &'a [Message],
    pub background: Vec<Background>,
    pub(crate) generator: &'a dyn Generate,
}

impl SessionIdle<'_> {
    /// A side request with the session's context (`Session::generate`):
    /// its history, then `prompt`, with tool calls off. Not saved; its
    /// tokens count toward the session. The reply is model output.
    pub fn generate<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Result<String, GenerateError>> {
        self.generator.generate(prompt)
    }
}

/// What a session does instead of going idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleAction {
    /// Go idle.
    Idle,
    /// Continue the turn with this message from the user's side.
    Continue(String),
    /// Compact the history, then run the idle hooks again.
    Compact,
}

/// Hooks whose input costs something to build, for [`super::Plugin::wants`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    Prompt,
    Context,
    /// The HTTP and WebSocket hooks.
    Http,
    SessionIdle,
}
