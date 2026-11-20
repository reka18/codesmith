//! One running language server: JSON-RPC over stdio with `Content-Length`
//! framing, the initialize handshake, document sync and the diagnostics the
//! server publishes. Ported from OpenCode's `lsp/client.ts`.

use super::{language_id, path_key, path_to_uri, uri_to_path, Diagnostic};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, oneshot};
use tokio::time::Instant;

/// Quiet period after a diagnostics push before the result is taken, since
/// servers often publish several times in a row (OpenCode: 150 ms).
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Quiet period after an empty push. Many servers clear a file's
/// diagnostics first and publish the real ones a moment later, so an empty
/// list is not trusted as quickly as a non-empty one.
pub const EMPTY_DEBOUNCE: Duration = Duration::from_millis(600);

/// Frame a JSON-RPC message with its `Content-Length` header.
pub fn encode(msg: &Value) -> Vec<u8> {
    let body = msg.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Read one framed message; `None` at end of stream. Headers other than
/// `Content-Length` (such as `Content-Type`) are ignored.
pub async fn read_message<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Option<Value>> {
    let mut length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            if length.is_none() {
                return Ok(None);
            }
            bail!("stream ended inside a message header");
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse().context("bad Content-Length")?);
            }
        }
    }
    let mut body = vec![0u8; length.unwrap_or(0)];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

/// How to launch a server.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub initialization: Option<Value>,
}

type Pending = Mutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>;

pub struct Client {
    pub server: String,
    pub root: PathBuf,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicI64,
    /// Latest diagnostics per file, keyed by `path_key`.
    diagnostics: Mutex<HashMap<PathBuf, Vec<Diagnostic>>>,
    /// Paths whose diagnostics were just published.
    events: broadcast::Sender<PathBuf>,
    /// Open documents and their versions.
    files: Mutex<HashMap<PathBuf, i64>>,
    /// A hash of the text last sent for each open document, so a watcher
    /// echo of an edit already sent is not sent again.
    sent: Mutex<HashMap<PathBuf, u64>>,
    /// File watchers the server registered (`client/registerCapability`).
    watchers: Mutex<Vec<Registered>>,
    initialization: Option<Value>,
    /// The first push for a file is not waited on (TypeScript publishes an
    /// early, incomplete one).
    skip_first: bool,
    _child: Child,
}

