//! Built-in secret-file read guards (security review 8, finding 1): every
//! agent asks before reading a file that holds credentials, a blanket
//! configured allow or a saved approval never lifts that, and only a
//! configured allow naming the path does. The plan agent keeps reading
//! other outside files without asking, as in OpenCode.
//!
//! These judge rules only: no file is opened, and the home folder's paths
//! are only spelled, never read.

use codesmith_core::agent;
use codesmith_core::approvals::Book;
use codesmith_core::config::Config;
use codesmith_core::permission::{
    evaluate, guarded, secret_guards_for, Asker, Effect, Gate, Reply, Request, Rule,
};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn config(v: serde_json::Value) -> Config {
    serde_json::from_value(v).unwrap()
}

fn home() -> String {
    dirs::home_dir()
        .unwrap()
        .display()
        .to_string()
        .trim_end_matches('/')
        .to_string()
}

/// One file under each guarded place, spelled from the real home folder.
fn secret_paths() -> Vec<String> {
    let h = home();
    let data = codesmith_core::paths::data_dir().display().to_string();
    let conf = codesmith_core::paths::config_dir().display().to_string();
    vec![
        format!("{h}/.ssh/id_ed25519"),
        format!("{h}/.ssh"),
        format!("{h}/.ssh/config"),
        format!("{h}/.aws/credentials"),
        format!("{h}/.aws/sso/cache/x.json"),
        format!("{h}/.gnupg/private-keys-v1.d/k.key"),
        format!("{h}/.netrc"),
        format!("{h}/.config/gh/hosts.yml"),
        format!("{h}/src/app/.git-credentials"),
        ".git-credentials".into(),
        format!("{h}/.docker/config.json"),
        format!("{h}/.kube/config"),
        format!("{h}/.npmrc"),
        format!("{h}/.pypirc"),
        format!("{data}/auth.json"),
        format!("{data}/auth.json.bak"),
        format!("{data}/mcp-auth.json"),
        format!("{conf}/codesmith.json"),
        format!("{conf}/codesmith.jsonc"),
        format!("{h}/.minisign/codesmith.key"),
        format!("{h}/.minisign"),
        ".env".into(),
        "config/.env.local".into(),
        format!("{h}/proj/.env"),
    ]
}

const AGENTS: [&str; 5] = ["build", "plan", "general", "explore", "review"];

/// The probe of the review: the plan agent read `~/.ssh`, the saved keys
/// and AWS credentials without asking. Now every agent asks (review,
/// which never leaves the project, still denies).
#[test]
fn every_secret_path_asks_for_every_agent() {
    let c = config(json!({}));
    for id in AGENTS {
        let a = agent::resolve(&c, id).unwrap();
        for p in secret_paths() {
            let e = evaluate("read", &p, &a.permissions);
            assert_ne!(e, Effect::Allow, "{id} read {p}");
            if id == "build" || id == "plan" {
                assert_eq!(e, Effect::Ask, "{id} read {p}");
            }
        }
    }
}

/// Custom agents start from the base rules, guards included.
#[test]
fn a_custom_agent_inherits_the_guards() {
    let c = config(json!({"agents": {"mine": {"permissions": [
        {"action": "read", "resource": "*", "effect": "allow"}
    ]}}}));
    let a = agent::resolve(&c, "mine").unwrap();
    for p in secret_paths() {
        assert_eq!(evaluate("read", &p, &a.permissions), Effect::Ask, "{p}");
    }
}

/// OpenCode parity kept: the plan agent reaches other outside files
/// without asking, and reads ordinary files.
#[test]
fn plan_still_reads_ordinary_outside_files() {
    let c = config(json!({}));
    let plan = agent::resolve(&c, "plan").unwrap();
    let h = home();
    for p in [
        "/etc/hosts".to_string(),
        format!("{h}/notes/todo.md"),
        format!("{h}/.zshrc"),
        format!("{h}/.ssh-notes.txt"),
        format!("{h}/.sshx/config"),
        format!("{h}/.config/gh/config.yml"),
        format!("{h}/.docker/daemon.json"),
        format!("{h}/.kube/cache/x"),
        format!("{h}/proj/.env.example"),
        format!("{h}/proj/.envrc"),
        "src/main.rs".into(),
    ] {
        assert_eq!(
            evaluate("read", &p, &plan.permissions),
            Effect::Allow,
            "{p}"
        );
    }
    assert_eq!(
        evaluate(
            "external_directory",
            &format!("{h}/notes/*"),
            &plan.permissions
        ),
        Effect::Allow
    );
}

