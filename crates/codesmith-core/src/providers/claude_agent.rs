//! Claude Agent: the user's own Claude Code as a provider (OpenCode's
//! `plugin/provider/claude-agent`; the runtime is
//! [`codesmith_ai::claude_agent`]).
//!
//! Claude Code signs in one of two ways:
//!
//! - **A Claude Code token** (recommended): a one-year token from
//!   `claude setup-token`, saved in CodeSmith's `auth.json` (owner-only)
//!   and handed to Claude Code as `CLAUDE_CODE_OAUTH_TOKEN`. It works where
//!   the macOS keychain is locked, as over SSH, and can only make model
//!   requests. It is the one Claude Code credential CodeSmith keeps.
//! - **Claude Code's own login** (`claude auth login`), in the macOS
//!   keychain or Claude Code's own file: CodeSmith only checks it, never
//!   reads, stores or passes it.

use super::{Input, Route};
use crate::config::Cost;
use codesmith_ai::claude_agent::catalog::{self, Catalog, State};
use codesmith_ai::{Auth, Protocol, Variant};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const ID: &str = codesmith_ai::claude_agent::ID;

/// The saved Claude Code token's entry in `auth.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub token: String,
    /// When it was saved, in milliseconds since the epoch (it lasts a year
    /// from when `claude setup-token` made it, about then).
    pub created: u64,
}

/// The token saved in the auth file at `path`, if any.
pub fn token_in(path: &std::path::Path) -> Option<Token> {
    let e = crate::auth::entry_in(path, ID)?;
    if e["type"] != "token" {
        return None;
    }
    let token = e["token"].as_str().filter(|t| !t.is_empty())?.to_string();
    Some(Token {
        token,
        created: e["created"].as_u64().unwrap_or(0),
    })
}

/// Save a Claude Code token (checked first) in the auth file at `path`.
pub fn save_token_in(path: &std::path::Path, input: &str, now_ms: u64) -> anyhow::Result<Token> {
    let token = codesmith_ai::claude_agent::token::validate(input).map_err(anyhow::Error::msg)?;
    crate::auth::set_entry_in(
        path,
        ID,
        json!({"type": "token", "token": token, "created": now_ms}),
    )?;
    Ok(Token {
        token,
        created: now_ms,
    })
}

/// Remove the saved token; false when there was none.
pub fn remove_token_in(path: &std::path::Path) -> anyhow::Result<bool> {
    if token_in(path).is_none() {
        return Ok(false);
    }
    crate::auth::remove_in(path, ID)
}

/// The saved token from the default auth file (the runtime's token
/// source, see [`codesmith_ai::claude_agent::set_token_source`]).
pub fn saved_token() -> Option<String> {
    token_in(&crate::paths::auth_file()).map(|t| t.token)
}

/// Hand the saved token to Claude Code from now on.
pub fn use_saved_token() {
    codesmith_ai::claude_agent::set_token_source(saved_token);
}

/// How the saved token stands, for the sign-in list: when it expires,
/// and a warning when that is close.
pub fn describe_token(t: &Token, now_ms: u64) -> String {
    use codesmith_ai::claude_agent::token;
    let when = chrono::DateTime::from_timestamp_millis(token::expires(t.created) as i64)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "in a year".into());
    if t.created == 0 {
        "Claude Code token".into()
    } else if token::expires(t.created) <= now_ms {
        format!("Claude Code token · expired {when}: make a new one")
    } else if token::expiring(t.created, now_ms) {
        format!("Claude Code token · expires {when}: make a new one soon")
    } else {
        format!("Claude Code token · until {when}")
    }
}

/// Where the provider keeps its records and model list (the runtime's
/// folder, in CodeSmith's data folder).
pub fn storage() -> PathBuf {
    crate::paths::data_dir().join(ID)
}

/// The saved model list.
pub fn catalog() -> Catalog {
    Catalog::load(&storage())
}

/// Requests run through Claude Code, so the route has no URL of its own;
/// a configured `baseURL` reaches Claude Code as `ANTHROPIC_BASE_URL`.
pub fn route(input: &Input) -> Route {
    Route {
        protocol: Protocol::ClaudeAgent,
        base_url: input.configured_url.unwrap_or_default().to_string(),
        auth: Auth::Default,
        api_key: None,
        headers: Vec::new(),
        model_id: None,
    }
}

/// Whether `claude` can be found.
pub fn available(env: &super::Env) -> bool {
    let mut vars = BTreeMap::new();
    for k in [codesmith_ai::claude_agent::PATH_VAR, "PATH"] {
        if let Some(v) = env(k) {
            vars.insert(k.to_string(), v);
        }
    }
    codesmith_ai::claude_agent::which(&vars).is_some()
}

