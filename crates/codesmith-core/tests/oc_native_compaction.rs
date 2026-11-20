//! Ports of OpenCode's native (provider) compaction tests
//! (`packages/core/test/session-native-compaction.test.ts`,
//! `session-native.test.ts`, `session-compaction.test.ts` and the native
//! scenarios of `session-runner.test.ts`, MIT), on a scripted OpenAI
//! Responses server that answers `compaction_trigger` requests and
//! `/responses/compact` with encrypted checkpoints. See
//! `crates/codesmith-core/PARITY.md`.

mod compaction_harness;

use codesmith_ai::{Message, Part, Role};
use codesmith_core::store::Store;
use compaction_harness::*;
use serde_json::{json, Value};

const MODEL: &str = "gpt-5.4-mini";

fn native(mechanism: Option<&str>, extra: Value) -> Value {
    let mut c = json!({"type": "native"});
    if let Some(m) = mechanism {
        c["mechanism"] = json!(m);
    }
    let mut v = extra;
    v["providers"]["fake"]["models"][MODEL]["compaction"] = c;
    v
}

/// The checkpoint's encrypted contents in a window.
fn checkpoints(window: &[Message]) -> Vec<String> {
    window
        .iter()
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            Part::Compaction(c) => Some(c.encrypted.clone()),
            _ => None,
        })
        .collect()
}

fn user_texts(window: &[Message]) -> Vec<String> {
    window
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            Part::Text(t) => Some(t.to_string()),
            _ => None,
        })
        .collect()
}

/// "manual trigger persists and continues, retains earlier users
/// repeatedly, and preserves context on failure/cancellation" (adapted: no
/// synthetic messages or retry hook; failures are a rejection and a 429).
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_manual_trigger_persists_continues_retains_users_and_preserves_context() {
    let dir = tempfile::tempdir().unwrap();
    let script: Shared = Default::default();
    let base = model(script.clone()).await;
    let l = loaded(
        dir.path(),
        &base,
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        native(None, json!({})),
    );
    let db = dir.path().join("sessions.db");
    let mut s = open(&l, dir.path(), Some(Store::open(&db).unwrap()), None);
    s.prompt("First real user request", &mut Quiet)
        .await
        .unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let first = s.messages.clone();
    assert_eq!(
        first.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
        [Role::User, Role::Assistant]
    );
    assert_eq!(checkpoints(&first), ["encrypted_1"]);
    let body = &requests(&script, Kind::Native)[0];
    assert!(body["input"]
        .as_array()
        .unwrap()
        .contains(&json!({"type": "compaction_trigger"})));
    assert!(body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "read"));
    assert!(body.get("context_management").is_none());

    // The next request replays the checkpoint with the current instructions.
    s.prompt("Second real user request", &mut Quiet)
        .await
        .unwrap();
    let next = requests(&script, Kind::Main).pop().unwrap().to_string();
    assert!(next.contains("encrypted_1"));
    assert!(next.contains("Second real user request"));
    assert!(next.contains(&s.system[..40]));
    s.compact(&mut Quiet).await.unwrap();
    let second = s.messages.clone();
    assert_eq!(
        user_texts(&second),
        ["First real user request", "Second real user request"]
    );
    assert_eq!(checkpoints(&second), ["encrypted_2"]);

    // Nothing new since the checkpoint is not compactable.
    let e = s.compact(&mut Quiet).await.unwrap_err();
    assert!(format!("{e:#}").contains("nothing to compact"), "{e:#}");

    // A failure, then a cancellation, leave the checkpoint as it was.
    s.prompt("Third real user request", &mut Quiet)
        .await
        .unwrap();
    let before = s.messages.clone();
    script.lock().unwrap().native_fail = 1;
    assert!(s.compact(&mut Quiet).await.is_err());
    assert_eq!(s.messages, before);
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    script.lock().unwrap().native_hold = Some(gate.clone());
    let cancelled =
        tokio::time::timeout(std::time::Duration::from_millis(300), s.compact(&mut Quiet)).await;
    assert!(cancelled.is_err(), "still waiting when cancelled");
    assert_eq!(s.messages, before);
    script.lock().unwrap().native_hold = None;
    gate.add_permits(10);
    // (The cancelled request is answered to nobody first.)
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // A transient failure retries under the session's retry policy.
    script.lock().unwrap().native_flaky = 1;
    s.compact(&mut Quiet).await.unwrap();
    assert_eq!(user_texts(&s.messages).len(), 3);
    let calls = requests(&script, Kind::Native).len();
    assert_eq!(
        calls, 6,
        "two compactions, a failure, a cancellation, a retry"
    );

    // The window is what a resumed session replays.
    let window = s.messages.clone();
    let id = s.id.clone();
    drop(s);
    let resumed = open(&l, dir.path(), Some(Store::open(&db).unwrap()), Some(id));
    assert_eq!(resumed.messages, window);
}

