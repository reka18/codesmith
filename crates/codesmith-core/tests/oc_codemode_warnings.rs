//! Port of OpenCode's `tool-execute.test.ts` (MIT) "a program that only
//! warns shows just its warnings": a promise a program rejects and never
//! awaits or handles is reported as a warning rather than dropped, with
//! guardrails on what counts and how much is shown.

use codesmith_core::codemode::{self, Entry};
use serde_json::{json, Value};
use std::time::Duration;

fn entry(namespace: &str, name: &str) -> Entry {
    Entry {
        tool: format!("{namespace}_{name}"),
        namespace: namespace.into(),
        name: name.into(),
        description: format!("The {name} tool"),
        schema: json!({"type": "object", "properties": {}}),
        output: None,
        freeform: false,
    }
}

/// Run `code` in this process; `x_lying` fails, every other tool returns
/// its name.
async fn run(code: &str) -> codemode::Outcome {
    codemode::execute_with(
        None,
        code,
        &[entry("x", "lying"), entry("x", "fine")],
        50,
        Duration::from_secs(10),
        |tool, _| async move {
            if tool == "x_lying" {
                Err("transport died".to_string())
            } else {
                Ok(tool)
            }
        },
    )
    .await
}

const PREFIX: &str = "Unhandled rejection from an un-awaited promise: ";

// tool-execute.test.ts: "a program that only warns shows just its
// warnings" (adapted: CodeSmith's result shows its value, null here, and
// the warning has no diagnostic kind)
#[tokio::test(flavor = "multi_thread")]
async fn oc_execute_a_program_that_only_warns_shows_its_warnings() {
    let out = run(r#"const rejected = Promise.reject(new Error("boom"))"#).await;
    assert_eq!(out.result, Ok(Value::Null));
    assert!(out.logs.is_empty());
    assert_eq!(out.warnings, [format!("{PREFIX}Error: boom")]);
    let (text, failed) = out.render();
    assert!(!failed, "a warning does not fail the program");
    assert!(
        text.contains(&format!("Warnings:\n- {PREFIX}Error: boom")),
        "{text}"
    );
}

// A rejection that is awaited, caught or handled later in the program is
// no warning.
#[tokio::test(flavor = "multi_thread")]
async fn handled_rejections_are_not_warnings() {
    for code in [
        r#"const p = Promise.reject(new Error("x")); try { await p } catch {} return 1"#,
        r#"Promise.reject(new Error("x")).catch(() => {}); return 1"#,
        r#"const p = Promise.reject(1); await null; await null; p.then(null, () => {}); return 1"#,
        r#"await Promise.allSettled([Promise.reject(1), Promise.resolve(2)]); return 1"#,
        r#"try { await tools.x.lying({}) } catch (e) {} return 1"#,
        r#"return await tools.x.fine({})"#,
    ] {
        let out = run(code).await;
        assert!(out.result.is_ok(), "{code}: {:?}", out.result);
        assert!(out.warnings.is_empty(), "{code}: {:?}", out.warnings);
    }
}

// The program's own failure is its result, not a warning.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_program_is_an_error_not_a_warning() {
    for code in [
        r#"throw new Error("top")"#,
        r#"await Promise.reject(new Error("top"))"#,
        r#"return await tools.x.lying({})"#,
    ] {
        let out = run(code).await;
        assert!(out.result.is_err(), "{code}");
        assert!(out.warnings.is_empty(), "{code}: {:?}", out.warnings);
        assert!(!out.render().0.contains("Warnings:"), "{code}");
    }
}

// Calls and async functions left running that fail are warnings, beside
// the program's result.
#[tokio::test(flavor = "multi_thread")]
async fn forgotten_failures_are_warnings() {
    let out = run(r#"tools.x.lying({}); return "done""#).await;
    assert_eq!(out.result, Ok(json!("done")));
    assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    assert!(out.warnings[0].starts_with(PREFIX), "{:?}", out.warnings);
    assert!(
        out.warnings[0].contains("transport died"),
        "{:?}",
        out.warnings
    );

    let out = run(r#"(async () => { throw new TypeError("inner") })(); return 2"#).await;
    assert_eq!(out.result, Ok(json!(2)));
    assert_eq!(out.warnings, [format!("{PREFIX}TypeError: inner")]);
}

// A rejected promise the program returns is its result, not a warning.
#[tokio::test(flavor = "multi_thread")]
async fn a_returned_rejection_is_the_result() {
    let out = run("Promise.reject(new Error('last'))").await;
    assert!(out.result.unwrap_err().contains("last"));
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
}

// Odd rejection values: not an Error, empty, unicode and control
// characters (shown escaped, newlines kept).
#[tokio::test(flavor = "multi_thread")]
async fn odd_rejection_values_are_shown_safely() {
    let cases: [(&str, &str); 6] = [
        ("Promise.reject(undefined)", "undefined"),
        ("Promise.reject('')", ""),
        ("Promise.reject({ a: 1 })", "[object Object]"),
        ("Promise.reject(42)", "42"),
        ("Promise.reject('é ✓ 漢字')", "é ✓ 漢字"),
        ("Promise.reject('a\\nb')", "a\nb"),
    ];
    for (code, shown) in cases {
        let out = run(&format!("{code}; return 0")).await;
        assert_eq!(out.warnings, [format!("{PREFIX}{shown}")], "{code}");
    }
    let out = run("Promise.reject('bell\\u0007esc\\u001b[31m'); return 0").await;
    let w = &out.warnings[0];
    assert!(!w.contains('\u{7}') && !w.contains('\u{1b}'), "{w:?}");
    assert!(w.contains("bell\\u{7}esc\\u{1b}[31m"), "{w:?}");
}

// A huge reason is cut, and many rejections are counted past the first
// ten, so the result stays small.
#[tokio::test(flavor = "multi_thread")]
async fn warnings_are_bounded() {
    let out = run("Promise.reject('x'.repeat(100000)); return 0").await;
    assert_eq!(out.warnings.len(), 1);
    let chars = out.warnings[0].chars().count();
    assert!(chars <= 501, "{chars}");
    assert!(out.warnings[0].ends_with('…'));

    let out =
        run("for (let i = 0; i < 25; i++) Promise.reject(new Error('e' + i)); return 0").await;
    assert_eq!(out.warnings.len(), 11, "{:?}", out.warnings);
    assert_eq!(out.warnings[0], format!("{PREFIX}Error: e0"));
    assert_eq!(out.warnings[9], format!("{PREFIX}Error: e9"));
    assert_eq!(out.warnings[10], "… and 15 more unhandled rejections");

    let out = run("for (let i = 0; i < 11; i++) Promise.reject(i); return 0").await;
    assert_eq!(out.warnings[10], "… and 1 more unhandled rejection");
}

// The same program warns the same way every run.
#[tokio::test(flavor = "multi_thread")]
async fn warnings_are_deterministic() {
    let code = "Promise.reject(1); Promise.reject(2); Promise.reject(3); return 0";
    let first = run(code).await.warnings;
    for _ in 0..3 {
        assert_eq!(run(code).await.warnings, first);
    }
    assert_eq!(
        first,
        [1, 2, 3].map(|n| format!("{PREFIX}{n}")).to_vec(),
        "in the order they were rejected"
    );
}
