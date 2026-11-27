//! Key bindings as OpenCode configures them (`config/keybind.ts`): every
//! command has an id (`session.list`), a description and a default
//! binding, which `tui.keybinds` overrides. A binding is `"none"` or
//! `false` (unbound), a string of comma-separated alternatives, a key
//! object (`{"name": "x", "ctrl": true}`), a binding object (`{"key":
//! "ctrl+v"}`) or a list of them. An alternative is a sequence of keys:
//! `ctrl+x`, `shift+return`, `<leader>m` (the leader key, then `m`),
//! `gg` (`g` twice).
//!
//! OpenCode's first config format named commands differently
//! (`session_list`); [`LEGACY`] maps those names, as OpenCode migrates
//! them.
//!
//! CodeSmith implements a subset of OpenCode's commands. The others keep
//! their ids and defaults, so a shared config is accepted, but do
//! nothing; conflicts are only reported between commands CodeSmith runs.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub struct Def {
    pub id: &'static str,
    pub default: &'static str,
    pub desc: &'static str,
}

const fn d(id: &'static str, default: &'static str, desc: &'static str) -> Def {
    Def { id, default, desc }
}

pub const LEADER_DEFAULT: &str = "ctrl+x";
/// How long a key sequence (the leader's) waits for its next key.
pub const LEADER_TIMEOUT_MS: u64 = 2000;

