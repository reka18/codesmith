//! Reasoning variants generated from a catalog model's reasoning controls,
//! spelled for the model's package. After OpenCode's `variant.ts`
//! (`Variant.supports` and `Variant.resolve`).

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

/// One generated variant: settings (provider options), body fields and
/// headers layered over the model's when `provider/model#id` is chosen.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Variant {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
}

impl Variant {
    /// The variant as CodeSmith's request overlay.
    pub fn to_overlay(&self) -> codesmith_ai::Variant {
        let mut v = Map::new();
        v.insert(
            "settings".into(),
            Value::Object(self.settings.clone().unwrap_or_default()),
        );
        if let Some(b) = &self.body {
            v.insert("body".into(), Value::Object(b.clone()));
        }
        if let Some(h) = &self.headers {
            v.insert("headers".into(), Value::Object(h.clone()));
        }
        codesmith_ai::variant_from_config(&Value::Object(v))
    }
}

/// A model's reasoning controls in models.dev v2's shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Reasoning {
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toggle: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Budget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Support {
    Effort(Option<Vec<String>>),
    Toggle,
    Budget(Budget),
}

/// Reasoning controls as supports, where `default` is the provider's
/// default rather than a variant.
pub fn supports(r: &Reasoning) -> Vec<Support> {
    if !r.supported {
        return vec![];
    }
    let mut out = Vec::new();
    if r.toggle == Some(true) {
        out.push(Support::Toggle);
    }
    if let Some(e) = &r.effort {
        out.push(Support::Effort(Some(
            e.iter().filter(|v| *v != "default").cloned().collect(),
        )));
    }
    if let Some(b) = r.budget {
        out.push(Support::Budget(b));
    }
    out
}

/// What a variant generator needs to know about the model.
pub struct Target<'a> {
    pub package: Option<&'a str>,
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub output: i64,
}

pub fn resolve(model: &Target, supports: &[Support]) -> Vec<Variant> {
    let Some(protocol) = model.package.and_then(protocol) else {
        return vec![];
    };
    let toggle = if supports.contains(&Support::Toggle) {
        protocol(model, &Support::Toggle)
    } else {
        vec![]
    };
    let effort = supports.iter().find(|s| matches!(s, Support::Effort(_)));
    let budget = supports.iter().find(|s| matches!(s, Support::Budget(_)));
    let main = match (effort, budget) {
        (Some(e), _) => protocol(model, e),
        (None, Some(b)) => protocol(model, b),
        _ => toggle.clone(),
    };
    let mut out: Vec<Variant> = toggle.into_iter().filter(|v| v.id == "none").collect();
    for v in main {
        if !out.iter().any(|o| o.id == v.id) {
            out.push(v);
        }
    }
    out
}

/// The default supports when a model's controls are unknown.
pub fn default_supports() -> Vec<Support> {
    vec![Support::Effort(None)]
}

type Protocol = fn(&Target, &Support) -> Vec<Variant>;

const EFFORTS: [&str; 3] = ["low", "medium", "high"];
const ANTHROPIC_OUTPUT_TOKEN_MAX: i64 = 32_000;
const ALIBABA_THINKING_BUDGET_MAX: i64 = 64_000;

