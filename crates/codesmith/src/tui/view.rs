//! Drawing the UI: transcript, input box, status line and dialogs.

use super::app::{App, Entry, Modal, HELP};
use codesmith_core::tool::todo::Status;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

fn wrap(text: &str, width: usize) -> Vec<String> {
    let w = width.max(10);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        for piece in textwrap::wrap(line, textwrap::Options::new(w).break_words(true)) {
            out.push(piece.into_owned());
        }
    }
    out
}

fn styled(lines: Vec<String>, prefix: &str, style: Style) -> Vec<Line<'static>> {
    let pad = " ".repeat(prefix.width());
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            Line::from(vec![
                Span::styled(
                    if i == 0 {
                        prefix.to_string()
                    } else {
                        pad.clone()
                    },
                    style,
                ),
                Span::styled(l, style),
            ])
        })
        .collect()
}

/// The transcript as display lines for a given width.
pub fn transcript(app: &App, width: u16) -> Vec<Line<'static>> {
    let w = width.saturating_sub(4) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let dim = Style::default().add_modifier(Modifier::DIM);
    for e in &app.entries {
        match e {
            Entry::User(t) => {
                out.push(Line::default());
                out.extend(styled(
                    wrap(t, w),
                    "› ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                out.push(Line::default());
            }
            Entry::Assistant(t) => {
                for l in wrap(t.trim_start_matches('\n'), w) {
                    let style = if l.starts_with('#') {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    out.push(Line::from(Span::styled(l, style)));
                }
            }
            Entry::Reasoning(t) if app.show_reasoning => {
                out.extend(styled(
                    wrap(t.trim(), w),
                    "  ",
                    dim.add_modifier(Modifier::ITALIC),
                ));
            }
            Entry::Reasoning(_) => {}
            Entry::Tool {
                name,
                detail,
                result,
            } => {
                let mut head = vec![
                    Span::styled(format!("⚙ {name} "), Style::default().fg(Color::Cyan)),
                    Span::styled(detail.clone(), dim),
                ];
                if result.is_none() {
                    head.push(Span::styled(" …", dim));
                }
                out.push(Line::from(head));
                if let Some((first, err)) = result {
                    let (mark, style) = if *err {
                        ("  ✗ ", Style::default().fg(Color::Red))
                    } else {
                        ("  → ", dim)
                    };
                    let shown: String = first.chars().take(w.saturating_sub(4)).collect();
                    out.push(Line::from(Span::styled(format!("{mark}{shown}"), style)));
                }
            }
            Entry::Notice(t) => {
                out.extend(styled(wrap(t, w), "", Style::default().fg(Color::Yellow)))
            }
            Entry::Error(t) => out.extend(styled(
                wrap(t, w),
                "error: ",
                Style::default().fg(Color::Red),
            )),
            Entry::Todos(todos) => {
                for t in todos {
                    let (mark, style) = match t.status {
                        Status::Completed => ("✓", dim),
                        Status::InProgress => ("▶", Style::default().add_modifier(Modifier::BOLD)),
                        Status::Pending => ("○", Style::default()),
                        Status::Cancelled => ("✗", dim.add_modifier(Modifier::CROSSED_OUT)),
                    };
                    out.push(Line::from(Span::styled(
                        format!("  {mark} {}", t.content),
                        style,
                    )));
                }
            }
        }
    }
    out
}

/// Rows the input box needs (text plus borders), between 3 and 10.
pub fn input_height(input: &str, width: u16) -> u16 {
    let rows = wrap(input, width.saturating_sub(2) as usize).len().max(1) as u16;
    (rows + 2).clamp(3, 10)
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let ih = input_height(&app.input, area.width);
    let [head, body, input, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(ih),
        Constraint::Length(1),
    ])
    .areas(area);

    let title = app
        .header
        .title
        .clone()
        .unwrap_or_else(|| "new session".into());
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " CodeSmith ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" {} · {} · ", app.header.agent, app.header.model)),
            Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
        ])),
        head,
    );

    let lines = transcript(app, body.width);
    let visible = body.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let scroll = app.scroll.min(max_scroll);
    let start = lines.len().saturating_sub(visible + scroll);
    let shown: Vec<Line> = lines.into_iter().skip(start).take(visible).collect();
    f.render_widget(
        Paragraph::new(shown).block(Block::default().borders(Borders::NONE)),
        Rect {
            x: body.x + 1,
            width: body.width.saturating_sub(2),
            ..body
        },
    );

    let hint = if app.busy {
        " working… Esc to interrupt "
    } else {
        " Enter send · Alt+Enter newline · /help "
    };
    f.render_widget(
        Paragraph::new(app.input.as_str())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(hint)
                    .border_style(Style::default().fg(if app.busy {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    })),
            ),
        input,
    );
    if app.modal.is_none() {
        let rows = wrap(&app.input, input.width.saturating_sub(2) as usize);
        let last = rows.last().cloned().unwrap_or_default();
        let y =
            input.y + 1 + (rows.len().saturating_sub(1) as u16).min(input.height.saturating_sub(3));
        f.set_cursor_position((input.x + 1 + last.width() as u16, y));
    }

    let mut st = format!(" {} in · {} out", app.usage.input, app.usage.output);
    if app.usage.cache_read > 0 {
        st.push_str(&format!(" · {} cached", app.usage.cache_read));
    }
    st.push_str(&format!(" · {}", app.header.session));
    if app.scroll > 0 {
        st.push_str(&format!(
            " · scrolled {} lines (Ctrl+End to follow)",
            app.scroll.min(max_scroll)
        ));
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            st,
            Style::default().add_modifier(Modifier::DIM),
        )),
        status,
    );

    if let Some(m) = &app.modal {
        draw_modal(f, area, m);
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_modal(f: &mut Frame, area: Rect, m: &Modal) {
    let (title, body, color): (String, Vec<Line>, Color) = match m {
        Modal::Permission { req, feedback, .. } => {
            let mut lines = vec![
                Line::from(Span::styled(
                    req.title.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::default(),
                Line::from(format!("action: {}", req.action)),
                Line::default(),
                Line::from("[y] allow once   [a] always allow   [n] reject"),
                Line::from("or type feedback for the agent and press Enter to reject with it"),
            ];
            if !feedback.is_empty() {
                lines.push(Line::default());
                lines.push(Line::from(Span::styled(
                    format!("> {feedback}"),
                    Style::default().fg(Color::Yellow),
                )));
            }
            ("Permission required".into(), lines, Color::Yellow)
        }
        Modal::Question {
            prompts,
            idx,
            input,
            ..
        } => {
            let p = &prompts[*idx];
            let mut lines = vec![
                Line::from(Span::styled(
                    p.question.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::default(),
            ];
            for (i, o) in p.options.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::raw(format!("{}. {}", i + 1, o.label)),
                    Span::styled(
                        if o.description.is_empty() {
                            String::new()
                        } else {
                            format!(" — {}", o.description)
                        },
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]));
            }
            lines.push(Line::default());
            lines.push(Line::from(if p.multiple {
                "numbers separated by commas, or type an answer; Enter to confirm"
            } else {
                "a number, or type an answer; Enter to confirm, Esc to dismiss"
            }));
            lines.push(Line::from(Span::styled(
                format!("> {input}"),
                Style::default().fg(Color::Magenta),
            )));
            (
                format!("Question {}/{} {}", idx + 1, prompts.len(), p.header),
                lines,
                Color::Magenta,
            )
        }
        Modal::Sessions { list, sel } => {
            let mut lines = Vec::new();
            if list.is_empty() {
                lines.push(Line::from("No sessions in this directory yet."));
            }
            for (i, s) in list.iter().enumerate().take(30) {
                let style = if i == *sel {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(
                    format!(" {}  {} ", s.id, s.title.as_deref().unwrap_or("(untitled)")),
                    style,
                )));
            }
            lines.push(Line::default());
            lines.push(Line::from("↑/↓ choose · Enter continue · Esc close"));
            ("Sessions".into(), lines, Color::Cyan)
        }
        Modal::Help => (
            "Help".into(),
            HELP.lines().map(|l| Line::from(l.to_string())).collect(),
            Color::Cyan,
        ),
    };
    let width = 76u16;
    let height = (body.len() as u16 + 2).max(5);
    let r = centered(area, width, height);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} "))
                .border_style(Style::default().fg(color)),
        ),
        r,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wrapping() {
        assert_eq!(
            wrap("a b c d e f g h i j k l", 10),
            vec!["a b c d e", "f g h i j", "k l"]
        );
        assert_eq!(wrap("one\n\ntwo", 20), vec!["one", "", "two"]);
        assert_eq!(wrap(&"x".repeat(25), 10).len(), 3, "long words break");
    }

    #[test]
    fn transcript_shapes() {
        let mut a = App::new();
        a.entries = vec![
            Entry::User("hi".into()),
            Entry::Assistant("# Title\nbody".into()),
            Entry::Reasoning("secret".into()),
            Entry::Tool {
                name: "read".into(),
                detail: "a.rs".into(),
                result: Some(("Read file".into(), false)),
            },
            Entry::Tool {
                name: "shell".into(),
                detail: "ls".into(),
                result: None,
            },
        ];
        let t = text(&transcript(&a, 60));
        assert_eq!(t[1], "› hi");
        assert!(t.contains(&"# Title".to_string()) && t.contains(&"body".to_string()));
        assert!(
            !t.iter().any(|l| l.contains("secret")),
            "reasoning hidden by default"
        );
        assert!(t.contains(&"⚙ read a.rs".to_string()) && t.contains(&"  → Read file".to_string()));
        assert!(
            t.contains(&"⚙ shell ls …".to_string()),
            "running tools show an ellipsis"
        );
        a.show_reasoning = true;
        assert!(text(&transcript(&a, 60))
            .iter()
            .any(|l| l.contains("secret")));
    }

    #[test]
    fn input_box_grows_and_caps() {
        assert_eq!(input_height("", 40), 3);
        assert_eq!(input_height("a\nb\nc", 40), 5);
        assert_eq!(input_height(&"line\n".repeat(30), 40), 10);
    }

    #[test]
    fn renders_without_panicking_at_any_size() {
        let mut a = App::new();
        a.header.agent = "Build".into();
        a.header.model = "omlx/qwen".into();
        a.entries.push(Entry::Assistant("x ".repeat(500)));
        a.modal = Some(Modal::Help);
        for (w, h) in [(20, 6), (80, 24), (200, 60), (12, 4)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &a)).unwrap();
        }
    }

    #[test]
    fn header_and_status_text() {
        let mut a = App::new();
        a.header.agent = "Build".into();
        a.header.model = "p/m".into();
        a.header.session = "ses_1".into();
        a.header.title = Some("Fix tests".into());
        let mut t = Terminal::new(TestBackend::new(80, 10)).unwrap();
        t.draw(|f| draw(f, &a)).unwrap();
        let buf = t.backend().buffer().clone();
        let row = |y: u16| {
            (0..80)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert!(
            row(0).contains("CodeSmith") && row(0).contains("Build · p/m · Fix tests"),
            "{}",
            row(0)
        );
        assert!(row(9).contains("ses_1"), "{}", row(9));
    }
}