/// OpenCode's commands and defaults (`TuiKeybind.Definitions`), then
/// CodeSmith's own. Where CodeSmith's default differs from OpenCode's it
/// is in [`CODESMITH_DEFAULTS`].
pub const DEFINITIONS: &[Def] = &[
    d(
        "leader",
        LEADER_DEFAULT,
        "Leader key for keybind combinations",
    ),
    d(
        "app.exit",
        "ctrl+c,ctrl+d,<leader>q",
        "Exit the application",
    ),
    d("app.clear", "ctrl+l", "Clear the screen in mini"),
    d("app.debug", "none", "Toggle debug panel"),
    d("app.console", "none", "Toggle console"),
    d("app.scrap", "none", "Open scrap screen"),
    d("app.toggle.animations", "none", "Toggle animations"),
    d("app.toggle.file_context", "none", "Toggle file context"),
    d("app.toggle.diffwrap", "none", "Toggle diff wrapping"),
    d("app.toggle.paste_summary", "none", "Toggle paste summary"),
    d("command.palette.show", "ctrl+p", "List available commands"),
    d("help.show", "none", "Open help dialog"),
    d("docs.open", "none", "Open documentation"),
    d("opencode.settings", "none", "Open settings"),
    d("server.pair", "none", "Pair device"),
    d("service.restart", "none", "Restart service"),
    d("location.reload", "none", "Reload configuration"),
    d("diff.open", "none", "Open diff viewer"),
    d("diff.close", "escape,q", "Close diff viewer"),
    d("diff.down", "j,down", "Move diff viewer down"),
    d("diff.up", "k,up", "Move diff viewer up"),
    d("diff.page.down", "pagedown,ctrl+f", "Page diff viewer down"),
    d("diff.page.up", "pageup,ctrl+b", "Page diff viewer up"),
    d(
        "diff.half_page.down",
        "ctrl+d",
        "Scroll diff viewer down half a page",
    ),
    d(
        "diff.half_page.up",
        "ctrl+u",
        "Scroll diff viewer up half a page",
    ),
    d("diff.first", "gg,home", "Go to the start of the diff"),
    d("diff.last", "shift+g,end", "Go to the end of the diff"),
    d(
        "diff.toggle",
        "none",
        "Deprecated: file tree is mouse-controlled",
    ),
    d(
        "diff.expand",
        "none",
        "Deprecated: file tree is mouse-controlled",
    ),
    d(
        "diff.expand_all",
        "none",
        "Deprecated: file tree is mouse-controlled",
    ),
    d(
        "diff.collapse",
        "none",
        "Deprecated: file tree is mouse-controlled",
    ),
    d(
        "diff.switch_focus",
        "none",
        "Deprecated: keyboard navigation always controls the diff",
    ),
    d("diff.next_hunk", "]", "Jump to next diff hunk"),
    d("diff.previous_hunk", "[", "Jump to previous diff hunk"),
    d("diff.next_file", "n,alt+down", "Jump to next diff file"),
    d(
        "diff.previous_file",
        "p,alt+up",
        "Jump to previous diff file",
    ),
    d("diff.toggle_file_tree", "b", "Toggle diff viewer file tree"),
    d("diff.single_patch", "s", "Toggle single patch view"),
    d("diff.switch_source", "d", "Switch diff viewer source"),
    d(
        "diff.toggle_view",
        "v",
        "Toggle diff viewer split or unified view",
    ),
    d(
        "diff.toggle_fullscreen",
        "f",
        "Toggle diff viewer full screen",
    ),
    d(
        "diff.mark_reviewed",
        "m",
        "Toggle selected diff file reviewed",
    ),
    d(
        "diff.help",
        "?,shift+?,shift+/",
        "Show more diff viewer shortcuts",
    ),
    d("prompt.editor", "<leader>e", "Open external editor"),
    d("theme.switch", "none", "List available themes"),
    d(
        "theme.switch_mode",
        "none",
        "Switch between light and dark theme mode",
    ),
    d("theme.mode.lock", "none", "Lock or unlock theme mode"),
    d("session.sidebar.toggle", "<leader>b", "Toggle sidebar"),
    d("pane.focus.left", "<leader>left", "Focus session pane"),
    d("pane.focus.right", "<leader>right", "Focus right pane"),
    d("terminal.select", "<leader>down", "Select terminal"),
    d("terminal.toggle", "<leader>t", "Toggle terminal pane"),
    d("terminal.close", "<leader>up", "Close terminal pane"),
    d(
        "session.toggle.scrollbar",
        "none",
        "Toggle session scrollbar",
    ),
    d("opencode.status", "<leader>s", "View status"),
    d("opencode.debug", "none", "View debug info"),
    d("session.export", "<leader>x", "Export session"),
    d("session.copy", "none", "Copy session transcript"),
    d("session.copy.id", "none", "Copy session ID"),
    d("session.move", "none", "Manage workspaces"),
    d("session.new", "<leader>n", "Create a new session"),
    d("session.list", "<leader>l", "List all sessions"),
    d("open.menu", "ctrl+o", "Open recent sessions and projects"),
    d(
        "session.tab.next",
        "ctrl+tab,alt+down",
        "Switch to next open session tab",
    ),
    d(
        "session.tab.previous",
        "ctrl+shift+tab,alt+up",
        "Switch to previous open session tab",
    ),
    d(
        "session.tab.history.back",
        "none",
        "Go back in session tab history",
    ),
    d(
        "session.tab.history.forward",
        "ctrl+i",
        "Go forward in session tab history",
    ),
    d(
        "session.tab.next_unread",
        "alt+shift+down",
        "Switch to next unread session tab",
    ),
    d(
        "session.tab.previous_unread",
        "alt+shift+up",
        "Switch to previous unread session tab",
    ),
    d(
        "session.tab.close",
        "<leader>w",
        "Close current session tab",
    ),
    d(
        "session.tab.reopen",
        "ctrl+shift+t",
        "Reopen last closed session tab",
    ),
    d("session.timeline", "<leader>g", "Show session timeline"),
    d("session.fork", "none", "Fork session from message"),
    d("session.rename", "ctrl+r", "Rename session"),
    d("session.delete", "ctrl+d", "Delete session"),
    d(
        "session.share",
        "none",
        "Deprecated: session sharing was removed",
    ),
    d(
        "session.unshare",
        "none",
        "Deprecated: session sharing was removed",
    ),
    d("session.interrupt", "escape", "Interrupt current session"),
    d(
        "session.background",
        "ctrl+b",
        "Background blocking session tools",
    ),
    d("session.compact", "<leader>c", "Compact the session"),
    d("session.aside", "none", "Ask a side question"),
    d("session.cd", "none", "Change working directory"),
    d(
        "session.queued_prompts",
        "<leader>q",
        "Manage queued prompts",
    ),
    d("queued_prompt.undo", "ctrl+u", "Undo queued prompt"),
    d("queued_prompt.delete", "ctrl+d", "Delete queued prompt"),
    d(
        "session.toggle.exploration_grouping",
        "none",
        "Toggle related tool call grouping",
    ),
    d(
        "session.verbosity.cycle",
        "none",
        "Cycle transcript verbosity",
    ),
    d("session.child.first", "down", "Toggle subagent picker"),
    d("session.child.next", "right", "Go to next child session"),
    d(
        "session.child.previous",
        "left",
        "Go to previous child session",
    ),
    d("session.parent", "up", "Go to parent session"),
    d(
        "session.pin.toggle",
        "ctrl+f",
        "Pin or unpin session in the session list",
    ),
    d(
        "session.quick_switch.1",
        "<leader>1",
        "Switch to session in quick slot 1",
    ),
    d(
        "session.quick_switch.2",
        "<leader>2",
        "Switch to session in quick slot 2",
    ),
    d(
        "session.quick_switch.3",
        "<leader>3",
        "Switch to session in quick slot 3",
    ),
    d(
        "session.quick_switch.4",
        "<leader>4",
        "Switch to session in quick slot 4",
    ),
    d(
        "session.quick_switch.5",
        "<leader>5",
        "Switch to session in quick slot 5",
    ),
    d(
        "session.quick_switch.6",
        "<leader>6",
        "Switch to session in quick slot 6",
    ),
    d(
        "session.quick_switch.7",
        "<leader>7",
        "Switch to session in quick slot 7",
    ),
    d(
        "session.quick_switch.8",
        "<leader>8",
        "Switch to session in quick slot 8",
    ),
    d(
        "session.quick_switch.9",
        "<leader>9",
        "Switch to session in quick slot 9",
    ),
    d(
        "session.tab.select.1",
        "<leader>1,ctrl+1",
        "Switch to session tab 1",
    ),
    d(
        "session.tab.select.2",
        "<leader>2,ctrl+2",
        "Switch to session tab 2",
    ),
    d(
        "session.tab.select.3",
        "<leader>3,ctrl+3",
        "Switch to session tab 3",
    ),
    d(
        "session.tab.select.4",
        "<leader>4,ctrl+4",
        "Switch to session tab 4",
    ),
    d(
        "session.tab.select.5",
        "<leader>5,ctrl+5",
        "Switch to session tab 5",
    ),
    d(
        "session.tab.select.6",
        "<leader>6,ctrl+6",
        "Switch to session tab 6",
    ),
    d(
        "session.tab.select.7",
        "<leader>7,ctrl+7",
        "Switch to session tab 7",
    ),
    d(
        "session.tab.select.8",
        "<leader>8,ctrl+8",
        "Switch to session tab 8",
    ),
    d(
        "session.tab.select.9",
        "<leader>9,ctrl+9",
        "Switch to session tab 9",
    ),
    d(
        "session.tab.select.10",
        "<leader>0,ctrl+0",
        "Switch to session tab 10",
    ),
    d("stash.delete", "ctrl+d", "Delete stash entry"),
    d(
        "model.dialog.provider",
        "ctrl+a",
        "Open provider list from model dialog",
    ),
    d(
        "model.dialog.favorite",
        "ctrl+f",
        "Toggle model favorite status",
    ),
    d(
        "model.dialog.usage",
        "ctrl+u",
        "Open usage limits from model dialog",
    ),
    d("model.list", "<leader>m", "List available models"),
    d("model.cycle_recent", "f2", "Next recently used model"),
    d(
        "model.cycle_recent_reverse",
        "shift+f2",
        "Previous recently used model",
    ),
    d("model.cycle_favorite", "none", "Next favorite model"),
    d(
        "model.cycle_favorite_reverse",
        "none",
        "Previous favorite model",
    ),
    d("mcp.list", "none", "List MCP servers"),
    d("usage.show", "none", "View usage limits"),
    d("provider.connect", "none", "Connect integration"),
    d("agent.list", "<leader>a", "List agents"),
    d("agent.cycle", "shift+tab", "Next agent"),
    d("agent.cycle.reverse", "none", "Previous agent"),
    d("variant.cycle", "ctrl+t", "Cycle model variants"),
    d("variant.list", "none", "List model variants"),
    d(
        "session.page.up",
        "pageup,ctrl+alt+b,kppageup",
        "Scroll messages up by one page",
    ),
    d(
        "session.page.down",
        "pagedown,ctrl+alt+f,kppagedown",
        "Scroll messages down by one page",
    ),
    d(
        "session.line.up",
        "ctrl+alt+y",
        "Scroll messages up by one line",
    ),
    d(
        "session.line.down",
        "ctrl+alt+e",
        "Scroll messages down by one line",
    ),
    d(
        "session.half.page.up",
        "ctrl+alt+u",
        "Scroll messages up by half page",
    ),
    d(
        "session.half.page.down",
        "ctrl+alt+d",
        "Scroll messages down by half page",
    ),
    d(
        "session.first",
        "ctrl+g,home,alt+home,kphome",
        "Navigate to first message",
    ),
    d(
        "session.last",
        "ctrl+alt+g,end,kpend",
        "Navigate to last message",
    ),
    d("session.message.next", "none", "Navigate to next message"),
    d(
        "session.message.previous",
        "none",
        "Navigate to previous message",
    ),
    d(
        "session.message.user.next",
        "none",
        "Navigate to next user message",
    ),
    d(
        "session.message.user.previous",
        "none",
        "Navigate to previous user message",
    ),
    d(
        "session.messages_last_user",
        "alt+end",
        "Navigate to last user message",
    ),
    d("messages.copy", "<leader>y", "Copy message"),
    d("session.undo", "<leader>u", "Undo message"),
    d("session.redo", "<leader>r", "Redo message"),
    d(
        "session.toggle.thinking",
        "none",
        "Toggle thinking blocks visibility",
    ),
    d("prompt.submit", "none", "Submit prompt"),
    d("prompt.queue", "<leader>return", "Queue prompt"),
    d(
        "prompt.editor_context.clear",
        "none",
        "Clear editor context",
    ),
    d("prompt.images.view", "<leader>i", "View image attachments"),
    d("prompt.skills", "none", "Open skill selector"),
    d("prompt.stash", "none", "Stash prompt"),
    d("prompt.stash.pop", "none", "Pop stashed prompt"),
    d("prompt.stash.list", "none", "List stashed prompts"),
    d("prompt.clear", "ctrl+c", "Clear input field"),
    d(
        "prompt.paste",
        r#"{"key":"ctrl+v","preventDefault":false}"#,
        "Paste from clipboard",
    ),
    d("input.submit", "return", "Submit input"),
    d(
        "input.newline",
        "shift+return,ctrl+return,alt+return,ctrl+j",
        "Insert newline in input",
    ),
    d(
        "input.move.left",
        "left,ctrl+b",
        "Move cursor left in input",
    ),
    d(
        "input.move.right",
        "right,ctrl+f",
        "Move cursor right in input",
    ),
    d("input.move.up", "up", "Move cursor up in input"),
    d("input.move.down", "down", "Move cursor down in input"),
    d("input.select.left", "shift+left", "Select left in input"),
    d("input.select.right", "shift+right", "Select right in input"),
    d("input.select.up", "shift+up", "Select up in input"),
    d("input.select.down", "shift+down", "Select down in input"),
    d(
        "input.line.home",
        "ctrl+a",
        "Move to start of line in input",
    ),
    d("input.line.end", "ctrl+e", "Move to end of line in input"),
    d(
        "input.select.line.home",
        "ctrl+shift+a",
        "Select to start of line in input",
    ),
    d(
        "input.select.line.end",
        "ctrl+shift+e",
        "Select to end of line in input",
    ),
    d(
        "input.visual.line.home",
        "alt+a",
        "Move to start of visual line in input",
    ),
    d(
        "input.visual.line.end",
        "alt+e",
        "Move to end of visual line in input",
    ),
    d(
        "input.select.visual.line.home",
        "alt+shift+a",
        "Select to start of visual line in input",
    ),
    d(
        "input.select.visual.line.end",
        "alt+shift+e",
        "Select to end of visual line in input",
    ),
    d(
        "input.buffer.home",
        "none",
        "Move to start of buffer in input",
    ),
    d("input.buffer.end", "none", "Move to end of buffer in input"),
    d(
        "input.select.buffer.home",
        "shift+home",
        "Select to start of buffer in input",
    ),
    d(
        "input.select.buffer.end",
        "shift+end",
        "Select to end of buffer in input",
    ),
    d("input.delete.line", "ctrl+shift+d", "Delete line in input"),
    d(
        "input.delete.to.line.end",
        "ctrl+k",
        "Delete to end of line in input",
    ),
    d(
        "input.delete.to.line.start",
        "ctrl+u",
        "Delete to start of line in input",
    ),
    d(
        "input.backspace",
        "backspace,shift+backspace",
        "Backspace in input",
    ),
    d(
        "input.delete",
        "ctrl+d,delete,shift+delete",
        "Delete character in input",
    ),
    d("input.undo", "ctrl+-,super+z", "Undo in input"),
    d("input.redo", "ctrl+.,super+shift+z", "Redo in input"),
    d(
        "input.word.forward",
        "alt+f,alt+right,ctrl+right",
        "Move word forward in input",
    ),
    d(
        "input.word.backward",
        "alt+b,alt+left,ctrl+left",
        "Move word backward in input",
    ),
    d(
        "input.select.word.forward",
        "alt+shift+f,alt+shift+right",
        "Select word forward in input",
    ),
    d(
        "input.select.word.backward",
        "alt+shift+b,alt+shift+left",
        "Select word backward in input",
    ),
    d(
        "input.delete.word.forward",
        "alt+d,alt+delete,ctrl+delete",
        "Delete word forward in input",
    ),
    d(
        "input.delete.word.backward",
        "ctrl+w,ctrl+backspace,alt+backspace",
        "Delete word backward in input",
    ),
    d("input.select.all", "super+a", "Select all in input"),
    d("prompt.history.previous", "up", "Previous history item"),
    d("prompt.history.next", "down", "Next history item"),
    d("composer.subagent.up", "up", "Previous subagent"),
    d("composer.subagent.down", "down", "Next subagent"),
    d("composer.subagent.select", "return", "Navigate to subagent"),
    d(
        "composer.subagent.interrupt",
        "ctrl+d",
        "Interrupt subagent",
    ),
    d("composer.shell.up", "up", "Previous shell"),
    d("composer.shell.down", "down", "Next shell"),
    d("composer.shell.select", "return", "View shell output"),
    d("composer.shell.kill", "ctrl+d", "Kill shell command"),
    d("composer.terminal.up", "up,k", "Previous terminal"),
    d("composer.terminal.down", "down,j", "Next terminal"),
    d("composer.terminal.select", "return", "Select terminal"),
    d(
        "dialog.select.prev",
        "up,ctrl+p",
        "Move to previous dialog item",
    ),
    d(
        "dialog.select.next",
        "down,ctrl+n",
        "Move to next dialog item",
    ),
    d(
        "dialog.select.page_up",
        "pageup",
        "Move up one page in dialog",
    ),
    d(
        "dialog.select.page_down",
        "pagedown",
        "Move down one page in dialog",
    ),
    d("dialog.select.home", "home", "Move to first dialog item"),
    d("dialog.select.end", "end", "Move to last dialog item"),
    d(
        "dialog.select.submit",
        "return",
        "Submit selected dialog item",
    ),
    d("dialog.prompt.submit", "return", "Submit dialog prompt"),
    d(
        "dialog.integration.rename",
        "ctrl+r",
        "Rename integration account",
    ),
    d(
        "dialog.integration.delete",
        "ctrl+d",
        "Delete integration account",
    ),
    d("dialog.integration.copy", "tab", "Copy authorization link"),
    d("dialog.worktree.generate", "tab", "Generate worktree name"),
    d("dialog.move_session.new", "ctrl+a", "New worktree"),
    d(
        "dialog.move_session.move",
        "ctrl+m",
        "Move session to worktree",
    ),
    d("dialog.move_session.delete", "ctrl+d", "Delete worktree"),
    d("dialog.move_session.refresh", "ctrl+r", "Refresh worktrees"),
    d(
        "prompt.autocomplete.prev",
        "up,ctrl+p",
        "Move to previous autocomplete item",
    ),
    d(
        "prompt.autocomplete.next",
        "down,ctrl+n",
        "Move to next autocomplete item",
    ),
    d("prompt.autocomplete.hide", "escape", "Hide autocomplete"),
    d(
        "prompt.autocomplete.select",
        "return",
        "Select autocomplete item",
    ),
    d(
        "prompt.autocomplete.complete",
        "tab",
        "Complete autocomplete item",
    ),
    d(
        "permission.prompt.fullscreen",
        "ctrl+f",
        "Toggle permission prompt fullscreen",
    ),
    d("plugins.toggle", "return", "Toggle plugin"),
    d("dialog.mcp.toggle", "space", "Toggle MCP server"),
    d("dialog.plugins.error", "space", "View plugin error"),
    d(
        "dialog.plugins.update",
        "ctrl+u",
        "Update plugin from plugin dialog",
    ),
    d(
        "dialog.plugins.check",
        "ctrl+r",
        "Check for plugin updates from plugin dialog",
    ),
    d("terminal.suspend", "ctrl+z", "Suspend terminal"),
    d("terminal.title.toggle", "none", "Toggle terminal title"),
    d("plugins.list", "none", "Open plugin manager dialog"),
    d("which-key.toggle", "ctrl+alt+k", "Toggle which-key panel"),
    d(
        "which-key.layout.toggle",
        "ctrl+alt+shift+k",
        "Switch which-key layout",
    ),
    d(
        "which-key.pending.toggle",
        "ctrl+alt+shift+p",
        "Toggle which-key pending preview",
    ),
    d(
        "which-key.group.previous",
        "ctrl+alt+left,ctrl+alt+[",
        "Previous which-key group",
    ),
    d(
        "which-key.group.next",
        "ctrl+alt+right,ctrl+alt+]",
        "Next which-key group",
    ),
    d(
        "which-key.scroll.up",
        "ctrl+alt+up,ctrl+alt+p",
        "Scroll which-key up",
    ),
    d(
        "which-key.scroll.down",
        "ctrl+alt+down,ctrl+alt+n",
        "Scroll which-key down",
    ),
    d("which-key.page.up", "ctrl+alt+pageup", "Page which-key up"),
    d(
        "which-key.page.down",
        "ctrl+alt+pagedown",
        "Page which-key down",
    ),
    d(
        "which-key.home",
        "ctrl+alt+home",
        "Jump to first which-key binding",
    ),
    d(
        "which-key.end",
        "ctrl+alt+end",
        "Jump to last which-key binding",
    ),
    // CodeSmith's own commands.
    d(
        "session.tab.move.left",
        "<leader>shift+left",
        "Move the session tab left",
    ),
    d(
        "session.tab.move.right",
        "<leader>shift+right",
        "Move the session tab right",
    ),
    d(
        "session.toggle.tool_output",
        "ctrl+o",
        "Expand or collapse tool output",
    ),
    d(
        "session.tool.select",
        "alt+up",
        "Select the latest tool group",
    ),
    d(
        "session.tool.previous",
        "up",
        "Select the tool group before",
    ),
    d("session.tool.next", "down", "Select the tool group after"),
    d(
        "session.tool.open",
        "return,space,right",
        "Open the selected tool group",
    ),
    d(
        "session.tool.close",
        "left",
        "Close the selected tool group",
    ),
    d(
        "session.tool.exit",
        "escape",
        "Leave the tool groups for the input",
    ),
    d(
        "permission.allow.once",
        "y",
        "Allow a permission request once",
    ),
    d(
        "permission.allow.always",
        "a",
        "Always allow a permission request",
    ),
    d("permission.reject", "n", "Reject a permission request"),
];

