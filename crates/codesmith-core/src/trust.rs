//! Project config trust: what a project's `codesmith.json[c]` (and
//! `.codesmith/codesmith.json[c]`) may do with this machine's secrets.
//!
//! A project file can name `{env:VAR}` and `{file:path}` substitutions,
//! point a provider, MCP server or OpenAPI API at any URL with any headers,
//! run programs (local MCP servers, custom language servers and
//! formatters, worktree setup, the shell), and read files outside the
//! project (instructions, skills, references). Together those can send a
//! host secret such as `ANTHROPIC_API_KEY` or `~/.aws/credentials` to a URL
//! the repository chose. So [`detect`] lists what a project's files ask
//! for, as [`Capability`]s, and [`crate::config::load_from`] applies them
//! only when the person has trusted that project for at least those
//! capabilities ([`Store`]): otherwise every `{env:}` and `{file:}` in the
//! project's files is empty and [`strip`] removes the rest, so the global
//! config's (or the built-in) values are used.
//!
//! The answer is kept per project (the id saved approvals use, see
//! [`crate::approvals::project_key`]) with the capabilities it covered: a
//! config that later asks for something new (another variable, file, URL,
//! header or program) is asked about again; one that asks for the same or
//! less is not. Global config, `$CODESMITH_CONFIG` and
//! `$CODESMITH_CONFIG_CONTENT` are the person's own and never asked about.
//!
//! This goes beyond OpenCode, which applies project config as trusted.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

/// What kind of thing a project's config asks to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Read an environment variable (`{env:}`, a provider's `env` list).
    Env,
    /// Read a file (`{file:}`, an instruction, skill or reference outside
    /// the project, an OpenAPI spec file).
    File,
    /// Send requests to a URL.
    Url,
    /// Set a request header (by name).
    Header,
    /// Set a credential (an `apiKey`, a token, OAuth settings).
    Credential,
    /// Run a program.
    Program,
    /// Change a setting that decides where requests go or what they carry
    /// (a provider's region or account, an MCP server's environment).
    Setting,
}

/// One thing a project's config asks to do: its kind, what it is for
/// (`provider anthropic`, `MCP server docs`; empty for a substitution) and
/// the variable, file, URL, header, program or setting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Capability {
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    pub value: String,
}

impl Capability {
    fn new(kind: Kind, owner: &str, value: impl Into<String>) -> Capability {
        Capability {
            kind,
            owner: owner.to_string(),
            value: value.into(),
        }
    }
}

/// What a project's config files ask for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Request {
    /// The project root.
    pub root: PathBuf,
    /// The project files that ask.
    pub files: Vec<PathBuf>,
    pub capabilities: BTreeSet<Capability>,
}

impl Request {
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// A short hash of the capabilities, which names a remembered answer.
    pub fn hash(&self) -> String {
        hash(&self.capabilities)
    }

    /// The prompt's summary: what the config wants to do, by kind, with
    /// control characters stripped and long lists cut short.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "This project's config ({}) wants to:\n",
            safe(&self.root.display().to_string())
        );
        for (kind, label) in LABELS {
            let items: Vec<String> = self
                .capabilities
                .iter()
                .filter(|c| c.kind == kind)
                .map(|c| {
                    if c.owner.is_empty() {
                        safe(&c.value)
                    } else {
                        format!("{} ({})", safe(&c.value), safe(&c.owner))
                    }
                })
                .collect();
            if items.is_empty() {
                continue;
            }
            out.push_str(&format!("  {label}: {}\n", list(&items, LIST)));
        }
        let files: Vec<String> = self
            .files
            .iter()
            .map(|f| safe(&shown(f, &self.root)))
            .collect();
        out.push_str(&format!("  (from {})\n", list(&files, LIST)));
        out
    }
}

const LABELS: [(Kind, &str); 7] = [
    (Kind::Env, "read environment variables"),
    (Kind::File, "read files"),
    (Kind::Url, "send requests to"),
    (Kind::Header, "set request headers"),
    (Kind::Credential, "set credentials"),
    (Kind::Program, "run programs"),
    (Kind::Setting, "change settings"),
];

