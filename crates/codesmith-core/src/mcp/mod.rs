//! Model Context Protocol client: tools from MCP servers become tools the
//! model can call, named `<server>_<tool>` as in OpenCode. Config follows
//! OpenCode's `mcp.servers` shape: `{"type": "local", "command": [...]}` for a
//! stdio server or `{"type": "remote", "url": ...}` for streamable HTTP.
//!
//! Beyond tools, as in OpenCode: a server's `instructions` join the system
//! prompt, its resources are read through the `list_mcp_resources` and
//! `read_mcp_resource` tools, its prompts become `/server:prompt` commands,
//! `list_changed` notifications refresh those lists, and MCP Events (the
//! draft `io.modelcontextprotocol/events` extension) drive event-triggered
//! automations.

pub mod blobs;
pub mod defaults;
pub mod elicit;
pub mod events;
pub mod oauth;

pub use events::{EventMessage, EventStream, EventType, StreamError};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_STARTUP_MS: u64 = 30_000;
const DEFAULT_CATALOG_MS: u64 = 30_000;
const DEFAULT_EXECUTION_MS: u64 = 5 * 60_000;
/// Stop following `nextCursor` after this many pages.
const MAX_PAGES: usize = 100;
/// The largest single message (an SSE event or a JSON reply) accepted from an
/// HTTP MCP server; a server that sends more fails the request.
const MAX_MESSAGE: usize = 16 << 20;

/// The tool that lists MCP resources (also its permission action).
pub const LIST_RESOURCES: &str = "list_mcp_resources";
/// The tool that reads one MCP resource (also its permission action).
pub const READ_RESOURCE: &str = "read_mcp_resource";
/// The tool that lists a server's push events (also its permission action).
pub const LIST_EVENTS: &str = "list_mcp_events";
/// The `_meta` key a tool call carries its session's ID under.
pub const SESSION_META: &str = "codesmith/sessionId";

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Timeouts {
    pub startup: Option<u64>,
    pub catalog: Option<u64>,
    pub execution: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerConfig {
    Local {
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
        #[serde(default)]
        disabled: bool,
        /// Offer this server's tools inside Code Mode (`execute`) instead of
        /// as direct tool calls. Default true, as in OpenCode.
        #[serde(default)]
        codemode: Option<bool>,
        #[serde(default)]
        timeout: Option<Timeouts>,
    },
    Remote {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// `false` turns OAuth off; an object pre-sets the OAuth client.
        #[serde(default)]
        oauth: Option<oauth::Setting>,
        #[serde(default)]
        disabled: bool,
        /// Offer this server's tools inside Code Mode (`execute`) instead of
        /// as direct tool calls. Default true, as in OpenCode.
        #[serde(default)]
        codemode: Option<bool>,
        #[serde(default)]
        timeout: Option<Timeouts>,
    },
}

impl ServerConfig {
    pub fn codemode(&self) -> bool {
        match self {
            ServerConfig::Local { codemode, .. } | ServerConfig::Remote { codemode, .. } => {
                codemode.unwrap_or(true)
            }
        }
    }

    pub fn disabled(&self) -> bool {
        match self {
            ServerConfig::Local { disabled, .. } | ServerConfig::Remote { disabled, .. } => {
                *disabled
            }
        }
    }
    fn timeouts(&self) -> Option<&Timeouts> {
        match self {
            ServerConfig::Local { timeout, .. } | ServerConfig::Remote { timeout, .. } => {
                timeout.as_ref()
            }
        }
    }
}

/// `mcp` in codesmith.json.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpConfig {
    pub timeout: Timeouts,
    pub servers: BTreeMap<String, ServerConfig>,
}

/// One tool offered by a server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// The name the model sees: `<server>_<tool>`.
    pub name: String,
    pub server: String,
    /// The tool's own name on its server.
    pub remote: String,
    pub description: String,
    pub schema: Value,
    /// The tool's `outputSchema`, when it declares one: shown as its result
    /// type in the Code Mode catalog, whose programs then get the call's
    /// `structuredContent` as its value.
    pub output_schema: Option<Value>,
    /// Called through Code Mode rather than directly.
    pub codemode: bool,
}

/// One prompt offered by a server, run as the `/server:prompt` command.
#[derive(Debug, Clone, PartialEq)]
pub struct McpPrompt {
    pub server: String,
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connected {
        tools: usize,
    },
    Disabled,
    Failed(String),
    /// The server wants an OAuth login: `codesmith mcp auth <name>`.
    NeedsAuth,
}

/// Replace anything outside `[A-Za-z0-9_-]` with `_`.
/// The largest tool input schema taken from a server, serialized. Schemas
/// are rendered into the system prompt and sent with every request; past
/// this a tool keeps an open object schema.
pub const MAX_INPUT_SCHEMA: usize = 256 * 1024;

/// A tool's `inputSchema`: an object no larger than [`MAX_INPUT_SCHEMA`],
/// otherwise an open object schema.
fn input_schema(schema: Option<&Value>) -> Value {
    schema
        .filter(|s| s.is_object())
        .filter(|s| serde_json::to_vec(s).is_ok_and(|b| b.len() <= MAX_INPUT_SCHEMA))
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}))
}

/// A tool's `outputSchema`, kept when it is a JSON object (any shape, as
/// OpenCode takes them: `anyOf` unions too) no larger than
/// [`MAX_INPUT_SCHEMA`].
fn output_schema(schema: Option<&Value>) -> Option<Value> {
    schema
        .filter(|s| s.is_object())
        .filter(|s| serde_json::to_vec(s).is_ok_and(|b| b.len() <= MAX_INPUT_SCHEMA))
        .cloned()
}