fn map(v: Value) -> Option<Map<String, Value>> {
    match v {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// An overlay: `{"settings": …, "body": …, "headers": …}`.
fn variant(id: &str, overlay: Value) -> Variant {
    Variant {
        id: id.to_string(),
        settings: overlay.get("settings").cloned().and_then(map),
        body: overlay.get("body").cloned().and_then(map),
        headers: overlay.get("headers").cloned().and_then(map),
    }
}

fn settings(v: Value) -> Value {
    json!({ "settings": v })
}

fn body(v: Value) -> Value {
    json!({ "body": v })
}

fn efforts(values: &[String], spell: impl Fn(&str) -> Value) -> Vec<Variant> {
    values.iter().map(|e| variant(e, spell(e))).collect()
}

fn values(support: &Option<Vec<String>>, default: &[&str]) -> Vec<String> {
    support
        .clone()
        .unwrap_or_else(|| default.iter().map(|s| s.to_string()).collect())
}

fn toggle(off: Value, on: Value) -> Vec<Variant> {
    vec![variant("none", off), variant("thinking", on)]
}

fn budgets(model: &Target, b: &Budget, ceiling: i64, spell: impl Fn(i64) -> Value) -> Vec<Variant> {
    let maximum = b
        .max
        .unwrap_or(ceiling - 1)
        .min(model.output - 1)
        .min(ceiling - 1);
    if maximum <= 0 {
        return vec![];
    }
    let high = b.min.unwrap_or(0).max((maximum + 1) / 2).min(maximum);
    vec![variant("high", spell(high)), variant("max", spell(maximum))]
}

struct Claude {
    family: Option<String>,
    major: Option<u32>,
    minor: u32,
    manual: bool,
    always: bool,
}

fn claude_info(model: &Target) -> Claude {
    let id = model.model_id;
    static FAMILY_FIRST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:claude-)?(opus|sonnet|haiku|fable|mythos)-(\d+)(?:[.-](\d+))?").unwrap()
    });
    static VERSION_FIRST: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)claude-(\d+)(?:[.-](\d+))?-(opus|sonnet|haiku|fable|mythos)").unwrap()
    });
    let (family, major, minor) = if let Some(c) = FAMILY_FIRST.captures(id) {
        (
            Some(c[1].to_ascii_lowercase()),
            c[2].parse().ok(),
            c.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
        )
    } else if let Some(c) = VERSION_FIRST.captures(id) {
        (
            Some(c[3].to_ascii_lowercase()),
            c[1].parse().ok(),
            c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
        )
    } else {
        (None, None, 0)
    };
    let manual = matches!((major, minor), (Some(3), 7)) || (major == Some(4) && minor < 6);
    let always = matches!(family.as_deref(), Some("fable") | Some("mythos"))
        || id.to_ascii_lowercase().contains("mythos-preview");
    Claude {
        family,
        major,
        minor,
        manual,
        always,
    }
}

fn manual_thinking(model: &Target) -> Option<Value> {
    let tokens = 16_000.min(model.output - 1);
    (tokens >= 1024).then(|| json!({"type": "enabled", "budgetTokens": tokens}))
}

fn adaptive() -> Value {
    json!({"type": "adaptive", "display": "summarized"})
}

fn openai_chat(_: &Target, s: &Support) -> Vec<Variant> {
    let Support::Effort(v) = s else { return vec![] };
    efforts(&values(v, &EFFORTS), |e| {
        settings(json!({"reasoningEffort": e}))
    })
}

fn vertex_chat(m: &Target, s: &Support) -> Vec<Variant> {
    if *s != Support::Toggle {
        return openai_chat(m, s);
    }
    let key = if m.model_id.to_ascii_lowercase().contains("deepseek") {
        "thinking"
    } else {
        "enable_thinking"
    };
    toggle(
        body(json!({"chat_template_kwargs": {key: false}})),
        body(json!({"chat_template_kwargs": {key: true}})),
    )
}

fn responses_effort(e: &str) -> Value {
    settings(
        json!({"reasoningEffort": e, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}),
    )
}

fn openai_responses(_: &Target, s: &Support) -> Vec<Variant> {
    let Support::Effort(v) = s else { return vec![] };
    efforts(
        &values(v, &["none", "minimal", "low", "medium", "high", "xhigh"]),
        responses_effort,
    )
}

fn open_responses(_: &Target, s: &Support) -> Vec<Variant> {
    let Support::Effort(v) = s else { return vec![] };
    efforts(&values(v, &EFFORTS), responses_effort)
}

fn google_interactions(_: &Target, s: &Support) -> Vec<Variant> {
    let Support::Effort(v) = s else { return vec![] };
    efforts(&values(v, &EFFORTS), |e| {
        settings(json!({"thinkingLevel": e, "thinkingSummaries": "auto"}))
    })
}

fn cloudflare_ai_gateway(m: &Target, s: &Support) -> Vec<Variant> {
    if m.model_id.starts_with("openai/") {
        openai_responses(m, s)
    } else if m.model_id.starts_with("anthropic/") {
        anthropic_messages(m, s)
    } else {
        openai_chat(m, s)
    }
}

