//! Ports of OpenCode's `tool-shell.test.ts` (MIT) from the porting backlog:
//! shells run in parallel from one Code Mode program, and a plugin's after
//! hook on a stopped call. Its own test binary, because the plugin registry
//! is process-wide.

mod support;

use codesmith_core::plugin::{self, Plugin, ToolCall};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{Quiet, H};

/// Rewrites a failed shell call's output, remembering the original.
#[derive(Default)]
struct Rewrite {
    seen: Mutex<Vec<String>>,
}

impl Plugin for Rewrite {
    fn id(&self) -> &str {
        "backlog3-rewrite"
    }
    fn tool_after(&self, call: &ToolCall, output: &mut String, is_error: &mut bool) {
        if call.name == "shell" && *is_error && call.input["command"] == "sleep 30" {
            self.seen.lock().unwrap().push(output.clone());
            *output = "rewritten by a plugin".into();
        }
    }
}

fn rewrite() -> Arc<Rewrite> {
    static R: std::sync::OnceLock<Arc<Rewrite>> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        let r = Arc::new(Rewrite::default());
        plugin::install(vec![r.clone()]);
        r
    })
    .clone()
}

fn tool_results(h: &H) -> Vec<String> {
    let req = h.requests().pop().unwrap();
    req["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect()
}

// tool-shell.test.ts: "runs parallel Code Mode shells with the long default
// and notifies after a shorter timeout" (adapted: results are text; the
// default inside Code Mode stays the shell's two minutes, under the
// program's own three-minute limit, rather than OpenCode's ten)
#[tokio::test(flavor = "multi_thread")]
async fn oc_shell_runs_parallel_code_mode_shells_and_notifies_after_a_shorter_timeout() {
    rewrite();
    let mut h = H::with(
        json!({"model": "fake/claude-sonnet-4-5", "full_codemode": true}),
        None,
    )
    .await;
    let root = h.root();
    let quick = json!({"command": "printf body; exit 7"});
    let long = json!({
        "command": "printf xxxxxx; while [ ! -f release ]; do sleep 0.05; done",
        "timeout": 10,
    });
    let code = format!("return await Promise.all([tools.shell({quick}), tools.shell({long})])");
    let started = Instant::now();
    h.s.prompt(
        &format!("call execute {}", json!({ "code": code })),
        &mut Quiet,
    )
    .await
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(60));
    let result = tool_results(&h).pop().unwrap();
    assert!(result.contains("body\\n\\nExit code: 7"), "{result}");
    assert!(
        result.contains("did not finish within its timeout of 0.01 seconds"),
        "{result}"
    );
    assert!(result.contains("Command moved to the background (shell ID: shell_"));
    assert!(result.contains("PID: ") && result.contains("Output is streaming to: "));
    assert_eq!(h.s.background_running(), 1);

    // Released, it finishes, and its output reaches the model.
    std::fs::write(root.join("release"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        while h.s.background_finished() == 0 {
            h.s.wait_background().await;
        }
    })
    .await
    .expect("the command finished");
    h.s.resume_background(&mut Quiet).await.unwrap();
    let sent = h.requests().pop().unwrap()["messages"].to_string();
    assert!(sent.contains("xxxxxx"), "{sent}");
}

// tool-shell.test.ts: "lets an execute.after hook replace a stopped call's
// failure" (adapted: the hook sees the failure's text, "Command cancelled",
// rather than its metadata)
#[tokio::test(flavor = "multi_thread")]
async fn oc_shell_lets_an_after_hook_replace_a_stopped_calls_failure() {
    let r = rewrite();
    let mut h = H::new().await;
    let jobs = h.s.jobs();
    let stop = async {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(j) = jobs.foreground().first() {
                assert!(jobs.cancel(&j.id));
                return;
            }
            assert!(Instant::now() < deadline, "the command never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let text = format!("call shell {}", json!({"command": "sleep 30"}));
    let mut quiet = Quiet;
    let call = h.s.prompt(&text, &mut quiet);
    let (done, ()) = tokio::join!(call, stop);
    done.unwrap();
    assert_eq!(*r.seen.lock().unwrap(), ["Command cancelled"]);
    assert_eq!(tool_results(&h), ["rewritten by a plugin"]);
}
