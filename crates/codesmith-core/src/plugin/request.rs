//! What the model request hooks see: which session request a hook runs for
//! (OpenCode's `SessionRequestKind` and hook scope), and the bridge from
//! plugins to the provider's [`codesmith_ai::Interceptor`].

use super::Plugin;
use std::sync::Arc;

/// Why a session makes a model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// A step of the agent loop.
    Primary,
    /// A compaction summary.
    Compaction,
    /// The session's title.
    Title,
    /// A side request with the session's context (`Session::generate`).
    Generate,
}

impl RequestKind {
    pub fn name(self) -> &'static str {
        match self {
            RequestKind::Primary => "primary",
            RequestKind::Compaction => "compaction",
            RequestKind::Title => "title",
            RequestKind::Generate => "generate",
        }
    }
}

/// The session request a hook runs for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestScope {
    pub session_id: String,
    pub agent: String,
    /// The model spec (`provider/model`).
    pub model: String,
    pub kind: RequestKind,
}

/// Runs the enabled plugins' HTTP and WebSocket hooks for one request.
pub(super) struct Bridge {
    pub plugins: Vec<Arc<dyn Plugin>>,
    pub scope: RequestScope,
}

impl codesmith_ai::Interceptor for Bridge {
    fn request(&self, req: &mut reqwest::Request) {
        for p in &self.plugins {
            p.http_request(&self.scope, req);
        }
    }
    fn response(&self, res: &mut reqwest::Response) {
        for p in &self.plugins {
            p.http_response(&self.scope, res);
        }
    }
    fn ws_handshake(&self, handshake: &mut codesmith_ai::ws::Handshake) {
        for p in &self.plugins {
            p.ws_handshake(&self.scope, &mut handshake.url, &mut handshake.headers);
        }
    }
    fn ws_send(&self, frame: &mut String) {
        for p in &self.plugins {
            p.ws_send(&self.scope, frame);
        }
    }
    fn ws_receive(&self, frame: &mut String) {
        for p in &self.plugins {
            p.ws_receive(&self.scope, frame);
        }
    }
}
