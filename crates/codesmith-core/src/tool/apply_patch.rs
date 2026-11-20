//! `patch`: add, update, move and delete files with one patch. Ported from
//! OpenCode's patch tool (its `apply_patch` before the rename), which GPT-5
//! and later models get instead of `edit` and `write`.
//!
//! A call runs in OpenCode's order: every hunk is resolved (asking
//! `external_directory` for paths outside the project) and verified
//! against the files, in sequence, before anything is written, so a patch
//! that does not apply changes nothing. One `edit` request then covers
//! every file, and the hunks are written in order. A write that fails
//! midway stops there and names the files already changed.
//!
//! Unlike OpenCode, when a file the patch reads is not plainly readable
//! (a `read` rule asks or denies), the `edit` request comes before the
//! files are read, as `edit` asks before reporting a mismatch, so a patch
//! cannot probe a file's contents through its verification errors.

use super::patch::{self as format, Hunk};
use super::{Ctx, Outcome, Output, Target, Tool};
use crate::permission::{Effect, Request};
use serde_json::{json, Value};
use std::io;
use std::path::{Path, PathBuf};

pub fn spec() -> (String, Value) {
    (
        include_str!("patch.txt").trim_end().to_string(),
        json!({
            "type": "object",
            "properties": {
                "patchText": {
                    "type": "string",
                    "description": "The full patch text describing add, update, and delete operations"
                }
            },
            "required": ["patchText"]
        }),
    )
}

/// Whether a model edits with `patch` rather than `edit` and `write`
/// (OpenCode's `gptMode`): GPT models named `gpt-…`, except `gpt-4…` and
/// the open-weight `gpt-oss`. `id` is the model's name in the config (or
/// catalog), `model_id` the id sent to the provider.
pub fn edits_with_patch(id: &str, model_id: &str) -> bool {
    let (id, model_id) = (id.to_lowercase(), model_id.to_lowercase());
    let either = |s: &str| id.contains(s) || model_id.contains(s);
    either("gpt-") && !either("oss") && !either("gpt-4")
}

/// Keep `patch` or `edit` and `write` among `tools`, for the model
/// (OpenCode's `modelTools`): models that edit with `patch` lose `edit`
/// and `write`; every other model loses `patch`.
pub fn offer_for_model(tools: &mut Vec<Tool>, id: &str, model_id: &str) {
    if edits_with_patch(id, model_id) {
        tools.retain(|t| !matches!(t, Tool::Edit | Tool::Write));
    } else {
        tools.retain(|t| *t != Tool::Patch);
    }
}

/// [`offer_for_model`] for the model `spec` (`provider/model#variant`)
/// names, whose provider id is `model_id`.
pub fn offer_for_spec(tools: &mut Vec<Tool>, spec: &str, model_id: &str) {
    let (base, _) = crate::config::split_variant(spec);
    let id = base.split_once('/').map_or(base, |(_, m)| m);
    offer_for_model(tools, id, model_id);
}

/// The file operations a call performs, so tests can make them fail.
pub(crate) trait Files: Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Write `data`, creating missing parent directories.
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
}

pub(crate) struct Disk;

impl Files for Disk {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        // A FIFO or device would block or never end; only files are patched.
        if std::fs::metadata(path).is_ok_and(|m| !m.is_file() && !m.is_dir()) {
            return Err(io::Error::other("not a regular file"));
        }
        std::fs::read(path)
    }
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, data)
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}

pub fn run(ctx: &Ctx, input: &Value) -> Outcome {
    run_with(ctx, input, &Disk)
}

