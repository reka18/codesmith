//! Language servers, as in OpenCode: after the agent writes or edits a file,
//! the servers for that file are told about it and the errors they report
//! are added to the tool result so the model can fix them.
//!
//! Off unless `"lsp"` is set in the config (OpenCode's default). Servers
//! start lazily, once per (server, workspace root), the first time a
//! matching file is touched; one that fails to start is not retried. Waits
//! are bounded so a slow or missing server never holds up an edit for long.

pub mod client;
pub mod server;

use client::{Client, Launch};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// `lsp` in codesmith.json: `true` for the built-in servers, or an object of
/// overrides and custom servers (which keeps the built-ins on).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Setting {
    Enabled(bool),
    Custom(BTreeMap<String, Entry>),
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Entry {
    pub disabled: bool,
    pub command: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    /// Sent as `initializationOptions` and returned for `workspace/configuration`.
    pub initialization: Option<Value>,
}

/// The servers a config enables: none unless `lsp` is set. A custom server
/// needs `command` and `extensions`; an override of a built-in may set just
/// the fields it changes.
pub fn resolve(setting: Option<&Setting>) -> Vec<server::Info> {
    match setting {
        None | Some(Setting::Enabled(false)) => vec![],
        Some(Setting::Enabled(true)) => server::builtins(),
        Some(Setting::Custom(map)) => {
            let mut list = server::builtins();
            for (id, e) in map {
                let at = list.iter().position(|s| &s.id == id);
                if e.disabled {
                    if let Some(i) = at {
                        list.remove(i);
                    }
                    continue;
                }
                let base = at.map(|i| list[i].clone());
                let mut env = base.as_ref().map(|b| b.env.clone()).unwrap_or_default();
                env.extend(e.env.clone());
                let s = server::Info {
                    id: id.clone(),
                    extensions: e
                        .extensions
                        .clone()
                        .or_else(|| base.as_ref().map(|b| b.extensions.clone()))
                        .unwrap_or_default(),
                    root: base
                        .as_ref()
                        .map_or(server::Root::Project, |b| b.root.clone()),
                    spawn: match (&e.command, &base) {
                        (Some(c), _) => server::Spawn::Fixed(c.clone()),
                        (None, Some(b)) => b.spawn.clone(),
                        (None, None) => server::Spawn::Fixed(vec![]),
                    },
                    env,
                    initialization: e
                        .initialization
                        .clone()
                        .or_else(|| base.as_ref().and_then(|b| b.initialization.clone())),
                };
                match at {
                    Some(i) => list[i] = s,
                    None => list.push(s),
                }
            }
            list
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Diagnostic {
    pub range: Range,
    /// 1 error, 2 warning, 3 information, 4 hint; missing means error.
    pub severity: Option<u8>,
    pub message: String,
    pub source: Option<String>,
}

impl Diagnostic {
    pub fn is_error(&self) -> bool {
        self.severity.unwrap_or(1) == 1
    }

    /// `ERROR [line:col] message`, 1-based, as OpenCode prints it.
    pub fn pretty(&self) -> String {
        let sev = match self.severity.unwrap_or(1) {
            2 => "WARN",
            3 => "INFO",
            4 => "HINT",
            _ => "ERROR",
        };
        format!(
            "{sev} [{}:{}] {}",
            self.range.start.line + 1,
            self.range.start.character + 1,
            self.message
        )
    }
}

/// At most this many errors are listed per file.
pub const MAX_PER_FILE: usize = 20;
/// At most this many other files are listed after a `write`.
pub const MAX_OTHER_FILES: usize = 5;

fn block(file: &Path, errors: &[&Diagnostic]) -> String {
    let lines: Vec<String> = errors
        .iter()
        .take(MAX_PER_FILE)
        .map(|d| d.pretty())
        .collect();
    let more = if errors.len() > MAX_PER_FILE {
        format!("\n... and {} more", errors.len() - MAX_PER_FILE)
    } else {
        String::new()
    };
    format!(
        "<diagnostics file=\"{}\">\n{}{more}\n</diagnostics>",
        file.display(),
        lines.join("\n")
    )
}

/// The note added to a tool result: errors in `file`, and with `others`
/// (the `write` tool) errors in up to [`MAX_OTHER_FILES`] other files.
/// Warnings and hints are left out, as in OpenCode.
pub fn report(
    diagnostics: &BTreeMap<PathBuf, Vec<Diagnostic>>,
    file: &Path,
    others: bool,
) -> String {
    let key = path_key(file);
    let mut out = String::new();
    let mut listed = 0;
    for (path, list) in diagnostics {
        let errors: Vec<&Diagnostic> = list.iter().filter(|d| d.is_error()).collect();
        if errors.is_empty() {
            continue;
        }
        if *path == key {
            out.push_str("\n\nLSP errors detected in this file, please fix:\n");
            out.push_str(&block(file, &errors));
            continue;
        }
        if !others || listed >= MAX_OTHER_FILES {
            continue;
        }
        listed += 1;
        out.push_str("\n\nLSP errors detected in other files:\n");
        out.push_str(&block(path, &errors));
    }
    out
}

/// How long to wait, at most, at each step.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Spawn and `initialize` (OpenCode: 45 s). Startup runs in the
    /// background, so this bounds the server, not the edit.
    pub initialize: Duration,
    /// How long a touch waits for a server that is still starting before
    /// going on without it; it is used on later touches once ready.
    pub startup_wait: Duration,
    /// Waiting for diagnostics after a change (OpenCode: 3 s).
    pub diagnostics: Duration,
    /// Requests from the `lsp` tool.
    pub request: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            initialize: Duration::from_secs(45),
            startup_wait: Duration::from_secs(5),
            diagnostics: Duration::from_secs(3),
            request: Duration::from_secs(20),
        }
    }
}

