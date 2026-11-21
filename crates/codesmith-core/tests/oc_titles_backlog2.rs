//! Ports of the title cases in PARITY.md's porting backlog
//! (`packages/core/test/session-title.test.ts`, MIT).

mod scripted;

use scripted::*;
use serde_json::json;
use std::sync::Arc;

/// "excludes commentary from titles when the response phase arrives at
/// ${timing}". Fixed: a title written over OpenAI Responses took every
/// message's text, so a model's commentary ("We need title only...") came
/// before its final answer in the title. The title model is a Responses
/// model on a server of its own.
#[tokio::test(flavor = "multi_thread")]
async fn oc_title_excludes_commentary_from_titles() {
    for at_start in [true, false] {
        let main: Shared = Arc::default();
        let titles: Shared = Arc::default();
        let (main_base, title_base) = (model(main.clone()).await, model(titles.clone()).await);
        let commentary = if at_start {
            json!({"type": "message", "id": "commentary", "phase": "commentary"})
        } else {
            json!({"type": "message", "id": "commentary"})
        };
        titles.lock().unwrap().replies.push_back(sse(&[
            json!({"type": "response.created", "response": {"id": "resp_title"}}),
            json!({"type": "response.output_item.added", "item": commentary}),
            json!({"type": "response.output_text.delta", "item_id": "commentary", "delta": "We need title only. Need capture investigation four machines performance read-only. Title concise."}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "commentary", "phase": "commentary"}}),
            json!({"type": "response.output_item.added", "item": {"type": "message", "id": "final", "phase": "final_answer"}}),
            json!({"type": "response.output_text.delta", "item_id": "final", "delta": "Read-only performance investigation"}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "id": "final", "phase": "final_answer"}}),
            json!({"type": "response.completed", "response": {"id": "resp_title", "status": "completed"}}),
        ]));
        let mut t = start(Setup {
            title: None,
            store: true,
            extra: json!({
                "providers": {
                    "fake": {"protocol": "openai-chat", "local": false, "settings": {"baseURL": main_base, "apiKey": "x"}},
                    "titles": {"protocol": "openai-responses", "local": false, "settings": {"baseURL": title_base, "apiKey": "x"}},
                },
                "agents": {"title": {"model": "titles/t"}},
            }),
            ..Default::default()
        })
        .await;
        t.prompt("Investigate performance across four hosts without making changes")
            .await
            .unwrap();
        assert_eq!(titles.lock().unwrap().log.len(), 1);
        assert_eq!(
            t.s.title.as_deref(),
            Some("Read-only performance investigation"),
            "phase at start: {at_start}"
        );
    }
}

/// "does not generate when the title agent is removed". Adapted: no title
/// request is made, and the session is named by its first line.
#[tokio::test(flavor = "multi_thread")]
async fn oc_title_does_not_generate_when_the_title_agent_is_removed() {
    let mut t = start(Setup {
        title: None,
        extra: json!({"agents": {"title": {"disabled": true}}}),
        ..Default::default()
    })
    .await;
    t.prompt("Help me debug the failing build").await.unwrap();
    assert!(t.script.lock().unwrap().title_log.is_empty());
    assert_eq!(
        t.s.title.as_deref(),
        Some("Help me debug the failing build")
    );
}
