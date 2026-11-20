//! Mini mode's footer: the live view of what the turn is doing (rows not
//! written to the scrollback yet: the answer's unfinished block, running
//! calls with their command output or subagent activity), autocomplete,
//! the prompt and a status line; or, while one waits, a permission,
//! question or picker dialog in place of the prompt. Keys go through the
//! keymap as in the full UI.

use super::settings::Settings;
use super::width::{self, Id, Tone};
use crate::tui::app::{App, Modal};
use crate::tui::theme;
use crate::tui::view;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// How the footer's rows are shared out, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Parts {
    pub live: u16,
    pub popup: u16,
    pub input: u16,
    pub status: u16,
    pub modal: u16,
}

impl Parts {
    pub fn height(&self) -> u16 {
        self.live + self.popup + self.input + self.status + self.modal
    }
}

/// Most rows the dialog boxes use for their text, as in the full UI.
const MODAL_WIDTH: u16 = 100;

fn modal_rows(app: &App, m: &Modal, cols: u16, room: u16) -> u16 {
    let room = room.max(5);
    match m {
        Modal::Permission {
            req,
            feedback,
            diff,
            from,
            scroll,
            ..
        } => {
            let width = MODAL_WIDTH.min(cols.saturating_sub(2)).saturating_sub(2) as usize;
            let lines = view::permission_lines(
                req,
                from.as_deref(),
                feedback,
                diff.as_ref(),
                scroll,
                width,
                room.saturating_sub(4).min(28) as usize,
                &view::permission_keys(&app.keymap),
            );
            (lines.len() as u16 + 4).min(room)
        }
        Modal::Question { form, .. } => {
            let width = MODAL_WIDTH.min(cols.saturating_sub(2)).saturating_sub(2) as usize;
            let lines = form.height(width, (cols, room), &app.keymap) as u16;
            (lines + 2).min(room)
        }
        Modal::Picker(pk) => (pk.shown.len().clamp(1, 20) as u16 + 9).min(room),
        Modal::Panel(_) => {
            let n = app.subagents().len().max(app.shells.len()).clamp(1, 10) as u16;
            (n + 6).min(room)
        }
        Modal::ShellOutput(_) => room,
        Modal::Mcp(v) => ((v.servers.len().clamp(1, 12) + 6) as u16).min(room),
        Modal::Help(_) | Modal::DiffHelp(_) => room,
    }
}

/// The footer's layout on a `cols` by `rows` terminal with `live` rows to
/// show.
pub fn parts(app: &App, live: usize, cols: u16, rows: u16) -> Parts {
    let live_max = (rows * 2 / 5).clamp(3, 16);
    if let Some(m) = &app.modal {
        let live = (live as u16).min(4).min(rows.saturating_sub(8));
        let modal = modal_rows(app, m, cols, rows.saturating_sub(live));
        return Parts {
            live,
            modal,
            ..Default::default()
        };
    }
    let input = view::input_height(app, cols).min(rows.saturating_sub(1).max(1));
    let status = 1.min(rows.saturating_sub(input));
    let popup = app
        .popup
        .as_ref()
        .map_or(0, |p| p.items.len().min(8) as u16 + 2)
        .min(rows.saturating_sub(input + status));
    let room = rows.saturating_sub(input + status + popup);
    Parts {
        live: (live as u16).min(live_max).min(room),
        popup,
        input,
        status,
        modal: 0,
    }
}

/// The status line's facts, for [`width::policy`].
pub fn status(
    app: &App,
    s: &Settings,
    spinner: Option<&str>,
    elapsed: Option<u64>,
    cols: u16,
) -> width::Input {
    let k = &app.keymap;
    let mut input = width::Input {
        width: cols as usize,
        mono: s.mono,
        ..Default::default()
    };
    if app.busy {
        input.spinner = spinner.map(str::to_string);
        let secs = elapsed.unwrap_or(0) / 1000;
        input.status = Some(("working".into(), Some(format!("working {secs}s"))));
        let key = k.hint("session.interrupt");
        if !key.is_empty() {
            input.escape = Some(width::action(&key, "interrupt"));
        }
    }
    if !s.footer {
        return input;
    }
    if app.background > 0 {
        input.work.push((
            Id::Queued,
            width::Action {
                key: String::new(),
                label: format!("{} bg", app.background),
                expanded: Some(format!(
                    "{} background command{}",
                    app.background,
                    if app.background == 1 { "" } else { "s" }
                )),
            },
        ));
    }
    if !app.header.model.is_empty() {
        input.model = Some((app.header.model.clone(), None));
    }
    if !app.header.agent.is_empty() {
        input.agent = Some(app.header.agent.clone());
    }
    if app.context > 0 {
        let t = view::tokens(app.context);
        input.context = Some(match app.context_percent() {
            Some(p) => (format!("{p}% ctx"), format!("{t} ({p}%)")),
            None => (format!("{t} ctx"), format!("{t} tokens")),
        });
    }
    if let Some(c) = app.cost() {
        input.cost = Some(format!("${c:.2}"));
    }
    let help = k.hint("help.show");
    if !help.is_empty() {
        input.menu = Some(width::action(&help, "help"));
    }
    input
}

fn status_line(l: &width::Layout) -> Line<'static> {
    let p = theme::current();
    let mut spans = Vec::new();
    for (i, g) in l.groups.iter().enumerate() {
        if i > 0 {
            let sep = if l.groups[i - 1].id == Id::Spinner {
                " "
            } else {
                " \u{b7} "
            };
            spans.push(Span::styled(sep, Style::default().fg(p.faint)));
        }
        for (text, tone) in &g.parts {
            let st = match tone {
                Tone::Text => Style::default().fg(p.text),
                Tone::Muted => Style::default().fg(p.muted),
                Tone::Agent => Style::default().fg(p.agent).add_modifier(Modifier::BOLD),
                Tone::Status => Style::default().fg(p.warning),
            };
            spans.push(Span::styled(text.replace('\n', " "), st));
        }
    }
    Line::from(spans)
}

/// Draw the footer laid out as `parts` (the frame is exactly its size).
pub fn draw(
    f: &mut Frame,
    app: &App,
    live: &[Line<'static>],
    status: &width::Layout,
    parts: Parts,
) {
    let area = f.area();
    let row = |y: u16, h: u16| Rect {
        x: area.x,
        y: area.y + y,
        width: area.width,
        height: h,
    };
    let mut y = 0;
    if parts.live > 0 {
        let skip = live.len().saturating_sub(parts.live as usize);
        let shown: Vec<Line> = live[skip..].to_vec();
        f.render_widget(Paragraph::new(shown), row(y, parts.live));
        y += parts.live;
    }
    if let Some(m) = &app.modal {
        view::draw_modal(f, row(y, parts.modal), m, app);
        return;
    }
    let popup = row(y, parts.popup);
    y += parts.popup;
    let input = row(y, parts.input);
    view::draw_input(f, app, input);
    y += parts.input;
    if parts.popup > 0 {
        view::draw_popup(f, app, popup, input);
    }
    if parts.status > 0 {
        f.render_widget(Paragraph::new(status_line(status)), row(y, parts.status));
    }
}
