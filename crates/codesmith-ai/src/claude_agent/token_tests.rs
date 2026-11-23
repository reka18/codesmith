use super::*;

const TOKEN: &str =
    "sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-xyzAA";

/// What `claude setup-token` prints around a token, as `script` records
/// it: colours, carriage returns, the token wrapped at `width`.
fn record(token: &str, width: usize, indent: &str) -> String {
    let wrapped: Vec<String> = token
        .as_bytes()
        .chunks(width)
        .map(|c| {
            format!(
                "{indent}\x1b[1m{}\x1b[22m\r\n",
                std::str::from_utf8(c).unwrap()
            )
        })
        .collect();
    format!(
        "\x1b]0;claude\x07\x1b[2J\x1b[32m✓ Long-lived authentication token created\x1b[39m\r\n\r\n\
         Your OAuth token (valid for 1 year):\r\n\r\n{}\r\n\
         \x1b[33mStore this token securely. You won't be able to see it again.\x1b[39m\r\n\
         Use this token by setting: export CLAUDE_CODE_OAUTH_TOKEN=<token>\r\n",
        wrapped.concat()
    )
}

#[test]
fn the_token_is_read_from_between_its_lines() {
    assert_eq!(parse(&record(TOKEN, 200, "")).as_deref(), Some(TOKEN));
}

#[test]
fn a_token_wrapped_by_a_narrow_terminal_is_joined() {
    for width in [10, 33, 40, 79] {
        for indent in ["", "  "] {
            assert_eq!(
                parse(&record(TOKEN, width, indent)).as_deref(),
                Some(TOKEN),
                "width {width}, indent {indent:?}"
            );
        }
    }
}

#[test]
fn the_last_token_in_the_record_wins() {
    let older = TOKEN.replace("xyzAA", "oldBB");
    let both = format!("{}{}", record(&older, 50, ""), record(TOKEN, 50, ""));
    assert_eq!(parse(&both).as_deref(), Some(TOKEN));
}

#[test]
fn no_token_without_both_lines_or_with_text_between_them() {
    // Cancelled or failed: nothing after the label.
    assert_eq!(parse("Your OAuth token (valid for 1 year):\r\n"), None);
    assert_eq!(parse("Error: authorization failed\r\n"), None);
    assert_eq!(parse(""), None);
    // No closing line: the token might be cut off.
    let cut = record(TOKEN, 200, "").replace("Store this token securely", "");
    assert_eq!(parse(&cut), None);
    // Words between the lines are not a token.
    let words = "Your OAuth token (valid for 1 year):\r\nsomething went wrong\r\nStore this token securely.";
    assert_eq!(parse(words), None);
}

#[test]
fn a_truncated_token_is_refused() {
    let short = &TOKEN[..30];
    assert_eq!(parse(&record(short, 200, "")), None);
}

#[test]
fn pasted_tokens_are_cleaned_and_checked() {
    assert_eq!(validate(&format!("  {TOKEN}\n")).unwrap(), TOKEN);
    let wrapped = format!("{}\n{}", &TOKEN[..40], &TOKEN[40..]);
    assert_eq!(validate(&wrapped).unwrap(), TOKEN);
    for bad in [
        "",
        "   ",
        "sk-proj-123",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789ABCD",
        "sk-ant-oat01-short",
        &format!("{TOKEN};rm -rf ~"),
        &format!("{TOKEN}$(whoami)"),
        &format!("sk-ant-{}", "a".repeat(600)),
    ] {
        assert!(validate(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn messages_never_echo_the_input() {
    let secret = "sk-ant-oat01-SECRETSECRET;";
    let e = validate(secret).unwrap_err();
    assert!(!e.contains("SECRET"), "{e}");
}

#[test]
fn expiry_is_a_year_on_with_a_warning_two_weeks_before() {
    let made = 1_000_000_000_000;
    assert_eq!(expires(made), made + LIFETIME_MS);
    assert!(!expiring(made, made));
    assert!(!expiring(made, made + LIFETIME_MS - WARN_BEFORE_MS - 1));
    assert!(expiring(made, made + LIFETIME_MS - WARN_BEFORE_MS));
    assert!(expiring(made, made + LIFETIME_MS + 1));
    assert!(!expiring(u64::MAX, 0), "no overflow");
}

#[test]
fn escapes_are_stripped_without_eating_text() {
    assert_eq!(strip_escapes("a\x1b[1;31mb\x1b[0mc"), "abc");
    assert_eq!(strip_escapes("a\x1b]0;title\x07b"), "ab");
    assert_eq!(strip_escapes("a\x1b]8;;http://x\x1b\\b"), "ab");
    assert_eq!(strip_escapes("plain"), "plain");
    assert_eq!(strip_escapes("dangling\x1b"), "dangling");
}

#[test]
fn any_record_parses_without_panicking() {
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let pieces = [
        "Your OAuth token",
        "Store this token securely",
        "\r\n",
        "\x1b[",
        "\x1b]",
        "sk-ant-",
        "AbC0_-",
        " ",
        "é",
        "\x07",
    ];
    for _ in 0..3000 {
        let mut s = String::new();
        for _ in 0..20 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            s.push_str(pieces[(seed % pieces.len() as u64) as usize]);
        }
        if let Some(t) = parse(&s) {
            assert!(validate(&t).is_ok(), "{s:?}");
        }
    }
}

#[test]
fn a_long_record_is_parsed_quickly() {
    let noise = "progress \x1b[2K\r".repeat(200_000);
    let big = format!("{noise}{}", record(TOKEN, 60, ""));
    let started = std::time::Instant::now();
    assert_eq!(parse(&big).as_deref(), Some(TOKEN));
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

#[cfg(unix)]
#[test]
fn setup_token_runs_under_script_recording_to_the_file() {
    let c = command(Path::new("/opt/claude bin/claude"), Path::new("/tmp/x/rec")).unwrap();
    let args: Vec<String> = c
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(c.get_program().to_string_lossy().ends_with("script"));
    assert!(args.iter().any(|a| a.contains("setup-token")), "{args:?}");
    assert!(args.iter().any(|a| a == "/tmp/x/rec"), "{args:?}");
}
