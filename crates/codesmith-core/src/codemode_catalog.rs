//! The Code Mode tool catalog as an instruction source: a canonical summary
//! of the tools callable inside `execute`, its system-prompt text, and the
//! short update the model sees when tools come, go or change mid-session.
//! Ported from OpenCode's `codemode/catalog.ts` and `codemode/instructions.ts`
//! (MIT), pinned by `tests/oc_instruction_state.rs`.
//!
//! As in OpenCode, the catalog lists signatures within an inline token
//! budget ([`INLINE_BUDGET`]) and points the model at the global
//! `search(...)` for the rest; dots in a tool's path nest it, so a
//! namespace is the path's first segment.

use crate::instruction_state::{self as is, Definition, Observed};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One tool in the inventory.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub namespace: String,
    pub name: String,
    pub description: String,
    /// How the catalog shows a call, e.g. `tools.ns.name({ q: string }): Promise<unknown>`.
    pub signature: String,
    /// Always listed, whatever the budget.
    pub pinned: bool,
}

impl Tool {
    pub fn path(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }
}

/// The tools callable inside `execute`, and namespace descriptions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inventory {
    pub tools: Vec<Tool>,
    pub descriptions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub path: String,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamespaceSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Every tool's path, so an update can tell a new tool from one that
    /// was not shown.
    pub tools: Vec<String>,
    pub entries: Vec<Listing>,
}

/// What the catalog shows: every namespace, and the listings that fit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub total: usize,
    pub namespaces: Vec<NamespaceSummary>,
}

const DESCRIPTION_LIMIT: usize = 500;
const CHARACTERS_PER_TOKEN: f64 = 4.0;
/// OpenCode's inline budget, in tokens.
pub const INLINE_BUDGET: usize = 2_000;
const UPDATE_BUDGET: f64 = 1_000.0;

/// The token cost OpenCode charges for text.
pub fn cost(text: &str) -> usize {
    (text.chars().count() as f64 / CHARACTERS_PER_TOKEN).round() as usize
}

fn listing(t: &Tool) -> Listing {
    let first = t.description.split('\n').next().unwrap_or("").trim();
    let description: String = if first.chars().count() > DESCRIPTION_LIMIT {
        first
            .chars()
            .take(DESCRIPTION_LIMIT - 3)
            .collect::<String>()
            + "..."
    } else {
        first.to_string()
    };
    let suffix = if description.is_empty() {
        String::new()
    } else {
        format!(" // {description}")
    };
    Listing {
        path: t.path(),
        line: format!("  - {}{suffix}", t.signature),
    }
}

/// `- ns (N tools[, M shown | none shown]) // description`.
pub fn namespace_line(ns: &NamespaceSummary, shown: Option<usize>) -> String {
    let shown = shown.unwrap_or(ns.entries.len());
    let n = ns.tools.len();
    let count = if n == 1 {
        "1 tool".to_string()
    } else {
        format!("{n} tools")
    };
    let label = if shown == n {
        count
    } else if shown == 0 {
        format!("{count}, none shown")
    } else {
        format!("{count}, {shown} shown")
    };
    let desc = ns
        .description
        .as_ref()
        .map(|d| format!(" // {d}"))
        .unwrap_or_default();
    format!("- {} ({label}){desc}", ns.name)
}

/// Keep every namespace visible, then select full listings one per
/// namespace per round, in path order, until the budget (tokens; `None`:
/// unlimited) is spent. Pinned tools are always listed.
pub fn summarize(inventory: &Inventory, budget: Option<usize>) -> Summary {
    let mut groups: BTreeMap<String, Vec<&Tool>> = BTreeMap::new();
    for t in &inventory.tools {
        let path = t.path();
        let top = path.split('.').next().unwrap_or(&path).to_string();
        groups.entry(top).or_default().push(t);
    }
    struct Work<'a> {
        summary: NamespaceSummary,
        order: Vec<&'a Tool>,
        index: usize,
    }
    let mut work: Vec<Work> = groups
        .into_iter()
        .map(|(name, mut tools)| {
            tools.sort_by_key(|t| t.path());
            let description = inventory.descriptions.get(&name).cloned();
            Work {
                summary: NamespaceSummary {
                    name,
                    description,
                    tools: tools.iter().map(|t| t.path()).collect(),
                    entries: tools
                        .iter()
                        .filter(|t| t.pinned)
                        .map(|t| listing(t))
                        .collect(),
                },
                order: tools.into_iter().filter(|t| !t.pinned).collect(),
                index: 0,
            }
        })
        .collect();
    let mut remaining: f64 = match budget {
        None => f64::INFINITY,
        Some(b) => {
            let mut r = b as f64;
            for w in &work {
                let bare = NamespaceSummary {
                    entries: vec![],
                    ..w.summary.clone()
                };
                r -= cost(&namespace_line(&bare, None)) as f64;
                r -= w
                    .summary
                    .entries
                    .iter()
                    .map(|l| cost(&l.line) as f64)
                    .sum::<f64>();
            }
            r
        }
    };
    let mut active: Vec<usize> = (0..work.len()).collect();
    while !active.is_empty() {
        let mut next = Vec::new();
        for i in active {
            let w = &mut work[i];
            let Some(tool) = w.order.get(w.index) else {
                continue;
            };
            let candidate = listing(tool);
            let c = cost(&candidate.line) as f64;
            if c > remaining {
                continue;
            }
            w.summary.entries.push(candidate);
            w.index += 1;
            remaining -= c;
            if w.index < w.order.len() {
                next.push(i);
            }
        }
        active = next;
    }
    Summary {
        total: inventory.tools.len(),
        namespaces: work.into_iter().map(|w| w.summary).collect(),
    }
}

