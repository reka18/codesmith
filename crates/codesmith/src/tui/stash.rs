//! The prompt stash (`prompt.stash`, `prompt.stash.pop`) and the `@`
//! file frecency, kept as OpenCode keeps them: `prompt-stash.jsonl`, its
//! latest 50 prompts, and `frecency.jsonl`, how often and how lately each
//! file was mentioned (its latest 1000 files). Lines that do not parse are
//! skipped, and a file is rewritten when lines were dropped.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const MAX_STASH_ENTRIES: usize = 50;
pub const MAX_FRECENCY_ENTRIES: usize = 1000;

/// The stashed prompts in `text`, oldest first: those with a prompt (its
/// text a string) and a numeric timestamp, the latest
/// [`MAX_STASH_ENTRIES`].
pub fn parse_stash(text: &str) -> Vec<Value> {
    let all: Vec<Value> = text
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["prompt"]["text"].is_string() && v["timestamp"].is_number())
        .collect();
    let skip = all.len().saturating_sub(MAX_STASH_ENTRIES);
    all.into_iter().skip(skip).collect()
}

/// How often and when (ms) a file was last mentioned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Use {
    pub frequency: u64,
    pub last_open: i64,
}

/// The files in `text` with their latest state, the latest opened first,
/// at most [`MAX_FRECENCY_ENTRIES`].
pub fn parse_frecency(text: &str) -> Vec<(String, Use)> {
    let mut latest: HashMap<String, Use> = HashMap::new();
    for l in text.lines().filter(|l| !l.is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let (Some(path), Some(frequency), Some(last_open)) = (
            v["path"].as_str(),
            v["frequency"].as_u64(),
            v["lastOpen"].as_i64(),
        ) else {
            continue;
        };
        latest.insert(
            path.to_string(),
            Use {
                frequency,
                last_open,
            },
        );
    }
    let mut all: Vec<(String, Use)> = latest.into_iter().collect();
    all.sort_by(|a, b| b.1.last_open.cmp(&a.1.last_open).then(a.0.cmp(&b.0)));
    all.truncate(MAX_FRECENCY_ENTRIES);
    all
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn rewrite(path: &Path, lines: impl Iterator<Item = String>) {
    let text: String = lines.map(|l| l + "\n").collect();
    let _ = codesmith_core::paths::write_private(path, text.as_bytes());
}

/// The stash in its file.
#[derive(Debug, Default)]
pub struct Stash {
    path: PathBuf,
    pub entries: Vec<Value>,
}

impl Stash {
    pub fn file() -> PathBuf {
        codesmith_core::paths::data_dir().join("prompt-stash.jsonl")
    }

    pub fn load(path: PathBuf) -> Stash {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let entries = parse_stash(&text);
        let s = Stash { path, entries };
        if !s.entries.is_empty() && s.entries.len() != text.lines().count() {
            s.save();
        }
        s
    }

    fn save(&self) {
        rewrite(&self.path, self.entries.iter().map(|e| e.to_string()));
    }

    /// Keep `text` as the newest stashed prompt.
    pub fn push(&mut self, text: &str) {
        let e = json!({ "prompt": { "text": text, "files": [], "agents": [], "pasted": [] }, "timestamp": now_ms() });
        self.entries.push(e.clone());
        if self.entries.len() > MAX_STASH_ENTRIES {
            self.entries.drain(..self.entries.len() - MAX_STASH_ENTRIES);
            self.save();
            return;
        }
        let _ = super::history::append_text(&self.path, &format!("{e}\n"));
    }

    /// Take the newest stashed prompt's text.
    pub fn pop(&mut self) -> Option<String> {
        let e = self.entries.pop()?;
        self.save();
        e["prompt"]["text"].as_str().map(str::to_string)
    }
}

/// The frecency in its file.
#[derive(Debug, Default)]
pub struct Frecency {
    path: PathBuf,
    pub data: HashMap<String, Use>,
}

impl Frecency {
    pub fn file() -> PathBuf {
        codesmith_core::paths::data_dir().join("frecency.jsonl")
    }

    pub fn load(path: PathBuf) -> Frecency {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let all = parse_frecency(&text);
        let f = Frecency {
            path,
            data: all.iter().cloned().collect(),
        };
        if !all.is_empty() && all.len() != text.lines().count() {
            f.save();
        }
        f
    }

    fn save(&self) {
        let mut all: Vec<(&String, &Use)> = self.data.iter().collect();
        all.sort_by_key(|e| std::cmp::Reverse(e.1.last_open));
        rewrite(
            &self.path,
            all.into_iter().map(|(p, u)| {
                json!({ "path": p, "frequency": u.frequency, "lastOpen": u.last_open }).to_string()
            }),
        );
    }

    /// The file at `abs` was mentioned.
    pub fn touch(&mut self, abs: &str) {
        let u = Use {
            frequency: self.data.get(abs).map_or(0, |u| u.frequency) + 1,
            last_open: now_ms(),
        };
        self.data.insert(abs.to_string(), u);
        let line = json!({ "path": abs, "frequency": u.frequency, "lastOpen": u.last_open });
        let _ = super::history::append_text(&self.path, &format!("{line}\n"));
        if self.data.len() > MAX_FRECENCY_ENTRIES {
            let mut all: Vec<(String, Use)> = self.data.drain().collect();
            all.sort_by_key(|e| std::cmp::Reverse(e.1.last_open));
            all.truncate(MAX_FRECENCY_ENTRIES);
            self.data = all.into_iter().collect();
            self.save();
        }
    }
}

/// A file's frecency, as OpenCode's: its uses, less for each day since.
pub fn score(u: Option<&Use>, now: i64) -> f64 {
    u.map_or(0.0, |u| {
        u.frequency as f64 / (1.0 + (now - u.last_open).max(0) as f64 / 86_400_000.0)
    })
}

/// `files` (relative to `cwd`) with the most frecent first, the rest in
/// their order: how the `@` list orders before anything is typed.
pub fn rank(files: &mut [String], cwd: &Path, data: &HashMap<String, Use>) {
    if data.is_empty() {
        return;
    }
    let now = now_ms();
    let key = |f: &String| score(data.get(&cwd.join(f).display().to_string()), now);
    files.sort_by(|a, b| key(b).total_cmp(&key(a)));
}
