//! The diff viewer and review panel (`/diff`), ported from OpenCode's
//! `diff-viewer` plugin: the changes of a source (the branch against its
//! base, its commits only, uncommitted changes, or the session's last turn)
//! as file cards with a file tree beside them, hunk and file navigation,
//! review marks, and split or unified patches.
//!
//! It opens docked beside the session when the terminal is wide enough
//! ([`SPLIT_MIN_WIDTH`]) and full screen otherwise, or when toggled. Only
//! the rows in view are drawn: the layout is a list of row items (a file's
//! title, one patch line, …) with their heights, rebuilt only when what it
//! depends on changes.
//!
//! This module holds the state and the commands; `diff_draw` draws it.

use super::diff_source::{Base, FileDiff, LineKind, Source, Status};
use super::diff_tree::{self, Tree};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

/// The narrowest terminal a docked panel fits in: the session keeps its
/// preferred width beside a minimum-width pane (OpenCode's
/// `SESSION_PANEL_SPLIT_MIN_WIDTH`).
pub const SPLIT_MIN_WIDTH: u16 = 64 + 44;
/// The narrowest patch pane split view is offered at.
pub const MIN_SPLIT_WIDTH: u16 = 100;
const TREE_MIN_WIDTH: u16 = 30;
const TREE_MAX_WIDTH: u16 = 40;
/// Cards are inset this far from the pane's edges in the full layout.
const FULL_INSET: u16 = 2;
/// The full layout keeps its tree and inset cards while patches stay as
/// wide as the narrowest docked panel.
pub const FULL_LAYOUT_MIN_WIDTH: u16 = TREE_MIN_WIDTH + FULL_INSET * 2 + SPLIT_MIN_WIDTH / 2;
/// A file title's rows: the title and the row below it.
pub const HEADER_HEIGHT: usize = 2;
/// Files longer than this are shown without syntax colors.
const HIGHLIGHT_MAX_LINES: usize = 5_000;

/// Split or unified patches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Split,
    Unified,
}

impl View {
    pub fn key(self) -> &'static str {
        match self {
            View::Split => "split",
            View::Unified => "unified",
        }
    }
}

/// The viewer's remembered layout: `tui.diffs` and the choices made since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefs {
    pub tree: bool,
    pub single: bool,
    /// `None`: split when there is room.
    pub view: Option<View>,
    pub fullscreen: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            tree: true,
            single: false,
            view: None,
            fullscreen: false,
        }
    }
}

/// What the viewer remembers until the UI exits: the source last chosen
/// and the base chosen for each branch.
#[derive(Debug, Clone, Default)]
pub struct Memory {
    pub source: Option<Source>,
    /// Base refs by [`base_key`].
    pub bases: HashMap<String, String>,
}

/// The key a base choice is remembered under: the folder and the branch
/// checked out there.
pub fn base_key(dir: &std::path::Path, branch: Option<&str>) -> String {
    format!("{}\0{}", dir.display(), branch.unwrap_or(""))
}

/// Work for the runner to do off the UI thread; its answer comes back as
/// a [`Response`] with the same `seq`.
#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    /// Compare `dir` for a VCS source. `lookup` asks to resolve the base
    /// first (under `key`); otherwise `base` is the ref to compare with
    /// (the default branch when `None`).
    Vcs {
        dir: PathBuf,
        source: Source,
        base: Option<String>,
        lookup: Option<String>,
    },
    /// The session's last turn.
    Turn,
    /// The branches the base picker lists.
    Branches { dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub seq: u64,
    pub job: Job,
}

/// A finished [`Job`].
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Diff {
        seq: u64,
        /// The base lookup the job made: under which key, and what it found
        /// (`None`: nothing to compare with, the repository has no commit).
        lookup: Option<(String, Result<Option<Base>, String>)>,
        /// The base the comparison used.
        base: Option<Base>,
        /// `None`: the committed comparison has no base to use.
        files: Result<Option<Vec<FileDiff>>, String>,
    },
    Branches {
        seq: u64,
        branches: Result<Vec<String>, String>,
    },
}

/// Where a job's answers come from: git and the snapshot store, or a
/// test's stand-in.
pub trait Sources {
    fn base(&self, dir: &std::path::Path) -> Result<Option<Base>, String>;
    fn branches(&self, dir: &std::path::Path) -> Result<Vec<String>, String>;
    fn vcs_diff(
        &self,
        dir: &std::path::Path,
        source: Source,
        base: Option<&str>,
    ) -> Result<Vec<FileDiff>, String>;
    fn turn(&self) -> Result<Vec<FileDiff>, String>;
}

/// The real sources; `turn` diffs the session's last turn.
pub struct Git<F: Fn() -> Result<Vec<FileDiff>, String>>(pub F);

