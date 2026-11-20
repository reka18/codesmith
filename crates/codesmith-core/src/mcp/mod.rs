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

/// The slash command that runs a server's prompt: `server:prompt`, as in OpenCode.
pub fn command_name(server: &str, prompt: &str) -> String {
    format!("{}:{}", sanitize(server), sanitize(prompt))
}

/// The text of a `tools/call` result: text parts joined, other content noted.
pub fn result_text(result: &Value) -> (String, bool) {
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let mut parts = Vec::new();
    for c in result["content"].as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("text") => parts.push(c["text"].as_str().unwrap_or("").to_string()),
            Some("resource") => {
                let r = &c["resource"];
                match r["text"].as_str() {
                    Some(t) => parts.push(t.to_string()),
                    None => parts.push(format!("[resource {}]", r["uri"].as_str().unwrap_or("?"))),
                }
            }
            Some("resource_link") => {
                parts.push(format!("[resource {}]", c["uri"].as_str().unwrap_or("?")))
            }
            Some(other) => parts.push(format!("[{other} content omitted]")),
            None => {}
        }
    }
    if parts.is_empty() {
        if let Some(s) = result.get("structuredContent") {
            parts.push(s.to_string());
        }
    }
    (parts.join("\n"), is_error)
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
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(
                f,
                "[{}] {logger}{data}",
                p["level"].as_str().unwrap_or("info")
            );
        }
    }

    /// The connection is gone: fail whatever still waits on it.
    fn close(&self, why: &str) {
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err(anyhow!("{why}")));
        }
        for (_, tx) in self.streams.lock().unwrap().drain() {
            let _ = tx.send(Err(StreamError::Disconnected(why.into())));
        }
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
        let r = match timeout_ms {
            Some(ms) => tokio::time::timeout(Duration::from_millis(ms), fut)
                .await
                .unwrap_or_else(|_| Err(anyhow!("{method} timed out after {ms} ms"))),
            None => fut.await,
        };
        self.shared.pending.lock().unwrap().remove(&id);
        r
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
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .context("reading the MCP server's stream")?
            {
                buf.extend(chunk.iter().filter(|b| **b != b'\r'));
                while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
                    let block: Vec<u8> = buf.drain(..end + 2).collect();
                    self.sse_event(&String::from_utf8_lossy(&block)).await;
                }
            }
            if !buf.is_empty() {
                self.sse_event(&String::from_utf8_lossy(&buf)).await;
            }
        } else {
            let body = resp.text().await?;
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
        let r = self
            .request(
                "tools/call",
                json!({"name": tool, "arguments": args}),
                self.timeouts.1,
            )
            .await?;
        Ok(result_text(&r))
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
    clients: BTreeMap<String, Arc<Client>>,
    pub statuses: BTreeMap<String, Status>,
}

impl Mcp {
    pub fn empty() -> Arc<Mcp> {
        Arc::new(Mcp {
            clients: BTreeMap::new(),
            statuses: BTreeMap::new(),
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
                let r = async {
                    let c = connect(&name, &server, &root, &defaults).await?;
                    let t = c.list_tools().await?;
                    // A broken prompt list should not cost the server its tools.
                    let p = c.list_prompts().await.unwrap_or_default();
                    *c.tools.write().unwrap() = t;
                    *c.prompts.write().unwrap() = p;
                    Ok::<_, anyhow::Error>(c)
                }
                .await;
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
        Arc::new(Mcp { clients, statuses })
    }

    pub fn client(&self, server: &str) -> Option<&Arc<Client>> {
        self.clients.get(server)
    }

    /// Every connected server's tools, by server name.
    pub fn tools(&self) -> Vec<McpTool> {
        self.clients
            .values()
            .flat_map(|c| c.tools.read().unwrap().clone())
            .collect()
    }

    pub fn tool(&self, name: &str) -> Option<McpTool> {
        self.clients.values().find_map(|c| {
            c.tools
                .read()
                .unwrap()
                .iter()
                .find(|t| t.name == name)
                .cloned()
        })
    }

    /// Every connected server's prompts, by server then name.
    pub fn prompts(&self) -> Vec<McpPrompt> {
        let mut all: Vec<McpPrompt> = self
            .clients
            .values()
            .flat_map(|c| c.prompts.read().unwrap().clone())
            .collect();
        all.sort_by(|a, b| (&a.server, &a.name).cmp(&(&b.server, &b.name)));
        all
    }

    /// (server, instructions) for every connected server that gave any.
    pub fn instructions(&self) -> Vec<(String, String)> {
        self.clients
            .iter()
            .filter_map(|(n, c)| c.instructions().map(|i| (n.clone(), i)))
            .collect()
    }

    /// Connected servers that advertise resources.
    pub fn resource_servers(&self) -> Vec<String> {
        self.clients
            .iter()
            .filter(|(_, c)| c.has_resources())
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Relist whatever a server announced as changed (`list_changed`).
    pub async fn refresh(&self) {
        for c in self.clients.values() {
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

    pub async fn call(&self, name: &str, args: Value) -> Result<(String, bool)> {
        let t = self
            .tool(name)
            .ok_or_else(|| anyhow!("no MCP tool {name}"))?;
        let c = self
            .clients
            .get(&t.server)
            .ok_or_else(|| anyhow!("MCP server {} is not connected", t.server))?;
        c.call(&t.remote, args).await
    }

    fn connected(&self, server: &str) -> Result<&Arc<Client>> {
        self.clients
            .get(server)
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
                for c in self.clients.values() {
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
            .clients
            .get(server)
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
            std::fs::create_dir_all(&log_dir)?;
            let stderr = std::fs::File::create(&log)?;
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
                    match out.read_until(b'\n', &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names() {
        assert_eq!(tool_name("github", "create_issue"), "github_create_issue");
        assert_eq!(
            tool_name("my server", "do.thing/now"),
            "my_server_do_thing_now"
        );
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
                "a\n[image content omitted]\ninline\n[resource file:///y]".into(),
                true
            )
        );
        assert_eq!(
            result_text(&json!({"content": [], "structuredContent": {"n": 1}})),
            ("{\"n\":1}".into(), false)
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
