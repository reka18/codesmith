//! Review dimensions: what `/calibrate` writes to `.codesmith/review.json`
//! and `/review` reads to decide which reviewers to run.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dimension {
    pub id: String,
    pub name: String,
    pub focus: String,
    /// Globs relative to the repository root; empty means everywhere.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub version: u32,
    /// RFC 3339 time of calibration.
    pub generated: String,
    /// The commit the repository was at.
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub summary: String,
    pub dimensions: Vec<Dimension>,
}

pub const VERSION: u32 = 1;

pub fn path(root: &Path) -> PathBuf {
    root.join(".codesmith").join("review.json")
}

pub fn load(root: &Path) -> Result<Option<Calibration>> {
    let p = path(root);
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Ok(None);
    };
    let c: Calibration =
        serde_json::from_str(&text).with_context(|| format!("reading {}", p.display()))?;
    validate(&c.dimensions).with_context(|| format!("in {}", p.display()))?;
    Ok(Some(c))
}

pub fn save(root: &Path, c: &Calibration) -> Result<PathBuf> {
    let p = path(root);
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, serde_json::to_string_pretty(c)? + "\n")?;
    Ok(p)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Tidy what a model proposed: slug ids, drop blanks, duplicates and
/// dimensions that repeat the always-on ones.
pub fn normalize(mut dims: Vec<Dimension>) -> Vec<Dimension> {
    let reserved = [CORRECTNESS, INSTRUCTIONS];
    let mut seen = std::collections::HashSet::new();
    for d in &mut dims {
        d.id = slug(if d.id.trim().is_empty() {
            &d.name
        } else {
            &d.id
        });
        d.paths.retain(|p| !p.trim().is_empty());
        d.paths
            .iter_mut()
            .for_each(|p| *p = p.trim().trim_start_matches("./").to_string());
        d.checks.retain(|c| !c.trim().is_empty());
    }
    dims.retain(|d| {
        !d.id.is_empty()
            && !d.focus.trim().is_empty()
            && !reserved.contains(&d.id.as_str())
            && seen.insert(d.id.clone())
    });
    dims
}

pub fn validate(dims: &[Dimension]) -> Result<()> {
    if dims.is_empty() {
        return Err(anyhow!("no review dimensions"));
    }
    let mut seen = std::collections::HashSet::new();
    for d in dims {
        if d.id.is_empty() || !seen.insert(&d.id) {
            return Err(anyhow!(
                "dimension ids must be unique and non-empty (got {:?})",
                d.id
            ));
        }
        for p in &d.paths {
            globset::Glob::new(p)
                .with_context(|| format!("dimension {}: bad path glob {p:?}", d.id))?;
        }
    }
    Ok(())
}

/// Whether a dimension covers any of the changed files.
pub fn applies(d: &Dimension, files: &[String]) -> bool {
    if d.paths.is_empty() {
        return true;
    }
    let mut b = globset::GlobSetBuilder::new();
    for p in &d.paths {
        if let Ok(g) = globset::GlobBuilder::new(p)
            .literal_separator(false)
            .build()
        {
            b.add(g);
        }
        // `src/store` should match everything below it too.
        if !p.contains('*') {
            if let Ok(g) = globset::Glob::new(&format!("{}/**", p.trim_end_matches('/'))) {
                b.add(g);
            }
        }
    }
    let Ok(set) = b.build() else { return true };
    files.iter().any(|f| set.is_match(f))
}

pub const CORRECTNESS: &str = "correctness";
pub const INSTRUCTIONS: &str = "instructions";

/// Dimensions that always run: Anthropic's bug passes and instruction compliance.
pub fn always(have_instructions: bool) -> Vec<Dimension> {
    let mut v = vec![Dimension {
        id: CORRECTNESS.into(),
        name: "Correctness".into(),
        focus: "Bugs introduced by the change: code that will not compile or load, and logic that produces wrong results, crashes, loses data or leaks resources for inputs that will occur. Includes security problems in the changed code.".into(),
        paths: vec![],
        checks: vec![
            "Every new reference resolves: names, imports, types, signatures, feature flags".into(),
            "Changed conditions, loops, indexes and error paths do what the change intends, including empty and boundary cases".into(),
            "Callers of changed functions still get what they expect".into(),
        ],
        evidence: vec![],
    }];
    if have_instructions {
        v.push(Dimension {
            id: INSTRUCTIONS.into(),
            name: "Project instructions".into(),
            focus: "Clear, unambiguous violations of the written project rules (AGENTS.md, CLAUDE.md and the like) that apply to the changed files. Quote the exact rule. A rule only applies to files in or below the folder of the file that states it.".into(),
            paths: vec![],
            checks: vec!["Quote the rule and the line that breaks it".into(), "Ignore rules the code explicitly and legitimately silences".into()],
            evidence: vec![],
        });
    }
    v
}

