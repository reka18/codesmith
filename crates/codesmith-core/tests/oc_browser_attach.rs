//! OpenCode's `browser-attach.test.ts` and `browser-idle.test.ts` (MIT): a
//! browser attached to a session grants it the browser rules for exactly as
//! long as the attachment lasts. Replacing an attachment, disposing the
//! registry and idle eviction each end a grant without leaving it behind or
//! taking the replacement's with it. CodeSmith has no browser driver; these
//! drive the attachment lifecycle and the permission checks in front of
//! one (`codesmith_core::plugin::browser`).
//!
//! Idleness is measured on a manual clock the tests move, so nothing waits
//! on wall time except a deadline over the background sweep.

mod support;

use codesmith_core::permission::{
    Asker, Effect, Gate, Granted, Policy, PolicyEffect, Reply, Request, Rule,
};
use codesmith_core::plugin::browser::{
    self, BrowserError, Browsers, Driver, Ended, ManualClock, Options,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

const TTL: Duration = Duration::from_secs(60);
const URL: &str = "https://example.test/";

/// A driver that records what it opened.
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl Driver for Recorder {
    fn open(&self, session: &str, url: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(format!("{session} {url}"));
        Ok(())
    }
}

/// Counts the requests it is asked, answering each with `reply`.
struct Counting(Arc<AtomicUsize>, Reply);

impl Asker for Counting {
    fn ask(&self, _: &Request) -> Reply {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.clone()
    }
}

struct Fixture {
    browsers: Arc<Browsers>,
    clock: Arc<ManualClock>,
    driver: Arc<Recorder>,
}

fn fixture(sweep_every: Option<Duration>) -> Fixture {
    let clock = Arc::new(ManualClock::default());
    let driver = Arc::new(Recorder::default());
    let browsers = Browsers::new(Options {
        time_to_live: TTL,
        sweep_every,
        clock: clock.clone(),
        driver: driver.clone(),
    });
    Fixture {
        browsers,
        clock,
        driver,
    }
}

/// OpenCode's session rules in these tests: the user's own `bash * ask`.
fn user() -> Rule {
    Rule::new("bash", "*", Effect::Ask)
}

/// Session `id`'s gate over `rules`, reading `browsers`' grants, and how
/// many times it has asked (answering `reply`).
fn gate(
    browsers: &Arc<Browsers>,
    id: &str,
    rules: Vec<Rule>,
    reply: Reply,
) -> (Gate, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let mut g = Gate::new(rules, Arc::new(Counting(asked.clone(), reply)));
    g.set_granted(Some(browsers.grants_for(id)));
    (g, asked)
}

/// The session's rules as OpenCode's `session.permissions` reads them: its
/// own, then what is granted.
fn permissions(g: &Gate) -> Vec<Rule> {
    let mut all = g.configured().to_vec();
    all.extend(g.granted());
    all
}

fn with_grant() -> Vec<Rule> {
    let mut all = vec![user()];
    all.extend(browser::rules());
    all
}

fn allowed(g: &Gate, action: &str, resource: &str) -> bool {
    g.allows(action, &[resource.to_string()])
}

// --- browser-attach.test.ts ---

#[test]
fn oc_browser_a_replaced_attachments_revoke_never_removes_its_replacements_grant() {
    let f = fixture(None);
    let (g, _) = gate(&f.browsers, "ses_1", vec![user()], Reply::Reject(None));
    let first = f.browsers.attach("ses_1", "conn_1").unwrap();
    assert_eq!(permissions(&g), with_grant());

    let second = f.browsers.attach("ses_1", "conn_2").unwrap();
    assert_eq!(first.ended_now(), Some(Ended::Replaced));
    assert_eq!(f.browsers.connection("ses_1").as_deref(), Some("conn_2"));
    // The replaced attachment's revoke lands after the replacement's grant.
    drop(first);
    assert_eq!(permissions(&g), with_grant());
    assert!(allowed(&g, "browser", URL));

    drop(second);
    assert_eq!(permissions(&g), vec![user()]);
    assert!(!allowed(&g, "browser", URL));
}

/// OpenCode's interleaving (a revoke reads the session, the replacement's
/// grant writes, the stale revoke writes) raced on threads: whatever the
/// order, the replacement keeps its grant, and it goes with it.
#[test]
fn oc_browser_a_stale_revoke_racing_a_replacement_never_drops_its_grant() {
    let f = fixture(None);
    let (g, _) = gate(&f.browsers, "ses_race", vec![user()], Reply::Reject(None));
    for _ in 0..200 {
        let first = f.browsers.attach("ses_race", "conn_1").unwrap();
        let go = Arc::new(Barrier::new(2));
        let revoke = {
            let go = go.clone();
            std::thread::spawn(move || {
                go.wait();
                drop(first);
            })
        };
        let attach = {
            let (go, b) = (go.clone(), f.browsers.clone());
            std::thread::spawn(move || {
                go.wait();
                b.attach("ses_race", "conn_2").unwrap()
            })
        };
        revoke.join().unwrap();
        let second = attach.join().unwrap();
        assert_eq!(second.ended_now(), None);
        assert_eq!(permissions(&g), with_grant());
        drop(second);
        assert_eq!(permissions(&g), vec![user()]);
    }
}

#[tokio::test]
async fn oc_browser_disposing_the_plugin_revokes_the_browser_rules_before_the_location_finishes_closing(
) {
    let f = fixture(None);
    let (g, _) = gate(&f.browsers, "ses_1", vec![user()], Reply::Reject(None));
    let mut attachment = f.browsers.attach("ses_1", "conn_1").unwrap();
    assert_eq!(permissions(&g), with_grant());

    // What closes after the plugin (the attachment's holder hearing it end)
    // must already find the rules gone.
    let seen = {
        let g = g.clone();
        tokio::spawn(async move {
            let why = attachment.ended().await;
            (why, permissions(&g))
        })
    };
    f.browsers.dispose();
    assert_eq!(permissions(&g), vec![user()]);
    let (why, then) = tokio::time::timeout(Duration::from_secs(10), seen)
        .await
        .expect("the attachment heard the dispose")
        .unwrap();
    assert_eq!(why, Ended::Disposed);
    assert_eq!(then, vec![user()]);
    assert_eq!(
        f.browsers.attach("ses_1", "conn_2").err(),
        Some(BrowserError::Unavailable)
    );
}

// --- browser-idle.test.ts ---

#[tokio::test]
async fn oc_browser_idle_eviction_ends_the_browser_attachment_and_permits_a_fresh_attachment() {
    // A real background sweep, as OpenCode's; only the test moves its clock.
    let f = fixture(Some(Duration::from_millis(10)));
    let (g, _) = gate(&f.browsers, "ses_idle", vec![], Reply::Reject(None));
    let mut pending = f.browsers.attach("ses_idle", "conn_1").unwrap();
    pending.touch().unwrap();
    assert!(allowed(&g, "browser", URL));

    // Idle for its whole time to live: the next sweep evicts it.
    f.clock.advance(TTL);
    let why = tokio::time::timeout(Duration::from_secs(10), pending.ended())
        .await
        .expect("the sweep ended the attachment");
    assert_eq!(why, Ended::Evicted);
    assert!(!f.browsers.attached("ses_idle"));
    assert!(!allowed(&g, "browser", URL));
    assert_eq!(pending.touch(), Err(BrowserError::Unavailable));

    let resumed = f.browsers.attach("ses_idle", "conn_1").unwrap();
    resumed.touch().unwrap();
    assert_eq!(resumed.ended_now(), None);
    assert!(allowed(&g, "browser", URL));
    // The evicted handle's drop leaves the fresh attachment alone.
    drop(pending);
    assert!(allowed(&g, "browser", URL));
    drop(resumed);
    assert!(!allowed(&g, "browser", URL));
}

// --- Guardrails ---

#[test]
fn a_browser_grant_never_lifts_a_configured_deny() {
    let f = fixture(None);
    let rules = vec![Rule::new("browser", "https://blocked.test/*", Effect::Deny)];
    let (g, asked) = gate(&f.browsers, "ses_deny", rules, Reply::Once);
    let _a = f.browsers.attach("ses_deny", "conn").unwrap();
    assert!(matches!(
        f.browsers.open(&g, "ses_deny", "https://blocked.test/x"),
        Err(BrowserError::Denied(_))
    ));
    assert!(!allowed(&g, "browser", "https://blocked.test/x"));
    f.browsers.open(&g, "ses_deny", URL).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert_eq!(*f.driver.0.lock().unwrap(), vec![format!("ses_deny {URL}")]);
}

#[test]
fn a_browser_grant_never_lifts_a_built_in_or_policy_deny() {
    let f = fixture(None);
    let builtin = vec![Rule::new("browser", "*", Effect::Deny).into_builtin()];
    let policy = vec![Policy::tool(PolicyEffect::Deny, "browser:*")
        .rule()
        .unwrap()];
    for (i, rules) in [builtin, policy].into_iter().enumerate() {
        let id = format!("ses_floor_{i}");
        let (g, asked) = gate(&f.browsers, &id, rules, Reply::Once);
        let _a = f.browsers.attach(&id, "conn").unwrap();
        assert!(matches!(
            f.browsers.open(&g, &id, URL),
            Err(BrowserError::Denied(_))
        ));
        assert_eq!(asked.load(Ordering::SeqCst), 0);
    }
    assert!(f.driver.0.lock().unwrap().is_empty());
}

#[test]
fn a_browser_grant_replaces_a_configured_ask_only_while_attached() {
    let f = fixture(None);
    let rules = vec![Rule::new("browser", "*", Effect::Ask)];
    let (g, asked) = gate(&f.browsers, "ses_ask", rules, Reply::Reject(None));
    let a = f.browsers.attach("ses_ask", "conn").unwrap();
    f.browsers.open(&g, "ses_ask", URL).unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    drop(a);
    let _b = f.browsers.attach("ses_other", "conn").unwrap();
    assert!(matches!(
        f.browsers.open(&g, "ses_ask", URL),
        Err(BrowserError::NotAttached(_))
    ));
    assert!(!allowed(&g, "browser", URL));
}

#[test]
fn opening_a_file_keeps_the_secret_guards_despite_the_grant() {
    let f = fixture(None);
    let root = tempfile::tempdir().unwrap();
    let mut rules = vec![Rule::new("read", "*", Effect::Allow)];
    rules.extend(codesmith_core::permission::secret_guards_for(
        &[],
        &[],
        &[],
        &[],
    ));
    let (g, asked) = gate(&f.browsers, "ses_guard", rules, Reply::Reject(None));
    let _a = f.browsers.attach("ses_guard", "conn").unwrap();
    f.browsers
        .open_file(&g, "ses_guard", root.path(), "index.html")
        .unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    // The guard asks (and the user declines), grant or no grant.
    assert!(matches!(
        f.browsers.open_file(&g, "ses_guard", root.path(), ".env"),
        Err(BrowserError::Denied(_))
    ));
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    for bad in ["../outside.html", "/etc/hosts", ""] {
        assert!(matches!(
            f.browsers.open_file(&g, "ses_guard", root.path(), bad),
            Err(BrowserError::Invalid(_))
        ));
    }
    assert_eq!(f.driver.0.lock().unwrap().len(), 1);
}

/// Whatever a grant source hands the gate, only plain allows count, and
/// they never lift a guard: a later layer cannot.
#[test]
fn a_grant_source_cannot_lift_a_guard_or_smuggle_a_deny_marker() {
    struct Hostile;
    impl Granted for Hostile {
        fn rules(&self) -> Vec<Rule> {
            vec![
                Rule::new("read", "*", Effect::Allow),
                Rule::new("read", "*.env", Effect::Allow),
                Rule::new("read", "*.env", Effect::Allow).into_guard(),
                Rule::new("edit", "*", Effect::Allow).into_builtin(),
            ]
        }
    }
    let asked = Arc::new(AtomicUsize::new(0));
    let mut g = Gate::new(
        codesmith_core::permission::secret_guards_for(&[], &[], &[], &[]),
        Arc::new(Counting(asked.clone(), Reply::Reject(None))),
    );
    g.set_granted(Some(Arc::new(Hostile)));
    assert_eq!(
        g.granted(),
        vec![
            Rule::new("read", "*", Effect::Allow),
            Rule::new("read", "*.env", Effect::Allow),
        ]
    );
    assert!(allowed(&g, "read", "/w/index.html"));
    assert!(!allowed(&g, "read", "/w/.env"));
    let req = Request {
        action: "read".into(),
        resources: vec!["/w/.env".into()],
        save: vec![],
        title: String::new(),
    };
    assert!(g.assert(&req).is_err());
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

#[test]
fn concurrent_attaches_leave_one_attachment_and_one_grant() {
    let f = fixture(None);
    let (g, _) = gate(&f.browsers, "ses_many", vec![user()], Reply::Reject(None));
    let go = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let (go, b) = (go.clone(), f.browsers.clone());
            std::thread::spawn(move || {
                go.wait();
                b.attach("ses_many", &format!("conn_{i}")).unwrap()
            })
        })
        .collect();
    let mut all: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let live: Vec<_> = all.iter().filter(|a| a.ended_now().is_none()).collect();
    assert_eq!(live.len(), 1);
    assert_eq!(
        f.browsers.connection("ses_many").as_deref(),
        Some(live[0].connection())
    );
    assert!(all
        .iter()
        .filter(|a| a.ended_now().is_some())
        .all(|a| a.ended_now() == Some(Ended::Replaced)));
    assert_eq!(permissions(&g), with_grant());
    // Every replaced handle goes first: the grant stays until the last.
    all.sort_by_key(|a| a.ended_now().is_none());
    let last = all.pop().unwrap();
    drop(all);
    assert_eq!(permissions(&g), with_grant());
    drop(last);
    assert_eq!(permissions(&g), vec![user()]);
}

