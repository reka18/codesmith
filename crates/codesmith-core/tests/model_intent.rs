//! What a user means by a model, end to end: a signed-in Claude Code
//! counts as available for the default model, typed specs are made
//! canonical (or refused) before a session sends anything, Claude without
//! an Anthropic key runs through Claude Code, and failed requests lead
//! with the step that fixes them.
//!
//! Claude Code is "signed in" here as CodeSmith sees it without starting
//! it: `claude` is found (`CODESMITH_CLAUDE_PATH`, a stub that is never
//! run) and its last check saved a model list without finding it signed
//! out. Every test holds [`LOCK`], as one test signs it out for a while.

mod support;

use codesmith_ai::claude_agent::catalog::{Catalog as ClaudeCatalog, Model as ClaudeModel};
use codesmith_core::catalog::{Json, ModelsDev, Snapshot};
use codesmith_core::config::{Config, Loaded};
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session, Sink};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::const_new(());

/// Claude Code's storage, with it signed in.
fn setup() -> (MutexGuard<'static, ()>, PathBuf) {
    signed_in(LOCK.blocking_lock())
}

/// [`setup`], from a test on a runtime.
async fn setup_async() -> (MutexGuard<'static, ()>, PathBuf) {
    signed_in(LOCK.lock().await)
}

fn signed_in(guard: MutexGuard<'static, ()>) -> (MutexGuard<'static, ()>, PathBuf) {
    static STORAGE: OnceLock<PathBuf> = OnceLock::new();
    let storage = STORAGE
        .get_or_init(|| {
            let base = support::isolate();
            // Keys from the developer's own environment would make other
            // providers available.
            for v in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
                std::env::remove_var(v);
            }
            let stub = base.join("claude-stub");
            std::fs::write(&stub, "#!/bin/sh\nexit 1\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            std::env::set_var(codesmith_ai::claude_agent::PATH_VAR, &stub);
            let storage = codesmith_core::providers::claude_agent::storage();
            std::fs::create_dir_all(&storage).unwrap();
            let model = |id: &str, resolved: &str| ClaudeModel {
                id: id.into(),
                name: id.into(),
                efforts: vec!["low".into(), "high".into()],
                context_window: None,
                resolved_model: Some(resolved.into()),
            };
            ClaudeCatalog {
                price_source: None,
                models: vec![
                    model("opus", "claude-opus-4-5-20251124"),
                    model("sonnet", "claude-sonnet-4-5-20250929"),
                ],
            }
            .save(&storage)
            .unwrap();
            storage
        })
        .clone();
    let _ = std::fs::remove_file(storage.join("signed-out"));
    (guard, storage)
}

fn catalog(providers: &[(&str, &[(&str, &str)])]) -> Vec<Snapshot> {
    let ps: Vec<String> = providers
        .iter()
        .map(|(pid, models)| {
            let ms: Vec<String> = models
                .iter()
                .map(|(id, date)| {
                    format!(
                        r#""{id}": {{"id": "{id}", "type": "chat", "name": "{id}", "release_date": "{date}",
                        "modalities": {{"input": ["text"], "output": ["text"]}},
                        "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}}},
                        "limit": {{"context": 128000, "output": 32000}},
                        "api": {{"chat-completions": {{"base_url": "https://api.test.invalid/v1",
                            "sdk": {{"opencodeai": "@opencode/ai/providers/openai-compatible"}}}}}}}}"#
                    )
                })
                .collect();
            format!(
                r#""{pid}": {{"id": "{pid}", "name": "{pid}", "env": [], "models": {{{}}}}}"#,
                ms.join(",")
            )
        })
        .collect();
    codesmith_core::catalog::normalize(&Json::parse(&format!("{{{}}}", ps.join(","))).unwrap())
}

/// A fresh machine's catalog: an LM Studio model (a local server) and
/// Anthropic's models.
fn fresh(v: Value) -> Config {
    let mut c: Config = serde_json::from_value(v).unwrap();
    c.models_dev = Some(Arc::new(ModelsDev::fixed(catalog(&[
        ("lmstudio", &[("openai/gpt-oss-20b", "2025-08-05")]),
        (
            "anthropic",
            &[
                ("claude-opus-4-5", "2025-11-24"),
                ("claude-sonnet-4-5", "2025-09-29"),
            ],
        ),
    ]))));
    c
}

