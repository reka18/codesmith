//! Ports of OpenCode's `execute` progress tests (`tool-execute.test.ts`):
//! the program's call rows, at most one update per 100 ms, the latest rows
//! sent before a failed or stopped program settles.

use codesmith_core::codemode::progress::Progress;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn row(tool: &str, status: &str) -> Value {
    json!({"tool": tool, "status": status})
}

fn rows(v: &[Value]) -> Value {
    json!({"toolCalls": v})
}

fn recorder() -> (Arc<Mutex<Vec<Value>>>, Progress) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let s = sent.clone();
    let p = Progress::new(move |m| s.lock().unwrap().push(m));
    (sent, p)
}

async fn settle_tasks() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn oc_execute_sends_at_most_one_progress_update_per_100_ms_with_the_latest_rows() {
    let (sent, p) = recorder();
    // for (const i of [1, 2, 3]) await tools.step({}); await tools.wait({})
    for _ in 0..3 {
        let i = p.start("step", Some(&json!({})));
        p.settle(i, true);
    }
    let wait = p.start("wait", Some(&json!({})));
    assert_eq!(*sent.lock().unwrap(), [rows(&[row("step", "running")])]);
    tokio::time::advance(Duration::from_millis(100)).await;
    settle_tasks().await;
    let steps = [
        row("step", "completed"),
        row("step", "completed"),
        row("step", "completed"),
    ];
    let mut with_wait = steps.to_vec();
    with_wait.push(row("wait", "running"));
    assert_eq!(sent.lock().unwrap()[1..], [rows(&with_wait)]);
    // The wait ends 150 ms after the trailing update, so its row goes out
    // at once; the last step's never does.
    tokio::time::advance(Duration::from_millis(150)).await;
    p.settle(wait, true);
    let last = p.start("step", Some(&json!({})));
    p.settle(last, true);
    p.finish(true);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle_tasks().await;
    let mut done = steps.to_vec();
    done.push(row("wait", "completed"));
    assert_eq!(sent.lock().unwrap()[2..], [rows(&done)]);
    done.push(row("step", "completed"));
    assert_eq!(p.rows(), done);
}

#[tokio::test(start_paused = true)]
async fn oc_execute_interrupted_sends_its_latest_progress_rows_before_it_settles() {
    let (sent, p) = recorder();
    for _ in 0..2 {
        let i = p.start("step", Some(&json!({})));
        p.settle(i, true);
    }
    p.start("wait", Some(&json!({})));
    // Stopped while the wait runs.
    drop(p);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle_tasks().await;
    assert_eq!(
        *sent.lock().unwrap(),
        [
            rows(&[row("step", "running")]),
            rows(&[
                row("step", "completed"),
                row("step", "completed"),
                row("wait", "error")
            ]),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_program_sends_its_pending_rows_and_shows_inputs() {
    let (sent, p) = recorder();
    let i = p.start("gh.issues", Some(&json!({"repo": "a/b"})));
    p.settle(i, false);
    p.finish(false);
    let s = sent.lock().unwrap();
    assert_eq!(s.len(), 2);
    assert_eq!(s[0]["toolCalls"][0]["input"], json!({"repo": "a/b"}));
    assert_eq!(s[1]["toolCalls"][0]["status"], "error");
}
