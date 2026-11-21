//! A port of OpenCode's `session-native.test.ts` (MIT) from the porting
//! backlog: instruction epochs across a native compaction, a resume and a
//! model switch, on the scripted OpenAI Responses server of
//! `oc_native_compaction.rs` (with the user's config kept out).

mod compaction_harness;
mod scripted;

use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session};
use codesmith_core::store::Store;
use compaction_harness::*;
use serde_json::{json, Value};
use std::sync::Arc;

const MODEL: &str = "gpt-5.4-mini";

/// The instruction file's text in a request's system prompt.
fn baseline(body: &Value) -> String {
    let sys = body["input"][0]["content"].as_str().unwrap();
    let at = sys.rfind("AGENTS.md\n").unwrap() + "AGENTS.md\n".len();
    sys[at..].lines().next().unwrap().to_string()
}

/// The request's input after the system prompt, as (type or role, text).
fn rest(body: &Value) -> Vec<(String, String)> {
    body["input"].as_array().unwrap()[1..]
        .iter()
        .map(|i| {
            let kind = i["role"].as_str().or(i["type"].as_str()).unwrap();
            let text = match &i["content"] {
                Value::String(s) => s.lines().last().unwrap_or("").to_string(),
                Value::Array(a) => a[0]["text"].as_str().unwrap_or("").to_string(),
                _ => i["encrypted_content"].as_str().unwrap_or("").to_string(),
            };
            (kind.to_string(), text)
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

// session-native.test.ts: "advances the native instruction epoch and omits
// superseded chronological updates after durable replay and provider
// switches" (adapted: instructions are the project's AGENTS.md, read before
// each step; the replay is a resume from the store; the switch is to
// another model of the provider)
#[tokio::test(flavor = "multi_thread")]
async fn oc_native_advances_the_instruction_epoch_and_omits_superseded_updates() {
    scripted::isolate();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let agents = root.join("AGENTS.md");
    std::fs::write(&agents, "original instructions").unwrap();
    let script: Shared = Default::default();
    let base = model(script.clone()).await;
    let mut extra = json!({"providers": {"fake": {"models": {"other": {}}}}});
    extra["providers"]["fake"]["models"][MODEL]["compaction"] = json!({"type": "native"});
    let l = loaded(
        &root,
        &base,
        "openai-responses",
        MODEL,
        200_000,
        32_000,
        extra,
    );
    let db = root.join("sessions.db");
    let mut s = open(&l, &root, Some(Store::open(&db).unwrap()), None);
    s.prompt("original request", &mut Quiet).await.unwrap();
    std::fs::write(&agents, "changed instructions").unwrap();
    s.prompt("second request", &mut Quiet).await.unwrap();
    let before = requests(&script, Kind::Main).pop().unwrap();
    assert_eq!(baseline(&before), "original instructions");
    assert!(rest(&before).contains(&("developer".into(), "changed instructions".into())));
    s.compact(&mut Quiet).await.unwrap();
    std::fs::write(&agents, "newest instructions").unwrap();
    s.prompt("continue", &mut Quiet).await.unwrap();

    // The compaction started a new epoch: the instructions it saw are the
    // baseline, and only the later change is an update after the window.
    let window = pairs(&[
        ("user", "original request"),
        ("user", "second request"),
        ("compaction", "encrypted_1"),
        ("developer", "newest instructions"),
        ("user", "continue"),
    ]);
    let after = requests(&script, Kind::Main).pop().unwrap();
    assert_eq!(baseline(&after), "changed instructions");
    assert_eq!(rest(&after), window);
    let id = s.id.clone();
    drop(s);

    // Replayed from the store on the same model: the same window.
    let mut same = open(&l, &root, Some(Store::open(&db).unwrap()), Some(id.clone()));
    same.prompt("again", &mut Quiet).await.unwrap();
    let replayed = requests(&script, Kind::Main).pop().unwrap();
    assert_eq!(baseline(&replayed), "changed instructions");
    assert_eq!(rest(&replayed)[..window.len()], window[..]);
    drop(same);

    // On another model the checkpoint does not apply: the history expands,
    // with the epoch's baseline and without the superseded update.
    let mut other = l.clone();
    other.config.model = Some("fake/other".into());
    let opts = Options {
        agent: None,
        model: Some("fake/other".into()),
        resume: Some(id),
        title: None,
        parent: None,
        questioner: None,
        mcp: None,
    };
    let store = Some(Store::open(&db).unwrap());
    let rejecting = Arc::new(Fixed(Reply::Reject(None)));
    let mut switched = Session::new(&other, root.clone(), opts, rejecting, store).unwrap();
    switched
        .prompt("after the switch", &mut Quiet)
        .await
        .unwrap();
    let expanded = requests(&script, Kind::Main).pop().unwrap();
    assert_eq!(baseline(&expanded), "changed instructions");
    let items = rest(&expanded);
    assert!(!items.iter().any(|(k, _)| k == "compaction"));
    let texts: Vec<&str> = items
        .iter()
        .filter(|(k, _)| k != "assistant")
        .map(|(_, t)| t.as_str())
        .collect();
    assert_eq!(
        texts,
        [
            "original request",
            "second request",
            "newest instructions",
            "continue",
            "again",
            "after the switch"
        ]
    );
}
