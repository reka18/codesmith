//! The terminal's own colors: asking for them (OSC 10, 11 and 4), the
//! light or dark mode its background implies, and OpenCode's `system`
//! theme built from them (`theme/system.ts`).

use super::color::{ansi, tint, Rgba};
use super::doc::Mode;
use serde_json::{json, Map, Value};

/// What the terminal reported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalColors {
    /// The 16 ANSI colors it reported (unanswered ones are `None`).
    pub palette: Vec<Option<Rgba>>,
    pub foreground: Option<Rgba>,
    pub background: Option<Rgba>,
}

impl TerminalColors {
    /// Whether there is enough to build the `system` theme from.
    pub fn usable(&self) -> bool {
        let slot = |i: usize| self.palette.get(i).copied().flatten();
        self.background.or(slot(0)).is_some() && self.foreground.or(slot(7)).is_some()
    }
}

/// The mode the terminal's background implies; nothing without one (the
/// first ANSI color is not the background).
pub fn terminal_mode(colors: &TerminalColors) -> Option<Mode> {
    let bg = colors.background?;
    Some(if bg.luminance() > 0.5 {
        Mode::Light
    } else {
        Mode::Dark
    })
}

fn gray_scale(bg: Rgba, dark: bool) -> [Rgba; 13] {
    let mut out = [Rgba::default(); 13];
    let (r, g, b) = (bg.r as f64, bg.g as f64, bg.b as f64);
    let lum = 0.299 * r + 0.587 * g + 0.114 * b;
    for (i, slot) in out.iter_mut().enumerate().skip(1) {
        let f = i as f64 / 12.0;
        if dark && lum < 10.0 {
            let v = (f * 0.4 * 255.0).floor() as u8;
            *slot = Rgba::rgb(v, v, v);
            continue;
        }
        if !dark && lum > 245.0 {
            let v = (255.0 - f * 0.4 * 255.0).floor() as u8;
            *slot = Rgba::rgb(v, v, v);
            continue;
        }
        let next = if dark {
            lum + (255.0 - lum) * f * 0.4
        } else {
            lum * (1.0 - f * 0.4)
        };
        let ratio = next / lum;
        let ch = |c: f64| (c * ratio).clamp(0.0, 255.0).floor() as u8;
        *slot = Rgba::rgb(ch(r), ch(g), ch(b));
    }
    out
}

fn muted_text(bg: Rgba, dark: bool) -> Rgba {
    let lum = 0.299 * bg.r as f64 + 0.587 * bg.g as f64 + 0.114 * bg.b as f64;
    let v = if dark {
        if lum < 10.0 {
            180.0
        } else {
            (160.0 + lum * 0.3).floor().min(200.0)
        }
    } else if lum > 245.0 {
        75.0
    } else {
        (100.0 - (255.0 - lum) * 0.2).floor().max(60.0)
    };
    let v = v as u8;
    Rgba::rgb(v, v, v)
}

/// OpenCode's `system` theme: a V1 theme from the terminal's colors, on a
/// transparent page so the terminal's own background shows.
pub fn generate(colors: &TerminalColors, mode: Mode) -> Value {
    let slot = |i: usize| {
        colors
            .palette
            .get(i)
            .copied()
            .flatten()
            .unwrap_or_else(|| ansi(i as u64))
    };
    let bg = colors.background.unwrap_or_else(|| slot(0));
    let fg = colors.foreground.unwrap_or_else(|| slot(7));
    let dark = mode == Mode::Dark;
    let grays = gray_scale(bg, dark);
    let muted = muted_text(bg, dark);
    let (red, green, yellow, blue, magenta, cyan) =
        (slot(1), slot(2), slot(3), slot(4), slot(5), slot(6));
    let (red_bright, green_bright) = (slot(9), slot(10));
    let alpha = if dark { 0.22 } else { 0.14 };
    let context_bg = grays[2];
    let h = |c: Rgba| Value::String(c.hex());
    let transparent = Rgba::new(bg.r, bg.g, bg.b, 0);
    let mut theme = Map::new();
    let mut set = |k: &str, c: Rgba| {
        theme.insert(k.to_string(), h(c));
    };
    set("primary", cyan);
    set("secondary", magenta);
    set("accent", cyan);
    set("error", red);
    set("warning", yellow);
    set("success", green);
    set("info", cyan);
    set("text", fg);
    set("textMuted", muted);
    set("selectedListItemText", bg);
    set("background", transparent);
    set("backgroundPanel", grays[2]);
    set("backgroundElement", grays[3]);
    set("backgroundMenu", grays[3]);
    set("borderSubtle", grays[6]);
    set("border", grays[7]);
    set("borderActive", grays[8]);
    set("diffAdded", green);
    set("diffRemoved", red);
    set("diffContext", grays[7]);
    set("diffHunkHeader", grays[7]);
    set("diffHighlightAdded", green_bright);
    set("diffHighlightRemoved", red_bright);
    set("diffAddedBg", tint(bg, green, alpha));
    set("diffRemovedBg", tint(bg, red, alpha));
    set("diffContextBg", context_bg);
    set("diffLineNumber", muted);
    set("diffAddedLineNumberBg", tint(context_bg, green, alpha));
    set("diffRemovedLineNumberBg", tint(context_bg, red, alpha));
    set("markdownText", fg);
    set("markdownHeading", fg);
    set("markdownLink", blue);
    set("markdownLinkText", cyan);
    set("markdownCode", green);
    set("markdownBlockQuote", yellow);
    set("markdownEmph", yellow);
    set("markdownStrong", fg);
    set("markdownHorizontalRule", grays[7]);
    set("markdownListItem", blue);
    set("markdownListEnumeration", cyan);
    set("markdownImage", blue);
    set("markdownImageText", cyan);
    set("markdownCodeBlock", fg);
    set("syntaxComment", muted);
    set("syntaxKeyword", magenta);
    set("syntaxFunction", blue);
    set("syntaxVariable", fg);
    set("syntaxString", green);
    set("syntaxNumber", yellow);
    set("syntaxType", cyan);
    set("syntaxOperator", cyan);
    set("syntaxPunctuation", fg);
    json!({ "theme": theme })
}

