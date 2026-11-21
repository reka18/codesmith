//! Session exports: OpenCode's export (the UI's `session.export` and the
//! CLI's `session export`), as a Markdown transcript or as the stored
//! session's JSON, raw or sanitized, and a private file to save one in.
//!
//! What each form leaves out:
//!
//! - **Every export** has the credentials CodeSmith itself holds replaced
//!   with `[redacted:credential]` wherever they appear ([`known_secrets`]:
//!   the providers' configured and saved keys and tokens, the values of
//!   their key variables, and the MCP servers' configured headers and
//!   environment). A secret CodeSmith does not know of (one a command
//!   printed, a key in a file a tool read) is not recognized.
//! - **Markdown** keeps the conversation as shown: prompts, replies,
//!   thinking when asked for, and tool calls with their input and output
//!   when asked for. URL query strings in tool input and output are
//!   replaced with `?…` (as error messages show URLs), and control
//!   characters other than newlines and tabs are dropped. Images and files
//!   are named, never included.
//! - **Raw JSON** is the whole stored session (for `session import`),
//!   credentials aside.
//! - **Sanitized JSON** (OpenCode's `--sanitize`) keeps the shape and drops
//!   the content: every text, tool input and output, reasoning, file
//!   change, summary, path and title becomes `[redacted:<field>:<record>]`
//!   (an empty one stays empty), media data included. Kept: record kinds
//!   and order, roles, ids, tool names, the agent and model, times and
//!   token counts. It still imports.

use crate::config::Config;
use crate::store::Export;
use codesmith_ai::{Message, Part, Role};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;

/// What the Markdown transcript includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transcript {
    pub thinking: bool,
    pub tools: bool,
}

/// The marker a sanitized or scrubbed value is replaced with.
const CREDENTIAL: &str = "[redacted:credential]";

/// The shortest value taken as a credential: shorter ones would match
/// ordinary words.
const MIN_SECRET: usize = 8;

/// Credentials CodeSmith holds for this config: configured and saved
/// provider keys and tokens, the values of the providers' key variables,
/// and MCP servers' configured header and environment values. Longest
/// first, so a secret that contains another is replaced whole.
pub fn known_secrets(config: &Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut ids: Vec<String> = config.providers.keys().cloned().collect();
    ids.extend(config.keyed_catalog_providers());
    ids.extend(crate::auth::providers_in(&crate::paths::auth_file()));
    for id in &ids {
        if let Some(p) = config.provider(id) {
            out.extend(p.settings.api_key.clone());
            for var in &p.env {
                out.extend(std::env::var(var).ok());
            }
        }
        if let Some(entry) = crate::auth::entry(id) {
            collect_strings(&entry, &mut out);
        }
    }
    for server in config.mcp.servers.values() {
        match server {
            crate::mcp::ServerConfig::Local { environment, .. } => {
                out.extend(environment.values().cloned())
            }
            crate::mcp::ServerConfig::Remote { headers, .. } => {
                for v in headers.values() {
                    out.push(v.clone());
                    // `Bearer <token>`: the token alone too.
                    if let Some((_, token)) = v.split_once(' ') {
                        out.push(token.to_string());
                    }
                }
            }
        }
    }
    out.retain(|s| s.chars().count() >= MIN_SECRET);
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out.dedup();
    out
}

/// The string values of a saved login (its key, tokens, ids).
fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(o) => {
            for (k, x) in o {
                // The login's kind and method are no secret.
                if !matches!(k.as_str(), "type" | "method") {
                    collect_strings(x, out);
                }
            }
        }
        _ => {}
    }
}

/// `text` with each of `secrets` replaced by `[redacted:credential]`.
pub fn scrub(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_string();
    for s in secrets.iter().filter(|s| s.chars().count() >= MIN_SECRET) {
        if text.contains(s.as_str()) {
            text = text.replace(s.as_str(), CREDENTIAL);
        }
    }
    text
}

/// Every string in `v` scrubbed (keys of maps too).
fn scrub_value(v: &mut Value, secrets: &[String]) {
    match v {
        Value::String(s) => *s = scrub(s, secrets),
        Value::Array(a) => a.iter_mut().for_each(|x| scrub_value(x, secrets)),
        Value::Object(o) => {
            let old = std::mem::take(o);
            for (k, mut x) in old {
                scrub_value(&mut x, secrets);
                o.insert(scrub(&k, secrets), x);
            }
        }
        _ => {}
    }
}