/// How many items of one kind the summary and notice name.
const LIST: usize = 12;
/// How long one shown value may be.
const VALUE: usize = 160;

fn list(items: &[String], max: usize) -> String {
    let mut s = items
        .iter()
        .take(max)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > max {
        s.push_str(&format!(" and {} more", items.len() - max));
    }
    s
}

/// `s` with control, bidirectional and zero-width characters replaced by
/// `?`, cut to [`VALUE`] characters: a config value shown on a terminal
/// cannot move the cursor, recolour or reorder the prompt.
pub fn safe(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == VALUE {
            out.push('…');
            break;
        }
        let hidden = c.is_control() || crate::unicode::format_char(c);
        out.push(if hidden { '?' } else { c });
    }
    out
}

fn hash(caps: &BTreeSet<Capability>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for c in caps {
        h.update(serde_json::to_string(c).unwrap_or_default().as_bytes());
        h.update(b"\n");
    }
    h.finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `p` as shown: `./x` inside `root`, `~/x` inside the home folder, else
/// absolute. Remembered answers name files this way, so a worktree or
/// another clone of the project matches.
pub fn shown(p: &Path, root: &Path) -> String {
    let p = normal(p);
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    for r in [&real_root, &root.to_path_buf()] {
        if let Ok(rel) = p.strip_prefix(r) {
            return format!("./{}", rel.display());
        }
    }
    if let Some(home) = dirs::home_dir() {
        let real = home.canonicalize().unwrap_or_else(|_| home.clone());
        for h in [&real, &home] {
            if let Ok(rel) = p.strip_prefix(h) {
                return format!("~/{}", rel.display());
            }
        }
    }
    p.display().to_string()
}

/// `p` without `.` and `..` components (lexically), and through its real
/// path when it exists, so a symlink into the home folder is shown (and
/// judged) where it leads.
fn normal(p: &Path) -> PathBuf {
    let mut lexical = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            c => lexical.push(c),
        }
    }
    // The deepest folder that exists, through its real path, then the rest.
    for dir in lexical.ancestors() {
        if let Ok(real) = dir.canonicalize() {
            let rest = lexical.strip_prefix(dir).unwrap_or(Path::new(""));
            return if rest.as_os_str().is_empty() {
                real
            } else {
                real.join(rest)
            };
        }
    }
    lexical
}

/// `raw` (a config path: `~/`, absolute, or relative to `base`) resolved.
fn resolve(raw: &str, base: &Path) -> PathBuf {
    let home = || dirs::home_dir().unwrap_or_default();
    if raw == "~" {
        return home();
    }
    match raw.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => base.join(raw),
    }
}

/// Whether `raw` (resolved against `base`) leads outside `root`: a host
/// file the project does not hold. A symlink inside the project that leads
/// out counts as outside.
fn outside(raw: &str, base: &Path, root: &Path) -> Option<String> {
    let p = normal(&resolve(raw, base));
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    (!p.starts_with(&real_root) && !p.starts_with(root)).then(|| shown(&p, root))
}

// --- Detection ----------------------------------------------------------

/// The `{env:NAME}` and `{file:path}` substitutions `text` makes outside
/// comments, as [`crate::config::substitute`] would make them: files are
/// found after the variables are put in (`{file:{env:HOME}/x}` reads
/// `$HOME/x`), and named as [`shown`] names them.
pub fn substitutions(text: &str, base: &Path, root: &Path) -> Vec<Capability> {
    let stripped = crate::config::strip_jsonc(text);
    let env = regex::Regex::new(r"\{env:([^}]+)\}").unwrap();
    let mut out: Vec<Capability> = env
        .captures_iter(&stripped)
        .map(|c| Capability::new(Kind::Env, "", &c[1]))
        .collect();
    let with_env = env.replace_all(&stripped, |c: &regex::Captures| {
        std::env::var(&c[1]).unwrap_or_default()
    });
    let file = regex::Regex::new(r"\{file:([^}]+)\}").unwrap();
    for c in file.captures_iter(&with_env) {
        let p = resolve(&c[1], base);
        out.push(Capability::new(Kind::File, "", shown(&p, root)));
    }
    out
}

