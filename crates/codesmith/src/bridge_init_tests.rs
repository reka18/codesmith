use super::*;
use codesmith_core::providers::signin::Kind;

fn headless() -> Bridge {
    Bridge {
        name: "coder".into(),
        owner: "me@example.com".into(),
        command: "/opt/homebrew/bin/codesmith".into(),
        workdir: "/b/coder/workspace".into(),
        home: "/b/coder/home".into(),
        path: "/opt/homebrew/bin:/usr/bin:/bin".into(),
        mode: Mode::Headless {
            model: "fireworks/kimi#high".into(),
            efforts: vec!["low".into(), "high".into()],
            secrets: vec![(
                "FIREWORKS_API_KEY".into(),
                "/b/coder/secrets/FIREWORKS_API_KEY".into(),
            )],
        },
    }
}

fn relay() -> Bridge {
    Bridge {
        mode: Mode::Relay {
            share: "reviewer".into(),
        },
        ..headless()
    }
}

fn parse(text: &str) -> toml_edit::DocumentMut {
    text.parse()
        .unwrap_or_else(|e| panic!("not TOML: {e}\n{text}"))
}

#[test]
fn names_are_what_tailbus_takes() {
    for ok in ["a", "coder", "web-app-2", "0x", &"a".repeat(63)] {
        assert!(valid_name(ok), "{ok}");
    }
    for bad in [
        "",
        "-lead",
        "Coder",
        "my bot",
        "a_b",
        "a.b",
        "naïve",
        "鍵",
        "a/b",
        "../x",
        "a\nb",
        "a\u{1b}[2J",
        &"a".repeat(64),
    ] {
        assert!(!valid_name(bad), "{bad:?}");
    }
    assert_eq!(suggest_name("My Bot").as_deref(), Some("my-bot"));
    assert_eq!(suggest_name("  web_app  ").as_deref(), Some("web-app"));
    assert_eq!(suggest_name("naïve").as_deref(), Some("na-ve"));
    assert_eq!(suggest_name("鍵"), None);
    assert_eq!(suggest_name("---"), None);
    let long = suggest_name(&"ab ".repeat(40)).unwrap();
    assert!(valid_name(&long) && long.len() <= 63, "{long}");
}

#[test]
fn a_headless_table_is_the_codesmith_kind_with_its_own_choices() {
    let text = toml_block(&headless());
    let doc = parse(&text);
    let t = &doc["bridges"]["coder"];
    assert_eq!(t["kind"].as_str(), Some("codesmith"));
    assert_eq!(t["owner"].as_str(), Some("me@example.com"));
    assert_eq!(t["command"].as_str(), Some("/opt/homebrew/bin/codesmith"));
    assert_eq!(t["working_directory"].as_str(), Some("/b/coder/workspace"));
    assert_eq!(t["home"].as_str(), Some("/b/coder/home"));
    assert_eq!(t["path"].as_str(), Some("/opt/homebrew/bin:/usr/bin:/bin"));
    assert_eq!(t["model"].as_str(), Some("fireworks/kimi#high"));
    assert_eq!(
        t["secrets"]["FIREWORKS_API_KEY"].as_str(),
        Some("/b/coder/secrets/FIREWORKS_API_KEY")
    );
    let efforts: Vec<&str> = t["options"]["efforts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(efforts, ["low", "high"]);
    // The kind's defaults give args, env and usage: the table doesn't.
    for k in ["args", "env", "usage"] {
        assert!(t.get(k).is_none(), "{k} in\n{text}");
    }
    // A model with no levels lets a request name none.
    let mut b = headless();
    if let Mode::Headless {
        efforts, secrets, ..
    } = &mut b.mode
    {
        efforts.clear();
        secrets.clear();
    }
    let doc = parse(&toml_block(&b));
    let t = &doc["bridges"]["coder"];
    assert!(t["options"]["efforts"].as_array().unwrap().is_empty());
    assert!(t.get("secrets").is_none());
}

#[test]
fn a_relay_table_runs_bus_relay_with_tailbus_placeholders() {
    let text = toml_block(&relay());
    let doc = parse(&text);
    let t = &doc["bridges"]["coder"];
    assert!(t.get("kind").is_none(), "{text}");
    let args: Vec<&str> = t["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        args,
        ["bus", "relay", "--to", "reviewer", "--from", "{SENDER}", "--id", "{ID}"]
    );
    assert_eq!(t["timeout_secs"].as_integer(), Some(3600));
    assert!(t.get("secrets").is_none() && t.get("model").is_none());
}

#[test]
fn odd_paths_and_owners_survive_the_table() {
    let mut b = headless();
    b.owner = "o\"wn\\er\u{7}".into();
    b.home = "/b/my \"bridge\"\\鍵 home".into();
    b.workdir = "/b/tab\there".into();
    let doc = parse(&toml_block(&b));
    let t = &doc["bridges"]["coder"];
    assert_eq!(t["owner"].as_str(), Some("o\"wn\\er\u{7}"));
    assert_eq!(t["home"].as_str(), Some("/b/my \"bridge\"\\鍵 home"));
    assert_eq!(t["working_directory"].as_str(), Some("/b/tab\there"));
}

/// Bounded property: any string comes back from a TOML basic string as it
/// went in.
#[test]
fn any_string_round_trips_through_toml() {
    let alphabet: Vec<char> = "a\"\\\n\t\r\u{0}\u{1b}\u{7f}é鍵 #=[]{}'".chars().collect();
    let mut seed = 11u64;
    for _ in 0..500 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (seed >> 40) as usize % 24;
        let s: String = (0..len)
            .map(|j| alphabet[((seed >> (j % 48)) as usize + j) % alphabet.len()])
            .collect();
        let doc = parse(&format!("v = {}\n", toml_str(&s)));
        assert_eq!(doc["v"].as_str(), Some(s.as_str()), "{s:?}");
    }
}

