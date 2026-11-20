//! OpenCode's diff viewer and review panel tests, ported:
//! `tui/test/cli/tui/diff-viewer`, `diff-viewer-file-tree`,
//! `diff-viewer-image`, `tui/test/session-panel`, `tui/test/panel` and the
//! diff settings of `tui/test/config-v2` (the file tree's own tests are in
//! `diff_tree.rs`, the git side's in `diff_source.rs`). Each `oc_*` test
//! names the OpenCode test it ports; "(adapted)" ones say what differs.
//!
//! OpenCode's tests count the requests its viewer makes to the server
//! (`vcs.base`, `vcs.diff`, `session.diff`, `vcs.branch.list`); here a
//! stand-in for git answers the viewer's loads and counts the same calls.
//! CodeSmith has no mouse, so clicks are keys or dropped, and OpenCode's
//! standalone route is the viewer opened full screen.

use super::app::{App, Modal, Pick};
use super::diff_source::{Base, FileDiff, Source, Status};
use super::diff_view::{self, Request, Sources, Viewer};
use super::keymap::Keymap;
use super::theme;
use super::view;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::style::Color;
use ratatui::Terminal;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

pub(crate) const HUNKS: &str = "--- a/src/file.txt
+++ b/src/file.txt
@@ -1,3 +1,3 @@
 const first = true
-const oldFirst = true
+const newFirst = true
 const afterFirst = true
@@ -20,3 +20,3 @@
 const second = true
-const oldSecond = true
+const newSecond = true
 const afterSecond = true
@@ -40,3 +40,3 @@
 const third = true
-const oldThird = true
+const newThird = true
 const afterThird = true";

fn hunk(file: &str) -> FileDiff {
    FileDiff::from_patch(file, Status::Modified, HUNKS)
}

fn hunks() -> Vec<FileDiff> {
    vec![hunk("src/file.txt")]
}

/// `file00.txt`…: `n` files of three hunks.
fn many(n: usize) -> Vec<FileDiff> {
    (0..n).map(|i| hunk(&format!("file{i:02}.txt"))).collect()
}

fn plain(file: &str, status: Status) -> FileDiff {
    FileDiff {
        file: file.into(),
        status,
        additions: 0,
        deletions: 0,
        hunks: vec![],
    }
}

fn added(file: &str, lines: &[String]) -> FileDiff {
    let body: Vec<String> = lines.iter().map(|l| format!("+{l}")).collect();
    let patch = format!(
        "--- /dev/null\n+++ b/{file}\n@@ -0,0 +1,{} @@\n{}",
        lines.len(),
        body.join("\n")
    );
    FileDiff::from_patch(file, Status::Added, &patch)
}

fn base(name: &str, reference: &str) -> Base {
    Base {
        name: name.into(),
        reference: reference.into(),
    }
}

type FilesFn = Box<dyn Fn(Source, Option<&str>) -> Result<Vec<FileDiff>, String>>;

/// OpenCode's test server, as a stand-in for git: what each call answers,
/// and the calls made.
struct Fake {
    base: Result<Option<Base>, String>,
    files: FilesFn,
    branches: Result<Vec<String>, String>,
    turn: Result<Vec<FileDiff>, String>,
    calls: RefCell<Calls>,
}

#[derive(Default, Debug)]
struct Calls {
    base: usize,
    /// Each comparison: its source and base ref.
    diff: Vec<(Source, Option<String>)>,
    turn: usize,
    branches: usize,
}

impl Sources for Fake {
    fn base(&self, _: &Path) -> Result<Option<Base>, String> {
        self.calls.borrow_mut().base += 1;
        self.base.clone()
    }
    fn branches(&self, _: &Path) -> Result<Vec<String>, String> {
        self.calls.borrow_mut().branches += 1;
        self.branches.clone()
    }
    fn vcs_diff(
        &self,
        _: &Path,
        source: Source,
        base: Option<&str>,
    ) -> Result<Vec<FileDiff>, String> {
        self.calls
            .borrow_mut()
            .diff
            .push((source, base.map(String::from)));
        (self.files)(source, base)
    }
    fn turn(&self) -> Result<Vec<FileDiff>, String> {
        self.calls.borrow_mut().turn += 1;
        self.turn.clone()
    }
}

fn fake(files: Vec<FileDiff>) -> Fake {
    let turn = files.clone();
    Fake {
        base: Ok(Some(base("v2", "refs/heads/v2"))),
        files: Box::new(move |_, _| Ok(files.clone())),
        branches: Ok(vec!["v2".into(), "release".into(), "origin/release".into()]),
        turn: Ok(turn),
        calls: RefCell::default(),
    }
}

struct T {
    app: App,
    w: u16,
    h: u16,
    fake: Rc<Fake>,
    /// Loads taken but not answered (`hold`).
    held: Vec<Request>,
    holding: bool,
}

struct Opts {
    w: u16,
    h: u16,
    keys: Value,
    fullscreen: bool,
    source: Option<Source>,
    tree: Option<bool>,
    single: bool,
    branch: Option<&'static str>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            w: 80,
            h: 20,
            keys: json!({}),
            // OpenCode's diff-viewer tests render its full-screen route.
            fullscreen: true,
            source: None,
            tree: None,
            single: false,
            branch: Some("feature"),
        }
    }
}

fn key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

impl T {
    fn new(files: Vec<FileDiff>, w: u16, h: u16) -> T {
        T::with(
            fake(files),
            Opts {
                w,
                h,
                ..Opts::default()
            },
        )
    }

    fn with(fake: Fake, o: Opts) -> T {
        let mut app = App::new();
        app.keymap = Arc::new(Keymap::new(o.keys.as_object().unwrap(), 2000, false));
        app.diff_source = o.source;
        app.diff_prefs.fullscreen = o.fullscreen;
        app.diff_prefs.single = o.single;
        if let Some(t) = o.tree {
            app.diff_prefs.tree = t;
        }
        let mut t = T {
            app,
            w: o.w,
            h: o.h,
            fake: Rc::new(fake),
            held: vec![],
            holding: false,
        };
        t.open_on(o.branch);
        t
    }

    /// `/diff`, on a folder with `branch` checked out.
    fn open_on(&mut self, branch: Option<&str>) {
        let dir = std::path::PathBuf::from("/repo/session");
        let mut v = Viewer::new(
            dir,
            branch,
            self.app
                .diff_memory
                .source
                .or(self.app.diff_source)
                .unwrap_or(Source::Branch),
            self.app.diff_prefs,
        );
        v.reload(&self.app.diff_memory);
        self.app.diff = Some(v);
        self.settle();
    }

    fn reopen(&mut self) {
        self.close();
        self.open_on(Some("feature"));
    }

    fn close(&mut self) {
        self.app.diff = None;
    }

    fn v(&self) -> &Viewer {
        self.app.diff.as_ref().unwrap()
    }

    fn vm(&mut self) -> &mut Viewer {
        self.app.diff.as_mut().unwrap()
    }

