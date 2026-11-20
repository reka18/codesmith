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

pub mod events;
pub mod oauth;

pub use events::{EventMessage, EventStream, EventType, StreamError};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
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

/// `<server>_<tool>` with anything outside `[A-Za-z0-9_-]` turned into `_`.
pub fn tool_name(server: &str, tool: &str) -> String {
    sanitize(&format!("{server}_{tool}"))
}

/// Tools with their final names, each with its index in `tools`. Names come
/// from [`tool_name`], which can give two tools the same name (server `x`
/// with tool `y_z` and server `x_y` with tool `z` are both `x_y_z`). Every
/// tool sharing a name with a different tool gets a suffix made from a hash
/// of its server and tool (`x_y_z_1a2b3c4d`), with a note in its
/// description, so no call meant for one server reaches another and no
/// rule written for the shared name applies to either. A tool a server
/// lists twice is kept once.
fn disambiguate(tools: Vec<McpTool>) -> Vec<(usize, McpTool)> {
    use sha2::Digest;
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
            let h = sha2::Sha256::digest(format!("{}\0{}", t.server, t.remote).as_bytes());
            let hex: String = h[..4].iter().map(|b| format!("{b:02x}")).collect();
            let note = format!(
                "(Named {}_{hex}: the name {} is shared with another MCP tool. This is tool {:?} on server {:?}.)",
                t.name, t.name, t.remote, t.server
            );
            t.name = format!("{}_{hex}", t.name);
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
/// (`<server>_*`, including names disambiguated after a collision).
pub fn tools_pattern(server: &str) -> String {
    format!("{}_*", sanitize(server))
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
                            None => parts.push(format!("[resource {uri}]")),
                        }
                    }
                    _ => parts.push(format!("[resource {uri}]")),
                }
            }
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
    let contents = result["contents"].as_array().cloned().unwrap_or_default();
    let several = contents.len() > 1;
    contents
        .iter()
        .map(|c| {
            let uri = c["uri"].as_str().unwrap_or("?");
            let body = match (c["text"].as_str(), c["blob"].as_str()) {
                (Some(t), _) => t.to_string(),
                (None, Some(b)) => format!(
                    "[binary content, {} omitted: {} bytes of base64]",
                    c["mimeType"].as_str().unwrap_or("unknown type"),
                    b.len()
                ),
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
    let mut out = vec!["<mcp_instructions>".to_string()];
    for (server, text, codemode) in servers {
        out.push(format!("  <server name=\"{server}\">"));
        if *codemode {
            out.push(format!(
                "    Use tools from this server through `execute` under `tools[{}]`.",
                serde_json::to_string(server).unwrap()
            ));
        }
        out.extend(text.lines().map(|l| format!("    {l}")));
        out.push("  </server>".into());
    }
    out.push("</mcp_instructions>".into());
    out.join("\n")
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
}

impl Shared {
    /// Route one message from the server. Returns the reply to send when it
    /// is a request.
    fn dispatch(&self, m: &Value) -> Option<Value> {
        let id = m.get("id").filter(|v| !v.is_null());
        match (m.get("method").and_then(Value::as_str), id) {
            (None, Some(id)) => {
                let tx = id
                    .as_u64()
                    .and_then(|i| self.pending.lock().unwrap().remove(&i));
                if let Some(tx) = tx {
                    let _ = tx.send(rpc_result(m));
                }
                None
            }
            (Some(method), Some(id)) => Some(match answer_server_request(method, &self.roots) {
                Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                Err((code, msg)) => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
                }
            }),
            (Some(method), None) => {
                self.notification(method, &m["params"]);
                None
            }
            _ => None,
        }
    }

    fn notification(&self, method: &str, params: &Value) {
        match method {
            "notifications/tools/list_changed" => self.tools_stale.store(true, Ordering::SeqCst),
            "notifications/prompts/list_changed" => {
                self.prompts_stale.store(true, Ordering::SeqCst)
            }
            "notifications/message" => self.log_line(params),
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

enum Transport {
    Stdio {
        stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
        _child: tokio::process::Child,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: BTreeMap<String, String>,
        session: Mutex<Option<String>>,
    },
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
            Transport::Stdio { stdin, .. } => {
                let mut line = serde_json::to_string(msg)?;
                line.push('\n');
                stdin
                    .lock()
                    .await
                    .write_all(line.as_bytes())
                    .await
                    .context("writing to the MCP server")
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
        rb.json(msg)
    }

    /// POST one message and route everything the server sends back, whether
    /// one JSON reply or an SSE stream that may carry notifications and
    /// requests before (or, for an event stream, instead of) the reply.
    async fn post(&self, msg: &Value) -> Result<()> {
        let Transport::Http { session, .. } = &self.transport else {
            unreachable!()
        };
        let mut resp = self
            .http_request(msg)
            .send()
            .await
            .context("contacting the MCP server")?;
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *session.lock().unwrap() = Some(s.to_string());
        }
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let www = resp
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            return Err(oauth::Unauthorized(www).into());
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
                    schema: t
                        .get("inputSchema")
                        .cloned()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
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
        self.request(
            "tools/call",
            json!({"name": tool, "arguments": args}),
            self.timeouts.1,
        )
        .await
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

/// Every configured server's client, tools, prompts and status.
pub struct Mcp {
    clients: RwLock<BTreeMap<String, Arc<Client>>>,
    pub statuses: BTreeMap<String, Status>,
    /// What reconnecting a server needs: its config, the project root and
    /// the default timeouts.
    configs: BTreeMap<String, ServerConfig>,
    root: PathBuf,
    defaults: Timeouts,
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

impl Mcp {
    pub fn empty() -> Arc<Mcp> {
        Arc::new(Mcp {
            clients: RwLock::new(BTreeMap::new()),
            statuses: BTreeMap::new(),
            configs: BTreeMap::new(),
            root: PathBuf::new(),
            defaults: Timeouts::default(),
        })
    }

    /// Connect to every enabled server concurrently.
    pub async fn connect(cfg: &McpConfig, root: &Path) -> Arc<Mcp> {
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
            match r {
                Ok(c) => {
                    let tools = c.tools.read().unwrap().len();
                    statuses.insert(name.clone(), Status::Connected { tools });
                    clients.insert(name, c);
                }
                Err(e) if e.downcast_ref::<oauth::Unauthorized>().is_some() => {
                    statuses.insert(name, Status::NeedsAuth);
                }
                Err(e) => {
                    statuses.insert(name, Status::Failed(format!("{e:#}")));
                }
            }
        }
        Arc::new(Mcp {
            clients: RwLock::new(clients),
            statuses,
            configs: cfg.servers.clone(),
            root: root.to_path_buf(),
            defaults: cfg.timeout.clone(),
        })
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
        let cfg = self.configs.get(server)?;
        match start(server, cfg, &self.root, &self.defaults).await {
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
        let clients = self.clients.read().unwrap();
        let mut live = Vec::new();
        let mut all = Vec::new();
        for c in clients.values() {
            for t in c.tools.read().unwrap().iter() {
                live.push(c.exited().is_none());
                all.push(t.clone());
            }
        }
        let named = disambiguate(all);
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

    /// Call a tool: its text, whether it failed, and its images and PDFs.
    pub async fn call_media(
        &self,
        name: &str,
        args: Value,
    ) -> Result<(String, bool, Vec<codesmith_ai::Media>)> {
        Ok(result_content(&self.call_raw(name, args).await?))
    }

    async fn call_raw(&self, name: &str, args: Value) -> Result<Value> {
        let t = self
            .tool(name)
            .ok_or_else(|| anyhow!("no MCP tool {name}"))?;
        let c = self.connected_or_restart(&t.server).await?;
        c.call_raw(&t.remote, args).await
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
        let c = self.connected(server)?;
        if !c.has_resources() {
            return Err(anyhow!("MCP server {server:?} does not expose resources"));
        }
        Ok(resource_text(&c.read_resource(uri).await?))
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
    };
    let client = match cfg {
        ServerConfig::Remote {
            url,
            headers,
            oauth: setting,
            ..
        } => {
            let http = reqwest::Client::new();
            let mut headers = headers.clone();
            let has_auth = headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"));
            if !has_auth && oauth::Setting::enabled(setting.as_ref()) {
                if let Some(tok) = oauth::token(&http, &oauth::store_path(), name, url).await {
                    headers.insert("Authorization".into(), format!("Bearer {tok}"));
                }
            }
            Arc::new(make(Transport::Http {
                client: http,
                url: url.clone(),
                headers,
                session: Mutex::new(None),
            }))
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
            let dir = cwd
                .as_deref()
                .map(|c| {
                    if Path::new(c).is_absolute() {
                        c.into()
                    } else {
                        root.join(c)
                    }
                })
                .unwrap_or_else(|| root.to_path_buf());
            crate::paths::ensure_parent(&log_dir)?;
            crate::paths::private_dir(&log_dir)?;
            let stderr = crate::paths::create_private(&log)?;
            let mut child = tokio::process::Command::new(prog)
                .args(args)
                .current_dir(dir)
                .envs(environment)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(stderr))
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("starting {prog}"))?;
            let stdout = child.stdout.take().unwrap();
            let stdin = child.stdin.take().unwrap();
            let client = Arc::new(make(Transport::Stdio {
                stdin: tokio::sync::Mutex::new(stdin),
                _child: child,
            }));
            let weak = Arc::downgrade(&client);
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut out = BufReader::new(stdout);
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
                        if let Some(c) = weak.upgrade() {
                            let _ = c.send(&reply).await;
                        }
                    }
                }
                shared.close("the MCP server exited");
            });
            client
        }
    };
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"roots": {"listChanged": false}, "extensions": {events::EXTENSION: {}}},
        "clientInfo": {"name": "codesmith", "version": env!("CARGO_PKG_VERSION")}
    });
    let init = client.request("initialize", params, startup).await?;
    let _ = client.init.set(init);
    client.notify("notifications/initialized", None).await?;
    Ok(client)
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
        match &mcp.statuses["huge"] {
            Status::Failed(e) => assert!(e.contains("larger than 16 MiB"), "{e}"),
            s => panic!("{s:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "it did not wait for a timeout"
        );
    }

    #[test]
    fn colliding_tool_names_are_disambiguated() {
        let t = |server: &str, remote: &str| McpTool {
            name: tool_name(server, remote),
            server: server.into(),
            remote: remote.into(),
            description: "d".into(),
            schema: json!({}),
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
            mcp.statuses["fake"],
            Status::Connected { tools: 2 },
            "both pages listed"
        );
        assert!(matches!(&mcp.statuses["broken"], Status::Failed(e) if e.contains("starting")));
        assert_eq!(mcp.statuses["off"], Status::Disabled);
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
        assert_eq!(mcp.statuses["once"], Status::Connected { tools: 2 });
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
        assert_eq!(mcp.statuses["calc"], Status::Connected { tools: 1 });
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
        assert_eq!(mcp.statuses["rich"], Status::Connected { tools: 1 });
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
        assert!(matches!(&mcp.statuses["slow"], Status::Failed(e) if e.contains("timed out")));
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
            Mcp::connect(&cfg, Path::new("/")).await.statuses[&name],
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
        assert_eq!(mcp.statuses[&name], Status::Connected { tools: 1 });
        let tool = tool_name(&name, "whoami");
        assert_eq!(mcp.call(&tool, json!({})).await.unwrap().0, "token at-2");
    }
}
