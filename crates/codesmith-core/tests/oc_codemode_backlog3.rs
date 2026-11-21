//! Ports of OpenCode's `tool-execute.test.ts` (MIT) from the porting
//! backlog: what `execute` says it does, what a program gets back from a
//! failing or denied tool, its logs and result, namespaces that are also
//! tools, and a stop while a program waits on `fetch`. The outcome is
//! checked as values (result and logs), not as the text the model is shown.

mod support;

use codesmith_core::codemode::{self, Entry};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::{Quiet, H};
use tokio::io::AsyncReadExt;

fn entry(namespace: &str, name: &str) -> Entry {
    Entry {
        tool: format!("{namespace}_{name}").replace('.', "_"),
        namespace: namespace.into(),
        name: name.into(),
        description: format!("The {name} tool"),
        schema: json!({"type": "object", "properties": {}}),
        output: None,
        freeform: false,
    }
}

/// Run `code` with `entries`; each call returns the tool's name, and
/// `x_lying` fails.
async fn run(code: &str, entries: &[Entry]) -> codemode::Outcome {
    codemode::execute_with(
        None,
        code,
        entries,
        50,
        Duration::from_secs(10),
        |tool, _| async move {
            if tool == "x_lying" {
                Err("transport died".to_string())
            } else {
                Ok(tool)
            }
        },
    )
    .await
}

// tool-execute.test.ts: "execute describes invariant Code Mode behavior"
// (adapted: CodeSmith's wording of the same rules)
#[test]
fn oc_execute_describes_invariant_code_mode_behavior() {
    let (description, parameters) = codemode::spec();
    for rule in [
        "Run JavaScript in a confined Code Mode runtime to script tool calls and HTTP requests and compose their results.",
        "no code fence",
        "`fetch` is available for HTTP requests. Imports, direct filesystem access, and timers are unavailable: there is no `setTimeout` or sleep. All other external access goes through `tools`.",
        "the only callable tools are those listed in the Code Mode catalog",
        "preserving bracket notation such as `tools.<namespace>[\"tool-name\"](input)`",
        "Prefer an explicit `return`; if omitted, the final top-level expression becomes the result.",
        "Await every call whose result matters.",
    ] {
        assert!(description.contains(rule), "{rule}\n{description}");
    }
    assert_eq!(parameters["required"], json!(["code"]));
}

// tool-execute.test.ts: "foreign typed failures settle as Tool.Error at the
// untrusted boundary" (adapted: any tool failure reaches the program as an
// Error carrying its message, named by the tool)
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_settles_tool_failures_as_errors_in_the_program() {
    let out = run(
        "try { await tools.x.lying({}) } catch (e) { return [e instanceof Error, e.message] }",
        &[entry("x", "lying")],
    )
    .await;
    let v = out.result.unwrap();
    assert_eq!(v[0], json!(true));
    assert!(v[1].as_str().unwrap().ends_with("transport died"), "{v}");
    assert_eq!(out.calls, 1);
}

