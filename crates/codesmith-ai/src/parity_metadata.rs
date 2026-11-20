//! Parity with OpenCode's tests for provider state the model declares or
//! signs: the "Configured Chat reasoning fields", "Signed text and signature
//! overrides" and "Tool namespaces" groups of `PARITY.md` (MIT, copyright
//! the OpenCode authors). Each case is named after the OpenCode test it
//! ports.

use super::*;
use crate::{collect, Compatibility, Provider, Reasoning, ReasoningMeta};

fn chat_provider(compat: Compatibility) -> Provider {
    Provider {
        base_url: COMPATIBLE.into(),
        compat,
        ..Default::default()
    }
}

fn chat_for(p: &Provider, r: &Request) -> Value {
    openai::body_for(r, &openai::Compat::for_provider(p, &r.model))
}

fn field(f: &str) -> Compatibility {
    Compatibility {
        reasoning_field: Some(f.into()),
        ..Default::default()
    }
}

fn chat_reasoning(text: &str, field: Option<&str>, details: Option<Value>) -> Part {
    let mut m = ReasoningMeta::of(Protocol::OpenaiChat);
    m.field = field.map(str::to_string);
    m.details = details.map(|d| d.as_array().unwrap().clone());
    Part::Reasoning(Reasoning::new(text, m))
}

fn signed_details() -> Value {
    json!([{"type": "reasoning.text", "text": "thinking", "signature": "signed", "format": "anthropic-claude-v1", "index": 0}])
}

#[test]
fn configured_chat_reasoning_fields() {
    run(&[
        ("openai-chat :: writes reasoning to a configured custom field on every assistant message", || {
            let p = chat_provider(field("vendor_reasoning"));
            let r = req(
                "gpt-4o-mini",
                vec![
                    asst(vec![chat_reasoning("thinking", Some("reasoning"), None), text("Hello")]),
                    asst(vec![text("Done")]),
                ],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([
                    {"role": "assistant", "content": "Hello", "vendor_reasoning": "thinking"},
                    {"role": "assistant", "content": "Done", "vendor_reasoning": ""},
                ])
            );
        }),
        ("openai-chat :: preserves observed reasoning fields when reasoning is required", || {
            let p = chat_provider(Compatibility {
                require_reasoning: Some(true),
                ..Default::default()
            });
            let r = req(
                "gpt-4o-mini",
                vec![
                    asst(vec![chat_reasoning("thinking", Some("reasoning_text"), None), text("Hello")]),
                    asst(vec![text("Done")]),
                ],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([
                    {"role": "assistant", "content": "Hello", "reasoning_text": "thinking"},
                    {"role": "assistant", "content": "Done", "reasoning_content": ""},
                ])
            );
        }),
        ("openai-chat :: omits empty configured reasoning fields when reasoning is explicitly optional", || {
            let p = chat_provider(Compatibility {
                reasoning_field: Some("reasoning_text".into()),
                require_reasoning: Some(false),
                ..Default::default()
            });
            let r = req(
                "gpt-4o-mini",
                vec![asst(vec![thinking("thinking"), text("Hello")]), asst(vec![text("Done")])],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([
                    {"role": "assistant", "content": "Hello", "reasoning_text": "thinking"},
                    {"role": "assistant", "content": "Done"},
                ])
            );
        }),
        ("openai-chat :: rejects reasoning fields that conflict with assistant message fields", || {
            let p = chat_provider(field("content"));
            let r = req("gpt-4o-mini", vec![asst(vec![thinking("thinking")])]);
            match p.prepare(&reqwest::Client::new(), &r) {
                Err(Error::Invalid(m)) => assert!(m.contains("reserved field content"), "{m}"),
                other => panic!("expected an invalid request, got {other:?}"),
            }
        }),
        ("openai-chat :: orders metadata-only reasoning before refusal output", || {
            let evs = chat(&sse(&[
                json!({"choices": [{"delta": {"reasoning_details": []}}]}),
                delta(json!({"refusal": "I can't help with that."})),
                delta_finish(json!({}), "stop"),
            ]))
            .unwrap();
            let mut meta = ReasoningMeta::of(Protocol::OpenaiChat);
            meta.details = Some(vec![]);
            assert_eq!(
                collect(&evs),
                [Part::Reasoning(Reasoning::new("", meta)), text("I can't help with that.")]
            );
        }),
        ("openai-chat :: does not replay reasoning details when the model declares another reasoning field", || {
            let p = chat_provider(field("reasoning_content"));
            let r = req(
                "gpt-4o-mini",
                vec![asst(vec![
                    chat_reasoning("thinking", Some("reasoning"), Some(signed_details())),
                    text("Hello"),
                ])],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello", "reasoning_content": "thinking"}])
            );
        }),
        ("openai-chat :: keeps replayed reasoning details when the model declares reasoning_details", || {
            let p = chat_provider(field("reasoning_details"));
            let r = req(
                "gpt-4o-mini",
                vec![asst(vec![
                    chat_reasoning("thinking", Some("reasoning"), Some(signed_details())),
                    text("Hello"),
                ])],
            );
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello", "reasoning_details": signed_details()}])
            );
        }),
        ("openai-chat :: parses and replays a configured custom reasoning field", || {
            let p = chat_provider(field("vendor_reasoning"));
            let compat = openai::Compat::for_provider(&p, "gpt-4o-mini");
            let evs = decode_all(
                openai::Decode::new(&compat),
                &sse(&[
                    json!({"choices": [{"delta": {"vendor_reasoning": "thinking"}}]}),
                    json!({"choices": [{"delta": {"content": "Hello"}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                ]),
            )
            .unwrap();
            assert_eq!(out(Ok(evs.clone())).reasoning, "thinking");
            let parts = collect(&evs);
            assert_eq!(
                parts[0],
                chat_reasoning("thinking", Some("vendor_reasoning"), None)
            );
            let r = req("gpt-4o-mini", vec![asst(parts)]);
            assert_eq!(
                chat_for(&p, &r)["messages"],
                json!([{"role": "assistant", "content": "Hello", "vendor_reasoning": "thinking"}])
            );
        }),
    ]);
}