// --- 2. A signed-in Claude Code is available ---

// Regression: a fresh install defaulted to `lmstudio/openai/gpt-oss-20b`,
// a local server that was not running, though Claude Code was signed in.
#[test]
fn a_fresh_machine_defaults_to_a_signed_in_claude_code() {
    let (_g, storage) = setup();
    let c = fresh(json!({}));
    assert_eq!(c.default_model().as_deref(), Some("claude-agent/opus"));
    let listed = codesmith_core::plugin::codesmith::available(&c, "chat");
    let opus = listed
        .iter()
        .find(|r| r.provider_id == "claude-agent" && r.id == "opus")
        .expect("Claude Code's models are listed");
    assert!(opus.released > 0, "dated by the model it resolves to");
    // Without the fix (Claude Code not counted), the local server won.
    std::fs::write(storage.join("signed-out"), b"").unwrap();
    assert_eq!(
        c.default_model().as_deref(),
        Some("lmstudio/openai/gpt-oss-20b")
    );
    std::fs::remove_file(storage.join("signed-out")).unwrap();
    assert_eq!(c.default_model().as_deref(), Some("claude-agent/opus"));
}

#[test]
fn a_configured_or_disabled_choice_still_wins_over_claude_code() {
    let (_g, _) = setup();
    let c = fresh(json!({"model": "lmstudio/openai/gpt-oss-20b"}));
    assert_eq!(
        c.default_model().as_deref(),
        Some("lmstudio/openai/gpt-oss-20b")
    );
    let c = fresh(json!({"disabled_providers": ["claude-agent"]}));
    assert_eq!(
        c.default_model().as_deref(),
        Some("lmstudio/openai/gpt-oss-20b")
    );
    // A newer keyed model is newer: Claude Code is available, not
    // preferred.
    let mut c = fresh(json!({"providers": {"other": {
        "settings": {"baseURL": "https://o.test.invalid/v1", "apiKey": "k"}
    }}}));
    c.models_dev = Some(Arc::new(ModelsDev::fixed(catalog(&[
        ("other", &[("future", "2030-01-01")]),
        ("anthropic", &[("claude-opus-4-5", "2025-11-24")]),
    ]))));
    assert_eq!(c.default_model().as_deref(), Some("other/future"));
}

#[test]
fn claude_code_not_installed_or_signed_out_is_not_available() {
    let (_g, storage) = setup();
    use codesmith_core::providers::claude_agent;
    let env = codesmith_core::providers::process_env();
    assert!(claude_agent::signed_in(&env));
    // Not installed: `claude` is nowhere.
    let none: codesmith_core::providers::Env = Arc::new(|_: &str| -> Option<String> { None });
    assert!(!claude_agent::signed_in(&none));
    let empty = tempfile::tempdir().unwrap();
    let bare_path = empty.path().display().to_string();
    let only_path: codesmith_core::providers::Env =
        Arc::new(move |k: &str| (k == "PATH").then(|| bare_path.clone()));
    assert!(!claude_agent::signed_in(&only_path));
    // Never checked: no saved list.
    assert!(!claude_agent::signed_in_at(empty.path()));
    // Signed out at the last check.
    std::fs::write(storage.join("signed-out"), b"").unwrap();
    assert!(!claude_agent::signed_in(&env));
    let c = fresh(json!({}));
    let listed = codesmith_core::plugin::codesmith::available(&c, "chat");
    assert!(listed.iter().all(|r| r.provider_id != "claude-agent"));
    std::fs::remove_file(storage.join("signed-out")).unwrap();
}