/// Detect what one project file's parsed value asks for (see [`audit`]).
pub fn structure(v: &Value, root: &Path) -> Vec<Capability> {
    let mut v = v.clone();
    let mut out = Vec::new();
    audit(&mut v, &Value::Null, root, false, &mut out, &mut Vec::new());
    out
}

/// Remove from a project file's value what [`structure`] detects, noting
/// each owner in `skipped`. `lower` is the config merged so far: an MCP
/// server or OpenAPI API the person's own config also defines keeps the
/// person's URL, headers and command; one only the project defines is
/// removed.
pub fn strip(v: &mut Value, lower: &Value, root: &Path, skipped: &mut Vec<String>) {
    audit(v, lower, root, true, &mut Vec::new(), skipped);
}

const URL_KEYS: [&str; 5] = ["baseURL", "baseUrl", "base_url", "endpoint", "url"];
const CREDENTIAL_KEYS: [&str; 7] = [
    "apiKey",
    "api_key",
    "token",
    "accessToken",
    "access_token",
    "serviceKey",
    "service_key",
];
/// Provider settings that change neither where requests go nor what
/// credentials they carry.
const SAFE_SETTINGS: [&str; 16] = [
    "cache",
    "encoding",
    "transport",
    "discovery",
    "headerTimeout",
    "header_timeout",
    "chunkTimeout",
    "chunk_timeout",
    "apiVersion",
    "api_version",
    "useDeploymentBasedUrls",
    "use_deployment_based_urls",
    "apiKeyHeader",
    "api_key_header",
    "queryParams",
    "query_params",
];

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn program(v: &Value) -> String {
    match v {
        Value::Array(a) => a.iter().map(text).collect::<Vec<_>>().join(" "),
        other => text(other),
    }
}

