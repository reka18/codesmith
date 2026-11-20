//! Ports of OpenCode's background compaction summary tests
//! (`packages/core/test/session-compaction.test.ts`, MIT). OpenCode's tests
//! call `precompute` and `compact` on a context they build; here whole
//! turns run on the scripted model, whose replies report the prompt sizes
//! of OpenCode's fixtures. A session starts a background summary when a
//! step is about to run and when its turn ends, and compacts automatically
//! at the next prompt past the ceiling. See `crates/codesmith-core/PARITY.md`.

mod compaction_harness;

use compaction_harness::*;
use serde_json::{json, Value};

const WINDOW: u64 = 200_000;
const OUTPUT: u32 = 32_000;

fn joined(b: &Value) -> String {
    b["messages"].to_string()
}

/// OpenCode's `precomputeHistory` up to `near`: a 200k window compacts at
/// 170k; the first response measured 80k, so a background summary starts
/// 45k before the threshold, at 125k, and the second answer (156k) is past
/// that point.
async fn near(s: &mut codesmith_core::session::Session, script: &Shared) {
    push(
        script,
        [
            reply("First answer", 80_000),
            reply("Second answer", 156_000),
        ],
    );
    s.prompt("First question", &mut Quiet).await.unwrap();
    s.prompt("Second question", &mut Quiet).await.unwrap();
}

/// `due`: one more exchange past the threshold.
async fn due(s: &mut codesmith_core::session::Session, script: &Shared) {
    push(script, [reply("Third answer", 182_000)]);
    s.prompt("Third question", &mut Quiet).await.unwrap();
}

/// The prompt that finds compaction due.
async fn next(s: &mut codesmith_core::session::Session) {
    s.prompt("Next question", &mut Quiet).await.unwrap();
}

/// "background summaries stay off when disabled"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_background_summaries_stay_off_when_disabled() {
    let (mut s, script, _d) =
        session(WINDOW, OUTPUT, json!({"compaction": {"background": false}})).await;
    near(&mut s, &script).await;
    due(&mut s, &script).await;
    next(&mut s).await;
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(joined(&summary[0]).contains("Third answer"));
}

/// "automatic compaction swaps in the background summary without another
/// model call"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_automatic_compaction_swaps_in_the_background_summary() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, json!({})).await;
    near(&mut s, &script).await;
    summaries(&script, 1).await;
    due(&mut s, &script).await;
    let before = s.usage;
    next(&mut s).await;
    // The one request summarized the prefix the latest response was
    // generated from.
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(joined(&summary[0]).contains("Second question"));
    assert!(!joined(&summary[0]).contains("Second answer"));
    // The checkpoint keeps everything since that response verbatim.
    let shown: Vec<String> = s.messages.iter().map(|m| format!("{m:?}")).collect();
    assert!(shown[0].contains("<conversation-checkpoint>"));
    assert!(shown[0].contains("manual summary"));
    assert!(shown[1].contains("Second answer"), "{shown:?}");
    assert!(shown.iter().any(|m| m.contains("Third answer")));
    // The background call is billed once, when compaction takes it.
    // (Only the summary reports cached tokens.)
    assert_eq!(s.usage.cache_read - before.cache_read, 3);
}