fn deepseek_chat(_: &Target, s: &Support) -> Vec<Variant> {
    match s {
        Support::Effort(v) => efforts(&values(v, &["low", "high", "max"]), |e| {
            if e == "none" {
                body(json!({"thinking": {"type": "disabled"}}))
            } else {
                json!({"settings": {"reasoningEffort": e}, "body": {"thinking": {"type": "enabled"}}})
            }
        }),
        Support::Toggle => toggle(
            body(json!({"thinking": {"type": "disabled"}})),
            body(json!({"thinking": {"type": "enabled"}})),
        ),
        Support::Budget(_) => vec![],
    }
}

fn template(kwargs: Value) -> Value {
    body(json!({ "chat_template_kwargs": kwargs }))
}

fn template_toggle(key: &str) -> Vec<Variant> {
    toggle(template(json!({key: false})), template(json!({key: true})))
}

fn nvidia_chat(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id.to_ascii_lowercase();
    if id.contains("deepseek-v4") {
        return match s {
            Support::Toggle => template_toggle("thinking"),
            Support::Effort(v) => efforts(&values(v, &["none", "high", "max"]), |e| {
                if e == "none" {
                    template(json!({"thinking": false}))
                } else {
                    template(json!({"thinking": true, "reasoning_effort": e}))
                }
            }),
            Support::Budget(_) => vec![],
        };
    }
    if id.contains("kimi-k2.6") || id.contains("kimi-k2-6") {
        return template_toggle("thinking");
    }
    if id.contains("kimi-k3") {
        return match s {
            Support::Toggle => template_toggle("thinking"),
            Support::Effort(v) => efforts(&values(v, &["low", "high", "max"]), |e| {
                settings(json!({"reasoningEffort": e}))
            }),
            Support::Budget(_) => vec![],
        };
    }
    if id.contains("minimax-m3") {
        return toggle(
            template(json!({"thinking_mode": "disabled"})),
            template(json!({"thinking_mode": "enabled"})),
        );
    }
    if id.contains("glm") {
        return toggle(
            template(json!({"enable_thinking": false})),
            template(json!({"enable_thinking": true, "clear_thinking": false})),
        );
    }
    let toggle_only = id.contains("qwen") || id.contains("gemma") || id.contains("nemotron");
    match s {
        Support::Effort(v) if toggle_only && v.is_none() => template_toggle("enable_thinking"),
        Support::Effort(_) => openai_chat(m, s),
        Support::Toggle => template_toggle("enable_thinking"),
        Support::Budget(b) => budgets(m, b, m.output, |t| {
            body(json!({"chat_template_kwargs": {"enable_thinking": true}, "reasoning_budget": t}))
        }),
    }
}

fn workers_template(thinking: bool) -> Value {
    body(json!({"chat_template_kwargs": {"enable_thinking": thinking, "thinking": thinking}}))
}

fn workers_ai_chat(_: &Target, s: &Support) -> Vec<Variant> {
    match s {
        Support::Effort(v) => efforts(&values(v, &EFFORTS), |e| {
            if e == "none" {
                workers_template(false)
            } else {
                settings(json!({"reasoningEffort": e}))
            }
        }),
        Support::Toggle => toggle(workers_template(false), workers_template(true)),
        Support::Budget(_) => vec![],
    }
}

fn baseten_chat(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id.to_ascii_lowercase();
    let tpl = |on: bool| body(json!({"chat_template_args": {"enable_thinking": on}}));
    match s {
        Support::Effort(v) => {
            let glm = id.contains("glm-5.2") || id.contains("glm-5-2") || id.contains("glm-5p2");
            efforts(&values(v, &EFFORTS), |e| {
                let mut out = json!({"settings": {"reasoningEffort": e}});
                if glm {
                    out["body"] = tpl(e != "none")["body"].clone();
                }
                if id.contains("deepseek-v4-pro-0813") && e != "none" {
                    out["body"] = json!({"thinking": {"type": "enabled"}});
                }
                out
            })
        }
        Support::Toggle => toggle(tpl(false), tpl(true)),
        Support::Budget(_) => vec![],
    }
}

