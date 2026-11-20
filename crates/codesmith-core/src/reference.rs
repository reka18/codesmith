//! References: other directories a project points the agent at, named in
//! config under `references` — a local folder, or a Git repository CodeSmith
//! keeps a checkout of (see [`crate::repository`]). Those with a
//! description are listed to the model, and the list is an instruction
//! source, so one added, changed or removed mid-session is announced as an
//! update. Ported from OpenCode's `reference.ts`, `reference/instructions.ts`
//! and `config/plugin/reference.ts` (MIT), pinned by `tests/oc_references.rs`.
//!
//! ```jsonc
//! "references": {
//!   "docs": "../product-docs",                         // a local folder
//!   "sdk": "owner/repo",                               // a GitHub repository
//!   "api": { "repository": "gitlab.com/team/api", "branch": "v2",
//!            "description": "Use for the API's server code" },
//!   "notes": { "path": "~/notes", "description": "Use for my notes" }
//! }
//! ```
//!
//! Deliberate difference: a relative local path is resolved against the
//! project root, where OpenCode resolves it against the folder of the config
//! file that names it.

use crate::instruction_state::{self as is, Definition, Observed};
use crate::repository::{self, Cache, Refresh};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One configured reference.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Entry {
    /// A path (starting with `.`, `/` or `~`) or a repository.
    Short(String),
    Git {
        repository: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        hidden: Option<bool>,
    },
    Local {
        path: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        hidden: Option<bool>,
    },
}

/// Where a reference's files come from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Local {
        path: PathBuf,
        description: Option<String>,
        hidden: Option<bool>,
    },
    Git {
        repository: String,
        branch: Option<String>,
        description: Option<String>,
        hidden: Option<bool>,
    },
}

/// A usable reference.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    pub name: String,
    /// The folder: the local path, or the checkout's place in the cache
    /// (whether or not it was cloned yet).
    pub path: PathBuf,
    pub description: Option<String>,
    /// Kept for OpenCode configs, which hide such a reference from people
    /// (mention completion); the model still sees it.
    pub hidden: Option<bool>,
    pub source: Source,
}

fn valid_alias(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '`', ',']) && !name.contains(char::is_whitespace)
}

fn local_path(base: &Path, home: &Path, value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        return home.join(rest);
    }
    let p = Path::new(value);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        normalize(&base.join(p))
    }
}

/// `a/./b/../c` → `a/c`, without touching the disk.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The sources configured entries name, with relative paths resolved
/// against `base`. Invalid names are skipped.
pub fn sources(
    entries: &BTreeMap<String, Entry>,
    base: &Path,
    home: &Path,
) -> Vec<(String, Source)> {
    entries
        .iter()
        .filter(|(name, _)| valid_alias(name))
        .map(|(name, e)| {
            let source = match e {
                Entry::Short(s) if s.starts_with(['.', '/', '~']) => Source::Local {
                    path: local_path(base, home, s),
                    description: None,
                    hidden: None,
                },
                Entry::Short(s) => Source::Git {
                    repository: s.clone(),
                    branch: None,
                    description: None,
                    hidden: None,
                },
                Entry::Local {
                    path,
                    description,
                    hidden,
                } => Source::Local {
                    path: local_path(base, home, path),
                    description: description.clone(),
                    hidden: *hidden,
                },
                Entry::Git {
                    repository,
                    branch,
                    description,
                    hidden,
                } => Source::Git {
                    repository: repository.clone(),
                    branch: branch.clone(),
                    description: description.clone(),
                    hidden: *hidden,
                },
            };
            (name.clone(), source)
        })
        .collect()
}

/// The references sources resolve to: a Git source must name a remote
/// repository and a valid branch, or it is left out.
pub fn list(sources: &[(String, Source)], cache: &Cache) -> Vec<Reference> {
    sources
        .iter()
        .filter_map(|(name, source)| {
            let (path, description, hidden) = match source {
                Source::Local {
                    path,
                    description,
                    hidden,
                } => (path.clone(), description.clone(), *hidden),
                Source::Git {
                    repository,
                    branch,
                    description,
                    hidden,
                } => {
                    let r = repository::parse(repository).filter(repository::is_remote)?;
                    if let Some(b) = branch {
                        repository::validate_branch(b).ok()?;
                    }
                    (
                        cache.path(&r, branch.as_deref()),
                        description.clone(),
                        *hidden,
                    )
                }
            };
            Some(Reference {
                name: name.clone(),
                path,
                description,
                hidden,
                source: source.clone(),
            })
        })
        .collect()
}

