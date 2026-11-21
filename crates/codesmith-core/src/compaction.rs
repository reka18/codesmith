//! Context compaction: when a conversation nears the model's context window,
//! older history is replaced by a structured summary (a checkpoint) and only
//! the newest exchanges are kept verbatim. The prompt and budget rules are
//! ported from OpenCode's `SessionCompaction`, and pinned to it by the
//! `oc_compaction_*` tests (from `packages/core/test/session-compaction.test.ts`
//! and `config/compaction.test.ts`; the session-level ones are in
//! `tests/parity.rs`).
//!
//! Deliberate differences:
//! - The kept tail stays as real messages, not a text transcript, so it must
//!   fit `keep` tokens: the cut moves forward to the next turn start, where
//!   OpenCode moves it back to the start of the turn it falls in.
//! - There are no per-media estimates, input-only limits or a separate
//!   compaction model. The prompt is sized as in OpenCode: the provider's
//!   count for the latest response plus an estimate since (see
//!   `session/compact.rs`), and [`flatten`] is OpenCode's
//!   `flattenAndDropOldest`, without a previous summary's recent text
//!   (CodeSmith's kept tail is real messages).
//! - The update prompt has no legacy-checkpoint rewrite variant.

use codesmith_ai::{Message, Part, Role};
use serde::Deserialize;

/// The `compaction` config section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `false` leaves compaction to `/compact`, context overflows included
    /// (default true).
    pub auto: Option<bool>,
    /// Room kept free below the window before compacting.
    pub buffer: Option<Buffer>,
    pub keep: Keep,
    /// Write the summary in the background as the session nears automatic
    /// compaction, so compaction can swap it in instead of waiting for one
    /// (default true). A summary that is never used still costs tokens.
    pub background: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub struct Keep {
    /// Tokens of recent history kept verbatim (default [`KEEP_TOKENS`]).
    pub tokens: Option<u64>,
}

/// Tokens, or a share of the window such as `"20%"`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Buffer {
    Tokens(u64),
    Share(String),
}

impl Buffer {
    /// The buffer in tokens for `window`; `None` for a share that is not a
    /// percentage strictly between 0 and 100.
    pub fn tokens(&self, window: u64) -> Option<u64> {
        match self {
            Buffer::Tokens(t) => Some(*t),
            Buffer::Share(s) => {
                let p: f64 = s.trim().strip_suffix('%')?.trim().parse().ok()?;
                (p > 0.0 && p < 100.0).then(|| (window as f64 * p / 100.0).round() as u64)
            }
        }
    }
}

impl Settings {
    pub fn auto(&self) -> bool {
        self.auto.unwrap_or(true)
    }
    pub fn background(&self) -> bool {
        self.background.unwrap_or(true)
    }
    pub fn keep(&self) -> u64 {
        self.keep.tokens.unwrap_or(KEEP_TOKENS)
    }
    /// The prompt size at which automatic compaction starts.
    pub fn ceiling(&self, window: u64) -> u64 {
        ceiling_with(window, self.buffer.as_ref())
    }
}

/// The least of the window kept free for the last reply and the summary.
pub const RESERVE_MIN: u64 = 16_000;
/// Share of the window kept free before automatic compaction.
pub const RESERVE_SHARE: f64 = 0.15;
/// Tokens of the newest history kept verbatim beside a summary.
pub const KEEP_TOKENS: u64 = 15_000;
/// Characters kept from the start and the end of a long tool input or output.
pub const CLIP: usize = 2_000;

const SUMMARY_TEMPLATE: &str = r#"You MUST use this format for your response (you may omit sections that aren't applicable). Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Requirements
- [constraints, preferences, requirements, and scope boundaries stated by the user, or "(none)"]

## Decisions
- [decisions already made and why, or "(none)"]

## Work State
Break the objective into smaller goals and report which are completed, which are being worked on, and which are blocked.
### Completed
- [goals that have been completed; otherwise "(none)"]

### Active
- [goals currently being worked on; otherwise "(none)"]

### Blocked
- [anything blocking progress, and why; otherwise "(none)"]

## Next Move
1. [ordered list of next actions, or "(none)"]