/// Where CodeSmith's default differs from OpenCode's, to keep its own keys:
/// Tab and Shift+Tab cycle agents both ways, Ctrl+X h opens the help, Ctrl+X
/// b backgrounds commands, Ctrl+End follows the transcript, Tab moves
/// down a picker, Alt+Up selects a tool group (so it no longer goes to the
/// previous tab), and Ctrl+X Shift+T reopens a tab (terminals without the
/// kitty keyboard protocol send Ctrl+Shift+T as Ctrl+T). Ctrl+O stays the
/// tool output key, so the open menu is Ctrl+X o; and since those terminals
/// send Ctrl+M as Enter, the worktrees dialog also moves the session with
/// Ctrl+G.
pub const CODESMITH_DEFAULTS: &[(&str, &str)] = &[
    ("session.tab.previous", "ctrl+shift+tab"),
    ("session.tab.reopen", "ctrl+shift+t,<leader>shift+t"),
    ("agent.cycle", "tab"),
    ("agent.cycle.reverse", "shift+tab"),
    ("help.show", "<leader>h"),
    ("session.background", "ctrl+b,<leader>b"),
    ("session.last", "ctrl+alt+g,end,kpend,ctrl+end"),
    ("dialog.select.next", "down,ctrl+n,tab"),
    ("open.menu", "<leader>o"),
    ("dialog.move_session.move", "ctrl+m,ctrl+g"),
];

