//! The conversation as Claude Code is given it, and how CodeSmith's history
//! is kept in step with Claude's own transcript (OpenCode's `sync/*` and
//! `convert-to-claude-message`).
//!
//! CodeSmith's history is the source of truth. Claude Code keeps its own
//! transcript of what it was sent and wrote, so each request compares the
//! two by fingerprints of their visible content: a request that only adds
//! to what Claude saw continues its session, one that adds turns another
//! model wrote catches up with them in one wrapped user message, and one
//! that changed earlier history branches the transcript at the last
//! checkpoint both agree on (or starts over).

use super::store::{Checkpoint, Record};
use crate::{Media, Message, Part, Role, Source};
use serde_json::{json, Value};

/// Who a message is from. Tool results are their own messages here, as in
/// OpenCode's prompt; CodeSmith keeps them in user messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    User,
    Assistant,
    Tool,
}

impl Who {
    fn name(self) -> &'static str {
        match self {
            Who::User => "user",
            Who::Assistant => "assistant",
            Who::Tool => "tool",
        }
    }
}

/// One part of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Text(String),
    Reasoning(String),
    File(Media),
    Call {
        id: String,
        name: String,
        input: Value,
    },
    Result {
        id: String,
        name: String,
        content: String,
        is_error: bool,
        media: Vec<Media>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub who: Who,
    pub parts: Vec<Piece>,
}

impl Msg {
    pub fn user(text: impl Into<String>) -> Msg {
        Msg {
            who: Who::User,
            parts: vec![Piece::Text(text.into())],
        }
    }
    pub fn assistant(text: impl Into<String>) -> Msg {
        Msg {
            who: Who::Assistant,
            parts: vec![Piece::Text(text.into())],
        }
    }
}

/// CodeSmith's messages as Claude Code's prompt: tool results become tool
/// messages (ahead of anything else their user message carried), a
/// mid-conversation system update becomes escaped `<system-update>` user
/// text (Claude Code cannot be given a system message mid-conversation),
/// and an effort change, which the process applies live, is left out, as
/// are hosted tool calls and compaction checkpoints of other protocols.
pub fn from_messages(messages: &[Message]) -> Vec<Msg> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {
                if crate::effort_update(m).is_some() {
                    continue;
                }
                if crate::system_text(m).is_empty() {
                    continue;
                }
                out.push(Msg::user(crate::wrap_system_update(m)));
            }
            Role::User => {
                let mut tool = Vec::new();
                let mut rest = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::ToolResult {
                            id,
                            name,
                            content,
                            is_error,
                            media,
                        } => tool.push(Piece::Result {
                            id: id.clone(),
                            name: name.clone(),
                            content: content.clone(),
                            is_error: *is_error,
                            media: media.clone(),
                        }),
                        Part::Text(t) => rest.push(Piece::Text(t.text.clone())),
                        Part::Media(media) => rest.push(Piece::File(media.clone())),
                        _ => {}
                    }
                }
                if !tool.is_empty() {
                    out.push(Msg {
                        who: Who::Tool,
                        parts: tool,
                    });
                }
                if !rest.is_empty() {
                    out.push(Msg {
                        who: Who::User,
                        parts: rest,
                    });
                }
            }
            Role::Assistant => {
                let parts: Vec<Piece> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) => Some(Piece::Text(t.text.clone())),
                        Part::Reasoning(r) => Some(Piece::Reasoning(r.text.clone())),
                        Part::ToolCall {
                            id, name, input, ..
                        } => Some(Piece::Call {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        }),
                        Part::Media(media) => Some(Piece::File(media.clone())),
                        _ => None,
                    })
                    .collect();
                out.push(Msg {
                    who: Who::Assistant,
                    parts,
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fingerprints

/// Messages that hash together: a run of user messages, or one assistant
/// or tool message. `start..end` indexes the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub hash: String,
    pub who: Who,
    pub start: usize,
    pub end: usize,
}

/// JSON with object keys sorted, so equal values hash the same however
/// their keys were ordered.
pub fn canonical(v: &Value) -> String {
    match v {
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .filter(|k| !o[*k].is_null())
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&o[k])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        other => other.to_string(),
    }
}

