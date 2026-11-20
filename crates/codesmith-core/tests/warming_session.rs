//! Prompt-cache warming in a session: after a turn, an idle session
//! repeats its latest request with the keep-alive prompt, on the same
//! prefix, until the warming window closes.

mod compaction_harness;

use compaction_harness::*;
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_session_repeats_its_latest_request_to_keep_the_cache_warm() {
    let (mut s, script, _d) = session(
        200_000,
        32_000,
        json!({"warming": {"interval": "200ms", "duration": "700ms"}}),
    )
    .await;
    s.prompt("Hello", &mut Quiet).await.unwrap();
    until(|| requests(&script, Kind::Main).len() >= 2).await;
    let main = requests(&script, Kind::Main);
    let (normal, ping) = (
        main[0]["messages"].as_array().unwrap(),
        main[1]["messages"].as_array().unwrap(),
    );
    assert_eq!(ping[..normal.len()], normal[..], "the same prefix");
    assert_eq!(
        ping.last().unwrap()["content"],
        codesmith_core::warming::DEFAULT_PROMPT
    );
    assert_eq!(main[1]["tools"], main[0]["tools"]);
    // The window closes 700ms after the request: at most three pings.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let pings = requests(&script, Kind::Main).len() - 1;
    assert!((2..=3).contains(&pings), "{pings}");
}

#[tokio::test(flavor = "multi_thread")]
async fn warming_is_off_by_default() {
    let (mut s, script, _d) = session(200_000, 32_000, json!({})).await;
    s.prompt("Hello", &mut Quiet).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&script, Kind::Main).len(), 1);
}
