//! Fitting paths and labels into a width: OpenCode's `truncateFilePath`
//! (`tui/src/ui/file-path`) and `Locale.truncateMiddle`. Widths are
//! terminal columns, and graphemes are never split.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Columns `s` takes in a terminal, a grapheme at a time.
fn width(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

/// An emoji sequence (`👩‍💻`) is one wide cell, not the sum of its parts.
fn grapheme_width(g: &str) -> usize {
    g.width().min(2)
}

/// The leading (or, with `end`, trailing) whole graphemes of `s` that fit
/// in `max` columns.
fn take(s: &str, max: usize, end: bool) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut w = 0;
    let graphemes: Vec<&str> = if end {
        s.graphemes(true).rev().collect()
    } else {
        s.graphemes(true).collect()
    };
    for g in graphemes {
        let gw = grapheme_width(g);
        if w + gw > max {
            break;
        }
        kept.push(g);
        w += gw;
    }
    if end {
        kept.reverse();
    }
    kept.concat()
}

/// `value` cut to `max` columns in the middle (`abcd…ghij`), keeping one
/// more column at the start than at the end; just the start and `…` when
/// no column is left for the end.
pub fn truncate_middle(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(value) <= max {
        return value.to_string();
    }
    let start = (max - 1).div_ceil(2);
    let end = (max - 1) / 2;
    let mut out = take(value, start, false);
    out.push('…');
    if end > 0 {
        out.push_str(&take(value, end, true));
    }
    out
}

/// A path's root and separator, as OpenCode reads them: a drive (`C:\`),
/// a UNC share (`\\server\share\`), or `/`.
struct Root<'a> {
    root: String,
    separator: &'a str,
    /// Bytes of `value` the root covers.
    len: usize,
    windows: bool,
}

fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

fn root(value: &str) -> Root<'_> {
    let b = value.as_bytes();
    // `^([A-Za-z]:)([\\/])`
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && is_sep(b[2] as char) {
        let separator = &value[2..3];
        return Root {
            root: format!("{}{separator}", &value[..2]),
            separator,
            len: 3,
            windows: true,
        };
    }
    // `^(\\\\|//)([^\\/]+)[\\/]([^\\/]+)(?:[\\/]|$)`
    if value.starts_with("\\\\") || value.starts_with("//") {
        let rest = &value[2..];
        if let Some(i) = rest.find(is_sep).filter(|&i| i > 0) {
            let share = &rest[i + 1..];
            let j = share.find(is_sep).unwrap_or(share.len());
            if j > 0 {
                let separator = if value.starts_with("//") { "/" } else { "\\" };
                let len = 2 + i + 1 + j + usize::from(j < share.len());
                return Root {
                    root: format!(
                        "{}{}{separator}{}{separator}",
                        &value[..2],
                        &rest[..i],
                        &share[..j]
                    ),
                    separator,
                    len,
                    windows: true,
                };
            }
        }
    }
    let windows = !value.contains('/') && value.contains('\\');
    let slash = value.starts_with('/');
    Root {
        root: if slash { "/".into() } else { String::new() },
        separator: if windows { "\\" } else { "/" },
        len: usize::from(slash),
        windows,
    }
}

/// A file name cut to `max` columns, keeping its extension (`dialog-….tsx`).
fn truncate_basename(value: &str, max: usize) -> String {
    if width(value) <= max {
        return value.to_string();
    }
    if max <= 1 {
        return take("…", max, false);
    }
    let dot = value.rfind('.').filter(|&d| d > 0);
    let extension = dot.map_or("", |d| &value[d..]);
    let ext_w = width(extension);
    if ext_w >= max {
        return format!("…{}", take(extension, max - 1, true));
    }
    let stem = dot.map_or(value, |d| &value[..d]);
    format!("{}…{extension}", take(stem, max - ext_w - 1, false))
}

/// `value`, a path, cut to `max` columns as OpenCode's `truncateFilePath`:
/// the root is kept, then `…`, then as many of the nearest parent folders
/// as fit (the last one cut if at least two columns are left) and the file
/// name, shortened before its extension when even it does not fit.
pub fn truncate_file_path(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(value) <= max {
        return value.to_string();
    }
    let Root {
        root,
        separator,
        len,
        windows,
    } = root(value);
    let source = &value[len..];
    let segments: Vec<&str> = if windows {
        source.split(is_sep).filter(|s| !s.is_empty()).collect()
    } else {
        source.split(separator).filter(|s| !s.is_empty()).collect()
    };
    let basename = segments.last().copied().unwrap_or(value);
    if segments.len() < 2 {
        let root_w = width(&root);
        if root_w >= max {
            return take(&root, max, false);
        }
        return format!("{root}{}", truncate_basename(basename, max - root_w));
    }
    let prefix = format!("{root}…{separator}");
    let prefix_w = width(&prefix);
    if prefix_w >= max {
        return take(&prefix, max, false);
    }
    let compact = truncate_basename(basename, max - prefix_w);
    if compact != basename {
        return prefix + &compact;
    }
    let mut selected = vec![basename.to_string()];
    let sep_w = width(separator);
    let mut used = prefix_w + width(basename);
    for segment in segments[..segments.len() - 1].iter().rev() {
        let next = width(segment) + sep_w;
        if used + next > max {
            let available = max.saturating_sub(used + sep_w);
            if available > 1 {
                selected.insert(0, format!("{}…", take(segment, available - 1, false)));
            }
            break;
        }
        selected.insert(0, segment.to_string());
        used += next;
    }
    prefix + &selected.join(separator)
}

