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
