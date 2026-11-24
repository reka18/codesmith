//! The broker's store (SQLite, written by the broker alone): messages
//! until they are handled or expire, the name each session holds, and the
//! sessions shared to relays. Everything a broker accepts is here before it
//! is acknowledged, so a broker that dies loses nothing it said it took.

use super::protocol::{Kind, Message};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// How long a message is kept, handled or not.
pub const KEEP_MS: i64 = 7 * 24 * 3600 * 1000;
/// How long a session that hasn't been on the bus keeps its name.
pub const NAME_MS: i64 = 30 * 24 * 3600 * 1000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS message (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    from_session TEXT NOT NULL,
    from_name TEXT NOT NULL,
    to_session TEXT NOT NULL,
    to_name TEXT NOT NULL,
    reply_to TEXT,
    body TEXT NOT NULL,
    sent INTEGER NOT NULL,
    hop INTEGER NOT NULL DEFAULT 0,
    project TEXT NOT NULL DEFAULT '',
    via TEXT,
    relay_key TEXT,
    state TEXT NOT NULL DEFAULT 'queued',
    answered INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS message_to ON message (to_session, state);
CREATE INDEX IF NOT EXISTS message_reply ON message (reply_to);
CREATE UNIQUE INDEX IF NOT EXISTS message_key ON message (relay_key) WHERE relay_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS name (
    name TEXT PRIMARY KEY,
    session TEXT NOT NULL UNIQUE,
    project TEXT NOT NULL DEFAULT '',
    seen INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS share (
    name TEXT PRIMARY KEY,
    session TEXT NOT NULL UNIQUE,
    project TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL
);
";

pub struct Mailbox {
    conn: Connection,
}

/// A message's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Queued,
    Delivered,
    Acked,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Delivered => "delivered",
            State::Acked => "acked",
        }
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    let kind: String = r.get("kind")?;
    Ok(Message {
        id: r.get("id")?,
        kind: Kind::parse(&kind).unwrap_or(Kind::Request),
        from: r.get("from_session")?,
        from_name: r.get("from_name")?,
        to: r.get("to_session")?,
        to_name: r.get("to_name")?,
        reply_to: r.get("reply_to")?,
        body: r.get("body")?,
        sent: r.get("sent")?,
        hop: r.get::<_, i64>("hop")? as u32,
        project: r.get("project")?,
        via: r.get("via")?,
    })
}

impl Mailbox {
    /// Open (creating, 0600 in a 0700 directory) the mailbox at `path`.
    pub fn open(path: &Path) -> Result<Mailbox> {
        if let Some(dir) = path.parent() {
            crate::daemon::fs::secure_dir(dir)
                .with_context(|| format!("the bus directory {}", dir.display()))?;
        }
        let new = !path.exists();
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        if new {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Mailbox { conn })
    }

