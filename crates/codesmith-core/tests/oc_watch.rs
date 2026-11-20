//! Ports of OpenCode's file watching tests (`packages/core/test/filesystem/`
//! `file-watch.test.ts`, `location-watcher-policy.test.ts`,
//! `watcher.test.ts`, MIT) against `codesmith_core::watch`. See
//! `crates/codesmith-core/PARITY.md`.
//!
//! OpenCode's Effect scopes and fibers become Rust drops and tokio tasks:
//! interrupting a consumer is aborting its task, closing a layer is
//! `Watcher::shutdown`, and `TestClock.adjust` is tokio's paused clock.

use codesmith_core::watch::file_watch::{self, FileWatch};
use codesmith_core::watch::location::{self, LocationWatcher, Vcs};
use codesmith_core::watch::native::{Mode, Notify};
use codesmith_core::watch::policy::{Policy, Registration};
use codesmith_core::watch::testing::TestNative;
use codesmith_core::watch::{
    BoxFuture, Change, Changed, Events, Kind, Native, Publish, Subscription, Target, Update,
    WatchInput, Watcher,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Let spawned tasks run (the paused clock advances only when all are idle).
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(1)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// Poll `f` until it holds, for up to `secs` seconds.
async fn until(secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

fn project() -> PathBuf {
    PathBuf::from("/project")
}

fn file(name: &str) -> PathBuf {
    project().join(name)
}

fn test_file_watch() -> (FileWatch, Arc<TestNative>, Events) {
    let native = TestNative::new();
    let events = Events::new();
    let fw = FileWatch::new(Watcher::new(native.clone()), events.clone());
    (fw, native, events)
}

/// Counts native subscribes and unsubscribes.
#[derive(Default)]
struct Counting {
    subscribes: AtomicUsize,
    unsubscribes: Arc<AtomicUsize>,
}

struct CountedSub(Arc<AtomicUsize>);

impl Subscription for CountedSub {}

impl Drop for CountedSub {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Native for Counting {
    fn subscribe(&self, _: Target, _: Publish) -> BoxFuture<Option<Box<dyn Subscription>>> {
        self.subscribes.fetch_add(1, Ordering::SeqCst);
        let un = self.unsubscribes.clone();
        Box::pin(async move { Some(Box::new(CountedSub(un)) as Box<dyn Subscription>) })
    }
}

// ---- file-watch.test.ts ------------------------------------------------

/// "shares one watch per file and resubscribes after unwatch"
#[tokio::test(start_paused = true)]
async fn oc_file_watch_shares_one_watch_per_file_and_resubscribes_after_unwatch() {
    let (fw, native, _) = test_file_watch();
    fw.watch(&project(), &file("a.ts")).unwrap();
    fw.watch(&project(), &file("a.ts")).unwrap();
    settle().await;
    assert_eq!(native.subscriptions().len(), 1);
    fw.unwatch(&project(), &file("a.ts"));
    fw.watch(&project(), &file("a.ts")).unwrap();
    settle().await;
    assert_eq!(native.subscriptions().len(), 2);
}

/// "expires a watch after 24 hours unless it is watched again"
#[tokio::test(start_paused = true)]
async fn oc_file_watch_expires_a_watch_after_24_hours_unless_watched_again() {
    let (fw, native, _) = test_file_watch();
    fw.watch(&project(), &file("a.ts")).unwrap();
    fw.watch(&project(), &file("b.ts")).unwrap();
    settle().await;
    tokio::time::advance(Duration::from_secs(23 * 3600)).await;
    settle().await;
    fw.watch(&project(), &file("a.ts")).unwrap();
    tokio::time::advance(Duration::from_secs(2 * 3600)).await;
    settle().await;
    assert_eq!(native.subscriptions().len(), 2);
    fw.watch(&project(), &file("a.ts")).unwrap();
    settle().await;
    assert_eq!(native.subscriptions().len(), 2);
    fw.watch(&project(), &file("b.ts")).unwrap();
    settle().await;
    assert_eq!(native.subscriptions().len(), 3);
}

/// "evicts the watch closest to expiring when a location is full"
#[tokio::test(start_paused = true)]
async fn oc_file_watch_evicts_the_watch_closest_to_expiring_when_a_location_is_full() {
    let (fw, native, _) = test_file_watch();
    fw.watch(&project(), &file("oldest.ts")).unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    for i in 0..file_watch::MAX_PER_LOCATION - 1 {
        fw.watch(&project(), &file(&format!("{i}.ts"))).unwrap();
    }
    fw.watch(Path::new("/other"), Path::new("/other/a.ts"))
        .unwrap();
    settle().await;
    let n = file_watch::MAX_PER_LOCATION;
    assert_eq!(native.subscriptions().len(), n + 1);
    fw.watch(&project(), &file("newest.ts")).unwrap();
    fw.watch(&project(), &file("0.ts")).unwrap();
    settle().await;
    assert_eq!(native.subscriptions().len(), n + 2);
    fw.watch(&project(), &file("oldest.ts")).unwrap();
    settle().await;
    assert_eq!(native.subscriptions().len(), n + 3);
}

/// "publishes filesystem.changed on the watch's location"
#[tokio::test]
async fn oc_file_watch_publishes_filesystem_changed_on_the_watchs_location() {
    let (fw, native, events) = test_file_watch();
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("a.ts");
    std::fs::write(&target, "a").unwrap();
    let mut rx = events.subscribe();
    fw.watch(tmp.path(), &target).unwrap();
    until(5, || native.active() == 1).await;
    native.emit(Update::new(&target, Kind::Update));
    let first = rx.recv().await.unwrap();
    std::fs::remove_file(&target).unwrap();
    native.emit(Update::new(&target, Kind::Update));
    let second = rx.recv().await.unwrap();
    let want = |event| Changed {
        location: tmp.path().into(),
        file: target.clone(),
        event,
    };
    assert_eq!(
        vec![first, second],
        vec![want(Change::Change), want(Change::Unlink)]
    );
}

/// A native watch that cannot start, like one on a directory that is gone.
#[derive(Default)]
struct Ended(AtomicUsize);

impl Native for Ended {
    fn subscribe(&self, _: Target, _: Publish) -> BoxFuture<Option<Box<dyn Subscription>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { None })
    }
}

/// "starts a new watch when the previous one ended"
#[tokio::test]
async fn oc_file_watch_starts_a_new_watch_when_the_previous_one_ended() {
    let native = Arc::new(Ended::default());
    let fw = FileWatch::new(Watcher::new(native.clone()), Events::new());
    fw.watch(&project(), &file("a.ts")).unwrap();
    until(5, || {
        native.0.load(Ordering::SeqCst) == 1 && fw.watched(&project()).is_empty()
    })
    .await;
    fw.watch(&project(), &file("a.ts")).unwrap();
    until(5, || native.0.load(Ordering::SeqCst) == 2).await;
}

/// A native watch that fails outright.
#[derive(Default)]
struct Failing(AtomicUsize);

impl Native for Failing {
    fn subscribe(&self, _: Target, _: Publish) -> BoxFuture<Option<Box<dyn Subscription>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { panic!("watch failed") })
    }
}