fn deepinfra_chat(m: &Target, s: &Support) -> Vec<Variant> {
    if KIMI_CODE.is_match(m.model_id) {
        return vec![];
    }
    match s {
        Support::Effort(v) => efforts(&values(v, &EFFORTS), |e| {
            settings(json!({"reasoningEffort": e}))
        }),
        Support::Toggle => toggle(
            body(json!({"reasoning": {"enabled": false}})),
            body(json!({"reasoning": {"enabled": true}})),
        ),
        Support::Budget(_) => vec![],
    }
}

fn openai_compatible(m: &Target, s: &Support) -> Vec<Variant> {
    if m.provider_id == "nvidia" {
        nvidia_chat(m, s)
    } else {
        openai_chat(m, s)
    }
}

fn venice_chat(_: &Target, s: &Support) -> Vec<Variant> {
    match s {
        Support::Effort(v) => efforts(&values(v, &EFFORTS), |e| {
            settings(json!({"reasoningEffort": e}))
        }),
        Support::Toggle => toggle(
            settings(json!({"reasoning": {"enabled": false}})),
            settings(json!({"reasoning": {"enabled": true}})),
        ),
        Support::Budget(_) => vec![],
    }
}

fn moonshot_chat(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id.to_ascii_lowercase();
    let toggleable = ["k2.5", "k2-5", "k2.6", "k2-6"]
        .iter()
        .any(|k| id.contains(k));
    let fixed = ["k2-thinking", "k2.7-code", "k2-7-code"]
        .iter()
        .any(|k| id.contains(k));
    if fixed {
        return vec![];
    }
    if toggleable && matches!(s, Support::Toggle | Support::Effort(None)) {
        return toggle(
            settings(json!({"thinking": {"type": "disabled"}})),
            settings(json!({"thinking": {"type": "enabled"}})),
        );
    }
    match s {
        Support::Effort(v) if !toggleable => efforts(&values(v, &["low", "high", "max"]), |e| {
            settings(json!({"reasoningEffort": e}))
        }),
        _ => vec![],
    }
}

fn alibaba_chat(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id.to_ascii_lowercase();
    let fixed = ["-thinking", "qwq", "deepseek-r1", "minimax-m2"]
        .iter()
        .any(|k| id.contains(k));
    match s {
        Support::Effort(v) => {
            if fixed && v.is_none() {
                return vec![];
            }
            let hosted = id.contains("glm") || id.contains("deepseek");
            let d: &[&str] = if hosted {
                &["high", "max"]
            } else {
                &["low", "medium", "xhigh"]
            };
            efforts(&values(v, d), |e| {
                if e == "none" {
                    settings(json!({"enableThinking": false}))
                } else {
                    settings(json!({"enableThinking": true, "reasoningEffort": e}))
                }
            })
        }
        Support::Toggle => toggle(
            settings(json!({"enableThinking": false})),
            settings(json!({"enableThinking": true})),
        ),
        Support::Budget(b) => budgets(m, b, ALIBABA_THINKING_BUDGET_MAX, |t| {
            settings(json!({"enableThinking": true, "thinkingBudget": t}))
        }),
    }
}

fn zai_chat(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id.to_ascii_lowercase();
    static VERSION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"glm-?(\d+)(?:(?:[.-]|p)(\d+))?").unwrap());
    let (major, minor) = match VERSION.captures(&id) {
        Some(c) => (
            c[1].parse::<u32>().ok(),
            c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
        ),
        None => (None, 0),
    };
    let coding = m.package.is_some_and(|p| p.contains("zai-coding-plan"));
    let latest =
        coding || major.is_none() || major.is_some_and(|mj| mj > 5 || (mj == 5 && minor >= 3));
    let effort = major == Some(5) && minor == 2;
    if *s == Support::Toggle || (!latest && !effort && *s == Support::Effort(None)) {
        if latest {
            return vec![];
        }
        return toggle(
            settings(json!({"thinking": {"type": "disabled"}})),
            settings(json!({"thinking": {"type": "enabled", "clear_thinking": false}})),
        );
    }
    let Support::Effort(v) = s else { return vec![] };
    let d: &[&str] = if latest {
        &["low", "high", "max"]
    } else {
        &["high", "max"]
    };
    efforts(&values(v, d), |e| {
        if e == "none" || e == "minimal" {
            settings(json!({"thinking": {"type": "disabled"}}))
        } else {
            settings(
                json!({"thinking": {"type": "enabled", "clear_thinking": false}, "reasoningEffort": e}),
            )
        }
    })
}

