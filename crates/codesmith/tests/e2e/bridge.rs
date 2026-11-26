//! Signing in for a tailbus bridge: `codesmith auth login --home DIR` and
//! `--to-file PATH`, the sign-in picker's way there, and `codesmith
//! bridge init NAME` driven in a pseudo-terminal: a bridge home, a 0600
//! secrets file, a try of the bridge as tailbus runs it (against the
//! scripted model), the `[bridges.NAME]` table, and tailbus's config and
//! `tailbus bridge install` only after a yes (a fake `tailbus` here).
//! Nothing touches a real tailbus or your own config: each world is a
//! temp folder.

use crate::harness::{Env, Out, BIN};
use crate::model::{Model, Reply, Script, M};
use crate::tui::{no_claude_path, Ui};
use serde_json::json;
use std::path::{Path, PathBuf};

const KEY: &str = "fw-bridge-secret-5d2e81";

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn no_secret(out: &Out, secret: &str) {
    assert!(
        !out.both().contains(secret),
        "the secret was printed:\n{}",
        out.both()
    );
}

/// A bridge home of its own, beside the world's home.
fn other_home(e: &Env) -> PathBuf {
    let h = e.root.join("bridge-home");
    std::fs::create_dir_all(&h).unwrap();
    h
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_login_home_saves_the_key_in_that_homes_store_only() {
    let e = Env::new();
    let h = other_home(&e);
    let out = e
        .run_with_stdin(
            &["auth", "login", "fireworks", "--home", h.to_str().unwrap()],
            &format!("{KEY}\n"),
        )
        .await;
    out.ok();
    no_secret(&out, KEY);
    let auth = h.join(".local/share/codesmith/auth.json");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
    assert_eq!(saved["fireworks"]["key"], KEY);
    assert_eq!(mode(&auth), 0o600);
    assert_eq!(mode(auth.parent().unwrap()), 0o700);
    assert!(
        out.stdout.contains(&auth.display().to_string()),
        "{}",
        out.both()
    );
    // Yours is untouched, and no default model was chosen for you.
    assert!(!e.data.join("codesmith/auth.json").exists());
    assert!(!out.stdout.contains("Default model"), "{}", out.both());
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_login_home_refuses_your_own_home_and_a_missing_folder() {
    let e = Env::new();
    let out = e
        .run(&[
            "auth",
            "login",
            "fireworks",
            "--home",
            e.home.to_str().unwrap(),
        ])
        .await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("your own home"), "{}", out.both());
    let out = e
        .run(&[
            "auth",
            "login",
            "fireworks",
            "--home",
            e.root.join("nope").to_str().unwrap(),
        ])
        .await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("create the folder"), "{}", out.both());
    assert!(!e.root.join("nope").exists());
    // --home and --to-file together make no sense.
    let h = other_home(&e);
    let out = e
        .run(&[
            "auth",
            "login",
            "fireworks",
            "--home",
            h.to_str().unwrap(),
            "--to-file",
            e.root.join("k").to_str().unwrap(),
        ])
        .await;
    assert_ne!(out.code, Some(0));
    no_secret(&out, KEY);
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_login_to_file_writes_the_key_owner_only_and_names_its_variable() {
    let e = Env::new();
    let f = e.root.join("secrets/fireworks");
    let out = e
        .run_with_stdin(
            &[
                "auth",
                "login",
                "fireworks",
                "--to-file",
                f.to_str().unwrap(),
            ],
            &format!("  {KEY}\n"),
        )
        .await;
    out.ok();
    no_secret(&out, KEY);
    assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("{KEY}\n"));
    assert_eq!(mode(&f), 0o600);
    assert_eq!(mode(f.parent().unwrap()), 0o700);
    assert!(
        out.stdout.contains("FIREWORKS_API_KEY = \""),
        "{}",
        out.both()
    );
    assert!(!e.data.join("codesmith/auth.json").exists());

    // Not replaced without a terminal to ask at.
    let out = e
        .run(&[
            "auth",
            "login",
            "fireworks",
            "--to-file",
            f.to_str().unwrap(),
        ])
        .await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("already exists"), "{}", out.both());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("{KEY}\n"));

    // Never through a symlink.
    let victim = e.root.join("victim");
    std::fs::write(&victim, "keep\n").unwrap();
    let link = e.root.join("link");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    let out = e
        .run(&[
            "auth",
            "login",
            "fireworks",
            "--to-file",
            link.to_str().unwrap(),
        ])
        .await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("symlink"), "{}", out.both());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_login_to_file_takes_only_a_key_or_a_token() {
    let e = Env::new();
    let f = e.root.join("k");
    for args in [
        vec!["openai", "--oauth"],
        vec!["amazon-bedrock", "--profile", "x"],
        vec!["github-copilot"],
        vec!["cloudflare-ai-gateway"],
        vec!["fireworks", "--token"],
    ] {
        let mut all = vec!["auth", "login"];
        all.extend(&args);
        all.extend(["--to-file", f.to_str().unwrap()]);
        // Refused before any key is read: no stdin to write to.
        let out = e.run(&all).await;
        assert_ne!(out.code, Some(0), "{args:?}: {}", out.both());
        no_secret(&out, KEY);
        assert!(!f.exists(), "{args:?} wrote a file");
    }
    // A Claude Code token for claude-agent, checked first.
    let out = e
        .run_with_stdin(
            &[
                "auth",
                "login",
                "claude-agent",
                "--to-file",
                f.to_str().unwrap(),
            ],
            "not-a-token",
        )
        .await;
    assert_ne!(out.code, Some(0));
    assert!(!f.exists());
    let token = format!("sk-ant-oat01-{}", "Ab9_".repeat(20));
    let out = e
        .run_with_stdin(
            &[
                "auth",
                "login",
                "claude-agent",
                "--to-file",
                f.to_str().unwrap(),
            ],
            &format!("{token}\n"),
        )
        .await;
    out.ok();
    no_secret(&out, &token);
    assert_eq!(std::fs::read_to_string(&f).unwrap().trim(), token);
    assert_eq!(mode(&f), 0o600);
    assert!(
        out.stdout.contains("CLAUDE_CODE_OAUTH_TOKEN"),
        "{}",
        out.both()
    );
}

