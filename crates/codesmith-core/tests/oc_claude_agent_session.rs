//! CodeSmith sessions on Claude Agent models, end to end against the fake
//! `claude` (see `claude_fake`): what a session's requests tell Claude
//! Code (OpenCode's session and instruction hooks for the provider,
//! `plugin.test.ts`, MIT), and that every tool call Claude makes is run
//! by CodeSmith under its own permission checks.
//!
//! The session reaches Claude Code through the process-wide registry, so
//! the tests set the environment it reads and run one at a time.

mod claude_fake;
mod plugin_support;

use claude_fake::{expect_startup, expect_turn, Fake, Kind, Step};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::Session;
use codesmith_core::store::Store;
use plugin_support::{options, serial, Collect};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// CodeSmith's data folder, for the whole test binary: the registry reads
/// it once.
fn data() -> &'static std::path::Path {
    static DATA: OnceLock<tempfile::TempDir> = OnceLock::new();
    DATA.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

struct Env {
    dir: tempfile::TempDir,
    fake: Fake,
    _server: tokio::task::JoinHandle<()>,
}

fn start(script: Vec<Step>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    // A chat's start lists models without context sizes, so the registry
    // also runs a login check: the fake answers those outside the script.
    let fake = Fake::new(script, true, &dir.path().join("claude"));
    let server = fake.serve(&dir.path().join("s"));
    std::env::set_var("XDG_DATA_HOME", data());
    std::env::set_var(
        codesmith_ai::claude_agent::PATH_VAR,
        claude_fake::executable(),
    );
    std::env::set_var("CLAUDE_CONFIG_DIR", dir.path().join("claude"));
    std::env::set_var("FAKE_CLAUDE_SOCKET", dir.path().join("s"));
    std::env::set_var(codesmith_core::SERVER_TOKEN_VAR, "secret-token");
    // Never the user's own config, skills or data.
    std::env::set_var("HOME", dir.path().join("home"));
    std::env::set_var("CODESMITH_CONFIG_DIR", dir.path().join("config"));
    std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
    std::fs::write(dir.path().join("AGENTS.md"), "PROJECT-RULE-MARKER").unwrap();
    Env {
        dir,
        fake,
        _server: server,
    }
}

fn loaded(dir: &std::path::Path, extra: Value) -> Loaded {
    let mut v = json!({"model": "claude-agent/haiku", "snapshots": false});
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    let config: Config = serde_json::from_value(v).unwrap();
    Loaded {
        config,
        files: vec![],
        root: dir.to_path_buf(),
    }
}

async fn close() {
    tokio::time::timeout(
        Duration::from_secs(30),
        codesmith_ai::claude_agent::global().close(),
    )
    .await
    .expect("the registry closed");
}

fn check(fake: &Fake) {
    if let Err(e) = fake.check() {
        panic!("{e}");
    }
}

// plugin.test.ts: "passes the agent's role to Claude on every kind of
// request", "passes opencode's instructions to Claude, except the date and
// environment Claude Code writes itself", "leaves the output budget to
// Claude and keeps tool error flags"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_passes_the_agents_role_and_instructions_but_not_the_date_or_environment() {
    let _serial = serial();
    let e = start(vec![
        expect_startup(),
        expect_turn(),
        claude_fake::expect_interrupt(),
    ]);
    let l = loaded(
        e.dir.path(),
        json!({"agents": {"reviewer": {"system": "You review code."}}}),
    );
    let mut o = options();
    o.agent = Some("reviewer".into());
    let mut s = Session::new(
        &l,
        e.dir.path().to_path_buf(),
        o,
        Arc::new(Fixed(Reply::Once)),
        None,
    )
    .unwrap();
    let mut sink = Collect::default();
    let r = tokio::time::timeout(
        Duration::from_secs(20),
        s.prompt(
            "Remember the marker COPPER-OTTER. Reply only with that marker.",
            &mut sink,
        ),
    )
    .await;
    eprintln!(
        "runs {} check {:?} r {:?}",
        e.fake.runs().len(),
        e.fake.check(),
        r.as_ref().map(|r| r.as_ref().map_err(|e| e.to_string()))
    );
    r.unwrap().unwrap();
    assert_eq!(sink.0, "COPPER-OTTER");
    let run = e.fake.run(0);
    let append = run.initialize()["appendSystemPrompt"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(append.contains("# Agent role (from CodeSmith)\nYou review code."));
    assert!(append.contains("PROJECT-RULE-MARKER"));
    assert!(!append.contains("Today's date"));
    assert!(!append.contains("useful information about the environment"));
    // Claude picks its own output budget.
    assert!(!run.env.contains_key("CLAUDE_CODE_MAX_OUTPUT_TOKENS"));
    // CodeSmith's server token never reaches Claude Code.
    assert!(!run.env.contains_key("CODESMITH_SERVER_TOKEN"));
    assert_eq!(
        run.cwd,
        e.dir.path().canonicalize().unwrap().display().to_string()
    );
    close().await;
    check(&e.fake);
}

