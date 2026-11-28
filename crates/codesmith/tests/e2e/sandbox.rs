//! The operating-system sandbox from the outside: `codesmith debug
//! sandbox`, a headless run whose command the sandbox refuses (nobody to
//! ask, so the error is reported), and the once-per-session notice when
//! your config turns it off. A repository's own config can't turn it off.

use crate::harness::Env;
use crate::model::{self, Model, Reply, Script, M};
use codesmith::core::sandbox::{support, Support};
use serde_json::{json, Value};

fn supported() -> bool {
    support() == Support::Yes
}

#[tokio::test(flavor = "multi_thread")]
async fn debug_sandbox_prints_the_policy_and_ignores_a_repositorys_config() {
    let e = Env::new();
    // A repository's own files can't turn the sandbox off.
    e.write(
        "codesmith.json",
        r#"{"sandbox": {"enabled": false, "unsupported": "run"}}"#,
    );
    e.write(
        ".codesmith/codesmith.json",
        r#"{"sandbox": {"enabled": false}}"#,
    );
    let out = e.run(&["debug", "sandbox"]).await;
    out.ok();
    let s = &out.stdout;
    let project = e.project.display().to_string();
    assert!(s.contains("supported: "), "{s}");
    assert!(s.contains("network: all"), "{s}");
    assert!(s.contains(&format!("  {project}\n")), "{s}");
    assert!(s.contains("/.ssh\n"), "{s}");
    if supported() {
        assert!(s.contains("commands: sandboxed"), "{s}");
    } else {
        assert!(s.contains("commands: refused"), "{s}");
        assert!(s.contains(r#""unsupported": "run""#), "{s}");
    }
    // Your own config can.
    std::fs::write(
        e.config_dir.join("codesmith.json"),
        r#"{"sandbox": {"enabled": false, "network": "none"}}"#,
    )
    .unwrap();
    let out = e.run(&["debug", "sandbox", "--profile"]).await;
    out.ok();
    assert!(
        out.stdout.contains("commands: run without the sandbox"),
        "{}",
        out.both()
    );
    assert!(out.stdout.contains("network: none"), "{}", out.both());
    assert!(out.stdout.contains("(deny default)"), "{}", out.both());
}

fn write_outside(e: &Env) -> std::path::PathBuf {
    let out = e.root.join("outside");
    std::fs::create_dir_all(&out).unwrap();
    e.write("p", &out.display().to_string());
    out
}

fn script() -> Script {
    let go = M::tools().and(M::prompt("write it"));
    Script::new()
        .titled("Write")
        .then(
            "write",
            go.clone().and(M::step(0)),
            Reply::call(
                "shell",
                json!({"command": r#"echo pwn > "$(cat p)/f""#, "description": "write"}),
            ),
        )
        .then("done", go.and(M::step(1)), Reply::text("Done."))
}

/// Every rule allows the command; the sandbox still refuses the write, and
/// with no terminal to ask the run reports it to the model.
#[tokio::test(flavor = "multi_thread")]
async fn a_headless_run_reports_a_write_the_sandbox_refused() {
    let e = Env::new();
    let out_dir = write_outside(&e);
    let m = Model::start(script()).await;
    e.config(
        &m.url,
        json!({"permissions": [{"action": "*", "resource": "*", "effect": "allow"}]}),
    );
    let out = e.run(&["run", "--format", "json", "write it"]).await;
    out.ok();
    assert!(!out_dir.join("f").exists(), "{}", out.both());
    let ev = out.events();
    let result: &Value = ev.iter().find(|x| x["type"] == "tool_result").unwrap();
    let text = result["output"].as_str().unwrap();
    if supported() {
        assert!(text.contains("The sandbox refused a write to"), "{text}");
        assert!(model::results(&m.by("done")[0])[0].contains("sandbox"));
    } else {
        assert_eq!(result["is_error"], true);
        assert!(text.contains(r#""unsupported": "run""#), "{text}");
    }
}

/// Turned off in your config: the command writes where it likes, and the
/// run says once that the sandbox is off.
#[tokio::test(flavor = "multi_thread")]
async fn turned_off_the_run_says_so_once() {
    let e = Env::new();
    let out_dir = write_outside(&e);
    let m = Model::start(script()).await;
    e.config(
        &m.url,
        json!({
            "permissions": [{"action": "*", "resource": "*", "effect": "allow"}],
            "sandbox": {"enabled": false}
        }),
    );
    let out = e.run(&["run", "--format", "json", "write it"]).await;
    out.ok();
    assert!(out_dir.join("f").exists(), "{}", out.both());
    let notices: Vec<Value> = out
        .events()
        .into_iter()
        .filter(|x| {
            x["type"] == "notice"
                && x["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("sandbox is off")
        })
        .collect();
    assert_eq!(notices.len(), 1, "{}", out.both());
}