fn hash(v: &Value) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(canonical(v).as_bytes()))
}

fn media_value(m: &Media) -> Value {
    let data = match &m.source {
        Source::Base64(d) => d.clone(),
        Source::Url(u) => u.clone(),
    };
    json!({"type": "file", "mediaType": m.mime, "data": data})
}

/// What a message shows, without reasoning or provider metadata, which
/// depend on the model that wrote it. Adjacent text parts join.
fn visible(m: &Msg) -> Vec<Value> {
    let mut parts = Vec::new();
    let mut text = String::new();
    for p in &m.parts {
        if let Piece::Reasoning(_) = p {
            continue;
        }
        if let Piece::Text(t) = p {
            text.push_str(t);
            continue;
        }
        if !text.is_empty() {
            parts.push(json!({"type": "text", "text": std::mem::take(&mut text)}));
        }
        match p {
            Piece::File(media) => parts.push(media_value(media)),
            Piece::Call { id, name, input } => {
                parts.push(json!({"type": "tool-call", "id": id, "name": name, "input": input}))
            }
            Piece::Result {
                id,
                name,
                content,
                is_error,
                media,
            } => {
                let mut output = json!({
                    "type": if *is_error { "error-text" } else { "text" },
                    "value": content,
                });
                if !media.is_empty() {
                    output["media"] = Value::Array(media.iter().map(media_value).collect());
                }
                parts.push(json!({"type": "tool-result", "id": id, "name": name, "output": output}))
            }
            Piece::Text(_) | Piece::Reasoning(_) => {}
        }
    }
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    parts
}

/// The prompt's groups. With `prefix` (an interrupted record's
/// checkpoints), a user group that matches its checkpoint ends there even
/// when another user message follows, since the input Claude was given
/// stays a boundary.
pub fn groups(prompt: &[Msg], prefix: &[Checkpoint]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut i = 0;
    while i < prompt.len() {
        let m = &prompt[i];
        let start = i;
        i += 1;
        let parts = visible(m);
        if parts.is_empty() {
            continue;
        }
        let mut members = vec![hash(&Value::Array(parts))];
        if m.who == Who::User {
            while i < prompt.len() {
                if let Some(checkpoint) = prefix.get(groups.len()) {
                    let mut sorted = members.clone();
                    sorted.sort();
                    if hash(&json!({"role": "user", "members": sorted})) == checkpoint.hash {
                        break;
                    }
                }
                let next = &prompt[i];
                if next.who != Who::User {
                    break;
                }
                members.push(hash(&Value::Array(visible(next))));
                i += 1;
            }
            members.sort();
        }
        groups.push(Group {
            hash: hash(&json!({"role": m.who.name(), "members": members})),
            who: m.who,
            start,
            end: i,
        });
    }
    groups
}

// ---------------------------------------------------------------------------
// Decisions

/// What a request does with the saved Claude session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing matches: start a new Claude session with everything.
    Fresh,
    /// Claude saw a different history: branch it, or rebuild.
    Changed,
    /// Claude missed turns another model wrote: catch up from `start`.
    CatchUp(usize),
    /// Only new user input from `start`.
    Continue(usize),
}

impl Decision {
    pub fn start(self) -> usize {
        match self {
            Decision::CatchUp(s) | Decision::Continue(s) => s,
            _ => 0,
        }
    }
}

pub fn decide(prompt: &[Msg], record: Option<&Record>) -> Decision {
    let saved: &[Checkpoint] = record.and_then(|r| r.history.as_deref()).unwrap_or(&[]);
    let interrupted = record.is_some_and(|r| r.interrupted.unwrap_or(false));
    let gs = groups(prompt, if interrupted { saved } else { &[] });
    if saved.is_empty() {
        return Decision::Fresh;
    }
    let mut matched = 0;
    while matched < saved.len() && matched < gs.len() {
        if saved[matched].hash != gs[matched].hash {
            break;
        }
        matched += 1;
    }
    if matched == 0 {
        return Decision::Fresh;
    }
    if matched < saved.len() {
        return Decision::Changed;
    }
    // Claude's drained transcript owns the interrupted reply, not
    // CodeSmith's partial copy.
    while interrupted && gs.get(matched).is_some_and(|g| g.who == Who::Assistant) {
        let reply = &prompt[gs[matched].start];
        if reply.parts.iter().any(|p| matches!(p, Piece::Call { .. })) {
            break;
        }
        matched += 1;
    }
    let start = gs.get(matched).map_or(prompt.len(), |g| g.start);
    if interrupted && start == prompt.len() {
        return Decision::Changed;
    }
    if gs[matched..].iter().any(|g| g.who != Who::User) {
        Decision::CatchUp(start)
    } else {
        Decision::Continue(start)
    }
}

