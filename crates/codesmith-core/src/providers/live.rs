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
        super::copilot::ID => {
            super::copilot_models::derive(base, &super::copilot_models::parse(&raw), templates)
        }
        super::azure::ID => super::azure::derive(&raw, templates),
        super::digitalocean::ID => super::digitalocean::derive(&raw, templates),
        _ => vec![],
    }
}

/// Fetch and keep the live list of a provider that has one, with the
/// resolved provider's endpoint and credentials: the model ids, or `None`
/// when the provider has no live list.
pub async fn refresh(
    client: &reqwest::Client,
    provider_id: &str,
    p: &codesmith_ai::Provider,
    settings: &crate::config::ProviderSettings,
) -> Option<Result<Vec<String>>> {
    match provider_id {
        super::digitalocean::ID => Some(
            async {
                let login = crate::auth::entry(super::digitalocean::ID).ok_or_else(|| {
                    anyhow::anyhow!("DigitalOcean's routers need a browser login: `codesmith auth login digitalocean --oauth`")
                })?;
                let e = super::digitalocean::Endpoints::digitalocean("");
                let names = super::digitalocean::refresh_routers(client, &e, &login, &file()).await?;
                Ok(names.into_iter().map(|n| format!("router:{n}")).collect())
            }
            .await,
        ),
        super::azure::ID => Some(
            refresh_azure(
                client,
                p,
                settings,
                &super::azure::Endpoints::default(),
                &file(),
                crate::auth::entry(super::azure::ID).as_ref(),
                &super::process_env(),
            )
            .await,
        ),
        super::modal::ID => Some(
            async {
                let key = p.api_key.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("Modal needs its proxy token: set MODAL_PROXY_TOKEN or run `codesmith auth login modal`")
                })?;
                let raw = super::modal::load(client, &p.base_url, key).await?;
                let ids = super::modal::parse(&raw).into_iter().map(|m| m.id).collect();
                save(provider_id, &fingerprint(key), &p.base_url, raw)?;
                Ok(ids)
            }
            .await,
        ),
        super::copilot::ID => Some(
            async {
                let (root, account, raw) = fetch_copilot(client, p).await?;
                let models = super::copilot_models::parse(&raw);
                let mut ids: Vec<String> = models.iter().filter(|m| m.picker).map(|m| m.id.clone()).collect();
                if super::copilot_models::auto_model(&root, &models).is_some() {
                    ids.push(super::copilot_models::AUTO.into());
                }
                save(provider_id, &account, &root, raw)?;
                Ok(ids)
            }
            .await,
        ),
        _ => None,
    }
}

/// Copilot's `/models` for the account: the API root it answered from,
/// the account's fingerprint, and the raw list.
async fn fetch_copilot(
    client: &reqwest::Client,
    p: &codesmith_ai::Provider,
) -> Result<(String, String, Vec<Value>)> {
    let root = p
        .base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string();
    let mut req = client
        .get(format!("{root}/models"))
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    for (k, v) in &p.headers {
        if let (Ok(k), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            req.headers_mut().insert(k, v);
        }
    }
    let codesmith_ai::Auth::Dynamic(a) = &p.auth else {
        anyhow::bail!("GitHub Copilot needs a login: run `codesmith auth login github-copilot`");
    };
    a.authorize(client, &mut req)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let account = req
        .headers()
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .map(fingerprint)
        .unwrap_or_default();
    let url = req.url().to_string();
    let root = url.strip_suffix("/models").unwrap_or(&root).to_string();
    let resp = client
        .execute(req)
        .await
        .map_err(|e| anyhow::anyhow!("listing Copilot models: {}", codesmith_ai::Error::from(e)))?;
    if !resp.status().is_success() {
        anyhow::bail!("listing Copilot models: HTTP {}", resp.status().as_u16());
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("listing Copilot models: the answer is not JSON"))?;
    let raw = v["data"]
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("listing Copilot models: the answer has no data list"))?;
    Ok((root, account, raw))
}

/// List and keep an Azure resource's deployments (see
/// [`super::azure::deployments`]); a custom endpoint keeps the catalog.
pub async fn refresh_azure(
    client: &reqwest::Client,
    p: &codesmith_ai::Provider,
    settings: &crate::config::ProviderSettings,
    e: &super::azure::Endpoints,
    path: &Path,
    stored: Option<&Value>,
    env: &super::Env,
) -> Result<Vec<String>> {
    use super::azure;
    if settings.base_url.is_some() {
        anyhow::bail!("a custom Azure endpoint keeps the catalog's models: configure them under providers.azure.models");
    }
    let resource = azure::resource_name(settings, stored, env).ok_or_else(|| {
        anyhow::anyhow!("Azure needs the resource name: set settings.resourceName or AZURE_RESOURCE_NAME, or log in with it")
    })?;
    let (list, account) = if azure::cli_login(stored) {
        let program =
            azure::find_cli(env).ok_or_else(|| anyhow::anyhow!("the Azure CLI is not on PATH"))?;
        let cli = azure::cli(program);
        let l = azure::deployments(client, e, &resource, &azure::Credential::Cli(&cli)).await?;
        (l, format!("azure-cli:{resource}"))
    } else {
        let key = p
            .api_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Azure needs a key or an Azure CLI login"))?;
        let l = azure::deployments(client, e, &resource, &azure::Credential::Key(key)).await?;
        (l, format!("{}:{resource}", fingerprint(key)))
    };
    let ids = list.iter().map(|(n, _)| n.to_ascii_lowercase()).collect();
    save_in(path, azure::ID, &account, "", azure::raw(&list))?;
    Ok(ids)
}

/// A model with OpenCode's defaults for one the catalog does not describe.
pub fn blank(provider_id: &str, id: &str) -> crate::catalog::Model {
    use crate::catalog;
    catalog::Model {
        id: id.to_string(),
        model_id: id.to_string(),
        provider_id: provider_id.to_string(),
        kind: "chat".into(),
        name: id.to_string(),
        compatibility: None,
        family: None,
        package: None,
        settings: None,
        capabilities: catalog::Capabilities {
            tools: true,
            input: vec!["text".into()],
            output: vec!["text".into()],
            ..Default::default()
        },
        variants: vec![],
        time: Default::default(),
        cost: vec![],
        status: "active".into(),
        enabled: true,
        limit: catalog::Limit {
            context: 200_000,
            input: None,
            output: 32_000,
        },
        headers: None,
        body: None,
        reasoning: Default::default(),
    }
}