impl Client {
    /// Spawn the server in `root` and run the initialize handshake.
    pub async fn start(
        server: &str,
        root: &Path,
        launch: Launch,
        skip_first: bool,
        timeout: Duration,
    ) -> Result<Arc<Client>> {
        let (bin, args) = launch
            .command
            .split_first()
            .ok_or_else(|| anyhow!("empty command"))?;
        let mut child = tokio::process::Command::new(bin)
            .env_remove(crate::SERVER_TOKEN_VAR)
            .args(args)
            .current_dir(root)
            .envs(&launch.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {bin}"))?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let (events, _) = broadcast::channel(256);
        let client = Arc::new(Client {
            server: server.to_string(),
            root: root.to_path_buf(),
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(0),
            diagnostics: Mutex::new(HashMap::new()),
            events,
            files: Mutex::new(HashMap::new()),
            sent: Mutex::new(HashMap::new()),
            watchers: Mutex::new(Vec::new()),
            initialization: launch.initialization,
            skip_first,
            _child: child,
        });
        tokio::spawn(read_loop(Arc::downgrade(&client), BufReader::new(stdout)));
        let uri = path_to_uri(root);
        let params = json!({
            "rootUri": uri,
            "processId": std::process::id(),
            "workspaceFolders": [{"name": "workspace", "uri": uri}],
            "initializationOptions": client.initialization.clone().unwrap_or(json!({})),
            "capabilities": {
                "window": {"workDoneProgress": true},
                "workspace": {
                    "configuration": true,
                    "didChangeWatchedFiles": {"dynamicRegistration": true}
                },
                "textDocument": {
                    "synchronization": {"didOpen": true, "didChange": true},
                    "publishDiagnostics": {"versionSupport": true}
                }
            }
        });
        client
            .request("initialize", params, timeout)
            .await
            .context("initialize")?;
        client.notify("initialized", json!({})).await?;
        if let Some(init) = &client.initialization {
            client
                .notify(
                    "workspace/didChangeConfiguration",
                    json!({"settings": init}),
                )
                .await?;
        }
        Ok(client)
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        let mut w = self.stdin.lock().await;
        w.write_all(&encode(msg)).await?;
        w.flush().await?;
        Ok(())
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let sent = self
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        if let Err(e) = sent {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!(
                "{method}: {}",
                e.get("message").and_then(Value::as_str).unwrap_or("error")
            )),
            Ok(Err(_)) => Err(anyhow!("{method}: the server exited")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("{method}: timed out after {timeout:?}"))
            }
        }
    }

    /// Tell the server a file changed: `didOpen` the first time, then
    /// `didChange` with the full text and a new version.
    pub async fn open(&self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)?;
        let uri = path_to_uri(path);
        let key = path_key(path);
        self.sent.lock().unwrap().insert(key.clone(), hash(&text));
        let version = self.files.lock().unwrap().get(&key).copied();
        match version {
            Some(v) => {
                self.notify(
                    "workspace/didChangeWatchedFiles",
                    json!({"changes": [{"uri": uri, "type": 2}]}),
                )
                .await?;
                self.files.lock().unwrap().insert(key, v + 1);
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {"uri": uri, "version": v + 1},
                        "contentChanges": [{"text": text}]
                    }),
                )
                .await
            }
            None => {
                self.notify(
                    "workspace/didChangeWatchedFiles",
                    json!({"changes": [{"uri": uri, "type": 1}]}),
                )
                .await?;
                self.diagnostics.lock().unwrap().remove(&key);
                self.files.lock().unwrap().insert(key, 0);
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {
                        "uri": uri,
                        "languageId": language_id(path),
                        "version": 0,
                        "text": text
                    }}),
                )
                .await
            }
        }
    }

    /// Whether the server registered a file watcher covering `path` for
    /// `kind` (1 created, 2 changed, 3 deleted); `None` when it registered
    /// none.
    pub fn watches(&self, path: &Path, kind: u8) -> Option<bool> {
        let w = self.watchers.lock().unwrap();
        if w.is_empty() {
            return None;
        }
        Some(w.iter().any(|r| r.matches(path, kind)))
    }

    /// Files changed outside the agent (`(path, kind)`, kinds as in
    /// [`Self::watches`]): open documents get their new text, unless it is
    /// what was last sent (the watcher echoing an edit already reported),
    /// and the changes `wanted` keeps go out as `didChangeWatchedFiles`.
    pub async fn external(
        &self,
        changes: &[(PathBuf, u8)],
        wanted: impl Fn(&Path, u8) -> bool,
    ) -> Result<usize> {
        let mut out = Vec::new();
        for (path, kind) in changes {
            let key = path_key(path);
            let open = self.files.lock().unwrap().get(&key).copied();
            if let (Some(v), 1 | 2) = (open, kind) {
                let Ok(text) = std::fs::read_to_string(path) else {
                    continue;
                };
                let h = hash(&text);
                if self.sent.lock().unwrap().insert(key.clone(), h) == Some(h) {
                    continue;
                }
                self.files.lock().unwrap().insert(key, v + 1);
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": {"uri": path_to_uri(path), "version": v + 1},
                        "contentChanges": [{"text": text}]
                    }),
                )
                .await?;
            }
            if wanted(path, *kind) {
                out.push(json!({"uri": path_to_uri(path), "type": kind}));
            }
        }
        if !out.is_empty() {
            self.notify(WATCHED, json!({ "changes": out })).await?;
        }
        Ok(out.len())
    }

    /// Subscribe before `open`, then wait for the push it causes.
    pub fn subscribe(&self) -> broadcast::Receiver<PathBuf> {
        self.events.subscribe()
    }

    /// Wait until diagnostics for `path` arrive and go quiet for
    /// [`DEBOUNCE`], or until `timeout`.
    pub async fn wait(
        &self,
        rx: &mut broadcast::Receiver<PathBuf>,
        path: &Path,
        timeout: Duration,
    ) {
        let empty = |key: &Path| {
            self.diagnostics
                .lock()
                .unwrap()
                .get(key)
                .is_none_or(|d| d.is_empty())
        };
        wait_for(rx, path, timeout, empty).await
    }

    pub fn diagnostics(&self) -> HashMap<PathBuf, Vec<Diagnostic>> {
        self.diagnostics.lock().unwrap().clone()
    }

    /// Ask the server to shut down and exit.
    pub async fn shutdown(&self) {
        let _ = self
            .request("shutdown", Value::Null, Duration::from_secs(2))
            .await;
        let _ = self.notify("exit", Value::Null).await;
    }

    fn on_diagnostics(&self, params: &Value) {
        let Some(path) = params
            .get("uri")
            .and_then(Value::as_str)
            .and_then(uri_to_path)
        else {
            return;
        };
        let list: Vec<Diagnostic> = params
            .get("diagnostics")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .unwrap_or_default();
        let key = path_key(&path);
        let existed = self
            .diagnostics
            .lock()
            .unwrap()
            .insert(key.clone(), list)
            .is_some();
        if !existed && self.skip_first {
            return;
        }
        let _ = self.events.send(key);
    }

    /// Answer a request from the server.
    fn answer(&self, method: &str, params: &Value) -> Value {
        match method {
            "workspace/configuration" => {
                let n = params
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(1, Vec::len);
                let item = self.initialization.clone().unwrap_or(json!({}));
                Value::Array(vec![item; n])
            }
            "workspace/workspaceFolders" => {
                json!([{"name": "workspace", "uri": path_to_uri(&self.root)}])
            }
            "client/registerCapability" => {
                let list = params.get("registrations").and_then(Value::as_array);
                let mut w = self.watchers.lock().unwrap();
                for r in list.into_iter().flatten() {
                    if r.get("method").and_then(Value::as_str) == Some(WATCHED) {
                        let id = r.get("id").and_then(Value::as_str).unwrap_or_default();
                        w.extend(Registered::parse(id, r.get("registerOptions")));
                    }
                }
                Value::Null
            }
            "client/unregisterCapability" => {
                // The spec's own spelling.
                let list = params
                    .get("unregisterations")
                    .or_else(|| params.get("unregistrations"))
                    .and_then(Value::as_array);
                let ids: Vec<&str> = list
                    .into_iter()
                    .flatten()
                    .filter(|r| r.get("method").and_then(Value::as_str) == Some(WATCHED))
                    .filter_map(|r| r.get("id").and_then(Value::as_str))
                    .collect();
                self.watchers
                    .lock()
                    .unwrap()
                    .retain(|w| !ids.contains(&w.id.as_str()));
                Value::Null
            }
            _ => Value::Null,
        }
    }
}

