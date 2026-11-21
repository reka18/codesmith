//! Ports of OpenCode's command tests (`packages/core/test/config/`
//! `command.test.ts` and `command-subagent.test.ts`, MIT): command files
//! loaded in config order, `` !`cmd` `` interpolation, and commands that
//! run in a child session, against a scripted OpenAI-compatible model.
//! Each test names the OpenCode test it ports; see
//! `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: OpenCode runs a subagent command's child in the background and
//! reports it to the parent later; here the child runs to completion and
//! its report joins the parent as an exchange (`command.rs` documents the
//! difference). Switching the session's agent and model for a command is
//! the front end's, through `Command::switches`.

use codesmith_ai::{Part, Role};
use codesmith_core::command::{self, Command};
use codesmith_core::config::{self, Loaded, Sources};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use codesmith_core::store::Store;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, Once};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// --- A scripted model: replies "Review complete" to every request ---

type Log = Arc<Mutex<VecDeque<Value>>>;

async fn read_body(s: &mut tokio::net::TcpStream) -> Value {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = s.read(&mut tmp).await.unwrap();
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(i) = text.find("\r\n\r\n") {
            let len: usize = text[..i]
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if buf.len() >= i + 4 + len {
                return serde_json::from_slice(&buf[i + 4..i + 4 + len]).unwrap();
            }
        }
    }
}

async fn model(log: Log) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let body = read_body(&mut s).await;
                log.lock().unwrap().push_back(body);
                let chunk = json!({"choices": [{"delta": {"content": "Review complete"}, "finish_reason": "stop"}]});
                let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

