//! The model list Claude Code reports, kept between runs so models can be
//! listed and priced without starting `claude` (OpenCode's `provider.ts`),
//! and the login status it comes with (OpenCode's `status.ts`).

use super::probe::{self, Probe};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub efforts: Vec<String>,
    #[serde(
        rename = "contextWindow",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub context_window: Option<u64>,
    #[serde(
        rename = "resolvedModel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resolved_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    /// The catalog provider whose prices apply, when the login pays per
    /// token.
    #[serde(
        rename = "priceSource",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub price_source: Option<String>,
    pub models: Vec<Model>,
}

impl Catalog {
    pub fn file(storage: &Path) -> PathBuf {
        storage.join("models.json")
    }

    /// The saved list; a missing or malformed one is empty.
    pub fn load(storage: &Path) -> Catalog {
        std::fs::read_to_string(Self::file(storage))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, storage: &Path) -> std::io::Result<()> {
        let text = serde_json::to_vec_pretty(self).unwrap_or_default();
        super::store::write_private(&Self::file(storage), &text)
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The list a check reported. `default` follows Claude Code's own
    /// default, which can change under a running session, so it is left
    /// out. A chat's start reports no context windows, so saved ones stay.
    pub fn from_probe(probe: &Probe, saved: &Catalog, base_url: Option<&str>) -> Catalog {
        let models = probe
            .models
            .iter()
            .filter(|m| m["value"] != "default")
            .filter_map(|m| {
                let id = m["value"].as_str()?.to_string();
                Some(Model {
                    name: m["displayName"].as_str().unwrap_or(&id).to_string(),
                    efforts: m["supportedEffortLevels"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                    context_window: m["contextWindow"]
                        .as_u64()
                        .or_else(|| saved.model(&id).and_then(|s| s.context_window)),
                    resolved_model: m["resolvedModel"].as_str().map(str::to_string),
                    id,
                })
            })
            .collect();
        Catalog {
            price_source: price_source(&probe.account, base_url),
            models,
        }
    }
}

/// Whose prices a login pays: none for a subscription, a custom base URL
/// or an enterprise gateway (they bill in their own ways); Anthropic's for
/// an API key, Claude on AWS or Google Cloud and Bedrock's Mantle endpoint;
/// the cloud's own on Bedrock, Vertex and Foundry.
pub fn price_source(account: &Value, base_url: Option<&str>) -> Option<String> {
    if base_url.is_some_and(|u| !u.is_empty()) {
        return None;
    }
    let provider = account["apiProvider"].as_str()?;
    if provider == "firstParty" && account["apiKeySource"].is_null() {
        return None;
    }
    let id = match provider {
        "firstParty" | "anthropicAws" | "anthropicGoogleCloud" | "mantle" => "anthropic",
        "bedrock" => "amazon-bedrock",
        "vertex" => "google-vertex",
        "foundry" => "azure",
        _ => return None,
    };
    Some(id.to_string())
}

/// The catalog ids a model's price may be listed under: the resolved
/// model without the 1M-context suffix (its tiers are on the base model),
/// without Mantle's `anthropic.` prefix, and as Vertex's `…@default`.
pub fn price_ids(model: &Model) -> Vec<String> {
    let id = model
        .resolved_model
        .as_deref()
        .unwrap_or(&model.id)
        .trim_end_matches("[1m]")
        .to_string();
    let mut ids = vec![id.clone()];
    if let Some(bare) = id.strip_prefix("anthropic.") {
        ids.push(bare.to_string());
    }
    ids.push(format!("{id}@default"));
    ids
}

/// Where Claude Code stands.
#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Checking,
    /// `claude` is not installed.
    Missing,
    /// It would not start.
    Failed(String),
    /// It started and reported this (from a chat's own start, or a check).
    Ready {
        probe: Probe,
        from_chat: bool,
    },
}

impl State {
    /// What the user needs to do, if anything.
    pub fn problem(&self) -> Option<String> {
        match self {
            State::Missing => Some(format!(
                "Claude Code is not installed. Install it, then run `claude auth login`. ({SETUP_URL})"
            )),
            State::Failed(m) => Some(m.clone()),
            State::Ready { probe, .. } if !probe::is_logged_in(&probe.account) => {
                Some(super::login_hint(
                    "Claude Code is not logged in. Run `claude auth login` in a terminal.",
                    super::saved_token().is_some(),
                    super::keychain_locked,
                ))
            }
            _ => None,
        }
    }
}

pub const SETUP_URL: &str = "https://code.claude.com/docs/en/setup";

/// The base URL Claude Code will use: the configured one, else the
/// user's own.
pub fn base_url(
    connection: &BTreeMap<String, String>,
    env: &BTreeMap<String, String>,
) -> Option<String> {
    connection
        .get("ANTHROPIC_BASE_URL")
        .or_else(|| env.get("ANTHROPIC_BASE_URL"))
        .filter(|u| !u.is_empty())
        .cloned()
}
