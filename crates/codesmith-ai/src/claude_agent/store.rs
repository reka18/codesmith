//! What the provider keeps: each CodeSmith session's Claude session and
//! checkpoints (OpenCode's `sync/record`), and access to Claude Code's own
//! transcripts for forking and cleanup (the SDK's session functions).
//!
//! Nothing here is a credential: Claude Code's login stays Claude Code's.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub hash: String,
    #[serde(
        rename = "claudeMessageID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub claude_message_id: Option<String>,
}

/// A CodeSmith session's Claude session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(rename = "claudeSessionID")]
    pub claude_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<Checkpoint>>,
    /// After a cancellation Claude owns the partial reply; history ends at
    /// the input it was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<bool>,
}

/// Records under `<storage>/sessions/`, one file per CodeSmith session.
#[derive(Debug, Clone)]
pub struct Records {
    dir: PathBuf,
}

/// A file name for a key: as-is when plain, else hex.
fn file_name(key: &str) -> String {
    if !key.is_empty()
        && key.len() <= 200
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return format!("{key}.json");
    }
    let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
    format!("x{}.json", &hex[..hex.len().min(240)])
}

/// Write `bytes` to `path` readable only by the user, through a temporary
/// file so a reader never sees half of it.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path)
}

impl Records {
    pub fn new(storage: &Path) -> Records {
        Records {
            dir: storage.join("sessions"),
        }
    }

    pub fn path(&self, session: &str) -> PathBuf {
        self.dir.join(file_name(session))
    }

    /// The saved record; a missing or malformed one is none.
    pub fn get(&self, session: &str) -> Option<Record> {
        let text = std::fs::read_to_string(self.path(session)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn set(&self, session: &str, record: &Record) -> std::io::Result<()> {
        let text = serde_json::to_vec_pretty(record).unwrap_or_default();
        write_private(&self.path(session), &text)
    }

    /// How many records are saved.
    pub fn count(&self) -> usize {
        std::fs::read_dir(&self.dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .count()
            })
            .unwrap_or(0)
    }
}

/// Claude Code's transcripts: `<config>/projects/<project>/<session>.jsonl`,
/// where `<config>` is `$CLAUDE_CONFIG_DIR` or `~/.claude`. CodeSmith only
/// reads them to fork a session, checks that one exists, and deletes the
/// temporary copies it made itself.
#[derive(Clone)]
pub struct Transcripts {
    pub root: PathBuf,
    /// Told of each fork and removal (for tests).
    pub observe: Option<Observer>,
}

/// A change CodeSmith made to Claude Code's transcripts.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Forked {
        source: String,
        boundary: String,
        session_id: String,
    },
    Removed(String),
}

pub type Observer = std::sync::Arc<dyn Fn(&Change) + Send + Sync>;

impl std::fmt::Debug for Transcripts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcripts")
            .field("root", &self.root)
            .finish()
    }
}

/// A transcript fork: its session id, and each copied message's new id by
/// the source's.
#[derive(Debug, Clone, PartialEq)]
pub struct Fork {
    pub session_id: String,
    pub messages: HashMap<String, String>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Transcripts {
    /// The transcripts of the Claude Code that runs with `env`.
    pub fn for_env(env: &std::collections::BTreeMap<String, String>) -> Transcripts {
        let root = env
            .get("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env.get("HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .or_else(dirs::home_dir)
                    .map(|h| h.join(".claude"))
            })
            .unwrap_or_else(|| PathBuf::from(".claude"));
        Transcripts {
            root,
            observe: None,
        }
    }

    /// The project folder Claude Code keeps a directory's sessions in.
    pub fn project_dir(&self, cwd: &Path) -> PathBuf {
        let name: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        self.root.join("projects").join(name)
    }

    /// The transcript file of `session`, in whichever project holds it.
    pub fn find(&self, session: &str) -> Option<PathBuf> {
        if !valid_id(session) {
            return None;
        }
        let file = format!("{session}.jsonl");
        let projects = std::fs::read_dir(self.root.join("projects")).ok()?;
        projects
            .filter_map(Result::ok)
            .map(|e| e.path().join(&file))
            .find(|p| p.is_file())
    }