fn anthropic_messages(m: &Target, s: &Support) -> Vec<Variant> {
    let info = claude_info(m);
    let opus45 = info.family.as_deref() == Some("opus") && info.major == Some(4) && info.minor == 5;
    match s {
        Support::Effort(v) => {
            if info.manual && !opus45 {
                return anthropic_messages(
                    m,
                    &Support::Budget(Budget {
                        min: Some(1024),
                        max: None,
                    }),
                );
            }
            let thinking = if opus45 {
                manual_thinking(m)
            } else {
                Some(adaptive())
            };
            let Some(thinking) = thinking else {
                return vec![];
            };
            let d: &[&str] = if info.major == Some(4) && info.minor == 6 {
                &["low", "medium", "high", "max"]
            } else {
                &["low", "medium", "high", "xhigh", "max"]
            };
            let vals: Vec<String> = values(v, d)
                .into_iter()
                .filter(|e| !info.always || e != "none")
                .collect();
            efforts(&vals, |e| {
                if e == "none" {
                    settings(json!({"thinking": {"type": "disabled"}}))
                } else {
                    settings(json!({"thinking": thinking.clone(), "effort": e}))
                }
            })
        }
        Support::Toggle => {
            if info.always {
                return vec![];
            }
            let thinking = if info.manual {
                manual_thinking(m)
            } else {
                Some(adaptive())
            };
            let Some(thinking) = thinking else {
                return vec![];
            };
            toggle(
                settings(json!({"thinking": {"type": "disabled"}})),
                settings(json!({ "thinking": thinking })),
            )
        }
        Support::Budget(b) => budgets(m, b, ANTHROPIC_OUTPUT_TOKEN_MAX, |t| {
            settings(json!({"thinking": {"type": "enabled", "budgetTokens": t}}))
        }),
    }
}

fn minimax_messages(m: &Target, s: &Support) -> Vec<Variant> {
    if MINIMAX_M2.is_match(m.model_id) {
        return vec![];
    }
    if !matches!(s, Support::Toggle | Support::Effort(None)) {
        return vec![];
    }
    toggle(
        settings(json!({"thinking": {"type": "disabled"}})),
        settings(json!({"thinking": {"type": "adaptive"}})),
    )
}

fn moonshot_messages(m: &Target, s: &Support) -> Vec<Variant> {
    let Support::Effort(v) = s else { return vec![] };
    if KIMI_K2.is_match(m.model_id) {
        return vec![];
    }
    efforts(&values(v, &["low", "high", "max"]), |e| {
        settings(json!({ "effort": e }))
    })
}

fn alibaba_messages(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id.to_ascii_lowercase();
    match s {
        Support::Effort(v) => {
            let hosted = id.contains("glm") || id.contains("deepseek");
            let d: &[&str] = if hosted {
                &["high", "max"]
            } else {
                &["low", "medium", "xhigh"]
            };
            efforts(&values(v, d), |e| {
                if e == "none" {
                    settings(json!({"thinking": {"type": "disabled"}}))
                } else {
                    settings(json!({"thinking": {"type": "enabled"}, "effort": e}))
                }
            })
        }
        Support::Toggle => toggle(
            settings(json!({"thinking": {"type": "disabled"}})),
            settings(json!({"thinking": {"type": "enabled"}})),
        ),
        Support::Budget(b) => budgets(m, b, ALIBABA_THINKING_BUDGET_MAX, |t| {
            settings(json!({"thinking": {"type": "enabled", "budgetTokens": t}}))
        }),
    }
}

fn zai_messages(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id.to_ascii_lowercase();
    let forced = ["glm-5.3", "glm-5-3", "glm-5p3"]
        .iter()
        .any(|k| id.contains(k));
    match s {
        Support::Effort(v) => {
            let d: &[&str] = if forced {
                &["low", "high", "max"]
            } else {
                &["high", "max"]
            };
            efforts(&values(v, d), |e| {
                let kind = if e == "none" || e == "minimal" {
                    "disabled"
                } else {
                    "enabled"
                };
                settings(json!({"thinking": {"type": kind}, "effort": e}))
            })
        }
        Support::Toggle if forced => vec![],
        Support::Toggle => toggle(
            settings(json!({"thinking": {"type": "disabled"}})),
            settings(json!({"thinking": {"type": "enabled"}})),
        ),
        Support::Budget(_) => vec![],
    }
}

