//! OpenCode's tests of the subagent and shell panel (its composer:
//! `cli/tui/composer-keymap`, `util/session`, `session-subagent-navigation`)
//! and the background command output viewer (`component/dialog-shell-output`).
//! (adapted) CodeSmith has no mouse: clicks and the wheel are keys.

use super::app::{Action, App, Entry, Modal, Pick, Sub, SubStatus, Tool};
use super::keymap::Keymap;
use super::panel::{self, Shell};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::{json, Value};
use std::sync::Arc;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn frame(a: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| super::view::draw(f, a)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The panel's box only (the transcript shows the subagents too).
fn panel_text(a: &App) -> String {
    let f = frame(a, 100, 20);
    let lines: Vec<&str> = f.lines().collect();
    let top = lines
        .iter()
        .position(|l| l.contains("Subagents and shells"))
        .expect("the panel is open");
    let end = lines[top + 1..]
        .iter()
        .position(|l| l.contains('└'))
        .map_or(lines.len(), |i| top + 1 + i);
    lines[top..end].join("\n")
}

fn sub(id: &str, title: &str, status: SubStatus) -> Entry {
    let mut s = Sub::default();
    s.session = id.into();
    s.agent = "explore".into();
    s.title = title.into();
    s.status = status.clone();
    Entry::Tool(Tool {
        id: format!("call-{id}"),
        name: "subagent".into(),
        result: (status != SubStatus::Running).then(|| ("done".to_string(), false)),
        sub: Some(Box::new(s)),
        ..Default::default()
    })
}

/// A session with two running subagents and a finished one, three
/// background commands, and the panel open on `tab`.
fn composer(tab: panel::Tab, keybinds: Value) -> App {
    let (s, errors) = super::settings::load(&json!({ "keybinds": keybinds }));
    assert!(errors.is_empty(), "{errors:?}");
    let mut a = App::new();
    a.keymap = Arc::new(Keymap::new(&s.keybinds, s.leader_timeout, true));
    a.busy = true;
    a.entries = vec![
        sub("child-a", "First", SubStatus::Running),
        sub("child-b", "Second", SubStatus::Running),
        sub("child-c", "Third", SubStatus::Completed),
    ];
    a.shells = [
        ("sh-a", "bun test"),
        ("sh-b", "bun dev"),
        ("sh-c", "python3 - <<'PY'\nimport json"),
    ]
    .iter()
    .map(|(id, c)| Shell {
        id: id.to_string(),
        command: c.to_string(),
        file: format!("/nonexistent/{id}").into(),
    })
    .collect();
    a.background = 3;
    a.toggle_panel(Some(tab));
    a
}

fn open(a: &App) -> bool {
    matches!(a.modal, Some(Modal::Panel(_)))
}

// composer-keymap.test.tsx: "disabled subagent bindings have no component fallbacks"
// (adapted) a command cannot be dispatched by id here: the default
// bindings show what the disabled ones would have done.
#[test]
fn oc_disabled_subagent_bindings_have_no_component_fallbacks() {
    let none = json!({
        "composer.subagent.up": "none", "composer.subagent.down": "none",
        "composer.subagent.select": "none", "composer.subagent.interrupt": "none",
    });
    let mut a = composer(panel::Tab::Subagents, none);
    assert!(panel_text(&a).contains("First"));
    assert_eq!(a.on_key(key(KeyCode::Up)), None);
    assert_eq!(a.on_key(key(KeyCode::Enter)), None);
    assert_eq!(a.on_key(ctrl('d')), None);
    assert!(open(&a));
    let mut a = composer(panel::Tab::Subagents, json!({}));
    assert_eq!(
        a.on_key(key(KeyCode::Enter)),
        Some(Action::Picked(Pick::Session, "child-a".into()))
    );
}

// composer-keymap.test.tsx: "the inactive subagent filter only changes with its toggle"
#[test]
fn oc_the_inactive_subagent_filter_only_changes_with_its_toggle() {
    let mut a = composer(panel::Tab::Subagents, json!({}));
    let f = panel_text(&a);
    assert!(f.contains("First") && !f.contains("Third"));
    a.on_key(ctrl('a'));
    let f = panel_text(&a);
    assert!(f.contains("Third") && !f.contains("First"));
    assert_eq!(
        a.on_key(key(KeyCode::Enter)),
        Some(Action::Picked(Pick::Session, "child-c".into()))
    );
    // Back on the panel, the filter is as it was.
    a.toggle_panel(None);
    let f = panel_text(&a);
    assert!(f.contains("Third") && f.contains("show active") && !f.contains("First"));
    a.on_key(ctrl('a'));
    let f = panel_text(&a);
    assert!(f.contains("First") && !f.contains("Third"));
}

// composer-keymap.test.tsx: "disabled shell bindings have no component fallbacks"
// (adapted) as the subagent case.
#[test]
fn oc_disabled_shell_bindings_have_no_component_fallbacks() {
    let none = json!({
        "composer.shell.up": "none", "composer.shell.down": "none",
        "composer.shell.select": "none", "composer.shell.kill": "none",
    });
    let mut a = composer(panel::Tab::Shell, none);
    assert!(panel_text(&a).contains("bun test"));
    assert_eq!(a.on_key(key(KeyCode::Up)), None);
    assert_eq!(a.on_key(key(KeyCode::Enter)), None);
    assert_eq!(a.on_key(ctrl('d')), None);
    assert!(open(&a));
    let mut a = composer(panel::Tab::Shell, json!({}));
    assert_eq!(a.on_key(ctrl('d')), Some(Action::KillShell("sh-a".into())));
}

// composer-keymap.test.tsx: "composer tabs switch by mouse and keyboard" (adapted: keys)
#[test]
fn oc_composer_tabs_switch_by_keyboard() {
    let mut a = composer(panel::Tab::Subagents, json!({}));
    a.on_key(key(KeyCode::Right));
    let f = panel_text(&a);
    assert!(f.contains("bun test") && !f.contains("First"), "{f}");
    a.on_key(key(KeyCode::Left));
    let f = panel_text(&a);
    assert!(f.contains("First") && !f.contains("bun test"));
    a.on_key(key(KeyCode::Right));
    assert!(panel_text(&a).contains("bun test"));
}

// composer-keymap.test.tsx: "shell list shows one line per command"
#[test]
fn oc_shell_list_shows_one_line_per_command() {
    let a = composer(panel::Tab::Shell, json!({}));
    let f = panel_text(&a);
    assert!(f.contains("python3 - <<'PY'") && !f.contains("import json"));
}

// composer-keymap.test.tsx: "configured composer bindings work with a focused textarea"
// (adapted) the prompt holds a draft.
#[test]
fn oc_configured_composer_bindings_work_with_a_draft() {
    let mut a = composer(
        panel::Tab::Subagents,
        json!({ "composer.shell.kill": "ctrl+u" }),
    );
    a.input.set("draft");
    a.on_key(key(KeyCode::Right));
    assert!(panel_text(&a).contains("bun test"));
    assert_eq!(a.on_key(ctrl('u')), Some(Action::KillShell("sh-a".into())));
    assert_eq!(a.input.text(), "draft");
}

// composer-keymap.test.tsx: "ctrl+c closes the active composer"
#[test]
fn oc_ctrl_c_closes_the_active_composer() {
    let mut a = composer(panel::Tab::Shell, json!({}));
    a.on_key(ctrl('c'));
    assert!(a.modal.is_none());
}

// composer-keymap.test.tsx: "shell output respects a configured binding with a focused textarea"
#[test]
fn oc_shell_output_respects_a_configured_binding() {
    let mut a = composer(
        panel::Tab::Shell,
        json!({ "composer.shell.select": "ctrl+o" }),
    );
    a.on_key(key(KeyCode::Enter));
    assert!(open(&a));
    a.on_key(ctrl('o'));
    assert!(frame(&a, 100, 30).contains("Shell output"));
    let Some(Modal::ShellOutput(v)) = &a.modal else {
        panic!("no viewer")
    };
    assert_eq!(v.id, "sh-a");
}

// (not in OpenCode's test) Down on an empty prompt opens the panel when
// the session has subagents or background commands.
#[test]
fn down_opens_the_panel_when_it_has_something() {
    let mut a = App::new();
    a.on_key(key(KeyCode::Down));
    assert!(a.modal.is_none());
    a.entries.push(sub("child-a", "First", SubStatus::Running));
    a.on_key(key(KeyCode::Down));
    assert!(open(&a));
}

// util/session.test.ts: "flattens nested subagents from any session in the family"
#[test]
fn oc_flattens_nested_subagents_from_any_session_in_the_family() {
    let sessions = [
        ("root", None),
        ("child-a", Some("root")),
        ("grandchild-a", Some("child-a")),
        ("great-grandchild-a", Some("grandchild-a")),
        ("grandchild-a2", Some("child-a")),
        ("child-b", Some("root")),
        ("grandchild-b", Some("child-b")),
    ];
    let got = panel::family(&sessions, "great-grandchild-a");
    let want = [
        ("child-a", ""),
        ("grandchild-a", "├─ "),
        ("great-grandchild-a", "│  └─ "),
        ("grandchild-a2", "└─ "),
        ("child-b", ""),
        ("grandchild-b", "└─ "),
    ];
    assert_eq!(
        got,
        want.iter()
            .map(|(i, p)| (*i, p.to_string()))
            .collect::<Vec<_>>()
    );
}

// session-subagent-navigation.test.tsx: "Home and End navigate a subagent transcript"
// (adapted) a subagent's session opens as any session does; its transcript
// is the one drawn here.
#[test]
fn oc_home_and_end_navigate_a_subagent_transcript() {
    let mut a = App::new();
    a.entries = (0..20)
        .map(|i| Entry::User(format!("Message {i}")))
        .collect();
    let f = frame(&a, 100, 30);
    assert!(f.contains("Message 19") && !f.contains("Message 0\n") && !f.contains("Message 0 "));
    a.on_key(key(KeyCode::Home));
    let f = frame(&a, 100, 30);
    assert!(f.contains("Message 0 ") && !f.contains("Message 19"), "{f}");
    a.on_key(key(KeyCode::End));
    let f = frame(&a, 100, 30);
    assert!(f.contains("Message 19") && !f.contains("Message 0 "), "{f}");
}

// ----- component/dialog-shell-output -------------------------------------------

struct V {
    app: App,
    dir: tempfile::TempDir,
}

fn file(v: &V) -> std::path::PathBuf {
    v.dir.path().join("capture.out")
}

fn viewer(output: &str) -> V {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.out");
    std::fs::write(&path, output).unwrap();
    let mut app = App::new();
    app.shells = vec![Shell {
        id: "sh_fixture".into(),
        command: "render-scene --quality high".into(),
        file: path,
    }];
    app.toggle_panel(Some(panel::Tab::Shell));
    V { app, dir }
}

impl V {
    fn view(&mut self) -> &mut panel::ShellView {
        match &mut self.app.modal {
            Some(Modal::ShellOutput(v)) => v,
            _ => panic!("no viewer"),
        }
    }
    fn append(&mut self, s: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(file(self))
            .unwrap();
        f.write_all(s.as_bytes()).unwrap();
        self.view().refresh();
    }
}

fn frames(n: usize) -> String {
    (1..=n).map(|i| format!("Frame {i}\n")).collect()
}

// dialog-shell-output.test.tsx: "shell output opens, follows, scrolls, and survives exit at %s columns" (2 cases)
#[test]
fn oc_shell_output_opens_follows_scrolls_and_survives_exit() {
    for width in [40, 100] {
        let mut v = viewer(&frames(50));
        assert!(frame(&v.app, width, 30).contains("output"));
        v.app.on_key(key(KeyCode::Enter));
        let f = frame(&v.app, width, 30);
        assert!(f.contains("Shell output") && f.contains("Frame 50"), "{f}");
        v.app.on_key(key(KeyCode::Home));
        let f = frame(&v.app, width, 30);
        assert!(f.contains("Frame 1 ") && !f.contains("Frame 50"), "{f}");
        v.append("Frame 51\n");
        assert_eq!(v.view().top, Some(0));
        v.app.on_key(key(KeyCode::End));
        assert!(frame(&v.app, width, 30).contains("Frame 51"));
        v.view().status = "Exited · code 0".into();
        assert!(frame(&v.app, width, 30).contains("code 0"));
        // The capture's last flush can come after the command exited.
        v.append("\u{1b}[32mRender complete\u{1b}[0m\r\n");
        let f = frame(&v.app, width, 30);
        assert!(f.contains("Render complete") && f.contains("code 0"));
        assert!(!f.contains("[32m"));
        v.app.shells.clear();
        v.app.on_key(key(KeyCode::Esc));
        let f = frame(&v.app, width, 30);
        assert!(
            !f.contains("Shell output") && f.contains("No shell commands"),
            "{f}"
        );
    }
}

// dialog-shell-output.test.tsx: "empty output explains redirection, retries errors, and preserves output after removal"
// (adapted) a directory in the file's place is the read that fails.
#[test]
fn oc_empty_output_explains_redirection_retries_errors_and_preserves_output_after_removal() {
    let mut v = viewer("");
    v.app.on_key(key(KeyCode::Enter));
    let f = frame(&v.app, 100, 30);
    assert!(
        f.contains("No captured output") && f.contains("redirected"),
        "{f}"
    );
    std::fs::remove_file(file(&v)).unwrap();
    std::fs::create_dir(file(&v)).unwrap();
    v.view().refresh();
    assert!(frame(&v.app, 100, 30).contains("Retrying"));
    std::fs::remove_dir(file(&v)).unwrap();
    std::fs::write(file(&v), "Recovered output\n").unwrap();
    v.view().refresh();
    let f = frame(&v.app, 100, 30);
    assert!(
        f.contains("Recovered output") && !f.contains("Retrying"),
        "{f}"
    );
    std::fs::remove_file(file(&v)).unwrap();
    v.view().refresh();
    let f = frame(&v.app, 100, 30);
    assert!(f.contains("no longer available") && f.contains("Recovered output"));
    // No more reads once it is gone.
    std::fs::write(file(&v), "back again\n").unwrap();
    assert!(!v.view().refresh());
}

// dialog-shell-output.test.tsx: "mouse-wheel scrolling pauses and resumes output following at %s columns" (2 cases)
// (adapted) ↑ and ↓ in place of the wheel.
#[test]
fn oc_scrolling_pauses_and_resumes_output_following() {
    for width in [40, 100] {
        let mut v = viewer(&frames(50));
        v.app.on_key(key(KeyCode::Enter));
        assert!(frame(&v.app, width, 30).contains("Frame 50"));
        v.app.on_key(key(KeyCode::Up));
        let paused = v.view().top;
        assert!(paused.is_some());
        v.append("Frame 51\n");
        assert_eq!(v.view().top, paused);
        assert!(frame(&v.app, width, 30).contains("Shell output"));
        v.app.on_key(key(KeyCode::Down));
        v.app.on_key(key(KeyCode::Down));
        assert_eq!(v.view().top, None, "back at the end, it follows");
        v.append("Frame 52\n");
        assert!(frame(&v.app, width, 30).contains("Frame 52"));
    }
}

// dialog-shell-output.test.tsx: "large captures open at a bounded tail and clicking a shell opens the viewer"
// (adapted) Enter on the shell's row.
#[test]
fn oc_large_captures_open_at_a_bounded_tail() {
    let mut v = viewer(&("old output\n".repeat(20000) + "Latest frame\n"));
    v.app.on_key(key(KeyCode::Enter));
    let f = frame(&v.app, 100, 30);
    assert!(
        f.contains("Latest frame") && f.contains("Earlier output omitted"),
        "{f}"
    );
    assert!(v.view().text.len() <= panel::PAGE_BYTES);
}

// dialog-shell-output.test.tsx: "new designs show footer actions instead of the legacy hint line"
#[test]
fn oc_the_viewer_shows_footer_actions() {
    let mut v = viewer("Frame 1\n");
    v.app.on_key(key(KeyCode::Enter));
    let f = frame(&v.app, 100, 30);
    assert!(f.contains("Shell output") && f.contains("Frame 1"));
    assert!(
        f.contains("follow end") && f.contains("copy command c") && f.contains("copy output o")
    );
    assert!(!f.contains("back esc"));
    v.app.on_key(key(KeyCode::Char('c')));
    assert_eq!(
        v.app.clipboard.as_deref(),
        Some("render-scene --quality high")
    );
    assert!(frame(&v.app, 100, 30).contains("✓ copied"));
}

// dialog-shell-output.test.tsx: "the retained 64 KiB tail starts on a whole character"
#[test]
fn oc_the_retained_64_kib_tail_starts_on_a_whole_character() {
    let mut v = viewer("😀");
    v.app.on_key(key(KeyCode::Enter));
    assert!(frame(&v.app, 100, 30).contains("😀"));
    let rest = format!(
        "{}{}",
        ("x".repeat(1023) + "\n").repeat(63),
        "x".repeat(1023)
    );
    v.append(&rest);
    assert!(frame(&v.app, 100, 30).contains("Earlier output omitted"));
    v.app.on_key(key(KeyCode::Char('o')));
    assert_eq!(v.app.clipboard.as_deref(), Some(rest.as_str()));
}

// (not in OpenCode's test) A read that ends inside a character keeps it
// for the next one; control characters are shown escaped.
#[test]
fn split_characters_and_controls_in_output() {
    let mut v = viewer("");
    v.app.on_key(key(KeyCode::Enter));
    let smile = "😀".as_bytes();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(file(&v))
            .unwrap();
        f.write_all(&smile[..2]).unwrap();
    }
    v.view().refresh();
    assert_eq!(v.view().text, "");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(file(&v))
            .unwrap();
        f.write_all(&smile[2..]).unwrap();
        f.write_all(b"\x07bell\n").unwrap();
    }
    v.view().refresh();
    assert_eq!(v.view().text, "😀\x07bell\n");
    let f = frame(&v.app, 100, 30);
    assert!(f.contains("\\x07bell"), "{f}");
}