/// OpenCode's `errorMessage` for a file that could not be read or changed.
fn io_message(path: &Path, e: &io::Error) -> String {
    if path.is_dir() {
        "path is a directory".into()
    } else if e.kind() == io::ErrorKind::NotFound {
        "file does not exist".into()
    } else {
        e.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Add,
    Delete,
    Update,
}

/// A verified hunk, ready to write.
struct Prepared {
    kind: Kind,
    target: Target,
    /// What to write (with its BOM); `None` for a delete.
    content: Option<String>,
    /// Where an update moves the file, when somewhere else.
    move_target: Option<Target>,
}

impl Prepared {
    fn destination(&self) -> &Target {
        self.move_target.as_ref().unwrap_or(&self.target)
    }
}

/// What each file holds after the hunks verified so far: `None` once
/// deleted (or moved away). In first-touched order.
#[derive(Default)]
struct Updates(Vec<(PathBuf, Target, Option<String>)>);

impl Updates {
    fn get(&self, path: &Path) -> Option<&Option<String>> {
        self.0.iter().find(|(p, ..)| p == path).map(|(_, _, c)| c)
    }
    fn set(&mut self, target: &Target, content: Option<String>) {
        match self.0.iter_mut().find(|(p, ..)| *p == target.absolute) {
            Some(slot) => slot.2 = content,
            None => self
                .0
                .push((target.absolute.clone(), target.clone(), content)),
        }
    }
}

fn verification(message: impl std::fmt::Display) -> String {
    format!("patch verification failed: {message}")
}

pub(crate) fn run_with(ctx: &Ctx, input: &Value, files: &dyn Files) -> Outcome {
    let text = input
        .get("patchText")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if text.is_empty() {
        return Err("patchText is required".into());
    }
    let hunks = format::parse(text).map_err(verification)?;
    if hunks.is_empty() {
        return Err("patch rejected: empty patch".into());
    }

    // Resolve every path (asking for the ones outside the project) first.
    let mut targets: Vec<(Target, Option<Target>)> = Vec::with_capacity(hunks.len());
    for hunk in &hunks {
        let target = ctx.resolve(hunk.path());
        ctx.authorize_external(&target, false)?;
        let move_target = match hunk {
            Hunk::Update {
                move_path: Some(m), ..
            } => {
                let dest = ctx.resolve(m);
                ctx.authorize_external(&dest, false)?;
                Some(dest).filter(|d| d.absolute != target.absolute)
            }
            _ => None,
        };
        targets.push((target, move_target));
    }
    let every = || {
        targets
            .iter()
            .flat_map(|(t, m)| std::iter::once(t).chain(m.as_ref()))
    };

    // One transaction per file at a time: lock them all, in a fixed order.
    let mut keys: Vec<PathBuf> = every().map(|t| t.real.clone()).collect();
    keys.sort();
    keys.dedup();
    let locks: Vec<_> = keys.iter().map(|k| super::edit::file_lock(k)).collect();
    let _guards: Vec<_> = locks
        .iter()
        .map(|l| l.lock().unwrap_or_else(|e| e.into_inner()))
        .collect();

    let mut resources: Vec<String> = Vec::new();
    for t in every() {
        for r in &t.resources {
            if !resources.contains(r) {
                resources.push(r.clone());
            }
        }
    }
    let shown: Vec<&str> = every().map(|t| t.resource.as_str()).collect();
    let request = Request {
        action: "edit".into(),
        resources,
        save: vec!["*".into()],
        title: format!("Patch {}", shown.join(", ")),
    };
    let early = every().any(|t| ctx.read_effect(&t.resources) != Effect::Allow);
    if early {
        ctx.ask(request.clone())?;
    }

    // Verify every hunk against the files as the earlier hunks leave them.
    let mut updates = Updates::default();
    let mut prepared: Vec<Prepared> = Vec::with_capacity(hunks.len());
    for (hunk, (target, move_target)) in hunks.iter().zip(&targets) {
        match hunk {
            Hunk::Add { contents, .. } => {
                let content = if contents.is_empty() || contents.ends_with('\n') {
                    contents.clone()
                } else {
                    format!("{contents}\n")
                };
                updates.set(target, Some(content.clone()));
                prepared.push(Prepared {
                    kind: Kind::Add,
                    target: target.clone(),
                    content: Some(content),
                    move_target: None,
                });
            }
            Hunk::Delete { .. } => {
                let fail = |why: &str| {
                    verification(format!("Failed to delete {}: {why}", target.resource))
                };
                match updates.get(&target.absolute) {
                    Some(None) => return Err(fail("file does not exist")),
                    Some(Some(_)) => {}
                    None => {
                        files
                            .read(&target.absolute)
                            .map_err(|e| fail(&io_message(&target.absolute, &e)))?;
                    }
                }
                updates.set(target, None);
                prepared.push(Prepared {
                    kind: Kind::Delete,
                    target: target.clone(),
                    content: None,
                    move_target: None,
                });
            }
            Hunk::Update { chunks, .. } => {
                let fail = |why: &str| {
                    verification(format!(
                        "Failed to read file to update {}: {why}",
                        target.absolute.display()
                    ))
                };
                let original = match updates.get(&target.absolute) {
                    Some(None) => return Err(fail("file does not exist")),
                    Some(Some(c)) => c.clone(),
                    None => {
                        let bytes = files
                            .read(&target.absolute)
                            .map_err(|e| fail(&io_message(&target.absolute, &e)))?;
                        let text = String::from_utf8(bytes)
                            .map_err(|_| fail(super::edit::INVALID_UTF8))?;
                        let (bom, text) = format::split_bom(&text);
                        format::join_bom(text, bom)
                    }
                };
                let update =
                    format::derive(hunk.path(), chunks, &original).map_err(verification)?;
                let next = format::join_bom(&update.content, update.bom);
                match move_target {
                    Some(dest) => {
                        updates.set(target, None);
                        updates.set(dest, Some(next.clone()));
                    }
                    None => updates.set(target, Some(next.clone())),
                }
                prepared.push(Prepared {
                    kind: Kind::Update,
                    target: target.clone(),
                    content: Some(next),
                    move_target: move_target.clone(),
                });
            }
        }
    }

    if !early {
        ctx.ask(request)?;
    }
    // Allowed: report each file's text before, read now, for the record.
    let mut reported: Vec<&Path> = Vec::new();
    for (target, move_target) in &targets {
        let dest = move_target.as_ref().unwrap_or(target);
        if !reported.contains(&dest.absolute.as_path()) {
            reported.push(&dest.absolute);
            ctx.changing_moved(&dest.absolute, &target.absolute);
        }
    }

    // Write the hunks in order; a failure names what was already done.
    let mut completed: Vec<String> = Vec::new();
    let fail = |completed: &[String], operation: String, e: &io::Error, path: &Path| {
        let done = if completed.is_empty() {
            String::new()
        } else {
            format!(". Completed before failure: {}", completed.join(", "))
        };
        format!("{operation}: {}{done}", io_message(path, e))
    };
    for change in &prepared {
        let dest = change.destination();
        let source = &change.target.absolute;
        // On a case-insensitive volume a move that only changes case names
        // the same file, so writing the new name and removing the old one
        // would delete it: write in place and rename instead. Symlinks keep
        // write-and-remove, which leaves their target alone.
        let recased = change.move_target.as_ref().is_some_and(|m| {
            m.absolute.to_string_lossy().to_lowercase() == source.to_string_lossy().to_lowercase()
                && std::fs::symlink_metadata(source).is_ok_and(|m| m.is_file())
                && std::fs::symlink_metadata(&m.absolute).is_ok()
        });
        if change.kind == Kind::Delete {
            files.remove(source).map_err(|e| {
                fail(
                    &completed,
                    format!("Failed to delete {}", change.target.resource),
                    &e,
                    source,
                )
            })?;
        }
        if let Some(content) = &change.content {
            let written = if recased { source } else { &dest.absolute };
            files.write(written, content.as_bytes()).map_err(|e| {
                fail(
                    &completed,
                    format!("Failed to write {}", dest.resource),
                    &e,
                    written,
                )
            })?;
        }
        if let Some(m) = &change.move_target {
            if recased {
                files.rename(source, &m.absolute).map_err(|e| {
                    fail(
                        &completed,
                        format!("Failed to rename {}", change.target.resource),
                        &e,
                        source,
                    )
                })?;
            } else {
                files.remove(source).map_err(|e| {
                    fail(
                        &completed,
                        format!(
                            "Wrote {} but failed to remove {}",
                            m.resource, change.target.resource
                        ),
                        &e,
                        source,
                    )
                })?;
            }
        }
        completed.push(dest.resource.clone());
    }

    // Format what was written and ask the language servers about it.
    let mut notes = String::new();
    let mut diagnostics = String::new();
    for (path, target, content) in &updates.0 {
        let Some(content) = content else { continue };
        if let Some(name) = ctx.format_name(path, content) {
            notes.push_str(&format!(
                "\n\n{} was reformatted by {name}, so it now differs from what you wrote; read it again before editing it.",
                target.resource
            ));
        }
        diagnostics.push_str(&ctx.diagnostics(path, false));
    }
    let mut out = String::from("Success. Updated the following files:");
    for change in &prepared {
        let status = match change.kind {
            Kind::Add => 'A',
            Kind::Delete => 'D',
            Kind::Update => 'M',
        };
        out.push_str(&format!("\n{status} {}", change.destination().resource));
    }
    Ok(Output::text(format!("{out}{notes}{diagnostics}")))
}

/// A path as a tool resolves it, without touching the disk: `~/` expanded,
/// made absolute against `cwd` and lexically normalized.
fn lexical(cwd: &Path, path: &str) -> PathBuf {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    };
    super::normalize(&if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    })
}