fn gemini(m: &Target, s: &Support) -> Vec<Variant> {
    match s {
        Support::Effort(v) => efforts(&values(v, &EFFORTS), |e| {
            settings(json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": e}}))
        }),
        Support::Toggle => toggle(
            settings(json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}})),
            settings(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1}})),
        ),
        Support::Budget(b) => budgets(m, b, m.output, |t| {
            settings(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": t}}))
        }),
    }
}

fn openrouter(m: &Target, s: &Support) -> Vec<Variant> {
    match s {
        Support::Effort(v) => efforts(&values(v, &EFFORTS), |e| {
            settings(json!({"reasoning": {"effort": e}}))
        }),
        Support::Toggle => toggle(
            settings(json!({"reasoning": {"enabled": false}})),
            settings(json!({"reasoning": {"enabled": true}})),
        ),
        Support::Budget(b) => budgets(m, b, m.output, |t| {
            settings(json!({"reasoning": {"max_tokens": t}}))
        }),
    }
}

fn bedrock_converse(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id;
    let claude = id.contains("anthropic");
    let fields = |f: Value| body(json!({ "additionalModelRequestFields": f }));
    match s {
        Support::Effort(v) => efforts(&values(v, &EFFORTS), |e| {
            if claude {
                let mut f = json!({"output_config": {"effort": e}});
                if !claude_info(m).manual {
                    f["thinking"] = adaptive();
                }
                fields(f)
            } else if id.contains("openai.gpt-oss") {
                fields(json!({ "reasoning_effort": e }))
            } else if id.contains("openai.") || id.contains("xai.") {
                fields(json!({"reasoning": {"effort": e}}))
            } else {
                fields(json!({"reasoningConfig": {"type": "enabled", "maxReasoningEffort": e}}))
            }
        }),
        Support::Toggle if claude => toggle(
            fields(json!({"thinking": {"type": "disabled"}})),
            fields(json!({ "thinking": adaptive() })),
        ),
        Support::Toggle => toggle(
            fields(json!({"reasoningConfig": {"type": "disabled"}})),
            fields(json!({"reasoningConfig": {"type": "enabled"}})),
        ),
        Support::Budget(b) => budgets(
            m,
            b,
            if claude {
                ANTHROPIC_OUTPUT_TOKEN_MAX
            } else {
                m.output
            },
            |t| {
                if claude {
                    settings(json!({"thinking": {"type": "enabled", "budgetTokens": t}}))
                } else {
                    fields(json!({"reasoningConfig": {"type": "enabled", "budgetTokens": t}}))
                }
            },
        ),
    }
}

fn cohere(m: &Target, s: &Support) -> Vec<Variant> {
    match s {
        Support::Effort(_) => vec![],
        Support::Toggle => toggle(
            settings(json!({"thinking": {"type": "disabled"}})),
            settings(json!({"thinking": {"type": "enabled"}})),
        ),
        Support::Budget(b) => budgets(m, b, m.output, |t| {
            settings(json!({"thinking": {"type": "enabled", "tokenBudget": t}}))
        }),
    }
}

fn vercel_gateway(m: &Target, s: &Support) -> Vec<Variant> {
    if m.model_id.starts_with("openai/gpt-") {
        openai_responses(m, s)
    } else if m.model_id.starts_with("spacexai/grok-") {
        open_responses(m, s)
    } else {
        vercel_messages(m, s)
    }
}

fn vercel_responses(m: &Target, s: &Support) -> Vec<Variant> {
    if m.model_id.starts_with("openai/gpt-") {
        openai_responses(m, s)
    } else {
        open_responses(m, s)
    }
}

fn vercel_messages(m: &Target, s: &Support) -> Vec<Variant> {
    if m.model_id.starts_with("anthropic/") {
        anthropic_messages(m, s)
    } else {
        vercel_chat(m, s)
    }
}