/// "manual and automatic endpoint compaction keep the provider replacement
/// unchanged"
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_manual_and_automatic_endpoint_compaction_keep_the_replacement_unchanged() {
    let (mut s, script, _d) = session_on(
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        native(Some("endpoint"), json!({})),
    )
    .await;
    s.prompt("Original user", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    // An overflow compacts automatically through the endpoint too.
    script.lock().unwrap().overflow = 1;
    s.prompt("Later user", &mut Quiet).await.unwrap();
    let natives = requests(&script, Kind::Native);
    assert_eq!(natives.len(), 2);
    assert!(natives[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "read"));
    assert!(natives[0].get("context_management").is_none());
    assert!(natives[0].get("stream").is_none());
    assert_eq!(user_texts(&s.messages[..1]), ["endpoint retained"]);
    assert!(!format!("{:?}", s.messages).contains("Original user"));
    assert_eq!(checkpoints(&s.messages), ["encrypted_2"]);
}

/// "automatic native failures, interruptions, and overflows retain the
/// checkpoint" (adapted: the failure is a rejection; the overflow is the
/// provider rejecting the compaction itself, which fails without a retry).
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_automatic_failures_interruptions_and_overflows_retain_the_checkpoint() {
    let (mut s, script, _d) = session_on(
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        native(None, json!({})),
    )
    .await;
    s.prompt("Original durable request", &mut Quiet)
        .await
        .unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let installed = s.messages.clone();
    // An overflow whose compaction fails reports the overflow.
    {
        let mut sc = script.lock().unwrap();
        sc.overflow = 1;
        sc.native_fail = 1;
    }
    let e = s.prompt("Recent request", &mut Quiet).await.unwrap_err();
    assert!(format!("{e:#}").contains("maximum context length"), "{e:#}");
    assert_eq!(s.messages[..2], installed[..]);
    // The provider rejecting the compaction as too long gives up at once.
    let before = s.messages.clone();
    script.lock().unwrap().native_overflow = true;
    let e = s.compact(&mut Quiet).await.unwrap_err();
    assert!(format!("{e:#}").contains("compaction failed"), "{e:#}");
    assert_eq!(requests(&script, Kind::Native).len(), 3);
    assert!(requests(&script, Kind::Native)[2]
        .to_string()
        .contains("encrypted_1"));
    assert_eq!(s.messages, before);
}

/// "compaction hooks supply the summary instead of provider compaction":
/// see `tests/oc_compaction_hooks.rs`, which installs the hook plugin.
///
/// "native compaction fails without a model call on a route that cannot
/// compact"
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_compaction_fails_without_a_model_call_on_a_route_that_cannot_compact() {
    let (mut s, script, _d) = session_on(
        "openai-chat",
        MODEL,
        200_000,
        32_000,
        native(None, json!({})),
    )
    .await;
    s.prompt("Compact this natively.", &mut Quiet)
        .await
        .unwrap();
    let before = script.lock().unwrap().log.len();
    let e = s.compact(&mut Quiet).await.unwrap_err();
    assert_eq!(
        format!("{e:#}"),
        "Native compaction is not supported for fake/OpenAI Chat"
    );
    assert_eq!(script.lock().unwrap().log.len(), before, "no model call");
}

/// "retained user budget counts attachments and drops whole oldest
/// messages"
#[test]
fn oc_native_retained_user_budget_counts_attachments_and_drops_whole_oldest_messages() {
    let user = |t: &str| Message::user(t);
    let newest = || {
        let mut m = user(&"x".repeat(63_000 * 4));
        m.parts.push(Part::Media(codesmith_ai::Media::base64(
            "image/png",
            "aGVsbG8=",
        )));
        m
    };
    let recent = codesmith_core::compaction::recent_users;
    assert!(recent(&[user("old"), newest()], 64_000).is_empty());
    let mut small = newest();
    small.parts[0] = Part::text("new");
    assert_eq!(
        recent(&[user(&"x".repeat(63_000 * 4)), small], 64_000).len(),
        1
    );
}

/// session-runner.test.ts: "automatically persists native windows,
/// retains earlier users, and waits for fresh usage" (adapted: one
/// automatic compaction past the ceiling, then none until a response
/// measures the window).
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_automatically_persists_windows_retains_users_and_waits_for_fresh_usage() {
    let (mut s, script, _d) = session_on(
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        native(None, json!({})),
    )
    .await;
    push(
        &script,
        [
            reply("Earlier answer", 185_000),
            reply("Next answer", 1_000),
        ],
    );
    s.prompt("Earlier request", &mut Quiet).await.unwrap();
    s.prompt("Retained recent request", &mut Quiet)
        .await
        .unwrap();
    assert_eq!(requests(&script, Kind::Native).len(), 1);
    assert_eq!(
        user_texts(&s.messages[..2]),
        ["Earlier request", "Retained recent request"]
    );
    assert!(!checkpoints(&s.messages).is_empty());
    // The encrypted window estimates as nothing: no compaction before a
    // response measures it.
    s.prompt("Another request", &mut Quiet).await.unwrap();
    assert_eq!(requests(&script, Kind::Native).len(), 1);
}

/// session-runner.test.ts: "recovers an overflowing native window with
/// another native compaction"
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_recovers_an_overflowing_native_window_with_another_native_compaction() {
    let (mut s, script, _d) = session_on(
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        native(None, json!({})),
    )
    .await;
    s.prompt("First", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    script.lock().unwrap().overflow = 1;
    s.prompt("Second", &mut Quiet).await.unwrap();
    let natives = requests(&script, Kind::Native);
    assert_eq!(natives.len(), 2);
    assert!(natives[1].to_string().contains("encrypted_1"));
    assert_eq!(checkpoints(&s.messages), ["encrypted_2"]);
    // The step after the recovery went out with the new window.
    let last = requests(&script, Kind::Main).pop().unwrap().to_string();
    assert!(last.contains("encrypted_2") && last.contains("Second"));
}

/// session-native.test.ts: "falls back to an earlier compatible native or
/// local checkpoint" (adapted: a session resumed on another model replays
/// from the summary before the window, with the messages it stood for).
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_falls_back_to_an_earlier_compatible_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let script: Shared = Default::default();
    let base = model(script.clone()).await;
    let extra = json!({"providers": {"fake": {"models": {"other": {}}}}});
    let summary = loaded(
        dir.path(),
        &base,
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        extra.clone(),
    );
    let natively = loaded(
        dir.path(),
        &base,
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        native(None, extra),
    );
    let db = dir.path().join("sessions.db");
    let mut s = open(&summary, dir.path(), Some(Store::open(&db).unwrap()), None);
    s.prompt("Summarized request", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let id = s.id.clone();
    drop(s);
    let mut s = open(
        &natively,
        dir.path(),
        Some(Store::open(&db).unwrap()),
        Some(id.clone()),
    );
    s.prompt("Natively compacted request", &mut Quiet)
        .await
        .unwrap();
    s.compact(&mut Quiet).await.unwrap();
    assert!(!checkpoints(&s.messages).is_empty());
    drop(s);
    // On the model that made it, the window replays.
    let same = open(
        &natively,
        dir.path(),
        Some(Store::open(&db).unwrap()),
        Some(id.clone()),
    );
    assert!(!checkpoints(&same.messages).is_empty());
    // On another model, the summary and what came after it do.
    let mut other = summary.clone();
    other.config.model = Some("fake/other".into());
    let opts = codesmith_core::session::Options {
        agent: None,
        model: Some("fake/other".into()),
        resume: Some(id),
        title: None,
        parent: None,
        questioner: None,
        mcp: None,
    };
    let resumed = codesmith_core::session::Session::new(
        &other,
        dir.path().to_path_buf(),
        opts,
        std::sync::Arc::new(codesmith_core::permission::Fixed(
            codesmith_core::permission::Reply::Reject(None),
        )),
        Some(Store::open(&db).unwrap()),
    )
    .unwrap();
    assert!(checkpoints(&resumed.messages).is_empty());
    let shown = format!("{:?}", resumed.messages);
    assert!(shown.contains("<conversation-checkpoint>"));
    assert!(shown.contains("Natively compacted request"));
}

/// session-runner.test.ts: "restores installed native context with auto
/// disabled and preserves it across fork and revert" (adapted: resumed and
/// forked with the store; no revert).
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_restores_installed_native_context_with_auto_disabled_and_across_a_fork() {
    let dir = tempfile::tempdir().unwrap();
    let script: Shared = Default::default();
    let base = model(script.clone()).await;
    let l = loaded(
        dir.path(),
        &base,
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        native(None, json!({"compaction": {"auto": false}})),
    );
    let db = dir.path().join("sessions.db");
    let mut s = open(&l, dir.path(), Some(Store::open(&db).unwrap()), None);
    s.prompt("Installed request", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    s.prompt("After the window", &mut Quiet).await.unwrap();
    let (id, history) = (s.id.clone(), s.messages.clone());
    drop(s);
    let resumed = open(
        &l,
        dir.path(),
        Some(Store::open(&db).unwrap()),
        Some(id.clone()),
    );
    assert_eq!(resumed.messages, history);
    drop(resumed);
    Store::open(&db).unwrap().fork(&id, "forked").unwrap();
    let mut forked = open(
        &l,
        dir.path(),
        Some(Store::open(&db).unwrap()),
        Some("forked".into()),
    );
    assert_eq!(forked.messages, history);
    forked.prompt("In the fork", &mut Quiet).await.unwrap();
    let last = requests(&script, Kind::Main).pop().unwrap().to_string();
    assert!(last.contains("encrypted_1") && last.contains("In the fork"));
}