/// "automatic compaction waits for a background summary still being
/// written"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_automatic_compaction_waits_for_a_background_summary_still_being_written() {
    struct Started(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl codesmith_core::session::Sink for Started {
        fn text(&mut self, _: &str) {}
        fn compaction_start(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let (mut s, script, _d) = session(WINDOW, OUTPUT, json!({})).await;
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    script.lock().unwrap().hold = Some(gate.clone());
    near(&mut s, &script).await;
    due(&mut s, &script).await;
    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut sink = Started(started.clone());
    let release = async {
        until(|| started.load(std::sync::atomic::Ordering::SeqCst)).await;
        // The compaction shows as running while the summary it waits for
        // is still being written; the turn's step has not run.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(requests(&script, Kind::Main).len(), 3);
        gate.add_permits(10);
    };
    let (r, ()) = tokio::join!(s.prompt("Next question", &mut sink), release);
    r.unwrap();
    assert_eq!(requests(&script, Kind::Summary).len(), 1);
    assert_eq!(requests(&script, Kind::Main).len(), 4);
}

/// "a model switch waits for the new model's first response before
/// summarizing in the background" (adapted: the model spec is switched in
/// place, and the prompt after the switch is what nears the threshold).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_model_switch_waits_for_the_new_models_first_response() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, json!({})).await;
    push(
        &script,
        [
            reply("First answer", 80_000),
            reply("Second answer", 120_000),
        ],
    );
    s.prompt("First question", &mut Quiet).await.unwrap();
    s.prompt("Second question", &mut Quiet).await.unwrap();
    s.model_spec = "fake/other".into();
    // Past the 125k start, but the latest response is the previous model's.
    push(&script, [reply("Third answer", 160_000)]);
    s.prompt(
        &format!("Third question {}", "x".repeat(40_000)),
        &mut Quiet,
    )
    .await
    .unwrap();
    summaries(&script, 1).await;
    let log = script.lock().unwrap().log.clone();
    let first = log.iter().position(|(k, _)| *k == Kind::Summary).unwrap();
    let mains = log[..first]
        .iter()
        .filter(|(k, _)| *k == Kind::Main)
        .count();
    assert_eq!(mains, 3, "nothing summarized before the new model answered");
    assert!(joined(&log[first].1).contains("Third question"));
    assert!(!joined(&log[first].1).contains("Third answer"));
}

/// A 100k window compacts at 84k. The first response measured 1k, so a
/// background summary starts 41.5k before that, at 42.5k, and may keep up
/// to 33.6k (40% of the budget) beside it: room for a tail that is long
/// for the summary but does not make compaction due.
async fn small_near(s: &mut codesmith_core::session::Session, script: &Shared) {
    push(
        script,
        [reply("First answer", 1_000), reply("Second answer", 43_000)],
    );
    s.prompt("First question", &mut Quiet).await.unwrap();
    s.prompt("Second question", &mut Quiet).await.unwrap();
}

/// "a background summary that fell too far behind is replaced from a newer
/// response" (adapted: on a 100k window, where a tail past what a summary
/// may keep beside it is not yet due for compaction).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_background_summary_that_fell_behind_is_replaced_from_a_newer_response() {
    let (mut s, script, _d) = session(100_000, 16_000, json!({})).await;
    small_near(&mut s, &script).await;
    summaries(&script, 1).await;
    // About 35k tokens arrive after the summarized prefix.
    push(
        &script,
        [reply("Burst handled", 60_000), reply("Next answer", 85_000)],
    );
    s.prompt(&"x".repeat(140_000), &mut Quiet).await.unwrap();
    summaries(&script, 2).await;
    s.prompt("Next question", &mut Quiet).await.unwrap();
    next(&mut s).await;
    // Compaction swapped in the newer summary, which ends before the
    // latest response it started from.
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 2);
    assert!(joined(&summary[1]).contains(&"x".repeat(1_000)));
    assert!(!joined(&summary[1]).contains("Burst handled"));
}

/// "a failed background summary is retried from a newer response, then
/// left until compaction"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_failed_background_summary_is_retried_then_left_until_compaction() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, json!({})).await;
    script.lock().unwrap().fail_summaries = 2;
    near(&mut s, &script).await;
    summaries(&script, 1).await;
    // The next step starts from the same response: no new request.
    push(&script, [reply("Third answer", 160_000)]);
    s.prompt("Third question", &mut Quiet).await.unwrap();
    summaries(&script, 2).await;
    let summary = requests(&script, Kind::Summary);
    let log = script.lock().unwrap().log.clone();
    let second = log
        .iter()
        .enumerate()
        .filter(|(_, (k, _))| *k == Kind::Summary)
        .nth(1)
        .unwrap()
        .0;
    assert_eq!(
        log[..second]
            .iter()
            .filter(|(k, _)| *k == Kind::Main)
            .count(),
        3,
        "retried only from the newer response"
    );
    assert!(joined(&summary[1]).contains("Second answer"));
    // Two failures in a row stop background summaries until compaction.
    push(&script, [reply("Fourth answer", 165_000)]);
    s.prompt("Fourth question", &mut Quiet).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(requests(&script, Kind::Summary).len(), 2);
}

/// "compaction summarizes as usual when the background summary no longer
/// fits the conversation" (adapted: the burst is the prompt that finds
/// compaction due, so no newer response replaced the summary first).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summarizes_as_usual_when_the_background_summary_no_longer_fits() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, json!({})).await;
    near(&mut s, &script).await;
    summaries(&script, 1).await;
    s.prompt(&"x".repeat(300_000), &mut Quiet).await.unwrap();
    // The background summary is stopped; the one kept covers the whole
    // conversation.
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 2);
    assert!(joined(&summary[1]).contains("Second answer"));
    assert!(joined(&summary[1]).contains(&"x".repeat(1_000)));
}

/// "a background summary is swapped in for a different model" (adapted:
/// the model spec is switched in place).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_background_summary_is_swapped_in_for_a_different_model() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, json!({})).await;
    near(&mut s, &script).await;
    summaries(&script, 1).await;
    s.model_spec = "fake/other".into();
    due(&mut s, &script).await;
    next(&mut s).await;
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(!joined(&summary[0]).contains("Second answer"));
}

/// "an overflow compaction swaps in the background summary"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_an_overflow_compaction_swaps_in_the_background_summary() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, json!({})).await;
    near(&mut s, &script).await;
    summaries(&script, 1).await;
    script.lock().unwrap().overflow = 1;
    s.prompt("Third question", &mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(!joined(&summary[0]).contains("Second answer"));
    assert!(format!("{:?}", s.messages[0]).contains("<conversation-checkpoint>"));
}

