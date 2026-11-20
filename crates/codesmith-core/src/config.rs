//! `codesmith.json` / `codesmith.jsonc` discovery, merging and parsing.
//!
//! Lowest to highest precedence:
//!   1. `<config dir>/codesmith.json[c]` (global)
//!   2. the file named by `$CODESMITH_CONFIG`
//!   3. from the project root down to the working directory, each folder's
//!      `codesmith.json[c]`
//!   4. then, in the same order, each folder's `.codesmith/codesmith.json[c]`
//!      (3 and 4 are skipped when `$CODESMITH_DISABLE_PROJECT_CONFIG` is set)
//!   5. inline JSON in `$CODESMITH_CONFIG_CONTENT`
//!
//! Objects merge key by key; `permissions` and `instructions` lists append,
//! so a later file's rules come after (and so override) earlier ones. A file
//! reached twice loads once.
//!
//! Pinned to OpenCode's config loading by the `oc_config_*` tests (ported
//! from `packages/core/test/config/config.test.ts` and `model.test.ts`).
//! Deliberate differences:
//! - Only `codesmith.json[c]` and `.codesmith/` are read, never
//!   `opencode.json`; there is no system, MDM, well-known or integration
//!   config, no v1 format migration (so `normalization.test.ts` has no
//!   counterpart) and no live reload or file watching.
//! - The project walk stops at the project root (the nearest `.git`), where
//!   OpenCode keeps walking to the filesystem root.
//! - A file that does not parse is an error naming the file. OpenCode skips
//!   it and loads the rest; skipping silently could drop `deny` rules.
//! - Named maps (`providers`, `agents`, `commands`) are ordered by key, not by
//!   source order, and model references have no `#variant` suffix.

use crate::paths;
use crate::permission::Rule;
use anyhow::{anyhow, Context, Result};
use codesmith_ai::Protocol;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    /// Default model as `provider/model`.
    pub model: Option<String>,
    pub default_agent: Option<String>,
    /// Shell for the `shell` tool; defaults to `$SHELL`, then `/bin/sh`.
    pub shell: Option<String>,
    pub permissions: Vec<Rule>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub agents: BTreeMap<String, AgentConfig>,
    /// Extra instruction files (paths or globs, relative to the project root).
    pub instructions: Vec<String>,
    pub tool_output: ToolOutputConfig,
    /// Snapshot files at each turn so turns can be undone (default true).
    pub snapshots: Option<bool>,
    /// Extra folders containing `<id>/SKILL.md` skills.
    pub skills: Vec<String>,
    /// Format files after edits: `true` for the built-in formatters, or an
    /// object of per-formatter overrides. Off when unset.
    pub formatter: Option<crate::format::Setting>,
    /// Language servers whose errors are added to `write`/`edit` results:
    /// `true` for the built-ins, or an object of per-server overrides and
    /// custom servers. Off when unset.
    pub lsp: Option<crate::lsp::Setting>,
    /// Where `--worktree` creates worktrees and what it runs in them.
    pub worktree: crate::worktree::WorktreeConfig,
    /// `false` keeps this process from running automations (the UI,
    /// `serve` and `acp` run them by default).
    pub automations: Option<bool>,
    /// Unattended automation runs: `permissions` are rules applied after
    /// the default unattended denies (shell, edit, webfetch,
    /// external_directory), to opt back in.
    pub automation: crate::automation::Settings,
    /// `false` offers every MCP tool as a direct tool call instead of
    /// through Code Mode (useful for models that write JavaScript poorly).
    pub codemode: Option<bool>,
    /// How deep subagents may nest: 1 (the default) lets the main session
    /// start subagents but not a subagent start its own.
    pub subagent_depth: Option<u32>,
    /// Compiled-in plugins: `false` turns one off, an object is its options.
    pub plugins: BTreeMap<String, Value>,
    /// Code review settings for `/review` and `/calibrate`.
    pub review: crate::review::ReviewConfig,
    /// Model Context Protocol servers whose tools the agent can use.
    pub mcp: crate::mcp::McpConfig,
    /// Web search through a self-hosted SearXNG instance.
    pub websearch: crate::tool::websearch::WebSearchConfig,
    /// Commands defined inline: name → template and options.
    pub commands: BTreeMap<String, crate::command::Command>,
    /// When to compact and how much recent history to keep verbatim.
    pub compaction: crate::compaction::Settings,
    /// Limits images are fitted to before they reach the model
    /// (`media.image`: `auto_resize`, `max_width`, `max_height`,
    /// `max_base64_bytes`).
    pub media: crate::media::MediaConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub name: Option<String>,
    pub protocol: Option<Protocol>,
    /// Environment variables that may hold the API key, in order.
    pub env: Vec<String>,
    pub settings: ProviderSettings,
    pub headers: BTreeMap<String, String>,
    /// Extra request-body fields for every model of this provider.
    pub body: Map<String, Value>,
    /// Provider options for every model of this provider (see
    /// [`ModelConfig::options`]).
    pub options: Map<String, Value>,
    pub models: BTreeMap<String, ModelConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProviderSettings {
    #[serde(alias = "baseURL", alias = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(alias = "apiKey")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub name: Option<String>,
    /// The model id sent to the provider, if different from the key.
    pub id: Option<String>,
    pub limit: ModelLimit,
    pub body: Map<String, Value>,
    pub temperature: Option<f32>,
    /// Price in dollars per million tokens, for `codesmith stats`.
    pub cost: Option<Cost>,
    /// What the model takes and makes, as in OpenCode and models.dev:
    /// `{"input": ["text", "image", "pdf"]}`. Unset means unknown, and
    /// images and PDFs are sent.
    pub modalities: Option<Modalities>,
    /// OpenCode's older flag: `false` means text input only.
    pub attachment: Option<bool>,
    /// Provider options sent with every request (OpenCode's model
    /// `options`/`settings`): `reasoningEffort`, `thinking`,
    /// `thinkingConfig`, `serviceTier`, `textVerbosity`, … in camelCase or
    /// snake_case. Each protocol lowers the ones it knows.
    #[serde(alias = "settings")]
    pub options: Map<String, Value>,
    /// Named request variants, selected as `provider/model#name`: a map of
    /// name to options (or to `{"options", "body", "headers"}`), or
    /// OpenCode's list of `{"id", "settings", "body", "headers"}`. They add
    /// to (and override) the built-in reasoning variants.
    pub variants: Option<Value>,
}