/// The last JSON object in a model's answer: a fenced ```json block if
/// there is one, else the outermost `{...}`.
pub fn extract_json(text: &str) -> Option<Value> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("```") {
        let after = &rest[i + 3..];
        let body_start = after.find('\n').map(|n| n + 1).unwrap_or(after.len());
        let lang = after[..body_start].trim();
        let body = &after[body_start..];
        let Some(end) = body.find("```") else { break };
        if lang.is_empty() || lang.eq_ignore_ascii_case("json") {
            blocks.push(&body[..end]);
        }
        rest = &body[end + 3..];
    }
    for b in blocks.iter().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(b.trim()) {
            return Some(v);
        }
    }
    // No usable block: the last complete JSON object anywhere in the text.
    let mut last = None;
    let mut i = 0;
    while let Some(off) = text[i..].find('{') {
        let s = i + off;
        let mut it = serde_json::Deserializer::from_str(&text[s..]).into_iter::<Value>();
        match it.next() {
            Some(Ok(v)) if v.is_object() => {
                last = Some(v);
                i = s + it.byte_offset();
            }
            _ => i = s + 1,
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dim(id: &str, paths: &[&str]) -> Dimension {
        Dimension {
            id: id.into(),
            name: id.into(),
            focus: "f".into(),
            paths: paths.iter().map(|s| s.to_string()).collect(),
            checks: vec![],
            evidence: vec![],
        }
    }

    #[test]
    fn json_extraction() {
        assert_eq!(
            extract_json("thinking\n```json\n{\"a\": 1}\n```\n"),
            Some(json!({"a": 1}))
        );
        assert_eq!(
            extract_json("```json\n{\"a\": 1}\n```\nlater\n```json\n{\"a\": 2}\n```"),
            Some(json!({"a": 2})),
            "the last block wins"
        );
        assert_eq!(
            extract_json("```rust\nfn x() {}\n```\nanswer {\"b\": true}"),
            Some(json!({"b": true}))
        );
        assert_eq!(extract_json("no json here"), None);
    }

    #[test]
    fn path_scoping() {
        let files = vec![
            "crates/core/src/store.rs".to_string(),
            "README.md".to_string(),
        ];
        assert!(applies(&dim("a", &[]), &files));
        assert!(applies(&dim("a", &["crates/core/**"]), &files));
        assert!(
            applies(&dim("a", &["crates/core"]), &files),
            "a bare folder covers what is below it"
        );
        assert!(applies(&dim("a", &["**/*.md"]), &files));
        assert!(!applies(&dim("a", &["web/**", "*.sql"]), &files));
    }

    #[test]
    fn normalizing_model_output() {
        let mut a = dim("Store Format!", &["./src/store/**", " "]);
        a.id = String::new();
        a.name = "Store Format!".into();
        let n = normalize(vec![
            a,
            dim("correctness", &[]),
            dim("store-format", &[]),
            dim("x", &[]),
        ]);
        assert_eq!(
            n.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["store-format", "x"],
            "slugged, deduplicated, reserved ids dropped"
        );
        assert_eq!(n[0].paths, ["src/store/**"]);
    }

    #[test]
    fn validation_and_round_trip() {
        assert!(validate(&[]).is_err());
        assert!(validate(&[dim("a", &["[bad"])]).is_err());
        assert!(validate(&[dim("a", &[]), dim("a", &[])]).is_err());
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load(d.path()).unwrap(), None);
        let c = Calibration {
            version: VERSION,
            generated: "t".into(),
            commit: None,
            summary: "s".into(),
            dimensions: vec![dim("a", &["src/**"])],
        };
        save(d.path(), &c).unwrap();
        assert_eq!(load(d.path()).unwrap(), Some(c));
        assert_eq!(always(false).len(), 1);
        assert_eq!(always(true)[1].id, INSTRUCTIONS);
    }
}