impl<F: Fn() -> Result<Vec<FileDiff>, String>> Sources for Git<F> {
    fn base(&self, dir: &std::path::Path) -> Result<Option<Base>, String> {
        super::diff_source::base(dir)
    }
    fn branches(&self, dir: &std::path::Path) -> Result<Vec<String>, String> {
        super::diff_source::branches(dir)
    }
    fn vcs_diff(
        &self,
        dir: &std::path::Path,
        source: Source,
        base: Option<&str>,
    ) -> Result<Vec<FileDiff>, String> {
        super::diff_source::vcs_diff(dir, source, base)
    }
    fn turn(&self) -> Result<Vec<FileDiff>, String> {
        (self.0)()
    }
}

/// Run a job (on a blocking thread): resolve the base first when asked,
/// and never compare with a guessed one.
pub fn run(job: Job, seq: u64, src: &dyn Sources) -> Response {
    match job {
        Job::Branches { dir } => Response::Branches {
            seq,
            branches: src.branches(&dir),
        },
        Job::Turn => Response::Diff {
            seq,
            lookup: None,
            base: None,
            files: src.turn().map(Some),
        },
        Job::Vcs {
            dir,
            source,
            base,
            lookup,
        } => {
            let mut found = None;
            let mut base = base.map(|r| Base {
                name: r.clone(),
                reference: r,
            });
            if let Some(key) = lookup {
                let b = src.base(&dir);
                found = Some((key, b.clone()));
                match b {
                    Ok(b) => base = b,
                    Err(e) => {
                        return Response::Diff {
                            seq,
                            lookup: found,
                            base: None,
                            files: Err(e),
                        }
                    }
                }
            }
            if source == Source::Committed && base.is_none() {
                return Response::Diff {
                    seq,
                    lookup: found,
                    base: None,
                    files: Ok(None),
                };
            }
            let files = src.vcs_diff(&dir, source, base.as_ref().map(|b| b.reference.as_str()));
            Response::Diff {
                seq,
                lookup: found,
                base,
                files: files.map(Some),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Load {
    Loading,
    Failed(String),
    /// A committed comparison with no base to compare with.
    Unavailable,
    Ready {
        base: Option<Base>,
    },
}

/// One row item of the patch pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// The half-row gap above a file's card (every file but the first).
    Gap(usize),
    /// A file's title and the row below it.
    Header(usize),
    /// A file with no patch to show: a note and a padding row.
    Message(usize),
    /// The row between two hunks.
    HunkGap(usize),
    Unified {
        file: usize,
        hunk: usize,
        line: usize,
    },
    Split {
        file: usize,
        hunk: usize,
        left: Option<usize>,
        right: Option<usize>,
    },
}

impl Item {
    pub fn file(self) -> usize {
        match self {
            Item::Gap(f) | Item::Header(f) | Item::Message(f) | Item::HunkGap(f) => f,
            Item::Unified { file, .. } | Item::Split { file, .. } => file,
        }
    }
}

/// Where a file's card is in the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Card {
    /// The first row of its node (its gap, or the title for the first).
    pub node: usize,
    pub header: usize,
    /// One past its last row.
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutKey {
    rev: u64,
    pub card: u16,
    pub split: bool,
    pub compact: bool,
}

/// The patch pane's rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub key: LayoutKey,
    pub items: Vec<Item>,
    /// Each item's first row.
    pub tops: Vec<usize>,
    pub total: usize,
    /// Cards in the order shown.
    pub cards: Vec<(usize, Card)>,
    /// Each hunk's first line: (file, hunk, row).
    pub hunks: Vec<(usize, usize, usize)>,
}

impl Layout {
    pub fn card(&self, file: usize) -> Option<Card> {
        self.cards.iter().find(|(f, _)| *f == file).map(|(_, c)| *c)
    }

    /// The item at `row` and the row within it.
    pub fn at(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total {
            return None;
        }
        let i = self.tops.partition_point(|&t| t <= row).saturating_sub(1);
        Some((i, row - self.tops[i]))
    }

    pub fn height(&self, i: usize) -> usize {
        self.tops.get(i + 1).copied().unwrap_or(self.total) - self.tops[i]
    }
}

/// Facts about a file the layout and drawing need, worked out once.
#[derive(Debug, Clone, Default)]
pub struct Prep {
    /// The widest line number's digits.
    pub digits: usize,
    /// Each hunk's lines' index into the old (context and removed) and new
    /// (context and added) side, for syntax colors.
    pub side: Vec<Vec<(u32, u32)>>,
    /// The display width of each line.
    pub width: Vec<Vec<u32>>,
    pub old_lines: usize,
    pub new_lines: usize,
    pub image: bool,
}

/// Where the viewer is and how big, worked out from the terminal's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Geom {
    pub width: u16,
    pub height: u16,
    /// Docked beside the session (else full screen).
    pub docked: bool,
    /// The pane's left column and width.
    pub x: u16,
    pub w: u16,
    /// No tree, flush cards, a footer.
    pub compact: bool,
    pub tree: bool,
    pub tree_w: u16,
    pub inset: u16,
    /// The patch pane (cards and their insets).
    pub patch_x: u16,
    pub patch_w: u16,
    /// The scrolled rows' first screen row and their count.
    pub view_y: u16,
    pub view_h: u16,
    pub split: bool,
}

impl Geom {
    pub fn card_w(&self) -> u16 {
        self.patch_w.saturating_sub(self.inset * 2)
    }
}

/// What a command asks the app to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    None,
    Close,
    /// Open the source chooser.
    Source,
    Help,
    FocusPrompt,
}

