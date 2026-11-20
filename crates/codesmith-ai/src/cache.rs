//! Prompt caching (after OpenCode's `cache-policy.ts` and the per-protocol
//! cache lowering).
//!
//! A request carries a [`Cache`] policy, on by default. Protocols with inline
//! cache markers (Anthropic Messages, and Chat gateways that take Anthropic's
//! `cache_control` format) get breakpoints placed on the lowered body: the
//! last tool definition, the first and last system blocks, and the last
//! message, at most four in all. Protocols that cache by prefix on their own
//! (OpenAI Chat and Responses) get a stable `prompt_cache_key` and the
//! requested lifetime as a request option; Gemini caches implicitly.
//!
//! Breakpoints are placed on the wire body rather than on CodeSmith's
//! messages, so they land on the blocks the provider sees after roles are
//! folded and tool results reordered. Markers already in the body (from
//! `body` overrides) are kept and count against the cap.

use serde_json::{json, Value};

/// Anthropic (and Anthropic-format gateways) accept at most four explicit
/// breakpoints per request.
pub const BREAKPOINT_CAP: usize = 4;

/// OpenAI limits `prompt_cache_key` to 64 characters.
pub const PROMPT_CACHE_KEY_MAX: usize = 64;

/// The prompt cache policy for one request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cache {
    pub placement: Placement,
    /// Cache lifetime such as `"5m"`, `"1h"` or `"24h"`, sent as given; each
    /// provider accepts its own set. `None` keeps the provider default.
    pub ttl: Option<String>,
}

/// Where breakpoints go.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Placement {
    /// The last tool, the first and last system blocks and the last message
    /// (per route: gateways to models that cache without breakpoints get
    /// none, Qwen gets no tool breakpoint).
    #[default]
    Auto,
    /// No caching: no breakpoints, no `prompt_cache_key`, no lifetime.
    Off,
    /// Exactly these placements.
    Custom {
        tools: bool,
        system: bool,
        messages: Option<Messages>,
    },
}

/// Which messages get a breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Messages {
    /// The last `n` messages.
    Tail(usize),
    LatestUser,
    LatestAssistant,
}

impl Cache {
    /// Caching switched off.
    pub fn off() -> Self {
        Cache {
            placement: Placement::Off,
            ttl: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.placement != Placement::Off
    }

    /// The lifetime to send, when caching is on.
    pub fn lifetime(&self) -> Option<&str> {
        self.ttl.as_deref().filter(|_| self.enabled())
    }

    /// A policy from config: `true`/`"auto"` (the default), `false`/`"none"`,
    /// or an object of `ttl` and, optionally, exact placements (`tools`,
    /// `system`, `messages`: a count, `{"tail": n}`, `"latest-user-message"`
    /// or `"latest-assistant"`). An object with `"enabled": false` is off.
    /// `None` for anything else.
    pub fn from_value(v: &Value) -> Option<Cache> {
        match v {
            Value::Null | Value::Bool(true) => Some(Cache::default()),
            Value::Bool(false) => Some(Cache::off()),
            Value::String(s) => match s.as_str() {
                "auto" | "on" => Some(Cache::default()),
                "none" | "off" | "disabled" => Some(Cache::off()),
                _ => None,
            },
            Value::Object(m) => {
                if m.get("enabled") == Some(&Value::Bool(false)) {
                    return Some(Cache::off());
                }
                let ttl = match m.get("ttl") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) if is_ttl(s) => Some(s.clone()),
                    Some(_) => return None,
                };
                let flag = |k: &str| m.get(k).and_then(Value::as_bool);
                let messages = match m.get("messages") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(messages_from(v)?),
                };
                let (tools, system) = (flag("tools"), flag("system"));
                let placement = if tools.is_none() && system.is_none() && messages.is_none() {
                    Placement::Auto
                } else {
                    Placement::Custom {
                        tools: tools.unwrap_or(false),
                        system: system.unwrap_or(false),
                        messages,
                    }
                };
                Some(Cache { placement, ttl })
            }
            _ => None,
        }
    }
}

fn messages_from(v: &Value) -> Option<Messages> {
    match v {
        Value::String(s) if s == "latest-user-message" => Some(Messages::LatestUser),
        Value::String(s) if s == "latest-assistant" => Some(Messages::LatestAssistant),
        Value::Number(n) => Some(Messages::Tail(n.as_u64()? as usize)),
        Value::Object(m) => Some(Messages::Tail(m.get("tail")?.as_u64()? as usize)),
        _ => None,
    }
}

/// `30m`, `1h`, `24h`, … (models.dev's duration format).
pub(crate) fn is_ttl(s: &str) -> bool {
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && matches!(&s[digits..], "ms" | "s" | "m" | "h" | "d")
}

