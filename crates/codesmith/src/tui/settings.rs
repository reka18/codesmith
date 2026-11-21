//! The UI's settings: `tui` in `codesmith.json`, in the shape of
//! OpenCode's TUI config (`theme`, `keybinds`, `leader`, `cursor`,
//! `session`), merged across config files like the rest. OpenCode's first
//! format (`tui.json`: `"theme": "name"`, `leader_timeout`, `session_list`
//! keybind names) is read too and migrated as it loads, as OpenCode
//! migrates it. Settings for features CodeSmith does not have are ignored.

use super::group::{Grouping, Verbosity};
use super::keymap::{self, LEADER_TIMEOUT_MS};
use super::theme::ModeSetting;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    Block,
    Underline,
    Line,
    /// The terminal's own.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub style: CursorStyle,
    pub blinking: bool,
}

/// How reasoning shows: rendered as Markdown, or as its highlighted source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Markdown {
    #[default]
    Rendered,
    Source,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// `theme.name`.
    pub theme: Option<String>,
    /// `theme.mode`: `system`, `dark` or `light`.
    pub mode: Option<ModeSetting>,
    /// `keybinds`, by command id.
    pub keybinds: Map<String, Value>,
    /// `leader.timeout`, in milliseconds.
    pub leader_timeout: u64,
    pub cursor: Cursor,
    /// `session.thinking`: show reasoning from the start.
    pub thinking: Option<bool>,
    /// `session.markdown`.
    pub markdown: Markdown,
    /// `session.verbosity`: `low`, `medium` or `high`.
    pub verbosity: Verbosity,
    /// `session.grouping`: `auto` or `none`.
    pub grouping: Grouping,
    /// `diffs`: the diff viewer's defaults.
    pub diffs: Diffs,
    /// `tabs`: the session tabs.
    pub tabs: super::tabs::Settings,
    /// `debug.turn_tokens`: a token usage row after each turn.
    pub turn_tokens: bool,
    /// `session.tps`: show a turn's throughput in its footer.
    pub tps: bool,
    /// `session.new_location: "inherit"`: a new session starts in the
    /// folder the UI was opened in last (from the open menu) rather than
    /// the one it was launched in (`"launch"`, the default).
    pub new_inherit: bool,
    /// `terminal.title`: keep the terminal window's title on the session's.
    pub terminal_title: bool,
    /// `attention`: system notifications and sounds.
    pub attention: Attention,
    /// `prompt.paste: "full"`: pastes go in whole rather than as
    /// `[Pasted ~N lines]` chips (`"compact"`, the default).
    pub paste_full: bool,
}

/// The attention sounds, as OpenCode names them.
pub const SOUND_NAMES: &[&str] = &[
    "default",
    "question",
    "permission",
    "error",
    "done",
    "subagent_done",
];

/// `tui.attention`, as OpenCode's: whether to show system notifications
/// and play sounds (both off by default), the volume, and sound files by
/// event (CodeSmith ships no sounds of its own: without a file, a sound is
/// the terminal's bell).
#[derive(Debug, Clone, PartialEq)]
pub struct Attention {
    pub notifications: bool,
    pub sound: bool,
    /// 0 to 1; 0 is silent.
    pub volume: f64,
    /// Sound files by name (one of [`SOUND_NAMES`]).
    pub sounds: std::collections::BTreeMap<String, String>,
}

impl Default for Attention {
    fn default() -> Self {
        Attention {
            notifications: false,
            sound: false,
            volume: 0.4,
            sounds: Default::default(),
        }
    }
}

/// `tui.diffs`, as OpenCode's `diffs` settings: the source the diff viewer
/// opens on, and its layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Diffs {
    /// `branch`, `committed`, `working` or `turn`.
    pub source: Option<super::diff_source::Source>,
    pub tree: Option<bool>,
    pub single: Option<bool>,
    /// `auto` (`None` here), `split` or `unified`.
    pub view: Option<super::diff_view::View>,
    pub fullscreen: Option<bool>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: None,
            mode: None,
            keybinds: Map::new(),
            leader_timeout: LEADER_TIMEOUT_MS,
            cursor: Cursor {
                style: CursorStyle::Block,
                blinking: true,
            },
            thinking: None,
            markdown: Markdown::Rendered,
            verbosity: Verbosity::Low,
            grouping: Grouping::Auto,
            diffs: Diffs::default(),
            tabs: super::tabs::Settings::default(),
            terminal_title: true,
            tps: true,
            new_inherit: false,
            turn_tokens: false,
            attention: Attention::default(),
            paste_full: false,
        }
    }
}