/// "starts a new watch when the previous one failed"
#[tokio::test]
async fn oc_file_watch_starts_a_new_watch_when_the_previous_one_failed() {
    let native = Arc::new(Failing::default());
    let fw = FileWatch::new(Watcher::new(native.clone()), Events::new());
    fw.watch(&project(), &file("a.ts")).unwrap();
    until(5, || {
        native.0.load(Ordering::SeqCst) == 1 && fw.watched(&project()).is_empty()
    })
    .await;
    fw.watch(&project(), &file("a.ts")).unwrap();
    until(5, || native.0.load(Ordering::SeqCst) == 2).await;
}

/// "fails watch requests" (FileWatch disabled)
#[tokio::test]
async fn oc_file_watch_disabled_fails_watch_requests() {
    let fw = FileWatch::disabled();
    let err = fw.watch(&project(), &file("a.ts")).unwrap_err();
    assert_eq!(err, file_watch::Disabled);
    assert_eq!(err.to_string(), "File watching is disabled");
}

/// "FileWatch native": reports in-place edits and atomic temp + rename saves.
#[tokio::test]
async fn oc_file_watch_native_reports_in_place_edits_and_atomic_saves() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("a.ts");
    std::fs::write(&target, "start").unwrap();
    let events = Events::new();
    let fw = FileWatch::new(Watcher::new(Arc::new(Notify::new())), events.clone());
    fw.watch(tmp.path(), &target).unwrap();

    async fn changed(events: &Events, target: &Path, write: impl Fn()) {
        let mut rx = events.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        // Native watches arm asynchronously, so repeat the write until one is seen.
        loop {
            write();
            let got = tokio::time::timeout(Duration::from_millis(50), async {
                loop {
                    match rx.recv().await {
                        Ok(e) if e.file == target && e.event == Change::Change => return,
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
            })
            .await;
            if got.is_ok() {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "no change event");
        }
    }

    changed(&events, &target, || {
        std::fs::write(&target, format!("inplace-{}", rand_suffix())).unwrap()
    })
    .await;
    changed(&events, &target, || {
        let temp = tmp.path().join(format!(".a.ts.{}.tmp", rand_suffix()));
        std::fs::write(&temp, format!("atomic-{}", rand_suffix())).unwrap();
        std::fs::rename(&temp, &target).unwrap();
    })
    .await;
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

// ---- location-watcher-policy.test.ts ---------------------------------

/// "reads batched registrations and disposals before notifying observers"
#[test]
fn oc_policy_reads_batched_registrations_and_disposals_before_notifying_observers() {
    let policy = Policy::new();
    let observed: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let _o = {
        let (p, observed) = (policy.clone(), observed.clone());
        policy.observe(move |ignore| {
            assert_eq!(p.current(), ignore);
            observed.lock().unwrap().push(ignore.to_vec());
        })
    };
    let kept = policy.batch(|| {
        let kept = policy.transform(|e| e.add(["node_modules"]));
        assert_eq!(policy.current(), ["node_modules"]);
        let overlay = policy.transform(|e| e.add([".git"]));
        assert_eq!(policy.current(), ["node_modules", ".git"]);
        assert!(observed.lock().unwrap().is_empty());
        overlay.dispose();
        assert_eq!(policy.current(), ["node_modules"]);
        assert!(observed.lock().unwrap().is_empty());
        kept
    });
    assert_eq!(
        *observed.lock().unwrap(),
        vec![vec!["node_modules".to_string()]]
    );
    drop(kept);
}

/// "passes the latest policy to later observers after a reentrant registration"
#[test]
fn oc_policy_passes_the_latest_policy_to_later_observers_after_a_reentrant_registration() {
    let policy = Policy::new();
    let observed: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let held: Arc<Mutex<Vec<Registration>>> = Arc::default();
    let _first = {
        let (p, held) = (policy.clone(), held.clone());
        let reentered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        policy.observe(move |_| {
            if reentered.swap(true, Ordering::SeqCst) {
                return;
            }
            let r = p.transform(|e| e.add([".git"]));
            held.lock().unwrap().push(r);
            assert_eq!(p.current(), ["node_modules", ".git"]);
        })
    };
    let _second = {
        let (p, observed) = (policy.clone(), observed.clone());
        policy.observe(move |ignore| {
            assert_eq!(p.current(), ignore);
            observed.lock().unwrap().push(ignore.to_vec());
        })
    };
    let _r = policy.transform(|e| e.add(["node_modules"]));
    assert_eq!(policy.current(), ["node_modules", ".git"]);
    let both = vec!["node_modules".to_string(), ".git".to_string()];
    assert_eq!(*observed.lock().unwrap(), vec![both.clone(), both]);
    held.lock().unwrap().clear();
}

// ---- watcher.test.ts: Watcher lifecycle -------------------------------

fn logging_native() -> (Arc<Notify>, Arc<Mutex<Vec<String>>>) {
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = {
        let errors = errors.clone();
        Arc::new(move |m: &str| errors.lock().unwrap().push(m.to_string()))
    };
    (Arc::new(Notify::with(Mode::platform(), log)), errors)
}

/// "ends the stream without logging an error when the watched directory is missing"
#[tokio::test]
async fn oc_watcher_ends_the_stream_without_logging_an_error_when_the_directory_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let (native, errors) = logging_native();
    let watcher = Watcher::new(native);
    let mut updates = watcher.subscribe(WatchInput::File(tmp.path().join("missing/a.ts")), None);
    assert_eq!(updates.recv().await, None);
    assert!(errors.lock().unwrap().is_empty());
}

/// "still logs an error when watching fails for a reason other than ENOENT"
#[tokio::test]
async fn oc_watcher_still_logs_an_error_when_watching_fails_for_another_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let (native, errors) = logging_native();
    let watcher = Watcher::new(native);
    let mut updates = watcher.subscribe(WatchInput::File(tmp.path().join("bad\0dir/a.ts")), None);
    assert_eq!(updates.recv().await, None);
    assert_eq!(errors.lock().unwrap().len(), 1);
}

