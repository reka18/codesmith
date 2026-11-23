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
//! `$CODESMITH_DISABLE_GLOBAL_CONFIG` skips the global config folder, and
//! the home folder's `.claude` and `.agents` folders, however they are
//! reached (see [`global_disabled`]).
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
    /// Default model as `provider/model[#variant]`, or
    /// `{"providerID", "model", "variant"}` as OpenCode writes it.
    #[serde(deserialize_with = "model_selection")]
    pub model: Option<String>,
    /// The model that writes session titles, as `provider/model[#variant]`
    /// (OpenCode's `small_model`); the session's own model when unset.
    #[serde(deserialize_with = "model_selection")]
    pub small_model: Option<String>,
    /// Providers never to use (OpenCode's `disabled_providers`): their
    /// models are not listed or picked, a model naming one does not
    /// resolve, and a default `model` on one is passed over for the newest
    /// available model ([`Config::default_model`]). Every config file's
    /// list counts.
    pub disabled_providers: Vec<String>,
    /// `false` turns generated session titles off: a session is named after
    /// its first message, with no model request.
    pub title: Option<bool>,
    pub default_agent: Option<String>,
    /// Shell for the `shell` tool; defaults to `$SHELL`, then `/bin/sh`.
    /// Written as a path, or as `{"path": ..., "env": {...}}` where `env`
    /// is [`ShellEnv`] (which [`load_from`] takes out into `shell_env`).
    #[serde(deserialize_with = "shell_path")]
    pub shell: Option<String>,
    pub permissions: Vec<Rule>,
    /// Policy statements (OpenCode's `policies`): `tool.use` ones deny
    /// permissions as `action:resource` and never grant; see
    /// [`crate::permission::Policy`]. A later file's statements go before
    /// an earlier file's, so the earlier file has the final say.
    pub policies: Vec<crate::permission::Policy>,
    /// Rules applied after every agent's own rules, so they beat both the
    /// built-in and the configured agent rules. Set by the process, never
    /// read from config files: unattended automation runs put their denies
    /// and `automation.permissions` opt-ins here.
    #[serde(skip)]
    pub overrides: Vec<Rule>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub agents: BTreeMap<String, AgentConfig>,
    /// Extra instruction files: paths or globs, `~/` for the home folder; a
    /// relative one is looked for from the working directory up to the
    /// project root.
    pub instructions: Vec<String>,
    pub tool_output: ToolOutputConfig,
    /// Snapshot files at each turn so turns can be undone (default true).
    pub snapshots: Option<bool>,
    /// Extra folders containing `<id>/SKILL.md` skills.
    pub skills: Vec<String>,
    /// Offer the skills shipped with CodeSmith (`codesmith`, for questions
    /// about CodeSmith itself, and `report`, for filing an issue). Off by
    /// default, so sessions' prompts stay as they were.
    pub builtin_skills: bool,
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
    /// OpenCode's `full_codemode`: glob, grep and webfetch are hidden, and
    /// GPT and Claude models call read, write, edit, patch, shell and
    /// websearch from inside `execute` (see
    /// [`crate::codemode::placement`]). Off by default.
    pub full_codemode: Option<bool>,
    /// How deep subagents may nest: 1 (the default) lets the main session
    /// start subagents but not a subagent start its own.
    pub subagent_depth: Option<u32>,
    /// Compiled-in plugins: `false` turns one off, an object is its options.
    pub plugins: BTreeMap<String, Value>,
    /// Code review settings for `/review` and `/calibrate`.
    pub review: crate::review::ReviewConfig,
    /// Model Context Protocol servers whose tools the agent can use.
    pub mcp: crate::mcp::McpConfig,
    /// OpenAPI specs whose operations Code Mode offers as
    /// `tools.<name>.<operation>(input)` (see [`crate::codemode::openapi`]).
    /// An entry that does not parse is kept, to be reported as a notice
    /// rather than failing the whole config.
    #[serde(deserialize_with = "crate::codemode::openapi::lenient")]
    pub openapi: BTreeMap<String, crate::codemode::openapi::ApiConfig>,
    /// Web search through a self-hosted SearXNG instance.
    pub websearch: crate::tool::websearch::WebSearchConfig,
    /// Commands defined inline: name → template and options.
    pub commands: BTreeMap<String, crate::command::Command>,
    /// When to compact and how much recent history to keep verbatim.
    pub compaction: crate::compaction::Settings,
    /// Prompt-cache warming: `true`, or `{"prompt", "interval",
    /// "duration"}` (see [`crate::warming`]). Off when unset.
    pub warming: Option<Value>,
    /// Experimental features (OpenCode's `experimental`): `image_generate`
    /// and `video_generate`, each `false` to turn the tool off or
    /// `{"model": "provider/model"}` to prefer a model.
    pub experimental: Experimental,
    /// Limits images are fitted to before they reach the model
    /// (`media.image`: `auto_resize`, `max_width`, `max_height`,
    /// `max_base64_bytes`).
    pub media: crate::media::MediaConfig,
    /// Where the models.dev catalog comes from: `{"fetch": true, "url":
    /// "..."}` opts in to fetching it (off by default; the bundled snapshot
    /// and the cache serve without the network).
    pub catalog: CatalogConfig,
    /// File watching: `ignore` lists paths or globs (relative to the
    /// project) to leave unwatched.
    pub watcher: crate::watch::WatcherConfig,
    /// The models.dev catalog that fills in what model config leaves out
    /// (limits, costs, modalities, variants) and adds its providers. Set by
    /// [`load`]; a config built any other way has none.
    #[serde(skip)]
    pub models_dev: Option<std::sync::Arc<crate::catalog::ModelsDev>>,
    /// The terminal UI's own settings (theme, key bindings, cursor, …),
    /// read by the UI as OpenCode reads its TUI config.
    pub tui: Value,

    // --- References ---
    /// Other directories the agent may consult, by name: a local path, or a
    /// Git repository CodeSmith keeps a checkout of (see
    /// [`crate::reference`]). Those with a description are listed to the model.
    pub references: BTreeMap<String, crate::reference::Entry>,

    /// `providers.<id>.swap` settings [`load_from`] dropped because they
    /// were not in the global config file, as `<id> in <source>`.
    #[serde(skip)]
    pub swap_ignored: Vec<String>,

    /// How many Claude Code processes the Claude Agent provider runs and
    /// which it starts ahead of need (`claude_agent`). Read only from the
    /// global config file: a project cannot change process caps.
    pub claude_agent: ClaudeAgentConfig,
    /// The sources whose `claude_agent` [`load_from`] dropped (anything but
    /// the global config file).
    #[serde(skip)]
    pub claude_agent_ignored: Vec<String>,

    /// What the programs CodeSmith starts lose and keep of its
    /// environment (see [`crate::child_env`]), from every source's
    /// `shell.env`.
    #[serde(skip)]
    pub shell_env: ShellEnv,

    /// What [`load_from`] did about the project's config: what it asks
    /// for, whether it is trusted, and what was ignored (see
    /// [`crate::trust`]).
    #[serde(skip)]
    pub trust: crate::trust::Outcome,
}

/// `claude_agent` in the global codesmith.json: the Claude Agent
/// provider's process pool (see [`codesmith_ai::claude_agent::Pool`]).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClaudeAgentConfig {
    /// Start a session's `claude` as its turn begins (default off).
    pub prewarm: Option<bool>,
    /// Never-used processes kept ready for new sessions (default 0).
    pub spare: Option<usize>,
    /// The most `claude` processes at once (default 4, at most 32).
    pub max_processes: Option<usize>,
}

impl ClaudeAgentConfig {
    pub fn pool(&self) -> codesmith_ai::claude_agent::Pool {
        let d = codesmith_ai::claude_agent::Pool::default();
        codesmith_ai::claude_agent::Pool {
            prewarm: self.prewarm.unwrap_or(d.prewarm),
            spare: self.spare.unwrap_or(d.spare),
            max_processes: self.max_processes.unwrap_or(d.max_processes),
        }
        .clamped()
    }
}

/// Drop `claude_agent` from a source other than the global config file.
pub fn strip_claude_agent(v: &mut Value, source: &str, ignored: &mut Vec<String>) {
    if v.as_object_mut()
        .and_then(|o| o.remove("claude_agent"))
        .is_some()
    {
        ignored.push(source.to_string());
    }
}

/// `shell.env` in codesmith.json (see [`crate::child_env`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellEnv {
    /// Variables passed through on purpose. Read only from the global
    /// config file.
    pub pass: Vec<String>,
    /// Further variables to remove (any source may add to it).
    pub remove: Vec<String>,
    /// The `{env:NAME}` names the config files use for credentials.
    pub credentials: Vec<String>,
    /// `shell.env.pass` lists [`load_from`] dropped because they were not
    /// in the global config file, as their sources.
    pub pass_ignored: Vec<String>,
}

/// `shell`: a path, or an object with an optional `path`.
fn shell_path<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        Value::Object(o) => o.get("path").and_then(Value::as_str).map(str::to_string),
        _ => None,
    })
}

/// Take `shell.env` out of a config document into `acc`: `pass` only from
/// the global config file (else noted as ignored), `remove` from any.
/// The `shell` key is left as its path alone, so a later source's plain
/// path merges over it.
pub fn take_shell_env(v: &mut Value, source: &str, global: bool, acc: &mut ShellEnv) {
    let Some(shell) = v.get_mut("shell") else {
        return;
    };
    let Some(o) = shell.as_object_mut() else {
        return;
    };
    let names = |x: Option<&Value>| -> Vec<String> {
        x.and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    if let Some(env) = o.remove("env") {
        let pass = names(env.get("pass"));
        if global {
            acc.pass.extend(pass);
        } else if env.get("pass").is_some() {
            acc.pass_ignored.push(source.to_string());
        }
        acc.remove.extend(names(env.get("remove")));
    }
    *shell = match o.get("path").and_then(Value::as_str) {
        Some(p) => Value::String(p.to_string()),
        None => Value::Null,
    };
    if shell.is_null() {
        if let Some(m) = v.as_object_mut() {
            m.remove("shell");
        }
    }
}

/// The `experimental` key.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Experimental {
    /// `false` turns `experimental_image_generate` off; `{"model": …}`
    /// names the image model it prefers.
    pub image_generate: Option<Value>,
    /// `false` turns `experimental_video_generate` off; `{"model": …}`
    /// names the video model it prefers.
    pub video_generate: Option<Value>,
}

/// The `catalog` key.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CatalogConfig {
    /// Fetch the catalog (in the background, every five minutes). Off
    /// unless set, or a URL is given.
    pub fetch: Option<bool>,
    /// The catalog server; setting it turns fetching on unless `fetch` is
    /// `false`. Default `https://models.dev`.
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub name: Option<String>,
    /// Whether the provider is a local server: one on this machine or
    /// network that one request at a time keeps busy. A local server's
    /// side requests (the session title, background compaction summaries)
    /// never run beside a turn's request, and a local server with
    /// `settings.discovery` has its models listed without being asked.
    /// Unset, [`ProviderConfig::is_local`] and
    /// [`ProviderConfig::probe_unprompted`] decide from the base URL.
    pub local: Option<bool>,
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
    /// Prompt caching for every model of this provider (see
    /// [`ModelConfig::cache`]).
    pub cache: Option<Value>,
    /// How every model of this provider compacts (see
    /// [`ModelConfig::compaction`]).
    pub compaction: Option<Value>,
    /// Overrides of detected OpenAI Chat behaviour for every model of this
    /// provider (see [`ModelConfig::compatibility`]).
    pub compatibility: Map<String, Value>,
    pub models: BTreeMap<String, ModelConfig>,
    /// Commands that list the models the server can load and load one
    /// (`{"list": [...], "use": [..., "{model}", ...], "timeout": 600}`);
    /// see [`crate::providers::swap`]. Read only from the global config
    /// file: [`load_from`] drops it from every other source.
    pub swap: Option<crate::providers::swap::SwapConfig>,
}

