//! The provider and model pickers: `/provider` lists every provider with
//! whether it is ready, and `/model` lists the current provider's models,
//! as the provider's API (or local server) last listed them.

use super::app::PickItem;
use codesmith_core::config::Config;
use codesmith_core::providers::signin::{Choice, State};
use std::collections::HashMap;

/// The model picker's last row: opens the provider picker.
pub const OTHER_PROVIDERS: &str = "provider:";

/// The provider of `provider/model[#variant]`.
pub fn provider_of(spec: &str) -> &str {
    spec.split_once('/').map(|(p, _)| p.trim()).unwrap_or("")
}

/// What the picker knows about one model.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    pub name: Option<String>,
    pub context: Option<u64>,
    pub released: i64,
}

/// The details of `pid`'s models: its kept live list (from the API or the
/// server), else the catalog's; Claude Code's as it resolves them; and the
/// configured ones over all of them.
pub fn model_info(cfg: &Config, pid: &str) -> HashMap<String, Info> {
    use codesmith_core::providers::claude_agent;
    let catalog = cfg.models_dev.as_ref().map(|c| c.catalog());
    let templates = catalog
        .as_ref()
        .and_then(|c| c.provider(pid))
        .map(|p| p.models.clone())
        .unwrap_or_default();
    let mut out: HashMap<String, Info> = HashMap::new();
    let mut add = |m: &codesmith_core::catalog::Model| {
        out.insert(
            m.id.clone(),
            Info {
                name: (m.name != m.id && !m.name.is_empty()).then(|| m.name.clone()),
                context: (m.limit.context > 0).then_some(m.limit.context),
                released: m.time.released,
            },
        );
    };
    let kept = cfg.live_models(pid);
    for m in if kept.is_empty() { &templates } else { &kept } {
        add(m);
    }
    if pid == claude_agent::ID {
        if let Some(c) = catalog.as_ref() {
            let saved = claude_agent::catalog();
            for m in &saved.models {
                if let Some(cm) = claude_agent::catalog_model(&saved, &m.id, c) {
                    let mut cm = cm.clone();
                    cm.id = m.id.clone();
                    add(&cm);
                }
            }
        }
    }
    if let Some(p) = cfg.providers.get(pid) {
        for (id, m) in &p.models {
            let e = out.entry(id.clone()).or_default();
            if m.name.is_some() {
                e.name = m.name.clone();
            }
            if m.limit.context.is_some() {
                e.context = m.limit.context;
            }
        }
    }
    out
}

/// The model picker: the chat models of the provider of `current` (or of
/// `pid` when given), newest first, then a row to switch provider.
/// Favourites and recents are ranked by the caller.
pub fn model_items(cfg: &Config, current: &str, pid: &str) -> Vec<PickItem> {
    let info = model_info(cfg, pid);
    let mut ids: Vec<String> = cfg
        .list_models(pid, &["chat"])
        .into_iter()
        .filter(|m| !codesmith_core::providers::hidden(pid, m))
        .collect();
    // Newest first; models without a date keep the listing's order after.
    ids.sort_by_key(|id| std::cmp::Reverse(info.get(id).map(|i| i.released).unwrap_or(0)));
    let current_base = current.split('#').next().unwrap_or(current);
    let mut items: Vec<PickItem> = ids
        .into_iter()
        .map(|mid| {
            let spec = format!("{pid}/{mid}");
            let i = info.get(&mid).cloned().unwrap_or_default();
            let mut detail = Vec::new();
            if let Some(n) = i.name {
                detail.push(n);
            }
            if let Some(c) = i.context {
                detail.push(format!("{}K context", c / 1000));
            }
            PickItem {
                current: spec == current_base,
                detail: detail.join(" · "),
                label: spec.clone(),
                id: spec,
            }
        })
        .collect();
    // The model in use stays choosable even when the list lacks it.
    if !current_base.is_empty()
        && provider_of(current_base) == pid
        && !items.iter().any(|i| i.current)
    {
        items.insert(
            0,
            PickItem {
                id: current_base.to_string(),
                label: current_base.to_string(),
                detail: "current".into(),
                current: true,
            },
        );
    }
    items
}

/// The model picker's closing row.
pub fn other_providers_item(pid: &str, name: &str) -> PickItem {
    PickItem {
        id: OTHER_PROVIDERS.into(),
        label: "Switch provider…".into(),
        detail: format!("now {name} ({pid}) · /provider"),
        current: false,
    }
}

/// How a provider stands, for its picker row and what choosing it does.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Credentials (or Claude Code) are there.
    Ready(String),
    /// A local server, with the models it listed when last asked.
    Local(usize),
    /// Needs a sign-in or key first.
    SignIn,
}

/// One provider row's data.
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub status: Status,
    pub models: usize,
}

