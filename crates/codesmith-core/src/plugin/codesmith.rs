//! The `codesmith` tool, built in (OpenCode's `opencode` tools,
//! `tool/plugin/opencode.ts`, MIT): CodeSmith managing itself. One tool
//! whose `action` picks what it does:
//!
//! - `models`: search the models the session can use (chat, image or
//!   video), grouped by provider with the session's own provider first,
//!   newest first, only the newest of each family unless `all`, in pages.
//! - `worktree_list`, `worktree_create`, `worktree_remove`: the project's
//!   git worktrees. Creating or removing one needs the approval editing
//!   files there needs (`edit` on its directory; "always" saves `*`).
//!
//! It never changes configuration. Its rules action is `codesmith` (and
//! `edit` for the worktree changes); `"plugins": {"codesmith": false}`
//! turns it off. CodeSmith sessions keep their directory, so there is no
//! `session_move` (and no hint for one).

use super::host::{assert_live, AssertPermission, PermissionError};
use super::{BoxFuture, Plugin, Tool, ToolContext};
use crate::config::Config;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The plugin's id, in config, and its tool's name.
pub const ID: &str = "codesmith";

const DESCRIPTION: &str = "Manage CodeSmith itself. `action` picks what to do:
- \"models\": search the models available to use. Use this to turn a model name the user mentions into an exact reference (provider/model) for a tool that takes a model. Check your own provider first. Takes query, provider, type (\"chat\", the default: a language model that powers sessions and subagents and can call tools; \"image\": generates or edits images, can't chat or call tools; \"video\": generates videos, can't chat or call tools), all (include older versions of each model family), limit (1-100, default 20) and offset.
- \"worktree_list\": list the project's git worktrees.
- \"worktree_create\": create a named worktree (name). Returns its directory.
- \"worktree_remove\": remove a worktree (directory, relative to the working directory or absolute). Set force only when the user has confirmed discarding its modified and untracked files.";

/// One model as `models` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRow {
    pub provider_id: String,
    pub provider_name: String,
    pub id: String,
    pub name: String,
    /// Release time in milliseconds, 0 when unknown.
    pub released: i64,
    pub family: Option<String>,
    pub variants: Vec<String>,
    /// Pricing in USD per million tokens, as the catalog gives it.
    pub cost: Value,
    pub status: String,
}

/// What `models` was asked for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelsQuery {
    pub query: Option<String>,
    pub provider: Option<String>,
    pub all: bool,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

impl ModelsQuery {
    fn from(input: &Value) -> Result<ModelsQuery, String> {
        let limit = match input.get("limit") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n @ 1..=100) => Some(n as usize),
                _ => return Err("limit must be an integer from 1 to 100".into()),
            },
        };
        let offset = match input.get("offset") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => return Err("offset must be a non-negative integer".into()),
            },
        };
        Ok(ModelsQuery {
            query: input
                .get("query")
                .and_then(Value::as_str)
                .map(str::to_string),
            provider: input
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_string),
            all: input.get("all").and_then(Value::as_bool).unwrap_or(false),
            limit,
            offset,
        })
    }
}

/// Filter, order and page `rows` (OpenCode's `models` tool): the provider
/// by id or name and every word of the query (anywhere in the reference or
/// name, ignoring case), the session's provider `own` first, then by
/// provider, newest first; only the newest of each family unless `all`.
pub fn list_models(rows: &[ModelRow], own: Option<&str>, q: &ModelsQuery) -> Value {
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(20);
    let terms: Vec<String> = q
        .query
        .as_deref()
        .unwrap_or("")
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let provider = q.provider.as_deref().map(str::to_lowercase);
    let mut matching: Vec<&ModelRow> = rows
        .iter()
        .filter(|m| {
            provider.as_ref().is_none_or(|p| {
                m.provider_id.to_lowercase() == *p || m.provider_name.to_lowercase() == *p
            })
        })
        .filter(|m| {
            let text = format!("{}/{} {}", m.provider_id, m.id, m.name).to_lowercase();
            terms.iter().all(|t| text.contains(t))
        })
        .collect();
    matching.sort_by(|a, b| {
        let mine = |m: &ModelRow| Some(m.provider_id.as_str()) == own;
        mine(b)
            .cmp(&mine(a))
            .then_with(|| a.provider_id.cmp(&b.provider_id))
            .then_with(|| b.released.cmp(&a.released))
    });
    if !q.all {
        let mut seen: Vec<(&str, &str)> = Vec::new();
        matching.retain(|m| match &m.family {
            None => true,
            Some(f) => {
                let key = (m.provider_id.as_str(), f.as_str());
                if seen.contains(&key) {
                    false
                } else {
                    seen.push(key);
                    true
                }
            }
        });
    }
    let total = matching.len();
    let page: Vec<&ModelRow> = matching.into_iter().skip(offset).take(limit).collect();
    let mut providers: Vec<Value> = Vec::new();
    for m in &page {
        let entry = json!({
            "id": format!("{}/{}", m.provider_id, m.id),
            "name": m.name,
            "released": m.released,
            "variants": m.variants,
            "cost": m.cost,
            "status": m.status,
        });
        match providers.iter_mut().find(|p| p["id"] == m.provider_id) {
            Some(p) => p["models"].as_array_mut().unwrap().push(entry),
            None => providers.push(json!({
                "id": m.provider_id,
                "name": m.provider_name,
                "models": [entry],
            })),
        }
    }
    let next = (offset + limit < total).then_some(offset + limit);
    json!({"providers": providers, "total": total, "next": next})
}

