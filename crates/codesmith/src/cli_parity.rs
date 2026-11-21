//! Command-line tests ported from OpenCode's `packages/cli/test` (MIT)
//! where CodeSmith's commands behave the same. Each test names the OpenCode
//! file and test it comes from. See `crates/codesmith/PARITY.md`.

use super::*;
use clap::CommandFactory;

fn run(args: &[&str]) -> Result<RunArgs, clap::Error> {
    let argv = ["codesmith", "run"].iter().chain(args).copied();
    match Cli::try_parse_from(argv)?.command {
        Some(Command::Run(r)) => Ok(r),
        _ => unreachable!("run parses as run"),
    }
}

// flag.test.ts: "parses $args", one case per row. CodeSmith's `run` has the
// same optional-value `--worktree` (no name is `""`, OpenCode's `true`).
#[test]
fn oc_flag_parses_an_optional_worktree_value() {
    let none: Option<&str> = None;
    let cases: &[(&[&str], Option<&str>, &[&str])] = &[
        (&[], none, &[]),
        (&["--worktree"], Some(""), &[]),
        (&["-w"], Some(""), &[]),
        (&["--worktree="], Some(""), &[]),
        (&["--worktree=feature"], Some("feature"), &[]),
        (&["-w=feature"], Some("feature"), &[]),
        (&["-w", "fix it"], Some(""), &["fix it"]),
        (&["--worktree", "fix", "it"], Some(""), &["fix", "it"]),
        (&["fix", "-w", "it"], Some(""), &["fix", "it"]),
        (
            &["--worktree=feature", "fix", "it"],
            Some("feature"),
            &["fix", "it"],
        ),
        (&["-w", "--", "-w", "fix"], Some(""), &["-w", "fix"]),
        (&["--", "--worktree=feature"], none, &["--worktree=feature"]),
    ];
    for (args, worktree, message) in cases {
        let r = run(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        let msg: Vec<&str> = r.message.iter().map(String::as_str).collect();
        assert_eq!(
            (r.worktree.as_deref(), msg.as_slice(), r.model.as_deref()),
            (*worktree, *message, None),
            "{args:?}"
        );
    }
}

// flag.test.ts: "leaves the next flag and its value alone" (CodeSmith's
// `run` has no `--fork`; `--auto` stands in for the boolean flag).
#[test]
fn oc_flag_leaves_the_next_flag_and_its_value_alone() {
    let r = run(&["-w", "--model", "openai/gpt", "--auto", "hi"]).unwrap();
    assert_eq!(r.worktree.as_deref(), Some(""));
    assert_eq!(r.model.as_deref(), Some("openai/gpt"));
    assert!(r.auto);
    assert_eq!(r.message, ["hi"]);
}

// flag.test.ts: "rejects repeated flags: $args", one case per row.
#[test]
fn oc_flag_rejects_repeated_flags() {
    for args in [
        &["-w", "--worktree=feature"][..],
        &["--worktree=feature", "-w"],
        &["--model", "a", "--model", "b"],
    ] {
        assert!(run(args).is_err(), "{args:?}");
    }
}

fn help(path: &[&str]) -> String {
    let mut cmd = Cli::command();
    let mut sub = &mut cmd;
    for p in path {
        sub = sub.find_subcommand_mut(p).expect("subcommand");
    }
    sub.render_help().to_string()
}

// flag.test.ts: "renders the value as optional in help".
#[test]
fn oc_flag_renders_the_value_as_optional_in_help() {
    let h = help(&["run"]);
    assert!(h.contains("-w, --worktree[=<NAME>]"), "{h}");
    assert!(help(&[]).contains("-w, --worktree[=<NAME>]"));
}

// mini.test.ts: "keeps option-like prompt text after the argument separator".
#[test]
fn oc_option_like_prompt_text_after_the_separator_is_the_message() {
    let r = run(&["--", "--foo"]).unwrap();
    assert_eq!(r.message, ["--foo"]);
}

// mini.test.ts: "parses model variants from the model reference".
#[test]
fn oc_model_references_carry_nested_ids_and_variants() {
    let c: config::Config = serde_json::from_value(serde_json::json!({
        "providers": {"openrouter": {"protocol": "openai-chat", "settings": {"baseURL": "http://x/v1"}}}
    }))
    .unwrap();
    let m = c.resolve_model("openrouter/openai/gpt-5#high").unwrap();
    assert_eq!(
        (
            m.provider_id.as_str(),
            m.model_id.as_str(),
            m.variant.as_deref()
        ),
        ("openrouter", "openai/gpt-5", Some("high"))
    );
}

// auth.test.ts: "registers authentication commands" (adapted: CodeSmith's
// subcommands save, list and remove API keys).
#[test]
fn oc_auth_registers_its_commands() {
    let h = help(&["auth"]);
    for sub in ["login", "list", "logout"] {
        assert!(h.contains(sub), "{h}");
    }
    assert!(help(&["auth", "login"]).contains("[PROVIDER]"));
    assert!(help(&["auth", "logout"]).contains("<PROVIDER>"));
}

// auth.test.ts: "requires a target outside an interactive terminal" (the
// `acp --login` half is a gap). A provider may be left out: in a terminal
// the sign-in picker asks for one; without one, `auth login` prints the
// providers and fails (e2e `oc_cli::auth_login_without_a_provider_or_terminal_prints_how_to_sign_in`).
#[test]
fn oc_auth_login_requires_a_target() {
    let Some(Command::Auth {
        what: AuthCmd::Login { provider, .. },
    }) = Cli::try_parse_from(["codesmith", "auth", "login"])
        .expect("the provider is optional")
        .command
    else {
        panic!("not auth login")
    };
    assert_eq!(provider, None);
}

// import-export.test.ts: "export requires a session outside an interactive
// terminal".
#[test]
fn oc_export_requires_a_session() {
    let e = Cli::try_parse_from(["codesmith", "session", "export"])
        .err()
        .expect("a session id is required");
    assert_eq!(e.kind(), clap::error::ErrorKind::MissingRequiredArgument);
}

// debug-paths.test.ts and debug-config.test.ts: "is included in
// troubleshooting help".
#[test]
fn oc_debug_paths_and_config_are_in_the_debug_help() {
    let h = help(&["debug"]);
    assert!(h.contains("paths") && h.contains("config"), "{h}");
}

// mcp-auth-choices.test.ts: "offers only OAuth-capable MCP servers by their
// server identity". Adapted: CodeSmith does not connect to list them, so
// the hint comes from the saved tokens (expired with no refresh token),
// not a live status, and servers are named by their config key.
#[test]
fn oc_mcp_auth_offers_only_oauth_capable_servers_by_their_server_identity() {
    use codesmith_core::mcp::{oauth, ServerConfig};
    let servers: std::collections::BTreeMap<String, ServerConfig> =
        serde_json::from_value(serde_json::json!({
            "Linear": {"type": "remote", "url": "https://linear.example/mcp"},
            "Local": {"type": "local", "command": ["true"]},
            "API key only": {"type": "remote", "url": "https://key.example/mcp", "oauth": false},
            "GitHub": {"type": "remote", "url": "https://github.example/mcp"},
        }))
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("mcp-auth.json");
    oauth::save(
        &store,
        "Linear",
        &oauth::Entry {
            server_url: "https://linear.example/mcp".into(),
            client_id: "c".into(),
            client_secret: None,
            token_endpoint: "https://linear.example/token".into(),
            access_token: "t".into(),
            refresh_token: None,
            expires_at: Some(1),
        },
    )
    .unwrap();
    let choices = mcp_auth_choices(&servers, &store);
    assert_eq!(
        choices,
        [
            AuthChoice {
                value: "GitHub".into(),
                label: "GitHub".into(),
                connected: false,
                hint: None
            },
            AuthChoice {
                value: "Linear".into(),
                label: "Linear".into(),
                connected: true,
                hint: Some("needs authentication")
            },
        ]
    );
}

// mcp-auth-choices.test.ts: "mcp auth accepts an optional server name and
// rejects no-name noninteractive calls before connecting" (the parsing
// half; the refusal is checked through the binary in tests/e2e/mcp.rs).
#[test]
fn oc_mcp_auth_accepts_an_optional_server_name() {
    let parse = |args: &[&str]| {
        let argv = ["codesmith", "mcp", "auth"].iter().chain(args).copied();
        match Cli::try_parse_from(argv).unwrap().command {
            Some(Command::Mcp {
                what: McpCmd::Auth { name },
            }) => name,
            _ => unreachable!(),
        }
    };
    assert_eq!(parse(&[]), None);
    assert_eq!(parse(&["linear"]).as_deref(), Some("linear"));
    let mut cmd = Cli::command();
    let help = cmd
        .find_subcommand_mut("mcp")
        .unwrap()
        .find_subcommand_mut("auth")
        .unwrap()
        .render_usage()
        .to_string();
    assert!(help.contains("[NAME]"), "{help}");
}
