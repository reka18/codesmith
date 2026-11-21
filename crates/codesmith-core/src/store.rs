//! Durable sessions in SQLite: one row per session and an append-only log of
//! records (messages and compaction checkpoints). History for the model is
//! rebuilt from the log, starting at the latest checkpoint.

use anyhow::{anyhow, Context, Result};
use codesmith_ai::{Message, Usage};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: i32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub id: String,
    pub directory: String,
    pub title: Option<String>,
    pub agent: String,
    pub model: String,
    pub parent_id: Option<String>,
    pub created: i64,
    pub updated: i64,
    pub usage: Usage,
}

/// One entry in a session's log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Message {
        message: Message,
    },
    /// A summary replacing everything before `keep_from` (a record sequence number).
    Checkpoint {
        summary: String,
        keep_from: i64,
    },
    /// The provider's own compaction (see [`codesmith_ai::Part::Compaction`]):
    /// `window` replaces everything before `keep_from`, and replays only on
    /// `model` (its spec without the variant), the model that made it.
    Native {
        window: Vec<Message>,
        model: String,
        keep_from: i64,
    },
    /// The project's files (a snapshot tree) at the start of a turn.
    Snapshot {
        tree: String,
    },
    /// The project's files at the end of a turn, so undo can restore only
    /// the files the turn changed.
    TurnEnd {
        tree: String,
    },
    /// Undo: everything from `from_seq` up to this record is hidden, and the
    /// files were restored. `after` is the tree just before undoing, for redo.
    Revert {
        from_seq: i64,
        after: String,
    },
    /// Redo of the revert at `revert_seq`.
    Redo {
        revert_seq: i64,
    },
    /// Token usage of one model step (for `codesmith stats`).
    Step {
        model: String,
        usage: Usage,
    },
    /// One tool call: how long it took and whether it failed.
    Tool {
        name: String,
        ms: u64,
        error: bool,
    },
    /// The text of a file an edit, write or patch call `call` changed,
    /// before (`None`: it was new) and after (empty when a patch deleted
    /// it), so a replay can show the diff. A patch records one per file.
    /// Recorded only while both fit in [`CHANGE_MAX`].
    Change {
        call: String,
        path: PathBuf,
        old: Option<String>,
        new: String,
    },
    /// An instruction state change (see [`crate::instruction_state`]):
    /// the values that changed and the ones removed. A `baseline` starts the
    /// state over with these values (the session's first, or after an undo
    /// dropped the state); `text` is the update the model sees here as a
    /// system message, absent for a baseline and a silent update. A `fork`
    /// baseline keeps the updates the fork copied in view.
    Instructions {
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        changed: BTreeMap<String, serde_json::Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        baseline: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fork: bool,
    },
    /// Metadata a plugin's prompt hook kept with the message at `seq`,
    /// for later context hooks; never sent to the model.
    Metadata {
        seq: i64,
        metadata: serde_json::Value,
    },
    /// A record kind from a newer CodeSmith; ignored.
    #[serde(other)]
    Unknown,
}

/// The most text (before and after together) a [`Record::Change`] keeps.
pub const CHANGE_MAX: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: i64,
    pub created: i64,
    pub record: Record,
}

/// A whole session, for export and import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Export {
    pub info: Info,
    pub entries: Vec<Entry>,
}

pub struct Store {
    conn: std::sync::Mutex<Connection>,
    /// Whether records keep their media in the blob store (see
    /// [`crate::blob`]); on unless `media.blob` is false.
    media_blob: std::sync::atomic::AtomicBool,
}

/// Session meta key: when the session was archived (ms).
pub const ARCHIVED_META: &str = "archived";

/// Session meta keys of its read state (OpenCode's `time.idle`,
/// `time.viewed` and `outcome`): when its last turn ended (ms), the idle
/// time a viewer last saw, and how that turn ended.
pub const IDLE_META: &str = "idle";
pub const VIEWED_META: &str = "viewed";
pub const OUTCOME_META: &str = "outcome";

/// Session meta key: the session's own permission rules (JSON), which a
/// child starts with and a fork copies (see [`Store::set_permissions`]).
pub const PERMISSIONS_META: &str = "permissions";

/// How a session's last turn ended, and whether anyone has seen it since
/// (see [`Store::view`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReadState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewed: Option<i64>,
    /// `succeeded`, `failed` or `interrupted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

impl ReadState {
    /// A finished turn nobody has seen.
    pub fn unread(&self) -> bool {
        self.idle.is_some_and(|i| self.viewed.is_none_or(|v| i > v))
    }
}

/// No such session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotFound {
    pub session: String,
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no session {}", self.session)
    }
}

impl std::error::Error for NotFound {}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

/// Whether `title` contains `search` (OpenCode's session title search):
/// case-insensitive beyond ASCII, with SQL `LIKE` wildcards (`%` any run,
/// `_` any one character); everything else, `*` and `?` included, is
/// literal.
pub fn title_matches(title: &str, search: &str) -> bool {
    let fold = |s: &str| -> Vec<char> { s.chars().flat_map(char::to_lowercase).collect() };
    let (t, p) = (fold(title), fold(search));
    // `%search%`, matched with one row of the usual wildcard table.
    let mut pat = vec!['%'];
    pat.extend(p);
    pat.push('%');
    let mut row = vec![false; t.len() + 1];
    row[0] = true;
    for c in &pat {
        let mut next = vec![false; t.len() + 1];
        match c {
            '%' => {
                let mut any = false;
                for i in 0..=t.len() {
                    any |= row[i];
                    next[i] = any;
                }
            }
            _ => {
                for i in 1..=t.len() {
                    next[i] = row[i - 1] && (*c == '_' || t[i - 1] == *c);
                }
            }
        }
        row = next;
    }
    row[t.len()]
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `$CODESMITH_DB`, else `<data dir>/codesmith.db`.
pub fn default_path() -> PathBuf {
    std::env::var_os("CODESMITH_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::data_dir().join("codesmith.db"))
}

/// The text the model sees for a checkpoint.
pub fn checkpoint_text(summary: &str) -> String {
    format!(
        "<conversation-checkpoint>\n<summary>\n{}\n</summary>\n</conversation-checkpoint>",
        summary.trim()
    )
}

/// Switch to write-ahead logging. SQLite answers "database is locked" at
/// once, without waiting out the busy timeout, when another connection
/// holds the new database while the mode changes (the UI and its automation
/// scheduler open it together on first run), so retry for a while.
fn wal(conn: &Connection) -> rusqlite::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match conn.execute_batch("PRAGMA journal_mode=WAL;") {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            r => return r,
        }
    }
}

const LEASE_TABLE: &str = "CREATE TABLE IF NOT EXISTS session_lease (
    session_id TEXT PRIMARY KEY, holder TEXT NOT NULL, expires INTEGER NOT NULL);";

/// Values a front end keeps with a session (such as the directories an
/// editor granted it), by key.
const META_TABLE: &str = "CREATE TABLE IF NOT EXISTS session_meta (
    session_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
    PRIMARY KEY (session_id, key));";

/// Context entries a host attached to a session (OpenCode's instruction
/// entries); a removed one stays until its removal is announced.
const CONTEXT_TABLE: &str = "CREATE TABLE IF NOT EXISTS session_context (
    session_id TEXT NOT NULL, key TEXT NOT NULL, value TEXT, removed INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, key));";

/// A context entry's value is larger than [`crate::instruction_state::MAX_ENTRY_BYTES`].
#[derive(Debug)]
pub struct ValueTooLarge {
    pub actual: usize,
    pub max: usize,
}

impl std::fmt::Display for ValueTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Instruction entry value is {} bytes; the limit is {} bytes",
            self.actual, self.max
        )
    }
}

impl std::error::Error for ValueTooLarge {}