## Relevant Files
List the files and directories, other than the current working directory, that another agent would need to open to continue this work. Include at most 15, most important first. Do not list every file that was read or changed. Include paths outside the current working directory when relevant. If none, write "(none)".
- `[file or directory path]`: [brief reason it matters]

## Important Context
- [facts the next agent cannot continue without and cannot easily find on its own; or "(none)"]
</template>"#;

const SUMMARY_RULES: &str = "Rules:
- Keep each section concise. Use terse, single-line bullets, not prose paragraphs or nested lists.
- Prefer short references over detailed restatement. It is fine to leave out information the next agent can recover from the code or the files listed above.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers.
- Carry forward only user questions or requests that remain unanswered or require further action. Do not repeat ones that newer history has answered or resolved. Preserve exact wording when carrying one forward.
- Preserve consequential workflow state, including whether changes are uncommitted, committed, pushed, under review, or merged.
- Do not mention the summary process or that context was compacted.";

/// The instruction appended after the history to summarize. `update` when
/// the history already opens with a checkpoint.
pub fn prompt(update: bool) -> String {
    let shared = [
        "Summarize only what the user and the assistant said and did. Leave out instructions and setup the assistant was given rather than told by the user: repository conventions, instruction files such as AGENTS.md, and environment details like the session ID. The next agent receives current versions of all of these separately.",
        SUMMARY_TEMPLATE,
        SUMMARY_RULES,
        "Do not continue the task or call tools.",
        "Return only the structured summary in the requested format. Do not include a preamble, explanation, or other commentary.",
    ];
    let mut parts: Vec<&str> = Vec::new();
    if update {
        parts.push("Update the existing checkpoint in the conversation above into one consolidated summary.");
        parts.push("Newer history always takes precedence over the existing checkpoint. Preserve previous information unless newer history clearly contradicts, supersedes, resolves, or makes it stale. If something is no longer relevant to continuing the work, you may remove it.");
        parts.push("Incorporate newer requirements, decisions, progress, and context. Reconcile Work State and Next Move: move completed work out of Active, remove resolved blockers and answered questions, and preserve unresolved or pending work.");
        parts.push("Return only the updated Markdown sections. Do not reproduce the `<conversation-checkpoint>`, `<summary>`, or `<recent-context>` wrapper tags from the previous checkpoint.");
    } else {
        parts.push("You MUST summarize the conversation above into a structured summary that will be given to another agent to resume the work.");
    }
    parts.extend(shared);
    parts.join("\n\n")
}

/// Sent when a summary came back without the template's headings.
pub const NUDGE: &str = "The previous response did not fill in the required summary template. Do not call tools. Return the summary as text using the exact section headings from the template.";

/// Whether text looks like a filled-in summary.
pub fn is_summary(text: &str) -> bool {
    text.contains("## Objective") || text.contains("## Work State") || text.contains("## Next Move")
}

/// The prompt size at which automatic compaction starts, for a context window.
pub fn ceiling(window: u64) -> u64 {
    ceiling_with(window, None)
}

/// [`ceiling`] with a configured buffer. A buffer that leaves no room (one
/// set for a larger model, say) is ignored in favour of the default reserve.
pub fn ceiling_with(window: u64, buffer: Option<&Buffer>) -> u64 {
    if let Some(t) = buffer.and_then(|b| b.tokens(window)) {
        if t < window {
            return window - t;
        }
    }
    let reserve = ((window as f64 * RESERVE_SHARE) as u64).max(if window >= 2 * RESERVE_MIN {
        RESERVE_MIN
    } else {
        0
    });
    window.saturating_sub(reserve)
}

/// Rough token estimate: four characters per token.
pub fn estimate(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_one).sum()
}

fn estimate_one(m: &Message) -> u64 {
    let chars: usize = m
        .parts
        .iter()
        .map(|p| match p {
            Part::Text(t) => t.len(),
            Part::Reasoning(r) => r.len(),
            Part::ToolCall { name, input, .. } => name.len() + input.to_string().len(),
            Part::ToolResult { content, media, .. } => {
                content.len() + media.iter().map(media_chars).sum::<usize>()
            }
            Part::Media(m) => media_chars(m),
            Part::Hosted(h) => {
                let result = h.result.iter();
                h.name.len()
                    + h.input.to_string().len()
                    + result
                        .map(|r| {
                            r.value.to_string().len()
                                + r.media.iter().map(media_chars).sum::<usize>()
                        })
                        .sum::<usize>()
            }
            // An encrypted provider checkpoint has no measurable size.
            Part::Effort { .. } | Part::Compaction(_) => 0,
        })
        .sum();
    (chars as u64).div_ceil(4)
}

