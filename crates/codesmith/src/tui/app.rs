//! UI state and input handling. No terminal I/O here: keys and session
//! events go in, state changes and `Action`s for the runner come out.

use super::bridge::{Answers, Ev};
use super::complete::{self, Kind, Popup};
use super::diff::Diff;
use super::editor::Editor;
use super::keymap::{self, scope, Keymap, Stroke};
use codesmith_ai::Usage;
use codesmith_core::config::Cost;
use codesmith_core::permission::{Reply, Request};
use codesmith_core::tool::todo::Todo;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::oneshot;

// Tool rows are most of a transcript, so boxing them would not save room.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool(Tool),
    Notice(String),
    Error(String),
    Todos(Vec<Todo>),
    /// An image (or other file) the model generated: its label.
    Media(String),
    /// The row after a turn: its agent, model, duration and throughput.
    Footer(Footer),
}

/// A turn's footer, as OpenCode's: `Build · model · 12.3s · 45.2 tok/s`.
#[derive(Debug, Clone, PartialEq)]
pub struct Footer {
    pub agent: String,
    pub model: String,
    pub duration_ms: u64,
    /// Output tokens a second, when shown (`tui.session.tps`) and known.
    pub tps: Option<f64>,
    pub interrupted: bool,
    /// Its steps' tokens, with `tui.debug.turn_tokens`.
    pub tokens: Vec<super::usage::Step>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub detail: String,
    /// The first line of the result and whether it failed; `None` while running.
    pub result: Option<(String, bool)>,
    /// The whole result.
    pub output: String,
    pub diff: Option<Diff>,
    /// Shown while the model streams the call; it has not started yet.
    pub forming: bool,
    /// What a running command printed so far (its latest [`LIVE_KEEP`]
    /// bytes), shown as a tail until the result replaces it.
    pub live: String,
    /// Labels of the images and files the call returned.
    pub media: Vec<String>,
    /// The subagent a `subagent` call runs, once it started.
    pub sub: Option<Box<Sub>>,
}

/// Most bytes of a running command's output kept for its row.
pub const LIVE_KEEP: usize = 64 * 1024;
/// Most of a subagent's latest tool calls kept for its row.
pub const SUB_TOOLS_KEEP: usize = 50;
/// Most bytes of a subagent's latest text kept for its row.
pub const SUB_TEXT_KEEP: usize = 8 * 1024;

/// How a subagent run stands.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum SubStatus {
    #[default]
    Running,
    Completed,
    Interrupted,
    Failed(String),
}

/// A subagent's activity, shown in the row of the call that runs it (as
/// OpenCode's task rows sum up their child session).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sub {
    pub session: String,
    /// The agent it runs, such as `explore`.
    pub agent: String,
    /// The call's description, the child session's title.
    pub title: String,
    pub status: SubStatus,
    /// Its latest tool calls (and its subagents'), at most [`SUB_TOOLS_KEEP`].
    pub tools: std::collections::VecDeque<Tool>,
    /// Every tool call it made.
    pub calls: usize,
    /// The text it wrote last (its latest [`SUB_TEXT_KEEP`] bytes).
    pub text: String,
    /// A tool call came after `text`: the next text starts over.
    text_done: bool,
}

impl Sub {
    fn push_text(&mut self, delta: &str) {
        if std::mem::take(&mut self.text_done) {
            self.text.clear();
        }
        self.text.push_str(delta);
        if self.text.len() > SUB_TEXT_KEEP {
            let mut cut = self.text.len() - SUB_TEXT_KEEP;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
        }
    }

    /// The running call `id` among its latest ones.
    fn running(&mut self, id: &str) -> Option<&mut Tool> {
        self.tools
            .iter_mut()
            .rev()
            .find(|t| t.result.is_none() && t.id == id)
    }
}

impl Tool {
    /// Add what the running call printed, keeping the latest
    /// [`LIVE_KEEP`] bytes from a line start where there is one.
    fn print(&mut self, delta: &str) {
        self.live.push_str(delta);
        if self.live.len() <= LIVE_KEEP {
            return;
        }
        let mut cut = self.live.len() - LIVE_KEEP;
        while !self.live.is_char_boundary(cut) {
            cut += 1;
        }
        if let Some(nl) = self.live[cut..].find('\n') {
            cut += nl + 1;
        }
        self.live.drain(..cut);
    }
}

/// How a returned or generated file is listed: `[image: a.png] image/png, 2 KB`.
pub fn media_label(m: &codesmith_ai::Media) -> String {
    let bytes = m.inline_len() / 4 * 3;
    let size = match bytes {
        0 => String::new(),
        b if b < 1024 => format!(", {b} B"),
        b if b < 1024 * 1024 => format!(", {} KB", (b + 512) / 1024),
        b => format!(", {:.1} MB", b as f64 / (1024.0 * 1024.0)),
    };
    format!("{} {}{size}", codesmith_core::media::label(m), m.mime)
}

impl Tool {
    /// The tool's own name, without a subagent prefix ("explore › read").
    pub fn base(&self) -> &str {
        self.name.rsplit(" › ").next().unwrap_or(&self.name)
    }
}

/// What a picker chooses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pick {
    Session,
    /// A message to fork the session before ("" forks all of it).
    Fork,
    Model,
    Agent,
    Theme,
    /// The diff viewer's source chooser.
    DiffSource,
    /// The diff viewer's base branch picker.
    DiffBase,
    /// The command palette (`command.palette.show`, ctrl+p).
    Command,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PickItem {
    pub id: String,
    pub label: String,
    pub detail: String,
    /// Marks the current choice.
    pub current: bool,
}

pub struct Picker {
    pub kind: Pick,
    pub title: String,
    pub items: Vec<PickItem>,
    pub filter: String,
    /// Indexes into `items` matching the filter, best first.
    pub shown: Vec<usize>,
    /// Position in `shown`.
    pub sel: usize,
    /// Typing filters the list (the diff source chooser has no filter).
    pub filterable: bool,
    /// Shown instead of the list while it is empty (a list still loading).
    pub empty: Option<String>,
    /// A note under the list.
    pub footer: Option<String>,
}

impl Picker {
    pub fn new(kind: Pick, title: &str, items: Vec<PickItem>) -> Self {
        let mut p = Picker {
            kind,
            title: title.into(),
            items,
            filter: String::new(),
            shown: vec![],
            sel: 0,
            filterable: true,
            empty: None,
            footer: None,
        };
        p.refilter();
        p.sel = p
            .shown
            .iter()
            .position(|&i| p.items[i].current)
            .unwrap_or(0);
        p
    }

    fn refilter(&mut self) {
        let keys: Vec<String> = self
            .items
            .iter()
            .map(|i| format!("{} {}", i.label, i.detail))
            .collect();
        self.shown = complete::filter(&keys, &self.filter, |k| k.as_str(), false, usize::MAX);
        self.sel = 0;
    }

    /// Filter the list by `filter`.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.refilter();
    }

    /// Move the selection to item `id`, when it is shown.
    pub fn select(&mut self, id: &str) {
        if let Some(at) = self.shown.iter().position(|&i| self.items[i].id == id) {
            self.sel = at;
        }
    }

    pub fn selected(&self) -> Option<&PickItem> {
        self.shown.get(self.sel).map(|&i| &self.items[i])
    }

    /// Replace the list (a refreshed one), keeping the filter and, when it
    /// is still listed, the selected item.
    pub fn replace_items(&mut self, items: Vec<PickItem>) {
        let selected = self.selected().map(|i| i.id.clone());
        self.items = items;
        self.refilter();
        if let Some(pos) =
            selected.and_then(|id| self.shown.iter().position(|&i| self.items[i].id == id))
        {
            self.sel = pos;
        }
    }
}

pub enum Modal {
    Permission {
        req: Request,
        reply: Option<oneshot::Sender<Reply>>,
        feedback: String,
        /// The edit being asked about, when known.
        diff: Option<Diff>,
        /// The subagent asking, when one is: `explore subagent · Find the adder`.
        from: Option<String>,
        /// First row of the request text shown when it is too long to fit
        /// (the view clamps it).
        scroll: std::cell::Cell<usize>,
    },
    /// A question form (the question tool's, or an MCP server's).
    Question {
        form: Box<super::form::Form>,
        reply: Option<FormReply>,
    },
    Picker(Picker),
    /// The subagent and shell panel.
    Panel(super::panel::Panel),
    /// A background command's output.
    ShellOutput(Box<super::panel::ShellView>),
    /// The help, scrolled this many rows.
    Help(usize),
    /// The diff viewer's shortcuts, scrolled this many rows.
    DiffHelp(usize),
}

/// Where a question form's answer goes.
pub enum FormReply {
    /// The question tool's prompts (this many), answered by label.
    Questions(usize, oneshot::Sender<Answers>),
    /// A whole form; `None` when dismissed.
    Form(oneshot::Sender<Option<codesmith_core::form::Answer>>),
}

impl FormReply {
    /// Send the answer (`None`: dismissed). False when nobody waits for
    /// it any more (the turn ended).
    pub fn send(self, answer: Option<codesmith_core::form::Answer>) -> bool {
        match self {
            FormReply::Questions(n, tx) => tx
                .send(answer.map(|a| super::form::question_answers(n, &a)))
                .is_ok(),
            FormReply::Form(tx) => tx.send(answer).is_ok(),
        }
    }
}

/// What the runner should do after a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Submit(String),
    Slash {
        name: String,
        args: String,
    },
    Interrupt,
    /// Move the running turn's foreground commands to the background.
    Background,
    Quit,
    Picked(Pick, String),
    /// Switch to the next (+1) or previous (-1) primary agent.
    CycleAgent(i32),
    /// A picker's selection moved (the theme picker previews it).
    Preview(Pick, String),
    /// A picker closed without a choice.
    Cancelled(Pick),
    /// Add the model to the favourites, or take it off (ctrl+f in the
    /// model picker).
    Favorite(String),
    /// Stop the background command (the shell panel's kill).
    KillShell(String),
    /// Open the URL in the browser (a form's external link).
    OpenUrl(String),
    /// A command the runner carries out: `theme.switch_mode`,
    /// `theme.mode.lock`, `terminal.suspend`.
    Run(&'static str),
}

#[derive(Default)]
pub struct Header {
    pub agent: String,
    pub model: String,
    pub session: String,
    pub title: Option<String>,
    /// The model's context window, when known.
    pub context_limit: Option<u64>,
    /// The model's price, when configured.
    pub cost: Option<Cost>,
}

