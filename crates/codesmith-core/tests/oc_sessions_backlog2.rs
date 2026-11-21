//! Ports of the rest of the session and session store cases in PARITY.md's
//! porting backlog (`packages/core/test/session-create.test.ts`,
//! `session-instructions.test.ts`, `session-prompt.test.ts`,
//! `session-runner-message.test.ts`, `session-runner-tool-events.test.ts`,
//! `session-stats.test.ts`, MIT). OpenCode admits a prompt's attachments in
//! its session service; CodeSmith's front ends read them (through
//! `Session::attach_local`) and hand the session `attachment::Attached`
//! values, so "adapted" notes say which of those stands in.

mod scripted;

use codesmith_ai::{Media, Message, Part, Role, Usage};
use codesmith_core::attachment::{self, Attached};
use codesmith_core::store::{self, Info, Record, Store};
use codesmith_core::{instructions, media, stats};
use scripted::*;
use serde_json::{json, Value};
use std::sync::Arc;

const PIXEL: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

fn info(id: &str, parent: Option<&str>, t: i64) -> Info {
    Info {
        id: id.into(),
        directory: "/project".into(),
        title: Some(id.into()),
        agent: "build".into(),
        model: "fake/m".into(),
        parent_id: parent.map(String::from),
        created: t,
        updated: t,
        usage: Usage::default(),
    }
}

/// The user messages a chat request sends, as their content parts.
fn user_contents(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].clone())
        .collect()
}

// --- session-create.test.ts ---------------------------------------------

/// "filters direct child sessions by parent ID". Adapted: the list with
/// children, filtered by parent (as the session tree is built).
#[test]
fn oc_session_list_filters_direct_child_sessions_by_parent() {
    let st = Store::memory().unwrap();
    st.create(&info("ses_parent", None, 1)).unwrap();
    st.create(&info("ses_child", Some("ses_parent"), 2))
        .unwrap();
    st.create(&info("ses_grandchild", Some("ses_child"), 3))
        .unwrap();
    st.create(&info("ses_other", None, 4)).unwrap();
    let children: Vec<String> = st
        .list(Some("/project"), true)
        .unwrap()
        .into_iter()
        .filter(|i| i.parent_id.as_deref() == Some("ses_parent"))
        .map(|i| i.id)
        .collect();
    assert_eq!(children, ["ses_child"]);
    // Roots only, without children.
    let roots: Vec<String> = st
        .list(Some("/project"), false)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(roots, ["ses_other", "ses_parent"]);
}

// --- session-instructions.test.ts ---------------------------------------

/// "loads instructions directly without a read". Adapted: the files read
/// and rendered as the session's instruction source renders them.
#[test]
fn oc_session_instructions_load_directly_without_a_read() {
    let d = tempfile::tempdir().unwrap();
    let sub = d.path().join("sub").join("AGENTS.md");
    std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
    std::fs::write(&sub, "sub-instructions\n").unwrap();
    let codesmith_core::instruction_state::Observed::Value(files) =
        instructions::read(std::slice::from_ref(&sub))
    else {
        panic!("not read")
    };
    assert_eq!(
        instructions::render_files(&files),
        format!("Instructions from: {}\nsub-instructions", sub.display())
    );
}

// --- session-prompt.test.ts ---------------------------------------------

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// "commits a staged revert before admitting a new prompt". Adapted: an
/// undo hides its turn at once and can be redone until the next prompt,
/// which commits it: the undone turn is not sent again, and there is
/// nothing left to redo.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_prompt_commits_an_undo_before_admitting_a_new_prompt() {
    if !git_available() {
        return;
    }
    let mut t = start(Setup {
        extra: json!({"snapshots": true}),
        store: true,
        ..Default::default()
    })
    .await;
    t.push([text("first answer"), text("second answer")]);
    t.prompt("boundary").await.unwrap();
    let root = t.dir.path().canonicalize().unwrap();
    let db = t.store();
    let snaps = codesmith_core::snapshot::Snapshots::for_project(&root).unwrap();
    assert!(codesmith_core::session::undo(&db, &t.s.id, &snaps)
        .unwrap()
        .is_some());
    let mut again = t.reopen();
    again
        .prompt("after revert", &mut Rec(Arc::default()))
        .await
        .unwrap();
    let reqs = t.requests();
    let users = user_texts(reqs.last().unwrap());
    assert!(!users.iter().any(|u| u.contains("boundary")), "{users:?}");
    assert!(users.iter().any(|u| u.contains("after revert")));
    assert!(codesmith_core::session::redo(&db, &t.s.id, &snaps)
        .unwrap()
        .is_none());
}

