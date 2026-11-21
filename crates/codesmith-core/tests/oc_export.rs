//! Session exports (OpenCode's `session.export` and `session export
//! --sanitize`, MIT): the Markdown transcript, the raw and sanitized JSON,
//! credentials scrubbed from every form, and the private file an export is
//! saved to. See `codesmith_core::export` for what each form keeps.

mod support;

use codesmith_ai::{Media, Message, Part, Protocol, Reasoning, ReasoningMeta, Role};
use codesmith_core::export::{self, Transcript};
use codesmith_core::store::{Entry, Export, Info, Record, Store};
use serde_json::{json, Value};
use support::H;

const SECRET: &str = "sk-live-0123456789abcdef";
const BOTH: Transcript = Transcript {
    thinking: true,
    tools: true,
};

fn info(id: &str, title: Option<&str>) -> Info {
    Info {
        id: id.into(),
        directory: "/home/someone/secret-project".into(),
        title: title.map(str::to_string),
        agent: "build".into(),
        model: "fake/m".into(),
        parent_id: None,
        created: 0,
        updated: 0,
        usage: Default::default(),
    }
}

fn entry(seq: i64, record: Record) -> Entry {
    Entry {
        seq,
        created: 0,
        record,
    }
}

fn msg(role: Role, parts: Vec<Part>) -> Record {
    Record::Message {
        message: Message { role, parts },
    }
}

/// A session with every kind of record and part, its content all secret.
fn rich() -> Export {
    let mut meta = ReasoningMeta::of(Protocol::AnthropicMessages);
    meta.signature = Some("sig-secret-signature".into());
    Export {
        info: info("ses_rich", Some("Secret title")),
        entries: vec![
            entry(
                1,
                Record::Snapshot {
                    tree: "tree-secret".into(),
                },
            ),
            entry(
                2,
                msg(
                    Role::User,
                    vec![
                        Part::text("user secret prompt"),
                        Part::Media(Media::base64("image/png", "aGVsbG8=")),
                    ],
                ),
            ),
            entry(
                3,
                msg(
                    Role::Assistant,
                    vec![
                        Part::Reasoning(Reasoning::new("thinking secret", meta)),
                        Part::text("assistant secret reply"),
                        Part::tool_call("call_1", "read", json!({"path": "secret-path.txt"})),
                    ],
                ),
            ),
            entry(
                4,
                msg(
                    Role::User,
                    vec![Part::ToolResult {
                        id: "call_1".into(),
                        name: "read".into(),
                        content: format!("tool secret output {SECRET}"),
                        is_error: false,
                        media: vec![Media::base64("image/png", "aGVsbG8=")],
                    }],
                ),
            ),
            entry(
                5,
                msg(
                    Role::System,
                    vec![Part::Effort {
                        effort: Some("high".into()),
                        previous: None,
                    }],
                ),
            ),
            entry(
                6,
                Record::Change {
                    call: "call_1".into(),
                    path: "secret-path.txt".into(),
                    old: Some("old secret text".into()),
                    new: "new secret text".into(),
                },
            ),
            entry(
                7,
                Record::Checkpoint {
                    summary: "summary secret".into(),
                    keep_from: 3,
                },
            ),
            entry(
                8,
                Record::Native {
                    window: vec![Message::user("native secret window")],
                    model: "fake/m".into(),
                    keep_from: 3,
                },
            ),
            entry(
                9,
                Record::Step {
                    model: "fake/m".into(),
                    usage: Default::default(),
                },
            ),
            entry(
                10,
                Record::Tool {
                    name: "read".into(),
                    ms: 5,
                    error: false,
                },
            ),
            entry(
                11,
                Record::Instructions {
                    changed: [("/secret/AGENTS.md".to_string(), json!("instruction secret"))]
                        .into_iter()
                        .collect(),
                    removed: vec!["/secret/OLD.md".into()],
                    text: Some("update secret".into()),
                    baseline: false,
                    fork: false,
                },
            ),
            entry(
                12,
                Record::Metadata {
                    seq: 2,
                    metadata: json!({"secret-key": "metadata secret"}),
                },
            ),
            entry(
                13,
                Record::TurnEnd {
                    tree: "tree-secret-end".into(),
                },
            ),
        ],
    }
}

