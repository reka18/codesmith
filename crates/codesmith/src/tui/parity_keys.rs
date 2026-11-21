//! OpenCode's key binding and TUI settings tests, ported: `tui/test/config`,
//! `tui/test/config-v2`, `cli/test/config`, `tui/test/keymap`,
//! `tui/test/cli/tui/dialog-prompt` and `tui/test/cli/tui/reasoning-body`.
//! Each `oc_*` test names the OpenCode test it ports; "(adapted)" ones say
//! what differs, and a dropped half is named.

use super::app::{Action, App, Entry, Modal};
use super::keymap::{self, Keymap};
use super::settings::{self, CursorStyle};
use codesmith_core::config;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use serde_json::{json, Map, Value};
use std::sync::Arc;

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// The resolved bindings of a `tui` config, as OpenCode's `resolve` gives
/// them (`terminal_suspend` as the host has it).
fn resolve(tui: Value, terminal_suspend: bool) -> keymap::Keybinds {
    let (s, _) = settings::load(&tui);
    keymap::resolve(&s.keybinds, terminal_suspend).0
}

fn keys(b: &keymap::Keybinds, id: &str) -> Vec<String> {
    b.get(id).iter().map(|b| b.key_text()).collect()
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn plain(code: KeyCode) -> KeyEvent {
    key(code, KeyModifiers::NONE)
}

fn app_with(tui: Value) -> App {
    let (s, errors) = settings::load(&tui);
    assert!(errors.is_empty(), "{errors:?}");
    let mut a = App::new();
    a.keymap = Arc::new(Keymap::new(&s.keybinds, s.leader_timeout, true));
    a
}

// ----- tui/test/config-v2 ---------------------------------------------------

// Mini mode lives on feature/slim-mode: an OpenCode `tui.mini` block, and
// its `app.clear` key, still load without complaint.
#[test]
fn mini_settings_are_ignored() {
    let (_, errors) = settings::load(&json!({
        "mini": { "replay": false, "replay_limit": 50, "verbosity": "quiet" },
        "keybinds": { "app.clear": "ctrl+l" },
    }));
    assert!(errors.is_empty(), "{errors:?}");
}

// (adapted) The leader key, its timeout, the cursor and tab defaults; the
// scroll, diffs, debug and session halves are settings CodeSmith does not
// have.
#[test]
fn oc_resolves_nested_config_and_keybind_defaults() {
    let (s, errors) = settings::load(&json!({
        "keybinds": { "leader": "ctrl+o" },
        "leader": { "timeout": 500 },
        "scroll": { "speed": 2, "acceleration": true },
        "diffs": { "view": "split" },
        "debug": { "devtools": true },
    }));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(s.leader_timeout, 500);
    let b = keymap::resolve(&s.keybinds, true).0;
    assert_eq!(keys(&b, "leader"), ["ctrl+o"]);
    assert_eq!(
        s.cursor,
        settings::Cursor {
            style: CursorStyle::Block,
            blinking: true
        }
    );
    let line = settings::load(&json!({ "cursor": { "style": "line" } })).0;
    assert_eq!(
        line.cursor,
        settings::Cursor {
            style: CursorStyle::Line,
            blinking: true
        }
    );
    let steady = settings::load(&json!({ "cursor": { "blinking": false } })).0;
    assert_eq!(
        steady.cursor,
        settings::Cursor {
            style: CursorStyle::Block,
            blinking: false
        }
    );
    use super::tabs::{Indicators, Mode, Orientation, Scope};
    let t = super::tabs::load(&json!({}), |_| None).0;
    assert_eq!(
        (t.mode, t.enabled, t.scope, t.layout, t.indicators),
        (
            Mode::Auto,
            true,
            Scope::Cwd,
            Orientation::Horizontal,
            Indicators::Status
        )
    );
}

#[test]
fn oc_uses_command_ids_as_keybind_keys() {
    let b = resolve(json!({ "keybinds": { "session.list": "ctrl+l" } }), true);
    assert_eq!(keys(&b, "session.list"), ["ctrl+l"]);
    assert_eq!(
        keymap::unknown_keys(&obj(json!({ "session_list": "ctrl+l" }))),
        ["session_list"]
    );
    assert!(keymap::DEFINITIONS
        .iter()
        .filter(|d| d.id != "leader")
        .all(|d| d.id.contains('.')));
}

#[test]
fn oc_preserves_current_navigation_defaults() {
    let b = resolve(Value::Null, true);
    for (id, want) in [
        // (adapted) Ctrl+O is CodeSmith's tool output key (OpenCode
        // expands output with a click; CodeSmith has no mouse), so the open
        // menu is Ctrl+X o.
        ("open.menu", "<leader>o"),
        ("session.tab.next", "ctrl+tab,alt+down"),
        ("session.tab.previous", "ctrl+shift+tab,alt+up"),
        ("session.tab.next_unread", "alt+shift+down"),
        ("session.tab.previous_unread", "alt+shift+up"),
        // (adapted) CodeSmith adds ctrl+x shift+t, which terminals
        // without the kitty keyboard protocol can send.
        ("session.tab.reopen", "ctrl+shift+t,<leader>shift+t"),
        ("session.tab.select.10", "<leader>0,ctrl+0"),
        ("prompt.images.view", "<leader>i"),
    ] {
        assert_eq!(keys(&b, id), [want], "{id}");
    }
    for id in [
        "session.message.next",
        "session.message.previous",
        "session.message.user.next",
        "session.message.user.previous",
        "input.buffer.home",
        "input.buffer.end",
    ] {
        assert!(b.get(id).is_empty(), "{id}");
    }
}

#[test]
fn oc_preserves_migrated_v1_keybind_defaults() {
    // OpenCode's first-format defaults, from `config/v1/keybind.ts`.
    for (name, command, v1_default) in [
        ("app_exit", "app.exit", json!("ctrl+c,ctrl+d,<leader>q")),
        (
            "input_paste",
            "prompt.paste",
            json!({ "key": "ctrl+v", "preventDefault": false }),
        ),
        ("prompt_queue", "prompt.queue", json!("<leader>return")),
        ("session_delete", "session.delete", json!("ctrl+d")),
        ("session_list", "session.list", json!("<leader>l")),
        ("agent_list", "agent.list", json!("<leader>a")),
    ] {
        assert_eq!(keymap::canonical(name), command);
        assert_eq!(keymap::default_json(command), v1_default, "{command}");
    }
}

#[test]
fn oc_accepts_every_v2_only_named_command_id() {
    let commands = [
        "server.pair",
        "session.toggle.exploration_grouping",
        "composer.subagent.up",
        "composer.subagent.down",
        "composer.subagent.select",
        "composer.subagent.interrupt",
        "composer.shell.up",
        "composer.shell.down",
        "composer.shell.kill",
        "diff.down",
        "diff.up",
        "diff.page.down",
        "diff.page.up",
        "diff.half_page.down",
        "diff.half_page.up",
        "diff.first",
        "diff.last",
        "diff.mark_reviewed",
        "opencode.settings",
        "service.restart",
        "session.cd",
        "app.scrap",
    ];
    let over: Map<String, Value> = commands
        .iter()
        .map(|c| (c.to_string(), json!("ctrl+alt+z")))
        .collect();
    let (s, errors) = settings::load(&json!({ "keybinds": over }));
    assert!(errors.is_empty(), "{errors:?}");
    let b = keymap::resolve(&s.keybinds, true).0;
    for c in commands {
        assert_eq!(keys(&b, c), ["ctrl+alt+z"], "{c}");
    }
}

#[test]
fn oc_centralizes_named_command_defaults_and_resolves_explicit_none() {
    let defaults = [
        ("composer.subagent.up", "up"),
        ("composer.subagent.down", "down"),
        ("composer.subagent.select", "return"),
        ("composer.subagent.interrupt", "ctrl+d"),
        ("composer.shell.up", "up"),
        ("composer.shell.down", "down"),
        ("composer.shell.kill", "ctrl+d"),
        ("diff.down", "j,down"),
        ("diff.up", "k,up"),
        ("diff.page.down", "pagedown,ctrl+f"),
        ("diff.page.up", "pageup,ctrl+b"),
        ("diff.half_page.down", "ctrl+d"),
        ("diff.half_page.up", "ctrl+u"),
        ("diff.first", "gg,home"),
        ("diff.last", "shift+g,end"),
        ("diff.next_file", "n,alt+down"),
        ("diff.previous_file", "p,alt+up"),
        ("diff.next_hunk", "]"),
        ("diff.previous_hunk", "["),
        ("diff.mark_reviewed", "m"),
        ("diff.toggle_fullscreen", "f"),
        ("diff.help", "?,shift+?,shift+/"),
    ];
    let b = resolve(Value::Null, true);
    for (id, want) in defaults {
        assert_eq!(keys(&b, id), [want], "{id}");
    }
    let none: Map<String, Value> = defaults
        .iter()
        .map(|(id, _)| (id.to_string(), json!("none")))
        .collect();
    let b = resolve(json!({ "keybinds": none }), true);
    for (id, _) in defaults {
        assert!(b.get(id).is_empty(), "{id}");
    }
}

#[test]
fn oc_retired_diff_tree_keybinds_remain_accepted_but_have_no_default_bindings() {
    let ids = [
        "diff.toggle",
        "diff.expand",
        "diff.expand_all",
        "diff.collapse",
        "diff.switch_focus",
    ];
    let over: Map<String, Value> = ids
        .iter()
        .map(|i| (i.to_string(), json!("ctrl+alt+x")))
        .collect();
    let (s, errors) = settings::load(&json!({ "keybinds": over.clone() }));
    assert!(errors.is_empty());
    assert_eq!(s.keybinds, over);
    let defaults = resolve(Value::Null, true);
    for id in ids {
        assert!(defaults.get(id).is_empty(), "{id}");
    }
}

#[test]
fn oc_rejects_orphaned_keybind_definitions() {
    let (s, _) = settings::load(&json!({ "keybinds": { "app.heap_snapshot": "ctrl+h" } }));
    assert_eq!(s.keybinds, Map::new());
}

#[test]
fn oc_uses_ctrl_z_for_input_undo_when_terminal_suspend_is_unavailable() {
    let b = resolve(Value::Null, false);
    assert!(!b.has("terminal.suspend"));
    assert_eq!(keys(&b, "input.undo"), ["ctrl+z,ctrl+-,super+z"]);
    let o = resolve(
        json!({ "keybinds": { "terminal.suspend": "ctrl+s", "input.undo": "ctrl+u" } }),
        false,
    );
    assert!(!o.has("terminal.suspend"));
    assert_eq!(keys(&o, "input.undo"), ["ctrl+u"]);
}

// ----- tui/test/config (OpenCode's first format) ----------------------------

// (adapted) The leader timeout and cursor constraints; the attention,
// prompt size, scroll speed and diff style halves are settings CodeSmith
// does not have.
#[test]
fn oc_validates_config_constraints() {
    let (s, errors) = settings::load(&json!({
        "leader_timeout": 250,
        "attention": { "volume": 1, "sounds": { "done": "done.wav" } },
        "prompt": { "max_height": 10, "max_width": "auto" },
        "scroll_speed": 0.001,
        "diff_style": "stacked",
        "cursor": { "blinking": false },
    }));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(s.leader_timeout, 250);
    assert!(!s.cursor.blinking);
    assert!(!settings::load(&json!({ "leader_timeout": 0 })).1.is_empty());
    assert!(!settings::load(&json!({ "cursor": { "style": "beam" } }))
        .1
        .is_empty());
}

// (adapted) The leader timeout and the keymap; the attention and mouse
// halves are settings CodeSmith does not have, and the cursor resolves to
// OpenCode's current default rather than staying unset.
#[test]
fn oc_resolves_host_neutral_defaults() {
    let (s, _) = settings::load(&Value::Null);
    assert_eq!(s.leader_timeout, keymap::LEADER_TIMEOUT_MS);
    let b = keymap::resolve(&s.keybinds, true).0;
    assert!(b.has("terminal.suspend"));
    assert!(b.has("session.list"));
}

// (adapted) without the attention and mouse halves.
#[test]
fn oc_resolves_overrides_without_mutating_input() {
    let input = json!({
        "theme": "custom",
        "mouse": false,
        "leader_timeout": 750,
        "keybinds": { "session_list": "ctrl+l" },
        "cursor": { "blinking": false },
    });
    let before = input.clone();
    let (s, errors) = settings::load(&input);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(s.theme.as_deref(), Some("custom"));
    assert_eq!(s.leader_timeout, 750);
    assert_eq!(
        s.cursor,
        settings::Cursor {
            style: CursorStyle::Block,
            blinking: false
        }
    );
    assert_eq!(
        keymap::resolve(&s.keybinds, true)
            .0
            .get("session.list")
            .len(),
        1
    );
    assert_eq!(input, before);
}

#[test]
fn oc_resolves_a_session_move_keybind() {
    let b = resolve(json!({ "keybinds": { "session_move": "ctrl+o" } }), true);
    assert_eq!(keys(&b, "session.move"), ["ctrl+o"]);
}

// (adapted) OpenCode's current table adds the keypad Home to
// `session.first`.
#[test]
fn oc_resolves_message_navigation_defaults() {
    let b = resolve(Value::Null, true);
    assert_eq!(keys(&b, "session.first"), ["ctrl+g,home,alt+home,kphome"]);
    for id in [
        "session.message.previous",
        "session.message.next",
        "session.message.user.previous",
        "session.message.user.next",
    ] {
        assert!(b.get(id).is_empty(), "{id}");
    }
    assert_eq!(keys(&b, "session.messages_last_user"), ["alt+end"]);
}

#[test]
fn oc_reserves_home_and_end_for_navigation() {
    let b = resolve(Value::Null, true);
    assert!(b.get("input.buffer.home").is_empty());
    assert!(b.get("input.buffer.end").is_empty());
    for (id, want) in [
        ("input.select.buffer.home", "shift+home"),
        ("input.select.buffer.end", "shift+end"),
        ("input.line.home", "ctrl+a"),
        ("input.line.end", "ctrl+e"),
        ("input.visual.line.home", "alt+a"),
        ("input.visual.line.end", "alt+e"),
    ] {
        assert_eq!(keys(&b, id), [want], "{id}");
    }
}

#[test]
fn oc_opens_the_subagent_picker_with_down() {
    assert_eq!(
        keys(&resolve(Value::Null, true), "session.child.first"),
        ["down"]
    );
}

#[test]
fn oc_navigates_session_tabs_with_option_arrows() {
    let b = resolve(Value::Null, true);
    assert_eq!(keys(&b, "session.tab.next"), ["ctrl+tab,alt+down"]);
    assert_eq!(keys(&b, "session.tab.previous"), ["ctrl+shift+tab,alt+up"]);
    assert_eq!(keys(&b, "session.tab.next_unread"), ["alt+shift+down"]);
    assert_eq!(keys(&b, "session.tab.previous_unread"), ["alt+shift+up"]);
}

#[test]
fn oc_preserves_pinned_session_bindings_alongside_tab_bindings() {
    let b = resolve(Value::Null, true);
    assert_eq!(keys(&b, "session.pin.toggle"), ["ctrl+f"]);
    assert_eq!(keys(&b, "session.quick_switch.1"), ["<leader>1"]);
    assert_eq!(keys(&b, "session.tab.select.1"), ["<leader>1,ctrl+1"]);
    assert_eq!(keys(&b, "session.tab.select.10"), ["<leader>0,ctrl+0"]);
}

#[test]
fn oc_disables_suspend_and_assigns_ctrl_z_to_undo_when_unsupported() {
    let b = resolve(Value::Null, false);
    assert!(!b.has("terminal.suspend"));
    assert_eq!(keys(&b, "input.undo"), ["ctrl+z,ctrl+-,super+z"]);
}

#[test]
fn oc_preserves_an_explicit_undo_binding_when_suspend_is_unsupported() {
    let b = resolve(
        json!({ "keybinds": { "input_undo": "ctrl+u", "terminal_suspend": "ctrl+s" } }),
        false,
    );
    assert!(!b.has("terminal.suspend"));
    assert_eq!(keys(&b, "input.undo"), ["ctrl+u"]);
}

// ----- cli/test/config ------------------------------------------------------

fn tui_of(cfg: &config::Config) -> settings::Settings {
    settings::load(&cfg.tui).0
}

// (adapted) `tui` in the global codesmith.json and in
// `$CODESMITH_CONFIG_CONTENT` merge key by key; the tabs, plugins and
// animations halves and the update half are not CodeSmith's.
#[test]
fn oc_merges_inline_cli_config_content_over_the_global_config() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("codesmith.json"),
        json!({ "tui": { "keybinds": { "app.exit": "ctrl+q" } } }).to_string(),
    )
    .unwrap();
    let sources = config::Sources {
        global_dir: tmp.path().to_path_buf(),
        file: None,
        content: Some(json!({ "tui": { "keybinds": { "help.show": false } } }).to_string()),
        project: false,
        trust: codesmith_core::trust::Mode::All,
    };
    let loaded = config::load_from(tmp.path(), &sources).unwrap();
    assert_eq!(
        Value::Object(tui_of(&loaded.config).keybinds),
        json!({ "app.exit": "ctrl+q", "help.show": false })
    );
}

