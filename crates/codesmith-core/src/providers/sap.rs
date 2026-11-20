//! SAP AI Core (after OpenCode's `SapAICorePlugin`): a deployment of the
//! generative AI hub, reached with the service key's OAuth client
//! credentials and the `AI-Resource-Group` header.
//!
//! The service key (the JSON SAP BTP gives for the AI Core service
//! instance) comes from `AICORE_SERVICE_KEY`, else `settings.serviceKey`.
//! With one, requests go to `settings.baseURL`, else `AICORE_DEPLOYMENT_URL`,
//! else `<AI_API_URL>/v2/inference/deployments/<deployment id>` (the id from
//! `settings.deploymentId` or `AICORE_DEPLOYMENT_ID`), with a token from the
//! key's `<url>/oauth/token` and the resource group of
//! `settings.resourceGroup` or `AICORE_RESOURCE_GROUP` (default `default`).
//! Without one, the deployment settings are not used and the endpoint takes
//! a plain key.
//!
//! OpenCode drives SAP's AI SDK; CodeSmith speaks Chat Completions to the
//! deployment, which suits deployments that serve that API (set the
//! protocol for others).

use super::{set, Input, Route};
use anyhow::{anyhow, Result};
use base64::Engine;
use codesmith_ai::{Auth, Error as AiError, Protocol};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ID: &str = "sap-ai-core";
pub const KEY_VAR: &str = "AICORE_SERVICE_KEY";

pub fn owns(input: &Input) -> bool {
    input.provider_id == ID
}

/// The service key: the variable, then the setting.
pub fn service_key(input: &Input) -> Option<String> {
    (input.env)(KEY_VAR).or_else(|| set(&input.settings.plugin.service_key).map(str::to_string))
}

/// What a service key holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceKey {
    pub client_id: String,
    pub client_secret: String,
    /// The OAuth server (`url`).
    pub auth_url: String,
    /// `serviceurls.AI_API_URL`.
    pub api_url: String,
}

pub fn parse_key(text: &str) -> Result<ServiceKey> {
    let v: Value = serde_json::from_str(text).map_err(|_| {
        anyhow!(
            "the SAP AI Core service key is not JSON; give the whole key of the service instance"
        )
    })?;
    let s = |path: &[&str]| -> Result<String> {
        let mut cur = &v;
        for p in path {
            cur = &cur[*p];
        }
        cur.as_str()
            .map(|s| s.trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("the SAP AI Core service key has no {}", path.join(".")))
    };
    Ok(ServiceKey {
        client_id: s(&["clientid"])?,
        client_secret: s(&["clientsecret"])?,
        auth_url: s(&["url"])?,
        api_url: s(&["serviceurls", "AI_API_URL"])?,
    })
}

/// Client-credentials tokens for a service key, cached until shortly
/// before they expire.
#[derive(Debug)]
pub struct ClientCredentials {
    key: ServiceKey,
    token: Mutex<Option<(String, std::time::Instant)>>,
    fetch: tokio::sync::Mutex<()>,
}

impl ClientCredentials {
    pub fn new(key: ServiceKey) -> ClientCredentials {
        ClientCredentials {
            key,
            token: Mutex::new(None),
            fetch: tokio::sync::Mutex::new(()),
        }
    }

    fn cached(&self) -> Option<String> {
        let t = self.token.lock().unwrap();
        t.as_ref()
            .filter(|(_, until)| *until > std::time::Instant::now())
            .map(|(t, _)| t.clone())
    }

    pub async fn token(&self, client: &reqwest::Client) -> Result<String> {
        if let Some(t) = self.cached() {
            return Ok(t);
        }
        let _one = self.fetch.lock().await;
        if let Some(t) = self.cached() {
            return Ok(t);
        }
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.key.client_id, self.key.client_secret));
        let url = format!("{}/oauth/token", self.key.auth_url);
        let v = match super::oauth::post_form(
            client,
            &url,
            &[("grant_type", "client_credentials")],
            &[("authorization", format!("Basic {basic}"))],
        )
        .await?
        {
            Ok(v) => v,
            Err((status, _)) => return Err(anyhow!("SAP AI Core token request failed ({status})")),
        };
        let token = v["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("SAP AI Core's token response has no access_token"))?
            .to_string();
        let life = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
        *self.token.lock().unwrap() = Some((
            token.clone(),
            std::time::Instant::now() + Duration::from_secs(life),
        ));
        Ok(token)
    }
}

impl codesmith_ai::Authorizer for ClientCredentials {
    fn authorize<'a>(
        &'a self,
        client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> BoxFuture<'a, Result<(), AiError>> {
        Box::pin(async move {
            let t = self
                .token(client)
                .await
                .map_err(|e| AiError::Invalid(e.to_string()))?;
            req.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
                    .map_err(|e| AiError::Invalid(e.to_string()))?,
            );
            Ok(())
        })
    }

    fn describe(&self) -> String {
        format!("SAP AI Core service key {}", self.key.client_id)
    }

    fn rejected<'a>(&'a self, _client: &'a reqwest::Client) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.token.lock().unwrap().take().is_some() })
    }
}

fn credentials(key: ServiceKey) -> Arc<ClientCredentials> {
    static SHARED: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, Arc<ClientCredentials>>>,
    > = std::sync::OnceLock::new();
    SHARED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(format!("{}\n{}", key.auth_url, key.client_id))
        .or_insert_with(|| Arc::new(ClientCredentials::new(key)))
        .clone()
}

pub fn route(input: &Input) -> Result<Route> {
    let protocol = input.configured_protocol.unwrap_or(Protocol::OpenaiChat);
    let configured = input
        .configured_url
        .map(str::to_string)
        .or_else(|| (input.env)("AICORE_DEPLOYMENT_URL"));
    let Some(text) = service_key(input) else {
        let base_url = configured.ok_or_else(|| {
            anyhow!("SAP AI Core needs its service key: set AICORE_SERVICE_KEY or settings.serviceKey (or settings.baseURL with a key)")
        })?;
        return Ok(Route {
            protocol,
            base_url,
            auth: Auth::Bearer,
            api_key: input.api_key.clone(),
            headers: vec![],
            model_id: None,
        });
    };
    let key = parse_key(&text)?;
    let s = &input.settings.plugin;
    let base_url = match configured {
        Some(u) => u,
        None => {
            let id = set(&s.deployment_id)
                .map(str::to_string)
                .or_else(|| (input.env)("AICORE_DEPLOYMENT_ID"))
                .ok_or_else(|| {
                    anyhow!("SAP AI Core needs the deployment: set settings.deploymentId or AICORE_DEPLOYMENT_ID (or AICORE_DEPLOYMENT_URL)")
                })?;
            format!(
                "{}/v2/inference/deployments/{}",
                key.api_url,
                super::encode(&id)
            )
        }
    };
    let group = set(&s.resource_group)
        .map(str::to_string)
        .or_else(|| (input.env)("AICORE_RESOURCE_GROUP"))
        .unwrap_or_else(|| "default".into());
    Ok(Route {
        protocol,
        base_url,
        auth: Auth::Dynamic(credentials(key)),
        api_key: None,
        headers: vec![("AI-Resource-Group".into(), group)],
        model_id: None,
    })
}