/// A blanket configured allow, at any level, does not lift a guard.
#[test]
fn broad_configured_allows_do_not_lift_the_guards() {
    for rules in [
        json!([{"action": "read", "resource": "*", "effect": "allow"}]),
        json!([{"action": "*", "resource": "*", "effect": "allow"}]),
        json!([{"action": "read", "resource": "~/*", "effect": "allow"}]),
        json!([{"action": "read", "resource": "~/.ssh/id_*", "effect": "allow"}]),
        json!([{"action": "read", "resource": "/*", "effect": "allow"}]),
    ] {
        for c in [
            config(json!({"permissions": rules})),
            config(
                json!({"agents": {"build": {"permissions": rules}, "plan": {"permissions": rules}}}),
            ),
        ] {
            for id in ["build", "plan"] {
                let a = agent::resolve(&c, id).unwrap();
                for p in secret_paths() {
                    assert_eq!(
                        evaluate("read", &p, &a.permissions),
                        Effect::Ask,
                        "{id} {rules} {p}"
                    );
                }
                assert_eq!(evaluate("read", "src/a.rs", &a.permissions), Effect::Allow);
            }
        }
    }
}

/// Decision: a configured allow lifts a guard only when it names the path
/// itself (no wildcard), or is letter for letter the guard's own pattern.
/// It lifts that path alone; a configured deny still denies.
#[test]
fn an_explicit_configured_allow_lifts_only_its_path() {
    let h = home();
    let c = config(json!({"permissions": [
        {"action": "read", "resource": "~/.ssh/config", "effect": "allow"},
        {"action": "read", "resource": ".env", "effect": "allow"},
        {"action": "read", "resource": format!("{h}/.aws/*"), "effect": "allow"}
    ]}));
    let b = agent::resolve(&c, "build").unwrap();
    let p = &b.permissions;
    assert_eq!(
        evaluate("read", &format!("{h}/.ssh/config"), p),
        Effect::Allow
    );
    assert_eq!(
        evaluate("read", &format!("{h}/.ssh/id_ed25519"), p),
        Effect::Ask
    );
    assert_eq!(evaluate("read", ".env", p), Effect::Allow);
    assert_eq!(evaluate("read", "app/.env", p), Effect::Ask);
    assert_eq!(
        evaluate("read", &format!("{h}/.aws/credentials"), p),
        Effect::Allow
    );
    let deny = config(json!({"permissions": [
        {"action": "read", "resource": "~/.ssh/config", "effect": "deny"}
    ]}));
    let b = agent::resolve(&deny, "build").unwrap();
    assert_eq!(
        evaluate("read", &format!("{h}/.ssh/config"), &b.permissions),
        Effect::Deny
    );
    // A later broad allow hides the explicit one: the last configured
    // match decides whether the guard is lifted.
    let later = config(json!({"permissions": [
        {"action": "read", "resource": "~/.ssh/config", "effect": "allow"},
        {"action": "read", "resource": "*", "effect": "allow"}
    ]}));
    let b = agent::resolve(&later, "build").unwrap();
    assert_eq!(
        evaluate("read", &format!("{h}/.ssh/config"), &b.permissions),
        Effect::Ask
    );
}

struct Counting(AtomicUsize, Reply);
impl Asker for Counting {
    fn ask(&self, _: &Request) -> Reply {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.clone()
    }
}

fn read_req(p: &str) -> Request {
    Request {
        action: "read".into(),
        resources: vec![p.into()],
        save: vec!["*".into()],
        title: format!("Read {p}"),
    }
}

/// Saved approvals for the project, even ones naming the path, never lift
/// a guard: the user is asked every time.
#[test]
fn saved_approvals_do_not_lift_the_guards() {
    let d = tempfile::tempdir().unwrap();
    let book = Arc::new(Book::open(&d.path().join("approvals.db")).unwrap());
    let h = home();
    let key = format!("{h}/.ssh/id_ed25519");
    book.add(
        "p",
        "read",
        &[
            "*".into(),
            key.clone(),
            format!("{h}/.ssh/*"),
            ".env".into(),
        ],
    )
    .unwrap();
    for id in ["build", "plan"] {
        let a = agent::resolve(&config(json!({})), id).unwrap();
        let asker = Arc::new(Counting(AtomicUsize::new(0), Reply::Once));
        let mut g = Gate::new(a.permissions.clone(), asker.clone());
        g.set_durable(book.clone(), "p".into());
        assert!(!g.allows("read", std::slice::from_ref(&key)), "{id}");
        assert!(!g.allows("read", &[".env".to_string()]), "{id}");
        assert!(g.allows("read", &["src/a.rs".to_string()]), "{id}");
        g.assert(&read_req(&key)).unwrap();
        g.assert(&read_req(".env")).unwrap();
        assert_eq!(asker.0.load(Ordering::SeqCst), 2, "{id}");
    }
}