#[test]
fn the_idle_boundary_evicts_at_the_time_to_live_not_before() {
    let f = fixture(None);
    let (g, _) = gate(&f.browsers, "ses_edge", vec![], Reply::Reject(None));
    let a = f.browsers.attach("ses_edge", "conn").unwrap();
    f.clock.advance(TTL - Duration::from_millis(1));
    assert_eq!(f.browsers.sweep(), 0);
    assert!(allowed(&g, "browser", URL));
    // Activity starts the idle time over.
    a.touch().unwrap();
    f.clock.advance(TTL - Duration::from_millis(1));
    assert!(allowed(&g, "browser", URL));
    assert_eq!(a.ended_now(), None);
    // At the deadline the grant stops counting, before any sweep runs.
    f.clock.advance(Duration::from_millis(1));
    assert!(!allowed(&g, "browser", URL));
    assert_eq!(a.ended_now(), Some(Ended::Evicted));
    assert_eq!(f.browsers.sweep(), 0);
}

#[test]
fn a_sweep_evicts_only_the_idle_attachments() {
    let f = fixture(None);
    let idle = f.browsers.attach("ses_a", "conn").unwrap();
    f.clock.advance(TTL / 2);
    let busy = f.browsers.attach("ses_b", "conn").unwrap();
    f.clock.advance(TTL / 2);
    assert_eq!(f.browsers.sweep(), 1);
    assert_eq!(idle.ended_now(), Some(Ended::Evicted));
    assert_eq!(busy.ended_now(), None);
    assert!(f.browsers.attached("ses_b"));
}