/// OpenCode's first-format names (`CommandMap`).
pub const LEGACY: &[(&str, &str)] = &[
    ("app_exit", "app.exit"),
    ("app_debug", "app.debug"),
    ("app_console", "app.console"),
    ("app_heap_snapshot", "app.heap_snapshot"),
    ("app_toggle_animations", "app.toggle.animations"),
    ("app_toggle_file_context", "app.toggle.file_context"),
    ("app_toggle_diffwrap", "app.toggle.diffwrap"),
    ("app_toggle_paste_summary", "app.toggle.paste_summary"),
    ("command_list", "command.palette.show"),
    ("help_show", "help.show"),
    ("docs_open", "docs.open"),
    ("diff_open", "diff.open"),
    ("diff_close", "diff.close"),
    ("diff_toggle", "diff.toggle"),
    ("diff_expand", "diff.expand"),
    ("diff_expand_all", "diff.expand_all"),
    ("diff_collapse", "diff.collapse"),
    ("diff_switch_focus", "diff.switch_focus"),
    ("diff_next_hunk", "diff.next_hunk"),
    ("diff_previous_hunk", "diff.previous_hunk"),
    ("diff_next_file", "diff.next_file"),
    ("diff_previous_file", "diff.previous_file"),
    ("diff_toggle_file_tree", "diff.toggle_file_tree"),
    ("diff_single_patch", "diff.single_patch"),
    ("diff_switch_source", "diff.switch_source"),
    ("diff_toggle_view", "diff.toggle_view"),
    ("diff_help", "diff.help"),
    ("editor_open", "prompt.editor"),
    ("theme_list", "theme.switch"),
    ("theme_switch_mode", "theme.switch_mode"),
    ("theme_mode_lock", "theme.mode.lock"),
    ("sidebar_toggle", "session.sidebar.toggle"),
    ("scrollbar_toggle", "session.toggle.scrollbar"),
    ("status_view", "opencode.status"),
    ("debug_view", "opencode.debug"),
    ("session_export", "session.export"),
    ("session_copy", "session.copy"),
    ("session_move", "session.move"),
    ("session_new", "session.new"),
    ("session_list", "session.list"),
    ("open_menu", "open.menu"),
    ("session_tab_next", "session.tab.next"),
    ("session_tab_previous", "session.tab.previous"),
    ("session_tab_next_unread", "session.tab.next_unread"),
    ("session_tab_previous_unread", "session.tab.previous_unread"),
    ("session_tab_close", "session.tab.close"),
    ("session_tab_reopen", "session.tab.reopen"),
    ("session_timeline", "session.timeline"),
    ("session_fork", "session.fork"),
    ("session_rename", "session.rename"),
    ("session_delete", "session.delete"),
    ("session_share", "session.share"),
    ("session_unshare", "session.unshare"),
    ("session_interrupt", "session.interrupt"),
    ("session_background", "session.background"),
    ("session_compact", "session.compact"),
    ("session_queued_prompts", "session.queued_prompts"),
    ("queued_prompt_delete", "queued_prompt.delete"),
    ("session_child_first", "session.child.first"),
    ("session_parent", "session.parent"),
    ("session_pin_toggle", "session.pin.toggle"),
    ("session_quick_switch_1", "session.quick_switch.1"),
    ("session_quick_switch_2", "session.quick_switch.2"),
    ("session_quick_switch_3", "session.quick_switch.3"),
    ("session_quick_switch_4", "session.quick_switch.4"),
    ("session_quick_switch_5", "session.quick_switch.5"),
    ("session_quick_switch_6", "session.quick_switch.6"),
    ("session_quick_switch_7", "session.quick_switch.7"),
    ("session_quick_switch_8", "session.quick_switch.8"),
    ("session_quick_switch_9", "session.quick_switch.9"),
    ("session_tab_select_1", "session.tab.select.1"),
    ("session_tab_select_2", "session.tab.select.2"),
    ("session_tab_select_3", "session.tab.select.3"),
    ("session_tab_select_4", "session.tab.select.4"),
    ("session_tab_select_5", "session.tab.select.5"),
    ("session_tab_select_6", "session.tab.select.6"),
    ("session_tab_select_7", "session.tab.select.7"),
    ("session_tab_select_8", "session.tab.select.8"),
    ("session_tab_select_9", "session.tab.select.9"),
    ("session_tab_select_10", "session.tab.select.10"),
    ("stash_delete", "stash.delete"),
    ("model_provider_list", "model.dialog.provider"),
    ("model_favorite_toggle", "model.dialog.favorite"),
    ("model_list", "model.list"),
    ("model_cycle_recent", "model.cycle_recent"),
    ("model_cycle_recent_reverse", "model.cycle_recent_reverse"),
    ("model_cycle_favorite", "model.cycle_favorite"),
    (
        "model_cycle_favorite_reverse",
        "model.cycle_favorite_reverse",
    ),
    ("mcp_list", "mcp.list"),
    ("provider_connect", "provider.connect"),
    ("agent_list", "agent.list"),
    ("agent_cycle", "agent.cycle"),
    ("agent_cycle_reverse", "agent.cycle.reverse"),
    ("variant_cycle", "variant.cycle"),
    ("variant_list", "variant.list"),
    ("messages_page_up", "session.page.up"),
    ("messages_page_down", "session.page.down"),
    ("messages_line_up", "session.line.up"),
    ("messages_line_down", "session.line.down"),
    ("messages_half_page_up", "session.half.page.up"),
    ("messages_half_page_down", "session.half.page.down"),
    ("messages_first", "session.first"),
    ("messages_last", "session.last"),
    ("messages_next", "session.message.next"),
    ("messages_previous", "session.message.previous"),
    ("messages_next_user", "session.message.user.next"),
    ("messages_previous_user", "session.message.user.previous"),
    ("messages_last_user", "session.messages_last_user"),
    ("messages_copy", "messages.copy"),
    ("messages_undo", "session.undo"),
    ("messages_redo", "session.redo"),
    ("display_thinking", "session.toggle.thinking"),
    ("prompt_submit", "prompt.submit"),
    ("prompt_queue", "prompt.queue"),
    ("prompt_editor_context_clear", "prompt.editor_context.clear"),
    ("prompt_images_view", "prompt.images.view"),
    ("prompt_skills", "prompt.skills"),
    ("prompt_stash", "prompt.stash"),
    ("prompt_stash_pop", "prompt.stash.pop"),
    ("prompt_stash_list", "prompt.stash.list"),
    ("input_clear", "prompt.clear"),
    ("input_paste", "prompt.paste"),
    ("input_submit", "input.submit"),
    ("input_newline", "input.newline"),
    ("input_move_left", "input.move.left"),
    ("input_move_right", "input.move.right"),
    ("input_move_up", "input.move.up"),
    ("input_move_down", "input.move.down"),
    ("input_select_left", "input.select.left"),
    ("input_select_right", "input.select.right"),
    ("input_select_up", "input.select.up"),
    ("input_select_down", "input.select.down"),
    ("input_line_home", "input.line.home"),
    ("input_line_end", "input.line.end"),
    ("input_select_line_home", "input.select.line.home"),
    ("input_select_line_end", "input.select.line.end"),
    ("input_visual_line_home", "input.visual.line.home"),
    ("input_visual_line_end", "input.visual.line.end"),
    (
        "input_select_visual_line_home",
        "input.select.visual.line.home",
    ),
    (
        "input_select_visual_line_end",
        "input.select.visual.line.end",
    ),
    ("input_buffer_home", "input.buffer.home"),
    ("input_buffer_end", "input.buffer.end"),
    ("input_select_buffer_home", "input.select.buffer.home"),
    ("input_select_buffer_end", "input.select.buffer.end"),
    ("input_delete_line", "input.delete.line"),
    ("input_delete_to_line_end", "input.delete.to.line.end"),
    ("input_delete_to_line_start", "input.delete.to.line.start"),
    ("input_backspace", "input.backspace"),
    ("input_delete", "input.delete"),
    ("input_undo", "input.undo"),
    ("input_redo", "input.redo"),
    ("input_word_forward", "input.word.forward"),
    ("input_word_backward", "input.word.backward"),
    ("input_select_word_forward", "input.select.word.forward"),
    ("input_select_word_backward", "input.select.word.backward"),
    ("input_delete_word_forward", "input.delete.word.forward"),
    ("input_delete_word_backward", "input.delete.word.backward"),
    ("input_select_all", "input.select.all"),
    ("history_previous", "prompt.history.previous"),
    ("history_next", "prompt.history.next"),
    ("terminal_suspend", "terminal.suspend"),
    ("terminal_title_toggle", "terminal.title.toggle"),
];