    /// An in-memory mailbox, for tests.
    pub fn memory() -> Result<Mailbox> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Mailbox { conn })
    }

    pub fn insert(&self, m: &Message, state: State, relay_key: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO message (id, kind, from_session, from_name, to_session, to_name,
                reply_to, body, sent, hop, project, via, relay_key, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                m.id,
                m.kind.as_str(),
                m.from,
                m.from_name,
                m.to,
                m.to_name,
                m.reply_to,
                m.body,
                m.sent,
                m.hop as i64,
                m.project,
                m.via,
                relay_key,
                state.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<Message>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM message WHERE id = ?1", [id], row)
            .optional()?)
    }

    /// The message a relay carried with `key`.
    pub fn by_key(&self, key: &str) -> Result<Option<Message>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM message WHERE relay_key = ?1", [key], row)
            .optional()?)
    }

    /// The response to request `id`, if one was sent.
    pub fn response_to(&self, id: &str) -> Result<Option<Message>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM message WHERE reply_to = ?1 AND kind = 'response'",
                [id],
                row,
            )
            .optional()?)
    }

    pub fn set_state(&self, id: &str, state: State) -> Result<()> {
        self.conn.execute(
            "UPDATE message SET state = ?2 WHERE id = ?1",
            params![id, state.as_str()],
        )?;
        Ok(())
    }

    /// Point a request's replies at another sender session (a relay that
    /// replaced the one that carried it).
    pub fn set_from(&self, id: &str, session: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE message SET from_session = ?2 WHERE id = ?1",
            params![id, session],
        )?;
        Ok(())
    }

    /// Address message `id` to another session, to be delivered again.
    pub fn set_to(&self, id: &str, session: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE message SET to_session = ?2, state = 'queued' WHERE id = ?1",
            params![id, session],
        )?;
        Ok(())
    }

    /// Mark request `id` answered; false if it already was.
    pub fn mark_answered(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE message SET answered = 1 WHERE id = ?1 AND answered = 0",
            [id],
        )?;
        Ok(n == 1)
    }

    pub fn answered(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT answered FROM message WHERE id = ?1", [id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some_and(|a| a == 1))
    }

    /// Messages to `session` not yet handled, in the order they were sent
    /// (ids made in the same millisecond don't sort by time); requests only
    /// when `requests`.
    pub fn pending(&self, session: &str, requests: bool) -> Result<Vec<Message>> {
        let mut st = self.conn.prepare(
            "SELECT * FROM message WHERE to_session = ?1 AND state != 'acked'
               AND (?2 OR kind != 'request') ORDER BY rowid",
        )?;
        let rows = st.query_map(params![session, requests], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Messages not yet handled, across every session.
    pub fn unhandled(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM message WHERE state != 'acked'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Requests `session` sent since `since`.
    pub fn requests_since(&self, session: &str, since: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM message WHERE from_session = ?1 AND kind = 'request' AND sent >= ?2",
            params![session, since],
            |r| r.get(0),
        )?)
    }

    /// Drop messages older than [`KEEP_MS`] and names unused for
    /// [`NAME_MS`] (a shared session's name is kept while it is shared).
    pub fn prune(&self, now: i64) -> Result<usize> {
        let n = self
            .conn
            .execute("DELETE FROM message WHERE sent < ?1", [now - KEEP_MS])?;
        self.conn.execute(
            "DELETE FROM name WHERE seen < ?1 AND session NOT IN (SELECT session FROM share)",
            [now - NAME_MS],
        )?;
        Ok(n)
    }

    // --- names ---

    /// The session holding `name`.
    pub fn name_owner(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT session FROM name WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// The name `session` holds.
    pub fn name_of(&self, session: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT name FROM name WHERE session = ?1", [session], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Give `name` to `session` (taking it from whoever held it, and
    /// dropping the session's old name).
    pub fn set_name(&self, session: &str, name: &str, project: &str, now: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM name WHERE session = ?1 OR name = ?2",
            params![session, name],
        )?;
        tx.execute(
            "INSERT INTO name (name, session, project, seen) VALUES (?1, ?2, ?3, ?4)",
            params![name, session, project, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn touch_name(&self, session: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE name SET seen = ?2 WHERE session = ?1",
            params![session, now],
        )?;
        Ok(())
    }

    // --- shares ---

    /// Share `session` as `name`, replacing its earlier share. Refused when
    /// another session has that name shared.
    pub fn share(&self, session: &str, name: &str, project: &str, now: i64) -> Result<bool> {
        if let Some((other, _)) = self.shared(name)? {
            if other != session {
                return Ok(false);
            }
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM share WHERE session = ?1", [session])?;
        tx.execute(
            "INSERT INTO share (name, session, project, created) VALUES (?1, ?2, ?3, ?4)",
            params![name, session, project, now],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn unshare(&self, session: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM share WHERE session = ?1", [session])?
            == 1)
    }

    /// The session shared as `name`, and its project.
    pub fn shared(&self, name: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT session, project FROM share WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// The name `session` is shared as.
    pub fn share_of(&self, session: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name FROM share WHERE session = ?1",
                [session],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::protocol::new_id;

    fn msg(kind: Kind, from: &str, to: &str, sent: i64) -> Message {
        Message {
            id: new_id(),
            kind,
            from: from.into(),
            from_name: from.into(),
            to: to.into(),
            to_name: to.into(),
            reply_to: None,
            body: "b".into(),
            sent,
            hop: 0,
            project: "/p".into(),
            via: None,
        }
    }

    #[test]
    fn pending_is_in_order_and_skips_handled() {
        let mb = Mailbox::memory().unwrap();
        let mut ids = vec![];
        for i in 0..5 {
            let m = msg(Kind::Request, "a", "b", i);
            mb.insert(&m, State::Queued, None).unwrap();
            ids.push(m.id);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let p = mb.pending("b", true).unwrap();
        assert_eq!(p.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), ids);
        mb.set_state(&ids[1], State::Acked).unwrap();
        mb.set_state(&ids[2], State::Delivered).unwrap();
        let p = mb.pending("b", true).unwrap();
        assert_eq!(p.len(), 4, "delivered but not handled stays pending");
        assert!(
            mb.pending("b", false).unwrap().is_empty(),
            "no requests asked for"
        );
        assert_eq!(mb.unhandled().unwrap(), 4);
    }

    #[test]
    fn a_request_is_answered_once() {
        let mb = Mailbox::memory().unwrap();
        let m = msg(Kind::Request, "a", "b", 1);
        mb.insert(&m, State::Queued, None).unwrap();
        assert!(!mb.answered(&m.id).unwrap());
        assert!(mb.mark_answered(&m.id).unwrap());
        assert!(!mb.mark_answered(&m.id).unwrap());
        assert!(mb.answered(&m.id).unwrap());
    }

    #[test]
    fn names_move_and_are_unique() {
        let mb = Mailbox::memory().unwrap();
        mb.set_name("s1", "reviewer", "/p", 1).unwrap();
        assert_eq!(mb.name_owner("reviewer").unwrap().as_deref(), Some("s1"));
        // A session renamed drops its old name.
        mb.set_name("s1", "critic", "/p", 2).unwrap();
        assert_eq!(mb.name_owner("reviewer").unwrap(), None);
        // Another session taking a name takes it whole.
        mb.set_name("s2", "critic", "/p", 3).unwrap();
        assert_eq!(mb.name_owner("critic").unwrap().as_deref(), Some("s2"));
        assert_eq!(mb.name_of("s1").unwrap(), None);
    }

    #[test]
    fn shares_belong_to_one_session() {
        let mb = Mailbox::memory().unwrap();
        assert!(mb.share("s1", "reviewer", "/p", 1).unwrap());
        assert!(!mb.share("s2", "reviewer", "/q", 1).unwrap(), "taken");
        assert!(
            mb.share("s1", "critic", "/p", 2).unwrap(),
            "re-sharing renames"
        );
        assert_eq!(mb.shared("reviewer").unwrap(), None);
        assert_eq!(mb.share_of("s1").unwrap().as_deref(), Some("critic"));
        assert!(mb.unshare("s1").unwrap());
        assert!(!mb.unshare("s1").unwrap());
    }

    #[test]
    fn old_messages_expire() {
        let mb = Mailbox::memory().unwrap();
        let now = 10 * KEEP_MS;
        let old = msg(Kind::Request, "a", "b", now - KEEP_MS - 1);
        let new = msg(Kind::Request, "a", "b", now - 1);
        mb.insert(&old, State::Queued, None).unwrap();
        mb.insert(&new, State::Queued, None).unwrap();
        assert_eq!(mb.prune(now).unwrap(), 1);
        assert!(mb.get(&old.id).unwrap().is_none());
        assert!(mb.get(&new.id).unwrap().is_some());
    }

    #[test]
    fn relay_keys_are_unique() {
        let mb = Mailbox::memory().unwrap();
        let a = msg(Kind::Request, "r1", "b", 1);
        mb.insert(&a, State::Queued, Some("k")).unwrap();
        let b = msg(Kind::Request, "r2", "b", 2);
        assert!(mb.insert(&b, State::Queued, Some("k")).is_err());
        assert_eq!(mb.by_key("k").unwrap().unwrap().id, a.id);
    }

    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("bus").join("mailbox.db");
        Mailbox::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dmode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dmode & 0o777, 0o700);
    }
}