#[test]
fn two_sessions_are_isolated() {
    let f = fixture(None);
    let (g1, _) = gate(&f.browsers, "ses_one", vec![], Reply::Reject(None));
    let (g2, _) = gate(&f.browsers, "ses_two", vec![], Reply::Reject(None));
    let one = f.browsers.attach("ses_one", "conn").unwrap();
    assert!(allowed(&g1, "browser", URL));
    assert!(!allowed(&g2, "browser", URL));
    assert!(matches!(
        f.browsers.open(&g2, "ses_two", URL),
        Err(BrowserError::NotAttached(_))
    ));
    let two = f.browsers.attach("ses_two", "conn").unwrap();
    assert_eq!(one.ended_now(), None);
    drop(two);
    assert!(allowed(&g1, "browser", URL));
    assert!(!allowed(&g2, "browser", URL));
    drop(one);
    assert!(!allowed(&g1, "browser", URL));
}

#[test]
fn detaching_a_replaced_handle_is_a_no_op() {
    let f = fixture(None);
    let first = f.browsers.attach("ses_x", "conn_1").unwrap();
    let second = f.browsers.attach("ses_x", "conn_2").unwrap();
    first.detach();
    assert_eq!(second.ended_now(), None);
    assert_eq!(f.browsers.granted("ses_x"), browser::rules());
    second.touch().unwrap();
    second.detach();
    assert!(f.browsers.granted("ses_x").is_empty());
}