/// "resolves attachment MIME before admission". Adapted: a data URL as
/// media, its type and data taken from the URL.
#[test]
fn oc_session_prompt_resolves_attachment_mime() {
    let uri = format!("data:image/png;base64,{PIXEL}");
    let m = Media::from_data_url(&uri).unwrap();
    assert_eq!(m.mime, "image/png");
    assert_eq!(m.inline(), Some(PIXEL));
}

/// "materializes directories as directory attachments"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_prompt_materializes_directories_as_directory_attachments() {
    let t = plain().await;
    std::fs::create_dir_all(t.path("source")).unwrap();
    std::fs::write(t.path("source/session-prompt.test.ts"), "x").unwrap();
    let a = t.s.attach_local("source").unwrap().unwrap();
    assert_eq!(a.media.mime, attachment::DIRECTORY_MIME);
    assert_eq!(a.media.filename.as_deref(), Some("source"));
    let listing = media::unbase64(a.media.inline().unwrap()).unwrap();
    assert!(String::from_utf8_lossy(&listing).contains("session-prompt.test.ts"));
    assert!(a.source.unwrap().ends_with("source"));
}

/// "materializes local image content before admission"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_prompt_materializes_local_image_content() {
    let t = plain().await;
    let bytes = media::unbase64(PIXEL).unwrap();
    std::fs::write(t.path("image.png"), &bytes).unwrap();
    let a = t.s.attach_local("image.png").unwrap().unwrap();
    assert_eq!(a.media.mime, "image/png");
    assert_eq!(a.media.inline(), Some(PIXEL));
    assert_eq!(a.media.filename.as_deref(), Some("image.png"));
    assert_eq!(
        a.source.as_deref(),
        Some(t.s.cwd.join("image.png").display().to_string().as_str())
    );
}

/// "admits captured image bytes with their source path after the source
/// disappears": the bytes were read before the file went, and the model
/// is still told where it came from.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_prompt_admits_captured_image_bytes_after_the_source_disappears() {
    let mut t = plain().await;
    let path = t.path("screenshot.png");
    std::fs::write(&path, media::unbase64(PIXEL).unwrap()).unwrap();
    let a = t.s.attach_local("screenshot.png").unwrap().unwrap();
    std::fs::remove_file(&path).unwrap();
    t.push([text("seen")]);
    t.s.prompt_attached(
        "[Image 1] Inspect this image",
        vec![a.labeled("[Image 1]")],
        &mut Rec(Arc::default()),
    )
    .await
    .unwrap();
    let content = user_contents(&t.requests()[0]).pop().unwrap();
    let text = content.to_string();
    assert!(
        text.contains(&format!(
            "Attached file: {}",
            t.s.cwd.join("screenshot.png").display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("data:image/png;base64,{PIXEL}")),
        "{text}"
    );
}

/// "normalizes large image content before validating persisted Base64".
/// Adapted: CodeSmith's resizer re-encodes the oversized image below the
/// byte limit (OpenCode's photon gives an empty one).
#[test]
fn oc_session_prompt_normalizes_large_image_content() {
    let pixel = media::unbase64(PIXEL).unwrap();
    let mut bytes = pixel.clone();
    bytes.resize(4_323_030, 0);
    let data = media::base64(&bytes);
    assert_eq!(data.len(), 5_764_040);
    let limits = media::ImageLimits::default();
    let (mime, out) = media::normalize("image/png", &data, &limits)
        .unwrap()
        .expect("re-encoded");
    assert!(mime == "image/png" || mime == "image/jpeg");
    assert!(
        out.len() <= limits.max_base64_bytes && out.len() < 1_000,
        "{}",
        out.len()
    );
}

