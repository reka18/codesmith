//! The agent loop: send the conversation, stream the reply, run any tool
//! calls, feed the results back, repeat until the model stops calling tools.
//! In-memory for now; durable event-sourced sessions arrive in phase 2.

use crate::agent::{self, Agent};
use crate::config::{Loaded, ModelRef};
use crate::permission::{Asker, Gate};
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
}

pub const MAX_RETRIES: u32 = 4;
pub const DEFAULT_MAX_STEPS: u32 = 200;

pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    pub root: PathBuf,
    pub agent: Agent,
    pub model: ModelRef,
    pub system: String,
    pub tools: Vec<Tool>,
    pub messages: Vec<Message>,
    pub usage: Usage,
    gate: Gate,
    shell: String,
    limits: crate::config::ToolOutputConfig,
    client: reqwest::Client,
}

pub struct Options {
    pub agent: Option<String>,
    pub model: Option<String>,
}

impl Session {
    pub fn new(
        loaded: &Loaded,
        cwd: PathBuf,
        opts: Options,
        asker: Box<dyn Asker>,
    ) -> Result<Session> {
        let config = &loaded.config;
        let agent_id = opts
            .agent
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
        Ok(Session {
            id: new_id(),
            cwd,
            root: loaded.root.clone(),
            agent,
            model,
            system,
            tools,
            messages: Vec::new(),
            usage: Usage::default(),
            gate,
            shell,
            limits: config.tool_output,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .build()?,
        })
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
        self.messages.push(Message::user(text));
        let max_steps = self.agent.steps.unwrap_or(DEFAULT_MAX_STEPS);
        for _ in 0..max_steps {
            let (parts, reason, usage) = self.step(sink).await?;
            add(&mut self.usage, &usage);
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
            self.messages.push(Message {
                role: Role::Assistant,
                parts,
            });
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
            self.messages.push(Message {
                role: Role::User,
                parts: results,
            });
        }
        Err(anyhow!(
            "stopped after {max_steps} steps without a final answer"
        ))
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