fn vercel_chat(m: &Target, s: &Support) -> Vec<Variant> {
    match s {
        Support::Effort(_) => openai_chat(m, s),
        Support::Toggle => toggle(
            settings(json!({"thinking": {"type": "disabled"}})),
            settings(json!({"thinking": {"type": "adaptive"}})),
        ),
        Support::Budget(b) => {
            let ceiling = if m.model_id.starts_with("alibaba/") {
                ALIBABA_THINKING_BUDGET_MAX
            } else {
                m.output
            };
            budgets(m, b, ceiling, |t| {
                settings(json!({"thinking": {"type": "enabled", "budgetTokens": t}}))
            })
        }
    }
}

fn sap_ai_core(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id;
    let sap = |p: Value| settings(json!({ "modelParams": p }));
    match s {
        Support::Effort(v) => efforts(&values(v, &EFFORTS), |e| {
            if id.contains("anthropic") {
                let mut f = json!({"output_config": {"effort": e}});
                if !claude_info(m).manual {
                    f["thinking"] = adaptive();
                }
                sap(json!({ "additionalModelRequestFields": f }))
            } else if id.contains("gemini") {
                sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": e}}))
            } else if id.contains("amazon--nova") {
                sap(json!({"additionalModelRequestFields": {"output_config": {"effort": e}}}))
            } else {
                sap(json!({ "reasoning_effort": e }))
            }
        }),
        Support::Toggle => {
            if id.contains("gemini") {
                toggle(
                    sap(json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}})),
                    sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1}})),
                )
            } else if id.contains("cohere") {
                toggle(
                    sap(json!({"thinking": {"type": "disabled"}})),
                    sap(json!({"thinking": {"type": "enabled"}})),
                )
            } else if id.contains("amazon--nova") {
                toggle(
                    sap(
                        json!({"additionalModelRequestFields": {"thinking": {"type": "disabled"}}}),
                    ),
                    sap(json!({"additionalModelRequestFields": {"thinking": {"type": "enabled"}}})),
                )
            } else if id.contains("anthropic") {
                toggle(
                    sap(
                        json!({"additionalModelRequestFields": {"thinking": {"type": "disabled"}}}),
                    ),
                    sap(json!({"additionalModelRequestFields": {"thinking": adaptive()}})),
                )
            } else {
                vec![]
            }
        }
        Support::Budget(b) => {
            if id.contains("anthropic") {
                budgets(m, b, ANTHROPIC_OUTPUT_TOKEN_MAX, |t| {
                    sap(
                        json!({"additionalModelRequestFields": {"thinking": {"type": "enabled", "budget_tokens": t}}}),
                    )
                })
            } else if id.contains("gemini") {
                budgets(m, b, m.output, |t| {
                    sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": t}}))
                })
            } else if id.contains("cohere") {
                budgets(m, b, m.output, |t| {
                    sap(json!({"thinking": {"type": "enabled", "token_budget": t}}))
                })
            } else {
                vec![]
            }
        }
    }
}

fn gitlab_anthropic(m: &Target, s: &Support) -> Vec<Variant> {
    let info = claude_info(m);
    let disabled = || settings(json!({"thinking": {"type": "disabled"}}));
    let adaptive_effort =
        |e: &str| settings(json!({"thinking": {"type": "adaptive", "effort": e}}));
    match s {
        Support::Effort(v) => {
            if info.manual {
                return gitlab_anthropic(
                    m,
                    &Support::Budget(Budget {
                        min: Some(1024),
                        max: None,
                    }),
                );
            }
            let d: &[&str] = if info.major == Some(4) && info.minor == 6 {
                &["low", "medium", "high", "max"]
            } else {
                &["low", "medium", "high", "xhigh", "max"]
            };
            efforts(&values(v, d), |e| {
                if e == "none" {
                    disabled()
                } else {
                    adaptive_effort(e)
                }
            })
        }
        Support::Toggle => {
            if info.always {
                return vec![];
            }
            let thinking = if info.manual {
                manual_thinking(m).map(|t| settings(json!({ "thinking": t })))
            } else {
                Some(adaptive_effort("high"))
            };
            match thinking {
                Some(t) => toggle(disabled(), t),
                None => vec![],
            }
        }
        Support::Budget(b) => budgets(m, b, ANTHROPIC_OUTPUT_TOKEN_MAX, |t| {
            settings(json!({"thinking": {"type": "enabled", "budgetTokens": t}}))
        }),
    }
}

