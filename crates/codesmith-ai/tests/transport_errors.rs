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