/// Any tool's listing as the catalog would show it.
pub fn lookup(inventory: &Inventory) -> impl Fn(&str) -> Option<Listing> + '_ {
    move |path| {
        inventory
            .tools
            .iter()
            .find(|t| t.path() == path)
            .map(listing)
    }
}

/// The exact call expression for a path: `tools.a.b`, with bracket
/// notation for a segment that is not an identifier.
pub fn tool_expression(path: &str) -> String {
    let mut s = String::from("tools");
    for seg in path.split('.') {
        if crate::codemode::signature::is_identifier(seg) {
            s.push('.');
            s.push_str(seg);
        } else {
            s.push_str(&format!("[{}]", serde_json::to_string(seg).unwrap()));
        }
    }
    s
}

fn complete(s: &Summary) -> bool {
    s.namespaces
        .iter()
        .all(|n| n.entries.len() == n.tools.len())
}

fn prompt(more: bool) -> String {
    let lead = format!(
        "# Code Mode\n\nThe tools listed below{} cannot be called directly: they only work as JavaScript inside the `execute` tool. Every other tool you have is a normal tool call: call it directly, never inside `execute`.\n\n",
        if more { ", and `search`," } else { "" }
    );
    let body = if more {
        format!("The catalog is partial. Before calling a tool that is not listed below, find it with the global `search(...)` function. Do not guess tool names. Each result's `path` is the complete call expression: write it out as code, for example `await tools.ns.tool_name({{ ... }})`. Do not use it as a key such as `tools[path]`.\n\n- {}", crate::codemode::search_signature())
    } else {
        "The catalog is complete. Do not guess tool names.".to_string()
    };
    format!("{lead}{body}\n\n## Code Mode tools (call only inside `execute`)")
}

fn search_note() -> String {
    format!("You can also look up tools that aren't listed with the global `search(...)` function inside `execute`. Before calling a tool that is not listed, find it this way. Do not guess tool names. Each result's `path` is the complete call expression: write it out as code, for example `await tools.ns.tool_name({{ ... }})`. Do not use it as a key such as `tools[path]`.\n\n- {}", crate::codemode::search_signature())
}

pub const EMPTY: &str = "No Code Mode tools are currently available. Later Code Mode catalog updates may add or remove tools. Do not call `execute` unless there is at least one available Code Mode tool.";

/// The catalog section of the system prompt.
pub fn render(catalog: &Summary) -> String {
    if catalog.total == 0 {
        return EMPTY.into();
    }
    let lines: Vec<String> = catalog
        .namespaces
        .iter()
        .flat_map(|ns| {
            std::iter::once(namespace_line(ns, None))
                .chain(ns.entries.iter().map(|e| e.line.clone()))
        })
        .collect();
    format!("{}\n\n{}", prompt(!complete(catalog)), lines.join("\n"))
}

/// The leading listings whose cost fits `budget`.
fn fit(listings: Vec<Listing>, budget: f64) -> Vec<Listing> {
    let mut total = 0usize;
    let mut out = Vec::new();
    for l in listings {
        total += cost(&l.line);
        if total as f64 > budget {
            break;
        }
        out.push(l);
    }
    out
}