/// The value a Code Mode program gets from a call to a tool that declares
/// an output schema: the result's `structuredContent`, as the server sent
/// it (not checked against the schema). `None` for a failed call, a tool
/// without a schema, or a result without structured content.
pub fn structured_result(result: &Value, declared: bool) -> Option<Value> {
    if !declared || result["isError"].as_bool().unwrap_or(false) {
        return None;
    }
    result.get("structuredContent").cloned()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The longest tool name providers accept (OpenAI, Anthropic and Gemini
/// all stop at 64 characters).
pub const MAX_TOOL_NAME: usize = 64;

/// How much of a name is kept before an 8-digit hash suffix (`_1a2b3c4d`)
/// so the whole stays within [`MAX_TOOL_NAME`].
const HASHED_BASE: usize = MAX_TOOL_NAME - 9;

/// The first 8 hex digits of the SHA-256 of `server` and `tool`.
fn tool_hash(server: &str, tool: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(format!("{server}\0{tool}").as_bytes());
    h[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// `name` cut to [`HASHED_BASE`] with the suffix `_<hash>`. Sanitized names
/// are ASCII, so cutting by bytes keeps whole characters.
fn hashed(name: &str, hex: &str) -> String {
    format!("{}_{hex}", &name[..name.len().min(HASHED_BASE)])
}

/// `<server>_<tool>` with anything outside `[A-Za-z0-9_-]` turned into `_`.
/// A longer name than [`MAX_TOOL_NAME`] is cut and given a hash of the
/// server and tool, so it is accepted and stays distinct.
pub fn tool_name(server: &str, tool: &str) -> String {
    let name = sanitize(&format!("{server}_{tool}"));
    if name.len() <= MAX_TOOL_NAME {
        return name;
    }
    hashed(&name, &tool_hash(server, tool))
}

impl McpTool {
    /// `<server>_<tool>`, sanitized but never cut or suffixed: the name
    /// rules are written for. It is the name the tool is offered under
    /// unless that was too long (and was cut and hashed) or collided with
    /// another tool's (and got a suffix). Rules for it still deny the tool:
    /// a session checks both names, and a deny on either wins.
    pub fn shared_name(&self) -> String {
        sanitize(&format!("{}_{}", self.server, self.remote))
    }
}

/// Tools with their final names, each with its index in `tools`. Names come
/// from [`tool_name`], which can give two tools the same name (server `x`
/// with tool `y_z` and server `x_y` with tool `z` are both `x_y_z`). Every
/// tool sharing a name with a different tool gets a suffix made from a hash
/// of its server and tool (`x_y_z_1a2b3c4d`), with a note in its
/// description, so no call meant for one server reaches another and no
/// rule allowing the shared name applies to either (one denying it still
/// does: [`McpTool::shared_name`]). The suffixed name is cut to stay within
/// [`MAX_TOOL_NAME`]. A tool a server lists twice is kept once.
fn disambiguate(tools: Vec<McpTool>) -> Vec<(usize, McpTool)> {
    let mut owners: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for t in &tools {
        let o = owners.entry(t.name.clone()).or_default();
        let key = (t.server.clone(), t.remote.clone());
        if !o.contains(&key) {
            o.push(key);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (i, mut t) in tools.into_iter().enumerate() {
        if owners[&t.name].len() > 1 {
            let named = hashed(&t.name, &tool_hash(&t.server, &t.remote));
            let note = format!(
                "(Named {named}: the name {} is shared with another MCP tool. This is tool {:?} on server {:?}.)",
                t.name, t.remote, t.server
            );
            t.name = named;
            t.description = if t.description.is_empty() {
                note
            } else {
                format!("{}\n\n{note}", t.description)
            };
        }
        // A duplicate listing, or a suffixed name equal to another tool's:
        // the first keeps it, so a name never stands for two tools.
        if seen.insert(t.name.clone()) {
            out.push((i, t));
        }
    }
    out
}

/// A permission pattern matching every tool name a server's tools get
/// (`<server>_*`, including names disambiguated after a collision). For a
/// server name too long to stay whole in a tool name, the part every one
/// keeps, which may match other servers' tools too.
pub fn tools_pattern(server: &str) -> String {
    let prefix = format!("{}_", sanitize(server));
    format!("{}*", &prefix[..prefix.len().min(HASHED_BASE)])
}

/// The slash command that runs a server's prompt: `server:prompt`, as in OpenCode.
pub fn command_name(server: &str, prompt: &str) -> String {
    format!("{}:{}", sanitize(server), sanitize(prompt))
}

/// A `tools/call` result as the model gets it: text parts joined, images
/// (and embedded image or PDF resources) as media, other content noted.
/// Image data is sniffed when the server names no or an odd media type; data
/// that is not an image or PDF is noted instead.
pub fn result_content(result: &Value) -> (String, bool, Vec<codesmith_ai::Media>) {
    result_content_in(result, None)
}

/// [`result_content`], decoding text blobs and saving other binary content
/// (resource blobs, audio) to files in `dir` (see [`blobs`]); without `dir`
/// binary content is noted.
pub fn result_content_in(
    result: &Value,
    dir: Option<&Path>,
) -> (String, bool, Vec<codesmith_ai::Media>) {
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let mut parts = Vec::new();
    let mut media = Vec::new();
    for c in result["content"].as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("text") => parts.push(c["text"].as_str().unwrap_or("").to_string()),
            Some("image") => match result_media(c["data"].as_str(), c["mimeType"].as_str(), None) {
                Some(m) => media.push(m),
                None => parts.push("[image content omitted: not a supported image]".into()),
            },
            Some("resource") => {
                let r = &c["resource"];
                let uri = r["uri"].as_str().unwrap_or("?");
                match (r["text"].as_str(), r["blob"].as_str()) {
                    (Some(t), _) => parts.push(t.to_string()),
                    (None, Some(blob)) => {
                        match result_media(Some(blob), r["mimeType"].as_str(), Some(uri)) {
                            Some(m) => media.push(m),
                            None => parts.push(
                                blobs::describe(uri, r["mimeType"].as_str(), blob, dir)
                                    .unwrap_or_else(|| format!("[resource {uri}]")),
                            ),
                        }
                    }
                    _ => parts.push(format!("[resource {uri}]")),
                }
            }
            Some("audio") => parts.push(
                c["data"]
                    .as_str()
                    .and_then(|d| blobs::describe("audio", c["mimeType"].as_str(), d, dir))
                    .unwrap_or_else(|| "[audio content omitted]".into()),
            ),
            Some("resource_link") => {
                parts.push(format!("[resource {}]", c["uri"].as_str().unwrap_or("?")))
            }
            Some(other) => parts.push(format!("[{other} content omitted]")),
            None => {}
        }
    }
    if parts.is_empty() && media.is_empty() {
        if let Some(s) = result.get("structuredContent") {
            parts.push(s.to_string());
        }
    }
    (parts.join("\n"), is_error, media)
}

/// Base64 data from a result as media, when its bytes are an image or PDF.
fn result_media(
    data: Option<&str>,
    mime: Option<&str>,
    uri: Option<&str>,
) -> Option<codesmith_ai::Media> {
    let data = data?.trim();
    let bytes = crate::media::unbase64(data)?;
    let detected = crate::media::detect(&bytes)?;
    // The bytes decide; a server's `image/jpg` or missing type is common.
    let mime = mime.filter(|m| *m == detected).unwrap_or(detected);
    let m = codesmith_ai::Media::base64(mime, data);
    let name = uri
        .and_then(|u| u.rsplit('/').next())
        .filter(|n| !n.is_empty());
    Some(match name {
        Some(n) => m.named(n),
        None => m,
    })
}

/// The text of a `tools/call` result, for callers that cannot pass media on
/// (Code Mode, automations): media is noted in the text instead.
pub fn result_text(result: &Value) -> (String, bool) {
    let (text, is_error, media) = result_content(result);
    (with_media_note(text, &media, "returned as text"), is_error)
}

/// `text` with a note that `media` was left out, and why.
pub fn with_media_note(mut text: String, media: &[codesmith_ai::Media], why: &str) -> String {
    if media.is_empty() {
        return text;
    }
    let n = media.len();
    let what = if media
        .iter()
        .all(|m| m.kind() == codesmith_ai::MediaKind::Image)
    {
        "image"
    } else {
        "attachment"
    };
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&format!(
        "[{n} {what}{} omitted: {why}]",
        if n == 1 { "" } else { "s" }
    ));
    text
}

/// The text of a `resources/read` result: each part's text, binary parts noted.
pub fn resource_text(result: &Value) -> String {
    resource_text_in(result, None)
}

/// [`resource_text`], decoding text blobs and saving binary ones to files
/// in `dir` (see [`blobs`]).
pub fn resource_text_in(result: &Value, dir: Option<&Path>) -> String {
    let contents = result["contents"].as_array().cloned().unwrap_or_default();
    let several = contents.len() > 1;
    contents
        .iter()
        .map(|c| {
            let uri = c["uri"].as_str().unwrap_or("?");
            let body = match (c["text"].as_str(), c["blob"].as_str()) {
                (Some(t), _) => t.to_string(),
                (None, Some(b)) => blobs::describe(uri, c["mimeType"].as_str(), b, dir)
                    .unwrap_or_else(|| {
                        format!(
                            "[binary content, {} omitted: {} bytes of base64]",
                            blobs::mime(c["mimeType"].as_str())
                                .unwrap_or_else(|| "unknown type".into()),
                            b.len()
                        )
                    }),
                _ => "[empty]".into(),
            };
            if several {
                format!("<resource uri=\"{uri}\">\n{body}\n</resource>")
            } else {
                body
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The text of a `prompts/get` result: its text messages joined, as in OpenCode.
pub fn prompt_result_text(result: &Value) -> String {
    result["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let c = &m["content"];
            match c {
                Value::String(s) => Some(s.clone()),
                _ if c["type"] == "text" => c["text"].as_str().map(str::to_string),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// `<mcp_instructions>` for the system prompt, as OpenCode renders it: one
/// `<server>` block per server, with a pointer to `execute` when its tools
/// are reached through Code Mode. Entries are (server, instructions, codemode).
pub fn render_instructions(servers: &[(String, String, bool)]) -> String {
    render_summaries(&summaries(servers))
}

/// One server's instructions as the instruction state keeps them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstructionSummary {
    pub server: String,
    pub instructions: String,
    /// `false` when the server's tools are direct tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codemode: Option<bool>,
}

fn summaries(servers: &[(String, String, bool)]) -> Vec<InstructionSummary> {
    servers
        .iter()
        .map(|(server, text, codemode)| InstructionSummary {
            server: server.clone(),
            instructions: text.clone(),
            codemode: (!codemode).then_some(false),
        })
        .collect()
}

fn summary_entries(servers: &[InstructionSummary]) -> Vec<String> {
    let mut out = Vec::new();
    for s in servers {
        out.push(format!("  <server name=\"{}\">", s.server));
        if s.codemode != Some(false) {
            out.push(format!(
                "    Use tools from this server through `execute` under `tools[{}]`.",
                serde_json::to_string(&s.server).unwrap()
            ));
        }
        out.extend(s.instructions.split('\n').map(|l| format!("    {l}")));
        out.push("  </server>".into());
    }
    out
}

fn render_summaries(servers: &[InstructionSummary]) -> String {
    let mut out = vec!["<mcp_instructions>".to_string()];
    out.extend(summary_entries(servers));
    out.push("</mcp_instructions>".into());
    out.join("\n")
}

const INSTRUCTION_UPDATE: crate::instruction_state::ListUpdate<InstructionSummary> =
    crate::instruction_state::ListUpdate {
        key: |s| s.server.clone(),
        changed: |a, b| a.instructions != b.instructions || a.codemode != b.codemode,
        entries: summary_entries,
        render: render_summaries,
        replaced: "The available MCP server instructions have changed. This list supersedes the previous one.",
        added: "New MCP server instructions are available in addition to those previously listed:",
        changed_text: "The following MCP server instructions have changed. Each entry replaces the previously listed server with the same name:",
        removed: |names| {
            format!("Instructions for the following MCP servers are no longer available: {names}.")
        },
    };

pub const INSTRUCTIONS_KEY: &str = "core/mcp-guidance";

/// Server instructions as an instruction source: `(server, instructions,
/// through Code Mode)` for the servers whose tools the session can reach,
/// none being a removal.
pub fn instruction_source(servers: &[(String, String, bool)]) -> crate::instruction_state::List {
    use crate::instruction_state::{make, Definition, Observed};
    let mut list = summaries(servers);
    list.sort_by(|a, b| a.server.cmp(&b.server));
    make::<Vec<InstructionSummary>>(Definition {
        key: INSTRUCTIONS_KEY.into(),
        read: Box::new(move || {
            if list.is_empty() {
                Observed::Removed
            } else {
                Observed::Value(list.clone())
            }
        }),
        initial: Box::new(|s| render_summaries(s)),
        changed: Box::new(|a, b| Some(INSTRUCTION_UPDATE.render(a, b))),
        removed: Some(Box::new(|_| {
            "MCP server instructions are no longer available.".into()
        })),
    })
}

/// The `list_mcp_resources` and `read_mcp_resource` tool definitions:
/// (name, description, parameters), with OpenCode's descriptions.
pub fn resource_tools() -> Vec<(&'static str, String, Value)> {
    vec![
        (
            LIST_RESOURCES,
            "List documents, records, and other data exposed by MCP servers. Use this when the user refers to something that is not a local file, such as a URI with a custom scheme, then load a match with read_mcp_resource. Each entry names the server to read it from. Templates are resources addressed by a parameter such as a record ID; fill in the uriTemplate placeholders before reading.".into(),
            json!({"type": "object", "properties": {
                "server": {"type": "string", "description": "MCP server name as configured. Omit to list every server."}
            }}),
        ),
        (
            READ_RESOURCE,
            "Read one MCP resource by server and URI. Not for local files. Always return the full contents rather than slicing or filtering them; oversized output is truncated automatically and the full content is saved to a file you can read.".into(),
            json!({"type": "object", "properties": {
                "server": {"type": "string", "description": "The server field of the discovered resource."},
                "uri": {"type": "string", "description": "Exact resource URI from discovery, an expanded uriTemplate, or the user."}
            }, "required": ["server", "uri"]}),
        ),
    ]
}

/// The `list_mcp_events` tool definition: (name, description, parameters),
/// with OpenCode's description.
pub fn events_tool() -> (&'static str, String, Value) {
    (
        LIST_EVENTS,
        "List the events an MCP server sends. Use one as an automation's event trigger.".into(),
        json!({"type": "object", "properties": {
            "server": {"type": "string", "description": "MCP server name as configured"}
        }, "required": ["server"]}),
    )
}

/// The longest event description listed; a server's longer one is cut.
const MAX_EVENT_DESCRIPTION: usize = 1000;

/// An asker for a server's elicitations while a call runs.
struct Asking {
    shared: Arc<Shared>,
    asker: Arc<dyn crate::question::Questioner>,
}

impl Asking {
    fn new(shared: &Arc<Shared>, asker: Arc<dyn crate::question::Questioner>) -> Asking {
        shared.askers.lock().unwrap().push(asker.clone());
        Asking {
            shared: shared.clone(),
            asker,
        }
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        let mut askers = self.shared.askers.lock().unwrap();
        if let Some(i) = askers.iter().rposition(|a| Arc::ptr_eq(a, &self.asker)) {
            askers.remove(i);
        }
    }
}

/// How to reply to a request the server sends us.
pub fn answer_server_request(method: &str, roots: &[String]) -> Result<Value, (i64, String)> {
    match method {
        "ping" => Ok(json!({})),
        "roots/list" => Ok(
            json!({"roots": roots.iter().map(|r| json!({"uri": format!("file://{r}"), "name": r})).collect::<Vec<_>>()}),
        ),
        m => Err((-32601, format!("method not found: {m}"))),
    }
}

/// A JSON-RPC error the server answered with.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

fn rpc_result(m: &Value) -> Result<Value> {
    if let Some(e) = m.get("error") {
        return Err(RpcError {
            code: e["code"].as_i64().unwrap_or(0),
            message: e["message"].as_str().unwrap_or("").to_string(),
        }
        .into());
    }
    Ok(m.get("result").cloned().unwrap_or(Value::Null))
}

type StreamTx = mpsc::UnboundedSender<Result<EventMessage, StreamError>>;

/// State the transport's reader shares with the client: replies awaited,
/// open event streams, and which lists the server said changed.
#[derive(Default)]
struct Shared {
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
    streams: Mutex<HashMap<u64, StreamTx>>,
    tools_stale: AtomicBool,
    prompts_stale: AtomicBool,
    roots: Vec<String>,
    log: Option<PathBuf>,
    /// Why the connection ended (a stdio server's process exited).
    exited: Mutex<Option<String>>,
    /// A reconnect was tried after it ended (only one is).
    reconnect_tried: AtomicBool,
    /// The server's name, for its elicitation forms.
    server: String,
    /// Who answers the server's elicitations: the question UIs of the
    /// calls in progress, the latest first.
    askers: Mutex<Vec<Arc<dyn crate::question::Questioner>>>,
    /// Pending URL elicitations' forms, by `elicitationId`.
    elicitations: Mutex<HashMap<String, String>>,
    /// Pending elicitations' forms, by the server's request id.
    elicit_requests: Mutex<HashMap<String, String>>,
    /// How many more elicitations the server may start.
    elicit_budget: Arc<elicit::Budget>,
    /// Run once when the connection ends (an attached connection leaves
    /// its stack).
    on_close: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run when the server says its tools changed (an attached connection
    /// lists them again while it serves its name).
    on_tools_changed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// What [`Shared::dispatch`] answers a server request with.
enum Answer {
    Now(Value),
    /// Once the user has answered (an elicitation).
    Later(std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>),
}

impl Answer {
    async fn value(self) -> Value {
        match self {
            Answer::Now(v) => v,
            Answer::Later(f) => f.await,
        }
    }
}

impl Shared {
    /// Route one message from the server. Returns the reply to send when it
    /// is a request.
    fn dispatch(self: &Arc<Self>, m: &Value) -> Option<Answer> {
        let id = m.get("id").filter(|v| !v.is_null());
        match (m.get("method").and_then(Value::as_str), id) {
            (Some("elicitation/create"), Some(id)) => {
                let (me, id, params) = (self.clone(), id.clone(), m["params"].clone());
                Some(Answer::Later(Box::pin(async move {
                    let result = me.elicit(&id, &params).await;
                    json!({"jsonrpc": "2.0", "id": id, "result": result})
                })))
            }
            (None, Some(id)) => {
                let tx = id
                    .as_u64()
                    .and_then(|i| self.pending.lock().unwrap().remove(&i));
                if let Some(tx) = tx {
                    let _ = tx.send(rpc_result(m));
                }
                None
            }
            (Some(method), Some(id)) => Some(Answer::Now(
                match answer_server_request(method, &self.roots) {
                    Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                    Err((code, msg)) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
                    }
                },
            )),
            (Some(method), None) => {
                self.notification(method, &m["params"]);
                None
            }
            _ => None,
        }
    }

    fn notification(&self, method: &str, params: &Value) {
        match method {
            "notifications/tools/list_changed" => {
                self.tools_stale.store(true, Ordering::SeqCst);
                let hook = self.on_tools_changed.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            "notifications/prompts/list_changed" => {
                self.prompts_stale.store(true, Ordering::SeqCst)
            }
            "notifications/message" => self.log_line(params),
            // A URL elicitation finished out of band.
            "notifications/elicitation/complete" => {
                let form = params["elicitationId"]
                    .as_str()
                    .and_then(|e| self.elicitations.lock().unwrap().get(e).cloned());
                if let Some(id) = form {
                    let answer = [(elicit::URL_FIELD.to_string(), json!(true))].into();
                    let _ = crate::form::global().reply(&id, answer);
                }
            }
            // The server gave up on a request: an elicitation's form goes.
            "notifications/cancelled" => {
                let form = self
                    .elicit_requests
                    .lock()
                    .unwrap()
                    .get(&params["requestId"].to_string())
                    .cloned();
                if let Some(id) = form {
                    let why = params["reason"].as_str().map(str::to_string);
                    let _ = crate::form::global().cancel(&id, why);
                }
            }
            m => {
                let Some(kind) = m.strip_prefix("notifications/events/") else {
                    return;
                };
                let sub = &params["_meta"][events::SUBSCRIPTION_ID];
                let Some(id) = sub
                    .as_u64()
                    .or_else(|| sub.as_str().and_then(|s| s.parse().ok()))
                else {
                    return;
                };
                let Some(msg) = events::parse(kind, params) else {
                    return;
                };
                let mut streams = self.streams.lock().unwrap();
                if let Some(tx) = streams.get(&id) {
                    let end = msg.is_err();
                    let _ = tx.send(msg);
                    if end {
                        streams.remove(&id);
                    }
                }
            }
        }
    }

    /// Answer an elicitation request `id` (see [`elicit`]): through a form
    /// the question UI of the latest call in progress answers.
    async fn elicit(&self, id: &Value, params: &Value) -> Value {
        let Some(create) = elicit::form_for(&self.server, params) else {
            return json!({"action": "accept", "content": {}});
        };
        let url = params["mode"] == "url";
        // A URL to open that is not https, or a server asking too often:
        // declined without asking.
        if url && elicit::shown_url(params).is_none() {
            return json!({"action": "decline"});
        }
        let Some(_permit) = self.elicit_budget.start(std::time::Instant::now()) else {
            return json!({"action": "decline"});
        };
        let forms = crate::form::global().clone();
        let Ok(form) = forms.create(create) else {
            return json!({"action": "cancel"});
        };
        let elicitation = params["elicitationId"].as_str().filter(|_| url);
        if let Some(e) = elicitation {
            self.elicitations
                .lock()
                .unwrap()
                .insert(e.to_string(), form.id.clone());
        }
        self.elicit_requests
            .lock()
            .unwrap()
            .insert(id.to_string(), form.id.clone());
        let asker = self.askers.lock().unwrap().last().cloned();
        match asker {
            Some(a) => crate::form::present_in_background(forms.clone(), form.id.clone(), a),
            None => {
                let _ = forms.cancel(&form.id, Some("nobody can answer it here".into()));
            }
        }
        let state = forms.wait(&form.id).await;
        if let Some(e) = elicitation {
            self.elicitations.lock().unwrap().remove(e);
        }
        self.elicit_requests.lock().unwrap().remove(&id.to_string());
        match state {
            Ok(s) => elicit::result(&s, url),
            Err(_) => json!({"action": "cancel"}),
        }
    }

    /// A `notifications/message` log entry, appended to the server's log file.
    fn log_line(&self, p: &Value) {
        use std::io::Write;
        let Some(path) = &self.log else { return };
        let data = match &p["data"] {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        let logger = p["logger"]
            .as_str()
            .map(|l| format!("{l}: "))
            .unwrap_or_default();
        if let Ok(mut f) = crate::paths::open_private_append(path) {
            let _ = writeln!(
                f,
                "[{}] {logger}{data}",
                p["level"].as_str().unwrap_or("info")
            );
        }
    }

    /// The connection is gone: fail whatever still waits on it.
    fn close(&self, why: &str) {
        self.exited
            .lock()
            .unwrap()
            .get_or_insert_with(|| why.to_string());
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(anyhow!("{why}")));
        }
        for (_, tx) in self.streams.lock().unwrap().drain() {
            let _ = tx.send(Err(StreamError::Disconnected(why.into())));
        }
        let hook = self.on_close.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// A request awaiting its reply; dropping it forgets the request.
struct Pending<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.shared.pending.lock().unwrap().remove(&self.id);
    }
}

/// Where a stream transport's messages go: a stdio server's stdin, or the
/// write half of a connection a client attached.
type Writer = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

enum Transport {
    /// Newline-delimited JSON-RPC: a local server's stdio, or a connection
    /// a client attached ([`Mcp::attach`]).
    Stream {
        writer: tokio::sync::Mutex<Writer>,
        _child: Option<Process>,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: BTreeMap<String, String>,
        session: Mutex<Option<String>>,
    },
}

/// A local server's process, in a process group of its own: when the
/// connection goes, the whole group is killed, so a wrapper's descendants
/// (`npx`, a shell script starting the real server) do not outlive it.
struct Process {
    child: tokio::process::Child,
    group: Option<i32>,
}

impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(g) = self.group.filter(|g| *g > 1) {
            // SAFETY: kill has no memory preconditions; a negative pid
            // names the process group the server leads.
            unsafe {
                libc::kill(-g, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}

pub struct Client {
    pub server: String,
    transport: Transport,
    shared: Arc<Shared>,
    next: AtomicU64,
    timeouts: (u64, u64),
    codemode: bool,
    /// The `initialize` result.
    init: OnceLock<Value>,
    tools: RwLock<Vec<McpTool>>,
    prompts: RwLock<Vec<McpPrompt>>,
    /// A stream transport's reader task, stopped with the client.
    reader: Mutex<Option<tokio::task::AbortHandle>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(r) = self.reader.lock().unwrap().take() {
            r.abort();
        }
    }
}

impl Client {
    async fn request(&self, method: &str, params: Value, timeout_ms: u64) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.send_request(id, method, params, Some(timeout_ms))
            .await
    }

    /// Send request `id` and wait for its reply (forever with no timeout).
    async fn send_request(
        &self,
        id: u64,
        method: &str,
        params: Value,
        timeout_ms: Option<u64>,
    ) -> Result<Value> {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let (tx, mut rx) = oneshot::channel();
        self.shared.pending.lock().unwrap().insert(id, tx);
        // Removes the entry however this ends, also when the caller drops
        // the future (a cancelled turn, a closed event stream).
        let _pending = Pending {
            shared: &self.shared,
            id,
        };
        let fut = async {
            self.send(&msg).await?;
            if matches!(self.transport, Transport::Http { .. }) {
                // Over HTTP the reply comes back in the POST's own response.
                return rx
                    .try_recv()
                    .map_err(|_| anyhow!("no response for request {id}"))?;
            }
            rx.await.map_err(|_| anyhow!("the MCP server exited"))?
        };
        match timeout_ms {
            Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), fut).await {
                Ok(r) => r,
                Err(_) => {
                    // Tell the server to stop working on it (best effort).
                    let cancel = json!({"requestId": id, "reason": "timed out"});
                    let _ = tokio::time::timeout(
                        Duration::from_secs(1),
                        self.notify("notifications/cancelled", Some(cancel)),
                    )
                    .await;
                    Err(anyhow!("{method} timed out after {ms} ms"))
                }
            },
            None => fut.await,
        }
    }

    /// Why the connection ended, if it has (a stdio server exited).
    pub fn exited(&self) -> Option<String> {
        self.shared.exited.lock().unwrap().clone()
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let mut msg = json!({"jsonrpc": "2.0", "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        self.send(&msg).await
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        match &self.transport {
            Transport::Stream { writer, .. } => {
                let mut line = serde_json::to_string(msg)?;
                line.push('\n');
                let mut w = writer.lock().await;
                w.write_all(line.as_bytes())
                    .await
                    .context("writing to the MCP server")?;
                w.flush().await.context("writing to the MCP server")
            }
            Transport::Http { .. } => self.post(msg).await,
        }
    }

    fn http_request(&self, msg: &Value) -> reqwest::RequestBuilder {
        let Transport::Http {
            client,
            url,
            headers,
            session,
        } = &self.transport
        else {
            unreachable!()
        };
        let mut rb = client
            .post(url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        if let Some(s) = session.lock().unwrap().clone() {
            rb = rb.header("Mcp-Session-Id", s);
        }
        if let Some(name) = events::header_name(msg) {
            rb = rb.header(events::NAME_HEADER, name);
        }
        rb.json(msg)
    }

    /// POST one message and route everything the server sends back, whether
    /// one JSON reply or an SSE stream that may carry notifications and
    /// requests before (or, for an event stream, instead of) the reply.
    async fn post(&self, msg: &Value) -> Result<()> {
        let Transport::Http { session, .. } = &self.transport else {
            unreachable!()
        };
        let sent_session = session.lock().unwrap().clone();
        let mut resp = self
            .http_request(msg)
            .send()
            .await
            .context("contacting the MCP server")?;
        // The server no longer knows the session (it restarted, or expired
        // it): the spec has the client start a new one.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            if let Some(old) = sent_session {
                let mut s = session.lock().unwrap();
                if s.as_deref() == Some(old.as_str()) {
                    *s = None;
                }
                return Err(SessionExpired.into());
            }
        }
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *session.lock().unwrap() = Some(s.to_string());
        }
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let www = resp
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let refused = oauth::Unauthorized(www);
            // A 403 is a login matter only when it asks for more scope.
            if status == reqwest::StatusCode::UNAUTHORIZED || refused.insufficient_scope() {
                return Err(refused.into());
            }
        }
        if !status.is_success() {
            let body: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect();
            return Err(anyhow!("HTTP {}: {body}", status.as_u16()));
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if content_type.contains("text/event-stream") {
            let mut frames = SseFrames::new(MAX_MESSAGE);
            while let Some(chunk) = resp
                .chunk()
                .await
                .context("reading the MCP server's stream")?
            {
                for block in frames.push(&chunk)? {
                    self.sse_event(&block).await;
                }
            }
            if let Some(block) = frames.finish() {
                self.sse_event(&block).await;
            }
        } else {
            let mut raw: Vec<u8> = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .context("reading the MCP server's reply")?
            {
                if raw.len() + chunk.len() > MAX_MESSAGE {
                    return Err(anyhow!(
                        "the MCP server's reply is larger than {} MiB",
                        MAX_MESSAGE >> 20
                    ));
                }
                raw.extend_from_slice(&chunk);
            }
            let body = String::from_utf8_lossy(&raw);
            if body.trim().is_empty() {
                return Ok(());
            }
            let v: Value =
                serde_json::from_str(&body).context("the MCP server sent invalid JSON")?;
            match v {
                Value::Array(list) => {
                    for m in list {
                        self.incoming(&m).await
                    }
                }
                m => self.incoming(&m).await,
            }
        }
        Ok(())
    }

    async fn sse_event(&self, block: &str) {
        let data: Vec<&str> = block
            .lines()
            .filter_map(|l| {
                l.strip_prefix("data:")
                    .map(|d| d.strip_prefix(' ').unwrap_or(d))
            })
            .collect();
        if data.is_empty() {
            return;
        }
        if let Ok(m) = serde_json::from_str::<Value>(&data.join("\n")) {
            self.incoming(&m).await;
        }
    }

    /// An HTTP-delivered message; a server request is answered with a POST.
    async fn incoming(&self, m: &Value) {
        if let Some(reply) = self.shared.dispatch(m) {
            let reply = reply.value().await;
            if let Ok(r) = self.http_request(&reply).send().await {
                let _ = r.bytes().await;
            }
        }
    }

    /// Every item of a paginated list (`tools/list`, `prompts/list`, …).
    async fn paged(&self, method: &str, key: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request(method, params, self.timeouts.0).await?;
            out.extend(r[key].as_array().cloned().unwrap_or_default());
            cursor = r["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    fn capability(&self, name: &str) -> bool {
        self.init
            .get()
            .is_some_and(|i| !i["capabilities"][name].is_null())
    }

    /// The server's `instructions` from `initialize`, if any.
    pub fn instructions(&self) -> Option<String> {
        self.init
            .get()
            .and_then(|i| i["instructions"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    pub fn has_resources(&self) -> bool {
        self.capability("resources")
    }

    /// Whether the server offers MCP Events.
    pub fn has_events(&self) -> bool {
        self.init
            .get()
            .is_some_and(|i| !i["capabilities"]["extensions"][events::EXTENSION].is_null())
    }

    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        Ok(self
            .paged("tools/list", "tools")
            .await?
            .iter()
            .filter_map(|t| {
                let remote = t["name"].as_str().filter(|n| !n.is_empty())?;
                Some(McpTool {
                    codemode: self.codemode,
                    name: tool_name(&self.server, remote),
                    server: self.server.clone(),
                    remote: remote.to_string(),
                    description: t["description"].as_str().unwrap_or("").to_string(),
                    schema: input_schema(t.get("inputSchema")),
                    output_schema: output_schema(t.get("outputSchema")),
                })
            })
            .collect())
    }

    /// The server's prompts; none when it does not advertise any.
    pub async fn list_prompts(&self) -> Result<Vec<McpPrompt>> {
        if !self.capability("prompts") {
            return Ok(vec![]);
        }
        Ok(self
            .paged("prompts/list", "prompts")
            .await?
            .iter()
            .filter_map(|p| {
                Some(McpPrompt {
                    server: self.server.clone(),
                    name: p["name"].as_str().filter(|n| !n.is_empty())?.to_string(),
                    description: p["description"].as_str().map(str::to_string),
                    arguments: p["arguments"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| {
                            Some(PromptArgument {
                                name: a["name"].as_str()?.to_string(),
                                description: a["description"].as_str().map(str::to_string),
                                required: a["required"].as_bool().unwrap_or(false),
                            })
                        })
                        .collect(),
                })
            })
            .collect())
    }

    /// `(resources, templates)`, each entry tagged with this server's name.
    pub async fn list_resources(&self) -> Result<(Vec<Value>, Vec<Value>)> {
        if !self.has_resources() {
            return Ok((vec![], vec![]));
        }
        let tag = |mut v: Value| {
            if let Some(o) = v.as_object_mut() {
                o.insert("server".into(), json!(self.server));
            }
            v
        };
        let resources = self.paged("resources/list", "resources").await?;
        // Templates are optional; a server without them may answer with an error.
        let templates = self
            .paged("resources/templates/list", "resourceTemplates")
            .await
            .unwrap_or_default();
        Ok((
            resources.into_iter().map(tag).collect(),
            templates.into_iter().map(tag).collect(),
        ))
    }

    pub async fn read_resource(&self, uri: &str) -> Result<Value> {
        self.request("resources/read", json!({"uri": uri}), self.timeouts.1)
            .await
    }

    pub async fn get_prompt(&self, name: &str, args: Value) -> Result<Value> {
        self.request(
            "prompts/get",
            json!({"name": name, "arguments": args}),
            self.timeouts.1,
        )
        .await
    }

    pub async fn call(&self, tool: &str, args: Value) -> Result<(String, bool)> {
        Ok(result_text(&self.call_raw(tool, args).await?))
    }

    /// The raw `tools/call` result.
    pub async fn call_raw(&self, tool: &str, args: Value) -> Result<Value> {
        self.call_in(tool, args, None).await
    }

    /// The raw `tools/call` result of a call made for `session`, whose ID
    /// the server sees as `_meta["codesmith/sessionId"]` (OpenCode sends
    /// its own `ai.opencode/sessionID`).
    pub async fn call_in(&self, tool: &str, args: Value, session: Option<&str>) -> Result<Value> {
        let mut params = json!({"name": tool, "arguments": args});
        if let Some(id) = session {
            params["_meta"] = json!({ SESSION_META: id });
        }
        self.request("tools/call", params, self.timeouts.1).await
    }

    /// Every event type the server offers (`events/list`).
    pub async fn list_events(&self) -> Result<Vec<EventType>> {
        if !self.has_events() {
            return Err(anyhow!("MCP server {} offers no events", self.server));
        }
        Ok(self
            .paged("events/list", "events")
            .await?
            .iter()
            .filter_map(EventType::from_json)
            .collect())
    }

    /// Push delivery of `name` (`events/stream`) from `cursor`, or from now.
    /// Dropping the stream cancels it.
    pub fn stream_events(
        self: &Arc<Self>,
        name: &str,
        args: Option<&Value>,
        cursor: Option<&str>,
    ) -> EventStream {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared.streams.lock().unwrap().insert(id, tx.clone());
        let mut params = json!({"name": name});
        if let Some(a) = args {
            params["arguments"] = a.clone();
        }
        if let Some(c) = cursor {
            params["cursor"] = json!(c);
        }
        let me = self.clone();
        let task = tokio::spawn(async move {
            // Heartbeats, not a request timeout, tell a quiet stream from a dead one.
            let r = me.send_request(id, "events/stream", params, None).await;
            me.shared.streams.lock().unwrap().remove(&id);
            let _ = tx.send(Err(match r {
                Ok(_) => StreamError::Disconnected("MCP event stream closed".into()),
                Err(e) => match e.downcast_ref::<RpcError>() {
                    Some(r) if events::REJECTED.contains(&r.code) => {
                        StreamError::Rejected(r.to_string())
                    }
                    _ => StreamError::Disconnected(format!("{e:#}")),
                },
            }));
        });
        EventStream::new(rx, task, Arc::downgrade(self), id)
    }
}

/// Every configured server's client, tools, prompts and status, and the
/// connections clients attached ([`Mcp::attach`]).
pub struct Mcp {
    clients: RwLock<BTreeMap<String, Arc<Client>>>,
    statuses: RwLock<BTreeMap<String, Status>>,
    /// What reconnecting a server needs: its config, the project root and
    /// the default timeouts.
    configs: RwLock<BTreeMap<String, ServerConfig>>,
    root: PathBuf,
    defaults: Timeouts,
    /// Attached connections by name, the latest last: the top one serves
    /// the name's tools, and those below take over as it closes.
    attached: Mutex<BTreeMap<String, Vec<Attached>>>,
    next_attach: AtomicU64,
    /// Serializes relisting an attached connection's tools.
    relisting: tokio::sync::Mutex<()>,
    /// Serializes starting a new session with a server that forgot one.
    renewing: tokio::sync::Mutex<()>,
    /// A server's name each time the tools under it change (attached
    /// connections only).
    changes: tokio::sync::broadcast::Sender<String>,
}

/// One attached connection in its name's stack.
#[derive(Clone)]
struct Attached {
    id: u64,
    client: Arc<Client>,
    tools: Arc<RwLock<Vec<McpTool>>>,
}

/// How long a connection taking over its name may take to list its tools
/// again (OpenCode's `FALLBACK_LIST_TIMEOUT`).
const FALLBACK_LIST_MS: u64 = 5_000;

/// An attached MCP connection, scoped to whoever holds it: dropping it
/// (or [`Attachment::detach`]) closes the connection and takes it off its
/// name's stack, as the connection closing from the other side does.
pub struct Attachment {
    mcp: std::sync::Weak<Mcp>,
    name: String,
    id: u64,
}

impl Attachment {
    /// The name the connection's tools are offered under.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn detach(self) {}
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if let Some(mcp) = self.mcp.upgrade() {
            mcp.detach(&self.name, self.id);
        }
    }
}

/// No configured server or attached connection has this name.
#[derive(Debug)]
pub struct NotFound(pub String);

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP server {:?} not found", self.0)
    }
}

impl std::error::Error for NotFound {}

/// A tool call reached a server (or client) and failed there.
#[derive(Debug)]
pub struct ToolCallError {
    pub server: String,
    pub tool: String,
    pub message: String,
}

impl std::fmt::Display for ToolCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolCallError {}

/// An HTTP server answered 404 to a request in its session: it has
/// forgotten the session, and a new one must be initialized.
#[derive(Debug)]
pub struct SessionExpired;

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the MCP server no longer knows this session")
    }
}

impl std::error::Error for SessionExpired {}

fn expired(e: &anyhow::Error) -> bool {
    e.downcast_ref::<SessionExpired>().is_some()
}

/// Connect to one server and list its tools and prompts.
async fn start(
    name: &str,
    server: &ServerConfig,
    root: &Path,
    defaults: &Timeouts,
) -> Result<Arc<Client>> {
    let c = connect(name, server, root, defaults).await?;
    let t = c.list_tools().await?;
    // A broken prompt list should not cost the server its tools.
    let p = c.list_prompts().await.unwrap_or_default();
    *c.tools.write().unwrap() = t;
    *c.prompts.write().unwrap() = p;
    Ok(c)
}

/// A server's status after starting it, and its client when it connected.
fn outcome(r: Result<Arc<Client>>) -> (Status, Option<Arc<Client>>) {
    match r {
        Ok(c) => {
            let tools = c.tools.read().unwrap().len();
            (Status::Connected { tools }, Some(c))
        }
        Err(e) if e.downcast_ref::<oauth::Unauthorized>().is_some() => (Status::NeedsAuth, None),
        Err(e) => (Status::Failed(format!("{e:#}")), None),
    }
}

impl Mcp {
    fn with(
        clients: BTreeMap<String, Arc<Client>>,
        statuses: BTreeMap<String, Status>,
        configs: BTreeMap<String, ServerConfig>,
        root: PathBuf,
        defaults: Timeouts,
    ) -> Arc<Mcp> {
        Arc::new(Mcp {
            clients: RwLock::new(clients),
            statuses: RwLock::new(statuses),
            configs: RwLock::new(configs),
            root,
            defaults,
            attached: Mutex::new(BTreeMap::new()),
            next_attach: AtomicU64::new(1),
            relisting: tokio::sync::Mutex::new(()),
            renewing: tokio::sync::Mutex::new(()),
            changes: tokio::sync::broadcast::channel(256).0,
        })
    }

    pub fn empty() -> Arc<Mcp> {
        Self::with(
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            PathBuf::new(),
            Timeouts::default(),
        )
    }

    /// An empty registry whose attached connections (and added servers)
    /// see `root` as their root.
    pub fn at(root: &Path) -> Arc<Mcp> {
        Self::with(
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            root.to_path_buf(),
            Timeouts::default(),
        )
    }

    /// Connect to every enabled server concurrently, known servers with
    /// their Code Mode defaults ([`defaults`]).
    pub async fn connect(cfg: &McpConfig, root: &Path) -> Arc<Mcp> {
        let mut cfg = cfg.clone();
        for server in cfg.servers.values_mut() {
            defaults::apply(server);
        }
        let cfg = &cfg;
        let mut clients = BTreeMap::new();
        let mut statuses = BTreeMap::new();
        let mut pending = Vec::new();
        for (name, server) in &cfg.servers {
            if server.disabled() {
                statuses.insert(name.clone(), Status::Disabled);
                continue;
            }
            let (name, server, root, defaults) = (
                name.clone(),
                server.clone(),
                root.to_path_buf(),
                cfg.timeout.clone(),
            );
            pending.push(tokio::spawn(async move {
                let r = start(&name, &server, &root, &defaults).await;
                (name, r)
            }));
        }
        for p in pending {
            let Ok((name, r)) = p.await else { continue };
            let (status, client) = outcome(r);
            statuses.insert(name.clone(), status);
            if let Some(c) = client {
                clients.insert(name, c);
            }
        }
        Self::with(
            clients,
            statuses,
            cfg.servers.clone(),
            root.to_path_buf(),
            cfg.timeout.clone(),
        )
    }

    /// Each configured server's status (attached connections are not
    /// servers and never show here).
    pub fn statuses(&self) -> BTreeMap<String, Status> {
        self.statuses.read().unwrap().clone()
    }

    /// Hear the name of each server whose tools change: an attached
    /// connection arriving, leaving, or listing new tools.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    fn changed(&self, server: &str) {
        let _ = self.changes.send(server.to_string());
    }

    /// Configure (or reconfigure) server `name` and connect it now, in
    /// place of any connection it had: its status after.
    pub async fn add(&self, name: &str, mut cfg: ServerConfig) -> Status {
        defaults::apply(&mut cfg);
        self.configs.write().unwrap().insert(name.to_string(), cfg);
        self.restart(name)
            .await
            .unwrap_or_else(|| Status::Failed("not configured".into()))
    }

    /// Forget configured server `name`, stopping it. Attached connections
    /// under the same name stay.
    pub fn remove(&self, name: &str) {
        self.configs.write().unwrap().remove(name);
        self.statuses.write().unwrap().remove(name);
        self.clients.write().unwrap().remove(name);
    }

    /// Connect configured server `name` again (after a sign-in, say),
    /// replacing its connection: its new status, or `None` when no server
    /// has that name.
    pub async fn restart(&self, name: &str) -> Option<Status> {
        let cfg = self.configs.read().unwrap().get(name).cloned()?;
        let (status, client) = if cfg.disabled() {
            (Status::Disabled, None)
        } else {
            outcome(start(name, &cfg, &self.root, &self.defaults).await)
        };
        {
            let mut clients = self.clients.write().unwrap();
            match client {
                Some(c) => clients.insert(name.to_string(), c),
                None => clients.remove(name),
            };
        }
        self.statuses
            .write()
            .unwrap()
            .insert(name.to_string(), status.clone());
        Some(status)
    }

    /// Attach a client-hosted MCP server, reached over `reader`/`writer`
    /// (newline-delimited JSON-RPC), under `name`, until the returned
    /// [`Attachment`] is dropped or the connection closes.
    ///
    /// Attaching under a name already attached stacks: the newest live
    /// connection serves the name's tools, and the one below takes over
    /// when it closes. An attached connection's tools shadow a configured
    /// server's tools of the same name; it never shows among the servers'
    /// statuses. Its tools are called like any MCP server's, through the
    /// same permission rules and plugin hooks, and what it sends back is
    /// tool output like any other.
    pub async fn attach<R, W>(
        self: &Arc<Self>,
        name: &str,
        reader: R,
        writer: W,
    ) -> Result<Attachment>
    where
        R: tokio::io::AsyncRead + Send + Unpin + 'static,
        W: tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let shared = Arc::new(Shared {
            roots: vec![self.root.display().to_string()],
            server: name.to_string(),
            ..Default::default()
        });
        let client = Arc::new(Client {
            server: name.to_string(),
            transport: Transport::Stream {
                writer: tokio::sync::Mutex::new(Box::new(writer)),
                _child: None,
            },
            shared,
            next: AtomicU64::new(1),
            timeouts: (
                self.defaults.catalog.unwrap_or(DEFAULT_CATALOG_MS),
                self.defaults.execution.unwrap_or(DEFAULT_EXECUTION_MS),
            ),
            codemode: true,
            init: OnceLock::new(),
            tools: RwLock::new(vec![]),
            prompts: RwLock::new(vec![]),
            reader: Mutex::new(None),
        });
        read_stream(&client, reader);
        // A failure drops the client, which closes the connection.
        handshake(&client, self.defaults.startup.unwrap_or(DEFAULT_STARTUP_MS))
            .await
            .with_context(|| format!("attaching MCP client {name}"))?;
        let tools = client
            .list_tools()
            .await
            .with_context(|| format!("listing the tools of MCP client {name}"))?;
        let id = self.next_attach.fetch_add(1, Ordering::SeqCst);
        let entry = Attached {
            id,
            client: client.clone(),
            tools: Arc::new(RwLock::new(tools)),
        };
        self.attached
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .push(entry.clone());
        let attachment = Attachment {
            mcp: Arc::downgrade(self),
            name: name.to_string(),
            id,
        };
        let (weak, owned) = (Arc::downgrade(self), name.to_string());
        *client.shared.on_close.lock().unwrap() = Some(Box::new(move || {
            if let Some(mcp) = weak.upgrade() {
                mcp.detach(&owned, id);
            }
        }));
        if client.exited().is_some() {
            drop(attachment);
            return Err(anyhow!("attaching MCP client {name}: Connection closed"));
        }
        let (weak, owned) = (Arc::downgrade(self), name.to_string());
        // Weak, so the client's own hook never keeps it open.
        let (conn, listed_tools) = (Arc::downgrade(&client), entry.tools.clone());
        *client.shared.on_tools_changed.lock().unwrap() = Some(Arc::new(move || {
            let (Some(mcp), Ok(rt)) = (weak.upgrade(), tokio::runtime::Handle::try_current())
            else {
                return;
            };
            let (name, conn, tools) = (owned.clone(), conn.clone(), listed_tools.clone());
            rt.spawn(async move {
                let _one = mcp.relisting.lock().await;
                if mcp.top(&name).map(|t| t.id) != Some(id) {
                    return;
                }
                let Some(conn) = conn.upgrade() else { return };
                let listed = tokio::time::timeout(
                    Duration::from_millis(FALLBACK_LIST_MS),
                    conn.list_tools(),
                )
                .await;
                if let Ok(Ok(listed)) = listed {
                    *tools.write().unwrap() = listed;
                    mcp.changed(&name);
                }
            });
        }));
        self.changed(name);
        Ok(attachment)
    }

    /// The connection serving attached name `name`.
    fn top(&self, name: &str) -> Option<Attached> {
        self.attached
            .lock()
            .unwrap()
            .get(name)
            .and_then(|s| s.last().cloned())
    }

    /// Take attached connection `id` off `name`'s stack and close it. When
    /// it served the name, the one below takes over and lists its tools
    /// again (keeping those it had if that fails).
    fn detach(self: &Arc<Self>, name: &str, id: u64) {
        let (entry, next, was_top) = {
            let mut map = self.attached.lock().unwrap();
            let Some(stack) = map.get_mut(name) else {
                return;
            };
            let Some(at) = stack.iter().position(|e| e.id == id) else {
                return;
            };
            let was_top = at + 1 == stack.len();
            let entry = stack.remove(at);
            let next = stack.last().cloned();
            if next.is_none() {
                map.remove(name);
            }
            (entry, next, was_top)
        };
        *entry.client.shared.on_close.lock().unwrap() = None;
        *entry.client.shared.on_tools_changed.lock().unwrap() = None;
        let gone = entry.tools.read().unwrap().clone();
        drop(entry);
        let Some(next) = next else {
            self.changed(name);
            return;
        };
        // Closing a connection below the top changes nothing visible.
        if !was_top {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            if gone != *next.tools.read().unwrap() {
                self.changed(name);
            }
            return;
        };
        let (mcp, name) = (self.clone(), name.to_string());
        rt.spawn(async move {
            let _one = mcp.relisting.lock().await;
            let before = next.tools.read().unwrap().clone();
            let listed = tokio::time::timeout(
                Duration::from_millis(FALLBACK_LIST_MS),
                next.client.list_tools(),
            )
            .await;
            if let Ok(Ok(tools)) = listed {
                *next.tools.write().unwrap() = tools;
            }
            let now = next.tools.read().unwrap().clone();
            // Readers may have seen the stored tools meanwhile: compare both.
            if gone != now || before != now {
                mcp.changed(&name);
            }
        });
    }

    /// Call tool `tool` on server `server` (its own name there) for
    /// `session`: the raw result. An attached connection owns the tools it
    /// lists, even when a configured server shares its name, and owns the
    /// name outright when no configured server has it.
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        session: Option<&str>,
    ) -> Result<Value> {
        self.call_tool_asking(server, tool, args, session, None)
            .await
    }

    async fn call_tool_asking(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        session: Option<&str>,
        asker: Option<Arc<dyn crate::question::Questioner>>,
    ) -> Result<Value> {
        let failed = |owner: &str, e: anyhow::Error| -> anyhow::Error {
            ToolCallError {
                server: server.to_string(),
                tool: tool.to_string(),
                message: format!("MCP tool {tool:?} on {owner} {server:?} failed: {e:#}"),
            }
            .into()
        };
        if let Some(top) = self.top(server) {
            let configured = self.configs.read().unwrap().contains_key(server);
            let owns = top.tools.read().unwrap().iter().any(|t| t.remote == tool);
            if owns || !configured {
                let _asking = asker.map(|a| Asking::new(&top.client.shared, a));
                return top
                    .client
                    .call_in(tool, args, session)
                    .await
                    .map_err(|e| failed("client", e));
            }
        }
        if !self.configs.read().unwrap().contains_key(server)
            && !self.clients.read().unwrap().contains_key(server)
        {
            return Err(NotFound(server.to_string()).into());
        }
        let c = self.connected_or_restart(server).await?;
        let first = {
            let _asking = asker.clone().map(|a| Asking::new(&c.shared, a));
            c.call_in(tool, args.clone(), session).await
        };
        match first {
            // The server forgot the session before it took the call:
            // initialize a new one and send the call once more.
            Err(e) if expired(&e) => {
                let c = self.renew(server, &c).await?;
                let _asking = asker.map(|a| Asking::new(&c.shared, a));
                c.call_in(tool, args, session).await
            }
            r => r,
        }
    }

    /// Start a new session with `server` in place of `old`, whose session
    /// the server forgot: connect it again (initialize, list its tools)
    /// unless a concurrent call already did.
    async fn renew(&self, server: &str, old: &Arc<Client>) -> Result<Arc<Client>> {
        let _one = self.renewing.lock().await;
        if let Some(c) = self.clients.read().unwrap().get(server) {
            if !Arc::ptr_eq(c, old) {
                return Ok(c.clone());
            }
        }
        let cfg = self
            .configs
            .read()
            .unwrap()
            .get(server)
            .cloned()
            .ok_or_else(|| anyhow!("MCP server {server} is not configured"))?;
        let (status, client) = outcome(start(server, &cfg, &self.root, &self.defaults).await);
        self.statuses
            .write()
            .unwrap()
            .insert(server.to_string(), status.clone());
        match client {
            Some(c) => {
                self.clients
                    .write()
                    .unwrap()
                    .insert(server.to_string(), c.clone());
                Ok(c)
            }
            None => {
                self.clients.write().unwrap().remove(server);
                Err(anyhow!(
                    "MCP server {server} forgot its session, and starting a new one failed: {}",
                    match status {
                        Status::Failed(e) => e,
                        Status::NeedsAuth => "it needs a login".into(),
                        _ => "it did not connect".into(),
                    }
                ))
            }
        }
    }

    /// A server's client while it is connected (a stdio server that exited
    /// is not).
    pub fn client(&self, server: &str) -> Option<Arc<Client>> {
        self.clients
            .read()
            .unwrap()
            .get(server)
            .filter(|c| c.exited().is_none())
            .cloned()
    }

    /// Live clients, by server name.
    fn live(&self) -> Vec<(String, Arc<Client>)> {
        self.clients
            .read()
            .unwrap()
            .iter()
            .filter(|(_, c)| c.exited().is_none())
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect()
    }

    /// Start a server again whose process exited, once per exit. Its tools
    /// come back if it starts.
    async fn reconnect(&self, server: &str) -> Option<Arc<Client>> {
        let old = self.clients.read().unwrap().get(server).cloned()?;
        if old.exited().is_none() {
            return Some(old);
        }
        if old.shared.reconnect_tried.swap(true, Ordering::SeqCst) {
            return None;
        }
        let cfg = self.configs.read().unwrap().get(server).cloned()?;
        match start(server, &cfg, &self.root, &self.defaults).await {
            Ok(c) => {
                self.clients
                    .write()
                    .unwrap()
                    .insert(server.to_string(), c.clone());
                Some(c)
            }
            Err(e) => {
                *old.shared.exited.lock().unwrap() =
                    Some(format!("it exited, and restarting it failed: {e:#}"));
                None
            }
        }
    }

    /// Try once to restart every server whose process exited.
    async fn reconnect_exited(&self) {
        let exited: Vec<String> = self
            .clients
            .read()
            .unwrap()
            .iter()
            .filter(|(_, c)| {
                c.exited().is_some() && !c.shared.reconnect_tried.load(Ordering::SeqCst)
            })
            .map(|(n, _)| n.clone())
            .collect();
        for s in exited {
            self.reconnect(&s).await;
        }
    }

    /// Every server's tools (with whether the server is live), named by
    /// [`disambiguate`] over all of them, exited servers included, so a
    /// name means the same tool whichever servers are up.
    fn named_tools(&self) -> Vec<(bool, McpTool)> {
        // The top attached connection's tools, which shadow a configured
        // server's tool of the same name.
        let mut attached: Vec<McpTool> = self
            .attached
            .lock()
            .unwrap()
            .values()
            .filter_map(|s| s.last())
            .flat_map(|e| e.tools.read().unwrap().clone())
            .collect();
        attached.sort_by(|a, b| (&a.server, &a.remote).cmp(&(&b.server, &b.remote)));
        let shadowed = |t: &McpTool| {
            attached
                .iter()
                .any(|a| a.server == t.server && a.remote == t.remote)
        };
        let clients = self.clients.read().unwrap();
        let mut both: Vec<(bool, McpTool)> = Vec::new();
        for c in clients.values() {
            for t in c.tools.read().unwrap().iter().filter(|t| !shadowed(t)) {
                both.push((c.exited().is_none(), t.clone()));
            }
        }
        both.extend(attached.into_iter().map(|t| (true, t)));
        // By server, each server's tools in their own order.
        both.sort_by(|a, b| a.1.server.cmp(&b.1.server));
        let live: Vec<bool> = both.iter().map(|(l, _)| *l).collect();
        let named = disambiguate(both.into_iter().map(|(_, t)| t).collect());
        named.into_iter().map(|(i, t)| (live[i], t)).collect()
    }

    /// Every connected server's tools, by server name.
    pub fn tools(&self) -> Vec<McpTool> {
        self.named_tools()
            .into_iter()
            .filter(|(live, _)| *live)
            .map(|(_, t)| t)
            .collect()
    }

    /// A tool by the name it was offered under, from any server that
    /// offered it (including one that has since exited).
    pub fn tool(&self, name: &str) -> Option<McpTool> {
        self.named_tools()
            .into_iter()
            .map(|(_, t)| t)
            .find(|t| t.name == name)
    }

    /// Every connected server's prompts, by server then name.
    pub fn prompts(&self) -> Vec<McpPrompt> {
        let mut all: Vec<McpPrompt> = self
            .live()
            .into_iter()
            .flat_map(|(_, c)| c.prompts.read().unwrap().clone())
            .collect();
        all.sort_by(|a, b| (&a.server, &a.name).cmp(&(&b.server, &b.name)));
        all
    }

    /// (server, instructions) for every connected server that gave any.
    pub fn instructions(&self) -> Vec<(String, String)> {
        self.live()
            .into_iter()
            .filter_map(|(n, c)| c.instructions().map(|i| (n, i)))
            .collect()
    }

    /// Connected servers that advertise resources.
    pub fn resource_servers(&self) -> Vec<String> {
        self.live()
            .into_iter()
            .filter(|(_, c)| c.has_resources())
            .map(|(n, _)| n)
            .collect()
    }

    /// Before each model step: restart (once) a server whose process
    /// exited, and relist whatever a server announced as changed
    /// (`list_changed`).
    pub async fn refresh(&self) {
        self.reconnect_exited().await;
        for (_, c) in self.live() {
            if c.shared.tools_stale.swap(false, Ordering::SeqCst) {
                if let Ok(t) = c.list_tools().await {
                    *c.tools.write().unwrap() = t;
                }
            }
            if c.shared.prompts_stale.swap(false, Ordering::SeqCst) {
                if let Ok(p) = c.list_prompts().await {
                    *c.prompts.write().unwrap() = p;
                }
            }
        }
    }

    /// Call a tool: its text (media noted) and whether it failed.
    pub async fn call(&self, name: &str, args: Value) -> Result<(String, bool)> {
        Ok(result_text(&self.call_raw(name, args).await?))
    }

    /// Call a tool from a Code Mode program: its text (media noted), or,
    /// for a tool that declares an output schema, its structured content as
    /// the program's value ([`structured_result`]); and whether it failed.
    pub async fn call_code(&self, name: &str, args: Value) -> Result<(String, bool)> {
        let declared = self.tool(name).is_some_and(|t| t.output_schema.is_some());
        let raw = self.call_raw(name, args).await?;
        Ok(match structured_result(&raw, declared) {
            Some(v) => (format!("{}{v}", crate::codemode::JSON_RESULT), false),
            None => result_text(&raw),
        })
    }

    /// Call a tool: its text, whether it failed, and its images and PDFs.
    pub async fn call_media(
        &self,
        name: &str,
        args: Value,
    ) -> Result<(String, bool, Vec<codesmith_ai::Media>)> {
        Ok(result_content(&self.call_raw(name, args).await?))
    }

    async fn call_raw(&self, name: &str, args: Value) -> Result<Value> {
        self.call_raw_asking(name, args, None).await
    }

    /// [`Mcp::call_media`], with `asker` answering the elicitations the
    /// server sends while the call runs.
    pub async fn call_media_asking(
        &self,
        name: &str,
        args: Value,
        asker: Option<Arc<dyn crate::question::Questioner>>,
    ) -> Result<(String, bool, Vec<codesmith_ai::Media>)> {
        Ok(result_content(
            &self.call_raw_asking(name, args, asker).await?,
        ))
    }

    /// The raw `tools/call` result of [`Mcp::call_media_asking`].
    pub async fn call_raw_asking(
        &self,
        name: &str,
        args: Value,
        asker: Option<Arc<dyn crate::question::Questioner>>,
    ) -> Result<Value> {
        self.call_raw_for(name, args, asker, None).await
    }

    /// [`Mcp::call_raw_asking`] for `session`, whose ID the server sees
    /// ([`SESSION_META`]).
    pub async fn call_raw_for(
        &self,
        name: &str,
        args: Value,
        asker: Option<Arc<dyn crate::question::Questioner>>,
        session: Option<&str>,
    ) -> Result<Value> {
        let t = self
            .tool(name)
            .ok_or_else(|| anyhow!("no MCP tool {name}"))?;
        self.call_tool_asking(&t.server, &t.remote, args, session, asker)
            .await
    }

    /// [`Mcp::call_media_asking`] for `session`.
    pub async fn call_media_for(
        &self,
        name: &str,
        args: Value,
        asker: Option<Arc<dyn crate::question::Questioner>>,
        session: Option<&str>,
    ) -> Result<(String, bool, Vec<codesmith_ai::Media>)> {
        Ok(result_content(
            &self.call_raw_for(name, args, asker, session).await?,
        ))
    }

    /// The server's client, restarting it once if its process exited.
    async fn connected_or_restart(&self, server: &str) -> Result<Arc<Client>> {
        if let Some(c) = self.client(server) {
            return Ok(c);
        }
        let old = self.clients.read().unwrap().get(server).cloned();
        let Some(old) = old else {
            return Err(anyhow!("MCP server {server} is not connected"));
        };
        if let Some(c) = self.reconnect(server).await {
            return Ok(c);
        }
        Err(anyhow!(
            "MCP server {server} is unavailable: {}. Its tools can't be used until CodeSmith reconnects it (restart the session).",
            old.exited().unwrap_or_else(|| "it exited".into())
        ))
    }

    fn connected(&self, server: &str) -> Result<Arc<Client>> {
        self.client(server)
            .ok_or_else(|| anyhow!("MCP server {server:?} is not connected"))
    }

    /// `{resources, templates}` from one server, or every server (skipping
    /// any that fail).
    pub async fn list_resources(&self, server: Option<&str>) -> Result<Value> {
        let (mut resources, mut templates) = (vec![], vec![]);
        match server {
            Some(s) => {
                let (r, t) = self.connected(s)?.list_resources().await?;
                resources.extend(r);
                templates.extend(t);
            }
            None => {
                for (_, c) in self.live() {
                    if let Ok((r, t)) = c.list_resources().await {
                        resources.extend(r);
                        templates.extend(t);
                    }
                }
            }
        }
        Ok(json!({"resources": resources, "templates": templates}))
    }

    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<String> {
        self.read_resource_in(server, uri, None).await
    }

    /// [`Mcp::read_resource`], saving binary content to files in `dir`.
    pub async fn read_resource_in(
        &self,
        server: &str,
        uri: &str,
        dir: Option<&Path>,
    ) -> Result<String> {
        let c = self.connected(server)?;
        if !c.has_resources() {
            return Err(anyhow!("MCP server {server:?} does not expose resources"));
        }
        let read = match c.read_resource(uri).await {
            Err(e) if expired(&e) => self.renew(server, &c).await?.read_resource(uri).await,
            r => r,
        };
        Ok(resource_text_in(&read?, dir))
    }

    /// The servers' prompts as slash commands named `server:prompt`.
    pub fn commands(&self) -> BTreeMap<String, crate::command::Command> {
        self.prompts()
            .into_iter()
            .map(|p| {
                let args: Vec<String> = p
                    .arguments
                    .iter()
                    .map(|a| {
                        if a.required {
                            format!("<{}>", a.name)
                        } else {
                            format!("[{}]", a.name)
                        }
                    })
                    .collect();
                let mut description = p
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("{} prompt from MCP server {}", p.name, p.server));
                if !args.is_empty() {
                    description = format!("{description} {}", args.join(" "));
                }
                (
                    command_name(&p.server, &p.name),
                    crate::command::Command {
                        description: Some(description),
                        mcp: Some((p.server.clone(), p.name.clone())),
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    /// Run a server's prompt with positional arguments, as OpenCode does:
    /// the arguments (quoted like a shell) fill the prompt's arguments in order.
    pub async fn prompt_text(&self, server: &str, name: &str, args: &str) -> Result<String> {
        let c = self.connected(server)?;
        let declared = c
            .prompts
            .read()
            .unwrap()
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.arguments.clone())
            .ok_or_else(|| anyhow!("MCP prompt not found: {server}:{name}"))?;
        let given = crate::command::parse_args(args);
        let map: serde_json::Map<String, Value> = declared
            .iter()
            .enumerate()
            .map(|(i, a)| {
                (
                    a.name.clone(),
                    json!(given.get(i).cloned().unwrap_or_default()),
                )
            })
            .collect();
        Ok(prompt_result_text(
            &c.get_prompt(name, Value::Object(map)).await?,
        ))
    }

    /// Configured servers that are connected and offer MCP Events.
    pub fn event_servers(&self) -> Vec<String> {
        self.live()
            .into_iter()
            .filter(|(_, c)| c.has_events())
            .map(|(n, _)| n)
            .collect()
    }

    /// `{events: [{server, event, description, args, data}]}`: the events
    /// `server` pushes, shaped like the event trigger they feed (OpenCode's
    /// `list_mcp_events`). Descriptions are cleaned and cut short; poll-only
    /// events are left out, since automations can use only pushed ones.
    pub async fn list_push_events(&self, server: &str) -> Result<Value> {
        let c = self
            .client(server)
            .filter(|c| c.has_events())
            .ok_or_else(|| {
                anyhow!("MCP server \"{server}\" is not connected or sends no events")
            })?;
        let events: Vec<Value> = c
            .list_events()
            .await?
            .into_iter()
            .filter(|t| t.delivery.iter().any(|d| d == "push"))
            .map(|t| {
                let mut e = json!({"server": server, "event": t.name});
                if let Some(d) = t.description {
                    e["description"] = json!(oauth::clean(&d, MAX_EVENT_DESCRIPTION));
                }
                if let Some(a) = t.input_schema {
                    e["args"] = a;
                }
                if let Some(d) = t.payload_schema {
                    e["data"] = d;
                }
                e
            })
            .collect();
        Ok(json!({ "events": events }))
    }

    /// Check that `server` offers `event` for push delivery and that `args`
    /// fit its input schema, as OpenCode does before it creates a watch.
    pub async fn check_event(
        &self,
        server: &str,
        event: &str,
        args: Option<&Value>,
    ) -> Result<EventType> {
        let c = self
            .client(server)
            .filter(|c| c.has_events())
            .ok_or_else(|| anyhow!("MCP server {server} is not connected or offers no events"))?;
        let t = c
            .list_events()
            .await?
            .into_iter()
            .find(|t| t.name == event && t.delivery.iter().any(|d| d == "push"))
            .ok_or_else(|| anyhow!("No push event {event} on {server}"))?;
        let empty = json!({});
        if let Some(schema) = &t.input_schema {
            if !events::matches(schema, args.unwrap_or(&empty)) {
                return Err(anyhow!("Event arguments do not match inputSchema"));
            }
        }
        Ok(t)
    }
}

/// Start (or reach) one server and complete the initialize handshake. The
/// stdio reader holds only a weak reference, so dropping the client stops
/// the server.
async fn connect(
    name: &str,
    cfg: &ServerConfig,
    root: &Path,
    defaults: &Timeouts,
) -> Result<Arc<Client>> {
    let t = cfg.timeouts();
    let pick = |f: fn(&Timeouts) -> Option<u64>, d: u64| t.and_then(f).or(f(defaults)).unwrap_or(d);
    let startup = pick(|t| t.startup, DEFAULT_STARTUP_MS);
    let timeouts = (
        pick(|t| t.catalog, DEFAULT_CATALOG_MS),
        pick(|t| t.execution, DEFAULT_EXECUTION_MS),
    );
    let log_dir = crate::paths::data_dir().join("log");
    let log = log_dir.join(format!("mcp-{}.log", sanitize(name)));
    let shared = Arc::new(Shared {
        roots: vec![root.display().to_string()],
        log: Some(log.clone()),
        server: name.to_string(),
        ..Default::default()
    });
    let make = |transport| Client {
        server: name.to_string(),
        transport,
        shared: shared.clone(),
        next: AtomicU64::new(1),
        timeouts,
        codemode: cfg.codemode(),
        init: OnceLock::new(),
        tools: RwLock::new(vec![]),
        prompts: RwLock::new(vec![]),
        reader: Mutex::new(None),
    };
    let client = match cfg {
        ServerConfig::Remote {
            url,
            headers,
            oauth: setting,
            ..
        } => {
            let http = reqwest::Client::new();
            let has_auth = headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"));
            let store = oauth::store_path();
            let limit = Duration::from_millis(startup).min(oauth::REFRESH_TIMEOUT);
            let mut token = None;
            if !has_auth && oauth::Setting::enabled(setting.as_ref()) {
                token = oauth::token_within(&http, &store, name, url, limit).await;
            }
            let mut retried = false;
            loop {
                let mut headers = headers.clone();
                if let Some(tok) = &token {
                    headers.insert("Authorization".into(), format!("Bearer {tok}"));
                }
                let client = Arc::new(make(Transport::Http {
                    client: http.clone(),
                    url: url.clone(),
                    headers,
                    session: Mutex::new(None),
                }));
                match handshake(&client, startup).await {
                    Ok(()) => return Ok(client),
                    // The server refused a saved token it should have
                    // taken (revoked, or rotated early): refresh it once
                    // and try again with the new one. A server wanting
                    // more scope needs a new login instead.
                    Err(e)
                        if !retried
                            && e.downcast_ref::<oauth::Unauthorized>()
                                .is_some_and(|u| !u.insufficient_scope()) =>
                    {
                        let Some(sent) = token.take() else {
                            return Err(e);
                        };
                        retried = true;
                        match oauth::refresh_refused(&store, name, url, &sent, limit).await {
                            Some(new) => token = Some(new),
                            None => return Err(e),
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        ServerConfig::Local {
            command,
            cwd,
            environment,
            ..
        } => {
            let (prog, args) = command
                .split_first()
                .ok_or_else(|| anyhow!("empty command"))?;
            let dir = resolve_cwd(root, cwd.as_deref());
            crate::paths::ensure_parent(&log_dir)?;
            crate::paths::private_dir(&log_dir)?;
            let stderr = crate::paths::create_private(&log)?;
            // The host's environment without its credentials (see
            // `crate::child_env`), then exactly the configured `environment`.
            let mut cmd = crate::child_env::scrubbed_tokio(prog);
            cmd.env_remove(crate::SERVER_TOKEN_VAR)
                .args(args)
                .current_dir(dir)
                .envs(environment)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(stderr))
                .kill_on_drop(true);
            #[cfg(unix)]
            cmd.process_group(0);
            let mut child = cmd.spawn().with_context(|| format!("starting {prog}"))?;
            let group = if cfg!(unix) {
                child.id().and_then(|p| i32::try_from(p).ok())
            } else {
                None
            };
            let stdout = child.stdout.take().unwrap();
            let stdin = child.stdin.take().unwrap();
            let client = Arc::new(make(Transport::Stream {
                writer: tokio::sync::Mutex::new(Box::new(stdin)),
                _child: Some(Process { child, group }),
            }));
            read_stream(&client, stdout);
            client
        }
    };
    handshake(&client, startup).await?;
    Ok(client)
}

/// A local server's working directory: `cwd` as configured, relative to
/// the project, with `~` the home folder; the project without one.
pub fn resolve_cwd(root: &Path, cwd: Option<&str>) -> PathBuf {
    let home = || dirs::home_dir().unwrap_or_default();
    match cwd {
        None => root.to_path_buf(),
        Some("~") => home(),
        Some(c) => match c.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None if Path::new(c).is_absolute() => c.into(),
            None => root.join(c),
        },
    }
}

/// Read a stream transport's messages on a task: replies go to their
/// requests and server requests are answered. The task holds only a weak
/// reference to the client and ends with it (or when the stream ends,
/// which fails whatever still waits on the connection).
fn read_stream<R: tokio::io::AsyncRead + Send + Unpin + 'static>(client: &Arc<Client>, reader: R) {
    let weak = Arc::downgrade(client);
    let shared = client.shared.clone();
    let task = tokio::spawn(async move {
        let mut out = BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match read_line_capped(&mut out, &mut buf, MAX_MESSAGE).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    // Pending requests fail with this reason; the
                    // process is stopped when the client is dropped.
                    shared.close(&format!("{e:#}"));
                    break;
                }
            }
            // Stray output (a banner, broken UTF-8) is skipped, not fatal.
            let line = String::from_utf8_lossy(&buf);
            let Ok(m) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let messages = match m {
                Value::Array(list) => list,
                m => vec![m],
            };
            for m in messages {
                let Some(reply) = shared.dispatch(&m) else {
                    continue;
                };
                match reply {
                    Answer::Now(reply) => {
                        if let Some(c) = weak.upgrade() {
                            let _ = c.send(&reply).await;
                        }
                    }
                    // Answered once the user has: reading goes on meanwhile.
                    later => {
                        let weak = weak.clone();
                        tokio::spawn(async move {
                            let reply = later.value().await;
                            if let Some(c) = weak.upgrade() {
                                let _ = c.send(&reply).await;
                            }
                        });
                    }
                }
            }
        }
        shared.close("the MCP server exited");
    });
    *client.reader.lock().unwrap() = Some(task.abort_handle());
}

/// The `initialize` handshake.
async fn handshake(client: &Client, startup: u64) -> Result<()> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"roots": {"listChanged": false}, "elicitation": {"form": {}, "url": {}}, "extensions": {events::EXTENSION: {}}},
        "clientInfo": {"name": "codesmith", "version": env!("CARGO_PKG_VERSION")}
    });
    let init = client.request("initialize", params, startup).await?;
    let _ = client.init.set(init);
    client.notify("notifications/initialized", None).await
}

/// Read one line (through its `\n`) into `buf`: `false` at the end of the
/// stream. A line longer than `max` bytes is an error, so a stdio server
/// can't make CodeSmith buffer without bound.
async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> Result<bool> {
    loop {
        let avail = r.fill_buf().await?;
        if avail.is_empty() {
            return Ok(!buf.is_empty());
        }
        let (n, done) = match avail.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (avail.len(), false),
        };
        if buf.len() + n - usize::from(done) > max {
            return Err(anyhow!(
                "the MCP server sent a message larger than {} MiB",
                max >> 20
            ));
        }
        buf.extend_from_slice(&avail[..n]);
        r.consume(n);
        if done {
            return Ok(true);
        }
    }
}