/// Characters a media part is counted as: an image as about 1,600 tokens
/// (a full-size image on most providers), a PDF by its size.
fn media_chars(m: &codesmith_ai::Media) -> usize {
    match m.kind() {
        codesmith_ai::MediaKind::Image => 6_400,
        _ => (m.inline_len() / 10).max(6_400),
    }
}

/// A user message that starts a new exchange (not a batch of tool results).
pub fn starts_turn(m: &Message) -> bool {
    m.role == Role::User
        && m.parts
            .iter()
            .any(|p| matches!(p, Part::Text(_) | Part::Media(_)))
        && !m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }))
}

/// Index of the first message to keep verbatim: the earliest turn start whose
/// suffix fits in `keep` tokens, never index 0 (something must be summarized).
/// When everything after the first message fits, only the latest turn is kept,
/// rather than repeating the whole conversation beside its summary.
/// `messages.len()` means keep nothing.
pub fn split(messages: &[Message], keep: u64) -> usize {
    let mut best = messages.len();
    let mut tail = 0u64;
    let mut all_fit = true;
    for i in (1..messages.len()).rev() {
        tail += estimate_one(&messages[i]);
        if tail > keep {
            all_fit = false;
            break;
        }
        if starts_turn(&messages[i]) {
            best = i;
        }
    }
    if all_fit {
        if let Some(latest) = (1..messages.len())
            .rev()
            .find(|&i| starts_turn(&messages[i]))
        {
            return latest;
        }
    }
    best
}

/// Keep the start and end of long text.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= 2 * max + 50 {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(max)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let cut = text.chars().count() - 2 * max;
    format!("{head}\n[… {cut} characters omitted …]\n{tail}")
}

/// History prepared for the summary request: reasoning dropped, long tool
/// inputs and outputs clipped, media named instead of sent.
pub fn for_summary(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| Message {
            role: m.role.clone(),
            parts: m
                .parts
                .iter()
                .filter(|p| !matches!(p, Part::Reasoning(_)))
                .map(|p| match p {
                    Part::ToolResult {
                        id,
                        name,
                        content,
                        is_error,
                        media,
                    } => Part::ToolResult {
                        id: id.clone(),
                        name: name.clone(),
                        content: std::iter::once(clip(content, CLIP))
                            .chain(media.iter().map(crate::media::label))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        is_error: *is_error,
                        media: vec![],
                    },
                    Part::Media(m) => Part::text(crate::media::label(m)),
                    Part::Text(t) => Part::text(clip(t, CLIP * 4)),
                    other => other.clone(),
                })
                .collect(),
        })
        .filter(|m| !m.parts.is_empty())
        .collect()
}

/// Whether a provider error means the request was too long for the model.
pub fn is_overflow(error: &str) -> bool {
    let e = error.to_lowercase();
    (e.contains("context")
        && (e.contains("length")
            || e.contains("window")
            || e.contains("exceed")
            || e.contains("too long")
            || e.contains("maximum")))
        || e.contains("prompt is too long")
        || e.contains("too many tokens")
        || e.contains("http 413")
}

/// After compaction mid-turn, tells the model to carry on. Sent as a system
/// update; sessions from before the system role stored it as user text
/// wrapped in `<system-reminder>`.
pub const CONTINUE: &str = "The conversation above was compacted into a checkpoint. Continue the task from where it left off.";