/// Walk a project file's value: note each capability in `out` and, with
/// `strip`, remove it, naming its owner in `skipped`.
fn audit(
    v: &mut Value,
    lower: &Value,
    root: &Path,
    strip: bool,
    out: &mut Vec<Capability>,
    skipped: &mut Vec<String>,
) {
    let Some(top) = v.as_object_mut() else {
        return;
    };
    let skip = |owner: String, skipped: &mut Vec<String>| {
        if strip && !skipped.contains(&owner) {
            skipped.push(owner);
        }
    };
    // Providers.
    if let Some(Value::Object(providers)) = top.get_mut("providers") {
        for (pid, p) in providers.iter_mut() {
            let Some(p) = p.as_object_mut() else { continue };
            let owner = format!("provider {pid}");
            let before = out.len();
            if let Some(env) = p.get("env") {
                for name in env.as_array().into_iter().flatten() {
                    out.push(Capability::new(Kind::Env, &owner, text(name)));
                }
                if strip {
                    p.remove("env");
                }
            }
            if let Some(Value::Object(s)) = p.get_mut("settings") {
                let keys: Vec<String> = s
                    .keys()
                    .filter(|k| !SAFE_SETTINGS.contains(&k.as_str()))
                    .cloned()
                    .collect();
                for k in keys {
                    let val = &s[&k];
                    if URL_KEYS.contains(&k.as_str()) {
                        out.push(Capability::new(Kind::Url, &owner, text(val)));
                    } else if CREDENTIAL_KEYS.contains(&k.as_str()) {
                        out.push(Capability::new(Kind::Credential, &owner, k.clone()));
                    } else if k == "headers" {
                        headers(val, &owner, out);
                    } else {
                        out.push(Capability::new(
                            Kind::Setting,
                            &owner,
                            format!("{k}={}", text(val)),
                        ));
                    }
                    if strip {
                        s.remove(&k);
                    }
                }
            }
            // Everything else, at any depth: headers, URLs, credentials.
            let kept: Vec<(String, Value)> = ["settings", "env", "swap"]
                .iter()
                .filter_map(|k| p.remove(*k).map(|v| (k.to_string(), v)))
                .collect();
            deep_map(p, &owner, strip, out);
            p.extend(kept);
            if out.len() > before {
                skip(owner, skipped);
            }
        }
    }
    // MCP servers.
    if let Some(Value::Object(servers)) = top.get_mut("mcp").and_then(|m| m.get_mut("servers")) {
        let mut gone = Vec::new();
        for (name, s) in servers.iter_mut() {
            let Some(s) = s.as_object_mut() else { continue };
            let owner = format!("MCP server {name}");
            let before = out.len();
            if let Some(u) = s.get("url") {
                out.push(Capability::new(Kind::Url, &owner, text(u)));
            }
            if let Some(h) = s.get("headers") {
                headers(h, &owner, out);
            }
            if let Some(o) = s.get("oauth").filter(|o| **o != Value::Bool(false)) {
                out.push(Capability::new(
                    Kind::Credential,
                    &owner,
                    format!("oauth {o}"),
                ));
            }
            if let Some(c) = s.get("command") {
                out.push(Capability::new(Kind::Program, &owner, program(c)));
            }
            if let Some(Value::Object(env)) = s.get("environment") {
                for (k, val) in env {
                    out.push(Capability::new(
                        Kind::Setting,
                        &owner,
                        format!("environment {k}={}", text(val)),
                    ));
                }
            }
            if let Some(c) = s.get("cwd") {
                out.push(Capability::new(
                    Kind::Setting,
                    &owner,
                    format!("cwd={}", text(c)),
                ));
            }
            if out.len() > before && strip {
                if lower
                    .pointer(&format!("/mcp/servers/{}", pointer(name)))
                    .is_some()
                {
                    for k in [
                        "type",
                        "url",
                        "headers",
                        "oauth",
                        "command",
                        "environment",
                        "cwd",
                    ] {
                        s.remove(k);
                    }
                } else {
                    gone.push(name.clone());
                }
                skip(owner, skipped);
            }
        }
        for n in gone {
            servers.remove(&n);
        }
    }
    // OpenAPI APIs: every request goes to the spec's servers or `base_url`.
    if let Some(Value::Object(apis)) = top.get_mut("openapi") {
        let mut gone = Vec::new();
        for (name, a) in apis.iter_mut() {
            let Some(a) = a.as_object_mut() else { continue };
            let owner = format!("OpenAPI API {name}");
            let before = out.len();
            match a.get("spec") {
                Some(Value::String(s)) if s.starts_with("http://") || s.starts_with("https://") => {
                    out.push(Capability::new(Kind::Url, &owner, s.clone()));
                }
                Some(Value::String(s)) if !s.trim_start().starts_with('{') => {
                    out.push(Capability::new(
                        Kind::File,
                        &owner,
                        shown(&resolve(s, root), root),
                    ));
                }
                Some(spec) => {
                    let doc = match spec {
                        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                        v => v.clone(),
                    };
                    for srv in doc["servers"].as_array().into_iter().flatten() {
                        out.push(Capability::new(Kind::Url, &owner, text(&srv["url"])));
                    }
                }
                None => {}
            }
            if let Some(u) = a.get("base_url").or_else(|| a.get("baseURL")) {
                out.push(Capability::new(Kind::Url, &owner, text(u)));
            }
            if let Some(h) = a.get("headers") {
                headers(h, &owner, out);
            }
            if let Some(Value::Object(auth)) = a.get("auth") {
                for k in auth.keys() {
                    out.push(Capability::new(
                        Kind::Credential,
                        &owner,
                        format!("auth {k}"),
                    ));
                }
            }
            if out.len() > before && strip {
                if lower
                    .pointer(&format!("/openapi/{}", pointer(name)))
                    .is_some()
                {
                    for k in ["spec", "base_url", "baseURL", "headers", "auth"] {
                        a.remove(k);
                    }
                } else {
                    gone.push(name.clone());
                }
                skip(owner, skipped);
            }
        }
        for n in gone {
            apis.remove(&n);
        }
    }
    // Single URLs.
    for (key, owner) in [("websearch", "web search"), ("catalog", "model catalog")] {
        if let Some(Value::Object(o)) = top.get_mut(key) {
            if let Some(u) = o.get("url") {
                out.push(Capability::new(Kind::Url, owner, text(u)));
                if strip {
                    o.remove("url");
                    skip(owner.to_string(), skipped);
                }
            }
        }
    }
    // Programs.
    // `shell` is a path, or an object with a `path` (and `env`, whose
    // `pass` only the global config may set and whose `remove` only
    // restricts).
    let shell = match top.get("shell") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(o)) => o.get("path").and_then(Value::as_str).map(str::to_string),
        _ => None,
    };
    if let Some(sh) = shell.filter(|s| s.contains('/') || s.contains('\\')) {
        out.push(Capability::new(Kind::Program, "shell tool", sh));
        if strip {
            match top.get_mut("shell") {
                Some(Value::Object(o)) => {
                    o.remove("path");
                }
                _ => {
                    top.remove("shell");
                }
            }
            skip("shell".into(), skipped);
        }
    }
    if let Some(Value::Object(w)) = top.get_mut("worktree") {
        if let Some(s) = w.get("start") {
            out.push(Capability::new(Kind::Program, "worktree setup", text(s)));
            if strip {
                w.remove("start");
                skip("worktree setup".into(), skipped);
            }
        }
    }
    for (key, what, env_key) in [
        ("lsp", "language server", "env"),
        ("formatter", "formatter", "environment"),
    ] {
        let Some(Value::Object(servers)) = top.get_mut(key) else {
            continue;
        };
        for (id, e) in servers.iter_mut() {
            let Some(e) = e.as_object_mut() else { continue };
            let owner = format!("{what} {id}");
            let before = out.len();
            if let Some(c) = e.get("command") {
                out.push(Capability::new(Kind::Program, &owner, program(c)));
            }
            if let Some(Value::Object(env)) = e.get(env_key) {
                for (k, val) in env {
                    out.push(Capability::new(
                        Kind::Setting,
                        &owner,
                        format!("environment {k}={}", text(val)),
                    ));
                }
            }
            if out.len() > before && strip {
                e.remove("command");
                e.remove(env_key);
                skip(owner, skipped);
            }
        }
    }
    // Files outside the project, and instructions from the network.
    for (key, owner) in [("instructions", "instructions"), ("skills", "skills")] {
        let Some(Value::Array(items)) = top.get_mut(key) else {
            continue;
        };
        let before = out.len();
        items.retain(|i| {
            let Some(s) = i.as_str() else { return true };
            let cap = if s.starts_with("http://") || s.starts_with("https://") {
                Some(Capability::new(Kind::Url, owner, s))
            } else {
                outside(s, root, root).map(|p| Capability::new(Kind::File, owner, p))
            };
            match cap {
                Some(c) => {
                    out.push(c);
                    !strip
                }
                None => true,
            }
        });
        if out.len() > before {
            skip(owner.to_string(), skipped);
        }
    }
    if let Some(Value::Object(refs)) = top.get_mut("references") {
        let mut gone = Vec::new();
        for (name, r) in refs.iter() {
            let path = match r {
                Value::String(s) if s.starts_with(['.', '/', '~']) => Some(s.as_str()),
                Value::Object(o) => o.get("path").and_then(Value::as_str),
                _ => None,
            };
            if let Some(p) = path.and_then(|p| outside(p, root, root)) {
                out.push(Capability::new(Kind::File, &format!("reference {name}"), p));
                gone.push(name.clone());
            }
        }
        if strip {
            for n in gone {
                refs.remove(&n);
                skip(format!("reference {n}"), skipped);
            }
        }
    }
}