/// Run a write transaction, retrying for a while when another connection
/// holds the write lock past the busy timeout.
fn retry_busy<T>(mut f: impl FnMut() -> rusqlite::Result<T>) -> Result<T> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match f() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            r => return Ok(r?),
        }
    }
}

/// A session is running a turn in another process (or another session
/// object), and the caller asked not to wait.
#[derive(Debug)]
pub struct Busy {
    pub session: String,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session {} is busy running another turn", self.session)
    }
}

impl std::error::Error for Busy {}

/// How long a turn lease lasts without renewal, and how often it is renewed.
pub const TURN_LEASE_MS: i64 = 30_000;
const TURN_RENEW: std::time::Duration = std::time::Duration::from_secs(10);

/// The turn lease on one session, held while a turn runs. It is renewed in
/// the background and released when dropped (also when the turn's future
/// is cancelled).
pub struct TurnLease {
    store: std::sync::Arc<Store>,
    id: String,
    holder: String,
    renew: tokio::task::JoinHandle<()>,
}

impl TurnLease {
    /// Take the lease on session `id` in the database at `path`. When
    /// another holder has it: with `wait`, call `waiting` once and poll
    /// until it is free; without, fail with [`Busy`].
    pub async fn acquire(
        path: &Path,
        id: &str,
        holder: &str,
        wait: bool,
        waiting: impl FnOnce(),
    ) -> Result<TurnLease> {
        let store = std::sync::Arc::new(Store::open(path)?);
        let mut waiting = Some(waiting);
        loop {
            let taken = {
                let st = store.clone();
                let (sid, who) = (id.to_string(), holder.to_string());
                let job = tokio::task::spawn_blocking(move || {
                    st.lease_session(&sid, &who, TURN_LEASE_MS)
                });
                // A turn cancelled while this runs drops the wait, not the
                // job: the lease it then takes would have no holder left to
                // release it, and the session's next turn would wait out
                // its expiry. Release it once the job is done.
                let mut guard = InFlight {
                    job: Some(job),
                    store: store.clone(),
                    id: id.to_string(),
                    holder: holder.to_string(),
                };
                let r = guard.job.as_mut().expect("just set").await;
                guard.job = None;
                r??
            };
            if taken {
                break;
            }
            if !wait {
                return Err(Busy {
                    session: id.to_string(),
                }
                .into());
            }
            if let Some(w) = waiting.take() {
                w();
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        let renew = {
            let st = store.clone();
            let (id, holder) = (id.to_string(), holder.to_string());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(TURN_RENEW).await;
                    let (st, id, holder) = (st.clone(), id.clone(), holder.clone());
                    let _ = tokio::task::spawn_blocking(move || {
                        st.lease_session(&id, &holder, TURN_LEASE_MS)
                    })
                    .await;
                }
            })
        };
        Ok(TurnLease {
            store,
            id: id.to_string(),
            holder: holder.to_string(),
            renew,
        })
    }
}

/// A lease being taken; see [`TurnLease::acquire`].
struct InFlight {
    job: Option<tokio::task::JoinHandle<Result<bool>>>,
    store: std::sync::Arc<Store>,
    id: String,
    holder: String,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let Some(job) = self.job.take() else { return };
        let (store, id, holder) = (self.store.clone(), self.id.clone(), self.holder.clone());
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Ok(Ok(true)) = job.await {
                    let _ =
                        tokio::task::spawn_blocking(move || store.release_session(&id, &holder))
                            .await;
                }
            });
        }
    }
}

impl Drop for TurnLease {
    fn drop(&mut self) {
        self.renew.abort();
        let _ = self.store.release_session(&self.id, &self.holder);
    }
}

impl Store {
    /// Open (or create) the database at `path`. Sessions hold code and
    /// possibly secrets, so the file is owner-only (0600) — an existing one
    /// that others can read is tightened — and so is the data directory
    /// when the database lives there.
    pub fn open(path: &Path) -> Result<Store> {
        crate::paths::ensure_parent(path)
            .with_context(|| format!("creating {}", path.parent().unwrap_or(path).display()))?;
        if !path.exists() {
            let _ = crate::paths::create_private(path);
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        let store = Self::init(conn)?;
        let name = path.as_os_str().to_owned();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = name.clone();
            p.push(suffix);
            crate::paths::tighten(Path::new(&p))
                .with_context(|| format!("restricting {}", Path::new(&p).display()))?;
        }
        Ok(store)
    }

    /// Run `f` with the connection (for other tables kept in the same database).
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    /// The database file, or `None` for an in-memory store.
    pub fn path(&self) -> Option<PathBuf> {
        self.conn
            .lock()
            .unwrap()
            .path()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    }