/// After each "too long" rejection of a summary request, the next attempt
/// aims at this share of the first rejected request's size.
pub const SHRINK_STEPS: [f64; 4] = [0.85, 0.7, 0.5, 0.35];
/// A common window size, assumed for a summary request when the model's
/// window is unknown.
pub const UNKNOWN_WINDOW: u64 = 200_000;
/// A summary gets at least this much output (or the room left, if less),
/// even when the estimate margin leaves less.
pub const SUMMARY_OUTPUT_FLOOR: u64 = 32_000;
/// The least output a request asks for.
pub const OUTPUT_TOKEN_MIN: u64 = 1_024;
/// The output a request asks for when the model's limit is unknown.
pub const OUTPUT_TOKEN_FALLBACK: u64 = 32_000;
/// How far an estimate may run low, in the room left for the output.
pub const ESTIMATE_ERROR: f64 = 0.15;
/// Characters kept, half from the start and half from the end, of a cut
/// tool input, tool output or user message in a summary transcript.
pub const TRANSCRIPT_CLIP: usize = 2_000;
/// Tokens before the ceiling at which a background summary starts: writing
/// one takes 30–60 s, and how much the conversation grows in that time does
/// not depend on the window, so the lead is fixed.
pub const PRECOMPUTE_LEAD: u64 = 60_000;
/// Prompt caches expire this long after their last use unless the model
/// sets `cache.ttl`.
pub const CACHE_LIFETIME: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// A background summary starts only while this much of the latest
/// response's cache lifetime is left.
pub const CACHE_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);
/// The most a swapped-in background summary keeps verbatim beside it, and
/// the tail at which a held one is replaced, as a share of the budget.
pub const PRECOMPUTED_TAIL_SHARE: f64 = 0.4;
/// Failed background summaries after which no new one starts until the
/// next compaction.
pub const PRECOMPUTE_MAX_FAILURES: u32 = 2;

/// A prompt's size: `measured` is what the provider reported for the latest
/// response, `estimated` the text since (or everything, unmeasured).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Estimate {
    pub measured: u64,
    pub estimated: u64,
}

impl Estimate {
    pub fn total(&self) -> u64 {
        self.measured + self.estimated
    }
}

/// Estimated size of a request: its system prompt, tool definitions and
/// messages.
pub fn estimate_request(req: &codesmith_ai::Request) -> u64 {
    let tools: usize = req
        .tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
        .sum();
    (req.system.len() + tools).div_ceil(4) as u64 + estimate(&req.messages)
}

/// The largest summary request that leaves [`RESERVE_MIN`] of the window
/// for the summary (less when the request cannot generate that much).
/// Zero for an unknown window.
pub fn summary_room(window: u64, max_output: Option<u32>) -> u64 {
    let output = max_output.map_or(RESERVE_MIN, |o| o as u64);
    window.saturating_sub(RESERVE_MIN.min(output))
}

/// The output limit a request of `input` tokens asks for: the model's
/// limit, fitted to the room the prompt leaves in the window (with a margin
/// for a low estimate), and never under [`OUTPUT_TOKEN_MIN`].
pub fn output_limit(window: Option<u64>, max_output: Option<u32>, input: Estimate) -> u64 {
    let requested = max_output.map_or(OUTPUT_TOKEN_FALLBACK, |o| o as u64);
    let Some(window) = window.filter(|w| *w > 0) else {
        return requested;
    };
    let room = window as i64
        - input.measured as i64
        - (input.estimated as f64 * (1.0 + ESTIMATE_ERROR)).ceil() as i64;
    requested.min((OUTPUT_TOKEN_MIN as i64).max(room) as u64)
}

/// Share of the window a turn's request leaves unused when its output limit
/// is fitted to the room left: providers count a few tokens more than the
/// estimate (chat template, tool schemas), and strict ones reject input
/// plus output past the window (OpenCode's `WINDOW_HEADROOM`). Summaries
/// size against their own reserve instead.
pub const WINDOW_HEADROOM: f64 = 0.01;

/// The output limit of a turn's request of `input` tokens: the model's
/// limit fitted to the room left in the window less [`WINDOW_HEADROOM`].
pub fn turn_output_limit(window: u64, max_output: u32, input: Estimate) -> u64 {
    let usable = (window as f64 * (1.0 - WINDOW_HEADROOM)).floor() as u64;
    output_limit(Some(usable), Some(max_output), input)
}

