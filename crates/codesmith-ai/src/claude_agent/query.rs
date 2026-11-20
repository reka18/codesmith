//! How `claude` is started (OpenCode's `query-options`): its flags, its
//! environment and its `initialize` request.
//!
//! Claude Code reads none of the user's Claude settings: settings files,
//! CLAUDE.md files, hooks, MCP servers, skills, auto compaction, auto
//! memory and its built-in tools are all off, and only CodeSmith's tools
//! may run, without Claude Code asking (anything else is refused). The
//! user's own login is used as Claude Code finds it; CodeSmith never reads
//! or passes a credential for it.

use super::tools;
use crate::ToolSpec;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Claude Code features that duplicate CodeSmith's, turned off (`Some`) or
/// cleared from the user's environment (`None`).
pub const ISOLATION: &[(&str, Option<&str>)] = &[
    ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", Some("1")),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", Some("1")),
    ("DISABLE_AUTO_COMPACT", Some("1")),
    ("CLAUDE_CODE_ENABLE_AWAY_SUMMARY", Some("0")),
    ("ENABLE_TOOL_SEARCH", Some("false")),
    ("CLAUDE_CODE_DISABLE_ADVISOR_TOOL", Some("1")),
    ("CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK", Some("1")),
    ("CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK", Some("1")),
    // CodeSmith's own truncation applies instead.
    ("MAX_MCP_OUTPUT_TOKENS", Some("200000")),
    // The system prompt and tools change mid-session (agent switches,
    // instruction edits); without this Claude Code replays thinking bound
    // to the old ones and the API rejects the request.
    ("CLAUDE_CODE_POLISHED_DEWDROP", Some("drop")),
    // Skips the subscription login.
    ("CLAUDE_CODE_SIMPLE", None),
    // Would override the effort chosen in CodeSmith.
    ("CLAUDE_CODE_EFFORT_LEVEL", None),
    // Would cap every CodeSmith tool call, failing subagents and
    // permission prompts that run longer.
    ("MCP_TOOL_TIMEOUT", None),
    // CodeSmith's server token is never handed to a child.
    ("CODESMITH_SERVER_TOKEN", None),
];

/// Names CodeSmith to Claude Code (which adds it to its user agent), so
/// requests say honestly which program drove them.
pub const CLIENT_APP_VAR: &str = "CLAUDE_AGENT_SDK_CLIENT_APP";

/// How Claude is prompted.
#[derive(Debug, Clone, PartialEq)]
pub enum System {
    /// Claude Code's own prompt, with this appended.
    Preset(String),
    /// This prompt instead (a title is a standalone task).
    Custom(String),
}

/// Everything a `claude` is started with.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub executable: PathBuf,
    /// The user's environment, before CodeSmith's settings.
    pub env: BTreeMap<String, String>,
    /// The configured connection (base URL, headers, body, timeouts).
    pub connection: BTreeMap<String, String>,
    pub directory: Option<PathBuf>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub max_output: Option<u32>,
    pub system: Option<System>,
    pub tools: Vec<ToolSpec>,
    /// A chat (it streams partial messages and takes hooks); otherwise the
    /// login check.
    pub chat: bool,
    pub persist: bool,
    pub resume: Option<String>,
    pub session_id: Option<String>,
}

