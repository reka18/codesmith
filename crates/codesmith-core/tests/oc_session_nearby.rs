//! Ports of OpenCode's nested instruction tests (MIT):
//! `session-instructions.test.ts`. Each test names the OpenCode test it
//! ports; see `crates/codesmith-core/PARITY.md`.
//!
//! Adapted: OpenCode's synthetic messages carry their paths as metadata;
//! CodeSmith's nested instruction messages are their own ledger (see
//! `codesmith_core::nearby`). Reads are the model's, through the fake model.

mod support;

use codesmith_ai::Message;
use codesmith_core::nearby;
use codesmith_core::store::Record;
use serde_json::json;
use std::path::Path;
use support::{text, H};

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// The nested instruction messages the model sees now.
fn injected(h: &H) -> Vec<String> {
    h.s.messages
        .iter()
        .filter(|m| nearby::is_nested(m))
        .map(text)
        .collect()
}

async fn read(h: &mut H, input: serde_json::Value) {
    h.prompt(&format!("call read {input}")).await;
}

// session-instructions.test.ts: "injects AGENTS.md files above a read, excludes the Location root, and dedups across reads"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_injects_agents_files_above_a_read_once() {
    let mut h = H::new().await;
    let dir = h.root();
    let sub = dir.join("sub/AGENTS.md");
    let deep = dir.join("sub/deep/AGENTS.md");
    let other = dir.join("sub/other/AGENTS.md");
    write(&dir.join("AGENTS.md"), "root-instructions");
    write(&sub, "sub-instructions");
    write(&deep, "deep-instructions");
    write(&other, "other-instructions\nmore rules");
    write(&dir.join("sub/deep/file.txt"), "file content");
    write(&dir.join("sub/other/file2.txt"), "file content 2");

    read(&mut h, json!({"path": "sub/deep/file.txt"})).await;
    let first = injected(&h);
    assert_eq!(
        first,
        [format!(
            "Instructions from: {}\ndeep-instructions\n\nInstructions from: {}\nsub-instructions",
            deep.display(),
            sub.display()
        )]
    );
    assert!(!first[0].contains("root-instructions"));
    // It follows the read's result, for the model's next step.
    let last = h.requests().last().unwrap().clone();
    let content = last["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(content.contains("deep-instructions"), "{last}");

    // Reading the file itself, all or part of it, adds no copy.
    read(&mut h, json!({"path": "sub/other/AGENTS.md"})).await;
    read(&mut h, json!({"path": "sub/other/AGENTS.md", "limit": 1})).await;
    assert_eq!(injected(&h).len(), 1);

    // A sibling read gives only the new file.
    read(&mut h, json!({"path": "sub/other/file2.txt"})).await;
    let second = injected(&h);
    assert_eq!(second.len(), 2);
    assert_eq!(
        second[1],
        format!(
            "Instructions from: {}\nother-instructions\nmore rules",
            other.display()
        )
    );
    assert!(!second.iter().any(|t| t.contains("root-instructions")));
    // Views of the conversation leave them out.
    assert!(codesmith_core::runner::is_engine(&Message::user(
        second[1].clone()
    )));
}

// session-instructions.test.ts: "falls back to CLAUDE.md above a read where a directory has no AGENTS.md"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_falls_back_to_claude_md_above_a_read() {
    let mut h = H::new().await;
    let dir = h.root();
    let sub = dir.join("sub/AGENTS.md");
    let deep = dir.join("sub/deep/CLAUDE.md");
    write(&sub, "sub-agents");
    write(&dir.join("sub/CLAUDE.md"), "sub-claude");
    write(&deep, "deep-claude");
    write(&dir.join("sub/deep/file.txt"), "content");
    read(&mut h, json!({"path": "sub/deep/file.txt"})).await;
    assert_eq!(
        injected(&h),
        [format!(
            "Instructions from: {}\ndeep-claude\n\nInstructions from: {}\nsub-agents",
            deep.display(),
            sub.display()
        )]
    );
}

// session-instructions.test.ts: "does not re-inject paths already recorded in durable session history"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_does_not_reinject_paths_in_the_stored_history() {
    let mut h = H::new().await;
    let dir = h.root();
    let sub = dir.join("sub/AGENTS.md");
    write(&dir.join("AGENTS.md"), "root-instructions");
    write(&sub, "sub-instructions");
    write(&dir.join("sub/file.txt"), "content");
    h.prompt("hello").await;
    let id = h.s.id.clone();
    h.store()
        .append(
            &id,
            &Record::Message {
                message: Message::user(format!("Instructions from: {}\nprior", sub.display())),
            },
        )
        .unwrap();
    h.s = h.open(&id);
    assert_eq!(injected(&h).len(), 1);
    read(&mut h, json!({"path": "sub/file.txt"})).await;
    assert_eq!(injected(&h).len(), 1);
}

// session-instructions.test.ts: "discovers AGENTS.md on a directory listing, including the listed directory's own, and dedups with a later file read"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_discovers_agents_files_on_a_directory_listing() {
    let mut h = H::new().await;
    let dir = h.root();
    let pkg = dir.join("packages/foo/AGENTS.md");
    write(&dir.join("AGENTS.md"), "root-instructions");
    write(&pkg, "pkg-instructions");
    write(&dir.join("packages/foo/file.txt"), "content");
    read(&mut h, json!({"path": "packages/foo"})).await;
    assert_eq!(
        injected(&h),
        [format!(
            "Instructions from: {}\npkg-instructions",
            pkg.display()
        )]
    );
    read(&mut h, json!({"path": "packages/foo/file.txt"})).await;
    assert_eq!(injected(&h).len(), 1);
}

// session-instructions.test.ts: "re-injects nested instructions dropped from history by compaction"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_reinjects_nested_instructions_dropped_by_compaction() {
    let mut h = H::with(json!({"compaction": {"keep": {"tokens": 0}}}), None).await;
    let dir = h.root();
    write(&dir.join("AGENTS.md"), "root-instructions");
    write(&dir.join("sub/AGENTS.md"), "sub-instructions");
    write(&dir.join("sub/file.txt"), "content");
    read(&mut h, json!({"path": "sub/file.txt"})).await;
    assert_eq!(injected(&h).len(), 1);
    h.s.compact(&mut support::Quiet).await.unwrap();
    assert_eq!(injected(&h).len(), 0);
    read(&mut h, json!({"path": "sub/file.txt"})).await;
    assert_eq!(injected(&h).len(), 1);
}

// session-instructions.test.ts: "listing the Location root directory injects no instructions"
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_listing_the_working_directory_injects_nothing() {
    let mut h = H::new().await;
    let dir = h.root();
    write(&dir.join("AGENTS.md"), "root-instructions");
    write(&dir.join("sub/AGENTS.md"), "sub-instructions");
    read(&mut h, json!({"path": "."})).await;
    assert!(injected(&h).is_empty());
}

/// Not in OpenCode's tests: a read outside the project brings nothing in.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_outside_the_project_injects_nothing() {
    let mut h = H::new().await;
    let outside = h.outside.path().canonicalize().unwrap();
    write(&outside.join("x/AGENTS.md"), "outside");
    write(&outside.join("x/f.txt"), "content");
    let path = outside.join("x/f.txt").display().to_string();
    read(&mut h, json!({"path": path})).await;
    assert!(injected(&h).is_empty());
}