/// The queries for the foreground, background and 16 ANSI colors, ended
/// by a device attributes request every terminal answers, so the reply
/// is known to be complete when its answer arrives.
pub const QUERY: &str = concat!(
    "\x1b]10;?\x07\x1b]11;?\x07",
    "\x1b]4;0;?\x07\x1b]4;1;?\x07\x1b]4;2;?\x07\x1b]4;3;?\x07",
    "\x1b]4;4;?\x07\x1b]4;5;?\x07\x1b]4;6;?\x07\x1b]4;7;?\x07",
    "\x1b]4;8;?\x07\x1b]4;9;?\x07\x1b]4;10;?\x07\x1b]4;11;?\x07",
    "\x1b]4;12;?\x07\x1b]4;13;?\x07\x1b]4;14;?\x07\x1b]4;15;?\x07",
    "\x1b[c"
);

/// `rgb:RRRR/GGGG/BBBB` (1 to 4 hex digits a channel) or `#rrggbb`.
fn parse_color(s: &str) -> Option<Rgba> {
    if let Some(rest) = s.strip_prefix("rgb:").or_else(|| s.strip_prefix("rgba:")) {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() < 3 {
            return None;
        }
        // One to four hex digits (the terminal's reply is untrusted input:
        // a longer channel must not overflow the shift).
        let ch = |p: &str| -> Option<u8> {
            if p.is_empty() || p.len() > 4 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let v = u32::from_str_radix(p, 16).ok()?;
            let max = (1u32 << (4 * p.len() as u32)) - 1;
            Some(((v as f64 / max as f64) * 255.0).round() as u8)
        };
        return Some(Rgba::rgb(ch(parts[0])?, ch(parts[1])?, ch(parts[2])?));
    }
    Rgba::from_hex(s)
}

/// The colors in the terminal's replies, and whether the closing device
/// attributes answer has arrived.
pub fn parse_replies(bytes: &[u8]) -> (TerminalColors, bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut out = TerminalColors {
        palette: vec![None; 16],
        ..Default::default()
    };
    for seq in text.split("\x1b]").skip(1) {
        let body = seq.split(['\x07', '\x1b']).next().unwrap_or_default();
        let mut parts = body.splitn(3, ';');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("10"), Some(c), None) => out.foreground = parse_color(c),
            (Some("11"), Some(c), None) => out.background = parse_color(c),
            (Some("4"), Some(i), Some(c)) => {
                if let Ok(i) = i.parse::<usize>() {
                    if i < 16 {
                        out.palette[i] = parse_color(c);
                    }
                }
            }
            _ => {}
        }
    }
    let done = text.split("\x1b[?").skip(1).any(|s| {
        s.trim_start_matches(|c: char| c.is_ascii_digit() || c == ';')
            .starts_with('c')
    });
    (out, done)
}

/// Ask the terminal for its colors, waiting at most `timeout` for the
/// answer. Needs raw mode; nothing on terminals that do not answer, and
/// outside Unix.
pub fn query(timeout: std::time::Duration) -> Option<TerminalColors> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        // SAFETY: isatty only inspects the descriptor.
        if unsafe { libc::isatty(fd) } != 1 {
            return None;
        }
        let mut out = std::io::stdout();
        out.write_all(QUERY.as_bytes()).ok()?;
        out.flush().ok()?;
        let deadline = std::time::Instant::now() + timeout;
        let mut buf = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            let n =
                unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
            if n <= 0 {
                break;
            }
            let mut chunk = [0u8; 1024];
            // SAFETY: reading into a local buffer of the given length.
            let r = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if r <= 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..r as usize]);
            if parse_replies(&buf).1 {
                break;
            }
        }
        let (colors, _) = parse_replies(&buf);
        (colors.background.is_some() || colors.palette.iter().any(Option::is_some))
            .then_some(colors)
    }
    #[cfg(not(unix))]
    {
        let _ = timeout;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_channels_must_be_one_to_four_hex_digits() {
        assert_eq!(
            parse_color("rgb:ffff/0000/8080"),
            Some(Rgba::rgb(255, 0, 128))
        );
        assert_eq!(parse_color("rgb:f/0/8"), Some(Rgba::rgb(255, 0, 136)));
        for bad in [
            "rgb:000000001/0/0",
            "rgb:fffffffff/0/0",
            "rgb:+f/0/0",
            "rgb:/0/0",
            "rgb:fffff/0/0",
            "rgb:gg/0/0",
        ] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
        // Through the reply parser too.
        let (c, _) = parse_replies(b"\x1b]11;rgb:000000001/0/0\x07");
        assert_eq!(c.background, None);
    }

    #[test]
    fn replies_are_parsed() {
        let r = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b]10;rgb:00/00/00\x07\x1b]4;1;rgb:cdcd/0000/0000\x07\x1b[?62;22c";
        let (c, done) = parse_replies(r);
        assert!(done);
        assert_eq!(c.background, Some(Rgba::rgb(255, 255, 255)));
        assert_eq!(c.foreground, Some(Rgba::rgb(0, 0, 0)));
        assert_eq!(c.palette[1], Some(Rgba::rgb(205, 0, 0)));
        assert_eq!(terminal_mode(&c), Some(Mode::Light));
        let (_, done) = parse_replies(b"\x1b]11;rgb:0/0/0\x07");
        assert!(!done);
    }
}
