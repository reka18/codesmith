//! GitHub Copilot's live model list (after OpenCode's
//! `github-copilot/models`): the account's `<api>/models`, decoded
//! defensively (a malformed or unusable entry is skipped, a disabled one
//! dropped), mapped onto the catalog's Copilot models, which keep their
//! names and aliases only while Copilot still advertises their model. Each
//! model gets the API Copilot says serves it, its limits, its prices (AI
//! credits per billing batch, as dollars per million tokens) and its
//! reasoning variants. Copilot's Auto model is built from the models in its
//! Auto pool.
//!
//! `codesmith models github-copilot` fetches and keeps the list (see
//! [`super::live`]).

use crate::catalog::{self, Model};
use serde_json::{json, Value};

pub const AUTO: &str = "auto";

/// One usable model of the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    pub id: String,
    pub name: String,
    pub version: String,
    pub picker: bool,
    pub endpoints: Option<Vec<String>>,
    pub family: String,
    pub context: Option<u64>,
    pub output: u64,
    pub prompt: u64,
    pub media: Vec<String>,
    pub vision: bool,
    pub tools: bool,
    pub efforts: Vec<String>,
    pub adaptive: bool,
    pub max_thinking: Option<u64>,
    pub auto_discount: Option<f64>,
    /// `(batch size, input, output, cache read)`.
    pub prices: Option<(f64, f64, f64, f64)>,
}

fn num(v: &Value) -> Result<Option<f64>, ()> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(n.as_f64()),
        _ => Err(()),
    }
}

fn boolean(v: &Value) -> Result<Option<bool>, ()> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        _ => Err(()),
    }
}

fn strings(v: &Value) -> Result<Option<Vec<String>>, ()> {
    match v {
        Value::Null => Ok(None),
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(str::to_string).ok_or(()))
            .collect::<Result<_, _>>()
            .map(Some),
        _ => Err(()),
    }
}

fn string(v: &Value) -> Result<String, ()> {
    v.as_str().map(str::to_string).ok_or(())
}

/// Decode one entry; `None` when it is malformed, disabled by policy, or
/// lacks the limits and tool support a session needs.
pub fn decode(v: &Value) -> Option<Remote> {
    (|| -> Result<Option<Remote>, ()> {
        let caps = &v["capabilities"];
        let limits = &caps["limits"];
        let supports = &caps["supports"];
        if !supports.is_object() {
            return Err(());
        }
        let vision_limits = &limits["vision"];
        let media = match vision_limits {
            Value::Null => vec![],
            o => {
                num(&o["max_prompt_image_size"])?.ok_or(())?;
                num(&o["max_prompt_images"])?.ok_or(())?;
                strings(&o["supported_media_types"])?.ok_or(())?
            }
        };
        let prices = match &v["billing"]["token_prices"] {
            Value::Null => None,
            p => {
                let d = &p["default"];
                Some((
                    num(&p["batch_size"])?.ok_or(())?,
                    num(&d["input_price"])?.ok_or(())?,
                    num(&d["output_price"])?.ok_or(())?,
                    // API version 2026-08-01 renamed cache_price.
                    num(&d["cache_read_price"])?
                        .or(num(&d["cache_price"])?)
                        .unwrap_or(0.0),
                ))
            }
        };
        let r = Remote {
            picker: boolean(&v["model_picker_enabled"])?.ok_or(())?,
            id: string(&v["id"])?,
            name: string(&v["name"])?,
            version: string(&v["version"])?,
            endpoints: strings(&v["supported_endpoints"])?,
            family: string(&caps["family"])?,
            context: num(&limits["max_context_window_tokens"])?.map(|n| n as u64),
            output: match num(&limits["max_output_tokens"])? {
                Some(n) => n as u64,
                None => return Ok(None),
            },
            prompt: match num(&limits["max_prompt_tokens"])? {
                Some(n) => n as u64,
                None => return Ok(None),
            },
            media,
            vision: boolean(&supports["vision"])?.unwrap_or(false),
            tools: match boolean(&supports["tool_calls"])? {
                Some(t) => t,
                None => return Ok(None),
            },
            efforts: strings(&supports["reasoning_effort"])?.unwrap_or_default(),
            adaptive: boolean(&supports["adaptive_thinking"])?.unwrap_or(false),
            max_thinking: num(&supports["max_thinking_budget"])?.map(|n| n as u64),
            auto_discount: num(&v["billing"]["auto_discount"])?,
            prices,
        };
        if v["policy"]["state"] == "disabled" {
            return Ok(None);
        }
        Ok(Some(r))
    })()
    .ok()
    .flatten()
}

pub fn parse(raw: &[Value]) -> Vec<Remote> {
    let mut out: Vec<Remote> = vec![];
    for r in raw.iter().filter_map(decode) {
        out.retain(|o| o.id != r.id);
        out.push(r);
    }
    out
}

/// The API serving a model: advertised, else Copilot's defaults.
fn endpoint(r: &Remote) -> &'static str {
    let has = |e: &str| {
        r.endpoints
            .as_ref()
            .is_some_and(|l| l.iter().any(|x| x == e))
    };
    if has("/v1/messages") {
        "messages"
    } else if has("/responses") {
        "responses"
    } else if has("/chat/completions") {
        "chat"
    } else {
        fallback(&r.id)
    }
}

fn fallback(id: &str) -> &'static str {
    match super::copilot::fallback_protocol(id) {
        codesmith_ai::Protocol::OpenaiResponses => "responses",
        _ => "chat",
    }
}

/// The package and base URL of a Copilot API.
pub fn route_for(endpoint: &str, root: &str) -> (&'static str, String) {
    match endpoint {
        "messages" => ("@opencode/ai/providers/anthropic", format!("{root}/v1")),
        "responses" => (
            "@opencode/ai/providers/openai-compatible-responses",
            root.to_string(),
        ),
        _ => ("@opencode/ai/providers/openai-compatible", root.to_string()),
    }
}