fn cost_of(c: Option<crate::catalog::Cost>) -> Value {
    match c {
        Some(c) => {
            json!([{"input": c.input, "output": c.output, "cache": {"read": c.cache.read, "write": c.cache.write}}])
        }
        None => json!([]),
    }
}

/// The models of `kind` (`chat`, `image` or `video`) this config can use:
/// chat models of configured providers, catalog providers with a key and
/// local servers; image and video models with a supported API and a key.
/// A provider `disabled_providers` names is left out.
pub fn available(config: &Config, kind: &str) -> Vec<ModelRow> {
    let catalog = config.models_dev.as_ref().map(|c| c.catalog());
    let claude_saved = std::sync::OnceLock::new();
    let bifrost_models = std::sync::OnceLock::new();
    let info = |pid: &str, id: &str| {
        let c = catalog.as_ref()?;
        if pid == crate::providers::claude_agent::ID {
            // The model Claude Code resolves to, for its release date.
            let saved = claude_saved.get_or_init(crate::providers::claude_agent::catalog);
            return crate::providers::claude_agent::catalog_model(saved, id, c).map(|m| {
                crate::catalog::Model {
                    // Claude Code's own price and variants, not the API's.
                    cost: Vec::new(),
                    variants: Vec::new(),
                    enabled: true,
                    ..m.clone()
                }
            });
        }
        if pid == crate::providers::bifrost::ID {
            return bifrost_models
                .get_or_init(|| config.live_models(pid))
                .iter()
                .find(|m| m.id == id)
                .cloned();
        }
        c.model(pid, id).cloned()
    };
    let provider_name = |pid: &str| {
        config
            .providers
            .get(pid)
            .and_then(|p| p.name.clone())
            .or_else(|| {
                catalog
                    .as_ref()
                    .and_then(|c| c.provider(pid))
                    .map(|p| p.info.name.clone())
            })
            .or_else(|| crate::providers::builtin(pid).and_then(|p| p.name))
            .unwrap_or_else(|| pid.to_string())
    };
    let row = |pid: &str, id: &str, name: Option<String>| -> Option<ModelRow> {
        if config.provider_disabled(pid) {
            return None;
        }
        let m = info(pid, id);
        if m.as_ref().is_some_and(|m| !m.enabled) {
            return None;
        }
        let configured = config.providers.get(pid).and_then(|p| p.models.get(id));
        Some(ModelRow {
            provider_id: pid.to_string(),
            provider_name: provider_name(pid),
            id: id.to_string(),
            name: configured
                .and_then(|c| c.name.clone())
                .or(name)
                .or_else(|| m.as_ref().map(|m| m.name.clone()))
                .unwrap_or_else(|| id.to_string()),
            released: m.as_ref().map(|m| m.time.released).unwrap_or(0),
            family: m.as_ref().and_then(|m| m.family.clone()),
            variants: m
                .as_ref()
                .map(|m| m.variants.iter().map(|v| v.id.clone()).collect())
                .unwrap_or_default(),
            cost: cost_of(m.as_ref().and_then(|m| m.base_cost())),
            status: m
                .as_ref()
                .map(|m| m.status.clone())
                .unwrap_or_else(|| "active".into()),
        })
    };
    if kind != "chat" {
        return crate::generate::models(config, kind)
            .into_iter()
            .filter_map(|m| row(&m.provider_id, &m.id, Some(m.name.clone())))
            .collect();
    }
    let mut providers: Vec<String> = config.providers.keys().cloned().collect();
    // A signed-in Claude Code runs models with no key, ahead of local
    // servers, which may not be running.
    let claude = crate::providers::claude_agent::ID;
    let claude_code = crate::providers::claude_agent::signed_in(&crate::providers::process_env())
        .then(|| claude.to_string());
    for id in config
        .keyed_catalog_providers()
        .into_iter()
        .chain(claude_code)
        .chain(
            crate::providers::bifrost::ready(config)
                .then(|| crate::providers::bifrost::ID.to_string()),
        )
        .chain(crate::providers::local::providers(config))
    {
        if !providers.contains(&id) {
            providers.push(id);
        }
    }
    let mut out = Vec::new();
    for pid in &providers {
        for id in config.list_models(pid, &["chat"]) {
            if crate::providers::hidden(pid, &id) {
                continue;
            }
            out.extend(row(pid, &id, None));
        }
    }
    out
}

