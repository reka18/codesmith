//! Built-in tools the model can call. Each tool has a name, a description and
//! a JSON Schema (ported from OpenCode), and runs against a `Ctx`.

mod edit;
mod glob;
mod grep;
mod read;
mod shell;
pub mod subagent;
pub mod todo;
mod webfetch;
mod write;

use crate::config::ToolOutputConfig;
use crate::paths;
use crate::permission::{Denied, Gate, Request};
use serde_json::Value;
use skeptical_ai::ToolSpec;
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
}

/// What a tool returns to the model. `Err` is shown to the model as a failed call.
pub type Outcome = Result<Output, String>;

#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub content: String,
    /// The tool already bounded its output; skip generic truncation.
    pub bounded: bool,
}

impl Output {
    pub fn text(s: impl Into<String>) -> Self {
        Output {
            content: s.into(),
            bounded: false,
        }
    }
    pub fn bounded(s: impl Into<String>) -> Self {
        Output {
            content: s.into(),
            bounded: true,
        }
    }
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
    Todowrite,
    Subagent,
}

pub const ALL: [Tool; 9] = [
    Tool::Read,
    Tool::Write,
    Tool::Edit,
    Tool::Glob,
    Tool::Grep,
    Tool::Shell,
    Tool::Webfetch,
    Tool::Todowrite,
    Tool::Subagent,
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
            Tool::Todowrite => "todowrite",
            Tool::Subagent => "subagent",
        }
    }

    /// Tools the session runs itself, because they need session state.
    pub fn session_level(self) -> bool {
        matches!(self, Tool::Todowrite | Tool::Subagent)
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
            Tool::Todowrite => todo::spec(),
            Tool::Subagent => subagent::spec(&[]),
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
            Tool::Todowrite | Tool::Subagent => {
                Err(format!("{} must be run by the session", self.name()))
            }
        }
    }
}

/// Run a named tool with generic input repair and output truncation.
pub async fn execute(ctx: &Ctx<'_>, name: &str, input: &Value) -> (String, bool) {
    let Some(tool) = Tool::from_name(name) else {
        let known: Vec<&str> = ALL.iter().map(|t| t.name()).collect();
        return (
            format!(
                "Unknown tool: {name}. Available tools: {}",
                known.join(", ")
            ),
            true,
        );
    };
    let input = repair(input);
    if let Some(raw) = input.get("__invalid_json") {
        return (format!("The tool input was not valid JSON: {raw}"), true);
    }
    match tool.run(ctx, &input).await {
        Ok(out) if out.bounded => (out.content, false),
        Ok(out) => (
            truncate(&out.content, ctx.limits, &paths::tool_output_dir()),
            false,
        ),
        Err(e) => (e, true),
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
    matches!(k, "offset" | "limit" | "timeout")
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
    /// containing directory first.
    pub fn authorize_external(&self, t: &Target, is_dir: bool) -> Result<(), String> {
        if !t.external {
            return Ok(());
        }
        let dir = if is_dir {
            t.absolute.clone()
        } else {
            t.absolute
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        };
        let glob = format!("{}/*", dir.display());
        self.ask(Request {
            action: "external_directory".into(),
            resources: vec![t.absolute.display().to_string()],
            save: vec![glob],
            title: format!("Access outside the project: {}", dir.display()),
        })
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

/// The largest char boundary at or below `max` bytes.
fn byte_boundary(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut i = max;
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn unique_id() -> String {
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
}