/// The command a configured name means: itself when it is a command id,
/// else the id its first-format name maps to.
pub fn canonical(name: &str) -> &str {
    LEGACY
        .iter()
        .find(|(old, _)| *old == name)
        .map(|(_, new)| *new)
        .unwrap_or(name)
}

pub fn def(id: &str) -> Option<&'static Def> {
    DEFINITIONS.iter().find(|d| d.id == id)
}

/// A command id with the program's lifetime.
pub fn static_id(id: &str) -> Option<&'static str> {
    def(id).map(|d| d.id)
}

/// The default binding as configured (CodeSmith's where it has its own):
/// a binding string, or a binding object as JSON.
pub fn default_value(id: &str) -> Option<&'static str> {
    CODESMITH_DEFAULTS
        .iter()
        .find(|(i, _)| *i == id)
        .map(|(_, v)| *v)
        .or_else(|| def(id).map(|d| d.default))
}

/// The default binding as a config value.
pub fn default_json(id: &str) -> Value {
    match default_value(id) {
        Some(v) if v.starts_with('{') => serde_json::from_str(v).unwrap_or(Value::Null),
        Some(v) => Value::String(v.into()),
        None => Value::String("none".into()),
    }
}

/// The names in `input` that are not command ids (OpenCode's `unknownKeys`).
#[cfg(test)]
pub fn unknown_keys(input: &Map<String, Value>) -> Vec<String> {
    input.keys().filter(|k| def(k).is_none()).cloned().collect()
}

/// Rewrite first-format names to command ids (an id set directly wins over
/// its old name) and drop names that are neither, returning those.
pub fn migrate(input: &Map<String, Value>) -> (Map<String, Value>, Vec<String>) {
    let mut out = Map::new();
    let mut dropped = Vec::new();
    for (k, v) in input {
        let id = canonical(k);
        if def(id).is_none() {
            // Commands OpenCode retired (`app.heap_snapshot`) go quietly.
            if id != "app.heap_snapshot" {
                dropped.push(k.clone());
            }
            continue;
        }
        if id != k && input.contains_key(id) {
            continue;
        }
        out.insert(id.to_string(), v.clone());
    }
    (out, dropped)
}

// ----- binding values -------------------------------------------------------

/// A key with its modifiers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Stroke {
    /// A character (letters lowercase, with `shift` for capitals) or a
    /// key name: `return`, `escape`, `tab`, `space`, `backspace`, `delete`,
    /// `insert`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`,
    /// `pagedown`, `f1`…
    pub key: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub sup: bool,
    pub hyper: bool,
}