impl ProviderConfig {
    /// Whether this provider is a local, single-instance server, whose side
    /// requests wait for the turn: `local`, else whether `base_url` is on
    /// this machine or a private network ([`is_local_url`]).
    pub fn is_local(&self, base_url: &str) -> bool {
        self.local.unwrap_or_else(|| is_local_url(base_url))
    }

    /// Whether this provider's server may be asked for its models without
    /// the user asking (`codesmith models`, the model picker): `local`,
    /// else whether `base_url` is on this machine ([`is_loopback_url`]).
    pub fn probe_unprompted(&self, base_url: &str) -> bool {
        self.local.unwrap_or_else(|| is_loopback_url(base_url))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProviderSettings {
    #[serde(alias = "baseURL", alias = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(alias = "apiKey")]
    pub api_key: Option<String>,
    /// OpenCode's spelling of the provider's `cache`.
    pub cache: Option<Value>,
    /// Query parameters added to every request URL.
    #[serde(alias = "queryParams")]
    pub query_params: BTreeMap<String, String>,
    /// The header that carries the key, sent as-is instead of the
    /// protocol's own (`Authorization: Bearer`, `x-api-key`,
    /// `x-goog-api-key`); a configured `Authorization` header is then not
    /// sent. Azure's default is `api-key`.
    #[serde(alias = "apiKeyHeader")]
    pub api_key_header: Option<String>,
    /// Request body compression: `gzip`, `zstd` or `none` (OpenCode's
    /// `encoding`).
    pub encoding: Option<String>,
    /// Azure: the resource name, which gives the base URL when `baseURL` is
    /// not set.
    #[serde(alias = "resourceName")]
    pub resource_name: Option<String>,
    /// Azure: the `api-version` query parameter (default `v1`).
    #[serde(alias = "apiVersion")]
    pub api_version: Option<String>,
    /// Azure: `<base>/deployments/<model>` URLs instead of the v1 API.
    #[serde(alias = "useDeploymentBasedUrls")]
    pub use_deployment_based_urls: bool,
    /// How session steps reach an OpenAI Responses provider: `websocket`
    /// (one socket per session, continuing each response incrementally) or
    /// `http`. OpenAI, xAI and v1 Azure endpoints default to `websocket`,
    /// every other provider to `http`.
    pub transport: Option<String>,
    /// How the provider's models are discovered from the server itself:
    /// `lmstudio`, `ollama`, `vllm` or `openai` (`GET /v1/models`). The
    /// built-in local providers have their own; see
    /// [`crate::providers::local`].
    pub discovery: Option<String>,
    /// Settings of the providers in [`crate::providers`] (`region`,
    /// `profile`, `endpoint`, `auth`, `project`, `location`, …).
    #[serde(flatten)]
    pub plugin: crate::providers::Settings,
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
    /// Prompt caching, on by default: `false` (or `"none"`) turns it off,
    /// `{"ttl": "1h"}` sets the cache lifetime, and `tools`, `system` and
    /// `messages` choose breakpoints exactly. Also read from `options`
    /// (`settings`), OpenCode's place for it. Variants cannot change it.
    pub cache: Option<Value>,
    /// How the model's sessions compact: `{"type": "summary"}` (the
    /// default) or `{"type": "native"}`, the provider's own compaction
    /// (OpenAI Responses), optionally with `"mechanism": "trigger"` or
    /// `"endpoint"`. Overrides the provider's.
    pub compaction: Option<Value>,
    /// Overrides of what OpenAI Chat detects from the endpoint and model id
    /// (OpenCode's `compatibility`): `maxTokensField`, `streamUsage`,
    /// `requireReasoning`, `requireAssistantAfterTool`. Merged over the
    /// provider's.
    pub compatibility: Map<String, Value>,
    /// What the model can do (OpenCode's `capabilities`), merged field by
    /// field over the catalog's: `input`, `output`, `tools`,
    /// `temperature`, and `reasoning` (`{"supported": true, "effort":
    /// [...]}`, `"toggle"`, `"budget"`), which regenerates the reasoning
    /// variants.
    pub capabilities: Option<CapabilitiesConfig>,
    /// OpenCode's older `interleaved`: the reasoning field name (or
    /// `{"field": name}`), the same as `compatibility.reasoningField`.
    pub interleaved: Option<Value>,
    /// The model type: `chat` (the default; the only type offered for
    /// sessions), `decision`, `image` or `video`. Unset keeps the catalog's.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CapabilitiesConfig {
    pub tools: Option<bool>,
    /// The freeform tool formats the model accepts (`text`, `regex`,
    /// `lark`), in place of the catalog's.
    pub freeform: Option<Vec<String>>,
    pub input: Option<Vec<String>>,
    pub output: Option<Vec<String>>,
    pub temperature: Option<bool>,
    pub reasoning: Option<ReasoningConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReasoningConfig {
    pub supported: bool,
    pub toggle: Option<bool>,
    pub effort: Option<Vec<String>>,
    pub budget: Option<BudgetConfig>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

impl ReasoningConfig {
    fn to_catalog(&self) -> crate::catalog::Reasoning {
        crate::catalog::Reasoning {
            supported: self.supported,
            toggle: self.toggle,
            effort: self.effort.clone(),
            budget: self.budget.map(|b| crate::catalog::Budget {
                min: b.min,
                max: b.max,
            }),
        }
    }
}

/// The reasoning field an `interleaved` value names: a string, or an
/// object's `field` (OpenCode's `Model.compatibility`); `true` and `false`
/// name none.
pub fn interleaved_field(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("field")?.as_str().map(str::to_string),
        _ => None,
    }
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
        if let Some(i) = self.capabilities.as_ref().and_then(|c| c.input.clone()) {
            return Some(i);
        }
        if let Some(m) = &self.modalities {
            return Some(m.input.clone());
        }
        match self.attachment {
            Some(false) => Some(vec!["text".into()]),
            _ => None,
        }
    }
}

/// Input modalities for models whose inputs are known, when the config says
/// nothing: DeepSeek's chat and reasoning models take text only, and
/// Gemini's chat models take text, images, audio, video and PDFs (as
/// models.dev lists them).
fn known_input(model: &str) -> Option<Vec<String>> {
    let m = model.to_ascii_lowercase();
    if m.contains("deepseek") && !m.contains("vl") {
        return Some(vec!["text".into()]);
    }
    let gemini = m.rsplit('/').next().unwrap_or(&m).starts_with("gemini-")
        && !["embedding", "tts", "image", "live"]
            .iter()
            .any(|x| m.contains(x));
    gemini.then(|| {
        ["text", "image", "audio", "video", "pdf"]
            .map(String::from)
            .to_vec()
    })
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
    /// What one step of `provider/model` with usage `u` costs at the
    /// catalog's context tier its prompt passes (OpenCode prices each step
    /// so: `calculateCost`). `None` when no tier applies, so the step is
    /// priced with the rest at the default price ([`Config::cost`]): below
    /// every tier, without tiers, or with a configured price, a ChatGPT
    /// plan or a Claude sign-in.
    pub fn tier_price(&self, spec: &str, u: &codesmith_ai::Usage) -> Option<f64> {
        let (p, m) = split_variant(spec).0.split_once('/')?;
        if (p == crate::providers::chatgpt::ID && self.chatgpt())
            || p == crate::providers::claude_agent::ID
        {
            return None;
        }
        let configured = self.provider(p).and_then(|c| c.models.get(m).cloned());
        if configured.as_ref().is_some_and(|c| c.cost.is_some()) {
            return None;
        }
        let id = configured.and_then(|c| c.id);
        let catalog = self.models_dev.as_ref()?.catalog();
        let cm = id
            .and_then(|id| catalog.model(p, &id))
            .or_else(|| catalog.model(p, m))?;
        let tier = crate::stats::context_tier(&cm.cost, u)?;
        Some(crate::stats::catalog_price(u, tier))
    }

    /// The price of `provider/model`: the configured one, else the
    /// catalog's default price.
    pub fn cost(&self, spec: &str) -> Option<Cost> {
        let (p, m) = split_variant(spec).0.split_once('/')?;
        // A ChatGPT plan pays, not per token.
        if p == crate::providers::chatgpt::ID && self.chatgpt() {
            return Some(Cost::default());
        }
        let configured = self.provider(p).and_then(|c| c.models.get(m).cloned());
        if let Some(c) = configured.as_ref().and_then(|c| c.cost) {
            return Some(c);
        }
        if p == crate::providers::claude_agent::ID {
            let saved = crate::providers::claude_agent::catalog();
            let catalog = self.models_dev.as_ref().map(|c| c.catalog());
            return match catalog.as_ref() {
                Some(c) => crate::providers::claude_agent::cost(&saved, m, c),
                None => saved
                    .model(m)
                    .filter(|_| saved.price_source.is_none())
                    .map(|_| Cost::default()),
            };
        }
        let id = configured.and_then(|c| c.id);
        let catalog = self.models_dev.as_ref()?.catalog();
        let cm = id
            .and_then(|id| catalog.model(p, &id))
            .or_else(|| catalog.model(p, m))?;
        let c = cm.base_cost()?;
        Some(Cost {
            input: c.input,
            output: c.output,
            cache_read: c.cache.read,
            cache_write: c.cache.write,
        })
    }

    /// A provider's models of the given types (chat when empty): the
    /// catalog's and the configured ones, by id. A configured `type`
    /// replaces the catalog's.
    pub fn list_models(&self, provider: &str, types: &[&str]) -> Vec<String> {
        let types: &[&str] = if types.is_empty() { &["chat"] } else { types };
        let mut out = self.list_models_of(provider, types);
        // A ChatGPT sign-in serves only ChatGPT's models.
        if provider == crate::providers::chatgpt::ID && self.chatgpt() {
            out.retain(|m| crate::providers::chatgpt::MODELS.contains(&m.as_str()));
        }
        out
    }

    /// Whether OpenAI runs on a ChatGPT sign-in (no API key overrides it).
    pub fn chatgpt(&self) -> bool {
        let keyed = self
            .provider(crate::providers::chatgpt::ID)
            .is_some_and(|p| p.settings.api_key.as_deref().is_some_and(|k| !k.is_empty()));
        crate::providers::chatgpt::signed_in(keyed)
    }

    fn list_models_of(&self, provider: &str, types: &[&str]) -> Vec<String> {
        let configured = self
            .provider(provider)
            .map(|p| p.models)
            .unwrap_or_default();
        let catalog = self.models_dev.as_ref().map(|c| c.catalog());
        let catalog_kind = |id: &str| {
            let p = catalog.as_ref()?.provider(provider)?;
            p.models.iter().find(|m| m.id == id).map(|m| m.kind.clone())
        };
        let mut out: Vec<String> = Vec::new();
        for (id, m) in &configured {
            let kind = m
                .kind
                .clone()
                .or_else(|| catalog_kind(m.id.as_deref().unwrap_or(id)))
                .or_else(|| catalog_kind(id))
                .unwrap_or_else(|| "chat".into());
            if types.contains(&kind.as_str()) {
                out.push(id.clone());
            }
        }
        let templates = catalog
            .as_ref()
            .and_then(|c| c.provider(provider))
            .map(|p| p.models.as_slice())
            .unwrap_or(&[]);
        // Claude Code's saved list (`codesmith models claude-agent`).
        if provider == crate::providers::claude_agent::ID && types.contains(&"chat") {
            for m in crate::providers::claude_agent::catalog().models {
                if !out.contains(&m.id) {
                    out.push(m.id);
                }
            }
        }
        // A kept live list stands in for the catalog's models.
        let live = crate::providers::live::models(
            provider,
            self.live_account(provider).as_deref(),
            templates,
        );
        for m in &live {
            if m.enabled
                && types.contains(&m.kind.as_str())
                && !configured.contains_key(&m.id)
                && !out.contains(&m.id)
            {
                out.push(m.id.clone());
            }
        }
        if let Some(p) = catalog
            .as_ref()
            .and_then(|c| c.provider(provider))
            .filter(|_| live.is_empty())
        {
            for m in &p.models {
                if !configured.contains_key(&m.id)
                    && types.contains(&m.kind.as_str())
                    && !out.contains(&m.id)
                {
                    out.push(m.id.clone());
                }
            }
        }
        out.sort();
        out
    }

    /// What `provider/model` can do: the catalog's capabilities (or, for a
    /// model the catalog lacks, OpenCode's defaults for a custom model:
    /// tools, text and image in, text out) with configured fields over them.
    pub fn capabilities(&self, spec: &str) -> Option<crate::catalog::Capabilities> {
        let (p, m) = split_variant(spec).0.split_once('/')?;
        let configured = self
            .provider(p)
            .and_then(|c| c.models.get(m).cloned())
            .unwrap_or_default();
        let catalog = self.models_dev.as_ref().map(|c| c.catalog());
        let cm = catalog.as_ref().and_then(|c| {
            configured
                .id
                .as_deref()
                .and_then(|id| c.model(p, id))
                .or_else(|| c.model(p, m))
        });
        let mut caps =
            cm.map(|cm| cm.capabilities.clone())
                .unwrap_or_else(|| crate::catalog::Capabilities {
                    tools: true,
                    input: vec!["text".into(), "image".into()],
                    output: vec!["text".into()],
                    ..Default::default()
                });
        if let Some(i) = configured.input() {
            caps.input = i;
        }
        if let Some(o) = configured.modalities.as_ref().map(|m| m.output.clone()) {
            if !o.is_empty() {
                caps.output = o;
            }
        }
        if let Some(c) = &configured.capabilities {
            if let Some(t) = c.tools {
                caps.tools = t;
            }
            if let Some(o) = &c.output {
                caps.output = o.clone();
            }
            if c.temperature.is_some() {
                caps.temperature = c.temperature;
            }
        }
        Some(caps)
    }

    /// Catalog providers CodeSmith can run that have a key (in one of their
    /// variables, or saved), by id: what OpenCode calls available. Only
    /// providers with a key are read.
    pub fn keyed_catalog_providers(&self) -> Vec<String> {
        let Some(c) = &self.models_dev else {
            return vec![];
        };
        let catalog = c.catalog();
        let saved = crate::auth::providers_in(&crate::paths::auth_file());
        catalog
            .ids()
            .filter(|id| {
                saved.iter().any(|s| s == id)
                    || catalog_env(id, &catalog.environment(id))
                        .iter()
                        .any(|v| std::env::var(v).is_ok_and(|k| !k.is_empty()))
            })
            .filter(|id| catalog.provider(id).and_then(catalog_provider).is_some())
            .map(str::to_string)
            .collect()
    }

    /// Catalog providers CodeSmith can run (a known protocol and a base
    /// URL), by id.
    pub fn catalog_providers(&self) -> Vec<String> {
        let Some(c) = &self.models_dev else {
            return vec![];
        };
        c.catalog()
            .providers()
            .filter(|p| catalog_provider(p).is_some())
            .map(|p| p.info.id.clone())
            .collect()
    }
}

/// `${VAR}` in a catalog URL, from the environment; `None` when a variable
/// is unset or one a catalog may not read ([`crate::catalog::url_variable`]),
/// or when the URL is not `https` (or `http` to this machine).
fn expand_url(url: &str) -> Option<String> {
    let re = regex::Regex::new(r"\$\{([A-Za-z0-9_]+)\}").unwrap();
    let mut missing = false;
    let out = re.replace_all(url, |c: &regex::Captures| {
        Some(&c[1])
            .filter(|v| crate::catalog::url_variable(v))
            .and_then(|v| std::env::var(v).ok())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                missing = true;
                String::new()
            })
    });
    (!missing && crate::catalog::secure_url(&out)).then(|| out.into_owned())
}

