//! Built-in tools the model can call. Each tool has a name, a description and
//! a JSON Schema (ported from OpenCode), and runs against a `Ctx`.

pub mod apply_patch;
mod edit;
mod glob;
mod grep;
pub mod html_markdown;
pub mod image_generate;
pub mod input_repair;
pub mod input_schema;
mod lsp;
pub mod patch;
pub(crate) mod read;
pub mod shell;
pub(crate) mod shell_proc;
pub mod shell_scan;
pub mod subagent;
pub mod todo;
pub mod video_generate;
pub(crate) mod webfetch;
pub mod websearch;
mod write;

use crate::config::ToolOutputConfig;
use crate::paths;
use crate::permission::{Denied, Effect, Gate, Request};
use codesmith_ai::ToolSpec;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub use edit::apply_edit;

/// What a running call reports before its result (OpenCode's
/// `context.progress`), for front ends to show while it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// Metadata about the call, such as the `shellID` of the command a
    /// `shell` call started (sent once, as soon as it runs).
    Metadata(Value),
    /// Output a foreground command printed since the last report.
    Output(String),
    /// A file an `edit`, `write` or `patch` call is about to change, with
    /// its text now: sent once the change is allowed, just before writing,
    /// so the session can record the change (see [`Ctx::changing`]).
    Change(Before),
}

/// Where a call sends its [`Progress`].
pub type ProgressTx = tokio::sync::mpsc::UnboundedSender<Progress>;

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
    /// Where the call reports progress; `None` when nobody listens.
    pub progress: Option<ProgressTx>,
}

impl Ctx<'_> {
    /// Report that `path` is about to change, with its text now when it is
    /// a regular file small enough for a change record (nothing when it is
    /// not). Call only once the change is allowed: it reads the file.
    pub fn changing(&self, path: &Path) {
        self.changing_moved(path, path);
    }

    /// [`Ctx::changing`] for a file moved to `path` from `source`: its text
    /// before is the source's.
    pub fn changing_moved(&self, path: &Path, source: &Path) {
        let Some(p) = &self.progress else { return };
        let text = match peek_text(source, crate::store::CHANGE_MAX) {
            Peek::Missing => None,
            Peek::Text(t) => Some(t),
            Peek::Skipped => return,
        };
        let _ = p.send(Progress::Change(Before {
            path: path.to_path_buf(),
            text,
        }));
    }

    /// [`Ctx::changing`] with the text already read (and checked).
    pub fn changing_from(&self, path: &Path, text: &str) {
        if let (Some(p), true) = (&self.progress, text.len() <= crate::store::CHANGE_MAX) {
            let _ = p.send(Progress::Change(Before {
                path: path.to_path_buf(),
                text: Some(text.to_string()),
            }));
        }
    }

    /// Run the formatter on a file just written. Returns a note for the
    /// model when the file changed, so it re-reads before editing again.
    pub fn format(&self, path: &std::path::Path, written: &str) -> String {
        match self.format_name(path, written) {
            Some(name) => format!("\n\nThe file was reformatted by {name}, so it now differs from what you wrote; read it again before editing it."),
            None => String::new(),
        }
    }

    /// Run the formatter on a file just written; the formatter's name when
    /// it changed the file from `written`.
    pub fn format_name(&self, path: &std::path::Path, written: &str) -> Option<String> {
        let f = self.format.as_ref()?;
        let handle = tokio::runtime::Handle::current();
        let name = tokio::task::block_in_place(|| handle.block_on(f.file(path)))?;
        let mut now = std::fs::read_to_string(path).ok()?;
        // Keep a UTF-8 BOM the formatter dropped, as OpenCode's `syncTextBom` does.
        if written.starts_with('\u{feff}') && !now.starts_with('\u{feff}') {
            now.insert(0, '\u{feff}');
            let _ = std::fs::write(path, &now);
        }
        (now != written).then_some(name)
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

/// A file's text before a call changes it (`text: None`: it was new).
#[derive(Debug, Clone, PartialEq)]
pub struct Before {
    pub path: PathBuf,
    pub text: Option<String>,
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

/// What [`peek_text`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peek {
    /// Nothing is there.
    Missing,
    /// A regular file of UTF-8 text.
    Text(String),
    /// Something that is not read: not a regular file (a FIFO, a device,
    /// a directory), larger than the cap, not UTF-8, or unreadable.
    Skipped,
}

/// The text of the regular file at `path` when it holds at most `cap`
/// bytes. Never blocks on a FIFO or device and never reads more than
/// `cap + 1` bytes, so it is safe on any path a model names.
pub fn peek_text(path: &Path, cap: usize) -> Peek {
    use std::io::Read;
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Peek::Missing,
        Err(_) => return Peek::Skipped,
        Ok(m) if !m.is_file() || m.len() > cap as u64 => return Peek::Skipped,
        Ok(_) => {}
    }
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    // Opening a FIFO swapped in after the check must not wait for a writer.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NONBLOCK);
    }
    let Ok(file) = open.open(path) else {
        return Peek::Skipped;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Peek::Skipped;
    }
    let mut bytes = Vec::new();
    if file.take(cap as u64 + 1).read_to_end(&mut bytes).is_err() || bytes.len() > cap {
        return Peek::Skipped;
    }
    String::from_utf8(bytes).map_or(Peek::Skipped, Peek::Text)
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
    Patch,
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

