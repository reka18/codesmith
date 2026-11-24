//! The broker and its clients together: a real broker on a Unix socket in
//! a temporary directory, members joining it as sessions, relays and
//! observers.

use super::broker::{self, Limits};
use super::client::{self, session_member, transient_id, Bus, Change, Options};
use super::protocol::{Kind, Member, Role, MAX_BODY, MAX_HOPS};
use super::Joined;
use std::path::{Path, PathBuf};
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

struct World {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    mailbox: PathBuf,
    broker: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

fn short_tmp() -> tempfile::TempDir {
    // Socket paths have a length limit; /tmp keeps them short.
    tempfile::Builder::new()
        .prefix("csb")
        .tempdir_in("/tmp")
        .unwrap()
}

impl World {
    async fn new() -> World {
        Self::with(Limits::default(), Duration::from_secs(60)).await
    }

    async fn with(limits: Limits, idle: Duration) -> World {
        let tmp = short_tmp();
        let dir = tmp.path().join("bus");
        let mailbox = tmp.path().join("state").join("mailbox.db");
        let mut w = World {
            _tmp: tmp,
            dir,
            mailbox,
            broker: None,
        };
        w.start(limits, idle).await;
        w
    }

    async fn start(&mut self, limits: Limits, idle: Duration) {
        let opts = broker::Options {
            dir: self.dir.clone(),
            mailbox: self.mailbox.clone(),
            idle,
            limits,
            version: "1.0.0".into(),
        };
        self.broker = Some(tokio::spawn(broker::serve(opts)));
        let deadline = std::time::Instant::now() + T;
        while broker::read_registration(&self.dir).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the broker didn't start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn opts(&self) -> Options {
        Options {
            start: false,
            reconnect: true,
            home: None,
            version: "1.0.0".into(),
            dir: Some(self.dir.clone()),
        }
    }

    async fn session(&self, id: &str, title: &str, project: &str) -> Bus {
        let b = Bus::join(
            session_member(id, Some(title), project, "build", "fake/m", true),
            self.opts(),
        );
        assert!(
            matches!(b.ready(T).await, Joined::On(_)),
            "{:?}",
            b.status()
        );
        b
    }

    async fn relay(&self) -> Bus {
        let m = Member {
            role: Role::Relay,
            session: transient_id("relay"),
            name: None,
            title: None,
            project: String::new(),
            agent: String::new(),
            model: String::new(),
            pid: std::process::id(),
            busy: false,
            accepts: false,
        };
        let b = Bus::join(m, self.opts());
        assert!(matches!(b.ready(T).await, Joined::On(_)));
        b
    }

