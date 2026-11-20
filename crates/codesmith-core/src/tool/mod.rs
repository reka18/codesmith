//! Built-in tools the model can call. Each tool has a name, a description and
//! a JSON Schema (ported from OpenCode), and runs against a `Ctx`.

mod edit;
mod glob;
mod grep;
pub mod html_markdown;
mod lsp;
mod read;
pub mod shell;
pub mod shell_scan;
pub mod subagent;
pub mod todo;
mod webfetch;
pub mod websearch;
mod write;

use crate::config::ToolOutputConfig;
use crate::paths;
use crate::permission::{Denied, Gate, Request};
use codesmith_ai::ToolSpec;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub use edit::apply_edit;

/// Everything a tool call can see.
pub struct Ctx<'a> {
    /// Working directory; relative paths resolve against it.
    pub cwd: PathBuf,
    /// Project root; paths outside it need `external_directory` approval.
    pub root: PathBuf,
    pub gate: &'a Gate,
    pub shell: String,
    pub session_id: String,
    pub limits: ToolOutputConfig,
    pub client: reqwest::Client,
    /// Background command registry; `None` where background commands are unavailable.
    pub jobs: Option<shell::Jobs>,
    pub websearch: Option<websearch::WebSearchConfig>,
    /// Formats files after `write` and `edit`; `None` when formatting is off.
    pub format: Option<std::sync::Arc<crate::format::Formatter>>,
    /// Language servers for diagnostics after `write` and `edit` and for the
    /// `lsp` tool; `None` when they are off.
    pub lsp: Option<std::sync::Arc<crate::lsp::Lsp>>,
}

impl Ctx<'_> {
    /// Run the formatter on a file just written. Returns a note for the
    /// model when the file changed, so it re-reads before editing again.
    pub fn format(&self, path: &std::path::Path, written: &str) -> String {
        let Some(f) = &self.format else {
            return String::new();
        };
        let handle = tokio::runtime::Handle::current();
        let Some(name) = tokio::task::block_in_place(|| handle.block_on(f.file(path))) else {
            return String::new();
        };
        match std::fs::read_to_string(path) {
            Ok(mut now) => {
                // Keep a UTF-8 BOM the formatter dropped, as OpenCode's `syncTextBom` does.
                if written.starts_with('\u{feff}') && !now.starts_with('\u{feff}') {
                    now.insert(0, '\u{feff}');
                    let _ = std::fs::write(path, &now);
                }
                if now != written {
                    format!("\n\nThe file was reformatted by {name}, so it now differs from what you wrote; read it again before editing it.")
                } else {
                    String::new()
                }
            }
            Err(_) => String::new(),
        }
    }

    /// Tell the language servers a file changed and return the errors they
    /// report, formatted for the model; with `others`, errors in other
    /// files too. Empty when language servers are off or report none.
    pub fn diagnostics(&self, path: &std::path::Path, others: bool) -> String {
        let Some(lsp) = &self.lsp else {
            return String::new();
        };
        let handle = tokio::runtime::Handle::current();
        tokio::task::block_in_place(|| handle.block_on(lsp.touch(path, true)));
        crate::lsp::report(&lsp.diagnostics(), path, others)
    }
}

/// What a tool returns to the model. `Err` is shown to the model as a failed call.
pub type Outcome = Result<Output, String>;

#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub content: String,
    /// The tool already bounded its output; skip generic truncation.
    pub bounded: bool,
    /// Images and files for the model to see beside the text.
    pub media: Vec<codesmith_ai::Media>,
}

impl Output {
    pub fn text(s: impl Into<String>) -> Self {
        Output {
            content: s.into(),
            bounded: false,
            media: vec![],
        }
    }
    pub fn bounded(s: impl Into<String>) -> Self {
        Output {
            content: s.into(),
            bounded: true,
            media: vec![],
        }
    }
    /// Text with media attached (already bounded).
    pub fn media(s: impl Into<String>, media: Vec<codesmith_ai::Media>) -> Self {
        Output {
            content: s.into(),
            bounded: true,
            media,
        }
    }
}

