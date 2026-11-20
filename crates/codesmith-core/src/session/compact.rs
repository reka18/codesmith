//! The session's side of compaction: when to compact, the summary request
//! and what replaces the history (the rules and prompt are in
//! [`crate::compaction`]).
//!
//! The summary request is the session's own request (the same system
//! prompt, tool definitions and messages, after plugin hooks) with the
//! summary prompt appended, so it reads the provider's prompt cache. It is
//! sized with the provider's count from the latest response while the
//! model and agent that were measured still match. When the whole
//! conversation does not fit, the history before the kept tail is sent
//! instead, and as text (see [`compaction::flatten`]) when that does not
//! fit either; a "too long" rejection shrinks it further, as in OpenCode's
//! `SessionCompaction.deliver`.
//!
//! Background summaries: once the prompt is within
//! [`compaction::PRECOMPUTE_LEAD`] of the ceiling, everything before the
//! latest response is summarized in the background (the prefix the provider
//! just cached), and held. Compaction swaps it in, keeping what came after
//! it verbatim, instead of waiting for a summary, as long as that tail is
//! short. OpenCode's `SessionCompaction.precompute`.

use super::{Session, Sink};
use crate::compaction::{self, Estimate};
use crate::config::ModelRef;
use crate::retry;
use crate::store::{self, Record};
use anyhow::{anyhow, Result};
use codesmith_ai::{Event, FinishReason, Message, Part, Request, Role, Usage};
use futures_util::StreamExt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What started a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `/compact`.
    Manual,
    /// The prompt passed the ceiling.
    Auto,
    /// The provider just rejected the prompt as too long.
    Overflow,
}

/// Compaction state of a session.
#[derive(Default)]
pub(super) struct State {
    /// The latest response the provider measured.
    anchor: Option<Anchor>,
    /// The first prompt the provider measured since the last compaction:
    /// where the cycle started.
    cycle_start: Option<u64>,
    /// The background summary held for the next compaction.
    held: Option<Held>,
}

/// What a summary request came to: the summary, and the usage of each call.
type Outcome = (Result<String>, Vec<Usage>);

/// A background summary of `messages[..keep]`.
struct Held {
    keep: usize,
    /// A hash of the messages it summarizes, to tell that it still
    /// describes this conversation (no compaction, undo or edit since).
    fingerprint: u64,
    /// The longest tail since `keep` compaction accepts beside it.
    tail_limit: u64,
    /// Background summaries that failed since the last compaction, before
    /// this one.
    failures: u32,
    task: tokio::task::JoinHandle<()>,
    outcome: Arc<Mutex<Option<Outcome>>>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Held {
    /// Whether it finished and failed.
    fn failed(&self) -> bool {
        matches!(&*self.outcome.lock().unwrap(), Some((Err(_), _)))
    }
}

fn fingerprint(messages: &[Message]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(messages)
        .unwrap_or_default()
        .hash(&mut h);
    h.finish()
}

/// A response whose prompt the provider counted.
#[derive(Debug, Clone)]
struct Anchor {
    /// Where the response is in the history.
    index: usize,
    /// Its prompt and output tokens.
    tokens: u64,
    /// Its prompt alone.
    prompt: u64,
    /// When it arrived, for the prompt cache's lifetime.
    at: Instant,
    /// The model spec and agent that made it; they shape the request.
    model: String,
    /// The provider that counted it.
    provider: String,
    agent: String,
}

/// A summary request ready to send.
pub(super) struct Plan {
    /// The conversation as the session sends it, after hooks, without the
    /// summary prompt.
    request: Request,
    /// The same without the tail kept beside the summary.
    history: Request,
    prompt: String,
    /// The size the request is shrunk to (before the prompt's overhead).
    target: u64,
    /// The size up to which the whole conversation goes out unchanged.
    fit: u64,
    /// The provider's count for the unchanged request, if it still applies.
    measured: Option<Estimate>,
    /// Send the history only as text (after an overflow).
    as_text: bool,
    window: Option<u64>,
    retry_scale: f64,
    /// A summary a compaction hook supplied.
    supplied: Option<crate::plugin::Summary>,
}

/// A summary request that came back unusable, or a provider error.
enum Failure {
    Provider(codesmith_ai::Error),
    Unusable(anyhow::Error),
}

impl Session {
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