fn isolate() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // A new folder with a random name, created exclusively (0700),
        // not a predictable one another local user could create first.
        let dir = tempfile::Builder::new()
            .prefix("codesmith-oc-commands-")
            .tempdir()
            .unwrap()
            .keep();
        std::env::set_var("XDG_DATA_HOME", dir);
    });
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A project whose config (inline, as `$CODESMITH_CONFIG_CONTENT` would
/// be) points `test/*` at the scripted model and defines a `reviewer`
/// subagent on `test/child`; `extra` adds to it, and the project's
/// `.codesmith/` folder is a config layer.
struct Project {
    dir: tempfile::TempDir,
    data: tempfile::TempDir,
    log: Log,
    loaded: Loaded,
}

async fn project(extra: Value, files: &[(&str, &str)]) -> Project {
    isolate();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    for (p, t) in files {
        write(&root.join(p), t);
    }
    let log: Log = Arc::default();
    let base = model(log.clone()).await;
    let mut cfg = json!({
        "model": "test/parent",
        "snapshots": false,
        // A hosted provider, as in OpenCode, though it listens on loopback.
        "providers": {"test": {"protocol": "openai-chat", "local": false, "settings": {"baseURL": base, "apiKey": "x"}}},
        "agents": {"reviewer": {"mode": "subagent", "model": "test/child"}},
    });
    for (k, v) in extra.as_object().unwrap() {
        cfg[k] = v.clone();
    }
    let global = tempfile::tempdir().unwrap();
    let sources = Sources {
        global_dir: global.path().to_path_buf(),
        file: None,
        content: Some(cfg.to_string()),
        project: true,
        trust: codesmith_core::trust::Mode::All,
    };
    let loaded = config::load_from(&root, &sources).unwrap();
    Project {
        dir,
        data: tempfile::tempdir().unwrap(),
        log,
        loaded,
    }
}

impl Project {
    fn store(&self) -> Store {
        Store::open(&self.data.path().join("sessions.db")).unwrap()
    }
    fn commands(&self) -> BTreeMap<String, Command> {
        command::discover(
            command::builtins(&self.loaded.root),
            &[],
            &self.loaded.config.commands,
        )
    }
    /// A stored session on `agent` and `model` (the parent's by default).
    fn session(&self, agent: Option<String>, model: Option<String>) -> Session {
        let opts = Options {
            agent: agent.or(Some("build".into())),
            model,
            resume: None,
            title: Some("Parent session".into()),
            parent: None,
            id: None,
            questioner: None,
            mcp: None,
        };
        let root = self.dir.path().canonicalize().unwrap();
        Session::new(
            &self.loaded,
            root,
            opts,
            Arc::new(Fixed(Reply::Once)),
            Some(self.store()),
        )
        .unwrap()
    }
    fn requests(&self) -> Vec<Value> {
        self.log.lock().unwrap().iter().cloned().collect()
    }
}

fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| match &m["content"] {
            Value::String(s) => s.clone(),
            v => v[0]["text"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

const TEMPLATE: &str = "Review $ARGUMENTS: !`printf ready`";

/// Run `/review changes` the way a front end does: switch the session
/// first when the command says so, then run it.
async fn run_review(p: &Project) -> Session {
    let all = p.commands();
    let (name, args) = command::find_invocation("/review changes", &all).unwrap();
    let c = all[&name].clone();
    let (agent, model) = c.switches(&p.loaded.config);
    let mut s = p.session(agent, model);
    s.run_command("/review changes", &c, &args, vec![], &mut Quiet)
        .await
        .unwrap();
    s
}

/// "runs ${fixture.name} in the background without switching the parent"
/// (3 cases): a native JSON command (`subagent: true` with an agent and a
/// model), a legacy Markdown one (`subtask: true`) and one whose agent is a
/// subagent each run in a child session, titled with the description, on
/// the right agent and model, with `` !`printf ready` `` filled in, while
/// the parent keeps its agent and model and gets the child's report.
#[tokio::test(flavor = "multi_thread")]
async fn oc_command_runs_subagent_commands_in_a_child_without_switching_the_parent() {
    let markdown =
        format!("---\ndescription: Review code\nsubtask: true\nagent: build\n---\n{TEMPLATE}");
    // (name, config, files, child agent, child model)
    type Fixture<'a> = (&'a str, Value, Vec<(&'a str, &'a str)>, &'a str, &'a str);
    let fixtures: [Fixture; 3] = [
        (
            "native JSON",
            json!({"commands": {"review": {"description": "Review code", "template": TEMPLATE, "subagent": true, "agent": "build", "model": "test/override"}}}),
            vec![],
            "build",
            "override",
        ),
        (
            "legacy Markdown",
            json!({}),
            vec![(".codesmith/commands/review.md", markdown.as_str())],
            "build",
            "parent",
        ),
        (
            "subagent mode by default",
            json!({"commands": {"review": {"description": "Review code", "template": TEMPLATE, "agent": "reviewer"}}}),
            vec![],
            "reviewer",
            "child",
        ),
    ];
    for (name, extra, files, agent, model) in fixtures {
        let p = project(extra, &files).await;
        let s = run_review(&p).await;
        let reqs = p.requests();
        assert_eq!(reqs.len(), 1, "{name}: only the child calls the model");
        assert_eq!(reqs[0]["model"], model, "{name}");
        assert_eq!(
            user_texts(&reqs[0]),
            ["You are a subagent spawned by another session.\nReview changes: ready"],
            "{name}"
        );
        let st = p.store();
        let children: Vec<_> = st
            .list(None, true)
            .unwrap()
            .into_iter()
            .filter(|i| i.parent_id.as_deref() == Some(s.id.as_str()))
            .collect();
        assert_eq!(children.len(), 1, "{name}");
        let child = &children[0];
        assert_eq!(child.agent, agent, "{name}");
        assert_eq!(child.model, format!("test/{model}"), "{name}");
        assert_eq!(child.title.as_deref(), Some("Review code"), "{name}");
        assert_eq!(s.agent.id, "build", "{name}: the parent keeps its agent");
        assert_eq!(s.model_spec, "test/parent", "{name}: and its model");
        let roles: Vec<Role> = s.messages.iter().map(|m| m.role.clone()).collect();
        assert_eq!(roles, [Role::User, Role::Assistant], "{name}");
        let report = match &s.messages[1].parts[0] {
            Part::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        };
        assert!(
            report.starts_with(&format!(
                "<subagent sessionID=\"{}\" state=\"completed\"",
                child.id
            )),
            "{name}: {report}"
        );
        assert!(report.contains("Review complete"), "{name}: {report}");
    }
}

/// "subagent: false overrides subagent mode and the legacy alias": the
/// command runs in the session, which switches to its agent (and that
/// agent's model).
#[tokio::test(flavor = "multi_thread")]
async fn oc_command_subagent_false_overrides_subagent_mode_and_the_legacy_alias() {
    let p = project(
        json!({"commands": {"review": {"description": "Review code", "template": TEMPLATE, "subagent": false, "subtask": true, "agent": "reviewer"}}}),
        &[],
    )
    .await;
    let s = run_review(&p).await;
    let st = p.store();
    assert!(st
        .list(None, true)
        .unwrap()
        .iter()
        .all(|i| i.parent_id.is_none()));
    assert_eq!(s.agent.id, "reviewer");
    assert_eq!(s.model_spec, "test/child");
    let reqs = p.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(user_texts(&reqs[0]), ["Review changes: ready"]);
}

/// "loads inline and file-based commands in config order": a command file
/// in a later layer replaces an inline command whole, nested files are
/// `dir/name`, an empty file is an empty command, and a file command
/// without placeholders gets the arguments appended.
#[tokio::test(flavor = "multi_thread")]
async fn oc_command_loads_inline_and_file_based_commands_in_config_order() {
    let inline = json!({"commands": {"review": {"template": "Inline review", "subagent": false}}});
    let p = project(
        json!({}),
        &[
            ("codesmith.json", inline.to_string().as_str()),
            (
                ".codesmith/commands/review.md",
                "---\ndescription: File review\nagent: reviewer\nmodel: anthropic/claude#high\nsubtask: true\n---\nReview files",
            ),
            (".codesmith/commands/nested/docs.md", "Write docs"),
            (".codesmith/commands/empty.md", ""),
        ],
    )
    .await;
    let all = p.commands();
    let review = &all["review"];
    assert_eq!(review.template, "Review files");
    assert_eq!(review.description.as_deref(), Some("File review"));
    assert_eq!(review.agent.as_deref(), Some("reviewer"));
    assert_eq!(review.model.as_deref(), Some("anthropic/claude#high"));
    assert_eq!(review.subtask, Some(true));
    assert_eq!(
        review.subagent, None,
        "replaced whole, not merged with the inline command"
    );
    assert_eq!(all["empty"].template, "");
    assert_eq!(all["nested/docs"].template, "Write docs");
    assert_eq!(
        command::find_invocation("/nested/docs details", &all),
        Some(("nested/docs".into(), "details".into()))
    );
    assert_eq!(
        command::find_invocation("/usr/bin is a path", &all),
        None,
        "a path is still a message"
    );
    let mut s = p.session(None, None);
    s.run_command(
        "/nested/docs details",
        &all["nested/docs"],
        "details",
        vec![],
        &mut Quiet,
    )
    .await
    .unwrap();
    assert_eq!(user_texts(&p.requests()[0]), ["Write docs\n\ndetails"]);
}

/// `` !`cmd` `` output (stdout and stderr) replaces each span, run in the
/// session's folder; a template without one is left alone.
#[tokio::test(flavor = "multi_thread")]
async fn oc_command_interpolates_shell_output() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("marker.txt"), "here").unwrap();
    let out = command::interpolate_shell(
        "A !`cat marker.txt` B !`printf x; printf y >&2` C !`printf $((1+2))`",
        "/bin/sh",
        d.path(),
    )
    .await
    .unwrap();
    assert_eq!(out, "A here B xy C 3");
    assert_eq!(
        command::interpolate_shell("no `shell` here! `x`", "/bin/sh", d.path())
            .await
            .unwrap(),
        "no `shell` here! `x`"
    );
}