/// A finished tool call: its text, whether it failed, and any media.
#[derive(Debug, Clone, PartialEq)]
pub struct Executed {
    pub content: String,
    pub is_error: bool,
    pub media: Vec<codesmith_ai::Media>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Read,
    Write,
    Edit,
    Glob,
    Grep,
    Shell,
    Webfetch,
    Websearch,
    Todowrite,
    Subagent,
    Skill,
    Question,
    Lsp,
}

pub const ALL: [Tool; 13] = [
    Tool::Read,
    Tool::Write,
    Tool::Edit,
    Tool::Glob,
    Tool::Grep,
    Tool::Shell,
    Tool::Webfetch,
    Tool::Websearch,
    Tool::Todowrite,
    Tool::Subagent,
    Tool::Skill,
    Tool::Question,
    Tool::Lsp,
];

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Read => "read",
            Tool::Write => "write",
            Tool::Edit => "edit",
            Tool::Glob => "glob",
            Tool::Grep => "grep",
            Tool::Shell => "shell",
            Tool::Webfetch => "webfetch",
            Tool::Websearch => "websearch",
            Tool::Todowrite => "todowrite",
            Tool::Subagent => "subagent",
            Tool::Skill => "skill",
            Tool::Question => "question",
            Tool::Lsp => "lsp",
        }
    }

    /// Tools the session runs itself, because they need session state.
    pub fn session_level(self) -> bool {
        matches!(
            self,
            Tool::Todowrite | Tool::Subagent | Tool::Skill | Tool::Question
        )
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        ALL.into_iter().find(|t| t.name() == name)
    }

    /// The permission action that guards this tool (`write` shares `edit`).
    pub fn action(self) -> &'static str {
        match self {
            Tool::Write | Tool::Edit => "edit",
            t => t.name(),
        }
    }

    pub fn spec(self, shell: &str) -> ToolSpec {
        let (description, parameters) = match self {
            Tool::Read => read::spec(),
            Tool::Write => write::spec(),
            Tool::Edit => edit::spec(),
            Tool::Glob => glob::spec(),
            Tool::Grep => grep::spec(),
            Tool::Shell => shell::spec(shell),
            Tool::Webfetch => webfetch::spec(),
            Tool::Websearch => websearch::spec(),
            Tool::Todowrite => todo::spec(),
            Tool::Subagent => subagent::spec(&[]),
            Tool::Skill => crate::skill::tool_spec(&[]),
            Tool::Question => crate::question::spec(),
            Tool::Lsp => lsp::spec(),
        };
        ToolSpec {
            name: self.name().to_string(),
            description,
            parameters,
        }
    }

    pub async fn run(self, ctx: &Ctx<'_>, input: &Value) -> Outcome {
        match self {
            Tool::Read => read::run(ctx, input),
            Tool::Write => write::run(ctx, input),
            Tool::Edit => edit::run(ctx, input),
            Tool::Glob => glob::run(ctx, input),
            Tool::Grep => grep::run(ctx, input),
            Tool::Shell => shell::run(ctx, input).await,
            Tool::Webfetch => webfetch::run(ctx, input).await,
            Tool::Websearch => websearch::run(ctx, input).await,
            Tool::Lsp => lsp::run(ctx, input).await,
            Tool::Todowrite | Tool::Subagent | Tool::Skill | Tool::Question => {
                Err(format!("{} must be run by the session", self.name()))
            }
        }
    }
}

/// Run a named tool with generic input repair and output truncation.
pub async fn execute(ctx: &Ctx<'_>, name: &str, input: &Value) -> (String, bool) {
    let r = run(ctx, name, input).await;
    (r.content, r.is_error)
}

