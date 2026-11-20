//! What plugins can do to sessions and permissions (OpenCode's plugin host:
//! `ctx.session` and `ctx.permission`), over the session store.
//!
//! A plugin reaches the process's host with [`super::host`]; tests and
//! embedders can install their own with [`super::set_host`]. Plugins are
//! trusted code, compiled in, so the host checks only what keeps the store
//! consistent (sessions exist, keys are well formed, values fit); values a
//! plugin takes from the model or the network and hands on here are its
//! own to check.

use crate::store::{self, Info, Store};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Why a host call failed.
#[derive(Debug)]
pub enum HostError {
    /// No session with this id.
    NotFound(String),
    /// An instruction entry key that is not `^[a-z0-9][a-z0-9._-]*$`.
    InvalidKey(String),
    /// An instruction entry value larger than 256 KiB as JSON.
    TooLarge {
        actual: usize,
        max: usize,
    },
    /// A list cursor this host did not hand out.
    InvalidCursor,
    /// The session is not running in this process (for permission checks).
    NotRunning(String),
    Store(anyhow::Error),
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::NotFound(id) => write!(f, "Session not found: {id}"),
            HostError::InvalidKey(k) => write!(
                f,
                "Invalid instruction entry key {k:?}: use lowercase letters, digits, '.', '_' and '-', starting with a letter or digit"
            ),
            HostError::TooLarge { actual, max } => write!(
                f,
                "Instruction entry value is {actual} bytes; the limit is {max} bytes"
            ),
            HostError::InvalidCursor => f.write_str("Invalid session list cursor"),
            HostError::NotRunning(id) => write!(f, "Session {id} is not running in this process"),
            HostError::Store(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<anyhow::Error> for HostError {
    fn from(e: anyhow::Error) -> Self {
        HostError::Store(e)
    }
}

pub type HostResult<T> = Result<T, HostError>;

/// A session for [`Host::create`] to make.
#[derive(Debug, Clone, Default)]
pub struct NewSession {
    pub title: Option<String>,
    /// Makes a child session: it starts in its parent's folder, with its
    /// agent and model.
    pub parent_id: Option<String>,
    /// The folder (default: the parent's, else the host's).
    pub directory: Option<PathBuf>,
    /// Empty: the configured default when the session runs.
    pub agent: Option<String>,
    pub model: Option<String>,
}

/// Which sessions a list shows by parent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Parent {
    #[default]
    Any,
    /// Top-level sessions only.
    Roots,
    /// The children of this session.
    Of(String),
}

/// A session list query (OpenCode's `session.list` input). A `cursor`
/// carries the query it came from: the next page needs only it.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub directory: Option<String>,
    pub parent: Parent,
    /// Sessions whose title contains this, ignoring case.
    pub search: Option<String>,
    /// At most this many (default 50).
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// One page of sessions, most recently updated first.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub data: Vec<Info>,
    /// The cursor of the next page, when there is one.
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    directory: Option<String>,
    parent: Parent,
    search: Option<String>,
    /// The last session of the page before: its `updated` and id.
    after: (i64, String),
}

/// When an admitted compaction runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// At the session's next step: between the steps of a running turn,
    /// else before its next prompt.
    #[default]
    Steer,
    /// Before the session's next prompt, after the running turn.
    Queue,
}

/// A compaction a plugin admitted (OpenCode's inbox entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compaction {
    pub id: String,
    pub session_id: String,
    pub delivery: Delivery,
}

/// The session meta key an admitted compaction waits under.
pub(crate) const COMPACT_KEY: &str = "plugin.compact";