/// The raw JSON export, credentials scrubbed.
pub fn raw_json(e: &Export, secrets: &[String]) -> Value {
    let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
    scrub_value(&mut v, secrets);
    v
}

/// OpenCode's redaction marker: empty text stays empty.
fn redact(kind: &str, id: &str, value: &str) -> String {
    if value.trim().is_empty() {
        value.to_string()
    } else {
        format!("[redacted:{kind}:{id}]")
    }
}

/// String fields kept as they are: tags, roles, ids, the session's agent
/// and model, a media type. Everything else that is a string is content.
const KEEP: [&str; 9] = [
    "kind",
    "type",
    "role",
    "id",
    "parent_id",
    "agent",
    "model",
    "mime",
    "protocol",
];

/// The sanitized JSON export (see the module docs): the shape kept, every
/// piece of content replaced with a marker naming its field and record.
pub fn sanitized(e: &Export) -> Value {
    let id = e.info.id.as_str();
    let mut info = serde_json::to_value(&e.info).unwrap_or(Value::Null);
    if let Some(t) = e.info.title.as_deref() {
        info["title"] = json!(redact("session-title", id, t));
    }
    info["directory"] = json!(format!(
        "/{}",
        redact("session-directory", id, &e.info.directory)
    ));
    let entries: Vec<Value> = e
        .entries
        .iter()
        .map(|en| {
            let mut v = serde_json::to_value(en).unwrap_or(Value::Null);
            let seq = en.seq.to_string();
            if let Some(r) = v.get_mut("record") {
                sanitize_value(r, "record", &seq, false);
            }
            v
        })
        .collect();
    json!({"info": info, "entries": entries})
}

/// Replace the content strings under `v` (at field `key` of record `seq`).
/// `tool` says `v`'s object is a tool call, result or record, whose `name`
/// is kept.
fn sanitize_value(v: &mut Value, key: &str, seq: &str, tool: bool) {
    match v {
        Value::String(s) => {
            let keep = KEEP.contains(&key) || (tool && key == "name");
            if !keep {
                *s = redact(key, seq, s);
            }
        }
        Value::Array(a) => a
            .iter_mut()
            .for_each(|x| sanitize_value(x, key, seq, false)),
        Value::Object(o) => {
            let is_tool = ["input", "is_error", "ms"]
                .iter()
                .any(|k| o.contains_key(*k));
            // Free-form maps (instruction values, metadata, tool input)
            // keep no keys of their own: their keys may be paths or data.
            if matches!(key, "changed" | "metadata" | "input") {
                let marker = redact(key, seq, "x");
                *o = Map::from_iter([("redacted".to_string(), json!(marker))]);
                return;
            }
            for (k, x) in o.iter_mut() {
                sanitize_value(x, k, seq, is_tool);
            }
        }
        _ => {}
    }
}

/// Strip control characters but newlines and tabs, and Unicode format
/// characters ([`crate::unicode::format_char`]: bidi controls, zero-width
/// characters), so a transcript is safe to print to a terminal.
fn printable(s: &str) -> String {
    s.chars()
        .filter(|c| {
            (!c.is_control() || matches!(c, '\n' | '\t')) && !crate::unicode::format_char(*c)
        })
        .collect()
}

/// A code fence longer than any run of backticks in `body`.
fn fence(body: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

fn local_time(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ms.to_string())
}

fn media_name(m: &codesmith_ai::Media) -> String {
    m.filename.clone().unwrap_or_else(|| m.mime.clone())
}

