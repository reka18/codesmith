//! The agent loop: send the conversation, stream the reply, run any tool
//! calls, feed the results back, repeat until the model stops calling tools.
//! Every message is saved to the session store as it happens, history is
//! compacted near the context limit, and new sessions get a title.

use crate::agent::{self, Agent};
use crate::compaction;
use crate::config::{Loaded, ModelRef};
use crate::permission::{Asker, Gate};
use crate::store::{self, Info, Record, Store};
use crate::title;
use crate::tool::{self, Ctx, Tool};
use crate::{instructions, paths, prompt};
use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use skeptical_ai::{Event, Message, Part, Request, Role, Usage};
use std::path::PathBuf;
use std::time::Duration;

/// Where the loop reports progress (the CLI prints; tests record).
pub trait Sink: Send {
    fn text(&mut self, delta: &str);
    fn reasoning(&mut self, _delta: &str) {}
    fn tool_start(&mut self, _id: &str, _name: &str, _input: &serde_json::Value) {}
    fn tool_end(&mut self, _id: &str, _name: &str, _output: &str, _is_error: bool) {}
    fn step_end(&mut self, _reason: &str, _usage: &Usage) {}
    fn retry(&mut self, _attempt: u32, _error: &str, _wait: Duration) {}
    fn compacted(&mut self, _before: u64, _after: u64) {}
}

pub const MAX_RETRIES: u32 = 4;
pub const DEFAULT_MAX_STEPS: u32 = 200;
const TITLE_MAX_TOKENS: u32 = 256;
const SUMMARY_SYSTEM: &str = "You are a helpful assistant that writes concise, structured summaries of coding sessions so another agent can resume the work.";

/// A stored session being continued: its row and its history with sequence numbers.
type Resumed = (Info, Vec<(Option<i64>, Message)>);

pub struct Options {
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Continue this stored session instead of starting a new one.
    pub resume: Option<String>,
    /// Use this title instead of generating one.
    pub title: Option<String>,
}

pub struct Session {
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
    pub usage: Usage,
    pub title: Option<String>,
    last_prompt_tokens: u64,
    gate: Gate,
    shell: String,
    limits: crate::config::ToolOutputConfig,
    client: reqwest::Client,
    store: Option<Store>,
    stored: bool,
    title_task: Option<tokio::task::JoinHandle<Option<String>>>,
}

