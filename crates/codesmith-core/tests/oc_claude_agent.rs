//! Ports of OpenCode's Claude Agent provider tests
//! (`packages/core/test/plugin/provider/claude-agent/**`, MIT): Claude Code
//! as a model provider, against a fake `claude` that replays Claude Code
//! sessions OpenCode recorded (see `claude_fake`). Each test names the
//! OpenCode test it ports; see `crates/codesmith-core/PARITY.md`.

mod claude_fake;

use claude_fake::{
    expect_interrupt, expect_startup, expect_turn, interrupt, replay, startup, turn, Fake, Gate,
    Kind, Step,
};
use codesmith_ai::claude_agent::{
    self as agent, history, registry, store::Change, Io, Registry, Timing,
};
use codesmith_ai::{Error, Event, Message, Part, Protocol, Provider, Request, Role, ToolSpec};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FIRST: &str = "Remember the marker COPPER-OTTER. Reply only with that marker.";
const FOLLOWUP: &str = "What was the marker? Reply only with it.";
const SYSTEM: &str = "You are a concise test assistant.";

// --- The harness (OpenCode's `lib/sessions.ts` and `lib/tools.ts`) ---

struct Test {
    dir: tempfile::TempDir,
    fake: Fake,
    registry: Registry,
    changes: Arc<Mutex<Vec<Change>>>,
    _server: tokio::task::JoinHandle<()>,
}

fn env_for(dir: &std::path::Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
    env.insert("HOME".into(), dir.join("home").display().to_string());
    env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        dir.join("claude").display().to_string(),
    );
    env.insert(
        "FAKE_CLAUDE_SOCKET".into(),
        dir.join("s").display().to_string(),
    );
    env
}

fn io_for(dir: &std::path::Path, changes: &Arc<Mutex<Vec<Change>>>) -> Io {
    let env = env_for(dir);
    let changes = changes.clone();
    Io {
        which: Arc::new(|| Some(claude_fake::executable())),
        env: Arc::new(move || env.clone()),
        storage: dir.join("storage"),
        observe: Some(Arc::new(move |c: &Change| {
            changes.lock().unwrap().push(c.clone())
        })),
        recheck: false,
    }
}

impl Test {
    async fn start(script: Vec<Step>) -> Test {
        Test::with(script, false, Timing::default()).await
    }

    async fn with(script: Vec<Step>, probes: bool, timing: Timing) -> Test {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        let fake = Fake::new(script, probes, &dir.path().join("claude"));
        let server = fake.serve(&dir.path().join("s"));
        let changes = Arc::default();
        let registry = Registry::new(io_for(dir.path(), &changes), timing);
        Test {
            dir,
            fake,
            registry,
            changes,
            _server: server,
        }
    }

    /// A second registry over the same storage, as after a restart.
    fn restart(&self) -> Registry {
        Registry::new(io_for(self.dir.path(), &self.changes), Timing::default())
    }

    fn forks(&self) -> Vec<(String, String, String)> {
        self.changes
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                Change::Forked {
                    source,
                    boundary,
                    session_id,
                } => Some((source.clone(), boundary.clone(), session_id.clone())),
                _ => None,
            })
            .collect()
    }

    fn removed(&self) -> Vec<String> {
        self.changes
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                Change::Removed(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    fn record(&self, session: &str) -> Option<agent::store::Record> {
        self.registry.records().get(session)
    }

    fn work(&self) -> String {
        self.dir.path().join("work").display().to_string()
    }
}

fn user(text: &str) -> Message {
    Message::user(text)
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        parts: vec![Part::text(text)],
    }
}

fn first() -> Vec<Message> {
    vec![user(FIRST)]
}

fn followup(text: &str) -> Vec<Message> {
    vec![user(FIRST), assistant("COPPER-OTTER"), user(text)]
}

/// One model request, as CodeSmith's session sends it.
struct Ask {
    model: String,
    session: String,
    kind: &'static str,
    messages: Vec<Message>,
    tools: Vec<ToolSpec>,
    options: serde_json::Map<String, Value>,
    max_tokens: Option<u32>,
    headers: Vec<(String, String)>,
    system: String,
}

impl Ask {
    fn new(messages: Vec<Message>) -> Ask {
        Ask {
            model: "haiku".into(),
            session: "session-a".into(),
            kind: "primary",
            messages,
            tools: Vec::new(),
            options: Default::default(),
            max_tokens: None,
            headers: Vec::new(),
            system: SYSTEM.into(),
        }
    }
    fn kind(mut self, k: &'static str) -> Ask {
        self.kind = k;
        self
    }
    fn session(mut self, s: &str) -> Ask {
        self.session = s.into();
        self
    }
    fn tools(mut self, t: Vec<ToolSpec>) -> Ask {
        self.tools = t;
        self
    }
    fn option(mut self, k: &str, v: Value) -> Ask {
        self.options.insert(k.into(), v);
        self
    }
    fn header(mut self, k: &str, v: &str) -> Ask {
        self.headers.push((k.into(), v.into()));
        self
    }

    fn request(&self, dir: &str) -> registry::Request {
        let mut options = self.options.clone();
        options.insert(agent::DIRECTORY_OPTION.into(), json!(dir));
        let mut headers = vec![
            (agent::SESSION_HEADER.to_string(), self.session.clone()),
            (agent::KIND_HEADER.to_string(), self.kind.to_string()),
        ];
        headers.extend(self.headers.clone());
        let req = Request {
            model: self.model.clone(),
            system: self.system.clone(),
            messages: self.messages.clone(),
            tools: self.tools.clone(),
            max_tokens: self.max_tokens,
            options: codesmith_ai::Options::from_map(options),
            headers,
            ..Default::default()
        };
        let p = Provider {
            protocol: Protocol::ClaudeAgent,
            name: agent::ID.into(),
            ..Default::default()
        };
        agent::agent_request(&p, &req).unwrap()
    }
}

type Parts = Vec<Result<Event, String>>;

async fn collect(mut s: codesmith_ai::EventStream) -> Parts {
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e.map_err(|e: Error| e.to_string()));
    }
    out
}

impl Test {
    fn stream(&self, ask: &Ask) -> codesmith_ai::EventStream {
        agent::stream_with(&self.registry, ask.request(&self.work()))
    }

    async fn reply(&self, ask: Ask) -> Parts {
        tokio::time::timeout(Duration::from_secs(30), collect(self.stream(&ask)))
            .await
            .expect("the reply finished")
    }

    async fn reply_with(&self, registry: &Registry, ask: Ask) -> Parts {
        let s = agent::stream_with(registry, ask.request(&self.work()));
        tokio::time::timeout(Duration::from_secs(30), collect(s))
            .await
            .expect("the reply finished")
    }

    async fn close(&self) {
        tokio::time::timeout(Duration::from_secs(30), self.registry.close())
            .await
            .expect("the registry closed");
    }
}

fn text(parts: &Parts) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            Ok(Event::TextDelta(d)) => Some(d.as_str()),
            _ => None,
        })
        .collect()
}

fn error(parts: &Parts) -> Option<String> {
    parts.iter().find_map(|p| p.as_ref().err().cloned())
}

fn finish(
    parts: &Parts,
) -> Option<(
    codesmith_ai::FinishReason,
    codesmith_ai::Usage,
    Option<Value>,
)> {
    parts.iter().rev().find_map(|p| match p {
        Ok(Event::Finish {
            reason,
            usage,
            raw_usage,
        }) => Some((reason.clone(), *usage, raw_usage.clone())),
        _ => None,
    })
}

fn calls(parts: &Parts) -> Vec<(String, String, Value)> {
    parts
        .iter()
        .filter_map(|p| match p {
            Ok(Event::ToolCall { id, name, input }) => {
                Some((id.clone(), name.clone(), input.clone()))
            }
            _ => None,
        })
        .collect()
}

fn check(fake: &Fake) {
    if let Err(e) = fake.check() {
        panic!("{e}");
    }
}

// --- sessions.test.ts ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_streams_consecutive_turns_once_on_one_process_with_per_turn_usage() {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        turn("text", 1),
        expect_interrupt(),
    ])
    .await;
    let one = t.reply(Ask::new(first())).await;
    let two = t.reply(Ask::new(followup(FOLLOWUP))).await;
    assert_eq!(text(&one), "COPPER-OTTER");
    assert_eq!(text(&two), "COPPER-OTTER");
    let (_, usage, _) = finish(&two).unwrap();
    let recorded = claude_fake::result("text", 1)["usage"].clone();
    assert_eq!(usage.input, recorded["input_tokens"].as_u64().unwrap());
    assert_eq!(
        usage.cache_read,
        recorded["cache_read_input_tokens"].as_u64().unwrap()
    );
    assert_eq!(
        usage.cache_write,
        recorded["cache_creation_input_tokens"].as_u64().unwrap()
    );
    assert_eq!(usage.output, recorded["output_tokens"].as_u64().unwrap());
    assert_eq!(t.fake.runs().len(), 1);
    let users = t.fake.run(0).users();
    assert_eq!(users.len(), 2);
    assert_eq!(
        users[1]["message"]["content"],
        json!([{"type": "text", "text": FOLLOWUP}])
    );
    assert_eq!(users[0]["client_composed"], true);
    t.close().await;
    t.fake.run(0).closed.wait().await;
    check(&t.fake);
}

fn svg() -> Message {
    Message {
        role: Role::User,
        parts: vec![Part::Media(codesmith_ai::Media::base64(
            "image/svg+xml",
            "PHN2Zy8+",
        ))],
    }
}