/// "sniffs data URL content instead of trusting its declared MIME".
/// Adapted: media is detected from its bytes, so text declared as video
/// (or as nothing) is not media.
#[test]
fn oc_session_prompt_sniffs_content_instead_of_trusting_its_declared_mime() {
    assert_eq!(media::detect(b"export const value = 1\n"), None);
    assert_eq!(media::detect(b"BMW service schedule\n"), None);
    assert_eq!(
        media::detect(&media::unbase64(PIXEL).unwrap()),
        Some("image/png")
    );
}

/// "fails for an unknown session" (`Session.inbox`). Adapted: resuming a
/// session the store does not have fails.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_fails_for_an_unknown_session() {
    let t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    let loaded = t.loaded();
    let opts = codesmith_core::session::Options {
        agent: None,
        model: None,
        resume: Some("ses_missing".into()),
        title: None,
        parent: None,
        id: None,
        questioner: None,
        mcp: None,
    };
    let root = loaded.root.clone();
    let r = codesmith_core::session::Session::new(
        &loaded,
        root,
        opts,
        Arc::new(codesmith_core::permission::Fixed(
            codesmith_core::permission::Reply::Once,
        )),
        Some(t.store()),
    );
    assert!(r.is_err());
}

// --- session-runner-message.test.ts -------------------------------------

/// "preserves attachment order after the prompt". Adapted: a directory
/// and an image (a text file is put in the prompt by the front end).
#[test]
fn oc_runner_message_preserves_attachment_order_after_the_prompt() {
    let dir = Attached::directory(std::path::Path::new("/project/src"), "index.ts");
    let image = Attached::local(
        Media::base64("image/png", "AAECAw==").named("a.png"),
        std::path::Path::new("/project/a.png"),
    );
    let parts = attachment::parts(vec![dir, image]);
    let shape: Vec<String> = parts
        .iter()
        .map(|p| match p {
            Part::Text(t) => t.text.clone(),
            Part::Media(m) => format!("media {}", m.mime),
            _ => "other".into(),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "Attached directory: /project/src\n\nindex.ts",
            "Attached file: /project/a.png",
            "media image/png",
        ]
    );
}

/// "omits empty prompt text before an attachment"
#[tokio::test(flavor = "multi_thread")]
async fn oc_runner_message_omits_empty_prompt_text_before_an_attachment() {
    let mut t = plain().await;
    std::fs::create_dir_all(t.path("src")).unwrap();
    std::fs::write(t.path("src/index.ts"), "x").unwrap();
    let a = t.s.attach_local("src").unwrap().unwrap();
    t.push([text("ok")]);
    t.s.prompt_attached("", vec![a], &mut Rec(Arc::default()))
        .await
        .unwrap();
    let content = user_contents(&t.requests()[0]).pop().unwrap();
    let texts: Vec<String> = match &content {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|p| p["text"].as_str().map(String::from))
            .collect(),
        v => panic!("{v}"),
    };
    assert!(!texts.iter().any(|t| t.trim().is_empty()), "{content}");
    assert!(texts[0].starts_with("Attached directory: "), "{content}");
    // Stored without the empty part, so no provider is sent an empty text block.
    let stored =
        &t.s.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .unwrap()
            .parts;
    assert!(!stored
        .iter()
        .any(|p| matches!(p, Part::Text(t) if t.text.is_empty())));
}

