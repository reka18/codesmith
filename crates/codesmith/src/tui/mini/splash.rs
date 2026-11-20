//! The entry and exit banners mini mode writes to the scrollback, as
//! OpenCode's (`mini/splash`): `▪ cs mini v0.1.0 · ~/src/project` as the
//! session opens, and the session's title and the command that resumes
//! it as it closes. Both are written once and stay as terminal history.

use super::width::take_width;
use crate::tui::theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

const TITLE_LIMIT: usize = 50;
const TITLE_FALLBACK: &str = "Untitled session";

/// The entry banner's parts at a width.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    pub label: String,
    pub version: String,
    pub path: String,
    /// The version and path as drawn after the label.
    pub metadata: String,
}

/// The entry banner at `width`: the label, then (stage by stage, while it
/// fits) the directory's name, the version, and longer ends of its path up
/// to the whole of it. It stops at the first stage that does not fit.
pub fn entry_layout(width: usize, version: &str, detail: Option<&str>, mono: bool) -> Entry {
    let detail = detail.unwrap_or("");
    let segments: Vec<&str> = detail
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    let leaf = segments.last().copied().unwrap_or(detail);
    let separator = if mono { " - " } else { " \u{b7} " };
    let ellipsis = if mono { "..." } else { "\u{2026}" };
    let slash = if detail.contains('\\') { "\\" } else { "/" };
    let mut paths: Vec<String> = (1..segments.len())
        .map(|i| format!("{ellipsis}{slash}{}", segments[i..].join(slash)))
        .filter(|p| p.width() < detail.width())
        .collect();
    paths.sort_by_key(|p| p.width());
    paths.dedup();
    let mut layout = Entry {
        label: take_width("cs mini", width).to_string(),
        ..Default::default()
    };
    let marked = format!("{} cs mini", if mono { "[O]" } else { "\u{25aa}" });
    let mut stages: Vec<Entry> = vec![Entry {
        label: marked,
        ..Default::default()
    }];
    let stage = |version: Option<&str>, path: Option<&str>| Entry {
        version: version.unwrap_or_default().to_string(),
        path: path.unwrap_or_default().to_string(),
        ..Default::default()
    };
    if !leaf.is_empty() {
        stages.push(stage(None, Some(leaf)));
    }
    if !version.is_empty() {
        stages.push(stage(Some(version), None));
    }
    for p in &paths {
        stages.push(stage(None, Some(p)));
    }
    if !detail.is_empty() {
        stages.push(stage(None, Some(detail)));
    }
    for s in stages {
        let mut next = layout.clone();
        if !s.label.is_empty() {
            next.label = s.label;
        }
        if !s.version.is_empty() {
            next.version = s.version;
        }
        if !s.path.is_empty() {
            next.path = s.path;
        }
        let mut metadata = String::new();
        if !next.version.is_empty() {
            metadata.push_str(&format!(" v{}", next.version));
        }
        if !next.path.is_empty() {
            metadata.push_str(separator);
            metadata.push_str(&next.path);
        }
        if format!("{}{metadata}", next.label).width() > width {
            break;
        }
        next.metadata = metadata;
        layout = next;
    }
    layout
}

/// The directory as the banner shows it: under the home directory as `~/…`.
pub fn directory_label(dir: &std::path::Path, home: Option<&std::path::Path>) -> String {
    if let Some(rest) = home.and_then(|h| dir.strip_prefix(h).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    dir.display().to_string()
}

/// The entry banner's rows: a blank one, then the label and its metadata.
pub fn entry_lines(
    width: usize,
    version: &str,
    detail: Option<&str>,
    mono: bool,
) -> Vec<Line<'static>> {
    let p = theme::current();
    let e = entry_layout(width, version, detail, mono);
    vec![
        Line::default(),
        Line::from(vec![
            Span::styled(e.label, Style::default().fg(p.text)),
            Span::styled(e.metadata, Style::default().fg(p.muted)),
        ]),
    ]
}

/// A session title as the exit banner shows it: whitespace runs as one
/// space, at most 50 characters.
pub fn title(text: Option<&str>, mono: bool) -> String {
    let value = text
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if value.is_empty() {
        return TITLE_FALLBACK.into();
    }
    if value.chars().count() <= TITLE_LIMIT {
        return value;
    }
    let (keep, mark) = if mono {
        (TITLE_LIMIT - 3, "...")
    } else {
        (TITLE_LIMIT - 1, "\u{2026}")
    };
    format!("{}{mark}", value.chars().take(keep).collect::<String>())
}

/// The command that resumes a session.
pub fn resume_command(session: &str) -> String {
    format!("codesmith mini -s {session}")
}