    pub fn memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        // The busy timeout comes first so a second process opening a new
        // database waits for the first instead of failing.
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        wal(&conn)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(anyhow!("database schema {version} is newer than this codesmith understands ({SCHEMA_VERSION}); upgrade codesmith"));
        }
        if version < 1 {
            // IF NOT EXISTS: two connections (`serve`'s scheduler and its
            // first request) can both find a new database at version 0.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    title TEXT,
                    agent TEXT NOT NULL,
                    model TEXT NOT NULL,
                    parent_id TEXT,
                    created INTEGER NOT NULL,
                    updated INTEGER NOT NULL,
                    usage TEXT NOT NULL DEFAULT '{}'
                );
                CREATE INDEX IF NOT EXISTS session_directory ON session(directory, updated);
                CREATE TABLE IF NOT EXISTS record (
                    session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
                    seq INTEGER NOT NULL,
                    created INTEGER NOT NULL,
                    body TEXT NOT NULL,
                    PRIMARY KEY (session_id, seq)
                );
                PRAGMA user_version = 1;",
            )?;
        }
        conn.execute_batch(crate::blob::TABLES)?;
        Ok(Store {
            conn: std::sync::Mutex::new(conn),
            media_blob: std::sync::atomic::AtomicBool::new(true),
        })
    }

    pub fn create(&self, info: &Info) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session (id, directory, title, agent, model, parent_id, created, updated, usage) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![info.id, info.directory, info.title, info.agent, info.model, info.parent_id, info.created, info.updated, serde_json::to_string(&info.usage)?],
        )?;
        Ok(())
    }

    fn row(r: &rusqlite::Row) -> rusqlite::Result<Info> {
        let usage: String = r.get(8)?;
        Ok(Info {
            id: r.get(0)?,
            directory: r.get(1)?,
            title: r.get(2)?,
            agent: r.get(3)?,
            model: r.get(4)?,
            parent_id: r.get(5)?,
            created: r.get(6)?,
            updated: r.get(7)?,
            usage: serde_json::from_str(&usage).unwrap_or_default(),
        })
    }

    const COLUMNS: &'static str =
        "id, directory, title, agent, model, parent_id, created, updated, usage";

    pub fn get(&self, id: &str) -> Result<Option<Info>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {} FROM session WHERE id = ?1", Self::COLUMNS),
                [id],
                Self::row,
            )
            .optional()?)
    }

    /// Sessions, newest first; only top-level ones unless `children`.
    pub fn list(&self, directory: Option<&str>, children: bool) -> Result<Vec<Info>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = format!("SELECT {} FROM session WHERE 1=1", Self::COLUMNS);
        if directory.is_some() {
            sql.push_str(" AND directory = ?1");
        }
        if !children {
            sql.push_str(" AND parent_id IS NULL");
        }
        sql.push_str(" ORDER BY updated DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = match directory {
            Some(d) => stmt
                .query_map([d], Self::row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map([], Self::row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// The most recently updated top-level session in `directory` that is
    /// not archived.
    pub fn latest(&self, directory: &str) -> Result<Option<Info>> {
        Ok(self
            .list_archived(Some(directory), false, Some(false))?
            .into_iter()
            .next())
    }

    pub fn append(&self, id: &str, record: &Record) -> Result<i64> {
        self.append_at(id, record, now())
    }

    /// Append with an explicit time (milliseconds), e.g. when importing.
    ///
    /// The next sequence number is read and the record written in one
    /// `BEGIN IMMEDIATE` transaction, so two connections (a UI and an
    /// automation in another process) appending to the same session cannot
    /// pick the same number.
    ///
    /// With the blob store on, the record's inline media is stored once as
    /// blobs and the record keeps references (see [`crate::blob`]); a
    /// message also ends any undone turn's chance of a redo, so the blobs
    /// only those turns named are let go.
    pub fn append_at(&self, id: &str, record: &Record, t: i64) -> Result<i64> {
        let blobs = self.media_blob();
        let mut conn = self.conn.lock().unwrap();
        retry_busy(|| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let seq = insert_record(&tx, id, record, t, blobs)?;
            tx.commit()?;
            Ok(seq)
        })
    }

    /// Hand a prompt to session `id` durably, as one transaction: `commit`
    /// runs first (an automation's compare-and-set on its own tables; when
    /// it returns false nothing is written), then the session row `info` is
    /// added unless it exists, then the user message `message`, tagged
    /// `key` in a [`Record::Metadata`] after it, unless a message with that
    /// tag is already there (insert-if-absent). Returns the message's
    /// sequence number, or `None` when `commit` declined.
    pub fn admit(
        &self,
        info: Option<&Info>,
        id: &str,
        message: &Message,
        key: &str,
        metadata: serde_json::Value,
        commit: impl Fn(&Connection) -> Result<bool>,
    ) -> Result<Option<i64>> {
        let blobs = self.media_blob();
        let record = Record::Message {
            message: message.clone(),
        };
        let mut conn = self.conn.lock().unwrap();
        retry_busy(|| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if !commit(&tx).map_err(sql_err)? {
                return Ok(None);
            }
            if let Some(info) = info {
                tx.execute(
                    "INSERT OR IGNORE INTO session (id, directory, title, agent, model, parent_id, created, updated, usage) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![info.id, info.directory, info.title, info.agent, info.model, info.parent_id, info.created, info.updated, serde_json::to_string(&info.usage).map_err(|e| sql_err(e.into()))?],
                )?;
            }
            let seq = match tagged(&tx, id, key)? {
                Some(seq) => seq,
                None => {
                    let t = now();
                    let seq = insert_record(&tx, id, &record, t, blobs)?;
                    let mut meta = metadata.clone();
                    meta["automation"]["message"] = serde_json::Value::String(key.to_string());
                    insert_record(
                        &tx,
                        id,
                        &Record::Metadata {
                            seq,
                            metadata: meta,
                        },
                        t,
                        blobs,
                    )?;
                    seq
                }
            };
            tx.commit()?;
            Ok(Some(seq))
        })
    }

    /// The sequence number of the message a [`Store::admit`] tagged `key`
    /// in session `id`.
    pub fn tagged_message(&self, id: &str, key: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        Ok(tagged(&conn, id, key)?)
    }

    /// Whether session `id` has an answer after the message at `seq`: its
    /// latest message from then on is the model's, with no tool calls
    /// left to run.
    pub fn replied_after(&self, id: &str, seq: i64) -> Result<bool> {
        let after: Vec<Message> = self
            .entries(id)?
            .into_iter()
            .filter(|e| e.seq >= seq)
            .filter_map(|e| match e.record {
                Record::Message { message } => Some(message),
                _ => None,
            })
            .collect();
        Ok(after.len() > 1 && !awaits_reply(&after))
    }

    /// Whether new records keep their media in the blob store.
    pub fn media_blob(&self) -> bool {
        self.media_blob.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Store new records' media as blobs (`true`, the default) or inline
    /// (config `media.blob: false`). Records already written read either way.
    pub fn set_media_blob(&self, on: bool) {
        self.media_blob
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Keep `bytes` as a blob `owner` references (a job's result), and
    /// return its hash; `None` past [`crate::blob::MAX_BYTES`].
    pub fn put_blob(&self, owner: &str, bytes: &[u8], mime: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let Some(h) = crate::blob::put(&conn, bytes, mime)? else {
            return Ok(None);
        };
        crate::blob::record_refs(&conn, owner, 0, &[h.clone()].into())?;
        Ok(Some(h))
    }

    /// A blob's type and bytes, if it is still kept.
    pub fn blob(&self, hash: &str) -> Result<Option<(String, Vec<u8>)>> {
        let conn = self.conn.lock().unwrap();
        crate::blob::get(&conn, hash)
    }

    /// Drop every reference `owner` holds, and the blobs nothing else names.
    pub fn release_blobs(&self, owner: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        crate::blob::release(&conn, owner, None)
    }

    /// Move session `id`'s inline media (records written inline, before the
    /// blob store or with it off) into blobs, whatever `media.blob` says:
    /// an offline backfill. Returns how many records were rewritten.
    pub fn migrate_media(&self, id: &str) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT seq, body FROM record WHERE session_id = ?1 AND body LIKE '%\"base64\"%' ORDER BY seq",
            )?;
            let rows = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut moved = 0;
        for (seq, body) in rows {
            let Ok(mut record) = serde_json::from_str::<Record>(&body) else {
                continue;
            };
            if !crate::blob::has_inline(&record) {
                continue;
            }
            let hashes = crate::blob::externalize(&tx, &mut record)?;
            let stored = serde_json::to_string(&record)?;
            if stored == body {
                continue;
            }
            tx.execute(
                "UPDATE record SET body = ?3 WHERE session_id = ?1 AND seq = ?2",
                params![id, seq, stored],
            )?;
            crate::blob::record_refs(&tx, id, seq, &hashes)?;
            moved += 1;
        }
        tx.commit()?;
        Ok(moved)
    }

    /// The newest record's sequence number in a session (0 if none).
    pub fn last_seq(&self, id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM record WHERE session_id = ?1",
            [id],
            |r| r.get(0),
        )?)
    }

    /// Take or renew the turn lease on a session for `ttl` ms: true if
    /// `holder` has it now. A session runs one turn at a time across every
    /// process sharing the database; an expired lease (its holder died) is
    /// taken over.
    pub fn lease_session(&self, id: &str, holder: &str, ttl: i64) -> Result<bool> {
        let now = now();
        let mut conn = self.conn.lock().unwrap();
        retry_busy(|| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch(LEASE_TABLE)?;
            tx.execute(
                "INSERT INTO session_lease (session_id, holder, expires) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET holder = ?2, expires = ?3
                 WHERE session_lease.expires < ?4 OR session_lease.holder = ?2",
                params![id, holder, now + ttl, now],
            )?;
            let h: String = tx.query_row(
                "SELECT holder FROM session_lease WHERE session_id = ?1",
                [id],
                |r| r.get(0),
            )?;
            tx.commit()?;
            Ok(h == holder)
        })
    }

    /// Give up the turn lease if `holder` has it.
    pub fn release_session(&self, id: &str, holder: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(LEASE_TABLE)?;
        conn.execute(
            "DELETE FROM session_lease WHERE session_id = ?1 AND holder = ?2",
            params![id, holder],
        )?;
        Ok(())
    }

    /// Who is running a turn in the session now (an unexpired lease).
    pub fn session_holder(&self, id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(LEASE_TABLE)?;
        Ok(conn
            .query_row(
                "SELECT holder FROM session_lease WHERE session_id = ?1 AND expires >= ?2",
                params![id, now()],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn entries(&self, id: &str) -> Result<Vec<Entry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT seq, created, body FROM record WHERE session_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, created, body) = row?;
            let mut record: Record =
                serde_json::from_str(&body).with_context(|| format!("record {seq} of {id}"))?;
            if body.contains(crate::blob::PREFIX) {
                crate::blob::hydrate(&conn, &mut record)?;
            }
            out.push(Entry {
                seq,
                created,
                record,
            });
        }
        Ok(out)
    }

    /// The conversation the model should see: the latest checkpoint (as a user
    /// message) followed by every message from its `keep_from` on.
    pub fn history(&self, id: &str) -> Result<Vec<Message>> {
        Ok(history_of(&self.entries(id)?))
    }

    pub fn set_title(&self, id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session SET title = ?2 WHERE id = ?1",
            params![id, title],
        )?;
        Ok(())
    }

    /// Move session `id` (and its child sessions) to `directory`, where it
    /// is resumed from now on. False when there is no such session.
    pub fn set_directory(&self, id: &str, directory: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "WITH RECURSIVE family(id) AS (
                SELECT ?1 UNION SELECT s.id FROM session s JOIN family f ON s.parent_id = f.id
            ) UPDATE session SET directory = ?2 WHERE id IN family",
            params![id, directory],
        )?;
        Ok(n > 0)
    }

    /// Record a switch of agent and model, so a reopened session keeps them.
    pub fn set_selection(&self, id: &str, agent: &str, model: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session SET agent = ?2, model = ?3 WHERE id = ?1",
            params![id, agent, model],
        )?;
        Ok(())
    }

    pub fn set_usage(&self, id: &str, usage: &Usage) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE session SET usage = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(usage)?],
        )?;
        Ok(())
    }

    /// Delete a session and, recursively, its child sessions.
    pub fn delete(&self, id: &str) -> Result<bool> {
        // The lock is released before recursing; std's Mutex is not reentrant.
        let children: Vec<String> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id FROM session WHERE parent_id = ?1")?;
            let rows = stmt
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            rows
        };
        for c in children {
            self.delete(&c)?;
        }
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(META_TABLE)?;
        conn.execute("DELETE FROM session_meta WHERE session_id = ?1", [id])?;
        conn.execute_batch(CONTEXT_TABLE)?;
        conn.execute("DELETE FROM session_context WHERE session_id = ?1", [id])?;
        crate::blob::release(&conn, id, None)?;
        Ok(conn.execute("DELETE FROM session WHERE id = ?1", [id])? > 0)
    }

    /// Keep `value` with session `id` under `key` (`None` removes it). The
    /// session need not be stored yet.
    pub fn set_meta(&self, id: &str, key: &str, value: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(META_TABLE)?;
        match value {
            Some(v) => conn.execute(
                "INSERT INTO session_meta (session_id, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id, key) DO UPDATE SET value = ?3",
                params![id, key, v],
            )?,
            None => conn.execute(
                "DELETE FROM session_meta WHERE session_id = ?1 AND key = ?2",
                params![id, key],
            )?,
        };
        Ok(())
    }

    /// What [`Store::set_meta`] kept with session `id` under `key`.
    pub fn meta(&self, id: &str, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(META_TABLE)?;
        Ok(conn
            .query_row(
                "SELECT value FROM session_meta WHERE session_id = ?1 AND key = ?2",
                params![id, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Attach `value` to session `id` under `key` as context the model sees
    /// (OpenCode's `InstructionEntry.put`): in the system prompt when the
    /// session's instruction baseline is made, else as an update at the next
    /// step. Fails with [`ValueTooLarge`] past 256 KiB of JSON.
    pub fn put_context(&self, id: &str, key: &str, value: &serde_json::Value) -> Result<()> {
        let json = serde_json::to_string(value)?;
        let max = crate::instruction_state::MAX_ENTRY_BYTES;
        if json.len() > max {
            return Err(ValueTooLarge {
                actual: json.len(),
                max,
            }
            .into());
        }
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(CONTEXT_TABLE)?;
        conn.execute(
            "INSERT INTO session_context (session_id, key, value, removed) VALUES (?1, ?2, ?3, 0)
             ON CONFLICT(session_id, key) DO UPDATE SET value = ?3, removed = 0",
            params![id, key, json],
        )?;
        Ok(())
    }

    /// Detach the context under `key`; the model is told it no longer applies.
    pub fn remove_context(&self, id: &str, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(CONTEXT_TABLE)?;
        conn.execute(
            "UPDATE session_context SET value = NULL, removed = 1 WHERE session_id = ?1 AND key = ?2",
            params![id, key],
        )?;
        Ok(())
    }

    /// The context attached to session `id`, by key; with `removed`, the
    /// detached entries too.
    pub fn context(
        &self,
        id: &str,
        removed: bool,
    ) -> Result<Vec<crate::instruction_state::ContextEntry>> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(CONTEXT_TABLE)?;
        let mut stmt = conn.prepare(
            "SELECT key, value, removed FROM session_context WHERE session_id = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value, gone) = row?;
            if gone && !removed {
                continue;
            }
            let value = match value {
                Some(v) => serde_json::from_str(&v)?,
                None => serde_json::Value::Null,
            };
            out.push(crate::instruction_state::ContextEntry {
                key,
                value,
                removed: gone,
            });
        }
        Ok(out)
    }

    /// Fork session `from` into a new session `to` through its latest
    /// message (see [`Store::fork_at`]).
    pub fn fork(&mut self, from: &str, to: &str) -> Result<Info> {
        self.fork_at(from, to, None)
    }

    /// Fork session `from` into a new top-level session `to`, as OpenCode's
    /// `Session.fork` does: its settled history through the latest message,
    /// or up to the message recorded at `before` (see [`crate::fork`]); its
    /// attached context and session meta; and an instruction baseline
    /// holding the newest values, so the fork starts from what its source
    /// last said (OpenCode seeds a fork the same way) while the copied
    /// updates stay in view. The fork starts with no usage and gives the
    /// model [`crate::fork::NOTICE`] before its first prompt. Fails with
    /// [`crate::fork::Empty`] for a session with no messages and
    /// [`crate::fork::MessageNotFound`] for a `before` that is not one.
    pub fn fork_at(&mut self, from: &str, to: &str, before: Option<i64>) -> Result<Info> {
        let parent = self
            .get(from)?
            .ok_or_else(|| anyhow!("no session {from}"))?;
        let entries = self.entries(from)?;
        let boundary = crate::fork::boundary(from, &entries, before)?;
        let t = now();
        let info = Info {
            id: to.to_string(),
            directory: parent.directory.clone(),
            title: crate::fork::title(parent.title.as_deref()),
            agent: parent.agent.clone(),
            model: parent.model.clone(),
            parent_id: None,
            created: t,
            updated: t,
            usage: Usage::default(),
        };
        self.import(&Export {
            info: info.clone(),
            entries: crate::fork::copied(&entries, boundary),
        })?;
        if let Some(state) = crate::instruction_state::fold(&entries) {
            self.append(
                to,
                &Record::Instructions {
                    changed: state.current,
                    removed: Vec::new(),
                    text: None,
                    baseline: true,
                    fork: true,
                },
            )?;
        }
        for c in self.context(from, true)? {
            if c.removed {
                let conn = self.conn.lock().unwrap();
                conn.execute(
                    "INSERT INTO session_context (session_id, key, value, removed) VALUES (?1, ?2, NULL, 1)",
                    params![to, c.key],
                )?;
            } else {
                self.put_context(to, &c.key, &c.value)?;
            }
        }
        {
            let conn = self.conn.lock().unwrap();
            conn.execute_batch(META_TABLE)?;
            // The parent's billing split, origin, archiving and read state
            // are not the fork's.
            conn.execute(
                "INSERT INTO session_meta (session_id, key, value)
                 SELECT ?2, key, value FROM session_meta
                 WHERE session_id = ?1
                   AND key NOT IN ('unbilled_usage', ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    from,
                    to,
                    crate::fork::ORIGIN_META,
                    crate::fork::NOTICE_META,
                    ARCHIVED_META,
                    IDLE_META,
                    VIEWED_META,
                    OUTCOME_META
                ],
            )?;
        }
        let origin = crate::fork::Origin {
            session: from.to_string(),
            boundary,
        };
        self.set_meta(
            to,
            crate::fork::ORIGIN_META,
            Some(&serde_json::to_string(&origin)?),
        )?;
        self.set_meta(to, crate::fork::NOTICE_META, Some("1"))?;
        Ok(info)
    }

    /// Archive session `id` (OpenCode's `Session.archive`): it stays, with
    /// its history, but lists leave it out unless asked for archived ones.
    /// Archiving it again keeps the first time. Fails with [`NotFound`].
    pub fn archive(&self, id: &str) -> Result<()> {
        if self.get(id)?.is_none() {
            return Err(NotFound { session: id.into() }.into());
        }
        if self.archived(id)?.is_none() {
            self.set_meta(id, ARCHIVED_META, Some(&now().to_string()))?;
        }
        Ok(())
    }

    /// Bring archived session `id` back. Fails with [`NotFound`].
    pub fn unarchive(&self, id: &str) -> Result<()> {
        if self.get(id)?.is_none() {
            return Err(NotFound { session: id.into() }.into());
        }
        self.set_meta(id, ARCHIVED_META, None)
    }

    /// Replace session `id`'s own permission rules (OpenCode's
    /// `Session.setPermissions`); an open session takes them when it is
    /// next opened. Fails with [`NotFound`].
    pub fn set_permissions(&self, id: &str, rules: &[crate::permission::Rule]) -> Result<()> {
        if self.get(id)?.is_none() {
            return Err(NotFound { session: id.into() }.into());
        }
        self.set_meta(id, PERMISSIONS_META, Some(&serde_json::to_string(rules)?))
    }

    /// Session `id`'s own permission rules, if it has any.
    pub fn permissions(&self, id: &str) -> Result<Option<Vec<crate::permission::Rule>>> {
        Ok(match self.meta(id, PERMISSIONS_META)? {
            Some(v) => Some(serde_json::from_str(&v)?),
            None => None,
        })
    }

    /// Record that session `id`'s turn ended with `outcome` (`succeeded`,
    /// `failed`, `interrupted`): its idle time moves forward, always past
    /// the last one, so a viewer's watermark from before it stays behind.
    /// The session's recency (`updated`) is left alone.
    pub fn set_idle(&self, id: &str, outcome: &str) -> Result<i64> {
        let last = self.read_state(id)?.idle.unwrap_or(0);
        let at = now().max(last + 1);
        self.set_meta(id, IDLE_META, Some(&at.to_string()))?;
        self.set_meta(id, OUTCOME_META, Some(outcome))?;
        Ok(at)
    }

    /// Session `id`'s read state.
    pub fn read_state(&self, id: &str) -> Result<ReadState> {
        let num =
            |k: &str| -> Result<Option<i64>> { Ok(self.meta(id, k)?.and_then(|v| v.parse().ok())) };
        Ok(ReadState {
            idle: num(IDLE_META)?,
            viewed: num(VIEWED_META)?,
            outcome: self.meta(id, OUTCOME_META)?,
        })
    }

    /// A viewer saw session `id` as of its idle time `idle` (OpenCode's
    /// `Session.view`): the viewed watermark moves to it, never past the
    /// session's idle time and never back, so a turn that ended after the
    /// viewer looked stays unread. Fails with [`NotFound`].
    pub fn view(&self, id: &str, idle: i64) -> Result<()> {
        if self.get(id)?.is_none() {
            return Err(NotFound { session: id.into() }.into());
        }
        let state = self.read_state(id)?;
        let fits = state.idle.is_some_and(|i| idle > 0 && idle <= i);
        if fits && state.viewed.is_none_or(|v| idle > v) {
            self.set_meta(id, VIEWED_META, Some(&idle.to_string()))?;
        }
        Ok(())
    }

    /// [`Store::list`] of the project at `root`, only the sessions started
    /// in its folder `subpath` (OpenCode's `subpath` filter). `subpath` is
    /// relative and stays inside the project.
    pub fn list_subpath(&self, root: &Path, subpath: &str, children: bool) -> Result<Vec<Info>> {
        let rel = Path::new(subpath);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(anyhow!("{subpath:?} is not a folder inside the project"));
        }
        self.list(Some(&root.join(rel).display().to_string()), children)
    }

    /// [`Store::list`], only the sessions whose title contains `text` (see
    /// [`title_matches`]).
    pub fn search(&self, directory: Option<&str>, children: bool, text: &str) -> Result<Vec<Info>> {
        let mut all = self.list(directory, children)?;
        all.retain(|i| i.title.as_deref().is_some_and(|t| title_matches(t, text)));
        Ok(all)
    }

    /// When session `id` was archived, if it is.
    pub fn archived(&self, id: &str) -> Result<Option<i64>> {
        Ok(self.meta(id, ARCHIVED_META)?.and_then(|v| v.parse().ok()))
    }

    /// [`Store::list`], only the archived sessions (`Some(true)`) or only
    /// the others (`Some(false)`).
    pub fn list_archived(
        &self,
        directory: Option<&str>,
        children: bool,
        archived: Option<bool>,
    ) -> Result<Vec<Info>> {
        let list = self.list(directory, children)?;
        let Some(want) = archived else {
            return Ok(list);
        };
        let mut out = Vec::new();
        for i in list {
            if self.archived(&i.id)?.is_some() == want {
                out.push(i);
            }
        }
        Ok(out)
    }

    /// Where session `id` was forked from, if it is a fork.
    pub fn fork_origin(&self, id: &str) -> Result<Option<crate::fork::Origin>> {
        Ok(self
            .meta(id, crate::fork::ORIGIN_META)?
            .and_then(|v| serde_json::from_str(&v).ok()))
    }

    /// Take the fork notice still owed to session `id`: true once, the
    /// first time after it was forked.
    pub fn take_fork_notice(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(META_TABLE)?;
        Ok(conn.execute(
            "DELETE FROM session_meta WHERE session_id = ?1 AND key = ?2",
            params![id, crate::fork::NOTICE_META],
        )? > 0)
    }

    pub fn export(&self, id: &str) -> Result<Export> {
        let info = self.get(id)?.ok_or_else(|| anyhow!("no session {id}"))?;
        Ok(Export {
            info,
            entries: self.entries(id)?,
        })
    }

    pub fn import(&mut self, e: &Export) -> Result<()> {
        if self.get(&e.info.id)?.is_some() {
            return Err(anyhow!("session {} already exists", e.info.id));
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO session (id, directory, title, agent, model, parent_id, created, updated, usage) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8)",
            params![e.info.id, e.info.directory, e.info.title, e.info.agent, e.info.model, e.info.created, e.info.updated, serde_json::to_string(&e.info.usage)?],
        )?;
        let blobs = self.media_blob();
        for en in &e.entries {
            // References carried in never reach blobs this session didn't
            // hold: they read as unavailable. Its own inline media becomes
            // blobs it references.
            let mut record = en.record.clone();
            if !crate::blob::hashes(&record).is_empty() {
                crate::blob::resolve(&mut record, &Default::default());
            }
            let hashes = if blobs {
                crate::blob::externalize(&tx, &mut record)?
            } else {
                Default::default()
            };
            tx.execute(
                "INSERT INTO record (session_id, seq, created, body) VALUES (?1, ?2, ?3, ?4)",
                params![
                    e.info.id,
                    en.seq,
                    en.created,
                    serde_json::to_string(&record)?
                ],
            )?;
            crate::blob::record_refs(&tx, &e.info.id, en.seq, &hashes)?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// An error inside a write transaction, as SQLite's so the busy retry
/// passes it on.
/// Write `record` as the next record of session `id` inside `tx` (see
/// [`Store::append_at`]).
fn insert_record(
    tx: &Connection,
    id: &str,
    record: &Record,
    t: i64,
    blobs: bool,
) -> rusqlite::Result<i64> {
    let blobs = blobs && crate::blob::has_inline(record);
    let ends_redo = matches!(
        record,
        Record::Message { .. } | Record::Checkpoint { .. } | Record::Native { .. }
    );
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM record WHERE session_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    let (body, hashes) = if blobs {
        let mut stored = record.clone();
        let hashes = crate::blob::externalize(tx, &mut stored).map_err(sql_err)?;
        (
            serde_json::to_string(&stored).map_err(|e| sql_err(e.into()))?,
            hashes,
        )
    } else {
        (
            serde_json::to_string(record).map_err(|e| sql_err(e.into()))?,
            crate::blob::hashes(record),
        )
    };
    tx.execute(
        "INSERT INTO record (session_id, seq, created, body) VALUES (?1, ?2, ?3, ?4)",
        params![id, seq, t, body],
    )?;
    crate::blob::record_refs(tx, id, seq, &hashes).map_err(sql_err)?;
    if ends_redo {
        crate::blob::release_undone(tx, id).map_err(sql_err)?;
    }
    tx.execute(
        "UPDATE session SET updated = ?2 WHERE id = ?1",
        params![id, t],
    )?;
    Ok(seq)
}

/// The sequence number of the message in session `id` that a
/// [`Store::admit`] tagged `key`, if there is one.
fn tagged(tx: &Connection, id: &str, key: &str) -> rusqlite::Result<Option<i64>> {
    tx.query_row(
        "SELECT json_extract(body, '$.seq') FROM record WHERE session_id = ?1
         AND json_extract(body, '$.kind') = 'metadata'
         AND json_extract(body, '$.metadata.automation.message') = ?2 LIMIT 1",
        params![id, key],
        |r| r.get(0),
    )
    .optional()
}

/// Whether a conversation still waits for the model: it ends with a
/// prompt or tool results, or with a response whose tool calls have no
/// results yet (not with a finished answer).
pub fn awaits_reply(messages: &[Message]) -> bool {
    messages.last().is_some_and(|m| {
        m.role != codesmith_ai::Role::Assistant
            || m.parts
                .iter()
                .any(|p| matches!(p, codesmith_ai::Part::ToolCall { .. }))
    })
}

fn sql_err(e: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(e.into())
}

/// Rebuild the model's view of a log.
pub fn history_of(entries: &[Entry]) -> Vec<Message> {
    history_with_seqs(entries)
        .into_iter()
        .map(|(_, m)| m)
        .collect()
}

/// The model's view of a log, each message with its sequence number (`None`
/// for the checkpoint that opens it).
pub fn history_with_seqs(entries: &[Entry]) -> Vec<(Option<i64>, Message)> {
    history_for(entries, None)
}

/// [`history_with_seqs`] for `model` (a model spec): a native window
/// another model made cannot be replayed, so history then starts from the
/// latest checkpoint that can (an earlier native window of this model, or a
/// summary), with the original messages the skipped window stood for.
pub fn history_for(entries: &[Entry], model: Option<&str>) -> Vec<(Option<i64>, Message)> {
    let hidden = hidden_ranges(entries);
    let visible = |seq: i64| !hidden.iter().any(|(from, to)| seq >= *from && seq < *to);
    let base = model.map(|m| crate::config::split_variant(m).0);
    let usable = |r: &Record| match r {
        Record::Checkpoint { .. } => true,
        Record::Native { model: made, .. } => base.is_none_or(|b| b == made),
        _ => false,
    };
    let last_checkpoint = entries
        .iter()
        .rposition(|e| usable(&e.record) && visible(e.seq));
    let (mut out, keep_from) = match last_checkpoint.map(|i| &entries[i].record) {
        Some(Record::Checkpoint { summary, keep_from }) => (
            vec![(None, Message::user(checkpoint_text(summary)))],
            *keep_from,
        ),
        Some(Record::Native {
            window, keep_from, ..
        }) => (
            window.iter().map(|m| (None, m.clone())).collect(),
            *keep_from,
        ),
        _ => (Vec::new(), 0),
    };
    // Instruction updates before the baseline's epoch are part of it.
    let epoch = crate::instruction_state::fold(entries).map_or(i64::MAX, |s| s.epoch);
    for e in entries {
        if e.seq < keep_from || !visible(e.seq) {
            continue;
        }
        match &e.record {
            Record::Message { message } => out.push((Some(e.seq), message.clone())),
            Record::Instructions {
                text: Some(text), ..
            } if e.seq >= epoch => out.push((Some(e.seq), Message::system(text.clone()))),
            _ => {}
        }
    }
    out
}

/// `[from, to)` sequence ranges hidden by reverts that have not been redone.
pub fn hidden_ranges(entries: &[Entry]) -> Vec<(i64, i64)> {
    hidden_ranges_except(entries, None)
}

/// The end-of-turn trees recorded after the turn snapshot at `from_seq`,
/// treating the revert at `except` (if any) as not applied. Empty for
/// turns recorded before end snapshots existed, or stopped before the end.
pub fn turn_ends(entries: &[Entry], from_seq: i64, except: Option<i64>) -> Vec<String> {
    let hidden = hidden_ranges_except(entries, except);
    let visible = |seq: i64| !hidden.iter().any(|(from, to)| seq >= *from && seq < *to);
    entries
        .iter()
        .filter(|e| e.seq > from_seq && except.is_none_or(|r| e.seq < r) && visible(e.seq))
        .filter_map(|e| match &e.record {
            Record::TurnEnd { tree } => Some(tree.clone()),
            _ => None,
        })
        .collect()
}

fn hidden_ranges_except(entries: &[Entry], except: Option<i64>) -> Vec<(i64, i64)> {
    let redone: Vec<i64> = entries
        .iter()
        .filter_map(|e| {
            if let Record::Redo { revert_seq } = e.record {
                Some(revert_seq)
            } else {
                None
            }
        })
        .collect();
    entries
        .iter()
        .filter_map(|e| match e.record {
            Record::Revert { from_seq, .. }
                if !redone.contains(&e.seq) && Some(e.seq) != except =>
            {
                Some((from_seq, e.seq))
            }
            _ => None,
        })
        .collect()
}

/// What `undo` would revert: the newest visible turn that has messages,
/// as (its snapshot record's seq, the snapshot tree).
pub fn undo_target(entries: &[Entry]) -> Option<(i64, String)> {
    let hidden = hidden_ranges(entries);
    let visible = |seq: i64| !hidden.iter().any(|(from, to)| seq >= *from && seq < *to);
    entries
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, e)| match &e.record {
            Record::Snapshot { tree } if visible(e.seq) => {
                let has_messages = entries[i + 1..]
                    .iter()
                    .any(|n| matches!(n.record, Record::Message { .. }) && visible(n.seq));
                has_messages.then(|| (e.seq, tree.clone()))
            }
            _ => None,
        })
}

/// What `redo` would restore: the newest revert not yet redone, as long as
/// no new messages were added after it. Returns (its seq, the tree to restore).
pub fn redo_target(entries: &[Entry]) -> Option<(i64, String)> {
    let redone: Vec<i64> = entries
        .iter()
        .filter_map(|e| {
            if let Record::Redo { revert_seq } = e.record {
                Some(revert_seq)
            } else {
                None
            }
        })
        .collect();
    for e in entries.iter().rev() {
        match &e.record {
            Record::Message { .. } | Record::Checkpoint { .. } | Record::Native { .. } => {
                return None
            }
            Record::Revert { after, .. } if !redone.contains(&e.seq) => {
                return Some((e.seq, after.clone()))
            }
            _ => {}
        }
    }
    None
}

/// Plugins' own key-value storage (OpenCode's plugin storage), one
/// namespace per plugin id.
const PLUGIN_TABLE: &str = "CREATE TABLE IF NOT EXISTS plugin_storage (
    plugin TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
    PRIMARY KEY (plugin, key));";