/// Run a built-in tool, keeping any media it returned (`read` on an image
/// or PDF). The caller fits images to the configured limits.
pub async fn run(ctx: &Ctx<'_>, name: &str, input: &Value) -> Executed {
    let fail = |content: String| Executed {
        content,
        is_error: true,
        media: vec![],
    };
    let Some(tool) = Tool::from_name(name) else {
        let known: Vec<&str> = ALL.iter().map(|t| t.name()).collect();
        return fail(format!(
            "Unknown tool: {name}. Available tools: {}",
            known.join(", ")
        ));
    };
    let input = repair(input);
    if let Some(raw) = input.get("__invalid_json") {
        return fail(format!("The tool input was not valid JSON: {raw}"));
    }
    match tool.run(ctx, &input).await {
        Ok(out) => Executed {
            content: if out.bounded {
                out.content
            } else {
                truncate(&out.content, ctx.limits, &paths::tool_output_dir())
            },
            is_error: false,
            media: out.media,
        },
        Err(e) => fail(e),
    }
}

/// Fix common model mistakes in tool input: a JSON object sent as a string,
/// or numbers/booleans sent as strings.
pub fn repair(input: &Value) -> Value {
    let v = match input {
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| input.clone()),
        Value::Null => Value::Object(Default::default()),
        _ => input.clone(),
    };
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let fixed = match &v {
                        Value::String(s) if s == "true" || s == "false" => Value::Bool(s == "true"),
                        Value::String(s)
                            if !s.is_empty()
                                && s.len() < 20
                                && s.chars().all(|c| c.is_ascii_digit())
                                && is_numeric_field(&k) =>
                        {
                            s.parse::<u64>().map(Value::from).unwrap_or(v.clone())
                        }
                        _ => v,
                    };
                    (k, fixed)
                })
                .collect(),
        ),
        other => other,
    }
}

fn is_numeric_field(k: &str) -> bool {
    matches!(k, "offset" | "limit" | "timeout" | "line" | "character")
}

pub(crate) fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Missing required string parameter `{key}`"))
}

pub(crate) fn opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && *s != "undefined" && *s != "null")
}

pub(crate) fn opt_u64(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(Value::as_u64)
}

pub(crate) fn opt_bool(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(Value::as_bool)
}

/// Lexically normalize a path (resolve `.` and `..` without touching disk).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// A path the model named, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub absolute: PathBuf,
    /// The resource name used in permission rules and messages: relative to
    /// the root when inside it, absolute otherwise.
    pub resource: String,
    pub external: bool,
}