/// `2026-06-01` (after an `<id>-` prefix) in milliseconds since the epoch.
fn released(id: &str, version: &str) -> i64 {
    let v = version.strip_prefix(&format!("{id}-")).unwrap_or(version);
    let parts: Vec<i64> = v.split('-').filter_map(|p| p.parse().ok()).collect();
    let [y, m, d] = parts[..] else { return 0 };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || v.split('-').count() != 3 {
        return 0;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400_000
}

fn variant(id: &str, settings: Value) -> catalog::Variant {
    catalog::Variant {
        id: id.to_string(),
        settings: settings.as_object().cloned(),
        body: None,
        headers: None,
    }
}

fn variants(r: &Remote, messages: bool) -> Vec<catalog::Variant> {
    if !messages && !r.efforts.is_empty() {
        return r
            .efforts
            .iter()
            .map(|e| variant(e, json!({"reasoningEffort": e, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]})))
            .collect();
    }
    if !r.efforts.is_empty() && r.adaptive {
        return r
            .efforts
            .iter()
            .map(|e| {
                variant(
                    e,
                    json!({"thinking": {"type": "adaptive", "display": "summarized"}, "effort": e}),
                )
            })
            .collect();
    }
    match r.max_thinking {
        None => vec![],
        Some(max) => vec![
            variant(
                "max",
                json!({"thinking": {"type": "enabled", "budgetTokens": max.saturating_sub(1)}}),
            ),
            variant(
                "high",
                json!({"thinking": {"type": "enabled", "budgetTokens": max / 2}}),
            ),
        ],
    }
}

fn blank(id: &str) -> Model {
    super::live::blank(super::copilot::ID, id)
}

fn build(id: &str, r: &Remote, root: &str, previous: Option<&Model>) -> Model {
    let ep = endpoint(r);
    let (package, base_url) = route_for(ep, root);
    let image = r.vision || r.media.iter().any(|m| m.starts_with("image/"));
    let pdf = r.vision && r.media.iter().any(|m| m == "application/pdf");
    let mut input = vec!["text".to_string()];
    if image {
        input.push("image".into());
    }
    if pdf {
        input.push("pdf".into());
    }
    let (per_million, i, o, c) = match r.prices {
        Some((batch, i, o, c)) if batch > 0.0 => (10_000.0 / batch, i, o, c),
        Some((_, i, o, c)) => (0.0, i, o, c),
        None => (0.0, 0.0, 0.0, 0.0),
    };
    let released = previous
        .map(|p| p.time.released)
        .filter(|t| *t != 0)
        .unwrap_or_else(|| released(&r.id, &r.version));
    let mut settings = previous
        .and_then(|p| p.settings.clone())
        .unwrap_or_default();
    settings.base_url = Some(base_url);
    let mut m = blank(id);
    m.model_id = r.id.clone();
    m.family = previous
        .and_then(|p| p.family.clone())
        .or(Some(r.family.clone()));
    m.name = previous.map(|p| p.name.clone()).unwrap_or(r.name.clone());
    m.package = Some(package.into());
    m.settings = Some(settings);
    m.headers = previous.and_then(|p| p.headers.clone());
    m.body = previous.and_then(|p| p.body.clone());
    m.capabilities.tools = r.tools;
    m.capabilities.input = input;
    m.variants = variants(r, ep == "messages");
    m.time.released = released;
    m.cost = vec![catalog::Cost {
        tier: None,
        input: i * per_million,
        output: o * per_million,
        cache: catalog::CacheCost {
            read: c * per_million,
            write: 0.0,
        },
    }];
    m.enabled = r.picker;
    m.limit = catalog::Limit {
        context: r.context.unwrap_or(r.prompt),
        input: Some(r.prompt),
        output: r.output,
    };
    m
}

/// The catalog's models that Copilot still advertises, then the rest of
/// the list, each built from Copilot's facts over its template (never over
/// a previous result).
pub fn derive(root: &str, remote: &[Remote], templates: &[Model]) -> Vec<Model> {
    let mut out: Vec<Model> = vec![];
    for t in templates {
        if let Some(r) = remote.iter().find(|r| r.id == t.model_id) {
            out.push(build(&t.id, r, root, Some(t)));
        }
    }
    for r in remote {
        if !out.iter().any(|m| m.id == r.id) {
            out.push(build(&r.id, r, root, None));
        }
    }
    if let Some(a) = auto_model(root, remote) {
        if !out.iter().any(|m| m.id == AUTO) {
            out.push(a);
        }
    }
    out
}

/// Copilot's Auto model, when the account has an Auto pool: Copilot's
/// router picks the model that serves each session; its limits are the
/// pool's smallest.
pub fn auto_model(root: &str, remote: &[Remote]) -> Option<Model> {
    let pool: Vec<&Remote> = remote
        .iter()
        .filter(|r| r.auto_discount.is_some())
        .collect();
    if pool.is_empty() {
        return None;
    }
    let (package, base_url) = route_for("chat", root);
    let mut m = blank(AUTO);
    m.name = "Auto".into();
    m.package = Some(package.into());
    m.settings = Some(catalog::Settings {
        base_url: Some(base_url),
        provider: None,
    });
    m.capabilities.input = vec!["text".into(), "image".into()];
    m.limit = catalog::Limit {
        context: pool
            .iter()
            .map(|r| r.context.unwrap_or(r.prompt))
            .min()
            .unwrap_or(0),
        input: pool.iter().map(|r| r.prompt).min(),
        output: pool.iter().map(|r| r.output).min().unwrap_or(0),
    };
    Some(m)
}