#[derive(Default)]
pub struct App {
    pub entries: Vec<Entry>,
    pub input: Editor,
    pub history: Vec<String>,
    /// The files each history entry attached, by index.
    pub history_files: Vec<Vec<super::attach::Pending>>,
    /// Prompts submitted since the runner last kept them in the history file.
    pub history_new: Vec<serde_json::Value>,
    hist: Option<usize>,
    /// What was typed before stepping into the history, restored when
    /// stepping back past the newest entry.
    draft: String,
    /// Lines scrolled up from the bottom of the transcript.
    pub scroll: usize,
    pub busy: bool,
    pub header: Header,
    pub usage: Usage,
    /// The session's spending: billed costs, and tokens to price.
    pub spend: codesmith_core::stats::Spend,
    /// Tokens the last model call used (prompt plus reply): the context in use.
    pub context: u64,
    pub modal: Option<Modal>,
    pub show_reasoning: bool,
    /// Show tool output and diffs in full rather than collapsed, and
    /// every group open.
    pub expand_tools: bool,
    /// How much tool activity the transcript shows.
    pub verbosity: super::group::Verbosity,
    /// Whether adjacent reads and searches are one row.
    pub grouping: super::group::Grouping,
    pub quit: bool,
    /// Slash commands offered by autocomplete.
    pub commands: Vec<complete::Item>,
    /// Project files offered for `@` mentions.
    pub files: Vec<String>,
    pub popup: Option<Popup>,
    /// The input text when the popup was dismissed with Esc (it stays closed
    /// until the text changes).
    dismissed: Option<String>,
    /// The keys of a binding typed so far (the leader key, say), and when
    /// the last one came.
    pending: Vec<Stroke>,
    pending_at: Option<Instant>,
    /// The key bindings.
    pub keymap: Arc<Keymap>,
    /// Show reasoning as its Markdown source rather than rendered.
    pub reasoning_source: bool,
    /// Pasted image and PDF paths, attached on submit while their
    /// placeholder is still in the prompt.
    pub attachments: Vec<super::attach::Pending>,
    /// Where pasted relative paths resolve.
    pub cwd: std::path::PathBuf,
    /// The session's background commands still running.
    pub background: usize,
    /// The subagents running now, outermost first.
    children: Vec<Child>,
    /// The call running the outermost of them, whose row shows them.
    sub_call: Option<String>,
    /// The diff viewer, while open.
    pub diff: Option<super::diff_view::Viewer>,
    /// What the diff viewer remembers until the UI exits.
    pub diff_memory: super::diff_view::Memory,
    /// The diff viewer's layout choices (`tui.diffs` and the remembered ones).
    pub diff_prefs: super::diff_view::Prefs,
    /// `tui.diffs.source`.
    pub diff_source: Option<super::diff_source::Source>,
    /// The layout choices changed: the runner remembers them.
    pub diff_prefs_changed: bool,
    /// Mini mode's settings, in mini mode.
    pub mini: Option<super::mini::Settings>,
    /// Entries before this one are in the terminal's scrollback (mini
    /// mode) and are left as they are.
    pub sealed: usize,
    /// `entries` was replaced (not appended to) since the last frame; the
    /// first this many came with the replacement.
    pub replaced: Option<usize>,
    /// The session tab strip, while tabs are on (set before each frame).
    pub tabs: Option<super::tabs::Strip>,
    /// The titles of sessions the transcript mentions, by ID (`None` for
    /// one that is archived or not in the store), which its links show.
    pub session_labels: std::collections::HashMap<String, Option<String>>,
    /// Permission requests and questions shown so far (each one's number,
    /// for the notifications).
    pub asks: u64,
    /// How the last turn ended, until the notifications have seen it.
    pub ended: Option<TurnEnd>,
    /// Subagents that started or ended, until the notifications have seen
    /// them.
    pub child_log: Vec<ChildMark>,
    /// The transcript's width and height as last drawn.
    pub view_size: std::cell::Cell<(u16, u16)>,
    /// Blank rows below the transcript, so a message jumped to near the
    /// end can be at the top.
    pub nav_slack: usize,
    /// The entry last jumped to.
    pub nav: Option<usize>,
    /// The running turn's steps, timed as they come in.
    pub recorder: Option<super::turn::Recorder>,
    /// `tui.session.tps`: show the turn's throughput.
    pub tps: bool,
    /// The last turn's duration (ms) and throughput.
    pub last_turn: Option<(u64, Option<f64>)>,
    /// Text to copy to the terminal's clipboard (`OSC 52`) before the
    /// next frame.
    pub clipboard: Option<String>,
    /// `tui.debug.turn_tokens`: a usage row after each turn.
    pub turn_tokens: bool,
    /// The session's steps so far, for the turn usage rows.
    pub usage_log: super::usage::Log,
    /// Question forms that came while a picker or the help was open.
    pub held: Vec<Modal>,
    /// Paste chips (`[Pasted ~4 lines]`) and what each stands for, sent
    /// in their place while the chip is whole in the prompt.
    pub pasted: Vec<(String, String)>,
    /// `tui.prompt.paste: "full"`: pastes go in whole, not as chips.
    pub paste_full: bool,
    /// The session's background commands, for the shell panel (kept
    /// current by the runner while the panel is open).
    pub shells: Vec<super::panel::Shell>,
    /// The subagent panel lists the inactive subagents (ctrl+a), kept
    /// until the UI exits as OpenCode keeps its filter.
    pub panel_inactive: bool,
    /// The toasts.
    pub toasts: super::toast::Toasts,
    /// The session's parent, for a subagent's session.
    pub parent: Option<String>,
    /// The session's goal (`/goal`), shown in the status line.
    pub goal: Option<codesmith_core::plugin::goal::Status>,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    Done,
    Failed(String),
    Interrupted,
}

/// A subagent starting (with its title) or ending (with its status).
#[derive(Debug, Clone, PartialEq)]
pub enum ChildMark {
    Started { id: String, title: String },
    Ended { id: String, end: TurnEnd },
}

/// A running subagent, as the UI tracks it.
#[derive(Debug, Clone, PartialEq)]
struct Child {
    id: String,
    agent: String,
    title: String,
}

/// What the prompt captures and its label (OpenCode's `PromptTraits`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromptTraits {
    pub capture: Option<Vec<&'static str>>,
    pub suspend: Option<bool>,
    pub status: Option<&'static str>,
}

pub fn prompt_traits(shell: bool, autocomplete: bool) -> PromptTraits {
    if shell {
        return PromptTraits {
            status: Some("SHELL"),
            ..Default::default()
        };
    }
    PromptTraits {
        capture: Some(if autocomplete {
            vec!["escape", "navigate", "submit", "tab"]
        } else {
            vec!["tab"]
        }),
        ..Default::default()
    }
}

/// Whether the palette lists command `id`: the prompt's commands, less
/// its editing, scrolling and numbered-tab keys.
fn palette_command(id: &str) -> bool {
    !(id.starts_with("input.")
        || id.starts_with("prompt.autocomplete.")
        || id.starts_with("prompt.history.")
        || id.starts_with("session.tab.select.")
        || id.starts_with("session.page.")
        || id.starts_with("session.half.page.")
        || id.starts_with("session.line.")
        || matches!(
            id,
            "prompt.clear" | "session.interrupt" | "command.palette.show" | "app.clear"
        ))
}

/// The slash commands the help lists.
const HELP_COMMANDS: &str = "/new            start a new session
/sessions       pick a session to continue
/fork           fork the session (all of it, or before a prompt you pick)
/undo, /redo    undo or redo the latest turn's file changes
/agent [id]     switch agent (build, plan, …)
/model [p/m]    switch model (p/m#variant picks a variant)
/variant [name] cycle the model's variants (reasoning effort), or pick one
/themes [name]  pick a theme (/theme too); /themes dark|light|system sets the mode
/compact        summarize older history now
/clear          clear the screen (the session is kept)
/thinking       show or hide reasoning
/verbosity [l]  cycle the transcript's verbosity, or set low, medium or high
/diff           open or close the diff viewer and review panel (? there: its keys)
/editor         edit the prompt in $VISUAL or $EDITOR
/help           this help
/exit           quit
/calibrate      survey the codebase and choose review dimensions
/review [what]  review changes (uncommitted, a commit, a..b, a branch, #pr)
/<command>      run a custom or built-in command (/init, …)";

/// The commands the help lists, in order, with CodeSmith's wording where
/// OpenCode's description does not fit.
const HELP_KEYS: &[(&str, &str)] = &[
    ("input.submit", "send"),
    ("input.newline", "new line"),
    (
        "input.move.up",
        "move up a line, then step through earlier messages",
    ),
    (
        "input.move.down",
        "move down a line, then step through later messages",
    ),
    ("input.word.backward", "move a word left"),
    ("input.word.forward", "move a word right"),
    ("input.line.home", "start of the line"),
    ("input.line.end", "end of the line"),
    ("input.delete.word.backward", "delete the previous word"),
    ("input.delete.word.forward", "delete the next word"),
    ("input.delete.to.line.end", "delete to the end of the line"),
    (
        "input.delete.to.line.start",
        "delete to the start of the line",
    ),
    ("input.delete.line", "delete the line"),
    (
        "prompt.clear",
        "clear the prompt (interrupts a running turn)",
    ),
    (
        "session.interrupt",
        "interrupt the running turn (clears the prompt when idle)",
    ),
    ("app.exit", "quit (on an empty prompt)"),
    ("agent.cycle", "next primary agent"),
    ("agent.cycle.reverse", "previous primary agent"),
    ("agent.list", "pick an agent"),
    ("model.list", "pick a model"),
    ("variant.cycle", "next model variant"),
    ("model.cycle_recent", "next recently used model"),
    ("model.cycle_recent_reverse", "previous recently used model"),
    ("session.list", "pick a session"),
    ("session.new", "new session"),
    ("session.tab.next", "next session tab"),
    ("session.tab.previous", "previous session tab"),
    (
        "session.tab.next_unread",
        "next tab that is unread or waiting for you",
    ),
    ("session.tab.close", "close the session tab"),
    ("session.tab.reopen", "reopen the last closed tab"),
    ("session.tab.move.left", "move the tab left"),
    ("session.tab.move.right", "move the tab right"),
    ("session.compact", "compact the session"),
    ("session.undo", "undo the latest turn's file changes"),
    ("session.redo", "redo them"),
    (
        "session.background",
        "move running commands to the background",
    ),
    (
        "session.toggle.tool_output",
        "expand or collapse tool output and groups",
    ),
    (
        "session.toggle.exploration_grouping",
        "group reads and searches, or show each",
    ),
    (
        "session.verbosity.cycle",
        "cycle the transcript's verbosity",
    ),
    ("session.toggle.thinking", "show or hide reasoning"),
    ("theme.switch", "pick a theme"),
    ("theme.switch_mode", "switch between light and dark"),
    ("theme.mode.lock", "pin or follow the terminal's mode"),
    ("session.page.up", "scroll up a page"),
    ("session.page.down", "scroll down a page"),
    ("session.half.page.up", "scroll up half a page"),
    ("session.half.page.down", "scroll down half a page"),
    ("session.line.up", "scroll up a line"),
    ("session.line.down", "scroll down a line"),
    ("session.first", "scroll to the top"),
    ("session.last", "follow the bottom again"),
    ("terminal.suspend", ""),
    ("prompt.editor", "edit the prompt in $VISUAL or $EDITOR"),
    ("help.show", "this help"),
    ("diff.open", "open or close the diff viewer"),
    ("pane.focus.right", "focus the docked diff viewer"),
];

/// The help: every bound command with its keys, then the slash commands.
pub fn help_text(k: &Keymap) -> String {
    let mut out = String::from("Keys (rebind them in tui.keybinds)\n");
    for (id, desc) in HELP_KEYS {
        let keys = k.list(id);
        if keys.is_empty() {
            continue;
        }
        let desc = match *desc {
            "" => keymap::def(id).map(|d| d.desc).unwrap_or_default(),
            d => d,
        };
        out.push_str(&format!("{:<30} {desc}\n", keys.join(", ")));
    }
    out.push_str(&format!(
        "{:<30} complete commands and files (also {}, {})\n",
        "/ and @",
        k.hint("prompt.autocomplete.complete"),
        k.hint("prompt.autocomplete.select")
    ));
    out.push('\n');
    out.push_str(HELP_COMMANDS);
    out
}

/// Built-in slash commands for autocomplete: (name, description).
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("new", "start a new session"),
    ("sessions", "pick a session to continue"),
    ("fork", "fork the session, all of it or before a prompt"),
    ("undo", "undo the latest turn's file changes"),
    ("redo", "redo the latest undone turn"),
    ("agent", "switch agent"),
    ("model", "switch model"),
    (
        "variant",
        "cycle or pick the model variant (reasoning effort)",
    ),
    ("compact", "summarize older history now"),
    ("clear", "clear the screen"),
    ("thinking", "show or hide reasoning"),
    (
        "verbosity",
        "cycle the transcript's verbosity, or set low, medium or high",
    ),
    ("help", "keys and commands"),
    ("diff", "open or close the diff viewer and review panel"),
    ("editor", "edit the prompt in $VISUAL or $EDITOR"),
    ("themes", "pick a theme, or set the light or dark mode"),
    ("exit", "quit"),
];

/// Pasted text without terminal escape sequences (CSI, OSC and other
/// `ESC`-introduced ones), which could restyle or move the screen. Other
/// control characters are kept.
pub fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates up to a final byte.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC, DCS, SOS, PM, APC: up to BEL or ESC \.
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-character sequence (or a lone ESC at the end).
            _ => {}
        }
    }
    out
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

impl App {
    pub fn new() -> Self {
        let mut a = App::default();
        a.set_commands(&[]);
        a
    }

    /// Set the autocomplete commands: the built-ins plus `extra`.
    pub fn set_commands(&mut self, extra: &[(String, String)]) {
        let mut cmds: Vec<complete::Item> = BUILTIN_COMMANDS
            .iter()
            .map(|(n, d)| complete::Item {
                label: n.to_string(),
                detail: d.to_string(),
            })
            .collect();
        for (n, d) in extra {
            if !cmds.iter().any(|c| &c.label == n) {
                cmds.push(complete::Item {
                    label: n.clone(),
                    detail: d.clone(),
                });
            }
        }
        self.commands = cmds;
    }

    /// Hand what all session tabs share (key bindings, completions, the
    /// prompt history, display settings, the diff viewer's memory) to
    /// `to`, the tab about to be shown. Done in pairs, so the shared state
    /// comes back with the shown tab.
    pub fn hand_over(&mut self, to: &mut App) {
        to.keymap = self.keymap.clone();
        std::mem::swap(&mut self.commands, &mut to.commands);
        std::mem::swap(&mut self.files, &mut to.files);
        std::mem::swap(&mut self.history, &mut to.history);
        std::mem::swap(&mut self.history_files, &mut to.history_files);
        std::mem::swap(&mut self.diff_memory, &mut to.diff_memory);
        std::mem::swap(&mut self.tabs, &mut to.tabs);
        std::mem::swap(&mut self.session_labels, &mut to.session_labels);
        to.show_reasoning = self.show_reasoning;
        to.expand_tools = self.expand_tools;
        to.verbosity = self.verbosity;
        to.grouping = self.grouping;
        to.tps = self.tps;
        to.paste_full = self.paste_full;
        to.panel_inactive = self.panel_inactive;
        to.turn_tokens = self.turn_tokens;
        to.reasoning_source = self.reasoning_source;
        to.diff_prefs = self.diff_prefs;
        to.diff_source = self.diff_source;
        to.mini = self.mini.clone();
        to.cwd = self.cwd.clone();
    }

    pub fn notice(&mut self, s: impl Into<String>) {
        self.entries.push(Entry::Notice(s.into()));
    }

