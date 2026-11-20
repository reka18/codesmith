//! What the session context hooks see (OpenCode's session `prompt` and
//! `context` hooks): a prompt as it is admitted, and the model context just
//! before a request is built from it.

use super::RequestKind;
use codesmith_ai::Message;
use serde_json::Value;

/// A user prompt about to join a session (mutable in [`super::Plugin::prompt`]).
#[derive(Debug, Clone)]
pub struct Prompt {
    pub session_id: String,
    pub text: String,
    /// Kept with the stored message, for later context hooks to read; it
    /// is never sent to the model. Kept only for a stored session.
    pub metadata: Option<Value>,
}

/// One message of the model context.
#[derive(Debug, Clone)]
pub struct ContextMessage {
    pub message: Message,
    /// The stored message's metadata (from a prompt hook), for reading:
    /// it is not sent to the model, whatever a hook sets here.
    pub metadata: Option<Value>,
}

/// One tool the request offers. Renaming it (changing `name`) offers it
/// under the new name, and a call by that name runs this tool; removing
/// it stops offering it. A tool a hook adds is dropped: hooks can only
/// offer the tools the session has.
#[derive(Debug, Clone)]
pub struct ContextTool {
    pub name: String,
    pub description: String,
    /// JSON Schema of the input; a copy, so edits reach this request only.
    pub parameters: Value,
    pub(crate) original: String,
}

impl ContextTool {
    /// The name the session knows the tool by.
    pub fn original(&self) -> &str {
        &self.original
    }
}

/// The model context for one request (mutable in [`super::Plugin::context`]).
/// Messages come from the session's history and the prompt, which carry
/// what the model and tools said: treat their content as untrusted.
#[derive(Debug, Clone)]
pub struct Context {
    pub session_id: String,
    pub agent: String,
    /// The model spec (`provider/model`).
    pub model: String,
    /// [`RequestKind::Primary`] for a step, [`RequestKind::Generate`] for
    /// `Session::generate`.
    pub kind: RequestKind,
    pub system: String,
    pub messages: Vec<ContextMessage>,
    pub tools: Vec<ContextTool>,
}

impl Context {
    pub(crate) fn tool(spec: &codesmith_ai::ToolSpec) -> ContextTool {
        ContextTool {
            name: spec.name.clone(),
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
            original: spec.name.clone(),
        }
    }
}
