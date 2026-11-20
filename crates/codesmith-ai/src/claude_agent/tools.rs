//! CodeSmith's tools as Claude Code sees them (OpenCode's `tools/*`).
//!
//! Claude Code's own tools are off. CodeSmith's tools reach Claude as the
//! tools of one MCP server that lives in this process and that Claude Code
//! talks to over its control channel. When Claude calls one, the call is
//! held: the step ends with the call, CodeSmith runs it like any other
//! model's tool call (its permission checks and prompts included), and the
//! next request hands the result back to the held call, which answers
//! Claude. So every tool call passes CodeSmith's permissions, and there is
//! one permission system.

use super::history::{groups, Msg, Piece, Who};
use crate::{Media, ToolSpec};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// The tool server's name, and the prefix of its tools' names.
pub const SERVER: &str = "codesmith";
pub const PREFIX: &str = "mcp__codesmith__";

/// Claude Code's longest tool name.
const NAME_LIMIT: usize = 64;

/// A tool's name on the server: as-is when it fits Claude's limit and
/// alphabet, else sanitized, cut and given a hash of the original.
pub fn short(name: &str) -> String {
    let limit = NAME_LIMIT - PREFIX.len();
    let plain = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if name.len() <= limit && plain && !name.is_empty() {
        return name.to_string();
    }
    use sha2::Digest;
    let hash = format!("{:x}", sha2::Sha256::digest(name.as_bytes()));
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(limit - 11)
        .collect();
    format!("{clean}_{}", &hash[..10])
}

/// The CodeSmith tool a Claude tool name calls; `None` for a tool that was
/// not offered, which Claude Code answers itself.
pub fn original<'a>(name: &str, tools: &'a [ToolSpec]) -> Option<&'a str> {
    tools
        .iter()
        .find(|t| format!("{PREFIX}{}", short(&t.name)) == name)
        .map(|t| t.name.as_str())
}

/// Claude Code cuts each MCP tool description at this many characters.
const DESCRIPTION_LIMIT: usize = 2048;

fn cut(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// A tool's description within Claude Code's limit. The subagent tool's
/// agent list comes last, so in a long description it moves first.
pub fn description(tool: &ToolSpec) -> String {
    let text = &tool.description;
    if tool.name != "subagent" || text.chars().count() <= DESCRIPTION_LIMIT {
        return cut(text, DESCRIPTION_LIMIT);
    }
    match text.find("Available agents:") {
        Some(at) => cut(
            &format!("{}\n\n{}", &text[at..], &text[..at]),
            DESCRIPTION_LIMIT,
        ),
        None => cut(text, DESCRIPTION_LIMIT),
    }
}

/// The server's tool list.
pub fn definitions(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": short(&t.name),
                "description": description(t),
                "inputSchema": t.parameters,
                // Claude runs tools that are not read-only one at a time, so
                // each call takes the id from the hook just before it.
                "annotations": {"readOnlyHint": false},
                "_meta": {"anthropic/alwaysLoad": true, "anthropic/maxResultSizeChars": 500_000},
            })
        })
        .collect()
}

/// The server's MCP `initialize` result.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {"tools": {}},
        "serverInfo": {"name": SERVER, "version": env!("CARGO_PKG_VERSION")},
    })
}

// ---------------------------------------------------------------------------
// Results

/// A tool result for Claude: the MCP result, and, when the result carries
/// media an MCP result cannot (PDFs, linked images), the content blocks the
/// after-call hook puts in its place.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub mcp: Value,
    pub replacement: Option<Vec<Value>>,
}

/// The result of a call to a tool that was removed while Claude waited.
pub const REMOVED: &str = "This tool is no longer available.";
/// The result of calls a stopped turn leaves waiting.
pub const CANCELLED: &str = "Cancelled by user.";

pub fn error_outcome(text: &str) -> Outcome {
    Outcome {
        mcp: json!({"isError": true, "content": [{"type": "text", "text": text}]}),
        replacement: None,
    }
}

/// A CodeSmith tool result as Claude's.
pub fn convert(content: &str, is_error: bool, media: &[Media]) -> Result<Outcome, String> {
    let mut parts = vec![Piece::Text(content.to_string())];
    parts.extend(media.iter().cloned().map(Piece::File));
    let blocks = super::history::content(&[Msg {
        who: Who::User,
        parts,
    }])?;
    let mut mcp = Vec::new();
    let mut replace = false;
    for b in &blocks {
        match b["type"].as_str() {
            Some("text") => mcp.push(json!({"type": "text", "text": b["text"]})),
            Some("image") if b["source"]["type"] == "base64" => mcp.push(json!({
                "type": "image",
                "mimeType": b["source"]["media_type"],
                "data": b["source"]["data"],
            })),
            _ => {
                replace = true;
                mcp.push(json!({"type": "text", "text": "Attached document or image follows."}));
            }
        }
    }
    Ok(Outcome {
        mcp: json!({"content": mcp, "isError": is_error}),
        replacement: replace.then_some(blocks),
    })
}

