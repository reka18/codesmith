//! Media kept once, outside the session records (after OpenCode's
//! `SessionBlob` and `BlobStore`).
//!
//! With `media.blob` on (the default), a record's inline media (an
//! attachment, a tool's image, a generated image or video) is moved into
//! the `blob` table under the lowercase hex SHA-256 of its bytes, and the
//! record keeps a `media:<hash>` reference in its place
//! ([`codesmith_ai::Source::Url`]). The same bytes are stored once however
//! many records (or sessions) name them. Each owner (a session's record at
//! its sequence number, or a video job) holds a `blob_ref` row per blob it
//! names; a blob is deleted once its last reference goes: with its session,
//! with a turn an undo hid for good, or with an expired job.
//!
//! Reads put the bytes back ([`hydrate`]), so the session, the model and
//! front ends see inline media as before, and records written before the
//! store existed, which keep their media inline, read unchanged. A
//! reference whose blob is gone reads as a `[media unavailable: …]` note,
//! never a bare reference. One item may be at most [`MAX_BYTES`]; larger
//! media stays inline.

use crate::store::Record;
use anyhow::Result;
use codesmith_ai::{Media, Message, Part, Source};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// What a stored reference's URL starts with.
pub const PREFIX: &str = "media:";

/// The most bytes one blob holds: the 20 MiB media cap.
pub const MAX_BYTES: usize = crate::media::MAX_INGEST_BYTES as usize;

pub(crate) const TABLES: &str = "CREATE TABLE IF NOT EXISTS blob (
    hash TEXT PRIMARY KEY, mime TEXT NOT NULL, bytes INTEGER NOT NULL,
    data BLOB NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS blob_ref (
    owner TEXT NOT NULL, seq INTEGER NOT NULL, hash TEXT NOT NULL,
    PRIMARY KEY (owner, seq, hash));
CREATE INDEX IF NOT EXISTS blob_ref_hash ON blob_ref(hash);";

/// A blob's name: the lowercase hex SHA-256 of its bytes.
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The reference a record keeps for blob `hash`.
pub fn reference(hash: &str) -> String {
    format!("{PREFIX}{hash}")
}

/// The blob a `media:<hash>` URL names. Anything else (a path that merely
/// looks like one) is not a reference.
pub fn ref_hash(url: &str) -> Option<&str> {
    let h = url.strip_prefix(PREFIX)?;
    (h.len() == 64
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(h)
}

/// What a reference to a missing blob reads as.
pub fn unavailable(m: &Media) -> String {
    match &m.filename {
        Some(n) => format!("[media unavailable: {n} ({})]", m.mime),
        None => format!("[media unavailable: {}]", m.mime),
    }
}

fn messages_of(record: &mut Record) -> Vec<&mut Message> {
    match record {
        Record::Message { message } => vec![message],
        Record::Native { window, .. } => window.iter_mut().collect(),
        _ => Vec::new(),
    }
}

/// Every media item of a record's messages.
fn each_media(record: &mut Record, f: &mut dyn FnMut(&mut Media)) {
    for m in messages_of(record) {
        for p in &mut m.parts {
            match p {
                Part::Media(media) => f(media),
                Part::ToolResult { media, .. } => media.iter_mut().for_each(&mut *f),
                Part::Hosted(h) => {
                    if let Some(r) = &mut h.result {
                        r.media.iter_mut().for_each(&mut *f);
                    }
                }
                _ => {}
            }
        }
    }
}

/// The blobs a record names.
pub fn hashes(record: &Record) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut copy = record.clone();
    each_media(&mut copy, &mut |m| {
        if let Source::Url(u) = &m.source {
            if let Some(h) = ref_hash(u) {
                out.insert(h.to_string());
            }
        }
    });
    out
}

/// Store `bytes` as a blob (kept as it is when one with these bytes
/// exists) and return its hash. `None` past [`MAX_BYTES`].
pub(crate) fn put(conn: &Connection, bytes: &[u8], mime: &str) -> Result<Option<String>> {
    if bytes.len() > MAX_BYTES {
        return Ok(None);
    }
    let h = hash(bytes);
    conn.execute(
        "INSERT OR IGNORE INTO blob (hash, mime, bytes, data, created) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![h, mime, bytes.len() as i64, bytes, crate::store::now()],
    )?;
    Ok(Some(h))
}