#[cfg(test)]
mod tests {
    //! OpenCode's `tui/test/ui/file-path.test.ts` and the middle-truncation
    //! case of `tui/test/util/locale.test.ts` (MIT), ported.
    use super::*;

    const PATH: &str = "packages/tui/src/ui/dialog-select.tsx";

    // file-path: "keeps the full path when it fits"
    #[test]
    fn oc_keeps_the_full_path_when_it_fits() {
        assert_eq!(truncate_file_path(PATH, 37), PATH);
    }

    // file-path: "adds nearest parent segments from right to left"
    #[test]
    fn oc_adds_nearest_parent_segments_from_right_to_left() {
        assert_eq!(truncate_file_path(PATH, 26), "…/src/ui/dialog-select.tsx");
        assert_eq!(truncate_file_path(PATH, 22), "…/ui/dialog-select.tsx");
        assert_eq!(truncate_file_path(PATH, 19), "…/dialog-select.tsx");
    }

    // file-path: "preserves the working directory and branch suffix"
    #[test]
    fn oc_preserves_the_working_directory_and_branch_suffix() {
        assert_eq!(
            truncate_file_path("~/code/experiments/category-theory:main", 30),
            "…/experi…/category-theory:main"
        );
    }

    // file-path: "uses remaining width for part of a long parent segment"
    #[test]
    fn oc_uses_remaining_width_for_part_of_a_long_parent_segment() {
        let path = "/private/var/folders/run-17f048ec-dbb2-4b36-860c-98637bb51a8d/files";
        assert_eq!(
            truncate_file_path(path, 40),
            "/…/run-17f048ec-dbb2-4b36-860c-98…/files"
        );
    }

    // file-path: "preserves the extension when the basename must shrink"
    #[test]
    fn oc_preserves_the_extension_when_the_basename_must_shrink() {
        assert_eq!(truncate_file_path(PATH, 16), "…/dialog-se….tsx");
        assert_eq!(truncate_file_path("dialog-select.tsx", 12), "dialog-….tsx");
    }

    // file-path: "preserves the input separator"
    #[test]
    fn oc_preserves_the_input_separator() {
        assert_eq!(
            truncate_file_path("packages\\tui\\src\\ui\\dialog-select.tsx", 22),
            "…\\ui\\dialog-select.tsx"
        );
    }

    // file-path: "does not treat a backslash in a POSIX filename as a separator"
    #[test]
    fn oc_does_not_treat_a_backslash_in_a_posix_filename_as_a_separator() {
        assert_eq!(
            truncate_file_path("dir/file\\name.ts", 14),
            "…/file\\name.ts"
        );
    }

    // file-path: "preserves absolute roots"
    #[test]
    fn oc_preserves_absolute_roots() {
        let t = truncate_file_path;
        assert_eq!(t("/file.ts", 7), "/fi….ts");
        assert_eq!(t("C:\\file.ts", 9), "C:\\fi….ts");
        assert_eq!(t("/usr/local/bin/file.ts", 14), "/…/bin/file.ts");
        assert_eq!(t("C:\\Users\\kit\\src\\file.ts", 16), "C:\\…\\src\\file.ts");
        assert_eq!(t("C:/Users/kit/src/file.ts", 16), "C:/…/src/file.ts");
        assert_eq!(t("C:\\Users\\kit/src/file.ts", 16), "C:\\…\\src\\file.ts");
        assert_eq!(
            t("\\\\server\\share\\src\\file.ts", 25),
            "\\\\server\\share\\…\\file.ts"
        );
    }

    // file-path: "measures terminal columns without splitting graphemes"
    #[test]
    fn oc_measures_terminal_columns_without_splitting_graphemes() {
        let t = truncate_file_path;
        assert_eq!(t("packages/组件/对话框.tsx", 12), "…/对话框.tsx");
        assert!(t("src/👩\u{200d}💻-notes.tsx", 12).contains("👩\u{200d}💻"));
        assert_eq!(t("中a.txt", 6), "….txt");
        assert_eq!(t("file.中a", 3), "…a");
    }

    // file-path: "never exceeds the requested width"
    #[test]
    fn oc_never_exceeds_the_requested_width() {
        for w in 0..=width(PATH) {
            assert!(width(&truncate_file_path(PATH, w)) <= w, "width {w}");
        }
    }

    // locale: "truncates text in the middle without overflowing small widths"
    #[test]
    fn oc_truncates_text_in_the_middle_without_overflowing_small_widths() {
        let t = truncate_middle;
        assert_eq!(t("abcdefghij", 0), "");
        assert_eq!(t("abcdefghij", 1), "…");
        assert_eq!(t("abcdefghij", 2), "a…");
        assert_eq!(t("abcdefghij", 3), "a…j");
        assert_eq!(t("abcdefghij", 4), "ab…j");
        assert_eq!(t("abcdefghij", 9), "abcd…ghij");
        assert_eq!(t("abcd", 4), "abcd");
        assert_eq!(t("abcd", 10), "abcd");
        assert_eq!(t("a", 1), "a");
    }
}
