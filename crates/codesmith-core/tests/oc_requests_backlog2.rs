//! Ports of model request, compaction and subagent cases in PARITY.md's
//! porting backlog (`packages/core/test/session-model-request.test.ts`,
//! `session-compaction.test.ts`, `tool-subagent.test.ts`, MIT).

mod scripted;

use codesmith_ai::{Media, Message, Part, Role};
use codesmith_core::{compaction, media};
use scripted::*;
use serde_json::json;

fn png(name: &str) -> Media {
    Media::base64("image/png", "aGVsbG8=").named(name)
}

fn tool_result(id: &str, content: &str, media: Vec<Media>) -> Part {
    Part::ToolResult {
        id: id.into(),
        name: "read".into(),
        content: content.into(),
        is_error: false,
        media,
    }
}

fn text_only() -> Vec<String> {
    vec!["text".into()]
}

/// "replaces unsupported user media with a visible error"
#[test]
fn oc_model_request_replaces_unsupported_user_media_with_a_visible_error() {
    let messages = vec![Message {
        role: Role::User,
        parts: vec![
            Part::text("Describe these files"),
            Part::Media(png("logo.png")),
            Part::Media(Media::base64("application/pdf", "JVBERg==").named("document.pdf")),
        ],
    }];
    let out = media::sendable(&messages, Some(&text_only()));
    assert_eq!(
        out[0].parts,
        vec![
            Part::text("Describe these files"),
            Part::text("ERROR: Cannot read \"logo.png\" (this model does not support image input). Inform the user."),
            Part::text("ERROR: Cannot read \"document.pdf\" (this model does not support pdf input). Inform the user."),
        ]
    );
}

/// "replaces unsupported media nested in tool results"
#[test]
fn oc_model_request_replaces_unsupported_media_nested_in_tool_results() {
    let messages = vec![Message {
        role: Role::User,
        parts: vec![tool_result(
            "call_1",
            "Image read successfully",
            vec![png("logo.png")],
        )],
    }];
    let out = media::sendable(&messages, Some(&text_only()));
    assert_eq!(
        out[0].parts,
        vec![tool_result(
            "call_1",
            "Image read successfully\nERROR: Cannot read \"logo.png\" (this model does not support image input). Inform the user.",
            vec![],
        )]
    );
}

/// "preserves supported media" and "returns the same messages when nothing
/// is unsupported"
#[test]
fn oc_model_request_preserves_supported_media() {
    let input = vec!["text".to_string(), "image".to_string()];
    let messages = vec![
        Message {
            role: Role::User,
            parts: vec![Part::text("hi"), Part::Media(png("a.png"))],
        },
        Message {
            role: Role::Assistant,
            parts: vec![Part::text("hello")],
        },
        Message {
            role: Role::User,
            parts: vec![tool_result("call_1", "", vec![png("logo.png")])],
        },
    ];
    assert_eq!(media::sendable(&messages, Some(&input)), messages);
}

/// "rebuilds only messages with unsupported media"
#[test]
fn oc_model_request_rebuilds_only_messages_with_unsupported_media() {
    let plain = Message::user("plain");
    let supported = Message {
        role: Role::User,
        parts: vec![tool_result("call_1", "no files", vec![])],
    };
    let untouched = tool_result("call_3", "done", vec![]);
    let unsupported = Message {
        role: Role::User,
        parts: vec![
            tool_result(
                "call_2",
                "Read",
                vec![Media::base64("application/pdf", "JVBERg==")],
            ),
            untouched.clone(),
        ],
    };
    let out = media::sendable(
        &[plain.clone(), supported.clone(), unsupported.clone()],
        Some(&text_only()),
    );
    assert_eq!(out[0], plain);
    assert_eq!(out[1], supported);
    assert_ne!(out[2], unsupported);
    assert_eq!(out[2].parts[1], untouched);
}

/// "compaction describes tool media without embedding base64"
#[test]
fn oc_compaction_describes_tool_media_without_embedding_base64() {
    let base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB";
    let messages = vec![Message {
        role: Role::User,
        parts: vec![tool_result(
            "call_1",
            "Image read successfully",
            vec![Media::base64("image/png", base64).named("pixel.png")],
        )],
    }];
    let prepared = compaction::for_summary(&messages);
    let text = format!("{prepared:?}");
    assert!(
        text.contains("Image read successfully\\n[image: pixel.png]"),
        "{text}"
    );
    assert!(!text.contains(base64));
}

/// "runs the child on an explicitly requested model"
#[tokio::test(flavor = "multi_thread")]
async fn oc_subagent_runs_the_child_on_an_explicitly_requested_model() {
    let mut t = start(Setup {
        extra: json!({"agents": {"reviewer": {"description": "Reviews", "mode": "subagent"}}}),
        models: json!({"m": {}, "override": {}}),
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "call-override",
            "subagent",
            json!({"agent": "reviewer", "description": "review", "prompt": "review this", "model": "fake/override"}),
        ),
        text("Looks fine."),
        text("Parent done"),
    ]);
    t.prompt("Delegate").await.unwrap();
    let reqs = t.requests();
    assert_eq!(reqs[0]["model"], "m");
    assert_eq!(reqs[1]["model"], "override");
    assert_eq!(reqs[2]["model"], "m");
}