#[test]
fn a_check_records_whether_claude_code_is_signed_in() {
    let (_g, _) = setup();
    use codesmith_ai::claude_agent::catalog::State;
    use codesmith_ai::claude_agent::probe::Probe;
    use codesmith_core::providers::claude_agent;
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("claude-agent");
    let probe = |account: Value| State::Ready {
        probe: Probe {
            version: Some("2.0.0".into()),
            account,
            models: vec![json!({"value": "opus"})],
        },
        from_chat: false,
    };
    ClaudeCatalog {
        price_source: None,
        models: vec![ClaudeModel {
            id: "opus".into(),
            name: "Opus".into(),
            efforts: vec![],
            context_window: None,
            resolved_model: None,
        }],
    }
    .save(&storage)
    .unwrap();
    assert!(claude_agent::signed_in_at(&storage));
    claude_agent::record(&storage, &probe(json!({"tokenSource": "none"})));
    assert!(!claude_agent::signed_in_at(&storage));
    // Not starting says nothing new about the login.
    claude_agent::record(&storage, &State::Missing);
    claude_agent::record(&storage, &State::Failed("x".into()));
    assert!(!claude_agent::signed_in_at(&storage));
    claude_agent::record(&storage, &probe(json!({"tokenSource": "claude.ai"})));
    assert!(claude_agent::signed_in_at(&storage));
}

// --- 1 and 4, as typed ---

#[test]
fn typed_specs_are_resolved_as_things_stand() {
    let (_g, _) = setup();
    let c = fresh(json!({}));
    // Claude Code is signed in and Anthropic has no key.
    let r = codesmith_core::model_spec::resolve(&c, "anthropic/ opus").unwrap();
    assert_eq!(r.spec, "claude-agent/opus");
    assert!(r.note.unwrap().contains("Claude Code"));
    let r = codesmith_core::model_spec::resolve(&c, "anthropic/claude-sonnet-4-5#high").unwrap();
    assert_eq!(r.spec, "claude-agent/sonnet#high");
    // A model name alone finds Claude Code's.
    assert_eq!(
        codesmith_core::model_spec::resolve(&c, "sonnet")
            .unwrap()
            .spec,
        "claude-agent/sonnet"
    );
    // A configured Anthropic URL (a proxy) is its own way in.
    let c = fresh(
        json!({"providers": {"anthropic": {"settings": {"baseURL": "https://proxy.test.invalid/v1"}}}}),
    );
    assert_eq!(
        codesmith_core::model_spec::resolve(&c, "anthropic/ opus")
            .unwrap()
            .spec,
        "anthropic/claude-opus-4-5"
    );
}

fn options(model: Option<&str>) -> Options {
    Options {
        agent: None,
        model: model.map(str::to_string),
        resume: None,
        title: Some("t".into()),
        parent: None,
        questioner: None,
        mcp: None,
        id: None,
    }
}

fn open(v: Value, model: Option<&str>) -> anyhow::Result<Session> {
    open_child(v, model, None)
}

fn open_child(v: Value, model: Option<&str>, parent: Option<&str>) -> anyhow::Result<Session> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut config: Config = serde_json::from_value(v).unwrap();
    config.snapshots = Some(false);
    let loaded = Loaded {
        config,
        files: vec![],
        root: root.clone(),
    };
    Session::new(
        &loaded,
        root,
        Options {
            parent: parent.map(str::to_string),
            ..options(model)
        },
        Arc::new(Fixed(Reply::Once)),
        None,
    )
}

fn hosted(extra: Value) -> Value {
    let mut v = json!({"providers": {"fake": {
        "protocol": "openai-chat", "local": false,
        "settings": {"baseURL": "https://fake.test.invalid/v1", "apiKey": "x"},
        "models": {"big-model-2": {}, "small-1": {}}
    }}});
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v
}

// Regression: a session took `fake/ big` as the model " big" and sent it.
#[test]
fn a_session_runs_the_canonical_model_of_a_loose_spec() {
    let (_g, _) = setup();
    let s = open(hosted(json!({})), Some(" fake / big ")).unwrap();
    assert_eq!(s.model_spec, "fake/big-model-2");
    assert_eq!(s.model.model_id, "big-model-2");
}

#[test]
fn a_session_refuses_an_unknown_model_before_sending_anything() {
    let (_g, _) = setup();
    let e = open(hosted(json!({})), Some("fake/bigg-model"))
        .err()
        .unwrap();
    let e = format!("{e:#}");
    assert!(e.contains("unknown model \"bigg-model\""), "{e}");
    assert!(e.contains("did you mean: fake/big-model-2"), "{e}");
    for bad in ["", "   ", "fake/", "nope/x"] {
        assert!(open(hosted(json!({})), Some(bad)).is_err(), "{bad:?}");
    }
}