/// "a background summary starts when the latest response already passed
/// the threshold" (adapted: the second answer stays under the start, so
/// the third one, past the ceiling, is the first to start one).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_background_summary_starts_when_the_latest_response_passed_the_threshold() {
    let (mut s, script, _d) = session(WINDOW, OUTPUT, json!({})).await;
    push(
        &script,
        [
            reply("First answer", 80_000),
            reply("Second answer", 120_000),
            reply("Third answer", 182_000),
        ],
    );
    s.prompt("First question", &mut Quiet).await.unwrap();
    s.prompt("Second question", &mut Quiet).await.unwrap();
    // The turn ends past the threshold; the summary is written while the
    // session is idle.
    s.prompt("Third question", &mut Quiet).await.unwrap();
    summaries(&script, 1).await;
    s.prompt("Fourth question", &mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(joined(&summary[0]).contains("Third question"));
    assert!(!joined(&summary[0]).contains("Third answer"));
}

/// "a background summary keeps a tail longer than twice keep while it
/// stays under 40% of the budget" (adapted: on the 100k window above).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_background_summary_keeps_a_tail_longer_than_twice_keep_under_40_percent() {
    let (mut s, script, _d) = session(100_000, 16_000, json!({})).await;
    small_near(&mut s, &script).await;
    summaries(&script, 1).await;
    // About 32k tokens arrive after the summarized prefix: more than twice
    // keep, less than 33.6k.
    push(
        &script,
        [reply("Burst handled", 60_000), reply("Next answer", 85_000)],
    );
    s.prompt(&"x".repeat(128_000), &mut Quiet).await.unwrap();
    s.prompt("Next question", &mut Quiet).await.unwrap();
    next(&mut s).await;
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(joined(&summary[0]).contains("Second question"));
    assert!(!joined(&summary[0]).contains("Second answer"));
}

/// Four turns whose answers report `sizes`, then the prompt that compacts.
async fn four_turns(s: &mut codesmith_core::session::Session, script: &Shared, sizes: [u64; 4]) {
    let names = ["First", "Second", "Third", "Fourth"];
    push(
        script,
        names
            .iter()
            .zip(sizes)
            .map(|(n, size)| reply(&format!("{n} answer"), size)),
    );
    for n in names {
        s.prompt(&format!("{n} question"), &mut Quiet)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    next(s).await;
}

/// "a background summary starts a fixed lead before the threshold, not at
/// a share of it": a 1M window compacts at 850k, so a background summary
/// starts 60k before that, at 790k.
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_background_summary_starts_a_fixed_lead_before_the_threshold() {
    let (mut s, script, _d) = session(1_000_000, 128_000, json!({})).await;
    // 760k is past 85% of the threshold, but still more than the lead away.
    four_turns(&mut s, &script, [100_000, 760_000, 800_000, 860_000]).await;
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(joined(&summary[0]).contains("Third question"));
    assert!(!joined(&summary[0]).contains("Third answer"));
}

/// "a background summary starts on a window whose threshold is under 64k":
/// a 64k window compacts at 48k; the first response measured 20k, so half
/// that room, 14k, sets the start at 34k.
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_background_summary_starts_on_a_window_whose_threshold_is_under_64k() {
    let (mut s, script, _d) = session(64_000, 16_000, json!({})).await;
    four_turns(&mut s, &script, [20_000, 30_000, 40_000, 50_000]).await;
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    assert!(joined(&summary[0]).contains("Third question"));
    assert!(!joined(&summary[0]).contains("Third answer"));
}

/// "a background summary waits for a response whose prompt is still
/// cached" (adapted: a 32s cache, so the latest response is cold two
/// seconds later, against a 1h one).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_a_background_summary_waits_for_a_response_whose_prompt_is_still_cached() {
    for (ttl, pause, cold) in [("32s", 2_100, true), ("1h", 0, false)] {
        let (mut s, script, _d) = session(
            WINDOW,
            OUTPUT,
            json!({"providers": {"fake": {"models": {"m": {"cache": {"ttl": ttl}}}}}}),
        )
        .await;
        push(
            &script,
            [
                reply("First answer", 80_000),
                reply("Second answer", 120_000),
                reply("Third answer", 160_000),
                reply("Fourth answer", 182_000),
            ],
        );
        s.prompt("First question", &mut Quiet).await.unwrap();
        s.prompt("Second question", &mut Quiet).await.unwrap();
        // After a break, a prompt that passes the 125k start.
        tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
        s.prompt(
            &format!("Third question {}", "x".repeat(40_000)),
            &mut Quiet,
        )
        .await
        .unwrap();
        summaries(&script, 1).await;
        s.prompt("Fourth question", &mut Quiet).await.unwrap();
        next(&mut s).await;
        let summary = requests(&script, Kind::Summary);
        assert_eq!(summary.len(), 1, "{ttl}");
        // Past the cache's life, the summary starts from the next response.
        assert_eq!(
            joined(&summary[0]).contains("Third question"),
            cold,
            "{ttl}"
        );
    }
}