#[test]
fn the_try_runs_what_tailbus_runs() {
    let s = Path::new("/run/1/session");
    assert_eq!(
        tailbus_args(&headless(), s, "crucible@mac", "01ID"),
        [
            "run",
            "-m",
            "fireworks/kimi#high",
            "--usage-log",
            "/run/1/session/usage.jsonl",
            "--no-mcp"
        ]
    );
    assert_eq!(
        tailbus_args(&relay(), s, "crucible@mac", "01ID"),
        [
            "bus",
            "relay",
            "--to",
            "reviewer",
            "--from",
            "crucible@mac",
            "--id",
            "01ID"
        ]
    );
    let yours = |k: &str| -> Option<String> {
        match k {
            "USER" => Some("me".into()),
            "LANG" => Some("C".into()),
            // Never passed on: a bridge's run sees none of these.
            "XDG_CONFIG_HOME" | "ANTHROPIC_API_KEY" | "CODESMITH_CONFIG_DIR" => Some("x".into()),
            _ => None,
        }
    };
    let env = tailbus_env(
        &headless(),
        Path::new("/run/1"),
        s,
        &yours,
        &[("FIREWORKS_API_KEY".into(), "fw-value".into())],
    );
    let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "USER",
            "LANG",
            "HOME",
            "PATH",
            "TMPDIR",
            "CODESMITH_DB",
            "FIREWORKS_API_KEY"
        ]
    );
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("HOME"), Some("/b/coder/home"));
    assert_eq!(get("TMPDIR"), Some("/run/1/tmp"));
    assert_eq!(get("CODESMITH_DB"), Some("/run/1/session/codesmith.db"));
    // A relay has no session database of its own.
    let env = tailbus_env(&relay(), Path::new("/run/1"), s, &yours, &[]);
    assert!(env.iter().all(|(k, _)| k != "CODESMITH_DB"));
    assert!(tailbus_message("hi").ends_with("\n\nhi"));
}

#[cfg(unix)]
#[test]
fn secrets_are_read_as_tailbus_reads_them() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("k");
    codesmith_core::home::write_secret(&f, "fw-value", false).unwrap();
    assert_eq!(read_secret_file("K", &f).unwrap(), "fw-value");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    let e = read_secret_file("K", &f).unwrap_err();
    assert!(format!("{e}").contains("chmod 600"), "{e}");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&f, " \n").unwrap();
    assert!(format!("{}", read_secret_file("K", &f).unwrap_err()).contains("empty"));
    assert!(read_secret_file("K", &d.path().join("missing")).is_err());
    // The error never carries the value.
    std::fs::write(&f, "fw-value").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
    let e = format!("{:#}", read_secret_file("K", &f).unwrap_err());
    assert!(!e.contains("fw-value"), "{e}");
}

#[test]
fn output_is_redacted_of_every_secret() {
    let secrets = vec![
        "fw-secret-123".to_string(),
        "fw-secret".to_string(),
        "ab".into(),
    ];
    let out = redact("key fw-secret-123 then fw-secret and ab", &secrets);
    assert_eq!(out, "key [secret] then [secret] and ab");
    assert_eq!(redact("nothing", &[]), "nothing");
}