    pub fn exists(&self, session: &str) -> bool {
        self.find(session).is_some()
    }

    pub fn remove(&self, session: &str) -> std::io::Result<()> {
        if let Some(o) = &self.observe {
            o(&Change::Removed(session.to_string()));
        }
        match self.find(session) {
            Some(p) => std::fs::remove_file(p),
            None => Ok(()),
        }
    }

    fn entries(path: &Path) -> std::io::Result<Vec<Value>> {
        let text = std::fs::read_to_string(path)?;
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    /// The conversation of a transcript, oldest first: the chain of user
    /// and assistant messages that ends at its latest one.
    pub fn messages(&self, session: &str) -> std::io::Result<Vec<Value>> {
        let Some(path) = self.find(session) else {
            return Ok(Vec::new());
        };
        Ok(chain(&Self::entries(&path)?))
    }

    /// Copy `session` through `message` into a new session (the SDK's
    /// `forkSession` with `upToMessageId`), with fresh message ids, and
    /// check that the copy holds the same conversation. `None` when the
    /// session or the message is gone.
    pub fn fork(&self, session: &str, message: &str) -> std::io::Result<Option<Fork>> {
        let Some(path) = self.find(session) else {
            return Ok(None);
        };
        let source = chain(&Self::entries(&path)?);
        let Some(end) = source.iter().position(|m| m["uuid"] == message) else {
            return Ok(None);
        };
        let id = super::uuid();
        let target = path.with_file_name(format!("{id}.jsonl"));
        let mut messages = HashMap::new();
        let mut out = String::new();
        let mut parent: Option<String> = None;
        for m in &source[..=end] {
            let old = m["uuid"].as_str().unwrap_or_default().to_string();
            let new = super::uuid();
            let mut copy = m.clone();
            copy["uuid"] = Value::String(new.clone());
            copy["parentUuid"] = parent.clone().map_or(Value::Null, Value::String);
            copy["sessionId"] = Value::String(id.clone());
            if copy.get("session_id").is_some() {
                copy["session_id"] = Value::String(id.clone());
            }
            out.push_str(&serde_json::to_string(&copy).unwrap_or_default());
            out.push('\n');
            messages.insert(old, new.clone());
            parent = Some(new);
        }
        write_private(&target, out.as_bytes())?;
        let copied = self.messages(&id)?;
        let same = copied.len() == end + 1
            && copied
                .iter()
                .zip(&source)
                .all(|(a, b)| a["type"] == b["type"] && a["message"] == b["message"]);
        if !same {
            let _ = std::fs::remove_file(&target);
            return Err(std::io::Error::other(
                "Claude fork did not preserve the requested history boundary",
            ));
        }
        if let Some(o) = &self.observe {
            o(&Change::Forked {
                source: session.to_string(),
                boundary: message.to_string(),
                session_id: id.clone(),
            });
        }
        Ok(Some(Fork {
            session_id: id,
            messages,
        }))
    }
}

/// The user and assistant entries on the chain that ends at the latest
/// one, oldest first.
fn chain(entries: &[Value]) -> Vec<Value> {
    let conversational = |e: &&Value| {
        matches!(e["type"].as_str(), Some("user" | "assistant")) && e["uuid"].is_string()
    };
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter(conversational)
        .filter_map(|e| Some((e["uuid"].as_str()?, e)))
        .collect();
    let Some(mut at) = entries.iter().rev().find(conversational) else {
        return Vec::new();
    };
    let mut out = vec![at.clone()];
    let mut seen = std::collections::HashSet::new();
    while let Some(parent) = at["parentUuid"].as_str() {
        if !seen.insert(parent.to_string()) {
            break;
        }
        match by_id.get(parent) {
            Some(p) => {
                out.push((*p).clone());
                at = p;
            }
            None => break,
        }
    }
    out.reverse();
    out
}