#[derive(Clone)]
enum State {
    Starting,
    Ready(Arc<Client>),
    Broken,
}

/// The language servers of one session.
pub struct Lsp {
    servers: Vec<server::Info>,
    project: PathBuf,
    /// (server id, root) → its state, so each starts at most once.
    slots: Mutex<HashMap<(String, PathBuf), watch::Receiver<State>>>,
    pub timeouts: Timeouts,
}

impl Lsp {
    /// `None` when the config turns language servers off.
    pub fn new(setting: Option<&Setting>, project: PathBuf) -> Option<Lsp> {
        let servers = resolve(setting);
        (!servers.is_empty()).then(|| Lsp {
            servers,
            project,
            slots: Mutex::new(HashMap::new()),
            timeouts: Timeouts::default(),
        })
    }

    /// Servers that handle `file`, with the root each would use. Files
    /// outside the project get none.
    fn matching(&self, file: &Path) -> Vec<(&server::Info, PathBuf)> {
        if !file.starts_with(&self.project) {
            return vec![];
        }
        let ext = file
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        self.servers
            .iter()
            .filter(|s| s.extensions.iter().any(|e| *e == ext || *e == name))
            .filter_map(|s| s.root.find(file, &self.project).map(|r| (s, r)))
            .collect()
    }

