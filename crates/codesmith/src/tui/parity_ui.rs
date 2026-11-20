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
