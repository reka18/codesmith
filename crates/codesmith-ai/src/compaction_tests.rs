//! Provider compaction checkpoints ([`Part::Compaction`]): replayed by
//! OpenAI Responses, left out by the other protocols, and read from
//! Responses streams.

use crate::*;
use serde_json::json;

fn checkpointed() -> Request {
    Request {
        model: "gpt-5".into(),
        system: "sys".into(),
        messages: vec![
            Message::user("First real user request"),
            Message {
                role: Role::Assistant,
                parts: vec![Part::Compaction(Compaction {
                    id: Some("cmp_1".into()),
                    encrypted: "encrypted_1".into(),
                })],
            },
            Message::user("Second real user request"),
        ],
        ..Default::default()
    }
}

#[test]
fn responses_replays_a_checkpoint_as_its_compaction_item() {
    let b = responses::body(&checkpointed());
    let input = b["input"].as_array().unwrap();
    assert!(input.contains(
        &json!({"type": "compaction", "id": "cmp_1", "encrypted_content": "encrypted_1"})
    ));
}

#[test]
fn other_protocols_leave_a_checkpoint_out() {
    let r = checkpointed();
    for body in [
        openai::body(&r),
        anthropic::body(&r),
        gemini::body(&r),
        bedrock::body(&r).unwrap(),
    ] {
        let text = body.to_string();
        assert!(!text.contains("encrypted_1"), "{text}");
        assert!(text.contains("Second real user request"));
    }
}

/// session-native.test.ts: "a native window round-trips tools, opaque
/// checkpoints and binary media through JSON" (adapted: a window of
/// messages with a tool exchange, a checkpoint and an image).
#[test]
fn oc_native_window_round_trips_tools_checkpoints_and_media_through_json() {
    let window = vec![
        Message {
            role: Role::User,
            parts: vec![
                Part::text("look"),
                Part::Media(Media::base64("image/png", "AJ+Slg==")),
            ],
        },
        Message {
            role: Role::Assistant,
            parts: vec![
                Part::tool_call("c1", "read", json!({"path": "a"})),
                Part::Compaction(Compaction {
                    id: None,
                    encrypted: "opaque".into(),
                }),
            ],
        },
        Message {
            role: Role::User,
            parts: vec![Part::ToolResult {
                id: "c1".into(),
                name: "read".into(),
                content: "x".into(),
                is_error: false,
                media: vec![],
            }],
        },
    ];
    let text = serde_json::to_string(&window).unwrap();
    let back: Vec<Message> = serde_json::from_str(&text).unwrap();
    assert_eq!(back, window);
}

#[test]
fn a_responses_stream_returns_the_checkpoint() {
    let mut s = responses::State::default();
    let all = responses::handle(
        &mut s,
        "response.completed",
        &json!({"response": {"output": [{"type": "compaction", "id": "cmp_1", "encrypted_content": "e1"}],
            "usage": {"input_tokens": 20, "output_tokens": 4}}}),
    )
    .unwrap();
    assert!(all.contains(&Event::Compaction(Compaction {
        id: Some("cmp_1".into()),
        encrypted: "e1".into()
    })));
    let mut c = Collector::default();
    for e in &all {
        c.push(e);
    }
    assert!(c.finish().iter().any(|p| matches!(p, Part::Compaction(_))));
}