/// Hands its publish out, then waits for `acquired` before resolving.
struct Gated {
    publish: Arc<Mutex<Option<Publish>>>,
    published: Arc<tokio::sync::Notify>,
    acquired: Arc<tokio::sync::Semaphore>,
    closed: Arc<AtomicUsize>,
}

impl Native for Gated {
    fn subscribe(&self, _: Target, publish: Publish) -> BoxFuture<Option<Box<dyn Subscription>>> {
        *self.publish.lock().unwrap() = Some(publish);
        self.published.notify_one();
        let (acquired, closed) = (self.acquired.clone(), self.closed.clone());
        Box::pin(async move {
            let _ = acquired.acquire().await.unwrap();
            Some(Box::new(CountedSub(closed)) as Box<dyn Subscription>)
        })
    }
}

/// "signals readiness after acquisition and buffers updates published by the ready callback"
#[tokio::test]
async fn oc_watcher_signals_readiness_after_acquisition_and_buffers_updates_published_by_the_ready_callback(
) {
    let gated = Arc::new(Gated {
        publish: Arc::default(),
        published: Arc::default(),
        acquired: Arc::new(tokio::sync::Semaphore::new(0)),
        closed: Arc::default(),
    });
    let ready = Arc::new(AtomicUsize::new(0));
    let watcher = Watcher::new(gated.clone());
    let on_ready: BoxFuture<()> = {
        let (ready, publish) = (ready.clone(), gated.publish.clone());
        Box::pin(async move {
            ready.fetch_add(1, Ordering::SeqCst);
            let p = publish.lock().unwrap().clone().unwrap();
            p.send(Update::new("/shared/opencode.json", Kind::Create));
        })
    };
    let mut updates = watcher.subscribe(
        WatchInput::Entries {
            path: "/shared".into(),
            names: vec!["opencode.json".into()],
        },
        Some(on_ready),
    );
    let consumer = tokio::spawn(async move { updates.recv().await });
    gated.published.notified().await;
    assert_eq!(ready.load(Ordering::SeqCst), 0);
    gated.acquired.add_permits(1);
    assert_eq!(
        consumer.await.unwrap(),
        Some(Update::new("/shared/opencode.json", Kind::Create))
    );
    assert_eq!(ready.load(Ordering::SeqCst), 1);
    assert_eq!(gated.closed.load(Ordering::SeqCst), 1);
}

