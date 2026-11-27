//! The programs CodeSmith starts besides shell commands — MCP stdio
//! servers, formatters, command-template interpolation (`!`cmd``) and the
//! worktree start command — do not inherit the credentials in this
//! process's environment, and still get what their config sets
//! explicitly (an MCP server's `environment`, a formatter's).
//!
//! This binary sets fake keys in its own environment, so it is a process
//! of its own; HOME, the config and the data folders are temporary.

#![cfg(unix)]

mod support;

use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Fake credentials in this process's environment, set once.
fn setup() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let base = support::isolate();
        // SAFETY: set once, before the tests start programs.
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-fake-openai");
            std::env::set_var("ANTHROPIC_API_KEY", "sk-fake-anthropic");
            std::env::set_var("SOME_SERVICE_TOKEN", "fake-token");
            std::env::set_var("CODESMITH_ENVTEST_PLAIN", "plain");
        }
        base
    })
    .clone()
}

/// The program's view of the variables, from the file it wrote.
fn seen(file: &Path) -> String {
    for _ in 0..200 {
        if let Ok(s) = std::fs::read_to_string(file) {
            if s.ends_with("end\n") {
                return s;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("{} was not written", file.display());
}

const PRINT: &str = r#"printf '%s|%s|%s|%s|%s|end\n' "${OPENAI_API_KEY-unset}" "${ANTHROPIC_API_KEY-unset}" "${SOME_SERVICE_TOKEN-unset}" "${CODESMITH_ENVTEST_PLAIN-unset}" "${EXPLICIT_API_KEY-unset}""#;

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_server_gets_its_configured_environment_but_no_inherited_credentials() {
    setup();
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("env.txt");
    let script = format!("{PRINT} > '{}'", out.display());
    let cfg: codesmith_core::mcp::McpConfig = serde_json::from_value(json!({"servers": {"probe": {
        "type": "local",
        "command": ["/bin/sh", "-c", script],
        "environment": {"EXPLICIT_API_KEY": "fake-explicit", "OPENAI_API_KEY": "fake-configured"}
    }}}))
    .unwrap();
    let _m = codesmith_core::mcp::Mcp::connect(&cfg, d.path()).await;
    // Configured variables reach it exactly, even ones named like keys.
    assert_eq!(
        seen(&out),
        "fake-configured|unset|unset|plain|fake-explicit|end\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_formatter_gets_its_configured_environment_but_no_inherited_credentials() {
    setup();
    let d = tempfile::tempdir().unwrap();
    let root = d.path().canonicalize().unwrap();
    let out = root.join("env.txt");
    let setting: codesmith_core::format::Setting = serde_json::from_value(json!({"probe": {
        "command": ["/bin/sh", "-c", format!("{PRINT} > '{}'", out.display())],
        "environment": {"EXPLICIT_API_KEY": "fake-explicit"},
        "extensions": [".probe"]
    }}))
    .unwrap();
    let f = codesmith_core::format::Formatter::new(Some(&setting), root.clone(), root.clone());
    std::fs::write(root.join("x.probe"), "").unwrap();
    f.file(&root.join("x.probe")).await;
    assert_eq!(seen(&out), "unset|unset|unset|plain|fake-explicit|end\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn command_template_interpolation_sees_no_credentials() {
    setup();
    let d = tempfile::tempdir().unwrap();
    let text = format!("before !`{PRINT}` after");
    let got = codesmith_core::command::interpolate_shell(&text, "/bin/sh", d.path(), &|_| Ok(()))
        .await
        .unwrap();
    assert!(got.contains("unset|unset|unset|plain|unset|end"), "{got}");
}

#[test]
fn the_worktree_start_command_sees_no_credentials() {
    setup();
    let d = tempfile::tempdir().unwrap();
    let r = d.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(&r).unwrap();
    let git = |args: &[&str]| {
        let mut c = std::process::Command::new("git"); // git-guard: test fixture
        for v in codesmith_core::git::LOCAL_ENV_VARS {
            c.env_remove(v);
        }
        assert!(c
            .current_dir(&r)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false"
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(r.join("a"), "a").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "i"]);
    let out = d.path().join("env.txt");
    let cfg = codesmith_core::worktree::WorktreeConfig {
        directory: Some(d.path().join("wt").display().to_string()),
        start: Some(format!("{PRINT} > '{}'", out.display())),
    };
    codesmith_core::worktree::create(
        &cfg,
        &r,
        &codesmith_core::worktree::Create {
            name: Some("probe".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(seen(&out), "unset|unset|unset|plain|unset|end\n");
}