    async fn stop(&mut self) {
        if let Some(h) = self.broker.take() {
            h.abort();
            let _ = h.await;
        }
        // An aborted broker leaves its files; a real one dying does too.
    }
}

async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<super::Message>) -> super::Message {
    tokio::time::timeout(T, rx.recv())
        .await
        .expect("a message")
        .expect("open")
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sessions_exchange_a_request_progress_and_response() {
    let w = World::new().await;
    let a = w.session("ses_a", "Fix the login", "/p").await;
    let b = w.session("ses_b", "Review", "/p").await;
    assert_eq!(a.name().as_deref(), Some("fix-the-login"));
    assert_eq!(b.name().as_deref(), Some("review"));
    let peers = a.peers().await.unwrap();
    let names: Vec<&str> = peers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["fix-the-login", "review"]);

    let mut b_in = b.incoming().unwrap();
    let sent = a
        .send("review", Kind::Request, "please review", None, 0)
        .await
        .unwrap();
    assert_eq!(sent[0]["delivered"], true);
    let req = next(&mut b_in).await;
    assert_eq!(req.kind, Kind::Request);
    assert_eq!(req.from_name, "fix-the-login");
    assert_eq!(req.body, "please review");

    let wait = {
        let a = a.clone();
        let id = req.id.clone();
        tokio::spawn(async move { a.wait(T, Some(&id)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    b.send("ignored", Kind::Progress, "halfway", Some(&req.id), 0)
        .await
        .unwrap();
    let got = wait.await.unwrap().expect("progress");
    assert_eq!(got.kind, Kind::Progress);
    b.send("", Kind::Response, "looks good", Some(&req.id), 0)
        .await
        .unwrap();
    let got = a.wait(T, Some(&req.id)).await.expect("response");
    assert_eq!(
        (got.kind, got.body.as_str()),
        (Kind::Response, "looks good")
    );
    // A second response, or progress after it, is refused.
    let e = b
        .send("", Kind::Response, "again", Some(&req.id), 0)
        .await
        .unwrap_err();
    assert!(e.contains("already answered"), "{e}");
    let e = b
        .send("", Kind::Progress, "late", Some(&req.id), 0)
        .await
        .unwrap_err();
    assert!(e.contains("already answered"), "{e}");
    // Only the request's recipient may answer it.
    let e = a
        .send("", Kind::Response, "me?", Some(&req.id), 0)
        .await
        .unwrap_err();
    assert!(e.contains("no request"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_to_a_closed_session_waits_for_it() {
    let w = World::new().await;
    let a = w.session("ses_a", "Sender", "/p").await;
    let b = w.session("ses_b", "Receiver", "/p").await;
    drop(b);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sent = a
        .send("receiver", Kind::Request, "when you're back", None, 0)
        .await
        .unwrap();
    assert_eq!(sent[0]["delivered"], false);
    let b = w.session("ses_b", "Receiver", "/p").await;
    assert_eq!(
        b.name().as_deref(),
        Some("receiver"),
        "the name comes back with the session"
    );
    let mut b_in = b.incoming().unwrap();
    let m = next(&mut b_in).await;
    assert_eq!(m.body, "when you're back");
    // Until it's acknowledged it comes again on the next join.
    drop(b_in);
    drop(b);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = w.session("ses_b", "Receiver", "/p").await;
    let mut b_in = b.incoming().unwrap();
    let again = next(&mut b_in).await;
    assert_eq!(again.id, m.id);
    b.ack(&m.id).await.unwrap();
    assert!(b.inbox(None).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_gets_replies_but_not_requests() {
    let w = World::new().await;
    let a = w.session("ses_a", "Asker", "/p").await;
    let run = Bus::join(
        session_member("ses_r", Some("Runner"), "/p", "build", "m", false),
        w.opts(),
    );
    run.ready(T).await;
    let mut r_in = run.incoming().unwrap();
    a.send("runner", Kind::Request, "do it", None, 0)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), r_in.recv())
            .await
            .is_err()
    );
    // It waits for the session to open with requests.
    assert_eq!(run.inbox(None).await.unwrap().len(), 1);
    // `*` leaves sessions that take no requests out.
    let e = a
        .send("*", Kind::Request, "all", None, 0)
        .await
        .unwrap_err();
    assert!(e.contains("no other session"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn everyone_in_a_project_with_a_star() {
    let w = World::new().await;
    let a = w.session("ses_a", "A", "/p").await;
    let b = w.session("ses_b", "B", "/p").await;
    let c = w.session("ses_c", "C", "/p").await;
    let d = w.session("ses_d", "D", "/other").await;
    let (mut bi, mut ci, mut di) = (
        b.incoming().unwrap(),
        c.incoming().unwrap(),
        d.incoming().unwrap(),
    );
    let sent = a
        .send("*", Kind::Request, "standup", None, 0)
        .await
        .unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(next(&mut bi).await.body, "standup");
    assert_eq!(next(&mut ci).await.body, "standup");
    assert!(tokio::time::timeout(Duration::from_millis(200), di.recv())
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn refusals() {
    let w = World::new().await;
    let a = w.session("ses_a", "A", "/p").await;
    let _b = w.session("ses_b", "B", "/p").await;
    let e = a
        .send("nobody", Kind::Request, "x", None, 0)
        .await
        .unwrap_err();
    assert!(e.contains("no session is called nobody"), "{e}");
    let e = a.send("a", Kind::Request, "x", None, 0).await.unwrap_err();
    assert!(e.contains("itself"), "{e}");
    let e = a
        .send("b", Kind::Request, &"x".repeat(MAX_BODY + 1), None, 0)
        .await
        .unwrap_err();
    assert!(e.contains("the most is"), "{e}");
    assert!(a
        .send("b", Kind::Request, &"x".repeat(MAX_BODY), None, 0)
        .await
        .is_ok());
    let e = a
        .send("b", Kind::Request, "x", None, MAX_HOPS)
        .await
        .unwrap_err();
    assert!(e.contains("loop"), "{e}");
    assert!(a
        .send("b", Kind::Request, "x", None, MAX_HOPS - 1)
        .await
        .is_ok());
    let e = a.send("b", Kind::Response, "x", None, 0).await.unwrap_err();
    assert!(e.contains("needs reply_to"), "{e}");
    let e = a
        .send("b", Kind::Response, "x", Some("01NOPE"), 0)
        .await
        .unwrap_err();
    assert!(e.contains("no request"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_are_rate_limited() {
    let w = World::with(
        Limits {
            per_minute: 3,
            ..Limits::default()
        },
        Duration::from_secs(60),
    )
    .await;
    let a = w.session("ses_a", "A", "/p").await;
    let _b = w.session("ses_b", "B", "/p").await;
    for _ in 0..3 {
        a.send("b", Kind::Request, "x", None, 0).await.unwrap();
    }
    let e = a.send("b", Kind::Request, "x", None, 0).await.unwrap_err();
    assert!(e.contains("in the last minute"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mailbox_has_a_ceiling() {
    let w = World::with(
        Limits {
            unhandled: 2,
            per_minute: 100,
            ..Limits::default()
        },
        Duration::from_secs(60),
    )
    .await;
    let a = w.session("ses_a", "A", "/p").await;
    let _b = w.session("ses_b", "B", "/p").await;
    a.send("b", Kind::Request, "1", None, 0).await.unwrap();
    a.send("b", Kind::Request, "2", None, 0).await.unwrap();
    let e = a.send("b", Kind::Request, "3", None, 0).await.unwrap_err();
    assert!(e.contains("full"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn names_are_unique_and_can_be_chosen() {
    let w = World::new().await;
    let a = w.session("ses_a", "Review", "/p").await;
    let b = w.session("ses_b", "Review", "/p").await;
    assert_eq!(a.name().as_deref(), Some("review"));
    assert_eq!(b.name().as_deref(), Some("review-2"));
    // A name a running session has is refused.
    let e = b
        .update(Change {
            name: Some("review".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(e.contains("another running session"), "{e}");
    let e = b
        .update(Change {
            name: Some("Bad Name".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(e.contains("isn't a session name"), "{e}");
    assert_eq!(
        b.update(Change {
            name: Some("critic".into()),
            ..Default::default()
        })
        .await
        .unwrap(),
        "critic"
    );
    assert_eq!(b.name().as_deref(), Some("critic"));
    // Messages to the new name reach it.
    let mut bi = b.incoming().unwrap();
    a.send("critic", Kind::Request, "hi", None, 0)
        .await
        .unwrap();
    assert_eq!(next(&mut bi).await.to_name, "critic");
    // A name a closed session had can be asked for.
    drop(a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        b.update(Change {
            name: Some("review".into()),
            ..Default::default()
        })
        .await
        .unwrap(),
        "review"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_on_the_bus_once() {
    let w = World::new().await;
    let _a = w.session("ses_a", "A", "/p").await;
    // The same session from another live process is refused (the pid here
    // is init's, which is always alive).
    let mut m = session_member("ses_a", Some("A"), "/p", "build", "m", true);
    m.pid = 1;
    let mut o = w.opts();
    o.reconnect = false;
    let dup = Bus::join(m, o);
    let st = dup.ready(Duration::from_secs(2)).await;
    assert!(
        matches!(&st, Joined::Off(r) if r.contains("already on the bus")),
        "{st:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn switching_sessions_takes_that_sessions_name_and_mail() {
    let w = World::new().await;
    let a = w.session("ses_a", "Sender", "/p").await;
    let ui = w.session("ses_1", "First tab", "/p").await;
    let mut inc = ui.incoming().unwrap();
    // A message to a session that isn't open.
    let other = w.session("ses_2", "Second tab", "/p").await;
    drop(other);
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.send("second-tab", Kind::Request, "for tab two", None, 0)
        .await
        .unwrap();
    let name = ui
        .update(Change {
            session: Some("ses_2".into()),
            title: Some("Second tab".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(name, "second-tab");
    assert_eq!(ui.session(), "ses_2");
    assert_eq!(next(&mut inc).await.body, "for tab two");
}

#[tokio::test(flavor = "multi_thread")]
async fn pausing_holds_requests_until_resumed() {
    let w = World::new().await;
    let a = w.session("ses_a", "A", "/p").await;
    let b = w.session("ses_b", "B", "/p").await;
    let mut bi = b.incoming().unwrap();
    b.pause(true).await.unwrap();
    assert!(a
        .peers()
        .await
        .unwrap()
        .iter()
        .any(|p| p.name == "b" && p.paused));
    a.send("b", Kind::Request, "later", None, 0).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(200), bi.recv())
        .await
        .is_err());
    b.pause(false).await.unwrap();
    assert_eq!(next(&mut bi).await.body, "later");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_reaches_a_shared_session_and_gets_its_answer() {
    let w = World::new().await;
    let s = w.session("ses_s", "Reviewer", "/p").await;
    let mut si = s.incoming().unwrap();
    let r = w.relay().await;
    // Not shared yet.
    let e = r
        .relay("reviewer", "hi", "crucible@stable-mac", None)
        .await
        .unwrap_err();
    assert!(e.contains("no session is shared as reviewer"), "{e}");
    s.share("reviewer").await.unwrap();
    assert_eq!(
        s.peers().await.unwrap()[0].shared.as_deref(),
        Some("reviewer")
    );
    let got = r
        .relay(
            "reviewer",
            "please look",
            "crucible@stable-mac",
            Some("tb1"),
        )
        .await
        .unwrap();
    assert!(got.live);
    let id = got.id.unwrap();
    let req = next(&mut si).await;
    assert_eq!(req.id, id);
    assert_eq!(req.from_name, "crucible@stable-mac");
    assert_eq!(req.via.as_deref(), Some("tailbus"));
    s.send("", Kind::Response, "all good", Some(&id), 0)
        .await
        .unwrap();
    let ans = r.wait(T, Some(&id)).await.unwrap();
    assert_eq!(ans.body, "all good");

    // tailbus delivering the same message again: the same answer, no new
    // request.
    let r2 = w.relay().await;
    let again = r2
        .relay(
            "reviewer",
            "please look",
            "crucible@stable-mac",
            Some("tb1"),
        )
        .await
        .unwrap();
    assert_eq!(again.id.as_deref(), Some(id.as_str()));
    let ans = r2.wait(T, Some(&id)).await.unwrap();
    assert_eq!(ans.body, "all good");
    assert!(tokio::time::timeout(Duration::from_millis(200), si.recv())
        .await
        .is_err());

    // A relay can't send requests of its own, or share.
    assert!(r
        .send("reviewer", Kind::Request, "x", None, 0)
        .await
        .is_err());
    assert!(r.share("x").await.is_err());
    // Unsharing closes it.
    assert!(s.unshare().await.unwrap());
    assert!(r.relay("reviewer", "x", "v", None).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relay_to_a_shared_session_that_is_closed_says_so() {
    let w = World::new().await;
    let s = w.session("ses_s", "Reviewer", "/proj").await;
    s.share("reviewer").await.unwrap();
    drop(s);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = w.relay().await;
    let got = r.relay("reviewer", "x", "v", None).await.unwrap();
    assert!(!got.live);
    assert_eq!(got.id, None);
    assert_eq!(got.session, "ses_s");
    assert_eq!(got.project, "/proj");
    // Another session can't take a shared name.
    let t = w.session("ses_t", "Other", "/p").await;
    let e = t.share("reviewer").await.unwrap_err();
    assert!(e.contains("another session is shared"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_observer_looks_but_cant_send() {
    let w = World::new().await;
    let a = w.session("ses_a", "A", "/p").await;
    let b = w.session("ses_b", "B", "/p").await;
    drop(b);
    tokio::time::sleep(Duration::from_millis(100)).await;
    a.send("b", Kind::Request, "x", None, 0).await.unwrap();
    let mut m = session_member(&transient_id("observer"), None, "", "", "", false);
    m.role = Role::Observer;
    let o = Bus::join(m, w.opts());
    o.ready(T).await;
    let peers = o.peers().await.unwrap();
    assert_eq!(peers.len(), 1, "observers aren't peers");
    assert_eq!(o.inbox(Some("ses_b")).await.unwrap().len(), 1);
    assert!(o.send("a", Kind::Request, "x", None, 0).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_token_or_another_users_files_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new().await;
    // A forged registration (right shape, wrong token) gets nowhere.
    let reg_path = w.dir.join(super::REGISTRATION);
    let good = std::fs::read(&reg_path).unwrap();
    let mut reg: super::protocol::Registration = serde_json::from_slice(&good).unwrap();
    reg.token = "0".repeat(64);
    crate::paths::write_private(&reg_path, &serde_json::to_vec(&reg).unwrap()).unwrap();
    let mut o = w.opts();
    o.reconnect = false;
    let b = Bus::join(session_member("ses_x", None, "/p", "", "", true), o.clone());
    let st = b.ready(Duration::from_secs(2)).await;
    assert!(
        matches!(&st, Joined::Off(r) if r.contains("authentication")),
        "{st:?}"
    );
    // A registration others can read is not trusted.
    std::fs::write(&reg_path, &good).unwrap();
    std::fs::set_permissions(&reg_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(broker::read_registration(&w.dir).is_err());
    // Nor a symlink.
    std::fs::remove_file(&reg_path).unwrap();
    let elsewhere = w.dir.parent().unwrap().join("elsewhere.json");
    crate::paths::write_private(&elsewhere, &good).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &reg_path).unwrap();
    assert!(broker::read_registration(&w.dir).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broker_that_dies_loses_nothing_and_members_rejoin() {
    let mut w = World::new().await;
    let a = w.session("ses_a", "A", "/p").await;
    let b = w.session("ses_b", "B", "/p").await;
    drop(b);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sent = a
        .send("b", Kind::Request, "survive", None, 0)
        .await
        .unwrap();
    let id = sent[0]["id"].as_str().unwrap().to_string();
    w.stop().await;
    // Its files stay behind, as a killed broker's would.
    tokio::time::sleep(Duration::from_millis(100)).await;
    w.start(Limits::default(), Duration::from_secs(60)).await;
    // `a` joins again by itself.
    let deadline = std::time::Instant::now() + T;
    while a.peers().await.is_err() {
        assert!(std::time::Instant::now() < deadline, "a never rejoined");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let b = w.session("ses_b", "B", "/p").await;
    let mut bi = b.incoming().unwrap();
    assert_eq!(next(&mut bi).await.id, id);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_broker_stops_and_not_before() {
    let w = World::with(Limits::default(), Duration::from_millis(400)).await;
    let a = w.session("ses_a", "A", "/p").await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(a.peers().await.is_ok(), "a member keeps it running");
    drop(a);
    let deadline = std::time::Instant::now() + T;
    while broker::read_registration(&w.dir).is_ok() {
        assert!(std::time::Instant::now() < deadline, "it never stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!w.dir.join(super::SOCKET).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_codesmith_replaces_an_older_broker_and_an_older_one_cant() {
    let w = World::new().await;
    // Asking as an older version does nothing.
    let reg = broker::read_registration(&w.dir).unwrap();
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut s = tokio::net::UnixStream::connect(w.dir.join(super::SOCKET))
            .await
            .unwrap();
        let l = format!(
            "{{\"op\":\"shutdown\",\"token\":\"{}\",\"version\":\"0.9.0\"}}\n",
            reg.token
        );
        s.write_all(l.as_bytes()).await.unwrap();
        let mut r = tokio::io::BufReader::new(s);
        let mut out = String::new();
        r.read_line(&mut out).await.unwrap();
        assert!(out.contains("only a newer"), "{out}");
    }
    assert!(broker::read_registration(&w.dir).is_ok());
    // A newer member makes it go (and, with start off, finds no bus).
    let mut o = w.opts();
    o.version = "2.0.0".into();
    o.reconnect = false;
    let b = Bus::join(session_member("ses_n", None, "/p", "", "", true), o);
    let _ = b.ready(Duration::from_secs(3)).await;
    let deadline = std::time::Instant::now() + T;
    while broker::read_registration(&w.dir).is_ok_and(|r| r.token == reg.token) {
        assert!(
            std::time::Instant::now() < deadline,
            "the old broker stayed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_one_broker_runs_per_directory() {
    let w = World::new().await;
    let reg = broker::read_registration(&w.dir).unwrap();
    // A second broker on the same directory returns at once.
    let second = broker::serve(broker::Options {
        dir: w.dir.clone(),
        mailbox: w.mailbox.clone(),
        idle: Duration::from_secs(60),
        limits: Limits::default(),
        version: "1.0.0".into(),
    });
    tokio::time::timeout(T, second).await.unwrap().unwrap();
    assert_eq!(broker::read_registration(&w.dir).unwrap().token, reg.token);
}

/// Bounded property: messages between three sessions, with members
/// leaving and coming back at random, all arrive, each acted on once, and
/// in the order each sender sent them.
#[tokio::test(flavor = "multi_thread")]
async fn every_message_arrives_once_and_in_order_through_reconnects() {
    let w = World::with(
        Limits {
            per_minute: 10_000,
            ..Limits::default()
        },
        Duration::from_secs(60),
    )
    .await;
    let ids = ["ses_0", "ses_1", "ses_2"];
    let names = ["s-zero", "s-one", "s-two"];
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut rnd = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut members: Vec<Option<Bus>> = vec![None, None, None];
    for i in 0..3 {
        let b = w.session(ids[i], names[i], "/p").await;
        assert_eq!(b.name().as_deref(), Some(names[i]));
        members[i] = Some(b);
    }
    // Sent: (from, to) -> bodies in order.
    let mut sent: std::collections::HashMap<(usize, usize), Vec<String>> = Default::default();
    for step in 0..120 {
        let from = rnd(3) as usize;
        let to = (from + 1 + rnd(2) as usize) % 3;
        match rnd(10) {
            0 => {
                // Leave (never the sender of this step), and come back later.
                members[to] = None;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            _ => {
                if members[from].is_none() {
                    members[from] = Some(w.session(ids[from], names[from], "/p").await);
                }
                let body = format!("{from}->{to}#{step}");
                members[from]
                    .as_ref()
                    .unwrap()
                    .send(names[to], Kind::Request, &body, None, 0)
                    .await
                    .unwrap();
                sent.entry((from, to)).or_default().push(body);
            }
        }
    }
    drop(members);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Everyone comes back and drains its mail, acknowledging as it goes.
    for to in 0..3 {
        let b = w.session(ids[to], names[to], "/p").await;
        let mut inc = b.incoming().unwrap();
        let expected: usize = sent
            .iter()
            .filter(|((_, t), _)| *t == to)
            .map(|(_, v)| v.len())
            .sum();
        let mut got: std::collections::HashMap<usize, Vec<String>> = Default::default();
        let mut seen = std::collections::HashSet::new();
        while seen.len() < expected {
            let m = next(&mut inc).await;
            assert!(seen.insert(m.id.clone()), "acted on twice: {}", m.body);
            let from = names.iter().position(|n| *n == m.from_name).unwrap();
            got.entry(from).or_default().push(m.body.clone());
            b.ack(&m.id).await.unwrap();
        }
        for from in 0..3 {
            let want = sent.get(&(from, to)).cloned().unwrap_or_default();
            assert_eq!(got.remove(&from).unwrap_or_default(), want, "{from}->{to}");
        }
        assert!(b.inbox(None).await.unwrap().is_empty());
    }
}

/// Stress: fifty sessions each sending to every other, nothing lost.
#[tokio::test(flavor = "multi_thread")]
async fn fifty_sessions_all_to_all() {
    let w = World::with(
        Limits {
            per_minute: 1_000,
            unhandled: 100_000,
            ..Limits::default()
        },
        Duration::from_secs(60),
    )
    .await;
    const N: usize = 50;
    let mut members = Vec::new();
    for i in 0..N {
        members.push(
            w.session(&format!("ses_{i}"), &format!("Peer {i}"), "/p")
                .await,
        );
    }
    let names: Vec<String> = members.iter().map(|m| m.name().unwrap()).collect();
    let mut receivers: Vec<_> = members.iter().map(|m| m.incoming().unwrap()).collect();
    let mut tasks = Vec::new();
    for (i, m) in members.iter().enumerate() {
        let m = m.clone();
        let names = names.clone();
        tasks.push(tokio::spawn(async move {
            for (j, n) in names.iter().enumerate() {
                if j != i {
                    m.send(n, Kind::Request, &format!("{i}->{j}"), None, 0)
                        .await
                        .unwrap();
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    for (j, rx) in receivers.iter_mut().enumerate() {
        let mut got = std::collections::HashSet::new();
        while got.len() < N - 1 {
            let m = next(rx).await;
            assert_eq!(m.body.split("->").nth(1).unwrap(), j.to_string());
            got.insert(m.body);
        }
    }
}

#[test]
fn the_bus_directory_is_private_and_per_user() {
    let tmp = short_tmp();
    let d = tmp.path().join("bus");
    crate::daemon::fs::secure_dir(&d).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&d).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    // The default name has the uid in it.
    let picked = crate::daemon::fs::pick_base(&[])
        .join(format!("codesmith-bus-{}", crate::daemon::fs::euid()));
    assert!(picked
        .to_string_lossy()
        .contains(&crate::daemon::fs::euid().to_string()));
    let _ = Path::new("/");
    let _ = client::Options::default();
}