// (adapted) OpenCode's first `tui.json` format, read as `tui`; CodeSmith
// never had `kv.json`, and writes no file, so those halves are dropped,
// as are the scroll, diff, attention and mouse settings it lacks.
#[test]
fn oc_migrates_tui_and_kv_config_into_cli_json() {
    let (s, errors) = settings::load(&json!({
        "theme": "legacy",
        "keybinds": {
            "leader": "ctrl+o",
            "app_exit": "ctrl+q",
            "app_heap_snapshot": "ctrl+h",
            "input_paste": { "key": "ctrl+v", "preventDefault": false },
            "session_delete": false,
            "dialog.select.next": "ctrl+n",
        },
        "leader_timeout": 500,
        "scroll_speed": 2,
        "scroll_acceleration": { "enabled": true },
        "diff_style": "stacked",
        "mouse": false,
    }));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(s.theme.as_deref(), Some("legacy"));
    assert_eq!(
        Value::Object(s.keybinds),
        json!({
            "leader": "ctrl+o",
            "app.exit": "ctrl+q",
            "prompt.paste": { "key": "ctrl+v", "preventDefault": false },
            "session.delete": false,
            "dialog.select.next": "ctrl+n",
        })
    );
    assert_eq!(s.leader_timeout, 500);
}

#[test]
fn oc_preserves_legacy_cursor_settings() {
    let (s, _) = settings::load(&json!({ "cursor": { "style": "underline", "blinking": false } }));
    assert_eq!(
        s.cursor,
        settings::Cursor {
            style: CursorStyle::Underline,
            blinking: false
        }
    );
}