/// One configured binding (OpenCode's `Binding`): the key as written and
/// whether it fires on release.
#[derive(Debug, Clone, PartialEq)]
pub struct Binding {
    pub key: BindingKey,
    pub release: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BindingKey {
    /// Comma-separated alternatives, as written.
    Text(String),
    Stroke(Stroke),
}

#[cfg(test)]
impl Binding {
    /// The key as written (`ctrl+x`), or a key object spelled out.
    pub fn key_text(&self) -> String {
        match &self.key {
            BindingKey::Text(s) => s.clone(),
            BindingKey::Stroke(s) => stroke_text(s),
        }
    }
}

#[cfg(test)]
fn stroke_text(s: &Stroke) -> String {
    let mut parts = Vec::new();
    for (on, m) in [
        (s.ctrl, "ctrl"),
        (s.alt, "alt"),
        (s.shift, "shift"),
        (s.sup, "super"),
        (s.hyper, "hyper"),
    ] {
        if on {
            parts.push(m.to_string());
        }
    }
    parts.push(s.key.clone());
    parts.join("+")
}

fn stroke_object(o: &Map<String, Value>) -> Result<Stroke, String> {
    let name = o
        .get("name")
        .and_then(Value::as_str)
        .ok_or("a key object needs a name")?;
    let flag = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
    let mut s = parse_stroke(name)?;
    s.ctrl |= flag("ctrl");
    s.alt |= flag("meta") || flag("alt");
    s.shift |= flag("shift");
    s.sup |= flag("super");
    s.hyper |= flag("hyper");
    Ok(s)
}

/// A configured binding value as bindings: `false` and `"none"` are none.
pub fn parse_value(v: &Value) -> Result<Vec<Binding>, String> {
    match v {
        Value::Bool(false) => Ok(vec![]),
        Value::String(s) if s == "none" => Ok(vec![]),
        Value::Array(items) => {
            let mut out = Vec::new();
            for i in items {
                match i {
                    Value::Array(_) | Value::Bool(_) => return Err(format!("{i} is not a key")),
                    _ => out.extend(parse_value(i)?),
                }
            }
            Ok(out)
        }
        Value::String(s) => Ok(vec![Binding {
            key: BindingKey::Text(s.clone()),
            release: false,
        }]),
        Value::Object(o) if o.contains_key("key") => {
            let key = match &o["key"] {
                Value::String(s) => BindingKey::Text(s.clone()),
                Value::Object(k) => BindingKey::Stroke(stroke_object(k)?),
                other => return Err(format!("{other} is not a key")),
            };
            let release = match o.get("event") {
                None => false,
                Some(Value::String(e)) if e == "press" => false,
                Some(Value::String(e)) if e == "release" => true,
                Some(other) => return Err(format!("{other} is not a key event")),
            };
            Ok(vec![Binding { key, release }])
        }
        Value::Object(o) => Ok(vec![Binding {
            key: BindingKey::Stroke(stroke_object(o)?),
            release: false,
        }]),
        other => Err(format!("{other} is not a key")),
    }
}

const NAMES: &[(&str, &str)] = &[
    ("return", "return"),
    ("enter", "return"),
    ("escape", "escape"),
    ("esc", "escape"),
    ("tab", "tab"),
    ("space", "space"),
    ("backspace", "backspace"),
    ("delete", "delete"),
    ("del", "delete"),
    ("insert", "insert"),
    ("up", "up"),
    ("down", "down"),
    ("left", "left"),
    ("right", "right"),
    ("home", "home"),
    ("end", "end"),
    ("pageup", "pageup"),
    ("pgup", "pageup"),
    ("pagedown", "pagedown"),
    ("pgdown", "pagedown"),
    ("pgdn", "pagedown"),
    ("comma", ","),
    ("plus", "+"),
];

fn key_name(s: &str) -> Option<(String, bool)> {
    let lower = s.to_lowercase();
    // Keypad keys are the keys they mirror.
    let bare = lower
        .strip_prefix("kp")
        .filter(|r| r.len() > 1)
        .unwrap_or(&lower);
    if let Some((_, n)) = NAMES.iter().find(|(a, _)| *a == bare) {
        return Some((n.to_string(), false));
    }
    if let Some(n) = bare.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        if (1..=24).contains(&n) {
            return Some((format!("f{n}"), false));
        }
    }
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if c.is_uppercase() {
        return Some((c.to_lowercase().to_string(), true));
    }
    Some((c.to_string(), false))
}

/// One key with modifiers: `ctrl+x`, `shift+return`, `?`, `E`.
pub fn parse_stroke(s: &str) -> Result<Stroke, String> {
    let (mods, key) = if s.len() > 1 && s.ends_with("++") {
        (&s[..s.len() - 2], "+")
    } else if s.len() > 1 && s.contains('+') {
        s.rsplit_once('+').unwrap()
    } else {
        ("", s)
    };
    let (key, upper) = key_name(key).ok_or_else(|| format!("`{s}` is not a key"))?;
    let mut st = Stroke {
        key,
        shift: upper,
        ..Default::default()
    };
    for m in mods.split('+').filter(|m| !m.is_empty()) {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => st.ctrl = true,
            "alt" | "meta" | "option" => st.alt = true,
            "shift" => st.shift = true,
            "super" | "cmd" | "command" | "win" => st.sup = true,
            "hyper" => st.hyper = true,
            _ => return Err(format!("`{s}` is not a key")),
        }
    }
    Ok(normal(st))
}

/// Shift only tells letters apart: `shift+/` is `/`, as terminals rarely
/// report shift with a symbol.
fn normal(mut s: Stroke) -> Stroke {
    let mut chars = s.key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if !c.is_alphabetic() {
            s.shift = false;
        }
    }
    s
}

/// A step of a key sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    Leader,
    Key(Stroke),
}

/// One alternative: `<leader>m`, `ctrl+x`, `gg`, `ctrl+x ctrl+s`.
pub fn parse_sequence(s: &str) -> Result<Vec<Tok>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("an empty key".into());
    }
    let mut out = Vec::new();
    for part in s.split_whitespace() {
        let mut rest = part;
        while let Some(r) = rest.strip_prefix("<leader>") {
            out.push(Tok::Leader);
            rest = r;
        }
        if rest.is_empty() {
            continue;
        }
        match parse_stroke(rest) {
            Ok(st) => out.push(Tok::Key(st)),
            // `gg`: plain characters typed in turn.
            Err(e) => {
                if rest.contains('+') || rest.chars().count() < 2 {
                    return Err(e);
                }
                for c in rest.chars() {
                    out.push(Tok::Key(parse_stroke(&c.to_string())?));
                }
            }
        }
    }
    Ok(out)
}

/// The alternatives of a binding.
pub fn sequences(b: &Binding) -> Result<Vec<Vec<Tok>>, String> {
    match &b.key {
        BindingKey::Stroke(s) => Ok(vec![vec![Tok::Key(s.clone())]]),
        BindingKey::Text(t) => {
            // A lone `,` is the comma key, not a separator.
            if t.trim() == "," {
                return Ok(vec![vec![Tok::Key(parse_stroke(",")?)]]);
            }
            t.split(',').map(parse_sequence).collect()
        }
    }
}

/// A terminal key event as a stroke.
pub fn stroke(k: &KeyEvent) -> Option<Stroke> {
    let m = k.modifiers;
    let mut s = Stroke {
        ctrl: m.contains(KeyModifiers::CONTROL),
        alt: m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::META),
        shift: m.contains(KeyModifiers::SHIFT),
        sup: m.contains(KeyModifiers::SUPER),
        hyper: m.contains(KeyModifiers::HYPER),
        ..Default::default()
    };
    s.key = match k.code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => {
            if c.is_uppercase() {
                s.shift = true;
            }
            c.to_lowercase().to_string()
        }
        KeyCode::Enter => "return".into(),
        KeyCode::Esc => "escape".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => {
            s.shift = true;
            "tab".into()
        }
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return None,
    };
    Some(normal(s))
}

// ----- display --------------------------------------------------------------

fn show_key(k: &str) -> &str {
    match k {
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "pageup" => "pgup",
        "pagedown" => "pgdn",
        "delete" => "del",
        "return" => "enter",
        "escape" => "esc",
        k => k,
    }
}

/// A key as shown: `ctrl+x`, `↑`, `G`.
pub fn show(s: &Stroke) -> String {
    show_stroke(s)
}

fn show_stroke(s: &Stroke) -> String {
    let mut out = String::new();
    for (on, m) in [
        (s.ctrl, "ctrl+"),
        (s.alt, "alt+"),
        (s.sup, "super+"),
        (s.hyper, "hyper+"),
    ] {
        if on {
            out.push_str(m);
        }
    }
    let letter = s.key.chars().count() == 1 && s.key.chars().all(char::is_alphabetic);
    if s.shift && letter && !s.ctrl && !s.alt {
        out.push_str(&s.key.to_uppercase());
        return out;
    }
    if s.shift {
        out.push_str("shift+");
    }
    out.push_str(show_key(&s.key));
    out
}

// ----- the compiled keymap --------------------------------------------------

/// The configured bindings by command (OpenCode's binding lookup).
#[derive(Debug, Clone, Default)]
pub struct Keybinds {
    map: HashMap<String, Vec<Binding>>,
    /// Commands whose binding the configuration sets.
    user: HashSet<String>,
}