    /// Start (once) or look up the server for `(s, root)`.
    fn slot(&self, s: &server::Info, root: PathBuf) -> watch::Receiver<State> {
        let key = (s.id.clone(), root.clone());
        let mut slots = self.slots.lock().unwrap();
        if let Some(rx) = slots.get(&key) {
            return rx.clone();
        }
        let (tx, rx) = watch::channel(State::Starting);
        slots.insert(key, rx.clone());
        let s = s.clone();
        let project = self.project.clone();
        let timeout = self.timeouts.initialize;
        tokio::spawn(async move {
            let detect = {
                let (s, root, project) = (s.clone(), root.clone(), project.clone());
                tokio::task::spawn_blocking(move || s.spawn.detect(&root, &project))
            };
            let state = match detect.await.ok().flatten() {
                None => State::Broken,
                Some(mut launch) => {
                    let mut env = s.env.clone();
                    env.extend(std::mem::take(&mut launch.env));
                    let launch = Launch {
                        env,
                        initialization: s.initialization.clone().or(launch.initialization),
                        ..launch
                    };
                    match Client::start(&s.id, &root, launch, s.id == "typescript", timeout).await {
                        Ok(c) => State::Ready(c),
                        Err(_) => State::Broken,
                    }
                }
            };
            let _ = tx.send(state);
        });
        rx
    }

    /// The ready clients for `file`, starting any that are not yet running
    /// and waiting up to `wait` for those still starting.
    async fn clients(&self, file: &Path, wait: Duration) -> Vec<Arc<Client>> {
        let rxs: Vec<_> = self
            .matching(file)
            .into_iter()
            .map(|(s, root)| self.slot(s, root))
            .collect();
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + wait;
        for mut rx in rxs {
            let ready =
                tokio::time::timeout_at(deadline, rx.wait_for(|s| !matches!(s, State::Starting)))
                    .await;
            if let Ok(Ok(state)) = ready {
                if let State::Ready(c) = &*state {
                    out.push(c.clone());
                }
            }
        }
        out
    }

    /// Whether some server can handle `file` (starting it if needed).
    pub async fn has_clients(&self, file: &Path) -> bool {
        !self
            .clients(file, self.timeouts.initialize)
            .await
            .is_empty()
    }

    /// Tell the servers for `file` that it changed and, with `wait`, wait
    /// (bounded) for the diagnostics they publish in response.
    pub async fn touch(&self, file: &Path, wait: bool) {
        let clients = self.clients(file, self.timeouts.startup_wait).await;
        let waits = clients.into_iter().map(|c| async move {
            let mut rx = c.subscribe();
            if c.open(file).await.is_ok() && wait {
                c.wait(&mut rx, file, self.timeouts.diagnostics).await;
            }
        });
        futures_util::future::join_all(waits).await;
    }