// tool-execute.test.ts: "a program that only logs shows just its logs"
// (adapted: no result is null)
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_a_program_that_only_logs_shows_its_logs() {
    let out = run(r#"console.log("found", 12, "files")"#, &[]).await;
    assert_eq!(out.result, Ok(Value::Null));
    assert_eq!(out.logs, ["found 12 files"]);
}

// tool-execute.test.ts: "a null result with nothing else still shows null"
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_a_null_result_with_nothing_else_still_shows_null() {
    let out = run("return null", &[]).await;
    assert_eq!(out.result, Ok(Value::Null));
    assert!(out.logs.is_empty());
    assert!(out.render().0.contains("null"));
}

// tool-execute.test.ts: "a returned value keeps its line before the logs"
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_a_returned_value_keeps_its_line_before_the_logs() {
    let out = run(r#"console.log("hi"); return { n: 1 }"#, &[]).await;
    assert_eq!(out.result, Ok(json!({"n": 1})));
    assert_eq!(out.logs, ["hi"]);
    let (text, failed) = out.render();
    assert!(!failed);
    let (value, logs) = (text.find("\"n\": 1").unwrap(), text.find("hi").unwrap());
    assert!(value < logs, "{text}");
}

// tool-execute.test.ts: "execute supports callable namespace tools"
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_supports_callable_namespace_tools() {
    let entries = [entry("slack", "admin"), entry("slack", "admin.create")];
    let out = run(
        "return [await tools.slack.admin({}), await tools.slack.admin.create({})]",
        &entries,
    )
    .await;
    assert_eq!(out.result, Ok(json!(["slack_admin", "slack_admin_create"])));
    assert_eq!(out.calls, 2);
}

// tool-execute.test.ts: "a denied permission reaches the program and
// changes nothing" (adapted: the built-ins a strong model calls inside
// `execute`, not Bun's file APIs)
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_a_denied_permission_reaches_the_program_and_changes_nothing() {
    // Denied for these files only, so the tools stay in the catalog (a tool
    // denied outright is not offered at all).
    for (action, code) in [
        ("read", "[await failure(() => tools.read({ path: 'keep.txt' }))]"),
        (
            "edit",
            "[await failure(() => tools.write({ path: 'new.txt', content: 'x' })), await failure(() => tools.edit({ path: 'keep.txt', oldString: 'keep', newString: 'gone' }))]",
        ),
    ] {
        let deny = json!([{"action": action, "resource": "*.txt", "effect": "deny"}]);
        let mut h = H::with(
            json!({"model": "fake/claude-sonnet-4-5", "full_codemode": true, "permissions": deny}),
            None,
        )
        .await;
        let root = h.root();
        std::fs::write(root.join("keep.txt"), "keep").unwrap();
        let code = format!(
            "const failure = async (run) => {{ try {{ await run(); return 'ran' }} catch (e) {{ return e.message }} }}\nreturn {code}"
        );
        h.s.prompt(
            &format!("call execute {}", json!({ "code": code })),
            &mut Quiet,
        )
        .await
        .unwrap();
        let sent = h.requests().pop().unwrap()["messages"].to_string();
        assert!(!sent.contains("\\\"ran\\\""), "{action}: {sent}");
        assert!(sent.to_lowercase().contains("denied"), "{action}: {sent}");
        let mut names: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["keep.txt"], "{action}");
        assert_eq!(std::fs::read_to_string(root.join("keep.txt")).unwrap(), "keep");
    }
}

// tool-execute.test.ts: "interrupting execute aborts a pending request"
// (adapted: the user's stop drops the turn, and with it the request)
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_interrupting_execute_aborts_a_pending_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/slow", listener.local_addr().unwrap());
    let started = Arc::new(tokio::sync::Notify::new());
    let closed = Arc::new(tokio::sync::Notify::new());
    let (s, c) = (started.clone(), closed.clone());
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        s.notify_one();
        // Never answers: reads until the client goes away.
        let mut buf = [0u8; 1024];
        while sock.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        c.notify_one();
    });
    let mut h = H::with(
        json!({"model": "fake/claude-sonnet-4-5", "full_codemode": true}),
        None,
    )
    .await;
    let code = format!("const res = await fetch({}); return res.status", json!(url));
    let text = format!("call execute {}", json!({ "code": code }));
    let mut quiet = Quiet;
    let deadline = Duration::from_secs(60);
    let began = Instant::now();
    tokio::select! {
        r = h.s.prompt(&text, &mut quiet) => panic!("the request never started: {r:?}"),
        _ = tokio::time::timeout(deadline, started.notified()) => {}
    }
    assert!(began.elapsed() < deadline, "the request never started");
    tokio::time::timeout(deadline, closed.notified())
        .await
        .expect("the request was aborted");
}