fn how(state: &State) -> String {
    match state {
        State::Saved(d) => d.clone(),
        State::Env(v) => format!("${v}"),
        State::Config => "configured".into(),
        State::Ambient(d) => d.clone(),
        State::None => String::new(),
    }
}

/// Every provider: the ones the sign-in picker knows (with their state)
/// and the local servers, never a disabled one.
pub fn providers(cfg: &Config, choices: &[Choice]) -> Vec<Provider> {
    let mut out: Vec<Provider> = Vec::new();
    for c in choices {
        if cfg.provider_disabled(&c.id) {
            continue;
        }
        let ready = c.state.signed_in();
        out.push(Provider {
            id: c.id.clone(),
            name: c.name.clone(),
            status: if ready {
                Status::Ready(how(&c.state))
            } else {
                Status::SignIn
            },
            models: if ready {
                cfg.list_models(&c.id, &["chat"]).len()
            } else {
                0
            },
        });
    }
    // A configured provider that needs no key (a custom endpoint) is not in
    // the sign-in list, and is ready as configured.
    for pid in cfg.providers.keys() {
        if cfg.provider_disabled(pid)
            || out.iter().any(|p| &p.id == pid)
            || codesmith_core::providers::local::providers(cfg).contains(pid)
        {
            continue;
        }
        let name = cfg
            .provider(pid)
            .and_then(|p| p.name)
            .unwrap_or_else(|| pid.clone());
        out.push(Provider {
            id: pid.clone(),
            name,
            status: Status::Ready("configured".into()),
            models: cfg.list_models(pid, &["chat"]).len(),
        });
    }
    for pid in codesmith_core::providers::local::providers(cfg) {
        if cfg.provider_disabled(&pid) || out.iter().any(|p| p.id == pid) {
            continue;
        }
        let name = cfg
            .provider(&pid)
            .and_then(|p| p.name)
            .unwrap_or_else(|| pid.clone());
        // A local server lists models only when it answers.
        let n = cfg
            .live_account(&pid)
            .map(|a| {
                codesmith_core::providers::live::models(&pid, Some(&a), &[])
                    .iter()
                    .filter(|m| m.kind == "chat")
                    .count()
            })
            .unwrap_or(0);
        out.push(Provider {
            id: pid,
            name,
            status: Status::Local(n),
            models: n,
        });
    }
    out
}

/// The provider picker: ready providers first, then local servers that
/// answered, then the rest; the current one marked.
pub fn provider_items(list: &[Provider], current: &str) -> Vec<PickItem> {
    let rank = |p: &Provider| match &p.status {
        Status::Ready(_) => 0,
        Status::Local(n) if *n > 0 => 1,
        Status::Local(_) => 2,
        Status::SignIn => 3,
    };
    let mut sorted: Vec<&Provider> = list.iter().collect();
    sorted.sort_by_key(|p| (p.id != current, rank(p), p.name.to_lowercase()));
    sorted
        .into_iter()
        .map(|p| {
            let count = |n: usize| match n {
                0 => String::new(),
                1 => " · 1 model".into(),
                n => format!(" · {n} models"),
            };
            let detail = match &p.status {
                Status::Ready(how) if how.is_empty() => format!("ready{}", count(p.models)),
                Status::Ready(how) => format!("ready · {how}{}", count(p.models)),
                Status::Local(0) => "local · not answering".into(),
                Status::Local(n) => format!("local{}", count(*n)),
                Status::SignIn => "sign in".into(),
            };
            let detail = if codesmith_core::providers::verified::verified(&p.id) {
                detail
            } else {
                format!("{detail} · {}", codesmith_core::providers::verified::TAG)
            };
            PickItem {
                id: p.id.clone(),
                label: p.name.clone(),
                detail,
                current: p.id == current,
            }
        })
        .collect()
}

/// The model to switch to on choosing provider `pid`: the one used there
/// last, else its newest text chat model, else the first it lists.
pub fn model_for(cfg: &Config, pid: &str, recent: &[super::models::Model]) -> Option<String> {
    let listed = cfg.list_models(pid, &["chat"]);
    if let Some(m) = recent
        .iter()
        .find(|m| m.provider == pid && listed.contains(&m.model))
    {
        return Some(m.key());
    }
    let info = model_info(cfg, pid);
    let newest = listed
        .iter()
        .filter(|m| !codesmith_core::providers::hidden(pid, m))
        .max_by_key(|m| info.get(*m).map(|i| i.released).unwrap_or(0))
        .filter(|m| info.get(*m).is_some_and(|i| i.released > 0));
    newest
        .or_else(|| listed.first())
        .map(|m| format!("{pid}/{m}"))
}

#[cfg(test)]
#[path = "provider_pick_tests.rs"]
mod tests;
