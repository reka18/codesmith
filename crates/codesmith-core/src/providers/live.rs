//! Model lists a provider serves for the signed-in account (Modal's proxy,
//! Copilot's `/models`), fetched by `codesmith models <provider>` and kept
//! in the data folder's `live-models.json` (only you can read it). Model
//! resolution reads the kept list, so no request is made unless asked:
//! CodeSmith fetches nothing in the background.
//!
//! Each provider's raw list is kept as fetched, with the account it was
//! fetched for (a fingerprint, never the key), and turned into catalog
//! models when read, so a newer catalog's templates apply.

use anyhow::Result;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub fn file() -> PathBuf {
    crate::paths::data_dir().join("live-models.json")
}

/// A short fingerprint of a credential, to tell accounts apart.
pub fn fingerprint(secret: &str) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(secret.as_bytes()))[..16].to_string()
}

/// The kept list of a provider: `{"account", "baseURL", "fetched",
/// "models"}`.
pub fn load_in(path: &Path, provider: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get(provider).cloned()
}

pub fn load(provider: &str) -> Option<Value> {
    load_in(&file(), provider)
}

/// Keep a provider's freshly fetched list, replacing the previous one.
pub fn save_in(
    path: &Path,
    provider: &str,
    account: &str,
    base_url: &str,
    models: Vec<Value>,
) -> Result<()> {
    let _lock = crate::auth::FileLock::acquire(path)?;
    let mut all: Map<String, Value> = crate::auth::read_for_update(path)?;
    all.insert(
        provider.to_string(),
        json!({
            "account": account,
            "baseURL": base_url,
            "fetched": super::oauth::now_ms(),
            "models": models,
        }),
    );
    crate::paths::write_private(path, serde_json::to_string_pretty(&all)?.as_bytes())?;
    Ok(())
}

pub fn save(provider: &str, account: &str, base_url: &str, models: Vec<Value>) -> Result<()> {
    save_in(&file(), provider, account, base_url, models)
}

/// The kept models of a provider as catalog models, when it keeps a list
/// for the account `account` (any account when `None`).
pub fn models(
    provider: &str,
    account: Option<&str>,
    templates: &[crate::catalog::Model],
) -> Vec<crate::catalog::Model> {
    let Some(kept) = load(provider) else {
        return vec![];
    };
    if account.is_some_and(|a| kept["account"].as_str() != Some(a)) {
        return vec![];
    }
    derive(provider, &kept, templates)
}

/// A kept list as catalog models.
pub fn derive(
    provider: &str,
    kept: &Value,
    templates: &[crate::catalog::Model],
) -> Vec<crate::catalog::Model> {
    let base = kept["baseURL"].as_str().unwrap_or("");
    let raw: Vec<Value> = kept["models"].as_array().cloned().unwrap_or_default();
    match provider {
        super::modal::ID => super::modal::derive(base, &super::modal::parse(&raw), templates),
        _ => vec![],
    }
}

/// Fetch and keep the live list of a provider that has one, for the
/// resolved base URL and key: the model ids, or `None` when the provider
/// has no live list.
pub async fn refresh(
    client: &reqwest::Client,
    provider: &str,
    base_url: &str,
    key: Option<&str>,
) -> Option<Result<Vec<String>>> {
    match provider {
        super::modal::ID => Some(
            async {
                let key = key.ok_or_else(|| {
                    anyhow::anyhow!("Modal needs its proxy token: set MODAL_PROXY_TOKEN or run `codesmith auth login modal`")
                })?;
                let raw = super::modal::load(client, base_url, key).await?;
                let ids = super::modal::parse(&raw).into_iter().map(|m| m.id).collect();
                save(provider, &fingerprint(key), base_url, raw)?;
                Ok(ids)
            }
            .await,
        ),
        _ => None,
    }
}