impl Keybinds {
    pub fn get(&self, id: &str) -> &[Binding] {
        self.map.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    #[cfg(test)]
    pub fn has(&self, id: &str) -> bool {
        !self.get(id).is_empty()
    }

    pub fn configured(&self, id: &str) -> bool {
        self.user.contains(id)
    }
}

/// Resolve configured bindings over the defaults (OpenCode's `resolve` of
/// `keybinds`). Without terminal suspend (`ctrl+z`), suspend is unbound
/// and `ctrl+z` undoes in the prompt unless undo is configured.
pub fn resolve(overrides: &Map<String, Value>, terminal_suspend: bool) -> (Keybinds, Vec<String>) {
    let mut over = overrides.clone();
    let mut warnings = Vec::new();
    if !terminal_suspend {
        over.insert("terminal.suspend".into(), Value::String("none".into()));
        if !over.contains_key("input.undo") {
            let mut keys = vec!["ctrl+z".to_string()];
            for k in default_value("input.undo").unwrap_or("").split(',') {
                if !keys.iter().any(|x| x == k) {
                    keys.push(k.to_string());
                }
            }
            over.insert("input.undo".into(), Value::String(keys.join(",")));
        }
    }
    let mut out = Keybinds::default();
    for d in DEFINITIONS {
        let configured = over.get(d.id);
        let value = configured.cloned().unwrap_or_else(|| default_json(d.id));
        let bindings = match parse_value(&value) {
            Ok(b) => b,
            Err(e) => {
                warnings.push(format!("keybinds: {}: {e}", d.id));
                parse_value(&default_json(d.id)).unwrap_or_default()
            }
        };
        if overrides.contains_key(d.id) {
            out.user.insert(d.id.to_string());
        }
        out.map.insert(d.id.to_string(), bindings);
    }
    (out, warnings)
}

/// What a key sequence matches.
#[derive(Debug, Default, PartialEq)]
pub struct Lookup {
    /// The commands it completes, those the configuration bound first.
    pub exact: Vec<&'static str>,
    /// Whether a longer sequence continues it.
    pub prefix: bool,
}

/// The bindings, compiled into key sequences for the commands CodeSmith
/// runs, with the leader key and its timeout.
#[derive(Debug, Clone)]
pub struct Keymap {
    pub binds: Keybinds,
    compiled: HashMap<&'static str, Vec<Vec<Stroke>>>,
    leader: Vec<Vec<Stroke>>,
    leader_text: String,
    pub timeout: Duration,
    /// Problems with the configuration, to show once.
    pub warnings: Vec<String>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::new(&Map::new(), LEADER_TIMEOUT_MS, false)
    }
}

/// The commands each part of the UI runs, in the order they are tried when
/// a key matches more than one.
pub mod scope {
    pub const AUTOCOMPLETE: &[&str] = &[
        "prompt.autocomplete.prev",
        "prompt.autocomplete.next",
        "prompt.autocomplete.hide",
        "prompt.autocomplete.select",
        "prompt.autocomplete.complete",
    ];
    pub const PROMPT: &[&str] = &[
        "prompt.clear",
        "session.interrupt",
        "session.background",
        "input.submit",
        "input.newline",
        "input.move.left",
        "input.move.right",
        "input.move.up",
        "input.move.down",
        "prompt.history.previous",
        "prompt.history.next",
        "input.line.home",
        "input.line.end",
        "input.visual.line.home",
        "input.visual.line.end",
        "input.buffer.home",
        "input.buffer.end",
        "input.word.forward",
        "input.word.backward",
        "input.backspace",
        "input.delete",
        "input.delete.word.backward",
        "input.delete.word.forward",
        "input.delete.to.line.end",
        "input.delete.to.line.start",
        "input.delete.line",
        "input.undo",
        "input.redo",
        "input.select.left",
        "input.select.right",
        "input.select.up",
        "input.select.down",
        "input.select.line.home",
        "input.select.line.end",
        "input.select.visual.line.home",
        "input.select.visual.line.end",
        "input.select.buffer.home",
        "input.select.buffer.end",
        "input.select.word.forward",
        "input.select.word.backward",
        "input.select.all",
        "app.toggle.paste_summary",
        "session.child.first",
        "prompt.stash",
        "prompt.stash.pop",
        "mcp.list",
        "app.exit",
        "terminal.suspend",
        "terminal.title.toggle",
        "prompt.editor",
        "session.message.next",
        "session.message.previous",
        "session.message.user.next",
        "session.message.user.previous",
        "session.messages_last_user",
        "command.palette.show",
        "session.copy.id",
        "model.cycle_recent",
        "model.cycle_recent_reverse",
        "model.cycle_favorite",
        "model.cycle_favorite_reverse",
        "agent.cycle",
        "agent.cycle.reverse",
        "agent.list",
        "model.list",
        "variant.cycle",
        "session.list",
        "open.menu",
        "session.move",
        "session.new",
        "session.tab.next",
        "session.tab.previous",
        "session.tab.next_unread",
        "session.tab.previous_unread",
        "session.tab.close",
        "session.tab.reopen",
        "session.tab.move.left",
        "session.tab.move.right",
        "session.tab.select.1",
        "session.tab.select.2",
        "session.tab.select.3",
        "session.tab.select.4",
        "session.tab.select.5",
        "session.tab.select.6",
        "session.tab.select.7",
        "session.tab.select.8",
        "session.tab.select.9",
        "session.tab.select.10",
        "session.compact",
        "session.undo",
        "session.redo",
        "help.show",
        "theme.switch",
        "theme.switch_mode",
        "theme.mode.lock",
        "session.toggle.thinking",
        "session.toggle.tool_output",
        "session.tool.select",
        "session.toggle.exploration_grouping",
        "session.verbosity.cycle",
        "session.page.up",
        "session.page.down",
        "session.half.page.up",
        "session.half.page.down",
        "session.line.up",
        "session.line.down",
        "session.first",
        "session.last",
        "diff.open",
        "pane.focus.right",
    ];
    /// The diff viewer's keys. Its own come before `app.exit` (Ctrl+C
    /// closes it), so Ctrl+D scrolls half a page there.
    pub const DIFF: &[&str] = &[
        "diff.close",
        "diff.down",
        "diff.up",
        "diff.page.down",
        "diff.page.up",
        "diff.half_page.down",
        "diff.half_page.up",
        "diff.first",
        "diff.last",
        "diff.next_hunk",
        "diff.previous_hunk",
        "diff.next_file",
        "diff.previous_file",
        "diff.mark_reviewed",
        "diff.toggle_file_tree",
        "diff.single_patch",
        "diff.switch_source",
        "diff.toggle_view",
        "diff.toggle_fullscreen",
        "diff.help",
        "pane.focus.left",
        "pane.focus.right",
        "diff.open",
        "app.exit",
    ];
    pub const PICKER: &[&str] = &[
        "dialog.select.prev",
        "dialog.select.next",
        "dialog.select.page_up",
        "dialog.select.page_down",
        "dialog.select.home",
        "dialog.select.end",
        "dialog.select.submit",
        "model.dialog.favorite",
    ];
    pub const QUESTION: &[&str] = &["dialog.prompt.submit"];
    /// The transcript cursor on a tool group (`session.tool.select`): any
    /// other key leaves it and does what it does in the prompt.
    pub const TRANSCRIPT: &[&str] = &[
        "session.tool.previous",
        "session.tool.next",
        "session.tool.open",
        "session.tool.close",
        "session.tool.exit",
        "session.toggle.tool_output",
        "session.page.up",
        "session.page.down",
        "session.half.page.up",
        "session.half.page.down",
        "session.line.up",
        "session.line.down",
        "session.first",
        "session.last",
    ];
    pub const PERMISSION: &[&str] = &[
        "permission.allow.once",
        "permission.allow.always",
        "permission.reject",
    ];
    /// The subagent and shell panel.
    pub const PANEL: &[&str] = &[
        "composer.subagent.up",
        "composer.subagent.down",
        "composer.subagent.select",
        "composer.subagent.interrupt",
        "composer.shell.up",
        "composer.shell.down",
        "composer.shell.select",
        "composer.shell.kill",
        "dialog.mcp.toggle",
    ];
    /// The worktrees dialog (`session.move`).
    pub const WORKTREES: &[&str] = &[
        "dialog.select.prev",
        "dialog.select.next",
        "dialog.select.page_up",
        "dialog.select.page_down",
        "dialog.select.home",
        "dialog.select.end",
        "dialog.select.submit",
        "dialog.move_session.new",
        "dialog.move_session.move",
        "dialog.move_session.delete",
        "dialog.move_session.refresh",
    ];
    pub const ALL: &[&[&str]] = &[
        AUTOCOMPLETE,
        PROMPT,
        DIFF,
        PICKER,
        QUESTION,
        TRANSCRIPT,
        PERMISSION,
        PANEL,
        WORKTREES,
    ];
}