/// The key variables a catalog lists for provider `pid`. Built-in and
/// plugin providers keep their own; a provider that only the (untrusted)
/// catalog defines may not claim a variable a built-in provider reads, or
/// one of the cloud and code-host credential families, since its key goes
/// to whatever URL the catalog gives. Config's `env` can still name any.
fn catalog_env(pid: &str, vars: &[String]) -> Vec<String> {
    if builtin(pid).is_some() || crate::providers::builtin(pid).is_some() {
        return vars.to_vec();
    }
    const FAMILIES: [&str; 9] = [
        "AWS_",
        "AZURE_",
        "GOOGLE_",
        "GEMINI_",
        "ANTHROPIC_",
        "OPENAI_",
        "GITHUB_",
        "GH_",
        "CODESMITH_",
    ];
    let claimed = |v: &str| {
        FAMILIES
            .iter()
            .any(|f| v.to_ascii_uppercase().starts_with(f))
            || BUILTIN.iter().any(|b| b.4.contains(&v))
            || crate::providers::PROVIDERS.iter().any(|p| p.2.contains(&v))
    };
    vars.iter().filter(|v| !claimed(v)).cloned().collect()
}

/// A catalog provider as provider config, when CodeSmith speaks its
/// package's protocol and it has a usable base URL.
fn catalog_provider(p: &crate::catalog::Snapshot) -> Option<ProviderConfig> {
    let protocol = crate::catalog::protocol(&p.info.package)?;
    let url = p.info.settings.as_ref()?.base_url.as_deref()?;
    Some(ProviderConfig {
        name: Some(p.info.name.clone()),
        protocol: Some(protocol),
        env: catalog_env(&p.info.id, &p.environment),
        settings: ProviderSettings {
            base_url: Some(expand_url(url)?),
            ..Default::default()
        },
        ..Default::default()
    })
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct ModelLimit {
    pub context: Option<u64>,
    pub output: Option<u32>,
}

/// An agent in config (`agents.<id>`) or an agent file (see
/// [`crate::agent::from_markdown`]). Fields set in a later layer replace
/// earlier ones; `permissions` append and `request` maps merge key by key.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub description: Option<String>,
    /// `provider/model[#variant]`, or `{"providerID", "model", "variant"}`.
    #[serde(deserialize_with = "model_selection")]
    pub model: Option<String>,
    pub system: Option<String>,
    pub mode: Option<String>,
    /// Kept out of the subagent list and never the default agent; it can
    /// still be chosen by name.
    pub hidden: Option<bool>,
    /// A `#rrggbb` color for front ends; anything else (such as an
    /// OpenCode theme token) is ignored.
    #[serde(deserialize_with = "hex_color")]
    pub color: Option<String>,
    pub steps: Option<u32>,
    #[serde(alias = "disable")]
    pub disabled: bool,
    pub permissions: Vec<Rule>,
    /// Extra request `headers` and `body` fields for this agent's model
    /// calls (`temperature`, `top_p`, provider options), over the model's.
    pub request: AgentRequest,
}

/// `agents.<id>.request`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AgentRequest {
    pub headers: BTreeMap<String, String>,
    pub body: Map<String, Value>,
}

/// A model selection: `provider/model[#variant]` or OpenCode's structured
/// `{"providerID", "model", "variant"}`, as a spec string.
pub fn model_selection<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    use serde::de::Error;
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Object(o)) => {
            let get = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
            let (Some(p), Some(m)) = (get("providerID"), get("model")) else {
                return Err(D::Error::custom(
                    "a model selection needs \"providerID\" and \"model\"",
                ));
            };
            Ok(Some(match get("variant") {
                Some(v) => format!("{p}/{m}#{v}"),
                None => format!("{p}/{m}"),
            }))
        }
        Some(_) => Err(D::Error::custom(
            "a model is \"provider/model\" or {\"providerID\", \"model\"}",
        )),
    }
}

fn hex_color<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<Value>::deserialize(d)?
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|s| {
            s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
        }))
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
    /// Project root (nearest folder with `.git` or `.hg`, else the working
    /// directory).
    pub root: PathBuf,
}

pub const NAMES: [&str; 2] = ["codesmith.json", "codesmith.jsonc"];

/// The nearest ancestor of `dir` (inclusive) containing `.git` or a
/// Mercurial `.hg`, when it is a repository this user controls, else `dir`
/// (see [`crate::vcs::trusted_marker`]).
pub fn project_root(dir: &Path) -> PathBuf {
    crate::vcs::trusted_marker(dir)
        .map(|(d, _)| d)
        .unwrap_or_else(|| dir.to_path_buf())
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
    /// Where the answers on trusting project config are (see
    /// [`crate::trust`]).
    pub trust: crate::trust::Mode,
}