/// "does not signal readiness for an unavailable native watch"
#[tokio::test]
async fn oc_watcher_does_not_signal_readiness_for_an_unavailable_native_watch() {
    let ready = Arc::new(AtomicUsize::new(0));
    let watcher = Watcher::new(Arc::new(Ended::default()));
    let r = ready.clone();
    let mut updates = watcher.subscribe(
        WatchInput::Directory {
            path: "/unavailable".into(),
            ignore: vec![],
        },
        Some(Box::pin(async move {
            r.fetch_add(1, Ordering::SeqCst);
        })),
    );
    while updates.recv().await.is_some() {}
    assert_eq!(ready.load(Ordering::SeqCst), 0);
}

/// "watches only named immediate entries, including missing directories"
#[tokio::test]
async fn oc_watcher_watches_only_named_immediate_entries_including_missing_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let native = Notify::new();
    let events: Arc<Mutex<Vec<Update>>> = Arc::default();
    let publish = {
        let events = events.clone();
        Publish::from_fn(move |u| events.lock().unwrap().push(u))
    };
    let target = Target::new(&WatchInput::Entries {
        path: dir.clone(),
        names: vec!["opencode.json".into(), ".opencode".into()],
    });
    let sub = native.subscribe(target, publish).await.unwrap();
    std::fs::create_dir(dir.join("nested")).unwrap();
    std::fs::write(dir.join("nested/opencode.json"), "ignored").unwrap();
    std::fs::write(dir.join("opencode.jsonc"), "ignored").unwrap();
    std::fs::write(dir.join("opencode.json"), "first").unwrap();
    until(5, || !events.lock().unwrap().is_empty()).await;
    assert!(events
        .lock()
        .unwrap()
        .iter()
        .all(|u| u.path == dir.join("opencode.json")));
    tokio::time::sleep(Duration::from_millis(10)).await;
    std::fs::create_dir(dir.join(".opencode")).unwrap();
    until(5, || {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|u| u.path == dir.join(".opencode"))
    })
    .await;
    drop(sub);
    assert_eq!(native.counts(), (0, 0));
}

