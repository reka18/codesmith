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
//! hooks around tool calls, shell commands, model requests, compactions,
//! permission prompts, session events and going idle.

mod context;
pub mod goal;
pub mod host;
pub(crate) mod idle;
mod request;

/// The message types the context hooks take.
pub use codesmith_ai::{Message, Part, Role};
pub use context::{Context, ContextMessage, ContextTool, Prompt};
pub use host::{AssertPermission, Host, HostError, PermissionError};
pub(crate) use idle::Generate;
pub use idle::{Background, GenerateError, Hook, IdleAction, SessionIdle};
pub use request::{RequestKind, RequestScope};
/// The HTTP types the request hooks take.
pub use reqwest;

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

/// What a plugin command gets when it runs (OpenCode's command invocation).
#[derive(Debug, Clone)]
pub struct CommandContext {
    pub session_id: String,
    pub cwd: PathBuf,
    pub root: PathBuf,
    /// Everything after `/name`.
    pub args: String,
    /// Images and files attached to the message.
    pub media: Vec<codesmith_ai::Media>,
}

/// A command a plugin runs as code (OpenCode's callback commands), invoked
/// as `/name args` like any other command. Config commands of the same
/// name replace it.
pub trait Command: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> Option<&str> {
        None
    }
    /// Run it: `Ok(Some(text))` sends `text` (with the attachments) to the
    /// session as a prompt, `Ok(None)` ends the command there, and `Err`
    /// fails it with that message.
    fn execute<'a>(&'a self, ctx: CommandContext) -> BoxFuture<'a, Result<Option<String>, String>>;
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

/// A compaction about to summarize the session (mutable in
/// [`Plugin::compaction`]), OpenCode's `compaction` hook.
#[derive(Debug, Clone)]
pub struct Compaction {
    pub session_id: String,
    pub agent: String,
    /// The session's model spec.
    pub model: String,
    /// The summary request as the session sends it: its system prompt,
    /// tools and the whole conversation, without the summary instructions,
    /// which are added after the hooks. Change it to shape the summary
    /// request; a change every request gets alike (see [`Plugin::request`])
    /// keeps it on the prompt cache.
    pub request: codesmith_ai::Request,
    /// Set it to supply the summary yourself: no model request is made.
    pub result: Option<Summary>,
}

/// A summary a plugin supplies in place of the model's.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub summary: String,
    /// What producing it cost, counted in the session's usage.
    pub usage: Option<Usage>,
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

    /// Commands run as code.
    fn commands(&self) -> Vec<Arc<dyn Command>> {
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

    /// Before a compaction summarizes: change the summary request, or
    /// supply the summary in `result`. Runs before [`Plugin::request`],
    /// which compaction requests get too (with
    /// `interaction: Interaction::Compaction`); a supplied summary skips both
    /// the request hooks and the model.
    fn compaction(&self, _compaction: &mut Compaction) {}

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

    // --- Session context hooks (OpenCode's session `prompt` and
    // `context`). ---

    /// A user prompt is about to join the session: change its text, or
    /// keep metadata with it for later context hooks (never sent to the
    /// model).
    fn prompt(&self, _prompt: &mut Prompt) {}

    /// Just before a step (or `Session::generate`) builds its request:
    /// change the system prompt, the messages, or which tools are offered
    /// and under what names. Edits reach this request only; the session's
    /// history is not changed. Runs before [`Plugin::request`].
    fn context(&self, _context: &mut Context) {}

    // --- HTTP and WebSocket hooks (OpenCode's session `http.request`,
    // `http.response` and `experimental.ws.*`). They run for every model
    // request a session makes (`scope.kind` says which), after the
    // provider's credentials are applied. Responses and inbound frames come
    // from the network: treat them as untrusted input. ---

    /// Change a model request just before it is sent: headers, URL or body.
    /// A request left alone is sent as it was built.
    fn http_request(&self, _scope: &RequestScope, _req: &mut reqwest::Request) {}

    /// Look at, or replace, a model response before it is read.
    fn http_response(&self, _scope: &RequestScope, _res: &mut reqwest::Response) {}

    /// Change the URL or headers a Responses WebSocket opens with; a change
    /// opens a new socket.
    fn ws_handshake(
        &self,
        _scope: &RequestScope,
        _url: &mut String,
        _headers: &mut Vec<(String, String)>,
    ) {
    }

    /// Change an outbound WebSocket frame; the replacement is sent verbatim.
    fn ws_send(&self, _scope: &RequestScope, _frame: &mut String) {}

    /// Change an inbound WebSocket frame before it is decoded.
    fn ws_receive(&self, _scope: &RequestScope, _frame: &mut String) {}

    // --- Session lifecycle hooks (OpenCode's session `experimental.idle`
    // and execution interrupts). ---

    /// Whether this plugin's `hook` has anything to do for `session_id`, so
    /// CodeSmith builds that hook's input (a copy of the history for
    /// `context`, the HTTP bridge for `http_*`) only when some plugin
    /// wants it. The default is yes.
    fn wants(&self, _hook: Hook, _session_id: &str) -> bool {
        true
    }

    /// The session finished its work and would go idle, background shells
    /// perhaps still running: continue it with a message, compact it and
    /// ask again, or let it go idle. Runs after [`Plugin::idle`] declined,
    /// and not after an error or a stop. CodeSmith bounds how often one
    /// turn continues ([`MAX_IDLE_CONTINUATIONS`]).
    fn session_idle<'a>(&'a self, _idle: &'a SessionIdle<'a>) -> BoxFuture<'a, IdleAction> {
        Box::pin(async { IdleAction::Idle })
    }

    /// The session's turn was stopped before it finished (the user
    /// interrupted it, or its caller dropped it).
    fn interrupted(&self, _session_id: &str) {}
}