    /// Replace the transcript (a session loaded or cleared).
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        self.replaced = Some(entries.len());
        self.sealed = 0;
        self.entries = entries;
    }

    pub fn error(&mut self, s: impl Into<String>) {
        self.entries.push(Entry::Error(s.into()));
    }

    /// The title a mention of session `id` shows: none for this session
    /// or one the store does not have (or has archived).
    pub fn session_label(&self, id: &str) -> Option<String> {
        if id == self.header.session {
            return None;
        }
        let title = self.session_labels.get(id)?.as_deref()?;
        Some(super::view::display_safe(title))
    }

    /// Mentioned sessions whose titles are not looked up yet.
    pub fn unknown_mentions(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.entries {
            let Entry::Assistant(t) = e else { continue };
            if !t.contains("ses_") {
                continue;
            }
            for m in super::mention::session_mentions(t) {
                if !self.session_labels.contains_key(m.id) && !out.iter().any(|o| o == m.id) {
                    out.push(m.id.to_string());
                }
            }
        }
        out
    }

    /// A turn starts now.
    pub fn start_turn(&mut self) {
        self.busy = true;
        self.recorder = Some(super::turn::Recorder::start());
        self.last_turn = None;
    }

    /// The turn ended: its footer (when a model step ran), outside mini
    /// mode, which writes its own turn summary.
    pub fn end_turn(&mut self, interrupted: bool) {
        let Some(r) = self.recorder.take() else {
            return;
        };
        if !r.stepped() {
            return;
        }
        let (ms, tps) = r.finish();
        let tps = tps.filter(|_| self.tps);
        self.last_turn = Some((ms, tps));
        let model = super::usage::Model::parse(&self.header.model);
        let mut tokens = self.usage_log.turn(&r.usage, &model);
        if !self.turn_tokens {
            tokens.clear();
        }
        if self.mini.is_some() {
            return;
        }
        self.entries.push(Entry::Footer(Footer {
            tokens,
            agent: self.header.agent.clone(),
            model: self.header.model.clone(),
            duration_ms: ms,
            tps,
            interrupted,
        }));
    }

    /// The subagents running now (the outermost ones' count).
    pub fn running_children(&self) -> usize {
        usize::from(!self.children.is_empty())
    }

    /// A subagent of this tab's session still runs.
    pub fn running_child(&self) -> bool {
        !self.children.is_empty()
    }

    /// The share of the context window in use, as a percentage.
    pub fn context_percent(&self) -> Option<u64> {
        let limit = self.header.context_limit.filter(|l| *l > 0)?;
        (self.context > 0).then(|| (self.context * 100).div_ceil(limit).min(100))
    }

    /// The session's cost so far in dollars, when the model is priced.
    pub fn cost(&self) -> Option<f64> {
        self.spend.cost(self.header.cost.as_ref())
    }

    /// The running call `id`, the latest one by that id.
    fn running(&mut self, id: &str) -> Option<&mut Tool> {
        self.entries.iter_mut().rev().find_map(|e| match e {
            Entry::Tool(t) if t.result.is_none() && !t.forming && t.id == id => Some(t),
            _ => None,
        })
    }

    /// The activity of the running outermost subagent, in its call's row.
    fn sub(&mut self) -> Option<&mut Sub> {
        let id = self.sub_call.clone()?;
        self.running(&id)?.sub.as_deref_mut()
    }

    /// A subagent's event, when one runs and its row is shown: `None`
    /// once handled.
    fn child_event(&mut self, ev: Ev) -> Option<Ev> {
        if self.children.is_empty() {
            return Some(ev);
        }
        let Some(sub) = self.sub() else {
            return Some(ev);
        };
        match ev {
            Ev::ToolStart {
                id,
                name,
                detail,
                diff,
            } => {
                // "explore › read" is the subagent's own "read".
                let name = match name.split_once(" › ") {
                    Some((_, rest)) => rest.to_string(),
                    None => name,
                };
                sub.tools.push_back(Tool {
                    id,
                    name,
                    detail,
                    diff,
                    ..Default::default()
                });
                if sub.tools.len() > SUB_TOOLS_KEEP {
                    sub.tools.pop_front();
                }
                sub.calls += 1;
                sub.text_done = true;
            }
            Ev::ToolEnd {
                id, output, error, ..
            } => {
                if let Some(t) = sub.running(&id) {
                    // Only the summary is kept: the parent's row stays small.
                    t.result = Some((first_line(&output), error));
                    t.diff = None;
                }
            }
            Ev::ChildText(d) => sub.push_text(&d),
            // A subagent's command output and media stay in its session.
            Ev::ToolOutput { .. } | Ev::ToolMedia { .. } => {}
            other => return Some(other),
        }
        None
    }

    /// Apply a session event.
    pub fn on_event(&mut self, ev: Ev) {
        let Some(ev) = self.child_event(ev) else {
            return;
        };
        if let Some(r) = &mut self.recorder {
            match &ev {
                Ev::Text(_) | Ev::Reasoning(_) | Ev::ToolInput { .. } => r.token(),
                Ev::ToolEnd { .. } => r.tool_ended(),
                Ev::StepEnd(u) if self.children.is_empty() => {
                    r.step_ended(u.output.saturating_sub(u.reasoning), u.reasoning);
                    r.usage.push((*u, false));
                }
                _ => {}
            }
        }
        match ev {
            Ev::Diff(r) => self.diff_answer(*r),
            Ev::Text(d) => match self.entries.last_mut() {
                Some(Entry::Assistant(t)) => t.push_str(&d),
                _ => self.entries.push(Entry::Assistant(d)),
            },
            Ev::Reasoning(d) => match self.entries.last_mut() {
                Some(Entry::Reasoning(t)) => t.push_str(&d),
                _ => self.entries.push(Entry::Reasoning(d)),
            },
            Ev::ToolInput { id, name } => self.entries.push(Entry::Tool(Tool {
                id,
                name,
                forming: true,
                ..Default::default()
            })),
            Ev::ToolInputEnd { id, call } => {
                let at = self
                    .entries
                    .iter()
                    .rposition(|e| matches!(e, Entry::Tool(t) if t.forming && t.id == id));
                match (at, call) {
                    (Some(i), Some(call)) => {
                        if let Entry::Tool(t) = &mut self.entries[i] {
                            t.id = call;
                        }
                    }
                    (Some(i), None) => {
                        self.entries.remove(i);
                    }
                    (None, _) => {}
                }
            }
            Ev::ToolStart {
                id,
                name,
                detail,
                diff,
            } => {
                // The row shown while the call streamed becomes the call.
                let formed = self.entries.iter_mut().find_map(|e| match e {
                    Entry::Tool(t) if t.forming && t.id == id => Some(t),
                    _ => None,
                });
                match formed {
                    Some(t) => {
                        t.forming = false;
                        t.name = name;
                        t.detail = detail;
                        t.diff = diff;
                    }
                    None => self.entries.push(Entry::Tool(Tool {
                        id,
                        name,
                        detail,
                        diff,
                        ..Default::default()
                    })),
                }
            }
            Ev::ToolEnd {
                id,
                name,
                output,
                error,
            } => {
                if let Some(Entry::Tool(t)) = self.entries.iter_mut().rev().find(|e| {
                    matches!(e, Entry::Tool(t) if t.result.is_none() && !t.forming
                        && (t.id == id || (id.is_empty() && t.name == name)))
                }) {
                    t.result = Some((first_line(&output), error));
                    t.live = String::new();
                    if error {
                        // A failed edit changed nothing.
                        t.diff = None;
                    }
                    t.output = output;
                }
            }
            Ev::ToolOutput { id, delta } => {
                if let Some(t) = self.running(&id) {
                    t.print(&delta);
                }
            }
            Ev::ToolMedia { id, labels } => {
                if let Some(t) = self.running(&id) {
                    t.media.extend(labels);
                }
            }
            Ev::Media(label) => self.entries.push(Entry::Media(label)),
            Ev::ChildStart { id, title, agent } => {
                if self.children.is_empty() {
                    let call = self.entries.iter_mut().rev().find_map(|e| match e {
                        Entry::Tool(t)
                            if t.result.is_none()
                                && !t.forming
                                && t.sub.is_none()
                                && t.base() == "subagent" =>
                        {
                            Some(t)
                        }
                        _ => None,
                    });
                    if let Some(t) = call {
                        t.sub = Some(Box::new(Sub {
                            session: id.clone(),
                            agent: agent.clone(),
                            title: title.clone(),
                            ..Default::default()
                        }));
                        self.sub_call = Some(t.id.clone());
                    }
                }
                self.child_log.push(ChildMark::Started {
                    id: id.clone(),
                    title: title.clone(),
                });
                self.children.push(Child { id, agent, title });
            }
            Ev::ChildEnd { id, status, error } => {
                self.child_log.push(ChildMark::Ended {
                    id: id.clone(),
                    end: match status.as_str() {
                        "completed" => TurnEnd::Done,
                        "interrupted" => TurnEnd::Interrupted,
                        _ => TurnEnd::Failed(error.clone().unwrap_or_else(|| "failed".into())),
                    },
                });
                if let Some(at) = self.children.iter().rposition(|c| c.id == id) {
                    self.children.truncate(at);
                }
                if self.children.is_empty() {
                    if let Some(sub) = self.sub() {
                        sub.status = match status.as_str() {
                            "completed" => SubStatus::Completed,
                            "interrupted" => SubStatus::Interrupted,
                            _ => SubStatus::Failed(error.unwrap_or_default()),
                        };
                    }
                    self.sub_call = None;
                }
            }
            // Text of a subagent whose row is not shown.
            Ev::ChildText(_) => {}
            Ev::Todos(t) => {
                // A list already in the scrollback stays there.
                let sealed = self.sealed;
                let mut i = 0;
                self.entries.retain(|e| {
                    i += 1;
                    i <= sealed || !matches!(e, Entry::Todos(_))
                });
                self.entries.push(Entry::Todos(t));
            }
            Ev::Notice(n) => self.notice(n),
            Ev::Compacted(b, a) => {
                self.usage_log.compacted();
                self.context = a;
                self.notice(format!("compacted context: ~{b} → ~{a} tokens"))
            }
            Ev::StepEnd(u) => {
                codesmith_core::stats::add(&mut self.usage, &u);
                self.spend.add(&u);
                self.context = u.input + u.cache_read + u.cache_write + u.output;
            }
            Ev::Permission(req, reply) => {
                self.asks += 1;
                // Prompts come in order with the events, so the asker is
                // the innermost running subagent, when one runs.
                let from = self.children.last().map(|c| {
                    let agent = if c.agent.is_empty() {
                        "subagent".to_string()
                    } else {
                        format!("{} subagent", c.agent)
                    };
                    format!("{agent} · {}", c.title)
                });
                // The asking tool is the latest one still running.
                let diff = if req.action != "edit" {
                    None
                } else if let Some(sub) = self.sub() {
                    sub.tools
                        .iter()
                        .rev()
                        .find(|t| t.result.is_none())
                        .and_then(|t| t.diff.clone())
                } else {
                    self.entries
                        .iter()
                        .rev()
                        .find_map(|e| match e {
                            Entry::Tool(t) if t.result.is_none() && !t.forming => {
                                Some(t.diff.clone())
                            }
                            _ => None,
                        })
                        .flatten()
                };
                self.modal = Some(Modal::Permission {
                    req,
                    reply: Some(reply),
                    feedback: String::new(),
                    diff,
                    from,
                    scroll: Default::default(),
                });
            }
            Ev::Question(prompts, reply) => {
                let form = super::form::Form::questions(&prompts);
                self.ask_form(form, FormReply::Questions(prompts.len(), reply));
            }
            Ev::Form(info, reply) => {
                let form = super::form::Form::new(&info);
                self.ask_form(form, FormReply::Form(reply));
            }
            Ev::Key(_)
            | Ev::Paste(_)
            | Ev::Resize
            | Ev::Focus(_)
            | Ev::Hangup
            | Ev::Files(_)
            | Ev::LocalModels
            | Ev::Done { .. }
            | Ev::Stopped
            | Ev::Background
            | Ev::Tab(..) => {}
        }
    }

    /// The turn is over: calls shown while they streamed that never ran
    /// (the turn stopped first) go, and a subagent it stopped is
    /// interrupted.
    pub fn drop_forming(&mut self) {
        self.entries
            .retain(|e| !matches!(e, Entry::Tool(t) if t.forming));
        if let Some(sub) = self.sub() {
            sub.status = SubStatus::Interrupted;
        }
        self.children.clear();
        self.sub_call = None;
    }

    /// Show a question form, or hold it while a picker or the help is
    /// open (it shows when that closes, as OpenCode keeps a form behind an
    /// open dialog).
    fn ask_form(&mut self, form: super::form::Form, reply: FormReply) {
        self.asks += 1;
        let m = Modal::Question {
            form: Box::new(form),
            reply: Some(reply),
        };
        match self.modal {
            Some(Modal::Picker(_) | Modal::Help(_) | Modal::DiffHelp(_)) => self.held.push(m),
            _ => self.modal = Some(m),
        }
    }

    /// The form went (answered or dismissed): the next held one shows.
    fn close_form(&mut self) {
        self.modal = None;
        self.unhold();
    }

    /// Show a held form once nothing else is open.
    pub fn unhold(&mut self) {
        if self.modal.is_none() && !self.held.is_empty() {
            self.modal = Some(self.held.remove(0));
        }
    }

    /// Whether a question form is open or held behind a dialog.
    pub fn asking(&self) -> bool {
        matches!(self.modal, Some(Modal::Question { .. })) || !self.held.is_empty()
    }

    /// Dismiss the open form (its asker sees a dismissal).
    fn dismiss_form(&mut self) {
        if let Some(Modal::Question { reply: Some(r), .. }) = self.modal.take() {
            r.send(None);
        }
        self.close_form();
    }

    /// Cancel an open prompt and any held forms (the session sees a
    /// rejection or dismissal).
    pub fn cancel_modal(&mut self) {
        for m in std::mem::take(&mut self.held) {
            if let Modal::Question { reply: Some(r), .. } = m {
                r.send(None);
            }
        }
        match self.modal.take() {
            Some(Modal::Permission { reply: Some(r), .. }) => {
                let _ = r.send(Reply::Reject(None));
            }
            Some(Modal::Question { reply: Some(r), .. }) => {
                r.send(None);
            }
            _ => {}
        }
    }

    pub fn on_paste(&mut self, s: &str) {
        if self.diff_focused() && !matches!(self.modal, Some(Modal::Picker(_))) {
            return;
        }
        match &mut self.modal {
            Some(Modal::Permission { feedback, .. }) => {
                feedback.push_str(s.lines().next().unwrap_or(""))
            }
            Some(Modal::Question { form, reply }) => {
                if let super::form::Outcome::Reply(a) = form.paste(s) {
                    if let Some(r) = reply.take() {
                        r.send(Some(a));
                    }
                    self.close_form();
                }
            }
            Some(Modal::Picker(p)) => {
                p.filter.push_str(s.lines().next().unwrap_or(""));
                p.refilter();
            }
            Some(Modal::Help(_) | Modal::DiffHelp(_) | Modal::Panel(_) | Modal::ShellOutput(_)) => {
            }
            None => {
                if let Some(files) = super::attach::pasted_files(s, &self.cwd) {
                    for path in files {
                        let label = super::attach::label(&self.attachments, &path);
                        self.input.insert_str(&format!("{label} "));
                        if !self.attachments.iter().any(|a| a.path == path) {
                            self.attachments
                                .push(super::attach::Pending { label, path });
                        }
                    }
                } else {
                    self.paste_text(s);
                }
                self.edited();
            }
        }
    }

    /// The project file list arrived (it is listed in the background).
    pub fn set_files(&mut self, files: Vec<String>) {
        self.files = files;
        if self.popup.is_some() {
            self.refresh_popup();
        }
    }

    /// Recompute the autocomplete popup after the input changed.
    fn edited(&mut self) {
        self.hist = None;
        self.refresh_popup();
    }

    fn refresh_popup(&mut self) {
        if self.dismissed.as_deref() == Some(self.input.text()) {
            self.popup = None;
            return;
        }
        self.dismissed = None;
        // Shell mode completes the command's directory arguments.
        if let Some((start, query)) =
            super::dirs::shell_argument(self.input.text(), self.input.cursor())
        {
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let home = home.unwrap_or_else(|| self.cwd.clone());
            let items: Vec<complete::Item> = super::dirs::complete(query, &self.cwd, &home)
                .into_iter()
                .take(complete::MAX_ITEMS)
                .map(|(value, abs)| complete::Item {
                    detail: super::dirs::recent_value(&abs, &home),
                    label: value,
                })
                .collect();
            let only_typed = items.len() == 1 && items[0].label == query;
            self.popup = (!items.is_empty() && !only_typed).then_some(Popup {
                kind: Kind::Dir,
                start,
                items,
                sel: 0,
            });
            return;
        }
        self.popup = complete::popup(
            self.input.text(),
            self.input.cursor(),
            &self.commands,
            &self.files,
        );
    }

    /// Accept the highlighted completion. `run` submits a slash command.
    fn accept(&mut self, run: bool) -> Option<Action> {
        let p = self.popup.take()?;
        let item = p.items.get(p.sel)?.clone();
        match p.kind {
            Kind::Slash => {
                if run {
                    self.input.set(format!("/{}", item.label));
                    return self.submit();
                }
                self.input
                    .replace_before_cursor(p.start, &format!("/{} ", item.label));
            }
            Kind::Dir => {
                self.input.replace_before_cursor(p.start, &item.label);
            }
            Kind::File => {
                // A directory stays open for drilling down.
                let sep = if item.label.ends_with('/') { "" } else { " " };
                self.input
                    .replace_before_cursor(p.start, &format!("@{}{sep}", item.label));
            }
        }
        self.edited();
        None
    }

    fn slash(name: &str) -> Option<Option<Action>> {
        Some(Some(Action::Slash {
            name: name.into(),
            args: String::new(),
        }))
    }

    #[cfg(test)]
    pub(super) fn end_navigation_for_tests(&mut self) {
        self.end_navigation();
        self.scroll = 0;
    }

    /// The command palette: the UI's commands (with their keys) and the
    /// slash commands, filtered as typed.
    pub fn open_palette(&mut self) {
        let mut items = Vec::new();
        for id in keymap::scope::PROMPT {
            if !palette_command(id) || (*id == "session.copy.id" && self.header.session.is_empty())
            {
                continue;
            }
            let Some(def) = keymap::def(id) else { continue };
            let keys = self.keymap.hint(id);
            items.push(PickItem {
                id: id.to_string(),
                label: def.desc.to_string(),
                detail: if keys.is_empty() {
                    id.to_string()
                } else {
                    format!("{keys} · {id}")
                },
                current: false,
            });
        }
        for c in &self.commands {
            items.push(PickItem {
                id: format!("/{}", c.label),
                label: format!("/{}", c.label),
                detail: c.detail.clone(),
                current: false,
            });
        }
        self.popup = None;
        self.modal = Some(Modal::Picker(Picker::new(Pick::Command, "Commands", items)));
    }

    /// Run what the palette chose.
    fn palette_run(&mut self, id: &str) -> Option<Action> {
        if let Some(name) = id.strip_prefix('/') {
            return Some(Action::Slash {
                name: name.to_string(),
                args: String::new(),
            });
        }
        let id = keymap::static_id(id)?;
        self.run(id, true).flatten()
    }

    /// Forget a message jump and the room added for it.
    fn end_navigation(&mut self) {
        self.nav = None;
        self.nav_slack = 0;
    }

    /// Jump to the next or previous message (prompt, with `user_only`).
    pub(super) fn jump(&mut self, forward: bool, user_only: bool) -> Option<Option<Action>> {
        let (w, h) = self.view_size.get();
        let h = h as usize;
        let (marks, content) = super::view::message_marks(self, w);
        let height = content + self.nav_slack;
        let scroll_top = height.saturating_sub(h + self.scroll);
        let Some(t) =
            super::navigation::find_boundary(&marks, forward, scroll_top, self.nav, user_only)
        else {
            return Some(None);
        };
        self.nav = Some(t.entry);
        self.nav_slack = super::navigation::slack(t.top, h, height, self.nav_slack);
        self.scroll = (content + self.nav_slack).saturating_sub(h + t.top);
        Some(None)
    }

    /// Scroll the transcript by `rows` (up when positive).
    fn scroll_by(&mut self, rows: i64) -> Option<Option<Action>> {
        self.scroll = if rows >= 0 {
            self.scroll.saturating_add(rows as usize)
        } else {
            self.scroll.saturating_sub(rows.unsigned_abs() as usize)
        };
        Some(None)
    }

    /// Run a prompt command: `None` when it does not apply now, so the
    /// next command bound to the key is tried (Ctrl+C clears a prompt with
    /// text and quits on an empty one; Up moves within the text, then
    /// through the history).
    fn run(&mut self, id: &str, via_leader: bool) -> Option<Option<Action>> {
        let empty = self.input.is_empty();
        match id {
            "prompt.autocomplete.prev" | "prompt.autocomplete.next" => {
                let p = self.popup.as_mut()?;
                let n = p.items.len();
                p.sel = if id.ends_with("prev") {
                    (p.sel + n - 1) % n
                } else {
                    (p.sel + 1) % n
                };
                return Some(None);
            }
            "prompt.autocomplete.complete" => {
                self.popup.as_ref()?;
                return Some(self.accept(false));
            }
            "prompt.autocomplete.select" => {
                let run = self.popup.as_ref()?.kind == Kind::Slash;
                return Some(self.accept(run));
            }
            "prompt.autocomplete.hide" => {
                self.popup.as_ref()?;
                self.popup = None;
                self.dismissed = Some(self.input.text().to_string());
                return Some(None);
            }
            "prompt.clear" if self.busy => return Some(Some(Action::Interrupt)),
            "prompt.clear" if !empty => self.input.clear(),
            "session.interrupt" if self.busy => return Some(Some(Action::Interrupt)),
            "session.interrupt" if !empty => self.input.clear(),
            "session.interrupt" => return Some(None),
            "session.background" if self.busy || via_leader => {
                return Some(Some(Action::Background))
            }
            "app.exit" if empty || via_leader => {
                self.quit = true;
                return Some(Some(Action::Quit));
            }
            "terminal.suspend" => return Some(Some(Action::Run("terminal.suspend"))),
            "terminal.title.toggle" => return Some(Some(Action::Run("terminal.title.toggle"))),
            "prompt.editor" => return Some(Some(Action::Run("prompt.editor"))),
            "model.cycle_recent" => return Some(Some(Action::Run("model.cycle_recent"))),
            "model.cycle_recent_reverse" => {
                return Some(Some(Action::Run("model.cycle_recent_reverse")))
            }
            "model.cycle_favorite" => return Some(Some(Action::Run("model.cycle_favorite"))),
            "model.cycle_favorite_reverse" => {
                return Some(Some(Action::Run("model.cycle_favorite_reverse")))
            }
            // Tab commands while tabs are on, as OpenCode enables them.
            id if id.starts_with("session.tab.") => {
                self.tabs.as_ref()?;
                return Some(Some(Action::Run(super::keymap::static_id(id)?)));
            }
            "app.clear" if self.mini.is_some() => return Some(Some(Action::Run("app.clear"))),
            "agent.cycle" => return Some(Some(Action::CycleAgent(1))),
            "agent.cycle.reverse" => return Some(Some(Action::CycleAgent(-1))),
            "agent.list" => return Self::slash("agent"),
            "model.list" => return Self::slash("model"),
            "variant.cycle" => return Self::slash("variant"),
            "session.list" => return Self::slash("sessions"),
            "session.new" => return Self::slash("new"),
            "session.compact" => return Self::slash("compact"),
            "session.undo" => return Self::slash("undo"),
            "session.redo" => return Self::slash("redo"),
            "help.show" => return Self::slash("help"),
            "command.palette.show" => {
                self.open_palette();
                return Some(None);
            }
            "session.copy.id" if !self.header.session.is_empty() => {
                self.clipboard = Some(self.header.session.clone());
                self.toast(super::toast::Toast::new(
                    super::toast::Variant::Success,
                    "Session ID copied to clipboard!",
                ));
                return Some(None);
            }
            "diff.open" => return Self::slash("diff"),
            "pane.focus.right" => {
                let v = self.diff.as_mut()?;
                if !v.now().docked {
                    return None;
                }
                v.focused = true;
                return Some(None);
            }
            "theme.switch" => return Self::slash("themes"),
            "theme.switch_mode" => return Some(Some(Action::Run("theme.switch_mode"))),
            "theme.mode.lock" => return Some(Some(Action::Run("theme.mode.lock"))),
            "session.toggle.thinking" => {
                self.show_reasoning = !self.show_reasoning;
                return Some(None);
            }
            "session.toggle.tool_output" => {
                self.expand_tools = !self.expand_tools;
                return Some(None);
            }
            "session.toggle.exploration_grouping" => {
                self.grouping = self.grouping.toggled();
                return Some(None);
            }
            "session.verbosity.cycle" => {
                self.verbosity = self.verbosity.next();
                return Some(None);
            }
            "session.page.up" => return self.scroll_by(10),
            "session.page.down" => return self.scroll_by(-10),
            "session.half.page.up" => return self.scroll_by(5),
            "session.half.page.down" => return self.scroll_by(-5),
            "session.line.up" => return self.scroll_by(1),
            "session.line.down" => return self.scroll_by(-1),
            "session.first" => {
                self.end_navigation();
                self.scroll = usize::MAX / 2;
                return Some(None);
            }
            "session.last" => {
                self.end_navigation();
                self.scroll = 0;
                return Some(None);
            }
            "session.message.next" => return self.jump(true, false),
            "session.message.previous" => return self.jump(false, false),
            "session.message.user.next" => return self.jump(true, true),
            "session.message.user.previous" => return self.jump(false, true),
            "session.messages_last_user" => {
                self.nav = None;
                self.scroll = 0;
                self.nav_slack = 0;
                return self.jump(false, true);
            }
            "input.submit" => return Some(self.submit()),
            "input.newline" => self.input.insert('\n'),
            "input.move.left" => self.input.left(),
            "input.move.right" => self.input.right(),
            "input.move.up" | "input.move.down" => {
                let moved = if id.ends_with("up") {
                    self.input.up()
                } else {
                    self.input.down()
                };
                if !moved {
                    return None;
                }
                self.popup = None;
                return Some(None);
            }
            // At the newest entry, down goes on to the next binding (the
            // subagent panel), as OpenCode's history rejects it.
            "prompt.history.next" if self.hist.is_none() => return None,
            "prompt.history.previous" | "prompt.history.next" => {
                self.history_step(if id.ends_with("previous") { -1 } else { 1 });
                self.popup = None;
                return Some(None);
            }
            "input.line.home" | "input.visual.line.home" => self.input.home(),
            "input.line.end" | "input.visual.line.end" => self.input.end(),
            "input.buffer.home" => self.input.buffer_home(),
            "input.buffer.end" => self.input.buffer_end(),
            "input.word.forward" => self.input.word_right(),
            "input.word.backward" => self.input.word_left(),
            "input.backspace" => self.input.backspace(),
            "input.delete" if !empty => self.input.delete(),
            "input.delete.word.backward" => self.input.delete_word_back(),
            "input.delete.word.forward" => self.input.delete_word_forward(),
            "input.delete.to.line.end" => self.input.kill_to_end(),
            "input.delete.to.line.start" => self.input.kill_to_start(),
            "input.delete.line" => self.input.delete_line(),
            "input.undo" => {
                self.input.undo();
            }
            "input.redo" => {
                self.input.redo();
            }
            // Down opens the panel only when it has something to show;
            // the palette's command always does.
            "session.child.first"
                if !via_leader && self.background == 0 && self.subagents().is_empty() =>
            {
                return None
            }
            "session.child.first" => {
                self.popup = None;
                self.toggle_panel(None);
                return Some(None);
            }
            "app.toggle.paste_summary" => {
                self.paste_full = !self.paste_full;
                self.notice(if self.paste_full {
                    "Paste summary off: pastes go in whole"
                } else {
                    "Paste summary on: long pastes become a chip"
                });
            }
            _ => return None,
        }
        Some(None)
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Option<Action> {
        let a = self.key(k);
        self.unhold();
        a
    }

    fn key(&mut self, k: KeyEvent) -> Option<Action> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // A focused diff viewer keeps its keys even with a permission or
        // question waiting beside it (Enter must not answer it).
        let diff = self.diff_focused();
        let asking = matches!(
            self.modal,
            Some(Modal::Permission { .. } | Modal::Question { .. })
        );
        if self.modal.is_some() && !(diff && asking) {
            return self.modal_key(k, ctrl);
        }
        if diff {
            return self.diff_key(k);
        }
        let stroke = keymap::stroke(&k)?;
        if self
            .pending_at
            .is_some_and(|at| at.elapsed() > self.keymap.timeout)
        {
            self.pending.clear();
        }
        self.pending_at = None;
        let mut seq = std::mem::take(&mut self.pending);
        let continued = !seq.is_empty();
        seq.push(stroke);
        let ids: Vec<&'static str> = if self.popup.is_some() {
            scope::AUTOCOMPLETE
                .iter()
                .chain(scope::PROMPT)
                .copied()
                .collect()
        } else {
            scope::PROMPT.to_vec()
        };
        let found = self.keymap.lookup(&ids, &seq);
        if found.exact.is_empty() && found.prefix {
            self.pending = seq;
            self.pending_at = Some(Instant::now());
            return None;
        }
        let via_leader = continued && self.keymap.after_leader(&seq);
        let browsing = self.hist.map(|_| self.input.text().to_string());
        let before = self.input.text().to_string();
        for id in found.exact {
            if let Some(action) = self.run(id, via_leader) {
                let edit = id.starts_with("input.")
                    && !matches!(id, "input.submit" | "input.move.up" | "input.move.down");
                let other = !id.starts_with("prompt.history.")
                    && !id.starts_with("prompt.autocomplete.")
                    && id != "input.submit"
                    && self.input.text() != before;
                if edit || other {
                    self.after_edit(browsing);
                }
                return action;
            }
        }
        if continued {
            // A sequence that went nowhere: after the leader the key is
            // dropped; otherwise the last key counts on its own.
            if self.keymap.after_leader(&seq) {
                return None;
            }
            return self.key(k);
        }
        // Unbound keys type their character.
        match k.code {
            KeyCode::Char(c)
                if !k.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                ) =>
            {
                self.input.insert(c);
                self.after_edit(browsing);
            }
            _ => {}
        }
        None
    }

    /// The input changed (or only its cursor moved): moving keeps the place
    /// in the history, editing leaves it.
    fn after_edit(&mut self, browsing: Option<String>) {
        if browsing.as_deref() == Some(self.input.text()) {
            self.refresh_popup();
        } else {
            self.edited();
        }
    }

    /// Whether the leader (or another binding's first keys) is waiting for
    /// the next key, and which.
    pub fn pending(&self) -> &[Stroke] {
        &self.pending
    }

    fn history_step(&mut self, dir: i32) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.hist, dir) {
            (None, -1) => {
                self.draft = self.input.text().to_string();
                Some(self.history.len() - 1)
            }
            (None, _) => return,
            (Some(i), -1) => Some(i.saturating_sub(1)),
            (Some(i), _) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), _) => None,
        };
        self.hist = next;
        match next {
            Some(i) => {
                self.input.set(self.history[i].clone());
                // Its files come back with it, as OpenCode restores them.
                if let Some(files) = self.history_files.get(i).filter(|f| !f.is_empty()) {
                    for f in files {
                        if !self.attachments.iter().any(|a| a.label == f.label) {
                            self.attachments.push(f.clone());
                        }
                    }
                }
            }
            None => self.input.set(std::mem::take(&mut self.draft)),
        }
    }

    fn submit(&mut self) -> Option<Action> {
        let text = self.input.text().trim().to_string();
        self.popup = None;
        if text.is_empty() {
            return None;
        }
        let files: Vec<super::attach::Pending> = self
            .attachments
            .iter()
            .filter(|a| text.contains(&a.label))
            .cloned()
            .collect();
        let entry = super::history::entry(&text, &files);
        let last = self.history.len().checked_sub(1);
        let repeat = last.is_some_and(|i| {
            self.history[i] == text
                && self
                    .history_files
                    .get(i)
                    .map_or(files.is_empty(), |f| *f == files)
        });
        if !repeat {
            self.history.push(text.clone());
            self.history_files
                .resize(self.history.len() - 1, Vec::new());
            self.history_files.push(files);
            self.history_new.push(entry);
        }
        self.hist = None;
        self.input.set("");
        self.scroll = 0;
        self.end_navigation();
        if text == ":q" || text == ":Q" {
            // OpenCode's `isExitCommand` takes Vim's `:q` too.
            return Some(Action::Slash {
                name: "quit".into(),
                args: String::new(),
            });
        }
        if let Some((name, args)) = codesmith_core::command::parse_invocation(&text) {
            return Some(Action::Slash { name, args });
        }
        if self.busy {
            self.notice("A turn is still running. Press Esc to interrupt it first.");
            self.input.set(text);
            return None;
        }
        let sent = self.expand_pasted(&text);
        self.pasted.clear();
        Some(Action::Submit(sent))
    }

    /// Bring a prompt back (one a fork was made before) after what is
    /// typed, a blank line between, as OpenCode's `appendPrompt`; the
    /// cursor goes to the end, and it can be undone.
    pub fn restore_prompt(&mut self, text: &str) {
        let now = self.input.text();
        let joined = if now.is_empty() {
            text.to_string()
        } else {
            format!("{now}\n\n{text}")
        };
        self.input.replace_all(joined);
        self.edited();
    }

    /// A `!command` could not run: say so in a toast and bring the
    /// prompt back to fix, as OpenCode restores the composer.
    pub fn shell_failed(&mut self, command: &str, error: &str) {
        let mut t = super::toast::Toast::new(super::toast::Variant::Error, error);
        t.title = Some("Failed to run shell command".into());
        self.toast(t);
        if self.input.is_empty() {
            self.input.set(format!("!{command}"));
        }
    }

    /// What the prompt captures from the keymap and how it is labelled,
    /// as OpenCode's prompt traits: in shell mode (a `!command`) nothing
    /// and `SHELL`; otherwise Tab, and the list's keys while it shows.
    pub fn prompt_traits(&self) -> PromptTraits {
        prompt_traits(self.input.text().starts_with('!'), self.popup.is_some())
    }

    /// Show a toast (or queue it behind the one shown).
    pub fn toast(&mut self, t: super::toast::Toast) {
        // Mini mode draws no overlay: a toast is a notice there.
        if self.mini.is_some() {
            let text = match &t.title {
                Some(title) => format!("{title}: {}", t.message),
                None => t.message,
            };
            self.notice(text);
            return;
        }
        self.toasts.show(t, Instant::now());
    }

    /// A toast about `session`: as it is for the shown session's family
    /// (itself, its parent and its subagents); for another session it is
    /// titled with that session's title, as OpenCode's plugin toasts are.
    pub fn toast_for(&mut self, session: &str, title: Option<String>, mut t: super::toast::Toast) {
        let family = session == self.header.session
            || self.parent.as_deref() == Some(session)
            || self.subagents().iter().any(|s| s.id == session);
        if !family {
            if let Some(title) = title {
                t.title = Some(match t.title.take() {
                    Some(what) => format!("{title} · {what}"),
                    None => title,
                });
            }
        }
        self.toast(t);
    }

    /// The session's subagents, from its transcript: those the session
    /// ran and, under each, the ones it ran.
    pub fn subagents(&self) -> Vec<super::panel::Subagent> {
        fn add(t: &Tool, parent: Option<&str>, out: &mut Vec<super::panel::Subagent>) {
            let Some(s) = &t.sub else { return };
            if out.iter().any(|o| o.id == s.session) {
                return;
            }
            out.push(super::panel::Subagent {
                id: s.session.clone(),
                parent: parent.map(str::to_string),
                agent: s.agent.clone(),
                title: s.title.clone(),
                running: s.status == SubStatus::Running && t.result.is_none(),
            });
            for n in &s.tools {
                add(n, Some(&s.session), out);
            }
        }
        let mut out = Vec::new();
        for e in &self.entries {
            if let Entry::Tool(t) = e {
                add(t, None, &mut out);
            }
        }
        out
    }

    /// Open the subagent and shell panel, or close it (OpenCode's
    /// `session.child.first`).
    pub fn toggle_panel(&mut self, tab: Option<super::panel::Tab>) {
        if matches!(self.modal, Some(Modal::Panel(_))) && tab.is_none() {
            self.modal = None;
            return;
        }
        if self.modal.is_none() || matches!(self.modal, Some(Modal::Panel(_))) {
            self.modal = Some(Modal::Panel(super::panel::Panel {
                tab: tab.unwrap_or_default(),
                active: !self.panel_inactive,
                ..Default::default()
            }));
        }
    }

    fn panel_key(&mut self, k: &KeyEvent) -> Option<Action> {
        use super::panel::{subagent_rows, ShellView, Tab};
        let st = keymap::stroke(k);
        let km = self.keymap.clone();
        let hit = |id: &'static str| {
            st.as_ref()
                .is_some_and(|s| !km.lookup(&[id], std::slice::from_ref(s)).exact.is_empty())
        };
        let subs = self.subagents();
        let Some(Modal::Panel(p)) = self.modal.as_mut() else {
            return None;
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match p.tab {
            Tab::Subagents => {
                let rows = subagent_rows(&subs, p.active);
                if hit("composer.subagent.up") {
                    if p.sel == 0 {
                        self.modal = None;
                    } else {
                        p.sel -= 1;
                    }
                    return None;
                }
                if hit("composer.subagent.down") {
                    if !rows.is_empty() {
                        p.sel = (p.sel + 1) % rows.len();
                    }
                    return None;
                }
                if hit("composer.subagent.select") {
                    let id = rows.get(p.sel).map(|(s, _)| s.id.clone())?;
                    self.modal = None;
                    return Some(Action::Picked(Pick::Session, id));
                }
                if hit("composer.subagent.interrupt") {
                    // A subagent runs inside the turn: stopping it stops the turn.
                    let running = rows.get(p.sel).is_some_and(|(s, _)| s.running);
                    return (running && self.busy).then_some(Action::Interrupt);
                }
                if ctrl && k.code == KeyCode::Char('a') {
                    p.active = !p.active;
                    p.sel = 0;
                    self.panel_inactive = !p.active;
                    return None;
                }
            }
            Tab::Shell => {
                let n = self.shells.len();
                if hit("composer.shell.up") {
                    if p.sel == 0 {
                        self.modal = None;
                    } else {
                        p.sel -= 1;
                    }
                    return None;
                }
                if hit("composer.shell.down") {
                    if n > 0 {
                        p.sel = (p.sel + 1) % n;
                    }
                    return None;
                }
                if hit("composer.shell.select") {
                    let shell = self.shells.get(p.sel)?.clone();
                    let back = Some(p.clone());
                    let mut v = ShellView::new(&shell, back);
                    v.refresh();
                    self.modal = Some(Modal::ShellOutput(Box::new(v)));
                    return None;
                }
                if hit("composer.shell.kill") {
                    return self
                        .shells
                        .get(p.sel)
                        .map(|s| Action::KillShell(s.id.clone()));
                }
            }
        }
        match k.code {
            KeyCode::Left | KeyCode::Right => {
                p.tab = match p.tab {
                    Tab::Subagents => Tab::Shell,
                    Tab::Shell => Tab::Subagents,
                };
                p.sel = 0;
            }
            KeyCode::Esc => self.modal = None,
            _ => {}
        }
        None
    }

    /// `text` with each paste chip still whole in it replaced by what was
    /// pasted (the first chip with a label by the first paste with it).
    pub fn expand_pasted(&self, text: &str) -> String {
        let mut out = text.to_string();
        let mut from = 0;
        for (label, pasted) in &self.pasted {
            if let Some(i) = out[from..].find(label.as_str()) {
                let at = from + i;
                out.replace_range(at..at + label.len(), pasted);
                from = at + pasted.len();
            }
        }
        out
    }

    /// Paste `text` into the prompt: a long paste (three lines or more,
    /// or over 150 characters) becomes a `[Pasted ~N lines]` chip, as
    /// OpenCode's paste summary, unless `tui.prompt.paste` is `full`.
    /// Pasting the same text again right after its chip expands it.
    fn paste_text(&mut self, raw: &str) {
        let text = strip_escapes(raw).replace("\r\n", "\n").replace('\r', "\n");
        let content = text.trim();
        let lines = content.matches('\n').count() + 1;
        if self.paste_full || (lines < 3 && content.chars().count() <= 150) {
            self.input.insert_str(&text);
            return;
        }
        let before = &self.input.text()[..self.input.cursor()];
        let again = self.pasted.iter().rev().find(|(label, p)| {
            p == content
                && (before.ends_with(label.as_str()) || before.ends_with(&format!("{label} ")))
        });
        if let Some((label, p)) = again.cloned() {
            let at = self.input.cursor();
            if before.ends_with(label.as_str()) {
                self.input.replace_before_cursor(at - label.len(), &p);
            } else {
                self.input
                    .replace_before_cursor(at - label.len() - 1, &format!("{p} "));
            }
            return;
        }
        let label = format!("[Pasted ~{lines} lines]");
        self.input.insert_str(&format!("{label} "));
        self.pasted.push((label, content.to_string()));
    }

    fn modal_key(&mut self, k: KeyEvent, ctrl: bool) -> Option<Action> {
        if ctrl && k.code == KeyCode::Char('c') {
            // Ctrl+C clears a picker's filter before it closes the picker.
            if let Some(Modal::Picker(p)) = self.modal.as_mut() {
                if !p.filter.is_empty() {
                    p.filter.clear();
                    p.refilter();
                    return self.preview(None);
                }
                let kind = p.kind;
                self.modal = None;
                return Some(Action::Cancelled(kind));
            }
            // A form clears its answer editor first.
            if let Some(Modal::Question { form, .. }) = self.modal.as_mut() {
                let km = self.keymap.clone();
                if form.key(&k, &km) != super::form::Outcome::Cancel {
                    return None;
                }
                self.dismiss_form();
                return self.busy.then_some(Action::Interrupt);
            }
            let was_prompt = matches!(
                self.modal,
                Some(Modal::Permission { .. } | Modal::Question { .. })
            );
            self.cancel_modal();
            return if self.busy && was_prompt {
                Some(Action::Interrupt)
            } else {
                None
            };
        }
        let st = keymap::stroke(&k);
        let km = self.keymap.clone();
        let hits = |ids: &[&'static str]| -> Vec<&'static str> {
            st.as_ref()
                .map(|s| km.lookup(ids, std::slice::from_ref(s)).exact)
                .unwrap_or_default()
        };
        let typed = match k.code {
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => Some(c),
            _ => None,
        };
        match self.modal.as_mut().unwrap() {
            Modal::Permission {
                feedback,
                reply,
                scroll,
                ..
            } => {
                let bound = if feedback.is_empty() {
                    hits(scope::PERMISSION).first().copied()
                } else {
                    None
                };
                let answer = match (bound, k.code) {
                    (Some("permission.allow.once"), _) => Some(Reply::Once),
                    (Some("permission.allow.always"), _) => Some(Reply::Always),
                    (Some(_), _) => Some(Reply::Reject(None)),
                    (None, KeyCode::Esc) => Some(Reply::Reject(None)),
                    // Allowing takes a deliberate y or a: a stray Enter rejects.
                    (None, KeyCode::Enter) if feedback.trim().is_empty() => {
                        Some(Reply::Reject(None))
                    }
                    (None, KeyCode::Enter) => {
                        Some(Reply::Reject(Some(feedback.trim().to_string())))
                    }
                    (None, KeyCode::Up) => {
                        scroll.set(scroll.get().saturating_sub(1));
                        None
                    }
                    (None, KeyCode::Down) => {
                        scroll.set(scroll.get() + 1);
                        None
                    }
                    (None, KeyCode::PageUp) => {
                        scroll.set(scroll.get().saturating_sub(10));
                        None
                    }
                    (None, KeyCode::PageDown) => {
                        scroll.set(scroll.get() + 10);
                        None
                    }
                    (None, KeyCode::Backspace) => {
                        feedback.pop();
                        None
                    }
                    (None, _) => {
                        if let Some(c) = typed {
                            feedback.push(c);
                        }
                        None
                    }
                };
                if let Some(a) = answer {
                    if let Some(r) = reply.take() {
                        let _ = r.send(a);
                    }
                    self.modal = None;
                }
            }
            Modal::Question { form, reply } => {
                use super::form::Outcome;
                match form.key(&k, &km) {
                    Outcome::Nothing => {}
                    Outcome::Reply(a) => {
                        if let Some(r) = reply.take() {
                            r.send(Some(a));
                        }
                        self.close_form();
                    }
                    Outcome::Cancel => self.dismiss_form(),
                    Outcome::Copy(url) => {
                        self.clipboard = Some(url);
                        self.toast(super::toast::Toast::new(
                            super::toast::Variant::Info,
                            "Copied URL to clipboard",
                        ));
                    }
                    Outcome::Open(url) => return Some(Action::OpenUrl(url)),
                }
            }
            Modal::Picker(p) => {
                let before = p.selected().map(|i| i.id.clone());
                let last = p.shown.len().saturating_sub(1);
                match hits(scope::PICKER).first().copied() {
                    Some("model.dialog.favorite") if p.kind == Pick::Model => {
                        return p.selected().map(|i| Action::Favorite(i.id.clone()));
                    }
                    Some("model.dialog.favorite") => {}
                    Some("dialog.select.prev") => p.sel = p.sel.saturating_sub(1),
                    Some("dialog.select.next") => p.sel = (p.sel + 1).min(last),
                    Some("dialog.select.page_up") => p.sel = p.sel.saturating_sub(10),
                    Some("dialog.select.page_down") => p.sel = (p.sel + 10).min(last),
                    Some("dialog.select.home") => p.sel = 0,
                    Some("dialog.select.end") => p.sel = last,
                    Some(_) if p.kind == Pick::Command => {
                        let id = p.selected().map(|i| i.id.clone())?;
                        self.modal = None;
                        return self.palette_run(&id);
                    }
                    Some(_) => {
                        let picked = p.selected().map(|i| (p.kind, i.id.clone()));
                        if picked.is_some() {
                            self.modal = None;
                        }
                        return picked.map(|(k, id)| Action::Picked(k, id));
                    }
                    None => match k.code {
                        KeyCode::Esc => {
                            let kind = p.kind;
                            self.modal = None;
                            return Some(Action::Cancelled(kind));
                        }
                        KeyCode::Backspace if p.filterable => {
                            p.filter.pop();
                            p.refilter();
                        }
                        _ => {
                            if let Some(c) = typed.filter(|_| p.filterable) {
                                p.filter.push(c);
                                p.refilter();
                            }
                        }
                    },
                }
                return self.preview(before);
            }
            Modal::Panel(_) => return self.panel_key(&k),
            Modal::ShellOutput(v) => {
                match (hits(&["dialog.select.page_up"]).is_empty(), k.code, typed) {
                    (_, KeyCode::Up, _) => v.scroll(-1),
                    (_, KeyCode::Down, _) => v.scroll(1),
                    (_, KeyCode::PageUp, _) => {
                        v.scroll(-(v.shape.get().1.max(1) as isize));
                    }
                    (_, KeyCode::PageDown, _) => v.scroll(v.shape.get().1.max(1) as isize),
                    (_, KeyCode::Home, _) => v.top = Some(0),
                    (_, KeyCode::End, _) => v.top = None,
                    (_, _, Some('c')) => {
                        v.copied = Some("command");
                        self.clipboard = Some(v.command.clone());
                    }
                    (_, _, Some('o')) if !v.text.is_empty() => {
                        v.copied = Some("output");
                        self.clipboard = Some(v.text.clone());
                    }
                    (_, KeyCode::Esc, _) => {
                        let back = v.back.take();
                        self.modal = back.map(Modal::Panel);
                    }
                    _ => {}
                }
            }
            Modal::Help(scroll) | Modal::DiffHelp(scroll) => match k.code {
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll += 1,
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::PageDown => *scroll += 10,
                _ => self.modal = None,
            },
        }
        None
    }

    /// The theme picker previews the theme it is on, when that changed.
    fn preview(&self, before: Option<String>) -> Option<Action> {
        let Some(Modal::Picker(p)) = &self.modal else {
            return None;
        };
        let now = p.selected().map(|i| i.id.clone());
        (p.kind == Pick::Theme && now != before)
            .then(|| now.map(|id| Action::Preview(Pick::Theme, id)))
            .flatten()
    }
}