/// Where a branch of the saved session can start: the last checkpoint
/// both histories agree on with a Claude message id, never between a tool
/// call and its results, and never inside the request's own user input.
#[derive(Debug, Clone, PartialEq)]
pub struct Boundary {
    pub message_id: String,
    pub start: usize,
    pub history: Vec<Checkpoint>,
}

pub fn boundary(prompt: &[Msg], record: Option<&Record>) -> Option<Boundary> {
    let gs = groups(prompt, &[]);
    let history: &[Checkpoint] = record.and_then(|r| r.history.as_deref()).unwrap_or(&[]);
    let mut pending = std::collections::HashSet::new();
    let mut found = None;
    for (i, g) in gs.iter().enumerate() {
        // Undo can resubmit identical input; keep the current user group
        // for the new turn.
        if i == gs.len() - 1 && g.who == Who::User {
            break;
        }
        let Some(checkpoint) = history.get(i).filter(|c| c.hash == g.hash) else {
            break;
        };
        for m in &prompt[g.start..g.end] {
            for p in &m.parts {
                match p {
                    Piece::Call { id, .. } => {
                        pending.insert(id.clone());
                    }
                    Piece::Result { id, .. } => {
                        pending.remove(id);
                    }
                    _ => {}
                }
            }
        }
        if let Some(id) = &checkpoint.claude_message_id {
            if pending.is_empty() {
                found = Some(Boundary {
                    message_id: id.clone(),
                    start: g.end,
                    history: history[..=i].to_vec(),
                });
            }
        }
    }
    found
}

/// A reply part kept for the checkpoints.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyPart {
    Text(String),
    Call {
        id: String,
        name: String,
        input: Value,
    },
}

pub struct Checkpoints<'a> {
    pub prompt: &'a [Msg],
    pub previous: Option<&'a Record>,
    pub user_id: Option<&'a str>,
    pub reply: &'a [ReplyPart],
    pub message_id: Option<&'a str>,
    pub tool_messages: &'a std::collections::HashMap<String, String>,
}

/// The checkpoints after a turn: one per group of the prompt, with the
/// Claude message it maps to where one is known, then the reply.
pub fn checkpoints(input: Checkpoints) -> Vec<Checkpoint> {
    let gs = groups(input.prompt, &[]);
    let mut history = Vec::new();
    for (i, g) in gs.iter().enumerate() {
        let previous = input
            .previous
            .and_then(|r| r.history.as_ref())
            .and_then(|h| h.get(i));
        let mut id = previous
            .filter(|p| p.hash == g.hash)
            .and_then(|p| p.claude_message_id.clone());
        if g.who == Who::Tool {
            for m in &input.prompt[g.start..g.end] {
                for p in &m.parts {
                    if let Piece::Result { id: call, .. } = p {
                        if let Some(t) = input.tool_messages.get(call) {
                            id = Some(t.clone());
                        }
                    }
                }
            }
        }
        if i == gs.len() - 1 {
            if let Some(u) = input.user_id {
                id = Some(u.to_string());
            }
        }
        history.push(Checkpoint {
            hash: g.hash.clone(),
            claude_message_id: id,
        });
    }
    if !input.reply.is_empty() {
        let parts = input
            .reply
            .iter()
            .map(|p| match p {
                ReplyPart::Text(t) => Piece::Text(t.clone()),
                ReplyPart::Call { id, name, input } => Piece::Call {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
            })
            .collect();
        let reply = [Msg {
            who: Who::Assistant,
            parts,
        }];
        if let Some(g) = groups(&reply, &[]).first() {
            history.push(Checkpoint {
                hash: g.hash.clone(),
                claude_message_id: input.message_id.map(str::to_string),
            });
        }
    }
    history
}

// ---------------------------------------------------------------------------
// Content

/// Claude Code's error for an attachment it cannot take.
pub fn unsupported(mime: &str) -> String {
    format!("Claude Code does not support {mime} attachments")
}

/// An attachment as a content block: PDFs as documents, PNG, JPEG, GIF and
/// WebP as images, by base64 or URL.
pub fn file_block(m: &Media) -> Result<Value, String> {
    let mime = m.mime.as_str();
    let kind = if mime == "application/pdf" {
        "document"
    } else if matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        "image"
    } else {
        return Err(unsupported(mime));
    };
    Ok(match &m.source {
        Source::Url(u) => json!({"type": kind, "source": {"type": "url", "url": u}}),
        Source::Base64(d) => {
            json!({"type": kind, "source": {"type": "base64", "media_type": mime, "data": d}})
        }
    })
}