/// The files a `patch` call changes, each with the file its text comes
/// from: a moved file's destination with its source, every other file
/// with itself. Destinations are listed once, in first-touched order. A
/// patch that does not parse touches nothing.
pub fn touched(cwd: &Path, patch_text: &str) -> Vec<(PathBuf, PathBuf)> {
    let Ok(hunks) = format::parse(patch_text) else {
        return vec![];
    };
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    for hunk in &hunks {
        let source = lexical(cwd, hunk.path());
        let dest = match hunk {
            Hunk::Update {
                move_path: Some(m), ..
            } => lexical(cwd, m),
            _ => source.clone(),
        };
        if !out.iter().any(|(d, _)| *d == dest) {
            out.push((dest, source));
        }
    }
    out
}

/// What one hunk of a patch would do, for a preview.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    /// The hunk's file (its destination, for a move), absolute.
    pub path: PathBuf,
    /// Its text before the hunk; `None` when it does not exist.
    pub old: Option<String>,
    /// Its text after the hunk; `None` when the hunk deletes it.
    pub new: Option<String>,
}

/// What each hunk of a `patch` call would change, applied in order to the
/// files as they are now, without asking or writing. Hunks that would not
/// apply are left out.
/// Files are read with [`super::peek_text`]: only bounded regular files.
pub fn preview(cwd: &Path, patch_text: &str) -> Vec<Preview> {
    preview_with(cwd, patch_text, |p| super::peek_text(p, PREVIEW_MAX))
}