/// The diff viewer's side of the app: opening and closing it, its keys,
/// its pickers and the answers to its loads.
impl App {
    /// Whether keys go to the diff viewer: it is open full screen, or
    /// docked and focused.
    pub fn diff_focused(&self) -> bool {
        self.diff
            .as_ref()
            .is_some_and(|v| v.focused || !v.now().docked)
    }

    /// `/diff`: open the viewer on `dir`'s changes (focused), or close it
    /// when it is open.
    pub fn toggle_diff(&mut self, dir: std::path::PathBuf) {
        if self.diff.take().is_some() {
            return;
        }
        let branch = super::diff_source::current_branch(&dir);
        let source = self
            .diff_memory
            .source
            .or(self.diff_source)
            .unwrap_or(super::diff_source::Source::Branch);
        let mut v = super::diff_view::Viewer::new(dir, branch.as_deref(), source, self.diff_prefs);
        v.reload(&self.diff_memory);
        self.diff = Some(v);
    }

    /// The loads the viewer asked for, for the runner to start.
    pub fn take_diff_requests(&mut self) -> Vec<super::diff_view::Request> {
        self.diff
            .as_mut()
            .map(|v| std::mem::take(&mut v.requests))
            .unwrap_or_default()
    }

    /// A load finished.
    pub fn diff_answer(&mut self, r: super::diff_view::Response) {
        let Some(v) = self.diff.as_mut() else { return };
        let Some(branches) = v.answer(r) else { return };
        let Some(Modal::Picker(p)) = self.modal.as_mut() else {
            return;
        };
        if p.kind != Pick::DiffBase {
            return;
        }
        let current = v
            .source_base(&self.diff_memory)
            .map(|b| short_ref(&b.reference).to_string());
        match branches {
            Ok(list) => {
                p.empty = Some("No branches found".into());
                p.items = list
                    .into_iter()
                    .map(|b| PickItem {
                        current: current.as_deref() == Some(b.as_str()),
                        label: b.clone(),
                        detail: String::new(),
                        id: b,
                    })
                    .collect();
                p.refilter();
                p.sel = p
                    .shown
                    .iter()
                    .position(|&i| p.items[i].current)
                    .unwrap_or(0);
            }
            Err(_) => {
                p.empty = Some("Could not load branches. Reopen the picker to try again.".into());
            }
        }
    }

