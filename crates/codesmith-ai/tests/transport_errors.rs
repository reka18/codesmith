//! Transport errors do not carry the request URL's query string, where a
//! provider's `queryParams` (an API key) travel.

use codesmith_ai::*;

#[tokio::test]
async fn a_transport_error_hides_the_query_key() {
    // A port nothing listens on.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let p = Provider {
        protocol: Protocol::OpenaiChat,
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: Some("hdr-key".into()),
        query: vec![("key".into(), "SECRET-QUERY-KEY".into())],
        ..Default::default()
    };
    let req = Request {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        ..Default::default()
    };
    let e = match p.stream(&reqwest::Client::new(), &req).await {
        Err(e) => e,
        Ok(_) => panic!("connected"),
    };
    let text = e.to_string();
    assert!(!text.contains("SECRET-QUERY-KEY"), "{text}");
    assert!(!text.contains("hdr-key"), "{text}");
    assert!(
        text.starts_with("request failed: error sending request"),
        "{text}"
    );
    assert!(e.retryable());
}

#[test]
fn query_strings_are_redacted_from_urls_in_text() {
    assert_eq!(
        redact_urls("failed for (https://h.example/v1/x?key=abc&b=2): refused"),
        "failed for (https://h.example/v1/x?…): refused"
    );
    assert_eq!(
        redact_urls("wss://a/b#frag and http://c/d"),
        "wss://a/b?… and http://c/d"
    );
    assert_eq!(redact_urls("no urls ?key=x"), "no urls ?key=x");
}

#[test]
fn quoted_payloads_are_capped() {
    let small = "x".repeat(QUOTE_MAX);
    assert_eq!(quote(&small), small);
    let big = format!("{}é{}", "x".repeat(QUOTE_MAX - 1), "y".repeat(10_000));
    let q = quote(&big);
    assert!(q.len() < QUOTE_MAX + 40, "{}", q.len());
    assert!(q.ends_with(&format!("… ({} more bytes)", big.len() - (QUOTE_MAX - 1))));
}

// A stream frame that does not parse is quoted only in part.
#[tokio::test]
async fn a_bad_frame_is_quoted_in_part() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let mut buf = [0u8; 65536];
        let _ = s.read(&mut buf).await;
        let body = format!("data: {{{}\n\n", "z".repeat(1_000_000));
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
    let p = Provider {
        protocol: Protocol::OpenaiChat,
        base_url: format!("http://{addr}/v1"),
        api_key: Some("k".into()),
        ..Default::default()
    };
    let req = Request {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        ..Default::default()
    };
    use futures_util::StreamExt;
    let mut s = p.stream(&reqwest::Client::new(), &req).await.unwrap();
    let e = loop {
        match s.next().await {
            Some(Err(e)) => break e,
            Some(Ok(_)) => continue,
            None => panic!("no error"),
        }
    };
    let text = e.to_string();
    assert!(text.len() < 3 * 1024, "{}", text.len());
    assert!(text.contains("more bytes)"), "{text}");
}
