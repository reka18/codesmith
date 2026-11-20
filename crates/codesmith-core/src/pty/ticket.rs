//! Short-lived, single-use tickets for opening a terminal's WebSocket, as
//! OpenCode's `PtyTicket`. A browser cannot put a bearer token on a
//! WebSocket upgrade, so an authenticated client mints a ticket and passes
//! it in the upgrade's query string instead. A ticket names one target (a
//! terminal, or a client MCP connection by client name) and the directory
//! and workspace it was issued for; it is good once, for that scope only,
//! until its time to live runs out.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a ticket stays good.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);
/// The most tickets outstanding at once; issuing more forgets the oldest.
pub const CAPACITY: usize = 10_000;

/// What a ticket opens.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Pty(String),
    McpClient(String),
}

/// A ticket's target and where it was issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub target: Target,
    pub directory: Option<String>,
    pub workspace_id: Option<String>,
}

impl Scope {
    pub fn pty(id: &str) -> Scope {
        Scope {
            target: Target::Pty(id.into()),
            directory: None,
            workspace_id: None,
        }
    }

    pub fn mcp_client(name: &str) -> Scope {
        Scope {
            target: Target::McpClient(name.into()),
            directory: None,
            workspace_id: None,
        }
    }

    pub fn directory(mut self, d: &str) -> Scope {
        self.directory = Some(d.into());
        self
    }

    pub fn workspace(mut self, w: &str) -> Scope {
        self.workspace_id = Some(w.into());
        self
    }
}

/// An issued ticket: the value to pass, and its lifetime in seconds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ConnectToken {
    pub ticket: String,
    pub expires_in: u64,
}

struct Stored {
    scope: Scope,
    expires: Instant,
}

pub struct Tickets {
    ttl: Duration,
    inner: Mutex<(HashMap<String, Stored>, VecDeque<String>)>,
}

impl Default for Tickets {
    fn default() -> Self {
        Tickets::new(DEFAULT_TTL)
    }
}

impl Tickets {
    pub fn new(ttl: Duration) -> Tickets {
        Tickets {
            ttl,
            inner: Mutex::new((HashMap::new(), VecDeque::new())),
        }
    }

    pub fn issue(&self, scope: Scope) -> ConnectToken {
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        let ticket: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let now = Instant::now();
        let mut g = self.inner.lock().unwrap();
        let (map, order) = &mut *g;
        map.retain(|_, s| s.expires > now);
        order.retain(|t| map.contains_key(t));
        while map.len() >= CAPACITY {
            match order.pop_front() {
                Some(old) => {
                    map.remove(&old);
                }
                None => break,
            }
        }
        map.insert(
            ticket.clone(),
            Stored {
                scope,
                expires: now + self.ttl,
            },
        );
        order.push_back(ticket.clone());
        ConnectToken {
            ticket,
            expires_in: self.ttl.as_secs_f64().round().max(1.0) as u64,
        }
    }

    /// Use `ticket` for `scope`: true, and the ticket is gone, when it was
    /// issued for exactly this scope and has not expired. A ticket offered
    /// for another scope stays good for its own.
    pub fn consume(&self, scope: &Scope, ticket: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        let (map, order) = &mut *g;
        let ok = map
            .get(ticket)
            .is_some_and(|s| s.expires > Instant::now() && s.scope == *scope);
        let expired = map.get(ticket).is_some_and(|s| s.expires <= Instant::now());
        if ok || expired {
            map.remove(ticket);
            order.retain(|t| t != ticket);
        }
        ok
    }
}
