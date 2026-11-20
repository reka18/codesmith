//! `codesmith.json` / `codesmith.jsonc` discovery, merging and parsing.
//!
//! Lowest to highest precedence:
//!   1. `<config dir>/codesmith.json[c]` (global)
//!   2. the file named by `$CODESMITH_CONFIG`
//!   3. from the project root down to the working directory, each folder's
//!      `codesmith.json[c]` and `.codesmith/codesmith.json[c]`
//!      (skipped when `$CODESMITH_DISABLE_PROJECT_CONFIG` is set)
//!   4. inline JSON in `$CODESMITH_CONFIG_CONTENT`
//!
//! Objects merge key by key; `permissions` and `instructions` lists append,
//! so a later file's rules come after (and so override) earlier ones.

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
    /// Where `--worktree` creates worktrees and what it runs in them.
    pub worktree: crate::worktree::WorktreeConfig,
    /// Code review settings for `/review` and `/calibrate`.
    pub review: crate::review::ReviewConfig,
    /// Model Context Protocol servers whose tools the agent can use.
    pub mcp: crate::mcp::McpConfig,
    /// Web search through a self-hosted SearXNG instance.
    pub websearch: crate::tool::websearch::WebSearchConfig,
    /// Commands defined inline: name → template and options.
    pub commands: BTreeMap<String, crate::command::Command>,
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
        let (p, m) = spec.split_once('/')?;
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

/// Config file candidates in precedence order (lowest first).
pub fn candidates(cwd: &Path, root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = NAMES.iter().map(|n| paths::config_dir().join(n)).collect();
    if let Some(p) = std::env::var_os("CODESMITH_CONFIG") {
        out.push(PathBuf::from(p));
    }
    if std::env::var_os("CODESMITH_DISABLE_PROJECT_CONFIG").is_none() {
        let mut dirs: Vec<&Path> = cwd
            .ancestors()
            .take_while(|d| d.starts_with(root))
            .collect();
        dirs.reverse();
        for d in dirs {
            for n in NAMES {
                out.push(d.join(n));
            }
            for n in NAMES {
                out.push(d.join(".codesmith").join(n));
            }
        }
    }
    out
}

pub fn load(cwd: &Path) -> Result<Loaded> {
    let root = project_root(cwd);
    let mut merged = Value::Object(Map::new());
    let mut files = Vec::new();
    for path in candidates(cwd, &root) {
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let v = parse(&text, path.parent().unwrap_or(Path::new(".")))
            .with_context(|| format!("in {}", path.display()))?;
        merge(&mut merged, v);
        files.push(path);
    }
    if let Ok(text) = std::env::var("CODESMITH_CONFIG_CONTENT") {
        merge(
            &mut merged,
            parse(&text, cwd).context("in $CODESMITH_CONFIG_CONTENT")?,
        );
    }
    let config: Config = serde_json::from_value(merged).context("invalid codesmith config")?;
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
/// contents (relative to `base`, `~/` expanded). Missing values become empty.
/// Values are JSON-string-escaped, since they appear inside JSON strings.
pub fn substitute(text: &str, base: &Path) -> String {
    let re = regex::Regex::new(r"\{(env|file):([^}]+)\}").unwrap();
    re.replace_all(text, |c: &regex::Captures| {
        let value = match &c[1] {
            "env" => std::env::var(&c[2]).unwrap_or_default(),
            _ => {
                let raw = &c[2];
                let p = match raw.strip_prefix("~/") {
                    Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
                    None => base.join(raw),
                };
                std::fs::read_to_string(p)
                    .map(|s| s.trim_end().to_string())
                    .unwrap_or_default()
            }
        };
        let quoted = serde_json::to_string(&value).unwrap();
        quoted[1..quoted.len() - 1].to_string()
    })
    .into_owned()
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

/// A resolved provider and model, ready to call.
#[derive(Debug, Clone)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
    pub provider: codesmith_ai::Provider,
    pub body: Map<String, Value>,
    pub max_output: Option<u32>,
    pub context: Option<u64>,
    pub temperature: Option<f32>,
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
                models: c.models,
            }),
        }
    }

    /// Resolve `provider/model` (the first `/` splits; model ids may contain more).
    pub fn resolve_model(&self, spec: &str) -> Result<ModelRef> {
        let (pid, mid) = spec
            .split_once('/')
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
        let mut body = p.body.clone();
        body.extend(m.body.clone());
        Ok(ModelRef {
            provider_id: pid.to_string(),
            model_id: m.id.clone().unwrap_or_else(|| mid.to_string()),
            provider: codesmith_ai::Provider {
                protocol,
                base_url,
                api_key,
                headers: p.headers.into_iter().collect(),
            },
            body,
            max_output: m.limit.output,
            context: m.limit.context,
            temperature: m.temperature,
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
    fn api_key_from_env_list() {
        std::env::set_var("CODESMITH_TEST_KEY_B", "kb");
        let c: Config = serde_json::from_value(json!({"providers": {"p": {"settings": {"baseURL": "http://x"}, "env": ["CODESMITH_TEST_KEY_A_UNSET", "CODESMITH_TEST_KEY_B"]}}})).unwrap();
        assert_eq!(
            c.resolve_model("p/m").unwrap().provider.api_key.as_deref(),
            Some("kb")
        );
    }
}