    /// Remember the provider's count for the response about to be added
    /// to the history.
    pub(super) fn measured(&mut self, usage: &Usage) {
        let prompt = usage.input + usage.cache_read + usage.cache_write;
        let tokens = prompt + usage.output;
        self.last_prompt_tokens = tokens;
        self.compact.anchor = (tokens > 0).then(|| Anchor {
            index: self.messages.len(),
            tokens,
            prompt,
            at: Instant::now(),
            model: self.model_spec.clone(),
            provider: self.model.provider_id.clone(),
            agent: self.agent.name.clone(),
        });
        if tokens > 0 && self.compact.cycle_start.is_none() {
            self.compact.cycle_start = Some(prompt);
        }
    }

    /// The provider's count for the latest response plus an estimate of
    /// what came after it, while the model and agent that were measured
    /// still match.
    fn measured_prompt(&self) -> Option<Estimate> {
        let a = self.compact.anchor.as_ref()?;
        if a.index >= self.messages.len()
            || a.model != self.model_spec
            || a.agent != self.agent.name
        {
            return None;
        }
        Some(Estimate {
            measured: a.tokens,
            estimated: compaction::estimate(&self.messages[a.index + 1..]),
        })
    }

    /// The size of the next prompt: the provider's count for the latest
    /// response plus an estimate of what came after it, or else an estimate
    /// of everything. Only the current provider's counts are trusted.
    pub(super) fn prompt_size(&self) -> u64 {
        match &self.compact.anchor {
            Some(a) if a.index < self.messages.len() && a.provider == self.model.provider_id => {
                a.tokens + compaction::estimate(&self.messages[a.index + 1..])
            }
            _ => self.estimate_prompt(),
        }
    }

    pub(super) fn due_for_compaction(&self) -> bool {
        // Only a checkpoint (or nothing) left: compacting again cannot help.
        let settings = &self.loaded.config.compaction;
        if self.messages.len() < 2 || !settings.auto() {
            return false;
        }
        match self.model.context {
            Some(window) if window > 0 => self.prompt_size() > settings.ceiling(window),
            _ => false,
        }
    }

    /// Start a background summary when the prompt nears the ceiling (see the
    /// module notes). Never waits for it. One is held at a time; it is
    /// replaced from a newer response when it failed or fell too far behind.
    pub(super) fn precompute(&mut self) {
        let settings = self.loaded.config.compaction.clone();
        if !settings.background() || !settings.auto() {
            return;
        }
        let Some(window) = self.model.context.filter(|w| *w > 0) else {
            return;
        };
        let ceiling = settings.ceiling(window);
        let start = self.compact.cycle_start.unwrap_or(0);
        let lead = compaction::PRECOMPUTE_LEAD.min(ceiling.saturating_sub(start) / 2);
        if self.prompt_size() < ceiling.saturating_sub(lead) {
            return;
        }
        let Some(anchor) = self.compact.anchor.clone() else {
            return;
        };
        if anchor.index == 0 || anchor.index >= self.messages.len() {
            return;
        }
        let mut failures = 0;
        if let Some(held) = &self.compact.held {
            let failed = held.failed();
            failures = held.failures + u32::from(failed);
            let valid = self.held_valid(held);
            let tail = compaction::estimate(&self.messages[held.keep.min(self.messages.len())..]);
            if valid && !failed && tail <= held.tail_limit {
                return;
            }
            // Summarizing up to the same response again would repeat the
            // same request.
            if valid
                && (held.keep == anchor.index || failures >= compaction::PRECOMPUTE_MAX_FAILURES)
            {
                return;
            }
            self.discard_held();
        }
        // After a model switch nothing is cached for the new model yet.
        if anchor.model != self.model_spec {
            return;
        }
        // Nor once the latest response is older than the cache lifetime.
        let lifetime = compaction::cache_lifetime(self.model.cache.ttl.as_deref());
        if anchor.at.elapsed() + compaction::CACHE_MARGIN > lifetime {
            return;
        }
        let cap = settings.ceiling(window);
        let measured = Estimate {
            measured: anchor.prompt,
            estimated: 0,
        };
        let plan = self.plan_on(anchor.index, Reason::Auto, anchor.index, Some(measured));
        let (client, model) = (self.client.clone(), self.model.clone());
        let outcome: Arc<Mutex<Option<Outcome>>> = Arc::default();
        let slot = outcome.clone();
        let task = tokio::spawn(async move {
            let r = deliver(&client, &model, plan, &mut |_: &str| {}).await;
            *slot.lock().unwrap() = Some(r);
        });
        self.compact.held = Some(Held {
            keep: anchor.index,
            fingerprint: fingerprint(&self.messages[..anchor.index]),
            tail_limit: (cap as f64 * compaction::PRECOMPUTED_TAIL_SHARE) as u64,
            failures,
            task,
            outcome,
        });
    }

