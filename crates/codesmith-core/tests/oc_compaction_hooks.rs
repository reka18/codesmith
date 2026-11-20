//! Ports of OpenCode's compaction hook tests
//! (`packages/core/test/session-compaction.test.ts`, MIT) on CodeSmith's
//! [`Plugin::compaction`] and [`Plugin::request`] hooks. The plugin is
//! installed for the whole test binary; each test sets what it does for its
//! own session. See `crates/codesmith-core/PARITY.md`.

mod compaction_harness;

use codesmith_ai::{Interaction, Message, Request};
use codesmith_core::plugin::{self, Compaction, Plugin, Summary};
use compaction_harness::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Once};

#[derive(Default, Clone)]
struct Behavior {
    /// Supply this summary.
    supply: Option<String>,
    /// Add this to the summary request's system prompt.
    system: Option<String>,
    /// Describe the `read` tool so on every request.
    describe_read: Option<String>,
}

#[derive(Default)]
struct Seen {
    /// What each compaction hook saw: agent, model, request messages.
    compactions: Vec<(String, String, Vec<Message>)>,
    /// Compaction requests the request hook saw.
    compaction_requests: usize,
}

/// What the plugin does for each session, and what it saw there.
type Registry = HashMap<String, (Behavior, Arc<Mutex<Seen>>)>;

static BEHAVIOR: Mutex<Option<Registry>> = Mutex::new(None);

struct Hooks;

fn behavior(session: &str) -> Option<(Behavior, Arc<Mutex<Seen>>)> {
    BEHAVIOR.lock().unwrap().as_ref()?.get(session).cloned()
}

impl Plugin for Hooks {
    fn id(&self) -> &str {
        "test-hooks"
    }
    fn compaction(&self, c: &mut Compaction) {
        let Some((b, seen)) = behavior(&c.session_id) else {
            return;
        };
        seen.lock().unwrap().compactions.push((
            c.agent.clone(),
            c.model.clone(),
            c.request.messages.clone(),
        ));
        if let Some(s) = b.system {
            c.request.system.push_str(&format!("\n\n{s}"));
        }
        if let Some(s) = b.supply {
            c.result = Some(Summary {
                summary: s,
                usage: None,
            });
        }
    }
    fn request(&self, session_id: &str, req: &mut Request) {
        let Some((b, seen)) = behavior(session_id) else {
            return;
        };
        if req.interaction == Interaction::Compaction {
            seen.lock().unwrap().compaction_requests += 1;
        }
        if let Some(d) = b.describe_read {
            for t in req.tools.iter_mut().filter(|t| t.name == "read") {
                t.description = d.clone();
            }
        }
    }
}

fn hooked(session: &str, b: Behavior) -> Arc<Mutex<Seen>> {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| plugin::install(vec![Arc::new(Hooks)]));
    let seen = Arc::<Mutex<Seen>>::default();
    BEHAVIOR
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(session.to_string(), (b, seen.clone()));
    seen
}

const WINDOW: u64 = 200_000;
const BUFFER: u64 = WINDOW / 8;
const SUMMARY_INPUT: u64 = (WINDOW - BUFFER) * 9 / 10;

fn manual_only() -> Value {
    json!({"compaction": {"auto": false, "background": false, "buffer": BUFFER}})
}

/// "compaction hooks can supply the summary instead of the model"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_hooks_can_supply_the_summary_instead_of_the_model() {
    let (mut s, script, _d) = session(WINDOW, 32_000, manual_only()).await;
    let seen = hooked(
        &s.id,
        Behavior {
            supply: Some("## Objective\n- hooked summary".into()),
            ..Default::default()
        },
    );
    s.prompt(
        "Hooked compaction should see this conversation.",
        &mut Quiet,
    )
    .await
    .unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.compactions.len(), 1);
    let (agent, model, messages) = &seen.compactions[0];
    assert_eq!(
        (agent.as_str(), model.as_str()),
        (s.agent.name.as_str(), "fake/m")
    );
    assert!(format!("{messages:?}").contains("Hooked compaction should see this conversation."));
    assert_eq!(seen.compaction_requests, 0, "no request hooks");
    assert!(requests(&script, Kind::Summary).is_empty(), "no model call");
    let checkpoint = format!("{:?}", s.messages[0]);
    assert!(checkpoint.contains("<conversation-checkpoint>"));
    assert!(checkpoint.contains("hooked summary"));
}

