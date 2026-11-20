//! The agent loop: send the conversation, stream the reply, run any tool
//! calls, feed the results back, repeat until the model stops calling tools.
//! Every message is saved to the session store as it happens, history is
//! compacted near the context limit, and new sessions get a title.
//!
//! Pinned to OpenCode's session runner by `tests/runner_parity.rs` (ports of
//! `session-runner*.test.ts`, `session-revert.test.ts` and the subagent
//! tests); the loop's texts and rules live in `runner.rs` and `retry.rs`.
//! Deliberate differences, beyond those listed there:
//! - Subagents run to completion inside the tool call (OpenCode runs them in
//!   the background and reports later), so there are no steers, queues,
//!   background-subagent reports or restart recovery.
//! - A declined permission or dismissed question ends the turn normally
//!   (`stopped()` says why) instead of interrupting it; calls after the
//!   declined one in the same response are not run.
//! - Retries never consult `retry-after`; see `retry.rs`.
//! - Background commands: as in OpenCode, a turn ends when the model
//!   answers, even while its background commands run. A command that
//!   finishes is reported with [`Session::resume_background`] (OpenCode
//!   resumes the session by itself) or else at the next prompt. One-shot
//!   callers wait with [`Session::settle_background`], as OpenCode's `run`
//!   does; a subagent waits too before it reports, since it cannot be
//!   resumed later. Dropping a session stops the commands it still runs.

mod compact;

use crate::agent::{self, Agent};
use crate::compaction;
use crate::config::{Loaded, ModelRef};
use crate::permission::{Asker, Gate};
use crate::snapshot::{Change, Snapshots};
use crate::store::{self, Info, Record, Store};
use crate::title;
use crate::tool::todo::Todo;
use crate::tool::{self, Ctx, Tool};
use crate::{instructions, paths, prompt, retry, runner};
use anyhow::{anyhow, Result};
use codesmith_ai::{Event, FinishReason, Message, Part, Request, Role, Usage};
use futures_util::StreamExt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Where the loop reports progress (the CLI prints; tests record).
pub trait Sink: Send {
    fn text(&mut self, delta: &str);
    fn reasoning(&mut self, _delta: &str) {}
    /// An image (or other file) the model generated, as it arrives; it is
    /// kept as an assistant [`Part::Media`].
    fn media(&mut self, _media: &codesmith_ai::Media) {}
    /// A tool call started streaming its arguments; its `tool_start` comes
    /// once the response is complete and the call runs.
    fn tool_input(&mut self, _id: &str, _name: &str) {}
    /// A call's arguments finished streaming: `call` is the id of the call
    /// they formed (which can differ from the streamed `id`: Gemini via Chat
    /// appends its signature), or `None` when the call was dropped (cut off
    /// by the output limit or a content filter, or the stream failed). A
    /// formed call may still not run, when the turn stops first.
    fn tool_input_end(&mut self, _id: &str, _call: Option<&str>) {}
    fn tool_start(&mut self, _id: &str, _name: &str, _input: &serde_json::Value) {}
    fn tool_end(&mut self, _id: &str, _name: &str, _output: &str, _is_error: bool) {}
    /// A built-in `edit`, `write` or `patch` call `id` changed `path`;
    /// `before` is its text before (`None`: it was new), read once the
    /// change was allowed. Sent before `tool_end`, after a failed patch too.
    fn tool_change(&mut self, _id: &str, _path: &Path, _before: Option<&str>) {}
    /// Images and PDFs a call returned, sent just before its `tool_end`.
    fn tool_media(&mut self, _id: &str, _media: &[codesmith_ai::Media]) {}
    /// A running call's metadata (OpenCode's tool progress), such as the
    /// `shellID` of the command a `shell` call started, sent once it runs.
    fn tool_progress(&mut self, _id: &str, _metadata: &serde_json::Value) {}
    /// Output a running foreground command printed since the last report,
    /// a few times a second, until its call ends or moves it to the
    /// background. Under a flood only the latest output is sent.
    fn tool_output(&mut self, _id: &str, _delta: &str) {}
    fn step_end(&mut self, _reason: &FinishReason, _usage: &Usage) {}
    fn retry(&mut self, _attempt: u32, _error: &str, _wait: Duration) {}
    /// A compaction started: its summary streams through
    /// `compaction_delta`, and it ends with `compaction_end`.
    fn compaction_start(&mut self) {}
    /// Summary text as the model writes it. A retried or nudged summary
    /// streams again from the start; `compaction_end` has the final one.
    fn compaction_delta(&mut self, _delta: &str) {}
    /// The compaction ended: `Ok` with the summary kept (then `compacted`
    /// follows), or `Err` with why it failed.
    fn compaction_end(&mut self, _outcome: Result<&str, &str>) {}
    fn compacted(&mut self, _before: u64, _after: u64) {}
    fn todos(&mut self, _todos: &[Todo]) {}
    /// Something worth telling the user that is not part of the answer.
    fn notice(&mut self, _message: &str) {}
    /// A subagent session started. Until its `child_end`, the tool calls
    /// this sink sees are its own (or its subagents'), and its text and
    /// reasoning arrive as `child_text` and `child_reasoning`.
    fn child_start(&mut self, _child: &ChildSession) {}
    /// The subagent `id` ended: `completed`, `interrupted` or `failed`
    /// (with why).
    fn child_end(&mut self, _id: &str, _status: &str, _error: Option<&str>) {}
    fn child_text(&mut self, _delta: &str) {}
    fn child_reasoning(&mut self, _delta: &str) {}
}

/// A subagent session, as its parent's sink sees it start.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildSession {
    pub id: String,
    /// The session that started it: the top session, or a subagent.
    pub parent_id: String,
    /// 1 for a subagent of the top session.
    pub depth: u32,
    pub title: String,
}

fn forward_progress(sink: &mut dyn Sink, id: &str, p: tool::Progress) {
    match p {
        tool::Progress::Metadata(m) => sink.tool_progress(id, &m),
        tool::Progress::Output(o) => sink.tool_output(id, &o),
        tool::Progress::Change(_) => {}
    }
}

/// Relays a subagent's tool activity to the parent's sink under a label.
struct ChildSink<'a> {
    parent: &'a mut dyn Sink,
    label: String,
}

impl Sink for ChildSink<'_> {
    fn text(&mut self, delta: &str) {
        self.parent.child_text(delta);
    }
    fn reasoning(&mut self, delta: &str) {
        self.parent.child_reasoning(delta);
    }
    fn child_text(&mut self, delta: &str) {
        self.parent.child_text(delta);
    }
    fn child_reasoning(&mut self, delta: &str) {
        self.parent.child_reasoning(delta);
    }
    fn child_start(&mut self, child: &ChildSession) {
        self.parent.child_start(child);
    }
    fn child_end(&mut self, id: &str, status: &str, error: Option<&str>) {
        self.parent.child_end(id, status, error);
    }
    fn tool_start(&mut self, id: &str, name: &str, input: &serde_json::Value) {
        self.parent
            .tool_start(id, &format!("{} › {name}", self.label), input);
    }
    fn tool_end(&mut self, id: &str, name: &str, output: &str, is_error: bool) {
        self.parent
            .tool_end(id, &format!("{} › {name}", self.label), output, is_error);
    }
    fn tool_media(&mut self, id: &str, media: &[codesmith_ai::Media]) {
        self.parent.tool_media(id, media);
    }
    fn tool_change(&mut self, id: &str, path: &Path, before: Option<&str>) {
        self.parent.tool_change(id, path, before);
    }
    fn tool_progress(&mut self, id: &str, metadata: &serde_json::Value) {
        self.parent.tool_progress(id, metadata);
    }
    fn tool_output(&mut self, id: &str, delta: &str) {
        self.parent.tool_output(id, delta);
    }
    fn retry(&mut self, attempt: u32, error: &str, wait: Duration) {
        self.parent.retry(attempt, error, wait);
    }
}

/// How deep subagents may nest unless `subagent_depth` says otherwise: a
/// subagent cannot start subagents of its own (OpenCode's default).
pub const MAX_DEPTH: u32 = 1;

pub const MAX_RETRIES: u32 = retry::MAX_RETRIES;
/// Steps for an agent without a `steps` setting; the last one is forced to
/// answer in text.
pub const DEFAULT_MAX_STEPS: u32 = 200;

/// Variant names that are reasoning efforts (OpenCode's `ReasoningEfforts`).
const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The model spec a stored session's history last ran with: its latest
/// visible step's, with the variant of an effort update stored after that
/// step (written for a turn that then made no step).
fn last_step_model(entries: &[store::Entry]) -> Option<String> {
    let hidden = store::hidden_ranges(entries);
    let mut update: Option<Option<String>> = None;
    for e in entries.iter().rev() {
        if hidden
            .iter()
            .any(|(from, to)| e.seq >= *from && e.seq < *to)
        {
            continue;
        }
        match &e.record {
            Record::Message { message } if update.is_none() => {
                if let (Role::System, [Part::Effort { effort, .. }]) =
                    (&message.role, message.parts.as_slice())
                {
                    update = Some(effort.clone());
                }
            }
            Record::Step { model, .. } => {
                return Some(match update {
                    None => model.clone(),
                    Some(effort) => {
                        let base = crate::config::split_variant(model).0;
                        match effort {
                            Some(v) => format!("{base}#{v}"),
                            None => base.to_string(),
                        }
                    }
                });
            }
            _ => {}
        }
    }
    None
}

/// The [`Store::set_meta`] key of a session's unbilled usage (see
/// [`Session::spend`]).
const UNBILLED_META: &str = "unbilled_usage";

/// The unbilled part of stored session `id`'s `usage`: kept beside it, or,
/// for a session stored before it was, all of it unless it carries a billed
/// cost ([`crate::stats::Spend::of_total`]).
fn stored_unbilled(st: &Store, id: &str, usage: &Usage) -> Usage {
    st.meta(id, UNBILLED_META)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_else(|| crate::stats::Spend::of_total(usage).unbilled)
}

/// A stored session being continued: its row and its history with sequence numbers.
type Resumed = (Info, Vec<(Option<i64>, Message)>);

pub struct Options {
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Continue this stored session instead of starting a new one.
    pub resume: Option<String>,
    /// Use this title instead of generating one.
    pub title: Option<String>,
    /// The session that delegated to this one, for subagents.
    pub parent: Option<String>,
    /// Connected MCP servers, shared across the sessions of one process.
    pub mcp: Option<Arc<crate::mcp::Mcp>>,
    /// Who answers the `question` tool; without one the tool is not offered.
    pub questioner: Option<Arc<dyn crate::question::Questioner>>,
}

/// Who wrote a prompt's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The user typed it (the TUI, `codesmith run`, a served or ACP
    /// client's message): its `@skill` mentions bring the skills in,
    /// asked for as the skill tool would.
    User,
    /// Anything else: a subagent task the model wrote, an automation's or
    /// Code Mode's prompt, an MCP server's prompt or event, a command's
    /// expanded text. `@skill` in it is plain text.
    Engine,
}

pub struct Session {
    /// Change it with [`Session::set_id`], which moves the session's
    /// tool-output allowance along.
    pub id: String,
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub agent: Agent,
    pub model: ModelRef,
    pub model_spec: String,
    /// The system prompt of the latest request: [`Session::base_system`]
    /// and the instruction baseline (see [`crate::instruction_state`]).
    pub system: String,
    /// The agent's own prompt, else the model's base prompt.
    base_system: String,
    /// The `<env>` block the environment instruction reads.
    env_block: String,
    /// The instruction baseline without the date and environment, which
    /// Claude Code writes itself (for Claude Agent models).
    claude_instructions: String,
    /// On a local server, the baseline's date ("Today's date: …"), sent as
    /// a system update before the history rather than at the end of the
    /// system prompt (where OpenCode has it, and where it stays for hosted
    /// providers), so the prefix the server caches (the system prompt, then
    /// the tools, in Qwen-style chat templates) does not change with the
    /// day. A date change during the session is an update in the history,
    /// as for any instruction.
    date_note: Option<String>,
    /// Today's date, as the date instruction reads it ([`prompt::today`]).
    today: Arc<dyn Fn() -> String + Send + Sync>,
    /// The instruction state; `None` until the next observation makes a
    /// baseline.
    instruction: Option<crate::instruction_state::State>,
    pub tools: Vec<Tool>,
    pub messages: Vec<Message>,
    /// Store sequence number of each message (`None` for a checkpoint).
    seqs: Vec<Option<i64>>,
    /// Metadata plugins' prompt hooks kept with messages, by sequence
    /// number (see [`crate::plugin::Prompt`]).
    metadata: std::collections::HashMap<i64, serde_json::Value>,
    /// The model spec the history last ran with (its latest step's), to
    /// tell a variant switch since then: see [`Session::effort_switch`].
    ran_with: Option<String>,
    pub usage: Usage,
    /// The part of `usage` from steps without a billed cost (priced from
    /// the model's configured cost), kept with the session so a reload
    /// can tell it from the billed part: see [`Session::spend`].
    unbilled: Usage,
    pub title: Option<String>,
    last_prompt_tokens: u64,
    gate: Gate,
    shell: String,
    limits: crate::config::ToolOutputConfig,
    client: reqwest::Client,
    /// The Responses WebSocket session steps reuse (see `ModelRef::websocket`).
    ws: Arc<codesmith_ai::ws::SessionTransport>,
    store: Option<Store>,
    stored: bool,
    title_task: Option<TitleTask>,
    /// A title waiting for the turn's first response: its first message
    /// and how it is made (see [`TitlePlan::defer`]).
    title_pending: Option<(String, TitlePlan)>,
    /// Whether the user was told that compaction cannot help (said once).
    compaction_skipped: bool,
    /// Whether a tool that can change files ran since the instructions
    /// were last observed (see [`Session::observe_instructions`]).
    agent_changed_files: bool,
    compact: compact::State,
    loaded: Loaded,
    parent: Option<String>,
    depth: u32,
    pub todos: Vec<Todo>,
    skills: Vec<crate::skill::Skill>,
    jobs: tool::shell::Jobs,
    questioner: Option<Arc<dyn crate::question::Questioner>>,
    mcp: Arc<crate::mcp::Mcp>,
    format: Option<Arc<crate::format::Formatter>>,
    lsp: Option<Arc<crate::lsp::Lsp>>,
    turn: Turn,
    shared: Shared,
    /// Whether the image and video generation tools are offered: a model
    /// of that type is reachable and config leaves the tool on.
    media_tools: [bool; 2],
}

/// What keeps a session consistent with other processes using the same
/// stored session (an automation run, another window).
struct Shared {
    /// The newest record sequence number this session has seen.
    seen: i64,
    /// Another writer appended in between this session's own records.
    stale: bool,
    /// This session's name in turn leases.
    holder: String,
    /// Wait for a turn another holder is running (else fail with `Busy`).
    wait: bool,
}

/// Per-session state of the running turn, kept apart from the rest.
struct Turn {
    /// Who answers permission requests, unwrapped (children get this one).
    asker: Arc<dyn Asker>,
    /// Set when the user declines a permission request without feedback.
    declined: Arc<AtomicBool>,
    /// Why the current turn stopped early, if it did.
    stopped: Option<&'static str>,
    /// Tool names offered to the model in the latest request, as offered.
    offered: Vec<String>,
    /// The names the latest request offered tools under (a context hook
    /// may have renamed some), and their input schemas there, for
    /// [`tool::input_repair`].
    schemas: Vec<(String, serde_json::Value)>,
    /// Tools a context hook renamed for the latest request: offered name,
    /// then the session's.
    renamed: std::collections::HashMap<String, String>,
    /// Scales retry waits (tests shorten them).
    retry_scale: f64,
    /// Resumes for finished background commands that failed in a row
    /// before reporting them, and when the next one may start.
    resume_failures: u32,
    resume_due: Option<std::time::Instant>,
    /// The turn already observed the instructions for its first step.
    observed: bool,
}

/// What a front end does for an idle session's background commands, from
/// [`Session::idle_wake`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleWake {
    /// Resume it now ([`Session::resume_background`]).
    Resume,
    /// Look again at this instant: resumes failed and back off.
    At(std::time::Instant),
    /// Look again when more than this many commands are finished and
    /// unreported ([`tool::shell::Reaper::wait_more`]).
    Finish(usize),
    /// Nothing runs or waits.
    Idle,
}

/// The longest wait between resumes that keep failing (see
/// [`Session::resume_due`]).
const RESUME_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The wait after `failures` resumes failed in a row: 1 s, doubling, at
/// most [`RESUME_BACKOFF_MAX`].
fn resume_backoff(failures: u32) -> Duration {
    Duration::from_secs(1)
        .saturating_mul(1u32 << failures.saturating_sub(1).min(16))
        .min(RESUME_BACKOFF_MAX)
}