/// An object key as a JSON pointer token.
fn pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn headers(v: &Value, owner: &str, out: &mut Vec<Capability>) {
    match v {
        Value::Object(h) => {
            for k in h.keys() {
                out.push(Capability::new(Kind::Header, owner, k.clone()));
            }
        }
        Value::Null => {}
        other => out.push(Capability::new(Kind::Header, owner, text(other))),
    }
}

/// Within a provider (outside `settings`): any `headers`, URL or
/// credential key, at any depth (model options, variants, bodies).
fn deep(v: &mut Value, owner: &str, strip: bool, out: &mut Vec<Capability>) {
    match v {
        Value::Object(m) => deep_map(m, owner, strip, out),
        Value::Array(a) => {
            for x in a {
                deep(x, owner, strip, out);
            }
        }
        _ => {}
    }
}

fn deep_map(m: &mut Map<String, Value>, owner: &str, strip: bool, out: &mut Vec<Capability>) {
    let keys: Vec<String> = m.keys().cloned().collect();
    for k in keys {
        let gated = if k.eq_ignore_ascii_case("headers") {
            headers(&m[&k], owner, out);
            true
        } else if URL_KEYS.contains(&k.as_str()) && m[&k].is_string() {
            out.push(Capability::new(Kind::Url, owner, text(&m[&k])));
            true
        } else if CREDENTIAL_KEYS.contains(&k.as_str()) && m[&k].is_string() {
            out.push(Capability::new(Kind::Credential, owner, k.clone()));
            true
        } else {
            false
        };
        if gated {
            if strip {
                m.remove(&k);
            }
        } else if let Some(v) = m.get_mut(&k) {
            deep(v, owner, strip, out);
        }
    }
}