/// Splits a Server-Sent Events byte stream into event blocks, refusing to
/// buffer more than `max` bytes of a single unfinished event.
struct SseFrames {
    buf: Vec<u8>,
    max: usize,
}

impl SseFrames {
    fn new(max: usize) -> Self {
        SseFrames {
            buf: Vec::new(),
            max,
        }
    }

    /// Add a chunk and return the events it completes.
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>> {
        // Only the unfinished tail can hold a separator split across chunks.
        let from = self.buf.len().saturating_sub(1);
        self.buf.extend(chunk.iter().filter(|b| **b != b'\r'));
        let mut out = Vec::new();
        let mut start = 0;
        let mut i = from;
        while let Some(off) = self.buf[i..].windows(2).position(|w| w == b"\n\n") {
            let end = i + off + 2;
            out.push(String::from_utf8_lossy(&self.buf[start..end]).into_owned());
            start = end;
            i = end;
        }
        self.buf.drain(..start);
        if self.buf.len() > self.max {
            self.buf = Vec::new();
            return Err(anyhow!(
                "the MCP server sent an event larger than {} MiB",
                self.max >> 20
            ));
        }
        Ok(out)
    }

    /// Whatever is left when the stream ends.
    fn finish(self) -> Option<String> {
        (!self.buf.is_empty()).then(|| String::from_utf8_lossy(&self.buf).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_input_schemas_are_replaced_with_an_open_object() {
        let open = json!({"type": "object", "properties": {}});
        let small = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        assert_eq!(input_schema(Some(&small)), small);
        assert_eq!(input_schema(None), open);
        assert_eq!(input_schema(Some(&json!("x"))), open);
        let big = json!({"type": "object", "description": "x".repeat(MAX_INPUT_SCHEMA)});
        assert_eq!(input_schema(Some(&big)), open);
    }

    #[test]
    fn sse_frames_split_and_cap() {
        let mut f = SseFrames::new(64);
        assert!(f.push(b"data: 1\r\n").unwrap().is_empty());
        assert_eq!(
            f.push(b"\r\ndata: 2\n\nda").unwrap(),
            ["data: 1\n\n", "data: 2\n\n"]
        );
        assert_eq!(f.push(b"ta: 3\n").unwrap(), Vec::<String>::new());
        assert_eq!(f.push(b"\n").unwrap(), ["data: 3\n\n"]);
        assert!(f.push(b"data: tail").unwrap().is_empty());
        assert_eq!(f.finish().as_deref(), Some("data: tail"));

        // Many small complete events are fine; one oversized event is not.
        let mut f = SseFrames::new(64);
        for _ in 0..100 {
            assert_eq!(f.push(b"data: 0123456789\n\n").unwrap().len(), 1);
        }
        assert!(f.push(&[b'x'; 40]).unwrap().is_empty());
        let e = f.push(&[b'x'; 40]).unwrap_err();
        assert!(e.to_string().contains("larger than"), "{e}");
    }

    #[test]
    fn tool_names() {
        assert_eq!(tool_name("github", "create_issue"), "github_create_issue");
        assert_eq!(
            tool_name("my server", "do.thing/now"),
            "my_server_do_thing_now"
        );
    }

    #[tokio::test]
    async fn stdio_lines_are_capped() {
        let mut r = BufReader::with_capacity(4, &b"ab\ncdef\n0123456789\ntail"[..]);
        let mut buf = Vec::new();
        assert!(read_line_capped(&mut r, &mut buf, 4).await.unwrap());
        assert_eq!(buf, b"ab\n");
        buf.clear();
        let ok = read_line_capped(&mut r, &mut buf, 4).await.unwrap();
        assert!(ok, "exactly the cap, plus its newline");
        assert_eq!(buf, b"cdef\n");
        buf.clear();
        let e = read_line_capped(&mut r, &mut buf, 4).await.unwrap_err();
        assert!(e.to_string().contains("larger than"), "{e}");
        assert!(buf.len() <= 4, "stopped before buffering it all");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stdio_server_sending_a_huge_line_fails_cleanly() {
        let script = "head -c 17000000 /dev/zero | tr '\\000' a; echo; sleep 10";
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "huge": {"type": "local", "command": ["/bin/sh", "-c", script]},
        }}))
        .unwrap();
        let started = std::time::Instant::now();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        match &mcp.statuses()["huge"] {
            Status::Failed(e) => assert!(e.contains("larger than 16 MiB"), "{e}"),
            s => panic!("{s:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "it did not wait for a timeout"
        );
    }