async fn read_loop<R: AsyncBufRead + Unpin>(client: Weak<Client>, mut r: R) {
    while let Ok(Some(msg)) = read_message(&mut r).await {
        let Some(c) = client.upgrade() else { return };
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").filter(|v| !v.is_null());
        match (method, id) {
            (Some(m), Some(id)) => {
                let result = c.answer(m, msg.get("params").unwrap_or(&Value::Null));
                let _ = c
                    .send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
                    .await;
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                c.on_diagnostics(msg.get("params").unwrap_or(&Value::Null))
            }
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let Some(tx) = c.pending.lock().unwrap().remove(&id) else {
                    continue;
                };
                let _ = tx.send(match msg.get("error") {
                    Some(e) => Err(e.clone()),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                });
            }
            (None, None) => {}
        }
    }
    // The server is gone: fail every waiting request.
    if let Some(c) = client.upgrade() {
        c.pending.lock().unwrap().clear();
    }
}

/// Wait until diagnostics for `path` arrive and go quiet, or `timeout`.
/// `empty` says whether the latest list for a path has no diagnostics.
pub async fn wait_for(
    rx: &mut broadcast::Receiver<PathBuf>,
    path: &Path,
    timeout: Duration,
    empty: impl Fn(&Path) -> bool,
) {
    let key = path_key(path);
    let deadline = Instant::now() + timeout;
    let mut until = deadline;
    loop {
        match tokio::time::timeout_at(until, rx.recv()).await {
            Ok(Ok(p)) if p == key => {
                let quiet = if empty(&key) {
                    EMPTY_DEBOUNCE
                } else {
                    DEBOUNCE
                };
                until = (Instant::now() + quiet).min(deadline)
            }
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return,
        }
    }
}

const WATCHED: &str = "workspace/didChangeWatchedFiles";

fn hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// One registered file watcher: a glob, optionally relative to a base
/// folder, and the change kinds it wants (bits 1 create, 2 change, 4
/// delete; default all).
struct Registered {
    id: String,
    glob: globset::GlobMatcher,
    base: Option<PathBuf>,
    kinds: u8,
}