pub struct Viewer {
    pub source: Source,
    /// The folder compared (the session's).
    pub dir: PathBuf,
    /// The branch checked out when the viewer opened, for base choices.
    pub key: String,
    pub load: Load,
    pub files: Vec<FileDiff>,
    pub prep: Vec<Prep>,
    pub tree: Tree,
    /// Files in tree order.
    pub order: Vec<usize>,
    pub expanded: BTreeSet<usize>,
    pub reviewed: HashSet<String>,
    pub selected: Option<usize>,
    /// The last file of single-file view was reviewed: reviewing again does
    /// not reopen it.
    exhausted: Option<usize>,
    /// The hunk jumped to, and the scroll it left (a scroll since forgets it).
    selected_hunk: Option<(usize, usize, usize)>,
    pub single: bool,
    pub tree_enabled: bool,
    pub view_override: Option<View>,
    pub prefs_view: Option<View>,
    pub scroll: usize,
    /// The presentation asked for (a narrow terminal shows full screen
    /// regardless).
    pub fullscreen: bool,
    /// Docked, whether keys come here (else to the prompt).
    pub focused: bool,
    /// The newest load asked for; older answers are dropped.
    seq: Cell<u64>,
    /// Base lookups this viewer made, by key.
    pub reported: HashMap<String, Option<Base>>,
    /// Loads to hand to the runner.
    pub requests: Vec<Request>,
    rev: u64,
    layout: RefCell<Option<std::rc::Rc<Layout>>>,
    /// The terminal size last drawn at.
    pub term: Cell<(u16, u16)>,
    pub tree_scroll: Cell<usize>,
    pub(crate) highlight: RefCell<super::diff_draw::Highlights>,
    /// The base picker's request, while it loads.
    pub branches_seq: Option<u64>,
}

/// Text as drawn: tabs as spaces, control characters escaped.
fn display(s: &str) -> String {
    let s = if s.contains('\t') {
        s.replace('\t', "    ")
    } else {
        s.to_string()
    };
    if s.chars().any(|c| c.is_control()) {
        super::view::escape_controls(&s)
    } else {
        s
    }
}

fn prep(f: &FileDiff) -> Prep {
    let mut p = Prep {
        image: super::diff_source::is_image(&f.file),
        ..Default::default()
    };
    let mut max = 1;
    for h in &f.hunks {
        let mut side = Vec::with_capacity(h.lines.len());
        let mut width = Vec::with_capacity(h.lines.len());
        for l in &h.lines {
            max = max.max(l.old).max(l.new);
            let o = p.old_lines as u32;
            let n = p.new_lines as u32;
            match l.kind {
                LineKind::Context => {
                    p.old_lines += 1;
                    p.new_lines += 1;
                }
                LineKind::Add => p.new_lines += 1,
                LineKind::Del => p.old_lines += 1,
            }
            side.push((o, n));
            width.push(l.text.width() as u32);
        }
        p.side.push(side);
        p.width.push(width);
    }
    p.digits = max.to_string().len();
    p
}

