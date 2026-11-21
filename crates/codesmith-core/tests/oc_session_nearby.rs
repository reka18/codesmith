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

/// Not in OpenCode: an instruction file that is a symlink is not followed
/// (it could point out of the project), even to a file the project holds.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_symlinked_instruction_file_is_not_given() {
    let mut h = H::new().await;
    let dir = h.root();
    let outside = h.outside.path().canonicalize().unwrap();
    write(&outside.join("secret.txt"), "outside-secret");
    write(&dir.join("sub/file.txt"), "content");
    std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("sub/AGENTS.md")).unwrap();
    write(&dir.join("in/real.md"), "inside");
    write(&dir.join("in/sub/file.txt"), "content");
    std::os::unix::fs::symlink(dir.join("in/real.md"), dir.join("in/sub/AGENTS.md")).unwrap();
    read(&mut h, json!({"path": "sub/file.txt"})).await;
    read(&mut h, json!({"path": "in/sub/file.txt"})).await;
    assert!(injected(&h).is_empty(), "{:?}", injected(&h));
}

/// Not in OpenCode: a file the read rules would not allow without asking
/// is left out, without asking; and dependency folders give nothing.
#[tokio::test(flavor = "multi_thread")]
async fn instruction_files_follow_the_read_rules_and_skip_dependencies() {
    let mut h = H::with(
        json!({"permissions": [
            {"action": "read", "resource": "private/AGENTS.md", "effect": "deny"},
            {"action": "read", "resource": "asked/AGENTS.md", "effect": "ask"},
        ]}),
        None,
    )
    .await;
    let dir = h.root();
    for d in ["private", "asked", "node_modules/pkg", "open"] {
        write(&dir.join(d).join("AGENTS.md"), &format!("{d}-instructions"));
        write(&dir.join(d).join("f.txt"), "content");
    }
    for d in ["private", "asked", "node_modules/pkg", "open"] {
        read(&mut h, json!({"path": format!("{d}/f.txt")})).await;
    }
    let got = injected(&h);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].contains("open-instructions"));
}

/// Not in OpenCode: a large instruction file is cut, with a note.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_instruction_file_is_capped() {
    let mut h = H::new().await;
    let dir = h.root();
    write(
        &dir.join("big/AGENTS.md"),
        &"x".repeat(nearby::MAX_BYTES * 2),
    );
    write(&dir.join("big/f.txt"), "content");
    read(&mut h, json!({"path": "big/f.txt"})).await;
    let got = injected(&h);
    assert_eq!(got.len(), 1);
    assert!(got[0].len() < nearby::MAX_BYTES + 500);
    assert!(got[0].contains("(Truncated: only the first 64 KiB"));
    // Its path is still recognised as given.
    assert_eq!(
        nearby::paths_of(&got[0]).unwrap(),
        [dir.join("big/AGENTS.md")]
    );
}

/// The instruction loads a front end shows (OpenCode's instruction
/// messages in the transcript): the files of each load, nearest first,
/// reported once, when the model is given them.
#[tokio::test(flavor = "multi_thread")]
async fn instruction_loads_are_reported_to_the_front_end() {
    struct Loads(Vec<Vec<std::path::PathBuf>>);
    impl codesmith_core::session::Sink for Loads {
        fn text(&mut self, _: &str) {}
        fn instructions(&mut self, paths: &[std::path::PathBuf]) {
            self.0.push(paths.to_vec());
        }
    }
    let mut h = H::new().await;
    let dir = h.root();
    let sub = dir.join("sub/AGENTS.md");
    let deep = dir.join("sub/deep/AGENTS.md");
    write(&sub, "sub-instructions");
    write(&deep, "deep-instructions");
    write(&dir.join("sub/deep/file.txt"), "file content");
    let mut loads = Loads(vec![]);
    let call = format!("call read {}", json!({"path": "sub/deep/file.txt"}));
    h.s.prompt(&call, &mut loads).await.unwrap();
    h.s.prompt(&call, &mut loads).await.unwrap();
    assert_eq!(loads.0, [vec![deep, sub]]);
}