/// `text` as a config file reads (JSONC).
fn tui_from_text(text: &str) -> settings::Settings {
    let v = config::parse(text, std::path::Path::new(".")).unwrap();
    settings::load(&v).0
}

// (adapted) CodeSmith reads the old names without rewriting the file, so
// its comments stay by construction.
#[test]
fn oc_migrates_legacy_keybind_names_in_an_existing_cli_json() {
    let s = tui_from_text(
        r#"{
  // Preserve this comment
  "keybinds": {
    // Session list shortcut
    "session_list": "ctrl+l",
    "app_heap_snapshot": "ctrl+h",
    // Legacy delete shortcut
    "session_delete": "ctrl+d",
    // Canonical delete shortcut
    "session.delete": "ctrl+x",
    "app.heap_snapshot": "ctrl+shift+h"
  }
}"#,
    );
    assert_eq!(
        Value::Object(s.keybinds),
        json!({ "session.list": "ctrl+l", "session.delete": "ctrl+x" })
    );
}

// (adapted) The migration is in memory: the file is never written.
#[test]
fn oc_uses_migrated_keybinds_when_persistence_fails() {
    let s = tui_from_text(r#"{"keybinds":{"session_list":"ctrl+l"}}"#);
    assert_eq!(
        Value::Object(s.keybinds),
        json!({ "session.list": "ctrl+l" })
    );
}

#[test]
fn oc_preserves_the_effective_value_when_migrating_duplicate_legacy_keybinds() {
    let s = tui_from_text(r#"{"keybinds":{"session_delete":"ctrl+a","session_delete":"ctrl+b"}}"#);
    assert_eq!(
        Value::Object(s.keybinds),
        json!({ "session.delete": "ctrl+b" })
    );
}

// (adapted) without the update half.
#[test]
fn oc_migrates_and_updates_the_effective_duplicate_top_level_keybinds() {
    let s = tui_from_text(
        r#"{"keybinds":{"session_delete":"first"},"keybinds":{"session_delete":"last"}}"#,
    );
    assert_eq!(
        Value::Object(s.keybinds),
        json!({ "session.delete": "last" })
    );
}

// (adapted) without the update half.
#[test]
fn oc_updates_effective_duplicate_canonical_keybinds() {
    let s = tui_from_text(
        r#"{"keybinds":{"session.delete":"first","session.delete":"last","opencode.settings":"off","opencode.settings":"on"}}"#,
    );
    assert_eq!(
        Value::Object(s.keybinds),
        json!({ "session.delete": "last", "opencode.settings": "on" })
    );
}

#[test]
fn oc_removes_orphaned_keybinds_without_deleting_trailing_comments() {
    let s = tui_from_text(
        r#"{
  "keybinds": {
    "app_heap_snapshot": "ctrl+h" /* Keep legacy explanation */,
    "app.heap_snapshot": "ctrl+shift+h" /* Keep canonical explanation */,
  },
}"#,
    );
    assert_eq!(s.keybinds, Map::new());
}

// ----- tui/test/keymap ------------------------------------------------------

#[test]
fn oc_legacy_page_key_aliases_compile_as_page_keys() {
    let (s, _) = settings::load(&json!({
        "keybinds": { "session.page.up": "pgup", "session.page.down": "pgdown" }
    }));
    let k = Keymap::new(&s.keybinds, 2000, true);
    assert_eq!(k.first("session.page.up").as_deref(), Some("pgup"));
    assert_eq!(k.first("session.page.down").as_deref(), Some("pgdn"));
    // And they are the page keys.
    let mut a = App::new();
    a.keymap = Arc::new(k);
    a.on_key(plain(KeyCode::PageUp));
    assert_eq!(a.scroll, 10);
}

#[test]
fn oc_formats_navigation_keys_as_arrows() {
    let k = Keymap::default();
    assert_eq!(k.first("session.parent").as_deref(), Some("↑"));
    assert_eq!(k.first("session.child.first").as_deref(), Some("↓"));
}

// (adapted) CodeSmith's commands are fixed, so a configured binding stands
// in for the plugin command's own.
#[test]
fn oc_returns_every_formatted_command_shortcut() {
    let k = Keymap::new(&obj(json!({ "help.show": "x,y" })), 2000, true);
    assert_eq!(k.list("help.show"), ["x", "y"]);
}

// ----- tui/test/cli/tui/dialog-prompt ---------------------------------------
//
// CodeSmith's prompt dialog is the question dialog: its input submits with
// `dialog.prompt.submit`, and its keys are the dialog's own.

fn question(a: &mut App) -> tokio::sync::oneshot::Receiver<super::bridge::Answers> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    a.on_event(super::bridge::Ev::Question(
        vec![codesmith_core::question::Prompt {
            question: "Rename the session?".into(),
            header: String::new(),
            options: vec![],
            multiple: false,
        }],
        tx,
    ));
    for c in "draft".chars() {
        a.on_key(plain(KeyCode::Char(c)));
    }
    rx
}