impl Viewer {
    /// A viewer on `dir`'s changes. `source` is the one to show first
    /// (the route's, the last chosen, or `tui.diffs.source`).
    pub fn new(dir: PathBuf, branch: Option<&str>, source: Source, prefs: Prefs) -> Viewer {
        let key = base_key(&dir, branch);
        Viewer {
            source,
            dir,
            key,
            load: Load::Loading,
            files: vec![],
            prep: vec![],
            tree: Tree::default(),
            order: vec![],
            expanded: BTreeSet::new(),
            reviewed: HashSet::new(),
            selected: None,
            exhausted: None,
            selected_hunk: None,
            single: prefs.single,
            tree_enabled: prefs.tree,
            view_override: prefs.view,
            prefs_view: prefs.view,
            scroll: 0,
            fullscreen: prefs.fullscreen,
            focused: true,
            seq: Cell::new(0),
            reported: HashMap::new(),
            requests: vec![],
            rev: 0,
            layout: RefCell::new(None),
            term: Cell::new((80, 24)),
            tree_scroll: Cell::new(0),
            highlight: RefCell::new(Default::default()),
            branches_seq: None,
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.set(self.seq.get() + 1);
        self.seq.get()
    }

    /// The base the source chooser shows: the one chosen, else the one
    /// found.
    pub fn source_base(&self, mem: &Memory) -> Option<Base> {
        match mem.bases.get(&self.key) {
            Some(r) => Some(Base {
                name: r.clone(),
                reference: r.clone(),
            }),
            None => self.reported.get(&self.key).cloned().flatten(),
        }
    }

    /// Ask for the current source's changes. What is shown stays until
    /// they come; a newer request makes older answers stale.
    pub fn reload(&mut self, mem: &Memory) {
        let seq = self.next_seq();
        let job = match self.source {
            Source::Turn => Job::Turn,
            Source::Working => Job::Vcs {
                dir: self.dir.clone(),
                source: Source::Working,
                base: None,
                lookup: None,
            },
            s => {
                let (base, lookup) = match mem.bases.get(&self.key) {
                    Some(r) => (Some(r.clone()), None),
                    None => match self.reported.get(&self.key) {
                        Some(Some(b)) => (Some(b.reference.clone()), None),
                        Some(None) => (None, None),
                        None => (None, Some(self.key.clone())),
                    },
                };
                if s == Source::Committed && base.is_none() && lookup.is_none() {
                    self.load = Load::Unavailable;
                    self.set_files(vec![]);
                    return;
                }
                Job::Vcs {
                    dir: self.dir.clone(),
                    source: s,
                    base,
                    lookup,
                }
            }
        };
        self.load = Load::Loading;
        self.requests.push(Request { seq, job });
    }

    /// Ask for the base picker's branches.
    pub fn load_branches(&mut self) {
        let seq = self.next_seq_branches();
        self.branches_seq = Some(seq);
        self.requests.push(Request {
            seq,
            job: Job::Branches {
                dir: self.dir.clone(),
            },
        });
    }

    fn next_seq_branches(&self) -> u64 {
        // Branch lists share the counter: their numbers never match a diff's.
        self.next_seq()
    }

    /// Take a finished job's answer. Returns the branches when it was the
    /// base picker's list.
    pub fn answer(&mut self, r: Response) -> Option<Result<Vec<String>, String>> {
        match r {
            Response::Branches { seq, branches } => (self.branches_seq == Some(seq)).then(|| {
                self.branches_seq = None;
                branches
            }),
            Response::Diff {
                seq,
                lookup,
                base,
                files,
            } => {
                if let Some((key, Ok(b))) = lookup {
                    self.reported.entry(key).or_insert(b);
                }
                if seq != self.seq.get() {
                    return None;
                }
                match files {
                    Err(e) => {
                        self.load = Load::Failed(e);
                        self.set_files(vec![]);
                    }
                    Ok(None) => {
                        self.load = Load::Unavailable;
                        self.set_files(vec![]);
                    }
                    Ok(Some(files)) => {
                        self.load = Load::Ready { base };
                        self.set_files(files);
                    }
                }
                None
            }
        }
    }

    /// New files: everything about the old ones is forgotten.
    pub fn set_files(&mut self, mut files: Vec<FileDiff>) {
        for f in &mut files {
            for h in &mut f.hunks {
                for l in &mut h.lines {
                    if l.text.contains('\t') || l.text.chars().any(|c| c.is_control()) {
                        l.text = display(&l.text);
                    }
                }
            }
        }
        self.prep = files.iter().map(prep).collect();
        self.tree = diff_tree::build(&files.iter().map(|f| f.file.as_str()).collect::<Vec<_>>());
        self.order = diff_tree::ordered_files(&diff_tree::flatten(&self.tree, None));
        self.expanded = diff_tree::all_expanded(&self.tree);
        self.files = files;
        self.selected = None;
        self.selected_hunk = None;
        self.reviewed.clear();
        self.exhausted = None;
        self.scroll = 0;
        self.tree_scroll.set(0);
        *self.highlight.borrow_mut() = Default::default();
        self.touch();
    }

    fn touch(&mut self) {
        self.rev += 1;
    }

    /// Switch source (from the chooser); remembered until the UI exits.
    pub fn switch_source(&mut self, s: Source, mem: &mut Memory) {
        mem.source = Some(s);
        self.source = s;
        self.reload(mem);
    }

    /// A base was chosen: remembered for this branch, and the comparisons
    /// that use one reload.
    pub fn choose_base(&mut self, reference: &str, mem: &mut Memory) {
        mem.bases.insert(self.key.clone(), reference.to_string());
        if self.source.needs_base() {
            self.reload(mem);
        }
    }

    /// The source label's detail: what it is compared with.
    pub fn detail(&self) -> Option<String> {
        let failed = matches!(self.load, Load::Failed(_));
        match self.source {
            Source::Turn => failed.then(|| "Diff unavailable".into()),
            Source::Working => Some("vs HEAD".into()),
            _ if failed => Some("Base or diff unavailable".into()),
            _ => match &self.load {
                Load::Loading => Some("Resolving diff…".into()),
                Load::Ready { base: Some(b) } => Some(format!("vs {}", b.name)),
                _ => Some("Base not reported".into()),
            },
        }
    }

    pub fn reviewed_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| self.reviewed.contains(&f.file))
            .count()
    }

    pub fn is_reviewed(&self, file: usize) -> bool {
        self.files
            .get(file)
            .is_some_and(|f| self.reviewed.contains(&f.file))
    }

    // ----- geometry -------------------------------------------------------

    /// The viewer's place and shape on a `width` × `height` terminal.
    pub fn geom(&self, width: u16, height: u16) -> Geom {
        let can_split = width >= SPLIT_MIN_WIDTH;
        let docked = can_split && !self.fullscreen;
        let (x, w) = if docked {
            // OpenCode's default pane width: half the terminal.
            let pane = (width / 2).max(1);
            (width - pane, pane)
        } else {
            (0, width)
        };
        let compact = docked || w < FULL_LAYOUT_MIN_WIDTH;
        let tree = !compact
            && diff_tree::show_tree(self.tree_enabled, self.files.len())
            && matches!(self.load, Load::Ready { .. });
        let tree_w = if tree {
            (w / 4).clamp(TREE_MIN_WIDTH, TREE_MAX_WIDTH)
        } else {
            0
        };
        let inset = if compact { 0 } else { FULL_INSET };
        let patch_x = x + tree_w;
        let patch_w = w - tree_w;
        // Rows above the patches: the source header (when the tree does not
        // hold it) and the pane's top edge.
        let header = if tree {
            0
        } else if docked {
            2
        } else {
            1
        };
        let footer = if compact {
            if docked {
                3
            } else {
                1
            }
        } else {
            0
        };
        let view_y = header + 1;
        let view_h = height.saturating_sub(view_y + footer).max(1);
        let split = patch_w.saturating_sub(inset * 2) >= MIN_SPLIT_WIDTH;
        Geom {
            width,
            height,
            docked,
            x,
            w,
            compact,
            tree,
            tree_w,
            inset,
            patch_x,
            patch_w,
            view_y,
            view_h,
            split,
        }
    }

    pub fn now(&self) -> Geom {
        let (w, h) = self.term.get();
        self.geom(w, h)
    }

    /// The patch view in effect: split only where there is room.
    pub fn view(&self, g: &Geom) -> View {
        if !g.split {
            return View::Unified;
        }
        self.view_override
            .or(self.prefs_view)
            .unwrap_or(View::Split)
    }

    // ----- layout ---------------------------------------------------------

    /// The files whose cards show, in order.
    pub fn visible_files(&self) -> Vec<usize> {
        if !self.single {
            return self.order.clone();
        }
        diff_tree::single_file(self.selected, None, self.order.first().copied())
            .into_iter()
            .collect()
    }

    /// The layout for `g`, rebuilt when what it depends on changed.
    pub fn layout(&self, g: &Geom) -> std::rc::Rc<Layout> {
        let key = LayoutKey {
            rev: self.rev,
            card: g.card_w(),
            split: self.view(g) == View::Split,
            compact: g.compact,
        };
        if let Some(l) = self.layout.borrow().as_ref().filter(|l| l.key == key) {
            return l.clone();
        }
        let l = std::rc::Rc::new(self.build_layout(key));
        *self.layout.borrow_mut() = Some(l.clone());
        l
    }

    /// The gutter's width: padding, the line number and a space.
    pub fn gutter(&self, file: usize, compact: bool) -> usize {
        1 + usize::from(compact) + self.prep[file].digits + 1
    }

    /// The note shown instead of a patch, if any.
    pub fn message(&self, file: usize) -> Option<&'static str> {
        let f = &self.files[file];
        let image = self.prep[file].image;
        if image && self.source == Source::Committed {
            return Some(
                "Committed image preview unavailable. The working-tree image is not shown.",
            );
        }
        if image && f.status == Status::Deleted {
            return Some("Deleted image. The previous revision is not available for preview.");
        }
        if image {
            return Some("Image preview is not shown in the terminal.");
        }
        f.hunks
            .is_empty()
            .then_some("No patch available for this file.")
    }

    /// Whether `file` shows in split view.
    pub fn split_file(&self, file: usize, key: &LayoutKey) -> bool {
        key.split && self.files[file].status == Status::Modified
    }

    fn build_layout(&self, key: LayoutKey) -> Layout {
        let mut items = Vec::new();
        let mut tops = Vec::new();
        let mut cards = Vec::new();
        let mut hunks = Vec::new();
        let mut row = 0usize;
        let push =
            |items: &mut Vec<Item>, tops: &mut Vec<usize>, row: &mut usize, it: Item, h: usize| {
                items.push(it);
                tops.push(*row);
                *row += h.max(1);
            };
        let rows = |w: u32, room: usize| -> usize { (w as usize).div_ceil(room.max(1)).max(1) };
        for (i, f) in self.visible_files().into_iter().enumerate() {
            let node = row;
            if i > 0 {
                push(&mut items, &mut tops, &mut row, Item::Gap(f), 1);
            }
            let header = row;
            push(
                &mut items,
                &mut tops,
                &mut row,
                Item::Header(f),
                HEADER_HEIGHT,
            );
            if !self.is_reviewed(f) {
                if self.message(f).is_some() {
                    push(&mut items, &mut tops, &mut row, Item::Message(f), 2);
                } else {
                    let gutter = self.gutter(f, key.compact);
                    let split = self.split_file(f, &key);
                    let card = key.card as usize;
                    let unified_room = card.saturating_sub(gutter + 2);
                    let left = card / 2;
                    let left_room = left.saturating_sub(gutter + 2);
                    let right_room = (card - left).saturating_sub(gutter + 2);
                    let file = &self.files[f];
                    let widths = &self.prep[f].width;
                    for (h, hunk) in file.hunks.iter().enumerate() {
                        if h > 0 {
                            push(&mut items, &mut tops, &mut row, Item::HunkGap(f), 1);
                        }
                        hunks.push((f, h, row));
                        if !split {
                            for (l, _) in hunk.lines.iter().enumerate() {
                                let height = rows(widths[h][l], unified_room);
                                push(
                                    &mut items,
                                    &mut tops,
                                    &mut row,
                                    Item::Unified {
                                        file: f,
                                        hunk: h,
                                        line: l,
                                    },
                                    height,
                                );
                            }
                            continue;
                        }
                        let lines = &hunk.lines;
                        let mut l = 0;
                        while l < lines.len() {
                            if lines[l].kind == LineKind::Context {
                                let height = rows(widths[h][l], left_room)
                                    .max(rows(widths[h][l], right_room));
                                push(
                                    &mut items,
                                    &mut tops,
                                    &mut row,
                                    Item::Split {
                                        file: f,
                                        hunk: h,
                                        left: Some(l),
                                        right: Some(l),
                                    },
                                    height,
                                );
                                l += 1;
                                continue;
                            }
                            let dels: Vec<usize> = (l..lines.len())
                                .take_while(|&j| lines[j].kind == LineKind::Del)
                                .collect();
                            let after = l + dels.len();
                            let adds: Vec<usize> = (after..lines.len())
                                .take_while(|&j| lines[j].kind == LineKind::Add)
                                .collect();
                            l = after + adds.len();
                            for k in 0..dels.len().max(adds.len()) {
                                let (a, b) = (dels.get(k).copied(), adds.get(k).copied());
                                let height = a
                                    .map(|a| rows(widths[h][a], left_room))
                                    .unwrap_or(1)
                                    .max(b.map(|b| rows(widths[h][b], right_room)).unwrap_or(1));
                                push(
                                    &mut items,
                                    &mut tops,
                                    &mut row,
                                    Item::Split {
                                        file: f,
                                        hunk: h,
                                        left: a,
                                        right: b,
                                    },
                                    height,
                                );
                            }
                        }
                    }
                }
            }
            cards.push((
                f,
                Card {
                    node,
                    header,
                    end: row,
                },
            ));
        }
        Layout {
            key,
            items,
            tops,
            total: row,
            cards,
            hunks,
        }
    }

    pub fn max_scroll(&self, g: &Geom) -> usize {
        self.layout(g).total.saturating_sub(g.view_h as usize)
    }

    fn scroll_to(&mut self, row: usize, g: &Geom) {
        self.scroll = row.min(self.max_scroll(g));
    }

    /// The file whose card is at the top of the view.
    pub fn current_file(&self, g: &Geom) -> Option<usize> {
        let l = self.layout(g);
        let top = self.scroll + 1;
        l.cards
            .iter()
            .rev()
            .find(|(_, c)| c.node <= top)
            .or(l.cards.first())
            .map(|(f, _)| *f)
    }

    // ----- commands -------------------------------------------------------

    fn clear_selection(&mut self) {
        self.selected_hunk = None;
        if !self.single {
            self.selected = None;
        }
    }

    fn select(&mut self, file: usize) {
        self.exhausted = None;
        if let Some(open) = diff_tree::file_selection(&self.tree, file) {
            self.expanded.extend(open);
        }
        if self.single && self.selected != Some(file) {
            self.selected = Some(file);
            self.touch();
        }
        self.selected = Some(file);
    }

    /// Scroll so `file`'s title is at the top (the pane's edge stands in
    /// for the gap above it), or `offset` rows into its node.
    fn scroll_to_file(&mut self, file: usize, offset: Option<isize>, g: &Geom) {
        let Some(c) = self.layout(g).card(file) else {
            return;
        };
        let off = offset.unwrap_or(if c.node > 0 { 1 } else { 0 });
        let row = (c.node as isize + off).max(0) as usize;
        self.scroll_to(row, g);
    }

    fn jump_file(&mut self, offset: i64, g: &Geom) {
        self.selected_hunk = None;
        let from = self.selected.or_else(|| self.current_file(g));
        let Some(next) = diff_tree::move_file(&self.order, from, offset) else {
            return;
        };
        self.select(next);
        if self.single {
            self.scroll = 0;
            return;
        }
        self.scroll_to_file(next, None, g);
    }

    fn jump_hunk(&mut self, offset: i64, g: &Geom) {
        let l = self.layout(g);
        let mut list: Vec<(usize, usize, usize)> = l
            .hunks
            .iter()
            .map(|&(f, h, row)| (f, h, row - usize::from(h > 0)))
            .collect();
        list.sort_by_key(|&(_, _, y)| y);
        let selected = self
            .selected_hunk
            .filter(|&(_, _, at)| at == self.scroll)
            .and_then(|(f, h, _)| list.iter().position(|&(lf, lh, _)| lf == f && lh == h));
        let top = self.scroll + HEADER_HEIGHT;
        let next = match selected {
            Some(i) => {
                let j = i as i64 + offset;
                (j >= 0).then(|| list.get(j as usize).copied()).flatten()
            }
            None if offset > 0 => list.iter().find(|h| h.2 > top).copied(),
            None => list.iter().rev().find(|h| h.2 < top).copied(),
        };
        let Some((f, h, y)) = next else { return };
        self.select(f);
        self.scroll_to(y.saturating_sub(HEADER_HEIGHT), g);
        self.selected_hunk = Some((f, h, self.scroll));
    }

    /// Mark a file reviewed, or unmark it. Reviewing the file in view moves
    /// to its title (single-file view: to the next file); another file's
    /// mark keeps the view where it is.
    pub fn toggle_reviewed(&mut self, file: Option<usize>, g: &Geom) {
        let Some(file) = file else { return };
        let Some(name) = self.files.get(file).map(|f| f.file.clone()) else {
            return;
        };
        let current = self.selected.or_else(|| self.current_file(g));
        let anchor = current
            .and_then(|c| self.layout(g).card(c))
            .map(|c| self.scroll as isize - c.node as isize);
        let was = self.reviewed.contains(&name);
        if !self.reviewed.remove(&name) {
            self.reviewed.insert(name);
        }
        self.touch();
        if Some(file) != current {
            if let (Some(c), Some(off), false) = (current, anchor, self.single) {
                self.scroll_to_file(c, Some(off), g);
            }
            return;
        }
        let next = if self.single && !was {
            diff_tree::move_file(&self.order, Some(file), 1).unwrap_or(file)
        } else {
            file
        };
        self.select(next);
        if self.single && !was && next == file {
            self.exhausted = Some(file);
        }
        self.selected_hunk = None;
        if self.single {
            self.scroll = 0;
        } else {
            self.scroll_to_file(next, None, g);
        }
    }

    fn page(&mut self, dir: isize, divisor: usize, g: &Geom) {
        self.clear_selection();
        let step = (g.view_h as usize / divisor).max(1) as isize;
        let row = (self.scroll as isize + dir * step).max(0) as usize;
        self.scroll_to(row, g);
    }

    /// Run a `diff.*` command (or `app.exit`, which closes the viewer).
    pub fn command(&mut self, id: &str) -> (Outcome, bool) {
        let g = self.now();
        let mut prefs = false;
        let out = match id {
            // `diff.open` toggles the viewer, as `/diff` does.
            "diff.close" | "diff.open" | "app.exit" => Outcome::Close,
            "diff.down" | "diff.up" => {
                self.clear_selection();
                let row = if id == "diff.down" {
                    self.scroll + 1
                } else {
                    self.scroll.saturating_sub(1)
                };
                self.scroll_to(row, &g);
                Outcome::None
            }
            "diff.page.down" => {
                self.page(1, 1, &g);
                Outcome::None
            }
            "diff.page.up" => {
                self.page(-1, 1, &g);
                Outcome::None
            }
            "diff.half_page.down" => {
                self.page(1, 2, &g);
                Outcome::None
            }
            "diff.half_page.up" => {
                self.page(-1, 2, &g);
                Outcome::None
            }
            "diff.first" => {
                self.clear_selection();
                self.scroll = 0;
                Outcome::None
            }
            "diff.last" => {
                self.clear_selection();
                self.scroll = self.max_scroll(&g);
                Outcome::None
            }
            "diff.next_hunk" => {
                self.jump_hunk(1, &g);
                Outcome::None
            }
            "diff.previous_hunk" => {
                self.jump_hunk(-1, &g);
                Outcome::None
            }
            "diff.next_file" => {
                self.jump_file(1, &g);
                Outcome::None
            }
            "diff.previous_file" => {
                self.jump_file(-1, &g);
                Outcome::None
            }
            "diff.mark_reviewed" => {
                let file = self.selected.or_else(|| self.current_file(&g));
                if file.is_none() || file != self.exhausted {
                    self.toggle_reviewed(file, &g);
                }
                Outcome::None
            }
            "diff.toggle_file_tree" => {
                if !g.compact {
                    self.tree_enabled = !self.tree_enabled;
                    prefs = true;
                }
                Outcome::None
            }
            "diff.single_patch" => {
                self.selected_hunk = None;
                if !self.single {
                    if let Some(f) = self
                        .current_file(&g)
                        .or(self.selected)
                        .or(self.order.first().copied())
                    {
                        self.select(f);
                    }
                    self.single = true;
                    self.touch();
                    self.scroll = 0;
                } else {
                    let f = self.visible_files().first().copied();
                    self.single = false;
                    self.touch();
                    if let Some(f) = f {
                        self.select(f);
                        let g = self.now();
                        self.scroll_to_file(f, None, &g);
                    }
                }
                prefs = true;
                Outcome::None
            }
            "diff.switch_source" => Outcome::Source,
            "diff.toggle_view" => {
                if g.split {
                    self.selected_hunk = None;
                    let next = match self.view(&g) {
                        View::Split => View::Unified,
                        View::Unified => View::Split,
                    };
                    self.view_override = Some(next);
                    prefs = true;
                }
                Outcome::None
            }
            "diff.toggle_fullscreen" => {
                // A narrow terminal is full screen whatever is asked; only
                // a real toggle is remembered.
                if g.width >= SPLIT_MIN_WIDTH {
                    self.fullscreen = !self.fullscreen;
                    prefs = true;
                }
                Outcome::None
            }
            "diff.help" => Outcome::Help,
            "pane.focus.left" => {
                if g.docked {
                    self.focused = false;
                    Outcome::FocusPrompt
                } else {
                    Outcome::None
                }
            }
            _ => Outcome::None,
        };
        (out, prefs)
    }

    /// The layout choices to remember.
    pub fn prefs(&self) -> Prefs {
        Prefs {
            tree: self.tree_enabled,
            single: self.single,
            view: self.view_override,
            fullscreen: self.fullscreen,
        }
    }

    /// The selected row of the tree (single-file view: the file shown).
    pub fn tree_selected(&self) -> Option<usize> {
        self.selected.or_else(|| {
            self.single
                .then(|| self.visible_files().first().copied())
                .flatten()
        })
    }

    /// Open or close a folder of the tree (OpenCode's click on it; CodeSmith
    /// has no mouse, so only the tests do).
    #[cfg(test)]
    pub fn toggle_folder(&mut self, node: usize) {
        self.expanded = diff_tree::toggle(&self.tree, &self.expanded, Some(node));
    }

    /// Whether syntax colors are worked out for `file`.
    pub fn highlights(&self, file: usize) -> bool {
        self.prep[file].old_lines.max(self.prep[file].new_lines) <= HIGHLIGHT_MAX_LINES
    }
}