impl Session {
    pub fn new(
        loaded: &Loaded,
        cwd: PathBuf,
        opts: Options,
        asker: Box<dyn Asker>,
        store: Option<Store>,
    ) -> Result<Session> {
        let config = &loaded.config;
        let resumed: Option<Resumed> = match (&opts.resume, &store) {
            (Some(id), Some(s)) => {
                let info = s.get(id)?.ok_or_else(|| anyhow!("no session {id}"))?;
                let entries = s.entries(id)?;
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
            .ok_or_else(|| anyhow!("no model configured: pass -m provider/model or set \"model\" in skeptical.json"))?;
        let model = config.resolve_model(&model_spec)?;
        let tools = agent::tools(&agent);
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
        if agent.id == "plan" {
            system.push_str("\n\n");
            system.push_str(agent::PLAN_REMINDER);
        }
        let shell = config
            .shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "/bin/sh".into());
        let gate = Gate::new(agent.permissions.clone(), asker);
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
            usage,
            title,
            last_prompt_tokens: 0,
            gate,
            shell,
            limits: config.tool_output,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .build()?,
            store,
            stored,
            title_task: None,
        })
    }

    /// Save a message (creating the session row on first use) and keep it in memory.
    fn push(&mut self, message: Message) -> Result<()> {
        let seq = match &self.store {
            Some(s) => {
                if !self.stored {
                    let t = store::now();
                    s.create(&Info {
                        id: self.id.clone(),
                        directory: self.cwd.display().to_string(),
                        title: self.title.clone(),
                        agent: self.agent.id.clone(),
                        model: self.model_spec.clone(),
                        parent_id: None,
                        created: t,
                        updated: t,
                        usage: Usage::default(),
                    })?;
                    self.stored = true;
                }
                Some(s.append(
                    &self.id,
                    &Record::Message {
                        message: message.clone(),
                    },
                )?)
            }
            None => None,
        };
        self.messages.push(message);
        self.seqs.push(seq);
        Ok(())
    }

    fn request(&self) -> Request {
        Request {
            model: self.model.model_id.clone(),
            system: self.system.clone(),
            messages: self.messages.clone(),
            tools: self.tools.iter().map(|t| t.spec(&self.shell)).collect(),
            max_tokens: self.model.max_output,
            temperature: self.model.temperature,
            body: self.model.body.clone(),
        }
    }

    /// One user turn: runs steps until the model answers without tool calls.
    pub async fn prompt(&mut self, text: &str, sink: &mut dyn Sink) -> Result<()> {
        if self.title.is_none() && self.title_task.is_none() {
            self.title_task = Some(self.spawn_title(text));
        }
        self.push(Message::user(text))?;
        let result = self.run_steps(sink).await;
        if let Some(s) = &self.store {
            if self.stored {
                let _ = s.set_usage(&self.id, &self.usage);
            }
        }
        self.finish_title().await;
        result
    }

    async fn run_steps(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let max_steps = self.agent.steps.unwrap_or(DEFAULT_MAX_STEPS);
        let mut overflow_compacted = false;
        let mut steps = 0;
        // At most one compaction between model steps, so a prompt that stays
        // over the limit after compacting cannot loop.
        let mut compacted_this_step = false;
        while steps < max_steps {
            if !compacted_this_step && self.due_for_compaction() {
                self.compact(sink).await?;
                compacted_this_step = true;
            }
            let (parts, reason, usage) = match self.step(sink).await {
                Ok(r) => r,
                Err(e) if !overflow_compacted && compaction::is_overflow(&e.to_string()) => {
                    overflow_compacted = true;
                    self.compact(sink).await?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            steps += 1;
            compacted_this_step = false;
            add(&mut self.usage, &usage);
            self.last_prompt_tokens =
                usage.input + usage.cache_read + usage.cache_write + usage.output;
            sink.step_end(&reason, &usage);
            let calls: Vec<(String, String, serde_json::Value)> = parts
                .iter()
                .filter_map(|p| match p {
                    Part::ToolCall { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();
            self.push(Message {
                role: Role::Assistant,
                parts,
            })?;
            if calls.is_empty() {
                return Ok(());
            }
            let mut results = Vec::new();
            for (id, name, input) in calls {
                sink.tool_start(&id, &name, &input);
                let ctx = Ctx {
                    cwd: self.cwd.clone(),
                    root: self.root.clone(),
                    gate: &self.gate,
                    shell: self.shell.clone(),
                    session_id: self.id.clone(),
                    limits: self.limits,
                };
                let (content, is_error) = tool::execute(&ctx, &name, &input).await;
                sink.tool_end(&id, &name, &content, is_error);
                results.push(Part::ToolResult {
                    id,
                    name,
                    content,
                    is_error,
                });
            }
            self.push(Message {
                role: Role::User,
                parts: results,
            })?;
        }
        Err(anyhow!(
            "stopped after {max_steps} steps without a final answer"
        ))
    }

    /// Estimated size of the next request: system prompt, tool definitions
    /// and history (about four characters per token).
    pub fn estimate_prompt(&self) -> u64 {
        let tools: usize = self
            .tools
            .iter()
            .map(|t| {
                serde_json::to_string(&t.spec(&self.shell))
                    .map(|s| s.len())
                    .unwrap_or(0)
            })
            .sum();
        (self.system.len() + tools).div_ceil(4) as u64 + compaction::estimate(&self.messages)
    }

    fn due_for_compaction(&self) -> bool {
        // Only a checkpoint (or nothing) left: compacting again cannot help.
        if self.messages.len() < 2 {
            return false;
        }
        match self.model.context {
            Some(window) if window > 0 => {
                self.last_prompt_tokens.max(self.estimate_prompt()) > compaction::ceiling(window)
            }
            _ => false,
        }
    }

    /// Replace older history with a structured summary, keeping the newest turns.
    pub async fn compact(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let before = self.estimate_prompt();
        let k = compaction::split(&self.messages, compaction::KEEP_TOKENS);
        if k == 0 || self.messages.is_empty() {
            return Err(anyhow!("nothing to compact yet"));
        }
        let update = self.seqs.first() == Some(&None);
        let mut convo = compaction::for_summary(&self.messages[..k]);
        convo.push(Message::user(compaction::prompt(update)));
        let (mut summary, _) =
            complete(&self.client, &self.model, SUMMARY_SYSTEM, &convo, None).await?;
        if !compaction::is_summary(&summary) {
            convo.push(Message {
                role: Role::Assistant,
                parts: vec![Part::Text(summary.clone())],
            });
            convo.push(Message::user(compaction::NUDGE));
            summary = complete(&self.client, &self.model, SUMMARY_SYSTEM, &convo, None)
                .await?
                .0;
        }
        if summary.trim().is_empty() {
            return Err(anyhow!(
                "compaction failed: the model returned an empty summary"
            ));
        }
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
            self.push(Message::user(compaction::CONTINUE))?;
        }
        self.last_prompt_tokens = 0;
        sink.compacted(before, self.estimate_prompt());
        Ok(())
    }

    fn spawn_title(&self, first: &str) -> tokio::task::JoinHandle<Option<String>> {
        let client = self.client.clone();
        let model = self.model.clone();
        let msg = vec![Message::user(title::request(first))];
        let fallback = title::fallback(first);
        tokio::spawn(async move {
            let reply = complete(&client, &model, title::SYSTEM, &msg, Some(TITLE_MAX_TOKENS))
                .await
                .ok()
                .map(|r| r.0);
            Some(reply.and_then(|r| title::clean(&r)).unwrap_or(fallback))
        })
    }

    async fn finish_title(&mut self) {
        let Some(task) = self.title_task.take() else {
            return;
        };
        if let Ok(Ok(Some(t))) = tokio::time::timeout(Duration::from_secs(20), task).await {
            if let (Some(s), true) = (&self.store, self.stored) {
                let _ = s.set_title(&self.id, &t);
            }
            self.title = Some(t);
        }
    }

    /// One model call with retries. A failure after output has streamed is not
    /// retried, since the user has already seen part of the answer.
    async fn step(&mut self, sink: &mut dyn Sink) -> Result<(Vec<Part>, String, Usage)> {
        let req = self.request();
        let mut attempt = 0;
        loop {
            match self.attempt(&req, sink).await {
                Ok(r) => return Ok(r),
                Err((e, streamed)) => {
                    if streamed || !e.retryable() || attempt >= MAX_RETRIES {
                        return Err(anyhow!("{e}"));
                    }
                    attempt += 1;
                    let wait = backoff(attempt);
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
    ) -> Result<(Vec<Part>, String, Usage), (skeptical_ai::Error, bool)> {
        let mut stream = self
            .model
            .provider
            .stream(&self.client, req)
            .await
            .map_err(|e| (e, false))?;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = Vec::new();
        let mut streamed = false;
        let mut finish = ("stop".to_string(), Usage::default());
        while let Some(ev) = stream.next().await {
            match ev.map_err(|e| (e, streamed))? {
                Event::TextDelta(d) => {
                    streamed = true;
                    text.push_str(&d);
                    sink.text(&d);
                }
                Event::ReasoningDelta(d) => {
                    streamed = true;
                    reasoning.push_str(&d);
                    sink.reasoning(&d);
                }
                Event::ToolCall { id, name, input } => {
                    calls.push(Part::ToolCall { id, name, input })
                }
                Event::Finish { reason, usage } => finish = (reason, usage),
            }
        }
        let mut parts = Vec::new();
        if !reasoning.is_empty() {
            parts.push(Part::Reasoning(reasoning));
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        parts.extend(calls);
        Ok((parts, finish.0, finish.1))
    }
}

/// A tool-free model call that returns the whole text reply.
pub async fn complete(
    client: &reqwest::Client,
    model: &ModelRef,
    system: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
) -> Result<(String, Usage)> {
    let req = Request {
        model: model.model_id.clone(),
        system: system.to_string(),
        messages: messages.to_vec(),
        tools: vec![],
        max_tokens: max_tokens.or(model.max_output),
        temperature: model.temperature,
        body: model.body.clone(),
    };
    let mut attempt = 0;
    loop {
        let mut out = String::new();
        let mut usage = Usage::default();
        let res: Result<(), skeptical_ai::Error> = async {
            let mut s = model.provider.stream(client, &req).await?;
            while let Some(ev) = s.next().await {
                match ev? {
                    Event::TextDelta(d) => out.push_str(&d),
                    Event::Finish { usage: u, .. } => usage = u,
                    _ => {}
                }
            }
            Ok(())
        }
        .await;
        match res {
            Ok(()) => return Ok((out, usage)),
            Err(e) if e.retryable() && attempt < MAX_RETRIES => {
                attempt += 1;
                tokio::time::sleep(backoff(attempt)).await;
            }
            Err(e) => return Err(anyhow!("{e}")),
        }
    }
}

fn add(total: &mut Usage, u: &Usage) {
    total.input += u.input;
    total.output += u.output;
    total.cache_read += u.cache_read;
    total.cache_write += u.cache_write;
}

/// Exponential backoff with jitter: about 1, 2, 4, 8 seconds.
pub fn backoff(attempt: u32) -> Duration {
    use rand::Rng;
    let base = 1000u64 << (attempt.saturating_sub(1)).min(5);
    let jitter = rand::thread_rng().gen_range(0..=base / 4);
    Duration::from_millis(base + jitter)
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
    fn backoff_grows_with_jitter() {
        for (a, lo) in [(1, 1000), (2, 2000), (3, 4000), (4, 8000)] {
            let d = backoff(a).as_millis() as u64;
            assert!(d >= lo && d <= lo + lo / 4, "attempt {a}: {d}");
        }
    }

    #[test]
    fn ids_look_right() {
        let a = new_id();
        assert!(a.starts_with("ses_") && a.len() > 10);
        assert_ne!(a, new_id());
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
            },
        );
        add(
            &mut t,
            &Usage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
            },
        );
        assert_eq!(
            t,
            Usage {
                input: 2,
                output: 4,
                cache_read: 6,
                cache_write: 8
            }
        );
    }
}
