//! Language-server diagnostics in `write`/`edit` results and the `lsp`
//! tool, end to end against a fake server (`fixtures/lsp_fake.py`).

use codesmith_core::config::ToolOutputConfig;
use codesmith_core::lsp::{Lsp, Setting};
use codesmith_core::permission::{base_rules, Fixed, Gate, Reply};
use codesmith_core::tool::{self, Ctx};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn ctx<'a>(root: &Path, gate: &'a Gate, lsp: Arc<Lsp>) -> Ctx<'a> {
    Ctx {
        cwd: root.into(),
        root: root.into(),
        gate,
        shell: "/bin/sh".into(),
        session_id: "test".into(),
        limits: ToolOutputConfig::default(),
        client: reqwest::Client::new(),
        jobs: None,
        websearch: None,
        format: None,
        lsp: Some(lsp),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_follow_writes_and_edits() {
    let Some(py) = python() else { return };
    let fixture = format!("{}/tests/fixtures/lsp_fake.py", env!("CARGO_MANIFEST_DIR"));
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let setting: Setting = serde_json::from_value(json!({
        "fake": {"command": [py, fixture], "extensions": [".fake"],
                 "initialization": {"flavor": "spicy"}},
        "broken": {"command": ["/nonexistent/language-server"], "extensions": [".nope"]},
        "silent": {"command": ["sleep", "30"], "extensions": [".slow"]}
    }))
    .unwrap();
    let mut lsp = Lsp::new(Some(&setting), root.clone()).unwrap();
    lsp.timeouts.startup_wait = Duration::from_millis(500);
    let lsp = Arc::new(lsp);
    let gate = Gate::new(base_rules(), Arc::new(Fixed(Reply::Once)));
    let ctx = ctx(&root, &gate, lsp.clone());
    let file = root.join("a.fake");

    // write: errors in this file (warnings left out) and in other files.
    let (out, err) = tool::execute(
        &ctx,
        "write",
        &json!({"path": "a.fake", "content": "fine\n  ERROR here\nWARN there\n"}),
    )
    .await;
    assert!(!err, "{out}");
    let expected = format!(
        "Created file successfully: a.fake\n\nLSP errors detected in this file, please fix:\n<diagnostics file=\"{}\">\nERROR [2:3] error found (spicy)\n</diagnostics>",
        file.display()
    );
    assert!(out.starts_with(&expected), "{out}");
    assert!(!out.contains("warn found"), "{out}");
    assert!(
        out.contains(&format!(
            "\n\nLSP errors detected in other files:\n<diagnostics file=\"{}\">\nERROR [1:1] error found (spicy)\n</diagnostics>",
            root.join("other.fake").display()
        )),
        "{out}"
    );

    // edit fixing the error: nothing to report, and other files are not listed.
    let (out, err) = tool::execute(
        &ctx,
        "edit",
        &json!({"path": "a.fake", "oldString": "ERROR here", "newString": "ok here"}),
    )
    .await;
    assert!(!err, "{out}");
    assert_eq!(out, "Edited a.fake (1 replacement)");

    // edit bringing one back: sent as a didChange, reported again.
    let (out, _) = tool::execute(
        &ctx,
        "edit",
        &json!({"path": "a.fake", "oldString": "fine", "newString": "ERROR fine"}),
    )
    .await;
    assert!(
        out.ends_with(&format!(
            "LSP errors detected in this file, please fix:\n<diagnostics file=\"{}\">\nERROR [1:1] error found (spicy)\n</diagnostics>",
            file.display()
        )),
        "{out}"
    );
    assert!(!out.contains("other files"), "{out}");

    // The lsp tool asks the same server.
    let (out, err) = tool::execute(
        &ctx,
        "lsp",
        &json!({"operation": "goToDefinition", "path": "a.fake", "line": 1, "character": 1}),
    )
    .await;
    assert!(!err, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v[0]["range"]["start"]["line"], 0);
    let (out, _) = tool::execute(
        &ctx,
        "lsp",
        &json!({"operation": "hover", "path": "a.fake", "line": 1, "character": 1}),
    )
    .await;
    assert_eq!(out, "No results found for hover");

    // A server that cannot start, or never answers, does not hold up edits.
    for name in ["x.nope", "x.slow"] {
        let t = Instant::now();
        let (out, err) =
            tool::execute(&ctx, "write", &json!({"path": name, "content": "ERROR"})).await;
        assert!(!err, "{out}");
        assert!(!out.contains(&format!("file=\"{}", root.join(name).display())));
        assert!(
            t.elapsed() < Duration::from_secs(4),
            "{name}: {:?}",
            t.elapsed()
        );
    }
    let (out, err) = tool::execute(
        &ctx,
        "lsp",
        &json!({"operation": "hover", "path": "x.nope", "line": 1, "character": 1}),
    )
    .await;
    assert!(err && out.contains("No LSP server available"), "{out}");

    lsp.shutdown().await;
}

/// Against a real rust-analyzer, when installed: `cargo test -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn rust_analyzer_reports_a_type_error() {
    let ok = std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    let mut lsp = Lsp::new(Some(&Setting::Enabled(true)), root.clone()).unwrap();
    lsp.timeouts.startup_wait = Duration::from_secs(30);
    lsp.timeouts.diagnostics = Duration::from_secs(30);
    let lsp = Arc::new(lsp);
    let gate = Gate::new(base_rules(), Arc::new(Fixed(Reply::Once)));
    let ctx = ctx(&root, &gate, lsp.clone());
    let (out, _) = tool::execute(
        &ctx,
        "write",
        &json!({"path": "src/lib.rs", "content": "pub fn f() -> u32 { \"no\" }\n"}),
    )
    .await;
    assert!(out.contains("LSP errors detected in this file"), "{out}");
    lsp.shutdown().await;
}

/// Against a real clangd, when installed: `cargo test -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clangd_reports_a_syntax_error() {
    let ok = std::process::Command::new("clangd")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut lsp = Lsp::new(Some(&Setting::Enabled(true)), root.clone()).unwrap();
    lsp.timeouts.startup_wait = Duration::from_secs(30);
    lsp.timeouts.diagnostics = Duration::from_secs(30);
    let lsp = Arc::new(lsp);
    let gate = Gate::new(base_rules(), Arc::new(Fixed(Reply::Once)));
    let ctx = ctx(&root, &gate, lsp.clone());
    let (out, _) = tool::execute(
        &ctx,
        "write",
        &json!({"path": "a.c", "content": "int main(void) {\n  int x = ;\n  return x;\n}\n"}),
    )
    .await;
    assert!(out.contains("LSP errors detected in this file"), "{out}");
    assert!(out.contains("ERROR [2:"), "{out}");
    lsp.shutdown().await;
}