impl ModelConfig {
    /// The configured variants by name.
    pub fn variants(&self) -> Vec<(String, codesmith_ai::Variant)> {
        match &self.variants {
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| (k.clone(), codesmith_ai::variant_from_config(v)))
                .collect(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| {
                    let id = v["id"].as_str()?.to_string();
                    Some((id, codesmith_ai::variant_from_config(v)))
                })
                .collect(),
            _ => vec![],
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Modalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

impl ModelConfig {
    /// The input modalities, when configured.
    pub fn input(&self) -> Option<Vec<String>> {
        if let Some(m) = &self.modalities {
            return Some(m.input.clone());
        }
        match self.attachment {
            Some(false) => Some(vec!["text".into()]),
            _ => None,
        }
    }
}

/// Input modalities for models known to take text only, when the config
/// says nothing: DeepSeek's chat and reasoning models.
fn known_input(model: &str) -> Option<Vec<String>> {
    let m = model.to_ascii_lowercase();
    (m.contains("deepseek") && !m.contains("vl")).then(|| vec!["text".into()])
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Config {
    /// The configured price of `provider/model`, if any.
    pub fn cost(&self, spec: &str) -> Option<Cost> {
        let (p, m) = split_variant(spec).0.split_once('/')?;
        self.provider(p)?.models.get(m)?.cost
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct ModelLimit {
    pub context: Option<u64>,
    pub output: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub description: Option<String>,
    pub model: Option<String>,
    pub system: Option<String>,
    pub mode: Option<String>,
    pub steps: Option<u32>,
    pub disabled: bool,
    pub permissions: Vec<Rule>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct ToolOutputConfig {
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        ToolOutputConfig {
            max_lines: 2_000,
            max_bytes: 50 * 1024,
        }
    }
}

/// A loaded config and the files it came from.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub config: Config,
    pub files: Vec<PathBuf>,
    /// Project root (nearest folder with `.git`, else the working directory).
    pub root: PathBuf,
}

pub const NAMES: [&str; 2] = ["codesmith.json", "codesmith.jsonc"];

/// The nearest ancestor of `dir` (inclusive) containing `.git`, else `dir`.
pub fn project_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// Where [`load_from`] looks. [`Sources::from_env`] is what [`load`] uses.
#[derive(Debug, Clone)]
pub struct Sources {
    /// The global config folder (`codesmith.json[c]` there loads first).
    pub global_dir: PathBuf,
    /// An explicit file (`$CODESMITH_CONFIG`), above the global config.
    pub file: Option<PathBuf>,
    /// Inline JSON (`$CODESMITH_CONFIG_CONTENT`), above everything.
    pub content: Option<String>,
    /// Whether to look for project files (`$CODESMITH_DISABLE_PROJECT_CONFIG` turns it off).
    pub project: bool,
}

impl Sources {
    pub fn from_env() -> Sources {
        Sources {
            global_dir: paths::config_dir(),
            file: std::env::var_os("CODESMITH_CONFIG").map(PathBuf::from),
            content: std::env::var("CODESMITH_CONFIG_CONTENT").ok(),
            project: std::env::var_os("CODESMITH_DISABLE_PROJECT_CONFIG").is_none(),
        }
    }
}

/// Config file candidates in precedence order (lowest first).
pub fn candidates(cwd: &Path, root: &Path) -> Vec<PathBuf> {
    candidates_from(cwd, root, &Sources::from_env())
}

/// Candidates for `sources`: global, explicit file, then every folder's
/// `codesmith.json[c]` from the project root down to `cwd`, then every
/// folder's `.codesmith/codesmith.json[c]` in the same order (as OpenCode
/// ranks its `.opencode/` folders above all plain project files).
pub fn candidates_from(cwd: &Path, root: &Path, sources: &Sources) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = NAMES.iter().map(|n| sources.global_dir.join(n)).collect();
    if let Some(p) = &sources.file {
        out.push(p.clone());
    }
    if sources.project {
        let mut dirs: Vec<&Path> = cwd
            .ancestors()
            .take_while(|d| d.starts_with(root))
            .collect();
        dirs.reverse();
        for d in &dirs {
            for n in NAMES {
                out.push(d.join(n));
            }
        }
        for d in &dirs {
            for n in NAMES {
                out.push(d.join(".codesmith").join(n));
            }
        }
    }
    out
}

pub fn load(cwd: &Path) -> Result<Loaded> {
    load_from(cwd, &Sources::from_env())
}

pub fn load_from(cwd: &Path, sources: &Sources) -> Result<Loaded> {
    let root = project_root(cwd);
    let mut merged = Value::Object(Map::new());
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for path in candidates_from(cwd, &root, sources) {
        if !path.is_file() {
            continue;
        }
        // A file reached twice (the project walk passing through the global
        // folder, or `$CODESMITH_CONFIG` naming a project file) loads once.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let v = parse(&text, path.parent().unwrap_or(Path::new(".")))
            .with_context(|| format!("in {}", path.display()))?;
        merge(&mut merged, v);
        files.push(path);
    }
    if let Some(text) = &sources.content {
        merge(
            &mut merged,
            parse(text, cwd).context("in $CODESMITH_CONFIG_CONTENT")?,
        );
    }
    let mut config: Config = serde_json::from_value(merged).context("invalid codesmith config")?;
    crate::plugin::active().transform(&mut config);
    Ok(Loaded {
        config,
        files,
        root,
    })
}

/// Parse JSONC text after `{env:VAR}` / `{file:path}` substitution.
pub fn parse(text: &str, base: &Path) -> Result<Value> {
    let substituted = substitute(text, base);
    let clean = strip_jsonc(&substituted);
    serde_json::from_str(&clean).map_err(|e| anyhow!("{e}"))
}

/// Replace `{env:VAR}` with the variable and `{file:path}` with the file's
/// contents (relative to `base`, `~/` expanded, trimmed). Missing or
/// unreadable values become empty. As in OpenCode, variables go in raw (so
/// `"snapshots": {env:FLAG}` or an already-escaped value keep their meaning)
/// unless that breaks the JSON, in which case they are JSON-string-escaped
/// (a Windows path, a value with quotes). File contents are always escaped.
/// A `{file:}` on a `//` comment line is left alone.
pub fn substitute(text: &str, base: &Path) -> String {
    let raw = substitute_with(text, base, false);
    if !text.contains("{env:") || serde_json::from_str::<Value>(&strip_jsonc(&raw)).is_ok() {
        return raw;
    }
    substitute_with(text, base, true)
}

fn escaped(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap();
    quoted[1..quoted.len() - 1].to_string()
}

fn substitute_with(text: &str, base: &Path, escape_env: bool) -> String {
    let env = regex::Regex::new(r"\{env:([^}]+)\}").unwrap();
    let text = env.replace_all(text, |c: &regex::Captures| {
        let value = std::env::var(&c[1]).unwrap_or_default();
        if escape_env {
            escaped(&value)
        } else {
            value
        }
    });
    let file = regex::Regex::new(r"\{file:([^}]+)\}").unwrap();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for c in file.captures_iter(&text) {
        let m = c.get(0).unwrap();
        out.push_str(&text[cursor..m.start()]);
        cursor = m.end();
        let line_start = text[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..m.start()].trim_start().starts_with("//") {
            out.push_str(m.as_str());
            continue;
        }
        let raw = &c[1];
        let p = match raw.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None => base.join(raw),
        };
        let content = std::fs::read_to_string(p).unwrap_or_default();
        out.push_str(&escaped(content.trim()));
    }
    out.push_str(&text[cursor..]);
    out
}

/// Remove `//` and `/* */` comments and trailing commas outside strings.
pub fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if !matches!(next, Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

const APPEND: [&str; 2] = ["permissions", "instructions"];

/// Deep-merge `top` into `base`.
pub fn merge(base: &mut Value, top: Value) {
    match (base, top) {
        (Value::Object(b), Value::Object(t)) => {
            for (k, v) in t {
                match b.get_mut(&k) {
                    Some(Value::Array(existing)) if APPEND.contains(&k.as_str()) => {
                        if let Value::Array(more) = v {
                            existing.extend(more);
                        } else {
                            b.insert(k, v);
                        }
                    }
                    Some(existing) if existing.is_object() && v.is_object() => merge(existing, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, t) => *b = t,
    }
}

/// Split `provider/model#variant` into the model and its variant
/// (OpenCode's syntax: the variant follows the first `#` after the `/`).
pub fn split_variant(spec: &str) -> (&str, Option<&str>) {
    let slash = spec.find('/').map_or(0, |i| i + 1);
    match spec[slash..].find('#') {
        Some(i) => {
            let (base, v) = spec.split_at(slash + i);
            (base, Some(&v[1..]).filter(|v| !v.is_empty()))
        }
        None => (spec, None),
    }
}

/// A resolved provider and model, ready to call.
#[derive(Debug, Clone)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
    pub provider: codesmith_ai::Provider,
    pub body: Map<String, Value>,
    /// Provider options: the provider's, the model's, then the variant's.
    pub options: codesmith_ai::Options,
    /// The selected variant (`#name` in the spec), if any.
    pub variant: Option<String>,
    /// Every variant the model offers, in order.
    pub variants: Vec<String>,
    pub max_output: Option<u32>,
    pub context: Option<u64>,
    pub temperature: Option<f32>,
    /// Input modalities (`text`, `image`, `pdf`, …); `None` when unknown,
    /// in which case images and PDFs are sent.
    pub input: Option<Vec<String>>,
}

/// Built-in providers: (id, name, protocol, base URL, key variables).
/// Local servers need no key.
pub const BUILTIN: &[(&str, &str, Protocol, &str, &[&str])] = &[
    (
        "anthropic",
        "Anthropic",
        Protocol::AnthropicMessages,
        "https://api.anthropic.com/v1",
        &["ANTHROPIC_API_KEY"],
    ),
    (
        "openai",
        "OpenAI",
        Protocol::OpenaiResponses,
        "https://api.openai.com/v1",
        &["OPENAI_API_KEY"],
    ),
    (
        "google",
        "Google Gemini",
        Protocol::GoogleGemini,
        "https://generativelanguage.googleapis.com/v1beta",
        &[
            "GEMINI_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
            "GOOGLE_API_KEY",
        ],
    ),
    (
        "openrouter",
        "OpenRouter",
        Protocol::OpenaiChat,
        "https://openrouter.ai/api/v1",
        &["OPENROUTER_API_KEY"],
    ),
    (
        "groq",
        "Groq",
        Protocol::OpenaiChat,
        "https://api.groq.com/openai/v1",
        &["GROQ_API_KEY"],
    ),
    (
        "deepseek",
        "DeepSeek",
        Protocol::OpenaiChat,
        "https://api.deepseek.com/v1",
        &["DEEPSEEK_API_KEY"],
    ),
    (
        "mistral",
        "Mistral",
        Protocol::OpenaiChat,
        "https://api.mistral.ai/v1",
        &["MISTRAL_API_KEY"],
    ),
    (
        "xai",
        "xAI",
        Protocol::OpenaiChat,
        "https://api.x.ai/v1",
        &["XAI_API_KEY"],
    ),
    (
        "together",
        "Together AI",
        Protocol::OpenaiChat,
        "https://api.together.xyz/v1",
        &["TOGETHER_API_KEY"],
    ),
    (
        "fireworks",
        "Fireworks AI",
        Protocol::OpenaiChat,
        "https://api.fireworks.ai/inference/v1",
        &["FIREWORKS_API_KEY"],
    ),
    (
        "cerebras",
        "Cerebras",
        Protocol::OpenaiChat,
        "https://api.cerebras.ai/v1",
        &["CEREBRAS_API_KEY"],
    ),
    (
        "ollama",
        "Ollama (local)",
        Protocol::OpenaiChat,
        "http://127.0.0.1:11434/v1",
        &[],
    ),
    (
        "lmstudio",
        "LM Studio (local)",
        Protocol::OpenaiChat,
        "http://127.0.0.1:1234/v1",
        &[],
    ),
    (
        "llamacpp",
        "llama.cpp server (local)",
        Protocol::OpenaiChat,
        "http://127.0.0.1:8080/v1",
        &[],
    ),
];

fn builtin(id: &str) -> Option<ProviderConfig> {
    let (_, name, protocol, url, env) = BUILTIN.iter().find(|b| b.0 == id)?;
    Some(ProviderConfig {
        name: Some(name.to_string()),
        protocol: Some(*protocol),
        env: env.iter().map(|s| s.to_string()).collect(),
        settings: ProviderSettings {
            base_url: Some(url.to_string()),
            api_key: None,
        },
        ..Default::default()
    })
}

impl Config {
    /// The provider config for `id`: the built-in one (if any) overlaid by config.
    pub fn provider(&self, id: &str) -> Option<ProviderConfig> {
        match (builtin(id), self.providers.get(id).cloned()) {
            (None, None) => None,
            (Some(b), None) => Some(b),
            (None, Some(c)) => Some(c),
            (Some(b), Some(c)) => Some(ProviderConfig {
                name: c.name.or(b.name),
                protocol: c.protocol.or(b.protocol),
                env: if c.env.is_empty() { b.env } else { c.env },
                settings: ProviderSettings {
                    base_url: c.settings.base_url.or(b.settings.base_url),
                    api_key: c.settings.api_key.or(b.settings.api_key),
                },
                headers: c.headers,
                body: c.body,
                options: c.options,
                models: c.models,
            }),
        }
    }

    /// Resolve `provider/model` or `provider/model#variant` (the first `/`
    /// splits; model ids may contain more).
    pub fn resolve_model(&self, spec: &str) -> Result<ModelRef> {
        let (base, variant) = split_variant(spec);
        let (pid, mid) = base
            .split_once('/')
            .filter(|(p, m)| !p.is_empty() && !m.is_empty())
            .ok_or_else(|| anyhow!("model must be provider/model, got {spec:?}"))?;
        let p = self.provider(pid).ok_or_else(|| {
            anyhow!("unknown provider {pid:?}; add it under \"providers\" in codesmith.json")
        })?;
        let protocol = p.protocol.unwrap_or(Protocol::OpenaiChat);
        let base_url = p
            .settings
            .base_url
            .clone()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| anyhow!("provider {pid:?} has no settings.baseURL"))?;
        let api_key = p
            .settings
            .api_key
            .clone()
            .filter(|k| !k.is_empty())
            .or_else(|| {
                p.env
                    .iter()
                    .find_map(|v| std::env::var(v).ok().filter(|k| !k.is_empty()))
            })
            .or_else(|| crate::auth::key(pid));
        let m = p.models.get(mid).cloned().unwrap_or_default();
        let model_id = m.id.clone().unwrap_or_else(|| mid.to_string());
        let input = m.input().or_else(|| known_input(&model_id));
        let mut body = p.body.clone();
        body.extend(m.body.clone());
        let mut options = codesmith_ai::Options::from_map(p.options.clone());
        options.merge(&codesmith_ai::Options::from_map(m.options.clone()));
        let mut headers: Vec<(String, String)> = p.headers.into_iter().collect();
        // Built-in reasoning variants, then the configured ones (which win).
        let mut variants = codesmith_ai::builtin_variants(protocol, &model_id, m.limit.output);
        for (name, v) in m.variants() {
            match variants.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = v,
                None => variants.push((name, v)),
            }
        }
        let variant = variant.filter(|v| *v != "default");
        if let Some(name) = variant {
            let (_, v) = variants.iter().find(|(n, _)| n == name).ok_or_else(|| {
                anyhow!(
                    "model {base:?} has no variant {name:?}; available: {}",
                    variants
                        .iter()
                        .map(|v| v.0.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            options.merge(&v.options);
            body.extend(v.body.clone());
            for (k, val) in &v.headers {
                headers.retain(|(h, _)| !h.eq_ignore_ascii_case(k));
                headers.push((k.clone(), val.clone()));
            }
        }
        Ok(ModelRef {
            provider_id: pid.to_string(),
            model_id,
            provider: codesmith_ai::Provider {
                protocol,
                base_url,
                api_key,
                headers,
            },
            body,
            options,
            variant: variant.map(str::to_string),
            variants: variants.into_iter().map(|v| v.0).collect(),
            max_output: m.limit.output,
            context: m.limit.context,
            temperature: m.temperature,
            input,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jsonc_comments_and_trailing_commas() {
        let t = r#"{
            // line comment
            "a": "x // not a comment", /* block */
            "b": [1, 2,],
            "c": "say \"hi\", ok",
        }"#;
        let v: Value = serde_json::from_str(&strip_jsonc(t)).unwrap();
        assert_eq!(
            v,
            json!({"a": "x // not a comment", "b": [1, 2], "c": "say \"hi\", ok"})
        );
    }

    #[test]
    fn env_and_file_substitution() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("key.txt"), "se\"cret\n").unwrap();
        std::env::set_var("CODESMITH_TEST_SUB", "v1");
        let v = parse(r#"{"a": "{env:CODESMITH_TEST_SUB}", "b": "{file:key.txt}", "c": "{env:CODESMITH_TEST_UNSET_X}"}"#, dir.path()).unwrap();
        assert_eq!(v, json!({"a": "v1", "b": "se\"cret", "c": ""}));
    }

    #[test]
    fn merge_objects_and_append_lists() {
        let mut a = json!({"model": "a/x", "permissions": [{"action": "*"}], "providers": {"p": {"name": "P", "env": ["A"]}}, "instructions": ["one.md"]});
        merge(
            &mut a,
            json!({"model": "b/y", "permissions": [{"action": "shell"}], "providers": {"p": {"env": ["B"]}}, "instructions": ["two.md"]}),
        );
        assert_eq!(a["model"], "b/y");
        assert_eq!(
            a["permissions"],
            json!([{"action": "*"}, {"action": "shell"}])
        );
        assert_eq!(
            a["providers"]["p"],
            json!({"name": "P", "env": ["B"]}),
            "plain lists replace"
        );
        assert_eq!(a["instructions"], json!(["one.md", "two.md"]));
    }

    #[test]
    fn candidates_walk_from_root_to_cwd() {
        let root = Path::new("/r");
        let c = candidates(Path::new("/r/a/b"), root);
        let project: Vec<String> = c
            .iter()
            .map(|p| p.display().to_string())
            .filter(|p| p.starts_with("/r"))
            .collect();
        assert_eq!(project.first().unwrap(), "/r/codesmith.json");
        assert_eq!(project.last().unwrap(), "/r/a/b/.codesmith/codesmith.jsonc");
        assert_eq!(project.len(), 12);
    }

    #[test]
    fn project_root_finds_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("repo/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("repo/src/x")).unwrap();
        assert_eq!(
            project_root(&dir.path().join("repo/src/x")),
            dir.path().join("repo")
        );
        assert_eq!(project_root(dir.path()), dir.path());
    }

    #[test]
    fn config_shape() {
        let c: Config = serde_json::from_value(json!({
            "$schema": "https://example/schema.json",
            "model": "omlx/default",
            "permissions": [{"action": "shell", "resource": "rm *", "effect": "deny"}],
            "providers": {"omlx": {
                "protocol": "openai-chat",
                "settings": {"baseURL": "http://127.0.0.1:18000/v1", "apiKey": "k"},
                "body": {"chat_template_kwargs": {"enable_thinking": false}},
                "models": {"default": {"limit": {"context": 32768, "output": 8192}}}
            }},
            "tool_output": {"max_lines": 10},
            "unknown_future_key": true
        }))
        .unwrap();
        assert_eq!(c.tool_output.max_lines, 10);
        assert_eq!(c.tool_output.max_bytes, 50 * 1024);
        let m = c.resolve_model("omlx/default").unwrap();
        assert_eq!(m.provider.base_url, "http://127.0.0.1:18000/v1");
        assert_eq!(m.provider.api_key.as_deref(), Some("k"));
        assert_eq!(m.max_output, Some(8192));
        assert_eq!(m.body["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn builtin_providers_and_overrides() {
        let c = Config::default();
        let m = c.resolve_model("anthropic/claude-x").unwrap();
        assert_eq!(m.provider.protocol, Protocol::AnthropicMessages);
        assert_eq!(m.model_id, "claude-x");
        let c: Config = serde_json::from_value(
            json!({"providers": {"openai": {"settings": {"baseURL": "http://proxy/v1"}}}}),
        )
        .unwrap();
        let m = c.resolve_model("openai/gpt-x").unwrap();
        assert_eq!(m.provider.base_url, "http://proxy/v1");
        assert_eq!(m.provider.protocol, Protocol::OpenaiResponses);
        let g = Config::default().resolve_model("google/gemini-x").unwrap();
        assert_eq!(g.provider.protocol, Protocol::GoogleGemini);
        let o = Config::default().resolve_model("ollama/qwen3").unwrap();
        assert_eq!(
            (o.provider.base_url.as_str(), o.provider.protocol),
            ("http://127.0.0.1:11434/v1", Protocol::OpenaiChat)
        );
        let c: Config =
            serde_json::from_value(json!({"providers": {"openai": {"protocol": "openai-chat"}}}))
                .unwrap();
        assert_eq!(
            c.resolve_model("openai/gpt-x").unwrap().provider.protocol,
            Protocol::OpenaiChat,
            "the protocol can be switched back"
        );
    }

    #[test]
    fn model_ids_may_contain_slashes() {
        let c: Config = serde_json::from_value(
            json!({"providers": {"or": {"settings": {"baseURL": "http://x"}}}}),
        )
        .unwrap();
        assert_eq!(
            c.resolve_model("or/meta/llama-3").unwrap().model_id,
            "meta/llama-3"
        );
    }

    #[test]
    fn model_id_override() {
        let c: Config = serde_json::from_value(json!({"providers": {"p": {"settings": {"baseURL": "http://x"}, "models": {"fast": {"id": "real-model-name"}}}}})).unwrap();
        assert_eq!(
            c.resolve_model("p/fast").unwrap().model_id,
            "real-model-name"
        );
    }

    #[test]
    fn resolve_errors() {
        let c = Config::default();
        assert!(c.resolve_model("nomodel").is_err());
        assert!(c
            .resolve_model("nope/x")
            .unwrap_err()
            .to_string()
            .contains("unknown provider"));
        let c: Config = serde_json::from_value(json!({"providers": {"p": {}}})).unwrap();
        assert!(c
            .resolve_model("p/x")
            .unwrap_err()
            .to_string()
            .contains("baseURL"));
    }

    #[test]
    fn options_and_variants() {
        let c: Config = serde_json::from_value(json!({"providers": {"p": {
            "protocol": "openai-chat",
            "settings": {"baseURL": "http://x"},
            "options": {"service_tier": "flex", "reasoningEffort": "low"},
            "headers": {"x-a": "1"},
            "models": {"m": {
                "options": {"textVerbosity": "low"},
                "variants": {
                    "high": {"reasoning_effort": "high"},
                    "fast": {"options": {"serviceTier": "priority"}, "body": {"extra": true}, "headers": {"x-a": "2"}}
                }
            }, "o": {
                "settings": {"reasoningEffort": "minimal"},
                "variants": [{"id": "deep", "settings": {"reasoningEffort": "xhigh"}}]
            }}
        }}}))
        .unwrap();
        let m = c.resolve_model("p/m").unwrap();
        assert_eq!(m.variant, None);
        assert_eq!(m.options.reasoning_effort(), Some("low"));
        assert_eq!(m.options.str("serviceTier"), Some("flex"));
        assert_eq!(m.options.str("textVerbosity"), Some("low"));
        // Built-in Chat effort variants, then the configured ones.
        assert_eq!(m.variants, ["low", "medium", "high", "fast"]);
        let high = c.resolve_model("p/m#high").unwrap();
        assert_eq!(high.variant.as_deref(), Some("high"));
        assert_eq!(high.options.reasoning_effort(), Some("high"));
        assert_eq!(high.options.str("textVerbosity"), Some("low"));
        let fast = c.resolve_model("p/m#fast").unwrap();
        assert_eq!(fast.options.str("serviceTier"), Some("priority"));
        assert_eq!(fast.body["extra"], true);
        assert_eq!(
            fast.provider.headers,
            [("x-a".to_string(), "2".to_string())]
        );
        let medium = c.resolve_model("p/m#medium").unwrap();
        assert_eq!(medium.options.reasoning_effort(), Some("medium"));
        assert_eq!(c.resolve_model("p/m#default").unwrap().variant, None);
        let err = c.resolve_model("p/m#nope").unwrap_err().to_string();
        assert!(
            err.contains("no variant \"nope\"") && err.contains("fast"),
            "{err}"
        );
        // OpenCode's list form and `settings` spelling.
        let o = c.resolve_model("p/o#deep").unwrap();
        assert_eq!(o.options.reasoning_effort(), Some("xhigh"));
        assert_eq!(
            c.resolve_model("p/o").unwrap().options.reasoning_effort(),
            Some("minimal")
        );
        assert_eq!(split_variant("p/a#b"), ("p/a", Some("b")));
        assert_eq!(split_variant("p#x/a"), ("p#x/a", None));
        assert_eq!(split_variant("p/a#"), ("p/a", None));
    }

    #[test]
    fn builtin_variants_need_no_config() {
        let c = Config::default();
        let m = c.resolve_model("anthropic/claude-sonnet-4-5#high").unwrap();
        assert_eq!(
            m.options.get("thinking"),
            Some(&json!({"type": "enabled", "budgetTokens": 16000}))
        );
        let g = c.resolve_model("openai/gpt-5#low").unwrap();
        assert_eq!(g.options.reasoning_effort(), Some("low"));
        assert_eq!(g.options.str("reasoningSummary"), Some("auto"));
    }

    #[test]
    fn api_key_from_env_list() {
        std::env::set_var("CODESMITH_TEST_KEY_B", "kb");
        let c: Config = serde_json::from_value(json!({"providers": {"p": {"settings": {"baseURL": "http://x"}, "env": ["CODESMITH_TEST_KEY_A_UNSET", "CODESMITH_TEST_KEY_B"]}}})).unwrap();
        assert_eq!(
            c.resolve_model("p/m").unwrap().provider.api_key.as_deref(),
            Some("kb")
        );
    }

    // --- Ported from OpenCode's config tests (`oc_config_*`). ---

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().canonicalize().unwrap();
        (d, p)
    }

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn sources(global: &Path) -> Sources {
        Sources {
            global_dir: global.to_path_buf(),
            file: None,
            content: None,
            project: true,
        }
    }

    /// A project folder with its own `.git`, so the walk stops there.
    fn project(t: &Path) -> PathBuf {
        let p = t.join("project");
        std::fs::create_dir_all(p.join(".git")).unwrap();
        p
    }

    #[test]
    fn oc_config_loads_explicit_file_and_content_overrides_in_priority_order() {
        let (_d, t) = tmp();
        let (global, project) = (t.join("global"), project(&t));
        let explicit = t.join("custom.json");
        write(&global.join("codesmith.json"), r#"{"shell": "global"}"#);
        write(&explicit, r#"{"shell": "explicit"}"#);
        write(&project.join("codesmith.json"), r#"{"shell": "project"}"#);
        let mut src = sources(&global);
        src.file = Some(explicit.clone());
        let l = load_from(&project, &src).unwrap();
        assert_eq!(
            l.files,
            vec![
                global.join("codesmith.json"),
                explicit,
                project.join("codesmith.json")
            ]
        );
        assert_eq!(l.config.shell.as_deref(), Some("project"));
        src.content = Some(r#"{"shell": "content"}"#.into());
        let l = load_from(&project, &src).unwrap();
        assert_eq!(l.config.shell.as_deref(), Some("content"));
    }

    #[test]
    fn oc_config_skips_project_configuration_when_project_discovery_is_disabled() {
        let (_d, t) = tmp();
        let (global, project) = (t.join("global"), project(&t));
        write(&global.join("codesmith.json"), r#"{"shell": "global"}"#);
        write(&project.join("codesmith.json"), r#"{"shell": "project"}"#);
        let mut src = sources(&global);
        src.project = false;
        let l = load_from(&project, &src).unwrap();
        assert_eq!(l.config.shell.as_deref(), Some("global"));
        assert_eq!(l.files, vec![global.join("codesmith.json")]);
    }

    #[test]
    fn oc_config_returns_the_latest_defined_scalar() {
        let mut v = json!({});
        for doc in [
            json!({"model": "openrouter/openai/gpt-5"}),
            json!({}),
            json!({"model": "openrouter/openai/gpt-5.5"}),
        ] {
            merge(&mut v, doc);
        }
        let c: Config = serde_json::from_value(v).unwrap();
        assert_eq!(c.model.as_deref(), Some("openrouter/openai/gpt-5.5"));
        assert_eq!(c.default_agent, None);
    }

    #[test]
    fn oc_config_returns_an_empty_configuration_when_no_files_exist() {
        let (_d, t) = tmp();
        let project = project(&t);
        let l = load_from(&project, &sources(&t.join("global"))).unwrap();
        assert!(l.files.is_empty());
        assert!(l.config.model.is_none() && l.config.providers.is_empty());
        assert_eq!(l.root, project);
    }

    #[test]
    fn oc_config_loads_json_and_jsonc_files_from_lowest_to_highest_priority() {
        let (_d, t) = tmp();
        let project = project(&t);
        let provider = r#"{"settings": {"baseURL": "http://x"}}"#;
        write(
            &project.join("codesmith.json"),
            &format!(r#"{{"$schema": "base", "providers": {{"base": {provider}}}}}"#),
        );
        write(
            &project.join("codesmith.jsonc"),
            &format!(
                "{{\n  // Later files override scalar fields while retaining providers.\n  \"$schema\": \"last\",\n  \"providers\": {{ \"last\": {provider} }},\n}}"
            ),
        );
        let l = load_from(&project, &sources(&t.join("global"))).unwrap();
        assert_eq!(
            l.files,
            vec![
                project.join("codesmith.json"),
                project.join("codesmith.jsonc")
            ]
        );
        assert_eq!(l.config.schema.as_deref(), Some("last"));
        let ids: Vec<&str> = l.config.providers.keys().map(String::as_str).collect();
        assert_eq!(ids, ["base", "last"]);
    }

    #[test]
    fn oc_config_substitutes_environment_variables_and_relative_file_contents() {
        let (_d, t) = tmp();
        std::env::set_var("CODESMITH_OC_TEST_FLAG", "false");
        std::env::set_var("CODESMITH_OC_TEST_TOKEN", r#"C:\Users\new\"secret""#);
        std::env::remove_var("CODESMITH_OC_TEST_MISSING");
        write(&t.join("token.txt"), "file\n\"token\"\n");
        let text = r#"{
            // Ignored reference: {file:.}
            "username": "user-{env:CODESMITH_OC_TEST_MISSING}",
            "snapshots": {env:CODESMITH_OC_TEST_FLAG},
            "headers": {
                "Authorization": "Bearer {env:CODESMITH_OC_TEST_TOKEN}",
                "X-Token": "{file:token.txt}",
                "X-Missing": "Bearer {file:~/.codesmith-test-missing/key}",
                "X-Folder": "{file:.}"
            }
        }"#;
        let v = parse(text, &t).unwrap();
        assert_eq!(v["username"], "user-");
        assert_eq!(v["snapshots"], false);
        assert_eq!(
            v["headers"],
            json!({
                "Authorization": r#"Bearer C:\Users\new\"secret""#,
                "X-Token": "file\n\"token\"",
                "X-Missing": "Bearer ",
                "X-Folder": ""
            })
        );
    }

    #[test]
    fn oc_config_keeps_env_values_unescaped_when_the_config_already_parses() {
        std::env::set_var("CODESMITH_OC_TEST_QUOTED", r#"a\"b"#);
        std::env::set_var("CODESMITH_OC_TEST_FRAGMENT", r#"{"k":"v"}"#);
        let v = parse(
            r#"{ "quoted": "{env:CODESMITH_OC_TEST_QUOTED}", "fragment": {env:CODESMITH_OC_TEST_FRAGMENT} }"#,
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(v, json!({"quoted": "a\"b", "fragment": {"k": "v"}}));
    }

    #[test]
    fn oc_config_leaves_file_references_in_line_comments_alone() {
        let (_d, t) = tmp();
        write(&t.join("x.txt"), "x");
        assert_eq!(
            substitute("// {file:x.txt}\n\"{file:x.txt}\"", &t),
            "// {file:x.txt}\n\"x\""
        );
    }

    #[test]
    fn oc_config_does_not_load_legacy_or_opencode_config_files() {
        let (_d, t) = tmp();
        let project = project(&t);
        write(&project.join("config.json"), r#"{"$schema": "legacy"}"#);
        write(&project.join("opencode.json"), r#"{"$schema": "opencode"}"#);
        write(
            &project.join(".opencode/opencode.json"),
            r#"{"$schema": "opencode"}"#,
        );
        let l = load_from(&project, &sources(&t.join("global"))).unwrap();
        assert!(l.files.is_empty());
        assert_eq!(l.config.schema, None);
    }

    #[test]
    fn oc_config_accepts_schema_metadata_without_writing_it_into_config_files() {
        let (_d, t) = tmp();
        let project = project(&t);
        let file = project.join("codesmith.json");
        let contents = r#"{"shell":"/bin/zsh","providers":{"local":{}}}"#;
        write(&file, contents);
        let l = load_from(&project, &sources(&t.join("global"))).unwrap();
        assert_eq!(l.config.schema, None);
        assert_eq!(l.config.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), contents);
    }

    #[test]
    fn oc_config_loads_supported_scalar_and_resource_configuration() {
        let c: Config = serde_json::from_value(json!({
            "shell": "/bin/bash",
            "model": "anthropic/claude",
            "default_agent": "reviewer",
            "permissions": [
                {"action": "shell", "resource": "*", "effect": "ask"},
                {"action": "shell", "resource": "git status", "effect": "allow"}
            ],
            "agents": {"reviewer": {
                "model": "openrouter/openai/gpt-5",
                "description": "Review changes for correctness",
                "system": "Find regressions.",
                "mode": "subagent",
                "steps": 12,
                "disabled": false,
                "permissions": [{"action": "edit", "resource": "*", "effect": "deny"}]
            }},
            "snapshots": false,
            "tool_output": {"max_lines": 1000, "max_bytes": 32768},
            "compaction": {"auto": true, "keep": {"tokens": 2000}, "buffer": 10000},
            "skills": ["./skills", "~/shared-skills"],
            "instructions": ["CONTRIBUTING.md", ".cursor/rules/*.md"],
            "commands": {}
        }))
        .unwrap();
        assert_eq!(c.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(c.model.as_deref(), Some("anthropic/claude"));
        assert_eq!(c.default_agent.as_deref(), Some("reviewer"));
        let rules: Vec<(String, String)> = c
            .permissions
            .iter()
            .map(|r| (r.action.clone(), r.resource.clone()))
            .collect();
        assert_eq!(
            rules,
            [("shell", "*"), ("shell", "git status")].map(|(a, r)| (a.to_string(), r.to_string()))
        );
        let reviewer = &c.agents["reviewer"];
        assert_eq!(reviewer.model.as_deref(), Some("openrouter/openai/gpt-5"));
        assert_eq!(reviewer.system.as_deref(), Some("Find regressions."));
        assert_eq!(reviewer.mode.as_deref(), Some("subagent"));
        assert_eq!(reviewer.steps, Some(12));
        assert!(!reviewer.disabled);
        assert_eq!(reviewer.permissions.len(), 1);
        assert_eq!(c.snapshots, Some(false), "an explicit false is kept");
        assert_eq!(
            (c.tool_output.max_lines, c.tool_output.max_bytes),
            (1000, 32768)
        );
        assert!(c.compaction.auto());
        assert_eq!(c.compaction.keep(), 2000);
        assert_eq!(c.compaction.ceiling(100_000), 90_000);
        assert_eq!(c.skills, ["./skills", "~/shared-skills"]);
        assert_eq!(c.instructions, ["CONTRIBUTING.md", ".cursor/rules/*.md"]);
    }

    /// Deliberate difference: OpenCode skips a file that does not parse.
    #[test]
    fn oc_config_an_invalid_file_is_an_error_naming_it() {
        let (_d, t) = tmp();
        let project = project(&t);
        write(&project.join("codesmith.json"), r#"{"$schema": "base"}"#);
        write(&project.join("codesmith.jsonc"), "{ invalid");
        let e = load_from(&project, &sources(&t.join("global"))).unwrap_err();
        assert!(format!("{e:#}").contains("codesmith.jsonc"), "{e:#}");
    }

    #[test]
    fn oc_config_loads_global_and_ancestor_configuration_in_order() {
        let (_d, t) = tmp();
        let global = t.join("global");
        let root = t.join("repo");
        let parent = root.join("packages");
        let directory = parent.join("app");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let files = [
            (t.join("codesmith.json"), "outside"),
            (global.join("codesmith.json"), "global"),
            (root.join("codesmith.json"), "root"),
            (parent.join("codesmith.jsonc"), "parent"),
            (directory.join("codesmith.json"), "directory"),
            (root.join(".codesmith/codesmith.json"), "root-dot"),
            (
                directory.join(".codesmith/codesmith.jsonc"),
                "directory-dot",
            ),
        ];
        for (p, schema) in &files {
            write(
                p,
                &format!(r#"{{"$schema": "{schema}", "instructions": ["{schema}"]}}"#),
            );
        }
        let l = load_from(&directory, &sources(&global)).unwrap();
        // OpenCode also loads "outside" (above the project root) after "global".
        assert_eq!(
            l.config.instructions,
            [
                "global",
                "root",
                "parent",
                "directory",
                "root-dot",
                "directory-dot"
            ]
        );
        assert_eq!(l.config.schema.as_deref(), Some("directory-dot"));
        assert_eq!(l.files.len(), 6);
    }

    #[test]
    fn oc_config_loads_the_global_config_once_when_the_project_walk_reaches_it() {
        let (_d, t) = tmp();
        let global = t.join("global");
        std::fs::create_dir_all(global.join(".git")).unwrap();
        write(
            &global.join("codesmith.json"),
            r#"{"permissions": [{"action": "shell", "resource": "*", "effect": "ask"}]}"#,
        );
        let l = load_from(&global, &sources(&global)).unwrap();
        assert_eq!(l.files, vec![global.join("codesmith.json")]);
        assert_eq!(
            l.config.permissions.len(),
            1,
            "rules are not appended twice"
        );
    }

    #[test]
    fn oc_config_normalizes_short_model_selections() {
        let m = Config::default()
            .resolve_model("openrouter/openai/gpt-5")
            .unwrap();
        assert_eq!(
            (m.provider_id.as_str(), m.model_id.as_str()),
            ("openrouter", "openai/gpt-5")
        );
        assert!(Config::default().resolve_model("gpt-5").is_err());
    }

    #[test]
    fn oc_config_compaction_buffer_shares_and_invalid_values() {
        use crate::compaction::Buffer;
        let c: Config = serde_json::from_value(json!({"compaction": {"buffer": "12.5%"}})).unwrap();
        assert_eq!(c.compaction.buffer, Some(Buffer::Share("12.5%".into())));
        assert_eq!(c.compaction.ceiling(100_000), 87_500);
        for bad in ["0%", "100%", "150%", "lots"] {
            assert_eq!(Buffer::Share(bad.into()).tokens(100_000), None, "{bad}");
        }
        let c: Config = serde_json::from_value(json!({"compaction": {"auto": false}})).unwrap();
        assert!(!c.compaction.auto());
        assert_eq!(c.compaction.keep(), crate::compaction::KEEP_TOKENS);
    }
}