// cli/test/import-export: "export is raw by default and supports explicit
// sanitization" (the sanitizing half): no content survives, the shape does,
// and the result still imports.
#[test]
fn oc_export_sanitized_drops_every_piece_of_content_and_still_imports() {
    support::isolate();
    let e = rich();
    let v = export::sanitized(&e);
    let text = v.to_string();
    for secret in [
        "secret",
        "Secret",
        SECRET,
        "aGVsbG8",
        "sig-secret",
        "tree-secret",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert_eq!(v["info"]["title"], "[redacted:session-title:ses_rich]");
    assert_eq!(
        v["info"]["directory"],
        "/[redacted:session-directory:ses_rich]"
    );
    // Kept: ids, kinds, roles, tool names, the model and agent, numbers.
    for kept in [
        "\"call_1\"",
        "\"read\"",
        "\"snapshot\"",
        "\"tool_result\"",
        "\"assistant\"",
        "\"fake/m\"",
        "\"build\"",
        "\"keep_from\":3",
    ] {
        assert!(text.contains(kept), "{kept} lost: {text}");
    }
    // It parses back and imports.
    let back: Export = serde_json::from_value(v.clone()).expect("a sanitized export parses");
    assert_eq!(back.entries.len(), e.entries.len());
    let dir = tempfile::tempdir().unwrap();
    let mut db = Store::open(&dir.path().join("s.db")).unwrap();
    db.import(&back).expect("a sanitized export imports");
    // Sanitizing is stable.
    assert_eq!(export::sanitized(&e), v);
}

#[test]
fn sanitizing_keeps_empty_text_empty_and_handles_odd_values() {
    let mut e = rich();
    e.info.title = Some("   ".into());
    e.entries = vec![
        entry(1, msg(Role::User, vec![Part::text("")])),
        entry(
            2,
            msg(Role::User, vec![Part::text("é ✓ 漢字\u{0}\u{1b}[31m")]),
        ),
        entry(3, msg(Role::User, vec![Part::text("x".repeat(1 << 20))])),
    ];
    let v = export::sanitized(&e);
    assert_eq!(v["info"]["title"], "   ");
    let text = v.to_string();
    assert!(
        !text.contains("漢字") && !text.contains("xxxx"),
        "{}",
        &text[..300.min(text.len())]
    );
    assert!(text.len() < 10_000, "a huge text becomes a marker");
    serde_json::from_value::<Export>(v).unwrap();
}

#[test]
fn the_markdown_transcript_follows_opencodes_layout() {
    let md = export::markdown(&rich(), BOTH, &[]);
    assert!(
        md.starts_with("# Secret title\n\n**Session ID:** ses_rich\n**Created:** "),
        "{md}"
    );
    assert!(
        md.contains("## User\n\nuser secret prompt\n\n[Attachment: image/png]"),
        "{md}"
    );
    assert!(md.contains("## Assistant\n\n_Thinking:_\n\nthinking secret\n\nassistant secret reply\n\n**Tool: read**\n\n**Input:**\n```json\n{\n  \"path\": \"secret-path.txt\"\n}\n```\n\ntool secret output"), "{md}");
    // Tool results and the engine's system updates are no sections.
    assert_eq!(md.matches("## User").count(), 1, "{md}");
    assert_eq!(md.matches("## Assistant").count(), 1, "{md}");
    assert!(!md.contains("Effort") && !md.contains("high"), "{md}");

    let plain = export::markdown(
        &rich(),
        Transcript {
            thinking: false,
            tools: false,
        },
        &[],
    );
    assert!(!plain.contains("thinking secret"), "{plain}");
    assert!(
        !plain.contains("**Tool:") && !plain.contains("tool secret output"),
        "{plain}"
    );
    assert!(plain.contains("assistant secret reply"));
}

// Secrets in tool outputs: the credentials CodeSmith holds are scrubbed
// from every form, and URL query strings from Markdown tool text.
#[test]
fn credentials_never_reach_an_export() {
    let mut e = rich();
    e.entries.push(entry(
        20,
        msg(
            Role::User,
            vec![Part::ToolResult {
                id: "call_2".into(),
                name: "webfetch".into(),
                content: format!("https://api.example/v1?key={SECRET}&x=1 then Bearer {SECRET}"),
                is_error: true,
                media: vec![],
            }],
        ),
    ));
    e.entries.push(entry(
        19,
        msg(
            Role::Assistant,
            vec![Part::tool_call(
                "call_2",
                "webfetch",
                json!({"url": format!("https://x.example/?token={SECRET}")}),
            )],
        ),
    ));
    let secrets = vec![SECRET.to_string()];
    let md = export::markdown(&e, BOTH, &secrets);
    assert!(!md.contains(SECRET), "{md}");
    assert!(md.contains("[redacted:credential]"), "{md}");
    assert!(md.contains("https://api.example/v1?…"), "{md}");
    let raw = export::raw_json(&e, &secrets).to_string();
    assert!(!raw.contains(SECRET), "{raw}");
    assert!(raw.contains("[redacted:credential]"));
    // Raw is otherwise the stored session.
    assert!(raw.contains("tool secret output"));
    serde_json::from_str::<Export>(&raw).unwrap();
    // Short values are not taken as secrets (they would match words).
    assert_eq!(export::scrub("the cat sat", &["cat".into()]), "the cat sat");
    assert_eq!(export::scrub("", &secrets), "");
}

#[test]
fn known_secrets_come_from_config_env_logins_and_mcp() {
    support::isolate();
    std::env::set_var("OC_EXPORT_TEST_KEY", "env-key-0123456789");
    codesmith_core::auth::set_entry_in(
        &codesmith_core::paths::auth_file(),
        "saved",
        json!({"type": "oauth", "access": "access-token-0123456789", "refresh": "refresh-token-0123456789"}),
    )
    .unwrap();
    let c: codesmith_core::config::Config = serde_json::from_value(json!({
        "providers": {
            "p": {"settings": {"baseURL": "https://p.invalid/v1", "apiKey": "config-key-0123456789"}, "env": ["OC_EXPORT_TEST_KEY"]},
            "saved": {"settings": {"baseURL": "https://s.invalid/v1"}}
        },
        "mcp": {"servers": {"r": {"type": "remote", "url": "https://m.invalid", "headers": {"Authorization": "Bearer mcp-token-0123456789"}}}}
    }))
    .unwrap();
    let s = export::known_secrets(&c);
    for want in [
        "config-key-0123456789",
        "env-key-0123456789",
        "access-token-0123456789",
        "refresh-token-0123456789",
        "mcp-token-0123456789",
        "Bearer mcp-token-0123456789",
    ] {
        assert!(s.contains(&want.to_string()), "{want}: {s:?}");
    }
    assert!(!s.contains(&"oauth".to_string()));
    // Longest first, so a secret containing another goes whole.
    assert!(s.windows(2).all(|w| w[0].len() >= w[1].len()));
    let text = export::scrub("Authorization: Bearer mcp-token-0123456789", &s);
    assert_eq!(text, "Authorization: [redacted:credential]");
    std::env::remove_var("OC_EXPORT_TEST_KEY");
}

#[test]
fn markdown_is_safe_to_print_and_fences_hold() {
    let mut e = Export {
        info: info("ses_odd", None),
        entries: vec![],
    };
    // An empty session: the header alone, titled by its id.
    let md = export::markdown(&e, BOTH, &[]);
    assert!(md.starts_with("# Session ses_odd\n"), "{md}");
    assert!(!md.contains("## "), "{md}");
    e.entries = vec![
        entry(
            1,
            msg(
                Role::User,
                vec![Part::text("bell\u{7} esc\u{1b}[2J é ✓ 漢字\ttab")],
            ),
        ),
        entry(
            2,
            msg(
                Role::Assistant,
                vec![Part::tool_call(
                    "c",
                    "shell",
                    json!({"command": "echo ```"}),
                )],
            ),
        ),
    ];
    let md = export::markdown(&e, BOTH, &[]);
    assert!(!md.contains('\u{7}') && !md.contains('\u{1b}'), "{md:?}");
    assert!(md.contains("bell esc[2J é ✓ 漢字\ttab"), "{md}");
    // The input holds three backticks, so its fence is four.
    assert!(md.contains("````json\n"), "{md}");
    // A tool call with no result yet has empty output.
    assert!(md.contains("**Tool: shell**"));
}

#[test]
fn default_names_are_plain_file_names() {
    assert_eq!(export::default_name("ses_1", false), "session-ses_1.md");
    assert_eq!(export::default_name("a/../b", true), "session-a____b.json");
    assert_eq!(export::default_name("", false), "session-.md");
}

#[cfg(unix)]
#[test]
fn saving_creates_a_private_file_and_never_overwrites_unasked() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.md");
    export::save(&path, "first", false).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
    let e = export::save(&path, "second", false).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
    // Confirmed, it is replaced, still private.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    export::save(&path, "second", true).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    assert_eq!(mode(&path), 0o600);
    // A folder, a missing folder and an empty name are refused.
    assert!(export::save(dir.path(), "x", true).is_err());
    assert!(export::save(&dir.path().join("missing/out.md"), "x", false).is_err());
    assert!(!dir.path().join("missing").exists(), "no folder is created");
    assert!(export::save(std::path::Path::new(""), "x", false).is_err());
    // A symlink there is never followed: refused, and replaced (not its
    // target) only when confirmed.
    let target = dir.path().join("target.txt");
    std::fs::write(&target, "keep").unwrap();
    let link = dir.path().join("link.md");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(
        export::save(&link, "x", false).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    export::save(&link, "replaced", true).unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    assert!(!std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    let dangling = dir.path().join("dangling.md");
    std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
    assert!(export::save(&dangling, "x", false).is_err());
    assert!(!dir.path().join("nowhere").exists());
    // Unicode names work.
    let u = dir.path().join("导出 ✓.md");
    export::save(&u, "ü", false).unwrap();
    assert_eq!(std::fs::read_to_string(&u).unwrap(), "ü");
}

// A real session through the store: what the transcript shows matches what
// was said and run.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_session_exports_its_turns() {
    let mut h = H::new().await;
    std::fs::write(h.root().join("notes.txt"), "the notes\n").unwrap();
    h.prompt(&format!("call read {}", json!({"path": "notes.txt"})))
        .await;
    let e = h.store().export(&h.s.id).unwrap();
    let md = export::markdown(&e, BOTH, &[]);
    assert!(md.contains("## User\n\ncall read"), "{md}");
    assert!(md.contains("**Tool: read**"), "{md}");
    assert!(md.contains("the notes"), "{md}");
    assert!(md.contains("done"), "{md}");
    let v: Value = export::sanitized(&e);
    assert!(!v.to_string().contains("the notes"));
    serde_json::from_value::<Export>(v).unwrap();
}
