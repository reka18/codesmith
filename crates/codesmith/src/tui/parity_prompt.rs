//! OpenCode's prompt tests for paste chips and undo (`tui/test/prompt-paste`,
//! `prompt/attachment`), and the prompt groups after them in `PARITY.md`.
//! Each `oc_*` test names the OpenCode test it ports.

use super::app::{Action, App};
use super::attach;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn with(c: char, m: KeyModifiers) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), m)
}

fn frame(a: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| super::view::draw(f, a)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

// ----- tui/test/prompt-paste --------------------------------------------------

// prompt-paste.test.tsx: "undo submits the restored %s paste chip" (2 cases)
// (adapted) CodeSmith sends the prompt trimmed, so the chip's trailing
// space is not sent.
#[test]
fn oc_undo_submits_the_restored_paste_chip() {
    for kind in ["whole", "fragment"] {
        let mut a = App::new();
        let pasted = "one\ntwo\nthree\nfour";
        a.on_paste(pasted);
        assert_eq!(a.input.text(), "[Pasted ~4 lines] ");
        if kind == "fragment" {
            a.on_key(key(KeyCode::Backspace));
            a.on_key(with('w', KeyModifiers::CONTROL));
        }
        a.on_key(with('u', KeyModifiers::CONTROL));
        assert_eq!(a.input.text(), "");
        a.on_key(with('z', KeyModifiers::SUPER));
        let want = if kind == "whole" {
            "[Pasted ~4 lines] "
        } else {
            "[Pasted ~4 "
        };
        assert_eq!(a.input.text(), want, "{kind}");
        let sent = a.on_key(key(KeyCode::Enter));
        let want = if kind == "whole" {
            pasted
        } else {
            "[Pasted ~4"
        };
        assert_eq!(sent, Some(Action::Submit(want.into())), "{kind}");
    }
}

// (not in OpenCode's test) Pasting the same text right after its chip
// expands it; `tui.prompt.paste: "full"` pastes in whole.
#[test]
fn a_repeated_paste_expands_its_chip_and_full_pastes_whole() {
    let mut a = App::new();
    a.on_paste("a\nb\nc");
    a.on_paste("a\nb\nc");
    assert_eq!(a.input.text(), "a\nb\nc ");
    let mut a = App::new();
    a.paste_full = true;
    a.on_paste("a\nb\nc");
    assert_eq!(a.input.text(), "a\nb\nc");
    let mut a = App::new();
    a.on_paste(&"x".repeat(151));
    assert_eq!(a.input.text(), "[Pasted ~1 lines] ");
}

fn images(names: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for n in names {
        std::fs::write(dir.path().join(n), [0x89, 0x50, 0x4e, 0x47]).unwrap();
    }
    dir
}

// prompt-paste.test.tsx: "a pasted image that reuses a wide label gets a
// chip as wide as the label" (adapted: OpenTUI's cell widths have no
// counterpart; the same image pasted again reuses its label, and goes
// once).
#[test]
fn oc_a_pasted_image_that_reuses_a_label_keeps_it() {
    let dir = images(&["a.png"]);
    let mut a = App::new();
    a.cwd = dir.path().to_path_buf();
    a.on_paste("a.png");
    a.on_paste("a.png");
    assert_eq!(a.input.text(), "[Image 1] [Image 1] ");
    assert_eq!(a.attachments.len(), 1);
}

// prompt-paste.test.tsx: "%s grows the prompt in the first frame that shows it" (2 cases)
// (adapted) CodeSmith has no server route that appends to the prompt (an
// IDE's `tui.prompt.append`); a prompt brought back after a fork stands in
// for it.
#[test]
fn oc_text_grows_the_prompt_in_the_first_frame_that_shows_it() {
    for (kind, lines) in [
        ("a plugin append", "ALPHA\nBRAVO\nCHARLIE\nDELTA"),
        ("an inline paste", "ALPHA\nBRAVO"),
    ] {
        let mut a = App::new();
        if kind == "an inline paste" {
            a.on_paste(lines);
        } else {
            a.restore_prompt(lines);
        }
        let f = frame(&a, 60, 20);
        for l in lines.split('\n') {
            assert!(f.contains(l), "{kind}: {f}");
        }
        assert_eq!(a.input.cursor(), a.input.text().len());
    }
}

// prompt-paste.test.tsx: "undo submits the restored %s chip with its
// attachment" (files; the agent and skill cases wait for @agent mentions).
#[test]
fn oc_undo_submits_the_restored_file_chip_with_its_attachment() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.ts"), "x").unwrap();
    let mut a = App::new();
    a.cwd = dir.path().to_path_buf();
    a.input.set("see @a.ts ");
    a.on_key(with('u', KeyModifiers::CONTROL));
    assert_eq!(a.input.text(), "");
    a.on_key(with('z', KeyModifiers::SUPER));
    assert_eq!(a.input.text(), "see @a.ts ");
    let Some(Action::Submit(t)) = a.on_key(key(KeyCode::Enter)) else {
        panic!("not sent")
    };
    assert_eq!(attach::mentions(&t, dir.path()), ["a.ts"]);
}