/// What one project file asks for: its substitutions, and what its value
/// (parsed with substitutions left as written, else empty, else filled in)
/// sets.
pub fn detect_file(text: &str, path: &Path, root: &Path) -> Vec<Capability> {
    let base = path.parent().unwrap_or(Path::new("."));
    let mut out = substitutions(text, base, root);
    let literal = crate::config::parse_as(text, base, crate::config::Fill::Literal);
    let value = literal
        .or_else(|_| crate::config::parse_as(text, base, crate::config::Fill::Empty))
        .or_else(|_| crate::config::parse_as(text, base, crate::config::Fill::Env));
    if let Ok(v) = value {
        out.extend(structure(&v, root));
    }
    out
}

/// What the project files `files` (path and text) ask for.
pub fn detect(root: &Path, files: &[(PathBuf, String)]) -> Request {
    let mut req = Request {
        root: root.to_path_buf(),
        ..Default::default()
    };
    for (path, text) in files {
        let caps = detect_file(text, path, root);
        if !caps.is_empty() {
            req.files.push(path.clone());
            req.capabilities.extend(caps);
        }
    }
    req
}

// --- Answers ------------------------------------------------------------

/// Where [`crate::config::load_from`] finds trust answers.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// The answers in this file ([`store_path`] by default).
    Store(PathBuf),
    /// Treat every project as trusted (tests of config loading itself).
    All,
}

/// One remembered answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// `trust_` and a hash of the project and its capabilities.
    pub id: String,
    /// The project's id (as saved approvals are keyed).
    pub project: String,
    /// The folder it was answered in.
    pub root: String,
    pub trusted: bool,
    pub capabilities: Vec<Capability>,
    /// When it was answered (ms since the epoch).
    pub time: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    projects: Vec<Record>,
}

/// The trust answers: `trust.json` beside the session database (where
/// saved approvals are), 0600.
pub fn store_path() -> PathBuf {
    let db = crate::store::default_path();
    match db.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.join("trust.json"),
        None => crate::paths::data_dir().join("trust.json"),
    }
}

/// The answers in one file.
#[derive(Debug, Clone)]
pub struct Store {
    pub path: PathBuf,
}

/// The state of a project's answer for what it asks now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// It asks for nothing that needs trust.
    #[default]
    NotNeeded,
    /// Trusted for everything it asks.
    Trusted,
    /// The person chose not to trust it (for everything it asks).
    Distrusted,
    /// Never answered, or it asks for more than was answered.
    Undecided,
}

/// Answers given in this process that could not be saved (they still hold
/// for the rest of it).
static SESSION: Mutex<Vec<Record>> = Mutex::new(Vec::new());

impl Store {
    pub fn new(path: PathBuf) -> Store {
        Store { path }
    }