/// "summary updates include the retained tail in compaction hooks and the
/// model request"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_updates_include_the_retained_tail_in_hooks_and_the_request() {
    let (mut s, script, _d) = session(WINDOW, 32_000, manual_only()).await;
    s.prompt("Previous exact request", &mut Quiet)
        .await
        .unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let seen = hooked(&s.id, Behavior::default());
    s.prompt(
        "The latest request supersedes the previous objective.",
        &mut Quiet,
    )
    .await
    .unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let seen = seen.lock().unwrap();
    let expected = &seen.compactions[0].2;
    assert!(format!("{:?}", expected[0]).contains("<conversation-checkpoint>"));
    assert!(format!("{expected:?}").contains("The latest request supersedes"));
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 2);
    let sent = summary[1]["messages"].as_array().unwrap();
    // The system prompt, what the hook saw (its system updates folded into
    // the message before them on the wire), the prompt.
    let shown = expected
        .iter()
        .filter(|m| m.role != codesmith_ai::Role::System)
        .count();
    assert_eq!(sent.len(), shown + 2);
    assert_eq!(
        sent.last().unwrap()["content"],
        codesmith_core::compaction::prompt(true)
    );
    assert_eq!(seen.compaction_requests, 1);
}

/// "summary cache prefix survives hooks that shape every request alike"
/// (adapted: one `request` hook shapes the session's requests and the
/// summary request alike).
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_cache_prefix_survives_hooks_that_shape_every_request_alike() {
    let (mut s, script, _d) = session(WINDOW, 32_000, manual_only()).await;
    hooked(
        &s.id,
        Behavior {
            describe_read: Some("Read files from the project".into()),
            ..Default::default()
        },
    );
    push(&script, [reply("Latest answer", SUMMARY_INPUT)]);
    s.prompt("Latest question", &mut Quiet).await.unwrap();
    s.compact(&mut Quiet).await.unwrap();
    let main = requests(&script, Kind::Main).pop().unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 1);
    let read = |b: &Value| {
        b["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == "read")
            .map(|t| t["function"]["description"].clone())
    };
    assert_eq!(
        read(&summary[0]),
        Some(json!("Read files from the project"))
    );
    assert_eq!(summary[0]["tools"], main["tools"]);
}

/// "summary resends as text when a hook makes the request larger than the
/// measured count"
#[tokio::test(flavor = "multi_thread")]
async fn oc_compaction_summary_resends_as_text_when_a_hook_makes_the_request_larger_than_measured()
{
    let (mut s, script, _d) = session(WINDOW, 32_000, manual_only()).await;
    hooked(
        &s.id,
        Behavior {
            system: Some("Plugin guidance".into()),
            ..Default::default()
        },
    );
    push(
        &script,
        [
            MainReply {
                text: "Earlier answer".into(),
                reasoning: token_text(WINDOW * 3),
                prompt_tokens: SUMMARY_INPUT,
                ..Default::default()
            },
            reply("Latest answer", SUMMARY_INPUT),
        ],
    );
    s.prompt("Earlier question", &mut Quiet).await.unwrap();
    s.prompt("Latest question", &mut Quiet).await.unwrap();
    // The provider rejects the unchanged request, which the measured count
    // said would fit.
    script.lock().unwrap().reject = Some(Box::new(|b: &Value| b.get("tools").is_some()));
    s.compact(&mut Quiet).await.unwrap();
    let summary = requests(&script, Kind::Summary);
    assert_eq!(summary.len(), 2);
    assert!(summary[0].get("tools").is_some());
    assert!(summary[1].get("tools").is_none());
    assert!(summary[0]["messages"][0]["content"]
        .to_string()
        .contains("Plugin guidance"));
}