/// The output limit for a summary request of `size` that was prepared for
/// `current`: lowered to the room the request leaves, but the summary still
/// gets up to [`SUMMARY_OUTPUT_FLOOR`] of the room left without the margin.
pub fn fit_output(current: u64, window: Option<u64>, size: Estimate) -> u64 {
    let margin = output_limit(window, Some(u32::MAX), size);
    let plain = output_limit(
        window,
        Some(u32::MAX),
        Estimate {
            measured: size.total(),
            estimated: 0,
        },
    );
    current.min(margin.max(SUMMARY_OUTPUT_FLOOR.min(plain)))
}

/// Milliseconds a prompt cache entry lives, from a `cache.ttl` such as
/// `"5m"` or `"1h"`; [`CACHE_LIFETIME`] when unset or unreadable.
pub fn cache_lifetime(ttl: Option<&str>) -> std::time::Duration {
    parse_duration(ttl.unwrap_or("")).unwrap_or(CACHE_LIFETIME)
}

/// A duration such as `"90s"`, `"5m"`, `"1.5h"` or `"250ms"`.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let ms = match unit.trim() {
        "ms" => 1.0,
        "s" => 1_000.0,
        "m" => 60_000.0,
        "h" => 3_600_000.0,
        "d" => 86_400_000.0,
        _ => return None,
    };
    (n >= 0.0).then(|| std::time::Duration::from_millis((n * ms) as u64))
}

/// How far one exchange of a summary transcript is shortened, in the
/// order the levels apply: whole; tool outputs cut; tool outputs left out
/// and tool inputs cut; only the user's text, cut; omitted.
const OUTPUTS_CLIPPED: usize = 1;
const OUTPUTS_REMOVED: usize = 2;
const USER_ONLY: usize = 3;
const OMITTED: usize = 4;
const LEVELS: usize = 5;

/// `text` cut to [`TRANSCRIPT_CLIP`] characters from its start and end.
fn clip_transcript(text: &str) -> String {
    let count = text.chars().count();
    if count <= TRANSCRIPT_CLIP {
        return text.to_string();
    }
    let half = TRANSCRIPT_CLIP / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(count - half).collect();
    format!(
        "{head}\n[… {} characters omitted …]\n{tail}",
        count - TRANSCRIPT_CLIP
    )
}

/// One message as transcript lines at a shortening level; empty for what
/// that level leaves out.
fn flatten_message(m: &Message, level: usize) -> String {
    if m.role == Role::System || level == OMITTED {
        return String::new();
    }
    if level >= USER_ONLY && m.role != Role::User {
        return String::new();
    }
    let speaker = if m.role == Role::User {
        "User"
    } else {
        "Assistant"
    };
    let mut lines = Vec::new();
    for p in &m.parts {
        match p {
            Part::Text(t) if !t.is_empty() => {
                let text = if level >= USER_ONLY {
                    clip_transcript(t)
                } else {
                    t.to_string()
                };
                lines.push(format!("[{speaker}]: {text}"));
            }
            _ if level >= USER_ONLY => {}
            Part::Media(media) => lines.push(format!("[{} omitted]", media.mime)),
            Part::ToolCall { name, input, .. } => {
                let input = input.to_string();
                let input = if level >= OUTPUTS_REMOVED {
                    clip_transcript(&input)
                } else {
                    input
                };
                lines.push(format!("[Assistant tool call]: {name}({input})"));
            }
            Part::Hosted(h) => {
                let input = h.input.to_string();
                lines.push(format!(
                    "[Assistant tool call]: {}({})",
                    h.name,
                    if level >= OUTPUTS_REMOVED {
                        clip_transcript(&input)
                    } else {
                        input
                    }
                ));
            }
            Part::ToolResult {
                content,
                is_error,
                media,
                ..
            } => {
                let label = if *is_error {
                    "Tool error"
                } else {
                    "Tool result"
                };
                if level >= OUTPUTS_REMOVED {
                    lines.push(format!("[{label} omitted]"));
                    continue;
                }
                let output = std::iter::once(content.clone())
                    .chain(media.iter().map(crate::media::label))
                    .collect::<Vec<_>>()
                    .join("\n");
                let output = if level >= OUTPUTS_CLIPPED {
                    clip_transcript(&output)
                } else {
                    output
                };
                lines.push(format!("[{label}]: {output}"));
            }
            _ => {}
        }
    }
    lines.join("\n")
}