    pub fn default_store() -> Store {
        Store::new(store_path())
    }

    /// Every remembered answer. A file another user owns, or one others may
    /// write, is ignored: an answer must come from this user.
    pub fn list(&self) -> Vec<Record> {
        let Ok(meta) = std::fs::symlink_metadata(&self.path) else {
            return vec![];
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            // SAFETY: geteuid has no preconditions.
            if !meta.file_type().is_file()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.permissions().mode() & 0o022 != 0
            {
                return vec![];
            }
        }
        #[cfg(not(unix))]
        let _ = meta;
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str::<StoreFile>(&t).ok())
            .map(|f| f.projects)
            .unwrap_or_default()
    }

    fn save(&self, projects: Vec<Record>) -> Result<()> {
        let f = StoreFile {
            version: 1,
            projects,
        };
        crate::paths::write_private(&self.path, serde_json::to_string_pretty(&f)?.as_bytes())
            .with_context(|| format!("saving {}", self.path.display()))
    }

    /// The state of `req` for `project`.
    pub fn state(&self, project: &str, req: &Request) -> State {
        if req.is_empty() {
            return State::NotNeeded;
        }
        let session = SESSION.lock().unwrap().clone();
        let record = session
            .into_iter()
            .rev()
            .find(|r| r.project == project)
            .or_else(|| self.list().into_iter().find(|r| r.project == project));
        match record {
            Some(r) if req.capabilities.iter().all(|c| r.capabilities.contains(c)) => {
                if r.trusted {
                    State::Trusted
                } else {
                    State::Distrusted
                }
            }
            _ => State::Undecided,
        }
    }

    /// Remember an answer for what `req` asks: a later request for the
    /// same or less gets the same answer. An earlier answer the same way
    /// is widened; one the other way is replaced. When the file cannot be
    /// written the answer holds for this process, and the error is
    /// returned.
    pub fn remember(&self, project: &str, req: &Request, trusted: bool) -> Result<Record> {
        let mut all = self.list();
        let mut caps: BTreeSet<Capability> = req.capabilities.clone();
        if let Some(old) = all
            .iter()
            .find(|r| r.project == project && r.trusted == trusted)
        {
            caps.extend(old.capabilities.iter().cloned());
        }
        all.retain(|r| r.project != project);
        let id = format!("trust_{}", hash_project(project, &caps));
        let record = Record {
            id,
            project: project.to_string(),
            root: req.root.display().to_string(),
            trusted,
            capabilities: caps.into_iter().collect(),
            time: crate::store::now(),
        };
        all.push(record.clone());
        {
            let mut s = SESSION.lock().unwrap();
            s.retain(|r| r.project != project);
            s.push(record.clone());
        }
        self.save(all)?;
        Ok(record)
    }

    /// Forget the answer with this id, or for this project: whether there
    /// was one.
    pub fn revoke(&self, id_or_project: &str) -> Result<bool> {
        let mut all = self.list();
        let n = all.len();
        all.retain(|r| r.id != id_or_project && r.project != id_or_project);
        SESSION
            .lock()
            .unwrap()
            .retain(|r| r.id != id_or_project && r.project != id_or_project);
        if all.len() == n {
            return Ok(false);
        }
        self.save(all)?;
        Ok(true)
    }
}

fn hash_project(project: &str, caps: &BTreeSet<Capability>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(project.as_bytes());
    h.update(b"\n");
    h.update(hash(caps).as_bytes());
    h.finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The project id answers are kept under (as saved approvals are).
pub fn project_key(root: &Path) -> String {
    crate::approvals::project_key(root)
}

// --- What loading did -----------------------------------------------------

/// What [`crate::config::load_from`] did about the project's config.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// What the project's files ask for (empty when nothing needs trust).
    pub request: Request,
    pub state: State,
    /// What was ignored because the project is not trusted: owners such as
    /// `provider x` or `MCP server y`, and the substitutions.
    pub skipped: Vec<String>,
}

impl Outcome {
    /// Whether the project's config was applied as written.
    pub fn trusted(&self) -> bool {
        matches!(self.state, State::NotNeeded | State::Trusted)
    }

