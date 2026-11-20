//! TUI tests ported from OpenCode's `packages/tui/test` (MIT) where the
//! behaviour maps to CodeSmith's prompt, pickers and attachments. Each test
//! names the OpenCode file and test it comes from; `(adapted)` marks one
//! rephrased for CodeSmith's editor (byte offsets instead of OpenTUI cells,
//! history from this run's prompts instead of a JSONL file, pasted paths
//! instead of data URIs). See `crates/codesmith/PARITY.md`.

use super::app::{Action, App, Modal, Pick, PickItem, Picker};
use super::attach;
use super::complete::{self, Kind};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use std::path::{Path, PathBuf};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent {
        modifiers: KeyModifiers::CONTROL,
        ..key(KeyCode::Char(c))
    }
}

fn typ(a: &mut App, s: &str) {
    for c in s.chars() {
        a.on_key(key(KeyCode::Char(c)));
    }
}

fn send(a: &mut App, s: &str) -> Option<Action> {
    a.input.set(s);
    a.on_key(key(KeyCode::Enter))
}

// mini/prompt.shared.test.ts: "filters blank prompts and dedupes consecutive
// history" and "push ignores blanks and dedupes only the latest item";
// prompt/history.test.ts: "dedupes only identical consecutive entries".
#[test]
fn oc_prompt_history_skips_blanks_and_dedupes_consecutive_prompts() {
    let mut a = App::new();
    for p in ["   ", "one", "one", "two", "one"] {
        send(&mut a, p);
    }
    assert_eq!(a.history, ["one", "two", "one"]);
    send(&mut a, "   ");
    send(&mut a, "one");
    assert_eq!(a.history, ["one", "two", "one"], "blank and repeated");
    send(&mut a, "two");
    assert_eq!(a.history, ["one", "two", "one", "two"]);
}

// mini/runtime.queue.test.ts: "ignores empty prompts".
#[test]
fn oc_empty_prompts_are_not_sent() {
    let mut a = App::new();
    assert_eq!(send(&mut a, ""), None);
    assert_eq!(send(&mut a, " \n\t "), None);
    assert!(a.history.is_empty());
}

// prompt/history-composer.test.tsx: "down past the newest entry restores the
// draft text and shell mode" (CodeSmith has no shell mode).
#[test]
fn oc_down_past_the_newest_entry_restores_the_draft() {
    let mut a = App::new();
    send(&mut a, "first");
    send(&mut a, "second");
    typ(&mut a, "draft");
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.input.text(), "second");
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.input.text(), "first");
    a.on_key(key(KeyCode::Down));
    assert_eq!(a.input.text(), "second");
    a.on_key(key(KeyCode::Down));
    assert_eq!(a.input.text(), "draft");
    assert_eq!(a.input.cursor(), "draft".len());
}

// prompt/history-composer.test.tsx: "clearing a recalled entry drops the
// draft so down stays empty".
#[test]
fn oc_clearing_a_recalled_entry_drops_the_draft() {
    let mut a = App::new();
    send(&mut a, "old");
    typ(&mut a, "draft");
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.input.text(), "old");
    a.on_key(ctrl('u'));
    a.on_key(key(KeyCode::Down));
    assert_eq!(a.input.text(), "");
}

// prompt/history-provider.test.tsx: "down rejects at the newest history item
// with an empty prompt".
#[test]
fn oc_down_at_the_newest_entry_with_an_empty_prompt_does_nothing() {
    let mut a = App::new();
    send(&mut a, "one");
    a.on_key(key(KeyCode::Down));
    assert_eq!(a.input.text(), "");
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.input.text(), "one");
}

// prompt-offset.test.tsx: "history navigation moves the cursor to the end
// of wide text" (adapted: the entries are this run's prompts).
#[test]
fn oc_history_navigation_moves_the_cursor_to_the_end_of_wide_text() {
    let wide = "中\t👨‍👩‍👧நி";
    let mut a = App::new();
    send(&mut a, wide);
    send(&mut a, "末");
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.input.text(), "末");
    a.on_key(key(KeyCode::Up));
    assert_eq!(a.input.text(), wide);
    assert_eq!(a.input.cursor(), wide.len());
    a.on_key(key(KeyCode::End));
    a.on_key(key(KeyCode::Down));
    typ(&mut a, "x");
    assert_eq!(a.input.text(), "末x");
}