/// A permission the worktree actions ask for.
pub struct Permit {
    pub action: String,
    pub resources: Vec<String>,
    pub save: Vec<String>,
    pub title: String,
}

/// How the tool checks a permission: `Err` is the message the model sees.
pub type Check = dyn Fn(Permit) -> BoxFuture<'static, Result<(), String>> + Send + Sync;

/// `path` as rules name it: relative to the project when inside it, with
/// forward slashes.
fn resource(root: &Path, path: &Path) -> String {
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let rel = path
        .strip_prefix(root)
        .or_else(|_| path.strip_prefix(&real_root))
        .ok()
        .filter(|r| !r.as_os_str().is_empty());
    rel.unwrap_or(path).display().to_string().replace('\\', "/")
}

fn resolve(cwd: &Path, dir: &str) -> PathBuf {
    let dir = dir.trim();
    let p = match dir.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(dir),
    };
    let p = if p.is_absolute() { p } else { cwd.join(p) };
    p.canonicalize().unwrap_or(p)
}

async fn edit_permit(check: &Check, root: &Path, dir: &Path, what: &str) -> Result<(), String> {
    check(Permit {
        action: "edit".into(),
        resources: vec![resource(root, dir)],
        save: vec!["*".into()],
        title: format!("{what} worktree {}", dir.display()),
    })
    .await
}