/// A model's effort variants, as Claude Code listed them.
pub fn variants(saved: &Catalog, model: &str) -> Vec<(String, Variant)> {
    saved
        .model(model)
        .map(|m| {
            m.efforts
                .iter()
                .map(|e| {
                    (
                        e.clone(),
                        Variant {
                            options: codesmith_ai::Options::from_value(json!({"effort": e})),
                            ..Default::default()
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A model's price: none for a subscription, a custom base URL or a
/// gateway; else the catalog price of the model Claude Code resolves, at
/// the provider whose prices the login pays.
pub fn cost(saved: &Catalog, model: &str, models: &crate::catalog::Catalog) -> Option<Cost> {
    let m = saved.model(model)?;
    let Some(source) = &saved.price_source else {
        return Some(Cost::default());
    };
    let priced = catalog::price_ids(m)
        .iter()
        .find_map(|id| models.model(source, id))?;
    let c = priced.base_cost()?;
    Some(Cost {
        input: c.input,
        output: c.output,
        cache_read: c.cache.read,
        cache_write: c.cache.write,
    })
}

/// The connection Claude Code is given, from the provider's config.
pub fn connection(p: &crate::config::ProviderConfig) -> BTreeMap<String, String> {
    let headers: Vec<(String, String)> = p.headers.clone().into_iter().collect();
    codesmith_ai::claude_agent::query::connection_env(
        p.settings.base_url.as_deref(),
        &headers,
        &p.body,
        p.settings.plugin.header_timeout,
        p.settings.plugin.chunk_timeout,
    )
}

/// Check Claude Code and save its models (`codesmith auth login
/// claude-agent`, `codesmith models claude-agent`).
pub async fn check(connection: &BTreeMap<String, String>) -> State {
    let registry = codesmith_ai::claude_agent::Registry::new(
        codesmith_ai::claude_agent::Io {
            storage: storage(),
            ..codesmith_ai::claude_agent::Io::real()
        },
        Default::default(),
    );
    let state = registry.refresh(connection).await;
    record(&storage(), &state);
    state
}

/// The file whose presence says the last check found Claude Code signed
/// out (its model list is saved either way).
const SIGNED_OUT: &str = "signed-out";

/// Keep what a check found about the login: a signed-out Claude Code still
/// lists its models, so the saved list alone does not say it is signed in.
pub fn record(storage: &std::path::Path, state: &State) {
    let marker = storage.join(SIGNED_OUT);
    match state {
        State::Ready { probe, .. }
            if codesmith_ai::claude_agent::probe::is_logged_in(&probe.account) =>
        {
            let _ = std::fs::remove_file(&marker);
        }
        State::Ready { .. } => {
            let _ = std::fs::create_dir_all(storage);
            let _ = std::fs::write(&marker, b"");
        }
        // Not installed or would not start: nothing new about the login.
        State::Missing | State::Failed(_) | State::Checking => {}
    }
}

/// Whether the saved state in `storage` says Claude Code is signed in: a
/// check listed its models and did not find it signed out.
pub fn signed_in_at(storage: &std::path::Path) -> bool {
    !storage.join(SIGNED_OUT).exists() && !Catalog::load(storage).models.is_empty()
}

/// Whether Claude Code can run models now without asking it: `claude` is
/// found and its last check found it signed in. Reads no credential.
pub fn signed_in(env: &super::Env) -> bool {
    available(env) && signed_in_at(&storage())
}

/// The models.dev entry of a Claude Code model (the model it resolves
/// to), for its release date: under the provider whose prices the login
/// pays, else Anthropic's.
pub fn catalog_model<'a>(
    saved: &Catalog,
    model: &str,
    models: &'a crate::catalog::Catalog,
) -> Option<&'a crate::catalog::Model> {
    let m = saved.model(model)?;
    let source = saved.price_source.as_deref().unwrap_or("anthropic");
    let mut ids = catalog::price_ids(m);
    // The catalog may list it only without its date (`claude-opus-4-5`).
    let undated: Vec<String> = ids
        .iter()
        .filter_map(|id| {
            let (head, date) = id.rsplit_once('-')?;
            (date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit())).then(|| head.to_string())
        })
        .collect();
    ids.extend(undated);
    [source, "anthropic"]
        .iter()
        .find_map(|p| ids.iter().find_map(|id| models.model(p, id)))
}

/// What `codesmith auth login claude-agent` says after a check.
pub fn describe(state: &State) -> anyhow::Result<String> {
    if let Some(problem) = state.problem() {
        return Err(anyhow::anyhow!(problem));
    }
    let State::Ready { probe, .. } = state else {
        return Err(anyhow::anyhow!("Claude Code could not be checked"));
    };
    let n = probe
        .models
        .iter()
        .filter(|m| m["value"] != "default")
        .count();
    Ok(format!(
        "Claude Code {} is signed in; {n} models saved. CodeSmith stores no credential for it: Claude Code keeps its own login.",
        probe.version.as_deref().unwrap_or("(unknown version)")
    ))
}