/// The placements a route uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Policy {
    pub tools: bool,
    pub system: bool,
    pub messages: Option<Messages>,
}

pub(crate) const AUTO: Policy = Policy {
    tools: true,
    system: true,
    messages: Some(Messages::Tail(1)),
};

/// Qwen ignores breakpoints on tool definitions and caches tools with the
/// system prompt.
pub(crate) const QWEN: Policy = Policy {
    tools: false,
    system: true,
    messages: Some(Messages::Tail(1)),
};

pub(crate) const NONE: Policy = Policy {
    tools: false,
    system: false,
    messages: None,
};

impl Policy {
    pub fn places(&self) -> bool {
        self.tools || self.system || self.messages.is_some()
    }
}

/// The policy a request asks for, `auto` standing for `auto_policy`.
pub(crate) fn resolve(cache: &Cache, auto_policy: Policy) -> Policy {
    match &cache.placement {
        Placement::Auto => auto_policy,
        Placement::Off => NONE,
        Placement::Custom {
            tools,
            system,
            messages,
        } => Policy {
            tools: *tools,
            system: *system,
            messages: *messages,
        },
    }
}

/// OpenRouter and similar gateways: Anthropic models take breakpoints, Qwen
/// takes them without tools, and other upstreams cache without them (Gemini
/// uses only the last breakpoint, so a moving tail would write a new cache
/// every step).
pub(crate) fn gateway_policy(model: &str) -> Policy {
    let id = model.trim_start_matches('~').to_ascii_lowercase();
    if id.starts_with("anthropic/") {
        AUTO
    } else if id.starts_with("qwen/") || id.starts_with("alibaba/qwen") {
        QWEN
    } else {
        NONE
    }
}

/// The `cache_control` marker.
fn marker(ttl: Option<&str>) -> Value {
    match ttl {
        Some(t) => json!({"type": "ephemeral", "ttl": t}),
        None => json!({"type": "ephemeral"}),
    }
}

fn marked(v: &Value) -> bool {
    v.get("cache_control").is_some_and(|c| !c.is_null())
}

/// Blocks that cannot carry a breakpoint.
fn cacheable(block: &Value) -> bool {
    !matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}

/// Text content as a one-block array, so a breakpoint can go on it.
fn blocks(content: &mut Value) -> Option<&mut Vec<Value>> {
    if let Some(s) = content.as_str() {
        if s.is_empty() {
            return None;
        }
        *content = json!([{"type": "text", "text": s}]);
    }
    content.as_array_mut()
}

/// Mark the message's last text block, else its last cacheable block. A
/// block already marked is left alone.
fn mark_message(m: &mut Value, hint: &Value, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    let Some(content) = m.get_mut("content").and_then(blocks) else {
        return;
    };
    let at = content
        .iter()
        .rposition(|b| b["type"] == "text")
        .or_else(|| content.iter().rposition(cacheable));
    let Some(block) = at.map(|i| &mut content[i]) else {
        return;
    };
    if marked(block) || !cacheable(block) || !block.is_object() {
        return;
    }
    block["cache_control"] = hint.clone();
    *budget -= 1;
}

/// The messages `which` names, as indices into `messages`.
fn targets(messages: &[Value], which: Messages) -> Vec<usize> {
    let last = |role: &str| messages.iter().rposition(|m| m["role"] == role);
    match which {
        // Messages without content (an effort update) take no marker; the
        // tail walks back past them.
        Messages::Tail(n) => {
            let mut at: Vec<usize> = (0..messages.len())
                .rev()
                .filter(|&i| !messages[i]["content"].as_array().is_some_and(Vec::is_empty))
                .take(n)
                .collect();
            at.reverse();
            at
        }
        Messages::LatestUser => last("user").into_iter().collect(),
        Messages::LatestAssistant => last("assistant").into_iter().collect(),
    }
}

/// Visit every `cache_control` slot in breakpoint priority order: tools,
/// then the system prompt (Anthropic's `system`, or Chat's leading system
/// messages), then messages.
fn each_slot(b: &mut serde_json::Map<String, Value>, f: &mut dyn FnMut(&mut Value)) {
    if let Some(Value::Array(t)) = b.get_mut("tools") {
        t.iter_mut().for_each(&mut *f);
    }
    if let Some(Value::Array(s)) = b.get_mut("system") {
        s.iter_mut().for_each(&mut *f);
    }
    if let Some(Value::Array(m)) = b.get_mut("messages") {
        for m in m.iter_mut() {
            if let Some(Value::Array(c)) = m.get_mut("content") {
                c.iter_mut().for_each(&mut *f);
            }
        }
    }
}