/// The Markdown transcript of the visible conversation (OpenCode's
/// `formatSessionTranscript`): a header, then each prompt and reply under
/// `## User` and `## Assistant`, with thinking and tool calls as asked.
/// Credentials in `secrets` are scrubbed.
pub fn markdown(e: &Export, opts: Transcript, secrets: &[String]) -> String {
    let visible = crate::fork::messages(&e.entries);
    // Each tool call's result, by call id.
    let results: HashMap<&str, (&str, bool)> = visible
        .iter()
        .flat_map(|(_, m)| &m.parts)
        .filter_map(|p| match p {
            Part::ToolResult {
                id,
                content,
                is_error,
                ..
            } => Some((id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect();
    let clean = |s: &str| printable(&scrub(s, secrets));
    let tool_text = |s: &str| clean(&codesmith_ai::redact_urls(s));
    let mut body: Vec<String> = Vec::new();
    for (_, m) in &visible {
        if crate::runner::is_engine(m) {
            continue;
        }
        match m.role {
            Role::System => {}
            Role::User => {
                let parts: Vec<String> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) => Some(clean(&t.text)),
                        Part::Media(md) => {
                            Some(format!("[Attachment: {}]", clean(&media_name(md))))
                        }
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    body.push(format!("## User\n\n{}", parts.join("\n\n")));
                }
            }
            Role::Assistant => {
                let parts = assistant_parts(m, opts, &results, &clean, &tool_text);
                if !parts.is_empty() {
                    body.push(format!("## Assistant\n\n{}", parts.join("\n\n")));
                }
            }
        }
    }
    let title = e
        .info
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .map(|t| clean(t).replace('\n', " "))
        .unwrap_or_else(|| format!("Session {}", e.info.id));
    format!(
        "# {title}\n\n**Session ID:** {}\n**Created:** {}\n**Updated:** {}\n\n---\n\n{}\n",
        e.info.id,
        local_time(e.info.created),
        local_time(e.info.updated),
        body.join("\n\n---\n\n")
    )
}

fn assistant_parts(
    m: &Message,
    opts: Transcript,
    results: &HashMap<&str, (&str, bool)>,
    clean: &dyn Fn(&str) -> String,
    tool_text: &dyn Fn(&str) -> String,
) -> Vec<String> {
    m.parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) if !t.text.trim().is_empty() => Some(clean(&t.text)),
            Part::Reasoning(r) if opts.thinking && !r.text.trim().is_empty() => {
                Some(format!("_Thinking:_\n\n{}", clean(&r.text)))
            }
            Part::ToolCall {
                id, name, input, ..
            } if opts.tools => {
                let input = tool_text(&serde_json::to_string_pretty(input).unwrap_or_default());
                let f = fence(&input);
                let output = match results.get(id.as_str()) {
                    Some((text, true)) => format!("Error: {}", tool_text(text)),
                    Some((text, false)) => tool_text(text),
                    None => String::new(),
                };
                Some(format!(
                    "**Tool: {}**\n\n**Input:**\n{f}json\n{input}\n{f}\n\n{output}",
                    clean(name)
                ))
            }
            Part::Media(md) => Some(format!("[Attachment: {}]", clean(&media_name(md)))),
            _ => None,
        })
        .collect()
}

/// The file name an export is offered under: `session-<id>.md` or `.json`.
pub fn default_name(id: &str, json: bool) -> String {
    let id: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("session-{id}.{}", if json { "json" } else { "md" })
}

/// Save `content` at `path`, owner-only (0600). A new file is created
/// exclusively, so nothing there is ever replaced; with `overwrite` an
/// existing file is replaced atomically (a fresh private file renamed over
/// it). Fails with `AlreadyExists` when the file is there and `overwrite`
/// is off, and for a folder or a missing parent folder (none is created).
pub fn save(path: &Path, content: &str, overwrite: bool) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind, Write};
    if path.as_os_str().is_empty() {
        return Err(Error::new(ErrorKind::InvalidInput, "no file name"));
    }
    if path.is_dir() {
        return Err(Error::new(ErrorKind::InvalidInput, "that is a folder"));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.is_dir() {
        return Err(Error::new(
            ErrorKind::NotFound,
            format!("no folder {}", parent.display()),
        ));
    }
    // A symlink there (even a dangling one) counts as a file: it is never
    // followed, and replaced only on confirmation.
    let exists = std::fs::symlink_metadata(path).is_ok();
    if exists && !overwrite {
        return Err(Error::new(ErrorKind::AlreadyExists, "the file exists"));
    }
    if exists {
        return crate::paths::write_private(path, content.as_bytes());
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()
}
