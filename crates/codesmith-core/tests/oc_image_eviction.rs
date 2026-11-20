//! Ports of OpenCode's old image eviction tests (MIT): the `boundImages`
//! cases of `session-model-request.test.ts`. Each test names the OpenCode
//! test it ports; see `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: JavaScript identity (`toBe`) is checked as equality; an image
//! dropped from a tool result leaves its note in the result's text.

use codesmith_ai::{Media, Message, Part, Role};
use codesmith_core::media::{bound_images, sendable, IMAGE_REMOVED};

const MIB: usize = 1024 * 1024;

fn image(size: usize, name: &str) -> Message {
    let mut m = Media::base64("image/png", "a".repeat(size));
    m.filename = Some(name.into());
    Message {
        role: Role::User,
        parts: vec![Part::Media(m)],
    }
}

fn removed(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m.parts.first(), Some(Part::Text(_))))
        .count()
}

// session-model-request.test.ts: "preserves images below the trigger"
#[test]
fn oc_images_below_the_trigger_are_kept() {
    let messages = vec![Message {
        role: Role::User,
        parts: vec![Part::Media(Media::base64("image/png", "aGVsbG8="))],
    }];
    assert_eq!(bound_images(&messages), messages);
    assert_eq!(sendable(&messages, None), messages);
}

// session-model-request.test.ts: "replaces oldest images until the retained payload reaches the target"
#[test]
fn oc_oldest_images_are_replaced_down_to_the_target() {
    let text = Message::user("plain");
    let third = image(9 * MIB, "third.png");
    let messages = vec![
        image(9 * MIB, "first.png"),
        text.clone(),
        image(9 * MIB, "second.png"),
        third.clone(),
    ];
    let result = bound_images(&messages);
    assert_eq!(result[0].role, Role::User);
    assert_eq!(result[0].parts, vec![Part::text(IMAGE_REMOVED)]);
    assert!(IMAGE_REMOVED.contains("image was removed"));
    assert_eq!(result[1], text);
    assert!(matches!(result[2].parts[0], Part::Text(_)));
    assert_eq!(result[3], third);
}

// session-model-request.test.ts: "keeps the replaced prefix stable as new images arrive, so the prompt cache survives"
#[test]
fn oc_the_replaced_prefix_stays_stable_as_images_arrive() {
    let img = |i: usize| image(3 * MIB, &format!("{i}.png"));
    let history: Vec<Message> = (0..9).map(img).collect();
    // 27 MiB passes the 25 MiB trigger: the oldest four go, leaving 15 MiB.
    assert_eq!(removed(&bound_images(&history)), 4);
    let with = |extra: std::ops::Range<usize>| {
        let mut h = history.clone();
        h.extend(extra.map(img));
        h
    };
    // The cut holds until the kept payload passes the trigger again.
    assert_eq!(removed(&bound_images(&with(9..10))), 4);
    assert_eq!(removed(&bound_images(&with(9..12))), 4);
    assert_eq!(removed(&bound_images(&with(9..13))), 8);
}

// session-model-request.test.ts: "replaces images nested in tool results"
#[test]
fn oc_images_in_tool_results_are_replaced() {
    let img = |name: &str| {
        let mut m = Media::base64("image/png", "a".repeat(13 * MIB));
        m.filename = Some(name.into());
        m
    };
    let messages = vec![Message {
        role: Role::User,
        parts: vec![Part::ToolResult {
            id: "call_1".into(),
            name: "read".into(),
            content: String::new(),
            is_error: false,
            media: vec![img("first.png"), img("second.png")],
        }],
    }];
    let result = bound_images(&messages);
    let Part::ToolResult { content, media, .. } = &result[0].parts[0] else {
        panic!("not a tool result");
    };
    assert_eq!(content, IMAGE_REMOVED);
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].filename.as_deref(), Some("second.png"));
}
