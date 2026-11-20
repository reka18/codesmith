//! OpenCode's tests of the interactive UI's smaller features, ported:
//! reasoning titles (`tui/test/cli/tui/thinking`), and the groups that
//! follow it in `PARITY.md`. Each `oc_*` test names the OpenCode test it
//! ports; "(adapted)" ones say what differs.

use super::group::reasoning_summary;

// thinking.test.ts: "extracts a leading summary title and leaves markdown body"
#[test]
fn oc_extracts_a_leading_summary_title_and_leaves_the_markdown_body() {
    assert_eq!(
        reasoning_summary("**Continuing Quality Review**\n\nDetails.\n\n**Next section**\n\nMore."),
        (
            Some("Continuing Quality Review"),
            "Details.\n\n**Next section**\n\nMore."
        )
    );
}

// thinking.test.ts: "extracts a completed title before its streamed body arrives"
#[test]
fn oc_extracts_a_completed_title_before_its_streamed_body_arrives() {
    assert_eq!(
        reasoning_summary("**Continuing Quality Review**"),
        (Some("Continuing Quality Review"), "")
    );
}

// thinking.test.ts: "preserves markdown-significant indentation in the extracted body"
#[test]
fn oc_preserves_markdown_significant_indentation_in_the_extracted_body() {
    assert_eq!(
        reasoning_summary("**Continuing Quality Review**\n\n    const value = true\n"),
        (Some("Continuing Quality Review"), "    const value = true")
    );
}

// thinking.test.ts: "does not consume ordinary leading bold content"
#[test]
fn oc_does_not_consume_ordinary_leading_bold_content() {
    assert_eq!(
        reasoning_summary("**Important:** keep this in the body."),
        (None, "**Important:** keep this in the body.")
    );
}

// thinking.test.ts: "leaves content without a leading title in its body"
#[test]
fn oc_leaves_content_without_a_leading_title_in_its_body() {
    assert_eq!(reasoning_summary("Details only."), (None, "Details only."));
}

// The transcript links a mentioned session by its title (session-link.ts
// labels it; the current session and unknown ones stay plain IDs).
#[test]
fn mentioned_sessions_show_their_titles() {
    use super::app::{App, Entry};
    let mut a = App::new();
    a.header.session = "ses_19a4b2c3d4e000000".into();
    a.entries.push(Entry::Assistant(
        "Continued from ses_19a4b2c3d4e123abc; this is ses_19a4b2c3d4e000000.".into(),
    ));
    assert_eq!(
        a.unknown_mentions(),
        vec!["ses_19a4b2c3d4e123abc", "ses_19a4b2c3d4e000000"]
    );
    a.session_labels.insert(
        "ses_19a4b2c3d4e123abc".into(),
        Some("Fix\x1b the build".into()),
    );
    a.session_labels
        .insert("ses_19a4b2c3d4e000000".into(), Some("This one".into()));
    let text: String = super::view::transcript(&a, 120)
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        text.contains("@Fix\\x1b the build (ses_19a4b2c3d4e123abc)"),
        "{text}"
    );
    assert!(text.contains("this is ses_19a4b2c3d4e000000."), "{text}");
}