    fn calls(&self) -> std::cell::Ref<'_, Calls> {
        self.fake.calls.borrow()
    }

    fn last_diff(&self) -> Option<(Source, Option<String>)> {
        self.calls().diff.last().cloned()
    }

    /// Answer the viewer's loads (unless holding them), then draw.
    fn settle(&mut self) {
        self.draw();
        loop {
            let reqs = self.app.take_diff_requests();
            if reqs.is_empty() {
                break;
            }
            for r in reqs {
                if self.holding {
                    self.held.push(r);
                } else {
                    self.app
                        .diff_answer(diff_view::run(r.job, r.seq, self.fake.as_ref()));
                }
            }
        }
        self.draw();
    }

    /// Answer a held load with `src` instead of the stand-in.
    fn release(&mut self, i: usize, src: &dyn Sources) {
        let r = self.held.remove(i);
        self.app.diff_answer(diff_view::run(r.job, r.seq, src));
        self.draw();
    }

    fn buffer(&self) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(self.w, self.h)).unwrap();
        t.draw(|f| view::draw(f, &self.app)).unwrap();
        t.backend().buffer().clone()
    }

    fn screen(&self) -> Vec<String> {
        let buf = self.buffer();
        (0..self.h)
            .map(|y| {
                (0..self.w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn draw(&mut self) {
        let _ = self.buffer();
    }

    fn text(&self) -> String {
        self.screen().join("\n")
    }

    fn has(&self, s: &str) -> bool {
        self.text().contains(s)
    }

    fn row(&self, y: u16) -> String {
        self.screen()[y as usize].clone()
    }

    /// The first row holding `s`.
    fn find(&self, s: &str) -> Option<usize> {
        self.screen().iter().position(|l| l.contains(s))
    }

    fn bg(&self, x: u16, y: u16) -> Color {
        self.buffer()[(x, y)].bg
    }

    fn fg(&self, x: u16, y: u16) -> Color {
        self.buffer()[(x, y)].fg
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app.on_key(key_event(code, modifiers));
        self.after_key();
    }

    /// The runner's part after a key: a picked choice is carried out, and
    /// loads are answered.
    fn after_key(&mut self) {
        self.settle();
    }

    fn press(&mut self, c: char) {
        self.key(KeyCode::Char(c), KeyModifiers::NONE);
    }

    fn typ(&mut self, s: &str) {
        for c in s.chars() {
            self.press(c);
        }
    }

    fn ctrl(&mut self, c: char) {
        self.key(KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    fn code(&mut self, c: KeyCode) {
        self.key(c, KeyModifiers::NONE);
    }

    /// Pick a picker row by moving `index` down from the top, as the
    /// runner carries the choice out.
    fn pick(&mut self, index: usize) {
        self.code(KeyCode::Home);
        for _ in 0..index {
            self.code(KeyCode::Down);
        }
        let action = self
            .app
            .on_key(key_event(KeyCode::Enter, KeyModifiers::NONE));
        if let Some(super::app::Action::Picked(kind, id)) = action {
            self.app.diff_picked(kind, &id);
        }
        self.settle();
    }

    /// OpenCode's `chooseSource`: the source chooser, row `index`.
    fn choose_source(&mut self, index: usize) {
        self.choose_source_with('d', index);
    }

    fn choose_source_with(&mut self, key: char, index: usize) {
        self.press(key);
        assert!(self.source_open(), "the source chooser opens with {key}");
        self.pick(index);
    }

    fn source_open(&self) -> bool {
        matches!(&self.app.modal, Some(Modal::Picker(p)) if p.kind == Pick::DiffSource)
    }

    fn base_open(&self) -> bool {
        matches!(&self.app.modal, Some(Modal::Picker(p)) if p.kind == Pick::DiffBase)
    }

    fn count(&self) -> String {
        format!("{}/{}", self.v().reviewed_count(), self.v().files.len())
    }

    fn scroll(&self) -> usize {
        self.v().scroll
    }

    fn geom(&self) -> diff_view::Geom {
        self.v().geom(self.w, self.h)
    }

    fn resize(&mut self, w: u16, h: u16) {
        self.w = w;
        self.h = h;
        self.draw();
    }

    /// The screen row of a file's title, when in view.
    fn title_row(&self, file: &str) -> Option<usize> {
        let g = self.geom();
        self.screen()
            .iter()
            .enumerate()
            .skip(g.view_y as usize)
            .find(|(_, l)| l[g.patch_x as usize..].contains(file))
            .map(|(i, _)| i)
    }
}

/// The screen column `s` starts at in `line`.
fn column(line: &str, s: &str) -> Option<usize> {
    line.find(s).map(|i| line[..i].chars().count())
}

// ----- tui/test/cli/tui/diff-viewer -----------------------------------------

// (adapted) The viewer closes back to the session it opened from.
#[test]
fn oc_closing_the_diff_viewer_returns_to_the_route_it_opened_from() {
    let mut t = T::new(vec![], 80, 20);
    t.app.input.set("draft");
    assert_eq!(
        t.last_diff(),
        Some((Source::Branch, Some("refs/heads/v2".into())))
    );
    t.code(KeyCode::Esc);
    assert!(t.app.diff.is_none());
    assert_eq!(t.app.input.text(), "draft");
    assert!(t.text().contains("Ask anything") || t.text().contains("draft"));
}

// (adapted) Too narrow to dock, the full-screen toggle does nothing; the
// 160-column half (OpenCode's standalone route) has no counterpart.
#[test]
fn oc_standalone_full_screen_diff_does_not_expose_panel_only_toggling_at_80_columns() {
    let mut t = T::new(vec![], 80, 20);
    let g = t.geom();
    t.press('f');
    assert_eq!(t.geom(), g);
    assert!(t.app.diff.is_some());
    assert!(!t.app.diff_prefs_changed);
}

#[test]
fn oc_ctrl_c_closes_the_diff_viewer_without_exiting_the_application() {
    let mut t = T::new(vec![], 80, 20);
    t.ctrl('c');
    assert!(t.app.diff.is_none());
    assert!(!t.app.quit);
}

#[test]
fn oc_shows_an_error_instead_of_an_empty_diff_when_loading_fails() {
    let mut f = fake(vec![]);
    f.files = Box::new(|_, _| Err("boom".into()));
    let t = T::with(f, Opts::default());
    assert!(t.has("Could not load diff"));
    assert!(!t.has("No changes to show"));
}

#[test]
fn oc_base_lookup_is_lazy_until_the_viewer_opens() {
    let mut a = App::new();
    assert!(a.take_diff_requests().is_empty());
    let t = T::new(vec![], 80, 20);
    assert_eq!(t.calls().base, 1);
    assert_eq!(t.calls().diff.len(), 1);
    assert_eq!(
        t.last_diff(),
        Some((Source::Branch, Some("refs/heads/v2".into())))
    );
    assert!(t.has("No changes to show"));
}

fn scope_fake() -> Fake {
    let mut f = fake(vec![]);
    f.files = Box::new(|source, _| {
        let names: &[&str] = match source {
            Source::Branch => &[
                "committed.txt",
                "staged.txt",
                "unstaged.txt",
                "untracked.txt",
            ],
            Source::Committed => &["committed.txt"],
            Source::Working => &["staged.txt", "unstaged.txt", "untracked.txt"],
            Source::Turn => &[],
        };
        Ok(names
            .iter()
            .map(|n| FileDiff {
                additions: 1,
                ..plain(n, Status::Modified)
            })
            .collect())
    });
    f
}

fn sources_directly(width: u16) {
    let mut t = T::with(
        scope_fake(),
        Opts {
            w: width,
            h: 30,
            ..Opts::default()
        },
    );
    assert_eq!(t.last_diff().unwrap().0, Source::Branch);
    for (source, index, label, first, absent) in [
        (
            Source::Committed,
            1,
            "Committed",
            "committed.txt",
            "untracked.txt",
        ),
        (
            Source::Working,
            2,
            "Uncommitted",
            "staged.txt",
            "committed.txt",
        ),
        (
            Source::Branch,
            0,
            "All",
            "committed.txt",
            "never-present.txt",
        ),
    ] {
        t.choose_source(index);
        assert!(t.has(label) && !t.has("Loading diff"));
        assert!(t.has(first), "{label}: {}", t.text());
        assert!(!t.has(absent), "{label}");
        let (s, b) = t.last_diff().unwrap();
        assert_eq!(s, source);
        assert_eq!(
            b,
            (source != Source::Working).then(|| "refs/heads/v2".to_string())
        );
    }
    assert_eq!(t.calls().base, 1);
    assert_eq!(t.calls().diff.len(), 4);
    // The choice is remembered for the session, not written as a setting.
    assert!(!t.app.diff_prefs_changed);
    assert_eq!(t.app.diff_source, None);
}

#[test]
fn oc_the_source_chooser_selects_all_three_scopes_directly_at_50_columns() {
    sources_directly(50);
}

#[test]
fn oc_the_source_chooser_selects_all_three_scopes_directly_at_160_columns() {
    sources_directly(160);
}

fn reopening_remembers(source: Source) {
    let mut t = T::with(
        fake(hunks()),
        Opts {
            h: 30,
            source: Some(source),
            ..Opts::default()
        },
    );
    assert_eq!(t.last_diff().unwrap().0, source);
    assert_eq!(t.calls().base, usize::from(source != Source::Working));
    t.choose_source(if source == Source::Working { 1 } else { 2 });
    assert!(t.has(if source == Source::Working {
        "Committed"
    } else {
        "Uncommitted"
    }));
    assert!(t.has("const first"));
    assert_eq!(t.app.diff_source, Some(source));
    t.reopen();
    assert!(t.has("const first"));
    let want = if source == Source::Working {
        Source::Committed
    } else {
        Source::Working
    };
    assert_eq!(t.last_diff().unwrap().0, want);
    assert_eq!(
        t.calls().base,
        if source == Source::Working { 2 } else { 1 }
    );
    assert_eq!(t.calls().diff.len(), 3);
}

#[test]
fn oc_reopening_remembers_the_last_source_choice_instead_of_the_initial_branch_setting() {
    reopening_remembers(Source::Branch);
}

#[test]
fn oc_reopening_remembers_the_last_source_choice_instead_of_the_initial_committed_setting() {
    reopening_remembers(Source::Committed);
}

#[test]
fn oc_reopening_remembers_the_last_source_choice_instead_of_the_initial_working_setting() {
    reopening_remembers(Source::Working);
}

#[test]
fn oc_the_turn_source_diffs_the_sessions_last_turn_without_resolving_a_base() {
    let mut t = T::new(hunks(), 80, 30);
    t.choose_source(3);
    assert!(t.has("Last turn") && t.has("const first"));
    assert!(!t.has("Base not reported"));
    assert_eq!(t.calls().turn, 1);
    assert_eq!(t.calls().diff.len(), 1);
    assert_eq!(t.calls().base, 1);
    t.choose_source(2);
    assert!(t.has("Uncommitted · vs HEAD") && t.has("const first"));
    assert_eq!(t.last_diff(), Some((Source::Working, None)));
}

#[test]
fn oc_the_turn_source_refreshes_only_when_the_session_finishes_a_turn() {
    let mut t = T::with(
        fake(hunks()),
        Opts {
            source: Some(Source::Turn),
            ..Opts::default()
        },
    );
    assert_eq!(t.calls().turn, 1);
    t.app.busy = true;
    t.settle();
    assert_eq!(t.calls().turn, 1);
    t.app.busy = false;
    t.app.diff_turn_done();
    t.settle();
    assert_eq!(t.calls().turn, 2);
    assert!(t.has("const first"));
    assert_eq!(t.calls().diff.len(), 0);
}

#[test]
fn oc_an_empty_or_failing_turn_diff_uses_the_viewers_empty_and_error_states() {
    let turn = Opts {
        source: Some(Source::Turn),
        ..Opts::default()
    };
    let empty = T::with(fake(vec![]), turn);
    assert!(empty.has("No changes to show"));
    assert_eq!(empty.calls().base, 0);
    assert_eq!(empty.calls().diff.len(), 0);
    assert_eq!(empty.calls().turn, 1);
    let mut f = fake(vec![]);
    f.turn = Err("boom".into());
    let failing = T::with(
        f,
        Opts {
            source: Some(Source::Turn),
            ..Opts::default()
        },
    );
    assert!(failing.has("Could not load diff. Reopen the diff viewer"));
    assert!(failing.has("Last turn · Diff unavailable"));
    assert!(!failing.has("Choose a base branch"));
}

fn one_row(width: u16) {
    let mut f = fake(hunks());
    f.base = Ok(Some(base("release", "refs/remotes/origin/release")));
    let mut t = T::with(
        f,
        Opts {
            w: width,
            h: 30,
            ..Opts::default()
        },
    );
    let g = t.geom();
    let y = if g.tree { 1 } else { 0 };
    let row = t.row(y);
    assert!(row.contains("All · vs release"), "{row}");
    assert!(
        row.trim_end().ends_with("0/1") || row.contains("0/1"),
        "{row}"
    );
    assert!(!t.has("reviewed"));
    assert_eq!(
        t.last_diff().unwrap().1.as_deref(),
        Some("refs/remotes/origin/release")
    );
    t.press('d');
    let text = t.text();
    assert!(text.contains("Base") && text.contains("release"));
    let rows = t.screen();
    let first = rows
        .iter()
        .position(|r| r.contains("Branch + local changes"))
        .unwrap();
    let col = column(&rows[first], "Branch + local changes").unwrap();
    for (i, (label, desc)) in [
        ("All", "Branch + local changes"),
        ("Committed", "Branch commits only"),
        ("Uncommitted", "Local changes only"),
        ("Last turn", "Latest session turn"),
        ("Base", "release"),
    ]
    .iter()
    .enumerate()
    {
        let r = &rows[first + i];
        assert!(r.contains(label) && r.contains(desc), "{r}");
        if i < 4 {
            assert_eq!(column(r, desc).unwrap(), col, "{r}");
        }
    }
    t.code(KeyCode::Esc);
    assert!(!t.source_open());
    t.press('m');
    assert!(t.row(y).contains("1/1"));
    t.choose_source(2);
    assert!(t.has("Uncommitted · vs HEAD"));
    t.press('d');
    let text = t.text();
    assert!(text
        .lines()
        .any(|l| l.contains("Base") && l.contains("release")));
    assert!(!text
        .lines()
        .any(|l| l.contains("Base") && l.contains("HEAD")));
    assert_eq!(t.calls().base, 1);
}

#[test]
fn oc_keeps_scope_base_and_review_count_on_one_row_with_a_selectable_base_at_50_columns() {
    one_row(50);
}

#[test]
fn oc_keeps_scope_base_and_review_count_on_one_row_with_a_selectable_base_at_80_columns() {
    one_row(80);
}

#[test]
fn oc_keeps_scope_base_and_review_count_on_one_row_with_a_selectable_base_at_160_columns() {
    one_row(160);
}

fn long_base(width: u16) {
    let long = "release/very-long-comparison-branch-name-that-does-not-fit-in-the-header";
    let mut f = fake(hunks());
    f.base = Ok(Some(base(long, "refs/heads/v2")));
    let t = T::with(
        f,
        Opts {
            w: width,
            source: Some(Source::Committed),
            ..Opts::default()
        },
    );
    let g = t.geom();
    let y = if g.tree { 1 } else { 0 };
    let row = t.row(y);
    assert!(row.contains("Committed · vs"), "{row}");
    assert!(row.contains("0/1"), "{row}");
    assert!(!row.contains(long));
    if width < 88 {
        // The compact layout moves help from the header into its footer.
        assert!(t.row(t.h - 1).contains("? see all"));
        assert!(g.view_y > y);
    }
}

#[test]
fn oc_long_comparison_bases_cannot_hide_the_source_or_review_count_at_50_columns() {
    long_base(50);
}

#[test]
fn oc_long_comparison_bases_cannot_hide_the_source_or_review_count_at_80_columns() {
    long_base(80);
}

#[test]
fn oc_long_comparison_bases_cannot_hide_the_source_or_review_count_at_100_columns() {
    long_base(100);
}

#[test]
fn oc_long_comparison_bases_cannot_hide_the_source_or_review_count_at_160_columns() {
    long_base(160);
}

#[test]
fn oc_opening_the_source_chooser_from_initial_uncommitted_does_not_resolve_a_branch_base() {
    let mut t = T::with(
        fake(hunks()),
        Opts {
            h: 30,
            source: Some(Source::Working),
            ..Opts::default()
        },
    );
    t.press('d');
    assert!(t.source_open());
    assert!(t.has("Uncommitted · vs HEAD"));
    assert!(t
        .text()
        .lines()
        .any(|l| l.contains("Base") && l.contains("Choose…")));
    assert_eq!(t.calls().base, 0);
    assert_eq!(t.calls().diff.len(), 1);
}

fn choosing_a_base(source: Source) {
    let mut t = T::with(
        fake(hunks()),
        Opts {
            h: 30,
            source: Some(source),
            ..Opts::default()
        },
    );
    t.press('m');
    assert_eq!(t.count(), "1/1");
    t.choose_source(4);
    assert!(t.base_open());
    assert!(t.has("Base branch") && t.has("origin/release"));
    if source != Source::Working {
        assert!(t
            .text()
            .lines()
            .any(|l| l.contains('●') && l.contains("v2")));
    }
    assert_eq!(t.calls().branches, 1);
    t.typ("origin/release");
    let action = t.app.on_key(key_event(KeyCode::Enter, KeyModifiers::NONE));
    if let Some(super::app::Action::Picked(kind, id)) = action {
        assert_eq!(id, "origin/release");
        t.app.diff_picked(kind, &id);
    }
    t.settle();
    assert!(!t.base_open() && t.has("src/file.txt"));
    let want = (source != Source::Working).then(|| "origin/release".to_string());
    assert_eq!(
        t.last_diff().unwrap(),
        (
            source,
            if source == Source::Working {
                None
            } else {
                want
            }
        )
    );
    assert_eq!(
        t.calls().diff.len(),
        if source == Source::Working { 1 } else { 2 }
    );
    assert_eq!(
        t.count(),
        if source == Source::Working {
            "1/1"
        } else {
            "0/1"
        }
    );
    if source != Source::Working {
        t.press('m');
    }
    assert_eq!(t.count(), "1/1");
    t.reopen();
    assert!(t.has("const first"));
    assert_eq!(t.count(), "0/1");
    assert_eq!(
        t.calls().diff.len(),
        if source == Source::Working { 2 } else { 3 }
    );
    if source != Source::Working {
        assert_eq!(t.last_diff().unwrap().1.as_deref(), Some("origin/release"));
    }
    t.choose_source(4);
    assert!(t
        .text()
        .lines()
        .any(|l| l.contains('●') && l.contains("origin/release")));
    assert_eq!(t.calls().base, usize::from(source != Source::Working));
}

#[test]
fn oc_choosing_a_base_remembers_it_and_refreshes_only_the_affected_branch_comparison() {
    choosing_a_base(Source::Branch);
}

#[test]
fn oc_choosing_a_base_remembers_it_and_refreshes_only_the_affected_committed_comparison() {
    choosing_a_base(Source::Committed);
}

#[test]
fn oc_choosing_a_base_remembers_it_and_refreshes_only_the_affected_working_comparison() {
    choosing_a_base(Source::Working);
}

fn ambiguous(source: Source) {
    let mut f = fake(vec![]);
    f.base = Err("choose a base".into());
    f.files = Box::new(|_, b| match b {
        Some(_) => Ok(hunks()),
        None => Err("choose a base".into()),
    });
    let mut t = T::with(
        f,
        Opts {
            h: 30,
            source: Some(source),
            ..Opts::default()
        },
    );
    assert!(t.has("Choose a base branch"));
    assert!(!t.has("No changes to show"));
    assert_eq!(t.calls().diff.len(), 0);
    t.choose_source(4);
    assert!(t.has("Base branch") && t.has("origin/release"));
    t.pick(1);
    assert!(t.has("vs release") && t.has("const first"), "{}", t.text());
    assert_eq!(t.last_diff().unwrap(), (source, Some("release".into())));
    assert_eq!(t.app.diff_memory.source, None);
    assert_eq!(t.calls().base, 1);
}

#[test]
fn oc_an_ambiguous_base_never_requests_a_guessed_branch_diff() {
    ambiguous(Source::Branch);
}

#[test]
fn oc_an_ambiguous_base_never_requests_a_guessed_committed_diff() {
    ambiguous(Source::Committed);
}

// (adapted) "A new TUI instance" is a new app: nothing was written.
#[test]
fn oc_base_and_scope_choices_survive_reopening_but_not_a_new_tui_instance() {
    let opts = || Opts {
        h: 30,
        source: Some(Source::Working),
        ..Opts::default()
    };
    let mut t = T::with(fake(hunks()), opts());
    t.choose_source(4);
    t.pick(1);
    assert!(!t.base_open());
    t.choose_source(1);
    assert!(t.has("Committed · vs release") && t.has("const first"));
    t.reopen();
    assert!(t.has("Committed · vs release") && t.has("const first"));
    assert!(!t.app.diff_prefs_changed);
    let mut restarted = T::with(fake(hunks()), opts());
    assert_eq!(restarted.last_diff().unwrap().0, Source::Working);
    restarted.choose_source(1);
    assert!(restarted.has("Committed · vs v2") && restarted.has("const first"));
    assert_eq!(
        restarted.last_diff().unwrap().1.as_deref(),
        Some("refs/heads/v2")
    );
}

// (adapted) The branch is read when the viewer opens, so the switch is
// seen on reopening.
#[test]
fn oc_base_choices_are_isolated_by_branch_within_the_same_location() {
    let mut t = T::new(hunks(), 80, 30);
    t.choose_source(4);
    t.pick(1);
    assert!(t.has("All · vs release") && t.has("const first"));
    t.close();
    t.open_on(Some("other-feature"));
    assert!(t.has("All · vs v2"));
    assert_eq!(t.last_diff().unwrap().1.as_deref(), Some("refs/heads/v2"));
    t.close();
    t.open_on(Some("feature"));
    assert!(t.has("All · vs release"));
    assert_eq!(t.last_diff().unwrap().1.as_deref(), Some("release"));
    assert_eq!(t.calls().base, 2);
}

#[test]
fn oc_an_invalid_comparison_reports_an_error_and_allows_another_base_choice() {
    let mut f = fake(hunks());
    f.files = Box::new(|_, b| {
        if b == Some("release") {
            Err("no common history".into())
        } else {
            Ok(hunks())
        }
    });
    let mut t = T::with(
        f,
        Opts {
            h: 30,
            ..Opts::default()
        },
    );
    t.choose_source(4);
    t.pick(1);
    assert!(t.has("Base or diff unavailable"));
    assert!(!t.has("No changes to show"));
    t.choose_source(4);
    t.pick(0);
    assert!(t.has("All · vs v2") && t.has("const first"));
    assert_eq!(t.calls().diff.len(), 3);
    assert_eq!(t.last_diff().unwrap().1.as_deref(), Some("v2"));
}

#[test]
fn oc_base_search_failures_are_visible_without_changing_the_diff() {
    let mut f = fake(hunks());
    f.branches = Err("branches unavailable".into());
    let mut t = T::with(
        f,
        Opts {
            h: 30,
            ..Opts::default()
        },
    );
    t.choose_source(4);
    assert!(t.has("Could not load branches"));
    assert!(t.has("All · vs v2"));
    assert_eq!(t.calls().diff.len(), 1);
}

#[test]
fn oc_a_late_base_lookup_cannot_overwrite_an_in_memory_base_choice() {
    let mut t = T::with(
        fake(hunks()),
        Opts {
            h: 30,
            ..Opts::default()
        },
    );
    // Start over with the first load held.
    t.close();
    t.holding = true;
    t.open_on(Some("feature"));
    assert!(t.has("Loading diff"));
    t.holding = false;
    t.choose_source(4);
    t.pick(1);
    assert!(t.has("All · vs release") && t.has("const first"));
    let first = fake(hunks());
    t.release(0, &first);
    assert!(t.has("All · vs release"));
    assert_eq!(t.last_diff().unwrap().1.as_deref(), Some("release"));
    t.choose_source(1);
    assert!(t.has("Committed · vs release"));
    assert_eq!(
        t.last_diff().unwrap(),
        (Source::Committed, Some("release".into()))
    );
    t.press('d');
    assert!(t
        .text()
        .lines()
        .any(|l| l.contains("Base") && l.contains("release")));
}

#[test]
fn oc_dismissing_the_base_picker_leaves_the_comparison_unchanged() {
    let mut t = T::new(hunks(), 80, 30);
    t.choose_source(4);
    t.code(KeyCode::Down);
    t.code(KeyCode::Esc);
    assert!(!t.base_open());
    t.press('d');
    assert!(t.source_open());
    assert!(t.has("All · vs v2"));
    assert_eq!(t.calls().diff.len(), 1);
}

#[test]
fn oc_tree_single_file_and_layout_preferences_do_not_refetch_base_or_diff() {
    let mut t = T::new(hunks(), 160, 20);
    let mut changed = 0;
    for k in ['b', 's', 'v'] {
        t.press(k);
        changed += usize::from(std::mem::take(&mut t.app.diff_prefs_changed));
    }
    assert_eq!(changed, 3);
    let p = t.app.diff_prefs;
    assert!(!p.tree && p.single);
    assert_eq!(p.view, Some(diff_view::View::Unified));
    assert_eq!(t.calls().base, 1);
    assert_eq!(t.calls().diff.len(), 1);
}

#[test]
fn oc_the_remapped_source_chooser_preserves_its_current_selection_marker() {
    let mut t = T::with(
        fake(hunks()),
        Opts {
            h: 30,
            keys: json!({"diff.switch_source": "x"}),
            ..Opts::default()
        },
    );
    t.press('d');
    assert!(!t.source_open());
    t.choose_source_with('x', 1);
    assert!(t.has("Committed") && t.has("const first"));
    t.press('x');
    let text = t.text();
    assert!(text
        .lines()
        .any(|l| l.contains('●') && l.contains("Committed")));
    assert!(text.contains("Branch commits only") && text.contains("Local changes only"));
    assert!(text.lines().any(|l| l.contains("Base") && l.contains("v2")));
}

#[test]
fn oc_pending_and_failed_scopes_never_render_the_previous_scopes_files() {
    let mut t = T::new(hunks(), 80, 30);
    assert!(t.has("const first"));
    t.holding = true;
    t.choose_source(1);
    assert!(t.has("Loading diff"));
    assert!(t.has("Committed"));
    assert!(!t.has("src/file.txt"));
    let mut failing = fake(vec![]);
    failing.files = Box::new(|_, _| Err("comparison failed".into()));
    t.release(0, &failing);
    assert!(t.has("Could not load diff"));
    assert!(!t.has("No changes to show"));
    assert!(!t.has("src/file.txt"));
    t.holding = false;
    t.choose_source(2);
    assert!(t.has("Uncommitted") && t.has("const first"));
    assert_eq!(t.calls().base, 1);
}

#[test]
fn oc_late_diff_responses_cannot_replace_the_selected_scope() {
    let mut f = fake(vec![]);
    f.files = Box::new(|_, _| Ok(vec![hunk("uncommitted.txt")]));
    let mut t = T::with(
        f,
        Opts {
            h: 30,
            ..Opts::default()
        },
    );
    t.holding = true;
    t.choose_source(1);
    assert!(t.has("Loading diff"));
    t.holding = false;
    t.choose_source(2);
    assert!(t.has("uncommitted.txt"));
    let late = fake(vec![hunk("committed.txt")]);
    t.release(0, &late);
    assert!(t.has("Uncommitted"));
    assert!(t.has("uncommitted.txt"));
    assert!(!t.text().contains(" committed.txt"));
}

#[test]
fn oc_switching_to_uncommitted_does_not_wait_for_or_display_a_late_base_result() {
    let mut t = T::new(hunks(), 80, 30);
    t.close();
    t.holding = true;
    t.open_on(Some("feature"));
    assert!(t.has("Loading diff"));
    assert_eq!(t.calls().diff.len(), 1);
    t.holding = false;
    t.choose_source(2);
    assert!(t.has("Uncommitted") && t.has("const first"));
    let late = fake(hunks());
    t.release(0, &late);
    assert!(t.has("vs HEAD"));
    assert!(!t.has("vs v2"));
    assert_eq!(t.last_diff().unwrap().0, Source::Working);
}

fn missing_base(changed: bool) {
    let mut f = fake(vec![]);
    f.base = Ok(None);
    f.files = Box::new(move |source, _| {
        Ok(match source {
            Source::Working => vec![hunk("dirty.txt")],
            _ if changed => vec![hunk("branch.txt")],
            _ => vec![],
        })
    });
    let mut t = T::with(
        f,
        Opts {
            h: 30,
            ..Opts::default()
        },
    );
    assert!(t.has("All"));
    assert!(t.has("Base not reported"));
    assert!(t.has(if changed {
        "branch.txt"
    } else {
        "No changes to show"
    }));
    assert!(!t.has("dirty.txt"));
    assert_eq!(t.last_diff().unwrap(), (Source::Branch, None));
    t.choose_source(1);
    assert!(t.has("Committed comparison unavailable"));
    assert!(t.has("Base not reported"));
    assert!(!t.has("No changes to show"));
    assert!(!t.has("branch.txt"));
    assert_eq!(t.calls().diff.len(), 1);
    t.choose_source(2);
    assert!(t.has("dirty.txt"));
    assert!(!t.has("Base not reported"));
    assert_eq!(t.last_diff().unwrap(), (Source::Working, None));
    assert_eq!(t.calls().diff.len(), 2);
    assert_eq!(t.calls().base, 1);
}

#[test]
fn oc_missing_base_metadata_preserves_provider_branch_review_has_branch_changes_false() {
    missing_base(false);
}

#[test]
fn oc_missing_base_metadata_preserves_provider_branch_review_has_branch_changes_true() {
    missing_base(true);
}

#[test]
fn oc_unresolved_branch_comparisons_report_errors_without_switching_to_working() {
    let mut f = fake(vec![]);
    f.base = Ok(None);
    f.files = Box::new(|source, _| match source {
        Source::Branch => Err("comparison base unavailable".into()),
        _ => Ok(hunks()),
    });
    let mut t = T::with(
        f,
        Opts {
            h: 30,
            ..Opts::default()
        },
    );
    assert!(t.has("Could not load diff"));
    assert!(!t.has("No changes to show"));
    assert_eq!(t.last_diff().unwrap().0, Source::Branch);
    assert_eq!(t.calls().diff.len(), 1);
    t.choose_source(2);
    assert!(t.has("const first"));
    assert_eq!(t.last_diff().unwrap().0, Source::Working);
    assert_eq!(t.calls().base, 1);
}

#[test]
fn oc_base_lookup_failures_are_errors_rather_than_missing_base_fallbacks() {
    let mut f = fake(vec![]);
    f.base = Err("base failed".into());
    let mut t = T::with(
        f,
        Opts {
            h: 30,
            ..Opts::default()
        },
    );
    assert!(t.has("Could not load diff"));
    assert!(!t.has("No changes to show"));
    assert_eq!(t.calls().diff.len(), 0);
    t.choose_source(2);
    assert!(t.has("No changes to show"));
    assert_eq!(t.last_diff().unwrap().0, Source::Working);
}

// (adapted) CodeSmith reads no image at all; the note is OpenCode's.
#[test]
fn oc_committed_images_never_read_the_working_tree_image_patch_undefined() {
    let t = T::with(
        fake(vec![plain("assets/preview.png", Status::Modified)]),
        Opts {
            source: Some(Source::Committed),
            ..Opts::default()
        },
    );
    assert!(t.has("Committed image preview unavailable"));
    assert!(!t.has("Working tree preview"));
}

#[test]
fn oc_committed_images_never_read_the_working_tree_image_patch_binary() {
    let f = FileDiff::from_patch(
        "assets/preview.png",
        Status::Modified,
        "Binary files a/assets/preview.png and b/assets/preview.png differ",
    );
    let t = T::with(
        fake(vec![f]),
        Opts {
            source: Some(Source::Committed),
            ..Opts::default()
        },
    );
    assert!(t.has("Committed image preview unavailable"));
}

#[test]
fn oc_brackets_navigate_diff_hunks() {
    let mut t = T::new(hunks(), 80, 12);
    assert!(t.has("const first"));
    let initial = t.scroll();
    t.press(']');
    let first = t.scroll();
    assert!(first > initial);
    // The second hunk sits below the sticky title.
    let second_row = t.find("const second").unwrap();
    assert!(second_row >= t.geom().view_y as usize + 2, "{}", t.text());
    t.press(']');
    let second = t.scroll();
    assert!(second > first);
    assert!(t.find("const third").unwrap() >= t.geom().view_y as usize + 2);
    t.press('[');
    assert_eq!(t.scroll(), first);
    t.press(']');
    assert_eq!(t.scroll(), second);
    t.vm().scroll = initial;
    t.press(']');
    assert_eq!(t.scroll(), first);
}

#[test]
fn oc_disabled_diff_keybinds_have_no_component_fallbacks() {
    let off = json!({
        "diff.down": "none", "diff.up": "none", "diff.page.down": "none", "diff.page.up": "none",
        "diff.half_page.down": "none", "diff.half_page.up": "none", "diff.mark_reviewed": "none",
    });
    let mut t = T::with(
        fake(hunks()),
        Opts {
            h: 12,
            keys: off.clone(),
            ..Opts::default()
        },
    );
    for id in off.as_object().unwrap().keys() {
        assert_eq!(t.app.keymap.hint(id), "", "{id}");
    }
    let initial = t.scroll();
    t.press('j');
    assert_eq!(t.scroll(), initial);
}

fn full_width(status: Status) {
    let patch = if status == Status::Added {
        "--- /dev/null\n+++ b/src/new.txt\n@@ -0,0 +1 @@\n+full width content\n"
    } else {
        "--- a/src/new.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-full width content\n"
    };
    let t = T::new(
        vec![FileDiff::from_patch("src/new.txt", status, patch)],
        160,
        20,
    );
    assert!(t.has("full width content"));
    let l = t.v().layout(&t.geom());
    assert!(l
        .items
        .iter()
        .all(|i| !matches!(i, diff_view::Item::Split { .. })));
    assert!(!t.text().chars().any(|c| "├└┌┐┬┴─".contains(c)));
}

#[test]
fn oc_added_files_use_full_width_even_in_split_view() {
    full_width(Status::Added);
}

#[test]
fn oc_deleted_files_use_full_width_even_in_split_view() {
    full_width(Status::Deleted);
}

fn adapts(width: u16) {
    let t = T::new(hunks(), width, 20);
    assert!(t.has("const first"));
    let l = t.v().layout(&t.geom());
    let split = l
        .items
        .iter()
        .any(|i| matches!(i, diff_view::Item::Split { .. }));
    assert_eq!(split, width == 160);
    assert!(t.has("0/1"));
}

#[test]
fn oc_adapts_navigation_and_modified_patches_at_80_columns() {
    adapts(80);
}

#[test]
fn oc_adapts_navigation_and_modified_patches_at_160_columns() {
    adapts(160);
}

#[test]
fn oc_the_first_file_title_sits_below_full_height_top_padding_with_a_blank_row_below() {
    let t = T::new(hunks(), 80, 20);
    let lines = t.screen();
    let title = t.find("src/file.txt").unwrap();
    assert!(title > 0);
    assert_eq!(lines[title - 1].trim(), "");
    let x = lines[title].find("src/").unwrap() as u16;
    assert_eq!(t.bg(x + 2, title as u16 - 1), t.bg(x, title as u16));
    assert_eq!(lines[title + 1].trim(), "");
    assert!(lines[title + 2].contains("const first"));
    assert!(lines[title].contains("+3 -3"));
}

fn pane_edges(width: u16) {
    let mut t = T::new(many(6), width, 40);
    assert!(t.has("const first"));
    let lines = t.screen();
    let title = t.title_row("file00.txt").unwrap();
    assert_eq!(t.find("All").unwrap(), title);
    assert_eq!(title, 1);
    // The row above is the tree's and the patch pane's top edges.
    assert_eq!(lines[title - 1].trim(), "");
    let g = t.geom();
    let card = g.patch_x + g.inset;
    assert_eq!(t.bg(2, 0), t.bg(2, 1));
    assert_eq!(t.bg(card + 1, 0), t.bg(card + 1, 1));
    assert!(lines[title + 2].contains("const first"));
    t.vm().scroll = 5;
    assert_eq!(t.row(0), lines[0]);
    assert!(t.row(1).contains("file00.txt"));
    t.press('n');
    assert_eq!(t.row(0), lines[0]);
    assert!(t.row(1).contains("file01.txt"), "{}", t.text());
    t.press('s');
    assert!(t.row(1).contains("file01.txt"));
    t.resize(80, 30);
    assert!(!t.geom().tree);
    // The compact header is one row, right above the pane's edge.
    assert!(t.row(2).contains("file01.txt"), "{}", t.text());
}

#[test]
fn oc_shared_pane_edges_align_headings_and_stay_fixed_at_100_columns() {
    pane_edges(100);
}

#[test]
fn oc_shared_pane_edges_align_headings_and_stay_fixed_at_160_columns() {
    pane_edges(160);
}

fn edge_matches(mode: theme::doc::Mode) {
    let _ = mode;
    let files = (0..4).map(|i| hunk(&format!("src/file{i}.txt"))).collect();
    let mut t = T::new(files, 160, 18);
    let g = t.geom();
    let p = theme::current();
    let card = g.patch_x + g.inset;
    let edge = |t: &T| t.bg(card + 1, g.view_y - 1);
    assert_eq!(edge(&t), p.diff_context_bg);
    t.press('m');
    assert_eq!(edge(&t), p.raised_high);
    // The collapsed card's bottom row is a half-block in its color.
    assert_eq!(
        t.screen()[g.view_y as usize + 1]
            .chars()
            .filter(|&c| c == '▀')
            .count(),
        g.card_w() as usize + 2 * g.inset as usize
    );
    assert_eq!(t.fg(card + 1, g.view_y + 1), p.raised_high);
    t.press('n');
    assert_eq!(edge(&t), p.diff_context_bg);
    t.press('m');
    assert_eq!(edge(&t), p.raised_high);
    t.press('n');
    assert_eq!(edge(&t), p.diff_context_bg);
    // Scrolling back follows the card in view, not the selection.
    t.vm().scroll = 0;
    assert_eq!(edge(&t), p.raised_high);
    t.press('s');
    assert_eq!(edge(&t), p.raised_high);
    t.press('m');
    assert!(t.has("const first"));
    assert_eq!(edge(&t), p.diff_context_bg);
}

// (adapted) Run in the default theme's dark mode; the light half checks
// the same colors.
#[test]
fn oc_the_pane_edge_matches_the_visible_reviewed_card_in_dark_mode() {
    edge_matches(theme::doc::Mode::Dark);
}

#[test]
fn oc_the_pane_edge_matches_the_visible_reviewed_card_in_light_mode() {
    edge_matches(theme::doc::Mode::Light);
}

#[test]
fn oc_the_sidebar_holds_a_compact_help_hint_and_the_patch_pane_reaches_the_screen_bottom() {
    let mut t = T::new(hunks(), 160, 40);
    let g = t.geom();
    assert_eq!(g.view_y + g.view_h, 40);
    let lines = t.screen();
    assert_eq!(lines[37].find("? help"), Some(2));
    let text = t.text();
    for s in [
        "n/p files",
        "tab focus",
        "next file",
        "next hunk",
        "half page",
    ] {
        assert!(!text.contains(s), "{s}");
    }
    t.code(KeyCode::Tab);
    assert!(t.has("? help"));
    t.press('?');
    let text = t.text();
    assert!(text.contains("Diff shortcuts"));
    for s in [
        "Next file",
        "Half page",
        "Split / unified",
        "alt+↓",
        "Review + collapse / reopen",
    ] {
        assert!(text.contains(s), "{s}: {text}");
    }
    assert!(!text.contains("Focus files"));
    t.code(KeyCode::Esc);
    assert!(!t.has("Diff shortcuts"));
    assert!(t.app.diff.is_some());
}

#[test]
fn oc_compact_gutter_help_leaves_the_full_patch_viewport_available_when_the_sidebar_is_hidden() {
    let mut t = T::new(hunks(), 160, 24);
    t.press('b');
    let g = t.geom();
    assert_eq!(t.row(0).chars().nth(159), Some('?'));
    assert_eq!(g.view_y, 2);
    assert_eq!(g.view_y + g.view_h, 24);
    t.press('?');
    assert!(t.has("Diff shortcuts"));
    t.code(KeyCode::Esc);
    t.press('b');
    assert_eq!(t.find("? help"), Some(21));
    // Too narrow for the tree: the compact layout, help in its footer.
    t.resize(80, 24);
    let g = t.geom();
    assert!(!g.tree);
    assert_eq!(g.view_y + g.view_h, 23);
    assert!(!t.has("? help"));
    assert!(t.row(0).contains("All"));
    let footer = t.row(23);
    assert!(
        footer.starts_with("  esc close") && footer.trim_end().ends_with("? see all"),
        "{footer}"
    );
    t.press('d');
    assert!(t.source_open());
    t.code(KeyCode::Esc);
    assert!(!t.source_open());
}

#[test]
fn oc_full_screen_keeps_its_tree_down_to_88_columns_and_uses_the_compact_layout_below() {
    let mut t = T::new(many(3), 88, 24);
    t.press('m');
    let g = t.geom();
    assert!(g.tree && !g.compact);
    assert_eq!(g.card_w(), g.patch_w - 4);
    t.resize(87, 24);
    let g = t.geom();
    assert!(!g.tree && g.compact);
    assert_eq!(g.card_w(), 87);
    // The gap above a flush card is the card's color across the width.
    let second = t.title_row("file01.txt").unwrap() as u16;
    let p = theme::current();
    for x in 0..87 {
        assert_eq!(t.fg(x, second - 1), p.diff_context_bg);
    }
    assert!(t.row(0).contains("1/3"));
    assert!(t.row(23).starts_with("  esc close"));
    t.press('b');
    assert!(!t.geom().tree);
    t.resize(88, 24);
    assert!(t.geom().tree);
    assert!(t.has("1/3"));
}

#[test]
fn oc_compact_help_scrolls_at_a_narrow_size_and_reflects_customized_bindings() {
    let mut t = T::with(
        fake(hunks()),
        Opts {
            w: 50,
            h: 20,
            keys: json!({"diff.next_file": "ctrl+n", "diff.previous_file": "ctrl+p", "diff.close": "x"}),
            ..Opts::default()
        },
    );
    t.press('?');
    let text = t.text();
    assert!(text.contains("Diff shortcuts"));
    assert!(
        text.lines()
            .any(|l| l.contains("ctrl+n") && l.contains("Next file")),
        "{text}"
    );
    assert!(!text.contains("alt+↓"));
    for _ in 0..30 {
        t.code(KeyCode::Down);
        if !matches!(t.app.modal, Some(Modal::DiffHelp(_))) {
            t.press('?');
        }
    }
    let lines = super::diff_draw::help_lines(&t.app.keymap, false, false);
    assert!(!lines.iter().any(|(_, _, l)| l == "Show / hide file tree"));
    assert!(lines
        .iter()
        .any(|(_, k, l)| k == "x" && l == "Close diff viewer"));
    t.code(KeyCode::Esc);
    assert!(!t.has("Diff shortcuts"));
    assert!(t.app.diff.is_some());
}

fn opens_help(code: KeyCode, modifiers: KeyModifiers) {
    let mut t = T::new(hunks(), 160, 40);
    assert!(t.has("const first") && t.has("? help"));
    t.key(code, modifiers);
    assert!(t.has("Diff shortcuts"));
    assert!(t.has("Next file"));
    t.code(KeyCode::Esc);
    assert!(!t.has("Diff shortcuts"));
    assert!(t.has("const first"));
}

#[test]
fn oc_the_viewer_opens_help_from_question_mark_with_shift_false() {
    opens_help(KeyCode::Char('?'), KeyModifiers::NONE);
}

#[test]
fn oc_the_viewer_opens_help_from_question_mark_with_shift_true() {
    opens_help(KeyCode::Char('?'), KeyModifiers::SHIFT);
}

#[test]
fn oc_the_viewer_opens_help_from_slash_with_shift_true() {
    opens_help(KeyCode::Char('/'), KeyModifiers::SHIFT);
}

fn sticky(width: u16) {
    let files = (0..4).map(|i| hunk(&format!("src/file{i}.txt"))).collect();
    let mut t = T::new(files, width, 18);
    let g = t.geom();
    let l = t.v().layout(&g);
    let next = l.card(1).unwrap().header;
    t.vm().scroll = 5;
    assert_eq!(t.title_row("src/file0.txt"), Some(g.view_y as usize));
    assert_eq!(t.text().matches("src/file0.txt").count(), 1);
    t.resize(if width == 160 { 80 } else { 160 }, 22);
    let g2 = t.geom();
    assert_eq!(t.title_row("src/file0.txt"), Some(g2.view_y as usize));
    t.resize(width, 18);
    assert_eq!(t.title_row("src/file0.txt"), Some(g.view_y as usize));
    if width == 160 {
        t.code(KeyCode::Tab);
        assert_eq!(t.title_row("src/file0.txt"), Some(g.view_y as usize));
    }
    t.vm().scroll = next + 3;
    assert_eq!(t.title_row("src/file1.txt"), Some(g.view_y as usize));
    assert!(t.title_row("src/file0.txt").is_none());
    let x = g.patch_x + g.inset + 1;
    let before = t.bg(x, g.view_y);
    t.press('m');
    assert_ne!(t.bg(x, g.view_y), before);
    assert_eq!(t.title_row("src/file1.txt"), Some(g.view_y as usize));
    t.vm().scroll = 0;
    assert_eq!(t.title_row("src/file0.txt"), Some(g.view_y as usize));
    t.press('s');
    assert!(t.title_row("src/file1.txt").is_none() || t.v().visible_files() == vec![0]);
    t.vm().scroll = 5;
    let shown = t.v().visible_files()[0];
    let name = format!("src/file{shown}.txt");
    assert_eq!(t.title_row(&name), Some(g.view_y as usize));
}

#[test]
fn oc_file_titles_stick_to_the_viewport_and_hand_off_while_scrolling_at_80_columns() {
    sticky(80);
}

#[test]
fn oc_file_titles_stick_to_the_viewport_and_hand_off_while_scrolling_at_160_columns() {
    sticky(160);
}

#[test]
fn oc_split_diff_filler_keeps_the_full_card_background() {
    let f = FileDiff::from_patch(
        ".gitignore",
        Status::Modified,
        "--- a/.gitignore\n+++ b/.gitignore\n@@ -1 +1,2 @@\n node_modules/\n+artifacts/\n",
    );
    let t = T::new(vec![f], 160, 20);
    assert!(t.has("artifacts/"));
    let row = t.find("artifacts/").unwrap() as u16;
    let g = t.geom();
    let card = g.patch_x + g.inset;
    let p = theme::current();
    for x in card..card + g.card_w() / 2 {
        assert_eq!(t.bg(x, row), p.diff_context_bg, "{x}");
    }
}

fn collapses(kind: &str) {
    let (file, content) = match kind {
        "patch" => (hunk("src/file.txt"), "const first"),
        "image" => (
            plain("assets/preview.png", Status::Modified),
            "Image preview is not shown",
        ),
        _ => (plain("bun.lock", Status::Modified), "No patch available"),
    };
    let name = file.file.clone();
    let mut t = T::new(vec![file], 80, if kind == "patch" { 16 } else { 24 });
    assert!(t.has(content));
    if kind == "patch" {
        t.vm().scroll = 4;
        assert!(t.scroll() > 0);
    }
    t.press('m');
    let g = t.geom();
    let l = t.v().layout(&g);
    let c = l.card(0).unwrap();
    assert_eq!(c.end - c.header, 2);
    // Compact cards are flush with the pane's edges.
    let header = t.title_row(&name).unwrap();
    assert_eq!(t.row(header as u16 + 1).trim(), "▀".repeat(80));
    assert_eq!(header, g.view_y as usize);
    assert!(t.has(&name) && t.has("✓"));
    assert!(!t.has(content));
    t.press('m');
    assert!(t.has(content));
    let l = t.v().layout(&t.geom());
    let c = l.card(0).unwrap();
    assert!(c.end - c.header > 2);
    let header = t.title_row(&name).unwrap();
    assert_eq!(t.row(header as u16 + 1).trim(), "");
    assert!(!t.has("✓"));
    if kind == "patch" {
        t.press(']');
        assert!(t.scroll() > 0);
    }
}

#[test]
fn oc_reviewing_collapses_a_patch_body_and_unmarking_reopens_it() {
    collapses("patch");
}

// (adapted) The image card has CodeSmith's note where OpenCode previews it.
#[test]
fn oc_reviewing_collapses_a_image_body_and_unmarking_reopens_it() {
    collapses("image");
}

#[test]
fn oc_reviewing_collapses_a_fallback_body_and_unmarking_reopens_it() {
    collapses("fallback");
}

#[test]
fn oc_reviewing_after_scrolling_targets_the_visible_file_rather_than_the_initial_file() {
    let mut t = T::new(vec![hunk("src/file0.txt"), hunk("src/file1.txt")], 160, 16);
    let g = t.geom();
    let l = t.v().layout(&g);
    // Scroll to the second file's second hunk.
    let row = l.hunks.iter().find(|h| h.0 == 1 && h.1 == 0).unwrap().2;
    t.vm().scroll = row;
    t.press('m');
    let text = t.text();
    assert!(
        text.lines()
            .any(|l| l.contains("file1.txt") && l.contains('✓')),
        "{text}"
    );
    assert!(!text
        .lines()
        .any(|l| l.contains("file0.txt") && l.contains('✓')));
}

fn single_review(width: u16) {
    let files = (0..3).map(|i| hunk(&format!("src/file{i}.txt"))).collect();
    let mut t = T::new(files, width, 24);
    t.press('s');
    if width == 160 {
        t.code(KeyCode::Tab);
    }
    t.press('m');
    assert_eq!(t.v().visible_files(), vec![1]);
    assert!(t.has("const first"));
    t.press('p');
    assert_eq!(t.v().visible_files(), vec![0]);
    assert!(!t.v().layout(&t.geom()).items.iter().any(|i| matches!(
        i,
        diff_view::Item::Unified { .. } | diff_view::Item::Split { .. }
    )));
    t.press('m');
    assert!(t.has("const first"));
    assert_eq!(t.v().visible_files(), vec![0]);
    t.press('n');
    t.press('m');
    assert_eq!(t.v().visible_files(), vec![2]);
    t.press('m');
    assert_eq!(t.v().visible_files(), vec![2]);
    assert!(!t.has("const first"));
    t.press('m');
    assert!(!t.has("const first"));
    t.press('?');
    assert!(t.has("Review + next / reopen"));
}

#[test]
fn oc_single_file_review_advances_but_reopening_and_the_last_file_stay_put_at_80_columns() {
    single_review(80);
}

#[test]
fn oc_single_file_review_advances_but_reopening_and_the_last_file_stay_put_at_160_columns() {
    single_review(160);
}

// (adapted) CodeSmith has no session tabs to steal the keys.
fn alt_arrows(after_tab: bool) {
    let mut t = T::new(many(40), 160, 24);
    // Session tabs are on (alt+arrows cycle them outside the viewer).
    t.app.tabs = Some(Default::default());
    if after_tab {
        t.code(KeyCode::Tab);
    }
    t.key(KeyCode::Down, KeyModifiers::ALT);
    assert!(
        t.row(t.geom().view_y).contains("file01.txt"),
        "{}",
        t.text()
    );
    t.key(KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(t.scroll(), 0);
}

#[test]
fn oc_alt_arrows_navigate_files_rather_than_session_tabs_after_tab_false() {
    alt_arrows(false);
}

#[test]
fn oc_alt_arrows_navigate_files_rather_than_session_tabs_after_tab_true() {
    alt_arrows(true);
}

fn fallback_padding(file: FileDiff, message: &str) {
    let name = file.file.clone();
    let t = T::new(vec![file], 80, 20);
    assert!(t.has(message));
    let lines = t.screen();
    let title = t.title_row(&name).unwrap();
    let row = t.find(message).unwrap();
    assert_eq!(lines[row].find(message), lines[title].find(&name));
    assert_eq!(lines[row + 1].trim(), "");
    let g = t.geom();
    let p = theme::current();
    assert_eq!(t.bg(g.patch_x, row as u16 + 1), p.diff_context_bg);
    assert_eq!(
        t.bg(g.patch_x + g.card_w() - 1, row as u16 + 1),
        p.diff_context_bg
    );
}

#[test]
fn oc_pads_bun_lock_fallback_messages_inside_the_card_without_reading_images() {
    fallback_padding(
        plain("bun.lock", Status::Modified),
        "No patch available for this file.",
    );
}

#[test]
fn oc_pads_assets_old_png_fallback_messages_inside_the_card_without_reading_images() {
    fallback_padding(plain("assets/old.png", Status::Deleted), "Deleted image.");
}

#[test]
fn oc_ctrl_d_and_ctrl_u_scroll_the_patch_viewport_by_half_a_page_without_closing() {
    let mut t = T::new(many(40), 160, 24);
    let h = t.geom().view_h as usize;
    t.ctrl('d');
    assert!(t.app.diff.is_some());
    assert_eq!(t.scroll(), h / 2);
    t.ctrl('u');
    assert_eq!(t.scroll(), 0);
    t.ctrl('f');
    assert_eq!(t.scroll(), h);
    t.ctrl('b');
    assert_eq!(t.scroll(), 0);
}

// (adapted) No clicks; Tab, even bound to the retired `diff.switch_focus`,
// does nothing.
fn tab_never_redirects(height: u16) {
    let mut t = T::with(
        fake(many(40)),
        Opts {
            w: 160,
            h: height,
            keys: json!({"diff.switch_focus": "tab"}),
            ..Opts::default()
        },
    );
    t.code(KeyCode::Tab);
    t.press('j');
    assert_eq!(t.scroll(), 1);
    assert_eq!(t.v().tree_scroll.get(), 0);
    t.press('k');
    assert_eq!(t.scroll(), 0);
    t.code(KeyCode::Down);
    assert_eq!(t.scroll(), 1);
    t.code(KeyCode::Up);
    assert_eq!(t.scroll(), 0);
    t.ctrl('d');
    let half = t.geom().view_h as usize / 2;
    assert!(t.app.diff.is_some());
    assert_eq!(t.scroll(), half);
    assert_eq!(t.v().tree_scroll.get(), 0);
    t.ctrl('u');
    assert_eq!(t.scroll(), 0);
    t.ctrl('f');
    assert_eq!(t.scroll(), t.geom().view_h as usize);
    assert_eq!(t.v().tree_scroll.get(), 0);
}

#[test]
fn oc_tab_and_file_clicks_never_redirect_scrolling_keys_into_the_tree_at_20_rows() {
    tab_never_redirects(20);
}

#[test]
fn oc_tab_and_file_clicks_never_redirect_scrolling_keys_into_the_tree_at_32_rows() {
    tab_never_redirects(32);
}

// (adapted) A folder is closed as a click closes it; the keys that try to
// move the tree do nothing to it.
#[test]
fn oc_folders_are_mouse_controlled_and_n_p_reveal_files_inside_collapsed_folders() {
    let files = vec![
        hunk("src/a/first.txt"),
        hunk("src/b/second.txt"),
        hunk("test/third.txt"),
    ];
    let mut t = T::new(files, 160, 28);
    let tree = |t: &T| {
        t.screen()
            .iter()
            .map(|l| l.chars().take(40).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(tree(&t).contains("first.txt"));
    let src = t
        .v()
        .tree
        .nodes
        .iter()
        .find(|n| n.name == "src")
        .unwrap()
        .id;
    t.vm().toggle_folder(src);
    t.draw();
    assert!(tree(&t).contains("▸ src"));
    assert!(!tree(&t).contains("first.txt"));
    for c in ['\t', 'l', '}', 'h'] {
        t.press(c);
    }
    assert!(tree(&t).contains("▸ src"));
    t.press('n');
    assert!(tree(&t).contains("▾ src"));
    assert!(tree(&t).contains("second.txt"));
    assert!(
        t.row(t.geom().view_y).contains("src/b/second.txt"),
        "{}",
        t.text()
    );
    t.press('p');
    assert_eq!(t.scroll(), 0);
}

#[test]
fn oc_n_p_jump_between_files_and_gg_g_always_navigate_the_diff() {
    let mut t = T::new(many(40), 160, 24);
    t.press('n');
    assert!(t.scroll() > 0);
    assert!(t.has("file01.txt"));
    t.press('p');
    assert_eq!(t.scroll(), 0);
    t.key(KeyCode::Char('G'), KeyModifiers::SHIFT);
    let max = t.v().max_scroll(&t.geom());
    assert_eq!(t.scroll(), max);
    t.press('g');
    t.press('g');
    assert_eq!(t.scroll(), 0);
    t.code(KeyCode::Tab);
    t.key(KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert_eq!(t.scroll(), max);
    assert_eq!(t.v().tree_scroll.get(), 0);
    t.press('g');
    t.press('g');
    t.press('m');
    assert!(t
        .text()
        .lines()
        .any(|l| l.contains("file00.txt") && l.contains('✓')));
}

fn big_added(width: u16) {
    let mut lines = vec!["{".to_string()];
    for i in 0..7500 {
        let value = "value".repeat(if i == 777 {
            2000
        } else if i % 7 == 0 {
            24
        } else {
            1
        });
        lines.push(format!(
            "  \"row-{i:04}\": \"{value}\"{}",
            if i == 7499 { "" } else { "," }
        ));
    }
    lines.push("}".into());
    let mut t = T::new(vec![added("snapshot.json", &lines)], width, 24);
    assert!(t.has("row-0000"));
    t.press('G');
    assert!(t.has("row-7499"), "{}", t.text());
    t.press('g');
    t.press('g');
    assert!(t.has("row-0000"));
    t.resize(if width == 80 { 160 } else { 80 }, 20);
    t.key(KeyCode::End, KeyModifiers::NONE);
    assert!(t.has("row-7499"));
}

// (adapted) Only the rows in view are drawn, so nothing is split up.
#[test]
fn oc_virtualizes_a_large_added_file_at_80_columns_without_losing_its_end() {
    big_added(80);
}

#[test]
fn oc_virtualizes_a_large_added_file_at_160_columns_without_losing_its_end() {
    big_added(160);
}

#[test]
fn oc_keeps_the_line_number_gutter_the_same_width_across_virtual_chunks() {
    let lines: Vec<String> = (1..=10500).map(|i| format!("line-{i:05}")).collect();
    let mut t = T::new(vec![added("wide.txt", &lines)], 120, 24);
    let column = |t: &T, s: &str| t.screen().iter().find_map(|l| column(l, s));
    let top = column(&t, "line-00001");
    t.key(KeyCode::End, KeyModifiers::NONE);
    assert!(top.is_some());
    assert_eq!(column(&t, "line-10500"), top);
}

#[test]
fn oc_highlights_virtual_chunks_with_whole_file_syntax_context() {
    let lines: Vec<String> = (0..3200)
        .map(|i| match i {
            370 => "/*".into(),
            399 => "*/".into(),
            i if i > 370 && i < 399 => format!("  comment {i}"),
            i => format!("const value{i} = {i}"),
        })
        .collect();
    let mut t = T::new(vec![added("big.ts", &lines)], 120, 40);
    t.vm().scroll = 360;
    let color = |t: &T, s: &str| {
        let screen = t.screen();
        let (y, line) = screen
            .iter()
            .enumerate()
            .find(|(_, l)| l.contains(s))
            .unwrap();
        let x = line[..line.find(s).unwrap()].chars().count() as u16;
        t.fg(x + 2, y as u16)
    };
    assert_ne!(color(&t, "comment 375"), color(&t, "value365"));
    assert_eq!(color(&t, "comment 390"), color(&t, "comment 375"));
}

#[test]
fn oc_file_navigation_and_review_still_work_after_a_virtualized_patch() {
    let lines: Vec<String> = (0..6600).map(|i| format!("added line {i}")).collect();
    let mut t = T::new(
        vec![added("a-large.txt", &lines), hunk("b-small.txt")],
        160,
        24,
    );
    t.vm().scroll = 2700;
    t.press('[');
    assert!(t.has("added line 0"), "{}", t.text());
    t.press(']');
    assert!(t.has("b-small.txt"));
    t.press('n');
    assert!(t.has("b-small.txt") && t.has("const first"));
    t.press('p');
    assert!(t.has("a-large.txt"));
    t.press('m');
    assert!(!t.has("added line 0"));
    t.press('m');
    assert!(t.has("added line 0"));
}

// (adapted) The source label opens the chooser by its key (no clicks).
fn sidebar_source_picker(width: u16) {
    let mut t = T::new(hunks(), width, 20);
    assert_eq!(
        t.last_diff().unwrap(),
        (Source::Branch, Some("refs/heads/v2".into()))
    );
    assert!(t.has("const first"));
    assert!(t.row(1).contains("All"));
    t.press('d');
    assert!(t.source_open());
    t.pick(2);
    assert!(!t.source_open() && t.has("Uncommitted") && t.has("const first"));
    assert_eq!(t.last_diff().unwrap(), (Source::Working, None));
    t.press('d');
    t.pick(0);
    assert!(t.has("All") && t.has("const first"));
    assert_eq!(
        t.last_diff().unwrap(),
        (Source::Branch, Some("refs/heads/v2".into()))
    );
    assert_eq!(t.calls().base, 1);
    assert!(!t.app.diff_prefs_changed);
}

#[test]
fn oc_the_sidebar_source_picker_switches_vcs_sources_at_100_columns() {
    sidebar_source_picker(100);
}

#[test]
fn oc_the_sidebar_source_picker_switches_vcs_sources_at_160_columns() {
    sidebar_source_picker(160);
}

// ----- tui/test/cli/tui/diff-viewer-file-tree --------------------------------
//
// (adapted) OpenCode renders the tree on its own, titled `Files 0/5
// reviewed`, with `?` for files of no status; here it is the viewer's tree
// (32 columns on a 128-column screen), titled by the source, and every
// file has a status.

fn tree_lines(t: &T) -> Vec<String> {
    let w = t.geom().tree_w as usize;
    t.screen()
        .iter()
        .map(|l| {
            let part: String = l.chars().take(w).collect();
            part.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|l| !l.is_empty())
        .collect()
}

fn tree_of(files: Vec<FileDiff>) -> T {
    let t = T::new(files, 128, 20);
    assert_eq!(t.geom().tree_w, 32);
    t
}

#[test]
fn oc_defaults_to_text_line_file_icons_and_triangle_folders_with_straight_rails() {
    let files = [
        "z-file.ts",
        "b/file.ts",
        "a/zeta.ts",
        "b/alpha.ts",
        "a/alpha.ts",
    ]
    .iter()
    .map(|f| hunk(f))
    .collect();
    let t = tree_of(files);
    let lines = tree_lines(&t);
    assert_eq!(
        lines[..8],
        [
            "All · vs v2 0/5",
            "▾ a",
            "│ ≡ alpha.ts M",
            "│ ≡ zeta.ts M",
            "▾ b",
            "│ ≡ alpha.ts M",
            "│ ≡ file.ts M",
            "≡ z-file.ts M",
        ]
    );
    let tree: String = lines.join("\n");
    assert!(!tree.chars().any(|c| "├└─".contains(c)));
}

#[test]
fn oc_tree_layout_uses_two_cell_horizontal_sidebar_padding() {
    let t = tree_of(vec![
        FileDiff::from_patch("src/a.ts", Status::Added, "@@ -0,0 +1 @@\n+a"),
        hunk("README.md"),
    ]);
    let rows: Vec<String> = t
        .screen()
        .iter()
        .map(|l| l.chars().take(32).collect::<String>())
        .collect();
    let body: Vec<&String> = rows
        .iter()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(column(body[0], "All"), Some(2));
    let file = body.iter().find(|l| l.contains("README.md")).unwrap();
    assert_eq!(column(file, "≡"), Some(2));
    let chars: Vec<char> = file.chars().collect();
    assert_eq!(chars[29..32].iter().collect::<String>(), "M  ");
    for l in body {
        let c: Vec<char> = l.chars().collect();
        assert!(l.starts_with("  ") && c[30] == ' ' && c[31] == ' ', "{l}");
    }
}

fn heading_one_row_down() {
    let t = tree_of(vec![hunk("README.md")]);
    let lines = t.screen();
    assert_eq!(lines[0].chars().take(32).collect::<String>().trim(), "");
    assert_eq!(column(&lines[1], "All"), Some(2));
    let readme = lines.iter().find(|l| l.contains("≡ README.md")).unwrap();
    assert_eq!(column(readme, "≡"), Some(2));
}

// (adapted) In the default theme; the layout does not depend on the mode.
#[test]
fn oc_full_top_padding_keeps_the_heading_one_row_down_in_dark_mode() {
    heading_one_row_down();
}

#[test]
fn oc_full_top_padding_keeps_the_heading_one_row_down_in_light_mode() {
    heading_one_row_down();
}

#[test]
fn oc_does_not_render_text_markers_for_selected_files() {
    let mut t = tree_of(vec![hunk("src/config/tui.ts"), hunk("README.md")]);
    let unselected = tree_lines(&t);
    t.press('n');
    assert!(t.v().tree_selected().is_some());
    let selected = tree_lines(&t);
    for lines in [&selected, &unselected] {
        assert!(lines.contains(&"▾ src/config".to_string()));
        assert!(!lines.iter().any(|l| l.contains('*')));
    }
}

#[test]
fn oc_renders_collapsed_and_expanded_directory_rows() {
    let mut t = tree_of(vec![hunk("src/config/tui.ts"), hunk("README.md")]);
    assert_eq!(
        tree_lines(&t)[1..4],
        ["▾ src/config", "│ ≡ tui.ts M", "≡ README.md M"]
    );
    let src = t
        .v()
        .tree
        .nodes
        .iter()
        .find(|n| n.name == "src")
        .unwrap()
        .id;
    t.vm().toggle_folder(src);
    t.draw();
    assert_eq!(tree_lines(&t)[1..3], ["▸ src/config", "≡ README.md M"]);
}

#[test]
fn oc_keeps_rows_quiet_straight_rails_single_status_letters_no_hooks_or_dots() {
    let t = tree_of(vec![
        FileDiff::from_patch("src/a.ts", Status::Added, "@@ -0,0 +1 @@\n+a"),
        hunk("src/b.ts"),
        FileDiff::from_patch("test/a.ts", Status::Deleted, "@@ -1 +0,0 @@\n-a"),
    ]);
    let lines = tree_lines(&t);
    assert_eq!(
        lines[1..6],
        ["▾ src", "│ ≡ a.ts A", "│ ≡ b.ts M", "▾ test", "│ ≡ a.ts D"]
    );
    // Below the source header, whose `·` is OpenCode's too.
    let tree = lines[1..].join("\n");
    assert!(!tree
        .chars()
        .any(|c| "├└─·".contains(c) || ('\u{E000}'..='\u{F8FF}').contains(&c)));
}

#[test]
fn oc_narrow_collapsed_chains_drop_whole_leading_segments() {
    // A 26-column tree leaves 18 columns for a folder chain's name.
    let name = "packages/tui/src/feature-plugins/system";
    assert_eq!(super::diff_draw::truncate_chain(name, 18), "…/system");
    assert!(!super::diff_draw::truncate_chain(name, 18).contains("…/f"));
}

// ----- tui/test/session-panel -----------------------------------------------
//
// OpenCode's session fixture has four changed files and a 160-column
// screen; the panel opens docked beside the session.

fn panel(w: u16, keys: Value) -> T {
    T::with(
        fake(many(4)),
        Opts {
            w,
            h: 36,
            keys,
            fullscreen: false,
            ..Opts::default()
        },
    )
}

/// The default `pane.focus.*` keys: the leader, then an arrow.
fn focus(t: &mut T, left: bool) {
    t.ctrl('x');
    t.code(if left { KeyCode::Left } else { KeyCode::Right });
}

#[test]
fn oc_diff_focuses_the_production_session_panel_without_sending_typing_to_the_prompt() {
    let mut t = panel(160, json!({}));
    t.app
        .entries
        .push(super::app::Entry::Assistant("Transcript item 11".into()));
    let g = t.geom();
    assert!(g.docked && g.w < t.w);
    assert!(t.app.diff_focused());
    assert!(t.text().contains("Transcript item 11"));
    t.typ("xyz");
    let action = t.app.on_key(key_event(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(action, None);
    assert_eq!(t.app.input.text(), "");
    focus(&mut t, true);
    assert!(!t.app.diff_focused());
    t.typ("fjm");
    assert_eq!(t.app.input.text(), "fjm");
    assert!(t.geom().docked);
    assert_eq!(t.scroll(), 0);
    assert_eq!(t.count(), "0/4");
    focus(&mut t, false);
    assert!(t.app.diff_focused());
    t.press('j');
    assert!(t.scroll() > 0);
    assert_eq!(t.app.input.text(), "fjm");
    t.press('q');
    assert!(t.app.diff.is_none());
    assert!(t.text().contains("Transcript item 11"));
    assert_eq!(t.app.input.text(), "fjm");
}

#[test]
fn oc_repeated_fullscreen_toggles_retain_the_viewer_transcript_scroll_and_reviewed_files() {
    let mut t = panel(160, json!({}));
    let split = t.geom().w;
    t.app.scroll = 4;
    t.press('m');
    assert_eq!(t.count(), "1/4");
    t.press('n');
    assert!(t.scroll() > 0);
    t.press('j');
    let top = t.scroll();
    for full in [true, false, true, false] {
        t.press('f');
        assert_eq!(t.geom().w, if full { t.w } else { split });
        assert_eq!(t.scroll(), top);
        assert_eq!(t.count(), "1/4");
        assert_eq!(t.app.scroll, 4);
        assert!(t.app.diff_focused());
        assert_eq!(t.calls().base, 1);
        assert_eq!(t.calls().diff.len(), 1);
    }
    t.code(KeyCode::Home);
    assert!(t.has("file00.txt"));
    let header = t.title_row("file00.txt").unwrap();
    assert!(t.row(header as u16).contains('✓'));
    assert_eq!(t.last_diff().unwrap().1.as_deref(), Some("refs/heads/v2"));
}

#[test]
fn oc_the_diff_panel_reopens_in_the_presentation_last_chosen_with_the_fullscreen_toggle() {
    let mut t = panel(160, json!({}));
    assert!(t.geom().docked);
    t.press('f');
    assert_eq!(t.geom().w, t.w);
    assert!(t.app.diff_prefs.fullscreen && std::mem::take(&mut t.app.diff_prefs_changed));
    t.reopen();
    assert_eq!(t.geom().w, t.w);
    t.press('f');
    assert!(t.geom().docked);
    assert!(!t.app.diff_prefs.fullscreen);
    t.reopen();
    assert!(t.geom().docked);
    // Too narrow to split, full screen is forced and the toggle ignored.
    t.app.diff_prefs_changed = false;
    t.resize(100, 36);
    assert_eq!(t.geom().w, 100);
    t.press('f');
    assert!(!t.app.diff_prefs.fullscreen && !t.app.diff_prefs_changed);
    t.resize(160, 36);
    assert!(t.geom().docked);
}

#[test]
fn oc_docked_tree_commands_leave_the_full_screen_tree_preference_unchanged() {
    let mut t = T::with(
        fake(many(4)),
        Opts {
            w: 160,
            h: 36,
            fullscreen: false,
            tree: Some(true),
            ..Opts::default()
        },
    );
    assert!(!t.geom().tree);
    t.press('b');
    assert!(!t.geom().tree);
    t.press('f');
    assert!(t.geom().tree);
    t.press('b');
    assert!(!t.geom().tree);
    t.press('f');
    assert!(t.geom().docked);
    t.press('f');
    assert!(!t.geom().docked && !t.geom().tree);
    assert_eq!(t.calls().diff.len(), 1);
}

fn grows(width: u16) {
    let mut t = panel(width, json!({}));
    assert_eq!(t.geom().w, width);
    t.press('f');
    assert_eq!(t.geom().w, width);
    assert_eq!(t.app.input.text(), "");
    t.resize(160, 36);
    let grown = t.geom().w;
    t.press('f');
    assert_ne!(t.geom().w, grown);
    t.press('f');
    assert_eq!(t.geom().w, grown);
    if t.geom().w == t.w {
        t.press('f');
    }
    assert!(t.geom().x > 0);
    t.resize(80, 36);
    assert_eq!(t.geom().w, 80);
    assert!(t.app.diff_focused());
    assert_eq!(t.calls().base, 1);
    assert_eq!(t.calls().diff.len(), 1);
}

#[test]
fn oc_a_panel_opened_at_60_columns_can_split_after_growing_without_remounting() {
    grows(60);
}

#[test]
fn oc_a_panel_opened_at_80_columns_can_split_after_growing_without_remounting() {
    grows(80);
}

#[test]
fn oc_narrow_terminals_show_the_panel_full_screen_then_compact_and_dock_again_without_remounting() {
    let mut t = T::with(
        fake(many(4)),
        Opts {
            w: 160,
            h: 36,
            fullscreen: false,
            tree: Some(true),
            ..Opts::default()
        },
    );
    t.press('m');
    assert_eq!(t.count(), "1/4");
    t.press('j');
    let top = t.scroll();
    assert!(top > 0);
    // Below the split threshold the panel takes over with the full layout.
    t.resize(107, 36);
    let g = t.geom();
    assert_eq!(g.w, 107);
    assert!(g.tree && !g.compact);
    assert_eq!(t.scroll(), top);
    assert!(t.row(1).contains("1/4"));
    t.press('j');
    assert!(t.scroll() > top);
    assert_eq!(t.app.input.text(), "");
    // Narrower still: the compact rendering, with no prompt to focus.
    t.resize(80, 36);
    let g = t.geom();
    assert!(!g.tree && g.compact && !g.docked);
    let footer = t.row(35);
    assert!(
        footer.trim_start().starts_with("esc close") && footer.trim_end().ends_with("? see all")
    );
    assert!(!footer.contains("focus prompt"));
    focus(&mut t, true);
    assert!(t.app.diff_focused());
    t.resize(108, 36);
    let g = t.geom();
    assert!(g.docked && g.w < 108 && g.x > 0 && !g.tree);
    assert!(t.row(34).contains("ctrl+x ← focus prompt"), "{}", t.text());
    focus(&mut t, true);
    assert!(!t.app.diff_focused());
    let docked = t.scroll();
    t.typ("jq");
    assert_eq!(t.app.input.text(), "jq");
    assert_eq!(t.scroll(), docked);
    assert!(t.app.diff.is_some());
}

#[test]
fn oc_panel_focus_scrolling_and_fullscreen_use_customized_keys() {
    let mut t = panel(
        240,
        json!({
            "leader": "ctrl+space",
            "pane.focus.left": "<leader>h",
            "pane.focus.right": "<leader>l",
            "diff.toggle_fullscreen": "f7",
            "diff.down": "x",
            "diff.up": "z",
        }),
    );
    let split = t.geom().w;
    let footer = t.row(34);
    assert!(footer.contains("ctrl+space h focus prompt"), "{footer}");
    assert!(footer.contains("? see all"));
    t.press('f');
    t.press('j');
    focus(&mut t, true);
    assert_eq!(t.geom().w, split);
    assert_eq!(t.scroll(), 0);
    assert!(t.app.diff_focused());
    t.press('x');
    assert!(t.scroll() > 0);
    t.code(KeyCode::F(7));
    assert_eq!(t.geom().w, t.w);
    t.code(KeyCode::F(7));
    assert_eq!(t.geom().w, split);
    t.ctrl(' ');
    t.press('h');
    assert!(!t.app.diff_focused());
    t.ctrl(' ');
    t.press('l');
    assert!(t.app.diff_focused());
    assert_eq!(t.app.input.text(), "");
}

fn disabled_panel_keys(off: Value) {
    let ids = [
        "pane.focus.left",
        "pane.focus.right",
        "diff.toggle_fullscreen",
        "diff.down",
        "diff.up",
        "diff.next_file",
        "diff.previous_file",
        "diff.next_hunk",
        "diff.previous_hunk",
        "diff.mark_reviewed",
        "diff.close",
        "diff.help",
    ];
    let keys: serde_json::Map<String, Value> =
        ids.iter().map(|id| (id.to_string(), off.clone())).collect();
    let mut t = panel(240, Value::Object(keys));
    let split = t.geom().w;
    t.typ("fjknpm?[]q");
    focus(&mut t, true);
    assert!(t.app.diff_focused());
    assert_eq!(t.geom().w, split);
    assert_eq!(t.scroll(), 0);
    assert_eq!(t.count(), "0/4");
    assert_eq!(t.app.input.text(), "");
    assert!(!t.has("? help"));
    let footer = t.row(34);
    assert!(
        !footer.contains("ctrl+x") && !footer.contains("see all"),
        "{footer}"
    );
    // (adapted) A click on the prompt focuses it; then the right-pane key
    // has nothing to fall back on.
    t.vm().focused = false;
    focus(&mut t, false);
    assert!(!t.app.diff_focused());
}

#[test]
fn oc_disabled_panel_keys_none_have_no_behavioral_or_hint_fallbacks() {
    disabled_panel_keys(json!("none"));
}

#[test]
fn oc_disabled_panel_keys_false_have_no_behavioral_or_hint_fallbacks() {
    disabled_panel_keys(json!(false));
}

fn ask(t: &mut T) -> tokio::sync::oneshot::Receiver<codesmith_core::permission::Reply> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    t.app.on_event(super::bridge::Ev::Permission(
        codesmith_core::permission::Request {
            action: "bash".into(),
            resources: vec!["ls".into()],
            save: vec![],
            title: "ls".into(),
        },
        tx,
    ));
    rx
}

// (adapted) CodeSmith allows with `y` (a stray Enter rejects), so once the
// prompt has the focus, `y` answers.
#[test]
fn oc_enter_in_the_diff_panel_cannot_approve_a_pending_permission_in_either_presentation() {
    let mut t = panel(160, json!({}));
    let mut rx = ask(&mut t);
    let split = t.geom().w;
    t.code(KeyCode::Enter);
    t.press('y');
    assert!(rx.try_recv().is_err());
    assert!(matches!(t.app.modal, Some(Modal::Permission { .. })));
    t.press('f');
    assert_eq!(t.geom().w, t.w);
    t.code(KeyCode::Enter);
    assert!(rx.try_recv().is_err());
    assert!(t.has("Waiting for your answer"));
    t.press('f');
    assert_eq!(t.geom().w, split);
    focus(&mut t, true);
    assert!(!t.app.diff_focused());
    t.press('y');
    assert_eq!(
        rx.try_recv().unwrap(),
        codesmith_core::permission::Reply::Once
    );
}

// (adapted) OpenCode's form is a question here; its input starts empty.
fn question_cannot_capture(pending: bool) {
    let question = |t: &mut T| {
        let (tx, rx) = tokio::sync::oneshot::channel();
        t.app.on_event(super::bridge::Ev::Question(
            vec![codesmith_core::question::Prompt {
                question: "Target?".into(),
                header: "Form".into(),
                options: vec![],
                multiple: false,
            }],
            tx,
        ));
        rx
    };
    let mut early = None;
    let mut t = panel(160, json!({}));
    if pending {
        t.close();
        early = Some(question(&mut t));
        t.open_on(Some("feature"));
    }
    let mut rx = early.unwrap_or_else(|| question(&mut t));
    let split = t.geom().w;
    t.press('j');
    assert!(t.scroll() > 0);
    t.press('m');
    assert_eq!(t.count(), "1/4");
    t.code(KeyCode::Enter);
    assert!(rx.try_recv().is_err());
    assert!(t.app.diff_focused());
    t.press('f');
    assert_eq!(t.geom().w, t.w);
    t.press('f');
    assert_eq!(t.geom().w, split);
    assert!(rx.try_recv().is_err());
    focus(&mut t, true);
    assert!(!t.app.diff_focused());
    t.press('x');
    let Some(Modal::Question { form, .. }) = &t.app.modal else {
        panic!("the question is still asked")
    };
    assert_eq!(form.editor.text(), "x");
    t.code(KeyCode::Enter);
    t.code(KeyCode::Enter);
    assert_eq!(rx.try_recv().unwrap(), Some(vec![vec!["x".to_string()]]));
}

#[test]
fn oc_a_form_cannot_capture_diff_keys_form_already_pending_true() {
    question_cannot_capture(true);
}

#[test]
fn oc_a_form_cannot_capture_diff_keys_form_already_pending_false() {
    question_cannot_capture(false);
}

// ----- tui/test/panel -------------------------------------------------------

// (adapted) The viewer is the panel; `/diff` while it is open closes it, as
// OpenCode's `/diff` does in a session, so the reopen half is dropped.
#[test]
fn oc_presentation_changes_preserve_the_selected_panel_identity() {
    let mut t = panel(160, json!({}));
    t.press('m');
    assert!(t.geom().docked);
    t.press('f');
    assert!(!t.geom().docked);
    assert_eq!(t.count(), "1/4");
    t.press('f');
    assert!(t.geom().docked);
    assert_eq!(t.count(), "1/4");
    assert_eq!(t.calls().diff.len(), 1);
}

#[test]
fn oc_narrow_geometry_overrides_presentation_without_discarding_the_users_choice() {
    let mut t = panel(160, json!({}));
    t.resize(diff_view::SPLIT_MIN_WIDTH - 1, 36);
    assert!(!t.geom().docked);
    t.press('f');
    t.resize(diff_view::SPLIT_MIN_WIDTH, 36);
    assert!(t.geom().docked);
    t.press('f');
    t.resize(60, 36);
    t.resize(160, 36);
    assert!(!t.geom().docked);
}

// ----- tui/test/config-v2 ---------------------------------------------------

#[test]
fn oc_validates_the_explicit_diff_source_defaults() {
    for s in ["branch", "committed", "working", "turn"] {
        let (got, errors) = super::settings::load(&json!({"diffs": {"source": s}}));
        assert!(errors.is_empty());
        assert_eq!(got.diffs.source, Source::parse(s));
        assert_eq!(got.diffs.source.map(Source::key), Some(s));
    }
    let (got, errors) = super::settings::load(&json!({"diffs": {}}));
    assert!(errors.is_empty());
    assert_eq!(got.diffs, super::settings::Diffs::default());
    let (_, errors) = super::settings::load(&json!({"diffs": {"source": "auto"}}));
    assert_eq!(errors.len(), 1);
}

// ----- CodeSmith's own ------------------------------------------------------

// The layout chosen in the viewer is kept against `tui.diffs`: a changed
// setting wins again.
#[test]
fn layout_choices_are_remembered_against_the_setting() {
    let c = super::settings::Diffs::default();
    let p = diff_view::Prefs {
        tree: false,
        single: true,
        view: Some(diff_view::View::Unified),
        fullscreen: true,
    };
    let text = diff_view::remember_text(&c, p);
    assert_eq!(diff_view::remembered(&c, Some(&text)), p);
    let changed = super::settings::Diffs {
        tree: Some(true),
        ..c
    };
    assert_eq!(
        diff_view::remembered(&changed, Some(&text)),
        diff_view::Prefs::default()
    );
}

// `/diff` toggles the viewer, which opens focused on the session's folder.
#[test]
fn slash_diff_toggles_the_viewer() {
    let mut a = App::new();
    let d = tempfile::tempdir().unwrap();
    a.toggle_diff(d.path().to_path_buf());
    assert!(a.diff.is_some() && a.diff_focused());
    let reqs = a.take_diff_requests();
    assert!(matches!(
        reqs[..],
        [Request {
            job: diff_view::Job::Vcs {
                source: Source::Branch,
                ..
            },
            ..
        }]
    ));
    a.toggle_diff(d.path().to_path_buf());
    assert!(a.diff.is_none());
}