    /// The session finished a turn: the last-turn source shows the new one.
    pub fn diff_turn_done(&mut self) {
        if let Some(v) = self.diff.as_mut() {
            if v.source == super::diff_source::Source::Turn {
                v.reload(&self.diff_memory);
            }
        }
    }

    /// The source chooser: the sources, then the base.
    fn open_diff_source(&mut self) {
        let Some(v) = self.diff.as_ref() else { return };
        let mut items: Vec<PickItem> = super::diff_source::Source::ALL
            .iter()
            .map(|s| PickItem {
                id: s.key().into(),
                label: format!("{:<11}", s.label()),
                detail: s.description().into(),
                current: *s == v.source,
            })
            .collect();
        items.push(PickItem {
            id: "base".into(),
            label: format!("{:<11}", "Base"),
            detail: v
                .source_base(&self.diff_memory)
                .map_or("Choose…".into(), |b| b.name),
            current: false,
        });
        let mut p = Picker::new(Pick::DiffSource, "Diff source", items);
        p.filterable = false;
        self.modal = Some(Modal::Picker(p));
    }

    /// A choice in one of the viewer's pickers.
    pub fn diff_picked(&mut self, kind: Pick, id: &str) {
        let Some(v) = self.diff.as_mut() else { return };
        match kind {
            Pick::DiffSource if id == "base" => {
                v.load_branches();
                let mut p = Picker::new(Pick::DiffBase, "Base branch", vec![]);
                p.empty = Some("Loading branches…".into());
                p.footer = Some("Remembered until the TUI exits".into());
                self.modal = Some(Modal::Picker(p));
            }
            Pick::DiffSource => {
                if let Some(s) = super::diff_source::Source::parse(id) {
                    v.switch_source(s, &mut self.diff_memory);
                }
            }
            Pick::DiffBase => v.choose_base(id, &mut self.diff_memory),
            _ => {}
        }
    }

