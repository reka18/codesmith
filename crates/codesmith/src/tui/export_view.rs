//! The export dialog (OpenCode's `DialogExportOptions` and
//! `DialogExportResult`, `session.export`, `<leader>x`, `/export`): the
//! format (Markdown or JSON), thinking and tools for Markdown, sanitizing
//! for JSON, then Copy (to the clipboard through the terminal) or Save.
//! Tab and Shift+Tab (or the arrows) move a visible focus through the
//! options and buttons, Enter or Space selects. Save asks for the file,
//! offering `session-<id>.<ext>` in the working folder; a file that is
//! already there is replaced only after a `y`. The keyboard drives it all:
//! the mouse does nothing here.

use super::theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};

/// The options an export is made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub json: bool,
    pub thinking: bool,
    pub tools: bool,
    pub sanitize: bool,
}

/// The focusable rows and buttons, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Markdown,
    Json,
    Thinking,
    Tools,
    Sanitize,
    Copy,
    Save,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Options,
    /// Asking for the file to save to.
    Path(String),
    /// The file exists: replace it?
    Confirm(PathBuf),
    /// Saved there.
    Done(PathBuf),
}

/// What a key asks the runner to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Nothing,
    Close,
    Copy(Options),
    Save {
        options: Options,
        path: PathBuf,
        overwrite: bool,
    },
    /// Put the saved file's path on the clipboard.
    CopyPath(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportView {
    pub options: Options,
    pub focus: Focus,
    pub stage: Stage,
    /// Why the last save failed.
    pub error: Option<String>,
    /// Whether the saved path was copied.
    pub copied: bool,
    cwd: PathBuf,
    session: String,
}

impl ExportView {
    /// The dialog for session `session`, saving relative to `cwd`;
    /// thinking is included when the transcript shows it.
    pub fn new(session: &str, cwd: &Path, thinking: bool) -> ExportView {
        ExportView {
            options: Options {
                json: false,
                thinking,
                tools: true,
                sanitize: false,
            },
            focus: Focus::Markdown,
            stage: Stage::Options,
            error: None,
            copied: false,
            cwd: cwd.to_path_buf(),
            session: session.to_string(),
        }
    }

    /// The focus order for the chosen format.
    fn order(&self) -> &'static [Focus] {
        use Focus::*;
        if self.options.json {
            &[Markdown, Json, Sanitize, Copy, Save]
        } else {
            &[Markdown, Json, Thinking, Tools, Copy, Save]
        }
    }

    fn step(&mut self, by: isize) {
        let order = self.order();
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0) as isize;
        let n = order.len() as isize;
        self.focus = order[((at + by).rem_euclid(n)) as usize];
    }

    /// The file a Save offers first.
    fn offered(&self) -> String {
        let name = codesmith_core::export::default_name(&self.session, self.options.json);
        self.cwd.join(name).display().to_string()
    }

    /// Where a typed path points: `~/` is the home folder, a relative path
    /// is the working folder's.
    pub fn resolve(&self, typed: &str) -> PathBuf {
        let typed = typed.trim();
        if let Some(rest) = typed.strip_prefix("~/") {
            if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
                return PathBuf::from(home).join(rest);
            }
        }
        let p = Path::new(typed);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }

    /// The runner's answer to a save: it is there, so ask before replacing
    /// it.
    pub fn exists(&mut self, path: PathBuf) {
        self.error = None;
        self.stage = Stage::Confirm(path);
    }

    /// The runner's answer to a save that failed: back to the file name.
    pub fn failed(&mut self, path: &Path, why: String) {
        self.error = Some(why);
        self.stage = Stage::Path(path.display().to_string());
    }

    pub fn saved(&mut self, path: PathBuf) {
        self.error = None;
        self.stage = Stage::Done(path);
    }

    pub fn key(&mut self, k: &KeyEvent) -> Outcome {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match self.stage.clone() {
            Stage::Done(path) => match k.code {
                KeyCode::Char('c') if !ctrl => {
                    self.copied = true;
                    Outcome::CopyPath(path)
                }
                KeyCode::Enter | KeyCode::Esc => Outcome::Close,
                _ => Outcome::Nothing,
            },
            Stage::Confirm(path) => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if !ctrl => Outcome::Save {
                    options: self.options,
                    path,
                    overwrite: true,
                },
                _ => {
                    // Anything else keeps the file: back to the name.
                    self.stage = Stage::Path(path.display().to_string());
                    Outcome::Nothing
                }
            },
            Stage::Path(mut typed) => {
                let out = match k.code {
                    KeyCode::Esc => {
                        self.error = None;
                        self.stage = Stage::Options;
                        return Outcome::Nothing;
                    }
                    KeyCode::Enter if !typed.trim().is_empty() => {
                        return Outcome::Save {
                            options: self.options,
                            path: self.resolve(&typed),
                            overwrite: false,
                        }
                    }
                    KeyCode::Backspace => {
                        typed.pop();
                        Outcome::Nothing
                    }
                    KeyCode::Char('u') if ctrl => {
                        typed.clear();
                        Outcome::Nothing
                    }
                    // Control characters never go into a file name.
                    KeyCode::Char(c) if !ctrl && !c.is_control() => {
                        typed.push(c);
                        Outcome::Nothing
                    }
                    _ => Outcome::Nothing,
                };
                self.stage = Stage::Path(typed);
                out
            }
            Stage::Options => match k.code {
                KeyCode::Esc => Outcome::Close,
                KeyCode::BackTab => {
                    self.step(-1);
                    Outcome::Nothing
                }
                KeyCode::Tab if shift => {
                    self.step(-1);
                    Outcome::Nothing
                }
                KeyCode::Tab | KeyCode::Down | KeyCode::Right => {
                    self.step(1);
                    Outcome::Nothing
                }
                KeyCode::Up | KeyCode::Left => {
                    self.step(-1);
                    Outcome::Nothing
                }
                KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
                _ => Outcome::Nothing,
            },
        }
    }

    fn activate(&mut self) -> Outcome {
        let o = &mut self.options;
        match self.focus {
            Focus::Markdown => o.json = false,
            Focus::Json => o.json = true,
            Focus::Thinking => o.thinking = !o.thinking,
            Focus::Tools => o.tools = !o.tools,
            Focus::Sanitize => o.sanitize = !o.sanitize,
            Focus::Copy => return Outcome::Copy(self.options),
            Focus::Save => {
                self.stage = Stage::Path(self.offered());
                return Outcome::Nothing;
            }
        }
        Outcome::Nothing
    }

    pub fn title(&self) -> String {
        match self.stage {
            Stage::Done(_) => "Session exported".into(),
            _ => "Export session".into(),
        }
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let p = theme::current();
        let fg = |c| Style::default().fg(c);
        let safe = |s: &str| super::view::display_safe(s);
        let label = |f: Focus| {
            if self.focus == f {
                fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                fg(p.dialog_text)
            }
        };
        let mark = |f: Focus, on: bool| {
            if self.focus == f {
                fg(p.accent)
            } else if on {
                fg(p.dialog_text)
            } else {
                fg(p.dialog_muted)
            }
        };
        match &self.stage {
            Stage::Done(path) => {
                return vec![
                    Line::from(Span::styled("Saved to", fg(p.dialog_muted))),
                    Line::from(Span::styled(
                        safe(&path.display().to_string()),
                        fg(p.dialog_text),
                    )),
                    Line::default(),
                    Line::from(vec![
                        Span::styled(
                            if self.copied {
                                "copied ✓"
                            } else {
                                "copy path"
                            },
                            fg(p.dialog_text),
                        ),
                        Span::styled(" c   ", fg(p.dialog_muted)),
                        Span::styled("close", fg(p.dialog_text)),
                        Span::styled(" enter", fg(p.dialog_muted)),
                    ]),
                ]
            }
            Stage::Confirm(path) => {
                return vec![
                    Line::from(Span::styled(
                        safe(&path.display().to_string()),
                        fg(p.dialog_text),
                    )),
                    Line::from(Span::styled("is already there. Replace it?", fg(p.error))),
                    Line::default(),
                    Line::from(vec![
                        Span::styled("replace", fg(p.dialog_text)),
                        Span::styled(" y   ", fg(p.dialog_muted)),
                        Span::styled("keep it", fg(p.dialog_text)),
                        Span::styled(" any other key", fg(p.dialog_muted)),
                    ]),
                ];
            }
            Stage::Path(typed) => {
                let mut v = vec![
                    Line::from(Span::styled("Save to file", fg(p.dialog_muted))),
                    Line::from(vec![
                        Span::styled(safe(typed), fg(p.accent)),
                        Span::styled("▏", fg(p.accent)),
                    ]),
                ];
                if let Some(e) = &self.error {
                    v.push(Line::from(Span::styled(safe(e), fg(p.error))));
                }
                v.push(Line::default());
                v.push(Line::from(vec![
                    Span::styled("save", fg(p.dialog_text)),
                    Span::styled(" enter   ", fg(p.dialog_muted)),
                    Span::styled("back", fg(p.dialog_text)),
                    Span::styled(" esc", fg(p.dialog_muted)),
                ]));
                return v;
            }
            Stage::Options => {}
        }
        let o = self.options;
        let radio = |f: Focus, on: bool, name: &'static str| {
            vec![
                Span::styled(if on { "◉ " } else { "○ " }, mark(f, on)),
                Span::styled(name, label(f)),
                Span::raw("  "),
            ]
        };
        let check = |f: Focus, on: bool, name: &'static str| {
            Line::from(vec![
                Span::styled(if on { "[x] " } else { "[ ] " }, mark(f, on)),
                Span::styled(name, label(f)),
            ])
        };
        let mut format = vec![Span::styled("Export as: ", fg(p.dialog_text))];
        format.extend(radio(Focus::Markdown, !o.json, "Markdown"));
        format.extend(radio(Focus::Json, o.json, "JSON"));
        let mut v = vec![Line::from(format), Line::default()];
        if o.json {
            v.push(check(
                Focus::Sanitize,
                o.sanitize,
                "Sanitize sensitive data",
            ));
        } else {
            v.push(check(Focus::Thinking, o.thinking, "Include thinking"));
            v.push(check(Focus::Tools, o.tools, "Include tools"));
        }
        v.push(Line::default());
        let button = |f: Focus, name: &'static str| {
            let style = if self.focus == f {
                Style::default()
                    .fg(p.selected_fg)
                    .bg(p.selected_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                fg(p.dialog_text)
            };
            Span::styled(format!("  {name}  "), style)
        };
        v.push(Line::from(vec![
            button(Focus::Copy, "Copy"),
            Span::raw(" "),
            button(Focus::Save, "Save"),
        ]));
        v.push(Line::from(Span::styled(
            "tab move · enter select · esc close",
            fg(p.dialog_muted),
        )));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn view() -> ExportView {
        ExportView::new("ses_1", Path::new("/work"), true)
    }
    fn text(v: &ExportView) -> String {
        v.lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// The style of the span whose text contains `what`.
    fn style(v: &ExportView, what: &str) -> Style {
        v.lines()
            .iter()
            .flat_map(|l| l.spans.clone())
            .find(|s| s.content.contains(what))
            .unwrap()
            .style
    }

    // tui/test/component/dialog-export-options: "tab and shift+tab move a
    // visible focus through the export options" (adapted: the focus is the
    // accent colour on a field and the selected colours on a button)
    #[test]
    fn oc_tab_and_shift_tab_move_a_visible_focus_through_the_export_options() {
        let p = theme::current();
        let mut v = view();
        let accent = p.accent;
        assert!(text(&v).contains("Copy") && text(&v).contains("Include tools"));
        // Only the focused field is highlighted; checked fields keep the
        // base label colour.
        assert_eq!(style(&v, "Markdown").fg, Some(accent));
        assert_eq!(style(&v, "Include thinking").fg, Some(p.dialog_text));
        assert_ne!(style(&v, "Copy").bg, Some(p.selected_bg));
        for _ in 0..4 {
            v.key(&key(KeyCode::Tab));
        }
        assert_eq!(v.focus, Focus::Copy);
        assert_eq!(style(&v, "Copy").bg, Some(p.selected_bg));
        assert_eq!(style(&v, "Include tools").fg, Some(p.dialog_text));
        v.key(&KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(style(&v, "Include tools").fg, Some(accent));
        assert_ne!(style(&v, "Copy").bg, Some(p.selected_bg));
        // Tab wraps around, both ways.
        for _ in 0..3 {
            v.key(&key(KeyCode::Tab));
        }
        assert_eq!(v.focus, Focus::Markdown);
        v.key(&KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(v.focus, Focus::Save);
    }

    #[test]
    fn json_swaps_the_markdown_options_for_sanitizing() {
        let mut v = view();
        v.key(&key(KeyCode::Tab));
        v.key(&key(KeyCode::Enter));
        assert!(v.options.json);
        let t = text(&v);
        assert!(
            t.contains("Sanitize sensitive data") && !t.contains("Include tools"),
            "{t}"
        );
        v.key(&key(KeyCode::Tab));
        assert_eq!(v.focus, Focus::Sanitize);
        v.key(&key(KeyCode::Char(' ')));
        assert!(v.options.sanitize);
        v.key(&key(KeyCode::Tab));
        assert_eq!(
            v.key(&key(KeyCode::Enter)),
            Outcome::Copy(Options {
                json: true,
                thinking: true,
                tools: true,
                sanitize: true
            })
        );
    }

    #[test]
    fn saving_asks_for_the_file_and_confirms_a_replacement() {
        let mut v = view();
        v.focus = Focus::Save;
        assert_eq!(v.key(&key(KeyCode::Enter)), Outcome::Nothing);
        assert_eq!(v.stage, Stage::Path("/work/session-ses_1.md".into()));
        // Edit the name: clear it, type a relative one.
        v.key(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "out/é.md".chars() {
            v.key(&key(KeyCode::Char(c)));
        }
        v.key(&key(KeyCode::Char('\u{7}')));
        assert_eq!(v.stage, Stage::Path("out/é.md".into()));
        let Outcome::Save {
            path, overwrite, ..
        } = v.key(&key(KeyCode::Enter))
        else {
            panic!()
        };
        assert_eq!(path, PathBuf::from("/work/out/é.md"));
        assert!(!overwrite);
        // It is there: only y replaces it.
        v.exists(path.clone());
        assert!(text(&v).contains("Replace it?"));
        assert_eq!(v.key(&key(KeyCode::Char('n'))), Outcome::Nothing);
        assert!(matches!(v.stage, Stage::Path(_)));
        v.exists(path.clone());
        assert_eq!(
            v.key(&key(KeyCode::Enter)),
            Outcome::Nothing,
            "enter keeps it"
        );
        v.exists(path.clone());
        assert!(matches!(
            v.key(&key(KeyCode::Char('y'))),
            Outcome::Save {
                overwrite: true,
                ..
            }
        ));
        v.saved(path.clone());
        assert!(text(&v).contains("/work/out/é.md"));
        assert_eq!(v.key(&key(KeyCode::Char('c'))), Outcome::CopyPath(path));
        assert_eq!(v.key(&key(KeyCode::Enter)), Outcome::Close);
    }

    #[test]
    fn an_empty_name_saves_nothing_and_esc_goes_back() {
        let mut v = view();
        v.stage = Stage::Path(String::new());
        assert_eq!(v.key(&key(KeyCode::Enter)), Outcome::Nothing);
        v.stage = Stage::Path("   ".into());
        assert_eq!(v.key(&key(KeyCode::Enter)), Outcome::Nothing);
        v.failed(Path::new("/x"), "no folder /x".into());
        assert!(text(&v).contains("no folder /x"));
        v.key(&key(KeyCode::Esc));
        assert_eq!(v.stage, Stage::Options);
        assert_eq!(v.key(&key(KeyCode::Esc)), Outcome::Close);
    }

    #[test]
    fn typed_paths_resolve_against_the_working_folder_and_home() {
        let v = view();
        assert_eq!(v.resolve("a.md"), PathBuf::from("/work/a.md"));
        assert_eq!(v.resolve("/tmp/a.md"), PathBuf::from("/tmp/a.md"));
        assert_eq!(v.resolve("  b.json "), PathBuf::from("/work/b.json"));
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            assert_eq!(v.resolve("~/c.md"), PathBuf::from(home).join("c.md"));
        }
    }

    #[test]
    fn a_name_with_markup_is_shown_safely() {
        let mut v = view();
        v.stage = Stage::Path("\u{1b}[31mred".into());
        assert!(!text(&v).contains('\u{1b}'));
    }
}