/// "Always" for a guarded read saves nothing (no rule could lift the guard
/// anyway), so the next read asks again and other reads stay as they were.
#[test]
fn always_on_a_guarded_read_saves_nothing() {
    let h = home();
    let key = format!("{h}/.aws/credentials");
    let mut rules = codesmith_core::permission::base_rules();
    rules.push(Rule::new("read", "*", Effect::Ask));
    let asker = Arc::new(Counting(AtomicUsize::new(0), Reply::Always));
    let g = Gate::new(rules, asker.clone());
    g.assert(&read_req(&key)).unwrap();
    assert!(g.saved().is_empty(), "{:?}", g.saved());
    g.assert(&read_req(&key)).unwrap();
    assert_eq!(asker.0.load(Ordering::SeqCst), 2);
    // An unguarded "always" still saves as before.
    g.assert(&read_req("src/a.rs")).unwrap();
    assert_eq!(g.saved().len(), 1);
    assert!(g.allows("read", &["src/b.rs".to_string()]));
    assert!(!g.allows("read", &[key]));
    // A session's own approvals shared from elsewhere do not lift it either.
    let mut other = Gate::new(codesmith_core::permission::base_rules(), asker.clone());
    other.share_approvals(Arc::new(std::sync::Mutex::new(vec![Rule::new(
        "read",
        &format!("{h}/.aws/credentials"),
        Effect::Allow,
    )])));
    assert!(!other.allows("read", &[format!("{h}/.aws/credentials")]));
}

/// Unicode: a guarded folder under a home folder with non-ASCII letters,
/// `.env` files in non-ASCII folders, and look-alike names that are not
/// the guarded ones.
#[test]
fn unicode_paths() {
    let homes = [PathBuf::from("/Users/zoë/ホーム")];
    let rules = {
        let mut r = vec![Rule::new("*", "*", Effect::Allow)];
        r.extend(secret_guards_for(&homes, &[], &[], &[]));
        r
    };
    let ask = |p: &str| evaluate("read", p, &rules);
    assert_eq!(ask("/Users/zoë/ホーム/.ssh/id_ed25519"), Effect::Ask);
    assert_eq!(ask("/Users/zoë/ホーム/.ssh/鍵"), Effect::Ask);
    assert_eq!(ask("/Users/zoë/ホーム/.aws/credentials"), Effect::Ask);
    assert_eq!(ask("données/.env"), Effect::Ask);
    assert_eq!(ask("données/.env.prod🔑"), Effect::Ask);
    assert_eq!(ask("données/.env.example"), Effect::Allow);
    // Cyrillic `ѕ` in `.ѕsh`, a full-width dot: other files.
    assert_eq!(
        ask("/Users/zoë/ホーム/.\u{0455}sh/id_ed25519"),
        Effect::Allow
    );
    assert_eq!(
        ask("/Users/zoë/ホーム/\u{ff0e}ssh/id_ed25519"),
        Effect::Allow
    );
    // Another home folder (the decomposed `ë`) is not this one.
    assert_eq!(ask("/Users/zoe\u{308}/ホーム/notes.md"), Effect::Allow);
    // The `.env.example` carve-out never reaches inside a guarded folder.
    assert_eq!(ask("/Users/zoë/ホーム/.ssh/x.env.example"), Effect::Ask);
    assert!(guarded(
        "read",
        &["/Users/zoë/ホーム/.gnupg".to_string()],
        &rules
    ));
    assert!(!guarded(
        "read",
        &["/Users/zoë/ホーム/a".to_string()],
        &rules
    ));
    // Only `read` is guarded.
    assert_eq!(
        evaluate("edit", "/Users/zoë/ホーム/.ssh/config", &rules),
        Effect::Allow
    );
}

/// On a volume that ignores case, a differently cased name is the same
/// file, and asks too.
#[cfg(target_os = "macos")]
#[test]
fn case_variants_ask_on_macos() {
    let h = home();
    let b = agent::resolve(&config(json!({})), "build").unwrap();
    for p in [
        format!("{h}/.SSH/id_ed25519"),
        format!("{h}/.Aws/Credentials"),
        ".ENV".into(),
    ] {
        assert_eq!(evaluate("read", &p, &b.permissions), Effect::Ask, "{p}");
    }
}

/// The resolved spelling of a folder reached through a symlink is guarded
/// too: the guards list each folder as written and as resolved.
#[cfg(unix)]
#[test]
fn guards_name_folders_as_written_and_resolved() {
    let d = tempfile::tempdir().unwrap();
    let real = d.path().canonicalize().unwrap().join("real-home");
    std::fs::create_dir_all(real.join(".ssh")).unwrap();
    let link = d.path().canonicalize().unwrap().join("link-home");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut homes = vec![link.clone()];
    if let Ok(r) = link.canonicalize() {
        homes.push(r);
    }
    let rules = secret_guards_for(&homes, &[], &[], &[]);
    for p in [link.join(".ssh/id_rsa"), real.join(".ssh/id_rsa")] {
        assert_eq!(
            evaluate("read", &p.display().to_string(), &rules),
            Effect::Ask,
            "{}",
            p.display()
        );
    }
}