const DEFAULT_LIMIT: usize = 50;

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// The plugin host: sessions, their instruction entries and admitted
/// compactions, permission checks for running sessions, and plugin storage.
pub struct Host {
    store: Store,
    directory: PathBuf,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl Host {
    /// A host over `store`, creating sessions in `directory` by default.
    pub fn new(store: Store, directory: PathBuf) -> Host {
        Host { store, directory }
    }

    /// A host over the session store at `path`.
    pub fn open(path: &std::path::Path, directory: PathBuf) -> anyhow::Result<Host> {
        Ok(Host::new(Store::open(path)?, directory))
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The folder sessions are created in by default.
    pub fn directory(&self) -> &std::path::Path {
        &self.directory
    }

    /// Create a session (OpenCode's `session.create`). A child starts in
    /// its parent's folder with its parent's agent and model.
    pub fn create(&self, new: NewSession) -> HostResult<Info> {
        let parent = match &new.parent_id {
            Some(p) => Some(self.get(p)?),
            None => None,
        };
        let now = store::now();
        let info = Info {
            id: crate::session::new_id(),
            directory: match (&new.directory, &parent) {
                (Some(d), _) => d.display().to_string(),
                (None, Some(p)) => p.directory.clone(),
                (None, None) => self.directory.display().to_string(),
            },
            title: new.title,
            agent: new
                .agent
                .or_else(|| parent.as_ref().map(|p| p.agent.clone()))
                .unwrap_or_default(),
            model: new
                .model
                .or_else(|| parent.as_ref().map(|p| p.model.clone()))
                .unwrap_or_default(),
            parent_id: new.parent_id,
            created: now,
            updated: now,
            usage: Default::default(),
        };
        self.store.create(&info)?;
        Ok(info)
    }

    /// The session `id` (OpenCode's `session.get`).
    pub fn get(&self, id: &str) -> HostResult<Info> {
        self.store
            .get(id)?
            .ok_or_else(|| HostError::NotFound(id.to_string()))
    }

    /// A page of sessions, most recently updated first (OpenCode's
    /// `session.list`).
    pub fn list(&self, q: &ListQuery) -> HostResult<Page> {
        let (directory, parent, search, after) = match &q.cursor {
            Some(c) => {
                let c = decode_cursor(c).ok_or(HostError::InvalidCursor)?;
                (c.directory, c.parent, c.search, Some(c.after))
            }
            None => (
                q.directory.clone(),
                q.parent.clone(),
                q.search.clone(),
                None,
            ),
        };
        let needle = search.as_ref().map(|s| s.to_lowercase());
        let mut all: Vec<Info> = self
            .store
            .list(directory.as_deref(), parent != Parent::Roots)?
            .into_iter()
            .filter(|s| match &parent {
                Parent::Any | Parent::Roots => true,
                Parent::Of(p) => s.parent_id.as_deref() == Some(p),
            })
            .filter(|s| {
                needle.as_ref().is_none_or(|n| {
                    s.title
                        .as_deref()
                        .is_some_and(|t| t.to_lowercase().contains(n))
                })
            })
            .collect();
        all.sort_by(|a, b| (b.updated, &b.id).cmp(&(a.updated, &a.id)));
        let start = match &after {
            Some((u, id)) => all
                .iter()
                .position(|s| (s.updated, &s.id) < (*u, id))
                .unwrap_or(all.len()),
            None => 0,
        };
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let data: Vec<Info> = all.iter().skip(start).take(limit).cloned().collect();
        let next = (start + data.len() < all.len())
            .then(|| data.last())
            .flatten()
            .map(|last| {
                encode_cursor(&Cursor {
                    directory,
                    parent,
                    search,
                    after: (last.updated, last.id.clone()),
                })
            });
        Ok(Page { data, next })
    }

    /// Remove a session and its children (OpenCode's `session.remove`).
    pub fn remove(&self, id: &str) -> HostResult<()> {
        if self.store.delete(id)? {
            Ok(())
        } else {
            Err(HostError::NotFound(id.to_string()))
        }
    }

    /// Rename a session; a running session shows it from its next turn.
    pub fn set_title(&self, id: &str, title: &str) -> HostResult<()> {
        self.get(id)?;
        self.store.set_title(id, title)?;
        Ok(())
    }

    /// Admit a compaction of session `id` (OpenCode's `session.compact`):
    /// the session compacts its history at its next step (`Steer`, the
    /// default) or before its next prompt (`Queue`). A later admission
    /// replaces one still waiting. `message_id` names it (default: a new id).
    pub fn compact(
        &self,
        id: &str,
        delivery: Option<Delivery>,
        message_id: Option<String>,
    ) -> HostResult<Compaction> {
        self.get(id)?;
        let c = Compaction {
            id: message_id.unwrap_or_else(|| format!("msg_{}", &crate::session::new_id()[4..])),
            session_id: id.to_string(),
            delivery: delivery.unwrap_or_default(),
        };
        let json = serde_json::to_string(&c).map_err(anyhow::Error::from)?;
        self.store.set_meta(id, COMPACT_KEY, Some(&json))?;
        Ok(c)
    }

    /// The compaction admitted for session `id` and not yet run.
    pub fn pending_compaction(&self, id: &str) -> HostResult<Option<Compaction>> {
        pending(&self.store, id)
    }

    /// Attach `value` to session `id` under `key` as context the model sees
    /// (OpenCode's `session.instructions.entry.put`): in the system prompt
    /// when the session's instruction baseline is made, else as an update
    /// at its next step.
    pub fn put_instruction(&self, id: &str, key: &str, value: &Value) -> HostResult<()> {
        self.get(id)?;
        if !valid_key(key) {
            return Err(HostError::InvalidKey(key.to_string()));
        }
        self.store.put_context(id, key, value).map_err(|e| {
            match e.downcast_ref::<store::ValueTooLarge>() {
                Some(t) => HostError::TooLarge {
                    actual: t.actual,
                    max: t.max,
                },
                None => HostError::Store(e),
            }
        })
    }

    /// The instruction entries attached to session `id`, by key.
    pub fn list_instructions(&self, id: &str) -> HostResult<Vec<(String, Value)>> {
        self.get(id)?;
        Ok(self
            .store
            .context(id, false)?
            .into_iter()
            .map(|e| (e.key, e.value))
            .collect())
    }

    /// Detach the entry under `key`; the model is told it no longer applies.
    pub fn remove_instruction(&self, id: &str, key: &str) -> HostResult<()> {
        self.get(id)?;
        if !valid_key(key) {
            return Err(HostError::InvalidKey(key.to_string()));
        }
        self.store.remove_context(id, key)?;
        Ok(())
    }
}

pub(crate) fn pending(store: &Store, id: &str) -> HostResult<Option<Compaction>> {
    Ok(store
        .meta(id, COMPACT_KEY)?
        .and_then(|j| serde_json::from_str(&j).ok()))
}

fn encode_cursor(c: &Cursor) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(c).unwrap_or_default())
}

fn decode_cursor(s: &str) -> Option<Cursor> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}