/// Put the agent's request headers and body fields over the model's: a
/// numeric `temperature` sets the request's temperature, and the rest go
/// into the body as they are.
fn apply_agent_request(model: &mut ModelRef, agent: &Agent) {
    for (k, v) in &agent.request.headers {
        model
            .provider
            .headers
            .retain(|(h, _)| !h.eq_ignore_ascii_case(k));
        model.provider.headers.push((k.clone(), v.clone()));
    }
    for (k, v) in &agent.request.body {
        match (k.as_str(), v.as_f64()) {
            ("temperature", Some(t)) => model.temperature = Some(t as f32),
            _ => {
                model.body.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Passes permission questions through and notes a plain decline, which ends
/// the turn (OpenCode interrupts the run on one).
struct DeclineWatch {
    inner: Arc<dyn Asker>,
    declined: Arc<AtomicBool>,
}

impl Asker for DeclineWatch {
    fn ask(&self, req: &crate::permission::Request) -> crate::permission::Reply {
        let reply = self.inner.ask(req);
        if matches!(reply, crate::permission::Reply::Reject(None)) {
            self.declined.store(true, Ordering::SeqCst);
        }
        reply
    }
}

fn watch(asker: Arc<dyn Asker>, declined: &Arc<AtomicBool>) -> Arc<dyn Asker> {
    Arc::new(DeclineWatch {
        inner: asker,
        declined: declined.clone(),
    })
}

impl Session {
    pub fn new(
        loaded: &Loaded,
        cwd: PathBuf,
        opts: Options,
        asker: Arc<dyn Asker>,
        store: Option<Store>,
    ) -> Result<Session> {
        let config = &loaded.config;
        if let Some(s) = &store {
            s.set_media_blob(config.media.blob.unwrap_or(true));
        }
        let mut seen = 0;
        let mut ran_with = None;
        let mut instruction = None;
        let mut metadata = std::collections::HashMap::new();
        let resumed: Option<Resumed> = match (&opts.resume, &store) {
            (Some(id), Some(s)) => {
                let info = s.get(id)?.ok_or_else(|| anyhow!("no session {id}"))?;
                let entries = s.entries(id)?;
                seen = entries.last().map_or(0, |e| e.seq);
                ran_with = last_step_model(&entries);
                instruction = crate::instruction_state::fold(&entries);
                for e in &entries {
                    if let Record::Metadata { seq, metadata: m } = &e.record {
                        metadata.insert(*seq, m.clone());
                    }
                }
                // A native window replays only on the model that made it.
                let model = opts.model.as_deref().unwrap_or(&info.model);
                let history = store::history_for(&entries, Some(model));
                Some((info, history))
            }
            (Some(_), None) => {
                return Err(anyhow!(
                    "cannot continue a session without the session store"
                ))
            }
            _ => None,
        };
        let agent_id = opts
            .agent
            // A session a plugin created without one takes the default.
            .or_else(|| {
                resumed
                    .as_ref()
                    .map(|r| r.0.agent.clone())
                    .filter(|a| !a.is_empty())
            })
            .unwrap_or_else(|| agent::default_id(config));
        let agent = agent::resolve(config, &agent_id).ok_or_else(|| {
            anyhow!(
                "unknown agent {agent_id:?}; available: {}",
                agent::list(config).join(", ")
            )
        })?;
        let model_spec = opts
            .model
            .or_else(|| resumed.as_ref().map(|r| r.0.model.clone()).filter(|m| !m.is_empty()))
            .or_else(|| agent.model.clone())
            .or_else(|| config.model.clone())
            .ok_or_else(|| anyhow!("no model configured: pass -m provider/model or set \"model\" in codesmith.json"))?;
        let mut model = config.resolve_model(&model_spec)?;
        apply_agent_request(&mut model, &agent);
        let subagents = agent::subagents(config);
        let home = dirs::home_dir().unwrap_or_default();
        let skills = crate::skill::discover(&crate::skill::roots(
            &cwd,
            &loaded.root,
            &paths::config_dir(),
            &home,
            &config.skills,
        ));
        let mut tools = agent::tools(&agent);
        if subagents.is_empty() {
            tools.retain(|t| *t != Tool::Subagent);
        }
        if skills.is_empty() {
            tools.retain(|t| *t != Tool::Skill);
        }
        if opts.questioner.is_none() {
            tools.retain(|t| *t != Tool::Question);
        }
        if !config.websearch.enabled() {
            tools.retain(|t| *t != Tool::Websearch);
        }
        // GPT-5 and later edit with `patch`; other models with `edit` and `write`.
        tool::apply_patch::offer_for_spec(&mut tools, &model_spec, &model.model_id);
        let lsp = crate::lsp::Lsp::new(config.lsp.as_ref(), loaded.root.clone())
            .map(|l| l.watch_with(crate::watch::global(), config.watcher.ignore.clone()))
            .map(Arc::new);
        if lsp.is_none() {
            tools.retain(|t| *t != Tool::Lsp);
        }
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let tmp = paths::tmp_dir();
        let _ = std::fs::create_dir_all(&tmp);
        let env_block = prompt::env_block(&prompt::Environment {
            cwd: &cwd,
            root: &loaded.root,
            is_git: loaded.root.join(".git").exists(),
            tmp: &tmp,
            date: String::new(),
        });
        let base_system = match &agent.system {
            Some(s) => s.clone(),
            None => {
                // Chosen by the model's id in config or the catalog, as
                // OpenCode's model-lab prompts are.
                let base = crate::config::split_variant(&model_spec).0;
                let key = base.split_once('/').map_or(base, |(_, m)| m);
                prompt::base_named(key, &prompt::model_name(config, &model_spec), &names)
            }
        };
        // Until the first observation, the base alone.
        let system = assemble_system(&base_system, "", agent.id == "plan");
        let shell = crate::shell_select::resolve(
            crate::shell_select::Priority::Compat,
            config.shell.as_deref(),
        );
        let declined = Arc::new(AtomicBool::new(false));
        let (id, messages, seqs, usage, title, stored) = match resumed {
            Some((info, hist)) => {
                let (seqs, messages): (Vec<_>, Vec<_>) = hist.into_iter().unzip();
                (
                    info.id,
                    messages,
                    seqs,
                    info.usage,
                    info.title.or(opts.title),
                    true,
                )
            }
            None => (
                new_id(),
                Vec::new(),
                Vec::new(),
                Usage::default(),
                opts.title,
                false,
            ),
        };
        let unbilled = match (&store, stored) {
            (Some(st), true) => stored_unbilled(st, &id, &usage),
            _ => Usage::default(),
        };
        let mut rules = agent.permissions.clone();
        crate::permission::scope_tool_output(&mut rules, &id);
        let mut gate = Gate::new(rules, watch(asker.clone(), &declined));
        // "Always" answers are kept for the project beside its sessions.
        if let Some(book) = store
            .as_ref()
            .and_then(|s| s.path())
            .and_then(|p| crate::approvals::Book::open(&p).ok())
        {
            gate.set_durable(
                std::sync::Arc::new(book),
                crate::approvals::project_key(&loaded.root),
            );
        }
        Ok(Session {
            id,
            cwd,
            root: loaded.root.clone(),
            agent,
            model,
            model_spec,
            system,
            base_system,
            env_block,
            claude_instructions: String::new(),
            date_note: None,
            today: Arc::new(prompt::today),
            instruction,
            tools,
            messages,
            seqs,
            metadata,
            ran_with,
            usage,
            unbilled,
            title,
            last_prompt_tokens: 0,
            compaction_skipped: false,
            agent_changed_files: false,
            compact: Default::default(),
            gate,
            shell,
            limits: config.tool_output,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .build()?,
            ws: Arc::default(),
            store,
            stored,
            title_task: None,
            title_pending: None,
            loaded: loaded.clone(),
            parent: opts.parent,
            depth: 0,
            todos: Vec::new(),
            skills,
            jobs: tool::shell::Jobs::default(),
            questioner: opts.questioner,
            mcp: opts.mcp.unwrap_or_else(crate::mcp::Mcp::empty),
            format: {
                let f = crate::format::Formatter::new(
                    config.formatter.as_ref(),
                    loaded.root.clone(),
                    loaded.root.clone(),
                );
                f.enabled().then(|| Arc::new(f))
            },
            lsp,
            turn: Turn {
                asker,
                declined,
                stopped: None,
                offered: Vec::new(),
                schemas: Vec::new(),
                renamed: Default::default(),
                retry_scale: 1.0,
                resume_failures: 0,
                resume_due: None,
                observed: false,
            },
            shared: Shared {
                seen,
                stale: false,
                holder: format!("{}-{}", std::process::id(), new_id()),
                wait: true,
            },
            media_tools: [tool::image_generate::offered(config), false],
        })
    }

    /// Whether a turn waits while another process runs one in this session
    /// (the default), or fails at once with [`store::Busy`] (the automation
    /// scheduler, which defers the run instead).
    pub fn set_wait_for_turn(&mut self, wait: bool) {
        self.shared.wait = wait;
    }

    /// Take the session's turn lease, so one turn runs at a time across
    /// every process sharing the store, then pick up records another
    /// process appended since this session last looked (an automation run
    /// sent to it, a turn from another window).
    async fn take_turn(&mut self, sink: &mut dyn Sink) -> Result<Option<store::TurnLease>> {
        let Some(path) = self.store.as_ref().and_then(|s| s.path()) else {
            return Ok(None);
        };
        let lease = store::TurnLease::acquire(
            &path,
            &self.id,
            &self.shared.holder,
            self.shared.wait,
            || sink.notice("Waiting for a turn running in this session elsewhere to finish"),
        )
        .await?;
        let st = self.store.as_ref().expect("checked above");
        if self.stored {
            let last = st.last_seq(&self.id)?;
            if self.shared.stale || last > self.shared.seen {
                let entries = st.entries(&self.id)?;
                let (seqs, messages) = store::history_for(&entries, Some(&self.model_spec))
                    .into_iter()
                    .unzip();
                self.messages = messages;
                self.seqs = seqs;
                self.ran_with = last_step_model(&entries);
                self.instruction = crate::instruction_state::fold(&entries);
                if let Some(info) = st.get(&self.id)? {
                    self.unbilled = stored_unbilled(st, &self.id, &info.usage);
                    self.usage = info.usage;
                }
                self.shared.seen = last;
                self.shared.stale = false;
            }
        }
        Ok(Some(lease))
    }

    /// The effort update a variant switch since the history last ran makes
    /// (OpenCode lowers its `model-switched` message the same way): on the
    /// same model, from one effort variant (or the default) to another.
    /// Routes that take it natively keep the request's top-level effort,
    /// and with it the cached prefix, as it was; the others drop it and
    /// send the new effort. Other switches (another model, or a variant
    /// that is not an effort name) make none.
    pub fn effort_switch(&self) -> Option<Message> {
        if self.messages.is_empty() {
            return None;
        }
        let (from_model, from) = crate::config::split_variant(self.ran_with.as_deref()?);
        let (to_model, to) = crate::config::split_variant(&self.model_spec);
        let effort = |v: Option<&str>| match v {
            None | Some("default") => Some(None),
            Some(v) if EFFORTS.contains(&v) => Some(Some(v.to_string())),
            Some(_) => None,
        };
        let (effort, previous) = (effort(to)?, effort(from)?);
        (from_model == to_model && effort != previous).then(|| Message {
            role: Role::System,
            parts: vec![Part::Effort { effort, previous }],
        })
    }

    /// Store the effort update of a variant switch, if there was one.
    fn note_effort_switch(&mut self) -> Result<()> {
        if let Some(m) = self.effort_switch() {
            self.push(m)?;
        }
        self.ran_with = Some(self.model_spec.clone());
        Ok(())
    }

    /// Add one call's usage to the session's: to the unbilled part too
    /// when the provider reported no billed cost for it.
    fn count_usage(&mut self, usage: &Usage) {
        add(&mut self.usage, usage);
        if usage.cost.is_none() {
            add(&mut self.unbilled, usage);
        }
    }

    /// Store the session's usage, with its unbilled part beside it.
    fn save_usage(&self, st: &Store) {
        let _ = st.set_usage(&self.id, &self.usage);
        if let Ok(json) = serde_json::to_string(&self.unbilled) {
            let _ = st.set_meta(&self.id, UNBILLED_META, Some(&json));
        }
    }

    /// What the session has spent: the cost providers billed, and the
    /// tokens of the steps they billed nothing for, to price. A session
    /// mixing both (a model switch, a provider billing some calls only)
    /// counts each part, also after a reload.
    pub fn spend(&self) -> crate::stats::Spend {
        crate::stats::Spend {
            billed: self.usage.cost,
            unbilled: self.unbilled,
        }
    }

    /// Save a message (creating the session row on first use) and keep it in memory.
    fn push(&mut self, message: Message) -> Result<()> {
        let seq = self.record(&Record::Message {
            message: message.clone(),
        })?;
        self.messages.push(message);
        self.seqs.push(seq);
        Ok(())
    }

    /// Run `command` as the user (OpenCode's `Session.shell`, `!command` at
    /// the prompt): no permission check and no model call. When it ends, its
    /// notification joins the history for the model's next step (see
    /// [`crate::user_shell`]). The command outlives a dropped call, and
    /// then records its notification itself; one that cannot start is
    /// recorded as such before the error.
    pub async fn shell(&mut self, command: &str) -> Result<crate::user_shell::Ran> {
        use crate::user_shell as us;
        let started = tool::shell::start_user(
            &self.shell,
            &self.id,
            &self.cwd,
            command,
            &self.jobs,
            self.limits,
        )
        .await;
        let handle = match started {
            Ok((_, h)) => h,
            Err(e) => {
                self.push(Message::user(us::failed(command, &e)))?;
                return Err(anyhow!(e));
            }
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let path = self.store.as_ref().and_then(|s| s.path());
        let (id, stored) = (self.id.clone(), self.stored);
        tokio::spawn(async move {
            let Ok(run) = handle.await else {
                return;
            };
            let text = us::notification(&run);
            if let Err((_, text)) = tx.send((run, text)) {
                if let Some(st) = path.filter(|_| stored).and_then(|p| Store::open(&p).ok()) {
                    let message = Message::user(text);
                    let _ = st.append(&id, &Record::Message { message });
                }
            }
        });
        let (run, text) = rx
            .await
            .map_err(|_| anyhow!("the command {command:?} could not be waited on"))?;
        self.push(Message::user(text.clone()))?;
        Ok(us::ran(&run, text))
    }

    /// The content of the skills `text` mentions (see
    /// [`crate::skill_mention`]), asked for like the skill tool's: fails
    /// for one the rules deny or the user refuses.
    fn mentioned_skills(&self, text: &str) -> Result<Vec<String>> {
        use crate::skill_mention as sm;
        let mentioned = sm::mentions(text, &self.skills, &self.cwd);
        for m in &mentioned {
            if self
                .gate
                .assert_not_denied("skill", std::slice::from_ref(&m.skill))
                .is_err()
            {
                return Err(sm::NotFound {
                    skill: m.skill.clone(),
                }
                .into());
            }
        }
        for m in &mentioned {
            let title = format!("Load the {} skill", m.skill);
            self.allowed("skill", &m.skill, title)
                .map_err(|e| anyhow!(e))?;
        }
        Ok(sm::contents(&mentioned, &self.skills))
    }

    /// Activate skill `id` on its own (OpenCode's `Session.skill`): its raw
    /// content joins the session for the model's next step, without a model
    /// call. Fails with [`crate::skill_mention::NotFound`] for a skill that
    /// is not available or that the rules deny, writing nothing.
    pub fn activate_skill(&mut self, id: &str) -> Result<()> {
        let skill = self
            .skills
            .iter()
            .find(|s| s.id == id)
            .filter(|_| {
                self.gate
                    .assert_not_denied("skill", &[id.to_string()])
                    .is_ok()
            })
            .ok_or_else(|| crate::skill_mention::NotFound { skill: id.into() })?;
        let text = skill.content.clone();
        self.push(Message::user(text))
    }

    /// A call as it runs: the name called, the name of the offered tool it
    /// reaches (another harness's name for an offered tool, see
    /// [`tool::input_repair::alias`]), the session's name for that tool (a
    /// context hook may have renamed it), and its input repaired against
    /// the schema it was offered with.
    fn repair_call(
        &self,
        name: String,
        input: serde_json::Value,
    ) -> (String, String, String, serde_json::Value) {
        // The name it was offered under: its own, or the one another
        // harness's name stands for (only a tool offered as itself, so an
        // alias never reaches a renamed or hidden tool).
        let offered = tool::input_repair::alias(&name, &self.turn.offered)
            .map_or_else(|| name.clone(), str::to_string);
        let ran = self
            .turn
            .renamed
            .get(&offered)
            .cloned()
            .unwrap_or_else(|| offered.clone());
        // Code Mode's input is trivial; the calls it makes are repaired.
        let schema = self
            .turn
            .schemas
            .iter()
            .find(|(n, _)| *n == offered && ran != crate::codemode::TOOL)
            .map(|(_, s)| s);
        let input = match schema {
            Some(s) if runner::invalid_json(&input).is_none() => {
                tool::input_repair::repair(&input, s)
            }
            _ => input,
        };
        (name, offered, ran, input)
    }

    /// After a step's tool results: the instruction files its reads bring
    /// in (see [`crate::nearby`]).
    fn give_nearby_instructions(&mut self) -> Result<()> {
        let n = self.messages.len();
        if n < 2 {
            return Ok(());
        }
        let texts = crate::nearby::after_step(
            &self.cwd,
            &self.root,
            &self.messages[n - 2],
            &self.messages[n - 1],
            &self.messages,
            &|names| self.gate.allows("read", names),
        );
        for text in texts {
            self.push(Message::user(text))?;
        }
        Ok(())
    }

    /// The store sequence number of each of [`Session::messages`] (`None`
    /// for a checkpoint): a message's id, as a fork boundary takes it.
    pub fn seqs(&self) -> &[Option<i64>] {
        &self.seqs
    }

    /// Give the session another id (a host that names its sessions itself
    /// before the first record is written). The rule letting it read its own
    /// saved tool output moves to the new id's directory: assigning
    /// [`Session::id`] directly leaves it on the generated one, so reading
    /// that output asks for `external_directory`.
    pub fn set_id(&mut self, id: impl Into<String>) {
        let id = id.into();
        self.gate.rescope_tool_output(&self.id, &id);
        self.id = id;
    }

    /// The "always" approvals given in this session, shared with it.
    pub fn approvals(&self) -> crate::permission::Approvals {
        self.gate.approvals()
    }

    /// Keep `approvals` (from an earlier copy of this session, before it
    /// was reloaded) in force here; later ones join them.
    pub fn share_approvals(&mut self, approvals: crate::permission::Approvals) {
        self.gate.share_approvals(approvals);
    }

    /// Replace who answers this session's permission requests.
    pub fn set_asker(&mut self, asker: Arc<dyn crate::permission::Asker>) {
        self.gate
            .set_asker(watch(asker.clone(), &self.turn.declined));
        self.turn.asker = asker;
    }

    /// Scale the waits between retried model calls (tests use a tiny factor).
    pub fn set_retry_scale(&mut self, scale: f64) {
        self.turn.retry_scale = scale;
    }

    /// Why the last turn stopped before the model finished, if it did: the
    /// user declined a tool call or dismissed a question.
    pub fn stopped(&self) -> Option<&'static str> {
        self.turn.stopped
    }

    /// Record a command and its result as a user/assistant exchange (used by
    /// native commands such as `/review`), so later turns can refer to it.
    pub fn append_exchange(&mut self, user: &str, assistant: &str, usage: &Usage) -> Result<()> {
        self.push(Message::user(user))?;
        self.push(Message {
            role: Role::Assistant,
            parts: vec![Part::text(assistant)],
        })?;
        self.count_usage(usage);
        if self.title.is_none() {
            self.title = Some(user.chars().take(60).collect());
        }
        if let Some(st) = &self.store {
            self.save_usage(st);
            if let Some(t) = &self.title {
                let _ = st.set_title(&self.id, t);
            }
        }
        Ok(())
    }

    /// Append a record, creating the session row on first use.
    fn record(&mut self, record: &Record) -> Result<Option<i64>> {
        let Some(s) = &self.store else {
            return Ok(None);
        };
        if !self.stored {
            let t = store::now();
            s.create(&Info {
                id: self.id.clone(),
                directory: self.cwd.display().to_string(),
                title: self.title.clone(),
                agent: self.agent.id.clone(),
                model: self.model_spec.clone(),
                parent_id: self.parent.clone(),
                created: t,
                updated: t,
                usage: Usage::default(),
            })?;
            self.stored = true;
        }
        let seq = s.append(&self.id, record)?;
        if seq != self.shared.seen + 1 {
            self.shared.stale = true;
        }
        self.shared.seen = seq;
        Ok(Some(seq))
    }

    fn request(&self) -> (Request, std::collections::HashMap<String, String>) {
        self.request_for(crate::plugin::RequestKind::Primary)
    }

    /// The request a `kind` model call starts from, after the plugins'
    /// context and request hooks, and the tools a context hook renamed
    /// (offered name, then the session's).
    fn request_for(
        &self,
        kind: crate::plugin::RequestKind,
    ) -> (Request, std::collections::HashMap<String, String>) {
        let plugins = crate::plugin::active();
        let config = &self.loaded.config;
        let mut req = self.base_request(&plugins);
        let mut renamed = std::collections::HashMap::new();
        if plugins.wants(config, crate::plugin::Hook::Context, &self.id) {
            use crate::plugin::{Context, ContextMessage};
            let mut ctx = Context {
                session_id: self.id.clone(),
                agent: self.agent.id.clone(),
                model: self.model_spec.clone(),
                kind,
                system: std::mem::take(&mut req.system),
                messages: self
                    .messages
                    .iter()
                    .zip(&self.seqs)
                    .map(|(m, seq)| ContextMessage {
                        message: m.clone(),
                        metadata: seq.and_then(|s| self.metadata.get(&s).cloned()),
                    })
                    .collect(),
                tools: req.tools.iter().map(Context::tool).collect(),
            };
            plugins.context(config, &mut ctx);
            req.system = ctx.system;
            // Metadata stays behind: it is never sent.
            let messages: Vec<Message> = ctx.messages.into_iter().map(|m| m.message).collect();
            req.messages = self.sendable_history(&messages);
            let known = std::mem::take(&mut req.tools);
            for t in ctx.tools {
                if !known.iter().any(|k| k.name == t.original) {
                    continue;
                }
                if t.name != t.original {
                    renamed.insert(t.name.clone(), t.original.clone());
                }
                req.tools.push(codesmith_ai::ToolSpec {
                    name: t.name,
                    description: t.description,
                    parameters: t.parameters,
                });
            }
        }
        plugins.request(config, &self.id, &mut req);
        if self.model.provider.protocol == codesmith_ai::Protocol::ClaudeAgent {
            self.claude_context(kind, &mut req);
        }
        (req, renamed)
    }

    /// What a Claude Agent request carries beside the conversation
    /// (OpenCode's session and instruction hooks for the provider): its
    /// kind, a fork's source, the agent's own prompt as its role,
    /// CodeSmith's instructions (Claude Code's own prompt replaces
    /// CodeSmith's base prompt, and it writes the date and environment
    /// itself), the tool calls that were stopped rather than run, and the
    /// session's folder.
    pub(crate) fn claude_context(&self, kind: crate::plugin::RequestKind, req: &mut Request) {
        use codesmith_ai::claude_agent as ca;
        req.headers
            .push((ca::KIND_HEADER.into(), kind.name().into()));
        if kind == crate::plugin::RequestKind::Primary {
            let origin = self
                .store
                .as_ref()
                .and_then(|st| st.meta(&self.id, crate::fork::ORIGIN_META).ok().flatten())
                .and_then(|o| serde_json::from_str::<crate::fork::Origin>(&o).ok());
            if let Some(o) = origin {
                req.headers.push((ca::FORK_HEADER.into(), o.session));
            }
        }
        let mut instructions = self.claude_instructions.clone();
        // What plugins appended to the system prompt.
        if let Some(extra) = req.system.strip_prefix(self.system.as_str()) {
            let extra = extra.trim_start_matches('\n');
            if !extra.is_empty() {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(extra);
            }
        }
        let o = &mut req.options;
        o.set(
            ca::AGENT_OPTION,
            self.agent.system.clone().unwrap_or_default().into(),
        );
        o.set(ca::INSTRUCTIONS_OPTION, instructions.into());
        o.set(ca::ABORTED_OPTION, aborted_calls(&req.messages).into());
        o.set(ca::DIRECTORY_OPTION, self.cwd.display().to_string().into());
    }

    /// Plugin tools this agent may use (hidden when its rules deny them).
    fn plugin_tools(&self, plugins: &crate::plugin::Registry) -> Vec<Arc<dyn crate::plugin::Tool>> {
        let rules = &self.agent.permissions;
        plugins
            .tools(&self.loaded.config)
            .into_iter()
            .filter(|t| {
                crate::permission::evaluate(t.name(), "*", rules) != crate::permission::Effect::Deny
            })
            .collect()
    }

    fn base_request(&self, plugins: &crate::plugin::Registry) -> Request {
        let code = self.code_entries();
        Request {
            model: self.model.model_id.clone(),
            system: self.system_prompt(&code),
            messages: self.sendable_history(&self.messages),
            tools: self.tool_specs(plugins, &code),
            namespaces: Vec::new(),
            max_tokens: self.model.max_output,
            temperature: self.model.temperature,
            options: self.model.options.clone(),
            body: self.model.body.clone(),
            cache: self.model.cache.clone(),
            // Stable for the session (a subagent shares its parent's), so
            // its requests reach the same cache.
            prompt_cache_key: Some(self.tag().cache_key),
            tool_choice: None,
            parallel_tool_calls: None,
            interaction: match self.parent {
                Some(_) => codesmith_ai::Interaction::Subagent,
                None => codesmith_ai::Interaction::Agent,
            },
            headers: self.tag().headers,
        }
    }

    /// What every model request of this session carries: its correlation
    /// headers and prompt cache key (see [`Tag`]).
    fn tag(&self) -> Tag {
        Tag::new(&self.id, self.parent.as_deref())
    }

    /// The model, with the plugins' HTTP hooks for a `kind` request when
    /// any plugin is enabled.
    fn model_for(&self, kind: crate::plugin::RequestKind) -> std::borrow::Cow<'_, ModelRef> {
        let scope = crate::plugin::RequestScope {
            session_id: self.id.clone(),
            agent: self.agent.id.clone(),
            model: self.model_spec.clone(),
            kind,
        };
        match crate::plugin::active().http_hooks(&self.loaded.config, scope) {
            Some(h) => {
                let mut m = self.model.clone();
                m.provider.hooks = Some(h);
                std::borrow::Cow::Owned(m)
            }
            None => std::borrow::Cow::Borrowed(&self.model),
        }
    }

    /// The system prompt: the base and the instruction baseline, Code Mode
    /// and MCP server instructions among them.
    fn system_prompt(&self, _code: &[crate::codemode::Entry]) -> String {
        self.system.clone()
    }

    /// The instruction sources, read fresh: most shared across sessions
    /// first (OpenCode's order), so the baseline stays a reusable cached
    /// prefix: the Code Mode catalog, MCP server instructions, references,
    /// skills, instruction files, then the date and environment, and the
    /// context a host attached.
    fn instruction_sources(&mut self) -> Result<crate::instruction_state::List> {
        let config = &self.loaded.config;
        let code = self.code_entries();
        let codemode = (!code.is_empty()).then(|| crate::codemode_catalog::inventory(&code));
        let mut lists = vec![
            crate::codemode_catalog::source(codemode, Some(crate::codemode_catalog::INLINE_BUDGET)),
            crate::mcp::instruction_source(&self.mcp_instructions(&code)),
        ];
        let references = crate::reference::configured(config, &self.root);
        crate::reference::refresh_in_background(references.clone());
        lists.push(crate::reference::source(&references));
        if self.tools.contains(&Tool::Skill) {
            self.skills = crate::skill::find(&self.cwd, &self.root, &config.skills);
            lists.push(crate::skill::source(&self.skills));
        }
        let files = instructions::discover(
            &self.cwd,
            &self.root,
            &paths::config_dir(),
            &config.instructions,
        );
        lists.push(instructions::source(instructions::read(&files)));
        let today = self.today.clone();
        lists.push(prompt::builtin_sources(
            move || today(),
            self.env_block.clone(),
        ));
        let context = match &self.store {
            Some(st) => st.context(&self.id, true)?,
            None => Vec::new(),
        };
        lists.push(crate::instruction_state::context_sources(&context));
        Ok(crate::instruction_state::combine(lists)?)
    }

    /// Read the instruction sources and record what changed: the first
    /// time, the baseline; later, an update the model sees here as a system
    /// message. Then render the system prompt from the baseline. Fails while
    /// a source cannot be read and there is no baseline yet.
    fn observe_instructions(&mut self) -> Result<()> {
        use crate::instruction_state as is;
        let list = self.instruction_sources()?;
        let o = is::observe(&list, self.instruction.as_ref())?;
        // Files the agent may have written since the last look (its tools
        // ran, or its background jobs still run) are not the user's word:
        // a change to instruction files or skills is shown to the model as
        // attributed project content, not as a system update.
        let agent = std::mem::take(&mut self.agent_changed_files) || self.jobs.running() > 0;
        let (delta, files): (is::Delta, is::Delta) = o
            .delta
            .clone()
            .into_iter()
            .partition(|(k, _)| o.initial || !agent || !runner::agent_writable_source(k));
        if o.changed() {
            let text = if o.initial {
                is::update_text(&list, &o)?
            } else {
                is::render_update(&list, &o.previous, &delta)?
            };
            let note = is::render_update(&list, &o.previous, &files)?;
            let seq = self.record(&is::record(&o.delta, Some(text.clone()), o.initial))?;
            let at = seq.unwrap_or(0);
            match &mut self.instruction {
                Some(s) if !o.initial => {
                    s.current = o.current;
                    s.through = at;
                }
                _ => self.instruction = Some(is::State::baseline(at, o.current)),
            }
            if !text.is_empty() {
                self.messages.push(Message::system(text));
                self.seqs.push(seq);
            }
            if !note.is_empty() {
                self.push(Message::user(runner::agent_change_note(&note)))?;
            }
        }
        let initial = self
            .instruction
            .as_ref()
            .map(|s| s.initial.clone())
            .unwrap_or_default();
        let rendered = is::render_initial(&list, &initial)?;
        // On a local server the date goes before the history instead (see
        // `date_note`).
        let local = self.model.local;
        let is_note = |r: &is::Rendered| local && r.key == prompt::DATE_KEY;
        self.date_note = rendered.iter().find(|r| is_note(r)).map(|r| r.text.clone());
        let rest: Vec<is::Rendered> = rendered.iter().filter(|r| !is_note(r)).cloned().collect();
        let text = is::join(&rest);
        self.system = assemble_system(&self.base_system, &text, self.agent.id == "plan");
        let own: Vec<is::Rendered> = rendered
            .into_iter()
            .filter(|r| !CLAUDE_WRITES.contains(&r.key.as_str()))
            .collect();
        let mut claude = is::join(&own);
        if self.agent.id == "plan" {
            if !claude.is_empty() {
                claude.push_str("\n\n");
            }
            claude.push_str(agent::PLAN_REMINDER);
        }
        self.claude_instructions = claude;
        Ok(())
    }

    /// Set where the date the session is told comes from (tests pass a
    /// fixed one).
    #[doc(hidden)]
    pub fn set_today(&mut self, today: impl Fn() -> String + Send + Sync + 'static) {
        self.today = Arc::new(today);
    }

    /// `history` as a request sends it: the baseline's date first (see
    /// `date_note`), then the messages as the model sees them.
    pub(super) fn sendable_history(&self, history: &[Message]) -> Vec<Message> {
        let mut all = Vec::with_capacity(history.len() + 1);
        all.extend(self.date_note.clone().map(Message::system));
        all.extend_from_slice(history);
        crate::media::sendable(&runner::model_messages(&all), self.model.input.as_deref())
    }

    /// Every tool offered to the model: built-in, MCP, resource, plugin,
    /// Code Mode and automation tools.
    fn tool_specs(
        &self,
        plugins: &crate::plugin::Registry,
        code: &[crate::codemode::Entry],
    ) -> Vec<codesmith_ai::ToolSpec> {
        let automation = self
            .store
            .as_ref()
            .filter(|_| {
                crate::permission::evaluate(crate::automation::TOOL, "*", &self.agent.permissions)
                    != crate::permission::Effect::Deny
            })
            .map(|_| {
                let (description, parameters) = crate::automation::tool_spec();
                codesmith_ai::ToolSpec {
                    name: crate::automation::TOOL.into(),
                    description,
                    parameters,
                }
            });
        let media = [(
            tool::image_generate::NAME,
            tool::image_generate::spec as fn() -> _,
        )]
        .into_iter()
        .zip(self.media_tools)
        .filter(|((name, _), on)| {
            *on && crate::permission::evaluate(name, "*", &self.agent.permissions)
                != crate::permission::Effect::Deny
        })
        .map(|((name, spec), _)| {
            let (description, parameters) = spec();
            codesmith_ai::ToolSpec {
                name: name.into(),
                description,
                parameters,
            }
        })
        .collect::<Vec<_>>();
        let execute = (!code.is_empty()).then(|| {
            let (description, parameters) = crate::codemode::spec();
            codesmith_ai::ToolSpec {
                name: crate::codemode::TOOL.into(),
                description,
                parameters,
            }
        });
        let (hidden, inside) = self.placement();
        self.tools
            .iter()
            .filter(|t| !hidden.contains(t) && !inside.contains(t))
            .map(|t| match t {
                Tool::Subagent => {
                    let (description, parameters) =
                        tool::subagent::spec(&agent::subagents(&self.loaded.config));
                    codesmith_ai::ToolSpec {
                        name: t.name().into(),
                        description,
                        parameters,
                    }
                }
                Tool::Skill => {
                    let (description, parameters) = crate::skill::tool_spec(&self.skills);
                    codesmith_ai::ToolSpec {
                        name: t.name().into(),
                        description,
                        parameters,
                    }
                }
                t => t.spec(&self.shell),
            })
            .chain(
                self.mcp_tools()
                    .into_iter()
                    .map(|t| codesmith_ai::ToolSpec {
                        name: t.name,
                        description: t.description,
                        parameters: t.schema,
                    }),
            )
            .chain(self.resource_tools())
            .chain(
                self.plugin_tools(plugins)
                    .iter()
                    .map(|t| codesmith_ai::ToolSpec {
                        name: t.name().to_string(),
                        description: t.description().to_string(),
                        parameters: t.parameters(),
                    }),
            )
            .chain(execute)
            .chain(automation)
            .chain(media)
            .collect()
    }

    /// One user turn: runs steps until the model answers without tool calls.
    ///
    /// Boxed with an explicit `Send` bound: a subagent's turn runs inside its
    /// parent's, and naming the type breaks that recursive future.
    ///
    /// The text is the user's own: its `@skill` mentions are applied (see
    /// [`Origin`]). A prompt written by anything else goes through
    /// [`Session::prompt_from`] with [`Origin::Engine`].
    pub fn prompt<'a>(
        &'a mut self,
        text: &'a str,
        sink: &'a mut dyn Sink,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.prompt_inner(Origin::User, text, vec![], sink))
    }

    /// A user turn with images or PDFs attached after the text; the text is
    /// the user's own, as for [`Session::prompt`].
    pub fn prompt_with<'a>(
        &'a mut self,
        text: &'a str,
        media: Vec<codesmith_ai::Media>,
        sink: &'a mut dyn Sink,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        let parts = media.into_iter().map(Part::Media).collect();
        Box::pin(self.prompt_inner(Origin::User, text, parts, sink))
    }

    /// A turn whose text comes from `origin`.
    pub fn prompt_from<'a>(
        &'a mut self,
        origin: Origin,
        text: &'a str,
        media: Vec<codesmith_ai::Media>,
        sink: &'a mut dyn Sink,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        let parts = media.into_iter().map(Part::Media).collect();
        Box::pin(self.prompt_inner(origin, text, parts, sink))
    }

    /// A user turn with files attached after the text, local ones
    /// announced by their path (see [`crate::attachment::parts`]); the text
    /// is the user's own.
    pub fn prompt_attached<'a>(
        &'a mut self,
        text: &'a str,
        files: Vec<crate::attachment::Attached>,
        sink: &'a mut dyn Sink,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.prompt_inner(Origin::User, text, crate::attachment::parts(files), sink))
    }

    async fn prompt_inner(
        &mut self,
        origin: Origin,
        text: &str,
        attached: Vec<Part>,
        sink: &mut dyn Sink,
    ) -> Result<()> {
        // A turn dropped before it finishes was stopped: plugins hear of it.
        let plugins = crate::plugin::active();
        let mut stop = Stopped {
            plugins: if plugins.is_empty() {
                Vec::new()
            } else {
                plugins.enabled_list(&self.loaded.config)
            },
            id: self.id.clone(),
            finished: false,
        };
        // However the turn ends, a Claude turn still waiting for tool
        // results will not get them: its idle timer starts.
        let _idle = (self.model.provider.protocol == codesmith_ai::Protocol::ClaudeAgent)
            .then(|| ClaudeIdle(self.id.clone()));
        let r = self.turn_inner(origin, text, attached, sink).await;
        stop.finished = true;
        r
    }

    async fn turn_inner(
        &mut self,
        origin: Origin,
        text: &str,
        attached: Vec<Part>,
        sink: &mut dyn Sink,
    ) -> Result<()> {
        let _lease = self.take_turn(sink).await?;
        let plugins = crate::plugin::active();
        // Plugins check permissions for this session through the host
        // while the turn runs.
        let _gate =
            (!plugins.is_empty()).then(|| crate::plugin::host::live_gate(&self.id, &self.gate));
        // Plugins' prompt hooks may change the text and keep metadata with it.
        let mut prompt_metadata = None;
        let hooked;
        let text = if plugins.wants(&self.loaded.config, crate::plugin::Hook::Prompt, &self.id) {
            let mut p = crate::plugin::Prompt {
                session_id: self.id.clone(),
                text: text.to_string(),
                metadata: None,
            };
            plugins.prompt(&self.loaded.config, &mut p);
            prompt_metadata = p.metadata;
            hooked = p.text;
            hooked.as_str()
        } else {
            text
        };
        // A denied or refused skill mention refuses the prompt before
        // anything is written (mentions in the text as the prompt hooks
        // left it). Only the user's own prompts mention skills.
        let skills = match origin {
            Origin::User => self.mentioned_skills(text)?,
            Origin::Engine => Vec::new(),
        };
        // What changed since the last turn comes before the new prompt; a
        // baseline that cannot be made yet leaves the prompt unsent.
        self.mcp.refresh().await;
        self.observe_instructions()?;
        self.turn.observed = true;
        // Subagents share the parent's turn snapshot.
        let mut snapshots = None;
        if self.parent.is_none()
            && self.store.is_some()
            && self.loaded.config.snapshots.unwrap_or(true)
        {
            if let Some(snaps) = Snapshots::for_project(&self.root) {
                match tokio::task::block_in_place(|| snaps.capture()) {
                    Ok(tree) => {
                        self.record(&Record::Snapshot { tree })?;
                        snapshots = Some(snaps);
                    }
                    Err(e) => sink.notice(&format!(
                        "snapshot skipped (undo unavailable for this turn): {e:#}"
                    )),
                }
            }
        }
        // A title a plugin gave the stored session meanwhile wins.
        if self.title.is_none() && self.stored {
            if let Some(t) = self
                .store
                .as_ref()
                .and_then(|s| s.get(&self.id).ok().flatten())
            {
                self.title = t.title;
            }
        }
        if self.title.is_none() && self.title_task.is_none() && self.title_pending.is_none() {
            // On a local server the title waits for the turn's first
            // response rather than compete with it.
            match self.title_plan() {
                Some(plan) if plan.defer => self.title_pending = Some((text.to_string(), plan)),
                plan => self.title_task = Some(self.spawn_title(text, plan)),
            }
        }
        // A stopped turn can leave calls without results; providers reject those.
        if let Some(results) = runner::unsettled(&self.messages) {
            self.push(results)?;
        }
        self.note_effort_switch()?;
        self.turn.stopped = None;
        // Commands that finished since the last turn ended come first, in
        // the order things happened.
        self.report_finished_jobs()?;
        self.run_admitted_compaction(sink, true).await?;
        // A fork is told it is one before its first prompt.
        if self.stored
            && self
                .store
                .as_ref()
                .is_some_and(|st| st.take_fork_notice(&self.id).unwrap_or(false))
        {
            self.push(Message::user(crate::fork::NOTICE))?;
        }
        // Mentioned skills' content comes before the prompt's text.
        let mut user = Message {
            role: Role::User,
            parts: skills
                .into_iter()
                .map(Part::text)
                .chain([Part::text(text)])
                .collect(),
        };
        user.parts.extend(attached);
        self.push(user)?;
        if let (Some(metadata), Some(Some(seq))) = (prompt_metadata, self.seqs.last().copied()) {
            self.record(&Record::Metadata {
                seq,
                metadata: metadata.clone(),
            })?;
            self.metadata.insert(seq, metadata);
        }
        plugins.event(
            &self.loaded.config,
            &crate::plugin::Event::TurnStart {
                session_id: &self.id,
                text,
            },
        );
        let result = self.run_steps(sink).await;
        // The end tree tells undo which files this turn changed.
        if let Some(snaps) = snapshots {
            if let Ok(tree) = tokio::task::block_in_place(|| snaps.capture()) {
                self.record(&Record::TurnEnd { tree })?;
            }
        }
        let err = result.as_ref().err().map(|e| format!("{e:#}"));
        plugins.event(
            &self.loaded.config,
            &crate::plugin::Event::TurnEnd {
                session_id: &self.id,
                error: err.as_deref(),
            },
        );
        // The title call's tokens count too, so finish it before saving
        // usage (a deferred one starts now if no response came).
        self.start_pending_title();
        self.finish_title().await;
        // Going idle leaves a background summary time to finish (after the
        // title, which a local server would otherwise also be busy with).
        if result.is_ok() {
            self.precompute_idle();
        }
        if let Some(s) = &self.store {
            if self.stored {
                self.save_usage(s);
            }
        }
        result
    }

    async fn run_steps(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let max_steps = self.agent.steps.unwrap_or(DEFAULT_MAX_STEPS).max(1);
        let mut overflow_compacted = false;
        let mut steps = 0;
        // At most one compaction between model steps, so a prompt that stays
        // over the limit after compacting cannot loop.
        let mut compacted_this_step = false;
        let mut repeats = RepeatGuard::default();
        let mut continued = 0u32;
        let mut idle_continued = 0u32;
        // Responses in a row cut off by the output token limit.
        let mut truncated = 0u32;
        loop {
            // A deferred title finishes before this session's next request.
            self.settle_title().await;
            self.report_finished_jobs()?;
            if !compacted_this_step {
                compacted_this_step = self.run_admitted_compaction(sink, false).await?;
            }
            if !compacted_this_step && self.due_for_compaction() {
                if self.worth_compacting() {
                    self.compact_for(sink, compact::Reason::Auto).await?;
                    compacted_this_step = true;
                } else if !self.compaction_skipped {
                    self.compaction_skipped = true;
                    sink.notice(&format!(
                        "the prompt (~{} tokens) is over the compaction limit, but the recent history is already small: the system prompt and tool definitions take most of it, so it is not compacted. Use a model with a larger context or fewer tools.",
                        self.last_prompt_tokens.max(self.estimate_prompt())
                    ));
                }
            } else if !compacted_this_step {
                self.precompute_between_steps();
            }
            // Each step sees the instructions as they are now.
            if !std::mem::take(&mut self.turn.observed) {
                // Tool lists a server announced as changed count already.
                self.mcp.refresh().await;
                self.observe_instructions()?;
            }
            // The last allowed step must answer in text (OpenCode's MAX_STEPS).
            let final_step = steps + 1 >= max_steps;
            let before = self.messages.len();
            // A background summary never runs beside a local server's step.
            self.postpone_held();
            let stepped = self.step(sink, final_step).await;
            self.warm_end();
            // The turn's first response is in: a deferred title may run.
            self.start_pending_title();
            let (parts, reason, usage) = match stepped {
                Ok(r) => r,
                // A context overflow is recovered by compacting once, unless
                // the response had already streamed output (which the step
                // kept in the history): then it fails the turn, as in
                // OpenCode. When the summary fails, the overflow is what
                // the turn reports; the compaction's own error went to the
                // sink.
                Err(e)
                    if !overflow_compacted
                        && self.loaded.config.compaction.auto()
                        && self.messages.len() == before
                        && compaction::is_overflow(&e.to_string()) =>
                {
                    overflow_compacted = true;
                    if self
                        .compact_for(sink, compact::Reason::Overflow)
                        .await
                        .is_err()
                    {
                        return Err(e);
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            steps += 1;
            compacted_this_step = false;
            self.count_usage(&usage);
            self.measured(&usage);
            sink.step_end(&reason, &usage);
            self.record(&Record::Step {
                model: self.model_spec.clone(),
                usage,
            })?;
            self.ran_with = Some(self.model_spec.clone());
            crate::plugin::active().event(
                &self.loaded.config,
                &crate::plugin::Event::StepEnd {
                    session_id: &self.id,
                    model: &self.model_spec,
                    usage: &usage,
                },
            );
            if reason.is_content_filter() {
                // Keep what was shown, but no calls: the response was blocked.
                let kept: Vec<Part> = parts
                    .into_iter()
                    .filter(|p| !matches!(p, Part::ToolCall { .. }))
                    .collect();
                self.push(Message {
                    role: Role::Assistant,
                    parts: kept,
                })?;
                return Err(anyhow!(runner::content_filtered(&reason)));
            }
            let length = reason.is_length();
            truncated = if length { truncated + 1 } else { 0 };
            let calls: Vec<(String, String, serde_json::Value)> = parts
                .iter()
                .filter_map(|p| match p {
                    Part::ToolCall {
                        id, name, input, ..
                    } => Some((id.clone(), name.clone(), input.clone())),
                    _ => None,
                })
                .collect();
            self.push(Message {
                role: Role::Assistant,
                parts,
            })?;
            if truncated > runner::OUTPUT_LIMIT_CONTINUATIONS {
                if !calls.is_empty() {
                    self.push(Message {
                        role: Role::User,
                        parts: calls
                            .iter()
                            .map(|(id, name, _)| Part::ToolResult {
                                id: id.clone(),
                                name: name.clone(),
                                content: runner::TOOLS_INTERRUPTED.into(),
                                is_error: true,
                                media: Vec::new(),
                            })
                            .collect(),
                    })?;
                }
                return Err(anyhow!(runner::OUTPUT_LIMIT_EXHAUSTED));
            }
            if calls.is_empty() {
                if final_step {
                    return Ok(());
                }
                if length {
                    // Cut off mid-answer: ask for the rest in smaller pieces.
                    self.push(Message::user(runner::CONTINUE_AFTER_OUTPUT_LIMIT))?;
                    continue;
                }
                if self.jobs_pending() > 0 {
                    // A background command finished meanwhile: report it
                    // (at the top of the loop) and let the model carry on.
                    continue;
                }
                if self.jobs.running() == 0 {
                    let answer = self.last_answer();
                    let idle = crate::plugin::Idle {
                        session_id: &self.id,
                        parent_id: self.parent.as_deref(),
                        answer: &answer,
                        continued,
                    };
                    if let Some(next) = crate::plugin::active().idle(&self.loaded.config, &idle) {
                        continued += 1;
                        sink.notice("a plugin continued the turn");
                        self.push(Message::user(next))?;
                        continue;
                    }
                }
                // The async idle hooks run with background commands still
                // running too: they see them, and may wait for them.
                if let Some(next) = self.session_idle(sink, &mut idle_continued).await? {
                    self.push(Message::user(next))?;
                    continue;
                }
                // With background commands still running, the turn ends
                // here, as in OpenCode, and each command is reported when
                // it finishes (`resume_background`, or the next prompt).
                // One-shot callers (`codesmith run`, subagents, automations)
                // wait with `settle_background`.
                return Ok(());
            }
            let mut results = Vec::new();
            let plugins = crate::plugin::active();
            // An error part way through still settles the batch: the calls
            // that ran keep their real results, the rest are interrupted.
            let mut failure: Option<anyhow::Error> = None;
            let mut pending = calls.into_iter();
            for (id, name, input) in pending.by_ref() {
                if self.turn.stopped.is_some() {
                    // The user stopped the turn: leave the rest unrun.
                    results.push(Part::ToolResult {
                        id,
                        name,
                        content: runner::TOOLS_INTERRUPTED.into(),
                        is_error: true,
                        media: Vec::new(),
                    });
                    continue;
                }
                // Repaired against the schema of the tool as offered (a
                // context hook's rename and edits included), as OpenCode
                // does before checking it; `bash` runs as `shell` only when
                // `shell` is offered under its own name.
                let (called, offered, name, input) = self.repair_call(name, input);
                sink.tool_start(&id, &name, &input);
                if !runner::read_only_tool(&name) {
                    self.agent_changed_files = true;
                }
                plugins.event(
                    &self.loaded.config,
                    &crate::plugin::Event::ToolStart {
                        session_id: &self.id,
                        id: &id,
                        name: &name,
                        input: &input,
                    },
                );
                let mut call = crate::plugin::ToolCall {
                    session_id: self.id.clone(),
                    id: id.clone(),
                    name: name.clone(),
                    input,
                };
                let refused = plugins.tool_before(&self.loaded.config, &mut call).err();
                let input = call.input.clone();
                let started = std::time::Instant::now();
                let verdict = repeats.check(&name, &input);
                if verdict == Repeat::Abort {
                    let why = format!(
                        "stopped: the model repeated the same {name} call {} times",
                        RepeatGuard::ABORT
                    );
                    sink.tool_end(&id, &name, &why, true);
                    results.push(Part::ToolResult {
                        id,
                        name,
                        content: why.clone(),
                        is_error: true,
                        media: Vec::new(),
                    });
                    failure = Some(anyhow!(why));
                    break;
                }
                self.turn.declined.store(false, Ordering::SeqCst);
                // What the call reports it is about to change, once allowed.
                let mut before: Vec<tool::Before> = Vec::new();
                // Changes already settled per inner call (`execute`'s).
                let mut settled: Vec<tool::Before> = Vec::new();
                let mut media = Vec::new();
                let (content, is_error) = match Tool::from_name(&name).filter(|t| t.session_level())
                {
                    _ if final_step => (runner::TOOLS_DISABLED.into(), true),
                    _ if !self.turn.offered.contains(&offered) => {
                        (runner::unavailable(&called), true)
                    }
                    _ if runner::invalid_json(&input).is_some() => (
                        runner::invalid_input(runner::invalid_json(&input).unwrap_or(""), length),
                        true,
                    ),
                    _ if verdict == Repeat::Refuse => (RepeatGuard::refusal(&name), true),
                    _ if refused.is_some() => (refused.clone().unwrap_or_default(), true),
                    _ if name == crate::codemode::TOOL && !self.code_entries().is_empty() => {
                        // The program's call rows, as the call's progress.
                        let (tx, mut rows) = tokio::sync::mpsc::unbounded_channel();
                        let run = self.run_code(&id, &input, tx);
                        tokio::pin!(run);
                        let (text, failed, changed) = loop {
                            tokio::select! {
                                r = &mut run => break r,
                                Some(m) = rows.recv() => sink.tool_progress(&id, &m),
                            }
                        };
                        while let Ok(m) = rows.try_recv() {
                            sink.tool_progress(&id, &m);
                        }
                        // Each inner call's changes stand whether or not
                        // the program as a whole failed.
                        settled = changed;
                        (text, failed)
                    }
                    _ if name == crate::automation::TOOL && self.store.is_some() => {
                        self.run_automation(&input).await
                    }
                    _ if name == tool::image_generate::NAME && self.media_tools[0] => {
                        self.run_media_generate(&name, &input).await
                    }
                    _ if name == tool::video_generate::NAME && self.media_tools[1] => {
                        self.run_media_generate(&name, &input).await
                    }
                    _ if self.mcp_tools().iter().any(|t| t.name == name) => {
                        let (content, is_error, m) = self.run_mcp(&name, &input).await;
                        media = m;
                        (content, is_error)
                    }
                    _ if self.resource_tools().iter().any(|t| t.name == name) => {
                        self.run_resource_tool(&name, &input).await
                    }
                    _ if self.plugin_tools(&plugins).iter().any(|t| t.name() == name) => {
                        self.run_plugin_tool(&plugins, &name, &input).await
                    }
                    Some(Tool::Todowrite) => self.run_todowrite(&input, sink),
                    Some(Tool::Subagent) => {
                        // A subagent's requests may reach the same server.
                        self.settle_title().await;
                        self.run_subagent(&input, sink).await
                    }
                    Some(Tool::Skill) => self.run_skill(&input),
                    Some(Tool::Question) => self.run_question(&input),
                    _ => {
                        let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
                        let ctx = Ctx {
                            cwd: self.cwd.clone(),
                            root: self.root.clone(),
                            gate: &self.gate,
                            shell: self.shell.clone(),
                            session_id: self.id.clone(),
                            limits: self.limits,
                            client: self.client.clone(),
                            jobs: Some(self.jobs.clone()),
                            websearch: Some(self.loaded.config.websearch.clone()),
                            format: self.format.clone(),
                            lsp: self.lsp.clone(),
                            progress: Some(progress),
                        };
                        let r = {
                            let run = tool::run(&ctx, &name, &input);
                            tokio::pin!(run);
                            let r = loop {
                                tokio::select! {
                                    r = &mut run => break r,
                                    Some(p) = updates.recv() => match p {
                                        tool::Progress::Change(b) => before.push(b),
                                        p => forward_progress(sink, &id, p),
                                    },
                                }
                            };
                            while let Ok(p) = updates.try_recv() {
                                match p {
                                    tool::Progress::Change(b) => before.push(b),
                                    p => forward_progress(sink, &id, p),
                                }
                            }
                            r
                        };
                        media = r.media;
                        (r.content, r.is_error)
                    }
                };
                let (mut content, mut is_error) = (content, is_error);
                if self.turn.declined.swap(false, Ordering::SeqCst) {
                    content = runner::DECLINED.into();
                    is_error = true;
                    self.turn.stopped = Some("you declined a tool call");
                }
                plugins.tool_after(&self.loaded.config, &call, &mut content, &mut is_error);
                if called != offered {
                    content.push_str("\n\n");
                    content.push_str(&tool::input_repair::alias_note(&called, &offered));
                }
                let (media, notes) =
                    crate::media::fit_tool_media(media, &self.loaded.config.media.image);
                for note in notes {
                    content.push('\n');
                    content.push_str(&note);
                }
                if let Err(e) = self.record(&Record::Tool {
                    name: name.clone(),
                    ms: started.elapsed().as_millis() as u64,
                    error: is_error,
                }) {
                    failure = Some(e);
                }
                // A patch that failed midway changed the files it wrote first.
                if is_error && name != Tool::Patch.name() {
                    before.clear();
                }
                before.extend(settled);
                for b in before {
                    sink.tool_change(&id, &b.path, b.text.as_deref());
                    if self.store.is_none() {
                        continue;
                    }
                    if let Err(e) = self.record_change(&id, b.path, b.text) {
                        failure = Some(e);
                    }
                }
                if !media.is_empty() {
                    sink.tool_media(&id, &media);
                }
                sink.tool_end(&id, &name, &content, is_error);
                plugins.event(
                    &self.loaded.config,
                    &crate::plugin::Event::ToolEnd {
                        session_id: &self.id,
                        id: &id,
                        name: &name,
                        output: &content,
                        is_error,
                    },
                );
                // The result answers the call by the name it was made by.
                results.push(Part::ToolResult {
                    id,
                    name: called,
                    content,
                    is_error,
                    media,
                });
                if failure.is_some() {
                    break;
                }
            }
            for (id, name, _) in pending {
                results.push(Part::ToolResult {
                    id,
                    name,
                    content: runner::TOOLS_INTERRUPTED.into(),
                    is_error: true,
                    media: Vec::new(),
                });
            }
            self.push(Message {
                role: Role::User,
                parts: results,
            })?;
            self.give_nearby_instructions()?;
            if let Some(e) = failure {
                return Err(e);
            }
            if let Some(why) = self.turn.stopped {
                sink.notice(&format!("stopped: {why}"));
                return Ok(());
            }
            if final_step {
                return Ok(());
            }
        }
    }

    /// Background commands still running.
    pub fn background_running(&self) -> usize {
        self.jobs.running()
    }

    /// Background commands that finished but were not reported to the model yet.
    pub fn background_finished(&self) -> usize {
        self.jobs_pending()
    }

    /// Wait until a background command finishes (at once if one already
    /// has, or if none are running).
    pub async fn wait_background(&self) {
        self.jobs.wait_any().await;
    }

    /// A handle on this session's commands (background jobs and their
    /// unreported results), for a front end that rebuilds the session
    /// (cancel, undo, a mode or model switch) to hand to the rebuilt one
    /// with [`Session::adopt_jobs`]. The commands run until the last
    /// handle goes or they are stopped, so one held across the rebuild
    /// keeps them alive while the old session is dropped.
    pub fn jobs(&self) -> tool::shell::Jobs {
        self.jobs.clone()
    }

    /// Take over `jobs` (from an earlier build of the same session): their
    /// results are reported by this session's turns and waits. Any job
    /// this session had started itself loses its handle here.
    pub fn adopt_jobs(&mut self, jobs: tool::shell::Jobs) {
        self.jobs = jobs;
    }

    /// Replace the environment this session's shell commands run in
    /// (OpenCode's `Session.environment`); see
    /// [`tool::shell::Jobs::set_environment`]. It goes with the jobs handle,
    /// so a rebuilt session that adopts them keeps it.
    pub fn set_environment(&self, variables: Option<std::collections::BTreeMap<String, String>>) {
        self.jobs.set_environment(variables);
    }

    /// The environment set with [`Session::set_environment`], if any.
    pub fn environment(&self) -> Option<std::collections::BTreeMap<String, String>> {
        self.jobs.environment()
    }

    /// Kills this session's commands from a signal handler, or waits for
    /// one to finish while the session is idle (see [`tool::shell::Reaper`]).
    pub fn reaper(&self) -> tool::shell::Reaper {
        self.jobs.reaper()
    }

    /// Stop every command this session still runs (SIGTERM to each process
    /// group, SIGKILL after a grace period).
    pub async fn stop_background(&self) {
        self.jobs.stop_all().await;
    }

    /// Report background commands that finished since the turn ended and
    /// let the model carry on (OpenCode resumes a session with each
    /// finished command's notice). Does nothing when none finished.
    pub fn resume_background<'a>(
        &'a mut self,
        sink: &'a mut dyn Sink,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.resume_inner(sink))
    }

    /// When a resume may start again after the last ones failed before
    /// reporting their commands (`None`: now). A front end that wakes the
    /// session for finished commands waits until then instead of retrying
    /// at once, which with a persistent failure (the store, another
    /// process holding the turn) would spin; [`Session::resume_background`]
    /// called earlier waits for it itself.
    pub fn resume_due(&self) -> Option<std::time::Instant> {
        self.turn
            .resume_due
            .filter(|due| *due > std::time::Instant::now())
    }

    /// What to do for this idle session's background commands. `held` is
    /// the front end's hold after the user interrupted a turn (set it to
    /// `Some(background_finished())` then): the commands already finished
    /// are not resumed for until another one finishes, so an interrupt is
    /// not followed by a turn the user did not ask for. The hold lowers as
    /// commands are reported and ends when one more finishes.
    pub fn idle_wake(&self, held: &mut Option<usize>) -> IdleWake {
        let finished = self.background_finished();
        if let Some(h) = *held {
            *held = (finished <= h).then_some(finished);
        }
        if finished > 0 && held.is_none() {
            return match self.resume_due() {
                Some(due) => IdleWake::At(due),
                None => IdleWake::Resume,
            };
        }
        if self.background_running() > 0 {
            IdleWake::Finish(finished)
        } else {
            IdleWake::Idle
        }
    }

    async fn resume_inner(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let finished = self.jobs.finished();
        if finished.is_empty() {
            return Ok(());
        }
        if let Some(due) = self.resume_due() {
            tokio::time::sleep_until(due.into()).await;
        }
        let result = self.resume_turn(sink, &finished).await;
        let still = self.jobs.finished();
        if result.is_err() && finished.iter().any(|j| still.iter().any(|s| s.id == j.id)) {
            // Failed before the model saw them: they are still waiting, so
            // a caller waking on them would come straight back.
            self.turn.resume_failures = self.turn.resume_failures.saturating_add(1);
            let wait = resume_backoff(self.turn.resume_failures).mul_f64(self.turn.retry_scale);
            self.turn.resume_due = Some(std::time::Instant::now() + wait);
        } else {
            self.turn.resume_failures = 0;
            self.turn.resume_due = None;
        }
        result
    }

    async fn resume_turn(
        &mut self,
        sink: &mut dyn Sink,
        finished: &[tool::shell::Job],
    ) -> Result<()> {
        // A turn like any other: one at a time across processes, after
        // what others appended meanwhile.
        let _lease = self.take_turn(sink).await?;
        for j in finished {
            sink.notice(&format!(
                "background command {} finished ({})",
                j.id,
                short_command(&j.command)
            ));
        }
        if let Some(results) = runner::unsettled(&self.messages) {
            self.push(results)?;
        }
        self.note_effort_switch()?;
        self.turn.stopped = None;
        let result = self.run_steps(sink).await;
        // Going idle leaves a background summary time to finish.
        if result.is_ok() {
            self.precompute_idle();
        }
        // Another end tree for the same turn: undo then covers these changes too.
        if self.parent.is_none() && self.stored && self.loaded.config.snapshots.unwrap_or(true) {
            if let Some(snaps) = Snapshots::for_project(&self.root) {
                if let Ok(tree) = tokio::task::block_in_place(|| snaps.capture()) {
                    self.record(&Record::TurnEnd { tree })?;
                }
            }
        }
        if let (Some(s), true) = (&self.store, self.stored) {
            self.save_usage(s);
        }
        result
    }

    /// Wait for this session's background commands and let the model act on
    /// each result, until none are left: what a one-shot caller does after
    /// [`Session::prompt`] (OpenCode's `run` waits for the jobs its prompt
    /// started, then exits). A command that never ends (a dev server) keeps
    /// this waiting until it is stopped.
    pub fn settle_background<'a>(
        &'a mut self,
        sink: &'a mut dyn Sink,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            while self.jobs.running_or_unreported() > 0 {
                if self.turn.stopped.is_some() {
                    return Ok(());
                }
                if self.jobs_pending() == 0 {
                    let running = self.jobs.running_jobs();
                    let names: Vec<String> = running
                        .iter()
                        .map(|j| format!("{} ({})", j.id, short_command(&j.command)))
                        .collect();
                    sink.notice(&format!(
                        "waiting for {} background command{}: {}",
                        running.len(),
                        if running.len() == 1 { "" } else { "s" },
                        names.join(", ")
                    ));
                    self.jobs.wait_any().await;
                }
                self.resume_inner(sink).await?;
            }
            Ok(())
        })
    }

    fn jobs_pending(&self) -> usize {
        // Finished but not yet reported jobs (peek without taking), counted
        // under one lock: the difference of two counts taken apart
        // underflowed when a job started in between.
        self.jobs.finished().len()
    }

    /// Tell the model about background commands that finished since its last step.
    /// Also tells it about commands the user moved to the background
    /// ([`tool::shell::Jobs::background_all`]), first.
    fn report_finished_jobs(&mut self) -> Result<()> {
        let moved = self.jobs.take_moved();
        let done = self.jobs.take_finished();
        if done.is_empty() && moved.is_empty() {
            return Ok(());
        }
        let note = (!moved.is_empty()).then(|| Part::text(tool::shell::moved_note(&moved)));
        let parts = note
            .into_iter()
            .chain(
                done.iter()
                    .map(|j| Part::text(tool::shell::notification(j, self.limits))),
            )
            .collect();
        self.push(Message {
            role: Role::User,
            parts,
        })
    }

    fn allowed(&self, action: &str, resource: &str, title: String) -> Result<(), String> {
        self.allowed_with(action, vec![resource.into()], vec![resource.into()], title)
    }

    /// A permission check whose remembered ("always") answer covers `save`.
    fn allowed_with(
        &self,
        action: &str,
        resources: Vec<String>,
        save: Vec<String>,
        title: String,
    ) -> Result<(), String> {
        let gate = &self.gate;
        let req = crate::permission::Request {
            action: action.into(),
            resources,
            save,
            title,
        };
        tokio::task::block_in_place(|| gate.assert(&req)).map_err(|e| e.to_string())
    }

    fn run_todowrite(&mut self, input: &serde_json::Value, sink: &mut dyn Sink) -> (String, bool) {
        if let Err(e) = self.allowed("todowrite", "*", "Update the todo list".into()) {
            return (e, true);
        }
        match tool::todo::parse(&tool::repair(input)) {
            Ok(todos) => {
                self.todos = todos;
                sink.todos(&self.todos);
                (tool::todo::summarize(&self.todos), false)
            }
            Err(e) => (e, true),
        }
    }

    /// The model's final text in the latest assistant message.
    fn last_answer(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| {
                        if let Part::Text(t) = p {
                            Some(t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn run_plugin_tool(
        &self,
        plugins: &crate::plugin::Registry,
        name: &str,
        input: &serde_json::Value,
    ) -> (String, bool) {
        let Some(t) = self
            .plugin_tools(plugins)
            .into_iter()
            .find(|t| t.name() == name)
        else {
            return (
                format!("The {name} tool is not available to this agent."),
                true,
            );
        };
        if let Err(e) = self.allowed(name, "*", format!("Run plugin tool {name}")) {
            return (e, true);
        }
        let ctx = crate::plugin::ToolContext {
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            session_id: self.id.clone(),
        };
        match t.execute(ctx, tool::repair(input)).await {
            Ok(text) => (
                tool::truncate(&text, self.limits, &paths::session_output_dir(&self.id)),
                false,
            ),
            Err(e) => (e, true),
        }
    }

    /// MCP tools this agent may use: hidden when its rules deny the tool
    /// outright, by the name it is offered under or, for a tool renamed
    /// after a collision, by the shared name rules were written for.
    fn permitted_mcp(&self) -> Vec<crate::mcp::McpTool> {
        let rules = &self.agent.permissions;
        let denied = |name: &str| {
            crate::permission::evaluate(name, "*", rules) == crate::permission::Effect::Deny
        };
        self.mcp
            .tools()
            .into_iter()
            .filter(|t| !denied(&t.name) && !denied(&t.shared_name()))
            .collect()
    }

    /// `list_mcp_resources` and `read_mcp_resource`, when a connected server
    /// has resources and the rules do not deny them.
    fn resource_tools(&self) -> Vec<codesmith_ai::ToolSpec> {
        if self.mcp.resource_servers().is_empty() {
            return vec![];
        }
        crate::mcp::resource_tools()
            .into_iter()
            .filter(|(name, ..)| {
                crate::permission::evaluate(name, "*", &self.agent.permissions)
                    != crate::permission::Effect::Deny
            })
            .map(|(name, description, parameters)| codesmith_ai::ToolSpec {
                name: name.into(),
                description,
                parameters,
            })
            .collect()
    }

    async fn run_resource_tool(&self, name: &str, input: &serde_json::Value) -> (String, bool) {
        let input = tool::repair(input);
        let server = input["server"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let result = if name == crate::mcp::LIST_RESOURCES {
            // Listing every server checks each name, so per-server rules still apply.
            let servers = match server {
                Some(s) => vec![s.to_string()],
                None => self.mcp.resource_servers(),
            };
            let title = format!("List MCP resources from {}", servers.join(", "));
            match self.allowed_with(name, servers.clone(), servers, title) {
                Err(e) => return (e, true),
                Ok(()) => self
                    .mcp
                    .list_resources(server)
                    .await
                    .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()),
            }
        } else {
            let (Some(server), Some(uri)) = (server, input["uri"].as_str()) else {
                return ("`server` and `uri` are required".into(), true);
            };
            let resource = format!("{server}:{uri}");
            let title = format!("Read MCP resource {resource}");
            if let Err(e) =
                self.allowed_with(name, vec![resource], vec![format!("{server}:*")], title)
            {
                return (e, true);
            }
            self.mcp
                .read_resource(server, uri)
                .await
                .map_err(|e| anyhow!("Unable to read MCP resource {server}:{uri}: {e:#}"))
        };
        match result {
            Ok(text) => (
                tool::truncate(&text, self.limits, &paths::session_output_dir(&self.id)),
                false,
            ),
            Err(e) => (format!("{e:#}"), true),
        }
    }

    /// The `<mcp_instructions>` block for servers whose tools this session can
    /// reach, directly or through Code Mode (OpenCode's rule).
    fn mcp_instructions(&self, code: &[crate::codemode::Entry]) -> Vec<(String, String, bool)> {
        let direct = self.mcp_tools();
        self.mcp
            .instructions()
            .into_iter()
            .filter_map(|(server, text)| {
                let in_code = code.iter().any(|e| e.namespace == server);
                let in_direct = direct.iter().any(|t| t.server == server);
                (in_code || in_direct).then_some((server, text, in_code))
            })
            .collect()
    }

    /// The prompt a custom command sends: its template filled in, or an MCP
    /// server's prompt fetched with the arguments.
    pub async fn expand_command(&self, c: &crate::command::Command, args: &str) -> Result<String> {
        match &c.mcp {
            Some((server, name)) => self.mcp.prompt_text(server, name, args).await,
            None => {
                let text = crate::command::expand(&c.template, args);
                crate::command::interpolate_shell(&text, &self.shell, &self.cwd).await
            }
        }
    }

    /// Run a custom or MCP command, shown in the history as `shown`: its
    /// prompt sent in this session, or, for a subagent command (see
    /// [`crate::command::Command::runs_as_subagent`]), sent to a child
    /// session of the command's agent (else this session's) on the
    /// command's model (else that agent's, else this session's), whose
    /// report then joins this session as an exchange. Switching this
    /// session's agent and model for other commands is the caller's (it
    /// builds the session).
    pub async fn run_command(
        &mut self,
        shown: &str,
        c: &crate::command::Command,
        args: &str,
        media: Vec<codesmith_ai::Media>,
        sink: &mut dyn Sink,
    ) -> Result<()> {
        if let Some(name) = &c.plugin {
            let plugins = crate::plugin::active();
            let found = plugins
                .commands(&self.loaded.config)
                .into_iter()
                .find(|p| p.name() == name)
                .ok_or_else(|| anyhow!("Command not found: {name}"))?;
            let ctx = crate::plugin::CommandContext {
                session_id: self.id.clone(),
                cwd: self.cwd.clone(),
                root: self.root.clone(),
                args: args.to_string(),
                media: media.clone(),
            };
            return match found.execute(ctx).await {
                Ok(Some(text)) => self.prompt_from(Origin::Engine, &text, media, sink).await,
                Ok(None) => Ok(()),
                Err(e) => Err(anyhow!("{e}")),
            };
        }
        let text = self.expand_command(c, args).await?;
        if !c.runs_as_subagent(&self.loaded.config) {
            return self.prompt_from(Origin::Engine, &text, media, sink).await;
        }
        let agent_id = c.agent.clone().unwrap_or_else(|| self.agent.id.clone());
        let target = agent::resolve(&self.loaded.config, &agent_id)
            .ok_or_else(|| anyhow!("unknown agent {agent_id:?}"))?;
        let model = c
            .model
            .clone()
            .or(target.model)
            .unwrap_or_else(|| self.model_spec.clone());
        let name = shown
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        let label = c.description.clone().unwrap_or(name);
        let store = self.child_store().map_err(|e| anyhow!(e))?;
        let task = format!("{}\n{text}", tool::subagent::PREAMBLE);
        let report = match self
            .spawn_child(agent_id, Some(model), label, task, None, store, sink)
            .await
        {
            Ok(r) | Err(r) => r,
        };
        self.append_exchange(shown, &report, &Usage::default())
    }

    fn codemode_on(&self) -> bool {
        self.loaded.config.codemode != Some(false)
            && crate::permission::evaluate(crate::codemode::TOOL, "*", &self.agent.permissions)
                != crate::permission::Effect::Deny
    }

    /// Where `full_codemode` puts this session's built-ins (see
    /// [`crate::codemode::placement`]): the ones hidden, and the ones
    /// called from inside `execute`. Both empty without it, or without
    /// `execute`.
    fn placement(&self) -> (Vec<Tool>, Vec<Tool>) {
        use crate::codemode::placement;
        if self.loaded.config.full_codemode != Some(true) || !self.codemode_on() {
            return (vec![], vec![]);
        }
        let (base, _) = crate::config::split_variant(&self.model_spec);
        let (pid, id) = base.split_once('/').unwrap_or(("", base));
        let family = self
            .loaded
            .config
            .models_dev
            .as_ref()
            .and_then(|c| c.catalog().model(pid, id).and_then(|m| m.family.clone()));
        let model = placement::Model {
            provider_id: self.model.provider_id.clone(),
            id: id.to_string(),
            model_id: self.model.model_id.clone(),
            family,
            canonical: None,
        };
        let placed = placement::model_tools(Some(&model), true, true);
        let hidden = self
            .tools
            .iter()
            .copied()
            .filter(|t| placement::FULL_CODE_MODE_HIDDEN.contains(&t.name()))
            .collect();
        let inside = if placement::strong(Some(&model)) {
            self.tools
                .iter()
                .copied()
                .filter(|t| {
                    placement::CODE_MODE_BUILTINS.contains(&t.name())
                        && !placed.hidden.contains(t.name())
                })
                .collect()
        } else {
            vec![]
        };
        (hidden, inside)
    }

    /// A built-in run from inside `execute` (inner call `n` of call
    /// `outer`), with the session's checks and plugins' per-call hooks, as a
    /// direct call: a `tool_before` veto refuses it, `tool_after` sees (and
    /// may redact) its output. Returns the files it changed, as a direct
    /// call's progress reports them, when it succeeded (or was a patch,
    /// which may fail midway after writing).
    async fn run_builtin_in_code(
        &self,
        outer: &str,
        n: usize,
        name: &str,
        input: &serde_json::Value,
    ) -> (String, bool, Vec<tool::Before>) {
        let plugins = crate::plugin::active();
        let mut call = crate::plugin::ToolCall {
            session_id: self.id.clone(),
            id: format!("{outer}/{n}"),
            name: name.to_string(),
            input: input.clone(),
        };
        if let Err(refused) = plugins.tool_before(&self.loaded.config, &mut call) {
            let (mut content, mut is_error) = (refused, true);
            plugins.tool_after(&self.loaded.config, &call, &mut content, &mut is_error);
            return (content, true, Vec::new());
        }
        let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let ctx = Ctx {
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            gate: &self.gate,
            shell: self.shell.clone(),
            session_id: self.id.clone(),
            limits: self.limits,
            client: self.client.clone(),
            jobs: Some(self.jobs.clone()),
            websearch: Some(self.loaded.config.websearch.clone()),
            format: self.format.clone(),
            lsp: self.lsp.clone(),
            progress: Some(progress),
        };
        let mut before = Vec::new();
        let r = {
            let run = tool::run(&ctx, name, &call.input);
            tokio::pin!(run);
            let r = loop {
                tokio::select! {
                    r = &mut run => break r,
                    Some(p) = updates.recv() => if let tool::Progress::Change(b) = p {
                        before.push(b);
                    },
                }
            };
            while let Ok(p) = updates.try_recv() {
                if let tool::Progress::Change(b) = p {
                    before.push(b);
                }
            }
            r
        };
        let mut text = crate::mcp::with_media_note(r.content, &r.media, "Code Mode returns text");
        let mut is_error = r.is_error;
        plugins.tool_after(&self.loaded.config, &call, &mut text, &mut is_error);
        if r.is_error && name != Tool::Patch.name() {
            before.clear();
        }
        (text, is_error, before)
    }

    /// MCP tools offered as direct tool calls.
    fn mcp_tools(&self) -> Vec<crate::mcp::McpTool> {
        let on = self.codemode_on();
        self.permitted_mcp()
            .into_iter()
            .filter(|t| !(on && t.codemode))
            .collect()
    }

    /// Tools callable inside Code Mode.
    fn code_entries(&self) -> Vec<crate::codemode::Entry> {
        if !self.codemode_on() {
            return vec![];
        }
        let builtins = self.placement().1.into_iter().map(|t| {
            let spec = t.spec(&self.shell);
            crate::codemode::Entry {
                tool: spec.name.clone(),
                namespace: String::new(),
                name: spec.name,
                description: spec.description,
                schema: spec.parameters,
            }
        });
        self.permitted_mcp()
            .into_iter()
            .filter(|t| t.codemode)
            .map(|t| crate::codemode::Entry {
                tool: t.name.clone(),
                namespace: t.server.clone(),
                name: t.remote.clone(),
                description: t.description.clone(),
                schema: t.schema.clone(),
            })
            .chain(builtins)
            .collect()
    }

    /// An image or video generation call: it starts a background job.
    async fn run_media_generate(&self, name: &str, input: &serde_json::Value) -> (String, bool) {
        let input = tool::repair(input);
        let ctx = Ctx {
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            gate: &self.gate,
            shell: self.shell.clone(),
            session_id: self.id.clone(),
            limits: self.limits,
            client: self.client.clone(),
            jobs: Some(self.jobs.clone()),
            websearch: None,
            format: None,
            lsp: None,
            progress: None,
        };
        let env = tool::image_generate::Env {
            config: &self.loaded.config,
            provider: Some(self.model.provider_id.as_str()),
            store: self.store.as_ref().and_then(|s| s.path()),
            gate: self.gate.clone(),
            jobs: self.jobs.clone(),
            retry: crate::generate::Options {
                retry_scale: self.turn.retry_scale,
                ..Default::default()
            },
        };
        let r = if name == tool::image_generate::NAME {
            tool::image_generate::run(&ctx, env, &input).await
        } else {
            tool::video_generate::run(&ctx, env, &input).await
        };
        match r {
            Ok(text) => (text, false),
            Err(e) => (e, true),
        }
    }

    async fn run_automation(&self, input: &serde_json::Value) -> (String, bool) {
        use crate::automation as au;
        let input = tool::repair(input);
        let Some(db) = &self.store else {
            return ("Automations need the session store.".into(), true);
        };
        let s = |k: &str| {
            input[k]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let dir = self.cwd.display().to_string();
        let action = s("action").unwrap_or_default();
        // An event trigger needs a connected server offering that push event.
        let trigger = &input["trigger"];
        if action == "create" && trigger["type"] == "event" {
            let (server, event) = (
                trigger["server"].as_str().unwrap_or(""),
                trigger["event"].as_str().unwrap_or(""),
            );
            if let Err(e) = self
                .mcp
                .check_event(server, event, trigger.get("args").filter(|a| !a.is_null()))
                .await
            {
                return (format!("{e:#}"), true);
            }
        }
        // Managing automations outlives this session, so it asks unless a
        // rule names the action: the blanket allow agents start with does
        // not cover it. (Unattended runs deny the tool outright.)
        let explicit = |action: &str, resource: &str, title: String| {
            use crate::permission::{evaluate, Denied, Effect, Gate, Request};
            let rules = self.gate.rules();
            let named = au::explicit_effect(action, resource, &rules);
            // A saved approval must not hide a configured deny.
            let configured = self.gate.configured();
            if named == Effect::Deny
                || au::explicit_effect(action, resource, configured) == Effect::Deny
                || evaluate(action, resource, configured) == Effect::Deny
            {
                let (action, resource) = (action.into(), resource.into());
                return Err(Denied::Blocked { action, resource }.to_string());
            }
            if named == Effect::Allow {
                return self.allowed(action, resource, title);
            }
            let req = Request {
                action: action.into(),
                resources: vec![resource.into()],
                save: vec![],
                title,
            };
            let gate = Gate::new(vec![], self.gate.asker());
            tokio::task::block_in_place(|| gate.assert(&req)).map_err(|e| e.to_string())
        };
        let result: anyhow::Result<String> = (|| match action.as_str() {
            "list" => Ok(au::table(&au::list(db, Some(&dir))?)),
            "create" => {
                let trigger: au::Trigger = serde_json::from_value(input["trigger"].clone())
                    .map_err(|e| anyhow!("trigger: {e}"))?;
                let prompt = s("prompt").ok_or_else(|| anyhow!("`prompt` is required"))?;
                // By default each run is a new child of this session; only
                // `target: "session"` continues this session itself.
                let to = match (s("target").as_deref(), s("agent")) {
                    (Some("session"), Some(_)) => {
                        return Err(anyhow!(
                            "`agent` starts a new session each run, so it can't go with target \"session\""
                        ))
                    }
                    (Some("session"), None) => au::Target::Session {
                        session: self.id.clone(),
                    },
                    (None | Some("child"), Some(a)) => {
                        let target = crate::agent::resolve(&self.loaded.config, &a)
                            .ok_or_else(|| anyhow!("unknown agent {a}"))?;
                        au::check_target(&self.agent.id, &target)?;
                        au::Target::Subagent { subagent: a }
                    }
                    (None | Some("child"), None) => {
                        au::Target::child_of(&self.id, &self.agent.id, &self.model_spec)
                    }
                    (Some(other), _) => {
                        return Err(anyhow!(
                            "unknown target {other:?}; use \"child\" (the default) or \"session\""
                        ))
                    }
                };
                let name = s("name").unwrap_or_else(|| prompt.chars().take(40).collect());
                let action = au::Action::Prompt { prompt, to };
                let title = au::create_title(&name, &trigger, &action);
                explicit(au::TOOL, "create", title).map_err(|e| anyhow!(e))?;
                if let au::Trigger::Event {
                    server,
                    event,
                    args,
                } = &trigger
                {
                    let args = match args.as_ref().filter(|a| !a.is_null()) {
                        Some(a) => format!(" with arguments {}", au::quote(&a.to_string(), 600)),
                        None => String::new(),
                    };
                    let title = format!(
                        "Watch {} events from MCP server {}{args}, for automation \"{}\": each event runs it unattended with the event as untrusted data",
                        au::quote(event, 200),
                        au::quote(server, 200),
                        au::quote(&name, 80),
                    );
                    explicit(au::WATCH_EVENT, server, title).map_err(|e| anyhow!(e))?;
                }
                let a = au::create(
                    db,
                    au::New {
                        name,
                        directory: dir.clone(),
                        session_id: Some(self.id.clone()),
                        trigger,
                        action,
                    },
                )?;
                let first = match a.next_due {
                    Some(_) => format!("first run {}", au::when(a.next_due)),
                    None => "it runs once per event".into(),
                };
                Ok(format!("Created {} ({}); {first}. It runs while any CodeSmith is open, or under `codesmith automation serve`.", a.id, au::describe(&a.trigger)))
            }
            "pause" | "resume" | "remove" => {
                let id = s("id").ok_or_else(|| anyhow!("`id` is required"))?;
                let a = au::get(db, &id)?
                    .filter(|a| a.directory == dir)
                    .ok_or_else(|| anyhow!("no automation {id} in this project"))?;
                // Resuming restarts unattended work, and pausing or removing
                // stops work the user set up, so each asks like create.
                let title = format!(
                    "{} automation \"{}\" ({id}, {}), which {}",
                    match action.as_str() {
                        "pause" => "Pause",
                        "resume" => "Resume",
                        _ => "Remove",
                    },
                    au::quote(&a.name, 80),
                    au::quote(&au::describe(&a.trigger), 600),
                    au::describe_action(&a.action, au::APPROVAL_PROMPT)
                );
                explicit(au::TOOL, &action, title).map_err(|e| anyhow!(e))?;
                if action == "remove" {
                    au::remove(db, &id)?;
                    Ok(format!("Removed {id}."))
                } else {
                    let a = au::set_status(
                        db,
                        &id,
                        if action == "pause" {
                            "paused"
                        } else {
                            "active"
                        },
                    )?;
                    Ok(format!(
                        "{id} is {}; next run {}.",
                        a.status,
                        au::when(a.next_due)
                    ))
                }
            }
            other => Err(anyhow!(
                "unknown action {other:?}; use create, list, pause, resume or remove"
            )),
        })();
        match result {
            Ok(t) => (t, false),
            Err(e) => (format!("{e:#}"), true),
        }
    }

    /// Run an `execute` call (`id`): its text, whether it failed, and the
    /// files its built-in calls changed.
    async fn run_code(
        &self,
        id: &str,
        input: &serde_json::Value,
        rows: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) -> (String, bool, Vec<tool::Before>) {
        let input = tool::repair(input);
        let Some(code) = input["code"].as_str().filter(|c| !c.trim().is_empty()) else {
            return (
                "Missing required string parameter `code`".into(),
                true,
                vec![],
            );
        };
        let changed: std::sync::Mutex<Vec<tool::Before>> = Default::default();
        let changed = &changed;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let calls = &calls;
        let progress = crate::codemode::progress::Progress::new(move |m| {
            let _ = rows.send(m);
        });
        let progress = &progress;
        let entries = self.code_entries();
        let builtins: Vec<&str> = entries
            .iter()
            .filter(|e| e.namespace.is_empty())
            .map(|e| e.tool.as_str())
            .collect();
        let builtins = &builtins;
        let out = crate::codemode::execute(
            code,
            &entries,
            crate::codemode::MAX_TOOL_CALLS,
            crate::codemode::TIMEOUT,
            |tool, args| async move {
                let row = progress.start(&tool, Some(&args));
                let (text, failed) = if builtins.contains(&tool.as_str()) {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let (text, failed, before) =
                        self.run_builtin_in_code(id, n, &tool, &args).await;
                    changed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend(before);
                    (text, failed)
                } else {
                    // Code Mode hands results to code as JSON, so images
                    // and PDFs are noted rather than passed on.
                    let (text, failed, media) = self.run_mcp(&tool, &args).await;
                    let text = crate::mcp::with_media_note(text, &media, "Code Mode returns text");
                    (text, failed)
                };
                progress.settle(row, !failed);
                if failed {
                    Err(text)
                } else {
                    Ok(text)
                }
            },
        )
        .await;
        progress.finish(out.result.is_ok());
        let (text, is_error) = out.render();
        let changed = std::mem::take(&mut *changed.lock().unwrap_or_else(|e| e.into_inner()));
        (
            tool::truncate(&text, self.limits, &paths::session_output_dir(&self.id)),
            is_error,
            changed,
        )
    }

    /// Run an MCP tool: its text, whether it failed, and the images and
    /// PDFs it returned (fitted to the image limits by the caller).
    async fn run_mcp(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> (String, bool, Vec<codesmith_ai::Media>) {
        let Some(t) = self.permitted_mcp().into_iter().find(|t| t.name == name) else {
            return (
                format!("The {name} tool is not available to this agent."),
                true,
                vec![],
            );
        };
        let shared = t.shared_name();
        if shared != name {
            // Renamed after a collision: a rule denying the shared name
            // (configured or not) still denies it.
            if let Err(e) = self.gate.assert_not_denied(&shared, &["*".to_string()]) {
                return (e.to_string(), true, vec![]);
            }
        }
        let args = tool::repair(&tool::input_repair::repair(input, &t.schema));
        let title = format!("Run MCP tool {name}");
        if let Err(e) = self.allowed(name, "*", title) {
            return (e, true, vec![]);
        }
        match self
            .mcp
            .call_media_asking(name, args, self.questioner.clone())
            .await
        {
            Ok((text, is_error, media)) => (
                tool::truncate(&text, self.limits, &paths::session_output_dir(&self.id)),
                is_error,
                media,
            ),
            Err(e) => (format!("MCP tool {name} failed: {e:#}"), true, vec![]),
        }
    }

    /// A file attached to a prompt by path, as `read` would see it: the path
    /// is checked with the same `external_directory` and `read` permission
    /// requests, then taken as media when it is an image, PDF or video
    /// (`Ok(None)` for anything else).
    pub fn attach(&self, path: &str) -> std::result::Result<Option<codesmith_ai::Media>, String> {
        let ctx = Ctx {
            cwd: self.cwd.clone(),
            root: self.root.clone(),
            gate: &self.gate,
            shell: self.shell.clone(),
            session_id: self.id.clone(),
            limits: self.limits,
            client: self.client.clone(),
            jobs: None,
            websearch: None,
            format: None,
            lsp: None,
            progress: None,
        };
        let target = crate::tool::read::authorize(&ctx, path)?;
        crate::media::attachment(&target, path, &self.loaded.config.media.image)
    }

    /// [`Session::attach`], with the file's absolute path as its source.
    pub fn attach_local(
        &self,
        path: &str,
    ) -> std::result::Result<Option<crate::attachment::Attached>, String> {
        let abs = self.cwd.join(path);
        Ok(self
            .attach(path)?
            .map(|m| crate::attachment::Attached::local(m, &abs)))
    }

    fn run_question(&mut self, input: &serde_json::Value) -> (String, bool) {
        let Some(q) = self.questioner.clone() else {
            return ("No user is available to answer questions in this session. Make a reasonable assumption, state it, and continue.".into(), true);
        };
        if let Err(e) = self.allowed("question", "*", "Ask the user".into()) {
            return (format!("Permission denied: question ({e})"), true);
        }
        let prompts = match crate::question::parse(&tool::repair(input)) {
            Ok(p) => p,
            Err(e) => return (e, true),
        };
        match tokio::task::block_in_place(|| q.ask(&prompts)) {
            Some(answers) => (crate::question::to_model_content(&prompts, &answers), false),
            None => {
                // Dismissing a question stops the turn, as a decline does.
                self.turn.stopped = Some("you dismissed the question");
                (runner::QUESTION_DISMISSED.into(), true)
            }
        }
    }

    fn run_skill(&self, input: &serde_json::Value) -> (String, bool) {
        let input = tool::repair(input);
        let Some(id) = input.get("id").and_then(|v| v.as_str()) else {
            return ("Missing required string parameter `id`".into(), true);
        };
        // Accept a valid prefix: quantized models sometimes add stray characters.
        let clean: String = id
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .collect();
        let Some(skill) = self.skills.iter().find(|s| s.id == id || s.id == clean) else {
            let ids: Vec<&str> = self.skills.iter().map(|s| s.id.as_str()).collect();
            return (
                format!("Unknown skill {id:?}. Available: {}", ids.join(", ")),
                true,
            );
        };
        if let Err(e) = self.allowed("skill", id, format!("Load the {id} skill")) {
            return (e, true);
        }
        (crate::skill::content(skill), false)
    }

    async fn run_subagent(
        &mut self,
        input: &serde_json::Value,
        sink: &mut dyn Sink,
    ) -> (String, bool) {
        match self.subagent(&tool::repair(input), sink).await {
            Ok(text) => (text, false),
            Err(e) => (e, true),
        }
    }

    async fn subagent(
        &mut self,
        input: &serde_json::Value,
        sink: &mut dyn Sink,
    ) -> Result<String, String> {
        let s = |k: &str| {
            input
                .get(k)
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let agent_id = s("agent").ok_or("Missing required string parameter `agent`")?;
        let task = s("prompt").ok_or("Missing required string parameter `prompt`")?;
        let label = s("description").unwrap_or_else(|| agent_id.clone());
        if !agent::delegable(&self.loaded.config, &agent_id) {
            let available = agent::subagents(&self.loaded.config);
            let names: Vec<&str> = available.iter().map(|a| a.0.as_str()).collect();
            return Err(format!(
                "Unknown subagent {agent_id:?}. Available: {}",
                names.join(", ")
            ));
        }
        let limit = self.loaded.config.subagent_depth.unwrap_or(MAX_DEPTH);
        if self.depth >= limit {
            return Err(format!("Subagent depth limit reached ({limit}). Increase \"subagent_depth\" to allow nested subagents."));
        }
        self.allowed(
            "subagent",
            &agent_id,
            format!("Run the {agent_id} agent: {label}"),
        )?;
        let store = self.child_store()?;
        // Only this session's own children can be continued.
        let resume = s("sessionID");
        if let Some(id) = &resume {
            let info = store
                .as_ref()
                .and_then(|st| st.get(id).ok().flatten())
                .ok_or_else(|| format!("Subagent session not found: {id}"))?;
            if info.parent_id.as_deref() != Some(self.id.as_str()) {
                return Err(format!(
                    "Session {id} is not a child of the current session"
                ));
            }
        }
        let task = match resume {
            Some(_) => task,
            None => format!("{}\n{task}", tool::subagent::PREAMBLE),
        };
        self.spawn_child(agent_id, s("model"), label, task, resume, store, sink)
            .await
    }

    fn child_store(&self) -> Result<Option<Store>, String> {
        match self.store.as_ref().and_then(|st| st.path()) {
            Some(p) => Ok(Some(Store::open(&p).map_err(|e| format!("{e:#}"))?)),
            None => Ok(None),
        }
    }

    /// Run `task` in a child session of `agent_id` to completion, and its
    /// report (`Err` when it failed or was cancelled).
    #[allow(clippy::too_many_arguments)]
    async fn spawn_child(
        &mut self,
        agent_id: String,
        model: Option<String>,
        label: String,
        task: String,
        resume: Option<String>,
        store: Option<Store>,
        sink: &mut dyn Sink,
    ) -> Result<String, String> {
        let opts = Options {
            questioner: None,
            mcp: Some(self.mcp.clone()),
            agent: Some(agent_id.clone()),
            model,
            resume,
            title: Some(label.clone()),
            parent: Some(self.id.clone()),
        };
        let mut child = Session::new(
            &self.loaded,
            self.cwd.clone(),
            opts,
            self.turn.asker.clone(),
            store,
        )
        .map_err(|e| format!("{e:#}"))?;
        child.depth = self.depth + 1;
        child.lsp = self.lsp.clone();
        sink.child_start(&ChildSession {
            id: child.id.clone(),
            parent_id: self.id.clone(),
            depth: child.depth,
            title: label.clone(),
        });
        let mut csink = ChildSink {
            parent: sink,
            label: agent_id.to_string(),
        };
        // The child is dropped (stopping its commands) once it reports, so
        // it waits for its background commands first. OpenCode reports at
        // once and resumes the child later; a child here cannot be resumed.
        let outcome = match child
            .prompt_from(Origin::Engine, &task, vec![], &mut csink)
            .await
        {
            Ok(()) => child.settle_background(&mut csink).await,
            Err(e) => Err(e),
        };
        add(&mut self.usage, &child.usage);
        // The child's own split: its total may mix billed and unbilled steps.
        add(&mut self.unbilled, &child.unbilled);
        match &outcome {
            Ok(()) if child.stopped().is_some() => sink.child_end(&child.id, "interrupted", None),
            Ok(()) => sink.child_end(&child.id, "completed", None),
            Err(e) => sink.child_end(&child.id, "failed", Some(&format!("{e:#}"))),
        }
        let answer = child
            .messages
            .iter()
            .rev()
            .find(|m| {
                m.role == Role::Assistant
                    && m.parts
                        .iter()
                        .any(|p| matches!(p, Part::Text(t) if !t.is_empty()))
            })
            .map(|m| {
                m.parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) if !t.is_empty() => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default();
        use tool::subagent::{report, State};
        match outcome {
            Ok(()) if child.stopped().is_some() => Err(report(
                &child.id,
                State::Cancelled,
                &label,
                if answer.is_empty() {
                    "Subagent cancelled"
                } else {
                    &answer
                },
            )),
            Ok(()) => Ok(report(&child.id, State::Completed, &label, &answer)),
            Err(e) => Err(report(&child.id, State::Error, &label, &format!("{e:#}"))),
        }
    }

    /// Run the plugins' async idle hooks (see
    /// [`crate::plugin::Plugin::session_idle`]): the message to continue
    /// the turn with, if one asks to. A hook asking to compact first gets
    /// its compaction (at most twice) and is asked again.
    async fn session_idle(
        &mut self,
        sink: &mut dyn Sink,
        continued: &mut u32,
    ) -> Result<Option<String>> {
        use crate::plugin::{Background, Hook, IdleAction, SessionIdle};
        let wanting =
            crate::plugin::active().wanting(&self.loaded.config, Hook::SessionIdle, &self.id);
        if wanting.is_empty() || *continued >= crate::plugin::MAX_IDLE_CONTINUATIONS {
            return Ok(None);
        }
        let mut compactions = 0;
        loop {
            let (action, usage) = {
                let generator = SideGenerator {
                    session: &*self,
                    usage: std::sync::Mutex::new(Usage::default()),
                };
                let idle = SessionIdle {
                    session_id: &self.id,
                    parent_id: self.parent.as_deref(),
                    messages: &self.messages,
                    background: self
                        .jobs
                        .running_jobs()
                        .into_iter()
                        .map(|j| Background::Shell {
                            id: j.id,
                            command: j.command,
                        })
                        .collect(),
                    generator: &generator,
                };
                let mut action = IdleAction::Idle;
                for p in &wanting {
                    action = p.session_idle(&idle).await;
                    if action != IdleAction::Idle {
                        break;
                    }
                }
                (action, generator.usage.into_inner().unwrap())
            };
            self.count_side_call(usage)?;
            match action {
                IdleAction::Idle => return Ok(None),
                IdleAction::Continue(text) => {
                    *continued += 1;
                    return Ok(Some(text));
                }
                IdleAction::Compact if compactions < 2 => {
                    compactions += 1;
                    if let Err(e) = self.compact(sink).await {
                        sink.notice(&format!(
                            "the compaction a plugin asked for did not run: {e:#}"
                        ));
                    }
                }
                IdleAction::Compact => return Ok(None),
            }
        }
    }

    /// A side request with this session's context (OpenCode's
    /// `session.generate`): the system prompt, history and tools the next
    /// step would send, then `prompt` as a user message, with tool calls
    /// switched off (the tools stay declared, and with them the cached
    /// prefix). Nothing is saved to the history; its tokens count toward
    /// the session's usage.
    ///
    /// The instructions are read fresh, as a step would read them, without
    /// recording anything: a change since the last step goes as an update
    /// before `prompt`, and a session whose instructions cannot be read yet
    /// fails before anything is sent.
    pub async fn generate(&mut self, prompt: &str) -> Result<String> {
        use crate::instruction_state as is;
        let list = self.instruction_sources()?;
        let o = is::observe(&list, self.instruction.as_ref())?;
        let preview = if o.initial {
            let text = is::join(&is::render_initial(&list, &o.current)?);
            let system = assemble_system(&self.base_system, &text, self.agent.id == "plan");
            (Some(system), None)
        } else {
            let text = is::update_text(&list, &o)?;
            (None, (!text.is_empty()).then_some(text))
        };
        let (text, usage) = self.generate_with(prompt, preview).await?;
        self.count_side_call(usage)?;
        if let (Some(s), true) = (&self.store, self.stored) {
            self.save_usage(s);
        }
        Ok(text)
    }

    /// [`Session::generate`] without counting its usage.
    async fn generate_text(&self, prompt: &str) -> Result<(String, Usage)> {
        self.generate_with(prompt, (None, None)).await
    }

    /// A side request with `preview`'s system prompt (when it has one) and
    /// instruction update (when there is one) before `prompt`.
    async fn generate_with(
        &self,
        prompt: &str,
        preview: (Option<String>, Option<String>),
    ) -> Result<(String, Usage)> {
        let (mut req, _) = self.request_for(crate::plugin::RequestKind::Generate);
        if let Some(system) = preview.0 {
            req.system = system;
        }
        if let Some(update) = preview.1 {
            req.messages.push(Message::system(update));
        }
        req.messages.push(Message::user(prompt));
        req.tool_choice = Some(codesmith_ai::ToolChoice::None);
        req.interaction = codesmith_ai::Interaction::Background;
        let model = self.model_for(crate::plugin::RequestKind::Generate);
        let (text, usage, _) = stream_text(
            &self.client,
            &model,
            &req,
            retry::Policy::default(),
            &mut |_| {},
        )
        .await?;
        Ok((text, usage))
    }

    /// Run the compaction a plugin admitted for this session (see
    /// [`crate::plugin::Host::compact`]), if one waits: a `Queue` one only
    /// before a prompt. Whether one ran; one that fails is reported to the
    /// sink and dropped.
    async fn run_admitted_compaction(
        &mut self,
        sink: &mut dyn Sink,
        at_prompt: bool,
    ) -> Result<bool> {
        use crate::plugin::host;
        if !crate::plugin::host_in_use() || !self.stored {
            return Ok(false);
        }
        let Some(st) = &self.store else {
            return Ok(false);
        };
        let Some(c) = host::pending(st, &self.id).ok().flatten() else {
            return Ok(false);
        };
        if !at_prompt && c.delivery == host::Delivery::Queue {
            return Ok(false);
        }
        st.set_meta(&self.id, host::COMPACT_KEY, None)?;
        if let Err(e) = self.compact(sink).await {
            sink.notice(&format!(
                "the compaction a plugin asked for did not run: {e:#}"
            ));
            return Ok(false);
        }
        Ok(true)
    }

    /// Record what call `id` changed in `path`, when the change is small
    /// enough to keep. A file that existed and is gone was deleted: its new
    /// text is empty.
    fn record_change(&mut self, id: &str, path: PathBuf, old: Option<String>) -> Result<()> {
        let new = match tool::peek_text(&path, store::CHANGE_MAX) {
            tool::Peek::Text(new) => new,
            tool::Peek::Missing if old.is_some() => String::new(),
            _ => return Ok(()),
        };
        let size = new.len() + old.as_ref().map_or(0, String::len);
        if old.as_deref() == Some(new.as_str()) || size > store::CHANGE_MAX {
            return Ok(());
        }
        self.record(&Record::Change {
            call: id.to_string(),
            path,
            old,
            new,
        })?;
        Ok(())
    }

    /// Count a model call made outside the steps (a summary or a title):
    /// its tokens go into the session usage, and a step record lets
    /// `codesmith stats` count them for the model that made the call.
    fn count_side_call(&mut self, usage: Usage) -> Result<()> {
        self.count_side_call_for(self.model_spec.clone(), usage)
    }

    /// [`Session::count_side_call`] for a call another model made.
    fn count_side_call_for(&mut self, model: String, usage: Usage) -> Result<()> {
        if usage == Usage::default() {
            return Ok(());
        }
        self.count_usage(&usage);
        self.record(&Record::Step { model, usage })?;
        Ok(())
    }

    /// How this session's title is made: by which model, and whether it
    /// waits for the turn. `None` when titles are turned off. A title model
    /// that does not resolve falls back to the session's.
    fn title_plan(&self) -> Option<TitlePlan> {
        let spec = title::model_spec(&self.loaded.config, &self.model_spec)?;
        let kind = crate::plugin::RequestKind::Title;
        let own = || TitlePlan {
            spec: self.model_spec.clone(),
            model: self.model_for(kind).into_owned(),
            defer: false,
            explicit_variant: false,
        };
        let mut plan = if spec == self.model_spec {
            own()
        } else {
            match self.loaded.config.resolve_model(&spec) {
                Ok(mut model) => {
                    let scope = crate::plugin::RequestScope {
                        session_id: self.id.clone(),
                        agent: self.agent.id.clone(),
                        model: spec.clone(),
                        kind,
                    };
                    if let Some(h) = crate::plugin::active().http_hooks(&self.loaded.config, scope)
                    {
                        model.provider.hooks = Some(h);
                    }
                    TitlePlan {
                        explicit_variant: crate::config::split_variant(&spec).1.is_some(),
                        spec,
                        model,
                        defer: false,
                    }
                }
                Err(_) => own(),
            }
        };
        plan.defer = plan.model.local && same_server(&plan.model, &self.model);
        Some(plan)
    }

    /// Start the title that waited for the turn's first response.
    fn start_pending_title(&mut self) {
        if let Some((first, plan)) = self.title_pending.take() {
            if self.title.is_none() && self.title_task.is_none() {
                self.title_task = Some(self.spawn_title(&first, Some(plan)));
            }
        }
    }

    /// Finish a deferred title still running, so it never overlaps this
    /// session's next request to the same server.
    async fn settle_title(&mut self) {
        if self.title_task.as_ref().is_some_and(|t| t.deferred) {
            self.finish_title().await;
        }
    }

    fn spawn_title(&self, first: &str, plan: Option<TitlePlan>) -> TitleTask {
        let fallback = title::fallback(first);
        let Some(plan) = plan else {
            // Titles are off: the first line names the session.
            return TitleTask {
                spec: self.model_spec.clone(),
                deferred: false,
                handle: tokio::spawn(async move { (Some(fallback), Usage::default()) }),
            };
        };
        let client = self.client.clone();
        let prompt = title::request(first);
        let session_id = self.id.to_string();
        let tag = self.tag();
        let TitlePlan {
            spec,
            model,
            defer,
            explicit_variant,
        } = plan;
        // A local server gets a short, thinking-free request; elsewhere the
        // reasoning options are left out (a capped request has no room).
        let max_tokens = if model.local {
            title::LOCAL_MAX_TOKENS
        } else {
            title::MAX_TOKENS
        };
        let (options, body) = if explicit_variant {
            (model.options.clone(), model.body.clone())
        } else if model.local {
            title::thinking_off(&model)
        } else {
            (Default::default(), model.body.clone())
        };
        let req = Request {
            model: model.model_id.clone(),
            system: title::SYSTEM.to_string(),
            messages: vec![Message::user(prompt.clone())],
            max_tokens: Some(max_tokens),
            temperature: model.temperature,
            options,
            body,
            cache: model.cache.clone(),
            interaction: codesmith_ai::Interaction::Background,
            prompt_cache_key: (!tag.cache_key.is_empty()).then(|| tag.cache_key.clone()),
            headers: tag.headers.clone(),
            ..Default::default()
        };
        let handle = tokio::spawn(async move {
            // GitHub Copilot names sessions with its free utility model;
            // when that fails the usual title request runs.
            if let Some(utility) = (model.provider_id == crate::providers::copilot::ID)
                .then(crate::providers::copilot::utility_title_model)
                .flatten()
            {
                let named = crate::providers::copilot::utility_title(
                    &client,
                    &model.provider,
                    utility,
                    &session_id,
                    title::SYSTEM,
                    &[("user", &prompt)],
                    Some(title::MAX_TOKENS),
                )
                .await;
                if let Some(t) = named.ok().and_then(|t| title::clean(&t)) {
                    return (Some(t), Usage::default());
                }
            }
            let reply = stream_text(
                &client,
                &model,
                &req,
                retry::Policy::one_shot(),
                &mut |_| {},
            )
            .await
            .ok();
            let usage = reply.as_ref().map(|r| r.1).unwrap_or_default();
            let title = reply.and_then(|r| title::clean(&r.0)).unwrap_or(fallback);
            (Some(title), usage)
        });
        TitleTask {
            spec,
            deferred: defer,
            handle,
        }
    }

    async fn finish_title(&mut self) {
        let Some(task) = self.title_task.take() else {
            return;
        };
        let timeout = tokio::time::timeout(Duration::from_secs(20), task.handle).await;
        if let Ok(Ok((title, usage))) = timeout {
            let _ = self.count_side_call_for(task.spec, usage);
            let Some(t) = title else {
                return;
            };
            // A title set while this one was generating wins.
            if self.title.is_some() {
                return;
            }
            if let (Some(s), true) = (&self.store, self.stored) {
                let _ = s.set_title(&self.id, &t);
            }
            self.title = Some(t);
        }
    }

    /// One model call with retries (OpenCode's schedule, see `retry`). A
    /// response that fails part way is retried from the prompt: its partial
    /// output is dropped, and no tool from it has run yet. A failure that
    /// ends the turn keeps the text and reasoning that streamed before it in
    /// the history, as OpenCode does; its tool calls never ran and are left
    /// out. On the final step the model is told to answer in text and tool
    /// calls are switched off.
    async fn step(
        &mut self,
        sink: &mut dyn Sink,
        final_step: bool,
    ) -> Result<(Vec<Part>, FinishReason, Usage)> {
        // Pick up tool and prompt lists a server announced as changed.
        self.mcp.refresh().await;
        let (mut req, renamed) = self.request();
        self.turn.offered = req.tools.iter().map(|t| t.name.clone()).collect();
        self.turn.schemas = req
            .tools
            .iter()
            .map(|t| (t.name.clone(), t.parameters.clone()))
            .collect();
        self.turn.renamed = renamed;
        if final_step {
            req.messages.push(Message::system(runner::MAX_STEPS_PROMPT));
            // The tool definitions stay, so the cached prefix does too.
            req.tool_choice = Some(codesmith_ai::ToolChoice::None);
        }
        self.warm_send(Some(&req));
        let mut policy = retry::Policy::default().for_protocol(self.model.provider.protocol);
        let mut attempt = 0;
        loop {
            match self.attempt(&req, sink).await.and_then(unknown_finish) {
                Ok(r) => return Ok(r),
                Err(codesmith_ai::Failure { error: e, partial }) => {
                    let Some(wait) = policy.next(&e, std::time::Instant::now()) else {
                        let kept: Vec<Part> = partial
                            .map(|p| p.parts)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|p| !matches!(p, Part::ToolCall { .. }))
                            .collect();
                        if !kept.is_empty() {
                            self.push(Message {
                                role: Role::Assistant,
                                parts: kept,
                            })?;
                        }
                        return Err(anyhow!("{e}"));
                    };
                    attempt += 1;
                    let wait = wait.mul_f64(self.turn.retry_scale);
                    sink.retry(attempt, &e.to_string(), wait);
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    async fn attempt(
        &self,
        req: &Request,
        sink: &mut dyn Sink,
    ) -> Result<(Vec<Part>, FinishReason, Usage), codesmith_ai::Failure> {
        let model = self.model_for(crate::plugin::RequestKind::Primary);
        let stream = match model.websocket {
            Some(route) => {
                self.ws
                    .stream(&model.provider, &self.client, req, route)
                    .await
            }
            None => model.provider.stream(&self.client, req).await,
        };
        let stream = stream.map_err(|error| codesmith_ai::Failure {
            error,
            partial: None,
        })?;
        // Parts in stream order, reasoning with its provider signature. A
        // stream that ends without finishing was cut off, and is retried.
        let mut inputs = InputPairs::default();
        let mut hosted = HostedRows::default();
        let r = codesmith_ai::assemble(stream, |ev| {
            inputs.push(ev, &mut *sink);
            hosted.push(ev, &mut *sink);
            match ev {
                Event::TextDelta(d) => sink.text(d),
                Event::ReasoningDelta(d) => sink.reasoning(d),
                Event::Media(m) => sink.media(m),
                _ => {}
            }
        })
        .await;
        inputs.settle(sink);
        hosted.settle(sink);
        match r {
            Ok(mut r) => {
                // A paused Anthropic turn resumes the call in the next
                // request; any other end leaves it failed for good.
                if r.reason.raw.as_deref() != Some("pause_turn") {
                    fail_unresolved_hosted(&mut r.parts);
                }
                Ok((r.parts, r.reason, r.usage))
            }
            Err(mut f) => {
                if let Some(p) = f.partial.as_mut() {
                    fail_unresolved_hosted(&mut p.parts);
                }
                Err(f)
            }
        }
    }
}

/// Settle hosted calls whose result never came as failed (OpenCode's
/// durable "Provider did not return a tool result"), so the stored step
/// never keeps a call that is still open.
fn fail_unresolved_hosted(parts: &mut [Part]) {
    for p in parts {
        if let Part::Hosted(h) = p {
            if h.result.is_none() {
                h.result = Some(h.missing_result());
            }
        }
    }
}

/// Pairs streamed tool inputs with the calls they form, for the sink. A
/// call follows the end of its input straight away, so it pairs with the
/// input that just ended whatever its id; an input that ends without a
/// call after it was dropped.
#[derive(Default)]
struct InputPairs {
    /// Inputs still streaming.
    forming: Vec<String>,
    /// The input that just ended, waiting for its call.
    ended: Option<String>,
}

impl InputPairs {
    fn push(&mut self, ev: &Event, sink: &mut dyn Sink) {
        match ev {
            Event::ToolCall { id, .. } => {
                if let Some(input) = self.ended.take() {
                    sink.tool_input_end(&input, Some(id));
                }
                return;
            }
            Event::ToolInputDelta { .. } => return,
            _ => {}
        }
        if let Some(input) = self.ended.take() {
            sink.tool_input_end(&input, None);
        }
        match ev {
            Event::ToolInputStart { id, name } if !self.forming.contains(id) => {
                self.forming.push(id.clone());
                sink.tool_input(id, name);
            }
            Event::ToolInputEnd { id } => {
                if let Some(i) = self.forming.iter().position(|f| f == id) {
                    self.ended = Some(self.forming.remove(i));
                }
            }
            _ => {}
        }
    }

    /// The stream is over: inputs that formed no call were dropped.
    fn settle(self, sink: &mut dyn Sink) {
        for input in self.ended.into_iter().chain(self.forming) {
            sink.tool_input_end(&input, None);
        }
    }
}

/// Shows the tools the provider ran itself (web search, code execution) as
/// tool rows: started at their call, settled by their result. They never
/// run here, and a call whose result never came settles as an error, as in
/// OpenCode.
#[derive(Default)]
struct HostedRows {
    /// Calls shown without a result yet: id and name.
    open: Vec<(String, String)>,
}

impl HostedRows {
    fn push(&mut self, ev: &Event, sink: &mut dyn Sink) {
        match ev {
            Event::HostedCall(h) => {
                self.open.push((h.id.clone(), h.name.clone()));
                sink.tool_start(&h.id, &h.name, &h.input);
            }
            Event::HostedResult(h) => {
                match self.open.iter().position(|(id, _)| *id == h.id) {
                    Some(i) => {
                        self.open.remove(i);
                    }
                    None => sink.tool_start(&h.id, &h.name, &h.input),
                }
                if let Some(r) = &h.result {
                    if !r.media.is_empty() {
                        sink.tool_media(&h.id, &r.media);
                    }
                    let summary = h.summary().unwrap_or_default();
                    sink.tool_end(&h.id, &h.name, &summary, r.is_error);
                }
            }
            _ => {}
        }
    }

    fn settle(self, sink: &mut dyn Sink) {
        for (id, name) in self.open {
            sink.tool_end(&id, &name, "Tool result missing", true);
        }
    }
}

/// A response that ended with a finish reason the provider did not name
/// (OpenCode's normalized `unknown`) and ran no tool is treated as a failed
/// attempt, so it is retried from the prompt like an incomplete stream; its
/// text and reasoning are kept only if the retries run out.
fn unknown_finish(
    r: (Vec<Part>, FinishReason, Usage),
) -> Result<(Vec<Part>, FinishReason, Usage), codesmith_ai::Failure> {
    let (parts, reason, usage) = r;
    let tools = parts
        .iter()
        .any(|p| matches!(p, Part::ToolCall { .. } | Part::Hosted(_)));
    if reason.normalized != codesmith_ai::Reason::Unknown || tools {
        return Ok((parts, reason, usage));
    }
    Err(codesmith_ai::Failure {
        error: codesmith_ai::Error::Provider {
            message: format!(
                "the provider ended the response with an unknown finish reason ({})",
                reason.as_str()
            ),
            retryable: true,
        },
        partial: Some(Box::new(codesmith_ai::Response {
            parts,
            reason,
            usage,
            ..Default::default()
        })),
    })
}

/// A tool-free model call that returns the whole text reply.
/// A command's first line, cut to 60 characters, for a progress notice.
fn short_command(command: &str) -> String {
    let line = command.lines().next().unwrap_or("").trim();
    if line.chars().count() > 60 || command.trim().contains('\n') {
        format!("{}…", line.chars().take(60).collect::<String>())
    } else {
        line.to_string()
    }
}

pub async fn complete(
    client: &reqwest::Client,
    model: &ModelRef,
    system: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
) -> Result<(String, Usage)> {
    complete_with(
        client,
        model,
        &Tag::default(),
        system,
        messages,
        max_tokens,
        retry::Policy::default(),
    )
    .await
}

/// Tells plugins about a turn dropped before it finished.
struct Stopped {
    plugins: Vec<Arc<dyn crate::plugin::Plugin>>,
    id: String,
    finished: bool,
}

impl Drop for Stopped {
    fn drop(&mut self) {
        if !self.finished {
            for p in &self.plugins {
                p.interrupted(&self.id);
            }
        }
    }
}

/// Side requests for an idle hook, with the session's context.
struct SideGenerator<'a> {
    session: &'a Session,
    usage: std::sync::Mutex<Usage>,
}

impl crate::plugin::Generate for SideGenerator<'_> {
    fn generate<'a>(
        &'a self,
        prompt: &'a str,
    ) -> crate::plugin::BoxFuture<'a, Result<String, crate::plugin::GenerateError>> {
        Box::pin(async move {
            match self.session.generate_text(prompt).await {
                Ok((text, usage)) => {
                    self.usage.lock().unwrap().add(&usage);
                    Ok(text)
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    Err(if compaction::is_overflow(&e) {
                        crate::plugin::GenerateError::Overflow(e)
                    } else {
                        crate::plugin::GenerateError::Failed(e)
                    })
                }
            }
        })
    }
}

/// A running title request and the model it went to.
struct TitleTask {
    spec: String,
    /// It waited for the turn (see [`TitlePlan::defer`]).
    deferred: bool,
    handle: tokio::task::JoinHandle<(Option<String>, Usage)>,
}

/// How a session's title is made.
struct TitlePlan {
    spec: String,
    model: ModelRef,
    /// The title model is a local server the turn's model also runs on,
    /// which serves one request at a time: the title waits for the turn's
    /// first response and never runs beside this session's requests.
    defer: bool,
    /// The title spec chose a variant, whose options are kept.
    explicit_variant: bool,
}

/// Whether two models are served from one place: the same provider, or the
/// same host and port.
fn same_server(a: &ModelRef, b: &ModelRef) -> bool {
    let at = |m: &ModelRef| {
        url::Url::parse(&m.provider.base_url)
            .ok()
            .map(|u| (u.host_str().map(str::to_string), u.port_or_known_default()))
    };
    a.provider_id == b.provider_id || (at(a).is_some() && at(a) == at(b))
}

/// The headers and prompt cache key that tie a session's model requests
/// together (OpenCode's session correlation headers, with CodeSmith's
/// names for its own): a subagent's requests use its parent's id as their
/// affinity, so they reach the same provider cache.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tag {
    pub headers: Vec<(String, String)>,
    pub cache_key: String,
}

impl Tag {
    pub(crate) fn new(id: &str, parent: Option<&str>) -> Tag {
        let affinity = parent.unwrap_or(id);
        let mut headers = vec![("x-codesmith-session-id".to_string(), id.to_string())];
        if let Some(p) = parent {
            headers.push(("x-codesmith-parent-session-id".into(), p.into()));
        }
        headers.push(("x-session-affinity".into(), affinity.into()));
        headers.push(("X-Session-Id".into(), affinity.into()));
        if let Some(p) = parent {
            headers.push(("x-parent-session-id".into(), p.into()));
        }
        headers.push((
            "User-Agent".into(),
            format!("codesmith/{}", env!("CARGO_PKG_VERSION")),
        ));
        headers.push(("x-codesmith-session".into(), affinity.into()));
        headers.push(("x-codesmith-client".into(), "codesmith".into()));
        Tag {
            headers,
            cache_key: prompt_cache_key(affinity),
        }
    }
}

/// The prompt cache key for a session id: OpenAI takes at most 64
/// characters, so a `ses_` id with a 64-digit hex body (OpenCode's, as
/// imported) loses its prefix instead of being cut at the end, which keeps
/// two such sessions apart.
pub fn prompt_cache_key(id: &str) -> String {
    match id.strip_prefix("ses_") {
        Some(hex) if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
            hex.to_string()
        }
        _ => id.to_string(),
    }
}

async fn complete_with(
    client: &reqwest::Client,
    model: &ModelRef,
    tag: &Tag,
    system: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
    policy: retry::Policy,
) -> Result<(String, Usage)> {
    complete_streaming(
        client,
        model,
        tag,
        system,
        messages,
        max_tokens,
        policy,
        &mut |_| {},
    )
    .await
    .map(|(text, usage, _)| (text, usage))
}

/// [`complete_with`], handing each text delta to `delta` as it arrives (a
/// retried attempt streams again from the start).
#[allow(clippy::too_many_arguments)]
async fn complete_streaming(
    client: &reqwest::Client,
    model: &ModelRef,
    tag: &Tag,
    system: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
    policy: retry::Policy,
    delta: &mut (dyn FnMut(&str) + Send),
) -> Result<(String, Usage, Option<FinishReason>)> {
    let req = Request {
        model: model.model_id.clone(),
        system: system.to_string(),
        messages: messages.to_vec(),
        tools: vec![],
        max_tokens: max_tokens.or(model.max_output),
        temperature: model.temperature,
        // A capped side request (the title) has no room for thinking, so it
        // leaves the reasoning options out.
        options: if max_tokens.is_some() {
            Default::default()
        } else {
            model.options.clone()
        },
        body: model.body.clone(),
        cache: model.cache.clone(),
        // Capped side requests are titles; the others summarize.
        interaction: if max_tokens.is_some() {
            codesmith_ai::Interaction::Background
        } else {
            codesmith_ai::Interaction::Compaction
        },
        prompt_cache_key: (!tag.cache_key.is_empty()).then(|| tag.cache_key.clone()),
        headers: tag.headers.clone(),
        ..Default::default()
    };
    stream_text(client, model, &req, policy, delta).await
}

/// Send `req`, retrying under `policy`, and collect its text.
async fn stream_text(
    client: &reqwest::Client,
    model: &ModelRef,
    req: &Request,
    policy: retry::Policy,
    delta: &mut (dyn FnMut(&str) + Send),
) -> Result<(String, Usage, Option<FinishReason>)> {
    let mut policy = policy.for_protocol(model.provider.protocol);
    loop {
        let mut out = String::new();
        let mut usage = Usage::default();
        let mut reason = None;
        let res: Result<(), codesmith_ai::Error> = async {
            let mut s = model.provider.stream(client, req).await?;
            while let Some(ev) = s.next().await {
                match ev? {
                    Event::TextDelta(d) => {
                        delta(&d);
                        out.push_str(&d)
                    }
                    Event::Finish {
                        usage: u,
                        reason: r,
                        ..
                    } => {
                        usage = u;
                        reason = Some(r);
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => return Ok((out, usage, reason)),
            Err(e) => match policy.next(&e, std::time::Instant::now()) {
                Some(wait) => tokio::time::sleep(wait).await,
                None => return Err(anyhow!("{e}")),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Run,
    Refuse,
    Abort,
}

/// Catches a model stuck repeating the identical tool call.
#[derive(Debug, Default)]
pub struct RepeatGuard {
    last: Option<String>,
    count: u32,
}

impl RepeatGuard {
    /// Identical calls in a row before further ones are refused.
    pub const REFUSE: u32 = 3;
    /// Identical calls in a row before the turn is stopped.
    pub const ABORT: u32 = 6;

    pub fn check(&mut self, name: &str, input: &serde_json::Value) -> Repeat {
        let key = format!("{name}\u{0}{input}");
        if self.last.as_deref() == Some(key.as_str()) {
            self.count += 1;
        } else {
            self.last = Some(key);
            self.count = 1;
        }
        if self.count > Self::ABORT {
            Repeat::Abort
        } else if self.count > Self::REFUSE {
            Repeat::Refuse
        } else {
            Repeat::Run
        }
    }

    pub fn refusal(name: &str) -> String {
        format!("Not run: this is the same {name} call with the same input {} times in a row. Repeating it will not change the result. Read the earlier result, then try a different approach or different input.", Self::REFUSE + 1)
    }
}

/// Undo the newest turn of a stored session: restore the files it changed
/// and hide its messages. Returns the restored changes, or `None` if there
/// is nothing to undo.
pub fn undo(db: &Store, id: &str, snaps: &Snapshots) -> Result<Option<Vec<Change>>> {
    let entries = db.entries(id)?;
    let Some((from_seq, tree)) = store::undo_target(&entries) else {
        return Ok(None);
    };
    let paths = turn_paths(snaps, &tree, &store::turn_ends(&entries, from_seq, None))?;
    let after = snaps.capture()?;
    let changes = snaps.restore_only(&tree, paths.as_deref())?;
    db.append(id, &Record::Revert { from_seq, after })?;
    Ok(Some(changes))
}

/// Redo the newest undo, if nothing was added since.
pub fn redo(db: &Store, id: &str, snaps: &Snapshots) -> Result<Option<Vec<Change>>> {
    let entries = db.entries(id)?;
    let Some((revert_seq, after)) = store::redo_target(&entries) else {
        return Ok(None);
    };
    let paths = entries
        .iter()
        .find_map(|e| match (&e.record, e.seq == revert_seq) {
            (Record::Revert { from_seq, .. }, true) => Some(*from_seq),
            _ => None,
        })
        .and_then(|from_seq| {
            let start = entries.iter().find_map(|e| match &e.record {
                Record::Snapshot { tree } if e.seq == from_seq => Some(tree.clone()),
                _ => None,
            })?;
            let ends = store::turn_ends(&entries, from_seq, Some(revert_seq));
            turn_paths(snaps, &start, &ends).ok().flatten()
        });
    let changes = snaps.restore_only(&after, paths.as_deref())?;
    db.append(id, &Record::Redo { revert_seq })?;
    Ok(Some(changes))
}

/// The paths changed between a turn's start tree and its end trees, or
/// `None` when no end was recorded (then everything since is restored).
fn turn_paths(snaps: &Snapshots, start: &str, ends: &[String]) -> Result<Option<Vec<String>>> {
    if ends.is_empty() {
        return Ok(None);
    }
    let mut paths = Vec::new();
    for end in ends {
        for c in snaps.changes(start, end)? {
            if !paths.contains(&c.path) {
                paths.push(c.path);
            }
        }
    }
    Ok(Some(paths))
}

/// The unified diff of files changed since the newest visible turn began.
pub fn turn_diff(db: &Store, id: &str, snaps: &Snapshots) -> Result<Option<String>> {
    let entries = db.entries(id)?;
    let Some((_, tree)) = store::undo_target(&entries) else {
        return Ok(None);
    };
    let now = snaps.capture()?;
    Ok(Some(snaps.diff(&tree, &now)?))
}

/// The system prompt: the base, the instruction baseline, and the plan
/// agent's reminder.
/// Tells Claude Agent's registry the session stopped running (OpenCode's
/// `session.execution.*` events).
struct ClaudeIdle(String);

impl Drop for ClaudeIdle {
    fn drop(&mut self) {
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let id = std::mem::take(&mut self.0);
            rt.spawn(async move { codesmith_ai::claude_agent::global().idle(&id).await });
        }
    }
}

/// The instruction sources Claude Code writes itself.
const CLAUDE_WRITES: [&str; 2] = ["core/date", "core/environment"];

/// The tool calls a stopped turn left with a result rather than run: a
/// declined call, a dismissed question, or a call the stop interrupted
/// (OpenCode's aborted tool results).
fn aborted_calls(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            Part::ToolResult {
                id,
                content,
                is_error: true,
                ..
            } if content == runner::DECLINED
                || content == runner::QUESTION_DISMISSED
                || content.starts_with(runner::TOOLS_INTERRUPTED) =>
            {
                Some(id.clone())
            }
            _ => None,
        })
        .collect()
}

fn assemble_system(base: &str, instructions: &str, plan: bool) -> String {
    let mut parts = vec![base];
    if !instructions.is_empty() {
        parts.push(instructions);
    }
    if plan {
        parts.push(agent::PLAN_REMINDER);
    }
    parts.join("\n\n")
}

fn add(total: &mut Usage, u: &Usage) {
    crate::stats::add(total, u);
}

/// A new session id.
pub fn new_id() -> String {
    use rand::Rng;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "ses_{t:x}{:06x}",
        rand::thread_rng().gen::<u32>() & 0xffffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_renamed_session_reaches_its_own_tool_output() {
        let dir = tempfile::tempdir().unwrap();
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "model": "fake/m", "snapshots": false,
            "providers": {"fake": {"protocol": "openai-chat",
                "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "x"}}}
        }))
        .unwrap();
        let loaded = Loaded {
            config,
            files: vec![],
            root: dir.path().to_path_buf(),
        };
        let mut s = Session::new(
            &loaded,
            dir.path().to_path_buf(),
            Options {
                agent: None,
                model: None,
                resume: None,
                title: Some("t".into()),
                parent: None,
                questioner: None,
                mcp: None,
            },
            Arc::new(crate::permission::Fixed(crate::permission::Reply::Reject(
                None,
            ))),
            None,
        )
        .unwrap();
        let output = |id: &str| {
            vec![paths::session_output_dir(id)
                .join("tool_1")
                .display()
                .to_string()]
        };
        let generated = s.id.clone();
        assert!(s.gate.allows("external_directory", &output(&generated)));
        s.set_id("ses_hosted");
        assert_eq!(s.id, "ses_hosted");
        assert!(s.gate.allows("external_directory", &output("ses_hosted")));
        assert!(!s.gate.allows("external_directory", &output(&generated)));
        assert!(!s.gate.allows("external_directory", &output("ses_other")));
    }

    #[test]
    fn failed_resumes_back_off_exponentially_up_to_30s() {
        let waits: Vec<u64> = (1..=8).map(|n| resume_backoff(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(resume_backoff(u32::MAX), RESUME_BACKOFF_MAX);
    }

    #[test]
    fn ids_look_right() {
        let a = new_id();
        assert!(a.starts_with("ses_") && a.len() > 10);
        assert_ne!(a, new_id());
    }

    #[test]
    fn repeat_guard() {
        let mut g = RepeatGuard::default();
        let a = serde_json::json!({"id": "x"});
        let b = serde_json::json!({"id": "y"});
        assert_eq!(g.check("skill", &a), Repeat::Run);
        assert_eq!(g.check("skill", &a), Repeat::Run);
        assert_eq!(g.check("skill", &a), Repeat::Run);
        assert_eq!(g.check("skill", &a), Repeat::Refuse);
        assert_eq!(g.check("skill", &b), Repeat::Run, "different input resets");
        for _ in 0..5 {
            g.check("read", &a);
        }
        assert_eq!(g.check("read", &a), Repeat::Refuse);
        assert_eq!(g.check("read", &a), Repeat::Abort);
        assert!(RepeatGuard::refusal("read").contains("4 times"));
    }

    #[test]
    fn usage_adds_up() {
        let mut t = Usage::default();
        add(
            &mut t,
            &Usage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
                ..Default::default()
            },
        );
        add(
            &mut t,
            &Usage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
                ..Default::default()
            },
        );
        assert_eq!(
            t,
            Usage {
                input: 2,
                output: 4,
                cache_read: 6,
                cache_write: 8,
                ..Default::default()
            }
        );
    }
}