fn expand(seq: &[Tok], leader: &[Vec<Stroke>]) -> Vec<Vec<Stroke>> {
    let mut out: Vec<Vec<Stroke>> = vec![vec![]];
    for t in seq {
        match t {
            Tok::Key(s) => out.iter_mut().for_each(|o| o.push(s.clone())),
            Tok::Leader => {
                out = out
                    .iter()
                    .flat_map(|o| {
                        leader.iter().map(move |l| {
                            let mut n = o.clone();
                            n.extend(l.iter().cloned());
                            n
                        })
                    })
                    .collect();
            }
        }
    }
    out.retain(|s| !s.is_empty());
    out
}

impl Keymap {
    /// Build from `tui.keybinds` (already [`migrate`]d), the leader timeout
    /// in milliseconds and whether `ctrl+z` suspends.
    pub fn new(overrides: &Map<String, Value>, timeout_ms: u64, terminal_suspend: bool) -> Keymap {
        let (binds, mut warnings) = resolve(overrides, terminal_suspend);
        let mut leader = Vec::new();
        for b in binds.get("leader") {
            match sequences(b) {
                Ok(alts) => {
                    for alt in alts {
                        let keys: Vec<Stroke> = alt
                            .into_iter()
                            .filter_map(|t| match t {
                                Tok::Key(s) => Some(s),
                                Tok::Leader => None,
                            })
                            .collect();
                        if !keys.is_empty() {
                            leader.push(keys);
                        }
                    }
                }
                Err(e) => warnings.push(format!("keybinds: leader: {e}")),
            }
        }
        let leader_text = leader
            .first()
            .map(|l| l.iter().map(show_stroke).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        let mut compiled = HashMap::new();
        for ids in scope::ALL {
            for id in ids.iter() {
                if compiled.contains_key(id) {
                    continue;
                }
                let mut seqs = Vec::new();
                for b in binds.get(id).iter().filter(|b| !b.release) {
                    match sequences(b) {
                        Ok(alts) => {
                            for a in alts {
                                seqs.extend(expand(&a, &leader));
                            }
                        }
                        Err(e) => warnings.push(format!("keybinds: {id}: {e}")),
                    }
                }
                compiled.insert(*id, seqs);
            }
        }
        let mut k = Keymap {
            binds,
            compiled,
            leader,
            leader_text,
            timeout: Duration::from_millis(timeout_ms.max(1)),
            warnings,
        };
        k.warnings.extend(k.conflicts());
        k
    }

    /// The key sequences that run `id`.
    pub fn sequences(&self, id: &str) -> &[Vec<Stroke>] {
        self.compiled.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The commands of `ids` that `seq` runs, and whether it is the start
    /// of a longer binding.
    pub fn lookup(&self, ids: &[&'static str], seq: &[Stroke]) -> Lookup {
        let mut out = Lookup::default();
        let mut user = Vec::new();
        for id in ids {
            for s in self.sequences(id) {
                if s.as_slice() == seq {
                    let list = if self.binds.configured(id) {
                        &mut user
                    } else {
                        &mut out.exact
                    };
                    if !list.contains(id) {
                        list.push(*id);
                    }
                } else if s.len() > seq.len() && s.starts_with(seq) {
                    out.prefix = true;
                }
            }
        }
        user.append(&mut out.exact);
        out.exact = user;
        out
    }

    /// Whether `seq` starts with the leader key.
    pub fn after_leader(&self, seq: &[Stroke]) -> bool {
        self.leader.iter().any(|l| seq.starts_with(l))
    }

    /// Clashes the configuration made: a configured binding that another
    /// command in the same place also has, or that starts a longer one
    /// (which then never runs).
    fn conflicts(&self) -> Vec<String> {
        let text = |s: &[Stroke]| s.iter().map(show_stroke).collect::<Vec<_>>().join(" ");
        let mut out = Vec::new();
        for ids in scope::ALL {
            for (i, a) in ids.iter().enumerate() {
                for b in ids.iter().skip(i + 1) {
                    let (ca, cb) = (self.binds.configured(a), self.binds.configured(b));
                    if !ca && !cb {
                        continue;
                    }
                    for sa in self.sequences(a) {
                        for sb in self.sequences(b) {
                            let msg = if sa == sb && ca && cb {
                                format!(
                                    "keybinds: {} is bound to both {a} and {b}; {a} goes first",
                                    text(sa)
                                )
                            } else if ca && sb.len() > sa.len() && sb.starts_with(sa) {
                                format!(
                                    "keybinds: {} ({a}) starts {} ({b}), which never runs",
                                    text(sa),
                                    text(sb)
                                )
                            } else if cb && sa.len() > sb.len() && sa.starts_with(sb) {
                                format!(
                                    "keybinds: {} ({b}) starts {} ({a}), which never runs",
                                    text(sb),
                                    text(sa)
                                )
                            } else {
                                continue;
                            };
                            if !out.contains(&msg) {
                                out.push(msg);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    fn show(&self, seq: &[Tok]) -> String {
        seq.iter()
            .map(|t| match t {
                Tok::Leader => self.leader_text.clone(),
                Tok::Key(s) => show_stroke(s),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Every alternative of `id`'s bindings as shown to the user.
    pub fn list(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        for b in self.binds.get(id) {
            let Ok(alts) = sequences(b) else { continue };
            for a in alts {
                if a.contains(&Tok::Leader) && self.leader.is_empty() {
                    continue;
                }
                let s = self.show(&a);
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
        out
    }

    /// The first binding of `id` as shown (OpenCode's shortcut).
    #[cfg(test)]
    pub fn first(&self, id: &str) -> Option<String> {
        self.list(id).into_iter().next()
    }

    /// The shortest binding of `id`, for hints; empty when unbound.
    pub fn hint(&self, id: &str) -> String {
        self.list(id)
            .into_iter()
            .min_by_key(|s| s.chars().count())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_parse() {
        let s = parse_stroke("ctrl+x").unwrap();
        assert!(s.ctrl && s.key == "x");
        assert_eq!(parse_stroke("shift+g").unwrap(), parse_stroke("G").unwrap());
        assert_eq!(parse_stroke("shift+/").unwrap(), parse_stroke("/").unwrap());
        assert_eq!(
            parse_stroke("pgup").unwrap(),
            parse_stroke("pageup").unwrap()
        );
        assert_eq!(
            parse_stroke("kphome").unwrap(),
            parse_stroke("home").unwrap()
        );
        assert_eq!(parse_stroke("ctrl+-").unwrap().key, "-");
        assert!(parse_stroke("ctrl+").is_err() || parse_stroke("ctrl+").unwrap().key == "+");
        assert!(parse_stroke("hyperdrive").is_err());
        assert_eq!(
            parse_sequence("<leader>m").unwrap(),
            [Tok::Leader, Tok::Key(parse_stroke("m").unwrap())]
        );
        assert_eq!(parse_sequence("gg").unwrap().len(), 2);
        assert!(parse_value(&json!(3)).is_err());
        assert_eq!(parse_value(&json!(false)).unwrap(), vec![]);
        let b = parse_value(&json!({ "name": "x", "ctrl": true })).unwrap();
        assert_eq!(b[0].key_text(), "ctrl+x");
    }
}