#[test]
fn disposing_twice_is_safe_and_ends_every_session() {
    let f = fixture(None);
    let a = f.browsers.attach("ses_a", "conn").unwrap();
    let b = f.browsers.attach("ses_b", "conn").unwrap();
    f.browsers.dispose();
    f.browsers.dispose();
    assert!(f.browsers.is_disposed());
    assert_eq!(a.ended_now(), Some(Ended::Disposed));
    assert_eq!(b.ended_now(), Some(Ended::Disposed));
    assert_eq!(a.touch(), Err(BrowserError::Unavailable));
    assert!(f.browsers.granted("ses_a").is_empty());
    let (g, _) = gate(&f.browsers, "ses_a", vec![], Reply::Once);
    assert_eq!(
        f.browsers.open(&g, "ses_a", URL),
        Err(BrowserError::Unavailable)
    );
    drop((a, b));
    assert_eq!(f.browsers.sweep(), 0);
}

#[test]
fn only_http_and_file_urls_open() {
    let f = fixture(None);
    let (g, _) = gate(&f.browsers, "ses_u", vec![], Reply::Once);
    let _a = f.browsers.attach("ses_u", "conn").unwrap();
    for bad in [
        "javascript:alert(1)",
        "data:text/html,x",
        "chrome://settings",
        "ftp://x",
    ] {
        assert!(matches!(
            f.browsers.open(&g, "ses_u", bad),
            Err(BrowserError::Invalid(_))
        ));
    }
    assert!(f.driver.0.lock().unwrap().is_empty());
}

/// A real session's gate reads the process's registry, under its id.
#[tokio::test]
async fn a_session_is_granted_the_browser_rules_while_attached() {
    let ask = json!({"permissions": [{"action": "browser", "resource": "*", "effect": "ask"}]});
    let mut h = support::H::with(ask.clone(), None).await;
    let stranger = support::H::with(ask, None).await;
    let global = Browsers::global();
    let id = h.s.id.clone();
    assert!(!h.s.allows("browser", URL));
    let a = global.attach(&id, "conn").unwrap();
    assert!(h.s.allows("browser", URL));
    assert!(h.s.allows("skill", "browser"));
    // Another session is unaffected.
    assert!(!stranger.s.allows("browser", URL));
    // A new id takes the session's grants with it.
    h.s.set_id(format!("{id}_renamed"));
    assert!(!h.s.allows("browser", URL));
    let b = global.attach(&format!("{id}_renamed"), "conn").unwrap();
    assert!(h.s.allows("browser", URL));
    drop(b);
    assert!(!h.s.allows("browser", URL));
    drop(a);
    assert!(!global.attached(&id));
}