// (adapted) to the question dialog.
#[test]
fn oc_dialog_prompt_submit_wins_when_return_is_also_input_newline() {
    let mut a = app_with(json!({ "keybinds": {
        "input.submit": "super+return",
        "input.newline": "return,shift+return,alt+return,ctrl+j",
    } }));
    let mut rx = question(&mut a);
    // The text field's Enter moves to the Submit tab; the next submits.
    a.on_key(plain(KeyCode::Enter));
    a.on_key(plain(KeyCode::Enter));
    assert!(a.modal.is_none());
    assert_eq!(
        rx.try_recv().unwrap(),
        Some(vec![vec!["draft".to_string()]])
    );
}

// (adapted) The question dialog's input is one line, so this is the
// prompt's: Alt+Return makes a new line with the default bindings.
#[test]
fn oc_alt_return_inserts_a_newline_with_default_keybinds() {
    let mut a = App::new();
    for c in "draft".chars() {
        a.on_key(plain(KeyCode::Char(c)));
    }
    assert_eq!(a.on_key(key(KeyCode::Enter, KeyModifiers::ALT)), None);
    assert_eq!(a.input.text(), "draft\n");
}

// (adapted) to the question dialog.
#[test]
fn oc_dialog_prompt_submit_can_be_rebound_separately_from_input_submit() {
    let mut a = app_with(json!({ "keybinds": {
        "input.submit": "return",
        "dialog.prompt.submit": "ctrl+y",
    } }));
    let mut rx = question(&mut a);
    a.on_key(plain(KeyCode::Enter));
    assert!(a.modal.is_some());
    assert!(
        matches!(&a.modal, Some(Modal::Question { form, .. }) if form.editor.text() == "draft")
    );
    a.on_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL));
    a.on_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert!(a.modal.is_none());
    assert_eq!(
        rx.try_recv().unwrap(),
        Some(vec![vec!["draft".to_string()]])
    );
}