impl Store {
    /// What plugin `plugin` stored under `key`.
    pub fn plugin_get(&self, plugin: &str, key: &str) -> Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(PLUGIN_TABLE)?;
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM plugin_storage WHERE plugin = ?1 AND key = ?2",
                params![plugin, key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|v| serde_json::from_str(&v).ok()))
    }

    /// Store `value` for plugin `plugin` under `key`.
    pub fn plugin_set(&self, plugin: &str, key: &str, value: &serde_json::Value) -> Result<()> {
        let json = serde_json::to_string(value)?;
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(PLUGIN_TABLE)?;
        conn.execute(
            "INSERT INTO plugin_storage (plugin, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(plugin, key) DO UPDATE SET value = ?3",
            params![plugin, key, json],
        )?;
        Ok(())
    }

    pub fn plugin_remove(&self, plugin: &str, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(PLUGIN_TABLE)?;
        conn.execute(
            "DELETE FROM plugin_storage WHERE plugin = ?1 AND key = ?2",
            params![plugin, key],
        )?;
        Ok(())
    }

    /// Plugin `plugin`'s entries whose keys start with `prefix`, by key.
    pub fn plugin_scan(
        &self,
        plugin: &str,
        prefix: &str,
    ) -> Result<Vec<(String, serde_json::Value)>> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(PLUGIN_TABLE)?;
        let mut stmt = conn.prepare(
            "SELECT key, value FROM plugin_storage WHERE plugin = ?1 AND substr(key, 1, length(?2)) = ?2 ORDER BY key",
        )?;
        let rows = stmt
            .query_map(params![plugin, prefix], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(k, v)| Some((k, serde_json::from_str(&v).ok()?)))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesmith_ai::{Part, Role};

    /// Regression: `append_at` read MAX(seq) and inserted outside a
    /// transaction, so two connections appending to one session could pick
    /// the same number and fail with "UNIQUE constraint failed".
    #[test]
    fn concurrent_appends_from_two_connections_get_distinct_seqs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let db = Store::open(&path).unwrap();
        db.create(&Info {
            id: "s".into(),
            directory: "/p".into(),
            title: None,
            agent: "build".into(),
            model: "m".into(),
            parent_id: None,
            created: 0,
            updated: 0,
            usage: Usage::default(),
        })
        .unwrap();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let st = Store::open(&path).unwrap();
                    for _ in 0..50 {
                        st.append("s", &Record::Unknown).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let seqs: Vec<i64> = db.entries("s").unwrap().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=200).collect::<Vec<_>>());
        assert_eq!(db.last_seq("s").unwrap(), 200);
    }

    #[test]
    fn session_turn_leases() {
        let db = Store::memory().unwrap();
        assert_eq!(db.session_holder("s").unwrap(), None);
        assert!(db.lease_session("s", "a", 10_000).unwrap());
        assert!(db.lease_session("s", "a", 10_000).unwrap(), "renewing");
        assert!(!db.lease_session("s", "b", 10_000).unwrap());
        assert_eq!(db.session_holder("s").unwrap().as_deref(), Some("a"));
        db.release_session("s", "b").unwrap();
        assert_eq!(
            db.session_holder("s").unwrap().as_deref(),
            Some("a"),
            "only the holder releases"
        );
        db.release_session("s", "a").unwrap();
        assert!(db.lease_session("s", "b", -1).unwrap());
        assert_eq!(db.session_holder("s").unwrap(), None, "expired");
        assert!(
            db.lease_session("s", "a", 10_000).unwrap(),
            "an expired lease is taken over"
        );
    }

    #[tokio::test]
    async fn a_turn_lease_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let lease = TurnLease::acquire(&path, "s", "a", false, || {})
            .await
            .unwrap();
        let busy = TurnLease::acquire(&path, "s", "b", false, || {}).await;
        assert!(busy.err().unwrap().downcast_ref::<Busy>().is_some());
        drop(lease);
        let _b = TurnLease::acquire(&path, "s", "b", false, || {})
            .await
            .unwrap();
    }

    /// Regression: a turn cancelled while its lease was being taken (an ACP
    /// `session/cancel` right after the prompt) left the lease taken with
    /// no one to release it, so the session's next turn waited 30 seconds
    /// for it to expire.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lease_taken_by_a_cancelled_acquire_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        drop(Store::open(&path).unwrap());
        for _ in 0..20 {
            {
                let acquire = TurnLease::acquire(&path, "s", "a", false, || {});
                tokio::pin!(acquire);
                // Started (the lease is being taken), then cancelled.
                let _ = futures_util::poll!(acquire.as_mut());
            }
            // The next turn gets it well before the 30-second expiry.
            let next = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                TurnLease::acquire(&path, "s", "b", true, || {}),
            )
            .await
            .expect("the lease was never released");
            drop(next.unwrap());
        }
    }

    /// Regression: a second process opening a new database while the first
    /// was creating its tables failed with "table session already exists"
    /// (`serve`'s scheduler and its first request did this).
    #[test]
    fn opening_a_database_another_opener_is_still_creating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let first = Store::open(&path).unwrap();
        first
            .with_conn(|c| Ok(c.execute_batch("PRAGMA user_version = 0;")?))
            .unwrap();
        let second = Store::open(&path).unwrap();
        second.create(&info("a", "/d", 1)).unwrap();
        assert_eq!(first.get("a").unwrap().unwrap().id, "a");
    }

    #[test]
    fn set_directory_moves_a_session_and_its_children() {
        let db = Store::memory().unwrap();
        db.create(&info("p", "/a", 1)).unwrap();
        let mut child = info("c", "/a", 2);
        child.parent_id = Some("p".into());
        db.create(&child).unwrap();
        db.create(&info("other", "/a", 3)).unwrap();
        assert!(db.set_directory("p", "/b").unwrap());
        assert_eq!(db.get("p").unwrap().unwrap().directory, "/b");
        assert_eq!(db.get("c").unwrap().unwrap().directory, "/b");
        assert_eq!(db.get("other").unwrap().unwrap().directory, "/a");
        assert!(!db.set_directory("missing", "/b").unwrap());
    }

    fn info(id: &str, dir: &str, updated: i64) -> Info {
        Info {
            id: id.into(),
            directory: dir.into(),
            title: None,
            agent: "build".into(),
            model: "p/m".into(),
            parent_id: None,
            created: updated,
            updated,
            usage: Usage::default(),
        }
    }

    fn msg(role: Role, t: &str) -> Record {
        Record::Message {
            message: Message {
                role,
                parts: vec![Part::Text(t.into())],
            },
        }
    }

    #[test]
    fn create_append_and_reload() {
        let s = Store::memory().unwrap();
        s.create(&info("a", "/p", 1)).unwrap();
        assert_eq!(s.append("a", &msg(Role::User, "hi")).unwrap(), 1);
        assert_eq!(s.append("a", &msg(Role::Assistant, "hello")).unwrap(), 2);
        let h = s.history("a").unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[1].parts[0], Part::Text("hello".into()));
        assert!(
            s.get("a").unwrap().unwrap().updated > 1,
            "append touches updated"
        );
        assert!(s.get("zzz").unwrap().is_none());
    }

    #[test]
    fn checkpoint_replaces_older_history() {
        let s = Store::memory().unwrap();
        s.create(&info("a", "/p", 1)).unwrap();
        for i in 0..4 {
            s.append("a", &msg(Role::User, &format!("m{i}"))).unwrap();
        }
        s.append(
            "a",
            &Record::Checkpoint {
                summary: "S1".into(),
                keep_from: 3,
            },
        )
        .unwrap();
        s.append("a", &msg(Role::User, "after")).unwrap();
        let h = s.history("a").unwrap();
        let texts: Vec<String> = h
            .iter()
            .map(|m| match &m.parts[0] {
                Part::Text(t) => t.to_string(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(texts[0], checkpoint_text("S1"));
        assert_eq!(&texts[1..], ["m2", "m3", "after"]);
        s.append(
            "a",
            &Record::Checkpoint {
                summary: "S2".into(),
                keep_from: 7,
            },
        )
        .unwrap();
        let h = s.history("a").unwrap();
        assert_eq!(h.len(), 1, "a newer checkpoint wins");
        assert!(matches!(&h[0].parts[0], Part::Text(t) if t.contains("S2")));
    }

    fn entry(seq: i64, record: Record) -> Entry {
        Entry {
            seq,
            created: 0,
            record,
        }
    }

    #[test]
    fn undo_and_redo_targets() {
        let snap = |s: &str| Record::Snapshot { tree: s.into() };
        let mut log = vec![
            entry(1, snap("t1")),
            entry(2, msg(Role::User, "first")),
            entry(3, msg(Role::Assistant, "done 1")),
            entry(4, snap("t2")),
            entry(5, msg(Role::User, "second")),
            entry(6, msg(Role::Assistant, "done 2")),
        ];
        assert_eq!(undo_target(&log), Some((4, "t2".into())));
        assert_eq!(redo_target(&log), None);
        log.push(entry(
            7,
            Record::Revert {
                from_seq: 4,
                after: "t3".into(),
            },
        ));
        assert_eq!(history_of(&log).len(), 2, "the second turn is hidden");
        assert_eq!(
            undo_target(&log),
            Some((1, "t1".into())),
            "a second undo goes further back"
        );
        assert_eq!(redo_target(&log), Some((7, "t3".into())));
        log.push(entry(
            8,
            Record::Revert {
                from_seq: 1,
                after: "t2b".into(),
            },
        ));
        assert!(history_of(&log).is_empty());
        assert_eq!(undo_target(&log), None);
        assert_eq!(
            redo_target(&log),
            Some((8, "t2b".into())),
            "redo pops the newest undo first"
        );
        log.push(entry(9, Record::Redo { revert_seq: 8 }));
        assert_eq!(history_of(&log).len(), 2);
        assert_eq!(redo_target(&log), Some((7, "t3".into())));
        log.push(entry(10, Record::Redo { revert_seq: 7 }));
        assert_eq!(history_of(&log).len(), 4);
        assert_eq!(redo_target(&log), None);
    }

    #[test]
    fn a_new_turn_after_undo_makes_it_permanent() {
        let log = vec![
            entry(1, Record::Snapshot { tree: "t1".into() }),
            entry(2, msg(Role::User, "a")),
            entry(
                3,
                Record::Revert {
                    from_seq: 1,
                    after: "t2".into(),
                },
            ),
            entry(4, Record::Snapshot { tree: "t1".into() }),
            entry(5, msg(Role::User, "b")),
        ];
        assert_eq!(redo_target(&log), None);
        assert_eq!(history_of(&log).len(), 1);
    }

    #[test]
    fn hidden_checkpoints_are_ignored() {
        let log = vec![
            entry(1, msg(Role::User, "a")),
            entry(2, Record::Snapshot { tree: "t".into() }),
            entry(3, msg(Role::User, "b")),
            entry(
                4,
                Record::Checkpoint {
                    summary: "S".into(),
                    keep_from: 3,
                },
            ),
            entry(
                5,
                Record::Revert {
                    from_seq: 2,
                    after: "t2".into(),
                },
            ),
        ];
        let h = history_of(&log);
        assert_eq!(h.len(), 1);
        assert!(matches!(&h[0].parts[0], Part::Text(t) if t == "a"));
    }

    #[test]
    fn latest_and_listing() {
        let s = Store::memory().unwrap();
        s.create(&info("old", "/p", 1)).unwrap();
        s.create(&info("new", "/p", 5)).unwrap();
        s.create(&info("other", "/q", 9)).unwrap();
        let mut child = info("kid", "/p", 10);
        child.parent_id = Some("new".into());
        s.create(&child).unwrap();
        assert_eq!(s.latest("/p").unwrap().unwrap().id, "new");
        assert_eq!(
            s.list(Some("/p"), false)
                .unwrap()
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["new", "old"]
        );
        assert_eq!(s.list(None, true).unwrap().len(), 4);
        assert!(s.latest("/none").unwrap().is_none());
    }

    #[test]
    fn title_usage_and_delete_cascade() {
        let s = Store::memory().unwrap();
        s.create(&info("p", "/p", 1)).unwrap();
        let mut c = info("c", "/p", 2);
        c.parent_id = Some("p".into());
        s.create(&c).unwrap();
        s.append("c", &msg(Role::User, "x")).unwrap();
        s.set_title("p", "Config refactor").unwrap();
        s.set_usage(
            "p",
            &Usage {
                input: 5,
                output: 6,
                cache_read: 7,
                cache_write: 8,
                ..Default::default()
            },
        )
        .unwrap();
        let p = s.get("p").unwrap().unwrap();
        assert_eq!(p.title.as_deref(), Some("Config refactor"));
        assert_eq!(p.usage.cache_read, 7);
        assert!(s.delete("p").unwrap());
        assert!(s.get("c").unwrap().is_none(), "children go too");
        assert!(s.entries("c").unwrap().is_empty(), "records cascade");
        assert!(!s.delete("p").unwrap());
    }

    #[test]
    fn export_import_round_trip() {
        let s = Store::memory().unwrap();
        s.create(&info("a", "/p", 1)).unwrap();
        s.append("a", &msg(Role::User, "hi")).unwrap();
        s.append(
            "a",
            &Record::Checkpoint {
                summary: "S".into(),
                keep_from: 2,
            },
        )
        .unwrap();
        let e = s.export("a").unwrap();
        let text = serde_json::to_string(&e).unwrap();
        let mut t = Store::memory().unwrap();
        t.import(&serde_json::from_str(&text).unwrap()).unwrap();
        assert_eq!(t.export("a").unwrap(), e);
        assert!(t.import(&e).is_err(), "no duplicate ids");
    }

    #[test]
    fn reopening_a_file_keeps_data() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("db/codesmith.db");
        {
            let s = Store::open(&path).unwrap();
            s.create(&info("a", "/p", 1)).unwrap();
            s.append("a", &msg(Role::User, "persisted")).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.history("a").unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn database_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("codesmith.db");
        {
            let s = Store::open(&path).unwrap();
            s.create(&info("a", "/p", 1)).unwrap();
            assert_eq!(mode(&path), 0o600, "a new database is created 0600");
            let wal = d.path().join("codesmith.db-wal");
            if wal.exists() {
                assert_eq!(mode(&wal), 0o600);
            }
        }
        // An existing world-readable database is tightened on open.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let s = Store::open(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(s.history("a").unwrap().len(), 0);
    }

    #[test]
    fn newer_schema_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        assert!(Store::init(conn).unwrap_err().to_string().contains("newer"));
    }

    #[test]
    fn session_meta_is_kept_and_deleted_with_the_session() {
        let d = tempfile::tempdir().unwrap();
        let st = Store::open(&d.path().join("s.db")).unwrap();
        assert_eq!(st.meta("s", "k").unwrap(), None);
        st.set_meta("s", "k", Some("a")).unwrap();
        st.set_meta("s", "k", Some("b")).unwrap();
        assert_eq!(st.meta("s", "k").unwrap().as_deref(), Some("b"));
        st.set_meta("s", "k", None).unwrap();
        assert_eq!(st.meta("s", "k").unwrap(), None);
        st.set_meta("s", "k", Some("c")).unwrap();
        st.delete("s").unwrap();
        assert_eq!(st.meta("s", "k").unwrap(), None);
    }
}
