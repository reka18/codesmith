//! Mini mode (`codesmith mini`): OpenCode's split-footer interface. The
//! finished transcript is written into the terminal's normal scrollback,
//! where it stays after the UI exits and can be scrolled, searched and
//! selected with the terminal's own tools, while a small live footer at
//! the bottom holds the prompt, the running turn and dialogs. It never
//! switches to the alternate screen (the diff viewer, a full-screen view,
//! borrows it while open).
//!
//! [`commit`] decides what is finished and renders it once; [`surface`]
//! writes it and the footer; [`footer`] lays the footer out.

pub mod commit;
pub mod editor;
pub mod footer;
pub mod menu;
pub mod queue;
pub mod settings;
pub mod splash;
pub mod surface;
pub mod variant;
pub mod width;

pub use settings::Settings;

use super::app::App;
use super::view;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Terminal;
use std::io::Write;
use std::time::{Duration, Instant};

/// Mini mode's screen.
pub struct Mini<W: Write> {
    surface: surface::Surface<W>,
    commit: commit::Committer,
    /// The footer is drawn into this, then written by `surface`.
    footer: Terminal<TestBackend>,
    /// The spinner's clock.
    started: Instant,
    /// The entry banner is written.
    splashed: bool,
    /// The version and directory the entry banner shows.
    version: String,
    detail: String,
    /// The full-screen view (the diff viewer) on the alternate screen.
    full: Option<ratatui::DefaultTerminal>,
}

/// The width transcript rows are rendered at.
fn text_width(cols: u16) -> usize {
    cols.saturating_sub(1).max(1) as usize
}

impl<W: Write> Mini<W> {
    /// Mini mode on `out`, `cols` by `rows`, with its footer starting on
    /// row `row`. `detail` is the directory the entry banner names.
    pub fn new(out: W, cols: u16, rows: u16, row: u16, detail: String) -> Self {
        Mini {
            surface: surface::Surface::new(out, cols, rows, row),
            commit: commit::Committer::default(),
            footer: Terminal::new(TestBackend::new(cols.max(1), 1)).expect("a test backend"),
            started: Instant::now(),
            splashed: false,
            version: env!("CARGO_PKG_VERSION").to_string(),
            detail,
            full: None,
        }
    }

    #[cfg(test)]
    pub fn surface(&self) -> &surface::Surface<W> {
        &self.surface
    }

    #[cfg(test)]
    pub fn surface_mut(&mut self) -> &mut surface::Surface<W> {
        &mut self.surface
    }

    fn settings(app: &App) -> Settings {
        app.mini.clone().unwrap_or_default()
    }

    /// How long to wait for an event before drawing again: the spinner's
    /// pace while a turn runs.
    pub fn tick(&self, app: &App) -> Option<Duration> {
        let s = Self::settings(app);
        app.busy.then(|| {
            let (_, every) = s.work_spinner.frames(s.mono);
            Duration::from_millis(every.clamp(50, 250))
        })
    }