/// Whether a message is the checkpoint a compaction leaves at the start
/// of the history.
pub fn is_checkpoint(m: &Message) -> bool {
    m.role == Role::User
        && matches!(m.parts.first(), Some(Part::Text(t)) if t.starts_with("<conversation-checkpoint>"))
}

/// The summary request's history as text: a previous checkpoint (or a
/// native window) stays in front as it is, and the conversation since is
/// one user message of `[User]: …` / `[Assistant]: …` lines, without tool
/// definitions, reasoning or media. When it does not fit `target` tokens
/// (with the system prompt), exchanges are shortened oldest first, one
/// level at a time (see the levels above), so whole exchanges are omitted
/// only as a last resort; the newest is never omitted. `None` when even so
/// it does not fit. Ported from OpenCode's `flattenAndDropOldest`.
pub fn flatten(req: &codesmith_ai::Request, target: u64) -> Option<codesmith_ai::Request> {
    let lead_len = req
        .messages
        .iter()
        .rposition(|m| is_checkpoint(m) || m.parts.iter().any(|p| matches!(p, Part::Compaction(_))))
        .map_or(0, |i| i + 1);
    let lead = &req.messages[..lead_len];
    // A real user message and everything after it, up to the next one, is
    // one exchange, rendered at every level once.
    let mut groups: Vec<Vec<&Message>> = Vec::new();
    for m in &req.messages[lead_len..] {
        if starts_turn(m) || groups.is_empty() {
            groups.push(vec![m]);
        } else {
            groups.last_mut().unwrap().push(m);
        }
    }
    let exchanges: Vec<Vec<String>> = groups
        .iter()
        .map(|g| {
            (0..LEVELS)
                .map(|level| {
                    g.iter()
                        .map(|m| flatten_message(m, level))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect::<Vec<String>>()
        })
        .filter(|r| !r[0].is_empty())
        .collect();
    if exchanges.is_empty() {
        return None;
    }
    let note = |omitted: usize| {
        if omitted == 0 {
            String::new()
        } else {
            format!(
                "[{omitted} older {} omitted]\n\n",
                if omitted == 1 {
                    "exchange"
                } else {
                    "exchanges"
                }
            )
        }
    };
    let size = |t: &str| {
        if t.is_empty() {
            0
        } else {
            (t.len() as u64).div_ceil(4) + 1
        }
    };
    let fixed = (req.system.len() as u64).div_ceil(4)
        + estimate(lead)
        + (note(exchanges.len()).len() as u64).div_ceil(4);
    let room = target as i64 - fixed as i64;
    let mut levels = vec![0usize; exchanges.len()];
    let mut total: i64 = exchanges.iter().map(|r| size(&r[0]) as i64).sum();
    let newest = exchanges.len() - 1;
    for level in 1..LEVELS {
        for i in 0..exchanges.len() {
            if total <= room {
                break;
            }
            // The newest exchange is shortened but never left out.
            if i == newest && exchanges[i][level].is_empty() {
                continue;
            }
            total += size(&exchanges[i][level]) as i64 - size(&exchanges[i][levels[i]]) as i64;
            levels[i] = level;
        }
    }
    if total > room {
        return None;
    }
    let omitted = levels
        .iter()
        .zip(&exchanges)
        .filter(|(l, r)| r[**l].is_empty())
        .count();
    let text = exchanges
        .iter()
        .zip(&levels)
        .map(|(r, l)| r[*l].as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut messages = lead.to_vec();
    messages.push(Message::user(format!("{}{text}", note(omitted))));
    Some(codesmith_ai::Request {
        tools: Vec::new(),
        namespaces: Vec::new(),
        tool_choice: None,
        messages,
        ..req.clone()
    })
}

/// The newest whole, real user messages (no tool results or checkpoints)
/// within `keep` tokens, attachments counted, oldest first: what a trigger
/// compaction keeps before the provider's checkpoint. OpenCode's
/// `recentUserMessages`.
pub fn recent_users(messages: &[Message], keep: u64) -> Vec<Message> {
    let users: Vec<&Message> = messages
        .iter()
        .filter(|m| starts_turn(m) && !is_checkpoint(m))
        .collect();
    let mut total = 0;
    let mut start = users.len();
    while start > 0 {
        let next = total + estimate_one(users[start - 1]);
        if next > keep {
            break;
        }
        total = next;
        start -= 1;
    }
    users[start..].iter().map(|m| (*m).clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(t: &str) -> Message {
        Message::user(t)
    }
    fn asst(t: &str) -> Message {
        Message {
            role: Role::Assistant,
            parts: vec![Part::Text(t.into())],
        }
    }
    fn call() -> Message {
        Message {
            role: Role::Assistant,
            parts: vec![Part::ToolCall {
                id: "c".into(),
                name: "read".into(),
                input: json!({"path": "a"}),
                meta: None,
            }],
        }
    }
    fn result(n: usize) -> Message {
        Message {
            role: Role::User,
            parts: vec![Part::ToolResult {
                id: "c".into(),
                name: "read".into(),
                content: "x".repeat(n),
                is_error: false,
                media: vec![],
            }],
        }
    }

    #[test]
    fn ceilings() {
        assert_eq!(ceiling(200_000), 170_000);
        assert_eq!(ceiling(32_768), 32_768 - 16_000);
        assert_eq!(ceiling(20_000), 17_000);
        assert_eq!(ceiling(0), 0);
    }

    #[test]
    fn prompts() {
        let fresh = prompt(false);
        assert!(fresh.starts_with("You MUST summarize the conversation above"));
        assert!(
            fresh.contains("## Objective")
                && fresh.contains("Do not continue the task or call tools.")
        );
        let update = prompt(true);
        assert!(update.starts_with("Update the existing checkpoint"));
        assert!(update.contains("<conversation-checkpoint>"));
        assert!(!fresh.to_lowercase().contains("opencode"));
    }

    #[test]
    fn summary_detection() {
        assert!(is_summary("## Objective\n- x"));
        assert!(!is_summary("Sure! Here's what we did."));
    }

    #[test]
    fn split_keeps_whole_recent_turns() {
        let m = vec![
            user("first"),
            call(),
            result(400_000),
            asst("done"),
            user("second"),
            call(),
            result(40),
            asst("ok"),
        ];
        assert_eq!(
            split(&m, KEEP_TOKENS),
            4,
            "keeps the second turn, summarizes the big first one"
        );
    }

    #[test]
    fn split_never_breaks_a_tool_exchange() {
        let m = vec![user("a"), call(), result(10), call(), result(10)];
        assert_eq!(
            split(&m, KEEP_TOKENS),
            m.len(),
            "no later turn start: keep nothing rather than orphan a result"
        );
        let m = vec![user("a"), asst("b")];
        assert_eq!(
            split(&m, KEEP_TOKENS),
            m.len(),
            "index 0 is never kept whole"
        );
    }

    #[test]
    fn split_stops_at_the_budget() {
        let m = vec![
            user("a"),
            asst("b"),
            user("c"),
            result(200_000),
            user("d"),
            asst("e"),
        ];
        assert_eq!(split(&m, 1_000), 4);
    }

    #[test]
    fn turn_starts() {
        assert!(starts_turn(&user("x")));
        assert!(!starts_turn(&result(1)));
        assert!(!starts_turn(&asst("x")));
    }

    #[test]
    fn clipping() {
        assert_eq!(clip("short", 10), "short");
        let long = format!("{}{}", "a".repeat(5000), "b".repeat(5000));
        let c = clip(&long, 100);
        assert!(c.starts_with(&"a".repeat(100)) && c.ends_with(&"b".repeat(100)));
        assert!(c.contains("[… 9800 characters omitted …]"));
    }

    #[test]
    fn summary_input_drops_reasoning_and_clips() {
        let m = vec![
            Message {
                role: Role::Assistant,
                parts: vec![Part::Reasoning("r".into())],
            },
            result(50_000),
        ];
        let s = for_summary(&m);
        assert_eq!(s.len(), 1, "reasoning-only messages vanish");
        assert!(
            matches!(&s[0].parts[0], Part::ToolResult { content, .. } if content.len() < 5_000)
        );
    }

    #[test]
    fn estimates() {
        assert_eq!(estimate(&[user("abcd"), user("abcde")]), 3);
    }

    #[test]
    fn overflow_errors() {
        assert!(is_overflow(
            "HTTP 400: This model's maximum context length is 32768 tokens"
        ));
        assert!(is_overflow(
            "HTTP 400: prompt is too long: 210000 tokens > 200000 maximum"
        ));
        assert!(is_overflow("HTTP 413: payload"));
        assert!(!is_overflow("HTTP 401: invalid api key"));
    }

    // --- Ported from OpenCode's session-compaction tests (`oc_compaction_*`). ---

    #[test]
    fn oc_compaction_prompt_preserves_detailed_work_state_and_relevant_files() {
        let p = prompt(false);
        for h in [
            "## Work State",
            "### Completed",
            "### Active",
            "### Blocked",
            "## Relevant Files",
        ] {
            assert!(p.contains(h), "{h}");
        }
    }

    #[test]
    fn oc_compaction_prompt_requires_the_checkpoint_headings_in_order() {
        let p = prompt(false);
        let headings: Vec<&str> = p
            .lines()
            .filter(|l| {
                (l.starts_with("## ") || l.starts_with("### "))
                    && l.trim_start_matches('#').len() > 1
            })
            .collect();
        assert_eq!(
            headings,
            [
                "## Objective",
                "## Requirements",
                "## Decisions",
                "## Work State",
                "### Completed",
                "### Active",
                "### Blocked",
                "## Next Move",
                "## Relevant Files",
                "## Important Context",
            ]
        );
    }

    #[test]
    fn oc_compaction_prompts_prohibit_task_execution() {
        for update in [false, true] {
            assert!(prompt(update).contains("Do not continue the task or call tools"));
        }
    }

    #[test]
    fn oc_compaction_update_prompt_has_no_legacy_rewrite() {
        // OpenCode adds this only for checkpoints in its earlier format.
        assert!(
            !prompt(true).contains("The existing checkpoint was written with an earlier format")
        );
    }

    /// The ceiling cases of "auto compaction estimates current content
    /// against the buffered prompt ceiling" that apply without input limits.
    #[test]
    fn oc_compaction_auto_ceiling_and_buffers() {
        // The summary's 16k reserve is more than 15% of a 100k window, so it sets the ceiling.
        assert_eq!(ceiling(100_000), 84_000);
        // 15% of a 128k window (OpenCode's overstated-input case lands here too).
        assert_eq!(ceiling(128_000), 108_800);
        // A buffer that leaves the model no room falls back to the default reserve.
        let huge = Buffer::Tokens(100_000);
        assert_eq!(ceiling_with(100_000, Some(&huge)), 84_000);
        // A percentage buffer is a share of the window.
        assert_eq!(
            ceiling_with(100_000, Some(&Buffer::Share("25%".into()))),
            75_000
        );
        assert_eq!(ceiling_with(100_000, Some(&Buffer::Tokens(10_000))), 90_000);
        // A window too small to give up the 16k minimum keeps 15%.
        assert_eq!(ceiling(20_000), 17_000);
    }

    #[test]
    fn oc_compaction_keeps_only_the_latest_exchange_when_everything_fits() {
        let m = vec![
            user("a"),
            asst("b"),
            user("c"),
            call(),
            result(10),
            asst("d"),
            user("e"),
            asst("f"),
        ];
        assert_eq!(split(&m, KEEP_TOKENS), 6);
    }

    #[test]
    fn oc_compaction_one_exchange_is_summarized_whole() {
        let m = vec![user("a"), call(), result(10), asst("b")];
        assert_eq!(split(&m, KEEP_TOKENS), m.len());
    }

    #[test]
    fn oc_compaction_truncation_does_not_split_characters() {
        let prefix = "a".repeat(1_249);
        let text = format!("{prefix}😀{}", "b".repeat(5_000));
        let c = clip(&text, 1_250);
        assert!(
            c.starts_with(&format!("{prefix}😀\n")),
            "cut after whole characters"
        );
        let whole = "😀".repeat(1_250);
        assert_eq!(
            clip(&whole, 1_250),
            whole,
            "short enough in characters, so kept"
        );
    }
}