/// "uses materialized image and PDF data as provider media"
#[tokio::test(flavor = "multi_thread")]
async fn oc_runner_message_uses_materialized_image_and_pdf_data_as_provider_media() {
    let mut t = plain().await;
    t.push([text("ok")]);
    t.s.prompt_attached(
        "Inspect this image",
        vec![
            Attached::inline(Media::base64("image/png", "AAECAw==").named("image.png")),
            Attached::inline(Media::base64("application/pdf", "JVBERg==").named("document.pdf")),
        ],
        &mut Rec(Arc::default()),
    )
    .await
    .unwrap();
    let content = user_contents(&t.requests()[0]).pop().unwrap();
    assert_eq!(
        content,
        json!([
            {"type": "text", "text": "Inspect this image"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAECAw=="}},
            {"type": "file", "file": {"filename": "document.pdf", "file_data": "data:application/pdf;base64,JVBERg=="}},
        ])
    );
}

/// "replays durable tool media into canonical tool messages without
/// structured base64": after a reload, a read image goes back as media,
/// and the tool's text result carries no base64.
#[tokio::test(flavor = "multi_thread")]
async fn oc_runner_message_replays_durable_tool_media_without_structured_base64() {
    let mut t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    std::fs::write(t.path("pixel.png"), media::unbase64(PIXEL).unwrap()).unwrap();
    t.push([
        call("c1", "read", json!({"path": "pixel.png"})),
        text("seen"),
        text("again"),
    ]);
    t.prompt("Look").await.unwrap();
    let mut again = t.reopen();
    again
        .prompt("Again", &mut Rec(Arc::default()))
        .await
        .unwrap();
    let reqs = t.requests();
    let last = reqs.last().unwrap();
    let tool = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    assert!(!tool.to_string().contains(PIXEL), "{tool}");
    assert_eq!(last.to_string().matches(PIXEL).count(), 1, "{last}");
    assert!(last
        .to_string()
        .contains(&format!("data:image/png;base64,{PIXEL}")));
}

/// "restores OpenAI encrypted reasoning metadata": a Responses reasoning
/// item's id and encrypted content go back in the next request, after a
/// reload.
#[tokio::test(flavor = "multi_thread")]
async fn oc_runner_message_restores_openai_encrypted_reasoning_metadata() {
    let mut t = start(Setup {
        store: true,
        protocol: "openai-responses",
        ..Default::default()
    })
    .await;
    t.push([sse(&[
        json!({"type": "response.created", "response": {"id": "resp_1"}}),
        json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1"}}),
        json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "Think"}),
        json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "encrypted-state", "summary": [{"type": "summary_text", "text": "Think"}]}}),
        json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1"}}),
        json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Answer"}),
        json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": "Answer"}]}}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
    ]), sse(&[
        json!({"type": "response.created", "response": {"id": "resp_2"}}),
        json!({"type": "response.completed", "response": {"id": "resp_2", "status": "completed"}}),
    ])]);
    t.prompt("Think first").await.unwrap();
    let mut again = t.reopen();
    again
        .prompt("Continue", &mut Rec(Arc::default()))
        .await
        .unwrap();
    let reqs = t.requests();
    assert_eq!(reqs.len(), 2);
    let item = reqs[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "reasoning")
        .cloned()
        .unwrap_or_else(|| panic!("{}", reqs[1]["input"]));
    assert_eq!(item["id"], "rs_1", "{item}");
    assert_eq!(item["encrypted_content"], "encrypted-state", "{item}");
}

// --- session-runner-tool-events.test.ts ---------------------------------

/// "local tool success serializes media base64 once through canonical
/// content": a read image is stored once, not again in the tool's text.
#[tokio::test(flavor = "multi_thread")]
async fn oc_tool_success_stores_media_base64_once() {
    let mut t = start(Setup {
        store: true,
        extra: json!({"media": {"blob": false}}),
        ..Default::default()
    })
    .await;
    std::fs::write(t.path("pixel.png"), media::unbase64(PIXEL).unwrap()).unwrap();
    t.push([
        call("c1", "read", json!({"path": "pixel.png"})),
        text("seen"),
    ]);
    t.prompt("Look").await.unwrap();
    let entries = t.store().entries(&t.s.id).unwrap();
    let stored: String = entries
        .iter()
        .map(|e| serde_json::to_string(&e.record).unwrap())
        .collect();
    assert_eq!(stored.matches(PIXEL).count(), 1);
    let result = results(&t.s);
    assert_eq!(result.len(), 1);
    assert!(!result[0].1.contains(PIXEL));
}