pub const ALL: [Tool; 14] = [
    Tool::Read,
    Tool::Write,
    Tool::Edit,
    Tool::Patch,
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
            Tool::Patch => "patch",
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

    /// The permission action that guards this tool (`write` and `patch`
    /// share `edit`).
    pub fn action(self) -> &'static str {
        match self {
            Tool::Write | Tool::Edit | Tool::Patch => "edit",
            t => t.name(),
        }
    }

    pub fn spec(self, shell: &str) -> ToolSpec {
        let (description, parameters) = match self {
            Tool::Read => read::spec(),
            Tool::Write => write::spec(),
            Tool::Edit => edit::spec(),
            Tool::Patch => apply_patch::spec(),
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
        // The patch as raw text matching OpenCode's patch grammar, for
        // models that take Lark-constrained custom tools.
        let freeform = (self == Tool::Patch).then(|| codesmith_ai::Freeform::Lark {
            grammar: apply_patch::GRAMMAR.into(),
        });
        ToolSpec {
            name: self.name().to_string(),
            description,
            parameters,
            freeform,
        }
    }

    pub async fn run(self, ctx: &Ctx<'_>, input: &Value) -> Outcome {
        match self {
            Tool::Read => read::run(ctx, input),
            Tool::Write => write::run(ctx, input),
            Tool::Edit => edit::run(ctx, input),
            Tool::Patch => apply_patch::run(ctx, input),
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
                truncate(
                    &out.content,
                    ctx.limits,
                    &paths::session_output_dir(&ctx.session_id),
                )
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

/// Resolve symlinks in an absolute, lexically normalized path. An existing
/// path is canonicalized; for one that does not exist yet, the nearest
/// existing ancestor is, and the rest is appended. A dangling symlink is
/// followed to where a write through it would land. On a case-insensitive
/// volume, existing components come back in their on-disk case.
pub fn real_path(p: &Path) -> PathBuf {
    real_path_depth(p, 0)
}

fn real_path_depth(p: &Path, depth: usize) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
        return p.to_path_buf();
    };
    let parent = real_path_depth(parent, depth);
    let here = parent.join(name);
    // A symlink that canonicalize could not follow: its target is missing.
    if depth < 40 {
        if let Ok(link) = std::fs::read_link(&here) {
            return real_path_depth(&normalize(&parent.join(link)), depth + 1);
        }
    }
    here
}

/// A path the model named, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// The path as named, made absolute and lexically normalized; tools do
    /// their I/O on it.
    pub absolute: PathBuf,
    /// `absolute` with symlinks resolved (see [`real_path`]).
    pub real: PathBuf,
    /// The resource name used in permission rules and messages: relative to
    /// the root when inside it, absolute otherwise.
    pub resource: String,
    /// Every name the target goes by in permission rules: `resource` first,
    /// then the real path's name when it differs (a symlink's target, or the
    /// on-disk spelling). Rules are checked against all of them, so a rule
    /// matches whichever form it was written in.
    pub resources: Vec<String>,
    /// The real path is outside the project.
    pub external: bool,
}

/// `p` relative to `root` (`.` for the root itself), or `None` outside it.
fn relative_to(p: &Path, root: &Path) -> Option<String> {
    p.strip_prefix(root).ok().map(|rel| {
        if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.display().to_string()
        }
    })
}

/// The project root and the scratch directory with symlinks resolved.
/// Resolving many paths at once (a search) shares one of these instead of
/// resolving both again for every path.
#[derive(Debug, Clone)]
pub struct Roots {
    root: PathBuf,
    /// Where paths are inside the project ([`Ctx::boundary`]).
    boundary: Option<PathBuf>,
    tmp: PathBuf,
}