/// Never resolves; says when it started and when it was dropped.
struct Pending {
    started: Arc<tokio::sync::Notify>,
    interrupted: Arc<tokio::sync::Notify>,
}

struct OnDrop(Arc<tokio::sync::Notify>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl Native for Pending {
    fn subscribe(&self, _: Target, _: Publish) -> BoxFuture<Option<Box<dyn Subscription>>> {
        let (started, guard) = (self.started.clone(), OnDrop(self.interrupted.clone()));
        Box::pin(async move {
            let _guard = guard;
            started.notify_one();
            std::future::pending::<()>().await;
            None
        })
    }
}

/// "interrupting a consumer interrupts a pending acquisition"
#[tokio::test]
async fn oc_watcher_interrupting_a_consumer_interrupts_a_pending_acquisition() {
    let native = Arc::new(Pending {
        started: Arc::default(),
        interrupted: Arc::default(),
    });
    let ready = Arc::new(AtomicUsize::new(0));
    let watcher = Watcher::new(native.clone());
    let r = ready.clone();
    let mut updates = watcher.subscribe(
        WatchInput::Directory {
            path: "/pending".into(),
            ignore: vec![],
        },
        Some(Box::pin(async move {
            r.fetch_add(1, Ordering::SeqCst);
        })),
    );
    let consumer = tokio::spawn(async move { while updates.recv().await.is_some() {} });
    native.started.notified().await;
    consumer.abort();
    let _ = consumer.await;
    tokio::time::timeout(Duration::from_secs(5), native.interrupted.notified())
        .await
        .expect("the acquisition was not interrupted");
    assert_eq!(ready.load(Ordering::SeqCst), 0);
    assert_eq!(watcher.active(), 0);
}

/// "shares equivalent entry sets and releases exactly once after the final consumer"
#[tokio::test]
async fn oc_watcher_shares_equivalent_entry_sets_and_releases_exactly_once_after_the_final_consumer(
) {
    let native = Arc::new(Counting::default());
    let watcher = Watcher::new(native.clone());
    let consume = |names: &[&str]| {
        let mut u = watcher.subscribe(
            WatchInput::Entries {
                path: "/shared".into(),
                names: names.iter().map(|s| s.to_string()).collect(),
            },
            None,
        );
        tokio::spawn(async move { while u.recv().await.is_some() {} })
    };
    let first = consume(&["opencode.json", ".opencode", "opencode.json"]);
    let second = consume(&[".opencode", "opencode.json"]);
    settle().await;
    assert_eq!(native.subscribes.load(Ordering::SeqCst), 1);

    first.abort();
    let _ = first.await;
    assert_eq!(native.unsubscribes.load(Ordering::SeqCst), 0);

    second.abort();
    let _ = second.await;
    assert_eq!(native.subscribes.load(Ordering::SeqCst), 1);
    assert_eq!(native.unsubscribes.load(Ordering::SeqCst), 1);
}

/// "scope shutdown releases an active subscription exactly once"
#[tokio::test]
async fn oc_watcher_shutdown_releases_an_active_subscription_exactly_once() {
    let native = Arc::new(Counting::default());
    let watcher = Watcher::new(native.clone());
    let mut updates = watcher.subscribe(
        WatchInput::Directory {
            path: "/active".into(),
            ignore: vec![],
        },
        None,
    );
    let consumer = tokio::spawn(async move { while updates.recv().await.is_some() {} });
    settle().await;
    assert_eq!(native.subscribes.load(Ordering::SeqCst), 1);
    assert_eq!(native.unsubscribes.load(Ordering::SeqCst), 0);
    // Shutdown tears the native subscription down while the consumer still
    // holds it; the consumer's own release as its stream ends must not do
    // it a second time.
    watcher.shutdown();
    consumer.await.unwrap();
    assert_eq!(native.unsubscribes.load(Ordering::SeqCst), 1);
}

// ---- watcher.test.ts: LocationWatcher subscriptions --------------------

fn git_init(dir: &Path) {
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap()
        .success();
    assert!(ok);
}

struct Located {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    native: Arc<TestNative>,
    watcher: Watcher,
    policy: Policy,
    events: Events,
}

fn located(vcs: &str) -> Located {
    let tmp = tempfile::tempdir().unwrap();
    let dir = real(tmp.path());
    match vcs {
        "git" => git_init(&dir),
        "hg" => std::fs::create_dir(dir.join(".hg")).unwrap(),
        _ => {}
    }
    let native = TestNative::new();
    Located {
        _tmp: tmp,
        dir,
        watcher: Watcher::new(native.clone()),
        native,
        policy: Policy::new(),
        events: Events::new(),
    }
}

impl Located {
    fn vcs(&self) -> Option<Vcs> {
        Vcs::detect(&self.dir)
    }

    fn start(&self, activation: Option<BoxFuture<()>>) -> LocationWatcher {
        self.start_with(location::discover(&self.dir, self.vcs()), activation)
    }

    fn start_with(
        &self,
        discover: location::Discover,
        activation: Option<BoxFuture<()>>,
    ) -> LocationWatcher {
        LocationWatcher::start(
            &self.dir,
            self.watcher.clone(),
            self.policy.clone(),
            self.events.clone(),
            discover,
            activation,
        )
    }
}

/// "watches only exact Git branch metadata"
#[tokio::test]
async fn oc_location_watcher_watches_only_exact_git_branch_metadata() {
    let l = located("git");
    let _w = l.start(None);
    until(5, || !l.native.subscriptions().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        l.native.subscriptions(),
        vec![WatchInput::File(l.dir.join(".git/HEAD"))]
    );
}

/// "watches only exact Hg branch metadata"
#[tokio::test]
async fn oc_location_watcher_watches_only_exact_hg_branch_metadata() {
    let l = located("hg");
    let _w = l.start(None);
    until(5, || !l.native.subscriptions().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        l.native.subscriptions(),
        vec![WatchInput::File(l.dir.join(".hg/branch"))]
    );
}

/// "reconciles config without duplicate subscriptions": config's
/// `watcher.ignore` read through a transform, replayed when config changes.
#[tokio::test]
async fn oc_location_watcher_reconciles_config_without_duplicate_subscriptions() {
    let l = located("git");
    let entries: Arc<Mutex<Vec<String>>> = Arc::default();
    let w = l.start(None);
    until(5, || l.native.subscriptions().len() == 1).await;
    assert_eq!(l.native.active(), 1);

    *entries.lock().unwrap() = vec![".git".into()];
    let config = {
        let entries = entries.clone();
        l.policy
            .transform(move |e| e.add(entries.lock().unwrap().iter()))
    };
    until(5, || l.native.active() == 0).await;
    assert_eq!(l.native.subscriptions().len(), 1);

    entries.lock().unwrap().clear();
    l.policy.reload();
    until(5, || l.native.subscriptions().len() == 2).await;
    assert_eq!(l.native.active(), 1);

    l.policy.reload();
    settle().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(l.native.subscriptions().len(), 2);

    w.stop().await;
    drop(w);
    until(5, || l.native.active() == 0).await;
    // Released twice in all: once for the ignore, once at the end.
    assert_eq!(l.native.subscriptions().len(), 2);
    drop(config);
}

/// "uses the policy changed while target discovery was suspended"
#[tokio::test]
async fn oc_location_watcher_uses_the_policy_changed_while_target_discovery_was_suspended() {
    let l = located("hg");
    let discovering = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let discover: location::Discover = {
        let (dir, vcs) = (l.dir.clone(), l.vcs().unwrap());
        let (discovering, release) = (discovering.clone(), release.clone());
        Arc::new(move || {
            let (dir, vcs, discovering, release) = (
                dir.clone(),
                vcs.clone(),
                discovering.clone(),
                release.clone(),
            );
            Box::pin(async move {
                discovering.notify_one();
                let _ = release.acquire().await.unwrap();
                location::target(&dir, &vcs)
            })
        })
    };
    let w = l.start_with(discover, None);
    discovering.notified().await;
    let registration = l.policy.transform(|e| e.add([".hg"]));
    assert_eq!(l.policy.current(), [".hg"]);
    release.add_permits(1);
    settle().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(l.native.subscriptions().is_empty());

    registration.dispose();
    until(5, || l.native.active() == 1).await;
    l.policy.reload();
    settle().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        l.native.subscriptions(),
        vec![WatchInput::File(l.dir.join(".hg/branch"))]
    );
    assert_eq!(l.native.active(), 1);
    drop(w);
    until(5, || l.native.active() == 0).await;
}