fn text(t: impl Into<String>) -> Value {
    json!({"type": "text", "text": t.into()})
}

/// `{"id","name",…}` in that order, as OpenCode writes them.
fn ordered(fields: &[(&str, &Value)]) -> String {
    let inner: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("{}:{}", Value::String(k.to_string()), v))
        .collect();
    format!("{{{}}}", inner.join(","))
}

/// The prompt as user content. Claude cannot be given earlier turns, so
/// when the prompt has any, each message is wrapped in a tag naming its
/// role; attachments stay real content blocks, and reasoning is left out.
pub fn content(prompt: &[Msg]) -> Result<Vec<Value>, String> {
    let history = prompt.iter().any(|m| m.who != Who::User);
    let mut blocks = Vec::new();
    for m in prompt {
        if history {
            blocks.push(text(format!("<{}>", m.who.name())));
        }
        for p in &m.parts {
            match p {
                Piece::Text(t) => blocks.push(text(t.clone())),
                Piece::Reasoning(_) => {}
                Piece::File(media) => blocks.push(file_block(media)?),
                Piece::Call { id, name, input } => {
                    let call = ordered(&[
                        ("id", &Value::String(id.clone())),
                        ("name", &Value::String(name.clone())),
                        ("input", input),
                    ]);
                    blocks.push(text(format!("<tool-call>\n{call}\n</tool-call>")));
                }
                Piece::Result {
                    id,
                    name,
                    content,
                    is_error,
                    media,
                } => {
                    let output = json!({
                        "type": if *is_error { "error-text" } else { "text" },
                        "value": content,
                    });
                    let result = ordered(&[
                        ("id", &Value::String(id.clone())),
                        ("name", &Value::String(name.clone())),
                        ("output", &output),
                    ]);
                    blocks.push(text(format!("<tool-result>\n{result}\n</tool-result>")));
                    for m in media {
                        blocks.push(file_block(m)?);
                    }
                }
            }
        }
        if history {
            blocks.push(text(format!("</{}>", m.who.name())));
        }
    }
    Ok(blocks)
}

/// The opening of a catch-up checkpoint.
pub const CHECKPOINT_OPEN: &str = "<conversation-checkpoint>\nThe following is conversation history from CodeSmith that you have not seen. Treat it as prior conversation, not a new request. Continue with the user messages after this checkpoint.";
pub const CHECKPOINT_CLOSE: &str = "</conversation-checkpoint>";

/// The user content for `prompt[start..]`: the turns Claude missed inside
/// a `<conversation-checkpoint>`, then the new user input outside it.
pub fn catch_up(prompt: &[Msg], start: usize) -> Result<Vec<Value>, String> {
    let missed = &prompt[start.min(prompt.len())..];
    let end = groups(missed, &[])
        .iter()
        .rev()
        .find(|g| g.who != Who::User)
        .map_or(0, |g| g.end);
    let mut blocks = Vec::new();
    if end > 0 {
        blocks.push(text(CHECKPOINT_OPEN));
        blocks.extend(content(&missed[..end])?);
        blocks.push(text(CHECKPOINT_CLOSE));
    }
    blocks.extend(content(&missed[end..])?);
    Ok(blocks)
}
