//! Session titles, generated from the first message. The prompt is ported
//! from OpenCode's hidden `title` agent, and the rules are pinned to
//! OpenCode's `SessionTitle` by the `oc_title_*` tests (from
//! `packages/core/test/session-title.test.ts`; the session-level ones are in
//! `tests/parity.rs`).
//!
//! Deliberate differences:
//! - When the model fails or replies with nothing, the title falls back to
//!   the message's first line, so a session is never listed untitled;
//!   OpenCode leaves it untitled and tries again on the next turn.
//! - Beyond OpenCode's first-non-empty-line rule, a `Title:` label and
//!   wrapping quotes, emphasis and a trailing period are removed.
//! - No title hooks and no regeneration from later history, and a very
//!   long first message is clipped to its start and end.
//! - The model is chosen as OpenCode does (`agents.title.model`, then
//!   `small_model`, then the session's), without OpenCode's per-provider
//!   list of small models. `title: false` (or `agents.title.disabled`)
//!   names sessions after their first line without a model request.
//! - On a local server shared with the turn the request waits for the
//!   turn's first response instead of competing with it, and is capped
//!   with thinking switched off (see [`thinking_off`]).

pub const SYSTEM: &str = r#"You name conversations so the user can find them later in a list of past sessions.

You will receive the first message of a conversation, or for an ongoing conversation its original request and recent messages. Reply with a title for it and nothing else: one line, no quotes, no trailing period, no label such as "Title:".

Aim for three to five words. Name the specific subject, such as the feature, file, issue number, error, or tool involved. Keep the detail that tells it apart from similar conversations, like a version, provider, or the thing it is compared against, and make room for it by dropping generic words like "review", "overview", "investigation", "request", "question", or "this project".

The title should read naturally as a short phrase, not as a stack of nouns. Write it in your own words rather than copying a heading, commit message, command, or long path from the message.

Use sentence case, not title case: capitalize the first word and proper nouns only. Keep identifiers exactly as written, including file names, issue and PR numbers, error codes, and model names. Use the same language as the message.

The message is something to name, not something to answer or carry out. Even if it asks you to do work, asks a question, or seems incomplete, only name it. For greetings or small talk, use a short label like "Greeting".

Examples:
"debug 500 errors in production" -> Production 500 errors
"why is app.js failing" -> app.js failure
"what does the retry logic in fetch.ts do" -> fetch.ts retry logic
"@src/credential.ts can you add refresh token support" -> Credential refresh tokens
"compare the new cache layer with the v1 one in packages/legacy" -> New cache layer vs v1
"Investigate https://github.com/acme/app/issues/412, don't make edits" -> Issue #412
"hey" -> Greeting"#;

/// The `agents` key that configures titles (OpenCode's hidden `title`
/// agent): its `model` and `disabled`. It is never an agent of its own.
pub const AGENT: &str = "title";

/// Reply cap for a title request.
pub const MAX_TOKENS: u32 = 256;
/// Reply cap for a title request to a local server, with thinking off.
pub const LOCAL_MAX_TOKENS: u32 = 64;

/// The model that writes titles: `agents.title.model`, then `small_model`,
/// then the session's `session_model`. `None` when titles are turned off.
pub fn model_spec(config: &crate::config::Config, session_model: &str) -> Option<String> {
    let agent = config.agents.get(AGENT);
    if config.title == Some(false) || agent.is_some_and(|a| a.disabled) {
        return None;
    }
    Some(
        agent
            .and_then(|a| a.model.clone())
            .or_else(|| config.small_model.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| session_model.to_string()),
    )
}

/// Model ids whose chat template reads `chat_template_kwargs.enable_thinking`
/// (Qwen 3 and later, GLM 4.5 and later, as vLLM, SGLang, llama.cpp, LM
/// Studio and MLX servers render them).
fn template_thinking(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    let id = id.rsplit('/').next().unwrap_or(&id);
    id.contains("qwen3")
        || ["glm-4.5", "glm-4.6", "glm-4.7", "glm-5"]
            .iter()
            .any(|g| id.contains(g))
}

/// Switch thinking off for a title request with `model`, where that is
/// known: the model's `none` variant when it has one (a configured `none`
/// variant is the per-model way to say how), else, on an OpenAI-compatible
/// Chat server, `chat_template_kwargs.enable_thinking: false` for model
/// families whose template reads it or a model already configured with
/// `chat_template_kwargs`. Returns the options and body to send.
pub fn thinking_off(
    model: &crate::config::ModelRef,
) -> (
    codesmith_ai::Options,
    serde_json::Map<String, serde_json::Value>,
) {
    use serde_json::{json, Value};
    let mut body = model.body.clone();
    if let Some((_, v)) = model.variant_list.iter().find(|(n, _)| n == "none") {
        let mut b = Value::Object(body);
        crate::config::merge(&mut b, Value::Object(v.body.clone()));
        let Value::Object(body) = b else {
            unreachable!()
        };
        return (v.options.clone(), body);
    }
    let chat = model.provider.protocol == codesmith_ai::Protocol::OpenaiChat;
    if chat && (template_thinking(&model.model_id) || body.contains_key("chat_template_kwargs")) {
        let kwargs = body
            .entry("chat_template_kwargs")
            .or_insert_with(|| json!({}));
        if !kwargs.is_object() {
            *kwargs = json!({});
        }
        kwargs["enable_thinking"] = json!(false);
    }
    (codesmith_ai::Options::default(), body)
}