impl Launch {
    pub fn args(&self) -> Vec<String> {
        let mut a: Vec<String> = [
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
            // Without summaries there is no reasoning text to show.
            "--thinking=adaptive",
            "--thinking-display=summarized",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if let Some(e) = &self.effort {
            a.push(format!("--effort={e}"));
        }
        if let Some(m) = &self.model {
            a.push(format!("--model={m}"));
        }
        if let Some(r) = &self.resume {
            a.push(format!("--resume={r}"));
        }
        a.push(format!("--allowedTools={}*", tools::PREFIX));
        a.push("--tools=".into());
        a.push("--setting-sources=".into());
        a.push("--strict-mcp-config".into());
        a.push("--permission-mode=dontAsk".into());
        if self.chat {
            a.push("--include-partial-messages".into());
        }
        if let Some(s) = &self.session_id {
            a.push(format!("--session-id={s}"));
        }
        if !self.persist {
            a.push("--no-session-persistence".into());
        }
        a.push("--settings".into());
        a.push(r#"{"disableAllHooks":true}"#.into());
        a
    }

    /// The user's environment, then the configured connection, then the
    /// settings CodeSmith turns off, later ones winning.
    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        env.extend(self.connection.clone());
        for (k, v) in ISOLATION {
            match v {
                Some(v) => env.insert(k.to_string(), v.to_string()),
                None => env.remove(*k),
            };
        }
        env.entry(CLIENT_APP_VAR.into())
            .or_insert_with(|| format!("codesmith/{}", env!("CARGO_PKG_VERSION")));
        if let Some(n) = self.max_output {
            env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), n.to_string());
        }
        env
    }

    pub fn initialize(&self) -> Value {
        let mut r = json!({"subtype": "initialize"});
        if self.chat {
            r["hooks"] = json!({
                "PreToolUse": [{"hookCallbackIds": [HOOK_BEFORE]}],
                "PostToolUse": [{"hookCallbackIds": [HOOK_AFTER]}],
            });
        }
        if !self.tools.is_empty() {
            r["sdkMcpServers"] = json!([tools::SERVER]);
            r["sdkMcpServerManifests"] = json!({
                tools::SERVER: {
                    "initializeResult": tools::initialize_result(),
                    "toolsListResult": {"tools": tools::definitions(&self.tools)},
                }
            });
        }
        match &self.system {
            Some(System::Preset(append)) => {
                r["appendSystemPrompt"] = Value::String(append.clone());
                // Agent changes take effect when a process resumes the same
                // session.
                r["systemPromptSnapshot"] = Value::Bool(false);
            }
            Some(System::Custom(text)) => r["systemPrompt"] = json!([text]),
            None => r["systemPrompt"] = json!([""]),
        }
        r["skills"] = json!([]);
        r
    }
}

pub const HOOK_BEFORE: &str = "hook_0";
pub const HOOK_AFTER: &str = "hook_1";

/// The configured connection as Claude Code's environment: a base URL,
/// header and stream timeouts, headers and extra body fields. Keys are not
/// passed: Claude Code uses its own login.
pub fn connection_env(
    base_url: Option<&str>,
    headers: &[(String, String)],
    body: &serde_json::Map<String, Value>,
    header_timeout: Option<u64>,
    chunk_timeout: Option<u64>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(u) = base_url.filter(|u| !u.is_empty()) {
        env.insert("ANTHROPIC_BASE_URL".into(), u.to_string());
    }
    if let Some(t) = header_timeout {
        env.insert("API_TIMEOUT_MS".into(), t.to_string());
    }
    if let Some(t) = chunk_timeout {
        env.insert("CLAUDE_STREAM_IDLE_TIMEOUT_MS".into(), t.to_string());
    }
    if !headers.is_empty() {
        let lines: Vec<String> = headers.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        env.insert("ANTHROPIC_CUSTOM_HEADERS".into(), lines.join("\n"));
    }
    if !body.is_empty() {
        env.insert(
            "CLAUDE_CODE_EXTRA_BODY".into(),
            Value::Object(body.clone()).to_string(),
        );
    }
    env
}

/// The note Claude Code's prompt is given in a chat.
pub const RUNTIME_INFO: &str = "<runtime_info>You are running in CodeSmith through the Claude Code harness. Mention this only when asked.</runtime_info>";

/// A chat's appended prompt: the note, the agent's role and CodeSmith's
/// instructions.
pub fn preset(role: Option<&str>, instructions: Option<&str>) -> System {
    let mut blocks = vec![RUNTIME_INFO.to_string()];
    if let Some(r) = role.filter(|r| !r.is_empty()) {
        blocks.push(format!("# Agent role (from CodeSmith)\n{r}"));
    }
    if let Some(i) = instructions.filter(|i| !i.is_empty()) {
        blocks.push(i.to_string());
    }
    System::Preset(blocks.join("\n\n"))
}

/// A side call's own prompt, or a plain helper's.
pub fn utility(system: &str) -> System {
    System::Custom(if system.trim().is_empty() {
        "You are a text generation helper. Return only the requested output.".into()
    } else {
        system.to_string()
    })
}