/// The references a config names.
pub fn configured(config: &crate::config::Config, root: &Path) -> Vec<Reference> {
    let home = dirs::home_dir().unwrap_or_default();
    list(&sources(&config.references, root, &home), &Cache::default())
}

/// Make sure each Git reference's checkout exists, refreshing it at most
/// daily. Failures are skipped (the reference stays listed; its folder
/// fills in on a later try).
pub fn refresh(
    references: &[Reference],
    cache: &Cache,
) -> Vec<Result<repository::Checkout, repository::CacheError>> {
    references
        .iter()
        .filter_map(|r| match &r.source {
            Source::Git {
                repository: repo,
                branch,
                ..
            } => {
                let parsed = repository::parse_remote(repo).ok()?;
                Some(cache.ensure(&parsed, branch.as_deref(), Refresh::Daily))
            }
            Source::Local { .. } => None,
        })
        .collect()
}

/// [`refresh`] on a background thread, at most once an hour per process
/// for the same checkouts.
pub fn refresh_in_background(references: Vec<Reference>) {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static LAST: Mutex<BTreeMap<PathBuf, Instant>> = Mutex::new(BTreeMap::new());
    let due: Vec<Reference> = {
        let mut last = LAST.lock().unwrap();
        references
            .into_iter()
            .filter(|r| matches!(r.source, Source::Git { .. }))
            .filter(|r| {
                let fresh = last
                    .get(&r.path)
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(3600));
                if !fresh {
                    last.insert(r.path.clone(), Instant::now());
                }
                !fresh
            })
            .collect()
    };
    if due.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let _ = refresh(&due, &Cache::default());
    });
}

/// What the model is told about one reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn entries(refs: &[Summary]) -> Vec<String> {
    refs.iter()
        .flat_map(|r| {
            let mut v = vec![
                "  <reference>".to_string(),
                format!("    <name>{}</name>", r.name),
                format!("    <path>{}</path>", r.path),
            ];
            if let Some(d) = &r.description {
                v.push(format!("    <description>{d}</description>"));
            }
            v.push("  </reference>".into());
            v
        })
        .collect()
}

fn render(refs: &[Summary]) -> String {
    let mut lines = vec![
        "Project references provide additional directories that can be accessed when relevant."
            .to_string(),
        "<available_references>".into(),
    ];
    lines.extend(entries(refs));
    lines.push("</available_references>".into());
    lines.join("\n")
}

const UPDATE: is::ListUpdate<Summary> = is::ListUpdate {
    key: |r| r.name.clone(),
    changed: |a, b| a.path != b.path || a.description != b.description,
    entries,
    render,
    replaced: "The available project references have changed. This list supersedes the previous reference list.",
    added: "New project references are available in addition to those previously listed:",
    changed_text: "The following project references have changed. Each entry replaces the previously listed reference with the same name:",
    removed: |names| {
        format!("The following project references are no longer available and must not be used: {names}.")
    },
};

pub const KEY: &str = "core/reference-guidance";

/// The references with a description, as an instruction source.
pub fn source(references: &[Reference]) -> is::List {
    let mut list: Vec<Summary> = references
        .iter()
        .filter(|r| r.description.is_some())
        .map(|r| Summary {
            name: r.name.clone(),
            path: r.path.display().to_string(),
            description: r.description.clone(),
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    is::make::<Vec<Summary>>(Definition {
        key: KEY.into(),
        read: Box::new(move || {
            if list.is_empty() {
                Observed::Removed
            } else {
                Observed::Value(list.clone())
            }
        }),
        initial: Box::new(|r| render(r)),
        changed: Box::new(|a, b| Some(UPDATE.render(a, b))),
        removed: Some(Box::new(|_| {
            "Project reference guidance is no longer available. Do not use previously listed references."
                .into()
        })),
    })
}