impl Ctx<'_> {
    pub fn resolve(&self, path: &str) -> Target {
        let expanded = match path.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None => PathBuf::from(path),
        };
        let absolute = normalize(&if expanded.is_absolute() {
            expanded
        } else {
            self.cwd.join(expanded)
        });
        let inside = absolute.starts_with(&self.root) || absolute.starts_with(paths::tmp_dir());
        let resource = match absolute.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => absolute.display().to_string(),
        };
        Target {
            absolute,
            resource,
            external: !inside,
        }
    }

    /// For paths outside the project, ask for `external_directory` on the
    /// containing directory first. A file is named by its path; a directory
    /// as `<dir>/*`, as OpenCode names it, so a rule for `<dir>/*` covers the
    /// directory itself.
    pub fn authorize_external(&self, t: &Target, is_dir: bool) -> Result<(), String> {
        if !t.external {
            return Ok(());
        }
        if is_dir {
            return self.authorize_external_dirs(std::slice::from_ref(&t.absolute));
        }
        let dir = t
            .absolute
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let glob = format!("{}/*", dir.display());
        self.ask(Request {
            action: "external_directory".into(),
            resources: vec![t.absolute.display().to_string()],
            save: vec![glob],
            title: format!("Access outside the project: {}", dir.display()),
        })
    }

    /// One `external_directory` request for every directory in `dirs`
    /// outside the project (absolute paths), each named `<dir>/*`.
    pub fn authorize_external_dirs(&self, dirs: &[PathBuf]) -> Result<(), String> {
        let mut globs: Vec<String> = Vec::new();
        for d in dirs {
            let t = self.resolve(&d.display().to_string());
            let glob = format!("{}/*", t.absolute.display());
            if t.external && !globs.contains(&glob) {
                globs.push(glob);
            }
        }
        if globs.is_empty() {
            return Ok(());
        }
        let shown: Vec<&str> = globs.iter().map(|g| g.trim_end_matches("/*")).collect();
        self.ask(Request {
            action: "external_directory".into(),
            resources: globs.clone(),
            save: globs.clone(),
            title: format!("Access outside the project: {}", shown.join(", ")),
        })
    }

    /// Refuse if a configured rule denies the request, without asking.
    pub fn check_not_denied(&self, action: &str, resources: &[String]) -> Result<(), String> {
        self.gate
            .assert_not_denied(action, resources)
            .map_err(|e| e.to_string())
    }

    pub fn ask(&self, req: Request) -> Result<(), String> {
        let gate = self.gate;
        tokio::task::block_in_place(|| gate.assert(&req)).map_err(|e: Denied| e.to_string())
    }

    /// Shorten an absolute path for display: relative to cwd when inside it.
    pub fn display(&self, p: &Path) -> String {
        p.strip_prefix(&self.cwd)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.display().to_string())
    }
}

/// Bound text to `limits`, saving the full text under `dir` when cut.
/// Ported from OpenCode's `ToolOutput.truncate`.
pub fn truncate(text: &str, limits: ToolOutputConfig, dir: &Path) -> String {
    let total = count_lines(text);
    if total <= limits.max_lines && text.len() <= limits.max_bytes {
        return text.to_string();
    }
    let end = after_lines(text, limits.max_lines).min(byte_boundary(text, limits.max_bytes));
    let shown_end = if text[..end].ends_with("\r\n") {
        end - 2
    } else if text[..end].ends_with('\n') {
        end - 1
    } else {
        end
    };
    let shown = &text[..shown_end];
    cleanup_once(dir);
    let file = dir.join(format!("tool_{}", unique_id()));
    let saved = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&file, text));
    let shown_lines = count_lines(&text[..end]);
    let rest = &text[shown.len()..];
    let last_cut = shown_lines > 0
        && shown.len() < text.len()
        && !rest.starts_with('\n')
        && !rest.starts_with("\r\n");
    let range = if shown_lines == 0 {
        "0 lines".to_string()
    } else {
        format!("lines 1-{shown_lines}")
    };
    let where_ = match saved {
        Ok(()) => format!("full output saved to {}", file.display()),
        Err(e) => format!("full output could not be saved: {e}"),
    };
    let marker = format!(
        "[showing {range} of {total}{}; {where_}]",
        if last_cut { " (last line cut)" } else { "" }
    );
    if shown.is_empty() {
        marker
    } else {
        format!("{shown}\n{marker}")
    }
}

/// Lines in `text`; a trailing line break does not start a new line.
pub fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let n = text.matches('\n').count();
    if text.ends_with('\n') {
        n
    } else {
        n + 1
    }
}

/// Byte offset just after the first `n` lines (including their line breaks).
fn after_lines(text: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    match text.match_indices('\n').nth(n - 1) {
        Some((i, _)) => i + 1,
        None => text.len(),
    }
}

/// The longest start of `text` within `max` bytes that splits neither a
/// character nor a grapheme cluster (emoji with modifiers or joiners, flag
/// pairs, combining marks) nor a `\r\n` (OpenCode's `byteBoundary`).
fn byte_boundary(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut i = max;
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let before = text[..i].chars().next_back().unwrap();
    let after = text[i..].chars().next().unwrap();
    if before.is_ascii() && after.is_ascii() {
        return if before == '\r' && after == '\n' {
            i - 1
        } else {
            i
        };
    }
    cluster_start(text, i)
}