    /// Whether a held summary still describes the start of this conversation.
    fn held_valid(&self, held: &Held) -> bool {
        held.keep < self.messages.len()
            && fingerprint(&self.messages[..held.keep]) == held.fingerprint
    }

    /// Stop the held summary, counting what its finished calls cost.
    fn discard_held(&mut self) {
        let Some(held) = self.compact.held.take() else {
            return;
        };
        let done = held.outcome.lock().unwrap().take();
        if let Some((_, usages)) = done {
            for u in usages {
                let _ = self.count_side_call(u);
            }
        }
    }

    /// The held summary and where the messages kept beside it start, when
    /// it still describes this conversation and that tail is short. Waits
    /// for one still being written.
    async fn use_held(&mut self, budget: u64) -> Option<(String, usize)> {
        let held = self.compact.held.take()?;
        let tail = compaction::estimate(&self.messages[held.keep.min(self.messages.len())..]);
        let limit = (budget as f64 * compaction::PRECOMPUTED_TAIL_SHARE) as u64;
        if !self.held_valid(&held) || tail > limit {
            self.compact.held = Some(held);
            self.discard_held();
            return None;
        }
        let mut held = held;
        let _ = (&mut held.task).await;
        let (result, usages) = held.outcome.lock().unwrap().take()?;
        for u in usages {
            let _ = self.count_side_call(u);
        }
        result.ok().map(|s| (s, held.keep))
    }

    /// Whether summarizing again could shrink the prompt: not when the
    /// history after the checkpoint still fits in what compaction keeps.
    /// The prompt is then over the limit because of the system prompt and
    /// tool definitions (many MCP tools on a small model), and every step
    /// would summarize again for nothing. Without a checkpoint the history
    /// itself may be what fills the window (the estimate can be well off),
    /// so the first compaction always runs.
    pub(super) fn worth_compacting(&self) -> bool {
        match self.messages.first() {
            Some(m) if compaction::is_checkpoint(m) => {
                compaction::estimate(&self.messages[1..]) > self.loaded.config.compaction.keep()
            }
            _ => true,
        }
    }

    /// Replace older history with a structured summary, keeping the newest turns.
    pub async fn compact(&mut self, sink: &mut dyn Sink) -> Result<()> {
        self.compact_for(sink, Reason::Manual).await
    }