// plugin.test.ts: "tags requests by kind, and marks a fork's primary
// requests with its source"
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_tags_requests_by_kind_and_marks_a_forks_primary_requests_with_its_source() {
    let _serial = serial();
    let e = start(vec![
        expect_startup(),
        expect_turn(),
        expect_startup(),
        claude_fake::turn("text", 1),
        claude_fake::expect_interrupt(),
        claude_fake::expect_interrupt(),
    ]);
    let l = loaded(e.dir.path(), json!({}));
    let db = e.dir.path().join("db.sqlite");
    let mut s = Session::new(
        &l,
        e.dir.path().to_path_buf(),
        options(),
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(&db).unwrap()),
    )
    .unwrap();
    let mut sink = Collect::default();
    s.prompt(
        "Remember the marker COPPER-OTTER. Reply only with that marker.",
        &mut sink,
    )
    .await
    .unwrap();
    let source = s.id.clone();
    drop(s);
    let mut st = Store::open(&db).unwrap();
    let fork = st.fork(&source, "fork").unwrap();
    let mut o = options();
    o.resume = Some(fork.id.clone());
    let mut f = Session::new(
        &l,
        e.dir.path().to_path_buf(),
        o,
        Arc::new(Fixed(Reply::Once)),
        Some(Store::open(&db).unwrap()),
    )
    .unwrap();
    let mut sink = Collect::default();
    let r = f
        .prompt("What was the marker? Reply only with it.", &mut sink)
        .await;
    r.unwrap();
    // The fork's Claude session is a copy of its source's transcript, not
    // a new one.
    let run = e
        .fake
        .runs()
        .into_iter()
        .filter(|r| r.args.iter().any(|a| a == "--include-partial-messages"))
        .nth(1)
        .unwrap();
    let resumed = run.arg("--resume=").expect("the fork resumes a copy");
    assert_ne!(Some(resumed), e.fake.run(0).arg("--session-id="));
    close().await;
    check(&e.fake);
}

// --- Permission bridging ---

const CALL: &str = "toolu_shell";

/// A turn whose reply calls CodeSmith's `shell` tool, held until CodeSmith
/// answers; then the reply `DONE`.
fn shell_turn(command: &str) -> Vec<Step> {
    let input = json!({"command": command, "description": "Write the marker"});
    let event = |e: Value| json!({"type": "stream_event", "event": e, "parent_tool_use_id": null, "session_id": "s", "uuid": claude_fake_uuid()});
    let mut turn = expect_turn();
    turn.replies = vec![
        json!({"type": "system", "subtype": "init", "session_id": "s", "mcp_servers": [], "tools": []}),
        event(
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-haiku-5-5", "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        ),
        json!({"type": "assistant", "uuid": "a-1", "parent_tool_use_id": null, "session_id": "s",
            "message": {"id": "msg_1", "role": "assistant", "content": [{"type": "tool_use", "id": CALL, "name": "mcp__codesmith__shell", "input": input}], "usage": {"input_tokens": 10, "output_tokens": 20}}}),
        json!({"type": "control_request", "request_id": "hook-pre", "request": {"subtype": "hook_callback", "callback_id": "hook_0",
            "input": {"hook_event_name": "PreToolUse", "tool_name": "mcp__codesmith__shell", "tool_use_id": CALL, "tool_input": input}}}),
    ];
    let after_hook = vec![
        event(
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        ),
        event(json!({"type": "message_stop"})),
        json!({"type": "control_request", "request_id": "call-1", "request": {"subtype": "mcp_message", "server_name": "codesmith",
            "message": {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "shell", "arguments": input}}}}),
    ];
    let exchange = |id: &str, replies: Vec<Value>| {
        let mut s = claude_fake::replay("tools")
            .into_iter()
            .find(|s| s.kind == Kind::Exchange && s.input["type"] == "control_response")
            .unwrap();
        s.input = json!({"type": "control_response", "response": {"request_id": id}});
        s.replies = replies;
        s
    };
    let done = vec![
        json!({"type": "user", "uuid": "u-2", "parent_tool_use_id": null, "session_id": "s",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": CALL, "content": "ran"}]}}),
        event(
            json!({"type": "message_start", "message": {"id": "msg_2", "model": "claude-haiku-5-5", "usage": {"input_tokens": 30, "output_tokens": 1}}}),
        ),
        event(
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        event(
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "DONE"}}),
        ),
        json!({"type": "assistant", "uuid": "a-2", "parent_tool_use_id": null, "session_id": "s",
            "message": {"id": "msg_2", "role": "assistant", "content": [{"type": "text", "text": "DONE"}], "usage": {"input_tokens": 30, "output_tokens": 2}}}),
        event(json!({"type": "content_block_stop", "index": 0})),
        event(
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        ),
        event(json!({"type": "message_stop"})),
        json!({"type": "result", "subtype": "success", "is_error": false, "result": "DONE", "num_turns": 2, "stop_reason": "end_turn", "session_id": "s",
            "usage": {"input_tokens": 40, "output_tokens": 22}}),
    ];
    vec![
        turn,
        // Claude calls the tool once the hook answered.
        exchange("hook-pre", after_hook),
        exchange(
            "call-1",
            vec![
                json!({"type": "control_request", "request_id": "hook-post", "request": {"subtype": "hook_callback", "callback_id": "hook_1",
                "input": {"hook_event_name": "PostToolUse", "tool_name": "mcp__codesmith__shell", "tool_use_id": CALL}}}),
            ],
        ),
        exchange("hook-post", done),
    ]
}

