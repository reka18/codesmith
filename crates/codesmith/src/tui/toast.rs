//! Toasts: short messages at the top right that go after a while, one at
//! a time with the rest queued (`+2 more`), as OpenCode's. A failure in a
//! session that is not shown gains the session's title.

use super::theme;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long a toast stays, as OpenCode's default.
pub const DURATION: Duration = Duration::from_millis(5000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Toast {
    pub title: Option<String>,
    pub message: String,
    pub variant: Variant,
    pub duration: Duration,
}

impl Toast {
    pub fn new(variant: Variant, message: impl Into<String>) -> Toast {
        Toast {
            title: None,
            message: message.into(),
            variant,
            duration: DURATION,
        }
    }
}

#[derive(Debug, Default)]
pub struct Toasts {
    current: Option<(Toast, Instant)>,
    queue: VecDeque<Toast>,
}

impl Toasts {
    /// Show `t`, or queue it behind the toasts waiting.
    pub fn show(&mut self, t: Toast, now: Instant) {
        if self.current.is_some() {
            self.queue.push_back(t);
            return;
        }
        self.current = Some((t, now));
    }

    pub fn current(&self) -> Option<&Toast> {
        self.current.as_ref().map(|(t, _)| t)
    }

    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Take the shown toast down; the next one shows.
    pub fn dismiss(&mut self, now: Instant) {
        self.current = self.queue.pop_front().map(|t| (t, now));
    }

    /// Let the toasts whose time is up go. Returns how long until the next
    /// one does, while one shows.
    pub fn tick(&mut self, now: Instant) -> Option<Duration> {
        while let Some((t, since)) = &self.current {
            let end = *since + t.duration;
            if now < end {
                return Some(end - now);
            }
            self.dismiss(end.min(now));
        }
        None
    }
}

fn lines(t: &Toast, pending: usize) -> Vec<Line<'static>> {
    let p = theme::current();
    let safe = |s: &str| super::view::display_safe(s);
    let mut out = Vec::new();
    match &t.title {
        Some(title) => {
            out.push(Line::from(Span::styled(
                safe(title),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            )));
            out.push(Line::from(Span::styled(
                safe(&t.message),
                Style::default().fg(p.text),
            )));
        }
        None => out.push(Line::from(Span::styled(
            safe(&t.message),
            Style::default().fg(p.text),
        ))),
    }
    if pending > 0 {
        out.push(Line::default());
        out.push(Line::from(Span::styled(
            format!("+{pending} more"),
            Style::default().fg(p.muted),
        )));
    }
    out
}

/// Where the shown toast is drawn on `area`.
pub fn rect(toasts: &Toasts, area: Rect) -> Option<Rect> {
    let t = toasts.current()?;
    let max = 60.min(area.width.saturating_sub(6));
    if max < 10 || area.height < 4 {
        return None;
    }
    let body = lines(t, toasts.pending());
    let widest = body.iter().map(Line::width).max().unwrap_or(0) as u16;
    // Side borders and two columns of padding each side.
    let width = (widest + 6).min(max);
    let inner = (width - 6).max(1) as usize;
    let rows: usize = body
        .iter()
        .map(|l| (l.width().max(1)).div_ceil(inner))
        .sum();
    let height = (rows as u16 + 2).min(area.height.saturating_sub(1));
    Some(Rect {
        x: area.right().saturating_sub(width + 2),
        y: area.y + 1,
        width,
        height,
    })
}

/// Whether the toast covers the terminal cursor at `pos` (the cursor is
/// drawn above every cell, so it is hidden there, as OpenCode does).
pub fn covers(toasts: &Toasts, area: Rect, pos: Position) -> bool {
    rect(toasts, area).is_some_and(|r| r.contains(pos))
}

pub fn draw(buf: &mut Buffer, area: Rect, toasts: &Toasts) {
    let (Some(t), Some(r)) = (toasts.current(), rect(toasts, area)) else {
        return;
    };
    let p = theme::current();
    let color = match t.variant {
        Variant::Info => p.info,
        Variant::Success => p.success,
        Variant::Error => p.error,
    };
    Clear.render(r, buf);
    Paragraph::new(lines(t, toasts.pending()))
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(p.raised_high))
        .block(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT)
                .border_style(Style::default().fg(color))
                .padding(ratatui::widgets::Padding::new(2, 2, 1, 1))
                .style(Style::default().bg(p.raised_high)),
        )
        .render(r, buf);
}