// prompt-paste.test.tsx: "the prompt keeps pasted control characters and
// drops escape sequences".
#[test]
fn oc_paste_keeps_control_characters_and_drops_escape_sequences() {
    let mut a = App::new();
    typ(&mut a, "x");
    a.on_paste("a\u{c}b\u{1b}[31mc\u{0}d");
    assert_eq!(a.input.text(), "xa\u{c}bc\u{0}d");
    let mut a = App::new();
    a.on_paste("t\u{1b}]0;title\u{7}i\u{1b}]8;;http://x\u{1b}\\tle\u{1b}7");
    assert_eq!(a.input.text(), "title", "OSC and two-character sequences");
}

fn picker(labels: &[&str]) -> App {
    let mut a = App::new();
    let items = labels
        .iter()
        .map(|l| PickItem {
            id: l.to_string(),
            label: l.to_string(),
            detail: String::new(),
            current: false,
        })
        .collect();
    a.modal = Some(Modal::Picker(Picker::new(Pick::Model, "Pick", items)));
    a
}

fn filter(a: &App) -> Option<(String, usize)> {
    match &a.modal {
        Some(Modal::Picker(p)) => Some((p.filter.clone(), p.shown.len())),
        _ => None,
    }
}

// cli/tui/dialog-select.test.tsx: "ctrl+c clears a dialog filter before
// closing the dialog".
#[test]
fn oc_ctrl_c_clears_a_picker_filter_before_closing_it() {
    let mut a = picker(&["Alpha"]);
    typ(&mut a, "alpha");
    assert_eq!(filter(&a), Some(("alpha".into(), 1)));
    assert_eq!(a.on_key(ctrl('c')), None);
    assert_eq!(filter(&a), Some((String::new(), 1)), "still open");
    a.on_key(ctrl('c'));
    assert!(a.modal.is_none());
    assert!(!a.quit);
}

// cli/tui/dialog-select.test.tsx: "shows no-match and still closes after a
// flat filter goes empty".
#[test]
fn oc_a_picker_with_no_matches_still_closes() {
    let mut a = picker(&["Alpha", "Beta"]);
    typ(&mut a, "zzz");
    assert_eq!(filter(&a), Some(("zzz".into(), 0)));
    assert_eq!(a.on_key(key(KeyCode::Enter)), None, "nothing to choose");
    assert!(a.modal.is_some());
    a.on_key(key(KeyCode::Esc));
    assert!(a.modal.is_none());
}

// app-lifecycle.test.tsx: "ctrl+c dismisses autocomplete and shell mode
// before exiting" (CodeSmith has no shell mode).
#[test]
fn oc_ctrl_c_dismisses_autocomplete_before_exiting() {
    let mut a = App::new();
    typ(&mut a, "/he");
    assert!(a.popup.is_some());
    assert_eq!(a.on_key(ctrl('c')), None);
    assert!(a.popup.is_none());
    assert!(!a.quit);
}

// prompt/display.test.ts: "ignores triggers inside a token or before
// whitespace".
#[test]
fn oc_mention_and_slash_triggers_ignore_tokens() {
    for text in [
        "中文@",
        "🙂@",
        "foo@bar.com",
        "中文 @src file",
        "\0@x",
        "Review /api design",
        "Review /tmp/file.ts",
        "https://opencode.ai/docs",
        "\0/x",
    ] {
        assert_eq!(complete::trigger(text, text.len()), None, "{text:?}");
    }
}

// prompt/display.test.ts: "places the @ trigger where the editor shows it"
// (adapted: byte offsets; the `/` case is a gap, CodeSmith completes
// commands only at the start of the prompt).
#[test]
fn oc_the_mention_trigger_is_found_after_any_separator() {
    let samples = [
        "\u{c}",
        "\0",
        "\u{1b}",
        "\u{7f}",
        "\u{85}",
        "\t",
        "中文",
        "e\u{301}",
        "👨‍👩‍👧‍👦",
        "\n",
        "\r\n",
    ];
    let mut texts = vec!["\t@file".to_string()];
    for s in samples {
        texts.push(format!("{s} @x {s}"));
        texts.push(format!("{s}\n@x\r\n{s}"));
    }
    for text in texts {
        let at = text.find('@').unwrap();
        let cursor = if text == "\t@file" {
            text.len()
        } else {
            at + 2
        };
        let found = complete::trigger(&text, cursor);
        assert!(
            matches!(found, Some((Kind::File, start, _)) if start == at),
            "{text:?}: {found:?}"
        );
    }
}