/// "does not start before configured policy is ready"
#[tokio::test]
async fn oc_location_watcher_does_not_start_before_configured_policy_is_ready() {
    let l = located("git");
    let held: Arc<Mutex<Option<Registration>>> = Arc::default();
    let activation: BoxFuture<()> = {
        let (policy, held) = (l.policy.clone(), held.clone());
        Box::pin(async move {
            *held.lock().unwrap() = Some(policy.transform(|e| e.add([".git"])));
        })
    };
    let _w = l.start(Some(activation));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(l.native.subscriptions().is_empty());
}

// ---- watcher.test.ts: LocationWatcher (native) -------------------------

/// Write `file` until `events` reports `want`, as the native watch arms
/// asynchronously.
async fn eventually(events: &Events, want: impl Fn(&Changed) -> bool, write: impl Fn()) -> Changed {
    let mut rx = events.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        write();
        let got = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                match rx.recv().await {
                    Ok(e) if want(&e) => return Some(e),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return None,
                }
            }
        })
        .await;
        if let Ok(Some(e)) = got {
            return e;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a watcher update"
        );
    }
}

fn native_located(vcs: &str) -> Located {
    let mut l = located(vcs);
    l.watcher = Watcher::new(Arc::new(Notify::new()));
    l
}

/// "LocationWatcher": publishes `.git/HEAD` events (and, below, through a
/// symlinked `.git`, `.hg/branch` events, exact file targets, and a
/// missing directory target appearing).
#[tokio::test]
async fn oc_location_watcher_native_publishes_git_head_events() {
    let l = native_located("git");
    let _w = l.start(None);
    let head = l.dir.join(".git/HEAD");
    // Ready: any event for HEAD.
    eventually(
        &l.events,
        |e| e.file == head,
        || std::fs::write(&head, "ref: refs/heads/main\n").unwrap(),
    )
    .await;
    // FSEvents may coalesce a recent creation into the same event, so wait
    // for the change rather than taking the first event (adapted).
    let e = eventually(
        &l.events,
        |e| e.file == head && e.event == Change::Change,
        || std::fs::write(&head, "ref: refs/heads/feature\n").unwrap(),
    )
    .await;
    assert_eq!(
        e,
        Changed {
            location: l.dir.clone(),
            file: head,
            event: Change::Change
        }
    );
}

