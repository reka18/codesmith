//! The gateway end to end, in process: loopback iroh with relays off, fake
//! device keys ([`super::phone`]), and the control socket as the dialog.
//! Waits are on frames and responses, never on sleeps.

use super::control::{self, Answer, Frame, GrantAnswer, PairRequest, Request};
use super::gateway::{self, Options, Ready};
use super::http::Response;
use super::phone::{self, Phone};
use super::settings::Relay;
use super::store::{Scope, Store};
use super::{close, PAIR_ALPN, REMOTE_ALPN};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(20);

struct Gw {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    db: PathBuf,
    ready: Ready,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Gw {
    async fn start(tweak: impl FnOnce(&mut Options)) -> Gw {
        // Under /tmp: short enough for the socket path.
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let mut o = Options::new(base.join("run"), Relay::None);
        o.key = base.join("data/remote-key");
        o.db = base.join("data/remote.db");
        o.config_dir = None;
        o.loopback = true;
        o.online_wait = Duration::ZERO;
        o.idle = Duration::from_secs(3600);
        o.code_ttl = Duration::from_secs(180);
        o.confirm_timeout = Duration::from_secs(30);
        o.machine = "test-mac".into();
        tweak(&mut o);
        let (dir, db) = (o.dir.clone(), o.db.clone());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(gateway::serve(o, Some(tx)));
        let ready = tokio::time::timeout(WAIT, rx).await.unwrap().unwrap();
        Gw {
            _tmp: tmp,
            dir,
            db,
            ready,
            task,
        }
    }

    fn id(&self) -> &str {
        &self.ready.node_id
    }

    async fn dial(&self, phone: &Phone, alpn: &[u8]) -> iroh::endpoint::Connection {
        tokio::time::timeout(WAIT, phone.connect(self.id(), &self.ready.addrs, alpn))
            .await
            .unwrap()
            .unwrap()
    }

    /// Open a pairing dialog and read its first code.
    async fn dialog(&self, session: Option<&str>) -> (control::Conn, String) {
        let mut c = control::connect(
            &self.dir,
            Request::Pair(PairRequest {
                session: session.map(String::from),
                title: Some("fix the login page".into()),
            }),
        )
        .await
        .unwrap();
        let code = match next(&mut c).await {
            Frame::Code { code, link, .. } => {
                let l = super::link::Link::parse(&link).unwrap();
                assert_eq!(l.code, code);
                assert_eq!(l.node_id, self.id());
                assert_eq!(l.session.as_deref(), session);
                assert_eq!(l.machine, "test-mac");
                code
            }
            f => panic!("{f:?}"),
        };
        (c, code)
    }

    fn store(&self) -> Store {
        Store::open(&self.db).unwrap()
    }

    async fn stop(self) {
        let _ = self.ready.stop.send(true);
        tokio::time::timeout(WAIT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

async fn next(c: &mut control::Conn) -> Frame {
    tokio::time::timeout(WAIT, c.next())
        .await
        .expect("a frame in time")
        .unwrap()
        .expect("the dialog is still open")
}

fn body(code: &str) -> Value {
    json!({"code": code, "device": {"name": "Ray's iPhone", "platform": "ios"}})
}

fn allow(id: u64, session: &str, scope: Scope) -> Answer {
    Answer {
        id,
        allow: true,
        grant: Some(GrantAnswer {
            session: session.into(),
            scope,
        }),
    }
}

/// Claim `code` from `phone` while the dialog answers its confirm with
/// `answer` (given the confirm's id); the claim's response.
async fn claim_answered(
    gw: &Gw,
    phone: &Phone,
    dialog: &mut control::Conn,
    code: &str,
    answer: impl FnOnce(u64) -> Option<Answer>,
) -> (Response, Frame) {
    let conn = gw.dial(phone, PAIR_ALPN).await;
    let b = body(code);
    let claim = tokio::spawn(async move { phone::claim(&conn, &b).await.unwrap() });
    let confirm = next(dialog).await;
    let Frame::Confirm { id, .. } = confirm.clone() else {
        panic!("{confirm:?}");
    };
    if let Some(a) = answer(id) {
        dialog.send(&a).await.unwrap();
    }
    (
        tokio::time::timeout(WAIT, claim).await.unwrap().unwrap(),
        confirm,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_right_code_and_yes_pair_the_device_with_a_grant() {
    let gw = Gw::start(|_| {}).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let p = Phone::new().await.unwrap();
    let (r, confirm) = claim_answered(&gw, &p, &mut d, &code, |id| {
        Some(allow(id, "ses_1", Scope::Control))
    })
    .await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["machine"]["id"], gw.id());
    assert_eq!(r.body["device"]["id"], p.id());
    assert_eq!(
        r.body["grants"],
        json!([{"session": "ses_1", "scope": "control"}])
    );
    let Frame::Confirm { device, check, .. } = confirm else {
        unreachable!()
    };
    assert_eq!(device.id, p.id(), "the id is the connection's key");
    assert_eq!(device.name, "Ray's iPhone");
    assert!(!device.paired);
    assert_eq!(check, super::link::check_code(&code, gw.id(), &p.id()));
    assert!(matches!(next(&mut d).await, Frame::Paired { .. }));
    let dev = gw.store().device(&p.id()).unwrap().unwrap();
    assert_eq!(dev.grants[0].scope, Scope::Control);
    // The same code again is spent: 410, even with a new code live.
    let (_d2, _) = gw.dialog(Some("ses_1")).await;
    let c = gw.dial(&p, PAIR_ALPN).await;
    let r = phone::claim(&c, &body(&code)).await.unwrap();
    assert_eq!(r.status, 410);
    assert_eq!(r.body["error"]["code"], "code_expired");
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_code_gets_410_and_leaves_the_live_one() {
    let gw = Gw::start(|_| {}).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let p = Phone::new().await.unwrap();
    let c = gw.dial(&p, PAIR_ALPN).await;
    let r = phone::claim(&c, &body("AAAAAAAAAAAAAAAAAAAAAA"))
        .await
        .unwrap();
    assert_eq!(r.status, 410);
    // The live code still works.
    let (r, _) = claim_answered(&gw, &p, &mut d, &code, |id| {
        Some(allow(id, "ses_1", Scope::Watch))
    })
    .await;
    assert_eq!(r.status, 200);
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn six_bad_claims_lock_that_key_out_without_burning_the_code() {
    let gw = Gw::start(|_| {}).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let bad = Phone::new().await.unwrap();
    let c = gw.dial(&bad, PAIR_ALPN).await;
    let mut statuses = vec![];
    for _ in 0..6 {
        statuses.push(
            phone::claim(&c, &body("wrongwrongwrongwrong00"))
                .await
                .unwrap()
                .status,
        );
    }
    assert_eq!(statuses, [410, 410, 410, 410, 410, 429]);
    // Locked: even the right code is refused, and isn't burned.
    let r = phone::claim(&c, &body(&code)).await.unwrap();
    assert_eq!(r.status, 429);
    assert_eq!(r.body["error"]["code"], "rate_limited");
    // Another key, with the live code, gets the confirmation.
    let good = Phone::new().await.unwrap();
    let (r, _) = claim_answered(&gw, &good, &mut d, &code, |id| {
        Some(allow(id, "ses_1", Scope::Control))
    })
    .await;
    assert_eq!(r.status, 200);
    let log = gw.store().log(None, 50).unwrap();
    assert!(log.iter().any(|a| a.action == "claim_locked"));
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_is_403_and_brings_a_new_code() {
    let gw = Gw::start(|_| {}).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let p = Phone::new().await.unwrap();
    let (r, _) = claim_answered(&gw, &p, &mut d, &code, |id| {
        Some(Answer {
            id,
            allow: false,
            grant: None,
        })
    })
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(r.body["error"]["code"], "pairing_refused");
    assert!(matches!(next(&mut d).await, Frame::Refused { .. }));
    let Frame::Code { code: fresh, .. } = next(&mut d).await else {
        panic!()
    };
    assert_ne!(fresh, code);
    assert!(!gw.store().is_paired(&p.id()).unwrap());
    // The refused code stays spent.
    let c = gw.dial(&p, PAIR_ALPN).await;
    assert_eq!(phone::claim(&c, &body(&code)).await.unwrap().status, 410);
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_answer_in_time_refuses() {
    let gw = Gw::start(|o| o.confirm_timeout = Duration::from_millis(300)).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let p = Phone::new().await.unwrap();
    let (r, _) = claim_answered(&gw, &p, &mut d, &code, |_| None).await;
    assert_eq!(r.status, 403);
    assert!(matches!(next(&mut d).await, Frame::Refused { reason } if reason.contains("time")));
    assert!(matches!(next(&mut d).await, Frame::Code { .. }));
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_dialog_refuses_its_claim() {
    let gw = Gw::start(|_| {}).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let p = Phone::new().await.unwrap();
    let conn = gw.dial(&p, PAIR_ALPN).await;
    let b = body(&code);
    let claim = tokio::spawn(async move { phone::claim(&conn, &b).await.unwrap() });
    assert!(matches!(next(&mut d).await, Frame::Confirm { .. }));
    drop(d);
    let r = tokio::time::timeout(WAIT, claim).await.unwrap().unwrap();
    assert_eq!(r.status, 403);
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_for_a_grant_not_offered_refuses() {
    let gw = Gw::start(|_| {}).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let p = Phone::new().await.unwrap();
    let (r, _) = claim_answered(&gw, &p, &mut d, &code, |id| {
        Some(allow(id, "ses_other", Scope::Control))
    })
    .await;
    assert_eq!(r.status, 403);
    assert!(!gw.store().is_paired(&p.id()).unwrap());
    // An answer to an id that isn't pending is ignored.
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let conn = gw.dial(&p, PAIR_ALPN).await;
    let b = body(&code);
    let claim = tokio::spawn(async move { phone::claim(&conn, &b).await.unwrap() });
    let Frame::Confirm { id, .. } = next(&mut d).await else {
        panic!()
    };
    d.send(&allow(id + 1000, "ses_1", Scope::Control))
        .await
        .unwrap();
    d.send(&Answer {
        id,
        allow: false,
        grant: None,
    })
    .await
    .unwrap();
    assert_eq!(claim.await.unwrap().status, 403);
    assert!(!gw.store().is_paired(&p.id()).unwrap());
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn all_sessions_and_a_paired_device_getting_only_the_new_grant() {
    let gw = Gw::start(|_| {}).await;
    let p = Phone::new().await.unwrap();
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let (r, _) = claim_answered(&gw, &p, &mut d, &code, |id| {
        Some(allow(id, "ses_1", Scope::Watch))
    })
    .await;
    assert_eq!(r.status, 200);
    let (mut d, code) = gw.dialog(Some("ses_2")).await;
    let (r, confirm) = claim_answered(&gw, &p, &mut d, &code, |id| {
        Some(allow(id, "*", Scope::Control))
    })
    .await;
    assert_eq!(r.status, 200);
    let Frame::Confirm { device, .. } = confirm else {
        unreachable!()
    };
    assert!(device.paired, "the prompt says (paired device)");
    let grants = gw.store().device(&p.id()).unwrap().unwrap().grants;
    let g: Vec<(&str, Scope)> = grants
        .iter()
        .map(|g| (g.session.as_str(), g.scope))
        .collect();
    assert_eq!(g, [("*", Scope::Control), ("ses_1", Scope::Watch)]);
    let log = gw.store().log(None, 50).unwrap();
    assert!(log.iter().any(|a| a.action == "grant_added"));
    gw.stop().await;
}

fn p256_spki() -> String {
    use base64::Engine;
    let mut der = vec![
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00, 0x04,
    ];
    der.extend([7u8; 64]);
    base64::engine::general_purpose::STANDARD.encode(der)
}

#[tokio::test(flavor = "multi_thread")]
async fn claims_are_checked_and_cleaned() {
    let gw = Gw::start(|_| {}).await;
    let (mut d, code) = gw.dialog(None).await;
    let p = Phone::new().await.unwrap();
    let c = gw.dial(&p, PAIR_ALPN).await;
    for bad in [
        json!({"code": code}),
        json!({"code": code, "device": {"name": "\u{7}\u{1b}", "platform": "ios"}}),
        json!({"code": code, "device": {"name": "a", "platform": "ios"},
               "unlock_key": {"alg": "RS256", "spki": p256_spki()}}),
        json!({"code": code, "device": {"name": "a", "platform": "ios"},
               "unlock_key": {"alg": "ES256", "spki": "not base64!"}}),
        json!({"code": code, "device": {"name": "a", "platform": "ios"},
               "unlock_key": {"alg": "ES256", "spki": "AAAA"}}),
        json!({"code": code, "device": {"name": "a", "platform": "ios"},
               "push": {"kind": "a b", "token": "t"}}),
        json!("nonsense"),
    ] {
        let r = phone::claim(&c, &bad).await.unwrap();
        assert_eq!(r.status, 400, "{bad}: {:?}", r.body);
        assert_eq!(r.body["error"]["code"], "bad_request");
    }
    // None of those burned the code. A long name with controls and bidi
    // characters is cleaned and cut; the unlock key and push are kept.
    let spki = p256_spki();
    let name = format!("\u{1b}[31mEvil\u{202e}{}", "x".repeat(100));
    let b = json!({"code": code, "device": {"name": name, "platform": "android"},
                   "unlock_key": {"alg": "ES256", "spki": spki},
                   "push": {"kind": "fcm", "token": "tok"}});
    let claim = tokio::spawn(async move { phone::claim(&c, &b).await.unwrap() });
    let Frame::Confirm { id, device, .. } = next(&mut d).await else {
        panic!()
    };
    assert!(!device.name.contains('\u{1b}') && !device.name.contains('\u{202e}'));
    assert_eq!(device.name.chars().count(), 64);
    assert!(device.name.starts_with("[31mEvil"));
    // No session offered: the device is paired with no grant.
    d.send(&Answer {
        id,
        allow: true,
        grant: None,
    })
    .await
    .unwrap();
    let r = claim.await.unwrap();
    assert_eq!(r.status, 200, "{:?}", r.body);
    assert_eq!(r.body["grants"], json!([]));
    let dev = gw.store().device(&p.id()).unwrap().unwrap();
    assert_eq!(dev.unlock_key.unwrap().spki, spki);
    assert_eq!(gw.store().push(&p.id()).unwrap().unwrap().token, "tok");
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_with_no_live_code_is_closed_with_code_3() {
    let gw = Gw::start(|_| {}).await;
    let p = Phone::new().await.unwrap();
    let c = gw.dial(&p, PAIR_ALPN).await;
    assert_eq!(phone::close_code(&c).await, Some(close::NO_CODE as u64));
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpaired_key_is_closed_with_code_1() {
    let gw = Gw::start(|_| {}).await;
    let p = Phone::new().await.unwrap();
    let c = gw.dial(&p, REMOTE_ALPN).await;
    assert_eq!(phone::close_code(&c).await, Some(close::UNPAIRED as u64));
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn other_alpns_are_refused() {
    let gw = Gw::start(|_| {}).await;
    let p = Phone::new().await.unwrap();
    // The handshake itself fails: TLS offers only the gateway's two ALPNs,
    // so no connection on another one is ever made to close with code 2.
    let r = tokio::time::timeout(
        WAIT,
        p.connect(gw.id(), &gw.ready.addrs, b"codesmith/other/1"),
    )
    .await
    .unwrap();
    assert!(r.is_err(), "connected on an unknown ALPN");
    // The gateway still serves.
    let c = gw.dial(&p, PAIR_ALPN).await;
    assert_eq!(phone::close_code(&c).await, Some(close::NO_CODE as u64));
    gw.stop().await;
}

/// Pair `p` with control of `session`.
async fn paired(gw: &Gw, p: &Phone, session: &str) {
    let (mut d, code) = gw.dialog(Some(session)).await;
    let (r, _) = claim_answered(gw, p, &mut d, &code, |id| {
        Some(allow(id, session, Scope::Control))
    })
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paired_device_connects_and_revoking_it_closes_the_connection() {
    let gw = Gw::start(|_| {}).await;
    let p = Phone::new().await.unwrap();
    paired(&gw, &p, "ses_1").await;
    let c = gw.dial(&p, REMOTE_ALPN).await;
    // The routes come next phase: a request answers 404 in the /v1 shape,
    // and one with an Origin header is refused.
    let r = phone::request(&c, "GET", "/v1/sessions", &[], b"")
        .await
        .unwrap();
    assert_eq!(r.status, 404);
    assert_eq!(r.body["error"]["code"], "not_found");
    let r = phone::request(
        &c,
        "GET",
        "/v1/sessions",
        &[("origin", "https://evil")],
        b"",
    )
    .await
    .unwrap();
    assert_eq!(r.status, 400);
    match control::call(
        &gw.dir,
        Request::Revoke {
            device: p.id()[..10].to_string(),
        },
    )
    .await
    .unwrap()
    {
        Frame::Ok => {}
        f => panic!("{f:?}"),
    }
    assert_eq!(
        tokio::time::timeout(WAIT, phone::close_code(&c))
            .await
            .unwrap(),
        Some(close::UNPAIRED as u64)
    );
    // And it can't come back.
    let c = gw.dial(&p, REMOTE_ALPN).await;
    assert_eq!(phone::close_code(&c).await, Some(close::UNPAIRED as u64));
    assert!(!gw.store().is_paired(&p.id()).unwrap());
    let log = gw.store().log(None, 50).unwrap();
    assert!(log.iter().any(|a| a.action == "revoked"));
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_code_is_replaced_and_refused() {
    let gw = Gw::start(|o| o.code_ttl = Duration::from_millis(400)).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let Frame::Code { code: fresh, .. } = next(&mut d).await else {
        panic!()
    };
    assert_ne!(fresh, code);
    let p = Phone::new().await.unwrap();
    let c = gw.dial(&p, PAIR_ALPN).await;
    assert_eq!(phone::claim(&c, &body(&code)).await.unwrap().status, 410);
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_dialog_replaces_the_first() {
    let gw = Gw::start(|_| {}).await;
    let (mut first, old) = gw.dialog(Some("ses_1")).await;
    let (_second, new) = gw.dialog(Some("ses_2")).await;
    assert!(matches!(next(&mut first).await, Frame::Replaced));
    assert!(tokio::time::timeout(WAIT, first.next())
        .await
        .unwrap()
        .unwrap()
        .is_none());
    let p = Phone::new().await.unwrap();
    let c = gw.dial(&p, PAIR_ALPN).await;
    assert_eq!(phone::claim(&c, &body(&old)).await.unwrap().status, 410);
    assert_ne!(old, new);
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn other_routes_and_origins_on_the_pairing_alpn() {
    let gw = Gw::start(|_| {}).await;
    let (_d, code) = gw.dialog(None).await;
    let p = Phone::new().await.unwrap();
    let c = gw.dial(&p, PAIR_ALPN).await;
    let r = phone::request(&c, "GET", "/v1/sessions", &[], b"")
        .await
        .unwrap();
    assert_eq!(r.status, 404);
    let r = phone::request(
        &c,
        "POST",
        "/v1/pair/claim",
        &[("origin", "https://x")],
        &serde_json::to_vec(&body(&code)).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(r.status, 400);
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_control_socket_needs_the_token() {
    let gw = Gw::start(|_| {}).await;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut s = tokio::net::UnixStream::connect(gw.dir.join(super::SOCKET))
        .await
        .unwrap();
    s.write_all(
        b"{\"token\":\"nope\",\"protocol\":1,\"version\":\"0\",\"request\":{\"op\":\"stop\"}}\n",
    )
    .await
    .unwrap();
    let mut line = String::new();
    tokio::io::BufReader::new(s)
        .read_line(&mut line)
        .await
        .unwrap();
    assert!(line.contains("authentication failed"), "{line}");
    // Still running.
    let st = control::status(&gw.dir).await.unwrap();
    assert_eq!(st.node_id, gw.id());
    assert_eq!(st.relay, "none");
    // A takeover by an older version is refused; a plain stop isn't.
    match control::call(
        &gw.dir,
        Request::Stop {
            version: Some("0.0.1".into()),
        },
    )
    .await
    .unwrap()
    {
        Frame::Error { .. } => {}
        f => panic!("{f:?}"),
    }
    assert!(control::status(&gw.dir).await.is_some());
    assert!(matches!(
        control::call(&gw.dir, Request::Stop { version: None })
            .await
            .unwrap(),
        Frame::Ok
    ));
    tokio::time::timeout(WAIT, gw.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(control::status(&gw.dir).await.is_none());
    assert!(!gw.dir.join(super::REGISTRATION).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_gateway_in_the_same_folder_defers_to_the_first() {
    let gw = Gw::start(|_| {}).await;
    let mut o = Options::new(gw.dir.clone(), Relay::None);
    o.config_dir = None;
    o.loopback = true;
    o.key = gw.db.with_file_name("remote-key");
    o.db = gw.db.clone();
    // Returns at once, leaving the first serving.
    tokio::time::timeout(WAIT, gateway::serve(o, None))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(control::status(&gw.dir).await.unwrap().node_id, gw.id());
    gw.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_to_do_ends_the_gateway() {
    let gw = Gw::start(|o| o.idle = Duration::from_millis(300)).await;
    tokio::time::timeout(WAIT, gw.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn turning_remote_off_stops_the_gateway() {
    let cfg = tempfile::tempdir().unwrap();
    std::fs::write(
        cfg.path().join("codesmith.json"),
        r#"{"remote": {"enabled": true}}"#,
    )
    .unwrap();
    let dir = cfg.path().to_path_buf();
    let gw = Gw::start(move |o| o.config_dir = Some(dir)).await;
    assert!(control::status(&gw.dir).await.is_some());
    std::fs::write(
        cfg.path().join("codesmith.json"),
        r#"{"remote": {"enabled": false}}"#,
    )
    .unwrap();
    tokio::time::timeout(WAIT, gw.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// Stress: many keys guessing at once never burn the live code, and the
/// gateway keeps answering.
#[tokio::test(flavor = "multi_thread")]
async fn many_guessers_at_once_dont_burn_the_code() {
    let gw = Gw::start(|_| {}).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..12 {
        let p = Phone::new().await.unwrap();
        let c = gw.dial(&p, PAIR_ALPN).await;
        tasks.spawn(async move {
            let mut out = vec![];
            for j in 0..8 {
                let guess = format!("guess{i:04}{j:04}xxxxxxxxx");
                out.push(phone::claim(&c, &body(&guess)).await.unwrap().status);
            }
            drop(p);
            out
        });
    }
    while let Some(r) = tasks.join_next().await {
        let statuses = r.unwrap();
        assert_eq!(&statuses[..5], &[410; 5]);
        assert!(statuses[5..].iter().all(|s| *s == 429), "{statuses:?}");
    }
    let p = Phone::new().await.unwrap();
    let (r, _) = claim_answered(&gw, &p, &mut d, &code, |id| {
        Some(allow(id, "ses_1", Scope::Control))
    })
    .await;
    assert_eq!(r.status, 200);
    gw.stop().await;
}

/// I11: with a tailnet (`100.64.0.0/10`) interface, its address is among
/// the gateway's direct addresses. Skipped where there is none.
#[tokio::test(flavor = "multi_thread")]
async fn a_tailnet_address_is_among_the_direct_addresses() {
    let Some(tailnet) = tailnet_address() else {
        eprintln!("skipped: no tailnet address on this machine");
        return;
    };
    let gw = Gw::start(|o| o.loopback = false).await;
    let (_d, _) = gw.dialog(None).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let st = control::status(&gw.dir).await.unwrap();
        if st
            .addrs
            .iter()
            .any(|a| a.starts_with(&format!("{tailnet}:")))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{tailnet} not in {:?}",
            st.addrs
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    gw.stop().await;
}

fn tailnet_address() -> Option<std::net::Ipv4Addr> {
    let mut out = None;
    // SAFETY: getifaddrs fills a list freed below with freeifaddrs.
    unsafe {
        let mut ifs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifs) != 0 {
            return None;
        }
        let mut p = ifs;
        while !p.is_null() {
            let a = (*p).ifa_addr;
            if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                let sin = &*(a as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let o = ip.octets();
                if o[0] == 100 && (64..128).contains(&o[1]) {
                    out = Some(ip);
                }
            }
            p = (*p).ifa_next;
        }
        libc::freeifaddrs(ifs);
    }
    out
}

/// Bound to every interface (as it runs for real), the gateway serves a
/// phone on this machine through whichever of its addresses it dials.
#[tokio::test(flavor = "multi_thread")]
async fn a_gateway_on_every_interface_serves_a_local_phone() {
    let gw = Gw::start(|o| o.loopback = false).await;
    let (mut d, code) = gw.dialog(Some("ses_1")).await;
    let p = Phone::new().await.unwrap();
    let port = gw.ready.addrs.iter().find(|a| a.is_ipv4()).unwrap().port();
    let conn = tokio::time::timeout(
        WAIT,
        p.connect(
            gw.id(),
            &[std::net::SocketAddr::from(([127, 0, 0, 1], port))],
            PAIR_ALPN,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let b = body(&code);
    let claim = tokio::spawn(async move { phone::claim(&conn, &b).await.unwrap() });
    let Frame::Confirm { id, .. } = next(&mut d).await else {
        panic!()
    };
    d.send(&allow(id, "ses_1", Scope::Control)).await.unwrap();
    assert_eq!(claim.await.unwrap().status, 200);
    gw.stop().await;
}