fn pending(labels: &[&str]) -> Vec<attach::Pending> {
    labels
        .iter()
        .map(|l| attach::Pending {
            label: l.to_string(),
            path: PathBuf::from("x"),
        })
        .collect()
}

// prompt/attachment.test.ts: "numbers PDFs independently from images".
#[test]
fn oc_pdfs_are_numbered_independently_from_images() {
    let p = pending(&["[Image 1]"]);
    assert_eq!(attach::label(&p, Path::new("doc.pdf")), "[PDF 1]");
    assert_eq!(attach::label(&p, Path::new("shot.PNG")), "[Image 2]");
}

// prompt/attachment.test.ts: "does not reuse numbers after an earlier
// attachment is removed" (adapted: placeholders deleted from the prompt).
#[test]
fn oc_attachment_numbers_are_not_reused_after_one_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    for f in ["a.png", "b.png", "c.png"] {
        std::fs::write(dir.path().join(f), [1, 2, 3]).unwrap();
    }
    let mut a = App::new();
    a.cwd = dir.path().to_path_buf();
    a.on_paste("a.png");
    a.on_paste("b.png");
    assert_eq!(a.input.text(), "[Image 1] [Image 2] ");
    a.input.set("[Image 2] ");
    a.on_paste("c.png");
    assert_eq!(a.input.text(), "[Image 2] [Image 3] ");
}

fn files(dir: &Path, names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(|n| dir.join(n)).collect()
}

// prompt/local-attachment.test.ts: "parses multi-file drops from POSIX,
// URI-list, and Windows terminals" (adapted: files that exist; the Windows
// cases do not apply).
#[test]
fn oc_multi_file_drops_from_posix_and_uri_list_terminals() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for f in [
        "one image.png",
        "two image.webp",
        "two.webp",
        "one.png",
        "one\\image.png",
        "O'Brien.png",
    ] {
        std::fs::write(d.join(f), [1]).unwrap();
    }
    let ds = d.display();
    let url = |name: &str| format!("file://{}/{}", ds, name.replace(' ', "%20"));
    let cases: Vec<(String, Vec<&str>)> = vec![
        (
            format!("'{ds}/one image.png' {ds}/two\\ image.webp"),
            vec!["one image.png", "two image.webp"],
        ),
        (
            format!("{}\r\n{}", url("one image.png"), url("two.webp")),
            vec!["one image.png", "two.webp"],
        ),
        (
            format!("# dropped files\n{}\n{}", url("one.png"), url("two.webp")),
            vec!["one.png", "two.webp"],
        ),
        (
            format!("{ds}/one\\\\image.png {ds}/two.webp"),
            vec!["one\\image.png", "two.webp"],
        ),
        (
            format!("\"{ds}/O'Brien.png\" {ds}/two.webp"),
            vec!["O'Brien.png", "two.webp"],
        ),
    ];
    for (text, want) in cases {
        assert_eq!(
            attach::pasted_files(&text, d),
            Some(files(d, &want)),
            "{text:?}"
        );
    }
}

// prompt/local-attachment.test.ts: "rejects unbounded and malformed
// multi-file drops".
#[test]
fn oc_unbounded_and_malformed_drops_are_text() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    for i in 0..33 {
        std::fs::write(d.join(format!("{i}.png")), [1]).unwrap();
    }
    assert_eq!(attach::pasted_files("'0.png 1.png", d), None);
    let many: Vec<String> = (0..33).map(|i| format!("{i}.png")).collect();
    assert_eq!(attach::pasted_files(&many.join(" "), d), None);
    assert!(attach::pasted_files(&many[..32].join(" "), d).is_some());
}

// prompt/local-attachment.test.ts: "resolves a single image path before
// splitting spaces".
#[test]
fn oc_a_single_image_path_is_resolved_before_splitting_spaces() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("one image.png");
    std::fs::write(&file, [1, 2, 3]).unwrap();
    let f = file.display().to_string();
    for input in [
        f.clone(),
        format!("'{f}'"),
        format!("file://{}", f.replace(' ', "%20")),
    ] {
        assert_eq!(
            attach::pasted_files(&input, dir.path()),
            Some(vec![file.clone()]),
            "{input:?}"
        );
    }
}

