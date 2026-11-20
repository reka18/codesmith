//! Model Context Protocol client: tools from MCP servers become tools the
//! model can call, named `<server>_<tool>` as in OpenCode. Config follows
//! OpenCode's `mcp.servers` shape: `{"type": "local", "command": [...]}` for a
//! stdio server or `{"type": "remote", "url": ...}` for streamable HTTP.

pub mod oauth;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_STARTUP_MS: u64 = 30_000;
const DEFAULT_CATALOG_MS: u64 = 30_000;
const DEFAULT_EXECUTION_MS: u64 = 5 * 60_000;

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

/// `<server>_<tool>` with anything outside `[A-Za-z0-9_-]` turned into `_`.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{server}_{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
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

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

enum Transport {
    Stdio {
        stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
        pending: Pending,
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
    next: AtomicU64,
    timeouts: (u64, u64),
}

impl Client {
    async fn request(&self, method: &str, params: Value, timeout_ms: u64) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let fut = async {
            match &self.transport {
                Transport::Stdio { stdin, pending, .. } => {
                    let (tx, rx) = oneshot::channel();
                    pending.lock().unwrap().insert(id, tx);
                    let mut line = serde_json::to_string(&msg)?;
                    line.push('\n');
                    stdin
                        .lock()
                        .await
                        .write_all(line.as_bytes())
                        .await
                        .context("writing to the MCP server")?;
                    rx.await
                        .map_err(|_| anyhow!("the MCP server exited"))?
                        .map_err(|e| anyhow!(e))
                }
                Transport::Http { .. } => self.http(msg, Some(id)).await,
            }
        };
        tokio::time::timeout(Duration::from_millis(timeout_ms), fut)
            .await
            .map_err(|_| anyhow!("{method} timed out after {timeout_ms} ms"))?
    }

    async fn notify(&self, method: &str) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut line = serde_json::to_string(&msg)?;
                line.push('\n');
                stdin.lock().await.write_all(line.as_bytes()).await?;
                Ok(())
            }
            Transport::Http { .. } => self.http(msg, None).await.map(|_| ()),
        }
    }

    async fn http(&self, msg: Value, id: Option<u64>) -> Result<Value> {
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
        let resp = rb
            .json(&msg)
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
        let Some(id) = id else { return Ok(Value::Null) };
        let is_sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|c| c.contains("text/event-stream"))
            .unwrap_or(false);
        let body = resp.text().await?;
        let messages: Vec<Value> = if is_sse {
            body.split("\n\n")
                .filter_map(|block| {
                    let data: Vec<&str> = block
                        .lines()
                        .filter_map(|l| l.strip_prefix("data:").map(str::trim_start))
                        .collect();
                    (!data.is_empty())
                        .then(|| serde_json::from_str(&data.join("\n")).ok())
                        .flatten()
                })
                .collect()
        } else {
            vec![serde_json::from_str(&body).context("the MCP server sent invalid JSON")?]
        };
        let reply = messages
            .into_iter()
            .find(|m| m["id"].as_u64() == Some(id))
            .ok_or_else(|| anyhow!("no response for request {id}"))?;
        rpc_result(&reply)
    }

    pub async fn list_tools(&self) -> Result<Vec<McpTool>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let r = self.request("tools/list", params, self.timeouts.0).await?;
            for t in r["tools"].as_array().into_iter().flatten() {
                let Some(remote) = t["name"].as_str() else {
                    continue;
                };
                out.push(McpTool {
                    codemode: false,
                    name: tool_name(&self.server, remote),
                    server: self.server.clone(),
                    remote: remote.to_string(),
                    description: t["description"].as_str().unwrap_or("").to_string(),
                    schema: t
                        .get("inputSchema")
                        .cloned()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                });
            }
            cursor = r["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                return Ok(out);
            }
        }
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
}

fn rpc_result(m: &Value) -> Result<Value> {
    if let Some(e) = m.get("error") {
        return Err(anyhow!(
            "MCP error {}: {}",
            e["code"],
            e["message"].as_str().unwrap_or("")
        ));
    }
    Ok(m.get("result").cloned().unwrap_or(Value::Null))
}

/// Every configured server's client, tools and status.
pub struct Mcp {
    clients: HashMap<String, Arc<Client>>,
    pub tools: Vec<McpTool>,
    pub statuses: BTreeMap<String, Status>,
}

impl Mcp {
    pub fn empty() -> Arc<Mcp> {
        Arc::new(Mcp {
            clients: HashMap::new(),
            tools: vec![],
            statuses: BTreeMap::new(),
        })
    }

    /// Connect to every enabled server concurrently.
    pub async fn connect(cfg: &McpConfig, root: &Path) -> Arc<Mcp> {
        let mut clients = HashMap::new();
        let mut tools = Vec::new();
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
                    let mut t = c.list_tools().await?;
                    t.iter_mut()
                        .for_each(|tool| tool.codemode = server.codemode());
                    Ok::<_, anyhow::Error>((c, t))
                }
                .await;
                (name, r)
            }));
        }
        for p in pending {
            let Ok((name, r)) = p.await else { continue };
            match r {
                Ok((c, t)) => {
                    statuses.insert(name.clone(), Status::Connected { tools: t.len() });
                    tools.extend(t);
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
            clients,
            tools,
            statuses,
        })
    }

    pub fn tool(&self, name: &str) -> Option<&McpTool> {
        self.tools.iter().find(|t| t.name == name)
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
            Arc::new(Client {
                server: name.to_string(),
                transport: Transport::Http {
                    client: http,
                    url: url.clone(),
                    headers,
                    session: Mutex::new(None),
                },
                next: AtomicU64::new(1),
                timeouts,
            })
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
            let log = crate::paths::data_dir().join("log");
            std::fs::create_dir_all(&log)?;
            let stderr = std::fs::File::create(log.join(format!("mcp-{name}.log")))?;
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
            let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
            let client = Arc::new(Client {
                server: name.to_string(),
                transport: Transport::Stdio {
                    stdin: tokio::sync::Mutex::new(stdin),
                    pending: pending.clone(),
                    _child: child,
                },
                next: AtomicU64::new(1),
                timeouts,
            });
            let weak = Arc::downgrade(&client);
            let roots = vec![root.display().to_string()];
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(m) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    match (
                        m.get("id").and_then(Value::as_u64),
                        m.get("method").and_then(Value::as_str),
                    ) {
                        (Some(id), None) => {
                            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(rpc_result(&m).map_err(|e| e.to_string()));
                            }
                        }
                        (_, Some(method)) if m.get("id").is_some() => {
                            let reply = match answer_server_request(method, &roots) {
                                Ok(r) => json!({"jsonrpc": "2.0", "id": m["id"], "result": r}),
                                Err((code, msg)) => {
                                    json!({"jsonrpc": "2.0", "id": m["id"], "error": {"code": code, "message": msg}})
                                }
                            };
                            if let Some(c) = weak.upgrade() {
                                if let Transport::Stdio { stdin, .. } = &c.transport {
                                    let _ = stdin
                                        .lock()
                                        .await
                                        .write_all(format!("{reply}\n").as_bytes())
                                        .await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                for (_, tx) in pending.lock().unwrap().drain() {
                    let _ = tx.send(Err("the MCP server exited".into()));
                }
            });
            client
        }
    };
    let params = json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {"roots": {"listChanged": false}}, "clientInfo": {"name": "codesmith", "version": env!("CARGO_PKG_VERSION")}});
    client.request("initialize", params, startup).await?;
    client.notify("notifications/initialized").await?;
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
        let names: Vec<&str> = mcp.tools.iter().map(|t| t.name.as_str()).collect();
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
