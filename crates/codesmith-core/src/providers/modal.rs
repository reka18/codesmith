//! Modal (after OpenCode's `ModalPlugin` and `modal/models`): an
//! OpenAI-compatible proxy whose live model list (`<baseURL>/models`, with
//! `MODAL_PROXY_TOKEN` or a saved key as the bearer token) is mapped onto
//! the catalog's Modal models: each live model takes the catalog model of
//! its base model (`base_model_id`, else `hugging_face_id`, else its id) as
//! a template, and the proxy's name, modalities, limits, prices and
//! reasoning options win over the template's.
//!
//! `codesmith models modal` fetches and keeps the list (see
//! [`super::live`]); until then the catalog's models are used.

use crate::catalog::{self, Model};
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::time::Duration;

pub const ID: &str = "modal";
pub const PACKAGE: &str = "@opencode/ai/providers/openai-compatible";

/// One model of the proxy's list.
#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    pub id: String,
    pub base_model_id: Option<String>,
    pub hugging_face_id: Option<String>,
    pub name: Option<String>,
    pub input_modalities: Option<Vec<String>>,
    pub output_modalities: Option<Vec<String>>,
    pub context_length: Option<f64>,
    pub max_output_length: Option<f64>,
    pub prompt: Option<Value>,
    pub completion: Option<Value>,
    pub input_cache_read: Option<Value>,
    pub supported_features: Option<Vec<String>>,
    pub reasoning_options: Option<Vec<Vec<Option<String>>>>,
    pub interleaved: Option<Value>,
}

fn opt_str(v: &Value, k: &str) -> Result<Option<String>, ()> {
    match v.get(k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(()),
    }
}

fn opt_strs(v: &Value, k: &str) -> Result<Option<Vec<String>>, ()> {
    match v.get(k) {
        None => Ok(None),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(str::to_string).ok_or(()))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => Err(()),
    }
}

fn opt_num(v: &Value, k: &str) -> Result<Option<f64>, ()> {
    match v.get(k) {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        _ => Err(()),
    }
}

/// A price as a string or number.
fn opt_price(v: &Value, k: &str) -> Result<Option<Value>, ()> {
    match v.get(k) {
        None => Ok(None),
        Some(p @ (Value::String(_) | Value::Number(_))) => Ok(Some(p.clone())),
        _ => Err(()),
    }
}

/// Decode one model; `None` for a malformed entry (which is skipped, so one
/// bad entry cannot discard the list).
pub fn decode(v: &Value) -> Option<Remote> {
    let r = (|| -> Result<Remote, ()> {
        let id = v.get("id").and_then(Value::as_str).ok_or(())?.to_string();
        let pricing = match v.get("pricing") {
            None => Value::Object(Default::default()),
            Some(p @ Value::Object(_)) => p.clone(),
            _ => return Err(()),
        };
        let reasoning_options = match v.get("reasoning_options") {
            None => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|o| {
                        if o["type"] != "effort" {
                            return Err(());
                        }
                        o["values"]
                            .as_array()
                            .ok_or(())?
                            .iter()
                            .map(|x| match x {
                                Value::Null => Ok(None),
                                Value::String(s) => Ok(Some(s.clone())),
                                _ => Err(()),
                            })
                            .collect()
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            _ => return Err(()),
        };
        let interleaved = match v.get("interleaved") {
            None => None,
            Some(b @ Value::Bool(_)) => Some(b.clone()),
            Some(o @ Value::Object(_))
                if matches!(
                    o["field"].as_str(),
                    Some("reasoning" | "reasoning_content" | "reasoning_details")
                ) =>
            {
                Some(o.clone())
            }
            _ => return Err(()),
        };
        Ok(Remote {
            id,
            base_model_id: opt_str(v, "base_model_id")?,
            hugging_face_id: opt_str(v, "hugging_face_id")?,
            name: opt_str(v, "name")?,
            input_modalities: opt_strs(v, "input_modalities")?,
            output_modalities: opt_strs(v, "output_modalities")?,
            context_length: opt_num(v, "context_length")?,
            max_output_length: opt_num(v, "max_output_length")?,
            prompt: opt_price(&pricing, "prompt")?,
            completion: opt_price(&pricing, "completion")?,
            input_cache_read: opt_price(&pricing, "input_cache_read")?,
            supported_features: opt_strs(v, "supported_features")?,
            reasoning_options,
            interleaved,
        })
    })();
    r.ok()
}

pub fn parse(raw: &[Value]) -> Vec<Remote> {
    raw.iter().filter_map(decode).collect()
}

/// Fetch the proxy's list (raw entries; a malformed envelope fails).
pub async fn load(client: &reqwest::Client, base_url: &str, key: &str) -> Result<Vec<Value>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(key)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map_err(|e| anyhow!("listing Modal models: {}", codesmith_ai::Error::from(e)))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "listing Modal models: HTTP {}",
            resp.status().as_u16()
        ));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|_| anyhow!("listing Modal models: the answer is not JSON"))?;
    let mut data = v["data"]
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("listing Modal models: the answer has no data list"))?;
    data.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(data)
}