/// Run the tool for `input`, checking permissions with `check`.
pub async fn run(ctx: &ToolContext, input: &Value, check: &Check) -> Result<Value, String> {
    let action = input.get("action").and_then(Value::as_str).unwrap_or("");
    match action {
        "models" => {
            let kind = match input.get("type").and_then(Value::as_str) {
                None => "chat",
                Some(k @ ("chat" | "image" | "video")) => k,
                Some(k) => {
                    return Err(format!(
                        "Unknown model type {k:?}: use \"chat\", \"image\" or \"video\"."
                    ))
                }
            };
            let q = ModelsQuery::from(input)?;
            let rows = available(&ctx.config, kind);
            let own = ctx.model.split_once('/').map(|(p, _)| p);
            Ok(list_models(&rows, own, &q))
        }
        "worktree_list" => {
            let entries = crate::worktree::list(&ctx.cwd)
                .map_err(|e| format!("Unable to list worktrees: {e:#}"))?;
            let worktrees: Vec<Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "directory": e.directory.display().to_string(),
                        "main": e.main,
                        "branch": e.branch,
                    })
                })
                .collect();
            Ok(json!({ "worktrees": worktrees }))
        }
        "worktree_create" => {
            let name = input
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .ok_or("worktree_create needs a name")?
                .to_string();
            let cfg = &ctx.config.worktree;
            let main = crate::worktree::main_worktree(&ctx.cwd)
                .map_err(|e| format!("Unable to create worktree {name}: {e:#}"))?;
            let at = crate::worktree::base_dir(cfg, &main).join(&name);
            edit_permit(check, &ctx.root, &at, "Create").await?;
            let (dir, _) = tokio::task::block_in_place(|| {
                crate::worktree::create(
                    cfg,
                    &ctx.cwd,
                    &crate::worktree::Create {
                        name: Some(name.clone()),
                        ..Default::default()
                    },
                )
            })
            .map_err(|e| format!("Unable to create worktree {name}: {e:#}"))?;
            Ok(json!({"directory": dir.display().to_string()}))
        }
        "worktree_remove" => {
            let given = input
                .get("directory")
                .and_then(Value::as_str)
                .filter(|d| !d.trim().is_empty())
                .ok_or("worktree_remove needs a directory")?;
            let force = input.get("force").and_then(Value::as_bool).unwrap_or(false);
            let dir = resolve(&ctx.cwd, given);
            let entries = crate::worktree::list(&ctx.cwd)
                .map_err(|e| format!("Unable to remove worktree {}: {e:#}", dir.display()))?;
            let linked = entries.iter().any(|e| {
                !e.main && e.directory.canonicalize().unwrap_or(e.directory.clone()) == dir
            });
            if !dir.is_dir() {
                return Err(format!(
                    "Unable to remove worktree {}: the directory does not exist or is not a directory.",
                    dir.display()
                ));
            }
            if !linked {
                return Err(format!(
                    "Unable to remove {}: it is not a worktree of this project. Use worktree_list to see the project's worktrees.",
                    dir.display()
                ));
            }
            // Removing a worktree deletes files: the approval editing them needs.
            edit_permit(check, &ctx.root, &dir, "Remove").await?;
            let d = dir.display().to_string();
            tokio::task::block_in_place(|| {
                crate::worktree::remove(&ctx.config.worktree, &ctx.cwd, &d, force)
            })
            .map_err(|e| {
                if e.to_string().contains("uncommitted changes") {
                    format!("Unable to remove worktree {d}: it has modified or untracked files.")
                } else {
                    format!("Unable to remove worktree {d}.")
                }
            })?;
            Ok(json!({"directory": d, "removed": true}))
        }
        "" => Err("The codesmith tool needs an action: models, worktree_list, worktree_create or worktree_remove.".into()),
        other => Err(format!(
            "Unknown action {other:?}: use models, worktree_list, worktree_create or worktree_remove."
        )),
    }
}

/// Checks a permission through the running session's gate.
fn session_check(session_id: String) -> Arc<Check> {
    Arc::new(move |p: Permit| {
        let input = AssertPermission {
            session_id: session_id.clone(),
            action: p.action,
            resources: p.resources,
            save: p.save,
            title: Some(p.title),
        };
        Box::pin(async move {
            assert_live(input).await.map_err(|e| match e {
                PermissionError::Corrected(f) => f,
                e => e.to_string(),
            })
        })
    })
}

struct CodesmithTool;

impl Tool for CodesmithTool {
    fn name(&self) -> &str {
        ID
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        let mut props = Map::new();
        props.insert(
            "action".into(),
            json!({"type": "string", "enum": ["models", "worktree_list", "worktree_create", "worktree_remove"]}),
        );
        props.insert("query".into(), json!({"type": "string", "description": "models: text to search for in model names and IDs."}));
        props.insert(
            "provider".into(),
            json!({"type": "string", "description": "models: provider ID or name to filter by."}),
        );
        props.insert("type".into(), json!({"type": "string", "enum": ["chat", "image", "video"], "description": "models: which kind of model to list (default chat)."}));
        props.insert("all".into(), json!({"type": "boolean", "description": "models: include older versions of each model family."}));
        props.insert("limit".into(), json!({"type": "integer", "minimum": 1, "maximum": 100, "description": "models: how many to return (default 20)."}));
        props.insert("offset".into(), json!({"type": "integer", "minimum": 0, "description": "models: how many to skip, for paging."}));
        props.insert(
            "name".into(),
            json!({"type": "string", "description": "worktree_create: the worktree's name."}),
        );
        props.insert(
            "directory".into(),
            json!({"type": "string", "description": "worktree_remove: the worktree's directory."}),
        );
        props.insert("force".into(), json!({"type": "boolean", "description": "worktree_remove: discard modified and untracked files."}));
        json!({"type": "object", "properties": props, "required": ["action"]})
    }

    fn execute<'a>(
        &'a self,
        ctx: ToolContext,
        input: Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let check = session_check(ctx.session_id.clone());
            run(&ctx, &input, check.as_ref())
                .await
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
        })
    }
}

/// The plugin [`super::builtin`] installs.
pub struct CodesmithPlugin;