// (not in OpenCode's test) Redo undoes the undo; typing a word is one step.
#[test]
fn redo_and_typing_runs() {
    let mut a = App::new();
    for c in "hello world".chars() {
        a.on_key(key(KeyCode::Char(c)));
    }
    a.on_key(with('z', KeyModifiers::SUPER));
    assert_eq!(a.input.text(), "hello ");
    a.on_key(with('z', KeyModifiers::SUPER | KeyModifiers::SHIFT));
    assert_eq!(a.input.text(), "hello world");
}

// ----- tui/test/prompt/attachment -------------------------------------------

// attachment.test.ts: "deduplicates identical inline images while
// preserving other attachments" (adapted: an image pasted twice is one
// attachment, beside the others).
#[test]
fn oc_deduplicates_identical_inline_images_while_preserving_other_attachments() {
    let dir = images(&["first.png", "second.png", "first.pdf"]);
    let mut a = App::new();
    a.cwd = dir.path().to_path_buf();
    for f in ["first.png", "first.pdf", "second.png", "first.png"] {
        a.on_paste(f);
    }
    assert_eq!(a.input.text(), "[Image 1] [PDF 1] [Image 2] [Image 1] ");
    let labels: Vec<_> = a.attachments.iter().map(|p| p.label.as_str()).collect();
    assert_eq!(labels, ["[Image 1]", "[PDF 1]", "[Image 2]"]);
}

// attachment.test.ts: "reuses labels for identical image data"
#[test]
fn oc_reuses_labels_for_identical_image_data() {
    let p = vec![attach::Pending {
        label: "[Image 1]".into(),
        path: "first.png".into(),
    }];
    assert_eq!(
        attach::label(&p, std::path::Path::new("first.png")),
        "[Image 1]"
    );
    assert_eq!(
        attach::label(&p, std::path::Path::new("second.png")),
        "[Image 2]"
    );
}

// attachment.test.ts: "does not reuse a label when attachment metadata
// differs" (adapted: the same bytes under another name).
#[test]
fn oc_does_not_reuse_a_label_when_attachment_metadata_differs() {
    let dir = images(&["one.png", "two.png"]);
    let mut a = App::new();
    a.cwd = dir.path().to_path_buf();
    a.on_paste("one.png");
    a.on_paste("two.png");
    assert_eq!(a.input.text(), "[Image 1] [Image 2] ");
}

// ----- tui/test/prompt/parse --------------------------------------------------

// parse.test.ts: "preserves file line-range parsing semantics"
#[test]
fn oc_preserves_file_line_range_parsing_semantics() {
    use attach::{parse_line_range as p, LineRange as R};
    let r = |start, end| Some(R { start, end });
    let cases = [
        ("src/app.ts#7", ("src/app.ts", r(7, None))),
        ("src/app.ts#12-20", ("src/app.ts", r(12, Some(20)))),
        ("src/app.ts#12-", ("src/app.ts", r(12, None))),
        ("src/app.ts#12-12", ("src/app.ts", r(12, None))),
        ("src/app.ts#bad", ("src/app.ts#bad", None)),
        ("src/app.ts#", ("src/app.ts#", None)),
        ("report#2026.md", ("report#2026.md", None)),
        ("#notes.md", ("#notes.md", None)),
        ("#3", ("#3", None)),
        ("report#2026.md#3", ("report#2026.md", r(3, None))),
        ("report#2026.md#3-10", ("report#2026.md", r(3, Some(10)))),
        ("#notes.md#3", ("#notes.md", r(3, None))),
        ("src/app.ts", ("src/app.ts", None)),
    ];
    for (input, want) in cases {
        assert_eq!(p(input), want, "{input}");
    }
    let strip = |s| p(s).0;
    assert_eq!(
        [
            strip("report#2026.md"),
            strip("#notes.md"),
            strip("report#2026.md#3"),
            strip("src/app.ts#12-20")
        ],
        [
            "report#2026.md",
            "#notes.md",
            "report#2026.md",
            "src/app.ts"
        ]
    );
}

// (not in OpenCode's test) An `@file#from-to` mention attaches those lines.
#[test]
fn a_line_range_mention_attaches_its_lines() {
    let dir = tempfile::tempdir().unwrap();
    let text: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("a.txt"), text).unwrap();
    let e = attach::expand("see @a.txt#12-14", dir.path(), &Default::default());
    assert_eq!(e.files, ["a.txt#12-14"]);
    assert!(
        e.text.contains("Read file a.txt#12-14, lines 12-14"),
        "{}",
        e.text
    );
    assert!(e.text.contains("12: line 12") && e.text.contains("14: line 14"));
    assert!(!e.text.contains("15: line 15") && !e.text.contains("11: line 11"));
    let e = attach::expand("see @a.txt#29", dir.path(), &Default::default());
    assert!(e.text.contains("lines 29-30"), "{}", e.text);
}