// --- session-stats.test.ts ----------------------------------------------

fn user(text: &str) -> Record {
    Record::Message {
        message: Message {
            role: Role::User,
            parts: vec![Part::text(text)],
        },
    }
}

fn day(d: u32, h: u32) -> i64 {
    use chrono::TimeZone;
    chrono::Local
        .with_ymd_and_hms(2026, 1, d, h, 0, 0)
        .single()
        .unwrap()
        .timestamp_millis()
}

fn step(model: &str, n: u64) -> Record {
    Record::Step {
        model: model.into(),
        usage: Usage {
            input: n,
            output: n,
            ..Default::default()
        },
    }
}

/// "aggregates activity and tool reliability without reading message
/// payloads outside the range". Adapted: CodeSmith's records (steps and
/// tool calls with their durations); no unfinished calls are stored, and
/// a fork's copied history is its own.
#[test]
fn oc_stats_aggregates_activity_and_tool_reliability_over_a_range() {
    let st = Store::memory().unwrap();
    st.create(&info("ses_root", None, day(2, 9))).unwrap();
    st.create(&info("ses_child", Some("ses_root"), day(3, 9)))
        .unwrap();
    st.create(&info("ses_other", None, day(1, 9) - 86_400_000 * 400))
        .unwrap();
    st.append_at("ses_root", &user("hello"), day(2, 9)).unwrap();
    st.append_at("ses_root", &step("sonnet", 10), day(2, 10))
        .unwrap();
    st.append_at(
        "ses_root",
        &Record::Tool {
            name: "read".into(),
            ms: 250,
            error: false,
        },
        day(2, 10),
    )
    .unwrap();
    st.append_at(
        "ses_root",
        &Record::Tool {
            name: "edit".into(),
            ms: 2_000,
            error: true,
        },
        day(2, 10),
    )
    .unwrap();
    st.append_at("ses_child", &user("subtask"), day(3, 10))
        .unwrap();
    st.append_at("ses_child", &step("large", 30), day(3, 10))
        .unwrap();
    // Outside the range: neither counted nor its payload read.
    st.append_at("ses_root", &step("sonnet", 1000), day(1, 9) - 86_400_000)
        .unwrap();
    st.append_at("ses_other", &step("old", 5), day(1, 9) - 86_400_000 * 400)
        .unwrap();
    let q = stats::Query {
        from: Some(day(1, 0)),
        to: day(31, 0),
        directory: None,
    };
    let s = stats::compute(&st, &q, &|_| None).unwrap();
    assert_eq!((s.sessions, s.subagents, s.prompts, s.steps), (1, 1, 1, 2));
    assert_eq!((s.tokens.input, s.tokens.output), (40, 40));
    let models: Vec<&str> = s.models.iter().map(|m| m.model.as_str()).collect();
    assert_eq!(models, ["large", "sonnet"]);
    let tools: Vec<(&str, u64, u64, u64, Option<u64>)> = s
        .tools
        .iter()
        .map(|t| (t.name.as_str(), t.calls, t.succeeded, t.failed, t.p50_ms))
        .collect();
    assert_eq!(
        tools,
        [("edit", 1, 0, 1, Some(2_000)), ("read", 1, 1, 0, Some(250))]
    );
    assert_eq!(
        s.activity.keys().cloned().collect::<Vec<_>>(),
        ["2026-01-02", "2026-01-03"]
    );
    assert_eq!(s.streak, 2);
    // Without a start, the range starts at the first record.
    let all = stats::compute(
        &st,
        &stats::Query {
            from: None,
            to: day(31, 0),
            directory: None,
        },
        &|_| None,
    )
    .unwrap();
    assert_eq!(all.from, day(1, 9) - 86_400_000 * 400);
    let _ = store::now();
}