fn claude_fake_uuid() -> String {
    codesmith_ai::claude_agent::uuid()
}

/// An interrupt that ends a turn still waiting for a tool result.
fn interrupt_waiting() -> Step {
    let mut s = claude_fake::expect_interrupt();
    s.replies = vec![
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": "", "response": {}}}),
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "errors": [], "num_turns": 1, "session_id": "s", "usage": {}}),
    ];
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_tool_calls_run_in_codesmith_when_its_permissions_allow_them() {
    let _serial = serial();
    let marker = tempfile::tempdir().unwrap();
    let file = marker.path().join("marker.txt");
    let command = format!("echo RAN > {}", file.display());
    let mut script = vec![expect_startup()];
    script.extend(shell_turn(&command));
    script.push(claude_fake::expect_interrupt());
    let e = start(script);
    let l = loaded(e.dir.path(), json!({}));
    let mut s = Session::new(
        &l,
        e.dir.path().to_path_buf(),
        options(),
        Arc::new(Fixed(Reply::Once)),
        None,
    )
    .unwrap();
    let mut sink = Collect::default();
    s.prompt("Write the marker.", &mut sink).await.unwrap();
    assert_eq!(sink.0, "DONE");
    assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), "RAN");
    // Claude Code was offered CodeSmith's tools only.
    let run = e.fake.run(0);
    assert!(run.args.contains(&"--tools=".to_string()));
    assert!(run
        .args
        .contains(&"--allowedTools=mcp__codesmith__*".to_string()));
    assert!(run.args.contains(&"--permission-mode=dontAsk".to_string()));
    let results = run.mcp_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["isError"], false);
    close().await;
    check(&e.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_tool_calls_never_run_when_codesmiths_permissions_refuse_them() {
    let _serial = serial();
    let marker = tempfile::tempdir().unwrap();
    let file = marker.path().join("marker.txt");
    let command = format!("echo RAN > {}", file.display());
    let mut steps = shell_turn(&command);
    steps.truncate(2);
    let mut script = vec![expect_startup()];
    script.extend(steps);
    script.push(interrupt_waiting());
    let e = start(script);
    let l = loaded(e.dir.path(), json!({}));
    let mut s = Session::new(
        &l,
        e.dir.path().to_path_buf(),
        options(),
        Arc::new(Fixed(Reply::Reject(None))),
        None,
    )
    .unwrap();
    let mut sink = Collect::default();
    let _ = s.prompt("Write the marker.", &mut sink).await;
    assert!(!file.exists());
    // The declined call is recorded as such, and the turn stopped.
    let declined = s.messages.iter().flat_map(|m| &m.parts).any(
        |p| matches!(p, codesmith_ai::Part::ToolResult { id, is_error: true, .. } if id == CALL),
    );
    assert!(declined);
    // Claude was not handed a result for it: the waiting turn is
    // interrupted instead.
    close().await;
    assert!(e.fake.run(0).mcp_results().is_empty());
    check(&e.fake);
}

// CodeSmith's own (not an OpenCode port): the global `claude_agent`
// settings reach the registry as a turn starts, and with `prewarm` the
// session's process starts then, before the request is built, launched as
// the request launches it: that one process serves the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_prewarm_starts_the_sessions_process_as_its_turn_begins_and_that_process_serves_it()
{
    use codesmith_ai::claude_agent::{global, Pool};
    let _serial = serial();
    let e = start(vec![
        expect_startup(),
        expect_turn(),
        claude_fake::expect_interrupt(),
    ]);
    let l = loaded(
        e.dir.path(),
        json!({"claude_agent": {"prewarm": true, "max_processes": 3}}),
    );
    let before = global().prewarmed();
    let mut s = Session::new(
        &l,
        e.dir.path().to_path_buf(),
        options(),
        Arc::new(Fixed(Reply::Once)),
        None,
    )
    .unwrap();
    let mut sink = Collect::default();
    tokio::time::timeout(
        Duration::from_secs(20),
        s.prompt(
            "Remember the marker COPPER-OTTER. Reply only with that marker.",
            &mut sink,
        ),
    )
    .await
    .expect("the turn finished")
    .unwrap();
    assert_eq!(sink.0, "COPPER-OTTER");
    assert_eq!(
        global().pool(),
        Pool {
            prewarm: true,
            spare: 0,
            max_processes: 3
        }
    );
    assert_eq!(global().prewarmed() - before, 1);
    // Login checks aside, one `claude` ran: the prewarmed one.
    let chats: Vec<_> = e
        .fake
        .runs()
        .into_iter()
        .filter(|r| r.args.iter().any(|a| a == "--include-partial-messages"))
        .collect();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0].users().len(), 1);
    close().await;
    global().set_pool(Pool::default());
    check(&e.fake);
}