/// A blob's type and bytes.
pub(crate) fn get(conn: &Connection, hash: &str) -> Result<Option<(String, Vec<u8>)>> {
    Ok(conn
        .query_row("SELECT mime, data FROM blob WHERE hash = ?1", [hash], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?)
}

/// Move a record's inline media into blobs, leaving references in its
/// place. Returns the blobs the record names afterwards (those it already
/// referenced too). Media that is not valid base64, or is too large, stays
/// inline.
pub(crate) fn externalize(conn: &Connection, record: &mut Record) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let mut failed = None;
    each_media(record, &mut |m| {
        if failed.is_some() {
            return;
        }
        match &m.source {
            Source::Base64(data) => {
                let Some(bytes) = crate::media::unbase64(data) else {
                    return;
                };
                match put(conn, &bytes, &m.mime) {
                    Ok(Some(h)) => {
                        m.source = Source::Url(reference(&h));
                        out.insert(h);
                    }
                    Ok(None) => {}
                    Err(e) => failed = Some(e),
                }
            }
            Source::Url(u) => {
                if let Some(h) = ref_hash(u) {
                    out.insert(h.to_string());
                }
            }
        }
    });
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// Whether a record holds inline media [`externalize`] would move.
pub(crate) fn has_inline(record: &Record) -> bool {
    let mut found = false;
    let mut copy = record.clone();
    each_media(&mut copy, &mut |m| {
        found |= matches!(&m.source, Source::Base64(_));
    });
    found
}

/// Note that `owner`'s record `seq` names `hashes`, so they are kept until
/// it lets go. A reference to a blob already gone keeps nothing.
pub(crate) fn record_refs(
    conn: &Connection,
    owner: &str,
    seq: i64,
    hashes: &BTreeSet<String>,
) -> Result<()> {
    for h in hashes {
        conn.execute(
            "INSERT OR IGNORE INTO blob_ref (owner, seq, hash) SELECT ?1, ?2, hash FROM blob WHERE hash = ?3",
            params![owner, seq, h],
        )?;
    }
    Ok(())
}

/// Put the bytes back for a record's references; one whose blob is gone
/// becomes a note saying so (an attachment turns into that text, a tool's
/// item is dropped and the note added to its output).
pub(crate) fn hydrate(conn: &Connection, record: &mut Record) -> Result<()> {
    let wanted = hashes(record);
    if wanted.is_empty() {
        return Ok(());
    }
    let mut blobs = BTreeMap::new();
    for h in &wanted {
        if let Some((_, data)) = get(conn, h)? {
            blobs.insert(h.clone(), crate::media::base64(&data));
        }
    }
    resolve(record, &blobs);
    Ok(())
}

/// [`hydrate`] against `blobs` (base64 by hash): what is missing there is
/// unavailable.
pub(crate) fn resolve(record: &mut Record, blobs: &BTreeMap<String, String>) {
    let found = |m: &Media| -> Result<Option<String>, ()> {
        match &m.source {
            Source::Url(u) => match ref_hash(u) {
                Some(h) => blobs.get(h).cloned().map(Some).ok_or(()),
                None => Ok(None),
            },
            Source::Base64(_) => Ok(None),
        }
    };
    for msg in messages_of(record) {
        for p in &mut msg.parts {
            match p {
                Part::Media(m) => match found(m) {
                    Ok(Some(data)) => m.source = Source::Base64(data),
                    Ok(None) => {}
                    Err(()) => *p = Part::text(unavailable(m)),
                },
                Part::ToolResult { content, media, .. } => {
                    media.retain_mut(|m| match found(m) {
                        Ok(Some(data)) => {
                            m.source = Source::Base64(data);
                            true
                        }
                        Ok(None) => true,
                        Err(()) => {
                            if !content.is_empty() {
                                content.push('\n');
                            }
                            content.push_str(&unavailable(m));
                            false
                        }
                    });
                }
                Part::Hosted(h) => {
                    if let Some(r) = &mut h.result {
                        r.media.retain_mut(|m| match found(m) {
                            Ok(Some(data)) => {
                                m.source = Source::Base64(data);
                                true
                            }
                            Ok(None) => true,
                            Err(()) => false,
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

/// Delete `owner`'s references in `[from, to)` (every one without a
/// range), then each blob among theirs no reference names any more.
pub(crate) fn release(conn: &Connection, owner: &str, range: Option<(i64, i64)>) -> Result<()> {
    let (from, to) = range.unwrap_or((i64::MIN, i64::MAX));
    let released: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT hash FROM blob_ref WHERE owner = ?1 AND seq >= ?2 AND seq < ?3",
        )?;
        let rows = stmt.query_map(params![owner, from, to], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if released.is_empty() {
        return Ok(());
    }
    conn.execute(
        "DELETE FROM blob_ref WHERE owner = ?1 AND seq >= ?2 AND seq < ?3",
        params![owner, from, to],
    )?;
    for h in released {
        conn.execute(
            "DELETE FROM blob WHERE hash = ?1 AND NOT EXISTS (SELECT 1 FROM blob_ref WHERE hash = ?1)",
            [h],
        )?;
    }
    Ok(())
}

/// Whether `owner` references any blob.
pub(crate) fn owns_any(conn: &Connection, owner: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM blob_ref WHERE owner = ?1 LIMIT 1",
            [owner],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Release what session `id`'s undone turns referenced once they can no
/// longer be redone (a later message ended the chance), so only the
/// records still in view keep their blobs.
pub(crate) fn release_undone(conn: &Connection, id: &str) -> Result<()> {
    if !owns_any(conn, id)? {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT seq, body FROM record WHERE session_id = ?1
         AND (body LIKE '{\"kind\":\"revert\"%' OR body LIKE '{\"kind\":\"redo\"%')",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut reverts = Vec::new();
    let mut redone = Vec::new();
    for (seq, body) in rows {
        match serde_json::from_str::<Record>(&body) {
            Ok(Record::Revert { from_seq, .. }) => reverts.push((from_seq, seq)),
            Ok(Record::Redo { revert_seq }) => redone.push(revert_seq),
            _ => {}
        }
    }
    for (from, to) in reverts {
        if !redone.contains(&to) {
            release(conn, id, Some((from, to)))?;
        }
    }
    Ok(())
}