fn gitlab(m: &Target, s: &Support) -> Vec<Variant> {
    let id = m.model_id.to_ascii_lowercase();
    if id.starts_with("duo-workflow") {
        vec![]
    } else if claude_info(m).family.is_some() {
        gitlab_anthropic(m, s)
    } else if id.contains("gpt") || id.contains("codex") {
        openai_chat(m, s)
    } else {
        vec![]
    }
}

static KIMI_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)kimi[-.]?k2[.-]7-code").unwrap());

static MINIMAX_M2: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)minimax[-.]?m2(?:[.-]|$)").unwrap());

static KIMI_K2: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)kimi[-.]?k2").unwrap());

fn protocol(package: &str) -> Option<Protocol> {
    let p = package.strip_prefix("@opencode/ai/providers/");
    if let Some(p) = p {
        return Some(match p {
            "openai"
            | "openai/responses"
            | "azure"
            | "azure/responses"
            | "amazon-bedrock/mantle"
            | "amazon-bedrock/mantle/responses"
            | "alibaba/responses"
            | "meta"
            | "meta/responses"
            | "minimax/responses"
            | "moonshot/responses"
            | "zai-coding-plan/responses" => openai_responses,
            "openai-compatible/responses"
            | "openai-compatible-responses"
            | "google-vertex/responses"
            | "xai"
            | "digitalocean/responses"
            | "openrouter/responses"
            | "venice/responses" => open_responses,
            "openai-compatible" => openai_compatible,
            "openai/chat"
            | "azure/chat"
            | "amazon-bedrock/mantle/chat"
            | "cerebras"
            | "cohere/chat"
            | "digitalocean"
            | "fireworks"
            | "groq"
            | "meta/chat"
            | "minimax/chat"
            | "mistral"
            | "google-vertex/mistral"
            | "togetherai" => openai_chat,
            "google-vertex/chat" => vertex_chat,
            "alibaba" | "alibaba/chat" => alibaba_chat,
            "baseten" => baseten_chat,
            "cloudflare-workers-ai" => workers_ai_chat,
            "deepinfra" => deepinfra_chat,
            "deepseek" => deepseek_chat,
            "moonshot" | "moonshot/chat" => moonshot_chat,
            "venice" => venice_chat,
            "zai" | "zai/chat" | "zai-coding-plan" | "zai-coding-plan/chat" => zai_chat,
            "anthropic"
            | "anthropic-compatible"
            | "amazon-bedrock/mantle/messages"
            | "azure/messages"
            | "google-vertex/messages"
            | "google-vertex-messages"
            | "meta/messages" => anthropic_messages,
            "alibaba/messages" => alibaba_messages,
            "minimax" | "minimax/messages" => minimax_messages,
            "moonshot/messages" => moonshot_messages,
            "zai-coding-plan/messages" => zai_messages,
            "google"
            | "google-vertex"
            | "google-vertex/gemini"
            | "google/images"
            | "google-vertex/images" => gemini,
            "google/interactions" | "google-vertex/interactions" => google_interactions,
            "amazon-bedrock" => bedrock_converse,
            "cohere" => cohere,
            "openrouter" => openrouter,
            "cloudflare-ai-gateway" => cloudflare_ai_gateway,
            "vercel-ai-gateway" => vercel_gateway,
            "vercel-ai-gateway/chat" => vercel_chat,
            "vercel-ai-gateway/messages" => vercel_messages,
            "vercel-ai-gateway/responses" => vercel_responses,
            _ => return None,
        });
    }
    Some(match package {
        "aisdk:venice-ai-sdk-provider" => venice_chat,
        "aisdk:@ai-sdk/gateway" => vercel_gateway,
        "aisdk:@jerome-benoit/sap-ai-provider-v2" => sap_ai_core,
        "aisdk:gitlab-ai-provider" => gitlab,
        _ => return None,
    })
}