#[test]
fn tailbus_config_is_where_tailbus_reads_it() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    };
    assert_eq!(
        tailbus_config(&env(&[
            ("HOME", "/u"),
            ("XDG_CONFIG_HOME", "/x"),
            ("TAILBUS_HOME", "/t")
        ])),
        Some(PathBuf::from("/t/config.toml"))
    );
    assert_eq!(
        tailbus_config(&env(&[("HOME", "/u"), ("XDG_CONFIG_HOME", "/x")])),
        Some(PathBuf::from("/x/tailbus/config.toml"))
    );
    assert_eq!(
        tailbus_config(&env(&[("HOME", "/u"), ("XDG_CONFIG_HOME", "")])),
        Some(PathBuf::from("/u/.config/tailbus/config.toml"))
    );
    assert_eq!(tailbus_config(&env(&[])), None);
}

#[test]
fn an_existing_bridge_is_found_however_it_is_written() {
    for text in [
        "[bridges.coder]\nowner = \"a\"\n",
        "[bridges.\"coder\"]\nowner = \"a\"\n",
        "bridges = { coder = { owner = \"a\" } }\n",
        "[bridges]\ncoder.owner = \"a\"\n",
        "[bridges.coder.env]\nX = \"1\"\n",
    ] {
        assert!(has_bridge(text, "coder").unwrap(), "{text}");
        assert!(!has_bridge(text, "other").unwrap(), "{text}");
    }
    assert!(!has_bridge("", "coder").unwrap());
    assert!(!has_bridge("owner = \"me\"\n[bridges.coders]\nowner=\"a\"\n", "coder").unwrap());
    assert!(has_bridge("[bridges.coder\nbroken", "coder").is_err());
}

#[test]
fn the_owner_offered_is_the_buses_or_a_bridges() {
    assert_eq!(
        default_owner("owner = \"me@x\"\n[bridges.a]\nowner = \"other@x\"\n").as_deref(),
        Some("me@x")
    );
    assert_eq!(
        default_owner("network = \"tailnet\"\n[bridges.a]\nowner = \"b@x\"\n").as_deref(),
        Some("b@x")
    );
    assert_eq!(default_owner("owner = \"\"\n"), None);
    assert_eq!(default_owner("owner = \"evil\\u001b[2J\"\n"), None);
    assert_eq!(default_owner("not toml ["), None);
}

#[test]
fn adding_keeps_everything_that_was_there() {
    let existing = "# my bus\nowner = \"me@x\"\n\n[bridges.other]\nowner = \"me@x\"\ncommand = \"/bin/x\"\n\n\n";
    let block = toml_block(&headless());
    let out = appended(existing, &block);
    assert!(out.starts_with(existing.trim_end()), "{out}");
    assert!(out.ends_with(&block));
    assert!(has_bridge(&out, "coder").unwrap());
    assert!(has_bridge(&out, "other").unwrap());
    assert_eq!(appended("", &block), block);
    assert_eq!(appended("\n\n", &block), block);
}

/// Stress: many bridges added one after another stay distinct and valid.
#[test]
fn many_bridges_append_cleanly() {
    let mut text = String::from("owner = \"me@x\"\n");
    for i in 0..200 {
        let mut b = if i % 2 == 0 { headless() } else { relay() };
        b.name = format!("b-{i}");
        assert!(!has_bridge(&text, &b.name).unwrap());
        text = appended(&text, &toml_block(&b));
        assert!(has_bridge(&text, &b.name).unwrap());
    }
    let doc = parse(&text);
    assert_eq!(doc["bridges"].as_table_like().unwrap().len(), 200);
}

#[test]
fn the_agents_path_has_each_folder_once() {
    let p = agent_path(&[
        "/opt/homebrew/bin".into(),
        "/usr/bin".into(),
        "/opt/homebrew/bin".into(),
        "/odd:folder".into(),
        "".into(),
    ]);
    assert_eq!(p, "/opt/homebrew/bin:/usr/bin:/bin");
}

#[test]
fn a_secret_goes_only_in_the_bridges_secrets_folder() {
    let root = Path::new("/b/coder");
    assert_eq!(
        secret_path(root, "FIREWORKS_API_KEY"),
        Some(PathBuf::from("/b/coder/secrets/FIREWORKS_API_KEY"))
    );
    // A variable name from config can't lead anywhere else.
    for bad in [
        "",
        "../x",
        "A/B",
        "..",
        "1KEY",
        "A B",
        "KEY\n",
        "鍵",
        &"A".repeat(129),
    ] {
        assert_eq!(secret_path(root, bad), None, "{bad:?}");
    }
}