#[test]
fn a_configured_model_is_made_canonical_when_it_can_be_and_kept_otherwise() {
    let (_g, _) = setup();
    let s = open(hosted(json!({"model": "fake/ small"})), None).unwrap();
    assert_eq!(s.model_spec, "fake/small-1");
    // Unknown to the list: kept as configured, as before.
    let s = open(hosted(json!({"model": "fake/retired"})), None).unwrap();
    assert_eq!(s.model_spec, "fake/retired");
}

// A subagent's model (its parent's, its agent's or a command's) is not
// refused: it ran before, and it is made canonical when it can be.
#[test]
fn a_subagent_keeps_a_model_the_list_lacks() {
    let (_g, _) = setup();
    let s = open_child(hosted(json!({})), Some("fake/retired"), Some("parent")).unwrap();
    assert_eq!(s.model_spec, "fake/retired");
    let s = open_child(hosted(json!({})), Some("fake/ small"), Some("parent")).unwrap();
    assert_eq!(s.model_spec, "fake/small-1");
}

// --- 5. Failed requests lead with the fix ---

/// A server answering every request with `status` and `body`, until the
/// test ends.
async fn answering(status: u16, body: &'static str) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let mut seen = Vec::new();
                // Read the head and the body it announces.
                loop {
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&seen).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len: usize = text[..i]
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse().ok())
                            })
                            .unwrap_or(0);
                        if seen.len() >= i + 4 + len {
                            break;
                        }
                    }
                }
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    format!("http://{addr}/v1")
}

struct Quiet;
impl Sink for Quiet {
    fn text(&mut self, _: &str) {}
}

// Regression: "HTTP 401: x-api-key header is required" was all the user
// saw.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_anthropic_key_reads_as_the_fix() {
    let (_g, _) = setup_async().await;
    let url = answering(
        401,
        r#"{"type":"error","error":{"type":"authentication_error","message":"x-api-key header is required"}}"#,
    )
    .await;
    let v = json!({"providers": {"anthropic": {"settings": {"baseURL": url}, "models": {"claude-opus-4-5": {}}}}});
    let mut s = open(v, Some("anthropic/claude-opus-4-5")).unwrap();
    s.set_retry_scale(0.0);
    let deadline = Duration::from_secs(60);
    let e = tokio::time::timeout(deadline, s.prompt("hi", &mut Quiet))
        .await
        .expect("the turn ends")
        .unwrap_err()
        .to_string();
    let first = e.lines().next().unwrap();
    assert_eq!(
        first,
        "No Anthropic API key. You're signed into Claude Code — switch to claude-agent/opus, or run `codesmith auth login anthropic`."
    );
    // The raw error stays.
    assert!(e.contains("HTTP 401"), "{e}");
    assert!(e.contains("x-api-key header is required"), "{e}");
}

// Regression: a local server that was not running reported only
// "Connection refused (os error 61)".
#[tokio::test(flavor = "multi_thread")]
async fn a_local_server_that_is_not_running_reads_as_the_fix() {
    let (_g, _) = setup_async().await;
    // A port nothing listens on: bound, then let go.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let base = format!("http://127.0.0.1:{port}/v1");
    let v = json!({"providers": {"lmstudio": {"settings": {"baseURL": base}}}});
    let mut s = open(v, Some("lmstudio/openai/gpt-oss-20b")).unwrap();
    s.set_retry_scale(0.0);
    let start = Instant::now();
    let e = tokio::time::timeout(Duration::from_secs(60), s.prompt("hi", &mut Quiet))
        .await
        .expect("the turn ends")
        .unwrap_err()
        .to_string();
    assert!(start.elapsed() < Duration::from_secs(60));
    assert_eq!(
        e.lines().next().unwrap(),
        format!(
            "The LM Studio server isn't running at {base}; start it, or switch models with ctrl+x m."
        )
    );
    assert!(e.to_lowercase().contains("connection refused"), "{e}");
}
