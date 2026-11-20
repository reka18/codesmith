//! Durable sessions in SQLite: one row per session and an append-only log of
//! records (messages and compaction checkpoints). History for the model is
//! rebuilt from the log, starting at the latest checkpoint.

use anyhow::{anyhow, Context, Result};
use codesmith_ai::{Message, Usage};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
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
    /// The project's files (a snapshot tree) at the start of a turn.
    Snapshot {
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
    /// A record kind from a newer CodeSmith; ignored.
    #[serde(other)]
    Unknown,
}

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
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
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

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
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
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(anyhow!("database schema {version} is newer than this codesmith understands ({SCHEMA_VERSION}); upgrade codesmith"));
        }
        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE session (
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
                CREATE INDEX session_directory ON session(directory, updated);
                CREATE TABLE record (
                    session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
                    seq INTEGER NOT NULL,
                    created INTEGER NOT NULL,
                    body TEXT NOT NULL,
                    PRIMARY KEY (session_id, seq)
                );
                PRAGMA user_version = 1;",
            )?;
        }
        Ok(Store {
            conn: std::sync::Mutex::new(conn),
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

    /// The most recently updated top-level session in `directory`.
    pub fn latest(&self, directory: &str) -> Result<Option<Info>> {
        Ok(self.list(Some(directory), false)?.into_iter().next())
    }

    pub fn append(&self, id: &str, record: &Record) -> Result<i64> {
        self.append_at(id, record, now())
    }

    /// Append with an explicit time (milliseconds), e.g. when importing.
    pub fn append_at(&self, id: &str, record: &Record, t: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM record WHERE session_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO record (session_id, seq, created, body) VALUES (?1, ?2, ?3, ?4)",
            params![id, seq, t, serde_json::to_string(record)?],
        )?;
        conn.execute(
            "UPDATE session SET updated = ?2 WHERE id = ?1",
            params![id, t],
        )?;
        Ok(seq)
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
            out.push(Entry {
                seq,
                created,
                record: serde_json::from_str(&body)
                    .with_context(|| format!("record {seq} of {id}"))?,
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
        Ok(conn.execute("DELETE FROM session WHERE id = ?1", [id])? > 0)
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
        for en in &e.entries {
            tx.execute(
                "INSERT INTO record (session_id, seq, created, body) VALUES (?1, ?2, ?3, ?4)",
                params![
                    e.info.id,
                    en.seq,
                    en.created,
                    serde_json::to_string(&en.record)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
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
    let hidden = hidden_ranges(entries);
    let visible = |seq: i64| !hidden.iter().any(|(from, to)| seq >= *from && seq < *to);
    let last_checkpoint = entries
        .iter()
        .rposition(|e| matches!(e.record, Record::Checkpoint { .. }) && visible(e.seq));
    let (mut out, keep_from) = match last_checkpoint.map(|i| &entries[i].record) {
        Some(Record::Checkpoint { summary, keep_from }) => (
            vec![(None, Message::user(checkpoint_text(summary)))],
            *keep_from,
        ),
        _ => (Vec::new(), 0),
    };
    for e in entries {
        if let Record::Message { message } = &e.record {
            if e.seq >= keep_from && visible(e.seq) {
                out.push((Some(e.seq), message.clone()));
            }
        }
    }
    out
}

/// `[from, to)` sequence ranges hidden by reverts that have not been redone.
pub fn hidden_ranges(entries: &[Entry]) -> Vec<(i64, i64)> {
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
            Record::Revert { from_seq, .. } if !redone.contains(&e.seq) => Some((from_seq, e.seq)),
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
            Record::Message { .. } | Record::Checkpoint { .. } => return None,
            Record::Revert { after, .. } if !redone.contains(&e.seq) => {
                return Some((e.seq, after.clone()))
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesmith_ai::{Part, Role};

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
                Part::Text(t) => t.clone(),
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

    #[test]
    fn newer_schema_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        assert!(Store::init(conn).unwrap_err().to_string().contains("newer"));
    }
}