    #[test]
    fn tool_names_stay_within_provider_limits() {
        let long = "s".repeat(40);
        let tool = "t".repeat(40);
        let a = tool_name(&long, &tool);
        let b = tool_name(&long, &format!("{tool}x"));
        assert_eq!(a.len(), MAX_TOOL_NAME, "{a}");
        assert_eq!(b.len(), MAX_TOOL_NAME, "{b}");
        assert_ne!(a, b, "cut names stay distinct");
        assert_eq!(a, tool_name(&long, &tool), "and stable");
        let pattern = tools_pattern(&long);
        for n in [&a, &b] {
            assert!(
                crate::wildcard::matches(n, &pattern, false),
                "{pattern} {n}"
            );
        }
        assert!(!crate::wildcard::matches(&a, &tools_pattern("x"), false));
        // Colliding long names get a suffix and still fit.
        let t = |server: &str, remote: &str| McpTool {
            name: tool_name(server, remote),
            server: server.into(),
            remote: remote.into(),
            description: String::new(),
            schema: json!({}),
            output_schema: None,
            codemode: false,
        };
        let base = "a".repeat(30);
        let named = disambiguate(vec![
            t(&base, &format!("b_{base}")),
            t(&format!("{base}_b"), &base),
        ]);
        assert_eq!(named.len(), 2);
        for (_, n) in &named {
            assert!(n.name.len() <= MAX_TOOL_NAME, "{}", n.name);
            assert_ne!(n.name, n.shared_name());
        }
        assert_ne!(named[0].1.name, named[1].1.name);
        assert_eq!(named[0].1.shared_name(), named[1].1.shared_name());
    }