    /// A key for the diff viewer: its commands (`diff.*`), the pane focus
    /// keys, and `app.exit`, which closes it. Other keys do nothing.
    fn diff_key(&mut self, k: KeyEvent) -> Option<Action> {
        let stroke = keymap::stroke(&k)?;
        if self
            .pending_at
            .is_some_and(|at| at.elapsed() > self.keymap.timeout)
        {
            self.pending.clear();
        }
        self.pending_at = None;
        let mut seq = std::mem::take(&mut self.pending);
        let continued = !seq.is_empty();
        seq.push(stroke);
        let found = self.keymap.lookup(scope::DIFF, &seq);
        if found.exact.is_empty() && found.prefix {
            self.pending = seq;
            self.pending_at = Some(Instant::now());
            return None;
        }
        let Some(id) = found.exact.first().copied() else {
            if continued && !self.keymap.after_leader(&seq) {
                return self.diff_key(k);
            }
            return None;
        };
        let v = self.diff.as_mut()?;
        let (out, prefs) = v.command(id);
        if prefs {
            self.diff_prefs = v.prefs();
            self.diff_prefs_changed = true;
        }
        match out {
            super::diff_view::Outcome::Close => self.diff = None,
            super::diff_view::Outcome::Source => self.open_diff_source(),
            super::diff_view::Outcome::Help => self.modal = Some(Modal::DiffHelp(0)),
            super::diff_view::Outcome::FocusPrompt | super::diff_view::Outcome::None => {}
        }
        None
    }
}