#[test]
fn picks_take_a_number_an_id_or_the_default() {
    let ids: Vec<String> = ["headless", "relay"].map(String::from).to_vec();
    assert_eq!(pick("", &ids, Some(0)), Some(0));
    assert_eq!(pick("", &ids, None), None);
    assert_eq!(pick("2", &ids, None), Some(1));
    assert_eq!(pick(" Relay ", &ids, None), Some(1));
    assert_eq!(pick("3", &ids, None), None);
    assert_eq!(pick("0", &ids, None), None);
    assert_eq!(pick("-1", &ids, None), None);
    assert_eq!(pick("nope", &ids, None), None);
}

#[test]
fn a_typed_model_is_found_narrowed_or_taken_as_given() {
    let ids: Vec<String> = [
        "fireworks/accounts/fireworks/models/kimi-k2",
        "fireworks/accounts/fireworks/models/kimi-k3",
        "fireworks/kimi",
        "fireworks/glm-5",
    ]
    .map(String::from)
    .to_vec();
    let chosen = |s: &str| ModelAnswer::Chosen(s.to_string());
    assert_eq!(
        model_answer("fireworks", "kimi", &ids),
        chosen("fireworks/kimi")
    );
    assert_eq!(
        model_answer("fireworks", "glm", &ids),
        chosen("fireworks/glm-5")
    );
    assert_eq!(
        model_answer("fireworks", "fireworks/glm-5", &ids),
        chosen("fireworks/glm-5")
    );
    assert_eq!(
        model_answer("fireworks", "KIMI-K", &ids),
        ModelAnswer::Narrowed(ids[..2].to_vec())
    );
    assert_eq!(
        model_answer("fireworks", "new-one", &ids),
        chosen("fireworks/new-one")
    );
    assert_eq!(
        model_answer("fireworks", "fireworks/new-one", &[]),
        chosen("fireworks/new-one")
    );
}

#[test]
fn a_bridge_is_offered_only_logins_it_can_use_on_its_own() {
    assert!(cli::portable(Kind::Key, false, true));
    assert!(cli::portable(Kind::Key, true, false));
    assert!(
        !cli::portable(Kind::Key, true, true),
        "a file holds a key alone"
    );
    assert!(cli::portable(
        Kind::ClaudeToken { create: true },
        true,
        false
    ));
    assert!(cli::portable(Kind::Browser { paste: true }, false, false));
    assert!(
        !cli::portable(Kind::Device, true, false),
        "OAuth needs an auth store"
    );
    for k in [
        Kind::ClaudeCode,
        Kind::AwsProfile,
        Kind::VertexProject,
        Kind::AzureCli,
    ] {
        assert!(!cli::portable(k, false, false), "{k:?}");
    }
    let d = tempfile::tempdir().unwrap();
    let all = signin::choices(
        &config::Config::default(),
        &d.path().join("auth.json"),
        &cli::no_env(),
    );
    let kept = bridge_choices(all.clone());
    let find = |id: &str| kept.iter().find(|c| c.id == id);
    assert!(find("google-vertex").is_none(), "Vertex has only ADC");
    let claude = find("claude-agent").unwrap();
    assert!(claude.methods.iter().all(|m| m.kind != Kind::ClaudeCode));
    assert!(find("fireworks").is_some_and(|c| c.methods[0].kind == Kind::Key));
    assert!(find("openai").is_some_and(|c| c.methods.len() == 2));
    let files = cli::portable_choices(all, true);
    assert!(files.iter().all(|c| c
        .methods
        .iter()
        .all(|m| matches!(m.kind, Kind::Key | Kind::ClaudeToken { .. }))));
    assert!(files.iter().all(|c| c.id != "github-copilot"));
}

#[test]
fn key_variables_are_the_ones_a_run_reads() {
    let c = config::Config::default();
    assert_eq!(
        cli::key_var(&c, "fireworks").as_deref(),
        Some("FIREWORKS_API_KEY")
    );
    assert_eq!(
        cli::key_var(&c, "anthropic").as_deref(),
        Some("ANTHROPIC_API_KEY")
    );
    assert_eq!(
        cli::key_var(&c, "claude-agent").as_deref(),
        Some("CLAUDE_CODE_OAUTH_TOKEN")
    );
}

#[test]
fn a_key_from_the_environment_is_named_plainly() {
    for bad in ["", "1X", "A-B", "A B", "$(rm)", "PATH=x"] {
        assert!(cli::key_from_env(bad).is_err(), "{bad:?}");
    }
    let e = cli::key_from_env("CODESMITH_TEST_SURELY_UNSET_9Q").unwrap_err();
    assert!(format!("{e}").contains("isn't set"), "{e}");
}