    #[test]
    fn colliding_tool_names_are_disambiguated() {
        let t = |server: &str, remote: &str| McpTool {
            name: tool_name(server, remote),
            server: server.into(),
            remote: remote.into(),
            description: "d".into(),
            schema: json!({}),
            output_schema: None,
            codemode: false,
        };
        // `x` + `y_z` and `x_y` + `z` are both `x_y_z`; neither keeps it.
        let named = disambiguate(vec![
            t("x", "y_z"),
            t("x", "other"),
            t("x_y", "z"),
            t("x_y", "z"),
        ]);
        let names: Vec<(usize, &str, &str, &str)> = named
            .iter()
            .map(|(i, t)| (*i, t.name.as_str(), t.server.as_str(), t.remote.as_str()))
            .collect();
        assert_eq!(
            names.len(),
            3,
            "the repeated listing is kept once: {names:?}"
        );
        assert_eq!(names[1], (1, "x_other", "x", "other"));
        let (a, b) = (&named[0].1, &named[2].1);
        assert_eq!((a.server.as_str(), b.server.as_str()), ("x", "x_y"));
        assert_ne!(a.name, b.name);
        for n in [&a.name, &b.name] {
            assert!(
                n.starts_with("x_y_z_") && n.len() == "x_y_z_".len() + 8,
                "{n}"
            );
        }
        assert!(
            a.description.starts_with("d\n\n(Named x_y_z_"),
            "{}",
            a.description
        );
        assert!(a.description.contains("tool \"y_z\" on server \"x\""));
        // The suffix depends only on the tool, so names are stable.
        let again = disambiguate(vec![t("x_y", "z"), t("x", "y_z")]);
        assert_eq!(again[0].1.name, b.name);
        assert_eq!(again[1].1.name, a.name);
        // No collision, no change.
        assert_eq!(disambiguate(vec![t("x", "y")])[0].1.name, "x_y");
    }