/// A branch ref as the base picker lists it.
pub fn short_ref(r: &str) -> &str {
    r.strip_prefix("refs/heads/")
        .or_else(|| r.strip_prefix("refs/remotes/"))
        .unwrap_or(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }
    fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            modifiers,
            ..key(code)
        }
    }
    fn ctrl(c: char) -> KeyEvent {
        with(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn typ(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_submitting() {
        let mut a = App::new();
        typ(&mut a, "hello");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Submit("hello".into()))
        );
        assert!(a.input.is_empty());
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            None,
            "empty input does nothing"
        );
    }

    #[test]
    fn newlines_and_editing() {
        let mut a = App::new();
        typ(&mut a, "ab");
        a.on_key(ctrl('j'));
        typ(&mut a, "c");
        a.on_key(key(KeyCode::Backspace));
        assert_eq!(a.input.text(), "ab\n");
        // Ctrl+U deletes to the start of the line (OpenCode's
        // `input.delete.to.line.start`), then the line break before it.
        a.on_key(ctrl('u'));
        assert_eq!(a.input.text(), "ab");
        a.on_key(ctrl('u'));
        assert!(a.input.is_empty());
    }

    #[test]
    fn editing_in_the_middle() {
        let mut a = App::new();
        typ(&mut a, "fix the bug");
        a.on_key(with(KeyCode::Left, KeyModifiers::ALT));
        a.on_key(with(KeyCode::Left, KeyModifiers::CONTROL));
        typ(&mut a, "nasty ");
        assert_eq!(a.input.text(), "fix nasty the bug");
        a.on_key(ctrl('a'));
        a.on_key(key(KeyCode::Delete));
        typ(&mut a, "F");
        a.on_key(ctrl('e'));
        a.on_key(ctrl('w'));
        assert_eq!(a.input.text(), "Fix nasty the ");
        a.on_key(ctrl('a'));
        a.on_key(with(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(a.input.text(), "\nFix nasty the ");
    }

    #[test]
    fn up_moves_within_text_before_history() {
        let mut a = App::new();
        typ(&mut a, "old");
        a.on_key(key(KeyCode::Enter));
        typ(&mut a, "line1");
        a.on_key(ctrl('j'));
        typ(&mut a, "l2");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input.cursor(), 2, "moved up within the text");
        assert_eq!(a.input.text(), "line1\nl2");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input.text(), "old", "then history");
    }

    #[test]
    fn slash_commands() {
        let mut a = App::new();
        typ(&mut a, "/review HEAD~1");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Slash {
                name: "review".into(),
                args: "HEAD~1".into()
            })
        );
    }

    #[test]
    fn leader_b_moves_running_commands_to_the_background() {
        let mut a = App::new();
        a.busy = true;
        a.on_key(ctrl('x'));
        assert_eq!(a.on_key(key(KeyCode::Char('b'))), Some(Action::Background));
        assert!(a.pending().is_empty());
    }

    #[test]
    fn slash_autocomplete() {
        let mut a = App::new();
        a.set_commands(&[("review".into(), "review changes".into())]);
        typ(&mut a, "/ses");
        let p = a.popup.as_ref().expect("popup opens");
        assert_eq!(p.items[0].label, "sessions");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Slash {
                name: "sessions".into(),
                args: String::new()
            }),
            "Enter runs the highlighted command"
        );
        typ(&mut a, "/revi");
        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.input.text(), "/review ", "Tab completes for arguments");
        assert!(a.popup.is_none());
        a.on_key(ctrl('u'));
        typ(&mut a, "/zzzz");
        assert!(a.popup.is_none(), "no match, no popup");
    }

    #[test]
    fn popup_navigation_and_dismissal() {
        let mut a = App::new();
        typ(&mut a, "/");
        let n = a.popup.as_ref().unwrap().items.len();
        assert!(n > 3);
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.popup.as_ref().unwrap().sel, 2);
        a.on_key(key(KeyCode::Up));
        a.on_key(key(KeyCode::Up));
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.popup.as_ref().unwrap().sel, n - 1, "wraps around");
        a.on_key(key(KeyCode::Esc));
        assert!(a.popup.is_none());
        assert_eq!(a.input.text(), "/", "Esc only closes the popup");
        typ(&mut a, "n");
        assert!(a.popup.is_some(), "typing reopens it");
    }

    #[test]
    fn file_mentions_complete() {
        let mut a = App::new();
        a.files = vec!["src/".into(), "src/main.rs".into(), "README.md".into()];
        typ(&mut a, "look at @mai");
        assert_eq!(a.popup.as_ref().unwrap().items[0].label, "src/main.rs");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.input.text(), "look at @src/main.rs ");
        typ(&mut a, "and @sr");
        a.on_key(key(KeyCode::Tab));
        assert_eq!(
            a.input.text(),
            "look at @src/main.rs and @src/",
            "directories stay open"
        );
        assert!(a.popup.is_some());
    }

    #[test]
    fn tab_cycles_agents_and_leader_keys() {
        let mut a = App::new();
        assert_eq!(a.on_key(key(KeyCode::Tab)), Some(Action::CycleAgent(1)));
        assert_eq!(
            a.on_key(key(KeyCode::BackTab)),
            Some(Action::CycleAgent(-1))
        );
        assert_eq!(a.on_key(ctrl('x')), None);
        assert_eq!(
            a.on_key(key(KeyCode::Char('m'))),
            Some(Action::Slash {
                name: "model".into(),
                args: String::new()
            })
        );
        typ(&mut a, "m");
        assert_eq!(a.input.text(), "m", "the leader is used once");
    }

    #[test]
    fn history() {
        let mut a = App::new();
        for m in ["one", "two"] {
            typ(&mut a, m);
            a.on_key(key(KeyCode::Enter));
        }
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input.text(), "two");
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.input.text(), "one");
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.input.text(), "two");
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.input.text(), "");
    }

    #[test]
    fn busy_keys() {
        let mut a = App::new();
        a.busy = true;
        assert_eq!(a.on_key(key(KeyCode::Esc)), Some(Action::Interrupt));
        assert_eq!(a.on_key(ctrl('c')), Some(Action::Interrupt));
        typ(&mut a, "more");
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            None,
            "no new turn while one runs"
        );
        assert_eq!(a.input.text(), "more", "the text is kept");
    }

    #[test]
    fn quitting() {
        let mut a = App::new();
        typ(&mut a, "x");
        assert_eq!(a.on_key(ctrl('c')), None, "first Ctrl+C clears the input");
        assert_eq!(a.on_key(ctrl('c')), Some(Action::Quit));
        assert!(a.quit);
    }

    fn start(id: &str, name: &str, detail: &str) -> Ev {
        Ev::ToolStart {
            id: id.into(),
            name: name.into(),
            detail: detail.into(),
            diff: None,
        }
    }

    #[test]
    fn a_tool_call_shows_while_it_streams() {
        let mut a = App::new();
        let input = |id: &str| Ev::ToolInput {
            id: id.into(),
            name: "glob".into(),
        };
        let end = |id: &str, call: Option<&str>| Ev::ToolInputEnd {
            id: id.into(),
            call: call.map(Into::into),
        };
        let rows = |a: &App| -> Vec<(String, bool, bool)> {
            a.entries
                .iter()
                .filter_map(|e| match e {
                    Entry::Tool(t) => Some((t.id.clone(), t.forming, t.result.is_some())),
                    _ => None,
                })
                .collect()
        };
        a.on_event(input("call_a"));
        assert_eq!(rows(&a), [("call_a".into(), true, false)]);
        // The call the input formed carries a signature after its id.
        a.on_event(end("call_a", Some("call_a~SIG")));
        a.on_event(input("call_b"));
        a.on_event(end("call_b", None));
        a.on_event(input("call_c"));
        a.on_event(end("call_c", Some("call_c")));
        assert_eq!(
            rows(&a),
            [
                ("call_a~SIG".into(), true, false),
                ("call_c".into(), true, false)
            ],
            "a dropped call's row goes"
        );
        // The row becomes the running call, then its result.
        a.on_event(start("call_a~SIG", "glob", "*.rs"));
        a.on_event(Ev::ToolEnd {
            id: "call_a~SIG".into(),
            name: "glob".into(),
            output: "a.rs".into(),
            error: false,
        });
        let Some(Entry::Tool(t)) = a.entries.first() else {
            panic!("{:?}", a.entries)
        };
        assert_eq!((t.detail.as_str(), t.forming), ("*.rs", false));
        assert_eq!(rows(&a)[0], ("call_a~SIG".into(), false, true));
        // A formed call the turn never ran goes when the turn ends.
        a.drop_forming();
        assert_eq!(rows(&a), [("call_a~SIG".into(), false, true)]);
    }

    #[test]
    fn streaming_events_build_the_transcript() {
        let mut a = App::new();
        a.on_event(Ev::Text("Hel".into()));
        a.on_event(Ev::Text("lo".into()));
        a.on_event(start("c1", "read", "a.rs"));
        a.on_event(Ev::ToolEnd {
            id: "c1".into(),
            name: "read".into(),
            output: "\nRead file a.rs, lines 1-3\n1: x".into(),
            error: false,
        });
        a.on_event(Ev::Text("Done".into()));
        assert_eq!(
            a.entries,
            vec![
                Entry::Assistant("Hello".into()),
                Entry::Tool(Tool {
                    id: "c1".into(),
                    name: "read".into(),
                    detail: "a.rs".into(),
                    result: Some(("Read file a.rs, lines 1-3".into(), false)),
                    output: "\nRead file a.rs, lines 1-3\n1: x".into(),
                    ..Default::default()
                }),
                Entry::Assistant("Done".into()),
            ]
        );
    }

    #[test]
    fn todos_replace_the_previous_list() {
        let mut a = App::new();
        a.on_event(Ev::Todos(vec![]));
        a.on_event(Ev::Text("x".into()));
        a.on_event(Ev::Todos(vec![]));
        assert_eq!(
            a.entries
                .iter()
                .filter(|e| matches!(e, Entry::Todos(_)))
                .count(),
            1
        );
    }

    fn req() -> Request {
        Request {
            action: "shell".into(),
            resources: vec!["rm x".into()],
            save: vec!["rm *".into()],
            title: "$ rm x".into(),
        }
    }

    #[test]
    fn permission_answers() {
        for (keys, want) in [
            (vec![key(KeyCode::Char('y'))], Reply::Once),
            (vec![key(KeyCode::Char('a'))], Reply::Always),
            (vec![key(KeyCode::Char('n'))], Reply::Reject(None)),
            // An empty Enter rejects (it used to allow once).
            (vec![key(KeyCode::Enter)], Reply::Reject(None)),
            (vec![key(KeyCode::Esc)], Reply::Reject(None)),
            (
                vec![
                    key(KeyCode::Down),
                    key(KeyCode::Up),
                    key(KeyCode::Char('y')),
                ],
                Reply::Once,
            ),
            (
                vec![
                    key(KeyCode::Char('u')),
                    key(KeyCode::Char('s')),
                    key(KeyCode::Char('e')),
                    key(KeyCode::Enter),
                ],
                Reply::Reject(Some("use".into())),
            ),
        ] {
            let mut a = App::new();
            let (tx, mut rx) = oneshot::channel();
            a.on_event(Ev::Permission(req(), tx));
            for k in keys {
                a.on_key(k);
            }
            assert!(a.modal.is_none());
            assert_eq!(rx.try_recv().unwrap(), want);
        }
    }

    #[test]
    fn edit_permissions_carry_the_running_tools_diff() {
        let mut a = App::new();
        let d = super::super::diff::diff("a\n", "b\n", 1);
        a.on_event(Ev::ToolStart {
            id: "c".into(),
            name: "edit".into(),
            detail: "x.rs".into(),
            diff: Some(d.clone()),
        });
        let (tx, _rx) = oneshot::channel();
        a.on_event(Ev::Permission(
            Request {
                action: "edit".into(),
                resources: vec!["x.rs".into()],
                save: vec![],
                title: "Edit x.rs".into(),
            },
            tx,
        ));
        assert!(matches!(&a.modal, Some(Modal::Permission { diff: Some(x), .. }) if *x == d));
    }

    #[test]
    fn questions_are_answered_in_order() {
        let p = |q: &str| codesmith_core::question::Prompt {
            question: q.into(),
            header: String::new(),
            options: vec![codesmith_core::question::Choice {
                label: "Yes".into(),
                description: String::new(),
            }],
            multiple: false,
        };
        let mut a = App::new();
        let (tx, mut rx) = oneshot::channel();
        a.on_event(Ev::Question(vec![p("A?"), p("B?")], tx));
        // A number picks a choice and moves to the next question.
        typ(&mut a, "1");
        assert!(a.modal.is_some());
        // Its custom row takes a typed answer.
        a.on_key(key(KeyCode::Down));
        typ(&mut a, "maybe");
        a.on_key(key(KeyCode::Enter));
        assert!(a.modal.is_some());
        a.on_key(key(KeyCode::Enter));
        assert!(a.modal.is_none());
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(vec![vec!["Yes".to_string()], vec!["maybe".to_string()]])
        );
    }

    #[test]
    fn cancelling_a_prompt_rejects_it() {
        let mut a = App::new();
        let (tx, mut rx) = oneshot::channel();
        a.on_event(Ev::Permission(req(), tx));
        a.cancel_modal();
        assert_eq!(rx.try_recv().unwrap(), Reply::Reject(None));
    }

    fn items(labels: &[&str]) -> Vec<PickItem> {
        labels
            .iter()
            .map(|l| PickItem {
                id: format!("id-{l}"),
                label: l.to_string(),
                detail: String::new(),
                current: *l == "beta",
            })
            .collect()
    }

    #[test]
    fn a_refreshed_picker_keeps_its_filter_and_selection() {
        let item = |id: &str| PickItem {
            id: id.into(),
            label: id.into(),
            detail: String::new(),
            current: false,
        };
        let mut p = Picker::new(Pick::Model, "Models", vec![item("a/one"), item("a/two")]);
        p.filter = "a/".into();
        p.refilter();
        p.sel = 1;
        p.replace_items(vec![
            item("a/new"),
            item("a/one"),
            item("a/two"),
            item("b/x"),
        ]);
        assert_eq!(p.filter, "a/");
        assert_eq!(p.shown.len(), 3, "b/x is filtered out");
        assert_eq!(p.selected().unwrap().id, "a/two");
        p.replace_items(vec![item("a/one")]);
        assert_eq!(
            p.selected().unwrap().id,
            "a/one",
            "a gone item moves the selection"
        );
    }

    #[test]
    fn pickers_filter_and_choose() {
        let mut a = App::new();
        a.modal = Some(Modal::Picker(Picker::new(
            Pick::Model,
            "Models",
            items(&["alpha", "beta", "gamma"]),
        )));
        let Some(Modal::Picker(p)) = &a.modal else {
            unreachable!()
        };
        assert_eq!(p.sel, 1, "starts on the current item");
        typ(&mut a, "gam");
        let Some(Modal::Picker(p)) = &a.modal else {
            unreachable!()
        };
        assert_eq!(p.shown.len(), 1);
        assert_eq!(
            a.on_key(key(KeyCode::Enter)),
            Some(Action::Picked(Pick::Model, "id-gamma".into()))
        );
        assert!(a.modal.is_none());
        a.modal = Some(Modal::Picker(Picker::new(
            Pick::Session,
            "Sessions",
            items(&["alpha"]),
        )));
        typ(&mut a, "zz");
        assert_eq!(a.on_key(key(KeyCode::Enter)), None, "nothing matches");
        a.on_key(key(KeyCode::Backspace));
        a.on_key(key(KeyCode::Backspace));
        a.on_key(key(KeyCode::Esc));
        assert!(a.modal.is_none());
    }

    #[test]
    fn context_and_cost() {
        let mut a = App::new();
        assert_eq!(a.context_percent(), None);
        a.header.context_limit = Some(1000);
        a.on_event(Ev::StepEnd(Usage {
            input: 200,
            output: 50,
            cache_read: 0,
            cache_write: 0,
            ..Default::default()
        }));
        assert_eq!(a.context_percent(), Some(25));
        assert_eq!(a.cost(), None);
        a.header.cost = Some(Cost {
            input: 1.0,
            output: 2.0,
            cache_read: 0.0,
            cache_write: 0.0,
        });
        let c = a.cost().unwrap();
        assert!((c - 0.0003).abs() < 1e-9, "{c}");
        // A step the provider billed counts at its billed cost.
        a.on_event(Ev::StepEnd(Usage {
            input: 1_000_000,
            cost: Some(0.01),
            ..Default::default()
        }));
        let c = a.cost().unwrap();
        assert!((c - 0.0103).abs() < 1e-9, "{c}");
        a.header.cost = None;
        assert_eq!(a.cost(), Some(0.01));
    }

    #[test]
    fn scrolling_and_toggles() {
        let mut a = App::new();
        a.on_key(key(KeyCode::PageUp));
        a.on_key(key(KeyCode::PageUp));
        assert_eq!(a.scroll, 20);
        a.on_key(key(KeyCode::PageDown));
        assert_eq!(a.scroll, 10);
        a.on_key(ctrl('o'));
        assert!(a.expand_tools);
        typ(&mut a, "x");
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.scroll, 0, "sending jumps to the bottom");
    }

    #[test]
    fn paste_inserts_at_the_cursor() {
        let mut a = App::new();
        typ(&mut a, "ac");
        a.on_key(key(KeyCode::Left));
        a.on_paste("b\r\nb");
        assert_eq!(a.input.text(), "ab\nbc");
    }
}