/// Where the layout choices made in the viewer are kept.
pub fn state_file() -> PathBuf {
    codesmith_core::paths::data_dir().join("tui-diff.json")
}

fn configured_json(c: &super::settings::Diffs) -> serde_json::Value {
    serde_json::json!({
        "tree": c.tree,
        "single": c.single,
        "view": c.view.map(View::key),
        "fullscreen": c.fullscreen,
    })
}

/// The layout to open with: `tui.diffs`, then the choices made in the
/// viewer since (OpenCode writes them back to its config; CodeSmith's UI
/// never writes the config, so they are kept beside it, as the theme
/// chosen in the UI is). A changed `tui.diffs` wins again.
pub fn remembered(c: &super::settings::Diffs, text: Option<&str>) -> Prefs {
    let mut p = Prefs {
        tree: c.tree.unwrap_or(true),
        single: c.single.unwrap_or(false),
        view: c.view,
        fullscreen: c.fullscreen.unwrap_or(false),
    };
    let v: serde_json::Value = text
        .and_then(|t| serde_json::from_str(t).ok())
        .unwrap_or_default();
    if v.get("configured") != Some(&configured_json(c)) {
        return p;
    }
    let b = |k: &str| v.get(k).and_then(serde_json::Value::as_bool);
    p.tree = b("tree").unwrap_or(p.tree);
    p.single = b("single").unwrap_or(p.single);
    p.fullscreen = b("fullscreen").unwrap_or(p.fullscreen);
    match v.get("view").and_then(serde_json::Value::as_str) {
        Some("split") => p.view = Some(View::Split),
        Some("unified") => p.view = Some(View::Unified),
        _ => {}
    }
    p
}

/// What [`remembered`] reads back.
pub fn remember_text(c: &super::settings::Diffs, p: Prefs) -> String {
    serde_json::json!({
        "configured": configured_json(c),
        "tree": p.tree,
        "single": p.single,
        "view": p.view.map(View::key),
        "fullscreen": p.fullscreen,
    })
    .to_string()
}
