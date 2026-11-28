//! `remote.db`: paired devices, their grants, the audit log, push tokens
//! and unlock keys. SQLite, owner-only (0600), like the session store.
//!
//! A device's id is the node id its connection authenticated, never
//! anything a device says about itself.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA_VERSION: i32 = 1;

/// What a grant lets a device do with a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Follow the session.
    Watch,
    /// Follow and drive it.
    Control,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Watch => "watch",
            Scope::Control => "control",
        }
    }

    fn parse(s: &str) -> Option<Scope> {
        match s {
            "watch" => Some(Scope::Watch),
            "control" => Some(Scope::Control),
            _ => None,
        }
    }
}

/// The session a grant covers: one, or every session on this machine,
/// now and future.
pub const ALL_SESSIONS: &str = "*";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// A session id, or [`ALL_SESSIONS`].
    pub session: String,
    pub scope: Scope,
    pub granted: i64,
}

/// A passkey the device registered for unlocking terminals (T4): ES256,
/// its DER SubjectPublicKeyInfo as standard base64 with padding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlockKey {
    pub alg: String,
    pub spki: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// The device's node id, lower-case hex.
    pub id: String,
    pub name: String,
    pub platform: String,
    pub paired: i64,
    /// When it was revoked; a revoked device is no longer paired.
    pub revoked: Option<i64>,
    pub unlock_key: Option<UnlockKey>,
    pub grants: Vec<Grant>,
}

/// A push token a device registered (`push` in its claim).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Push {
    pub kind: String,
    pub token: String,
}