/// What changed for the model: namespaces and tools that came or went, and
/// signatures it was shown that changed, even if no longer shown. Shown
/// tools shifting because other namespaces came or went is left out. `None`
/// when nothing the model knows changed; the whole catalog restated when
/// the delta would be longer.
pub fn update(
    previous: &Summary,
    current: &Summary,
    lookup: &dyn Fn(&str) -> Option<Listing>,
) -> Option<String> {
    let replacement = format!(
        "The Code Mode tool catalog has changed. This catalog supersedes the previous Code Mode tool catalog.\n\n{}",
        render(current)
    );
    if current.total == 0 || previous.total == 0 {
        return Some(replacement);
    }
    let namespaces = is::diff_by_key(
        &previous.namespaces,
        &current.namespaces,
        |n| n.name.clone(),
        |a, b| a.description != b.description,
    );
    // A namespace whose description changed is restated as if new.
    let restated: Vec<NamespaceSummary> = namespaces
        .added
        .iter()
        .cloned()
        .chain(namespaces.changed.iter().map(|(_, c)| c.clone()))
        .collect();
    let unchanged: Vec<&str> = current
        .namespaces
        .iter()
        .map(|n| n.name.as_str())
        .filter(|name| !restated.iter().any(|r| r.name == *name))
        .collect();
    let kept = |path: &str| unchanged.contains(&path.split('.').next().unwrap_or(path));
    let before: Vec<&String> = previous.namespaces.iter().flat_map(|n| &n.tools).collect();
    let after: Vec<&String> = current.namespaces.iter().flat_map(|n| &n.tools).collect();
    let added: Vec<&String> = after
        .iter()
        .copied()
        .filter(|p| kept(p) && !before.contains(p))
        .collect();
    let removed: Vec<&String> = before
        .iter()
        .copied()
        .filter(|p| kept(p) && !after.contains(p))
        .collect();
    let changed: Vec<Listing> = previous
        .namespaces
        .iter()
        .flat_map(|n| &n.entries)
        .filter(|e| kept(&e.path) && after.contains(&&e.path))
        .filter_map(|e| lookup(&e.path).filter(|l| l.line != e.line))
        .collect();
    let budget = if complete(current) {
        f64::INFINITY
    } else {
        let n = restated.len() + usize::from(!added.is_empty());
        UPDATE_BUDGET / n.max(1) as f64
    };
    let mut parts = vec!["The Code Mode tool catalog has changed.".to_string()];
    if !restated.is_empty() {
        let mut lines = vec!["New tools are available:".to_string()];
        for ns in &restated {
            let shown = fit(ns.tools.iter().filter_map(|p| lookup(p)).collect(), budget);
            let mut block = vec![namespace_line(ns, Some(shown.len()))];
            block.extend(shown.into_iter().map(|l| l.line));
            lines.push(block.join("\n"));
        }
        parts.push(lines.join("\n"));
    }
    if !added.is_empty() {
        let shown = fit(added.iter().filter_map(|p| lookup(p)).collect(), budget);
        let rest = added.len() - shown.len();
        let mut lines =
            vec!["New tools are available in addition to those previously listed:".to_string()];
        lines.extend(shown.into_iter().map(|l| l.line));
        if rest > 0 {
            lines.push(format!("  - {rest} more; search for them"));
        }
        parts.push(lines.join("\n"));
    }
    if complete(previous) && !complete(current) {
        parts.push(search_note());
    }
    if !changed.is_empty() {
        let mut lines =
            vec!["Changed tool listings supersede the previously listed ones:".to_string()];
        lines.extend(changed.into_iter().map(|l| l.line));
        parts.push(lines.join("\n"));
    }
    if !removed.is_empty() {
        parts.push(format!(
            "The following tools are no longer available and must not be called: {}.",
            removed
                .iter()
                .map(|p| tool_expression(p))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !namespaces.removed.is_empty() {
        parts.push(format!(
            "The following tool namespaces are no longer available and must not be used: {}.",
            namespaces
                .removed
                .iter()
                .map(|n| format!("`{}`", n.name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if parts.len() == 1 {
        return None;
    }
    let delta = parts.join("\n\n");
    if delta.chars().count() < replacement.chars().count() {
        Some(delta)
    } else {
        Some(replacement)
    }
}

pub const KEY: &str = "core/codemode";

/// The catalog as an instruction source; `None` (Code Mode off) reads as
/// removed. Updates render against `inventory`, the one that produced the
/// current summary.
pub fn source(inventory: Option<Inventory>, budget: Option<usize>) -> is::List {
    let summary = inventory.as_ref().map(|i| summarize(i, budget));
    is::make::<Summary>(Definition {
        key: KEY.into(),
        read: Box::new(move || match &summary {
            Some(s) => Observed::Value(s.clone()),
            None => Observed::Removed,
        }),
        initial: Box::new(render),
        changed: Box::new(move |before, after| match &inventory {
            Some(inv) => update(before, after, &lookup(inv)),
            None => update(before, after, &|_| None),
        }),
        removed: Some(Box::new(|_| {
            "The `execute` tool is no longer available. Permitted tools are now available as direct tool calls.".into()
        })),
    })
}

/// The Code Mode inventory for CodeSmith's Code Mode entries.
pub fn inventory(entries: &[crate::codemode::Entry]) -> Inventory {
    Inventory {
        tools: entries
            .iter()
            .map(|e| Tool {
                namespace: e.namespace.clone(),
                name: e.name.clone(),
                description: e.description.clone(),
                signature: crate::codemode::signature(e),
                pinned: false,
            })
            .collect(),
        descriptions: BTreeMap::new(),
    }
}
