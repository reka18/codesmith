//! Native Rust plugins, compiled into the binary.
//!
//! A plugin implements [`Plugin`]; every method has a default that does
//! nothing, so a plugin overrides only the hooks it needs. A custom build of
//! CodeSmith installs plugins with `codesmith::main(vec![Arc::new(MyPlugin)])`.
//! Config can turn one off (`"plugins": {"my-plugin": false}`) or give it
//! options (`"plugins": {"my-plugin": {...}}`), read in [`Plugin::config`].
//!
//! The hooks follow OpenCode's plugin API: transforms of the configuration
//! (agents, commands, providers, MCP servers, …), extra tools, and runtime
//! hooks around tool calls, shell commands, model requests, permission
//! prompts, session events and going idle.

use crate::config::Config;
use crate::permission::{Reply, Request};
use codesmith_ai::Usage;
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a plugin tool gets to work with.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub session_id: String,
}

/// A tool contributed by a plugin. Calls go through permissions with the
/// tool's name as the action (so `{"action": "my_tool", "effect": "ask"}` works).
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema of the input object.
    fn parameters(&self) -> Value;
    /// Run the tool; `Err` is shown to the model as a failed call.
    fn execute<'a>(
        &'a self,
        ctx: ToolContext,
        input: Value,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// A tool call about to run (mutable in [`Plugin::tool_before`]).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub session_id: String,
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// A shell command about to start (mutable in [`Plugin::shell_before`]).
#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub session_id: String,
    pub cwd: PathBuf,
    pub command: String,
    pub env: BTreeMap<String, String>,
}

/// Things that happen in a session, for observers.
#[derive(Debug, Clone)]
pub enum Event<'a> {
    TurnStart {
        session_id: &'a str,
        text: &'a str,
    },
    ToolStart {
        session_id: &'a str,
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    ToolEnd {
        session_id: &'a str,
        id: &'a str,
        name: &'a str,
        output: &'a str,
        is_error: bool,
    },
    StepEnd {
        session_id: &'a str,
        model: &'a str,
        usage: &'a Usage,
    },
    TurnEnd {
        session_id: &'a str,
        error: Option<&'a str>,
    },
}

/// A session that has finished its work and is about to go idle.
#[derive(Debug, Clone)]
pub struct Idle<'a> {
    pub session_id: &'a str,
    pub parent_id: Option<&'a str>,
    /// The model's final text this turn.
    pub answer: &'a str,
    /// How many times plugins have already continued this turn.
    pub continued: u32,
}

pub trait Plugin: Send + Sync {
    /// A stable id, used in config (`"plugins": {"<id>": ...}`).
    fn id(&self) -> &str;

    /// Transform the merged configuration: add or change agents, commands,
    /// providers, MCP servers, formatters, permissions and so on. `options`
    /// is this plugin's entry under `plugins` (or `null`).
    fn config(&self, _config: &mut Config, _options: &Value) {}

    /// Extra tools offered to the model.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![]
    }

    /// Before a tool runs: change its input, or refuse it with a message the model sees.
    fn tool_before(&self, _call: &mut ToolCall) -> Result<(), String> {
        Ok(())
    }

    /// After a tool ran: change what the model sees.
    fn tool_after(&self, _call: &ToolCall, _output: &mut String, _is_error: &mut bool) {}

    /// Before a shell command starts: change the command or its environment.
    fn shell_before(&self, _cmd: &mut ShellCommand) {}

    /// Just before a request goes to the model: change the system prompt,
    /// messages, tools or body.
    fn request(&self, _session_id: &str, _req: &mut codesmith_ai::Request) {}

    /// Answer a permission prompt instead of asking the user (`None`: no opinion).
    fn permission(&self, _req: &Request) -> Option<Reply> {
        None
    }

    /// Observe session events.
    fn event(&self, _event: &Event) {}

    /// The session is about to go idle; return a message to continue the
    /// turn with it instead (bounded by CodeSmith to a few continuations).
    fn idle(&self, _idle: &Idle) -> Option<String> {
        None
    }
}

/// How many times `idle` may continue one turn.
pub const MAX_CONTINUATIONS: u32 = 5;

#[derive(Default)]
pub struct Registry {
    plugins: Vec<Arc<dyn Plugin>>,
    /// Plugins the most recently loaded config turned off, for the hooks
    /// that run without a config at hand (permission, shell).
    disabled: RwLock<std::collections::HashSet<String>>,
}

static ACTIVE: RwLock<Option<Arc<Registry>>> = RwLock::new(None);

/// Install the plugins for this process (called once at startup).
pub fn install(plugins: Vec<Arc<dyn Plugin>>) {
    *ACTIVE.write().unwrap() = Some(Arc::new(Registry {
        plugins,
        disabled: Default::default(),
    }));
}

/// The installed plugins (empty if none).
pub fn active() -> Arc<Registry> {
    ACTIVE.read().unwrap().clone().unwrap_or_default()
}

fn enabled(config: &Config, id: &str) -> bool {
    !matches!(config.plugins.get(id), Some(Value::Bool(false)))
}

impl Registry {
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn ids(&self) -> Vec<String> {
        self.plugins.iter().map(|p| p.id().to_string()).collect()
    }

    /// Plugins enabled by this config.
    pub fn enabled<'a>(
        &'a self,
        config: &'a Config,
    ) -> impl Iterator<Item = &'a Arc<dyn Plugin>> + 'a {
        self.plugins.iter().filter(move |p| enabled(config, p.id()))
    }

    fn live(&self) -> impl Iterator<Item = &Arc<dyn Plugin>> {
        let off = self.disabled.read().unwrap().clone();
        self.plugins.iter().filter(move |p| !off.contains(p.id()))
    }

    pub fn transform(&self, config: &mut Config) {
        *self.disabled.write().unwrap() = self
            .plugins
            .iter()
            .filter(|p| !enabled(config, p.id()))
            .map(|p| p.id().to_string())
            .collect();
        for p in &self.plugins {
            if enabled(config, p.id()) {
                let options = config.plugins.get(p.id()).cloned().unwrap_or(Value::Null);
                p.config(config, &options);
            }
        }
    }

    pub fn tools(&self, config: &Config) -> Vec<Arc<dyn Tool>> {
        self.enabled(config).flat_map(|p| p.tools()).collect()
    }

    pub fn tool_before(&self, config: &Config, call: &mut ToolCall) -> Result<(), String> {
        for p in self.enabled(config) {
            p.tool_before(call)?;
        }
        Ok(())
    }

    pub fn tool_after(
        &self,
        config: &Config,
        call: &ToolCall,
        output: &mut String,
        is_error: &mut bool,
    ) {
        for p in self.enabled(config) {
            p.tool_after(call, output, is_error);
        }
    }

    pub fn shell_before(&self, cmd: &mut ShellCommand) {
        for p in self.live() {
            p.shell_before(cmd);
        }
    }

    pub fn request(&self, config: &Config, session_id: &str, req: &mut codesmith_ai::Request) {
        for p in self.enabled(config) {
            p.request(session_id, req);
        }
    }

    pub fn permission(&self, req: &Request) -> Option<Reply> {
        self.live().find_map(|p| p.permission(req))
    }

    pub fn event(&self, config: &Config, e: &Event) {
        for p in self.enabled(config) {
            p.event(e);
        }
    }

    pub fn idle(&self, config: &Config, idle: &Idle) -> Option<String> {
        if idle.continued >= MAX_CONTINUATIONS {
            return None;
        }
        self.enabled(config).find_map(|p| p.idle(idle))
    }
}