    #[test]
    fn result_texts() {
        let r = json!({"content": [
            {"type": "text", "text": "a"},
            {"type": "image", "data": "..."},
            {"type": "resource", "resource": {"uri": "file:///x", "text": "inline"}},
            {"type": "resource_link", "uri": "file:///y"}
        ], "isError": true});
        assert_eq!(
            result_text(&r),
            (
                "a\n[image content omitted: not a supported image]\ninline\n[resource file:///y]"
                    .into(),
                true
            )
        );
        assert_eq!(
            result_text(&json!({"content": [], "structuredContent": {"n": 1}})),
            ("{\"n\":1}".into(), false)
        );
    }

    #[test]
    fn result_images_become_media() {
        const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let r = json!({"content": [
            {"type": "text", "text": "shot"},
            {"type": "image", "data": PIXEL, "mimeType": "image/jpg"},
            {"type": "resource", "resource": {"uri": "file:///r/doc.pdf", "mimeType": "application/pdf", "blob": "JVBERi0xLjQ="}},
            {"type": "resource", "resource": {"uri": "file:///r/a.bin", "blob": "AAEC"}},
            {"type": "audio", "data": "AAEC", "mimeType": "audio/wav"}
        ]});
        let (text, is_error, media) = result_content(&r);
        assert_eq!(
            text,
            "shot\n[resource file:///r/a.bin]\n[audio content omitted]"
        );
        assert!(!is_error);
        assert_eq!(
            media,
            vec![
                codesmith_ai::Media::base64("image/png", PIXEL),
                codesmith_ai::Media::base64("application/pdf", "JVBERi0xLjQ=").named("doc.pdf"),
            ]
        );
        // Without a way to pass media on, it is noted.
        assert_eq!(
            result_text(&r).0,
            "shot\n[resource file:///r/a.bin]\n[audio content omitted]\n[2 attachments omitted: returned as text]"
        );
    }