/// "replaces commands with later definitions": a later layer's command
/// replaces the earlier one whole (its description and all).
#[test]
fn oc_command_replaces_commands_with_later_definitions() {
    let mut merged = json!({});
    config::merge_layer(
        &mut merged,
        json!({"commands": {"goal": {"template": "x", "description": "First", "agent": "plan"}, "other": {"template": "o"}}}),
    );
    config::merge_layer(
        &mut merged,
        json!({"commands": {"goal": {"template": "y", "description": "Second"}}}),
    );
    assert_eq!(
        merged["commands"],
        json!({"goal": {"template": "y", "description": "Second"}, "other": {"template": "o"}})
    );
}

/// An agent's request fields reach its model calls: `temperature` as the
/// request's temperature, other body fields as they are.
#[tokio::test(flavor = "multi_thread")]
async fn agent_request_fields_reach_the_model_request() {
    let p = project(
        json!({"agents": {"tuned": {"request": {"body": {"temperature": 0.25, "top_p": 0.9, "seed": 7}, "headers": {"x-agent": "tuned"}}}}}),
        &[],
    )
    .await;
    let mut s = p.session(Some("tuned".into()), None);
    s.prompt("hi", &mut Quiet).await.unwrap();
    let req = &p.requests()[0];
    assert_eq!(req["temperature"], json!(0.25));
    assert_eq!(req["top_p"], json!(0.9));
    assert_eq!(req["seed"], json!(7));
    assert!(s
        .model
        .provider
        .headers
        .contains(&("x-agent".into(), "tuned".into())));
}
