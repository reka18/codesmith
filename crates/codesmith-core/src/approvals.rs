//! Saved approvals (after OpenCode's `permission/saved.ts`): what "always"
//! answers allow, kept per project in the session database, so later
//! sessions of the project are not asked again. They can be listed, edited
//! and removed (`codesmith permissions`, the local server's
//! `/permissions/saved`), and a change applies to running sessions at their
//! next request.
//!
//! A saved approval is only ever an `allow` rule, judged after the
//! configured and built-in rules: a configured or built-in deny (or a
//! policy deny) still refuses the request, as [`crate::permission::Gate`]
//! checks those first.

use crate::permission::{Effect, Rule};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One saved approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Saved {
    pub id: String,
    pub project: String,
    pub action: String,
    pub resource: String,
    /// When it was saved or last edited (ms since the epoch).
    pub time: i64,
}

/// Why an edit was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    NotFound {
        id: String,
    },
    /// The project already has the edited approval.
    Conflict {
        id: String,
        resource: String,
    },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::NotFound { id } => write!(f, "no saved approval {id}"),
            EditError::Conflict { id, resource } => write!(
                f,
                "{id}: the project already has a saved approval for {resource}"
            ),
        }
    }
}

impl std::error::Error for EditError {}

const TABLE: &str = "CREATE TABLE IF NOT EXISTS permission_saved (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    action TEXT NOT NULL,
    resource TEXT NOT NULL,
    time INTEGER NOT NULL,
    UNIQUE (project, action, resource)
)";

/// The saved approvals of a session database.
pub struct Book {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl std::fmt::Debug for Book {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Book").field("path", &self.path).finish()
    }
}

/// The key approvals are saved under: `dir:` and the project root's real
/// path (symlinks resolved, as per-repo config matching resolves it). Only
/// the folder decides: nothing read from the repository (its commits,
/// remotes or `.git` file) does, since a fork or a crafted repository can
/// copy any of those. So a second clone or a worktree of a repository
/// starts with no approvals, and a symlink to the folder shares them.
///
/// Approvals saved before under a repository's first commit are left in
/// the database unread: no key is ever a bare commit id again.
pub fn project_key(root: &Path) -> String {
    let real = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    format!("dir:{}", real.display())
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Saved> {
    Ok(Saved {
        id: r.get(0)?,
        project: r.get(1)?,
        action: r.get(2)?,
        resource: r.get(3)?,
        time: r.get(4)?,
    })
}

fn new_id() -> String {
    use rand::Rng;
    let mut r = rand::thread_rng();
    let tail: String = (0..16)
        .map(|_| {
            let n = r.gen_range(0..36u32);
            char::from_digit(n, 36).unwrap()
        })
        .collect();
    format!("psv_{tail}")
}

impl Book {
    /// The saved approvals in the session database at `path`.
    pub fn open(path: &Path) -> Result<Book> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(TABLE)?;
        Ok(Book {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved approvals, of one project or all, oldest first.
    pub fn list(&self, project: Option<&str>) -> Result<Vec<Saved>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT id, project, action, resource, time FROM permission_saved
             WHERE ?1 IS NULL OR project = ?1 ORDER BY time, rowid",
        )?;
        let rows = st.query_map(params![project], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// One saved approval.
    pub fn get(&self, id: &str) -> Result<Option<Saved>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT id, project, action, resource, time FROM permission_saved WHERE id = ?1",
            params![id],
            row,
        )
        .optional()?)
    }

    /// A project's saved approvals as `allow` rules.
    pub fn rules(&self, project: &str) -> Vec<Rule> {
        self.list(Some(project))
            .unwrap_or_default()
            .into_iter()
            .map(|s| Rule::new(&s.action, &s.resource, Effect::Allow))
            .collect()
    }

    /// Save approvals of `action` for `resources`; one already saved is
    /// kept as it is. The approvals, in the order of `resources`.
    pub fn add(&self, project: &str, action: &str, resources: &[String]) -> Result<Vec<Saved>> {
        let c = self.conn.lock().unwrap();
        let now = crate::store::now();
        let mut out = Vec::new();
        for r in resources {
            c.execute(
                "INSERT OR IGNORE INTO permission_saved (id, project, action, resource, time)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![new_id(), project, action, r, now],
            )?;
            out.push(c.query_row(
                "SELECT id, project, action, resource, time FROM permission_saved
                 WHERE project = ?1 AND action = ?2 AND resource = ?3",
                params![project, action, r],
                row,
            )?);
        }
        Ok(out)
    }

    /// Change a saved approval's action or resource.
    pub fn update(
        &self,
        id: &str,
        action: Option<&str>,
        resource: Option<&str>,
    ) -> Result<std::result::Result<Saved, EditError>> {
        let Some(old) = self.get(id)? else {
            return Ok(Err(EditError::NotFound { id: id.into() }));
        };
        let action = action.unwrap_or(&old.action).to_string();
        let resource = resource.unwrap_or(&old.resource).to_string();
        let c = self.conn.lock().unwrap();
        let clash: Option<String> = c
            .query_row(
                "SELECT id FROM permission_saved
                 WHERE project = ?1 AND action = ?2 AND resource = ?3 AND id != ?4",
                params![old.project, action, resource, id],
                |r| r.get(0),
            )
            .optional()?;
        if clash.is_some() {
            return Ok(Err(EditError::Conflict {
                id: id.into(),
                resource,
            }));
        }
        let time = crate::store::now();
        c.execute(
            "UPDATE permission_saved SET action = ?1, resource = ?2, time = ?3 WHERE id = ?4",
            params![action, resource, time, id],
        )?;
        Ok(Ok(Saved {
            action,
            resource,
            time,
            ..old
        }))
    }

    /// Remove a saved approval: whether there was one.
    pub fn remove(&self, id: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        Ok(c.execute("DELETE FROM permission_saved WHERE id = ?1", params![id])? > 0)
    }
}