/// How many times `idle` may continue one turn.
pub const MAX_CONTINUATIONS: u32 = 5;

/// How many times `session_idle` may continue one turn (a goal loop runs
/// up to 50 rounds).
pub const MAX_IDLE_CONTINUATIONS: u32 = 100;

#[derive(Default)]
pub struct Registry {
    plugins: Vec<Arc<dyn Plugin>>,
    /// Plugins the most recently loaded config turned off, for the hooks
    /// that run without a config at hand (permission, shell).
    disabled: RwLock<std::collections::HashSet<String>>,
}

static ACTIVE: RwLock<Option<Arc<Registry>>> = RwLock::new(None);

static HOST: RwLock<Option<Arc<Host>>> = RwLock::new(None);

/// The plugin host of this process: the session store at
/// [`crate::store::default_path`] (opened on first use), creating sessions
/// in the current folder, unless [`set_host`] installed another.
pub fn host() -> Result<Arc<Host>, HostError> {
    if let Some(h) = HOST.read().unwrap().clone() {
        return Ok(h);
    }
    let mut slot = HOST.write().unwrap();
    if let Some(h) = slot.clone() {
        return Ok(h);
    }
    let path = crate::store::default_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let h = Arc::new(Host::open(&path, cwd)?);
    *slot = Some(h.clone());
    Ok(h)
}

/// Whether this process has a plugin host yet (one was installed or used):
/// without one, nothing can have admitted a compaction.
pub(crate) fn host_in_use() -> bool {
    HOST.read().unwrap().is_some()
}

/// Use `host` as this process's plugin host (tests, and embedders with
/// their own store).
pub fn set_host(host: Arc<Host>) {
    *HOST.write().unwrap() = Some(host);
}

/// The plugins every CodeSmith build has: the goal plugin (`/goal`).
/// `codesmith::main` installs them before a build's own.
pub fn builtin() -> Vec<Arc<dyn Plugin>> {
    vec![Arc::new(goal::GoalPlugin::new())]
}

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

    /// Commands from enabled plugins; a later plugin's replaces an earlier
    /// one's of the same name.
    pub fn commands(&self, config: &Config) -> Vec<Arc<dyn Command>> {
        let mut out: Vec<Arc<dyn Command>> = Vec::new();
        for c in self.enabled(config).flat_map(|p| p.commands()) {
            out.retain(|o| o.name() != c.name());
            out.push(c);
        }
        out
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

    /// Run the compaction hooks in order; the first plugin to supply a
    /// summary ends them.
    pub fn compaction(&self, config: &Config, compaction: &mut Compaction) {
        for p in self.enabled(config) {
            p.compaction(compaction);
            if compaction.result.is_some() {
                return;
            }
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

    /// Whether any plugin is enabled, so a hook needs its input built.
    pub fn any(&self, config: &Config) -> bool {
        self.enabled(config).next().is_some()
    }

    /// Whether any enabled plugin wants `hook` for `session_id`.
    pub fn wants(&self, config: &Config, hook: Hook, session_id: &str) -> bool {
        self.enabled(config).any(|p| p.wants(hook, session_id))
    }

    /// The enabled plugins that want `hook` for `session_id`.
    pub fn wanting(&self, config: &Config, hook: Hook, session_id: &str) -> Vec<Arc<dyn Plugin>> {
        self.enabled(config)
            .filter(|p| p.wants(hook, session_id))
            .cloned()
            .collect()
    }

    /// The enabled plugins, for a hook that runs after `config` is gone.
    pub fn enabled_list(&self, config: &Config) -> Vec<Arc<dyn Plugin>> {
        self.enabled(config).cloned().collect()
    }

    pub fn prompt(&self, config: &Config, prompt: &mut Prompt) {
        for p in self.enabled(config) {
            if p.wants(Hook::Prompt, &prompt.session_id) {
                p.prompt(prompt);
            }
        }
    }

    pub fn context(&self, config: &Config, context: &mut Context) {
        for p in self.enabled(config) {
            if p.wants(Hook::Context, &context.session_id) {
                p.context(context);
            }
        }
    }

    /// The provider hooks that run the enabled plugins' HTTP and WebSocket
    /// hooks for one request; `None` without plugins, so a build without
    /// any pays nothing.
    pub fn http_hooks(&self, config: &Config, scope: RequestScope) -> Option<codesmith_ai::Hooks> {
        let plugins = self.wanting(config, Hook::Http, &scope.session_id);
        (!plugins.is_empty())
            .then(|| codesmith_ai::Hooks(Arc::new(request::Bridge { plugins, scope })))
    }

    pub fn idle(&self, config: &Config, idle: &Idle) -> Option<String> {
        if idle.continued >= MAX_CONTINUATIONS {
            return None;
        }
        self.enabled(config).find_map(|p| p.idle(idle))
    }
}