/// The tool results at `prompt[start..]`.
#[derive(Debug, Default)]
pub struct Collected {
    pub results: HashMap<String, Outcome>,
    /// Where the results end.
    pub end: usize,
    /// Some call was stopped rather than run (declined, or the turn was
    /// interrupted): the CodeSmith turn stopped.
    pub aborted: bool,
}

pub fn collect(
    prompt: &[Msg],
    start: usize,
    aborted: &std::collections::HashSet<String>,
) -> Result<Collected, String> {
    let mut out = Collected {
        end: start,
        ..Default::default()
    };
    let rest = &prompt[start.min(prompt.len())..];
    for g in groups(rest, &[]) {
        if g.who != Who::Tool {
            break;
        }
        for m in &rest[g.start..g.end] {
            for p in &m.parts {
                if let Piece::Result {
                    id,
                    content,
                    is_error,
                    media,
                    ..
                } = p
                {
                    if aborted.contains(id) {
                        out.aborted = true;
                    }
                    out.results
                        .insert(id.clone(), convert(content, *is_error, media)?);
                }
            }
        }
        out.end = start + g.end;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Held calls

struct Call {
    name: String,
    tx: Option<oneshot::Sender<Outcome>>,
    rx: Option<oneshot::Receiver<Outcome>>,
}

#[derive(Default)]
struct State {
    calls: HashMap<String, Call>,
    replacements: HashMap<String, Vec<Value>>,
    next: Option<String>,
}

/// Claude's tool calls waiting for CodeSmith's results (OpenCode's
/// `PendingTools`). The before-call hook gives the call's id, which the
/// MCP call itself does not carry, so tools run one at a time.
#[derive(Default)]
pub struct Pending {
    state: Mutex<State>,
}

impl Pending {
    fn expect_in(state: &mut State, id: &str, name: &str) -> Result<(), String> {
        let call = state.calls.entry(id.to_string()).or_insert_with(|| {
            let (tx, rx) = oneshot::channel();
            Call {
                name: name.to_string(),
                tx: Some(tx),
                rx: Some(rx),
            }
        });
        if call.name != name {
            return Err(format!("Mismatched Claude tool call {id}"));
        }
        Ok(())
    }

    /// Note a call Claude made, by its id.
    pub fn expect(&self, id: &str, name: &str) -> Result<(), String> {
        Self::expect_in(&mut self.state.lock().unwrap(), id, name)
    }

    /// The before-call hook: the next MCP call is this one.
    pub fn before(&self, id: &str, name: &str) -> Result<(), String> {
        let mut s = self.state.lock().unwrap();
        Self::expect_in(&mut s, id, name)?;
        s.next = Some(id.to_string());
        Ok(())
    }

    /// The MCP call: waits for the result CodeSmith sends.
    pub async fn call(&self, name: &str) -> Result<Value, String> {
        let (id, rx) = {
            let mut s = self.state.lock().unwrap();
            let id = s
                .next
                .take()
                .ok_or("Claude invoked a CodeSmith tool without its PreToolUse ID")?;
            Self::expect_in(&mut s, &id, name)?;
            let rx = s.calls.get_mut(&id).and_then(|c| c.rx.take());
            (id, rx)
        };
        let outcome = match rx {
            Some(rx) => rx.await.unwrap_or_else(|_| error_outcome(CANCELLED)),
            None => error_outcome(CANCELLED),
        };
        let mut s = self.state.lock().unwrap();
        if let Some(r) = outcome.replacement.clone() {
            s.replacements.insert(id.clone(), r);
        }
        s.calls.remove(&id);
        Ok(outcome.mcp)
    }

    /// Answer a waiting call.
    pub fn resolve(&self, id: &str, outcome: Outcome) -> Result<(), String> {
        let mut s = self.state.lock().unwrap();
        let call = s
            .calls
            .get_mut(id)
            .ok_or_else(|| format!("No waiting Claude tool call {id}"))?;
        if let Some(tx) = call.tx.take() {
            let _ = tx.send(outcome);
        }
        Ok(())
    }

    /// The after-call hook: the content that replaces the result, if any.
    pub fn after(&self, id: &str) -> Option<Vec<Value>> {
        self.state.lock().unwrap().replacements.remove(id)
    }

    /// Answer every waiting call as cancelled. Only after an interrupt has
    /// drained the turn: answering earlier could start another model call.
    pub fn clear(&self) {
        let mut s = self.state.lock().unwrap();
        for (_, mut c) in s.calls.drain() {
            if let Some(tx) = c.tx.take() {
                let _ = tx.send(error_outcome(CANCELLED));
            }
        }
        s.replacements.clear();
        s.next = None;
    }
}