    pub(super) async fn compact_for(&mut self, sink: &mut dyn Sink, reason: Reason) -> Result<()> {
        let before = self.estimate_prompt();
        let k = compaction::split(&self.messages, self.loaded.config.compaction.keep());
        if k == 0 || self.messages.is_empty() {
            return Err(anyhow!("nothing to compact yet"));
        }
        sink.compaction_start();
        // A background summary still being written shows as a running
        // compaction while it is awaited.
        let budget = self.budget(reason);
        let (result, k) = match self.use_held(budget).await {
            Some((summary, keep)) => (Ok(summary), keep),
            None => (self.summarize(sink, reason, k).await, k),
        };
        match result {
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

    /// The summary of the conversation, streamed to `sink`; `k` is where
    /// the tail kept beside it starts.
    async fn summarize(&mut self, sink: &mut dyn Sink, reason: Reason, k: usize) -> Result<String> {
        let end = self.messages.len();
        let plan = self.plan_on(end, reason, k, self.measured_prompt());
        let mut delta = |d: &str| sink.compaction_delta(d);
        let (result, usages) = deliver(&self.client, &self.model, plan, &mut delta).await;
        for u in usages {
            self.count_side_call(u)?;
        }
        result
    }

    /// The size a summary request aims for.
    fn budget(&self, reason: Reason) -> u64 {
        let settings = &self.loaded.config.compaction;
        let window = self.model.context.filter(|w| *w > 0);
        let cap = settings.ceiling(window.unwrap_or(compaction::UNKNOWN_WINDOW));
        // The provider just rejected this context, so the estimate ran
        // low; the first attempt already aims below it.
        match reason {
            Reason::Overflow => {
                let size = self.prompt_size().max(self.estimate_prompt());
                cap.min((size as f64 * compaction::SHRINK_STEPS[0]) as u64)
            }
            _ => cap,
        }
    }

    /// The summary request for `self.messages[..end]`, with `k` where the
    /// kept tail starts and `measured` the provider's count for it.
    fn plan_on(&self, end: usize, reason: Reason, k: usize, measured: Option<Estimate>) -> Plan {
        // Unstored sessions have no sequence numbers at all, so look at the text.
        let update = self.seqs.first() == Some(&None)
            && self.messages.first().is_some_and(compaction::is_checkpoint);
        let prompt = compaction::prompt(update);
        let plugins = crate::plugin::active();
        let mut request = self.base_request(&plugins);
        request.interaction = codesmith_ai::Interaction::Compaction;
        if end < self.messages.len() {
            request.messages = crate::media::sendable(
                &crate::runner::model_messages(&self.messages[..end]),
                self.model.input.as_deref(),
            );
        }
        // Hooks see the request without the summary prompt, as they see the
        // session's own requests, so a hook that shapes every request alike
        // keeps the cached prefix. A compaction hook may supply the summary
        // itself, and then no request is made.
        let sent = request.messages.len();
        let mut hooked = crate::plugin::Compaction {
            session_id: self.id.clone(),
            agent: self.agent.name.clone(),
            model: self.model_spec.clone(),
            request,
            result: None,
        };
        plugins.compaction(&self.loaded.config, &mut hooked);
        let supplied = hooked.result;
        let mut request = hooked.request;
        if supplied.is_none() {
            plugins.request(&self.loaded.config, &self.id, &mut request);
        }
        let head = crate::media::sendable(
            &crate::runner::model_messages(&self.messages[..k]),
            self.model.input.as_deref(),
        )
        .len();
        let mut history = request.clone();
        if request.messages.len() == sent && head < sent {
            history.messages.truncate(head);
        }
        let window = self.model.context.filter(|w| *w > 0);
        let budget = self.budget(reason);
        // The output limit leaves room for the most `deliver` sends.
        let hooked_output = request.max_tokens;
        if window.is_some() || hooked_output.is_some() {
            let limit = compaction::output_limit(
                window,
                hooked_output,
                Estimate {
                    measured: budget,
                    estimated: 0,
                },
            );
            let limit = Some(limit.min(u32::MAX as u64) as u32);
            request.max_tokens = limit;
            history.max_tokens = limit;
        }
        let overhead = tokens(&prompt) + tokens(compaction::NUDGE);
        // Past the ceiling the whole conversation still goes out unchanged
        // while the window can fit the summary.
        let fit = match reason {
            Reason::Overflow => budget,
            _ => budget.max(compaction::summary_room(window.unwrap_or(0), hooked_output)),
        };
        Plan {
            request,
            history,
            prompt,
            target: budget.saturating_sub(overhead),
            fit: fit.saturating_sub(overhead),
            measured: (reason != Reason::Overflow).then_some(measured).flatten(),
            as_text: reason == Reason::Overflow,
            window,
            retry_scale: self.turn.retry_scale,
            supplied,
        }
    }

    /// Replace `self.messages[..k]` with the checkpoint of `summary`.
    pub(super) fn keep_summary(&mut self, summary: String, k: usize) -> Result<()> {
        let kept_all_turn = k < self.messages.len();
        let keep_from = match self.seqs.get(k).copied().flatten() {
            Some(seq) => seq,
            None => self.seqs.iter().flatten().max().copied().unwrap_or(0) + 1,
        };
        let mut updates = Vec::new();
        let mut at = 0;
        if let (Some(s), true) = (&self.store, self.stored) {
            at = s.append(
                &self.id,
                &Record::Checkpoint {
                    summary: summary.clone(),
                    keep_from,
                },
            )?;
            updates = s
                .entries(&self.id)?
                .into_iter()
                .filter(|e| matches!(e.record, Record::Instructions { text: Some(_), .. }))
                .map(|e| Some(e.seq))
                .collect();
        }
        // The checkpoint starts a new instruction epoch: the baseline takes
        // the newest values, and the updates kept after it are part of it.
        if let Some(state) = &mut self.instruction {
            state.advance(at);
        }
        let mut messages = vec![Message::user(store::checkpoint_text(&summary))];
        let mut seqs = vec![None];
        for (m, seq) in self.messages.drain(k..).zip(self.seqs.drain(k..)) {
            if !updates.contains(&seq) {
                messages.push(m);
                seqs.push(seq);
            }
        }
        self.messages = messages;
        self.seqs = seqs;
        if !kept_all_turn {
            // The current turn itself was summarized; tell the model to carry on.
            self.push(Message::system(compaction::CONTINUE))?;
        }
        self.last_prompt_tokens = 0;
        self.compact.anchor = None;
        self.compact.cycle_start = None;
        self.discard_held();
        Ok(())
    }
}

/// Four characters per token.
fn tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Send the summary request of `plan`: the whole conversation unchanged if
/// it is estimated (or measured) to fit `plan.fit`, else the history before
/// the kept tail unchanged if it fits `plan.target`, and otherwise that
/// history as text aimed at `plan.target`. A request that cannot be made to
/// fit goes out whole until the provider rejects something as too long;
/// then each attempt aims at 85%, 70%, 50% and 35% of the first rejected
/// size, and a rejection after those gives up. A "payload too large" on an
/// unchanged request resends it as text. Other provider errors retry under
/// the session's policy. Returns the summary and the usage of every call.
async fn deliver(
    client: &reqwest::Client,
    model: &ModelRef,
    plan: Plan,
    delta: &mut (dyn FnMut(&str) + Send),
) -> (Result<String>, Vec<Usage>) {
    let mut usages = Vec::new();
    if let Some(s) = plan.supplied {
        usages.extend(s.usage);
        return (Ok(s.summary), usages);
    }
    let mut policy = retry::Policy::default();
    let mut rejections = 0usize;
    let mut rejected: Option<u64> = None;
    let mut as_text = plan.as_text;
    let mut target = plan.target;
    let mut fit = plan.fit;
    loop {
        let measured = if rejections == 0 { plan.measured } else { None };
        // Which request goes out, and whether it is one of the unchanged ones.
        let full_size = measured.map_or_else(
            || compaction::estimate_request(&plan.request),
            |m| m.total(),
        );
        let (req, unchanged): (Option<Request>, bool) = if !as_text && full_size <= fit {
            (Some(plan.request.clone()), true)
        } else if !as_text && compaction::estimate_request(&plan.history) <= target {
            (Some(plan.history.clone()), true)
        } else if let Some(r) = compaction::flatten(&plan.history, target) {
            (Some(r), false)
        } else if rejections > 0 {
            (None, false)
        } else if as_text {
            (compaction::flatten(&plan.history, u64::MAX / 4), false)
        } else {
            (Some(plan.history.clone()), true)
        };
        let Some(req) = req else {
            return (
                Err(anyhow!("compaction failed: the compaction request cannot be reduced further without losing the latest exchange or checkpoint")),
                usages,
            );
        };
        let size = (unchanged && req.messages.len() == plan.request.messages.len())
            .then_some(measured)
            .flatten();
        match send(client, model, &plan, req.clone(), size, delta, &mut usages).await {
            Ok(summary) => return (Ok(summary), usages),
            Err(Failure::Unusable(e)) => return (Err(e), usages),
            Err(Failure::Provider(e)) => {
                let class = retry::classify(&e);
                let too_large = class == retry::Class::PayloadTooLarge;
                if too_large && unchanged {
                    as_text = true;
                    continue;
                }
                if too_large || class == retry::Class::ContextOverflow {
                    let Some(step) = compaction::SHRINK_STEPS.get(rejections) else {
                        return (Err(anyhow!("{e}")), usages);
                    };
                    let r = *rejected.get_or_insert_with(|| {
                        size.map_or_else(|| compaction::estimate_request(&req), |s| s.total())
                    });
                    // The characters can still say the rejected request
                    // fits, so it only goes out again as text.
                    if size.is_some() {
                        as_text = true;
                    }
                    target = (r as f64 * step) as u64;
                    fit = target;
                    rejections += 1;
                    continue;
                }
                match policy.next(&e, std::time::Instant::now()) {
                    Some(wait) => tokio::time::sleep(wait.mul_f64(plan.retry_scale)).await,
                    None => return (Err(anyhow!("{e}")), usages),
                }
            }
        }
    }
}

/// One summary request with its prompt appended (and one reminder if the
/// reply skips the template).
async fn send(
    client: &reqwest::Client,
    model: &ModelRef,
    plan: &Plan,
    req: Request,
    size: Option<Estimate>,
    delta: &mut (dyn FnMut(&str) + Send),
    usages: &mut Vec<Usage>,
) -> Result<String, Failure> {
    let mut prompted = req;
    prompted.messages.push(Message::user(plan.prompt.clone()));
    let sized = |r: &Request, extra: &str| {
        size.map(|s| Estimate {
            measured: s.measured,
            estimated: s.estimated + tokens(&plan.prompt) + tokens(extra),
        })
        .unwrap_or(Estimate {
            measured: 0,
            estimated: compaction::estimate_request(r),
        })
    };
    if let Some(m) = prompted.max_tokens {
        let fitted = compaction::fit_output(m as u64, plan.window, sized(&prompted, ""));
        prompted.max_tokens = Some(fitted.min(u32::MAX as u64) as u32);
    }
    let mut summary = stream(client, model, &prompted, delta, usages).await?;
    if !compaction::is_summary(&summary) {
        prompted.messages.push(Message {
            role: Role::Assistant,
            parts: vec![Part::text(summary.clone())],
        });
        prompted.messages.push(Message::user(compaction::NUDGE));
        summary = stream(client, model, &prompted, delta, usages).await?;
    }
    if summary.trim().is_empty() {
        return Err(Failure::Unusable(anyhow!(
            "compaction failed: the model returned an empty summary"
        )));
    }
    Ok(summary)
}

/// Stream one summary reply; its usage goes into `usages`.
async fn stream(
    client: &reqwest::Client,
    model: &ModelRef,
    req: &Request,
    delta: &mut (dyn FnMut(&str) + Send),
    usages: &mut Vec<Usage>,
) -> Result<String, Failure> {
    let mut out = String::new();
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
                    usage, reason: r, ..
                } => {
                    usages.push(usage);
                    reason = Some(r);
                }
                _ => {}
            }
        }
        Ok(())
    }
    .await;
    res.map_err(Failure::Provider)?;
    cut_short(reason.as_ref()).map_err(Failure::Unusable)?;
    if reason
        .as_ref()
        .is_some_and(|r| r.normalized == codesmith_ai::Reason::Unknown)
    {
        return Err(Failure::Provider(codesmith_ai::Error::Provider {
            message: "the provider ended the summary with an unknown finish reason".into(),
            retryable: true,
        }));
    }
    Ok(out)
}

/// A summary the output limit or a content filter cut short is not a
/// summary: compacting with it would lose the history it left out. As in
/// OpenCode, it fails the compaction without a retry.
fn cut_short(reason: Option<&FinishReason>) -> Result<()> {
    match reason {
        Some(r) if r.is_length() => Err(anyhow!(
            "compaction failed: the summary was cut off at the output limit"
        )),
        Some(r) if r.is_content_filter() => Err(anyhow!(
            "Compaction summary was blocked by the provider{}",
            crate::runner::content_filtered(r)
                .strip_prefix(crate::runner::CONTENT_FILTERED)
                .unwrap_or_default()
        )),
        _ => Ok(()),
    }
}