// prompt/local-attachment.test.ts: "resolves quoted paths and URI lists as
// ordered attachments".
#[test]
fn oc_quoted_paths_and_uri_lists_are_ordered_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("one image.png");
    let pdf = dir.path().join("two file.pdf");
    std::fs::write(&image, [1, 2, 3]).unwrap();
    std::fs::write(&pdf, [4, 5, 6]).unwrap();
    let url = |p: &Path| format!("file://{}", p.display().to_string().replace(' ', "%20"));
    for input in [
        format!("'{}' \"{}\"", image.display(), pdf.display()),
        format!("# dropped files\r\n{}\r\n{}", url(&image), url(&pdf)),
    ] {
        assert_eq!(
            attach::pasted_files(&input, dir.path()),
            Some(vec![image.clone(), pdf.clone()]),
            "{input:?}"
        );
    }
}

// prompt/local-attachment.test.ts: "falls back to plain text for unsupported
// or incomplete drops".
#[test]
fn oc_unsupported_or_incomplete_drops_are_plain_text() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("image.png");
    let text = dir.path().join("notes.txt");
    std::fs::write(&image, [1]).unwrap();
    std::fs::write(&text, "notes").unwrap();
    for input in [
        String::new(),
        "plain\r\ntext".into(),
        "https://example.com/image.png".into(),
        text.display().to_string(),
        format!("{} {}", image.display(), text.display()),
        format!(
            "{} {}",
            image.display(),
            dir.path().join("missing.png").display()
        ),
    ] {
        assert_eq!(attach::pasted_files(&input, dir.path()), None, "{input:?}");
    }
}

/// The built-in command a prompt runs, as the UI dispatches it.
fn builtin_of(a: &mut App, s: &str) -> Option<&'static str> {
    match send(a, s)? {
        Action::Slash { name, args } => super::builtin(&name, &args),
        _ => None,
    }
}

// mini/prompt.shared.test.ts: "recognizes exit commands".
#[test]
fn oc_exit_commands_ignore_case_and_space_but_not_arguments() {
    let mut a = App::new();
    assert_eq!(builtin_of(&mut a, "/exit"), Some("exit"));
    assert_eq!(builtin_of(&mut a, " /Quit "), Some("quit"));
    assert_eq!(builtin_of(&mut a, "/quit now"), None);
    assert_eq!(builtin_of(&mut a, "/EXIT"), Some("exit"));
    assert_eq!(builtin_of(&mut a, "/q"), Some("q"));
    assert_eq!(builtin_of(&mut a, ":q"), Some("quit"));
}

// mini/prompt.shared.test.ts: "recognizes the new-session command".
#[test]
fn oc_the_new_session_command_ignores_case_and_space_but_not_arguments() {
    let mut a = App::new();
    assert_eq!(builtin_of(&mut a, "/new"), Some("new"));
    assert_eq!(builtin_of(&mut a, " /NEW "), Some("new"));
    assert_eq!(builtin_of(&mut a, "/new now"), None);
}

// mini/prompt.shared.test.ts: "recognizes only the compact command".
#[test]
fn oc_only_the_compact_command_compacts() {
    let mut a = App::new();
    assert_eq!(builtin_of(&mut a, "/compact"), Some("compact"));
    assert_eq!(builtin_of(&mut a, " /COMPACT "), Some("compact"));
    assert_eq!(builtin_of(&mut a, "/summarize"), None);
    assert_eq!(builtin_of(&mut a, "/compact now"), None);
}

// The same rules for every built-in (adapted): names ignore case; those
// that take an argument keep it, the others are only themselves without one.
#[test]
fn oc_builtin_commands_ignore_case_and_take_arguments_only_where_they_can() {
    for n in [
        "help", "clear", "thinking", "sessions", "undo", "redo", "exit", "quit", "q", "new",
        "compact",
    ] {
        assert_eq!(super::builtin(&n.to_uppercase(), ""), Some(n));
        assert_eq!(super::builtin(n, "x"), None, "/{n} x");
    }
    for n in ["agent", "model", "variant"] {
        assert_eq!(super::builtin(&n.to_uppercase(), ""), Some(n));
        assert_eq!(super::builtin(n, "x"), Some(n), "/{n} x");
    }
    assert_eq!(
        super::builtin("review", ""),
        None,
        "commands are not built-ins"
    );
}