/// The breakpoints left under `limit` after the markers already in the body.
fn budget(b: &mut serde_json::Map<String, Value>, limit: usize) -> usize {
    let mut n = 0;
    each_slot(b, &mut |s| n += usize::from(marked(s)));
    limit.saturating_sub(n)
}

/// Keep the first `limit` markers and drop the rest.
fn cap(b: &mut serde_json::Map<String, Value>, limit: usize) {
    let mut kept = 0;
    each_slot(b, &mut |s| {
        if !marked(s) {
            return;
        }
        if kept < limit {
            kept += 1;
        } else if let Some(o) = s.as_object_mut() {
            o.remove("cache_control");
        }
    });
}

/// Mark the last tool definition.
fn mark_tools(b: &mut serde_json::Map<String, Value>, hint: &Value, budget: &mut usize) {
    if let Some(Value::Array(t)) = b.get_mut("tools") {
        if let Some(last) = t.last_mut().filter(|l| l.is_object() && !marked(l)) {
            if *budget > 0 {
                last["cache_control"] = hint.clone();
                *budget -= 1;
            }
        }
    }
}

/// The first and the last of `n` items (one when they are the same).
fn ends(n: usize) -> Vec<usize> {
    match n {
        0 => vec![],
        1 => vec![0],
        n => vec![0, n - 1],
    }
}

/// Place breakpoints on an Anthropic Messages body.
pub(crate) fn mark_anthropic(body: &mut Value, policy: Policy, ttl: Option<&str>) {
    let hint = marker(ttl);
    let Some(b) = body.as_object_mut() else {
        return;
    };
    if policy.system {
        if let Some(s) = b.get_mut("system") {
            blocks(s);
        }
    }
    // Top-level `cache_control` (Anthropic's automatic caching, the
    // `cacheControl` option) takes one of the four slots.
    let automatic = b.get("cache_control").is_some_and(|c| !c.is_null());
    let limit = BREAKPOINT_CAP - usize::from(automatic);
    let mut budget = budget(b, limit);
    if policy.tools {
        mark_tools(b, &hint, &mut budget);
    }
    if policy.system {
        if let Some(Value::Array(s)) = b.get_mut("system") {
            for i in ends(s.len()) {
                let block = &mut s[i];
                if block.is_object() && !marked(block) && budget > 0 {
                    block["cache_control"] = hint.clone();
                    budget -= 1;
                }
            }
        }
    }
    if let (Some(which), Some(Value::Array(m))) = (policy.messages, b.get_mut("messages")) {
        for i in targets(m, which) {
            mark_message(&mut m[i], &hint, &mut budget);
        }
    }
    cap(b, limit);
}

/// Place breakpoints on a Chat Completions body (Anthropic's format on
/// `cache_control`, as OpenRouter, DigitalOcean and Alibaba accept): tool
/// objects carry the marker beside `function`, the system prompt and the
/// last message carry it on a text content part.
pub(crate) fn mark_chat(body: &mut Value, policy: Policy, ttl: Option<&str>) {
    let hint = marker(ttl);
    let Some(b) = body.as_object_mut() else {
        return;
    };
    let mut budget = budget(b, BREAKPOINT_CAP);
    if policy.tools {
        mark_tools(b, &hint, &mut budget);
    }
    if let Some(Value::Array(m)) = b.get_mut("messages") {
        // The leading system messages are the system prompt.
        let lead = m.iter().take_while(|m| m["role"] == "system").count();
        if policy.system {
            for i in ends(lead) {
                mark_message(&mut m[i], &hint, &mut budget);
            }
        }
        if let Some(which) = policy.messages {
            let rest = &mut m[lead..];
            for i in targets(rest, which) {
                mark_message(&mut rest[i], &hint, &mut budget);
            }
        }
    }
    cap(b, BREAKPOINT_CAP);
}

/// OpenAI's request-level lifetime: `prompt_cache_retention` takes only
/// `24h` (and is the only control before GPT-5.6); every other lifetime goes
/// to `prompt_cache_options.ttl`, which GPT-5.6+, Kimi and DeepInfra read.
pub(crate) fn openai_ttl(ttl: Option<&str>) -> Option<(&'static str, Value)> {
    match ttl? {
        "24h" => Some(("prompt_cache_retention", json!("24h"))),
        t => Some(("prompt_cache_options", json!({"ttl": t}))),
    }
}

/// The request's `prompt_cache_key`, clamped to OpenAI's limit, unless
/// caching is off.
pub(crate) fn prompt_cache_key(req: &crate::Request) -> Option<String> {
    if !req.cache.enabled() {
        return None;
    }
    let key = req.prompt_cache_key.as_deref().filter(|k| !k.is_empty())?;
    Some(key.chars().take(PROMPT_CACHE_KEY_MAX).collect())
}