// ----- cli/tui/dialog-mcp -----------------------------------------------------

fn mcp(status: super::mcp_view::Status) -> App {
    let mut a = App::new();
    a.modal = Some(Modal::Mcp(super::mcp_view::McpView::new(vec![
        super::mcp_view::Server {
            name: "linear".into(),
            status,
        },
    ])));
    a
}

fn mcp_frame(a: &App) -> String {
    frame(a, 100, 24)
}

// dialog-mcp.test.tsx: "starts OAuth with %s for an MCP server requiring authentication" (2 cases)
#[test]
fn oc_starts_oauth_for_an_mcp_server_requiring_authentication() {
    for k in [KeyCode::Enter, KeyCode::Char(' ')] {
        let mut a = mcp(super::mcp_view::Status::NeedsAuth);
        let f = mcp_frame(&a);
        assert!(
            f.contains("Sign in required") && !f.contains("view error"),
            "{f}"
        );
        assert_eq!(a.on_key(key(k)), Some(Action::McpSignIn("linear".into())));
        assert!(mcp_frame(&a).contains("Waiting for authorization"));
    }
}

// dialog-mcp.test.tsx: "returns to the MCP server list after OAuth completes"
#[test]
fn oc_returns_to_the_mcp_server_list_after_oauth_completes() {
    let mut a = mcp(super::mcp_view::Status::NeedsAuth);
    a.on_key(key(KeyCode::Enter));
    assert!(mcp_frame(&a).contains("Waiting for authorization"));
    a.on_event(super::bridge::Ev::McpSignedIn {
        server: "linear".into(),
        result: Ok(()),
    });
    let f = mcp_frame(&a);
    assert!(
        f.contains("MCP servers") && !f.contains("Waiting for authorization"),
        "{f}"
    );
}

// dialog-mcp.test.tsx: "opens an investigation draft for a failed MCP server at its originating location"
// (adapted) one location: the draft goes to a new session in the UI's folder.
#[test]
fn oc_opens_an_investigation_draft_for_a_failed_mcp_server() {
    let mut a = mcp(super::mcp_view::Status::Failed(
        "MCP error -32000: Connection closed".into(),
    ));
    let f = mcp_frame(&a);
    assert!(
        f.contains("Failed !") && f.contains("view error enter"),
        "{f}"
    );
    a.on_key(key(KeyCode::Enter));
    assert!(mcp_frame(&a).contains("investigate i"));
    let Some(Action::Investigate(prompt)) = a.on_key(key(KeyCode::Char('i'))) else {
        panic!("no investigation")
    };
    assert!(a.modal.is_none());
    for s in [
        "OpenCode component failed in the current project",
        "MCP server: linear",
        "Status: failed",
        "Configuration: mcp.servers.linear",
        "MCP error -32000: Connection closed",
        "project and global OpenCode configuration",
    ] {
        assert!(prompt.contains(s), "{s}");
    }
}
