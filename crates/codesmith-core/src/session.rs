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
use std::path::PathBuf;
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
const TITLE_MAX_TOKENS: u32 = 256;
const SUMMARY_SYSTEM: &str = "You are a helpful assistant that writes concise, structured summaries of coding sessions so another agent can resume the work.";

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

pub struct Session {
    /// Change it with [`Session::set_id`], which moves the session's
    /// tool-output allowance along.
    pub id: String,
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub agent: Agent,
    pub model: ModelRef,
    pub model_spec: String,
    pub system: String,
    pub tools: Vec<Tool>,
    pub messages: Vec<Message>,
    /// Store sequence number of each message (`None` for a checkpoint).
    seqs: Vec<Option<i64>>,
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
    title_task: Option<tokio::task::JoinHandle<(Option<String>, Usage)>>,
    /// Whether the user was told that compaction cannot help (said once).
    compaction_skipped: bool,
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
    /// Tool names offered to the model in the latest request.
    offered: Vec<String>,
    /// Scales retry waits (tests shorten them).
    retry_scale: f64,
    /// Resumes for finished background commands that failed in a row
    /// before reporting them, and when the next one may start.
    resume_failures: u32,
    resume_due: Option<std::time::Instant>,
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
        let mut seen = 0;
        let mut ran_with = None;
        let resumed: Option<Resumed> = match (&opts.resume, &store) {
            (Some(id), Some(s)) => {
                let info = s.get(id)?.ok_or_else(|| anyhow!("no session {id}"))?;
                let entries = s.entries(id)?;
                seen = entries.last().map_or(0, |e| e.seq);
                ran_with = last_step_model(&entries);
                Some((info, store::history_with_seqs(&entries)))
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
            .or_else(|| resumed.as_ref().map(|r| r.0.agent.clone()))
            .or_else(|| config.default_agent.clone())
            .unwrap_or_else(|| "build".into());
        let agent = agent::resolve(config, &agent_id).ok_or_else(|| {
            anyhow!(
                "unknown agent {agent_id:?}; available: {}",
                agent::list(config).join(", ")
            )
        })?;
        let model_spec = opts
            .model
            .or_else(|| resumed.as_ref().map(|r| r.0.model.clone()))
            .or_else(|| agent.model.clone())
            .or_else(|| config.model.clone())
            .ok_or_else(|| anyhow!("no model configured: pass -m provider/model or set \"model\" in codesmith.json"))?;
        let model = config.resolve_model(&model_spec)?;
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
        let lsp = crate::lsp::Lsp::new(config.lsp.as_ref(), loaded.root.clone()).map(Arc::new);
        if lsp.is_none() {
            tools.retain(|t| *t != Tool::Lsp);
        }
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let tmp = paths::tmp_dir();
        let _ = std::fs::create_dir_all(&tmp);
        let env = prompt::environment(&prompt::Environment {
            cwd: &cwd,
            root: &loaded.root,
            is_git: loaded.root.join(".git").exists(),
            tmp: &tmp,
            date: prompt::today(),
        });
        let files = instructions::discover(
            &cwd,
            &loaded.root,
            &paths::config_dir(),
            &config.instructions,
        );
        let mut system = prompt::assemble(
            agent.system.as_deref(),
            &model.model_id,
            &names,
            &env,
            &instructions::render(&files),
        );
        if tools.contains(&Tool::Skill) {
            system.push_str("\n\n");
            system.push_str(&crate::skill::instructions(&skills));
        }
        if agent.id == "plan" {
            system.push_str("\n\n");
            system.push_str(agent::PLAN_REMINDER);
        }
        let shell = config
            .shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "/bin/sh".into());
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
        let gate = Gate::new(rules, watch(asker.clone(), &declined));
        Ok(Session {
            id,
            cwd,
            root: loaded.root.clone(),
            agent,
            model,
            model_spec,
            system,
            tools,
            messages,
            seqs,
            ran_with,
            usage,
            unbilled,
            title,
            last_prompt_tokens: 0,
            compaction_skipped: false,
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
                retry_scale: 1.0,
                resume_failures: 0,
                resume_due: None,
            },
            shared: Shared {
                seen,
                stale: false,
                holder: format!("{}-{}", std::process::id(), new_id()),
                wait: true,
            },
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
                let (seqs, messages) = store::history_with_seqs(&entries).into_iter().unzip();
                self.messages = messages;
                self.seqs = seqs;
                self.ran_with = last_step_model(&entries);
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

    fn request(&self) -> Request {
        let plugins = crate::plugin::active();
        let mut req = self.base_request(&plugins);
        plugins.request(&self.loaded.config, &self.id, &mut req);
        req
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
            messages: crate::media::sendable(
                &runner::model_messages(&self.messages),
                self.model.input.as_deref(),
            ),
            tools: self.tool_specs(plugins, &code),
            namespaces: Vec::new(),
            max_tokens: self.model.max_output,
            temperature: self.model.temperature,
            options: self.model.options.clone(),
            body: self.model.body.clone(),
            cache: self.model.cache.clone(),
            // Stable for the session, so its requests reach the same cache.
            prompt_cache_key: Some(self.id.clone()),
            tool_choice: None,
            parallel_tool_calls: None,
        }
    }

    /// The system prompt with Code Mode and MCP server instructions.
    fn system_prompt(&self, code: &[crate::codemode::Entry]) -> String {
        let mut system = self.system.clone();
        if !code.is_empty() {
            system = format!("{system}\n\n{}", crate::codemode::instructions(code));
        }
        if let Some(mcp) = self.mcp_instructions(code) {
            system = format!("{system}\n\n{mcp}");
        }
        system
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
        let execute = (!code.is_empty()).then(|| {
            let (description, parameters) = crate::codemode::spec();
            codesmith_ai::ToolSpec {
                name: crate::codemode::TOOL.into(),
                description,
                parameters,
            }
        });
        self.tools
            .iter()
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
            .collect()
    }

    /// One user turn: runs steps until the model answers without tool calls.
    ///
    /// Boxed with an explicit `Send` bound: a subagent's turn runs inside its
    /// parent's, and naming the type breaks that recursive future.
    pub fn prompt<'a>(
        &'a mut self,
        text: &'a str,
        sink: &'a mut dyn Sink,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.prompt_inner(text, vec![], sink))
    }

