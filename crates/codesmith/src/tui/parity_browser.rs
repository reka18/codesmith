//! OpenCode's project, worktree and session browser tests, ported:
//! `tui/test/cli/tui/dialog-open` (the open menu), `cli/tui/prompt-move`
//! (the worktrees dialog), `cli/tui/dialog-session-list` and the
//! `session.new_location` setting of `config-v2`.
//!
//! OpenCode drives the dialogs through a mock server; here the dialog is
//! driven directly, and what it asks the runner for ([`Request`]) stands
//! for the server requests. OpenCode's projects come from their own read;
//! CodeSmith's are the Git projects of the sessions listed and the UI's
//! own, so where a test filters to a project its sessions match too, and
//! ⌥↓ moves past them first (marked adapted).

use super::app::App;
use super::browse_host::{self, Plan, Relaunch};
use super::browser::{
    project_of, Browser, Context, Outcome, Request, Response, SessionRow, Target, NO_PROJECT,
};
use super::keymap::Keymap;
use super::settings;
use super::tabs::Attention;
use codesmith_core::store::{Info, Store};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use serde_json::json;
use std::path::{Path, PathBuf};

const NOW: i64 = 1_700_000_000_000;

fn km() -> Keymap {
    let (s, errors) = settings::load(&json!({}));
    assert!(errors.is_empty(), "{errors:?}");
    Keymap::new(&s.keybinds, s.leader_timeout, true)
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn ctx(cwd: &str, project: Option<&str>) -> Context {
    Context {
        cwd: p(cwd),
        project: project.map(p),
        ..Default::default()
    }
}

fn session(id: &str, title: &str, dir: &str, root: Option<&str>, updated: i64) -> SessionRow {
    SessionRow {
        id: id.into(),
        title: Some(title.into()),
        dir: p(dir),
        root: root.map(p),
        updated,
        parent: None,
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn press(b: &mut Browser, code: KeyCode) -> Outcome {
    b.key(&key(code, KeyModifiers::NONE), &km())
}

fn ctrl(b: &mut Browser, c: char) -> Outcome {
    if c == 'c' {
        return b.interrupt();
    }
    b.key(&key(KeyCode::Char(c), KeyModifiers::CONTROL), &km())
}

fn alt(b: &mut Browser, code: KeyCode) -> Outcome {
    b.key(&key(code, KeyModifiers::ALT), &km())
}

fn type_text(b: &mut Browser, text: &str) {
    for c in text.chars() {
        assert_eq!(press(b, KeyCode::Char(c)), Outcome::Nothing);
    }
}

/// The dialog as drawn: its title, then its lines.
fn frame_rows(b: &Browser, rows: usize) -> String {
    let mut out = b.title();
    for l in b.lines(96, rows, &km(), NOW) {
        out.push('\n');
        out.extend(l.spans.iter().map(|s| s.content.as_ref()));
    }
    out
}

fn frame(b: &Browser) -> String {
    frame_rows(b, 22)
}

fn worktree_requests(b: &mut Browser) -> Vec<PathBuf> {
    b.take_requests()
        .into_iter()
        .filter_map(|r| match r {
            Request::Worktrees(root) => Some(root),
            _ => None,
        })
        .collect()
}

/// The open menu, its recent list read.
fn open_with(c: Context, list: Vec<SessionRow>) -> Browser {
    let mut b = Browser::open(c, Vec::new());
    b.take_requests();
    assert_eq!(b.answer(Response::Recent(Ok(list))), Outcome::Nothing);
    b
}

fn open_session(id: &str, dir: &str) -> Outcome {
    Outcome::Open(Target::Session {
        id: id.into(),
        dir: p(dir),
    })
}

fn open_dir(dir: &str) -> Outcome {
    Outcome::Open(Target::Directory(p(dir)))
}

fn selected_title(b: &Browser) -> String {
    let rows = b.rows(96, NOW);
    b.selected(&rows)
        .map(|r| r.title.clone())
        .unwrap_or_default()
}

// ----- tui/test/cli/tui/dialog-open ----------------------------------------

#[test]
fn oc_selecting_an_unhydrated_session_preserves_its_location() {
    let remote = tempfile::tempdir().unwrap();
    let dir = remote.path().display().to_string();
    let mut b = open_with(
        ctx("/tmp/opencode/launch", None),
        vec![session("ses_remote", "Remote session", &dir, None, 2)],
    );
    assert!(frame(&b).contains("Remote session"));
    let out = press(&mut b, KeyCode::Enter);
    assert_eq!(out, open_session("ses_remote", &dir));
    // Elsewhere, the UI starts again in the session's folder.
    let Outcome::Open(t) = out else {
        unreachable!()
    };
    assert_eq!(
        browse_host::plan(&t, Path::new("/tmp/opencode/launch")),
        Plan::Relaunch(Relaunch {
            cwd: p(&dir),
            resume: Some("ses_remote".into()),
        })
    );
}

#[test]
fn oc_finds_and_opens_an_exact_child_session_id_outside_the_recent_list() {
    let id = "ses_04a7a3d82ffeIphUJgd3SnEqiv";
    let mut b = open_with(ctx("/tmp/opencode/launch", None), Vec::new());
    assert!(frame(&b).contains("Search sessions and projects"));
    type_text(&mut b, id);
    assert_eq!(b.take_requests(), vec![Request::Lookup(id.into())]);
    let mut child = session(
        id,
        "TUI plugin slot API v2",
        "/tmp/opencode/archive",
        None,
        2,
    );
    child.parent = Some("ses_04a7a3d82ffeParentSessionID".into());
    b.answer(Response::Lookup(id.into(), Some(child)));
    assert!(frame(&b).contains("TUI plugin slot API v2"));
    assert_eq!(
        press(&mut b, KeyCode::Enter),
        open_session(id, "/tmp/opencode/archive")
    );
}

#[test]
fn oc_shows_the_current_project_and_opens_its_root() {
    let root = "/tmp/opencode/OpenCode";
    let mut b = open_with(
        ctx("/tmp/opencode/OpenCode/packages/tui", Some(root)),
        Vec::new(),
    );
    let f = frame(&b);
    assert!(f.contains("OpenCode") && f.contains('●'), "{f}");
    assert!(f.contains(root));
    assert_eq!(press(&mut b, KeyCode::Enter), open_dir(root));
}

#[test]
fn oc_shows_only_canonical_projects_while_retaining_sessions_from_other_directories() {
    let root = "/tmp/opencode/OpenCode";
    let mut b = open_with(
        ctx(root, Some(root)),
        vec![
            session(
                "ses_global",
                "Standalone session",
                "/tmp/standalone-notes",
                None,
                3,
            ),
            session(
                "ses_worktree",
                "Worktree session",
                "/tmp/opencode/feature-branch",
                Some(root),
                2,
            ),
        ],
    );
    let f = frame(&b);
    assert!(
        f.contains("Standalone session") && f.contains("Projects"),
        "{f}"
    );
    assert!(f.contains("standalone-notes"));
    assert!(f.contains("OpenCode · feature-branch"));
    assert!(f.contains(root));
    assert!(!f.contains("/tmp/opencode/feature-branch"));
    assert!(!f.contains("/tmp/standalone-notes"));
    type_text(&mut b, "standalone-notes");
    assert!(frame(&b).contains("standalone-notes"));
    assert_eq!(
        press(&mut b, KeyCode::Enter),
        open_session("ses_global", "/tmp/standalone-notes")
    );
}

#[test]
fn oc_keeps_nested_git_session_directories_out_of_projects_and_in_their_session_footer() {
    let root = "/tmp/opencode/OpenCode";
    let b = open_with(
        ctx(root, Some(root)),
        vec![session(
            "ses_dashboard",
            "Improve dashboard",
            "/tmp/opencode/OpenCode/packages/dashboard",
            Some(root),
            2,
        )],
    );
    let f = frame(&b);
    assert!(f.contains("Improve dashboard") && f.contains("Projects"));
    assert!(f.contains("OpenCode · dashboard"));
    assert!(!f.contains("browse directories"));
    assert!(f.contains(root));
    assert!(!f.contains("/tmp/opencode/OpenCode/packages/dashboard"));
}

#[test]
fn oc_loads_git_worktrees_only_when_drilling_into_the_canonical_project() {
    let root = "/tmp/opencode/OpenCode";
    let current = "/tmp/opencode/current-branch";
    let other = "/tmp/opencode/other-branch";
    let mut b = open_with(ctx(current, Some(root)), Vec::new());
    let projects = frame(&b);
    assert!(projects.contains("OpenCode") && projects.contains(root) && projects.contains('→'));
    assert!(!projects.contains("Browse directories"));
    assert!(!projects.contains(current));
    assert!(worktree_requests(&mut b).is_empty());

    press(&mut b, KeyCode::Right);
    assert_eq!(worktree_requests(&mut b), [p(root)]);
    b.answer(Response::Worktrees(
        p(root),
        Ok(vec![p(other), p(root), p(current)]),
    ));
    let w = frame(&b);
    assert!(w.contains("other-branch") && w.contains("ctrl+n"), "{w}");
    assert!(w.contains("Worktrees"));
    assert!(w.contains('●'));
    assert!(w.find("OpenCode") < w.find("current-branch"));
    assert!(w.find("current-branch") < w.find("other-branch"));

    type_text(&mut b, "other-branch");
    assert_eq!(press(&mut b, KeyCode::Enter), open_dir(other));
}

#[test]
fn oc_does_not_show_or_trigger_worktree_navigation_for_non_git_and_global_directories() {
    let mut b = open_with(
        ctx("/tmp/plain-project", None),
        vec![session(
            "ses_global",
            "Standalone session",
            "/tmp/standalone-notes",
            None,
            2,
        )],
    );
    let f = frame(&b);
    assert!(f.contains("plain-project") && f.contains("standalone-notes"));
    assert!(!f.contains('→'), "{f}");
    for code in [KeyCode::Down, KeyCode::Right, KeyCode::Down, KeyCode::Right] {
        press(&mut b, code);
    }
    assert!(frame(&b).contains("Search sessions and projects"));
    assert!(worktree_requests(&mut b).is_empty());
}

#[test]
fn oc_does_not_show_the_previous_projects_worktrees_while_loading_another_project() {
    let (alpha, beta) = ("/tmp/opencode/Alpha", "/tmp/opencode/Beta");
    let mut b = open_with(
        ctx("/tmp/opencode/elsewhere", None),
        vec![
            session("ses_a", "Work", alpha, Some(alpha), 2),
            session("ses_b", "Work", beta, Some(beta), 2),
        ],
    );
    let f = frame(&b);
    assert!(f.contains("Alpha") && f.contains("Beta"));
    type_text(&mut b, "Alpha");
    // (adapted) past the project's session.
    alt(&mut b, KeyCode::Down);
    press(&mut b, KeyCode::Right);
    assert_eq!(worktree_requests(&mut b), [p(alpha)]);
    b.answer(Response::Worktrees(
        p(alpha),
        Ok(vec![p(alpha), p("/tmp/opencode/alpha-checkout")]),
    ));
    assert!(frame(&b).contains("alpha-checkout"));
    press(&mut b, KeyCode::Esc);
    ctrl(&mut b, 'c');
    type_text(&mut b, "Beta");
    alt(&mut b, KeyCode::Down);
    press(&mut b, KeyCode::Right);
    assert_eq!(worktree_requests(&mut b), [p(beta)]);
    let loading = frame(&b);
    assert!(
        loading.contains("Beta / Worktrees") && loading.contains("Loading worktrees"),
        "{loading}"
    );
    assert!(!loading.contains("alpha-checkout"));
    b.answer(Response::Worktrees(
        p(beta),
        Ok(vec![p(beta), p("/tmp/opencode/beta-checkout")]),
    ));
    let loaded = frame(&b);
    assert!(loaded.contains("beta-checkout"));
    assert!(!loaded.contains("alpha-checkout"));
    press(&mut b, KeyCode::Down);
    assert_eq!(
        press(&mut b, KeyCode::Enter),
        open_dir("/tmp/opencode/beta-checkout")
    );
}

#[test]
fn oc_creates_a_worktree_named_by_project_and_opens_its_local_directory() {
    // (adapted) OpenCode also selects a word of the prompt's description
    // with the mouse before Escape; CodeSmith has no mouse.
    for name in ["", "search-ui"] {
        let root = "/tmp/opencode/OpenCode";
        let created = "/tmp/opencode/created-branch";
        let mut b = open_with(ctx(root, Some(root)), Vec::new());
        let f = frame(&b);
        assert!(f.contains("OpenCode") && f.contains('→'));
        press(&mut b, KeyCode::Right);
        assert_eq!(worktree_requests(&mut b), [p(root)]);
        b.answer(Response::Worktrees(p(root), Ok(vec![p(root)])));
        let w = frame(&b);
        assert!(w.contains("ctrl+n"));
        assert!(!w.contains("Actions"));
        assert!(!w.contains("+ New worktree"));
        type_text(&mut b, "unmatched-search");
        let filtered = frame(&b);
        assert!(filtered.contains("unmatched-search"));
        assert!(filtered.contains("ctrl+n"));
        assert!(filtered.contains("No matching worktrees"), "{filtered}");
        ctrl(&mut b, 'n');
        assert!(frame(&b).contains("Worktree name (optional)"));
        press(&mut b, KeyCode::Esc);
        let restored = frame(&b);
        assert!(restored.contains("unmatched-search") && restored.contains("ctrl+n"));
        assert!(b.take_requests().is_empty());
        ctrl(&mut b, 'n');
        assert!(frame(&b).contains("Worktree name (optional)"));
        type_text(&mut b, name);
        press(&mut b, KeyCode::Enter);
        assert_eq!(
            b.take_requests(),
            vec![Request::Create {
                root: p(root),
                name: (!name.is_empty()).then(|| name.to_string()),
            }]
        );
        assert_eq!(
            b.answer(Response::Created(p(root), Ok(p(created)))),
            open_dir(created),
            "{name}"
        );
    }
}

#[test]
fn oc_shows_projects_while_sessions_refresh_and_preserves_the_selected_project() {
    let (first, second) = (
        "/tmp/opencode/First project",
        "/tmp/opencode/Second project",
    );
    let known = vec![
        session("ses_1", "Earlier", first, Some(first), 2),
        session("ses_2", "Earlier", second, Some(second), 1),
    ];
    let mut b = Browser::open(ctx("/tmp/opencode/elsewhere", None), known);
    let f = frame(&b);
    assert!(
        f.contains("Second project") && f.contains("Refreshing"),
        "{f}"
    );
    assert!(f.contains("Search sessions and projects"));
    // (adapted) past the sessions, to the second project.
    alt(&mut b, KeyCode::Down);
    press(&mut b, KeyCode::Down);
    b.answer(Response::Recent(Ok(vec![
        session("ses_recent", "Recent session", first, Some(first), 3),
        session("ses_2", "Earlier", second, Some(second), 1),
    ])));
    let f = frame(&b);
    assert!(f.contains("Recent session") && f.contains("Second project"));
    assert_eq!(press(&mut b, KeyCode::Enter), open_dir(second));
}

#[test]
fn oc_keeps_a_filtered_selection_visible_after_refresh_with_query_reset() {
    for reset in [false, true] {
        let (first, second) = (
            "/tmp/opencode/First shared project",
            "/tmp/opencode/Second shared project",
        );
        let known = vec![
            session("ses_1", "Earlier", first, Some(first), 2),
            session("ses_2", "Earlier", second, Some(second), 1),
        ];
        let mut b = Browser::open(ctx("/fixture", None), known.clone());
        assert!(frame(&b).contains("Refreshing"));
        type_text(&mut b, "shared");
        alt(&mut b, KeyCode::Down);
        assert_eq!(selected_title(&b), "First shared project");
        press(&mut b, KeyCode::Down);
        assert_eq!(selected_title(&b), "Second shared project");
        let mut list: Vec<SessionRow> = (0..12)
            .map(|i| {
                session(
                    &format!("ses_shared_{i}"),
                    "shared",
                    "/fixture",
                    None,
                    i + 3,
                )
            })
            .collect();
        list.extend(known);
        b.answer(Response::Recent(Ok(list)));
        // A list taller than the dialog: the selected row stays in view.
        let f = frame_rows(&b, 8);
        assert!(f.contains("Open") && !f.contains("Refreshing"));
        assert!(f.contains("Second shared project"), "{f}");
        assert_eq!(selected_title(&b), "Second shared project");
        let want = if reset {
            type_text(&mut b, " project");
            // (adapted) the first match is the top row again: here the
            // first project's session, which matches its name too.
            let rows = b.rows(96, NOW);
            assert_eq!(b.selected(&rows), rows.first());
            assert_ne!(selected_title(&b), "Second shared project");
            Outcome::Open(rows[0].target.clone())
        } else {
            open_dir(second)
        };
        assert_eq!(press(&mut b, KeyCode::Enter), want, "reset {reset}");
    }
}

fn recent() -> SessionRow {
    session("ses_recent", "Recent session", "/fixture", None, 2)
}

#[test]
fn oc_sessions_remain_selectable_while_projects_are_still_loading() {
    // (adapted) the projects come with the sessions, in one read.
    let mut b = open_with(ctx("/tmp/elsewhere", None), vec![recent()]);
    assert!(frame(&b).contains("Recent session"));
    assert_eq!(
        press(&mut b, KeyCode::Enter),
        open_session("ses_recent", "/fixture")
    );
}

#[test]
fn oc_shows_hydrated_sessions_immediately_without_waiting_for_either_read() {
    let mut b = Browser::open(ctx("/tmp/elsewhere", None), vec![recent()]);
    let f = frame(&b);
    assert!(f.contains("Recent session") && f.contains("Refreshing"));
    assert_eq!(
        press(&mut b, KeyCode::Enter),
        open_session("ses_recent", "/fixture")
    );
}

#[test]
fn oc_keeps_the_previous_recent_list_usable_when_reopening_fails_to_refresh() {
    let mut app = App::new();
    app.browser = Some(Box::new(open_with(
        ctx("/tmp/elsewhere", None),
        vec![recent()],
    )));
    assert!(!frame(app.browser.as_ref().unwrap()).contains("Refreshing"));
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.browser.is_none());
    // Opened again: the last list at once, then a failed refresh.
    let mut b = Browser::open(ctx("/tmp/elsewhere", None), app.browser_known.clone());
    b.answer(Response::Recent(Err("Unavailable".into())));
    let f = frame(&b);
    assert!(f.contains("Could not refresh sessions"), "{f}");
    assert!(f.contains("Recent session"));
    assert_eq!(
        press(&mut b, KeyCode::Enter),
        open_session("ses_recent", "/fixture")
    );
}

#[test]
fn oc_shows_an_initial_loading_shell_instead_of_reporting_an_empty_list() {
    let mut b = Browser::open(ctx("/fixture", None), Vec::new());
    let f = frame(&b);
    assert!(f.contains("Search sessions and projects") && f.contains("Refreshing"));
    assert!(!f.contains("No items available"));
    type_text(&mut b, "missing");
    let f = frame(&b);
    assert!(f.contains("Searching sessions and projects"), "{f}");
    assert!(!f.contains("No matches"));
    b.answer(Response::Recent(Ok(Vec::new())));
    let f = frame(&b);
    assert!(f.contains("No matches") && !f.contains("Refreshing"), "{f}");
}

#[test]
fn oc_reports_both_refresh_failures_while_keeping_hydrated_sessions_usable() {
    let mut b = Browser::open(ctx("/tmp/elsewhere", None), vec![recent()]);
    b.answer(Response::Recent(Err("Unavailable".into())));
    let f = frame(&b);
    assert!(f.contains("Could not refresh sessions and projects"));
    assert!(f.contains("Recent session"));
}

#[test]
fn oc_option_arrows_jump_between_sections() {
    let dir = "/tmp/opencode/recent";
    let list = || vec![session("ses_recent", "Recent session", dir, Some(dir), 2)];
    for code in [KeyCode::Down, KeyCode::Up] {
        let mut b = open_with(ctx("/tmp/elsewhere", None), list());
        let f = frame(&b);
        assert!(f.contains("Recent session") && f.contains("recent"));
        alt(&mut b, code);
        assert_eq!(press(&mut b, KeyCode::Enter), open_dir(dir), "{code:?}");
    }
}

#[test]
fn oc_option_arrows_stay_in_the_only_visible_section() {
    let mut b = open_with(
        ctx("/tmp/Effect", None),
        vec![session(
            "ses_o",
            "Fix a bug",
            "/tmp/opencode",
            Some("/tmp/opencode"),
            2,
        )],
    );
    let f = frame(&b);
    assert!(f.contains("Effect") && f.contains("opencode"));
    type_text(&mut b, "Effect");
    let f = frame(&b);
    assert!(f.contains("Effect") && !f.contains("opencode"), "{f}");
    alt(&mut b, KeyCode::Down);
    assert_eq!(press(&mut b, KeyCode::Enter), open_dir("/tmp/Effect"));
}

#[test]
fn oc_surfaces_sessions_awaiting_permissions_or_questions_at_the_top_with_attention_icons() {
    // (adapted) subagents run inside their parent's turn, so the parent
    // is the session asking.
    let remote = "/tmp/opencode/remote";
    let mut c = ctx("/tmp/elsewhere", None);
    c.attention
        .insert("ses_parent_question".into(), Attention::Question);
    c.attention
        .insert("ses_blocked_permission".into(), Attention::Permission);
    let mut b = Browser::open(c, Vec::new());
    let Request::Recent { include } = &b.take_requests()[0] else {
        panic!("no recent request")
    };
    // However old, the sessions waiting are read.
    assert_eq!(include, &["ses_blocked_permission", "ses_parent_question"]);
    let mut list: Vec<SessionRow> = (0..9)
        .map(|i| {
            session(
                &format!("ses_recent_{}", i + 1),
                &format!("Recent session {}", i + 1),
                "/fixture",
                None,
                100 - i,
            )
        })
        .collect();
    list.push(session(
        "ses_parent_question",
        "Parent awaiting question",
        remote,
        None,
        5,
    ));
    list.push(session(
        "ses_blocked_permission",
        "Old session awaiting permission",
        "/fixture",
        None,
        1,
    ));
    b.answer(Response::Recent(Ok(list)));
    let f = frame(&b);
    let (q, perm, first) = (
        f.find("? Parent awaiting question"),
        f.find("! Old session awaiting permission"),
        f.find("Recent session 1"),
    );
    assert!(q.is_some() && perm.is_some() && first.is_some(), "{f}");
    assert!(q < perm && perm < first);
    assert_eq!(
        press(&mut b, KeyCode::Enter),
        open_session("ses_parent_question", remote)
    );
}

// ----- tui/test/cli/tui/prompt-move ----------------------------------------

const MAIN: &str = "/tmp/opencode/main";
const CLONE: &str = "/tmp/opencode/other-clone";
const LINKED: &str = "/tmp/opencode/linked";
const CREATED: &str = "/tmp/opencode/main-fresh";

/// The worktrees dialog in `cwd` (of the project `main`), with the
/// session `ses_clone` unless on a new one (OpenCode's Home).
fn worktrees(cwd: &str, home: bool) -> Browser {
    let mut c = ctx(cwd, Some(MAIN));
    c.session = (!home).then(|| "ses_clone".to_string());
    let mut b = Browser::worktrees(c).expect("a Git project");
    assert_eq!(worktree_requests(&mut b), [p(MAIN)]);
    b.answer(Response::Worktrees(
        p(MAIN),
        Ok(vec![p(MAIN), p(CLONE), p(LINKED)]),
    ));
    b
}

/// Ctrl+A, `fresh`, Enter: the create request sent.
fn create_fresh(b: &mut Browser) -> Vec<Request> {
    let f = frame(b);
    assert!(f.contains("Worktrees"), "{f}");
    ctrl(b, 'a');
    assert!(frame(b).contains("Name worktree"));
    type_text(b, "fresh");
    press(b, KeyCode::Enter);
    b.take_requests()
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[test]
fn oc_passes_the_current_project_and_uses_server_worktree_defaults() {
    // (adapted) the project is the one the UI's folder is in, found on
    // disk: a clone is a project of its own, a linked worktree belongs to
    // its main one.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let main = base.join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&base, &["clone", "-q", "main", "other-clone"]);
    git(&main, &["worktree", "add", "-q", "../linked"]);
    let (clone, linked) = (base.join("other-clone"), base.join("linked"));
    for d in [&clone, &linked] {
        std::fs::create_dir_all(d.join("packages/tui")).unwrap();
    }
    for (name, dir, project, home) in [
        ("a session in another clone", clone.clone(), &clone, false),
        (
            "a session in a clone subdirectory",
            clone.join("packages/tui"),
            &clone,
            false,
        ),
        (
            "a session in a linked worktree",
            linked.clone(),
            &main,
            false,
        ),
        (
            "a session in a linked worktree subdirectory",
            linked.join("packages/tui"),
            &main,
            false,
        ),
        (
            "the home location",
            clone.join("packages/tui"),
            &clone,
            true,
        ),
    ] {
        assert_eq!(project_of(&dir).as_ref(), Some(project), "{name}");
        let mut c = Context {
            cwd: dir.clone(),
            project: project_of(&dir),
            ..Default::default()
        };
        c.session = (!home).then(|| "ses_clone".to_string());
        let mut b = Browser::worktrees(c).unwrap();
        assert_eq!(
            worktree_requests(&mut b),
            std::slice::from_ref(project),
            "{name}"
        );
        b.answer(Response::Worktrees(
            project.clone(),
            Ok(vec![project.clone()]),
        ));
        assert_eq!(
            create_fresh(&mut b),
            [Request::Create {
                root: project.clone(),
                name: Some("fresh".into()),
            }],
            "{name}"
        );
        let made = base.join("fresh");
        // A new worktree opens a new session there; nothing moves.
        assert_eq!(
            b.answer(Response::Created(project.clone(), Ok(made.clone()))),
            Outcome::Open(Target::Directory(made)),
            "{name}"
        );
    }
}

#[test]
fn oc_uses_homes_selected_location_instead_of_launch() {
    // (adapted) the dialog never reads the launch folder: the UI's own
    // folder is the location, its worktree listed first.
    for launch in ["another clone", "another project"] {
        let mut b = worktrees(&format!("{CLONE}/packages/tui"), true);
        let f = frame(&b);
        assert!(f.contains(CLONE));
        assert!(f.find(CLONE) < f.find(MAIN), "{launch}: {f}");
        assert_eq!(
            create_fresh(&mut b),
            [Request::Create {
                root: p(MAIN),
                name: Some("fresh".into()),
            }]
        );
    }
}

#[test]
fn oc_selecting_a_workspace_opens_home_without_moving_a_session() {
    for home in [false, true] {
        let mut b = worktrees(CLONE, home);
        assert!(frame(&b).contains(LINKED));
        type_text(&mut b, "linked");
        let f = frame(&b);
        assert!(f.contains(LINKED) && !f.contains(CLONE), "{f}");
        assert_eq!(press(&mut b, KeyCode::Enter), open_dir(LINKED), "{home}");
        assert!(b.take_requests().is_empty());
    }
}

#[test]
fn oc_removal_sends_project_ownership_and_the_destination_without_a_configuration_location() {
    let mut b = worktrees(CLONE, true);
    type_text(&mut b, "linked");
    assert!(!frame(&b).contains(CLONE));
    ctrl(&mut b, 'd');
    assert!(frame(&b).contains("again to confirm"));
    assert!(b.take_requests().is_empty());
    ctrl(&mut b, 'd');
    assert_eq!(
        b.take_requests(),
        [Request::Remove {
            root: p(MAIN),
            dir: p(LINKED),
        }]
    );
}

#[test]
fn oc_refresh_explicitly_scans_the_project_and_preserves_the_worktree_filter() {
    let mut b = worktrees(CLONE, true);
    type_text(&mut b, "linked");
    assert!(!frame(&b).contains(CLONE));
    ctrl(&mut b, 'r');
    assert_eq!(worktree_requests(&mut b), [p(MAIN)]);
    b.answer(Response::Worktrees(
        p(MAIN),
        Ok(vec![p(MAIN), p(CLONE), p(LINKED)]),
    ));
    let f = frame(&b);
    assert!(f.contains(LINKED) && !f.contains(CLONE), "{f}");
}

#[test]
fn oc_ctrl_m_moves_only_an_existing_session() {
    for home in [false, true] {
        let mut b = worktrees(CLONE, home);
        let f = frame(&b);
        assert!(f.contains("new ctrl+a"), "{f}");
        assert_eq!(f.contains("move ctrl+m"), !home, "{f}");
        type_text(&mut b, "linked");
        let out = ctrl(&mut b, 'm');
        let want = if home {
            Outcome::Nothing
        } else {
            Outcome::Move {
                session: "ses_clone".into(),
                dir: p(LINKED),
            }
        };
        assert_eq!(out, want, "{home}");
        assert!(b.take_requests().is_empty());
    }
}

fn recovering() -> Browser {
    let mut b = Browser::recover(
        Context {
            session: Some("ses_clone".into()),
            ..ctx(MAIN, Some(MAIN))
        },
        "ses_clone".into(),
        p(CLONE),
    )
    .unwrap();
    assert!(frame(&b).contains("Session location unavailable"));
    assert_eq!(worktree_requests(&mut b), [p(MAIN)]);
    b.answer(Response::Worktrees(p(MAIN), Ok(vec![p(MAIN), p(LINKED)])));
    b
}

#[test]
fn oc_choosing_a_directory_recovers_the_session_when_its_location_is_unavailable() {
    let mut b = recovering();
    assert!(frame(&b).contains(LINKED));
    type_text(&mut b, "linked");
    let f = frame(&b);
    assert!(f.contains(LINKED) && !f.contains(MAIN), "{f}");
    assert_eq!(
        press(&mut b, KeyCode::Enter),
        Outcome::Move {
            session: "ses_clone".into(),
            dir: p(LINKED),
        }
    );
}

#[test]
fn oc_creating_a_worktree_recovers_the_session_without_reading_its_removed_location() {
    let mut b = recovering();
    let sent = create_fresh(&mut b);
    assert_eq!(
        sent,
        [Request::Create {
            root: p(MAIN),
            name: Some("fresh".into()),
        }]
    );
    assert!(!format!("{sent:?}").contains(CLONE));
    assert_eq!(
        b.answer(Response::Created(p(MAIN), Ok(p(CREATED)))),
        Outcome::Move {
            session: "ses_clone".into(),
            dir: p(CREATED),
        }
    );
}

fn stored(id: &str, dir: &str) -> Info {
    Info {
        id: id.into(),
        directory: dir.into(),
        title: None,
        agent: "build".into(),
        model: "p/m".into(),
        parent_id: None,
        created: 1,
        updated: 1,
        usage: Default::default(),
    }
}

#[test]
fn oc_failed_recovery_does_not_navigate_away_from_the_session() {
    // (adapted) the runner moves the session in the store; a move that
    // fails leaves it where it was and shows OpenCode's error toast.
    let db = Store::memory().unwrap();
    db.create(&stored("ses_clone", CLONE)).unwrap();
    let gone = tempfile::tempdir().unwrap().path().join("gone");
    assert!(browse_host::move_to(&db, "ses_clone", &gone).is_err());
    assert_eq!(db.get("ses_clone").unwrap().unwrap().directory, CLONE);
    assert_eq!(browse_host::MOVE_FAILED, "Failed to move session");
    // A move that works.
    let to = tempfile::tempdir().unwrap();
    let dir = browse_host::move_to(&db, "ses_clone", to.path()).unwrap();
    assert_eq!(
        db.get("ses_clone").unwrap().unwrap().directory,
        dir.display().to_string()
    );
    assert!(browse_host::move_to(&db, "ses_missing", to.path()).is_err());
}

#[test]
fn oc_does_not_create_from_another_clone_when_lookup_fails() {
    for what in ["session", "location"] {
        let mut b = worktrees(&format!("{LINKED}/packages/tui"), false);
        assert_eq!(create_fresh(&mut b).len(), 1);
        let out = b.answer(Response::Created(
            p(MAIN),
            Err(format!("{what} unavailable")),
        ));
        assert_eq!(
            out,
            Outcome::Failed(
                "Creating workspace failed".into(),
                format!("{what} unavailable")
            )
        );
        // No longer creating: the name can be sent again.
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.take_requests().len(), 1, "{what}");
    }
}

#[test]
fn oc_does_not_open_creation_when_the_selected_home_location_lookup_fails() {
    assert!(Browser::worktrees(ctx(&format!("{LINKED}/packages/tui"), None)).is_none());
    assert_eq!(NO_PROJECT, "Unable to determine current project");
}

// ----- tui/test/cli/tui/dialog-session-list --------------------------------

#[test]
fn oc_scopes_sessions_to_the_active_session_location() {
    // The active session's folder is the UI's (one elsewhere restarts the
    // UI there), and /sessions lists that folder's.
    let db = Store::memory().unwrap();
    let mut a = stored("ses_a", "/tmp/opencode/project-a");
    a.title = Some("Project A session".into());
    let mut b = stored("ses_b", "/tmp/opencode/project-b");
    b.title = Some("Project B session".into());
    db.create(&a).unwrap();
    db.create(&b).unwrap();
    let list = super::folder_sessions(&db, Path::new("/tmp/opencode/project-b")).unwrap();
    let titles: Vec<_> = list.iter().filter_map(|i| i.title.as_deref()).collect();
    assert_eq!(titles, ["Project B session"]);
}

// ----- tui/test/config-v2 --------------------------------------------------

#[test]
fn oc_shows_the_new_session_location_default_in_settings() {
    let (s, errors) = settings::load(&json!({}));
    assert!(errors.is_empty());
    assert!(!s.new_inherit, "launch is the default");
    let (s, errors) = settings::load(&json!({ "session": { "new_location": "inherit" } }));
    assert!(errors.is_empty() && s.new_inherit);
    let (_, errors) = settings::load(&json!({ "session": { "new_location": "current" } }));
    assert_eq!(
        errors,
        ["tui.session.new_location: must be \"launch\" or \"inherit\""]
    );
}

#[test]
fn untrusted_titles_and_paths_lose_their_control_characters() {
    let dir = "/tmp/opencode/evil\u{1b}[2Jdir";
    let b = open_with(
        ctx("/tmp/elsewhere", None),
        vec![session(
            "ses_x",
            "bad\u{1b}]52;c;AAAA\u{7}\u{202e}title\nnext",
            dir,
            None,
            2,
        )],
    );
    let f = frame(&b);
    assert!(!f.contains('\u{1b}') && !f.contains('\u{7}') && !f.contains('\u{202e}'));
    assert!(f.contains("bad]52;c;AAAAtitle next"), "{f}");
}

#[test]
fn a_codesmith_session_id_is_looked_up_once_whole() {
    let id = codesmith_core::session::new_id();
    let mut b = open_with(ctx("/tmp/elsewhere", None), Vec::new());
    type_text(&mut b, &id);
    assert_eq!(b.take_requests(), vec![Request::Lookup(id)]);
}

#[cfg(unix)]
#[test]
fn a_world_writable_repository_makes_no_project() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let shared = tmp.path().canonicalize().unwrap().join("shared");
    std::fs::create_dir_all(shared.join(".git")).unwrap();
    std::fs::create_dir_all(shared.join("work")).unwrap();
    assert_eq!(project_of(&shared.join("work")), Some(shared.clone()));
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(project_of(&shared.join("work")), None);
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
}
