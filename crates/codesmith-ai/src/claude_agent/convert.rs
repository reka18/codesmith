//! Claude Code's stream as CodeSmith events (OpenCode's
//! `convert-claude-stream`, `usage` and `claude-agent-error`).

use super::tools;
use crate::{Error, Event, FinishReason, Protocol, Reason, ReasoningMeta, ToolSpec, Usage};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    input: u64,
    read: u64,
    write: u64,
    output: u64,
    reasoning: u64,
}

/// Usage for one CodeSmith step. Claude Code reports one model response's
/// usage several times while it streams, and again summed for the turn:
/// each response keeps its highest counts, output adds up across the
/// step's responses, and the input (the context size) is the last one's.
#[derive(Debug, Default)]
pub struct Tally {
    messages: Vec<(String, Counts)>,
}

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

impl Tally {
    pub fn update(&mut self, id: &str, usage: &Value) {
        let i = match self.messages.iter().position(|(m, _)| m == id) {
            Some(i) => i,
            None => {
                self.messages.push((id.to_string(), Counts::default()));
                self.messages.len() - 1
            }
        };
        let c = &mut self.messages[i].1;
        c.input = c.input.max(n(&usage["input_tokens"]));
        c.read = c.read.max(n(&usage["cache_read_input_tokens"]));
        c.write = c.write.max(n(&usage["cache_creation_input_tokens"]));
        c.output = c.output.max(n(&usage["output_tokens"]));
        c.reasoning = c
            .reasoning
            .max(n(&usage["output_tokens_details"]["thinking_tokens"]));
    }

    /// The turn's total, used only when the turn made one model call.
    pub fn result(&mut self, usage: &Value) {
        if self.messages.len() <= 1 {
            let id = self
                .messages
                .last()
                .map_or("result".to_string(), |m| m.0.clone());
            self.update(&id, usage);
        }
    }

    pub fn snapshot(&self) -> Usage {
        let output: u64 = self.messages.iter().map(|m| m.1.output).sum();
        let reasoning: u64 = self.messages.iter().map(|m| m.1.reasoning).sum();
        let last = self.messages.last().map(|m| m.1).unwrap_or_default();
        Usage {
            input: last.input,
            output,
            cache_read: last.read,
            cache_write: last.write,
            cost: None,
            reasoning,
            total: None,
        }
    }
}

/// Turns Claude Code's terminal result into an error when the turn failed,
/// with what came before it (the assistant's error code, a rejected plan
/// window). A usage warning alone is no failure; Claude Code has already
/// retried.
#[derive(Debug, Default)]
pub struct Failures {
    code: Option<String>,
    rejected: Option<Value>,
}