    #[test]
    fn server_requests() {
        assert_eq!(answer_server_request("ping", &[]).unwrap(), json!({}));
        assert_eq!(
            answer_server_request("roots/list", &["/p".into()]).unwrap()["roots"][0]["uri"],
            "file:///p"
        );
        assert_eq!(
            answer_server_request("sampling/createMessage", &[])
                .unwrap_err()
                .0,
            -32601
        );
    }

    #[test]
    fn config_shapes() {
        let c: McpConfig = serde_json::from_value(json!({
            "timeout": {"startup": 5000},
            "servers": {
                "fs": {"type": "local", "command": ["npx", "-y", "server-fs"], "environment": {"K": "v"}},
                "web": {"type": "remote", "url": "https://mcp.example/mcp", "headers": {"Authorization": "Bearer x"}, "disabled": true}
            }
        }))
        .unwrap();
        assert_eq!(c.timeout.startup, Some(5000));
        assert!(
            matches!(&c.servers["fs"], ServerConfig::Local { command, .. } if command[0] == "npx")
        );
        assert!(c.servers["web"].disabled());
        assert!(serde_json::from_value::<ServerConfig>(json!({"type": "carrier-pigeon"})).is_err());
    }

    fn python() -> Option<&'static str> {
        ["python3", "python"].into_iter().find(|p| {
            std::process::Command::new(p)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
    }

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[tokio::test]
    async fn servers_whose_names_collide_keep_their_own_tools() {
        let Some(py) = python() else { return };
        // `my srv` and `my_srv` would both offer `my_srv_echo`.
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "my srv": {"type": "local", "command": [py, fixture("mcp_stdio.py")]},
            "my_srv": {"type": "local", "command": [py, fixture("mcp_stdio.py")]},
        }}))
        .unwrap();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        let tools = mcp.tools();
        assert_eq!(tools.len(), 4);
        assert!(
            mcp.tool("my_srv_echo").is_none(),
            "the shared name routes nowhere"
        );
        assert!(mcp.call("my_srv_echo", json!({"text": "x"})).await.is_err());
        let echoes: Vec<&McpTool> = tools.iter().filter(|t| t.remote == "echo").collect();
        assert_eq!(echoes.len(), 2);
        assert_ne!(echoes[0].name, echoes[1].name);
        for t in echoes {
            assert_eq!(mcp.tool(&t.name).unwrap().server, t.server);
            assert_eq!(
                mcp.call(&t.name, json!({"text": "hi"})).await.unwrap(),
                ("echo: hi".into(), false)
            );
        }
    }

    #[tokio::test]
    async fn stdio_server_end_to_end() {
        let Some(py) = python() else { return };
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "fake": {"type": "local", "command": [py, fixture("mcp_stdio.py")]},
            "broken": {"type": "local", "command": ["/nonexistent/server"]},
            "off": {"type": "local", "command": ["x"], "disabled": true}
        }}))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mcp = Mcp::connect(&cfg, dir.path()).await;
        assert_eq!(
            mcp.statuses()["fake"],
            Status::Connected { tools: 2 },
            "both pages listed"
        );
        assert!(matches!(&mcp.statuses()["broken"], Status::Failed(e) if e.contains("starting")));
        assert_eq!(mcp.statuses()["off"], Status::Disabled);
        let names: Vec<String> = mcp.tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["fake_echo", "fake_fail_now"]);
        assert_eq!(
            mcp.tool("fake_fail_now").unwrap().schema,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            mcp.call("fake_echo", json!({"text": "hi"})).await.unwrap(),
            ("echo: hi".into(), false),
            "the server's mid-call ping was answered"
        );
        assert_eq!(
            mcp.call("fake_fail_now", json!({})).await.unwrap(),
            ("boom".into(), true)
        );
        assert!(mcp.call("nope", json!({})).await.is_err());
    }

    #[tokio::test]
    async fn timed_out_and_dropped_requests_are_forgotten() {
        let Some(py) = python() else { return };
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "fake": {"type": "local", "command": [py, fixture("mcp_stdio.py")], "timeout": {"execution": 300}}
        }}))
        .unwrap();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        let c = mcp.client("fake").unwrap();
        let pending = || c.shared.pending.lock().unwrap().len();
        let e = mcp
            .call("fake_echo", json!({"text": "hang"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("timed out"), "{e}");
        assert_eq!(pending(), 0);
        // A call whose caller gives up (a cancelled turn) is forgotten too.
        let m = mcp.clone();
        let call = tokio::spawn(async move { m.call("fake_echo", json!({"text": "x"})).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pending(), 1);
        call.abort();
        let _ = call.await;
        assert_eq!(pending(), 0);
    }

    #[tokio::test]
    async fn an_exited_server_is_restarted_once() {
        let Some(py) = python() else { return };
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "fake": {"type": "local", "command": [py, fixture("mcp_stdio.py")]}
        }}))
        .unwrap();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        let e = mcp
            .call("fake_echo", json!({"text": "crash"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("exited"), "{e}");
        assert!(
            mcp.tools().is_empty(),
            "an exited server's tools are not offered"
        );
        assert!(mcp.client("fake").is_none());
        // The next call (or the next model step's refresh) starts it again.
        assert_eq!(
            mcp.call("fake_echo", json!({"text": "back"}))
                .await
                .unwrap(),
            ("echo: back".into(), false)
        );
        assert_eq!(mcp.tools().len(), 2);
        let _ = mcp.call("fake_echo", json!({"text": "crash"})).await;
        mcp.refresh().await;
        assert_eq!(mcp.tools().len(), 2, "restarted at the next step");

        // A server that cannot start again stays unavailable, with the reason.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let script = format!(
            "import os, runpy, sys\nif os.path.exists({m:?}): sys.exit(3)\nopen({m:?}, 'w').close()\nsys.argv = [{f:?}]\nrunpy.run_path({f:?}, run_name='__main__')",
            m = marker.display().to_string(),
            f = fixture("mcp_stdio.py")
        );
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "once": {"type": "local", "command": [py, "-c", script], "timeout": {"startup": 2000}}
        }}))
        .unwrap();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        assert_eq!(mcp.statuses()["once"], Status::Connected { tools: 2 });
        let _ = mcp.call("once_echo", json!({"text": "crash"})).await;
        let e = mcp
            .call("once_echo", json!({"text": "hi"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("MCP server once is unavailable"), "{e}");
        assert!(e.contains("restarting it failed"), "{e}");
        let again = mcp
            .call("once_echo", json!({"text": "hi"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            again.contains("unavailable"),
            "only one restart is tried: {again}"
        );
        assert!(mcp.tools().is_empty());
    }

    #[tokio::test]
    async fn http_server_with_sse_and_session_id() {
        let Some(py) = python() else { return };
        let mut child = tokio::process::Command::new(py)
            .arg(fixture("mcp_http.py"))
            // The fixture exits when this pipe closes (see the fixture).
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        let port: u16 = line.trim().parse().unwrap();
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {"calc": {"type": "remote", "url": format!("http://127.0.0.1:{port}/mcp")}}})).unwrap();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        assert_eq!(mcp.statuses()["calc"], Status::Connected { tools: 1 });
        assert_eq!(
            mcp.call("calc_add", json!({"a": 2, "b": 3})).await.unwrap(),
            ("5".into(), false)
        );
        // The call's stream carried a ping (answered by POST) and list_changed.
        mcp.refresh().await;
        let names: Vec<String> = mcp.tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["calc_add", "calc_pinged"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn instructions_resources_prompts_and_list_changed() {
        let Some(py) = python() else { return };
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "rich": {"type": "local", "command": [py, fixture("mcp_rich.py")]},
            "plain": {"type": "local", "command": [py, fixture("mcp_stdio.py")]}
        }}))
        .unwrap();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        assert_eq!(mcp.statuses()["rich"], Status::Connected { tools: 1 });
        assert_eq!(
            mcp.instructions(),
            [(
                "rich".to_string(),
                "Always cite ticket ids.\nPrefer memo resources.".to_string()
            )]
        );
        assert_eq!(mcp.resource_servers(), ["rich"], "only rich has resources");

        let all = mcp.list_resources(None).await.unwrap();
        assert_eq!(all["resources"][0]["uri"], "memo://one");
        assert_eq!(all["resources"][0]["server"], "rich");
        assert_eq!(all["templates"][0]["uriTemplate"], "memo://{id}");
        assert_eq!(
            mcp.read_resource("rich", "memo://one").await.unwrap(),
            "the first memo"
        );
        assert!(mcp
            .read_resource("rich", "memo://logo")
            .await
            .unwrap()
            .contains("image/png omitted"));
        assert!(format!(
            "{:#}",
            mcp.read_resource("rich", "memo://nope").await.unwrap_err()
        )
        .contains("Resource not found"));
        assert!(mcp.read_resource("plain", "x").await.is_err());
        assert!(mcp.list_resources(Some("nope")).await.is_err());

        let cmds = mcp.commands();
        assert_eq!(cmds.keys().collect::<Vec<_>>(), ["rich:review"]);
        let c = &cmds["rich:review"];
        assert_eq!(c.mcp, Some(("rich".into(), "review".into())));
        assert_eq!(
            c.description.as_deref(),
            Some("Review a change <target> [focus]")
        );
        assert_eq!(
            mcp.prompt_text("rich", "review", "\"src/main.rs\" errors")
                .await
                .unwrap(),
            "Review src/main.rs focusing on errors."
        );
        assert_eq!(
            mcp.prompt_text("rich", "review", "lib.rs").await.unwrap(),
            "Review lib.rs focusing on everything.",
            "a missing argument is sent empty"
        );
        assert!(mcp.prompt_text("rich", "nope", "").await.is_err());

        // A call that announces new tools and prompts; the next refresh sees them.
        assert_eq!(mcp.call("rich_grow", json!({})).await.unwrap().0, "grown");
        assert_eq!(mcp.tools().iter().filter(|t| t.server == "rich").count(), 1);
        mcp.refresh().await;
        let rich: Vec<String> = mcp
            .tools()
            .into_iter()
            .filter(|t| t.server == "rich")
            .map(|t| t.name)
            .collect();
        assert_eq!(rich, ["rich_grow", "rich_extra"]);
        assert!(mcp.commands().contains_key("rich:later"));

        // Events: push types only, with arguments checked against inputSchema.
        let t = mcp
            .check_event("rich", "ticket.created", Some(&json!({"queue": "ops"})))
            .await
            .unwrap();
        assert!(t.payload_schema.is_some());
        let err = |e: anyhow::Error| format!("{e:#}");
        assert!(err(mcp
            .check_event("rich", "ticket.created", None)
            .await
            .unwrap_err())
        .contains("inputSchema"));
        assert!(
            err(mcp.check_event("rich", "polled", None).await.unwrap_err())
                .contains("No push event")
        );
        assert!(err(mcp.check_event("plain", "x", None).await.unwrap_err())
            .contains("offers no events"));

        // A stream: active, two events, a recoverable error, then heartbeats.
        let c = mcp.client("rich").unwrap();
        let mut s = c.stream_events("ticket.created", Some(&json!({"queue": "ops"})), None);
        let mut seen = vec![];
        while seen.len() < 5 {
            seen.push(s.next().await.unwrap().unwrap());
        }
        assert_eq!(
            seen[0],
            EventMessage::Active {
                cursor: Some("c0".into()),
                truncated: false
            }
        );
        assert!(
            matches!(&seen[2], EventMessage::Event { id, cursor, data, .. }
            if id == "e2" && cursor.as_deref() == Some("c2") && data["title"] == "second")
        );
        assert!(matches!(seen[3], EventMessage::Error { .. }));
        assert_eq!(
            seen[4],
            EventMessage::Heartbeat {
                cursor: Some("c2".into())
            }
        );
        drop(s);
        // Resuming replays from the cursor; a refused stream says so.
        let mut s = c.stream_events("ticket.created", Some(&json!({"queue": "ops"})), Some("c2"));
        assert_eq!(s.next().await.unwrap().unwrap().cursor(), Some("c2"));
        assert!(
            matches!(s.next().await.unwrap().unwrap(), EventMessage::Event { id, .. } if id == "e2")
        );
        let mut r = c.stream_events("ticket.created", Some(&json!({"queue": "reject"})), None);
        assert!(
            matches!(r.next().await.unwrap(), Err(StreamError::Rejected(m)) if m.contains("Forbidden"))
        );
        // The connection still works after all that.
        assert_eq!(mcp.call("rich_grow", json!({})).await.unwrap().0, "grown");
    }

    #[test]
    fn instruction_blocks() {
        let text = render_instructions(&[
            ("a".into(), "one\ntwo".into(), true),
            ("b c".into(), "three".into(), false),
        ]);
        assert_eq!(
            text,
            "<mcp_instructions>\n  <server name=\"a\">\n    Use tools from this server through `execute` under `tools[\"a\"]`.\n    one\n    two\n  </server>\n  <server name=\"b c\">\n    three\n  </server>\n</mcp_instructions>"
        );
        assert_eq!(command_name("my server", "do.it"), "my_server:do_it");
        assert_eq!(
            prompt_result_text(
                &json!({"messages": [{"content": {"type": "text", "text": " a "}}, {"content": "b"}]})
            ),
            "a \nb"
        );
    }

    #[tokio::test]
    async fn startup_timeout() {
        let Some(py) = python() else { return };
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "slow": {"type": "local", "command": [py, "-c", "import time; time.sleep(30)"], "timeout": {"startup": 300}}
        }}))
        .unwrap();
        let start = std::time::Instant::now();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(matches!(&mcp.statuses()["slow"], Status::Failed(e) if e.contains("timed out")));
    }
}