/// The exit banner's rows. With room, `Session  <title>` and `Continue
/// <command>`; on a narrow terminal only the command, wrapped by
/// character so it can still be copied whole.
pub fn exit_lines(
    width: usize,
    title_text: Option<&str>,
    session: &str,
    show_session: bool,
    mono: bool,
) -> Vec<Line<'static>> {
    let p = theme::current();
    let width = width.max(1);
    let (left, right) = (Style::default().fg(p.muted), Style::default().fg(p.text));
    let command = resume_command(session);
    let label = "Continue ";
    let mut out = vec![Line::default()];
    if label.width() + command.width() <= width {
        if show_session {
            let t = title(title_text, mono);
            out.push(Line::from(vec![
                Span::styled("Session  ", left),
                Span::styled(
                    take_width(&t, width.saturating_sub(9)).to_string(),
                    right.add_modifier(ratatui::style::Modifier::BOLD),
                ),
            ]));
        }
        out.push(Line::from(vec![
            Span::styled(label, left),
            Span::styled(command, right.add_modifier(ratatui::style::Modifier::BOLD)),
        ]));
        return out;
    }
    let mut rest = command.as_str();
    while !rest.is_empty() {
        let head = take_width(rest, width);
        let head = if head.is_empty() {
            &rest[..rest.chars().next().map_or(0, char::len_utf8)]
        } else {
            head
        };
        out.push(Line::from(Span::styled(
            head.to_string(),
            right.add_modifier(ratatui::style::Modifier::BOLD),
        )));
        rest = &rest[head.len()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREVIEW: &str = "1.18.4-preview.abcd1234567890";
    const MARK: &str = "\u{25aa}";

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    // mini/splash.test.ts: "entry renders metadata with distinct
    // foreground roles" (three cases; adapted: CodeSmith's label is
    // `cs mini`, and the roles are the theme's text and muted colors).
    #[test]
    fn oc_entry_renders_metadata_with_distinct_foreground_roles() {
        for (width, mono, detail, metadata) in [
            (80, false, "~/src/wt/oc-mini-v2", " vlocal \u{b7} ~/src/wt/oc-mini-v2"),
            (80, true, "~/src/wt/oc-mini-v2", " vlocal - ~/src/wt/oc-mini-v2"),
            (
                20,
                false,
                "/home/\u{7814}\u{7a76}/\u{9577}\u{3044}\u{30c7}\u{30a3}\u{30ec}\u{30af}\u{30c8}\u{30ea}/\u{753b}\u{9762}/\u{8a2d}\u{5b9a}/\u{754c}e\u{301}\u{1f642}",
                " \u{b7} \u{754c}e\u{301}\u{1f642}",
            ),
        ] {
            let lines = entry_lines(width, "local", Some(detail), mono);
            let label = format!("{} cs mini", if mono { "[O]" } else { MARK });
            assert_eq!(text(&lines[0]), "");
            assert_eq!(text(&lines[1]), format!("{label}{metadata}"));
            let p = theme::current();
            assert_eq!(lines[1].spans[0].content, label);
            assert_eq!(lines[1].spans[0].style.fg, Some(p.text));
            assert_eq!(lines[1].spans[1].style.fg, Some(p.muted));
            assert!(lines[1].spans.iter().all(|s| s.style.add_modifier.is_empty()));
        }
    }

    // "entry progressively admits the basename, whole version, and parent
    // directories" (eight cases).
    #[test]
    fn oc_entry_progressively_admits_basename_version_and_parents() {
        for (width, mono, version, expected) in [
            (20, false, "local", format!("{MARK} cs mini")),
            (
                22,
                false,
                "local",
                format!("{MARK} cs mini \u{b7} oc-mini-v2"),
            ),
            (
                29,
                false,
                "local",
                format!("{MARK} cs mini vlocal \u{b7} oc-mini-v2"),
            ),
            (
                31,
                false,
                "local",
                format!("{MARK} cs mini vlocal \u{b7} \u{2026}/oc-mini-v2"),
            ),
            (20, false, PREVIEW, format!("{MARK} cs mini")),
            (
                24,
                false,
                PREVIEW,
                format!("{MARK} cs mini \u{b7} oc-mini-v2"),
            ),
            (24, true, PREVIEW, "[O] cs mini - oc-mini-v2".to_string()),
            (
                32,
                false,
                PREVIEW,
                format!("{MARK} cs mini \u{b7} oc-mini-v2"),
            ),
        ] {
            let l = entry_layout(width, version, Some("~/src/wt/oc-mini-v2"), mono);
            assert_eq!(format!("{}{}", l.label, l.metadata), expected, "{width}");
        }
    }

    // "entry handles absent and root locations" (five cases).
    #[test]
    fn oc_entry_handles_absent_and_root_locations() {
        for detail in [
            None,
            Some(""),
            Some("/"),
            Some("~/"),
            Some("C:\\projects\\mini\\"),
        ] {
            let l = entry_layout(80, "local", detail, false);
            let want = match detail {
                Some(d) if !d.is_empty() => format!(" vlocal \u{b7} {d}"),
                _ => " vlocal".to_string(),
            };
            assert_eq!(l.metadata, want, "{detail:?}");
        }
    }

    fn suffix(v: &str) -> String {
        let v = v
            .strip_prefix("\u{2026}/")
            .or_else(|| v.strip_prefix("\u{2026}\\"))
            .or_else(|| v.strip_prefix(".../"))
            .or_else(|| v.strip_prefix("...\\"))
            .unwrap_or(v);
        let mut out = String::new();
        for c in v.chars() {
            let c = if c == '\\' { '/' } else { c };
            if !(c == '/' && out.ends_with('/')) {
                out.push(c);
            }
        }
        out.trim_end_matches('/').to_string()
    }

    // "entry layout preserves admitted information at every width
    // (mono=false|true)".
    #[test]
    fn oc_entry_layout_preserves_admitted_information_at_every_width() {
        for mono in [false, true] {
            for detail in [
                None,
                Some(""),
                Some("/"),
                Some("~/"),
                Some("x/y"),
                Some("a/b/c"),
                Some("~/src/wt/oc-mini-v2"),
                Some("C:\\projects\\mini\\"),
                Some("/home/\u{7814}\u{7a76}/\u{753b}\u{9762}/\u{754c}e\u{301}\u{1f642}"),
                Some("~/project-directory-that-cannot-meaningfully-fit"),
            ] {
                for version in ["", "local", PREVIEW] {
                    let mut previous = entry_layout(0, version, detail, mono);
                    for width in 1..=160 {
                        let l = entry_layout(width, version, detail, mono);
                        assert!(format!("{}{}", l.label, l.metadata).width() <= width);
                        assert!(l.version.is_empty() || l.version == version);
                        if !previous.version.is_empty() {
                            assert_eq!(l.version, previous.version);
                        }
                        if !previous.path.is_empty() {
                            assert!(!l.path.is_empty());
                            assert!(suffix(&l.path).ends_with(&suffix(&previous.path)));
                        }
                        if !l.version.is_empty() && detail.is_some_and(|d| !d.is_empty()) {
                            assert!(!l.path.is_empty());
                        }
                        if !l.path.is_empty() {
                            assert!(suffix(detail.unwrap()).ends_with(&suffix(&l.path)));
                        }
                        let marked = format!("{} cs mini", if mono { "[O]" } else { MARK });
                        let want = if marked.width() <= width {
                            marked
                        } else {
                            "cs mini".chars().take(width).collect()
                        };
                        assert_eq!(l.label, want);
                        previous = l;
                    }
                    assert_eq!(previous.path, detail.unwrap_or(""));
                    assert_eq!(previous.version, version);
                }
            }
        }
    }

    // "entry skips abbreviated paths that are longer than the full path".
    #[test]
    fn oc_entry_skips_abbreviated_paths_longer_than_the_full_path() {
        for mono in [false, true] {
            let label = format!("{} cs mini", if mono { "[O]" } else { MARK });
            let metadata = if mono {
                " vlocal - a/b/c"
            } else {
                " vlocal \u{b7} a/b/c"
            };
            let l = entry_layout(
                format!("{label}{metadata}").width(),
                "local",
                Some("a/b/c"),
                mono,
            );
            assert_eq!(
                (l.label.as_str(), l.metadata.as_str(), l.path.as_str()),
                (label.as_str(), metadata, "a/b/c")
            );
        }
    }

    // "exit retains the complete resume command" (eight cases; adapted:
    // CodeSmith's banner has no logo, so the wide form needs only room for
    // `Continue <command>`).
    #[test]
    fn oc_exit_retains_the_complete_resume_command() {
        let id = "ses_fac1eb1b0ffeOk15TDttH2E1Oy";
        let command = resume_command(id);
        for (width, show) in [(16, true), (40, true), (80, true), (80, false)] {
            for mono in [false, true] {
                let rows: Vec<String> =
                    exit_lines(width, Some("Review mini layout"), id, show, mono)
                        .iter()
                        .map(text)
                        .collect();
                assert_eq!(rows[0], "");
                let wide = width >= 9 + command.len();
                if wide {
                    let last = rows.last().unwrap();
                    assert_eq!(last, &format!("Continue {command}"));
                    assert_eq!(
                        rows.iter().any(|r| r == "Session  Review mini layout"),
                        show
                    );
                } else {
                    let joined: String = rows[1..].concat();
                    assert_eq!(joined, command);
                    assert!(rows
                        .iter()
                        .all(|r| !r.contains("Session") && !r.contains("Continue")));
                    assert_eq!(rows.len(), 1 + command.len().div_ceil(width));
                }
                if mono {
                    assert!(rows.concat().is_ascii());
                }
            }
        }
    }

    #[test]
    fn titles_collapse_whitespace_and_truncate() {
        assert_eq!(title(None, false), "Untitled session");
        assert_eq!(title(Some("  \n "), false), "Untitled session");
        assert_eq!(title(Some(" Fix\tthe \n build "), false), "Fix the build");
        let long = "x".repeat(60);
        assert_eq!(title(Some(&long), false).chars().count(), 50);
        assert!(title(Some(&long), true).ends_with("..."));
    }

    #[test]
    fn home_directories_are_abbreviated() {
        use std::path::Path;
        let home = Path::new("/home/me");
        assert_eq!(
            directory_label(Path::new("/home/me/src/x"), Some(home)),
            "~/src/x"
        );
        assert_eq!(directory_label(Path::new("/home/me"), Some(home)), "~");
        assert_eq!(directory_label(Path::new("/srv/x"), Some(home)), "/srv/x");
    }
}