    /// The one-line notice naming what was skipped, when anything was.
    pub fn notice(&self) -> Option<String> {
        if self.trusted() {
            return None;
        }
        let mut what: Vec<String> = Vec::new();
        let env: Vec<String> = self
            .request
            .capabilities
            .iter()
            .filter(|c| c.kind == Kind::Env && c.owner.is_empty())
            .map(|c| safe(&c.value))
            .collect();
        if !env.is_empty() {
            what.push(format!("{{env:}} {}", list(&env, 6)));
        }
        let files: Vec<String> = self
            .request
            .capabilities
            .iter()
            .filter(|c| c.kind == Kind::File && c.owner.is_empty())
            .map(|c| safe(&c.value))
            .collect();
        if !files.is_empty() {
            what.push(format!("{{file:}} {}", list(&files, 6)));
        }
        let owners: Vec<String> = self.skipped.iter().map(|s| safe(s)).collect();
        if !owners.is_empty() {
            what.push(list(&owners, 8));
        }
        let why = match self.state {
            State::Distrusted => "you chose not to trust this project's config",
            _ => "this project's config is not trusted",
        };
        Some(format!(
            "{why}, so CodeSmith ignored: {}; global config is used instead. Run `codesmith trust {}` to trust it.",
            what.join("; "),
            quote(&safe(&self.request.root.display().to_string()))
        ))
    }
}

/// `s` quoted for a POSIX shell when it needs it.
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-~+@:".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

static ANNOUNCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static ANNOUNCED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether [`announce`] writes notices to stderr (the terminal UI turns it
/// off and shows the notice itself).
pub fn set_announce(on: bool) {
    ANNOUNCE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Write the outcome's notice to stderr, once per project and request in
/// this process.
pub fn announce(o: &Outcome) {
    let Some(n) = o.notice() else { return };
    if !ANNOUNCE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let key = format!("{}\n{}", o.request.root.display(), o.request.hash());
    {
        let mut seen = ANNOUNCED.lock().unwrap();
        if seen.contains(&key) {
            return;
        }
        seen.push(key);
    }
    eprintln!("\x1b[33mnote:\x1b[0m {n}");
}

// --- Asking ----------------------------------------------------------------

/// Ask whether to trust `req`, on `out`, reading the answer from `input`:
/// `t`/`trust`/`y`/`yes` trusts; anything else, an empty line or the end
/// of input does not.
pub fn ask(req: &Request, input: &mut dyn BufRead, out: &mut dyn Write) -> std::io::Result<bool> {
    write!(
        out,
        "{}\nTrusting lets it use these. Not trusting ignores them: {{env:}} and {{file:}} become empty,\n\
         and your global config's providers, MCP servers and programs are used instead.\n\
         [t] Trust   [n] Don't trust (default) > ",
        req.summary()
    )?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let a = line.trim().to_ascii_lowercase();
    Ok(matches!(a.as_str(), "t" | "trust" | "y" | "yes"))
}

/// Ask about `cwd`'s project when its config needs an answer it does not
/// have, and remember the answer. `None` when there was nothing to ask.
pub fn ensure(
    cwd: &Path,
    sources: &crate::config::Sources,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<Option<bool>> {
    let Mode::Store(path) = &sources.trust else {
        return Ok(None);
    };
    let req = crate::config::trust_request(cwd, sources)?;
    if req.is_empty() {
        return Ok(None);
    }
    let store = Store::new(path.clone());
    let project = project_key(&req.root);
    if store.state(&project, &req) != State::Undecided {
        return Ok(None);
    }
    let yes = ask(&req, input, out)?;
    if let Err(e) = store.remember(&project, &req, yes) {
        writeln!(
            out,
            "could not save the answer ({e:#}); it holds until CodeSmith exits"
        )?;
    }
    writeln!(
        out,
        "{}",
        if yes {
            "Trusted. `codesmith trust --revoke` forgets this."
        } else {
            "Not trusted. `codesmith trust` trusts it later."
        }
    )?;
    Ok(Some(yes))
}