    /// A user turn with images or PDFs attached after the text.
    pub fn prompt_with<'a>(
        &'a mut self,
        text: &'a str,
        media: Vec<codesmith_ai::Media>,
        sink: &'a mut dyn Sink,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.prompt_inner(text, media, sink))
    }

    async fn prompt_inner(
        &mut self,
        text: &str,
        media: Vec<codesmith_ai::Media>,
        sink: &mut dyn Sink,
    ) -> Result<()> {
        let _lease = self.take_turn(sink).await?;
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
        if self.title.is_none() && self.title_task.is_none() {
            self.title_task = Some(self.spawn_title(text));
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
        let mut user = Message::user(text);
        user.parts.extend(media.into_iter().map(Part::Media));
        self.push(user)?;
        let plugins = crate::plugin::active();
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
        // The title call's tokens count too, so finish it before saving usage.
        self.finish_title().await;
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
        // Responses in a row cut off by the output token limit.
        let mut truncated = 0u32;
        loop {
            self.report_finished_jobs()?;
            if !compacted_this_step && self.due_for_compaction() {
                if self.worth_compacting() {
                    self.compact(sink).await?;
                    compacted_this_step = true;
                } else if !self.compaction_skipped {
                    self.compaction_skipped = true;
                    sink.notice(&format!(
                        "the prompt (~{} tokens) is over the compaction limit, but the recent history is already small: the system prompt and tool definitions take most of it, so it is not compacted. Use a model with a larger context or fewer tools.",
                        self.last_prompt_tokens.max(self.estimate_prompt())
                    ));
                }
            }
            // The last allowed step must answer in text (OpenCode's MAX_STEPS).
            let final_step = steps + 1 >= max_steps;
            let (parts, reason, usage) = match self.step(sink, final_step).await {
                Ok(r) => r,
                Err(e)
                    if !overflow_compacted
                        && self.loaded.config.compaction.auto()
                        && compaction::is_overflow(&e.to_string()) =>
                {
                    overflow_compacted = true;
                    self.compact(sink).await?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            steps += 1;
            compacted_this_step = false;
            self.count_usage(&usage);
            self.last_prompt_tokens =
                usage.input + usage.cache_read + usage.cache_write + usage.output;
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
                    return Ok(());
                }
                // The model answered while background commands still run:
                // the turn ends here, as in OpenCode, and each command is
                // reported when it finishes (`resume_background`, or the
                // next prompt). One-shot callers (`codesmith run`,
                // subagents, automations) wait with `settle_background`.
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
                sink.tool_start(&id, &name, &input);
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
                let before = self.file_before(&name, &input);
                let mut media = Vec::new();
                let (content, is_error) = match Tool::from_name(&name).filter(|t| t.session_level())
                {
                    _ if final_step => (runner::TOOLS_DISABLED.into(), true),
                    _ if !self.turn.offered.contains(&name) => (runner::unavailable(&name), true),
                    _ if runner::invalid_json(&input).is_some() => (
                        runner::invalid_input(runner::invalid_json(&input).unwrap_or(""), length),
                        true,
                    ),
                    _ if verdict == Repeat::Refuse => (RepeatGuard::refusal(&name), true),
                    _ if refused.is_some() => (refused.clone().unwrap_or_default(), true),
                    _ if name == crate::codemode::TOOL && !self.code_entries().is_empty() => {
                        self.run_code(&input).await
                    }
                    _ if name == crate::automation::TOOL && self.store.is_some() => {
                        self.run_automation(&input).await
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
                    Some(Tool::Subagent) => self.run_subagent(&input, sink).await,
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
                                    Some(p) = updates.recv() => forward_progress(sink, &id, p),
                                }
                            };
                            while let Ok(p) = updates.try_recv() {
                                forward_progress(sink, &id, p);
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
                if let (false, Some((path, old))) = (is_error, before) {
                    if let Err(e) = self.record_change(&id, path, old) {
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
                results.push(Part::ToolResult {
                    id,
                    name,
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
    fn mcp_instructions(&self, code: &[crate::codemode::Entry]) -> Option<String> {
        let direct = self.mcp_tools();
        let servers: Vec<(String, String, bool)> = self
            .mcp
            .instructions()
            .into_iter()
            .filter_map(|(server, text)| {
                let in_code = code.iter().any(|e| e.namespace == server);
                let in_direct = direct.iter().any(|t| t.server == server);
                (in_code || in_direct).then_some((server, text, in_code))
            })
            .collect();
        (!servers.is_empty()).then(|| crate::mcp::render_instructions(&servers))
    }

    /// The prompt a custom command sends: its template filled in, or an MCP
    /// server's prompt fetched with the arguments.
    pub async fn expand_command(&self, c: &crate::command::Command, args: &str) -> Result<String> {
        match &c.mcp {
            Some((server, name)) => self.mcp.prompt_text(server, name, args).await,
            None => Ok(crate::command::expand(&c.template, args)),
        }
    }

    fn codemode_on(&self) -> bool {
        self.loaded.config.codemode != Some(false)
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
            .collect()
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

    async fn run_code(&self, input: &serde_json::Value) -> (String, bool) {
        let input = tool::repair(input);
        let Some(code) = input["code"].as_str().filter(|c| !c.trim().is_empty()) else {
            return ("Missing required string parameter `code`".into(), true);
        };
        let entries = self.code_entries();
        let out = crate::codemode::execute(
            code,
            &entries,
            crate::codemode::MAX_TOOL_CALLS,
            crate::codemode::TIMEOUT,
            |tool, args| async move {
                // Code Mode hands results to code as JSON, so images and
                // PDFs are noted rather than passed on.
                let (text, failed, media) = self.run_mcp(&tool, &args).await;
                let text = crate::mcp::with_media_note(text, &media, "Code Mode returns text");
                if failed {
                    Err(text)
                } else {
                    Ok(text)
                }
            },
        )
        .await;
        let (text, is_error) = out.render();
        (
            tool::truncate(&text, self.limits, &paths::session_output_dir(&self.id)),
            is_error,
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
        let args = tool::repair(input);
        let title = format!("Run MCP tool {name}");
        if let Err(e) = self.allowed(name, "*", title) {
            return (e, true, vec![]);
        }
        match self.mcp.call_media(name, args).await {
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
    /// requests, then taken as media when it is an image or PDF (`Ok(None)`
    /// for anything else).
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
        let available = agent::subagents(&self.loaded.config);
        if !available.iter().any(|(id, _)| *id == agent_id) {
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
        let store = match self.store.as_ref().and_then(|st| st.path()) {
            Some(p) => Some(Store::open(&p).map_err(|e| format!("{e:#}"))?),
            None => None,
        };
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
        let opts = Options {
            questioner: None,
            mcp: Some(self.mcp.clone()),
            agent: Some(agent_id.clone()),
            model: s("model"),
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
        let outcome = match child.prompt(&task, &mut csink).await {
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

    /// Estimated size of the next request: system prompt, every offered
    /// tool definition (MCP, plugin and the rest, not only the built-in
    /// tools) and history (about four characters per token).
    pub fn estimate_prompt(&self) -> u64 {
        let plugins = crate::plugin::active();
        let code = self.code_entries();
        let tools: usize = self
            .tool_specs(&plugins, &code)
            .iter()
            .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
            .sum();
        (self.system_prompt(&code).len() + tools).div_ceil(4) as u64
            + compaction::estimate(&self.messages)
    }

    fn due_for_compaction(&self) -> bool {
        // Only a checkpoint (or nothing) left: compacting again cannot help.
        let settings = &self.loaded.config.compaction;
        if self.messages.len() < 2 || !settings.auto() {
            return false;
        }
        match self.model.context {
            Some(window) if window > 0 => {
                self.last_prompt_tokens.max(self.estimate_prompt()) > settings.ceiling(window)
            }
            _ => false,
        }
    }

    /// Whether summarizing again could shrink the prompt: not when the
    /// history after the checkpoint still fits in what compaction keeps.
    /// The prompt is then over the limit because of the system prompt and
    /// tool definitions (many MCP tools on a small model), and every step
    /// would summarize again for nothing. Without a checkpoint the history
    /// itself may be what fills the window (the estimate can be well off),
    /// so the first compaction always runs.
    fn worth_compacting(&self) -> bool {
        match self.messages.first() {
            Some(m) if is_checkpoint(m) => {
                compaction::estimate(&self.messages[1..]) > self.loaded.config.compaction.keep()
            }
            _ => true,
        }
    }

    /// Replace older history with a structured summary, keeping the newest turns.
    pub async fn compact(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let before = self.estimate_prompt();
        let k = compaction::split(&self.messages, self.loaded.config.compaction.keep());
        if k == 0 || self.messages.is_empty() {
            return Err(anyhow!("nothing to compact yet"));
        }
        sink.compaction_start();
        match self.summarize(sink, k).await {
            Ok(summary) => {
                sink.compaction_end(Ok(&summary));
                self.keep_summary(summary, k)?;
                sink.compacted(before, self.estimate_prompt());
                Ok(())
            }
            Err(e) => {
                sink.compaction_end(Err(&format!("{e:#}")));
                Err(e)
            }
        }
    }

    /// The summary of `self.messages[..k]`, streamed to `sink`.
    async fn summarize(&mut self, sink: &mut dyn Sink, k: usize) -> Result<String> {
        // Unstored sessions have no sequence numbers at all, so look at the text.
        let update =
            self.seqs.first() == Some(&None) && self.messages.first().is_some_and(is_checkpoint);
        let mut convo = compaction::for_summary(&self.messages[..k]);
        convo.push(Message::user(compaction::prompt(update)));
        let mut delta = |d: &str| sink.compaction_delta(d);
        let (mut summary, usage) = complete_streaming(
            &self.client,
            &self.model,
            SUMMARY_SYSTEM,
            &convo,
            None,
            retry::Policy::default(),
            &mut delta,
        )
        .await?;
        self.count_side_call(usage)?;
        if !compaction::is_summary(&summary) {
            convo.push(Message {
                role: Role::Assistant,
                parts: vec![Part::text(summary.clone())],
            });
            convo.push(Message::user(compaction::NUDGE));
            let usage;
            let mut delta = |d: &str| sink.compaction_delta(d);
            (summary, usage) = complete_streaming(
                &self.client,
                &self.model,
                SUMMARY_SYSTEM,
                &convo,
                None,
                retry::Policy::default(),
                &mut delta,
            )
            .await?;
            self.count_side_call(usage)?;
        }
        if summary.trim().is_empty() {
            return Err(anyhow!(
                "compaction failed: the model returned an empty summary"
            ));
        }
        Ok(summary)
    }

    /// Replace `self.messages[..k]` with the checkpoint of `summary`.
    fn keep_summary(&mut self, summary: String, k: usize) -> Result<()> {
        let kept_all_turn = k < self.messages.len();
        let keep_from = match self.seqs.get(k).copied().flatten() {
            Some(seq) => seq,
            None => self.seqs.iter().flatten().max().copied().unwrap_or(0) + 1,
        };
        if let (Some(s), true) = (&self.store, self.stored) {
            s.append(
                &self.id,
                &Record::Checkpoint {
                    summary: summary.clone(),
                    keep_from,
                },
            )?;
        }
        let mut messages = vec![Message::user(store::checkpoint_text(&summary))];
        let mut seqs = vec![None];
        messages.extend(self.messages.drain(k..));
        seqs.extend(self.seqs.drain(k..));
        self.messages = messages;
        self.seqs = seqs;
        if !kept_all_turn {
            // The current turn itself was summarized; tell the model to carry on.
            self.push(Message::system(compaction::CONTINUE))?;
        }
        self.last_prompt_tokens = 0;
        Ok(())
    }

    /// The file a built-in edit or write call is about to change, and its
    /// text (`None`: it does not exist yet), so the change can be recorded.
    fn file_before(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<(PathBuf, Option<String>)> {
        if !matches!(Tool::from_name(name), Some(Tool::Edit | Tool::Write)) || self.store.is_none()
        {
            return None;
        }
        let p = input["path"].as_str().or(input["filePath"].as_str())?;
        let path = tool::normalize(&self.cwd.join(p));
        let old = std::fs::read_to_string(&path).ok();
        if old.as_ref().is_some_and(|o| o.len() > store::CHANGE_MAX) {
            return None;
        }
        Some((path, old))
    }

    /// Record what call `id` changed in `path`, when the change is small
    /// enough to keep.
    fn record_change(&mut self, id: &str, path: PathBuf, old: Option<String>) -> Result<()> {
        let Ok(new) = std::fs::read_to_string(&path) else {
            return Ok(());
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
        if usage == Usage::default() {
            return Ok(());
        }
        self.count_usage(&usage);
        self.record(&Record::Step {
            model: self.model_spec.clone(),
            usage,
        })?;
        Ok(())
    }

    fn spawn_title(&self, first: &str) -> tokio::task::JoinHandle<(Option<String>, Usage)> {
        let client = self.client.clone();
        let model = self.model.clone();
        let msg = vec![Message::user(title::request(first))];
        let fallback = title::fallback(first);
        tokio::spawn(async move {
            let policy = retry::Policy::one_shot();
            let reply = complete_with(
                &client,
                &model,
                title::SYSTEM,
                &msg,
                Some(TITLE_MAX_TOKENS),
                policy,
            )
            .await
            .ok();
            let usage = reply.as_ref().map(|r| r.1).unwrap_or_default();
            let title = reply.and_then(|r| title::clean(&r.0)).unwrap_or(fallback);
            (Some(title), usage)
        })
    }

    async fn finish_title(&mut self) {
        let Some(task) = self.title_task.take() else {
            return;
        };
        if let Ok(Ok((title, usage))) = tokio::time::timeout(Duration::from_secs(20), task).await {
            let _ = self.count_side_call(usage);
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
        let mut req = self.request();
        self.turn.offered = req.tools.iter().map(|t| t.name.clone()).collect();
        if final_step {
            req.messages.push(Message::system(runner::MAX_STEPS_PROMPT));
            // The tool definitions stay, so the cached prefix does too.
            req.tool_choice = Some(codesmith_ai::ToolChoice::None);
        }
        let mut policy = retry::Policy::default();
        let mut attempt = 0;
        loop {
            match self.attempt(&req, sink).await {
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
        let stream = match self.model.websocket {
            Some(route) => {
                self.ws
                    .stream(&self.model.provider, &self.client, req, route)
                    .await
            }
            None => self.model.provider.stream(&self.client, req).await,
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
        let r = r?;
        Ok((r.parts, r.reason, r.usage))
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

/// A tool-free model call that returns the whole text reply.
/// The summary message a compaction leaves at the start of the history.
fn is_checkpoint(m: &Message) -> bool {
    m.role == Role::User
        && matches!(m.parts.first(), Some(Part::Text(t)) if t.starts_with("<conversation-checkpoint>"))
}

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
        system,
        messages,
        max_tokens,
        retry::Policy::default(),
    )
    .await
}

async fn complete_with(
    client: &reqwest::Client,
    model: &ModelRef,
    system: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
    policy: retry::Policy,
) -> Result<(String, Usage)> {
    complete_streaming(
        client,
        model,
        system,
        messages,
        max_tokens,
        policy,
        &mut |_| {},
    )
    .await
}

/// [`complete_with`], handing each text delta to `delta` as it arrives (a
/// retried attempt streams again from the start).
async fn complete_streaming(
    client: &reqwest::Client,
    model: &ModelRef,
    system: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
    mut policy: retry::Policy,
    delta: &mut (dyn FnMut(&str) + Send),
) -> Result<(String, Usage)> {
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
        ..Default::default()
    };
    loop {
        let mut out = String::new();
        let mut usage = Usage::default();
        let res: Result<(), codesmith_ai::Error> = async {
            let mut s = model.provider.stream(client, &req).await?;
            while let Some(ev) = s.next().await {
                match ev? {
                    Event::TextDelta(d) => {
                        delta(&d);
                        out.push_str(&d)
                    }
                    Event::Finish { usage: u, .. } => usage = u,
                    _ => {}
                }
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => return Ok((out, usage)),
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

fn add(total: &mut Usage, u: &Usage) {
    crate::stats::add(total, u);
}

fn new_id() -> String {
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
