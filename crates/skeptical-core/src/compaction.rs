//! Context compaction: when a conversation nears the model's context window,
//! older history is replaced by a structured summary (a checkpoint) and only
//! the newest exchanges are kept verbatim. The prompt and budget rules are
//! ported from OpenCode's `SessionCompaction`.

use skeptical_ai::{Message, Part, Role};

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
            Part::Text(t) | Part::Reasoning(t) => t.len(),
            Part::ToolCall { name, input, .. } => name.len() + input.to_string().len(),
            Part::ToolResult { content, .. } => content.len(),
        })
        .sum();
    (chars as u64).div_ceil(4)
}

/// A user message that starts a new exchange (not a batch of tool results).
pub fn starts_turn(m: &Message) -> bool {
    m.role == Role::User
        && m.parts.iter().any(|p| matches!(p, Part::Text(_)))
        && !m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }))
}

/// Index of the first message to keep verbatim: the earliest turn start whose
/// suffix fits in `keep` tokens, never index 0 (something must be summarized).
/// `messages.len()` means keep nothing.
pub fn split(messages: &[Message], keep: u64) -> usize {
    let mut best = messages.len();
    let mut tail = 0u64;
    for i in (1..messages.len()).rev() {
        tail += estimate_one(&messages[i]);
        if tail > keep {
            break;
        }
        if starts_turn(&messages[i]) {
            best = i;
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
/// inputs and outputs clipped.
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
                    } => Part::ToolResult {
                        id: id.clone(),
                        name: name.clone(),
                        content: clip(content, CLIP),
                        is_error: *is_error,
                    },
                    Part::Text(t) => Part::Text(clip(t, CLIP * 4)),
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

/// After compaction mid-turn, tells the model to carry on.
pub const CONTINUE: &str = "<system-reminder>\nThe conversation above was compacted into a checkpoint. Continue the task from where it left off.\n</system-reminder>";

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
}
