//! Model swaps from the CLI: `codesmith models PROVIDER` lists what a
//! provider's server can load, and `codesmith models swap PROVIDER MODEL`
//! loads one, through the fake `list`/`use` script in codesmith-core's
//! fixtures (bounded; every branch exits). Only the global config may set
//! `swap`, and a swap never starts without a terminal or from inside a
//! CodeSmith shell.

use crate::harness::{Env, Out, BIN, TIMEOUT};
use serde_json::{json, Value};

const FAKE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../codesmith-core/tests/fixtures/swap/fake-swap.sh"
);

fn provider(swap: Value) -> Value {
    json!({"providers": {"stable": {
        "name": "Stable",
        "protocol": "openai-chat",
        // Nothing listens here: the server's own listing fails at once.
        "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "k"},
        "swap": swap
    }}})
}

fn fake() -> Value {
    json!({"list": ["/bin/sh", FAKE, "list"], "use": ["/bin/sh", FAKE, "use", "{model}"], "timeout": 30})
}

fn write(path: &std::path::Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
}

async fn run_env(e: &Env, args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut c = e.command(args);
    c.envs(env.iter().copied());
    let o = tokio::time::timeout(TIMEOUT, c.output())
        .await
        .expect("timed out")
        .unwrap();
    Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn models_lists_what_the_server_can_load() {
    let e = Env::new();
    write(&e.config_dir.join("codesmith.json"), &provider(fake()));
    let o = e.run(&["models", "stable"]).await;
    o.ok();
    assert!(
        o.stdout
            .contains("stable can load (`codesmith models swap stable MODEL`"),
        "{}",
        o.both()
    );
    assert!(
        o.stdout.contains("stable/small  (live, configured"),
        "{}",
        o.both()
    );
    assert!(o.stdout.contains("stable/big"), "{}", o.both());
    assert!(o.stdout.contains("stable/huge"), "{}", o.both());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_cannot_set_a_swap() {
    let e = Env::new();
    // The project's swap would run `touch` in the project.
    let evil = json!({"list": ["/bin/echo", "big"], "use": ["/usr/bin/touch", "{model}"]});
    write(&e.config_dir.join("codesmith.json"), &provider(json!(null)));
    write(&e.path("codesmith.json"), &provider(evil.clone()));
    write(&e.path(".codesmith/codesmith.json"), &provider(evil));
    let o = e.run(&["models", "stable"]).await;
    assert!(!o.stdout.contains("can load"), "{}", o.both());
    let o = e.run(&["models", "swap", "stable", "big"]).await;
    assert_ne!(o.code, Some(0));
    assert!(
        o.stderr
            .contains("only read from the global codesmith.json"),
        "{}",
        o.both()
    );
    assert!(!e.path("big").exists(), "nothing ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_swap_needs_a_terminal_and_never_runs_from_a_codesmith_shell() {
    let e = Env::new();
    let marker = json!({"list": ["/bin/echo", "big"], "use": ["/usr/bin/touch", "{model}"]});
    write(&e.config_dir.join("codesmith.json"), &provider(marker));
    let o = e.run(&["models", "swap", "stable", "big"]).await;
    assert_ne!(o.code, Some(0));
    assert!(
        o.stderr.contains("runs only from a terminal"),
        "{}",
        o.both()
    );
    for var in ["CODESMITH", "CODESMITH_SESSION_ID"] {
        let o = run_env(&e, &["models", "swap", "stable", "big"], &[(var, "1")]).await;
        assert_ne!(o.code, Some(0));
        assert!(
            o.stderr.contains("inside a CodeSmith session's shell"),
            "{}",
            o.both()
        );
    }
    assert!(!e.path("big").exists(), "nothing ran");
}

/// `codesmith models swap` in a pseudo-terminal (`script(1)`).
async fn swap_in_a_terminal(e: &Env, model: &str) -> (bool, String) {
    let inner = format!("exec '{BIN}' models swap stable {model}");
    let args: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["-q", "/dev/null", "sh", "-c", &inner]
    } else {
        vec!["-qfec", &inner, "/dev/null"]
    };
    let o = tokio::time::timeout(
        TIMEOUT,
        tokio::process::Command::new("/usr/bin/script")
            .args(&args)
            .current_dir(&e.project)
            .env_clear()
            .envs(e.vars())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("timed out")
    .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).replace('\r', ""),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_swap_from_a_terminal_loads_the_model_or_says_why_not() {
    if !std::path::Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: script(1) is not installed");
        return;
    }
    let e = Env::new();
    write(&e.config_dir.join("codesmith.json"), &provider(fake()));
    let (ok, out) = swap_in_a_terminal(&e, "big").await;
    assert!(ok, "{out}");
    assert!(out.contains("loading big on stable's server"), "{out}");
    assert!(out.contains("  loading big"), "{out}");
    assert!(out.contains("stable/big\n"), "{out}");
    assert!(
        !out.contains("\x1b[1m"),
        "control sequences are stripped: {out:?}"
    );

    let (ok, out) = swap_in_a_terminal(&e, "huge").await;
    assert!(!ok, "{out}");
    assert!(out.contains("huge does not fit"), "{out}");
    assert!(out.contains("3460000000 bytes (3.46 GB) short"), "{out}");

    let (ok, out) = swap_in_a_terminal(&e, "not-listed").await;
    assert!(!ok, "{out}");
    assert!(
        out.contains("not one of the models the server lists"),
        "{out}"
    );
}