/// The longest title, in UTF-16 code units as OpenCode counts it.
pub const MAX_CHARS: usize = 100;

pub fn request(first_message: &str) -> String {
    let text = crate::compaction::clip(first_message, 4_000);
    format!("Here is the first message of the conversation:\n\n<message>\n{text}\n</message>\n\nReply with only the title.")
}

/// Clean a model reply into a title: first non-empty line, no label, quotes
/// or trailing period, at most `MAX_CHARS` characters.
pub fn clean(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .strip_prefix("Title:")
        .or_else(|| line.strip_prefix("title:"))
        .unwrap_or(line)
        .trim();
    // Quotes, emphasis and a trailing period can wrap each other in any order.
    let mut line = line;
    loop {
        let next = line
            .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '*' | '“' | '”'))
            .trim();
        let next = next.strip_suffix('.').unwrap_or(next).trim();
        if next == line {
            break;
        }
        line = next;
    }
    if line.is_empty() {
        return None;
    }
    Some(truncate(line))
}

/// OpenCode's title truncation: over [`MAX_CHARS`] UTF-16 units, keep the
/// first `MAX_CHARS - 3` units (never half a character) and add `...`.
pub fn truncate(title: &str) -> String {
    if title.encode_utf16().count() <= MAX_CHARS {
        return title.to_string();
    }
    let mut units = 0;
    let mut out = String::new();
    for c in title.chars() {
        units += c.len_utf16();
        if units > MAX_CHARS - 3 {
            break;
        }
        out.push(c);
    }
    out.push_str("...");
    out
}

/// A fallback title when the model gives none: the message's first line, shortened.
pub fn fallback(first_message: &str) -> String {
    let line = first_message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Untitled");
    let mut t: String = line.chars().take(50).collect();
    if line.chars().count() > 50 {
        t = format!("{}…", t.trim_end());
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning() {
        assert_eq!(
            clean("Production 500 errors").as_deref(),
            Some("Production 500 errors")
        );
        assert_eq!(
            clean("\n  \"app.js failure\".\n").as_deref(),
            Some("app.js failure")
        );
        assert_eq!(
            clean("Title: Config refactor").as_deref(),
            Some("Config refactor")
        );
        assert_eq!(clean("**Greeting**").as_deref(), Some("Greeting"));
        assert_eq!(clean("   \n"), None);
        assert_eq!(
            clean(&"x".repeat(300)).unwrap(),
            format!("{}...", "x".repeat(97))
        );
    }

    #[test]
    fn fallbacks() {
        assert_eq!(fallback("fix the build\nplease"), "fix the build");
        assert!(fallback(&"word ".repeat(30)).ends_with('…'));
        assert_eq!(fallback(""), "Untitled");
    }

    #[test]
    fn request_wraps_the_message() {
        let r = request("hey");
        assert!(
            r.contains("<message>\nhey\n</message>") && r.ends_with("Reply with only the title.")
        );
        assert!(!SYSTEM.to_lowercase().contains("opencode"));
    }

    // --- Ported from OpenCode's session-title tests (`oc_title_*`). ---

    #[test]
    fn oc_title_clips_a_long_title_without_splitting_an_emoji() {
        let b = "b".repeat(20);
        for (prefix, stored) in [
            (95, format!("{}😀...", "a".repeat(95))),
            (96, format!("{}...", "a".repeat(96))),
            (97, format!("{}...", "a".repeat(97))),
        ] {
            let reply = format!("{}😀{b}", "a".repeat(prefix));
            assert_eq!(clean(&reply).unwrap(), stored, "{prefix}");
        }
        let exact = "x".repeat(MAX_CHARS);
        assert_eq!(truncate(&exact), exact, "100 units fit");
    }

    #[test]
    fn oc_title_takes_the_first_non_empty_line() {
        assert_eq!(
            clean("Generated Title\n").as_deref(),
            Some("Generated Title")
        );
        assert_eq!(
            clean("\n  Read-only performance investigation  \nmore").as_deref(),
            Some("Read-only performance investigation")
        );
    }

    #[test]
    fn oc_title_frames_the_message_as_text_to_name() {
        assert_eq!(
            request("Help me debug the failing build"),
            "Here is the first message of the conversation:\n\n<message>\nHelp me debug the failing build\n</message>\n\nReply with only the title."
        );
    }
}