/// Dollars per token as dollars per million tokens.
fn price(v: &Option<Value>, fallback: f64) -> f64 {
    let n = match v {
        None => return fallback,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    n.map(|n| n * 1_000_000.0)
        .filter(|p| p.is_finite())
        .unwrap_or(fallback)
}

fn limit(v: Option<f64>, fallback: u64) -> u64 {
    match v {
        Some(n) if n.is_finite() && n.trunc() >= 0.0 => n.trunc() as u64,
        Some(_) => fallback,
        None => fallback,
    }
}

/// The live models built on their catalog templates (never on a previous
/// result), in the proxy's order.
pub fn derive(base_url: &str, remote: &[Remote], templates: &[Model]) -> Vec<Model> {
    let mut out: Vec<Model> = Vec::new();
    for r in remote {
        let key = r
            .base_model_id
            .as_deref()
            .or(r.hugging_face_id.as_deref())
            .unwrap_or(&r.id);
        let t = templates.iter().find(|m| m.id == key);
        let m = build(r, base_url, t);
        out.retain(|o| o.id != m.id);
        out.push(m);
    }
    out
}

fn build(r: &Remote, base_url: &str, t: Option<&Model>) -> Model {
    let cost = t.and_then(|t| t.cost.first().copied()).unwrap_or_default();
    let reasoning_field = match &r.interleaved {
        Some(Value::Object(o)) => o.get("field").and_then(Value::as_str).map(str::to_string),
        Some(Value::Bool(true)) => Some("reasoning_content".to_string()),
        _ => None,
    };
    let compatibility = match reasoning_field {
        Some(f) => Some(catalog::Compatibility {
            reasoning_field: Some(f),
            ..t.and_then(|t| t.compatibility.clone()).unwrap_or_default()
        }),
        None => t.and_then(|t| t.compatibility.clone()),
    };
    let variants = match &r.reasoning_options {
        None => t.map(|t| t.variants.clone()).unwrap_or_default(),
        Some(opts) => {
            let mut seen: Vec<catalog::Variant> = vec![];
            for e in opts.iter().flatten() {
                let effort = e.clone().unwrap_or_else(|| "none".into());
                if !seen.iter().any(|v| v.id == effort) {
                    let mut settings = serde_json::Map::new();
                    settings.insert("reasoningEffort".into(), effort.clone().into());
                    seen.push(catalog::Variant {
                        id: effort,
                        settings: Some(settings),
                        body: None,
                        headers: None,
                    });
                }
            }
            seen
        }
    };
    let mut settings = t.and_then(|t| t.settings.clone()).unwrap_or_default();
    settings.base_url = Some(base_url.to_string());
    settings.provider = Some(ID.to_string());
    Model {
        id: r.id.clone(),
        model_id: r.id.clone(),
        provider_id: ID.to_string(),
        kind: "chat".into(),
        name: r
            .name
            .clone()
            .or_else(|| t.map(|t| t.name.clone()))
            .unwrap_or_else(|| r.id.clone()),
        compatibility,
        family: t.and_then(|t| t.family.clone()),
        package: Some(PACKAGE.into()),
        settings: Some(settings),
        capabilities: catalog::Capabilities {
            tools: r
                .supported_features
                .as_ref()
                .map(|f| f.iter().any(|x| x == "tools"))
                .or(t.map(|t| t.capabilities.tools))
                .unwrap_or(true),
            freeform: t.and_then(|t| t.capabilities.freeform.clone()),
            input: r
                .input_modalities
                .clone()
                .or_else(|| t.map(|t| t.capabilities.input.clone()))
                .unwrap_or_else(|| vec!["text".into()]),
            output: r
                .output_modalities
                .clone()
                .or_else(|| t.map(|t| t.capabilities.output.clone()))
                .unwrap_or_else(|| vec!["text".into()]),
            temperature: t.and_then(|t| t.capabilities.temperature),
        },
        variants,
        time: t.map(|t| t.time).unwrap_or_default(),
        cost: vec![catalog::Cost {
            tier: None,
            input: price(&r.prompt, cost.input),
            output: price(&r.completion, cost.output),
            cache: catalog::CacheCost {
                read: price(&r.input_cache_read, cost.cache.read),
                write: cost.cache.write,
            },
        }],
        status: t
            .map(|t| t.status.clone())
            .unwrap_or_else(|| "active".into()),
        enabled: t.map(|t| t.enabled).unwrap_or(true),
        limit: catalog::Limit {
            context: limit(r.context_length, t.map(|t| t.limit.context).unwrap_or(0)),
            input: t.and_then(|t| t.limit.input),
            output: limit(r.max_output_length, t.map(|t| t.limit.output).unwrap_or(0)),
        },
        headers: t.and_then(|t| t.headers.clone()),
        body: t.and_then(|t| t.body.clone()),
        reasoning: t.map(|t| t.reasoning.clone()).unwrap_or_default(),
    }
}