#[cfg(test)]
mod defaults_tests {
    use super::*;

    // Servers connect with the known-server defaults; disabled ones are
    // never started, so nothing is contacted.
    #[tokio::test]
    async fn connecting_applies_the_code_mode_defaults() {
        let cfg: McpConfig = serde_json::from_value(json!({"servers": {
            "exe": {"type": "local", "command": ["executor", "mcp"], "disabled": true},
            "ph": {"type": "remote", "url": "https://mcp.posthog.com/mcp", "disabled": true}
        }}))
        .unwrap();
        let m = Mcp::connect(&cfg, Path::new("/")).await;
        let configs = m.configs.read().unwrap().clone();
        assert!(!configs["exe"].codemode());
        let ServerConfig::Remote { headers, .. } = &configs["ph"] else {
            panic!()
        };
        assert_eq!(headers[defaults::POSTHOG_MODE], "tools");
        m.add(
            "exe2",
            serde_json::from_value(
                json!({"type": "local", "command": ["executor", "mcp"], "disabled": true}),
            )
            .unwrap(),
        )
        .await;
        assert!(!m.configs.read().unwrap()["exe2"].codemode());
    }
}

#[cfg(test)]
mod oauth_flow_tests {
    use super::*;

    #[tokio::test]
    async fn login_refresh_and_use() {
        let Some(py) = ["python3", "python"].into_iter().find(|p| {
            std::process::Command::new(p)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        }) else {
            return;
        };
        let mut child = tokio::process::Command::new(py)
            .arg(format!(
                "{}/tests/fixtures/mcp_oauth.py",
                env!("CARGO_MANIFEST_DIR")
            ))
            // The fixture exits when this pipe closes (see the fixture).
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        let url = format!("http://127.0.0.1:{}/mcp", line.trim());
        let name = format!("oauth-test-{}", std::process::id());

        // Without a token the server is reported as needing a login.
        let cfg: McpConfig =
            serde_json::from_value(json!({"servers": {&name: {"type": "remote", "url": &url}}}))
                .unwrap();
        assert_eq!(
            Mcp::connect(&cfg, Path::new("/")).await.statuses()[&name],
            Status::NeedsAuth
        );

        // The browser is played by an HTTP client that follows the redirect to our callback.
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("mcp-auth.json");
        let http = reqwest::Client::new();
        let setting: oauth::Setting = serde_json::from_value(json!({"port": 0})).unwrap();
        let e = oauth::login(&http, &store, &name, &url, Some(&setting), |u| {
            let u = u.to_string();
            tokio::spawn(async move {
                let _ = reqwest::get(u).await;
            });
        })
        .await
        .unwrap();
        assert_eq!(
            (
                e.client_id.as_str(),
                e.access_token.as_str(),
                e.refresh_token.as_deref()
            ),
            ("cid-1", "at-1", Some("rt-1"))
        );

        // at-1 expires at once, so asking for a token refreshes it.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let tok = oauth::token(&http, &store, &name, &url).await.unwrap();
        assert_eq!(tok, "at-2");
        assert_eq!(
            oauth::load(&store, &name, &url)
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rt-1"),
            "the refresh token is kept"
        );

        let cfg: McpConfig = serde_json::from_value(json!({"servers": {&name: {"type": "remote", "url": &url, "headers": {"Authorization": format!("Bearer {tok}")}}}})).unwrap();
        let mcp = Mcp::connect(&cfg, Path::new("/")).await;
        assert_eq!(mcp.statuses()[&name], Status::Connected { tools: 1 });
        let tool = tool_name(&name, "whoami");
        assert_eq!(mcp.call(&tool, json!({})).await.unwrap().0, "token at-2");
    }
}