/// One audit entry: what a device (or the terminal) did, never content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Audit {
    pub ts: i64,
    pub device: Option<String>,
    pub action: String,
    pub session: Option<String>,
    pub detail: Option<String>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Store {
    /// Open (creating, owner-only) the database at `path`.
    pub fn open(path: &Path) -> Result<Store> {
        crate::paths::ensure_parent(path)
            .with_context(|| format!("creating {}", path.parent().unwrap_or(path).display()))?;
        if !path.exists() {
            let _ = crate::paths::create_private(path);
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        let store = Store::init(conn)?;
        let name = path.as_os_str().to_owned();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = name.clone();
            p.push(suffix);
            crate::paths::tighten(Path::new(&p))?;
        }
        Ok(store)
    }

    /// An in-memory store, for tests.
    pub fn memory() -> Result<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL;");
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(anyhow!(
                "remote.db is from a newer CodeSmith (schema {version}); upgrade to use it"
            ));
        }
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS devices (
               id TEXT PRIMARY KEY,
               name TEXT NOT NULL,
               platform TEXT NOT NULL,
               paired INTEGER NOT NULL,
               revoked INTEGER,
               unlock_alg TEXT,
               unlock_spki TEXT
             );
             CREATE TABLE IF NOT EXISTS grants (
               device TEXT NOT NULL,
               session TEXT NOT NULL,
               scope TEXT NOT NULL,
               granted INTEGER NOT NULL,
               PRIMARY KEY (device, session)
             );
             CREATE TABLE IF NOT EXISTS audit (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               ts INTEGER NOT NULL,
               device TEXT,
               action TEXT NOT NULL,
               session TEXT,
               detail TEXT
             );
             CREATE TABLE IF NOT EXISTS push_tokens (
               device TEXT PRIMARY KEY,
               kind TEXT NOT NULL,
               token TEXT NOT NULL,
               updated INTEGER NOT NULL
             );
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Record a pairing the terminal allowed: the device (new, or paired
    /// again after a revoke), and `grant`, which replaces any grant it had
    /// on the same session. Its unlock key and push token are replaced
    /// when the claim carried new ones.
    pub fn pair(
        &self,
        id: &str,
        name: &str,
        platform: &str,
        unlock_key: Option<&UnlockKey>,
        push: Option<&Push>,
        grant: Option<(&str, Scope)>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let now = now_ms();
        let active: bool = tx
            .query_row(
                "SELECT revoked IS NULL FROM devices WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if active {
            tx.execute(
                "UPDATE devices SET name = ?2, platform = ?3 WHERE id = ?1",
                params![id, name, platform],
            )?;
        } else {
            tx.execute("DELETE FROM grants WHERE device = ?1", [id])?;
            tx.execute(
                "INSERT INTO devices (id, name, platform, paired, revoked, unlock_alg, unlock_spki)
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL)
                 ON CONFLICT(id) DO UPDATE SET name = ?2, platform = ?3, paired = ?4,
                   revoked = NULL, unlock_alg = NULL, unlock_spki = NULL",
                params![id, name, platform, now],
            )?;
        }
        if let Some(k) = unlock_key {
            tx.execute(
                "UPDATE devices SET unlock_alg = ?2, unlock_spki = ?3 WHERE id = ?1",
                params![id, k.alg, k.spki],
            )?;
        }
        if let Some(p) = push {
            tx.execute(
                "INSERT INTO push_tokens (device, kind, token, updated) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(device) DO UPDATE SET kind = ?2, token = ?3, updated = ?4",
                params![id, p.kind, p.token, now],
            )?;
        }
        if let Some((session, scope)) = grant {
            tx.execute(
                "INSERT INTO grants (device, session, scope, granted) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(device, session) DO UPDATE SET scope = ?3, granted = ?4",
                params![id, session, scope.as_str(), now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Revoke a device: it's no longer paired, and its grants and push
    /// token are gone. `false` when it wasn't paired.
    pub fn revoke(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE devices SET revoked = ?2 WHERE id = ?1 AND revoked IS NULL",
            params![id, now_ms()],
        )?;
        tx.execute("DELETE FROM grants WHERE device = ?1", [id])?;
        tx.execute("DELETE FROM push_tokens WHERE device = ?1", [id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Whether `id` is a paired, unrevoked device.
    pub fn is_paired(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT 1 FROM devices WHERE id = ?1 AND revoked IS NULL",
                [id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// One device, revoked or not.
    pub fn device(&self, id: &str) -> Result<Option<Device>> {
        Ok(self.devices(true)?.into_iter().find(|d| d.id == id))
    }

    /// The paired devices (and with `revoked`, the revoked ones too),
    /// most recently paired first.
    pub fn devices(&self, revoked: bool) -> Result<Vec<Device>> {
        let conn = self.conn.lock().unwrap();
        let mut q = conn.prepare(
            "SELECT id, name, platform, paired, revoked, unlock_alg, unlock_spki FROM devices
             WHERE ?1 OR revoked IS NULL ORDER BY paired DESC, id",
        )?;
        let mut out: Vec<Device> = q
            .query_map([revoked], |r| {
                let alg: Option<String> = r.get(5)?;
                let spki: Option<String> = r.get(6)?;
                Ok(Device {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    platform: r.get(2)?,
                    paired: r.get(3)?,
                    revoked: r.get(4)?,
                    unlock_key: alg.zip(spki).map(|(alg, spki)| UnlockKey { alg, spki }),
                    grants: vec![],
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut g = conn.prepare(
            "SELECT session, scope, granted FROM grants WHERE device = ?1 ORDER BY session",
        )?;
        for d in &mut out {
            d.grants = g
                .query_map([&d.id], |r| {
                    let scope: String = r.get(1)?;
                    Ok(Grant {
                        session: r.get(0)?,
                        scope: Scope::parse(&scope).unwrap_or(Scope::Watch),
                        granted: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
        }
        Ok(out)
    }

    /// The push token a device registered.
    pub fn push(&self, id: &str) -> Result<Option<Push>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT kind, token FROM push_tokens WHERE device = ?1",
                [id],
                |r| {
                    Ok(Push {
                        kind: r.get(0)?,
                        token: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// The device `query` names: its full id, a unique id prefix of at
    /// least 4 characters, or its exact name when only one paired device
    /// has it.
    pub fn find(&self, query: &str) -> Result<Device> {
        let q = query.trim();
        let all = self.devices(false)?;
        if let Some(d) = all.iter().find(|d| d.id == q.to_ascii_lowercase()) {
            return Ok(d.clone());
        }
        let lower = q.to_ascii_lowercase();
        let by_id: Vec<&Device> = if q.len() >= 4 && lower.bytes().all(|b| b.is_ascii_hexdigit()) {
            all.iter().filter(|d| d.id.starts_with(&lower)).collect()
        } else {
            vec![]
        };
        let by_name: Vec<&Device> = all.iter().filter(|d| d.name == q).collect();
        match (by_id.as_slice(), by_name.as_slice()) {
            ([d], []) | ([], [d]) => Ok((*d).clone()),
            ([a], [b]) if a.id == b.id => Ok((*a).clone()),
            ([], []) => Err(anyhow!("no paired device is {q:?}")),
            _ => Err(anyhow!(
                "{q:?} names more than one device; give more of its id"
            )),
        }
    }

    /// Add an audit entry.
    pub fn audit(
        &self,
        device: Option<&str>,
        action: &str,
        session: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO audit (ts, device, action, session, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now_ms(), device, action, session, detail],
        )?;
        Ok(())
    }

    /// The newest `limit` audit entries (of one device, when given),
    /// oldest first.
    pub fn log(&self, device: Option<&str>, limit: usize) -> Result<Vec<Audit>> {
        let conn = self.conn.lock().unwrap();
        let mut q = conn.prepare(
            "SELECT ts, device, action, session, detail FROM audit
             WHERE ?1 IS NULL OR device = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let mut out: Vec<Audit> = q
            .query_map(params![device, limit as i64], |r| {
                Ok(Audit {
                    ts: r.get(0)?,
                    device: r.get(1)?,
                    action: r.get(2)?,
                    session: r.get(3)?,
                    detail: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        out.reverse();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[test]
    fn pairing_grants_and_pairing_again_adds_only_the_new_grant() {
        let s = Store::memory().unwrap();
        let k = UnlockKey {
            alg: "ES256".into(),
            spki: "AAAA".into(),
        };
        s.pair(
            &id('a'),
            "Phone",
            "ios",
            Some(&k),
            None,
            Some(("ses_1", Scope::Control)),
        )
        .unwrap();
        assert!(s.is_paired(&id('a')).unwrap());
        assert!(!s.is_paired(&id('b')).unwrap());
        s.pair(
            &id('a'),
            "Phone",
            "ios",
            None,
            None,
            Some(("ses_2", Scope::Watch)),
        )
        .unwrap();
        let d = s.device(&id('a')).unwrap().unwrap();
        assert_eq!(d.unlock_key, Some(k), "kept when the claim carries none");
        let g: Vec<(&str, Scope)> = d
            .grants
            .iter()
            .map(|g| (g.session.as_str(), g.scope))
            .collect();
        assert_eq!(g, [("ses_1", Scope::Control), ("ses_2", Scope::Watch)]);
        // The same session again replaces its grant.
        s.pair(
            &id('a'),
            "Phone",
            "ios",
            None,
            None,
            Some(("ses_1", Scope::Watch)),
        )
        .unwrap();
        let d = s.device(&id('a')).unwrap().unwrap();
        assert_eq!(d.grants[0].scope, Scope::Watch);
    }

    #[test]
    fn revoking_ends_the_pairing_and_its_grants() {
        let s = Store::memory().unwrap();
        let push = Push {
            kind: "apns".into(),
            token: "t".into(),
        };
        let k = UnlockKey {
            alg: "ES256".into(),
            spki: "AAAA".into(),
        };
        s.pair(
            &id('a'),
            "P",
            "android",
            Some(&k),
            Some(&push),
            Some(("*", Scope::Control)),
        )
        .unwrap();
        assert_eq!(s.push(&id('a')).unwrap(), Some(push));
        assert!(s.revoke(&id('a')).unwrap());
        assert!(!s.revoke(&id('a')).unwrap(), "already revoked");
        assert!(!s.is_paired(&id('a')).unwrap());
        assert!(s.devices(false).unwrap().is_empty());
        let d = s.devices(true).unwrap().pop().unwrap();
        assert!(d.revoked.is_some() && d.grants.is_empty());
        assert_eq!(s.push(&id('a')).unwrap(), None);
        // Paired again, it starts afresh: the old unlock key is not revived.
        s.pair(
            &id('a'),
            "P",
            "android",
            None,
            None,
            Some(("ses_9", Scope::Watch)),
        )
        .unwrap();
        let d = s.device(&id('a')).unwrap().unwrap();
        assert_eq!(d.revoked, None);
        assert_eq!(d.unlock_key, None);
        assert_eq!(d.grants.len(), 1);
    }

    #[test]
    fn finding_a_device_by_id_prefix_or_name() {
        let s = Store::memory().unwrap();
        s.pair(
            &format!("ab{}", "1".repeat(62)),
            "Ray's iPhone",
            "ios",
            None,
            None,
            None,
        )
        .unwrap();
        s.pair(
            &format!("ac{}", "2".repeat(62)),
            "Pixel",
            "android",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.find("ab11").unwrap().name, "Ray's iPhone");
        assert_eq!(s.find("AB11").unwrap().name, "Ray's iPhone");
        assert_eq!(s.find("Pixel").unwrap().platform, "android");
        assert!(s.find("a").is_err(), "too short");
        assert!(s.find("nobody").is_err());
        s.pair(
            &format!("ad{}", "3".repeat(62)),
            "Pixel",
            "android",
            None,
            None,
            None,
        )
        .unwrap();
        assert!(s.find("Pixel").is_err(), "ambiguous");
    }

    #[test]
    fn the_audit_log_keeps_order_and_filters() {
        let s = Store::memory().unwrap();
        s.audit(Some(&id('a')), "paired", Some("ses_1"), Some("control"))
            .unwrap();
        s.audit(None, "revoked", None, None).unwrap();
        s.audit(Some(&id('b')), "paired", None, None).unwrap();
        let all = s.log(None, 10).unwrap();
        assert_eq!(
            all.iter().map(|a| a.action.as_str()).collect::<Vec<_>>(),
            ["paired", "revoked", "paired"]
        );
        assert_eq!(s.log(Some(&id('a')), 10).unwrap().len(), 1);
        assert_eq!(s.log(None, 2).unwrap()[0].action, "revoked");
    }

    #[test]
    fn the_file_is_private() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("data/remote.db");
        let s = Store::open(&p).unwrap();
        s.audit(None, "x", None, None).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
        drop(s);
        assert_eq!(Store::open(&p).unwrap().log(None, 5).unwrap().len(), 1);
    }
}