/// The largest file [`preview`] reads.
pub const PREVIEW_MAX: usize = 1024 * 1024;

/// [`preview`] reading the files as `read` does; hunks on a file it skips
/// are left out.
pub fn preview_with(
    cwd: &Path,
    patch_text: &str,
    read: impl Fn(&Path) -> super::Peek,
) -> Vec<Preview> {
    let Ok(hunks) = format::parse(patch_text) else {
        return vec![];
    };
    let mut state: Vec<(PathBuf, Option<String>)> = Vec::new();
    // `None`: the file could not be read.
    let current =
        |state: &[(PathBuf, Option<String>)], p: &Path| match state.iter().find(|(q, _)| q == p) {
            Some((_, c)) => Some(c.clone()),
            None => match read(p) {
                super::Peek::Missing => Some(None),
                super::Peek::Text(t) => Some(Some(t)),
                super::Peek::Skipped => None,
            },
        };
    let set = |state: &mut Vec<(PathBuf, Option<String>)>, p: &Path, c: Option<String>| match state
        .iter_mut()
        .find(|(q, _)| q == p)
    {
        Some(slot) => slot.1 = c,
        None => state.push((p.to_path_buf(), c)),
    };
    let mut out = Vec::new();
    for hunk in &hunks {
        let path = lexical(cwd, hunk.path());
        let Some(old) = current(&state, &path) else {
            continue;
        };
        match hunk {
            Hunk::Add { contents, .. } => {
                let new = if contents.is_empty() || contents.ends_with('\n') {
                    contents.clone()
                } else {
                    format!("{contents}\n")
                };
                set(&mut state, &path, Some(new.clone()));
                out.push(Preview {
                    path,
                    old,
                    new: Some(new),
                });
            }
            Hunk::Delete { .. } => {
                if old.is_none() {
                    continue;
                }
                set(&mut state, &path, None);
                out.push(Preview {
                    path,
                    old,
                    new: None,
                });
            }
            Hunk::Update {
                move_path, chunks, ..
            } => {
                let Some(original) = old.clone() else {
                    continue;
                };
                let Ok(update) = format::derive(hunk.path(), chunks, &original) else {
                    continue;
                };
                let new = format::join_bom(&update.content, update.bom);
                let dest = move_path
                    .as_deref()
                    .map(|m| lexical(cwd, m))
                    .unwrap_or_else(|| path.clone());
                if dest != path {
                    set(&mut state, &path, None);
                }
                set(&mut state, &dest, Some(new.clone()));
                out.push(Preview {
                    path: dest,
                    old,
                    new: Some(new),
                });
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
