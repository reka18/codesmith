//! A saved Claude Code token reaches every `claude` CodeSmith starts: the
//! real launch environment carries it in `CLAUDE_CODE_OAUTH_TOKEN`, with
//! the credentials Claude Code would rank above it left out. Its own test
//! binary, since the token source is set once per process.

use codesmith_ai::claude_agent::{query::Launch, set_token_source, token, Io};

const TOKEN: &str =
    "sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-xyzAA";

fn saved() -> Option<String> {
    Some(TOKEN.to_string())
}

#[test]
fn the_real_launch_environment_carries_the_saved_token() {
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-api03-would-outrank");
    std::env::set_var("ANTHROPIC_AUTH_TOKEN", "bearer-would-outrank");
    set_token_source(saved);
    let env = (Io::real().env)();
    assert_eq!(env.get(token::VAR).map(String::as_str), Some(TOKEN));
    assert!(!env.contains_key("ANTHROPIC_API_KEY"));
    assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
    // What the process is started with keeps it after CodeSmith's own
    // settings are applied.
    let launch = Launch {
        env,
        ..Default::default()
    };
    let started = launch.environment();
    assert_eq!(started.get(token::VAR).map(String::as_str), Some(TOKEN));
    // And a program started with that environment sees it.
    #[cfg(unix)]
    {
        let out = std::process::Command::new("/usr/bin/env")
            .env_clear()
            .envs(&started)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains(&format!("{}={TOKEN}", token::VAR)));
        assert!(!text.contains("would-outrank"));
    }
}
