//! Command-line tests ported from OpenCode's `packages/cli/test` (MIT) that
//! need the real binary. Each names the OpenCode file and test it comes
//! from; see `crates/codesmith/PARITY.md`.

use crate::harness::*;
use crate::model::*;
use serde_json::{json, Value};
use std::process::Stdio;

async fn run_env(e: &Env, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> Out {
    use tokio::io::AsyncWriteExt;
    let mut c = e.command(args);
    c.envs(env.iter().copied());
    if stdin.is_some() {
        c.stdin(Stdio::piped());
    }
    let mut child = c.spawn().unwrap();
    if let Some(input) = stdin {
        let mut s = child.stdin.take().unwrap();
        s.write_all(input.as_bytes()).await.unwrap();
    }
    let o = child.wait_with_output().await.unwrap();
    Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

// debug-paths.test.ts: "prints resolved global paths without starting a
// server".
#[tokio::test(flavor = "multi_thread")]
async fn oc_debug_paths_prints_the_resolved_folders() {
    let e = Env::new();
    let out = e.run(&["debug", "paths"]).await;
    out.ok();
    let config = e.config_dir.display().to_string();
    let data = e.data.display().to_string();
    assert!(
        out.stdout.contains(&config) && out.stdout.contains(&data),
        "{}",
        out.both()
    );
}

// mini.test.ts: "uses piped stdin as the initial prompt" (with a message as
// well, see run_cli.rs `piped_stdin_attached_files_and_titles`).
#[tokio::test(flavor = "multi_thread")]
async fn oc_piped_stdin_alone_is_the_prompt() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Piped").then(
        "answer",
        M::tools().and(M::prompt("only from stdin")),
        Reply::text("Got it."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let out = e.run_with_stdin(&["run"], "only from stdin\n").await;
    out.ok();
    assert_eq!(out.stdout.trim(), "Got it.");
    m.assert_done();
}

// import-export.test.ts: "export reports a missing session without a stack
// trace".
#[tokio::test(flavor = "multi_thread")]
async fn oc_export_reports_a_missing_session_without_a_stack_trace() {
    let e = Env::new();
    let out = e.run(&["session", "export", "ses_missing"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(out.stderr.contains("ses_missing"), "{}", out.both());
    assert!(
        !out.stderr.contains("panicked") && !out.stderr.contains("stack backtrace"),
        "{}",
        out.both()
    );
    assert!(out.stdout.is_empty());
}

/// A finished one-turn session; returns its id.
async fn one_session(e: &Env) -> String {
    let m = Model::start(Script::new().titled("Hello").then(
        "answer",
        M::tools().and(M::prompt("hello")),
        Reply::text("Hi."),
    ))
    .await;
    e.config(&m.url, json!({}));
    e.run(&["run", "hello"]).await.ok();
    e.store().list(None, false).unwrap()[0].id.clone()
}

// import-export.test.ts: "export is raw by default and supports explicit
// sanitization" and
// "import sends the file unchanged to the resolved location" (adapted:
// imported into a fresh store, it exports the same).
#[tokio::test(flavor = "multi_thread")]
async fn oc_export_is_raw_and_import_keeps_it_unchanged() {
    let e = Env::new();
    let id = one_session(&e).await;
    let first = e.run(&["session", "export", &id]).await;
    first.ok();
    let v: Value = serde_json::from_str(&first.stdout).unwrap();
    assert!(first.stdout.contains("hello") && first.stdout.contains("Hi."));
    assert_eq!(v["info"]["id"], json!(id));

    let other = Env::new();
    std::fs::write(other.path("s.json"), &first.stdout).unwrap();
    other.run(&["session", "import", "s.json"]).await.ok();
    let again = other.run(&["session", "export", &id]).await;
    again.ok();
    assert_eq!(serde_json::from_str::<Value>(&again.stdout).unwrap(), v);

    // --sanitize keeps the shape and none of the content, and still imports.
    let clean = e.run(&["session", "export", &id, "--sanitize"]).await;
    clean.ok();
    assert!(
        !clean.stdout.contains("hello") && !clean.stdout.contains("Hi."),
        "{}",
        clean.stdout
    );
    assert!(clean
        .stdout
        .contains(&format!("[redacted:session-title:{id}]")));
    let s: Value = serde_json::from_str(&clean.stdout).unwrap();
    assert_eq!(s["info"]["id"], json!(id));
    assert_eq!(
        s["entries"].as_array().unwrap().len(),
        v["entries"].as_array().unwrap().len()
    );
    let third = Env::new();
    std::fs::write(third.path("s.json"), &clean.stdout).unwrap();
    third.run(&["session", "import", "s.json"]).await.ok();
}

// import-export.test.ts: "import reports an existing session without a stack
// trace".
#[tokio::test(flavor = "multi_thread")]
async fn oc_import_reports_an_existing_session_without_a_stack_trace() {
    let e = Env::new();
    let id = one_session(&e).await;
    let out = e.run(&["session", "export", &id]).await;
    std::fs::write(e.path("s.json"), &out.stdout).unwrap();
    let out = e.run(&["session", "import", "s.json"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(out.stderr.contains("already exists"), "{}", out.both());
    assert!(!out.stderr.contains("panicked"), "{}", out.both());
}

// Sign in with ChatGPT never registers a client, nor borrows one, without
// the opt-in; `--register` goes only with `--oauth` for openai. Each case
// fails before any request is sent.
#[tokio::test(flavor = "multi_thread")]
async fn chatgpt_sign_in_registers_only_with_the_opt_in() {
    let e = Env::new();
    let out = e.run(&["auth", "login", "openai", "--oauth"]).await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert!(out.stderr.contains("--register"), "{}", out.both());
    assert!(!out.stderr.contains("Open this URL"), "{}", out.both());
    let out = e.run(&["auth", "login", "openai", "--register"]).await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert!(out.stderr.contains("--oauth"), "{}", out.both());
    let out = e
        .run(&["auth", "login", "poe", "--oauth", "--register"])
        .await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert!(
        out.stderr.contains("--register is for openai"),
        "{}",
        out.both()
    );
    let out = e
        .run(&[
            "auth",
            "login",
            "openai",
            "--oauth",
            "--register",
            "--client-id",
            "oaiapp_x",
        ])
        .await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    assert!(out.stderr.contains("Give one of them"), "{}", out.both());
}

// auth.test.ts: "lists stored and environment connections" (adapted:
// CodeSmith lists where each provider's key comes from).
#[tokio::test(flavor = "multi_thread")]
async fn oc_auth_list_shows_stored_and_environment_keys() {
    let e = Env::new();
    run_env(
        &e,
        &["auth", "login", "openai"],
        &[],
        Some("sk-saved-123456\n"),
    )
    .await
    .ok();
    let out = run_env(
        &e,
        &["auth", "list"],
        &[("ANTHROPIC_API_KEY", "sk-env")],
        None,
    )
    .await;
    out.ok();
    let line = |id: &str| {
        out.stdout
            .lines()
            .find(|l| l.split_whitespace().next() == Some(id))
            .unwrap_or_default()
            .to_string()
    };
    assert!(
        line("anthropic").contains("$ANTHROPIC_API_KEY"),
        "{}",
        out.both()
    );
    assert!(line("openai").contains("saved"), "{}", out.both());
    assert!(!out.stdout.contains("sk-saved-123456"), "keys are masked");
}

// auth.test.ts: "removes a selected stored credential".
#[tokio::test(flavor = "multi_thread")]
async fn oc_auth_logout_removes_a_stored_key() {
    let e = Env::new();
    run_env(
        &e,
        &["auth", "login", "openai"],
        &[],
        Some("sk-saved-123456\n"),
    )
    .await
    .ok();
    let out = e.run(&["auth", "logout", "openai"]).await;
    out.ok();
    assert!(out.stdout.contains("Removed"), "{}", out.both());
    let out = e.run(&["auth", "list"]).await;
    let openai = out
        .stdout
        .lines()
        .find(|l| l.starts_with("openai"))
        .unwrap_or_default();
    assert!(!openai.contains("saved"), "{}", out.both());
}

// env.test.ts: "session environment omits server credentials" (adapted:
// CodeSmith's server token, `$CODESMITH_SERVER_TOKEN`, never reaches a
// session's shell commands; other variables do).
#[tokio::test(flavor = "multi_thread")]
async fn oc_shell_commands_do_not_see_the_server_token() {
    let e = Env::new();
    let ask = M::tools().and(M::prompt("print it"));
    let m = Model::start(
        Script::new()
            .titled("Env")
            .then(
                "shell",
                ask.clone().and(M::step(0)),
                Reply::call(
                    "shell",
                    json!({"command": "echo \"[$CODESMITH_SERVER_TOKEN][$SESSION_ENV_TEST]\"", "description": "env"}),
                ),
            )
            .then("done", ask.and(M::step(1)), Reply::text("Done.")),
    )
    .await;
    e.config(&m.url, json!({}));
    let out = run_env(
        &e,
        &["run", "--auto", "print it"],
        &[
            ("CODESMITH_SERVER_TOKEN", "secret-token"),
            ("SESSION_ENV_TEST", "included"),
        ],
        None,
    )
    .await;
    out.ok();
    m.assert_done();
    let results: Vec<String> = m.seen().iter().flat_map(|s| results(&s.body)).collect();
    assert!(
        results.iter().any(|r| r.contains("[][included]")),
        "{results:?}"
    );
}

/// `debug paths` with this world's environment, `set` on top and `unset`
/// removed.
async fn paths_with(e: &Env, set: &[(&str, String)], unset: &[&str]) -> Vec<(String, String)> {
    let mut c = e.command(&["debug", "paths"]);
    for (k, v) in set {
        c.env(k, v);
    }
    for k in unset {
        c.env_remove(k);
    }
    let o = c.output().await.unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(char::is_whitespace)?;
            Some((k.to_string(), v.trim().to_string()))
        })
        .collect()
}

// util global-roots.test.ts: "uses XDG overrides" (CodeSmith keeps its
// temporary files in the data folder).
#[tokio::test(flavor = "multi_thread")]
async fn oc_paths_use_xdg_overrides() {
    let e = Env::new();
    let root = e.root.join("xdg");
    let p = |s: &str| root.join(s).display().to_string();
    let got = paths_with(
        &e,
        &[
            ("XDG_CONFIG_HOME", p("config")),
            ("XDG_DATA_HOME", p("data")),
        ],
        &["CODESMITH_CONFIG_DIR"],
    )
    .await;
    let want = [
        ("config", p("config/codesmith")),
        ("data", p("data/codesmith")),
        ("output", p("data/codesmith/tool-output")),
        ("tmp", p("data/codesmith/tmp")),
    ];
    for (k, v) in want {
        assert!(got.contains(&(k.to_string(), v.clone())), "{k}: {got:?}");
    }
}

// util global-roots.test.ts: "empty XDG overrides use home directory
// defaults".
#[tokio::test(flavor = "multi_thread")]
async fn oc_empty_xdg_overrides_use_the_home_defaults() {
    let e = Env::new();
    let got = paths_with(
        &e,
        &[
            ("XDG_CONFIG_HOME", String::new()),
            ("XDG_DATA_HOME", String::new()),
        ],
        &["CODESMITH_CONFIG_DIR"],
    )
    .await;
    let home = |s: &str| e.home.join(s).display().to_string();
    assert!(
        got.contains(&("config".into(), home(".config/codesmith"))),
        "{got:?}"
    );
    assert!(
        got.contains(&("data".into(), home(".local/share/codesmith"))),
        "{got:?}"
    );
}

/// `session messages` numbers the messages, `session fork` copies a session
/// through its latest message or up to one (OpenCode's `session fork`).
#[tokio::test(flavor = "multi_thread")]
async fn session_fork_copies_history_through_or_before_a_message() {
    let e = Env::new();
    let id = one_session(&e).await;
    let listed = e.run(&["session", "messages", &id]).await;
    listed.ok();
    let user = listed
        .stdout
        .lines()
        .find(|l| l.contains("user") && l.contains("hello"))
        .unwrap_or_else(|| panic!("{}", listed.both()));
    assert!(listed.stdout.contains("Hi."), "{}", listed.both());
    let seq = user.split_whitespace().next().unwrap().to_string();

    let out = e.run(&["session", "fork", &id]).await;
    out.ok();
    let fork = out.stdout.trim().rsplit(' ').next().unwrap().to_string();
    assert!(
        out.stdout.contains(&format!("Forked {id} into ")),
        "{}",
        out.both()
    );
    let info = e.store().get(&fork).unwrap().unwrap();
    assert_eq!(info.title.as_deref(), Some("Hello (fork #1)"));
    assert_eq!(e.history(&fork), e.history(&id));

    let out = e.run(&["session", "fork", &id, "--before", &seq]).await;
    out.ok();
    let before = out.stdout.trim().rsplit(' ').next().unwrap().to_string();
    assert!(e.history(&before).is_empty());

    let out = e.run(&["session", "fork", &id, "--before", "9999"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(out.stderr.contains("no message 9999"), "{}", out.both());
}

/// `session archive` takes a session out of `session list` (and `-c`), and
/// `session list --archived` shows it; `run --archived` archives its run.
#[tokio::test(flavor = "multi_thread")]
async fn session_archive_hides_sessions_from_lists() {
    let e = Env::new();
    let id = one_session(&e).await;
    e.run(&["session", "archive", &id]).await.ok();
    let listed = e.run(&["session", "list"]).await;
    listed.ok();
    assert!(!listed.stdout.contains(&id), "{}", listed.both());
    let archived = e.run(&["session", "list", "--archived"]).await;
    assert!(archived.stdout.contains(&id), "{}", archived.both());
    e.run(&["session", "unarchive", &id]).await.ok();
    let listed = e.run(&["session", "list"]).await;
    assert!(listed.stdout.contains(&id), "{}", listed.both());
    let out = e.run(&["session", "archive", "ses_missing"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(
        out.stderr.contains("no session ses_missing"),
        "{}",
        out.both()
    );

    let m = Model::start(Script::new().titled("Again").always(
        "answer",
        M::tools(),
        Reply::text("Hi again."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "--archived", "hello"]).await;
    out.ok();
    let st = e.store();
    let newest = st.list(None, false).unwrap()[0].id.clone();
    assert_ne!(newest, id);
    assert!(st.archived(&newest).unwrap().is_some());
}

// run/archived.test.ts: "run --archived archives the new session before
// subscribing to its prompt". Adapted: the run's model call fails rather
// than its event stream, and the session it stored is archived all the
// same.
#[tokio::test(flavor = "multi_thread")]
async fn oc_run_archived_archives_the_new_session_before_its_prompt() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Down").always(
        "answer",
        M::tools(),
        Reply::status(400, r#"{"error": {"message": "bad request"}}"#),
    ))
    .await;
    e.config(&m.url, json!({}));
    let out = e.run(&["run", "--archived", "review this diff"]).await;
    assert_ne!(out.code, Some(0), "{}", out.both());
    let st = e.store();
    let all = st.list_archived(None, false, Some(true)).unwrap();
    assert_eq!(all.len(), 1, "{}", out.both());
    assert!(st
        .list_archived(None, false, Some(false))
        .unwrap()
        .is_empty());
}

// `codesmith auth login` without a provider and without a terminal: the
// providers, their states and how to sign in to each, and an error, at
// once (nothing is asked). A saved key is not shown.
#[tokio::test(flavor = "multi_thread")]
async fn auth_login_without_a_provider_or_terminal_prints_how_to_sign_in() {
    let e = Env::new();
    run_env(
        &e,
        &["auth", "login", "groq"],
        &[],
        Some("gsk-noninteractive-secret\n"),
    )
    .await
    .ok();
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_env(&e, &["auth", "login"], &[], Some("")),
    )
    .await
    .expect("auth login waited for input");
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(
        out.stdout.contains("Providers you can sign in to"),
        "{}",
        out.both()
    );
    let line = |id: &str| {
        out.stdout
            .lines()
            .find(|l| l.split_whitespace().next() == Some(id))
            .unwrap_or_default()
            .to_string()
    };
    assert!(line("groq").contains("signed in"), "{}", out.both());
    assert!(
        line("openai").contains("codesmith auth login openai --oauth"),
        "{}",
        out.both()
    );
    assert!(
        line("anthropic").contains("codesmith auth login claude-agent"),
        "{}",
        out.both()
    );
    assert!(out.stderr.contains("Pass a provider id"), "{}", out.both());
    assert!(!out.both().contains("gsk-noninteractive-secret"));
    // Options with no provider to go with are refused.
    let out = e.run(&["auth", "login", "--oauth"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(out.stderr.contains("name the provider"), "{}", out.both());
}

// `codesmith auth login claude-agent` (the sign-in picker's Claude Code
// entry) with no Claude Code installed: what to install and run, and
// nothing saved.
#[tokio::test(flavor = "multi_thread")]
async fn claude_code_login_reports_a_missing_claude_code() {
    if ["/usr/bin", "/bin"]
        .iter()
        .any(|d| std::path::Path::new(d).join("claude").exists())
    {
        eprintln!("skipped: a claude binary is in the system folders");
        return;
    }
    let e = Env::new();
    let out = run_env(
        &e,
        &["auth", "login", "claude-agent"],
        &[("PATH", "/usr/bin:/bin")],
        None,
    )
    .await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    assert!(
        out.stderr.contains("Claude Code is not installed")
            && out.stderr.contains("claude auth login"),
        "{}",
        out.both()
    );
    assert!(!e.data.join("codesmith/auth.json").exists());
}

// CodeSmith's own: a failed `codesmith upgrade` ends with the failure
// banner, after the error, plain off a terminal; other commands don't.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_upgrade_ends_with_the_failure_banner() {
    let e = Env::new();
    let out = e.run(&["upgrade", "not-a-version"]).await;
    assert_eq!(out.code, Some(1), "{}", out.both());
    let err = &out.stderr;
    let error = err.find("Invalid version").expect("the error");
    let banner = err.find("CodeSmith forge failed").expect("the banner");
    assert!(error < banner, "{err}");
    assert!(err.contains("The anvil cracked"), "{err}");
    assert!(!err.contains("Press Enter"), "nothing waits: {err}");
    assert!(err.trim_end().ends_with('╝'), "{err}");
    assert!(
        !err[banner..].contains('\x1b'),
        "no colour off a terminal: {err}"
    );
    assert!(!out.stdout.contains("forged successfully"));

    let other = e.run(&["session", "export", "ses_missing"]).await;
    assert_eq!(other.code, Some(1));
    assert!(!other.stderr.contains("forge failed"), "{}", other.both());
}
