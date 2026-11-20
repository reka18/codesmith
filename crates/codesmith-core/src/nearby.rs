//! Nested instructions on read, as OpenCode's read tool and
//! `SessionInstructions` load them: once a `read` of a file (or a listing of
//! a directory) inside the project succeeds, the `AGENTS.md` files (else
//! `CLAUDE.md`) of the folders from it up to the working directory, which
//! the baseline does not cover, are given to the model in a message of
//! their own after the step's tool results: `Instructions from: <path>`
//! sections, nearest first.
//!
//! Each file is given once while its message is in the history the model
//! sees: the messages are the ledger, so a file whose message an undo or a
//! compaction dropped is given again by the next read under it. Reading an
//! instruction file itself gives nothing more, and so does a read outside
//! the project.

use codesmith_ai::{Message, Part, Role};
use std::path::{Path, PathBuf};

const HEAD: &str = "Instructions from: ";

/// Whether `path` names an instruction file.
fn is_instruction_file(path: &str) -> bool {
    matches!(
        Path::new(path).file_name().and_then(|n| n.to_str()),
        Some("AGENTS.md" | "CLAUDE.md")
    )
}

/// The files a nested instruction message gives, if `text` is one.
pub fn paths_of(text: &str) -> Option<Vec<PathBuf>> {
    let first = text.strip_prefix(HEAD)?.lines().next()?;
    if !is_instruction_file(first) {
        return None;
    }
    let mut out = vec![PathBuf::from(first)];
    for section in text.split("\n\nInstructions from: ").skip(1) {
        let path = section.lines().next().unwrap_or("");
        if is_instruction_file(path) {
            out.push(PathBuf::from(path));
        }
    }
    Some(out)
}

/// Whether `m` is a nested instruction message (the engine's, not the user's).
pub fn is_nested(m: &Message) -> bool {
    m.role == Role::User
        && !m.parts.is_empty()
        && m.parts
            .iter()
            .all(|p| matches!(p, Part::Text(t) if paths_of(&t.text).is_some()))
}

/// The instruction files the messages in `history` already gave.
pub fn given(history: &[Message]) -> Vec<PathBuf> {
    history
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            Part::Text(t) => paths_of(&t.text),
            _ => None,
        })
        .flatten()
        .collect()
}

/// The instruction files a read of `target` (resolved, existing) brings
/// in: from it (a directory) or its folder up to `cwd`, exclusive, nearest
/// first. Nothing for a target outside `root`.
pub fn discover(cwd: &Path, root: &Path, target: &Path) -> Vec<PathBuf> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let (cwd, root, target) = (canon(cwd), canon(root), canon(target));
    if !target.starts_with(&root) {
        return Vec::new();
    }
    let start = if target.is_dir() {
        target.as_path()
    } else {
        match target.parent() {
            Some(p) => p,
            None => return Vec::new(),
        }
    };
    let claude = crate::instructions::claude_fallback();
    let mut out = Vec::new();
    for d in start.ancestors() {
        // The working directory and the folders above it are in the baseline.
        if cwd.starts_with(d) || !d.starts_with(&root) {
            break;
        }
        let agents = d.join("AGENTS.md");
        let claude_md = d.join("CLAUDE.md");
        let file = if agents.is_file() {
            agents
        } else if claude && claude_md.is_file() {
            claude_md
        } else {
            continue;
        };
        if file != target {
            out.push(file);
        }
    }
    out
}

/// The message giving `files`, those of them that can be read.
pub fn message(files: &[PathBuf]) -> Option<String> {
    let sections: Vec<String> = files
        .iter()
        .filter_map(|f| {
            let text = std::fs::read_to_string(f).ok()?;
            Some(format!("{HEAD}{}\n{}", f.display(), text.trim_end()))
        })
        .collect();
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

/// The messages to add after a step, one per successful `read` among the
/// calls of `assistant` (answered in `results`), in call order, given what
/// `history` already gave.
pub fn after_step(
    cwd: &Path,
    root: &Path,
    assistant: &Message,
    results: &Message,
    history: &[Message],
) -> Vec<String> {
    let mut seen = given(history);
    let mut out = Vec::new();
    for p in &assistant.parts {
        let Part::ToolCall {
            id, name, input, ..
        } = p
        else {
            continue;
        };
        if name != crate::tool::Tool::Read.name() {
            continue;
        }
        let ok = results
            .parts
            .iter()
            .any(|r| matches!(r, Part::ToolResult { id: rid, is_error: false, .. } if rid == id));
        let Some(path) = input.get("path").and_then(|v| v.as_str()).filter(|_| ok) else {
            continue;
        };
        let target = match path.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None => cwd.join(path),
        };
        let files: Vec<PathBuf> = discover(cwd, root, &target)
            .into_iter()
            .filter(|f| !seen.contains(f))
            .collect();
        if let Some(text) = message(&files) {
            seen.extend(files);
            out.push(text);
        }
    }
    out
}
