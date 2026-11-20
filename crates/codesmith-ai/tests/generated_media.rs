//! Generated media over the cap is dropped with a note instead of kept.

use codesmith_ai::*;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A server answering one request with these SSE events.
async fn server(events: Vec<Value>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let mut buf = [0u8; 65536];
        let _ = s.read(&mut buf).await;
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let _ = s
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
    });
    format!("http://{addr}/v1")
}

async fn events(protocol: Protocol, base: String) -> Vec<Event> {
    let p = Provider {
        protocol,
        base_url: base,
        api_key: Some("k".into()),
        ..Default::default()
    };
    let req = Request {
        model: "m".into(),
        messages: vec![Message::user("draw")],
        ..Default::default()
    };
    let mut s = p.stream(&reqwest::Client::new(), &req).await.unwrap();
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e.unwrap());
    }
    out
}

fn big() -> String {
    "A".repeat((GENERATED_MEDIA_MAX / 3 + 1) * 4)
}

#[test]
fn the_cap_counts_decoded_bytes() {
    assert_eq!(oversized_media("image/png", "AQID"), None);
    let note = oversized_media("image/png\x1b", &big()).unwrap();
    assert_eq!(
        note,
        "[generated image/png dropped: about 20 MiB, over the 20 MiB limit]"
    );
}

#[tokio::test]
async fn gemini_drops_an_oversized_image() {
    let base = server(vec![
        json!({"candidates": [{"content": {"role": "model", "parts": [
        {"inlineData": {"mimeType": "image/png", "data": big()}},
        {"inlineData": {"mimeType": "image/png", "data": "AQID"}},
    ]}, "finishReason": "STOP"}]}),
    ])
    .await;
    let evs = events(Protocol::GoogleGemini, base).await;
    let media: Vec<&Media> = evs
        .iter()
        .filter_map(|e| match e {
            Event::Media(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(media.len(), 1);
    assert!(evs.iter().any(
        |e| matches!(e, Event::TextDelta(t) if t.starts_with("[generated image/png dropped"))
    ));
}

#[tokio::test]
async fn responses_drops_an_oversized_generated_image() {
    let item = json!({"type": "image_generation_call", "id": "ig_1", "status": "completed", "result": big()});
    let base = server(vec![
        json!({"type": "response.created", "response": {"id": "r1"}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        json!({"type": "response.completed", "response": {"id": "r1", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ])
    .await;
    let evs = events(Protocol::OpenaiResponses, base).await;
    let results: Vec<&Hosted> = evs
        .iter()
        .filter_map(|e| match e {
            Event::HostedResult(h) => Some(h),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "{evs:?}");
    let r = results[0].result.as_ref().unwrap();
    assert!(r.media.is_empty());
    assert!(r.value["message"]
        .as_str()
        .unwrap()
        .starts_with("[generated image dropped"));
}