    /// All diagnostics from every running server, merged per file.
    pub fn diagnostics(&self) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        let mut out: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
        for c in self.running() {
            for (path, list) in c.diagnostics() {
                out.entry(path).or_default().extend(list);
            }
        }
        out
    }

    fn running(&self) -> Vec<Arc<Client>> {
        self.slots
            .lock()
            .unwrap()
            .values()
            .filter_map(|rx| match &*rx.borrow() {
                State::Ready(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    /// Send a request to the servers for `file` (or to every running server
    /// when `file` is `None`) and collect the non-empty results.
    pub async fn request(&self, file: Option<&Path>, method: &str, params: Value) -> Vec<Value> {
        let clients = match file {
            Some(f) => self.clients(f, self.timeouts.startup_wait).await,
            None => self.running(),
        };
        let calls = clients.iter().map(|c| {
            let params = params.clone();
            async move {
                c.request(method, params, self.timeouts.request)
                    .await
                    .unwrap_or(Value::Null)
            }
        });
        futures_util::future::join_all(calls)
            .await
            .into_iter()
            .filter(|v| !v.is_null())
            .collect()
    }

    /// Shut every running server down gracefully.
    pub async fn shutdown(&self) {
        let clients = self.running();
        futures_util::future::join_all(clients.iter().map(|c| c.shutdown())).await;
        self.slots.lock().unwrap().clear();
    }
}

/// Paths are compared canonicalized, since servers may report symlinks
/// resolved (`/var` vs `/private/var` on macOS).
pub fn path_key(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `file://` URI for an absolute path, percent-encoding what needs it.
pub fn path_to_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = if s.starts_with('/') {
        s
    } else {
        format!("/{s}")
    };
    let mut out = String::from("file://");
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path of a `file://` URI.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = rest
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8(out).ok()?;
    // `/C:/x` on Windows.
    let s = match s.as_bytes() {
        [b'/', d, b':', ..] if d.is_ascii_alphabetic() && cfg!(windows) => s[1..].to_string(),
        _ => s,
    };
    Some(PathBuf::from(s))
}

/// The LSP language id for a file (OpenCode's `LANGUAGE_EXTENSIONS`).
pub fn language_id(p: &Path) -> &'static str {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name == "Dockerfile" {
        return "dockerfile";
    }
    if name == "Makefile" || name == "makefile" {
        return "makefile";
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "abap" => "abap",
        "bat" => "bat",
        "bib" => "bibtex",
        "c" => "c",
        "h" => "c",
        "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" => "cpp",
        "clj" | "cljs" | "cljc" | "edn" => "clojure",
        "coffee" => "coffeescript",
        "cs" | "csx" => "csharp",
        "css" => "css",
        "d" => "d",
        "dart" => "dart",
        "diff" | "patch" => "diff",
        "dockerfile" => "dockerfile",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "fs" | "fsi" | "fsx" | "fsscript" => "fsharp",
        "gleam" => "gleam",
        "go" => "go",
        "groovy" => "groovy",
        "hbs" | "handlebars" => "handlebars",
        "hs" | "lhs" => "haskell",
        "html" | "htm" => "html",
        "ini" => "ini",
        "java" => "java",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "json" => "json",
        "jsonc" => "jsonc",
        "jl" => "julia",
        "kt" | "kts" => "kotlin",
        "tex" => "latex",
        "less" => "less",
        "lua" => "lua",
        "md" | "markdown" => "markdown",
        "nix" => "nix",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "objc" => "objective-c",
        "objcpp" => "objective-cpp",
        "ml" => "ocaml",
        "mli" => "ocaml.interface",
        "pl" | "pm" => "perl",
        "php" => "php",
        "ps1" | "psm1" => "powershell",
        "prisma" => "prisma",
        "py" | "pyi" => "python",
        "r" | "R" => "r",
        "rb" | "rake" | "gemspec" | "ru" => "ruby",
        "rs" => "rust",
        "scss" => "scss",
        "sass" => "sass",
        "scala" => "scala",
        "sh" | "bash" | "zsh" | "ksh" => "shellscript",
        "sql" => "sql",
        "svelte" => "svelte",
        "swift" => "swift",
        "tf" | "tfvars" => "terraform",
        "toml" => "toml",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "typ" | "typc" => "typst",
        "vue" => "vue",
        "astro" => "astro",
        "xml" => "xml",
        "yaml" | "yml" => "yaml",
        "zig" | "zon" => "zig",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn diag(line: u32, col: u32, severity: Option<u8>, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: col,
                },
                end: Position::default(),
            },
            severity,
            message: msg.into(),
            source: None,
        }
    }

    #[test]
    fn config_shapes() {
        assert!(resolve(None).is_empty());
        assert!(resolve(Some(&Setting::Enabled(false))).is_empty());
        let all = resolve(Some(&Setting::Enabled(true)));
        assert!(all.len() > 30);
        let s: Setting = serde_json::from_value(json!({
            "rust": {"command": ["ra-wrapper"], "env": {"RUST_LOG": "debug"}},
            "typescript": {"disabled": true},
            "custom": {"command": ["my-ls", "--stdio"], "extensions": [".foo"],
                       "initialization": {"x": 1}}
        }))
        .unwrap();
        let list = resolve(Some(&s));
        assert_eq!(list.len(), all.len());
        assert!(!list.iter().any(|s| s.id == "typescript"));
        let rust = list.iter().find(|s| s.id == "rust").unwrap();
        assert!(matches!(&rust.spawn, server::Spawn::Fixed(c) if c == &["ra-wrapper"]));
        assert!(matches!(rust.root, server::Root::Rust));
        assert_eq!(rust.extensions, [".rs"]);
        assert_eq!(rust.env["RUST_LOG"], "debug");
        let custom = list.iter().find(|s| s.id == "custom").unwrap();
        assert_eq!(custom.extensions, [".foo"]);
        assert_eq!(custom.initialization, Some(json!({"x": 1})));
        // `{}` keeps the built-ins on.
        let s: Setting = serde_json::from_value(json!({})).unwrap();
        assert_eq!(resolve(Some(&s)).len(), all.len());
        assert!(Lsp::new(None, "/".into()).is_none());
    }

    #[test]
    fn pretty_matches_opencode() {
        assert_eq!(diag(0, 4, Some(1), "bad").pretty(), "ERROR [1:5] bad");
        assert_eq!(diag(9, 0, Some(2), "meh").pretty(), "WARN [10:1] meh");
        assert_eq!(diag(0, 0, None, "x").pretty(), "ERROR [1:1] x");
        let d: Diagnostic = serde_json::from_value(json!({
            "range": {"start": {"line": 2, "character": 3}, "end": {"line": 2, "character": 5}},
            "severity": 4, "message": "hint", "code": 7
        }))
        .unwrap();
        assert_eq!(d.pretty(), "HINT [3:4] hint");
    }

    #[test]
    fn report_lists_errors_only_and_bounds_output() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        let mut all = BTreeMap::new();
        let mut mine: Vec<Diagnostic> = (0..25)
            .map(|i| diag(i, 0, Some(1), &format!("e{i}")))
            .collect();
        mine.push(diag(0, 0, Some(2), "just a warning"));
        all.insert(path_key(&file), mine);
        for i in 0..7 {
            all.insert(
                dir.path().join(format!("o{i}.rs")),
                vec![diag(0, 0, Some(1), "other")],
            );
        }
        all.insert(dir.path().join("w.rs"), vec![diag(0, 0, Some(2), "warn")]);
        let edit = report(&all, &file, false);
        assert!(edit.starts_with(&format!(
            "\n\nLSP errors detected in this file, please fix:\n<diagnostics file=\"{}\">\nERROR [1:1] e0\n",
            file.display()
        )));
        assert!(edit.contains("ERROR [20:1] e19\n... and 5 more\n</diagnostics>"));
        assert!(!edit.contains("e20") && !edit.contains("warning") && !edit.contains("other"));
        let write = report(&all, &file, true);
        assert_eq!(
            write.matches("LSP errors detected in other files").count(),
            5
        );
        assert!(!write.contains("w.rs"));
        assert_eq!(report(&BTreeMap::new(), &file, true), "");
    }

    #[test]
    fn uris_round_trip() {
        let p = Path::new("/tmp/a b/ü#1.rs");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///tmp/a%20b/%C3%BC%231.rs");
        assert_eq!(uri_to_path(&uri).unwrap(), p);
        assert_eq!(uri_to_path("file:///x/y.rs").unwrap(), Path::new("/x/y.rs"));
        assert_eq!(uri_to_path("http://x"), None);
    }

    #[test]
    fn language_ids() {
        assert_eq!(language_id(Path::new("a.rs")), "rust");
        assert_eq!(language_id(Path::new("a.tsx")), "typescriptreact");
        assert_eq!(language_id(Path::new("x/Dockerfile")), "dockerfile");
        assert_eq!(language_id(Path::new("a.unknown")), "plaintext");
    }

    #[tokio::test]
    async fn files_outside_the_project_or_unmatched_get_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let lsp = Lsp::new(Some(&Setting::Enabled(true)), dir.path().into()).unwrap();
        assert!(lsp.matching(Path::new("/elsewhere/a.rs")).is_empty());
        assert!(lsp.matching(&dir.path().join("notes.txt")).is_empty());
        assert!(!lsp.matching(&dir.path().join("a.rs")).is_empty());
    }
}