/// Read a reply until its first text, then drop it (a cancel).
async fn cancel_after_text(mut s: codesmith_ai::EventStream) -> String {
    loop {
        match s.next().await {
            Some(Ok(Event::TextDelta(d))) => return d,
            Some(_) => {}
            None => panic!("the reply ended before any text"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_missing_claude_transcript_is_rebuilt_from_history_even_after_a_rejected_attachment() {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        expect_interrupt(),
        expect_startup(),
        turn("text", 1),
        expect_interrupt(),
    ])
    .await;
    t.reply(Ask::new(first())).await;
    let before = t.record("session-a").unwrap();
    t.close().await;
    // Claude Code lost the transcript.
    t.fake.delete_transcript(&before.claude_session_id);
    let registry = t.restart();
    let mut prompt = followup("Inspect this image");
    prompt.push(svg());
    let failed = t.reply_with(&registry, Ask::new(prompt)).await;
    assert!(error(&failed)
        .unwrap()
        .contains("Claude Code does not support image/svg+xml attachments"));
    t.reply_with(&registry, Ask::new(followup(FOLLOWUP))).await;
    assert_eq!(t.fake.runs().len(), 2);
    let run = t.fake.run(1);
    assert!(run.arg("--resume=").is_none());
    let content = run.users()[0]["message"]["content"].to_string();
    assert!(content.contains("<conversation-checkpoint>"));
    assert!(content.contains("COPPER-OTTER"));
    assert!(!content.contains("image/svg+xml"));
    assert_ne!(
        t.record("session-a").unwrap().claude_session_id,
        before.claude_session_id
    );
    registry.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_cancellation_drains_past_the_interrupt_receipt_before_accepting_another_turn() {
    let drain = Gate::default();
    let stop = interrupt("text-cancel").before_result(&drain);
    let t = Test::start(vec![
        expect_startup(),
        turn("text-cancel", 0),
        stop.clone(),
        turn("text-cancel", 1),
        expect_interrupt(),
    ])
    .await;
    let partial = cancel_after_text(t.stream(&Ask::new(first()))).await;
    stop.started.wait().await;
    let mut prompt = first();
    prompt.push(assistant(&partial));
    prompt.push(user("Continue"));
    let ask = Ask::new(prompt);
    let next = tokio::spawn({
        let s = t.stream(&ask);
        async move { collect(s).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.fake.run(0).users().len(), 1);
    drain.open();
    let next = next.await.unwrap();
    assert_eq!(text(&next), "COPPER-OTTER");
    // Claude's transcript already holds the partial reply, so only the new
    // input is sent, on the same process.
    assert_eq!(t.fake.runs().len(), 1);
    let users = t.fake.run(0).users();
    assert_eq!(
        users[1]["message"]["content"],
        json!([{"type": "text", "text": "Continue"}])
    );
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_cancelled_history_resumes_after_a_registry_restart_without_replaying_the_partial_reply()
{
    let t = Test::start(vec![
        expect_startup(),
        turn("text-cancel", 0),
        interrupt("text-cancel"),
        expect_interrupt(),
        expect_startup(),
        turn("text-cancel", 1),
        expect_interrupt(),
    ])
    .await;
    cancel_after_text(t.stream(&Ask::new(first()))).await;
    // The cancel drains in the background; closing waits for it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let saved = t.record("session-a").unwrap();
    assert_eq!(saved.interrupted, Some(true));
    t.close().await;
    let registry = t.restart();
    // CodeSmith may keep no assistant text when the cancel came early.
    let mut prompt = first();
    prompt.push(user("Continue"));
    assert_eq!(
        text(&t.reply_with(&registry, Ask::new(prompt)).await),
        "COPPER-OTTER"
    );
    let run = t.fake.run(1);
    assert_eq!(run.arg("--resume="), Some(saved.claude_session_id.clone()));
    assert_eq!(
        run.users()[0]["message"]["content"],
        json!([{"type": "text", "text": "Continue"}])
    );
    assert_eq!(t.record("session-a").unwrap().interrupted, Some(false));
    registry.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_chat_keeps_the_claude_code_preset_when_opencodes_built_in_prompt_changes() {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        turn("text", 1),
        expect_interrupt(),
    ])
    .await;
    t.reply(Ask::new(first())).await;
    let mut ask = Ask::new(followup(FOLLOWUP));
    ask.system = "A changed CodeSmith built-in agent prompt".into();
    t.reply(ask).await;
    assert_eq!(t.fake.runs().len(), 1);
    let run = t.fake.run(0);
    assert!(run.initialize().get("systemPrompt").is_none());
    assert!(!serde_json::to_string(&run.received())
        .unwrap()
        .contains("A changed CodeSmith built-in agent prompt"));
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_turn_arriving_during_idle_shutdown_waits_for_the_old_process_then_resumes_its_session(
) {
    let release = Gate::default();
    let stop = expect_interrupt().hold(&release);
    let t = Test::with(
        vec![
            expect_startup(),
            expect_turn(),
            stop.clone(),
            expect_startup(),
            turn("text-resume", 0),
            expect_interrupt(),
        ],
        false,
        Timing {
            idle: Duration::from_millis(200),
            ..Timing::default()
        },
    )
    .await;
    t.reply(Ask::new(first())).await;
    let id = t.fake.run(0).arg("--session-id=").unwrap();
    // The idle timer stops the process.
    stop.started.wait().await;
    let ask = Ask::new(followup(FOLLOWUP));
    let next = tokio::spawn({
        let s = t.stream(&ask);
        async move { collect(s).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.fake.runs().len(), 1);
    release.open();
    assert_eq!(text(&next.await.unwrap()), "COPPER-OTTER");
    t.fake.run(0).closed.wait().await;
    assert_eq!(t.fake.runs().len(), 2);
    assert_eq!(t.fake.run(1).arg("--resume="), Some(id));
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_cancelling_during_startup_closes_it_and_does_not_save_a_nonexistent_session() {
    let held = expect_startup().hold(&Gate::never());
    let t = Test::start(vec![
        held.clone(),
        expect_startup(),
        expect_turn(),
        expect_interrupt(),
    ])
    .await;
    let s = t.stream(&Ask::new(first()));
    held.started.wait().await;
    drop(s);
    t.fake.wait_run(0).await.closed.wait().await;
    assert_eq!(t.registry.records().count(), 0);
    assert_eq!(text(&t.reply(Ask::new(first())).await), "COPPER-OTTER");
    assert!(t.fake.run(1).arg("--resume=").is_none());
    t.close().await;
    assert_eq!(t.fake.check().unwrap_err(), "1 script steps did not finish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_chat_checks_compatibility_before_sending_a_prompt() {
    let mut old = expect_startup();
    old.reply["claude_code_version"] = json!("2.1.247");
    let t = Test::start(vec![old]).await;
    let failed = t.reply(Ask::new(first())).await;
    assert!(error(&failed).unwrap().contains("claude update"));
    assert!(t.fake.run(0).users().is_empty());
    t.fake.run(0).closed.wait().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_changes_model_and_effort_live_and_resumes_when_the_output_limit_changes_or_is_cleared()
{
    let controls: Vec<Step> = replay("controls")
        .into_iter()
        .filter(|s| s.is_exchange_of("set_model") || s.is_exchange_of("apply_flag_settings"))
        .collect();
    let recorded = |i| turn("controls", i);
    let t = Test::start(vec![
        expect_startup(),
        recorded(0),
        controls[0].clone(),
        recorded(1),
        controls[1].clone(),
        recorded(2),
        controls[2].clone(),
        controls[3].clone(),
        recorded(3),
        expect_interrupt(),
        expect_startup(),
        recorded(4),
        expect_interrupt(),
        expect_startup(),
        recorded(4),
        expect_interrupt(),
    ])
    .await;
    let mut prompt = vec![user("Remember CONTROL-OTTER. Reply only CONTROL-OTTER.")];
    let settings: [(&str, Option<&str>, Option<u32>); 6] = [
        ("haiku", Some("low"), Some(2048)),
        ("haiku", Some("high"), Some(2048)),
        ("haiku", None, Some(2048)),
        ("sonnet", Some("low"), Some(2048)),
        ("sonnet", Some("low"), Some(4096)),
        ("sonnet", Some("low"), None),
    ];
    for (model, effort, max) in settings {
        let mut ask = Ask::new(prompt.clone());
        ask.model = model.into();
        ask.max_tokens = max;
        if let Some(e) = effort {
            ask = ask.option("effort", json!(e));
        }
        assert_eq!(text(&t.reply(ask).await), "CONTROL-OTTER");
        prompt.push(assistant("CONTROL-OTTER"));
        prompt.push(user(FOLLOWUP));
    }
    assert_eq!(t.fake.runs().len(), 3);
    let first_run = t.fake.run(0);
    let id = first_run.arg("--session-id=").unwrap();
    let requests: Vec<Value> = first_run
        .received()
        .iter()
        .map(|m| m["request"].clone())
        .collect();
    assert!(requests.contains(&json!({"subtype": "set_model", "model": "sonnet"})));
    assert!(requests
        .contains(&json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": "high"}})));
    assert!(requests
        .contains(&json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": null}})));
    assert_eq!(
        first_run
            .env
            .get("CLAUDE_CODE_MAX_OUTPUT_TOKENS")
            .map(String::as_str),
        Some("2048")
    );
    assert_eq!(
        t.fake
            .run(1)
            .env
            .get("CLAUDE_CODE_MAX_OUTPUT_TOKENS")
            .map(String::as_str),
        Some("4096")
    );
    assert!(!t
        .fake
        .run(2)
        .env
        .contains_key("CLAUDE_CODE_MAX_OUTPUT_TOKENS"));
    for run in &t.fake.runs()[1..] {
        assert!(run.args.contains(&"--model=sonnet".to_string()));
        assert_eq!(run.arg("--resume="), Some(id.clone()));
    }
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_titles_use_their_own_task_prompt_in_a_throwaway_session_that_leaves_the_chat_alone() {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        expect_startup(),
        turn("utility", 0),
        expect_interrupt(),
        turn("text", 1),
        expect_interrupt(),
    ])
    .await;
    t.reply(Ask::new(first())).await;
    t.reply(Ask::new(followup(FOLLOWUP)).kind("title")).await;
    assert_eq!(
        text(&t.reply(Ask::new(followup(FOLLOWUP))).await),
        "COPPER-OTTER"
    );
    assert_eq!(t.fake.runs().len(), 2);
    let title = t.fake.run(1);
    assert_eq!(title.initialize()["systemPrompt"], json!([SYSTEM]));
    assert!(title.args.contains(&"--no-session-persistence".to_string()));
    assert_eq!(t.registry.records().count(), 1);
    t.close().await;
    check(&t.fake);
}

// --- tools (OpenCode's `lib/tools.ts`) ---

fn tool(name: &str, description: &str, parameters: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        parameters,
    }
}

fn lookup_tools() -> Vec<ToolSpec> {
    vec![
        tool(
            "lookup",
            "Look up a fixture marker by key.",
            json!({"type": "object", "properties": {"key": {"type": "string", "enum": ["a", "b"]}}, "required": ["key"], "additionalProperties": false}),
        ),
        tool(
            "confirm",
            "Confirm the two looked-up markers and get the final marker.",
            json!({"type": "object", "properties": {"markers": {"type": "array", "items": {"type": "string"}}}, "required": ["markers"], "additionalProperties": false}),
        ),
    ]
}

fn tool_prompt() -> Vec<Message> {
    vec![user("Call lookup twice in a single response, once with key a and once with key b. After both results, call confirm with those two markers. Then reply only with the final marker returned by confirm.")]
}

fn output(name: &str, input: &Value) -> (String, bool) {
    let v = if name == "confirm" {
        "COPPER-OTTER"
    } else if input["key"] == "a" {
        "LILAC-HERON"
    } else {
        "LEMON-IBIS"
    };
    (v.to_string(), false)
}

/// Add a step's reply and its calls' results to `prompt`, as CodeSmith's
/// session does.
fn append(
    prompt: &mut Vec<Message>,
    parts: &Parts,
    result: impl Fn(&str, &Value) -> (String, bool),
) {
    let mut content = Vec::new();
    let t = text(parts);
    if !t.is_empty() {
        content.push(Part::text(t));
    }
    let mut results = Vec::new();
    for (id, name, input) in calls(parts) {
        content.push(Part::tool_call(&id, &name, input.clone()));
        let (out, is_error) = result(&name, &input);
        results.push(Part::ToolResult {
            id,
            name,
            content: out,
            is_error,
            media: Vec::new(),
        });
    }
    prompt.push(Message {
        role: Role::Assistant,
        parts: content,
    });
    if !results.is_empty() {
        prompt.push(Message {
            role: Role::User,
            parts: results,
        });
    }
}

impl Test {
    /// A step with tools; fails on an error.
    async fn step(&self, ask: Ask) -> Parts {
        let parts = self.reply(ask).await;
        if let Some(e) = error(&parts) {
            panic!("step failed: {e}");
        }
        parts
    }
}

fn denied(_: &str, _: &Value) -> (String, bool) {
    ("Cancelled by user.".into(), true)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_conversation_branches_send_the_chats_tools_and_system_prompt_and_a_tool_call_ends_the_branch(
) {
    let waiting = replay("tools-waiting-cancel");
    let resumed = waiting
        .iter()
        .enumerate()
        .position(|(i, s)| i > 1 && s.kind == Kind::Exchange && s.input["type"] == "user")
        .unwrap();
    let mut script = vec![expect_startup(), expect_turn()];
    script.extend(waiting[..resumed].iter().cloned());
    script.push(expect_interrupt());
    let t = Test::start(script).await;
    let settings = |a: Ask| {
        a.tools(lookup_tools())
            .option(agent::AGENT_OPTION, json!("AGENT-ROLE"))
            .option(agent::INSTRUCTIONS_OPTION, json!("CODESMITH-INSTRUCTIONS"))
    };
    t.step(settings(Ask::new(first()))).await;
    let parts = t
        .step(settings(Ask::new(followup(FOLLOWUP))).kind("generate"))
        .await;
    let chat = t.fake.run(0).initialize();
    let branch = t.fake.run(1).initialize();
    assert_eq!(chat["sdkMcpServers"], json!(["codesmith"]));
    assert_eq!(branch["sdkMcpServers"], chat["sdkMcpServers"]);
    let append = chat["appendSystemPrompt"].as_str().unwrap();
    assert!(append.contains("AGENT-ROLE"));
    assert!(append.contains("CODESMITH-INSTRUCTIONS"));
    assert_eq!(branch["appendSystemPrompt"], chat["appendSystemPrompt"]);
    // The reply ends at the tool call; the throwaway process and fork go.
    assert_eq!(calls(&parts).len(), 2);
    t.fake.run(1).closed.wait().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.removed(), vec![t.forks()[0].2.clone()]);
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_closing_the_registry_interrupts_and_drains_a_running_side_call() {
    let t = Test::start(vec![
        expect_startup(),
        turn("text-cancel", 0),
        interrupt("text-cancel"),
        expect_interrupt(),
    ])
    .await;
    let mut s = t.stream(&Ask::new(first()).kind("generate"));
    loop {
        match s.next().await {
            Some(Ok(Event::TextDelta(_))) => break,
            Some(_) => {}
            None => panic!("ended before shutdown could be tested"),
        }
    }
    t.close().await;
    t.fake.run(0).closed.wait().await;
    drop(s);
    assert_eq!(t.registry.records().count(), 0);
    check(&t.fake);
}

// --- tools.test.ts ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_two_tool_steps_keep_one_turn_alive_past_the_idle_timeout_and_match_every_result_to_its_call(
) {
    let idle = Duration::from_millis(150);
    let t = Test::with(
        replay("tools"),
        false,
        Timing {
            idle,
            ..Timing::default()
        },
    )
    .await;
    let mut prompt = tool_prompt();
    let mut steps = Vec::new();
    for _ in 0..3 {
        // Subagents and unanswered permission prompts can keep CodeSmith's
        // tools running this long.
        tokio::time::sleep(idle * 2).await;
        let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
        append(&mut prompt, &parts, output);
        steps.push(parts);
    }
    assert_eq!(calls(&steps[0]).len(), 2);
    assert_eq!(calls(&steps[1])[0].1, "confirm");
    assert_eq!(text(&steps[2]), "COPPER-OTTER");
    assert_eq!(t.fake.runs().len(), 1);
    let run = t.fake.run(0);
    assert_eq!(run.users().len(), 1);
    assert!(run.requests("interrupt").is_empty());
    let outputs: Vec<Value> = run
        .mcp_results()
        .iter()
        .map(|r| r["content"][0]["text"].clone())
        .collect();
    assert_eq!(
        outputs,
        vec![
            json!("LILAC-HERON"),
            json!("LEMON-IBIS"),
            json!("COPPER-OTTER")
        ]
    );
    let usage: Vec<Value> = claude_fake::load("tools")
        .into_iter()
        .filter(|l| {
            l["from"] == "claude"
                && l["message"]["type"] == "stream_event"
                && l["message"]["event"]["type"] == "message_delta"
        })
        .collect();
    for (i, s) in steps.iter().enumerate() {
        assert_eq!(
            finish(s).unwrap().1.output,
            usage[i]["message"]["event"]["usage"]["output_tokens"]
                .as_u64()
                .unwrap()
        );
    }
    t.close().await;
    check(&t.fake);
}

fn until_next_user(steps: Vec<Step>) -> Vec<Step> {
    let next = steps
        .iter()
        .enumerate()
        .position(|(i, s)| i > 1 && s.kind == Kind::Exchange && s.input["type"] == "user")
        .unwrap();
    steps[..next].to_vec()
}

fn terminal(steps: &[Step]) -> usize {
    steps
        .iter()
        .position(|s| s.kind == Kind::Exchange && s.replies.iter().any(|m| m["type"] == "result"))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_closing_the_registry_waits_for_a_running_tool_call_to_drain_without_answering_it() {
    let mut steps = until_next_user(replay("tools-waiting-cancel"));
    let drain = Gate::default();
    let end = terminal(&steps);
    steps[end] = steps[end].clone().before_result(&drain);
    let ending = steps[end].clone();
    let t = Arc::new(Test::start(steps).await);
    t.step(Ask::new(tool_prompt()).tools(lookup_tools())).await;
    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let close = tokio::spawn({
        let t = t.clone();
        let stopped = stopped.clone();
        async move {
            t.close().await;
            stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
    ending.started.wait().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!stopped.load(std::sync::atomic::Ordering::SeqCst));
    let call = claude_fake::load("tools-waiting-cancel")
        .into_iter()
        .find(|l| {
            l["from"] == "claude" && l["message"]["request"]["message"]["method"] == "tools/call"
        })
        .unwrap();
    let answered = |t: &Test| {
        t.fake
            .run(0)
            .received()
            .iter()
            .any(|m| m["response"]["request_id"] == call["message"]["request_id"])
    };
    assert!(!answered(&t));
    drain.open();
    close.await.unwrap();
    assert!(!answered(&t));
    assert_eq!(t.fake.run(0).requests("interrupt").len(), 1);
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_turn_left_waiting_for_tool_results_stops_idle_ms_after_opencode_stops_running_the_session(
) {
    let t = Test::with(
        until_next_user(replay("tools-waiting-cancel")),
        false,
        Timing {
            idle: Duration::from_millis(150),
            ..Timing::default()
        },
    )
    .await;
    t.step(Ask::new(tool_prompt()).tools(lookup_tools())).await;
    // Parked: the timer does not run while CodeSmith runs the tools.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!t.fake.run(0).closed.is_open());
    t.registry.idle("session-a").await;
    tokio::time::timeout(Duration::from_secs(10), t.fake.run(0).closed.wait())
        .await
        .expect("the parked process stopped");
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_process_exit_while_opencode_runs_tools_resumes_with_their_saved_results() {
    let steps = replay("tools-cancel");
    let at = steps
        .iter()
        .position(|s| s.is_exchange_of("interrupt"))
        .unwrap();
    // The first process exits instead of taking an orderly interrupt.
    let mut script = steps[..at].to_vec();
    script.extend(steps[at + 1..].iter().cloned());
    let t = Test::start(script).await;
    let mut prompt = tool_prompt();
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
    append(&mut prompt, &parts, output);
    t.fake.run(0).exit(1).await;
    // Let CodeSmith see the exit before the next step.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let parts = t.step(Ask::new(prompt).tools(lookup_tools())).await;
    assert!(text(&parts).contains("CANCELLED-OTTER"));
    let resumed = t.fake.run(1);
    let content = resumed.users()[0]["message"]["content"].to_string();
    assert!(content.contains("LILAC-HERON"));
    assert!(content.contains("LEMON-IBIS"));
    assert!(resumed.arg("--resume=").is_some());
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_results_that_do_not_answer_every_waiting_call_stop_the_turn_and_rebuild_from_the_full_history(
) {
    let t = Test::start(replay("tools-cancel")).await;
    let mut prompt = tool_prompt();
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
    append(&mut prompt, &parts, output);
    // Drop the second lookup's result, so one waiting call has no answer.
    prompt.last_mut().unwrap().parts.pop();
    prompt.push(user(
        "Do not call any tools. Reply only with CANCELLED-OTTER.",
    ));
    let parts = t.step(Ask::new(prompt).tools(lookup_tools())).await;
    assert!(text(&parts).contains("CANCELLED-OTTER"));
    assert_eq!(t.fake.runs().len(), 2);
    assert_eq!(t.fake.run(0).requests("interrupt").len(), 1);
    let rebuilt = t.fake.run(1);
    assert!(rebuilt.arg("--resume=").is_none());
    let content = rebuilt.users()[0]["message"]["content"].to_string();
    assert!(content.contains("<conversation-checkpoint>"));
    assert!(content.contains("LILAC-HERON"));
    assert!(!content.contains("LEMON-IBIS"));
    t.close().await;
    check(&t.fake);
}

const CONTINUE: &str =
    "The calls were cancelled. Do not call any more tools. Reply only with CONTINUED-OTTER.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_active_turn_steering_delivers_tool_feedback_and_new_input_through_the_sdk_queue() {
    let t = Test::start(replay("tools-continue")).await;
    let mut prompt = tool_prompt();
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
    append(&mut prompt, &parts, denied);
    prompt.push(user(CONTINUE));
    let parts = t.step(Ask::new(prompt).tools(lookup_tools())).await;
    assert_eq!(text(&parts).trim(), "CONTINUED-OTTER");
    assert_eq!(t.fake.runs().len(), 1);
    let run = t.fake.run(0);
    assert!(run.requests("interrupt").is_empty());
    let users = run.users();
    let last = users.last().unwrap();
    assert_eq!(last["priority"], "next");
    assert_eq!(
        last["message"]["content"],
        json!([{"type": "text", "text": CONTINUE}])
    );
    let results = run.mcp_results();
    assert_eq!(results.len(), 2);
    for r in results {
        assert_eq!(
            r,
            json!({"isError": true, "content": [{"type": "text", "text": "Cancelled by user."}]})
        );
    }
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_an_aborted_tool_drains_the_old_turn_before_sending_a_fresh_user_message_on_the_same_process(
) {
    let mut steps = replay("tools-abort");
    let drain = Gate::default();
    let end = terminal(&steps);
    steps[end] = steps[end].clone().before_result(&drain);
    let ending = steps[end].clone();
    let t = Arc::new(Test::start(steps).await);
    let mut prompt = tool_prompt();
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
    let ids: Vec<String> = calls(&parts).into_iter().map(|c| c.0).collect();
    append(&mut prompt, &parts, denied);
    prompt.push(user(CONTINUE));
    let ask = Ask::new(prompt)
        .tools(lookup_tools())
        .option(agent::ABORTED_OPTION, json!(ids));
    let next = tokio::spawn({
        let s = t.stream(&ask);
        async move { collect(s).await }
    });
    ending.started.wait().await;
    let run = t.fake.run(0);
    // The interrupt's receipt is not enough: no new input until the
    // terminal result flushed.
    assert_eq!(run.users().len(), 1);
    assert!(run.mcp_results().is_empty());
    drain.open();
    assert_eq!(text(&next.await.unwrap()).trim(), "CONTINUED-OTTER");
    assert_eq!(t.fake.runs().len(), 1);
    assert_eq!(run.requests("interrupt").len(), 1);
    let users = run.users();
    let last = users.last().unwrap();
    assert!(last.get("priority").is_none());
    assert!(last["message"]["content"]
        .to_string()
        .contains("Cancelled by user."));
    assert_eq!(
        last["message"]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        &json!({"type": "text", "text": CONTINUE})
    );
    t.close().await;
    check(&t.fake);
}

fn fixture(name: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .encode(std::fs::read(claude_fake::fixtures().join(name)).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_tool_images_and_pdfs_reach_claude_as_image_and_document_content() {
    // Only the first process: the recording goes on to change the tools.
    let steps = replay("tools-media");
    let restart = steps
        .iter()
        .enumerate()
        .position(|(i, s)| i > 0 && s.kind == Kind::Startup)
        .unwrap();
    let t = Test::start(steps[..restart].to_vec()).await;
    let tools = vec![tool(
        "attachment",
        "Get the fixture image and PDF.",
        json!({"type": "object", "properties": {}}),
    )];
    let mut prompt = vec![user(
        "Call attachment once. Report the image color and the marker from its PDF.",
    )];
    let parts = t.step(Ask::new(prompt.clone()).tools(tools.clone())).await;
    let (png, pdf) = (fixture("red.png"), fixture("marker.pdf"));
    append(&mut prompt, &parts, |_, _| {
        ("Read the attached image and PDF.".into(), false)
    });
    if let Some(Part::ToolResult { media, .. }) = prompt.last_mut().unwrap().parts.last_mut() {
        media.push(codesmith_ai::Media::base64("image/png", &png));
        media.push(codesmith_ai::Media::base64("application/pdf", &pdf));
    }
    t.step(Ask::new(prompt).tools(tools)).await;
    let replacement = t
        .fake
        .run(0)
        .received()
        .into_iter()
        .find(|m| {
            !m["response"]["response"]["hookSpecificOutput"]["updatedMCPToolOutput"].is_null()
        })
        .unwrap();
    assert_eq!(
        replacement["response"]["response"]["hookSpecificOutput"]["updatedMCPToolOutput"],
        json!([
            {"type": "text", "text": "Read the attached image and PDF."},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png}},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": pdf}},
        ])
    );
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_last_step_without_tools_stops_the_parked_turn_and_resumes_with_the_saved_result() {
    let t = Test::start(replay("max-steps")).await;
    let lookup = vec![tool(
        "lookup",
        "Look up a fixture marker by key.",
        json!({"type": "object", "properties": {}}),
    )];
    let mut prompt = vec![user(
        "Call lookup once, then call it again, then report the marker.",
    )];
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup)).await;
    append(&mut prompt, &parts, |_, _| ("STEP-OTTER".into(), false));
    prompt.push(user("MAXIMUM STEPS REACHED. Respond with text only."));
    t.step(Ask::new(prompt)).await;
    assert_eq!(t.fake.runs().len(), 2);
    let last = t.fake.run(1);
    assert!(last.arg("--resume=").is_some());
    assert!(last.initialize().get("sdkMcpServers").is_none());
    assert!(last.users()[0]["message"]["content"]
        .to_string()
        .contains("STEP-OTTER"));
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_tool_list_change_lets_the_parked_turn_finish_and_answers_calls_to_removed_tools() {
    let t = Test::start(replay("tools")).await;
    let mut prompt = tool_prompt();
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
    append(&mut prompt, &parts, output);
    let remaining: Vec<ToolSpec> = lookup_tools()
        .into_iter()
        .filter(|t| t.name != "confirm")
        .collect();
    t.step(Ask::new(prompt).tools(remaining)).await;
    let last = t.fake.run(0).mcp_results().last().cloned().unwrap();
    assert_eq!(
        last,
        json!({"isError": true, "content": [{"type": "text", "text": "This tool is no longer available."}]})
    );
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_an_unavailable_opencode_server_fails_closed() {
    // The turn stops at its start, so the failure interrupts a running
    // turn, which then drains.
    let mut turn = turn("text-cancel", 0);
    let init = turn
        .replies
        .iter()
        .position(|m| m["type"] == "system" && m["subtype"] == "init")
        .unwrap();
    turn.replies.truncate(init + 1);
    turn.replies[init]["mcp_servers"] = json!([{"name": "codesmith", "status": "failed"}]);
    let t = Test::start(vec![
        expect_startup(),
        turn,
        interrupt("text-cancel"),
        expect_interrupt(),
    ])
    .await;
    let parts = t.reply(Ask::new(tool_prompt()).tools(lookup_tools())).await;
    assert!(error(&parts)
        .unwrap()
        .contains("CodeSmith tool server did not connect"));
    assert_eq!(t.fake.runs().len(), 1);
    t.close().await;
    check(&t.fake);
}

// --- branches.test.ts ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_failed_compaction_deletes_its_temporary_branch_and_keeps_the_unmodified_primary_history_resumable(
) {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        expect_startup(),
        turn("billing-error", 0),
        expect_interrupt(),
        turn("text", 1),
        expect_interrupt(),
    ])
    .await;
    t.reply(Ask::new(first())).await;
    let before = t.record("session-a");
    let reply = t
        .reply(Ask::new(followup("Summarize")).kind("compaction"))
        .await;
    assert!(error(&reply).unwrap().contains("extra usage"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.removed(), vec![t.forks()[0].2.clone()]);
    assert_eq!(t.record("session-a"), before);
    assert_eq!(
        text(&t.reply(Ask::new(followup(FOLLOWUP))).await).trim(),
        "COPPER-OTTER"
    );
    assert_eq!(t.fake.runs().len(), 2);
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_an_unsupported_attachment_in_changed_history_fails_with_its_own_message_before_forking()
{
    let t = Test::start(vec![expect_startup(), expect_turn(), expect_interrupt()]).await;
    t.reply(Ask::new(first())).await;
    let mut edited = followup(FOLLOWUP);
    edited[1] = assistant("An edited reply");
    edited[2] = svg();
    let failed = t.reply(Ask::new(edited)).await;
    assert!(error(&failed)
        .unwrap()
        .contains("Claude Code does not support image/svg+xml attachments"));
    assert!(t.forks().is_empty());
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_fork_finds_its_source_after_reload_and_rebuilds_safely_if_that_transcript_has_disappeared(
) {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        turn("text", 1),
        expect_interrupt(),
        expect_startup(),
        expect_turn(),
        expect_startup(),
        expect_turn(),
        expect_interrupt(),
        expect_interrupt(),
    ])
    .await;
    t.reply(Ask::new(first())).await;
    t.reply(Ask::new(followup(FOLLOWUP))).await;
    let source = t.record("session-a").unwrap();
    t.close().await;
    let next = t.restart();
    let fork = |child: &str| {
        Ask::new(followup("A forked question"))
            .session(child)
            .header(agent::FORK_HEADER, "session-a")
    };
    t.reply_with(&next, fork("child-a")).await;
    let forks = t.forks();
    assert_eq!(forks[0].0, source.claude_session_id);
    assert_eq!(t.fake.run(1).arg("--resume="), Some(forks[0].2.clone()));
    t.fake.delete_transcript(&source.claude_session_id);
    t.reply_with(&next, fork("child-b")).await;
    assert_eq!(t.forks().len(), 1);
    let rebuilt = t.fake.run(2);
    assert!(rebuilt.arg("--resume=").is_none());
    let content = rebuilt.users()[0]["message"]["content"].to_string();
    assert!(content.contains("COPPER-OTTER"));
    assert!(content.contains("A forked question"));
    next.close().await;
    check(&t.fake);
}

fn checkpoint(r: &agent::store::Record, i: usize) -> Option<String> {
    r.history.as_ref().unwrap()[i].claude_message_id.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_undo_resends_an_unchanged_question_then_branches_again_using_remapped_checkpoints() {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        turn("text", 1),
        expect_interrupt(),
        expect_startup(),
        expect_turn(),
        expect_interrupt(),
        expect_startup(),
        expect_turn(),
        expect_interrupt(),
    ])
    .await;
    t.reply(Ask::new(first())).await;
    t.reply(Ask::new(followup(FOLLOWUP))).await;
    let original = t.record("session-a").unwrap();
    // Undo, then the same question again.
    t.reply(Ask::new(followup(FOLLOWUP))).await;
    let branch = t.record("session-a").unwrap();
    assert_eq!(Some(t.forks()[0].1.clone()), checkpoint(&original, 1));
    assert_eq!(
        t.fake.run(1).users()[0]["message"]["content"],
        json!([{"type": "text", "text": FOLLOWUP}])
    );
    assert_ne!(branch.claude_session_id, original.claude_session_id);
    assert_ne!(checkpoint(&branch, 1), checkpoint(&original, 1));
    t.reply(Ask::new(followup("Another replacement"))).await;
    let forks = t.forks();
    assert_eq!(forks.len(), 2);
    assert_eq!(forks[1].0, branch.claude_session_id);
    assert_eq!(Some(forks[1].1.clone()), checkpoint(&branch, 1));
    assert_eq!(
        t.fake.run(2).users()[0]["message"]["content"],
        json!([{"type": "text", "text": "Another replacement"}])
    );
    assert_eq!(t.record("session-a").unwrap().claude_session_id, forks[1].2);
    assert!(t.removed().is_empty());
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_an_edited_earlier_reply_forks_before_it_and_sends_the_edited_history() {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        expect_interrupt(),
        expect_startup(),
        expect_turn(),
        expect_interrupt(),
    ])
    .await;
    t.reply(Ask::new(first())).await;
    let mut prompt = followup(FOLLOWUP);
    prompt[1] = assistant("REVISED-MARKER");
    t.reply(Ask::new(prompt)).await;
    let forks = t.forks();
    assert_eq!(forks.len(), 1);
    assert_eq!(t.fake.run(1).arg("--resume="), Some(forks[0].2.clone()));
    let content = t.fake.run(1).users()[0]["message"]["content"].to_string();
    assert!(content.contains("REVISED-MARKER"));
    assert!(!content.contains("COPPER-OTTER"));
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_cancelling_one_generate_branch_leaves_another_and_the_primary_session_usable_and_deletes_both_temporary_branches(
) {
    let t = Test::start(vec![
        expect_startup(),
        expect_turn(),
        expect_startup(),
        turn("text-cancel", 0),
        expect_startup(),
        turn("utility", 0),
        expect_interrupt(),
        interrupt("text-cancel"),
        expect_interrupt(),
        turn("text", 1),
        expect_interrupt(),
    ])
    .await;
    t.reply(Ask::new(first())).await;
    let before = t.record("session-a");
    let mut long = t.stream(&Ask::new(followup("Write a long answer")).kind("generate"));
    loop {
        match long.next().await {
            Some(Ok(Event::TextDelta(_))) => break,
            Some(_) => {}
            None => panic!("generate ended before the cancel"),
        }
    }
    let other = t
        .reply(Ask::new(followup("Reply only LILAC-HERON")).kind("generate"))
        .await;
    assert_eq!(text(&other).trim(), "LILAC-HERON");
    assert_eq!(t.forks().len(), 2);
    drop(long);
    t.fake.run(1).closed.wait().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut removed = t.removed();
    removed.sort();
    let mut forks: Vec<String> = t.forks().into_iter().map(|f| f.2).collect();
    forks.sort();
    assert_eq!(removed, forks);
    assert_eq!(t.record("session-a"), before);
    assert!(t.fake.run(0).requests("interrupt").is_empty());
    assert_eq!(
        text(&t.reply(Ask::new(followup(FOLLOWUP))).await).trim(),
        "COPPER-OTTER"
    );
    t.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_compaction_leaves_a_parked_tool_turn_running_and_the_next_primary_request_starts_from_the_summary(
) {
    let tools = replay("tools");
    let parked = tools
        .iter()
        .position(|s| {
            s.kind == Kind::Exchange && !s.input["response"]["response"]["mcp_response"].is_null()
        })
        .unwrap();
    let mut script = tools[..parked].to_vec();
    script.extend([expect_startup(), turn("utility", 0), expect_interrupt()]);
    script.extend(tools[parked..].iter().cloned());
    script.extend([expect_startup(), expect_turn(), expect_interrupt()]);
    let t = Test::start(script).await;
    let mut prompt = tool_prompt();
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
    append(&mut prompt, &parts, output);
    t.reply(Ask::new(prompt.clone()).kind("compaction")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.forks().len(), 1);
    assert_eq!(t.removed(), vec![t.forks()[0].2.clone()]);
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
    append(&mut prompt, &parts, output);
    let parts = t.step(Ask::new(prompt.clone()).tools(lookup_tools())).await;
    assert_eq!(text(&parts), "COPPER-OTTER");
    assert_eq!(t.fake.runs().len(), 2);
    assert!(t.fake.run(0).requests("interrupt").is_empty());
    let compacted = vec![user(
        "<conversation-checkpoint>Only SUMMARY-HERON remains.</conversation-checkpoint>",
    )];
    t.reply(Ask::new(compacted)).await;
    let run = t.fake.run(2);
    assert!(run.arg("--resume=").is_none());
    let input = run.users()[0]["message"]["content"].to_string();
    assert!(input.contains("SUMMARY-HERON"));
    assert!(!input.contains("lookup"));
    t.close().await;
    check(&t.fake);
}

// --- sync.test.ts (claude) ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_catches_up_once_saves_transcript_boundaries_then_continues_without_replaying_history() {
    let t = Test::start(vec![
        expect_startup(),
        turn("sync", 0),
        turn("sync", 1),
        turn("sync", 2),
        expect_interrupt(),
    ])
    .await;
    let mut history = vec![
        user("Give me a marker."),
        assistant("My marker is LILAC-HERON."),
    ];
    async fn ask(t: &Test, history: &mut Vec<Message>, q: &str) {
        history.push(user(q));
        let reply = text(&t.reply(Ask::new(history.clone())).await);
        history.push(assistant(&reply));
    }
    ask(
        &t,
        &mut history,
        "Remember COPPER-OTTER too. Reply only with COPPER-OTTER.",
    )
    .await;
    history.push(user("Read the marker file."));
    history.push(Message {
        role: Role::Assistant,
        parts: vec![Part::tool_call(
            "marker-read",
            "read",
            json!({"path": "marker.txt"}),
        )],
    });
    history.push(Message {
        role: Role::User,
        parts: vec![Part::ToolResult {
            id: "marker-read".into(),
            name: "read".into(),
            content: "LEMON-IBIS".into(),
            is_error: false,
            media: Vec::new(),
        }],
    });
    history.push(assistant("I read the file successfully."));
    ask(
        &t,
        &mut history,
        "List all three markers from our conversation, including the file. Output only the markers.",
    )
    .await;
    ask(&t, &mut history, "Repeat those three markers.").await;
    assert_eq!(t.fake.runs().len(), 1);
    let users = t.fake.run(0).users();
    assert!(users[0]["message"]["content"]
        .to_string()
        .contains("LILAC-HERON"));
    assert!(users[1]["message"]["content"]
        .to_string()
        .contains("LEMON-IBIS"));
    assert!(!users[1]["message"]["content"]
        .to_string()
        .contains("LILAC-HERON"));
    assert_eq!(
        users[2]["message"]["content"],
        json!([{"type": "text", "text": "Repeat those three markers."}])
    );
    let record = t.record("session-a").unwrap();
    assert_eq!(record.history.as_ref().unwrap().len(), 12);
    let uuid = |i: usize| users[i]["uuid"].as_str().map(str::to_string);
    assert_eq!(checkpoint(&record, 2), uuid(0));
    assert_eq!(checkpoint(&record, 8), uuid(1));
    assert_eq!(checkpoint(&record, 10), uuid(2));
    // Catch-up's interior messages claim no transcript boundaries.
    assert_eq!(checkpoint(&record, 5), None);
    t.close().await;
    check(&t.fake);
}

// --- session.test.ts ---

async fn exit_before_a_result(code: i32) {
    let held = expect_turn().hold(&Gate::never());
    let t = Test::start(vec![expect_startup(), held.clone()]).await;
    let s = t.stream(&Ask::new(vec![user("Reply with a marker.")]));
    let reply = tokio::spawn(collect(s));
    held.started.wait().await;
    t.fake.run(0).exit(code).await;
    let parts = reply.await.unwrap();
    let want = if code == 0 {
        "Claude Code stopped before finishing its reply"
    } else {
        "code 1"
    };
    assert!(error(&parts).unwrap().contains(want), "{parts:?}");
    assert!(t.fake.run(0).requests("interrupt").is_empty());
    t.close().await;
    assert_eq!(t.fake.check().unwrap_err(), "1 script steps did not finish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_exit_code_0_before_a_result_reports_failure_and_allows_cleanup() {
    exit_before_a_result(0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_exit_code_1_before_a_result_reports_failure_and_allows_cleanup() {
    exit_before_a_result(1).await;
}

// --- probe.test.ts ---

impl Test {
    fn launch(&self) -> agent::query::Launch {
        agent::query::Launch {
            executable: claude_fake::executable(),
            env: env_for(self.dir.path()),
            ..Default::default()
        }
    }
}

fn context_window(probe: &agent::probe::Probe, model: &str) -> Option<u64> {
    probe.model(model).and_then(|m| m["contextWindow"].as_u64())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_discovers_each_models_capacity_without_sending_a_prompt() {
    let mut steps = replay("startup");
    let context = steps
        .iter()
        .position(|s| s.is_exchange_of("get_context_usage"))
        .unwrap();
    // Claude's auto-compaction window is not the model's capacity.
    steps[context].replies[0]["response"]["response"]["maxTokens"] = json!(200_000);
    let t = Test::start(steps).await;
    let probe = agent::probe::run(&t.launch(), Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(context_window(&probe, "opus"), Some(1_000_000));
    assert_eq!(context_window(&probe, "claude-opus-4-6"), Some(200_000));
    assert!(t.fake.run(0).users().is_empty());
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_keeps_the_login_and_other_limits_when_one_model_reports_an_invalid_capacity() {
    let mut steps = replay("startup");
    let context = steps
        .iter()
        .position(|s| s.is_exchange_of("get_context_usage"))
        .unwrap();
    steps[context].replies[0]["response"]["response"]["rawMaxTokens"] = json!(0);
    let t = Test::start(steps).await;
    let probe = agent::probe::run(&t.launch(), Duration::from_secs(30))
        .await
        .unwrap();
    assert!(agent::probe::is_logged_in(&probe.account));
    assert_eq!(context_window(&probe, "opus"), None);
    assert_eq!(context_window(&probe, "sonnet"), Some(1_000_000));
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_closes_stuck_context_discovery_without_turning_it_into_a_login_failure() {
    let mut steps = replay("startup")[..3].to_vec();
    steps[2] = steps[2].clone().hold(&Gate::never());
    let t = Test::start(steps).await;
    let probe = agent::probe::run(&t.launch(), Duration::from_millis(500))
        .await
        .unwrap();
    assert!(agent::probe::is_logged_in(&probe.account));
    assert_eq!(context_window(&probe, "opus"), None);
    t.fake.run(0).closed.wait().await;
    assert_eq!(t.fake.check().unwrap_err(), "1 script steps did not finish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_times_out_a_stuck_startup_and_closes_the_process() {
    let t = Test::start(vec![expect_startup().hold(&Gate::never())]).await;
    let e = agent::probe::run(&t.launch(), Duration::from_millis(500))
        .await
        .unwrap_err();
    assert!(e.contains("did not start within"), "{e}");
    t.fake.run(0).closed.wait().await;
}

#[test]
fn oc_only_2_1_248_or_later_is_compatible() {
    assert_eq!(agent::probe::version_error(Some("2.1.248")), None);
    assert!(agent::probe::version_error(Some("2.1.247"))
        .unwrap()
        .contains("claude update"));
    assert!(agent::probe::version_error(None)
        .unwrap()
        .contains("claude update"));
    assert_eq!(agent::probe::version_error(Some("2.2.0-beta+1")), None);
}

#[test]
fn oc_only_a_missing_credential_counts_as_logged_out() {
    use agent::probe::is_logged_in;
    assert!(!is_logged_in(
        &json!({"tokenSource": "none", "apiProvider": "firstParty"})
    ));
    assert!(is_logged_in(
        &json!({"tokenSource": "none", "apiKeySource": "ANTHROPIC_API_KEY"})
    ));
    assert!(is_logged_in(
        &json!({"subscriptionType": "Claude Team", "apiProvider": "firstParty"})
    ));
}

// --- status.test.ts ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_finds_claude_when_it_is_installed_after_the_plugin_starts() {
    let t = Test::start(vec![expect_startup()]).await;
    let installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut io = io_for(t.dir.path(), &t.changes);
    let flag = installed.clone();
    io.which = Arc::new(move || {
        flag.load(std::sync::atomic::Ordering::SeqCst)
            .then(claude_fake::executable)
    });
    let registry = Registry::new(io, Timing::default());
    let none = BTreeMap::new();
    assert_eq!(
        registry.refresh(&none).await,
        agent::catalog::State::Missing
    );
    installed.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        registry.refresh(&none).await,
        agent::catalog::State::Ready { .. }
    ));
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_a_reconnect_waits_for_the_startup_check_then_publishes_the_new_login() {
    let hold = Gate::default();
    let at_start = startup("startup-logged-out").hold(&hold);
    let t = Arc::new(Test::start(vec![at_start.clone(), expect_startup()]).await);
    let first = tokio::spawn({
        let t = t.clone();
        async move { t.registry.refresh(&BTreeMap::new()).await }
    });
    at_start.started.wait().await;
    let reconnect = tokio::spawn({
        let t = t.clone();
        async move { t.registry.refresh(&BTreeMap::new()).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(t.fake.runs().len(), 1);
    hold.open();
    first.await.unwrap();
    reconnect.await.unwrap();
    match t.registry.status().borrow().clone() {
        agent::catalog::State::Ready { probe, .. } => {
            assert!(agent::probe::is_logged_in(&probe.account))
        }
        s => panic!("not ready: {s:?}"),
    }
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_reports_a_failed_startup_and_recovers_on_the_next_check() {
    let mut failing = expect_startup();
    failing.exit_code = Some(1);
    let t = Test::start(vec![failing, expect_startup()]).await;
    match t.registry.refresh(&BTreeMap::new()).await {
        agent::catalog::State::Failed(m) => assert!(m.contains("code 1"), "{m}"),
        s => panic!("not failed: {s:?}"),
    }
    assert!(matches!(
        t.registry.refresh(&BTreeMap::new()).await,
        agent::catalog::State::Ready { .. }
    ));
    check(&t.fake);
}

// --- recorder.test.ts ---

#[test]
fn oc_checked_in_recordings_contain_no_thinking_blocks_or_deltas() {
    fn thinking(v: &Value) -> bool {
        match v {
            Value::Object(o) => {
                matches!(
                    o.get("type").and_then(Value::as_str),
                    Some("thinking" | "thinking_delta" | "redacted_thinking" | "signature_delta")
                ) || o.values().any(thinking)
            }
            Value::Array(a) => a.iter().any(thinking),
            _ => false,
        }
    }
    let dir = claude_fake::fixtures().join("recordings");
    for e in std::fs::read_dir(dir).unwrap() {
        let path = e.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            // A boolean, so a failure cannot print the content.
            assert!(!thinking(&v), "{}", path.display());
        }
    }
}

// --- language-model/convert.test.ts ---

/// The recorded first reply, which calls lookup twice.
fn first_tool_reply(tools: &[ToolSpec]) -> Vec<Event> {
    let mut c = agent::convert::Converter::new(tools);
    let mut out = Vec::new();
    for line in claude_fake::load("tools") {
        let m = &line["message"];
        if line["from"] != "claude" || m["type"] == "control_response" {
            continue;
        }
        out.extend(c.push(m).into_iter().map(Result::unwrap));
        if m["type"] == "stream_event" && m["event"]["type"] == "message_stop" {
            break;
        }
    }
    out
}

#[test]
fn oc_calls_to_tools_we_did_not_offer_are_left_to_claude_code_and_do_not_end_the_step() {
    let offered = first_tool_reply(&lookup_tools());
    let n = offered
        .iter()
        .filter(|e| matches!(e, Event::ToolCall { .. }))
        .count();
    assert_eq!(n, 2);
    match offered.last().unwrap() {
        Event::Finish { reason, .. } => {
            assert_eq!(reason.normalized, codesmith_ai::Reason::ToolCalls)
        }
        e => panic!("{e:?}"),
    }
    let without: Vec<ToolSpec> = lookup_tools()
        .into_iter()
        .filter(|t| t.name != "lookup")
        .collect();
    assert!(!first_tool_reply(&without)
        .iter()
        .any(|e| matches!(e, Event::ToolCall { .. } | Event::Finish { .. })));
}

#[test]
fn oc_a_steps_finish_carries_the_plan_usage_claude_code_reported_during_it() {
    let recorded = claude_fake::load("tools")
        .into_iter()
        .find(|l| l["from"] == "claude" && l["message"]["type"] == "rate_limit_event")
        .unwrap();
    match first_tool_reply(&lookup_tools()).last().unwrap() {
        Event::Finish { raw_usage, .. } => assert_eq!(
            raw_usage.as_ref().unwrap()["claude-agent"]["rateLimit"],
            recorded["message"]["rate_limit_info"]
        ),
        e => panic!("{e:?}"),
    }
    let mut c = agent::convert::Converter::new(&[]);
    match c
        .push(&claude_fake::result("text", 0))
        .pop()
        .unwrap()
        .unwrap()
    {
        Event::Finish { raw_usage, .. } => assert_eq!(raw_usage, None),
        e => panic!("{e:?}"),
    }
}

#[test]
fn oc_utility_calls_keep_system_instructions_separate_from_conversation_content_and_omit_thinking()
{
    assert_eq!(
        agent::query::utility("Summarize this conversation.\n\nUse one sentence."),
        agent::query::System::Custom("Summarize this conversation.\n\nUse one sentence.".into())
    );
    let prompt = history::from_messages(&[
        user("First question"),
        Message {
            role: Role::Assistant,
            parts: vec![
                Part::Reasoning("Private reasoning".into()),
                Part::text("First answer"),
            ],
        },
        user("Follow-up"),
    ]);
    let t = |s: &str| json!({"type": "text", "text": s});
    assert_eq!(
        history::content(&prompt).unwrap(),
        vec![
            t("<user>"),
            t("First question"),
            t("</user>"),
            t("<assistant>"),
            t("First answer"),
            t("</assistant>"),
            t("<user>"),
            t("Follow-up"),
            t("</user>"),
        ]
    );
}

#[test]
fn oc_preserves_byte_base64_url_image_and_pdf_attachments() {
    use codesmith_ai::Media;
    let prompt = history::from_messages(&[Message {
        role: Role::User,
        parts: vec![
            Part::text("/compact @README.md are literal text"),
            Part::Media(Media::base64("image/png", "AQID")),
            Part::Media(Media::base64("application/pdf", "cGRm")),
            Part::Media(Media::url("image/jpeg", "https://example.com/photo.jpg")),
        ],
    }]);
    assert_eq!(
        history::content(&prompt).unwrap(),
        vec![
            json!({"type": "text", "text": "/compact @README.md are literal text"}),
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AQID"}}),
            json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "cGRm"}}),
            json!({"type": "image", "source": {"type": "url", "url": "https://example.com/photo.jpg"}}),
        ]
    );
}

// --- language-model/error.test.ts ---

fn assistant_error() -> Value {
    claude_fake::load("billing-error")
        .into_iter()
        .find(|l| {
            l["from"] == "claude"
                && l["message"]["type"] == "assistant"
                && !l["message"]["error"].is_null()
        })
        .unwrap()["message"]
        .clone()
}

#[test]
fn oc_a_recorded_failed_turn_ends_in_an_error_after_its_text_without_a_finish() {
    let mut c = agent::convert::Converter::new(&[]);
    let mut out = Vec::new();
    for line in claude_fake::load("text-cancel") {
        let m = &line["message"];
        if line["from"] != "claude" || m["type"] == "control_response" {
            continue;
        }
        out.extend(c.push(m));
        if m["type"] == "result" {
            break;
        }
    }
    assert!(out.iter().any(|e| matches!(e, Ok(Event::TextDelta(_)))));
    assert!(out.last().unwrap().is_err());
    assert!(!out.iter().any(|e| matches!(e, Ok(Event::Finish { .. }))));
}

#[test]
fn oc_a_recorded_billing_failure_is_an_error_not_answer_text() {
    let mut c = agent::convert::Converter::new(&[]);
    assert!(c.push(&assistant_error()).is_empty());
    let out = c.push(&claude_fake::result("billing-error", 0));
    assert_eq!(out.len(), 1);
    assert!(out[0]
        .as_ref()
        .unwrap_err()
        .to_string()
        .contains("extra usage"));
}

#[test]
fn oc_a_rejected_weekly_window_gives_its_reset_date_while_allowed_windows_do_not_fail() {
    let info = json!({"type": "rate_limit_event", "uuid": "u", "session_id": "s",
        "rate_limit_info": {"status": "rejected", "rateLimitType": "seven_day_sonnet", "resetsAt": 1792800000}});
    let mut c = agent::convert::Converter::new(&[]);
    assert!(c.push(&info).is_empty());
    let mut limited = assistant_error();
    limited["error"] = json!("rate_limit");
    assert!(c.push(&limited).is_empty());
    let mut failed = claude_fake::result("text", 0);
    failed["subtype"] = json!("success");
    failed["is_error"] = json!(true);
    failed["result"] = json!("Weekly limit reached.");
    let e = c.push(&failed).pop().unwrap().unwrap_err().to_string();
    let reset = chrono::DateTime::from_timestamp(1792800000, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    assert!(e.contains(&reset), "{e}");
    for status in ["allowed", "allowed_warning", "rejected"] {
        let mut ok = agent::convert::Converter::new(&[]);
        let mut i = info.clone();
        i["rate_limit_info"]["status"] = json!(status);
        i["rate_limit_info"]["overageStatus"] = json!("allowed");
        ok.push(&i);
        assert!(matches!(
            ok.push(&claude_fake::result("text", 0)).last(),
            Some(Ok(Event::Finish { .. }))
        ));
    }
    let mut recovered = agent::convert::Converter::new(&[]);
    recovered.push(&info);
    recovered.push(&json!({"type": "rate_limit_event",
        "rate_limit_info": {"status": "allowed", "overageStatus": "rejected"}}));
    failed["api_error_status"] = json!(429);
    let e = recovered
        .push(&failed)
        .pop()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(!e.contains("Resets at"), "{e}");
}

#[test]
fn oc_empty_successes_after_api_retries_report_the_terminal_http_status() {
    for status in [500, 529] {
        let mut c = agent::convert::Converter::new(&[]);
        let mut r = claude_fake::result("text", 0);
        r["subtype"] = json!("success");
        r["is_error"] = json!(false);
        r["result"] = json!("");
        r["api_error_status"] = json!(status);
        let out = c.push(&r);
        let e = out[0].as_ref().unwrap_err().to_string();
        assert!(
            e.contains(&status.to_string()) && e.contains("retries"),
            "{e}"
        );
        assert!(!out.iter().any(|e| matches!(e, Ok(Event::Finish { .. }))));
    }
    let mut r = claude_fake::result("text", 0);
    r["subtype"] = json!("success");
    r["result"] = json!("");
    r["is_error"] = json!(false);
    r["api_error_status"] = Value::Null;
    let mut c = agent::convert::Converter::new(&[]);
    assert!(matches!(c.push(&r).last(), Some(Ok(Event::Finish { .. }))));
}

// --- tools/tools.test.ts ---

#[test]
fn oc_names_round_trip_within_claudes_limit_including_names_that_need_sanitizing() {
    use agent::tools::{original, short, PREFIX};
    let names = [
        "read".to_string(),
        "resource_".repeat(15),
        "resource_".repeat(15) + "b",
        "mcp.example/search".to_string(),
    ];
    let tools: Vec<ToolSpec> = names
        .iter()
        .map(|n| tool(n, "", json!({"type": "object"})))
        .collect();
    let mut seen = std::collections::HashSet::new();
    for t in &tools {
        let name = format!("{PREFIX}{}", short(&t.name));
        assert!(name.len() <= 64, "{name}");
        assert!(name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
        assert_eq!(original(&name, &tools), Some(t.name.as_str()));
        seen.insert(name);
    }
    assert_eq!(seen.len(), tools.len());
    assert_eq!(original("Bash", &tools), None);
}

#[test]
fn oc_long_subagent_descriptions_retain_the_actual_opencode_agent_list() {
    let d = agent::tools::description(&tool(
        "subagent",
        &("Long instructions. ".repeat(200)
            + "\n\nAvailable agents:\n- explore: Explore the repository.\n- general: General tasks."),
        json!({}),
    ));
    assert!(d.chars().count() <= 2048);
    assert!(d.starts_with("Available agents:"));
    assert!(d.contains("- general:"));
}

#[test]
fn oc_execution_failures_and_denied_permissions_stay_errors_and_json_stays_intact() {
    let ok = agent::tools::convert(r#"{"items":[1,{"enabled":true}]}"#, false, &[]).unwrap();
    assert_eq!(
        ok.mcp,
        json!({"content": [{"type": "text", "text": "{\"items\":[1,{\"enabled\":true}]}"}], "isError": false})
    );
    for text in ["failed", "{\"error\":\"failed\"}", "denied"] {
        assert_eq!(
            agent::tools::convert(text, true, &[]).unwrap().mcp["isError"],
            true
        );
    }
}

#[test]
fn oc_tool_media_remains_content_blocks_and_does_not_consume_a_newly_queued_user_message() {
    use codesmith_ai::Media;
    let result = |id: &str, text: &str, media: Vec<Media>| Part::ToolResult {
        id: id.into(),
        name: "read".into(),
        content: text.into(),
        is_error: false,
        media,
    };
    let prompt = history::from_messages(&[
        Message {
            role: Role::User,
            parts: vec![
                result("a", "first", vec![]),
                result(
                    "b",
                    "second",
                    vec![
                        Media::base64("image/png", "aW1hZ2U="),
                        Media::base64("application/pdf", "cGRm"),
                    ],
                ),
            ],
        },
        user("New instructions"),
    ]);
    let c = agent::tools::collect(&prompt, 0, &Default::default()).unwrap();
    assert_eq!(c.end, 1);
    assert_eq!(
        c.results["a"].mcp["content"],
        json!([{"type": "text", "text": "first"}])
    );
    assert_eq!(
        c.results["b"].replacement,
        Some(vec![
            json!({"type": "text", "text": "second"}),
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aW1hZ2U="}}),
            json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "cGRm"}}),
        ])
    );
    let image =
        agent::tools::convert("", false, &[Media::base64("image/png", "aW1hZ2U=")]).unwrap();
    assert_eq!(
        image.mcp["content"][1],
        json!({"type": "image", "mimeType": "image/png", "data": "aW1hZ2U="})
    );
    assert_eq!(image.replacement, None);
}

// --- sync/sync.test.ts ---

use history::{Decision, Msg, Piece, Who};

fn past() -> Vec<Msg> {
    vec![Msg::user("First marker"), Msg::assistant("COPPER-OTTER")]
}

fn record_of(prompt: &[Msg]) -> agent::store::Record {
    agent::store::Record {
        claude_session_id: "claude-a".into(),
        history: Some(
            history::groups(prompt, &[])
                .into_iter()
                .map(|g| agent::store::Checkpoint {
                    hash: g.hash,
                    claude_message_id: None,
                })
                .collect(),
        ),
        interrupted: None,
    }
}

fn cat(parts: &[&[Msg]]) -> Vec<Msg> {
    parts.iter().flat_map(|p| p.iter().cloned()).collect()
}

#[test]
fn oc_an_interrupted_reply_continues_after_the_submitted_input_without_hiding_earlier_edits_or_missed_turns(
) {
    let input = cat(&[&past(), &[Msg::user("List animals")]]);
    let mut saved = record_of(&input);
    saved.interrupted = Some(true);
    let d = |p: Vec<Msg>| history::decide(&p, Some(&saved));
    assert_eq!(
        d(cat(&[
            &input,
            &[Msg::assistant("1. Ot"), Msg::user("Stop")]
        ])),
        Decision::Continue(4)
    );
    // Without a saved partial reply the next user message stays apart.
    assert_eq!(
        d(cat(&[&input, &[Msg::user("Stop")]])),
        Decision::Continue(3)
    );
    assert_eq!(
        d(cat(&[
            &input,
            &[
                Msg::assistant("1. Ot"),
                Msg::user("Other model"),
                Msg::assistant("New answer"),
                Msg::user("Back"),
            ]
        ])),
        Decision::CatchUp(4)
    );
    assert_eq!(
        d(vec![
            past()[0].clone(),
            Msg::assistant("Edited"),
            input[2].clone(),
            Msg::user("Stop"),
        ]),
        Decision::Changed
    );
}

fn calls_msg(calls: &[(&str, &str)]) -> Msg {
    Msg {
        who: Who::Assistant,
        parts: calls
            .iter()
            .map(|(id, path)| Piece::Call {
                id: id.to_string(),
                name: "read".into(),
                input: json!({"path": path}),
            })
            .collect(),
    }
}

fn result_msg(id: &str, value: &str) -> Msg {
    Msg {
        who: Who::Tool,
        parts: vec![Piece::Result {
            id: id.into(),
            name: "read".into(),
            content: value.into(),
            is_error: false,
            media: vec![],
        }],
    }
}

#[test]
fn oc_branches_only_at_a_matching_checkpoint_never_midway_through_a_group_of_tool_calls() {
    let history = cat(&[
        &past(),
        &[
            Msg::user("Read two files"),
            calls_msg(&[("a", "a"), ("b", "b")]),
            result_msg("a", "A"),
            result_msg("b", "B"),
            Msg::assistant("Read both"),
        ],
    ]);
    let saved = agent::store::Record {
        claude_session_id: "source".into(),
        history: Some(
            history::groups(&history, &[])
                .into_iter()
                .enumerate()
                .map(|(i, g)| agent::store::Checkpoint {
                    hash: g.hash,
                    claude_message_id: Some(format!("uuid-{i}")),
                })
                .collect(),
        ),
        interrupted: None,
    };
    let at = |p: &[Msg]| history::boundary(p, Some(&saved)).map(|b| b.message_id);
    assert_eq!(at(&history[..1]), None);
    assert_eq!(at(&history[..3]).as_deref(), Some("uuid-1"));
    assert_eq!(at(&history[..4]).as_deref(), Some("uuid-2"));
    assert_eq!(at(&history[..5]).as_deref(), Some("uuid-2"));
    assert_eq!(at(&history).as_deref(), Some("uuid-6"));
    let edited = cat(&[&[Msg::user("Edited source")], &history[1..]]);
    assert_eq!(at(&edited), None);
    let mut no_ids = saved.clone();
    for c in no_ids.history.as_mut().unwrap() {
        c.claude_message_id = None;
    }
    assert_eq!(history::boundary(&history, Some(&no_ids)), None);
}

#[test]
fn oc_chooses_all_four_cases_and_never_resumes_a_record_without_history() {
    let record = record_of(&past());
    let d = |p: Vec<Msg>| history::decide(&p, Some(&record));
    assert_eq!(
        d(cat(&[&past(), &[Msg::user("Next")]])),
        Decision::Continue(2)
    );
    assert_eq!(
        d(cat(&[
            &past(),
            &[
                Msg::user("Other model"),
                Msg::assistant("LILAC-HERON"),
                Msg::user("Next")
            ]
        ])),
        Decision::CatchUp(2)
    );
    assert_eq!(
        d(vec![
            past()[0].clone(),
            Msg::assistant("Edited"),
            Msg::user("Next")
        ]),
        Decision::Changed
    );
    assert_eq!(
        d(vec![past()[0].clone(), Msg::user("Undo")]),
        Decision::Fresh
    );
    assert_eq!(d(vec![past()[0].clone()]), Decision::Changed);
    assert_eq!(
        d(vec![Msg::user("Compacted summary"), Msg::user("Next")]),
        Decision::Fresh
    );
    let bare = agent::store::Record {
        claude_session_id: "old".into(),
        history: None,
        interrupted: None,
    };
    assert_eq!(
        history::decide(&cat(&[&past(), &[Msg::user("Next")]]), Some(&bare)),
        Decision::Fresh
    );
}

#[test]
fn oc_ignores_thinking_metadata_and_reordered_user_notes_but_notices_changed_visible_text() {
    let hashes = |p: &[Msg]| -> Vec<String> {
        history::groups(p, &[])
            .into_iter()
            .map(|g| g.hash)
            .collect()
    };
    let original = cat(&[&[Msg::user("Plan reminder")], &past()]);
    let changed = vec![
        past()[0].clone(),
        Msg::user("Plan reminder"),
        Msg {
            who: Who::Assistant,
            parts: vec![
                Piece::Reasoning("Private reasoning".into()),
                Piece::Text("COPPER-".into()),
                Piece::Text("OTTER".into()),
            ],
        },
    ];
    assert_eq!(hashes(&changed), hashes(&original));
    assert_ne!(
        hashes(&cat(&[&[Msg::user("Changed reminder")], &past()])),
        hashes(&original)
    );
}

#[test]
fn oc_compares_tool_inputs_independent_of_key_order_and_keeps_tool_images_with_their_result() {
    use codesmith_ai::Media;
    let call = |input: Value| Msg {
        who: Who::Assistant,
        parts: vec![Piece::Call {
            id: "call-a".into(),
            name: "read".into(),
            input,
        }],
    };
    let res = |text: &str| Msg {
        who: Who::Tool,
        parts: vec![Piece::Result {
            id: "call-a".into(),
            name: "read".into(),
            content: text.into(),
            is_error: false,
            media: vec![Media::base64("image/png", "AQI=")],
        }],
    };
    let mut prompt = vec![
        call(serde_json::from_str(r#"{"path":"image.png","offset":1}"#).unwrap()),
        res("Image follows"),
    ];
    let groups = history::groups(&prompt, &[]);
    assert_eq!(
        groups.iter().map(|g| g.who).collect::<Vec<_>>(),
        vec![Who::Assistant, Who::Tool]
    );
    prompt[0] = call(serde_json::from_str(r#"{"offset":1,"path":"image.png"}"#).unwrap());
    assert_eq!(history::groups(&prompt, &[]), groups);
    prompt[1] = res("Edited output");
    assert_ne!(history::groups(&prompt, &[])[1].hash, groups[1].hash);
}

#[test]
fn oc_catch_up_includes_only_missed_turns_and_leaves_the_current_request_outside_the_checkpoint() {
    let prompt = cat(&[
        &past(),
        &[
            Msg::user("Other model"),
            Msg::assistant("LILAC-HERON"),
            Msg::user("What happened?"),
        ],
    ]);
    let blocks = history::catch_up(&prompt, 2).unwrap();
    let text = serde_json::to_string(&blocks).unwrap();
    assert!(!text.contains("COPPER-OTTER"));
    assert!(text.contains("LILAC-HERON"));
    assert_eq!(
        blocks[blocks.len() - 2],
        json!({"type": "text", "text": "</conversation-checkpoint>"})
    );
    assert_eq!(
        blocks[blocks.len() - 1],
        json!({"type": "text", "text": "What happened?"})
    );
    assert_eq!(
        history::catch_up(&prompt, 4).unwrap(),
        vec![json!({"type": "text", "text": "What happened?"})]
    );
}

#[test]
fn oc_catch_up_preserves_tool_calls_results_images_and_pdfs_excluding_reasoning() {
    use codesmith_ai::Media;
    let blocks = history::catch_up(
        &[
            Msg::user("Read the files"),
            Msg {
                who: Who::Assistant,
                parts: vec![
                    Piece::Reasoning("PRIVATE".into()),
                    Piece::Call {
                        id: "a".into(),
                        name: "read".into(),
                        input: json!({"path": "marker.txt"}),
                    },
                ],
            },
            Msg {
                who: Who::Tool,
                parts: vec![Piece::Result {
                    id: "a".into(),
                    name: "read".into(),
                    content: "LEMON-IBIS\nSecond line".into(),
                    is_error: false,
                    media: vec![
                        Media::base64("image/png", "AQI="),
                        Media::base64("application/pdf", "AwQ="),
                    ],
                }],
            },
            Msg::user("Continue"),
        ],
        0,
    )
    .unwrap();
    let text = serde_json::to_string(&blocks).unwrap();
    assert!(text.contains("marker.txt"));
    assert!(text.contains("LEMON-IBIS"));
    assert!(!text.contains("PRIVATE"));
    assert!(blocks.iter().any(|b| b["type"] == "image"));
    assert!(blocks.iter().any(|b| b["type"] == "document"));
    assert_eq!(
        blocks.last().unwrap(),
        &json!({"type": "text", "text": "Continue"})
    );
}

// --- plugin.test.ts ---

use agent::catalog::{Catalog, State};
use codesmith_core::providers::claude_agent as provider;

/// models.dev listing `model` at `provider` with OpenCode's test price.
fn priced(provider_id: &str, model: &str) -> codesmith_core::catalog::Catalog {
    let text = json!({provider_id: {
        "id": provider_id, "name": provider_id, "env": [], "npm": "@ai-sdk/anthropic",
        "models": {model: {
            "id": model, "name": model, "release_date": "2026-01-01",
            "cost": {"input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75},
            "limit": {"context": 200000, "output": 64000},
        }},
    }})
    .to_string();
    codesmith_core::catalog::Catalog::from_text(text.into()).expect("a catalog")
}

fn test_price() -> codesmith_core::config::Cost {
    codesmith_core::config::Cost {
        input: 3.0,
        output: 15.0,
        cache_read: 0.3,
        cache_write: 3.75,
    }
}

/// Connect (the login check) with `account`, then price sonnet.
async fn sonnet_cost(
    account: Value,
    shell: Option<(&str, &str)>,
    prices: &codesmith_core::catalog::Catalog,
) -> Option<codesmith_core::config::Cost> {
    let mut on_connect = expect_startup();
    on_connect.reply["account"] = account;
    let t = Test::start(vec![on_connect]).await;
    let mut io = io_for(t.dir.path(), &t.changes);
    if let Some((k, v)) = shell {
        let mut env = env_for(t.dir.path());
        env.insert(k.into(), v.into());
        io.env = Arc::new(move || env.clone());
    }
    let registry = Registry::new(io, Timing::default());
    registry.refresh(&BTreeMap::new()).await;
    check(&t.fake);
    provider::cost(&registry.catalog(), "sonnet", prices)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_starts_without_models_or_a_claude_run_and_connecting_fills_and_saves_the_list() {
    let t = Test::start(vec![expect_startup()]).await;
    assert!(t.registry.catalog().models.is_empty());
    assert!(t.fake.runs().is_empty());
    let state = t.registry.refresh(&BTreeMap::new()).await;
    assert!(matches!(state, State::Ready { .. }));
    let saved = t.registry.catalog();
    assert!(!saved.models.is_empty());
    assert!(saved.model("default").is_none());
    let variants = provider::variants(&saved, "sonnet");
    let high = variants.iter().find(|v| v.0 == "high").unwrap();
    assert_eq!(high.1.options.str("effort"), Some("high"));
    assert_eq!(
        saved.model("sonnet").unwrap().context_window,
        Some(1_000_000)
    );
    // A subscription: no cost.
    let prices = priced("anthropic", "claude-sonnet-5-5");
    for m in &saved.models {
        assert_eq!(
            provider::cost(&saved, &m.id, &prices),
            Some(Default::default())
        );
    }
    // Saved, so a restart lists them without running claude.
    assert_eq!(t.restart().catalog(), saved);
    check(&t.fake);
}

#[test]
fn oc_starts_with_the_saved_list_and_its_prices_without_running_claude() {
    let opus = "us.anthropic.claude-opus-4-6-v1[1m]";
    let saved = Catalog {
        price_source: Some("amazon-bedrock".into()),
        models: vec![agent::catalog::Model {
            id: opus.into(),
            name: "Opus".into(),
            efforts: vec!["high".into()],
            context_window: Some(1_000_000),
            resolved_model: Some(opus.into()),
        }],
    };
    let prices = priced("amazon-bedrock", "us.anthropic.claude-opus-4-6-v1");
    assert_eq!(provider::cost(&saved, opus, &prices), Some(test_price()));
    assert_eq!(saved.model(opus).unwrap().context_window, Some(1_000_000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_shows_no_cost_for_a_subscription() {
    let prices = priced("anthropic", "claude-sonnet-5-5");
    let cost = sonnet_cost(
        json!({"subscriptionType": "Claude Team", "apiProvider": "firstParty"}),
        None,
        &prices,
    )
    .await;
    assert_eq!(cost, Some(Default::default()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_shows_anthropics_prices_for_an_api_key_login() {
    let prices = priced("anthropic", "claude-sonnet-5-5");
    let cost = sonnet_cost(
        json!({"apiKeySource": "ANTHROPIC_API_KEY", "apiProvider": "firstParty"}),
        None,
        &prices,
    )
    .await;
    assert_eq!(cost, Some(test_price()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_shows_no_cost_through_a_custom_base_url_which_bills_in_its_own_way() {
    let prices = priced("anthropic", "claude-sonnet-5-5");
    let cost = sonnet_cost(
        json!({"apiKeySource": "ANTHROPIC_API_KEY", "apiProvider": "firstParty"}),
        Some(("ANTHROPIC_BASE_URL", "https://gateway.example")),
        &prices,
    )
    .await;
    assert_eq!(cost, Some(Default::default()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_shows_a_cloud_account_its_clouds_prices() {
    let prices = priced("google-vertex", "claude-sonnet-5-5@default");
    let cost = sonnet_cost(json!({"apiProvider": "vertex"}), None, &prices).await;
    assert_eq!(cost, Some(test_price()));
}

#[test]
fn oc_ignores_a_malformed_saved_list_and_checks_claude_when_already_connected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(Catalog::file(dir.path()), r#"[{"id": 1}]"#).unwrap();
    assert_eq!(Catalog::load(dir.path()), Catalog::default());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_binds_chats_to_plugin_storage_and_checks_context_sizes_for_models_a_chat_reports_without_one(
) {
    let t = Test::with(
        vec![expect_startup(), expect_turn(), expect_interrupt()],
        true,
        Timing::default(),
    )
    .await;
    let mut io = io_for(t.dir.path(), &t.changes);
    io.recheck = true;
    let registry = Registry::new(io, Timing::default());
    let parts = t.reply_with(&registry, Ask::new(first())).await;
    assert_eq!(text(&parts), "COPPER-OTTER");
    assert!(registry.records().get("session-a").is_some());
    // The chat's start lists models without context sizes: one full check.
    let mut found = false;
    for _ in 0..200 {
        if registry
            .catalog()
            .model("sonnet")
            .is_some_and(|m| m.context_window == Some(1_000_000))
        {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(found);
    registry.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_connecting_without_claude_installed_asks_the_user_to_install_it() {
    let t = Test::start(vec![]).await;
    let mut io = io_for(t.dir.path(), &t.changes);
    io.which = Arc::new(|| None);
    let registry = Registry::new(io, Timing::default());
    let state = registry.refresh(&BTreeMap::new()).await;
    assert!(state
        .problem()
        .unwrap()
        .contains("https://code.claude.com/docs/en/setup"));
    assert!(registry.catalog().models.is_empty());
    assert!(t.fake.runs().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_connecting_while_logged_out_of_claude_asks_the_user_to_sign_in() {
    let t = Test::start(vec![startup("startup-logged-out")]).await;
    let state = t.registry.refresh(&BTreeMap::new()).await;
    assert!(state.problem().unwrap().contains("claude auth login"));
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_connecting_with_an_old_claude_still_lists_its_models() {
    let mut old = expect_startup();
    old.reply["claude_code_version"] = json!("2.1.247");
    let t = Test::start(vec![old]).await;
    let state = t.registry.refresh(&BTreeMap::new()).await;
    assert_eq!(state.problem(), None);
    assert!(!t.registry.catalog().models.is_empty());
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_passes_the_configured_connection_to_claude_code_for_the_login_check_and_for_chats() {
    let t = Test::with(
        vec![expect_startup(), expect_turn(), expect_interrupt()],
        true,
        Timing::default(),
    )
    .await;
    let mut env = env_for(t.dir.path());
    env.insert(
        "ANTHROPIC_BASE_URL".into(),
        "https://from-shell.example".into(),
    );
    let mut io = io_for(t.dir.path(), &t.changes);
    io.env = Arc::new(move || env.clone());
    let registry = Registry::new(io, Timing::default());
    // The provider's config, as `providers.claude-agent` in codesmith.json.
    let config: codesmith_core::config::ProviderConfig = serde_json::from_value(json!({
        "settings": {
            "baseURL": "https://gateway.example",
            "apiKey": "never-passed",
            "authToken": "never-passed",
            "headerTimeout": 60000,
            "chunkTimeout": 600000,
        },
        "headers": {"X-Team": "core"},
        "body": {"metadata": {"user_id": "dan"}},
    }))
    .unwrap();
    let connection = provider::connection(&config);
    registry.refresh(&connection).await;
    let check_env = t.fake.run(0).env.clone();
    assert_eq!(check_env["ANTHROPIC_BASE_URL"], "https://gateway.example");
    assert_eq!(check_env["API_TIMEOUT_MS"], "60000");
    assert_eq!(check_env["CLAUDE_STREAM_IDLE_TIMEOUT_MS"], "600000");
    assert_eq!(check_env["ANTHROPIC_CUSTOM_HEADERS"], "X-Team: core");
    assert_eq!(
        check_env["CLAUDE_CODE_EXTRA_BODY"],
        json!({"metadata": {"user_id": "dan"}}).to_string()
    );
    // CodeSmith passes no credential for Claude Code (adapted: OpenCode
    // maps `apiKey` and `authToken`).
    assert!(!check_env.values().any(|v| v == "never-passed"));
    // A chat gets the same connection, and only the configured headers,
    // not CodeSmith's own.
    let p = Provider {
        protocol: Protocol::ClaudeAgent,
        name: agent::ID.into(),
        base_url: "https://gateway.example".into(),
        headers: vec![("X-Team".into(), "core".into())],
        ..Default::default()
    };
    let mut req = Request {
        model: "haiku".into(),
        system: SYSTEM.into(),
        messages: first(),
        headers: vec![
            (agent::SESSION_HEADER.into(), "session-a".into()),
            ("User-Agent".into(), "codesmith/test".into()),
        ],
        ..Default::default()
    };
    req.body = serde_json::from_value(json!({"metadata": {"user_id": "dan"}})).unwrap();
    req.options.set(agent::DIRECTORY_OPTION, json!(t.work()));
    req.options.set(agent::HEADER_TIMEOUT_OPTION, json!(60000));
    let parts = collect(agent::stream_with(
        &registry,
        agent::agent_request(&p, &req).unwrap(),
    ))
    .await;
    assert_eq!(text(&parts), "COPPER-OTTER");
    let chat = t
        .fake
        .runs()
        .into_iter()
        .find(|r| r.args.iter().any(|a| a == "--include-partial-messages"))
        .unwrap();
    assert_eq!(chat.env["ANTHROPIC_BASE_URL"], "https://gateway.example");
    assert_eq!(chat.env["ANTHROPIC_CUSTOM_HEADERS"], "X-Team: core");
    assert_eq!(chat.env["API_TIMEOUT_MS"], "60000");
    // CodeSmith names itself, and never hands over its server token.
    assert!(chat.env["CLAUDE_AGENT_SDK_CLIENT_APP"].starts_with("codesmith/"));
    assert!(!chat.env.contains_key("CODESMITH_SERVER_TOKEN"));
    registry.close().await;
    check(&t.fake);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oc_leaves_the_users_environment_alone_when_nothing_is_configured() {
    let t = Test::start(vec![expect_startup()]).await;
    let mut env = env_for(t.dir.path());
    env.insert(
        "ANTHROPIC_BASE_URL".into(),
        "https://from-shell.example".into(),
    );
    env.insert("CODESMITH_SERVER_TOKEN".into(), "secret".into());
    let mut io = io_for(t.dir.path(), &t.changes);
    io.env = Arc::new(move || env.clone());
    let registry = Registry::new(io, Timing::default());
    let none = provider::connection(&Default::default());
    assert!(none.is_empty());
    registry.refresh(&none).await;
    let env = t.fake.run(0).env.clone();
    assert_eq!(env["ANTHROPIC_BASE_URL"], "https://from-shell.example");
    assert!(!env.contains_key("ANTHROPIC_CUSTOM_HEADERS"));
    assert!(!env.contains_key("CLAUDE_CODE_EXTRA_BODY"));
    assert!(!env.contains_key("CODESMITH_SERVER_TOKEN"));
    check(&t.fake);
}

#[test]
fn oc_never_retries_a_turn_since_claude_code_already_retries_api_failures() {
    use codesmith_core::retry::Policy;
    let overloaded = Error::Status {
        status: 529,
        body: "overloaded".into(),
    };
    let now = std::time::Instant::now();
    assert!(Policy::default().next(&overloaded, now).is_some());
    assert!(Policy::default()
        .for_protocol(Protocol::ClaudeAgent)
        .next(&overloaded, now)
        .is_none());
    assert!(Policy::one_shot()
        .for_protocol(Protocol::ClaudeAgent)
        .next(&Error::Transport("refused".into()), now)
        .is_none());
}