/// Whether `dir` is too broad to be the project's boundary: `/` (a drive
/// root) or the home folder, where everything, `~/.ssh` included, would be
/// inside the project.
fn too_broad(dir: &Path) -> bool {
    dir.parent().is_none()
        || dirs::home_dir().is_some_and(|h| h == dir || real_path(&h) == real_path(dir))
}

impl Ctx<'_> {
    /// [`Roots`] as they are on disk now.
    pub fn roots(&self) -> Roots {
        Roots {
            root: real_path(&self.root),
            boundary: self.boundary().map(|b| real_path(&b)),
            tmp: real_path(&paths::tmp_dir()),
        }
    }

    /// The folder paths are inside the project under, needing no
    /// `external_directory` approval: the project root. A root of `/` or
    /// the home folder is no boundary (OpenCode treats a `/` project the
    /// same way); the working directory is then, unless it is as broad,
    /// and with neither every path but the scratch directory is outside.
    pub fn boundary(&self) -> Option<PathBuf> {
        if !too_broad(&self.root) {
            return Some(self.root.clone());
        }
        (!too_broad(&self.cwd)).then(|| self.cwd.clone())
    }

    pub fn resolve(&self, path: &str) -> Target {
        self.resolve_in(&self.roots(), path)
    }

    /// [`Ctx::resolve`] against roots resolved earlier.
    pub fn resolve_in(&self, roots: &Roots, path: &str) -> Target {
        let expanded = match path.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
            None if path == "~" => dirs::home_dir().unwrap_or_default(),
            None => PathBuf::from(path),
        };
        let absolute = normalize(&if expanded.is_absolute() {
            expanded
        } else {
            self.cwd.join(expanded)
        });
        let real = real_path(&absolute);
        self.target(roots, absolute, real)
    }

    /// Resolve `path`, which a directory walk that does not follow symlinks
    /// found under `base` (a resolved, existing directory). Every directory
    /// between `base` and `path` is then a real directory, so unless the
    /// entry is itself a symlink its real path is `base.real` plus the same
    /// relative path, in the case the walk read from disk; nothing needs to
    /// be resolved again. Anything else is resolved in full.
    pub fn resolve_walked(
        &self,
        roots: &Roots,
        base: &Target,
        path: &Path,
        is_symlink: bool,
    ) -> Target {
        match path.strip_prefix(&base.absolute) {
            Ok(rel)
                if !is_symlink
                    && !rel.as_os_str().is_empty()
                    && rel.components().all(|c| matches!(c, Component::Normal(_))) =>
            {
                let absolute = base.absolute.join(rel);
                let real = base.real.join(rel);
                self.target(roots, absolute, real)
            }
            _ => self.resolve_in(roots, &path.display().to_string()),
        }
    }

    fn target(&self, roots: &Roots, absolute: PathBuf, real: PathBuf) -> Target {
        let root = &roots.root;
        let lexical = relative_to(&absolute, &self.root);
        let real_rel = relative_to(&real, root);
        let external = !(roots.boundary.as_ref().is_some_and(|b| real.starts_with(b))
            || real.starts_with(&roots.tmp));
        let lexical_name = lexical
            .clone()
            .unwrap_or_else(|| absolute.display().to_string());
        let real_name = real_rel
            .clone()
            .unwrap_or_else(|| real.display().to_string());
        // Named from outside the project but really inside it (another
        // spelling of the root): the project-relative name is the useful one.
        let resource = if lexical.is_none() && real_rel.is_some() {
            real_name.clone()
        } else {
            lexical_name.clone()
        };
        let mut resources = vec![resource.clone()];
        for r in [lexical_name, real_name] {
            if !resources.contains(&r) {
                resources.push(r);
            }
        }
        Target {
            absolute,
            real,
            resource,
            resources,
            external,
        }
    }

    /// Absolute names for the external directory `dir` of `t`: the real
    /// one, and the one as named when that is also outside the project.
    fn external_names(&self, t: &Target, dir: impl Fn(&Path) -> PathBuf) -> Vec<PathBuf> {
        let mut out = vec![dir(&t.real)];
        let lexical_outside = !self.boundary().is_some_and(|b| t.absolute.starts_with(b))
            && !t.absolute.starts_with(paths::tmp_dir());
        if lexical_outside && !out.contains(&dir(&t.absolute)) {
            out.push(dir(&t.absolute));
        }
        out
    }

    /// For paths outside the project, ask for `external_directory` on the
    /// containing directory first (answering "always" saves the enclosing
    /// repository; see [`repository_glob`]). A file is named by its path; a directory
    /// as `<dir>/*`, as OpenCode names it, so a rule for `<dir>/*` covers the
    /// directory itself. A symlink is judged by where it points.
    pub fn authorize_external(&self, t: &Target, is_dir: bool) -> Result<(), String> {
        if !t.external {
            return Ok(());
        }
        if is_dir {
            return self.authorize_external_dirs(std::slice::from_ref(&t.absolute));
        }
        let files = self.external_names(t, Path::to_path_buf);
        let dirs =
            self.external_names(t, |p| p.parent().map(Path::to_path_buf).unwrap_or_default());
        let mut save: Vec<String> = Vec::new();
        for d in &dirs {
            let g = repository_glob(d);
            if !save.contains(&g) {
                save.push(g);
            }
        }
        self.ask(Request {
            action: "external_directory".into(),
            resources: files.iter().map(|f| f.display().to_string()).collect(),
            save,
            title: format!("Access outside the project: {}", dirs[0].display()),
        })
    }

    /// One `external_directory` request for every directory in `dirs`
    /// outside the project (absolute paths), each named `<dir>/*`.
    pub fn authorize_external_dirs(&self, dirs: &[PathBuf]) -> Result<(), String> {
        let mut globs: Vec<String> = Vec::new();
        for d in dirs {
            let t = self.resolve(&d.display().to_string());
            if !t.external {
                continue;
            }
            for name in self.external_names(&t, Path::to_path_buf) {
                let glob = format!("{}/*", name.display());
                if !globs.contains(&glob) {
                    globs.push(glob);
                }
            }
        }
        if globs.is_empty() {
            return Ok(());
        }
        let shown: Vec<&str> = globs.iter().map(|g| g.trim_end_matches("/*")).collect();
        let mut save: Vec<String> = Vec::new();
        for g in &globs {
            let s = repository_glob(Path::new(g.trim_end_matches("/*")));
            if !save.contains(&s) {
                save.push(s);
            }
        }
        self.ask(Request {
            action: "external_directory".into(),
            resources: globs.clone(),
            save,
            title: format!("Access outside the project: {}", shown.join(", ")),
        })
    }

    /// How the rules settle `read` for `resources` without asking: a
    /// configured deny denies, otherwise configured and saved rules allow
    /// or leave it to ask.
    pub fn read_effect(&self, resources: &[String]) -> Effect {
        if self.check_not_denied("read", resources).is_err() {
            Effect::Deny
        } else if self.gate.allows("read", resources) {
            Effect::Allow
        } else {
            Effect::Ask
        }
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
    cleanup_once(&cleanup_root(dir));
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

/// The directory whose old output to clean up when saving in `dir`: the
/// whole tool-output directory for a session's directory in it.
pub(crate) fn cleanup_root(dir: &Path) -> PathBuf {
    let root = paths::tool_output_dir();
    if dir.parent() == Some(root.as_path()) {
        root
    } else {
        dir.to_path_buf()
    }
}

/// Delete files in `dir`, and in its session directories, last modified
/// more than `retention` ago (by modification time, as OpenCode's
/// `ToolOutput.cleanup` does); session directories left empty go too.
pub fn cleanup_outputs(dir: &Path, retention: std::time::Duration) {
    cleanup_level(dir, retention, true);
}

fn cleanup_level(dir: &Path, retention: std::time::Duration, descend: bool) {
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
        } else if meta.is_dir() && descend {
            cleanup_level(&entry.path(), retention, false);
            if old {
                // A session directory untouched for that long; only
                // removed when nothing in it is newer.
                let _ = std::fs::remove_dir(entry.path());
            }
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

/// What "always" saves for an external directory, as OpenCode saves it:
/// the enclosing repository (the nearest folder above it with `.git` or
/// `.hg`) as `<repo>/*`, else the directory itself. A repository at the
/// home folder or above still saves only the file
/// ([`crate::permission::always_saves`]).
///
/// Only a repository this user controls counts (see [`trusted_repository`]):
/// a `.git` another user planted in a shared folder such as `/tmp` must not
/// turn one approval into all of it. A shared folder itself saves only the
/// file ([`crate::permission::shared_dir`]).
pub fn repository_glob(dir: &Path) -> String {
    let root = crate::vcs::trusted_marker(dir)
        .map(|(r, _)| r)
        .unwrap_or_else(|| dir.to_path_buf());
    format!("{}/*", root.display())
}

/// Whether `root` (a folder [`crate::vcs::marker`] found) is a repository
/// this user controls: the folder is owned by this user and not
/// world-writable, and its marker is a real `.git` or `.hg` directory, or a
/// `.git` file (a worktree's) naming an existing git directory.
pub fn trusted_repository(root: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        let owned = |m: &std::fs::Metadata| m.uid() == me && m.mode() & 0o002 == 0;
        let Ok(dir) = std::fs::metadata(root) else {
            return false;
        };
        if !dir.is_dir() || !owned(&dir) || crate::permission::shared_dir(root) {
            return false;
        }
        let is_owned_dir =
            |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && owned(&m));
        let git = root.join(".git");
        match std::fs::symlink_metadata(&git) {
            Ok(m) if m.is_dir() => owned(&m),
            Ok(m) if m.is_file() => {
                if !owned(&m) {
                    return false;
                }
                let Ok(text) = std::fs::read_to_string(&git) else {
                    return false;
                };
                let Some(target) = text.lines().next().and_then(|l| l.strip_prefix("gitdir:"))
                else {
                    return false;
                };
                let target = Path::new(target.trim());
                let target = if target.is_absolute() {
                    target.to_path_buf()
                } else {
                    root.join(target)
                };
                std::fs::metadata(&target).is_ok_and(|m| m.is_dir())
            }
            Ok(_) => false,
            Err(_) => is_owned_dir(&root.join(".hg")),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        true
    }
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

    /// A reply from [`serve`]: status, extra header lines (each ending in
    /// `\r\n`), body.
    pub type Reply3 = (u16, String, Vec<u8>);

    /// Serve `reply(path)` on a loopback port for as long as the test's
    /// runtime lives; returns the base URL and the paths requested.
    pub async fn serve(
        reply: impl Fn(&str) -> Reply3 + Send + Sync + 'static,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split(' ').nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let (status, headers, body) = reply(&path);
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                out.extend_from_slice(&body);
                let _ = s.write_all(&out).await;
            }
        });
        (url, seen)
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
                progress: None,
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
    fn only_a_repository_this_user_controls_widens_an_external_approval() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let base = d.path().canonicalize().unwrap();
        let mode = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        // A real repository: the folder and its .git directory are ours.
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        assert!(trusted_repository(&repo));
        assert_eq!(
            repository_glob(&repo.join("src")),
            format!("{}/*", repo.display())
        );
        // A worktree's .git file must name an existing git directory.
        let wt = base.join("wt");
        std::fs::create_dir_all(wt.join("sub")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /nonexistent/git/dir\n").unwrap();
        assert!(!trusted_repository(&wt));
        assert_eq!(
            repository_glob(&wt.join("sub")),
            format!("{}/*", wt.join("sub").display())
        );
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", repo.join(".git").display()),
        )
        .unwrap();
        assert!(trusted_repository(&wt));
        // A .git that is a symlink, or a marker anything can write to, is not.
        let linked = base.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(repo.join(".git"), linked.join(".git")).unwrap();
        assert!(!trusted_repository(&linked));
        let shared = base.join("shared");
        std::fs::create_dir_all(shared.join(".git")).unwrap();
        std::fs::create_dir_all(shared.join("victim")).unwrap();
        mode(&shared, 0o1777);
        assert!(!trusted_repository(&shared));
        assert_eq!(
            repository_glob(&shared.join("victim")),
            format!("{}/*", shared.join("victim").display())
        );
        mode(&shared, 0o755);
        mode(&shared.join(".git"), 0o777);
        assert!(!trusted_repository(&shared));
        mode(&shared.join(".git"), 0o755);
        assert!(trusted_repository(&shared));
    }

    #[test]
    fn always_in_a_shared_folder_saves_only_the_file() {
        use crate::permission::{always_saves, Request};
        use std::os::unix::fs::PermissionsExt;
        let req = |file: &str, save: &str| Request {
            action: "external_directory".into(),
            resources: vec![file.into()],
            save: vec![save.into()],
            title: String::new(),
        };
        for tmp in ["/tmp", "/private/tmp", "/var/tmp"] {
            let file = format!("{tmp}/x.txt");
            assert_eq!(always_saves(&req(&file, &format!("{tmp}/*"))), [file]);
        }
        let d = tempfile::tempdir().unwrap();
        let base = d.path().canonicalize().unwrap();
        let file = base.join("x.txt").display().to_string();
        let glob = format!("{}/*", base.display());
        assert_eq!(always_saves(&req(&file, &glob)), vec![glob.clone()]);
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert_eq!(always_saves(&req(&file, &glob)), vec![file.clone()]);
        // A folder that does not exist yet is judged by the one it would be made in.
        let deeper = base.join("new/y.txt").display().to_string();
        assert_eq!(
            always_saves(&req(&deeper, &format!("{}/new/*", base.display()))),
            [deeper]
        );
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    // Only a bounded regular file is read: a FIFO or a device is skipped
    // without blocking or reading without end.
    #[cfg(unix)]
    #[test]
    fn peek_reads_only_bounded_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, "hello").unwrap();
        assert_eq!(peek_text(&f, 5), Peek::Text("hello".into()));
        assert_eq!(peek_text(&f, 4), Peek::Skipped);
        assert_eq!(peek_text(&dir.path().join("none"), 5), Peek::Missing);
        assert_eq!(peek_text(dir.path(), 5), Peek::Skipped);
        let fifo = dir.path().join("pipe");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert_eq!(peek_text(&fifo, 5), Peek::Skipped);
        assert_eq!(peek_text(Path::new("/dev/zero"), 5), Peek::Skipped);
        std::fs::write(&f, [0xff, 0xfe]).unwrap();
        assert_eq!(peek_text(&f, 5), Peek::Skipped);
    }

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

    // ---------------------------------------------------------------------
    // Path resolution follows symlinks and the on-disk spelling, so the
    // permission rules see where a path really leads.
    // ---------------------------------------------------------------------

    use crate::permission::{base_rules, Effect, Rule};

    /// Base rules with `external_directory` left to ask, plus `extra`.
    fn rules(extra: &[(&str, &str, Effect)]) -> Vec<Rule> {
        let mut r = base_rules();
        r.push(Rule::new("external_directory", "*", Effect::Ask));
        r.extend(extra.iter().map(|(a, p, e)| Rule::new(a, p, *e)));
        r
    }

    fn outside() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().canonicalize().unwrap();
        (d, p)
    }

    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn resolve_follows_symlinks() {
        let env = Env::new();
        let (_o, out) = outside();
        env.write(".env", "SECRET=1");
        symlink(&env.root().join(".env"), &env.root().join("notes.txt"));
        symlink(&out, &env.root().join("ext"));
        let ctx = env.ctx();
        let t = ctx.resolve("notes.txt");
        assert_eq!(t.resource, "notes.txt");
        assert_eq!(t.resources, vec!["notes.txt", ".env"]);
        assert!(!t.external);
        let t = ctx.resolve("ext/new/file.txt");
        assert!(t.external);
        assert_eq!(t.real, out.join("new/file.txt"));
        // A dangling symlink resolves to where a write through it lands.
        symlink(&out.join("missing.txt"), &env.root().join("dangling"));
        let t = ctx.resolve("dangling");
        assert!(t.external);
        assert_eq!(t.real, out.join("missing.txt"));
        // A relative dangling symlink, inside the project.
        symlink(Path::new("sub/later.txt"), &env.root().join("rel"));
        let t = ctx.resolve("rel");
        assert!(!t.external);
        assert_eq!(t.resources, vec!["rel", "sub/later.txt"]);
    }

    /// Whether the volume holding `dir` ignores case.
    fn ignores_case(dir: &Path) -> bool {
        std::fs::write(dir.join("case-probe"), "").unwrap();
        let yes = dir.join("CASE-PROBE").exists();
        std::fs::remove_file(dir.join("case-probe")).unwrap();
        yes
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upper_cased_path_meets_the_relative_deny() {
        let (env, _rec) = Env::recording(rules(&[("edit", "secrets/*", Effect::Deny)]));
        if !ignores_case(&env.root()) {
            eprintln!("skipped: the temp volume is case-sensitive");
            return;
        }
        let key = env.write("secrets/key", "original");
        let upper = key.display().to_string().to_uppercase();
        let ctx = env.ctx();
        let t = ctx.resolve(&upper);
        assert!(!t.external);
        assert_eq!(t.resource, "secrets/key");
        let e = Tool::Write
            .run(&ctx, &json!({"path": upper, "content": "pwned"}))
            .await
            .unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
        let e = Tool::Edit
            .run(
                &ctx,
                &json!({"path": upper, "oldString": "original", "newString": "pwned"}),
            )
            .await
            .unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
        assert_eq!(std::fs::read_to_string(&key).unwrap(), "original");
        // A new file under an upper-cased existing directory too.
        let new = env.root().join("secrets/new").display().to_string();
        let e = Tool::Write
            .run(&ctx, &json!({"path": new.to_uppercase(), "content": "x"}))
            .await
            .unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn symlink_to_env_asks_like_env() {
        let (env, rec) = Env::recording(rules(&[]));
        env.write(".env", "SECRET=hunter2");
        symlink(Path::new(".env"), &env.root().join("notes.txt"));
        rec.deny_action("read");
        let ctx = env.ctx();
        let e = Tool::Read
            .run(&ctx, &json!({"path": "notes.txt"}))
            .await
            .unwrap_err();
        assert!(!e.contains("hunter2"), "{e}");
        assert_eq!(
            rec.requests(),
            vec![(
                "read".to_string(),
                vec!["notes.txt".to_string(), ".env".to_string()]
            )]
        );
        // A deny written for the target applies through the link.
        let (env, _rec) = Env::recording(rules(&[("read", ".env", Effect::Deny)]));
        env.write(".env", "SECRET=hunter2");
        symlink(Path::new(".env"), &env.root().join("notes.txt"));
        let e = Tool::Read
            .run(&env.ctx(), &json!({"path": "notes.txt"}))
            .await
            .unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn symlinked_dir_outside_needs_external_approval() {
        let (env, rec) = Env::recording(rules(&[]));
        let (_o, out) = outside();
        std::fs::write(out.join("file.txt"), "outside data").unwrap();
        symlink(&out, &env.root().join("ext"));
        rec.deny_action("external_directory");
        let ctx = env.ctx();
        let e = Tool::Read
            .run(&ctx, &json!({"path": "ext/file.txt"}))
            .await
            .unwrap_err();
        assert!(!e.contains("outside data"), "{e}");
        let e = Tool::Read
            .run(&ctx, &json!({"path": "ext"}))
            .await
            .unwrap_err();
        assert!(!e.contains("file.txt"), "{e}");
        let e = Tool::Grep
            .run(&ctx, &json!({"pattern": "outside", "path": "ext"}))
            .await
            .unwrap_err();
        assert!(!e.contains("outside data"), "{e}");
        assert_eq!(
            rec.requests(),
            vec![
                (
                    "external_directory".to_string(),
                    vec![out.join("file.txt").display().to_string()]
                ),
                (
                    "external_directory".to_string(),
                    vec![format!("{}/*", out.display())]
                ),
                (
                    "external_directory".to_string(),
                    vec![format!("{}/*", out.display())]
                ),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_path_env_asks_for_read() {
        let (env, rec) = Env::recording(rules(&[]));
        env.write(".env", "SECRET=hunter2");
        rec.deny_action("read");
        let e = Tool::Grep
            .run(&env.ctx(), &json!({"pattern": "SECRET", "path": ".env"}))
            .await
            .unwrap_err();
        assert!(!e.contains("hunter2"), "{e}");
        assert_eq!(
            rec.requests(),
            vec![("read".to_string(), vec![".env".to_string()])]
        );
        // Approved, it searches the file.
        let (env, rec) = Env::recording(rules(&[]));
        env.write(".env", "SECRET=hunter2");
        let out = Tool::Grep
            .run(&env.ctx(), &json!({"pattern": "SECRET", "path": ".env"}))
            .await
            .unwrap()
            .content;
        assert!(out.contains("hunter2"), "{out}");
        assert_eq!(rec.actions(), vec!["read"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_over_a_directory_skips_files_reads_would_ask_or_deny() {
        let (env, rec) = Env::recording(rules(&[("read", "secrets/*", Effect::Deny)]));
        env.write("prod.env", "TOKEN=hunter2");
        env.write(".env", "TOKEN=hunter3");
        env.write("secrets/key", "TOKEN=hunter4");
        env.write("app.rs", "let TOKEN = 1;");
        let ctx = env.ctx();
        let out = Tool::Grep
            .run(&ctx, &json!({"pattern": "TOKEN"}))
            .await
            .unwrap()
            .content;
        assert!(rec.requests().is_empty());
        assert!(out.starts_with("Found 1 matches"), "{out}");
        assert!(!out.contains("hunter"), "{out}");
        assert!(
            out.ends_with("(Not searched, because reading them needs approval: prod.env. To search one, pass it as `path`.)"),
            "{out}"
        );
        assert!(!out.contains("secrets"), "{out}");
        // A denied directory named as `path` is refused outright.
        let e = Tool::Grep
            .run(&ctx, &json!({"pattern": "TOKEN", "path": "secrets/key"}))
            .await
            .unwrap_err();
        assert!(e.contains("Permission denied"), "{e}");
        // Glob leaves out files the read rules deny.
        let out = Tool::Glob
            .run(&ctx, &json!({"pattern": "**/*"}))
            .await
            .unwrap()
            .content;
        assert!(out.contains("app.rs") && !out.contains("secrets"), "{out}");
    }

    /// Every entry a non-following walk of `dir` finds, with whether it is
    /// a symlink.
    fn walk(dir: &Path) -> Vec<(PathBuf, bool)> {
        ignore::WalkBuilder::new(dir)
            .standard_filters(false)
            .follow_links(false)
            .build()
            .flatten()
            .map(|e| {
                let link = e.path_is_symlink();
                (e.into_path(), link)
            })
            .collect()
    }

    #[test]
    fn walked_paths_resolve_as_resolving_them_in_full_does() {
        let env = Env::new();
        let (_o, out) = outside();
        std::fs::write(out.join("o.txt"), "").unwrap();
        env.write(".env", "S=1");
        env.write("Src/Deep/Mixed.RS", "");
        env.write("secrets/key", "");
        env.write("a/b/c/d.txt", "");
        symlink(&env.root().join(".env"), &env.root().join("a/notes.txt"));
        symlink(&out, &env.root().join("a/ext"));
        symlink(&env.root().join("secrets"), &env.root().join("hidden"));
        symlink(&env.root(), &out.join("proj"));
        let ctx = env.ctx();
        let roots = ctx.roots();
        let mut bases = vec![".".to_string(), "a".into(), "hidden".into()];
        bases.push(out.join("proj/a").display().to_string());
        if ignores_case(&env.root()) {
            bases.push(env.root().join("SRC").display().to_string().to_uppercase());
        }
        let mut checked = 0;
        for b in &bases {
            let base = ctx.resolve(b);
            for (p, link) in walk(&base.absolute) {
                let full = ctx.resolve(&p.display().to_string());
                assert_eq!(ctx.resolve_walked(&roots, &base, &p, link), full, "{p:?}");
                checked += 1;
            }
        }
        assert!(checked > 10);
        // A symlinked directory searched by its link: entries are judged by
        // where they really are.
        let base = ctx.resolve("hidden");
        let key = base.absolute.join("key");
        let t = ctx.resolve_walked(&roots, &base, &key, false);
        assert_eq!(t.resources, vec!["hidden/key", "secrets/key"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grep_and_glob_judge_files_by_their_real_names() {
        let (env, rec) = Env::recording(rules(&[("read", "secrets/*", Effect::Deny)]));
        env.write("secrets/key", "TOKEN=hunter4");
        env.write("app.rs", "let TOKEN = 1;");
        symlink(&env.root().join("secrets"), &env.root().join("hidden"));
        let ctx = env.ctx();
        // Through a symlinked directory.
        let out = Tool::Grep
            .run(&ctx, &json!({"pattern": "TOKEN", "path": "hidden"}))
            .await
            .unwrap()
            .content;
        assert!(!out.contains("hunter"), "{out}");
        let out = Tool::Glob
            .run(&ctx, &json!({"pattern": "*", "path": "hidden"}))
            .await
            .unwrap()
            .content;
        assert_eq!(out, "No files found");
        // Through another spelling of the project.
        if ignores_case(&env.root()) {
            let upper = env.root().display().to_string().to_uppercase();
            let out = Tool::Grep
                .run(&ctx, &json!({"pattern": "TOKEN", "path": upper}))
                .await
                .unwrap()
                .content;
            assert!(
                out.starts_with("Found 1 matches") && !out.contains("hunter"),
                "{out}"
            );
            let out = Tool::Glob
                .run(&ctx, &json!({"pattern": "**/*", "path": upper}))
                .await
                .unwrap()
                .content;
            assert!(out.contains("APP.RS") || out.contains("app.rs"), "{out}");
            assert!(!out.to_lowercase().contains("secrets"), "{out}");
        }
        assert!(rec.requests().is_empty(), "{:?}", rec.requests());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_that_escape_the_project_ask_external() {
        let (env, rec) = Env::recording(rules(&[]));
        let (_o, out) = outside();
        let sub = env.root().join("sub");
        std::fs::create_dir(&sub).unwrap();
        symlink(&out, &env.root().join("ext"));
        symlink(&out.join("missing.txt"), &env.root().join("dangling"));
        rec.deny_action("external_directory");
        let mut ctx = env.ctx();
        ctx.cwd = sub;
        // `..` traversal out of the project.
        let up = format!(
            "../../{}/dotdot.txt",
            out.file_name().unwrap().to_string_lossy()
        );
        let parent = env.root().parent().unwrap().to_path_buf();
        assert_eq!(out.parent(), Some(parent.as_path()));
        for path in [up.as_str(), "../ext/new.txt", "../dangling"] {
            let e = Tool::Write
                .run(&ctx, &json!({"path": path, "content": "x"}))
                .await
                .unwrap_err();
            assert!(e.contains("declined"), "{path}: {e}");
        }
        assert_eq!(rec.actions(), vec!["external_directory"; 3]);
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
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