impl Failures {
    pub fn check(&mut self, m: &Value) -> Option<String> {
        if m["type"] == "rate_limit_event" {
            let info = &m["rate_limit_info"];
            self.rejected = (info["status"] == "rejected").then(|| info.clone());
        }
        if m["type"] == "assistant" && m["parent_tool_use_id"].is_null() {
            if let Some(e) = m["error"].as_str() {
                self.code = Some(e.to_string());
            }
        }
        if m["type"] != "result" {
            return None;
        }
        let success = m["subtype"] == "success";
        let status = if success {
            m["api_error_status"].as_u64()
        } else {
            None
        };
        let http = status.is_some_and(|s| s >= 400);
        if success && m["is_error"] != true && !http {
            return None;
        }
        let result = if success {
            m["result"].as_str().unwrap_or_default().to_string()
        } else {
            m["errors"]
                .as_array()
                .map(|e| {
                    e.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        }
        .replace("Please run /login", "Run `claude auth login` in a terminal");
        let failure = match status {
            Some(s) if s >= 500 => format!(
                "{} Claude Code exhausted its retries. Try again shortly.",
                if s == 529 {
                    "Anthropic is overloaded (529).".to_string()
                } else {
                    format!("Anthropic returned HTTP {s}.")
                }
            ),
            _ => super::login_hint(&result, super::keychain_locked),
        };
        let limited = self.code.as_deref() == Some("rate_limit")
            || (self.rejected.is_some() && status == Some(429));
        let window = self
            .rejected
            .as_ref()
            .and_then(|r| r["rateLimitType"].as_str())
            .map(|w| w.replace('_', " "));
        let limit = if limited && failure.is_empty() {
            format!(
                "Claude Code subscription limit reached{}.",
                window.map(|w| format!(" ({w})")).unwrap_or_default()
            )
        } else {
            failure
        };
        let reset = match self.rejected.as_ref().and_then(|r| r["resetsAt"].as_i64()) {
            Some(at) if limited => chrono::DateTime::from_timestamp(at, 0)
                .map(|t| {
                    format!(
                        " Resets at {}.",
                        t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                    )
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        let billing = if self.code.as_deref() == Some("billing_error") {
            " Check your Claude plan and extra-usage settings."
        } else {
            ""
        };
        let text = format!("{limit}{reset}{billing}");
        Some(if !text.is_empty() {
            text
        } else if let Some(s) = status {
            format!("Claude Code could not finish its reply (HTTP {s})")
        } else {
            "Claude Code could not finish its reply".to_string()
        })
    }
}

/// The error a failed turn ends with. Claude Code already retried it, so
/// CodeSmith does not.
pub fn failure(message: impl Into<String>) -> Error {
    Error::Provider {
        message: message.into(),
        retryable: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Block {
    Text,
    Reasoning,
}

/// Claude Code's messages for one step as CodeSmith events.
pub struct Converter {
    tools: Vec<ToolSpec>,
    usage: Tally,
    failures: Failures,
    streamed: HashSet<String>,
    blocks: HashMap<u64, Block>,
    message_id: String,
    called: bool,
    rate_limit: Option<Value>,
}

impl Converter {
    /// For a step that offered `tools`.
    pub fn new(tools: &[ToolSpec]) -> Converter {
        Converter {
            tools: tools.to_vec(),
            usage: Tally::default(),
            failures: Failures::default(),
            streamed: HashSet::new(),
            blocks: HashMap::new(),
            message_id: String::new(),
            called: false,
            rate_limit: None,
        }
    }

    /// The plan usage Claude Code reported during the step, which a finish
    /// carries (plugins read it there instead of polling for it).
    fn raw(&self) -> Option<Value> {
        self.rate_limit
            .as_ref()
            .map(|r| json!({super::ID: {"rateLimit": r}}))
    }

    fn finish(&self, reason: FinishReason) -> Event {
        Event::Finish {
            reason,
            usage: self.usage.snapshot(),
            raw_usage: self.raw(),
        }
    }

    pub fn push(&mut self, m: &Value) -> Vec<Result<Event, Error>> {
        let mut out: Vec<Result<Event, Error>> = Vec::new();
        if let Some(e) = self.failures.check(m) {
            return vec![Err(failure(e))];
        }
        let top = m["parent_tool_use_id"].is_null();
        match m["type"].as_str() {
            Some("rate_limit_event") => self.rate_limit = Some(m["rate_limit_info"].clone()),
            Some("stream_event") if top => {
                let ev = &m["event"];
                let index = ev["index"].as_u64().unwrap_or(0);
                let id = format!("{}:{index}", self.message_id);
                match ev["type"].as_str() {
                    Some("message_start") => {
                        self.message_id = ev["message"]["id"].as_str().unwrap_or_default().into();
                        self.streamed.insert(self.message_id.clone());
                        let usage = ev["message"]["usage"].clone();
                        self.usage.update(&self.message_id.clone(), &usage);
                    }
                    Some("content_block_start") => {
                        let block = &ev["content_block"];
                        match block["type"].as_str() {
                            Some("text") => {
                                self.blocks.insert(index, Block::Text);
                                out.push(Ok(Event::TextStart(id)));
                                if let Some(t) = block["text"].as_str().filter(|t| !t.is_empty()) {
                                    out.push(Ok(Event::TextDelta(t.into())));
                                }
                            }
                            Some("thinking") => {
                                self.blocks.insert(index, Block::Reasoning);
                                out.push(Ok(Event::ReasoningStart(id)));
                                if let Some(t) =
                                    block["thinking"].as_str().filter(|t| !t.is_empty())
                                {
                                    out.push(Ok(Event::ReasoningDelta(t.into())));
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_delta") => {
                        let delta = &ev["delta"];
                        match delta["type"].as_str() {
                            Some("text_delta") => out.push(Ok(Event::TextDelta(
                                delta["text"].as_str().unwrap_or_default().into(),
                            ))),
                            Some("thinking_delta") => {
                                if let Some(t) =
                                    delta["thinking"].as_str().filter(|t| !t.is_empty())
                                {
                                    out.push(Ok(Event::ReasoningDelta(t.into())));
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("content_block_stop") => match self.blocks.remove(&index) {
                        Some(Block::Text) => out.push(Ok(Event::TextEnd(id))),
                        Some(Block::Reasoning) => out.push(Ok(Event::ReasoningEnd(
                            ReasoningMeta::of(Protocol::ClaudeAgent),
                        ))),
                        None => {}
                    },
                    Some("message_delta") => {
                        let usage = ev["usage"].clone();
                        self.usage.update(&self.message_id.clone(), &usage);
                    }
                    // The same boundary at which the turn parks, or the step
                    // would end without a finish.
                    Some("message_stop") if self.called => {
                        out.push(Ok(
                            self.finish(FinishReason::new(Reason::ToolCalls, "tool_use"))
                        ));
                    }
                    _ => {}
                }
            }
            Some("assistant") if top => {
                // Synthetic API errors are diagnostics, not answers.
                if !m["error"].is_null() {
                    return out;
                }
                let msg = &m["message"];
                let api_id = msg["id"].as_str().unwrap_or_default().to_string();
                self.usage.update(&api_id, &msg["usage"]);
                let content = msg["content"].as_array().cloned().unwrap_or_default();
                for block in &content {
                    if block["type"] != "tool_use" {
                        continue;
                    }
                    let Some(name) =
                        tools::original(block["name"].as_str().unwrap_or_default(), &self.tools)
                    else {
                        continue;
                    };
                    self.called = true;
                    out.push(Ok(Event::ToolCall {
                        id: block["id"].as_str().unwrap_or_default().into(),
                        name: name.into(),
                        input: block["input"].clone(),
                    }));
                }
                // A complete message repeats what streamed; use it only
                // when nothing did.
                if !self.streamed.contains(&api_id) {
                    let uuid = m["uuid"].as_str().unwrap_or_default();
                    for (i, block) in content.iter().enumerate() {
                        let id = format!("{uuid}:{i}");
                        match block["type"].as_str() {
                            Some("text") => {
                                out.push(Ok(Event::TextStart(id.clone())));
                                out.push(Ok(Event::TextDelta(
                                    block["text"].as_str().unwrap_or_default().into(),
                                )));
                                out.push(Ok(Event::TextEnd(id)));
                            }
                            Some("thinking") => {
                                out.push(Ok(Event::ReasoningStart(id)));
                                out.push(Ok(Event::ReasoningDelta(
                                    block["thinking"].as_str().unwrap_or_default().into(),
                                )));
                                out.push(Ok(Event::ReasoningEnd(ReasoningMeta::of(
                                    Protocol::ClaudeAgent,
                                ))));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("result") => {
                if m["num_turns"] == 1 {
                    self.usage.result(&m["usage"]);
                }
                let raw = m["stop_reason"].as_str();
                let reason = match raw {
                    Some("max_tokens") => FinishReason::new(Reason::Length, "max_tokens"),
                    Some(r) => FinishReason::new(Reason::Stop, r),
                    None => FinishReason::inferred(Reason::Stop),
                };
                out.push(Ok(self.finish(reason)));
            }
            _ => {}
        }
        out
    }
}