/// Rebuild the transcript from a session's stored history (when continuing).
pub fn replay(messages: &[codesmith_ai::Message]) -> Vec<Entry> {
    use codesmith_ai::{Part, Role};
    let mut out: Vec<Entry> = Vec::new();
    for m in messages {
        for p in &m.parts {
            match (&m.role, p) {
                // The engine's own notes (the compaction continue note).
                (Role::System, _) => {}
                (Role::User, Part::Text(t)) if t.starts_with("<conversation-checkpoint>") => {
                    out.push(Entry::Notice("(earlier conversation summarized)".into()));
                }
                // A command the user ran with `!`.
                (Role::User, Part::Text(t)) if codesmith_core::user_shell::parse(t).is_some() => {
                    let shown = codesmith_core::user_shell::parse(t).unwrap();
                    out.push(Entry::Tool(Tool {
                        id: format!("user-shell-{}", out.len()),
                        name: "shell".into(),
                        detail: shown.command.lines().next().unwrap_or("").to_string(),
                        result: Some((first_line(&shown.text), shown.failed)),
                        output: shown.text,
                        ..Default::default()
                    }))
                }
                (Role::User, Part::Text(t))
                    if t.starts_with("<shell ")
                        || t.starts_with("<system-reminder>")
                        || codesmith_core::runner::is_engine_message(t) => {}
                (Role::User, Part::Text(t)) => {
                    out.push(Entry::User(super::attach::typed(t).to_string()))
                }
                (Role::User, Part::Media(m)) => out.push(Entry::Notice(format!(
                    "attached {}",
                    codesmith_core::media::label(m)
                ))),
                (Role::Assistant, Part::Text(t)) => out.push(Entry::Assistant(t.to_string())),
                (Role::Assistant, Part::Reasoning(t)) => out.push(Entry::Reasoning(t.to_string())),
                (Role::Assistant, Part::Media(m)) => out.push(Entry::Media(media_label(m))),
                (
                    Role::Assistant,
                    Part::ToolCall {
                        id, name, input, ..
                    },
                ) => out.push(Entry::Tool(Tool {
                    id: id.clone(),
                    name: name.clone(),
                    detail: crate::render::describe(name, input),
                    ..Default::default()
                })),
                // A tool the provider ran: already settled.
                (Role::Assistant, Part::Hosted(h)) => {
                    let (output, failed) = match h.summary() {
                        Some(s) => (s, h.result.as_ref().is_some_and(|r| r.is_error)),
                        None => ("Tool result missing".to_string(), true),
                    };
                    out.push(Entry::Tool(Tool {
                        id: h.id.clone(),
                        name: h.name.clone(),
                        detail: crate::render::describe(&h.name, &h.input),
                        result: Some((first_line(&output), failed)),
                        output,
                        ..Default::default()
                    }))
                }
                (
                    Role::User,
                    Part::ToolResult {
                        id,
                        name,
                        content,
                        is_error,
                        media,
                    },
                ) => {
                    if let Some(Entry::Tool(t)) = out.iter_mut().rev().find(|e| {
                        matches!(e, Entry::Tool(t) if t.result.is_none()
                            && (t.id == *id || t.name == *name))
                    }) {
                        t.result = Some((first_line(content), *is_error));
                        t.output = content.clone();
                        t.media = media.iter().map(media_label).collect();
                    }
                }
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use codesmith_ai::{Message, Part, Role};
    use serde_json::json;

    #[test]
    fn replays_a_stored_conversation() {
        let msgs = vec![
            Message::user(
                "<conversation-checkpoint>\n<summary>S</summary>\n</conversation-checkpoint>",
            ),
            Message::user(format!(
                "fix @a.rs{}{{\"filePath\":\"/x/a.rs\"}}\nRead file a.rs",
                super::super::attach::MARKER
            )),
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::Text("Looking.".into()),
                    Part::ToolCall {
                        id: "c".into(),
                        name: "read".into(),
                        input: json!({"path": "a.rs"}),
                        meta: None,
                    },
                ],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "c".into(),
                    name: "read".into(),
                    content: "Read file a.rs, lines 1-2".into(),
                    is_error: false,
                    media: vec![],
                }],
            },
            Message::user("<shell id=\"shell_1\" state=\"completed\" command=\"x\">\nok\n</shell>"),
            Message::system("Continue the task from where it left off."),
            Message::user(codesmith_core::runner::CONTINUE_AFTER_OUTPUT_LIMIT),
            Message {
                role: Role::Assistant,
                parts: vec![Part::Text("Fixed.".into())],
            },
        ];
        assert_eq!(
            replay(&msgs),
            vec![
                Entry::Notice("(earlier conversation summarized)".into()),
                Entry::User("fix @a.rs".into()),
                Entry::Assistant("Looking.".into()),
                Entry::Tool(Tool {
                    id: "c".into(),
                    name: "read".into(),
                    detail: "a.rs".into(),
                    result: Some(("Read file a.rs, lines 1-2".into(), false)),
                    output: "Read file a.rs, lines 1-2".into(),
                    ..Default::default()
                }),
                Entry::Assistant("Fixed.".into()),
            ]
        );
    }
}

#[cfg(test)]
mod replay_media_tests {
    use super::*;
    use codesmith_ai::{Media, Message, Part, Role};
    use serde_json::json;

    #[test]
    fn replays_returned_and_generated_media() {
        let png = Media::base64("image/png", "A".repeat(2732)).named("cat.png");
        let msgs = vec![
            Message {
                role: Role::Assistant,
                parts: vec![Part::tool_call("c", "read", json!({"path": "cat.png"}))],
            },
            Message {
                role: Role::User,
                parts: vec![Part::ToolResult {
                    id: "c".into(),
                    name: "read".into(),
                    content: "Image read successfully".into(),
                    is_error: false,
                    media: vec![png],
                }],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    Part::text("Here it is."),
                    Part::Media(
                        Media::base64("image/png", "AAECAw==")
                            .with_meta("google", json!({"thoughtSignature": "s"})),
                    ),
                ],
            },
        ];
        let e = replay(&msgs);
        let Entry::Tool(t) = &e[0] else {
            panic!("{e:?}")
        };
        assert_eq!(t.media, ["[image: cat.png] image/png, 2 KB"]);
        assert_eq!(
            e[1..],
            [
                Entry::Assistant("Here it is.".into()),
                Entry::Media("[image] image/png, 6 B".into()),
            ]
        );
    }
}