impl Plugin for CodesmithPlugin {
    fn id(&self) -> &str {
        ID
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(CodesmithTool)]
    }

    fn wants(&self, _hook: super::Hook, _session_id: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: &str, pname: &str, id: &str, name: &str, released: i64) -> ModelRow {
        ModelRow {
            provider_id: pid.into(),
            provider_name: pname.into(),
            id: id.into(),
            name: name.into(),
            released,
            family: None,
            variants: vec![],
            cost: json!([]),
            status: "active".into(),
        }
    }

    fn entry(id: &str, name: &str, released: i64, variants: &[&str], status: &str) -> Value {
        json!({"id": id, "name": name, "released": released, "variants": variants, "cost": [], "status": status})
    }

    fn fixture() -> Vec<ModelRow> {
        let mut alpha = row("test", "test", "alpha", "Alpha", 300);
        alpha.variants = vec!["fast".into()];
        alpha.status = "beta".into();
        let beta = row("other", "Other Provider", "beta", "Beta", 200);
        let mut gamma = row("other", "Other Provider", "gamma", "Gamma Flash", 100);
        gamma.family = Some("gamma".into());
        let mut old = row(
            "other",
            "Other Provider",
            "gamma-old",
            "Gamma Flash Old",
            50,
        );
        old.family = Some("gamma".into());
        vec![beta, old, alpha, gamma]
    }

    fn q(v: Value) -> ModelsQuery {
        ModelsQuery::from(&v).unwrap()
    }

    // tool-opencode.test.ts: "groups available models by provider with paging"
    #[test]
    fn oc_codesmith_groups_available_models_by_provider_with_paging() {
        let rows = fixture();
        let run = |v: Value| list_models(&rows, Some("test"), &q(v));
        let alpha = entry("test/alpha", "Alpha", 300, &["fast"], "beta");
        let beta = entry("other/beta", "Beta", 200, &[], "active");
        let gamma = entry("other/gamma", "Gamma Flash", 100, &[], "active");
        let old = entry("other/gamma-old", "Gamma Flash Old", 50, &[], "active");
        // Grouped by provider, own first, newest first within each.
        assert_eq!(
            run(json!({})),
            json!({"providers": [
                {"id": "test", "name": "test", "models": [alpha]},
                {"id": "other", "name": "Other Provider", "models": [beta, gamma]},
            ], "total": 3, "next": null})
        );
        // A page can end inside a provider group.
        assert_eq!(
            run(json!({"limit": 2})),
            json!({"providers": [
                {"id": "test", "name": "test", "models": [alpha]},
                {"id": "other", "name": "Other Provider", "models": [beta]},
            ], "total": 3, "next": 2})
        );
        assert_eq!(
            run(json!({"limit": 2, "offset": 2})),
            json!({"providers": [{"id": "other", "name": "Other Provider", "models": [gamma]}], "total": 3, "next": null})
        );
        let r = run(json!({"provider": "other provider"}));
        assert_eq!(
            (r["total"].clone(), r["providers"][0]["id"].clone()),
            (json!(2), json!("other"))
        );
        assert_eq!(
            run(json!({"provider": "test"})),
            json!({"providers": [{"id": "test", "name": "test", "models": [alpha]}], "total": 1, "next": null})
        );
        // Every word of the query, anywhere in the reference or name.
        assert_eq!(
            run(json!({"query": "GAMMA"})),
            json!({"providers": [{"id": "other", "name": "Other Provider", "models": [gamma]}], "total": 1, "next": null})
        );
        assert_eq!(run(json!({"query": "test/"}))["total"], 1);
        assert_eq!(
            run(json!({"query": "other flash"}))["providers"][0]["models"],
            json!([gamma])
        );
        assert_eq!(
            run(json!({"query": "gamma beta"})),
            json!({"providers": [], "total": 0, "next": null})
        );
        // The newest of each family unless `all`; the query applies first.
        let all = run(json!({"all": true}));
        assert_eq!(all["total"], 4);
        assert_eq!(all["providers"][1]["models"], json!([beta, gamma, old]));
        assert_eq!(
            run(json!({"query": "old"}))["providers"][0]["models"],
            json!([old])
        );
        assert_eq!(
            run(json!({"provider": "other", "query": "alpha"})),
            json!({"providers": [], "total": 0, "next": null})
        );
        assert!(ModelsQuery::from(&json!({"limit": 0})).is_err());
        assert!(ModelsQuery::from(&json!({"limit": 101})).is_err());
    }
}