/// A project's config can't change which variable a key goes in, or
/// where a login for another home is saved.
#[tokio::test(flavor = "multi_thread")]
async fn a_projects_config_does_not_steer_a_bridges_credential() {
    let e = Env::new();
    e.write(
        "codesmith.json",
        &json!({"providers": {"fireworks": {"env": ["EVIL_VAR"]}}}).to_string(),
    );
    let f = e.root.join("k");
    let out = e
        .run_with_stdin(
            &[
                "auth",
                "login",
                "fireworks",
                "--to-file",
                f.to_str().unwrap(),
            ],
            KEY,
        )
        .await;
    out.ok();
    assert!(out.stdout.contains("FIREWORKS_API_KEY ="), "{}", out.both());
    assert!(!out.both().contains("EVIL_VAR"), "{}", out.both());
    let h = other_home(&e);
    e.run_with_stdin(
        &["auth", "login", "fireworks", "--home", h.to_str().unwrap()],
        KEY,
    )
    .await
    .ok();
    assert!(h.join(".local/share/codesmith/auth.json").exists());
    assert!(!e.path(".local").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_init_needs_a_terminal_and_writes_nothing_without_one() {
    let e = Env::new();
    let dir = e.root.join("b");
    let out = e
        .run(&["bridge", "init", "coder", "--dir", dir.to_str().unwrap()])
        .await;
    assert_ne!(out.code, Some(0));
    assert!(out.stderr.contains("in a terminal"), "{}", out.both());
    assert!(out.stderr.contains("nothing was written"), "{}", out.both());
    assert!(!dir.exists());
    assert!(!e.home.join(".config/tailbus").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_init_refuses_a_name_tailbus_cant_take() {
    let e = Env::new();
    for (name, hint) in [
        ("My Bot", "such as my-bot"),
        ("鍵", "lowercase letters"),
        ("../evil", "such as evil"),
        ("-rf", "such as rf"),
    ] {
        let out = e.run(&["bridge", "init", "--", name]).await;
        assert_ne!(out.code, Some(0), "{name}");
        assert!(
            out.stderr.contains("can't be a tailbus agent's name"),
            "{}",
            out.both()
        );
        assert!(out.stderr.contains(hint), "{name}: {}", out.both());
    }
    assert!(!e.data.join("codesmith/bridges").exists());
}

/// A world for `bridge init`: a fake `tailbus` that records what it was
/// asked, tailbus's config with another bridge, and a bridge folder whose
/// home's config points Fireworks at the scripted model.
struct World {
    e: Env,
    model: Model,
    dir: PathBuf,
    bin: PathBuf,
    tailbus_log: PathBuf,
    config: PathBuf,
}

const EXISTING: &str = "owner = \"me@example.com\"\n\n[bridges.other]\nowner = \"me@example.com\"\ncommand = \"/bin/echo\"\nworking_directory = \"/tmp\"\nhome = \"/tmp\"\npath = \"/bin\"\n";

async fn world(tailbus: bool) -> World {
    let e = Env::new();
    let model =
        Model::start(Script::new().always("answer", M::f(|_| true), Reply::text("Ready."))).await;
    let dir = e.root.join("bridges/coder");
    let cfg = dir.join("home/.config/codesmith");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("codesmith.json"),
        json!({
            "bus": false,
            "repo_warning": false,
            "providers": {"fireworks": {
                "settings": {"baseURL": model.url},
                "models": {"kimi": {
                    "limit": {"context": 100000},
                    "variants": {"low": {"reasoningEffort": "low"}, "high": {"reasoningEffort": "high"}}
                }}
            }}
        })
        .to_string(),
    )
    .unwrap();
    let bin = e.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tailbus_log = e.root.join("tailbus.log");
    if tailbus {
        let script = bin.join("tailbus");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\n", tailbus_log.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = e.home.join(".config/tailbus/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, EXISTING).unwrap();
    World {
        e,
        model,
        dir,
        bin,
        tailbus_log,
        config,
    }
}

impl World {
    fn start(&self, name: &str) -> Option<Ui> {
        let path = format!("{}:/usr/bin:/bin", self.bin.display());
        Ui::start_with(
            &self.e,
            &format!("bridge init {name} --dir '{}'", self.dir.display()),
            &[("PATH", path.as_str()), ("FW_BRIDGE_KEY", KEY)],
        )
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_init_sets_up_a_headless_bridge_tries_it_and_installs_it_after_a_yes() {
    let w = world(true).await;
    let Some(mut ui) = w.start("coder") else {
        return;
    };
    ui.wait_for("How should it run CodeSmith?").await;
    ui.send(b"\r").await;
    ui.wait_for("already exists. Use it").await;
    ui.send(b"y\r").await;
    ui.wait_for("Where the agent works").await;
    ui.send(b"\r").await;
    ui.wait_for("Which provider should it use?").await;
    ui.wait_for("untested").await;
    ui.send(b"fireworks\r").await;
    ui.wait_for("Take the key from an environment variable?")
        .await;
    ui.send(b"FW_BRIDGE_KEY\r").await;
    ui.wait_for("Took the key from $FW_BRIDGE_KEY").await;
    ui.wait_for("Which model?").await;
    ui.send(b"kimi\r").await;
    ui.wait_for("Which effort, by default?").await;
    ui.send(b"high\r").await;
    ui.wait_for("[me@example.com]").await;
    ui.send(b"\r").await;
    ui.wait_for("It answered:").await;
    ui.wait_for("Ready.").await;
    ui.wait_for("[bridges.coder]").await;
    ui.wait_for("Add it to").await;
    ui.send(b"y\r").await;
    ui.wait_for("Added [bridges.coder]").await;
    ui.wait_for("tailbus bridge install coder`?").await;
    ui.send(b"y\r").await;
    ui.wait_for("The bridge coder is running.").await;
    ui.read_to_end().await;
    assert!(!ui.screen.contains(KEY), "the key was shown");

    // The key file, owner-only, in a folder of its own.
    let key = w.dir.join("secrets/FIREWORKS_API_KEY");
    assert_eq!(std::fs::read_to_string(&key).unwrap(), format!("{KEY}\n"));
    assert_eq!(mode(&key), 0o600);
    assert_eq!(mode(key.parent().unwrap()), 0o700);
    assert_eq!(mode(&w.dir.join("home")), 0o700);
    assert!(w.dir.join("workspace").is_dir());

    // The try was a run as tailbus does it: the model, its effort, the key.
    let asked = w.model.by("answer");
    let step = asked
        .iter()
        .find(|b| crate::model::prompt(b).contains("codesmith bridge init"))
        .expect("the try reached the model");
    assert_eq!(step["reasoning_effort"], "high", "{step}");
    assert!(crate::model::prompt(step).starts_with("This message is from another agent"));

    // The config kept the other bridge and gained this one.
    let text = std::fs::read_to_string(&w.config).unwrap();
    assert!(text.starts_with(EXISTING.trim_end()), "{text}");
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    let t = &doc["bridges"]["coder"];
    assert_eq!(t["kind"].as_str(), Some("codesmith"));
    assert_eq!(t["model"].as_str(), Some("fireworks/kimi#high"));
    assert_eq!(t["owner"].as_str(), Some("me@example.com"));
    assert_eq!(
        t["home"].as_str(),
        Some(w.dir.join("home").to_str().unwrap())
    );
    assert_eq!(
        t["secrets"]["FIREWORKS_API_KEY"].as_str(),
        Some(key.to_str().unwrap())
    );
    assert_eq!(
        Path::new(t["command"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        Path::new(BIN).canonicalize().unwrap()
    );
    assert!(!text.contains(KEY));
    assert!(doc["bridges"].get("other").is_some());
    assert_eq!(
        std::fs::read_to_string(&w.tailbus_log).unwrap().trim(),
        "bridge install coder"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_init_leaves_an_existing_table_and_tailbus_alone() {
    let w = world(true).await;
    let before = format!("{EXISTING}\n[bridges.coder]\nowner = \"me@example.com\"\n");
    std::fs::write(&w.config, &before).unwrap();
    // A key from an earlier setup is kept when you say so.
    let key = w.dir.join("secrets/FIREWORKS_API_KEY");
    codesmith::core::home::write_secret(&key, KEY, false).unwrap();
    let Some(mut ui) = w.start("coder") else {
        return;
    };
    ui.wait_for("How should it run CodeSmith?").await;
    ui.send(b"1\r").await;
    ui.wait_for("already exists. Use it").await;
    ui.send(b"y\r").await;
    ui.wait_for("Where the agent works").await;
    ui.send(b"\r").await;
    ui.wait_for("Which provider should it use?").await;
    ui.send(b"fireworks\r").await;
    ui.wait_for("is already in").await;
    ui.wait_for("Keep it?").await;
    ui.send(b"\r").await;
    ui.wait_for("Which model?").await;
    ui.send(b"kimi\r").await;
    ui.wait_for("Which effort").await;
    ui.send(b"\r").await;
    ui.wait_for("[me@example.com]").await;
    ui.send(b"\r").await;
    ui.wait_for("Ready.").await;
    ui.wait_for("already has a bridge named coder").await;
    ui.wait_for("tailbus bridge install coder").await;
    ui.read_to_end().await;
    assert!(!ui.screen.contains(KEY));
    assert_eq!(std::fs::read_to_string(&w.config).unwrap(), before);
    assert!(!w.tailbus_log.exists(), "tailbus was run");
    // The default effort is the provider's own: no variant.
    assert!(
        ui.screen.contains("model = \"fireworks/kimi\""),
        "{}",
        ui.screen
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_init_asks_before_changing_the_config_and_refuses_a_symlinked_key() {
    let w = world(false).await;
    // A symlink where the key would go: refused, the target untouched.
    let victim = w.e.root.join("victim");
    std::fs::write(&victim, "keep\n").unwrap();
    let secrets = w.dir.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::os::unix::fs::symlink(&victim, secrets.join("FIREWORKS_API_KEY")).unwrap();
    let Some(mut ui) = w.start("coder") else {
        return;
    };
    ui.wait_for("How should it run CodeSmith?").await;
    ui.send(b"\r").await;
    ui.wait_for("already exists. Use it").await;
    ui.send(b"y\r").await;
    ui.wait_for("Where the agent works").await;
    ui.send(b"\r").await;
    ui.wait_for("Which provider should it use?").await;
    ui.send(b"fireworks\r").await;
    ui.wait_for("is a symlink").await;
    ui.read_to_end().await;
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
    assert_eq!(std::fs::read_to_string(&w.config).unwrap(), EXISTING);

    // Without the link, saying no leaves tailbus's config as it was, and
    // with no tailbus installed nothing offers to run it.
    std::fs::remove_file(secrets.join("FIREWORKS_API_KEY")).unwrap();
    let Some(mut ui) = w.start("coder") else {
        return;
    };
    ui.wait_for("How should it run CodeSmith?").await;
    ui.send(b"\r").await;
    ui.wait_for("already exists. Use it").await;
    ui.send(b"y\r").await;
    ui.wait_for("Where the agent works").await;
    ui.send(b"\r").await;
    ui.wait_for("Which provider should it use?").await;
    ui.send(b"fireworks\r").await;
    ui.wait_for("Take the key from an environment variable?")
        .await;
    ui.send(b"\r").await;
    ui.wait_for("Open it in the browser?").await;
    ui.send(b"n\r").await;
    ui.wait_for("API key for Fireworks AI").await;
    ui.send(KEY.as_bytes()).await;
    ui.send(b"\r").await;
    ui.wait_for("Which model?").await;
    ui.send(b"kimi\r").await;
    ui.wait_for("Which effort").await;
    ui.send(b"low\r").await;
    ui.wait_for("[me@example.com]").await;
    ui.send(b"\r").await;
    ui.wait_for("Add it to").await;
    ui.send(b"n\r").await;
    ui.wait_for("When the table is in").await;
    ui.read_to_end().await;
    assert!(!ui.screen.contains(KEY), "the pasted key was echoed");
    assert_eq!(std::fs::read_to_string(&w.config).unwrap(), EXISTING);
    assert_eq!(
        std::fs::read_to_string(secrets.join("FIREWORKS_API_KEY")).unwrap(),
        format!("{KEY}\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_init_sets_up_a_relay_and_says_when_tailbus_is_missing() {
    let w = world(false).await;
    std::fs::remove_file(&w.config).unwrap();
    let Some(mut ui) = w.start("reviewer") else {
        return;
    };
    ui.wait_for("How should it run CodeSmith?").await;
    ui.send(b"relay\r").await;
    ui.wait_for("already exists. Use it").await;
    ui.send(b"y\r").await;
    ui.wait_for("The name the session is shared as").await;
    ui.send(b"\r").await;
    // No config yet: nothing to offer as the owner.
    ui.wait_for("your local bus's owner)").await;
    ui.send(b"\r").await;
    ui.wait_for("The bridge needs an owner.").await;
    ui.send("me@exämple.com\r".as_bytes()).await;
    ui.wait_for("Send a test message").await;
    ui.send(b"\r").await;
    ui.wait_for("bus\", \"relay\", \"--to\", \"reviewer\"")
        .await;
    ui.wait_for("doesn't exist yet. Create it").await;
    ui.send(b"y\r").await;
    ui.wait_for("tailbus isn't on your PATH").await;
    ui.read_to_end().await;
    let doc: toml_edit::DocumentMut = std::fs::read_to_string(&w.config).unwrap().parse().unwrap();
    let t = &doc["bridges"]["reviewer"];
    assert_eq!(t["owner"].as_str(), Some("me@exämple.com"));
    assert!(t.get("kind").is_none());
    assert_eq!(t["timeout_secs"].as_integer(), Some(3600));
    assert_eq!(mode(&w.config), 0o600);
    // A relay needs no key.
    assert!(!w.dir.join("secrets").exists());
}

/// The sign-in picker's last row leads to signing in for another home.
#[tokio::test(flavor = "multi_thread")]
async fn the_sign_in_picker_signs_in_for_another_home() {
    let Some((_, path)) = no_claude_path() else {
        return;
    };
    let e = Env::new();
    let h = other_home(&e);
    let Some(mut ui) = Ui::start_with(&e, "auth login", &[("PATH", path), ("FW_BRIDGE_KEY", KEY)])
    else {
        return;
    };
    ui.wait_for("For a bridge or another home").await;
    ui.wait_for("Provider to sign in to").await;
    // The last row.
    let n = ui
        .screen
        .lines()
        .filter_map(|l| {
            l.trim()
                .split_once(". ")
                .and_then(|(n, _)| n.trim().parse::<usize>().ok())
        })
        .max()
        .unwrap();
    ui.send(format!("{n}\r").as_bytes()).await;
    ui.wait_for("Save a login in another CodeSmith home").await;
    ui.send(b"2\r").await;
    ui.wait_for("The home folder").await;
    ui.send(format!("{}\r", h.display()).as_bytes()).await;
    ui.wait_for("Signing in for the CodeSmith home").await;
    ui.send(b"fireworks\r").await;
    ui.wait_for("Take the key from an environment variable?")
        .await;
    ui.send(b"$FW_BRIDGE_KEY\r").await;
    ui.wait_for("Saved a key for fireworks").await;
    ui.read_to_end().await;
    assert!(!ui.screen.contains(KEY));
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(h.join(".local/share/codesmith/auth.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["fireworks"]["key"], KEY);
    assert!(!e.data.join("codesmith/auth.json").exists());
}

/// Without a terminal, the picker for another home or a file prints the
/// providers it can sign in to there and how, and asks nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_picker_for_another_home_without_a_terminal_says_how() {
    let e = Env::new();
    let h = other_home(&e);
    let out = e
        .run(&["auth", "login", "--home", h.to_str().unwrap()])
        .await;
    assert_ne!(out.code, Some(0));
    assert!(out.stdout.contains("fireworks"), "{}", out.both());
    assert!(
        out.stdout.contains(&format!("Add --home {}", h.display())),
        "{}",
        out.both()
    );
    // Logins that rest on your own home's files aren't offered there.
    assert!(!out.stdout.contains("google-vertex"), "{}", out.both());
    let f = e.root.join("k");
    let out = e
        .run(&["auth", "login", "--to-file", f.to_str().unwrap()])
        .await;
    assert_ne!(out.code, Some(0));
    assert!(out.stdout.contains("Add --to-file"), "{}", out.both());
    // A file holds a key or a token: no OAuth-only provider.
    assert!(!out.stdout.contains("github-copilot"), "{}", out.both());
    assert!(!f.exists());
    // Other flags still need a provider.
    let out = e
        .run(&["auth", "login", "--home", h.to_str().unwrap(), "--oauth"])
        .await;
    assert!(out.stderr.contains("name the provider"), "{}", out.both());
}

/// `/login`'s last row hands the terminal to `codesmith auth login
/// --to-file`, and takes it back after.
#[tokio::test(flavor = "multi_thread")]
async fn login_in_the_ui_writes_a_key_file_through_the_terminal() {
    let Some((_, path)) = no_claude_path() else {
        return;
    };
    let e = Env::new();
    e.config("http://127.0.0.1:9/v1", json!({}));
    let f = e.root.join("secrets/fw");
    let Some(mut ui) = Ui::start_with(&e, "", &[("PATH", path), ("FW_BRIDGE_KEY", KEY)]) else {
        return;
    };
    ui.wait_for("fake/m").await;
    ui.send(b"/login\r").await;
    ui.wait_for("type to filter").await;
    ui.send(b"bridge").await;
    ui.wait_for("For a bridge or another home").await;
    ui.send(b"\r").await;
    ui.wait_for("Write a key or Claude Code token to a file")
        .await;
    for _ in 0..2 {
        ui.send(b"\x1b[B").await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    ui.send(b"\r").await;
    ui.wait_for("created readable only by you").await;
    ui.send(f.display().to_string().as_bytes()).await;
    ui.send(b"\r").await;
    ui.wait_for("The key or Claude Code token goes in").await;
    ui.wait_for("Provider to sign in to").await;
    ui.send(b"fireworks\r").await;
    ui.wait_for("Take the key from an environment variable?")
        .await;
    ui.send(b"FW_BRIDGE_KEY\r").await;
    ui.wait_for("Wrote the key for fireworks").await;
    ui.wait_for("Press Enter to go back to CodeSmith").await;
    ui.send(b"\r").await;
    ui.wait_for("done").await;
    assert!(!ui.screen.contains(KEY), "the key was shown");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("{KEY}\n"));
    assert_eq!(mode(&f), 0o600);
    assert!(!e.data.join("codesmith/auth.json").exists());
}
