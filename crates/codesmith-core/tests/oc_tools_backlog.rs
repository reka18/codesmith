//! Ports of the tool registry and tool tests in PARITY.md's porting backlog
//! (`packages/core/test/tool-registry.test.ts`, `file-mutation.test.ts`,
//! `tool-search.test.ts`, MIT). Each test names the OpenCode case it ports;
//! see `crates/codesmith-core/PARITY.md`.

mod scripted;

use codesmith_core::agent;
use codesmith_core::config::Config;
use scripted::*;
use serde_json::{json, Value};

/// The built-in tools the build agent is offered under `rules`.
fn offered(rules: Value) -> Vec<&'static str> {
    let c: Config = serde_json::from_value(json!({"permissions": rules})).unwrap();
    let a = agent::resolve(&c, "build").unwrap();
    agent::tools(&a).into_iter().map(|t| t.name()).collect()
}

/// "filters disabled tools with edit aliases and ordered wildcard precedence"
#[test]
fn oc_registry_filters_disabled_tools_with_edit_aliases_and_ordered_wildcards() {
    let all = offered(json!([]));
    for name in ["question", "shell", "edit", "write", "patch"] {
        assert!(all.contains(&name), "{name}: {all:?}");
    }
    let names = offered(json!([{"action": "question", "resource": "*", "effect": "deny"}]));
    assert!(!names.contains(&"question"));
    assert_eq!(names.len(), all.len() - 1, "{names:?}");
    // A narrower allow after a blanket deny keeps that tool alone.
    assert_eq!(
        offered(json!([
            {"action": "*", "resource": "*", "effect": "deny"},
            {"action": "question", "resource": "private", "effect": "allow"}
        ])),
        ["question"]
    );
    // The same rules the other way round: the deny wins.
    assert!(offered(json!([
        {"action": "question", "resource": "private", "effect": "allow"},
        {"action": "*", "resource": "*", "effect": "deny"}
    ]))
    .is_empty());
    // `edit` covers the tools that change files.
    let names = offered(json!([{"action": "edit", "resource": "*", "effect": "deny"}]));
    for name in ["edit", "write", "patch"] {
        assert!(!names.contains(&name), "{name}: {names:?}");
    }
    assert!(names.contains(&"shell") && names.contains(&"question"));
}

/// "are not available without full_codemode" (Code Mode's `path` and
/// `Bun`). Adapted: CodeSmith's programs run in Boa, which has neither, with
/// `full_codemode` or without.
#[tokio::test(flavor = "multi_thread")]
async fn oc_registry_code_mode_programs_have_no_path_or_bun() {
    let mut t = start(Setup {
        extra: json!({"full_codemode": true, "model": "fake/gpt-5"}),
        ..Default::default()
    })
    .await;
    t.push([
        call(
            "call-globals",
            "execute",
            json!({"code": "return [typeof path, typeof Bun]"}),
        ),
        text("Done"),
    ]);
    t.prompt("Check the globals").await.unwrap();
    let r = results(&t.s);
    assert_eq!(r.len(), 1, "{r:?}");
    let out = r[0].1.replace([' ', '\n'], "");
    assert!(out.contains(r#"["undefined","undefined"]"#), "{}", r[0].1);
}

/// The `toolCalls` rows of each `tool_progress`, and where the call ended.
struct Rows(Vec<Value>, Option<usize>);
impl codesmith_core::session::Sink for Rows {
    fn text(&mut self, _: &str) {}
    fn tool_progress(&mut self, _: &str, metadata: &Value) {
        self.0.push(metadata["toolCalls"].clone());
    }
    fn tool_end(&mut self, _: &str, _: &str, _: &str, _: bool) {
        self.1 = Some(self.0.len());
    }
}

/// "execute counts its finished nested calls instead of itself"
/// (`tui/test/cli/tui/session-verbosity`), its core half: a program that
/// ends well reports its final call rows before the call ends, as
/// OpenCode's result metadata carries them, so the transcript can count
/// them.
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_reports_its_final_nested_calls_before_it_ends() {
    let mut t = start(Setup {
        extra: json!({"full_codemode": true, "model": "fake/gpt-5"}),
        reply: codesmith_core::permission::Reply::Always,
        ..Default::default()
    })
    .await;
    std::fs::write(t.path("a.txt"), "a\n").unwrap();
    let code = "await tools.read({path: 'a.txt'}); await tools.read({path: 'a.txt'}); return 1";
    t.push([
        call("call-reads", "execute", json!({"code": code})),
        text("Done"),
    ]);
    let mut sink = Rows(vec![], None);
    t.s.prompt("Read twice", &mut sink).await.unwrap();
    let end = sink.1.expect("the call ended");
    let last = &sink.0[..end].last().cloned().unwrap_or_default();
    assert_eq!(
        last,
        &json!([
            {"tool": "read", "status": "completed", "input": {"path": "a.txt"}},
            {"tool": "read", "status": "completed", "input": {"path": "a.txt"}},
        ]),
        "{:?}",
        sink.0
    );
    assert_eq!(sink.0.len(), end, "nothing after the end");
}

// --- Shell tool and runtime ---

/// "returns both parallel CodeMode shell results" (`tool-shell.test.ts`)
#[tokio::test(flavor = "multi_thread")]
async fn oc_shell_returns_both_parallel_code_mode_shell_results() {
    let mut t = start(Setup {
        extra: json!({"full_codemode": true, "model": "fake/gpt-5"}),
        reply: codesmith_core::permission::Reply::Always,
        ..Default::default()
    })
    .await;
    let code = "return await Promise.all([tools.shell({command: 'echo one; sleep 0.1'}), tools.shell({command: 'echo two; sleep 0.1'})])";
    t.push([
        call("call-parallel-shells", "execute", json!({"code": code})),
        text("Done"),
    ]);
    t.prompt("Run both").await.unwrap();
    let r = results(&t.s);
    assert_eq!(r.len(), 1, "{r:?}");
    assert!(!r[0].2, "{r:?}");
    let one = r[0].1.find("one").unwrap_or_else(|| panic!("{}", r[0].1));
    let two = r[0].1.find("two").unwrap_or_else(|| panic!("{}", r[0].1));
    assert!(one < two, "{}", r[0].1);
}

/// "reports a command killed without an exit code or signal of its own"
/// (`shell-result.test.ts`). Adapted: no metadata flag; the notice says it.
#[test]
fn oc_shell_reports_a_command_killed_without_an_exit_code_of_its_own() {
    use codesmith_core::tool::shell::{notice, render, Ending};
    assert_eq!(
        notice(&Ending::Lost, 1_000).as_deref(),
        Some("Command was killed: the process daemon stopped")
    );
    assert_eq!(
        render("partial\n", &Ending::Lost, 1_000),
        "partial\n\nCommand was killed: the process daemon stopped"
    );
}

/// "removes expired output files across projects" (`shell-cleanup.test.ts`).
/// Adapted: CodeSmith's own tool-output directory, one folder per session;
/// every file there ages out, not only shell output.
#[test]
fn oc_shell_cleanup_removes_expired_output_files_across_sessions() {
    use codesmith_core::tool::{cleanup_outputs, OUTPUT_RETENTION};
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("first/tool_0123456789ab");
    let recent = dir.path().join("second/tool_0123456789cd");
    for f in [&old, &recent] {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "output").unwrap();
    }
    let expired = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(expired)
        .unwrap();
    cleanup_outputs(dir.path(), OUTPUT_RETENTION);
    assert!(!old.exists());
    assert!(recent.exists());
}