impl Registered {
    fn parse(id: &str, options: Option<&Value>) -> Vec<Registered> {
        let list = options
            .and_then(|o| o.get("watchers"))
            .and_then(Value::as_array);
        list.into_iter()
            .flatten()
            .filter_map(|w| {
                let g = w.get("globPattern")?;
                let (pattern, base) = match g {
                    Value::String(p) => (p.clone(), None),
                    Value::Object(o) => {
                        let base = match o.get("baseUri")? {
                            Value::String(u) => uri_to_path(u),
                            v => v.get("uri").and_then(Value::as_str).and_then(uri_to_path),
                        }?;
                        (
                            o.get("pattern")?.as_str()?.to_string(),
                            Some(path_key(&base)),
                        )
                    }
                    _ => return None,
                };
                let glob = globset::GlobBuilder::new(&pattern)
                    .literal_separator(true)
                    .build()
                    .ok()?
                    .compile_matcher();
                let kinds = w.get("kind").and_then(Value::as_u64).unwrap_or(7) as u8;
                Some(Registered {
                    id: id.to_string(),
                    glob,
                    base,
                    kinds,
                })
            })
            .collect()
    }

    fn matches(&self, path: &Path, kind: u8) -> bool {
        let bit = match kind {
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if self.kinds & bit == 0 {
            return false;
        }
        match &self.base {
            Some(base) => {
                let key = path_key(path);
                key.strip_prefix(base)
                    .or_else(|_| path.strip_prefix(base))
                    .is_ok_and(|rel| self.glob.is_match(rel))
            }
            None => self.glob.is_match(path) || self.glob.is_match(path_key(path)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let a = json!({"jsonrpc": "2.0", "id": 1, "result": "héllo"});
        let b = json!({"jsonrpc": "2.0", "method": "x"});
        let mut bytes = encode(&a);
        assert!(bytes.starts_with(b"Content-Length: "));
        bytes.extend(encode(&b));
        let mut r = BufReader::new(&bytes[..]);
        assert_eq!(read_message(&mut r).await.unwrap(), Some(a));
        assert_eq!(read_message(&mut r).await.unwrap(), Some(b));
        assert_eq!(read_message(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn length_counts_bytes_and_other_headers_are_ignored() {
        let body = r#"{"a":"é"}"#;
        let raw = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut r = BufReader::new(raw.as_bytes());
        assert_eq!(read_message(&mut r).await.unwrap(), Some(json!({"a": "é"})));
    }

    #[tokio::test]
    async fn truncated_input_is_an_error() {
        let mut r = BufReader::new(&b"Content-Length: 10\r\n\r\n{}"[..]);
        assert!(read_message(&mut r).await.is_err());
        let mut r = BufReader::new(&b"Content-Length: 10\r\n"[..]);
        assert!(read_message(&mut r).await.is_err());
        let mut r = BufReader::new(&b"Content-Length: x\r\n\r\n"[..]);
        assert!(read_message(&mut r).await.is_err());
    }

    #[tokio::test]
    async fn wait_debounces_and_times_out() {
        let (tx, mut rx) = broadcast::channel(8);
        let path = PathBuf::from("/nonexistent/a.rs");
        // Nothing arrives: returns at the timeout.
        let t = Instant::now();
        wait_for(&mut rx, &path, Duration::from_millis(50), |_| false).await;
        assert!(t.elapsed() >= Duration::from_millis(50));
        // A push arrives: returns after the debounce, well before the timeout.
        tx.send(path_key(&path)).unwrap();
        let t = Instant::now();
        wait_for(&mut rx, &path, Duration::from_secs(5), |_| false).await;
        assert!(t.elapsed() < Duration::from_secs(2));
        // An empty push waits longer for the follow-up real list.
        tx.send(path_key(&path)).unwrap();
        let t = Instant::now();
        wait_for(&mut rx, &path, Duration::from_secs(5), |_| true).await;
        assert!(t.elapsed() >= EMPTY_DEBOUNCE, "{:?}", t.elapsed());
        // ... and a real list arriving after it ends the wait one debounce later.
        let tx2 = tx.clone();
        let key = path_key(&path);
        tokio::spawn(async move {
            tx2.send(key.clone()).unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            tx2.send(key).unwrap();
        });
        let t = Instant::now();
        let n = std::sync::atomic::AtomicUsize::new(0);
        wait_for(&mut rx, &path, Duration::from_secs(5), |_| {
            n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
        })
        .await;
        let took = t.elapsed();
        assert!(
            took >= Duration::from_millis(300) + DEBOUNCE
                && took < Duration::from_millis(300) + EMPTY_DEBOUNCE,
            "{took:?}"
        );
    }
}