impl Sources {
    pub fn from_env() -> Sources {
        Sources {
            global_dir: paths::config_dir(),
            file: std::env::var_os("CODESMITH_CONFIG").map(PathBuf::from),
            content: std::env::var("CODESMITH_CONFIG_CONTENT").ok(),
            project: std::env::var_os("CODESMITH_DISABLE_PROJECT_CONFIG").is_none(),
            trust: crate::trust::Mode::Store(crate::trust::store_path()),
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
    layers(cwd, root, sources)
        .into_iter()
        .filter_map(|l| match l {
            Layer::File(p) => Some(p),
            Layer::Dir(_) => None,
        })
        .collect()
}

/// One source of config, lowest precedence first: a config file, or a
/// config folder whose `agents/`, `modes/` and `commands/` files are read
/// (OpenCode's directory entries).
#[derive(Debug, Clone, PartialEq)]
pub enum Layer {
    File(PathBuf),
    Dir(PathBuf),
}

/// Whether `$CODESMITH_DISABLE_GLOBAL_CONFIG` turns the global sources off
/// (OpenCode's `global: false`): the global config folder and the home
/// folder's `.claude` and `.agents` folders are then not read, even when
/// the project walk passes through them. The explicit file and inline
/// config still load.
/// Whether `$CODESMITH_DISABLE_PROJECT_CONFIG` turns project config off:
/// no project config files, instructions or nested instructions.
pub fn project_disabled() -> bool {
    std::env::var_os("CODESMITH_DISABLE_PROJECT_CONFIG").is_some()
}

pub fn global_disabled() -> bool {
    std::env::var_os("CODESMITH_DISABLE_GLOBAL_CONFIG").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Whether `dir` is a global source that [`global_disabled`] turns off:
/// the global config folder (also through a symlink) or the home folder.
pub fn global_source(dir: &Path, global_dir: &Path, home: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let d = real(dir);
    d == real(global_dir) || d == real(home)
}

/// Every layer: the global files and then the global folder, the explicit
/// file, every folder's `codesmith.json[c]`, then each folder's
/// `.codesmith/codesmith.json[c]` followed by its `.codesmith/` folder.
pub fn layers(cwd: &Path, root: &Path, sources: &Sources) -> Vec<Layer> {
    split_layers(cwd, root, sources).0
}

/// [`layers`], and the index of the first project layer: those before it
/// (the global config and `$CODESMITH_CONFIG`) are the person's own.
fn split_layers(cwd: &Path, root: &Path, sources: &Sources) -> (Vec<Layer>, usize) {
    let global = !global_disabled();
    let mut out: Vec<Layer> = Vec::new();
    if global {
        out.extend(
            NAMES
                .iter()
                .map(|n| Layer::File(sources.global_dir.join(n))),
        );
        out.push(Layer::Dir(sources.global_dir.clone()));
    }
    let home = dirs::home_dir().unwrap_or_default();
    // The project walk may reach the global folder (a project inside it):
    // it is still global.
    let walked = |d: &Path| global || !global_source(d, &sources.global_dir, &home);
    if let Some(p) = &sources.file {
        out.push(Layer::File(p.clone()));
    }
    let project_start = out.len();
    if sources.project {
        let mut dirs: Vec<&Path> = cwd
            .ancestors()
            .take_while(|d| d.starts_with(root))
            .filter(|d| walked(d))
            .collect();
        dirs.reverse();
        for d in &dirs {
            for n in NAMES {
                out.push(Layer::File(d.join(n)));
            }
        }
        for d in &dirs {
            for n in NAMES {
                out.push(Layer::File(d.join(".codesmith").join(n)));
            }
            out.push(Layer::Dir(d.join(".codesmith")));
        }
    }
    (out, project_start)
}

/// Load the config for `cwd`, with the process-wide models.dev catalog
/// (its options from the environment and the first config loaded).
pub fn load(cwd: &Path) -> Result<Loaded> {
    let mut loaded = load_from(cwd, &Sources::from_env())?;
    let c = &loaded.config.catalog;
    let options = crate::catalog::Options::resolve(c.fetch, c.url.as_deref());
    loaded.config.models_dev = Some(crate::catalog::ModelsDev::global(options));
    // What programs CodeSmith starts may see of its environment.
    crate::child_env::install(&loaded.config);
    crate::trust::announce(&loaded.config.trust);
    Ok(loaded)
}

/// Remove `providers.*.swap` from a config document that is not the global
/// config file, noting each one as `<id> in <source>`: a swap runs
/// commands, so a project (or anything else) may not set one.
pub fn strip_swap(v: &mut Value, source: &str, ignored: &mut Vec<String>) {
    let Some(providers) = v.get_mut("providers").and_then(Value::as_object_mut) else {
        return;
    };
    for (id, p) in providers.iter_mut() {
        if p.as_object_mut().and_then(|o| o.remove("swap")).is_some() {
            ignored.push(format!("{id} in {source}"));
        }
    }
}

/// One config source [`load_from`] reads, in order.
enum Entry {
    /// A config folder whose agent and command files are read.
    Dir(PathBuf),
    /// A config file: its path, real path, text, and whether it is a
    /// project file (rather than the global config or `$CODESMITH_CONFIG`).
    File {
        path: PathBuf,
        canonical: PathBuf,
        text: String,
        project: bool,
    },
}

/// The sources [`load_from`] reads for `cwd`, each read once.
fn gather(cwd: &Path, root: &Path, sources: &Sources) -> Result<Vec<Entry>> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    let (layers, project_start) = split_layers(cwd, root, sources);
    for (i, layer) in layers.into_iter().enumerate() {
        let path = match layer {
            Layer::File(p) if p.is_file() => p,
            Layer::Dir(d) if d.is_dir() => {
                let canonical = d.canonicalize().unwrap_or_else(|_| d.clone());
                if seen.contains(&canonical) {
                    continue;
                }
                seen.push(canonical);
                out.push(Entry::Dir(d));
                continue;
            }
            _ => continue,
        };
        // A file reached twice (the project walk passing through the global
        // folder, or `$CODESMITH_CONFIG` naming a project file) loads once,
        // as the first source that reached it.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical.clone());
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        out.push(Entry::File {
            path,
            canonical,
            text,
            project: i >= project_start,
        });
    }
    Ok(out)
}

/// What `cwd`'s project config files ask for that needs trust (see
/// [`crate::trust`]).
pub fn trust_request(cwd: &Path, sources: &Sources) -> Result<crate::trust::Request> {
    let root = project_root(cwd);
    let files: Vec<(PathBuf, String)> = gather(cwd, &root, sources)?
        .into_iter()
        .filter_map(|e| match e {
            Entry::File {
                path,
                text,
                project: true,
                ..
            } => Some((path, text)),
            _ => None,
        })
        .collect();
    Ok(crate::trust::detect(&root, &files))
}

pub fn load_from(cwd: &Path, sources: &Sources) -> Result<Loaded> {
    use crate::trust::{Mode, State};
    let root = project_root(cwd);
    let mut merged = Value::Object(Map::new());
    let mut files: Vec<PathBuf> = Vec::new();
    let mut swap_ignored: Vec<String> = Vec::new();
    let mut claude_agent_ignored: Vec<String> = Vec::new();
    let mut shell_env = ShellEnv::default();
    // Only the global config file may set a provider's `swap`.
    let global_dir = (!global_disabled()).then(|| {
        sources
            .global_dir
            .canonicalize()
            .unwrap_or_else(|_| sources.global_dir.clone())
    });
    let entries = gather(cwd, &root, sources)?;
    // What the project's files ask for, and whether the person trusts the
    // project for it.
    let project_files: Vec<(PathBuf, String)> = entries
        .iter()
        .filter_map(|e| match e {
            Entry::File {
                path,
                text,
                project: true,
                ..
            } => Some((path.clone(), text.clone())),
            _ => None,
        })
        .collect();
    let request = crate::trust::detect(&root, &project_files);
    let state = match &sources.trust {
        _ if request.is_empty() => State::NotNeeded,
        Mode::All => State::Trusted,
        Mode::Store(p) => {
            crate::trust::Store::new(p.clone()).state(&crate::trust::project_key(&root), &request)
        }
    };
    // A project file's substitutions are filled in only when it is trusted
    // for them; otherwise they are empty (so no host file is even read).
    let fill = if state == State::Trusted {
        Fill::Real
    } else {
        Fill::Empty
    };
    let untrusted = matches!(state, State::Distrusted | State::Undecided);
    let mut skipped: Vec<String> = Vec::new();
    for entry in entries {
        let (path, canonical, text, project) = match entry {
            Entry::Dir(d) => {
                merge_layer(&mut merged, crate::agent::documents(&d));
                merge_layer(&mut merged, crate::command::documents(&d));
                continue;
            }
            Entry::File {
                path,
                canonical,
                text,
                project,
            } => (path, canonical, text, project),
        };
        let base = path.parent().unwrap_or(Path::new("."));
        let mut v = if project {
            parse_as(&text, base, fill).with_context(|| {
                if untrusted {
                    format!(
                        "in {} (its {{env:}} and {{file:}} are empty because the project is not trusted)",
                        path.display()
                    )
                } else {
                    format!("in {}", path.display())
                }
            })?
        } else {
            parse(&text, base).with_context(|| format!("in {}", path.display()))?
        };
        if project && untrusted {
            crate::trust::strip(&mut v, &merged, &root, &mut skipped);
        }
        let global_file = global_dir
            .as_deref()
            .is_some_and(|g| canonical.parent() == Some(g));
        if !global_file {
            strip_swap(&mut v, &path.display().to_string(), &mut swap_ignored);
            strip_claude_agent(
                &mut v,
                &path.display().to_string(),
                &mut claude_agent_ignored,
            );
        }
        take_shell_env(
            &mut v,
            &path.display().to_string(),
            global_file,
            &mut shell_env,
        );
        shell_env
            .credentials
            .extend(crate::child_env::credential_refs(&text));
        merge_layer(&mut merged, v);
        files.push(path);
    }
    if let Some(text) = &sources.content {
        let mut v = parse(text, cwd).context("in $CODESMITH_CONFIG_CONTENT")?;
        strip_swap(&mut v, "$CODESMITH_CONFIG_CONTENT", &mut swap_ignored);
        strip_claude_agent(
            &mut v,
            "$CODESMITH_CONFIG_CONTENT",
            &mut claude_agent_ignored,
        );
        take_shell_env(&mut v, "$CODESMITH_CONFIG_CONTENT", false, &mut shell_env);
        shell_env
            .credentials
            .extend(crate::child_env::credential_refs(text));
        merge_layer(&mut merged, v);
    }
    let mut config: Config = serde_json::from_value(merged).context("invalid codesmith config")?;
    config.swap_ignored = swap_ignored;
    config.claude_agent_ignored = claude_agent_ignored;
    config.shell_env = shell_env;
    config.trust = crate::trust::Outcome {
        request,
        state,
        skipped,
    };
    crate::plugin::active().transform(&mut config);
    Ok(Loaded {
        config,
        files,
        root,
    })
}

/// Parse JSONC text after `{env:VAR}` / `{file:path}` substitution.
pub fn parse(text: &str, base: &Path) -> Result<Value> {
    parse_as(text, base, Fill::Real)
}

/// What `{env:}` and `{file:}` become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// The variable's value and the file's contents.
    Real,
    /// Nothing (an untrusted project's config): no file is read.
    Empty,
    /// Left as written (to see a file's shape without its secrets).
    Literal,
    /// The variable's value, but no file is read.
    Env,
}

/// [`parse`] with substitutions filled in as `fill` says.
pub fn parse_as(text: &str, base: &Path, fill: Fill) -> Result<Value> {
    let env = |name: &str| match fill {
        Fill::Real | Fill::Env => std::env::var(name).unwrap_or_default(),
        Fill::Empty => String::new(),
        Fill::Literal => format!("{{env:{name}}}"),
    };
    let file = |raw: &str, p: &Path| match fill {
        Fill::Real => std::fs::read_to_string(p).unwrap_or_default(),
        Fill::Empty | Fill::Env => String::new(),
        Fill::Literal => format!("{{file:{raw}}}"),
    };
    let substituted = substitute_using(text, base, &env, &file);
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
    substitute_using(
        text,
        base,
        &|name| std::env::var(name).unwrap_or_default(),
        &|_, p| std::fs::read_to_string(p).unwrap_or_default(),
    )
}

/// [`substitute`] with `env` giving a variable's value and `file` a file's
/// contents (from the path as written and resolved).
pub fn substitute_using(
    text: &str,
    base: &Path,
    env: &dyn Fn(&str) -> String,
    file: &dyn Fn(&str, &Path) -> String,
) -> String {
    let raw = substitute_with(text, base, false, env, file);
    if !text.contains("{env:") || serde_json::from_str::<Value>(&strip_jsonc(&raw)).is_ok() {
        return raw;
    }
    substitute_with(text, base, true, env, file)
}

fn escaped(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap();
    quoted[1..quoted.len() - 1].to_string()
}

fn substitute_with(
    text: &str,
    base: &Path,
    escape_env: bool,
    env_value: &dyn Fn(&str) -> String,
    file_value: &dyn Fn(&str, &Path) -> String,
) -> String {
    let env = regex::Regex::new(r"\{env:([^}]+)\}").unwrap();
    let text = env.replace_all(text, |c: &regex::Captures| {
        let value = env_value(&c[1]);
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
        let content = file_value(raw, &p);
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

const APPEND: [&str; 3] = ["permissions", "instructions", "disabled_providers"];

/// Merge one layer over the layers below it: as [`merge`], except that a
/// command a later layer defines replaces the earlier definition whole
/// (OpenCode's commands), where agents merge field by field.
pub fn merge_layer(base: &mut Value, mut top: Value) {
    let commands = top.as_object_mut().and_then(|t| t.remove("commands"));
    match (base.get_mut("commands"), commands) {
        (Some(Value::Object(b)), Some(Value::Object(t))) => b.extend(t),
        (_, Some(c)) => {
            if let Value::Object(b) = base {
                b.insert("commands".into(), c);
            }
        }
        _ => {}
    }
    merge(base, top);
}

/// Deep-merge `top` into `base`.
pub fn merge(base: &mut Value, top: Value) {
    match (base, top) {
        (Value::Object(b), Value::Object(t)) => {
            for (k, v) in t {
                match b.get_mut(&k) {
                    // Earlier-loaded policy wins (OpenCode reverses its
                    // documents): a later layer's statements go first.
                    Some(Value::Array(existing)) if k == "policies" => {
                        if let Value::Array(mut more) = v {
                            more.append(existing);
                            *existing = more;
                        } else {
                            b.insert(k, v);
                        }
                    }
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
    /// The same variants with their request overlays: the generated ones
    /// (from the catalog's reasoning controls, else the built-in ones), with
    /// the configured ones merged in.
    pub variant_list: Vec<(String, codesmith_ai::Variant)>,
    pub max_output: Option<u32>,
    pub context: Option<u64>,
    pub temperature: Option<f32>,
    /// Input modalities (`text`, `image`, `pdf`, …); `None` when unknown,
    /// in which case images and PDFs are sent.
    pub input: Option<Vec<String>>,
    /// The freeform tool formats the model accepts (`text`, `regex`,
    /// `lark`): a tool declaring one of them goes out as a raw-text custom
    /// tool. `None` when it accepts none (or nobody says).
    pub freeform: Option<Vec<String>>,
    /// Prompt caching for this model's requests.
    pub cache: codesmith_ai::Cache,
    /// The WebSocket route session steps take (`settings.transport`), or
    /// `None` for HTTP.
    pub websocket: Option<codesmith_ai::ws::Route>,
    /// `Some` when the provider compacts the conversation itself
    /// (`compaction: {"type": "native"}`), with the mechanism to use when
    /// one was chosen.
    pub native_compaction: Option<Option<codesmith_ai::CompactMechanism>>,
    /// A server on this machine or network (see [`ProviderConfig::local`]).
    pub local: bool,
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
        "azure",
        "Azure OpenAI",
        Protocol::OpenaiResponses,
        "",
        &[
            "AZURE_OPENAI_API_KEY",
            "AZURE_API_KEY",
            "AZURE_COGNITIVE_SERVICES_API_KEY",
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
    (
        "vllm",
        "vLLM (local)",
        Protocol::OpenaiChat,
        "http://127.0.0.1:8000/v1",
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
            base_url: Some(url.to_string()).filter(|u| !u.is_empty()),
            ..Default::default()
        },
        ..Default::default()
    })
}

impl Config {
    /// The provider config for `id`: the built-in one, else the catalog's
    /// (if any), overlaid by config.
    ///
    /// A configured provider never takes its protocol from the catalog: it
    /// keeps its own (`openai-chat` unless set), and the catalog's name and
    /// key variables fill in, with its URL only when the catalog speaks that
    /// same protocol there. Only a provider that is not configured at all
    /// runs on the catalog's protocol.
    pub fn provider(&self, id: &str) -> Option<ProviderConfig> {
        let base = builtin(id)
            .or_else(|| crate::providers::builtin(id))
            .or_else(|| {
                let c = self.models_dev.as_ref()?.catalog();
                let p = c.provider(id)?;
                let Some(user) = self.providers.get(id) else {
                    return catalog_provider(p);
                };
                let mine = user.protocol.unwrap_or(Protocol::OpenaiChat);
                let same = crate::catalog::protocol(&p.info.package) == Some(mine);
                let url = catalog_provider(p)
                    .filter(|_| same)
                    .and_then(|c| c.settings.base_url);
                Some(ProviderConfig {
                    name: Some(p.info.name.clone()),
                    env: catalog_env(id, &p.environment),
                    settings: ProviderSettings {
                        base_url: url,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            });
        match (base, self.providers.get(id).cloned()) {
            (None, None) => None,
            (Some(b), None) => Some(b),
            (None, Some(c)) => Some(c),
            (Some(b), Some(c)) => Some(ProviderConfig {
                name: c.name.or(b.name),
                local: c.local.or(b.local),
                protocol: c.protocol.or(b.protocol),
                env: if c.env.is_empty() { b.env } else { c.env },
                settings: ProviderSettings {
                    base_url: c.settings.base_url.or(b.settings.base_url),
                    api_key: c.settings.api_key.or(b.settings.api_key),
                    ..c.settings
                },
                headers: c.headers,
                body: c.body,
                options: c.options,
                cache: c.cache,
                compaction: c.compaction.or(b.compaction),
                compatibility: c.compatibility,
                models: c.models,
                swap: c.swap,
            }),
        }
    }

    /// Whether `disabled_providers` turns provider `id` off.
    pub fn provider_disabled(&self, id: &str) -> bool {
        self.disabled_providers.iter().any(|d| d == id)
    }

    /// The model a session runs when none is chosen: the configured
    /// `model`, unless its provider is disabled, else the newest available
    /// chat model that takes and makes text (OpenCode's default model).
    /// Available is what the model picker lists: configured providers,
    /// catalog providers with a key, and local servers, never a disabled
    /// provider. Models without a release date come last, in listing
    /// order. `None` when there is no model to run.
    pub fn default_model(&self) -> Option<String> {
        let configured = self.model.as_deref().filter(|m| {
            let (base, _) = split_variant(m);
            !base
                .split_once('/')
                .is_some_and(|(p, _)| self.provider_disabled(p))
        });
        if let Some(m) = configured {
            return Some(m.to_string());
        }
        self.newest_model()
    }

    /// The newest available chat model that takes and makes text.
    pub fn newest_model(&self) -> Option<String> {
        let text = |v: &[String]| v.is_empty() || v.iter().any(|m| m.starts_with("text"));
        let mut best: Option<crate::plugin::codesmith::ModelRow> = None;
        for row in crate::plugin::codesmith::available(self, "chat") {
            // The catalog's capabilities with the configured ones over them.
            let text = self
                .capabilities(&format!("{}/{}", row.provider_id, row.id))
                .is_some_and(|c| text(&c.input) && text(&c.output));
            // The first of equally new models wins, as a stable sort keeps it.
            if text && best.as_ref().is_none_or(|b| row.released > b.released) {
                best = Some(row);
            }
        }
        best.map(|b| format!("{}/{}", b.provider_id, b.id))
    }

    /// Resolve `provider/model` or `provider/model#variant` (the first `/`
    /// splits; model ids may contain more).
    pub fn resolve_model(&self, spec: &str) -> Result<ModelRef> {
        // Spaces around the parts are not part of them (`anthropic/ opus`).
        let (base, variant) = split_variant(spec.trim());
        let variant = variant.map(str::trim).filter(|v| !v.is_empty());
        let (pid, mid) = base
            .split_once('/')
            .map(|(p, m)| (p.trim(), m.trim()))
            .filter(|(p, m)| !p.is_empty() && !m.is_empty())
            .ok_or_else(|| anyhow!("model must be provider/model, got {spec:?}"))?;
        if self.provider_disabled(pid) {
            return Err(anyhow!(
                "provider {pid:?} is disabled (\"disabled_providers\" in codesmith.json)"
            ));
        }
        let p = self.provider(pid).ok_or_else(|| {
            anyhow!("unknown provider {pid:?}; add it under \"providers\" in codesmith.json")
        })?;
        let user = self.providers.get(pid);
        let m = p.models.get(mid).cloned().unwrap_or_default();
        // The catalog's model, found by the configured model id, then the key.
        let catalog = self.models_dev.as_ref().map(|c| c.catalog());
        let cp = catalog.as_ref().and_then(|c| c.provider(pid));
        // A model of the provider's kept live list wins over the catalog's.
        let live = live_model(
            cp,
            pid,
            self.live_account(pid).as_deref(),
            m.id.as_deref().unwrap_or(mid),
        );
        let cm = live.as_ref().or_else(|| {
            catalog.as_ref().and_then(|c| {
                m.id.as_deref()
                    .and_then(|id| c.model(pid, id))
                    .or_else(|| c.model(pid, mid))
            })
        });
        let catalog_package = cm
            .and_then(|cm| cm.package.as_deref())
            .or(cp.map(|p| p.info.package.as_str()));
        // A model of an unconfigured catalog provider may speak another
        // protocol than the provider's; a built-in or configured provider
        // keeps its own (a configured one `openai-chat` unless set).
        let protocol = match (user, builtin(pid)) {
            (Some(u), _) => u
                .protocol
                .or_else(|| builtin(pid).and_then(|b| b.protocol))
                .unwrap_or(Protocol::OpenaiChat),
            (None, Some(b)) => b.protocol.unwrap_or(Protocol::OpenaiChat),
            (None, None) => cm
                .and_then(|cm| cm.package.as_deref())
                .and_then(crate::catalog::protocol)
                .or(p.protocol)
                .unwrap_or(Protocol::OpenaiChat),
        };
        let azure = pid == "azure";
        // Azure's resource: the saved login's, the configured one, or the
        // environment's; it fills `${AZURE_RESOURCE_NAME}` in URLs.
        let azure_login = azure
            .then(|| crate::auth::entry(crate::providers::azure::ID))
            .flatten();
        let azure_resource = azure
            .then(|| {
                crate::providers::azure::resource_name(
                    &p.settings,
                    azure_login.as_ref(),
                    &crate::providers::process_env(),
                )
            })
            .flatten();
        let resource_url = |u: &str| match &azure_resource {
            Some(r) => crate::providers::azure::expand(u, r),
            None => u.to_string(),
        };
        let model_id =
            m.id.clone()
                .or_else(|| cm.map(|cm| cm.model_id.clone()))
                .unwrap_or_else(|| mid.to_string());
        let configured_url = user
            .and_then(|u| u.settings.base_url.clone())
            .filter(|u| !u.is_empty());
        let plugin = self.plugin_route(PluginRoute {
            pid,
            p: &p,
            user,
            configured_url: configured_url.as_deref(),
            package: catalog_package,
            catalog_url: cm.and_then(|cm| cm.settings.as_ref()?.base_url.as_deref()),
            model_id: &model_id,
        })?;
        let protocol = plugin.as_ref().map_or(protocol, |r| r.protocol);
        let wire_model = plugin
            .as_ref()
            .and_then(|r| r.model_id.clone())
            .unwrap_or_else(|| model_id.clone());
        // A configured provider URL beats the catalog model's own (when the
        // catalog speaks this protocol there), which beats the provider's
        // default.
        let catalog_url = cm
            .filter(|_| catalog_package.and_then(crate::catalog::protocol) == Some(protocol))
            .and_then(|cm| cm.settings.as_ref()?.base_url.as_deref())
            .and_then(|u| expand_url(&resource_url(u)));
        let configured_url = configured_url.map(|u| resource_url(&u));
        let base_url = configured_url
            .clone()
            .or_else(|| plugin.as_ref().map(|r| r.base_url.clone()))
            .or(catalog_url)
            .or_else(|| p.settings.base_url.clone())
            .filter(|u| !u.is_empty())
            // Claude Agent runs through Claude Code, which has its own.
            .or_else(|| (protocol == Protocol::ClaudeAgent).then(String::new))
            .or_else(|| {
                azure_resource
                    .as_deref()
                    .filter(|_| azure)
                    .map(|r| codesmith_ai::azure_resource_url(r, protocol))
            })
            .ok_or_else(|| match azure {
                true => {
                    anyhow!("provider \"azure\" needs settings.resourceName or settings.baseURL")
                }
                false => anyhow!("provider {pid:?} has no settings.baseURL"),
            })?;
        // An Azure CLI login signs each request with an Entra ID token.
        let azure_cli = azure && crate::providers::azure::cli_login(azure_login.as_ref());
        let api_key = match &plugin {
            Some(r) => r.api_key.clone(),
            None if azure_cli => None,
            None => provider_key(pid, &p),
        };
        let query: Vec<(String, String)> = p.settings.query_params.clone().into_iter().collect();
        let (base_url, query) = match (azure, protocol) {
            // Claude on Foundry: Messages with the resource key as `x-api-key`.
            (true, Protocol::AnthropicMessages) | (false, _) => (base_url, query),
            (true, _) => codesmith_ai::azure_endpoint(
                &base_url,
                &model_id,
                p.settings.api_version.as_deref(),
                &query,
                p.settings.use_deployment_based_urls,
            ),
        };
        let auth = match p.settings.api_key_header.clone().filter(|h| !h.is_empty()) {
            Some(h) => codesmith_ai::Auth::Header(h),
            None if plugin.is_some() => plugin.as_ref().unwrap().auth.clone(),
            None if azure_cli => {
                crate::providers::azure::cli_auth(&crate::providers::process_env())?
            }
            None if azure && protocol != Protocol::AnthropicMessages => {
                codesmith_ai::Auth::Header("api-key".into())
            }
            None => codesmith_ai::Auth::Default,
        };
        let encoding = match p.settings.encoding.as_deref() {
            None => codesmith_ai::Encoding::None,
            Some(e) => codesmith_ai::Encoding::parse(e).ok_or_else(|| {
                anyhow!("provider {pid:?} has an invalid encoding {e:?}; use gzip, zstd or none")
            })?,
        };
        // The catalog's compatibility, then the provider's, then the model's.
        let mut compat = Map::new();
        if let Some(c) = cm.and_then(|cm| cm.compatibility.as_ref()) {
            if let Some(f) = &c.reasoning_field {
                compat.insert("reasoningField".into(), f.clone().into());
            }
            if let Some(f) = &c.max_tokens_field {
                compat.insert("maxTokensField".into(), f.clone().into());
            }
        }
        let (plugin_compat, plugin_options) = crate::providers::defaults(pid, &model_id);
        compat.extend(plugin_compat);
        compat.extend(p.compatibility.clone());
        if let Some(f) = m.interleaved.as_ref().and_then(interleaved_field) {
            compat.insert("reasoningField".into(), f.into());
        }
        compat.extend(m.compatibility.clone());
        let compat: codesmith_ai::Compatibility = serde_json::from_value(Value::Object(compat))
            .map_err(|e| anyhow!("model {base:?} has an invalid compatibility setting: {e}"))?;
        let claude = (pid == crate::providers::claude_agent::ID)
            .then(crate::providers::claude_agent::catalog);
        let input = m
            .input()
            .or_else(|| {
                claude
                    .as_ref()
                    .map(|_| vec!["text".into(), "image".into(), "pdf".into()])
            })
            .or_else(|| {
                cm.map(|cm| cm.capabilities.input.clone())
                    .filter(|i| !i.is_empty())
            })
            .or_else(|| known_input(&model_id));
        let context = m
            .limit
            .context
            .or(cm.map(|cm| cm.limit.context).filter(|c| *c > 0))
            .or_else(|| {
                claude
                    .as_ref()
                    .and_then(|c| c.model(&model_id))
                    .and_then(|m| m.context_window)
            });
        // As in OpenCode, no reply asks for more than 256k tokens, however
        // much the model (configured or in the catalog) allows.
        let max_output = m.limit.output.map(|o| o.min(256_000)).or(cm
            .map(|cm| cm.limit.output.min(256_000) as u32)
            .filter(|o| *o > 0));
        let mut body = p.body.clone();
        if let Some(b) = cm.and_then(|cm| cm.body.as_ref()) {
            body.extend(b.clone());
        }
        body.extend(m.body.clone());
        let mut options = codesmith_ai::Options::from_map(plugin_options);
        if claude.is_some() {
            use codesmith_ai::claude_agent::{CHUNK_TIMEOUT_OPTION, HEADER_TIMEOUT_OPTION};
            let s = &p.settings.plugin;
            for (k, v) in [
                (HEADER_TIMEOUT_OPTION, s.header_timeout),
                (CHUNK_TIMEOUT_OPTION, s.chunk_timeout),
            ] {
                if let Some(v) = v {
                    options.set(k, v.into());
                }
            }
        }
        options.merge(&codesmith_ai::Options::from_map(p.options.clone()));
        options.merge(&codesmith_ai::Options::from_map(m.options.clone()));
        let cache = cache_setting(&p, &m)
            .map(|v| {
                codesmith_ai::Cache::from_value(&v)
                    .ok_or_else(|| anyhow!("model {base:?} has an invalid cache setting: {v}"))
            })
            .transpose()?
            .unwrap_or_default();
        let local = p.is_local(&base_url);
        let mut headers: Vec<(String, String)> = p.headers.into_iter().collect();
        crate::providers::gateways::apply(pid, &base_url, &mut headers);
        for (k, v) in plugin.iter().flat_map(|r| r.headers.iter()) {
            if !headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(k)) {
                headers.push((k.clone(), v.clone()));
            }
        }
        for (k, v) in cm.and_then(|cm| cm.headers.as_ref()).into_iter().flatten() {
            if let Some(v) = v.as_str() {
                headers.retain(|(h, _)| !h.eq_ignore_ascii_case(k));
                headers.push((k.clone(), v.to_string()));
            }
        }
        // Generated reasoning variants, then the configured ones, which
        // merge into a generated one of the same name.
        let mut variants = generated_variants(GeneratedVariants {
            protocol,
            provider_id: pid,
            model_id: &model_id,
            output: max_output,
            catalog: cm,
            catalog_package,
            reasoning: m
                .capabilities
                .as_ref()
                .and_then(|c| c.reasoning.as_ref())
                .map(ReasoningConfig::to_catalog),
        });
        if let Some(c) = &claude {
            variants.extend(crate::providers::claude_agent::variants(c, &model_id));
        }
        for (name, v) in m.variants() {
            match variants.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => {
                    slot.1.options.merge(&v.options);
                    let mut b = Value::Object(std::mem::take(&mut slot.1.body));
                    merge(&mut b, Value::Object(v.body));
                    if let Value::Object(b) = b {
                        slot.1.body = b;
                    }
                    for (k, val) in v.headers {
                        slot.1.headers.retain(|(h, _)| !h.eq_ignore_ascii_case(&k));
                        slot.1.headers.push((k, val));
                    }
                }
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
        // The cache belongs to the model, not to a request option.
        options.remove("cache");
        // WebSocket handshakes carry static headers only, so dynamic
        // credentials stay on HTTP.
        let websocket = match auth {
            codesmith_ai::Auth::Dynamic(_) => None,
            _ => websocket_route(pid, protocol, &p.settings, &base_url)?,
        };
        let mut native_compaction =
            native_compaction(m.compaction.as_ref().or(p.compaction.as_ref()));
        let mut context = context;
        // A ChatGPT sign-in: summary compaction, no effort updates, and the
        // plan's input window.
        if matches!(&auth, codesmith_ai::Auth::Dynamic(a) if a.describe() == crate::providers::chatgpt::DESCRIBE)
        {
            native_compaction = None;
            // The ChatGPT backend takes no `configuration_update` items: a
            // mid-session effort switch sends the new effort for the whole
            // request instead, as OpenCode sends it.
            options.set("effortUpdates", false.into());
            if let Some(c) = context {
                let out = max_output.unwrap_or(0) as u64;
                if let Some((ctx, _)) = crate::providers::chatgpt::capped(&wire_model, c, None, out)
                {
                    context = Some(ctx);
                }
            }
        }
        // The configured formats, else the catalog's (OpenCode's
        // `mergeCapabilities`).
        let freeform = m
            .capabilities
            .as_ref()
            .and_then(|c| c.freeform.clone())
            .or_else(|| cm.and_then(|cm| cm.capabilities.freeform.clone()));
        Ok(ModelRef {
            provider_id: pid.to_string(),
            model_id: wire_model,
            provider: codesmith_ai::Provider {
                protocol,
                name: pid.to_string(),
                base_url,
                api_key,
                headers,
                query,
                auth,
                encoding,
                compat,
                hooks: None,
            },
            body,
            options,
            variant: variant.map(str::to_string),
            variants: variants.iter().map(|v| v.0.clone()).collect(),
            variant_list: variants,
            max_output,
            context,
            temperature: m.temperature,
            input,
            freeform,
            cache,
            websocket,
            native_compaction,
            local,
        })
    }
}

// Two questions are asked of a server's address, with different answers:
//
// - May CodeSmith contact it without being asked (to list a local
//   provider's models)? Only on this machine: [`is_loopback_url`]. Asking
//   a host elsewhere on the network unprompted would announce the user to
//   it, and a name that merely looks local (`gpu.lan`) may not be.
// - Is it a local, single-instance server, which one request keeps busy,
//   so that side requests wait for the turn? The broader
//   [`is_local_url`]: this machine or a private network (a box on the LAN,
//   a Tailscale peer).
//
// A provider's `local` setting answers both when it is set: `true` says
// the server is the user's own (so it may be asked for its models too),
// `false` that it is a hosted service that runs requests side by side
// (so it is not asked unprompted, even on loopback, as a proxy there
// would forward the request). See [`ProviderConfig::is_local`] and
// [`ProviderConfig::probe_unprompted`].

/// Whether `url`'s host is this machine: `localhost`, `127.0.0.0/8`, `::1`
/// or an IPv4-mapped loopback address (no other name, which a resolver may
/// send out to the network). The servers that may be asked for their
/// models without being asked.
pub fn is_loopback_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    match u.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost"
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        None => false,
    }
}

/// Whether `url` points at this machine or a private network: a loopback,
/// private, link-local, shared (CGNAT, as Tailscale uses) or unique-local
/// address, `localhost`, a single-label host name, or a `.local`, `.lan`,
/// `.internal`, `.home.arpa` or `.ts.net` name. The servers taken to be
/// local, single-instance ones (see [`ProviderConfig::is_local`]).
pub fn is_local_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    match u.host() {
        Some(url::Host::Ipv4(ip)) => {
            let [a, b, ..] = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || (a == 100 && (64..128).contains(&b))
        }
        Some(url::Host::Ipv6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_local_url(&format!("http://{v4}/")))
        }
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost"
                || !d.contains('.')
                || [
                    ".localhost",
                    ".local",
                    ".lan",
                    ".internal",
                    ".home.arpa",
                    ".ts.net",
                ]
                .iter()
                .any(|s| d.ends_with(s))
        }
        None => false,
    }
}

/// A model of a provider's kept live list (see [`crate::providers::live`]).
fn live_model(
    cp: Option<&crate::catalog::Snapshot>,
    pid: &str,
    account: Option<&str>,
    id: &str,
) -> Option<crate::catalog::Model> {
    let templates = cp.map(|p| p.models.as_slice()).unwrap_or(&[]);
    crate::providers::live::models(pid, account, templates)
        .into_iter()
        .find(|m| m.id == id)
}

/// A `compaction` setting of a provider or model: `Some` for
/// `{"type": "native"}`, with its `mechanism` when given.
fn native_compaction(v: Option<&Value>) -> Option<Option<codesmith_ai::CompactMechanism>> {
    let v = v?;
    if v.get("type").and_then(Value::as_str) != Some("native") {
        return None;
    }
    Some(match v.get("mechanism").and_then(Value::as_str) {
        Some("trigger") => Some(codesmith_ai::CompactMechanism::Trigger),
        Some("endpoint") => Some(codesmith_ai::CompactMechanism::Endpoint),
        _ => None,
    })
}

impl Config {
    /// The key of a provider: configured, from its key variables, or saved.
    pub fn provider_key(&self, pid: &str, p: &ProviderConfig) -> Option<String> {
        provider_key(pid, p)
    }

    /// The account a provider's kept live list must have been fetched for:
    /// a local provider's endpoint (see [`crate::providers::local`]), or a
    /// hosted provider's key fingerprint (see [`crate::providers::hosted`]).
    pub fn live_account(&self, pid: &str) -> Option<String> {
        let p = self.provider(pid)?;
        // A hosted provider's list is for the key it was fetched with.
        if crate::providers::hosted::hosted(pid) {
            return Some(crate::providers::hosted::account(
                provider_key(pid, &p).as_deref(),
            ));
        }
        let kind = crate::providers::local::kind(pid, &p.settings)?;
        let root = crate::providers::local::root(kind, p.settings.base_url.as_deref()?)?;
        Some(crate::providers::local::account(&root))
    }
}

/// The key of a provider: configured, from its key variables, or saved.
fn provider_key(pid: &str, p: &ProviderConfig) -> Option<String> {
    p.settings
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| {
            p.env
                .iter()
                .find_map(|v| std::env::var(v).ok().filter(|k| !k.is_empty()))
        })
        .or_else(|| crate::auth::key(pid))
}

/// What [`Config::plugin_route`] needs from `resolve_model`.
struct PluginRoute<'a> {
    pid: &'a str,
    p: &'a ProviderConfig,
    user: Option<&'a ProviderConfig>,
    configured_url: Option<&'a str>,
    package: Option<&'a str>,
    catalog_url: Option<&'a str>,
    model_id: &'a str,
}

impl Config {
    /// The route a provider plugin gives the model, if one owns it.
    fn plugin_route(&self, r: PluginRoute) -> Result<Option<crate::providers::Route>> {
        crate::providers::route(&crate::providers::Input {
            provider_id: r.pid,
            settings: &r.p.settings,
            configured_url: r.configured_url,
            configured_protocol: r.user.and_then(|u| u.protocol),
            package: r.package,
            catalog_url: r.catalog_url,
            model_id: r.model_id,
            api_key: provider_key(r.pid, r.p),
            stored: crate::auth::entry(crate::providers::login_id(r.pid)),
            env: codesmith_ai::aws::process_env(),
        })
    }
}

struct GeneratedVariants<'a> {
    protocol: Protocol,
    provider_id: &'a str,
    model_id: &'a str,
    output: Option<u32>,
    catalog: Option<&'a crate::catalog::Model>,
    catalog_package: Option<&'a str>,
    reasoning: Option<crate::catalog::Reasoning>,
}

/// A model's reasoning variants before config adds its own: from configured
/// reasoning controls, else the catalog's (respelled when the model runs on
/// another protocol than its catalog package's), else the built-in ones.
fn generated_variants(g: GeneratedVariants) -> Vec<(String, codesmith_ai::Variant)> {
    use crate::catalog::{self, variant};
    // The catalog package when CodeSmith speaks it here, else the
    // protocol's own.
    // Cloudflare AI Gateway's package spells each upstream route itself.
    let same = g.catalog_package.filter(|p| {
        catalog::protocol(p) == Some(g.protocol)
            || *p == "@opencode/ai/providers/cloudflare-ai-gateway"
    });
    let package = same.unwrap_or_else(|| catalog::protocol_package(g.protocol));
    let target = variant::Target {
        package: Some(package),
        provider_id: g.provider_id,
        model_id: g.model_id,
        output: g
            .output
            .map(i64::from)
            .or(g.catalog.map(|c| c.limit.output as i64))
            .unwrap_or(32_000),
    };
    let list = match (g.reasoning, g.catalog) {
        (Some(r), _) => variant::resolve(&target, &variant::supports(&r)),
        (None, Some(cm)) if same.is_some() => cm.variants.clone(),
        // A catalog model without variants has no reasoning to respell.
        (None, Some(cm)) if cm.variants.is_empty() => vec![],
        (None, Some(_)) => variant::resolve(&target, &variant::default_supports()),
        (None, None) => return codesmith_ai::builtin_variants(g.protocol, g.model_id, g.output),
    };
    list.iter()
        .map(|v| (v.id.clone(), v.to_overlay()))
        .collect()
}

/// The WebSocket route of a provider's session steps: its `transport`
/// setting, else (as in OpenCode) `websocket` for OpenAI, xAI and Azure v1
/// endpoints on the Responses protocol.
fn websocket_route(
    pid: &str,
    protocol: Protocol,
    settings: &ProviderSettings,
    base_url: &str,
) -> Result<Option<codesmith_ai::ws::Route>> {
    if protocol != Protocol::OpenaiResponses {
        return Ok(None);
    }
    let on = match settings.transport.as_deref() {
        Some("websocket") => true,
        Some("http") => false,
        Some(t) => {
            return Err(anyhow!(
                "provider {pid:?} has an invalid transport {t:?}; use websocket or http"
            ))
        }
        None => match pid {
            "openai" | "xai" => true,
            "azure" => {
                !settings.use_deployment_based_urls
                    && settings.api_version.as_deref().is_none_or(|v| v == "v1")
                    && settings.base_url.as_deref().is_none_or(|u| {
                        reqwest::Url::parse(u).is_ok_and(|u| {
                            u.scheme() == "https"
                                && u.host_str().is_some_and(|h| {
                                    h.to_ascii_lowercase().ends_with(".openai.azure.com")
                                })
                        })
                    })
            }
            _ => false,
        },
    };
    Ok(on.then(|| codesmith_ai::ws::Route::for_provider(pid, base_url)))
}

/// The model's `cache` setting: the model's own (field, then options), then
/// the provider's (field, `settings`, options).
fn cache_setting(p: &ProviderConfig, m: &ModelConfig) -> Option<Value> {
    m.cache
        .clone()
        .or_else(|| m.options.get("cache").cloned())
        .or_else(|| p.cache.clone())
        .or_else(|| p.settings.cache.clone())
        .or_else(|| p.options.get("cache").cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_inputs_cover_deepseek_and_gemini() {
        let all = Some(
            ["text", "image", "audio", "video", "pdf"]
                .map(String::from)
                .to_vec(),
        );
        assert_eq!(known_input("deepseek-chat"), Some(vec!["text".into()]));
        assert_eq!(known_input("deepseek-vl2"), None);
        assert_eq!(known_input("gemini-2.5-pro"), all);
        assert_eq!(known_input("google/gemini-2.5-flash"), all);
        for m in [
            "gemini-embedding-001",
            "gemini-2.5-flash-image",
            "gemma-3",
            "x-gemini-1",
        ] {
            assert_eq!(known_input(m), None, "{m}");
        }
        // Configured modalities win.
        let c: Config = serde_json::from_value(json!({"providers": {"google": {"models": {
            "gemini-2.5-pro": {"modalities": {"input": ["text"]}}}}}}))
        .unwrap();
        let m = c.resolve_model("google/gemini-2.5-pro").unwrap();
        assert_eq!(m.input, Some(vec!["text".into()]));
        assert_eq!(
            c.resolve_model("google/gemini-2.5-flash").unwrap().input,
            all
        );
    }

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

    #[cfg(unix)]
    #[test]
    fn a_world_writable_repository_is_not_the_project_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(shared.join(".git")).unwrap();
        std::fs::create_dir_all(shared.join("work")).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        // The folder itself, not the planted repository.
        assert_eq!(project_root(&shared.join("work")), shared.join("work"));
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(project_root(&shared.join("work")), shared);
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
    fn prompt_cache_settings() {
        use codesmith_ai::Cache;
        let c: Config = serde_json::from_value(json!({"providers": {
            "anthropic": {"cache": false, "models": {
                "claude-on": {"cache": {"ttl": "1h"}},
                "claude-opts": {"options": {"cache": true}},
            }},
            "openai": {"settings": {"cache": {"ttl": "24h"}}},
            "custom": {"settings": {"baseURL": "http://x/v1"}, "options": {"cache": "none"}},
        }}))
        .unwrap();
        let m = |spec: &str| c.resolve_model(spec).unwrap();
        assert_eq!(m("anthropic/claude-x").cache, Cache::off());
        let on = m("anthropic/claude-on");
        assert!(on.cache.enabled());
        assert_eq!(on.cache.ttl.as_deref(), Some("1h"));
        let opts = m("anthropic/claude-opts");
        assert_eq!(opts.cache, Cache::default());
        assert!(opts.options.get("cache").is_none(), "not sent as an option");
        assert_eq!(m("openai/gpt-x").cache.ttl.as_deref(), Some("24h"));
        assert_eq!(m("custom/any").cache, Cache::off());
        assert_eq!(
            Config::default().resolve_model("google/g").unwrap().cache,
            Cache::default()
        );
        let bad: Config =
            serde_json::from_value(json!({"providers": {"openai": {"cache": "sometimes"}}}))
                .unwrap();
        assert!(bad.resolve_model("openai/gpt-x").is_err());
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

    /// The HTTP request a resolved model sends.
    fn prepared(c: &Config, spec: &str) -> reqwest::Request {
        let m = c.resolve_model(spec).unwrap();
        let req = codesmith_ai::Request {
            model: m.model_id.clone(),
            messages: vec![codesmith_ai::Message::user("Say hello.")],
            ..Default::default()
        };
        m.provider.prepare(&reqwest::Client::new(), &req).unwrap()
    }

    // Ported from OpenCode's `provider/openai-responses.test.ts` (MIT).
    #[test]
    fn openai_responses_loads_openai_default_auth_from_effect_config() {
        // Adapted: a test variable stands in for OPENAI_API_KEY.
        std::env::set_var("CODESMITH_TEST_OPENAI_KEY", "env-key");
        let c: Config = serde_json::from_value(json!({"providers": {"openai": {
            "settings": {"baseURL": "https://api.openai.test/v1/"},
            "env": ["CODESMITH_TEST_OPENAI_KEY"]
        }}}))
        .unwrap();
        let r = prepared(&c, "openai/gpt-4.1-mini");
        assert_eq!(r.url().as_str(), "https://api.openai.test/v1/responses");
        assert_eq!(r.headers()["authorization"], "Bearer env-key");
    }

    #[test]
    fn openai_responses_lets_explicit_auth_override_openai_default_api_key_auth() {
        std::env::set_var("CODESMITH_TEST_OPENAI_KEY_2", "env-key");
        let c: Config = serde_json::from_value(json!({"providers": {"openai": {
            "settings": {"baseURL": "https://api.openai.test/v1/", "apiKey": "oauth-token"},
            "env": ["CODESMITH_TEST_OPENAI_KEY_2"]
        }}}))
        .unwrap();
        let r = prepared(&c, "openai/gpt-4.1-mini");
        assert_eq!(r.headers()["authorization"], "Bearer oauth-token");
    }

    #[test]
    fn azure_provider() {
        let c: Config = serde_json::from_value(json!({"providers": {"azure": {
            "settings": {"resourceName": "opencode-test", "apiKey": "azure-key"},
            "headers": {"authorization": "Bearer stale"},
            "models": {"chat": {"id": "gpt-4o-mini"}}
        }}}))
        .unwrap();
        let r = prepared(&c, "azure/gpt-4.1-mini");
        assert_eq!(
            r.url().as_str(),
            "https://opencode-test.openai.azure.com/openai/v1/responses?api-version=v1"
        );
        assert_eq!(r.headers()["api-key"], "azure-key");
        assert!(r.headers().get("authorization").is_none());
        // Deployment URLs, an api version, extra query parameters.
        let c: Config = serde_json::from_value(json!({"providers": {"azure": {
            "protocol": "openai-chat",
            "settings": {"baseURL": "https://r.openai.azure.com/openai/", "apiKey": "k",
                "apiVersion": "2025-04-01-preview", "useDeploymentBasedUrls": true,
                "queryParams": {"x": "1"}}
        }}}))
        .unwrap();
        assert_eq!(
            prepared(&c, "azure/dep").url().as_str(),
            "https://r.openai.azure.com/openai/deployments/dep/chat/completions?api-version=2025-04-01-preview&x=1"
        );
        // Claude on Foundry: the services host and `x-api-key`.
        let c: Config = serde_json::from_value(json!({"providers": {"azure": {
            "protocol": "anthropic-messages",
            "settings": {"resourceName": "r", "apiKey": "k"}
        }}}))
        .unwrap();
        let r = prepared(&c, "azure/claude-sonnet-4-5");
        assert_eq!(
            r.url().as_str(),
            "https://r.services.ai.azure.com/anthropic/v1/messages"
        );
        assert_eq!(r.headers()["x-api-key"], "k");
        let c: Config = serde_json::from_value(json!({})).unwrap();
        assert!(c
            .resolve_model("azure/gpt-5")
            .unwrap_err()
            .to_string()
            .contains("resourceName"));
    }

    #[test]
    fn endpoint_settings_and_compatibility() {
        let c: Config = serde_json::from_value(json!({"providers": {"p": {
            "settings": {"baseURL": "http://x/v1", "apiKey": "k",
                "queryParams": {"api-version": "2026-01-01"}, "apiKeyHeader": "x-key",
                "encoding": "gzip"},
            "compatibility": {"streamUsage": false, "maxTokensField": "max_tokens",
                "reasoningField": "vendor_reasoning"},
            "models": {"m": {"compatibility": {"maxTokensField": "max_completion_tokens",
                "requireSignature": false}}}
        }}}))
        .unwrap();
        let m = c.resolve_model("p/m").unwrap();
        assert_eq!(
            m.provider.query,
            [("api-version".to_string(), "2026-01-01".to_string())]
        );
        assert_eq!(m.provider.auth, codesmith_ai::Auth::Header("x-key".into()));
        assert_eq!(m.provider.encoding, codesmith_ai::Encoding::Gzip);
        assert_eq!(m.provider.compat.stream_usage, Some(false));
        assert_eq!(
            m.provider.compat.reasoning_field.as_deref(),
            Some("vendor_reasoning")
        );
        assert_eq!(m.provider.compat.require_signature, Some(false));
        // Detection by provider (DeepSeek, Kimi) reads the provider's id.
        assert_eq!(m.provider.name, "p");
        assert_eq!(
            m.provider.compat.max_tokens_field.as_deref(),
            Some("max_completion_tokens")
        );
        let r = prepared(&c, "p/m");
        assert_eq!(
            r.url().as_str(),
            "http://x/v1/chat/completions?api-version=2026-01-01"
        );
        assert_eq!(r.headers()["x-key"], "k");
        assert_eq!(r.headers()["content-encoding"], "gzip");
        let bad: Config = serde_json::from_value(json!({"providers": {"p": {
            "settings": {"baseURL": "http://x", "encoding": "brotli"}}}}))
        .unwrap();
        assert!(bad.resolve_model("p/m").is_err());
        let bad: Config = serde_json::from_value(json!({"providers": {"p": {
            "settings": {"baseURL": "http://x"}, "compatibility": {"streamUsage": "no"}}}}))
        .unwrap();
        assert!(bad.resolve_model("p/m").is_err());
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
            trust: crate::trust::Mode::All,
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

    // The Claude Agent process caps guard the machine, so only the global
    // config file sets them: a project file, an explicit file and
    // $CODESMITH_CONFIG_CONTENT cannot raise (or change) them.
    #[test]
    fn claude_agent_pool_comes_only_from_the_global_config_file() {
        use codesmith_ai::claude_agent::Pool;
        let (_d, t) = tmp();
        let (global, project) = (t.join("global"), project(&t));
        let l = load_from(&project, &sources(&global)).unwrap();
        assert_eq!(
            l.config.claude_agent.pool(),
            Pool {
                prewarm: false,
                spare: 0,
                max_processes: 4
            }
        );
        write(
            &global.join("codesmith.json"),
            r#"{"claude_agent": {"prewarm": true, "spare": 1, "max_processes": 6}}"#,
        );
        let explicit = t.join("custom.json");
        write(&explicit, r#"{"claude_agent": {"max_processes": 30}}"#);
        write(
            &project.join("codesmith.json"),
            r#"{"claude_agent": {"max_processes": 32, "spare": 8}}"#,
        );
        write(
            &project.join(".codesmith/codesmith.json"),
            r#"{"claude_agent": {"prewarm": false}}"#,
        );
        let mut src = sources(&global);
        src.file = Some(explicit);
        src.content = Some(r#"{"claude_agent": {"max_processes": 31}}"#.into());
        let l = load_from(&project, &src).unwrap();
        assert_eq!(
            l.config.claude_agent.pool(),
            Pool {
                prewarm: true,
                spare: 1,
                max_processes: 6
            }
        );
        assert_eq!(l.config.claude_agent_ignored.len(), 4);
        // Out of range values are brought within bounds.
        let c: Config =
            serde_json::from_value(json!({"claude_agent": {"spare": 99, "max_processes": 1000}}))
                .unwrap();
        assert_eq!(
            c.claude_agent.pool(),
            Pool {
                prewarm: false,
                spare: 31,
                max_processes: 32
            }
        );
        let c: Config =
            serde_json::from_value(json!({"claude_agent": {"spare": 3, "max_processes": 0}}))
                .unwrap();
        assert_eq!(c.claude_agent.pool().max_processes, 1);
        assert_eq!(c.claude_agent.pool().spare, 0);
    }

    // A provider's `swap` runs commands, so only the global config file
    // may set it; every other source has it dropped, with a note.
    #[test]
    fn swap_comes_only_from_the_global_config_file() {
        let (_d, t) = tmp();
        let (global, project) = (t.join("global"), project(&t));
        let swap = |prog: &str| {
            format!(
                r#"{{"providers": {{"stable": {{"swap": {{"list": ["{prog}", "list"], "use": ["{prog}", "use", "{{model}}"], "timeout": 900}}}}}}}}"#
            )
        };
        write(&global.join("codesmith.json"), &swap("/usr/bin/ssh"));
        let l = load_from(&project, &sources(&global)).unwrap();
        let s = l.config.providers["stable"].swap.clone().unwrap();
        assert_eq!(s.use_, ["/usr/bin/ssh", "use", "{model}"]);
        assert_eq!(s.timeout, Some(900));
        assert!(l.config.swap_ignored.is_empty());
        assert_eq!(l.config.provider("stable").unwrap().swap, Some(s.clone()));

        // A project file (plain or under .codesmith) cannot replace or add one.
        write(&project.join("codesmith.json"), &swap("/tmp/evil"));
        write(
            &project.join(".codesmith/codesmith.json"),
            r#"{"providers": {"other": {"swap": {"use": ["/tmp/evil", "{model}"]}, "name": "Other"}}}"#,
        );
        let l = load_from(&project, &sources(&global)).unwrap();
        assert_eq!(l.config.providers["stable"].swap, Some(s.clone()));
        assert_eq!(l.config.providers["other"].swap, None);
        assert_eq!(
            l.config.providers["other"].name.as_deref(),
            Some("Other"),
            "the rest still loads"
        );
        assert_eq!(l.config.swap_ignored.len(), 2);
        assert!(l.config.swap_ignored[0].starts_with("stable in "));

        // Nor can $CODESMITH_CONFIG or $CODESMITH_CONFIG_CONTENT.
        std::fs::remove_file(global.join("codesmith.json")).unwrap();
        std::fs::remove_file(project.join("codesmith.json")).unwrap();
        let explicit = t.join("custom.json");
        write(&explicit, &swap("/tmp/evil"));
        let mut src = sources(&global);
        src.file = Some(explicit);
        src.content = Some(swap("/tmp/evil2"));
        let l = load_from(&project, &src).unwrap();
        assert!(l.config.providers["stable"].swap.is_none());
        assert!(l
            .config
            .swap_ignored
            .iter()
            .any(|i| i == "stable in $CODESMITH_CONFIG_CONTENT"));
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

    #[test]
    fn local_urls() {
        for u in [
            "http://127.0.0.1:8000/v1",
            "http://localhost:1234/v1",
            "http://[::1]:8080/v1",
            "http://10.0.0.5:8000/v1",
            "http://192.168.1.20:8000/v1",
            "http://172.20.0.2/v1",
            "http://100.101.102.103:8000/v1",
            "http://skeptic-macmini.local:8000/v1",
            "http://macmini:8000/v1",
            "http://gpu.tail1234.ts.net/v1",
            "http://[fd00::1]/v1",
        ] {
            assert!(is_local_url(u), "{u}");
        }
        for u in [
            "https://api.openai.com/v1",
            "https://openrouter.ai/api/v1",
            "http://8.8.8.8/v1",
            "http://172.32.0.1/v1",
            "http://100.128.0.1/v1",
            "not a url",
        ] {
            assert!(!is_local_url(u), "{u}");
        }
    }

    #[test]
    fn loopback_urls() {
        for u in [
            "http://127.0.0.1:8000/v1",
            "http://127.1.2.3/v1",
            "http://localhost:1234/v1",
            "http://LOCALHOST./v1",
            "http://[::1]:8080/v1",
            "http://[::ffff:127.0.0.1]/v1",
        ] {
            assert!(is_loopback_url(u), "{u}");
            assert!(is_local_url(u), "{u}: loopback is local too");
        }
        for u in [
            "http://192.168.1.20:8000/v1",
            "http://skeptic-macmini.local:8000/v1",
            "http://macmini:8000/v1",
            "http://100.101.102.103:8000/v1",
            "http://0.0.0.0:8000/v1",
            "http://omlx.localhost:8000/v1",
            "https://api.openai.com/v1",
            "not a url",
        ] {
            assert!(!is_loopback_url(u), "{u}");
        }
    }

    #[test]
    fn the_local_setting_answers_both_questions() {
        let p = |local: Option<bool>| ProviderConfig {
            local,
            ..Default::default()
        };
        let lan = "http://192.168.1.20:8000/v1";
        let lo = "http://127.0.0.1:8000/v1";
        let far = "https://gpu.example.com/v1";
        // Unset: the address decides, loopback only for probing.
        assert!(p(None).is_local(lan) && !p(None).probe_unprompted(lan));
        assert!(p(None).is_local(lo) && p(None).probe_unprompted(lo));
        assert!(!p(None).is_local(far) && !p(None).probe_unprompted(far));
        // Set, it wins both ways.
        for u in [lan, lo, far] {
            assert!(p(Some(true)).is_local(u) && p(Some(true)).probe_unprompted(u));
            assert!(!p(Some(false)).is_local(u) && !p(Some(false)).probe_unprompted(u));
        }
    }

    #[test]
    fn local_providers() {
        let c: Config = serde_json::from_value(json!({"providers": {
            "box": {"settings": {"baseURL": "http://192.168.1.2:8000/v1"}, "models": {"m": {}}},
            "far": {"local": true, "settings": {"baseURL": "https://gpu.example.com/v1"}, "models": {"m": {}}},
            "near": {"local": false, "settings": {"baseURL": "http://127.0.0.1:9/v1"}, "models": {"m": {}}},
        }}))
        .unwrap();
        assert!(c.resolve_model("box/m").unwrap().local);
        assert!(c.resolve_model("far/m").unwrap().local);
        assert!(!c.resolve_model("near/m").unwrap().local);
        assert!(c.resolve_model("ollama/llama3").unwrap().local);
        assert!(!c.resolve_model("openrouter/x").unwrap().local);
    }

    #[test]
    fn title_models() {
        use crate::title::model_spec;
        let c = |v: Value| serde_json::from_value::<Config>(v).unwrap();
        assert_eq!(model_spec(&c(json!({})), "a/b").as_deref(), Some("a/b"));
        assert_eq!(
            model_spec(&c(json!({"small_model": "s/m"})), "a/b").as_deref(),
            Some("s/m")
        );
        assert_eq!(
            model_spec(
                &c(json!({"small_model": {"providerID": "s", "model": "m"}})),
                "a/b"
            )
            .as_deref(),
            Some("s/m")
        );
        assert_eq!(
            model_spec(
                &c(json!({"small_model": "s/m", "agents": {"title": {"model": "t/m"}}})),
                "a/b"
            )
            .as_deref(),
            Some("t/m")
        );
        assert_eq!(model_spec(&c(json!({"title": false})), "a/b"), None);
        assert_eq!(
            model_spec(&c(json!({"title": true})), "a/b").as_deref(),
            Some("a/b")
        );
    }
}