/// Characters that continue the grapheme cluster before them.
fn extends(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0x200D | 0x1F3FB..=0x1F3FF
        | 0xE0020..=0xE007F | 0xE0100..=0xE01EF)
}

fn regional(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

/// Move `i` (a char boundary) back to the start of the cluster it is in.
fn cluster_start(text: &str, mut i: usize) -> usize {
    while i > 0 {
        let Some(next) = text[i..].chars().next() else {
            return i;
        };
        let prev = text[..i].chars().next_back().unwrap();
        if extends(next) || prev == '\u{200D}' {
            i -= prev.len_utf8();
            continue;
        }
        if regional(next) && regional(prev) {
            let run = text[..i].chars().rev().take_while(|c| regional(*c)).count();
            if run % 2 == 1 {
                i -= prev.len_utf8();
            }
        }
        break;
    }
    i
}

/// How long saved tool output is kept.
pub const OUTPUT_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Delete files in `dir` last modified more than `retention` ago (by
/// modification time, as OpenCode's `ToolOutput.cleanup` does).
pub fn cleanup_outputs(dir: &Path, retention: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > retention);
        if meta.is_file() && old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Clean up `dir` the first time output is saved in this process.
fn cleanup_once(dir: &Path) {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| cleanup_outputs(dir, OUTPUT_RETENTION));
}

pub(crate) fn unique_id() -> String {
    use rand::Rng;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{t:012x}{:08x}", rand::thread_rng().gen::<u32>())
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::permission::{base_rules, Fixed, Reply};
    use std::sync::Arc;

    pub struct Env {
        pub dir: tempfile::TempDir,
        pub gate: Gate,
    }

    /// Records every permission request that reaches the asker, and
    /// declines those matching `deny_action` or `deny_resource`.
    #[derive(Default)]
    pub struct Recorder {
        pub asked: std::sync::Mutex<Vec<Request>>,
        pub deny_action: std::sync::Mutex<Option<String>>,
        pub deny_resource: std::sync::Mutex<Option<String>>,
    }

    impl Recorder {
        /// The actions asked so far, in order.
        pub fn actions(&self) -> Vec<String> {
            self.asked
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.action.clone())
                .collect()
        }
        /// (action, resources) pairs asked so far, in order.
        pub fn requests(&self) -> Vec<(String, Vec<String>)> {
            self.asked
                .lock()
                .unwrap()
                .iter()
                .map(|r| (r.action.clone(), r.resources.clone()))
                .collect()
        }
        pub fn deny_action(&self, action: &str) {
            *self.deny_action.lock().unwrap() = Some(action.into());
        }
        pub fn deny_resource(&self, resource: &str) {
            *self.deny_resource.lock().unwrap() = Some(resource.into());
        }
    }

    impl crate::permission::Asker for Recorder {
        fn ask(&self, req: &Request) -> Reply {
            self.asked.lock().unwrap().push(req.clone());
            let action = self.deny_action.lock().unwrap().clone();
            let resource = self.deny_resource.lock().unwrap().clone();
            if action.as_deref() == Some(req.action.as_str())
                || resource.is_some_and(|r| req.resources.contains(&r))
            {
                Reply::Reject(None)
            } else {
                Reply::Once
            }
        }
    }

    impl Env {
        pub fn new() -> Self {
            Env {
                dir: tempfile::tempdir().unwrap(),
                gate: Gate::new(base_rules(), Arc::new(Fixed(Reply::Once))),
            }
        }
        pub fn with_reply(reply: Reply) -> Self {
            Env {
                dir: tempfile::tempdir().unwrap(),
                gate: Gate::new(vec![], Arc::new(Fixed(reply))),
            }
        }
        /// Every request not settled by `rules` goes to a `Recorder`.
        pub fn recording(rules: Vec<crate::permission::Rule>) -> (Self, Arc<Recorder>) {
            let rec = Arc::new(Recorder::default());
            let env = Env {
                dir: tempfile::tempdir().unwrap(),
                gate: Gate::new(rules, rec.clone()),
            };
            (env, rec)
        }
        pub fn root(&self) -> PathBuf {
            self.dir.path().canonicalize().unwrap()
        }
        pub fn ctx(&self) -> Ctx<'_> {
            Ctx {
                cwd: self.root(),
                root: self.root(),
                gate: &self.gate,
                shell: "/bin/sh".into(),
                session_id: "test".into(),
                limits: ToolOutputConfig::default(),
                client: reqwest::Client::new(),
                jobs: Some(shell::Jobs::default()),
                websearch: None,
                format: None,
                lsp: None,
            }
        }
        pub fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.root().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Env;
    use super::*;
    use serde_json::json;

    #[test]
    fn names_and_actions() {
        for t in ALL {
            assert_eq!(Tool::from_name(t.name()), Some(t));
        }
        assert_eq!(Tool::Write.action(), "edit");
        assert_eq!(Tool::Edit.action(), "edit");
        assert_eq!(Tool::Shell.action(), "shell");
        assert_eq!(Tool::from_name("bash"), None);
    }

    #[test]
    fn every_spec_is_an_object_schema_with_a_description() {
        for t in ALL {
            let s = t.spec("/bin/zsh");
            assert_eq!(s.parameters["type"], "object", "{}", s.name);
            assert!(!s.description.is_empty());
            for req in s.parameters["required"].as_array().unwrap() {
                assert!(
                    s.parameters["properties"]
                        .get(req.as_str().unwrap())
                        .is_some(),
                    "{} requires undeclared {req}",
                    s.name
                );
            }
        }
    }

    #[test]
    fn repair_fixes_common_mistakes() {
        assert_eq!(repair(&json!("{\"path\": \"a\"}")), json!({"path": "a"}));
        assert_eq!(
            repair(&json!({"replaceAll": "true", "offset": "10", "path": "10"})),
            json!({"replaceAll": true, "offset": 10, "path": "10"})
        );
        assert_eq!(repair(&Value::Null), json!({}));
        assert_eq!(repair(&json!("not json")), json!("not json"));
    }

    #[test]
    fn resolve_inside_and_outside() {
        let env = Env::new();
        let ctx = env.ctx();
        let t = ctx.resolve("src/../a.rs");
        assert_eq!(t.absolute, env.root().join("a.rs"));
        assert_eq!(t.resource, "a.rs");
        assert!(!t.external);
        assert_eq!(ctx.resolve(".").resource, ".");
        let t = ctx.resolve("/etc/hosts");
        assert!(t.external);
        assert_eq!(t.resource, "/etc/hosts");
        let t = ctx.resolve("../outside.txt");
        assert!(t.external);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_tool_and_bad_json() {
        let env = Env::new();
        let (msg, err) = execute(&env.ctx(), "bash", &json!({})).await;
        assert!(err && msg.starts_with("Unknown tool: bash"));
        let (msg, err) = execute(&env.ctx(), "read", &json!({"__invalid_json": "{oops"})).await;
        assert!(err && msg.contains("not valid JSON"));
    }

    #[test]
    fn short_text_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            truncate("a\nb\n", ToolOutputConfig::default(), dir.path()),
            "a\nb\n"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn truncates_by_lines_and_saves_full_output() {
        let dir = tempfile::tempdir().unwrap();
        let text: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let out = truncate(
            &text,
            ToolOutputConfig {
                max_lines: 3,
                max_bytes: 1000,
            },
            dir.path(),
        );
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some("line 1"));
        assert_eq!(lines.nth(1), Some("line 3"));
        let marker = lines.next().unwrap();
        assert!(
            marker.starts_with("[showing lines 1-3 of 10; full output saved to "),
            "{marker}"
        );
        let saved: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(saved.len(), 1);
        assert_eq!(
            std::fs::read_to_string(saved[0].as_ref().unwrap().path()).unwrap(),
            text
        );
    }

    #[test]
    fn truncates_by_bytes_and_marks_a_cut_line() {
        let dir = tempfile::tempdir().unwrap();
        let out = truncate(
            "abcdefghij\nxyz",
            ToolOutputConfig {
                max_lines: 100,
                max_bytes: 5,
            },
            dir.path(),
        );
        assert!(
            out.starts_with("abcde\n[showing lines 1-1 of 2 (last line cut); "),
            "{out}"
        );
    }

    #[test]
    fn byte_cut_respects_char_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let out = truncate(
            "ééééé",
            ToolOutputConfig {
                max_lines: 100,
                max_bytes: 3,
            },
            dir.path(),
        );
        assert!(out.starts_with("é\n"), "{out}");
    }

    #[test]
    fn line_counting() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\n"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("\n\n"), 2);
    }

    // ---------------------------------------------------------------------
    // Ported from OpenCode's packages/core/test/tool-output.test.ts (MIT).
    // One `oc_tool_output_*` test per OpenCode case, in the same order.
    //
    // Intentional differences: a result is one text, so the preview and the
    // marker are joined by a line break and the multi-part cases (cutting
    // inside a later part, joining parts, keeping file parts in order) have
    // no counterpart; tools that bound their own output return
    // `Output::bounded`, which `execute` leaves alone (OpenCode's
    // "truncation state" metadata).
    // ---------------------------------------------------------------------

    fn limits(max_lines: usize, max_bytes: usize) -> ToolOutputConfig {
        ToolOutputConfig {
            max_lines,
            max_bytes,
        }
    }

    fn cut(text: &str, l: ToolOutputConfig) -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (truncate(text, l, dir.path()), dir)
    }

    /// The preview and the marker.
    fn split(out: &str) -> (&str, &str) {
        let i = out.rfind("[showing").unwrap();
        (out[..i].strip_suffix('\n').unwrap_or(&out[..i]), &out[i..])
    }

    #[test]
    fn oc_tool_output_writes_oversized_text_and_returns_a_preview() {
        let (out, dir) = cut("one\ntwo\nthree", limits(2, 1_000));
        let (shown, marker) = split(&out);
        assert_eq!(shown, "one\ntwo");
        let file = marker
            .strip_prefix("[showing lines 1-2 of 3; full output saved to ")
            .and_then(|m| m.strip_suffix(']'))
            .unwrap();
        assert!(file.starts_with(&dir.path().display().to_string()));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "one\ntwo\nthree");
    }

    #[test]
    fn oc_tool_output_keeps_the_preview_when_saving_fails() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("tool-output");
        std::fs::write(&blocked, "not a directory").unwrap();
        let out = truncate("one\ntwo\nthree", limits(2, 1_000), &blocked);
        let (shown, marker) = split(&out);
        assert_eq!(shown, "one\ntwo");
        assert!(
            marker.starts_with("[showing lines 1-2 of 3; full output could not be saved: ")
                && marker.ends_with(']'),
            "{marker}"
        );
    }

    #[test]
    fn oc_tool_output_cuts_at_the_byte_limit_inside_a_line() {
        let (out, _d) = cut("one\ntwo", limits(100, 5));
        let (shown, marker) = split(&out);
        assert_eq!(shown, "one\nt");
        assert!(
            marker.starts_with("[showing lines 1-2 of 2 (last line cut); full output saved to ")
        );
    }

    #[test]
    fn oc_tool_output_shows_the_start_of_a_long_single_line() {
        let text = serde_json::json!({ "items": (0..50).collect::<Vec<_>>() }).to_string();
        let (out, _d) = cut(&text, limits(100, 20));
        let (shown, marker) = split(&out);
        assert_eq!(shown, &text[..20]);
        assert!(
            marker.starts_with("[showing lines 1-1 of 1 (last line cut); full output saved to ")
        );
    }

    #[test]
    fn oc_tool_output_never_splits_a_character_or_emoji() {
        let family = "👨\u{200d}👩\u{200d}👧\u{200d}👦";
        for (text, shown) in [
            ("ab✓✓✓".to_string(), "ab✓"),
            (format!("ab{family}cd"), "ab"),
            ("ab👍🏽cd".to_string(), "ab"),
            ("ab🇯🇵cd".to_string(), "ab"),
            ("ab𝒳𝒳".to_string(), "ab𝒳"),
        ] {
            let (out, _d) = cut(&text, limits(100, 7));
            assert_eq!(split(&out).0, shown, "{text}");
        }
    }

    #[test]
    fn oc_tool_output_keeps_flags_and_long_clusters_whole() {
        let (out, _d) = cut(&format!("a{}", "🇯🇵".repeat(40)), limits(100, 165));
        assert_eq!(split(&out).0, format!("a{}", "🇯🇵".repeat(20)));
        let cluster = format!("👍{}", "🏽\u{301}".repeat(60));
        let (out, _d) = cut(&format!("ab{cluster}z"), limits(100, 165));
        assert_eq!(split(&out).0, "ab");
    }

    #[test]
    fn oc_tool_output_ends_a_line_cut_before_its_line_break() {
        let (out, _d) = cut("one\r\ntwo\r\nthree", limits(1, 1_000));
        let (shown, marker) = split(&out);
        assert_eq!(shown, "one");
        assert!(marker.starts_with("[showing lines 1-1 of 3;"));
        let (out, _d) = cut("\nfoo\nbar", limits(1, 1_000));
        assert!(out.starts_with("[showing lines 1-1 of 3;"), "{out:?}");
        // A byte cut between `\r` and `\n` keeps the pair together.
        let (out, _d) = cut("one\r\ntwo", limits(100, 4));
        assert_eq!(split(&out).0, "one");
    }

    #[test]
    fn oc_tool_output_counts_a_blank_last_line_of_the_preview() {
        let (out, _d) = cut("a\n\n\nb", limits(2, 1_000));
        assert!(out.starts_with("a\n\n[showing lines 1-2 of 4;"), "{out:?}");
    }

    #[test]
    fn oc_tool_output_shows_nothing_under_a_zero_line_limit() {
        let (out, _d) = cut("secret", limits(0, 1_000));
        assert!(out.starts_with("[showing 0 lines of 1;"), "{out}");
        assert!(!out.contains("secret\n"));
    }

    #[test]
    fn oc_tool_output_leaves_text_that_fits_unchanged() {
        let (out, dir) = cut("small", ToolOutputConfig::default());
        assert_eq!(out, "small");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn oc_tool_output_does_not_count_a_trailing_newline_as_a_line() {
        let (out, _d) = cut("one\ntwo\n", limits(2, 1_000));
        assert_eq!(out, "one\ntwo\n");
    }

    #[test]
    fn oc_tool_output_reports_a_trailing_newline_cut_by_the_byte_limit() {
        let (out, _d) = cut("one\n", limits(2, 3));
        let (shown, marker) = split(&out);
        assert_eq!(shown, "one");
        assert!(
            marker.starts_with("[showing lines 1-1 of 1; full output saved to "),
            "{marker}"
        );
    }

    #[test]
    fn oc_tool_output_cleans_up_by_modification_time() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("tool_ffffffffffff0000");
        let recent = dir.path().join("tool_000000000001ffff");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&recent, "recent").unwrap();
        let eight_days =
            std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(eight_days)
            .unwrap();
        cleanup_outputs(dir.path(), OUTPUT_RETENTION);
        assert!(!old.exists());
        assert!(recent.exists());
    }
}