// (adapted) to the question dialog.
#[test]
fn oc_dialog_prompt_submit_can_be_disabled() {
    let mut a = app_with(json!({ "keybinds": {
        "input.submit": "return",
        "dialog.prompt.submit": "none",
    } }));
    let _rx = question(&mut a);
    a.on_key(plain(KeyCode::Enter));
    assert!(
        matches!(&a.modal, Some(Modal::Question { form, .. }) if form.editor.text() == "draft" && !form.confirm())
    );
}

// (adapted) The cursor shape is set when the UI starts (CodeSmith does not
// watch its config for changes, so the hot-reload half is dropped): each
// style and blinking combination resolves, and unset is a blinking block.
#[test]
fn oc_dialog_prompt_renders_block_cursor_and_survives_cursor_hot_reloads() {
    assert_eq!(
        settings::load(&Value::Null).0.cursor,
        settings::Cursor {
            style: CursorStyle::Block,
            blinking: true
        }
    );
    for (name, style) in [
        ("block", CursorStyle::Block),
        ("underline", CursorStyle::Underline),
        ("default", CursorStyle::Default),
        ("line", CursorStyle::Line),
    ] {
        for blinking in [true, false] {
            let (s, e) =
                settings::load(&json!({ "cursor": { "style": name, "blinking": blinking } }));
            assert!(e.is_empty());
            assert_eq!(s.cursor, settings::Cursor { style, blinking });
        }
    }
}