    fn splash(&self, s: &Settings) -> Vec<Line<'static>> {
        if !s.splash {
            return vec![];
        }
        splash::entry_lines(
            self.surface.cols as usize,
            &self.version,
            Some(&self.detail),
            s.mono,
        )
    }

    /// Bring the screen up to date with `app` at `cols` by `rows`.
    pub fn draw_at(&mut self, app: &mut App, cols: u16, rows: u16) -> std::io::Result<()> {
        let s = Self::settings(app);
        self.surface.mono = s.mono;
        if (cols, rows) != (self.surface.cols, self.surface.rows) {
            self.surface.resize(cols, rows, s.replay);
            if s.replay {
                let mut lines = self.splash(&s);
                let mut history = self.commit.replay(app, &s, text_width(cols));
                if !lines.is_empty() && history.first().is_some_and(|l| l.width() > 0) {
                    lines.push(Line::default());
                }
                lines.append(&mut history);
                self.surface.commit(&lines);
            }
        }
        if !self.splashed {
            self.splashed = true;
            let lines = self.splash(&s);
            if !lines.is_empty() {
                self.surface.commit(&lines);
                self.commit.wrote_text();
            }
        }
        let lines = self.commit.step(app, &s, text_width(cols));
        if std::mem::take(&mut self.commit.clear) {
            self.surface.clear();
        }
        self.surface.commit(&lines);
        let elapsed = self.started.elapsed().as_millis() as u64;
        let spinner = s.work_spinner.frame(s.mono, elapsed);
        let turn = self
            .commit
            .turn_started()
            .map(|t| t.elapsed().as_millis() as u64);
        let layout = width::policy(&footer::status(app, &s, Some(spinner), turn, cols));
        let parts = footer::parts(app, self.commit.live.len(), cols, rows);
        let h = parts.height().max(1);
        if self.footer.size().ok() != Some(ratatui::layout::Size::new(cols, h)) {
            self.footer.backend_mut().resize(cols, h);
            let _ = self.footer.resize(Rect::new(0, 0, cols, h));
        }
        let live = &self.commit.live;
        let app_ref: &App = app;
        let _ = self
            .footer
            .draw(|f| footer::draw(f, app_ref, live, &layout, parts));
        let backend = self.footer.backend();
        let cursor = backend.cursor_visible().then(|| {
            let p = backend.cursor_position();
            (p.x, p.y)
        });
        let buf = backend.buffer().clone();
        self.surface.footer(&buf, cursor);
        self.surface.flush()
    }

    /// Clear the screen (`app.clear`), keeping the scrollback.
    pub fn clear(&mut self) {
        self.surface.clear();
    }

    /// Write what is left, and the exit banner, and clear the footer.
    pub fn close(&mut self, app: &mut App) -> std::io::Result<()> {
        let s = Self::settings(app);
        app.busy = false;
        let mut lines = self.commit.step(app, &s, text_width(self.surface.cols));
        let live = std::mem::take(&mut self.commit.live);
        lines.extend(live);
        if s.splash && !app.header.session.is_empty() {
            lines.extend(splash::exit_lines(
                self.surface.cols as usize,
                app.header.title.as_deref(),
                &app.header.session,
                true,
                s.mono,
            ));
        }
        self.surface.commit(&lines);
        self.surface.close();
        self.surface.flush()
    }
}

impl Mini<std::io::Stdout> {
    /// Draw at the terminal's size. While the diff viewer is open it has
    /// the alternate screen, as in the full UI; the footer comes back as
    /// it was when it closes.
    pub fn draw(&mut self, app: &mut App) -> std::io::Result<()> {
        use ratatui::crossterm::{execute, terminal};
        if app.diff.is_some() {
            if self.full.is_none() {
                execute!(std::io::stdout(), terminal::EnterAlternateScreen)?;
                let t = Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
                self.full = Some(t);
            }
            if let Some(t) = &mut self.full {
                t.draw(|f| view::draw(f, app))?;
            }
            return Ok(());
        }
        if self.full.take().is_some() {
            execute!(std::io::stdout(), terminal::LeaveAlternateScreen)?;
            self.surface.restart(self.surface.top());
        }
        let (cols, rows) = terminal::size()?;
        self.draw_at(app, cols, rows)
    }

    /// Hand the terminal back to the shell (`ctrl+z`) and take it again
    /// once continued; the footer starts again under what the shell
    /// printed meanwhile.
    #[cfg(unix)]
    pub fn suspend(&mut self) {
        use ratatui::crossterm::{event, execute, terminal};
        self.surface.close();
        let _ = self.surface.flush();
        let _ = execute!(std::io::stdout(), event::DisableBracketedPaste);
        let _ = terminal::disable_raw_mode();
        // SAFETY: raising a signal at ourselves; the default action stops
        // the process until it is continued.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
        let _ = terminal::enable_raw_mode();
        let _ = execute!(std::io::stdout(), event::EnableBracketedPaste);
        // The shell has printed a line or two: start at the bottom (the
        // input thread is reading, so the cursor cannot be asked for).
        self.surface.restart(self.surface.rows.saturating_sub(1));
    }

    #[cfg(not(unix))]
    pub fn suspend(&mut self) {}

    /// Hand the terminal to another program (an external editor) in its
    /// normal mode, below the footer.
    pub fn release(&mut self) {
        use ratatui::crossterm::{event, execute, terminal};
        self.surface.close();
        let _ = self.surface.flush();
        let _ = execute!(std::io::stdout(), event::DisableBracketedPaste);
        let _ = terminal::disable_raw_mode();
    }

    /// Take the terminal back after [`release`](Self::release).
    pub fn retake(&mut self) {
        use ratatui::crossterm::{event, execute, terminal};
        let _ = terminal::enable_raw_mode();
        let _ = execute!(std::io::stdout(), event::EnableBracketedPaste);
        self.surface.restart(self.surface.rows.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests;