/// OpenCode's first format, in the current shape: a theme name becomes
/// `theme.name`, `leader_timeout` `leader.timeout`, and keybind names
/// command ids (one set directly wins over its old name). Returns the
/// keybind names that are neither.
pub fn migrate(input: &Value) -> (Value, Vec<String>) {
    let Some(o) = input.as_object() else {
        return (input.clone(), vec![]);
    };
    let mut out = o.clone();
    if let Some(Value::String(name)) = o.get("theme") {
        out.insert("theme".into(), serde_json::json!({ "name": name }));
    }
    if let Some(t) = o.get("leader_timeout") {
        out.remove("leader_timeout");
        let leader = out
            .entry("leader")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(l) = leader.as_object_mut() {
            l.entry("timeout").or_insert_with(|| t.clone());
        }
    }
    let mut dropped = Vec::new();
    if let Some(Value::Object(k)) = o.get("keybinds") {
        let (k, d) = keymap::migrate(k);
        dropped = d;
        out.insert("keybinds".into(), Value::Object(k));
    }
    (Value::Object(out), dropped)
}

fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, k| cur.get(*k))
}

/// The settings in `tui` (null when unset), and what was wrong with them:
/// a value that does not fit is reported and its default used.
pub fn load(tui: &Value) -> (Settings, Vec<String>) {
    let (v, dropped) = migrate(tui);
    let mut s = Settings::default();
    let mut errors: Vec<String> = dropped
        .into_iter()
        .map(|k| format!("tui.keybinds: there is no command `{k}`"))
        .collect();
    let mut bad = |path: &str, want: &str| errors.push(format!("tui.{path}: {want}"));
    if !v.is_null() && !v.is_object() {
        bad("", "must be an object");
        return (s, errors);
    }
    match get(&v, &["theme", "name"]) {
        None => {}
        Some(Value::String(n)) if !n.is_empty() => s.theme = Some(n.clone()),
        Some(_) => bad("theme.name", "must be a theme's name"),
    }
    match get(&v, &["theme", "mode"]) {
        None => {}
        Some(m) => match m.as_str().and_then(ModeSetting::parse) {
            Some(m) => s.mode = Some(m),
            None => bad("theme.mode", "must be \"system\", \"dark\" or \"light\""),
        },
    }
    if let Some(k) = v.get("keybinds") {
        match k.as_object() {
            Some(k) => s.keybinds = k.clone(),
            None => bad("keybinds", "must be an object of command ids to keys"),
        }
    }
    match get(&v, &["leader", "timeout"]) {
        None => {}
        Some(t) => match t.as_u64().filter(|t| *t > 0) {
            Some(t) => s.leader_timeout = t,
            None => bad(
                "leader.timeout",
                "must be a whole number of milliseconds above 0",
            ),
        },
    }
    match get(&v, &["cursor", "style"]) {
        None => {}
        Some(c) => match c.as_str() {
            Some("block") => s.cursor.style = CursorStyle::Block,
            Some("underline") => s.cursor.style = CursorStyle::Underline,
            Some("line") => s.cursor.style = CursorStyle::Line,
            Some("default") => s.cursor.style = CursorStyle::Default,
            _ => bad(
                "cursor.style",
                "must be \"block\", \"underline\", \"line\" or \"default\"",
            ),
        },
    }
    match get(&v, &["cursor", "blinking"]) {
        None => {}
        Some(b) => match b.as_bool() {
            Some(b) => s.cursor.blinking = b,
            None => bad("cursor.blinking", "must be true or false"),
        },
    }
    match get(&v, &["session", "thinking"]) {
        None => {}
        Some(t) => match t.as_str() {
            Some("show") => s.thinking = Some(true),
            Some("hide") => s.thinking = Some(false),
            _ => bad("session.thinking", "must be \"show\" or \"hide\""),
        },
    }
    match get(&v, &["session", "markdown"]) {
        None => {}
        Some(m) => match m.as_str() {
            Some("rendered") => s.markdown = Markdown::Rendered,
            Some("source") => s.markdown = Markdown::Source,
            _ => bad("session.markdown", "must be \"rendered\" or \"source\""),
        },
    }
    match get(&v, &["session", "verbosity"]) {
        None => {}
        Some(m) => match m.as_str().and_then(Verbosity::parse) {
            Some(m) => s.verbosity = m,
            None => bad(
                "session.verbosity",
                "must be \"low\", \"medium\" or \"high\"",
            ),
        },
    }
    match get(&v, &["session", "grouping"]) {
        None => {}
        Some(m) => match m.as_str().and_then(Grouping::parse) {
            Some(m) => s.grouping = m,
            None => bad("session.grouping", "must be \"auto\" or \"none\""),
        },
    }
    match v.get("diffs") {
        None => {}
        Some(d) if !d.is_object() => bad("diffs", "must be an object"),
        Some(d) => {
            match d.get("source") {
                None => {}
                Some(m) => match m.as_str().and_then(super::diff_source::Source::parse) {
                    Some(m) => s.diffs.source = Some(m),
                    None => bad(
                        "diffs.source",
                        "must be \"branch\", \"committed\", \"working\" or \"turn\"",
                    ),
                },
            }
            for (key, slot) in [
                ("tree", &mut s.diffs.tree),
                ("single", &mut s.diffs.single),
                ("fullscreen", &mut s.diffs.fullscreen),
            ] {
                match d.get(key) {
                    None => {}
                    Some(b) => match b.as_bool() {
                        Some(b) => *slot = Some(b),
                        None => errors.push(format!("tui.diffs.{key}: must be true or false")),
                    },
                }
            }
            match d.get("view").map(|m| m.as_str()) {
                None | Some(Some("auto")) => {}
                Some(Some("split")) => s.diffs.view = Some(super::diff_view::View::Split),
                Some(Some("unified")) => s.diffs.view = Some(super::diff_view::View::Unified),
                Some(_) => {
                    errors.push("tui.diffs.view: must be \"auto\", \"split\" or \"unified\"".into())
                }
            }
        }
    }
    match get(&v, &["debug", "turn_tokens"]) {
        None => {}
        // OpenCode's "verbose" also lists each step's tool calls.
        Some(Value::Bool(b)) => s.turn_tokens = *b,
        Some(Value::String(v)) if v == "verbose" => s.turn_tokens = true,
        Some(_) => errors.push("tui.debug.turn_tokens: must be true, false or \"verbose\"".into()),
    }
    match get(&v, &["session", "tps"]) {
        None => {}
        Some(b) => match b.as_bool() {
            Some(b) => s.tps = b,
            None => errors.push("tui.session.tps: must be true or false".into()),
        },
    }
    match get(&v, &["session", "new_location"]) {
        None => {}
        Some(Value::String(p)) if p == "launch" => s.new_inherit = false,
        Some(Value::String(p)) if p == "inherit" => s.new_inherit = true,
        Some(_) => {
            errors.push("tui.session.new_location: must be \"launch\" or \"inherit\"".into())
        }
    }
    match get(&v, &["prompt", "paste"]) {
        None => {}
        Some(Value::String(p)) if p == "full" => s.paste_full = true,
        Some(Value::String(p)) if p == "compact" => s.paste_full = false,
        Some(_) => errors.push("tui.prompt.paste: must be \"compact\" or \"full\"".into()),
    }
    match get(&v, &["terminal", "title"]) {
        None => {}
        Some(b) => match b.as_bool() {
            Some(b) => s.terminal_title = b,
            None => errors.push("tui.terminal.title: must be true or false".into()),
        },
    }
    match v.get("attention") {
        None => {}
        Some(a) if !a.is_object() => errors.push("tui.attention: must be an object".into()),
        Some(a) => {
            for (key, slot) in [
                ("notifications", &mut s.attention.notifications),
                ("sound", &mut s.attention.sound),
            ] {
                match a.get(key) {
                    None => {}
                    Some(b) => match b.as_bool() {
                        Some(b) => *slot = b,
                        None => errors.push(format!("tui.attention.{key}: must be true or false")),
                    },
                }
            }
            match a.get("volume") {
                None => {}
                Some(n) => match n.as_f64().filter(|n| (0.0..=1.0).contains(n)) {
                    Some(n) => s.attention.volume = n,
                    None => {
                        errors.push("tui.attention.volume: must be a number from 0 to 1".into())
                    }
                },
            }
            match a.get("sounds") {
                None => {}
                Some(Value::Object(m)) => {
                    // As in OpenCode, a sound name it does not have is dropped.
                    for (k, f) in m {
                        match f.as_str() {
                            Some(f) if SOUND_NAMES.contains(&k.as_str()) => {
                                s.attention.sounds.insert(k.clone(), f.to_string());
                            }
                            Some(_) => {}
                            None => errors
                                .push(format!("tui.attention.sounds.{k}: must be a file path")),
                        }
                    }
                }
                Some(_) => errors
                    .push("tui.attention.sounds: must be an object of sound names to files".into()),
            }
        }
    }
    let (tabs, tab_errors) = super::tabs::load(&v, |k| std::env::var(k).ok());
    s.tabs = tabs;
    errors.extend(tab_errors);
    (s, errors)
}