// ----- tui/test/cli/tui/reasoning-body --------------------------------------

// (adapted) Rendered, reasoning draws as Markdown (CodeSmith's table rule
// rather than OpenTUI's grid, no heading or fence markers); as source, it is the highlighted Markdown text; the
// setting is read per frame, so a change shows at once. At medium
// verbosity, where shown reasoning is not summed up in an activity row.
#[test]
fn oc_reasoning_follows_the_markdown_setting_also_when_it_changes() {
    let thought =
        "## Plan\n\n| step | risk |\n| --- | --- |\n| check | low |\n\n```ts\nconst probe = 1\n```";
    let mut a = App::new();
    a.verbosity = super::group::Verbosity::Medium;
    a.show_reasoning = true;
    a.entries.push(Entry::Reasoning(thought.into()));
    let frame = |a: &App| {
        super::view::transcript(a, 60)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    for (source, shown, hidden) in [
        (
            false,
            vec!["check │ low", "Plan", "const probe = 1"],
            vec!["## Plan", "| --- |", "```"],
        ),
        (
            true,
            vec!["## Plan", "| --- | --- |", "```ts", "const probe = 1"],
            vec!["check │"],
        ),
        (false, vec!["check │ low", "Plan"], vec!["## Plan", "```"]),
    ] {
        a.reasoning_source = source;
        let f = frame(&a);
        for s in shown {
            assert!(f.contains(s), "{source}: {s:?} in {f}");
        }
        for s in hidden {
            assert!(!f.contains(s), "{source}: {s:?} not in {f}");
        }
    }
}

// ----- CodeSmith's own wiring -------------------------------------------------

// Every key CodeSmith had goes through the keymap, so it can be rebound
// and unbound.
#[test]
fn rebinding_and_unbinding_prompt_keys() {
    let mut a = app_with(json!({ "keybinds": {
        "agent.cycle": "ctrl+g",
        "session.toggle.tool_output": "none",
        "leader": "ctrl+s",
        "model.list": "<leader>p",
    } }));
    assert_eq!(
        a.on_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        Some(Action::CycleAgent(1))
    );
    assert_eq!(a.on_key(plain(KeyCode::Tab)), None, "tab is unbound now");
    a.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(!a.expand_tools);
    assert_eq!(
        a.on_key(key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        None
    );
    assert_eq!(
        a.on_key(plain(KeyCode::Char('p'))),
        Some(Action::Slash {
            name: "model".into(),
            args: String::new()
        })
    );
    assert!(a.input.is_empty(), "the leader's key is not typed");
    // The help lists the bindings in effect.
    let help = super::app::help_text(&a.keymap);
    assert!(help.contains("ctrl+s p"), "{help}");
    assert!(help.contains("ctrl+g"), "{help}");
}

#[test]
fn the_leader_times_out() {
    let mut a = app_with(json!({ "leader": { "timeout": 1 } }));
    a.on_key(key(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert_eq!(a.pending().len(), 1);
    std::thread::sleep(std::time::Duration::from_millis(5));
    a.on_key(plain(KeyCode::Char('m')));
    assert_eq!(a.input.text(), "m", "after the timeout the key is typed");
}

#[test]
fn configured_conflicts_and_unknown_commands_are_reported() {
    let (s, errors) = settings::load(&json!({ "keybinds": {
        "agent.list": "ctrl+k",
        "model.list": "ctrl+k",
        "help.show": "ctrl+y",
        "session.list": "ctrl+y z",
        "no.such": "ctrl+q",
    } }));
    assert_eq!(errors, ["tui.keybinds: there is no command `no.such`"]);
    let k = Keymap::new(&s.keybinds, 2000, true);
    assert!(
        k.warnings
            .iter()
            .any(|w| w.contains("bound to both agent.list and model.list")),
        "{:?}",
        k.warnings
    );
    assert!(
        k.warnings.iter().any(|w| w.contains("never runs")),
        "{:?}",
        k.warnings
    );
    let bad = Keymap::new(&obj(json!({ "help.show": "ctrl+nope" })), 2000, true);
    assert!(
        bad.warnings.iter().any(|w| w.contains("help.show")),
        "{:?}",
        bad.warnings
    );
}