#[cfg(unix)]
#[tokio::test]
async fn oc_location_watcher_native_publishes_head_events_through_a_symlinked_git_directory() {
    let l = native_located("git");
    let outer = tempfile::tempdir().unwrap();
    let actual = real(outer.path()).join("actual");
    std::fs::rename(l.dir.join(".git"), &actual).unwrap();
    std::os::unix::fs::symlink(&actual, l.dir.join(".git")).unwrap();
    let _w = l.start(None);
    let head = l.dir.join(".git/HEAD");
    let e = eventually(
        &l.events,
        |e| e.file == actual.join("HEAD") && e.event == Change::Change,
        || std::fs::write(&head, "ref: refs/heads/other\n").unwrap(),
    )
    .await;
    assert_eq!(e.event, Change::Change);
}

#[tokio::test]
async fn oc_location_watcher_native_publishes_hg_branch_events() {
    let l = native_located("hg");
    let _w = l.start(None);
    let branch = l.dir.join(".hg/branch");
    let e = eventually(
        &l.events,
        |e| e.file == branch,
        || std::fs::write(&branch, "feature\n").unwrap(),
    )
    .await;
    assert_eq!(e.file, branch);
}

#[tokio::test]
async fn oc_location_watcher_native_limits_file_watches_to_the_exact_target() {
    let tmp = tempfile::tempdir().unwrap();
    let watcher = Watcher::new(Arc::new(Notify::new()));
    let target = tmp.path().join("opencode.json");
    let sibling = tmp.path().join("other.json");
    let mut updates = watcher.subscribe(WatchInput::File(target.clone()), None);
    let first = tokio::spawn(async move { updates.recv().await });
    std::fs::write(&sibling, "sibling").unwrap();
    let writer = {
        let target = target.clone();
        tokio::spawn(async move {
            loop {
                std::fs::write(&target, format!("target-{}", rand_suffix())).unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let got = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap();
    writer.abort();
    assert_eq!(got.unwrap().path, target);
}

#[tokio::test]
async fn oc_location_watcher_native_detects_creation_of_a_missing_directory_target() {
    let tmp = tempfile::tempdir().unwrap();
    let watcher = Watcher::new(Arc::new(Notify::new()));
    let target = tmp.path().join("generated");
    let mut updates = watcher.subscribe(WatchInput::File(target.clone()), None);
    let first = tokio::spawn(async move { updates.recv().await });
    let creator = {
        let target = target.clone();
        tokio::spawn(async move {
            loop {
                let _ = std::fs::remove_dir_all(&target);
                let _ = std::fs::create_dir_all(&target);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let got = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap();
    creator.abort();
    assert_eq!(got.unwrap().path, target);
}

// ---- CodeSmith: directory trees ---------------------------------------

/// A tree subscription leaves out ignored directories, gitignored paths
/// and the target's ignore list, in both tree modes, and picks up
/// directories created after it started.
#[tokio::test]
async fn watch_tree_respects_ignores_and_follows_new_directories() {
    for mode in [Mode::Recursive, Mode::PerDirectory] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
        for d in ["src", "build", "node_modules/pkg", "dist", "a/__pycache__"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let native = Arc::new(Notify::with(mode, codesmith_core::watch::default_log()));
        let watcher = Watcher::new(native.clone());
        let mut updates = watcher.subscribe(
            WatchInput::Directory {
                path: root.clone(),
                ignore: vec!["dist".into(), "**/*.tmp".into()],
            },
            None,
        );
        let seen: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let consumer = {
            let seen = seen.clone();
            tokio::spawn(async move {
                while let Some(u) = updates.recv().await {
                    seen.lock().unwrap().push(u.path);
                }
            })
        };
        // Arm: write until the first event shows up.
        let probe = root.join("src/probe.rs");
        until(5, || {
            std::fs::write(&probe, rand_suffix().to_string()).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            !seen.lock().unwrap().is_empty()
        })
        .await;
        for f in [
            "build/out.o",
            "node_modules/pkg/index.js",
            "dist/bundle.js",
            "a/__pycache__/x.pyc",
            "src/app.log",
            "src/x.tmp",
        ] {
            std::fs::write(root.join(f), "x").unwrap();
        }
        std::fs::create_dir_all(root.join("later/deeper")).unwrap();
        let late = root.join("later/deeper/new.rs");
        until(5, || {
            std::fs::write(&late, rand_suffix().to_string()).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            seen.lock().unwrap().contains(&late)
        })
        .await;
        let seen = seen.lock().unwrap().clone();
        for p in &seen {
            let rel = p.strip_prefix(&root).unwrap().to_string_lossy().to_string();
            assert!(
                rel.starts_with("src/probe") || rel.starts_with("later"),
                "{mode:?}: unexpected update for {rel}"
            );
        }
        watcher.shutdown();
        consumer.await.unwrap();
        assert_eq!(native.counts(), (0, 0), "{mode:?}");
    }
}

/// With watching turned off, streams end at once.
#[tokio::test]
async fn watch_disabled_streams_end_at_once() {
    let watcher = Watcher::disabled();
    let mut u = watcher.subscribe(WatchInput::File("/x".into()), None);
    assert_eq!(u.recv().await, None);
}
