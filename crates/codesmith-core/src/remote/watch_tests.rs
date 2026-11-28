//! Watching sessions end to end, in process: the gateway on loopback iroh
//! with relays off, an owner's hub on its control socket, fake device keys
//! ([`super::phone`]), and a session store on disk. Waits are on
//! responses, events and the hub's sync, never on sleeps.

use super::client::{Transcript, Verdict};
use super::control::{self, Request};
use super::gateway::{self, Options, Ready};
use super::http::Response;
use super::phone::{self, Events, Phone, Sse};
use super::settings::Relay;
use super::store::{Scope, Store};
use super::tap::{kind, Hub, Info, Limits, Origin, Tap};
use super::REMOTE_ALPN;
use codesmith_ai::{Message, Part, Role};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(20);

struct W {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    db: PathBuf,
    sessions: PathBuf,
    ready: Ready,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    hub: Hub,
}

fn instant() -> Limits {
    Limits {
        merge_window: Duration::ZERO,
        ..Default::default()
    }
}

impl W {
    async fn start(limits: Limits) -> W {
        W::start_with(limits, |_| {}).await
    }

    async fn start_with(limits: Limits, tweak: impl FnOnce(&mut Options)) -> W {
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let mut o = Options::new(base.join("run"), Relay::None);
        o.key = base.join("data/remote-key");
        o.db = base.join("data/remote.db");
        o.sessions_db = Some(base.join("data/codesmith.db"));
        o.config_dir = None;
        o.loopback = true;
        o.online_wait = Duration::ZERO;
        o.idle = Duration::from_secs(3600);
        o.machine = "test-mac".into();
        tweak(&mut o);
        let (dir, db, sessions) = (o.dir.clone(), o.db.clone(), o.sessions_db.clone().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(gateway::serve(o, Some(tx)));
        let ready = tokio::time::timeout(WAIT, rx).await.unwrap().unwrap();
        let hub = Hub::connect_to(kind::TUI, dir.clone(), limits, Duration::from_millis(20));
        W {
            _tmp: tmp,
            dir,
            db,
            sessions,
            ready,
            task,
            hub,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.db).unwrap()
    }

    fn sessions(&self) -> crate::store::Store {
        crate::store::Store::open(&self.sessions).unwrap()
    }

    /// A session in the store, titled after its id.
    fn stored(&self, id: &str) {
        let now = crate::store::now();
        self.sessions()
            .create(&crate::store::Info {
                id: id.into(),
                directory: "/p".into(),
                title: Some(format!("title of {id}")),
                agent: "build".into(),
                model: "m/x".into(),
                parent_id: None,
                created: now,
                updated: now,
                usage: Default::default(),
            })
            .unwrap();
    }

    fn append(&self, id: &str, m: Message) -> i64 {
        self.sessions()
            .append(id, &crate::store::Record::Message { message: m })
            .unwrap()
    }

    /// The hub's tap for stored session `id`, registered with the gateway.
    async fn tap(&self, id: &str) -> Tap {
        let t = self.hub.tap_for(
            id,
            Info {
                title: Some(format!("title of {id}")),
                agent: "build".into(),
                model: "m/x".into(),
                directory: "/p".into(),
                effort: None,
            },
            self.sessions().last_seq(id).unwrap(),
        );
        assert!(self.hub.sync().await, "the hub connected");
        t
    }

    /// A paired phone with these grants.
    async fn phone(&self, grants: &[(&str, Scope)]) -> (Phone, iroh::endpoint::Connection) {
        let p = Phone::new().await.unwrap();
        let s = self.store();
        s.pair(&p.id(), "Phone", "ios", None, None, None).unwrap();
        for (session, scope) in grants {
            s.pair(&p.id(), "Phone", "ios", None, None, Some((session, *scope)))
                .unwrap();
        }
        let c = tokio::time::timeout(
            WAIT,
            p.connect(&self.ready.node_id, &self.ready.addrs, REMOTE_ALPN),
        )
        .await
        .unwrap()
        .unwrap();
        (p, c)
    }

    async fn stop(self) {
        drop(self.hub);
        let _ = self.ready.stop.send(true);
        tokio::time::timeout(WAIT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

async fn get(c: &iroh::endpoint::Connection, path: &str) -> Response {
    tokio::time::timeout(WAIT, phone::request(c, "GET", path, &[], b""))
        .await
        .unwrap()
        .unwrap()
}

async fn stream(c: &iroh::endpoint::Connection, path: &str, cursor: Option<&str>) -> Events {
    tokio::time::timeout(WAIT, phone::events(c, path, cursor))
        .await
        .unwrap()
        .unwrap()
}

async fn next(e: &mut Events) -> (Option<String>, Value) {
    tokio::time::timeout(WAIT, e.event())
        .await
        .expect("an event in time")
        .expect("the stream is open")
}

/// Read `e` into `t` until it has applied `last`.
async fn follow(e: &mut Events, t: &mut Transcript, last: u64) {
    while t.cursor.as_ref().is_none_or(|c| c.seq < last) {
        let (id, data) = next(e).await;
        let v = t.deliver(id.as_deref(), &data);
        assert!(!matches!(v, Verdict::Reset(_)), "{v:?} at {data}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_machine_and_the_granted_sessions() {
    let w = W::start(instant()).await;
    for id in ["ses_a", "ses_b", "ses_c"] {
        w.stored(id);
    }
    let _a = w.tap("ses_a").await;
    let (_p, c) = w
        .phone(&[("ses_a", Scope::Control), ("ses_c", Scope::Watch)])
        .await;
    let m = get(&c, "/v1/machine").await;
    assert_eq!(m.status, 200);
    assert_eq!(m.body["id"], w.ready.node_id);
    assert_eq!(m.body["name"], "test-mac");
    assert_eq!(m.body["features"], json!(["watch"]));
    let l = get(&c, "/v1/sessions").await;
    let items = l.body.as_array().unwrap();
    let ids: Vec<&str> = items.iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["ses_a", "ses_c"], "granted only (S2)");
    assert_eq!(items[0]["owner"], "tui");
    assert_eq!(items[0]["busy"], json!(false), "a JSON boolean");
    assert_eq!(items[0]["title"], "title of ses_a");
    for k in ["id", "title", "agent", "model", "directory", "busy"] {
        assert!(items[1].get(k).is_some(), "S12: {k}");
    }
    assert_eq!(items[1]["owner"], "closed", "S3");
    assert_eq!(items[1]["scope"], "watch");
    // No grant: 404 on every route of it, never 403.
    for path in [
        "/v1/sessions/ses_b",
        "/v1/sessions/ses_b/messages",
        "/v1/sessions/nope",
    ] {
        let r = get(&c, path).await;
        assert_eq!(r.status, 404, "{path}");
        assert_eq!(r.body["error"]["code"], "not_found", "E1");
    }
    let e = stream(&c, "/v1/sessions/ses_b/events", None).await;
    assert_eq!(e.status, 404);
    // Control isn't offered yet: watch gets 403, control 404.
    let r = phone::request(&c, "POST", "/v1/sessions/ses_c/messages", &[], b"{}")
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.body["error"]["code"].as_str()),
        (403, Some("forbidden_scope"))
    );
    let r = phone::request(&c, "POST", "/v1/sessions/ses_a/messages", &[], b"{}")
        .await
        .unwrap();
    assert_eq!(r.status, 404);
    let r = phone::request(&c, "POST", "/v1/sessions/ses_b/abort", &[], b"")
        .await
        .unwrap();
    assert_eq!(r.status, 404, "ungranted: 404 even for control");
    w.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_all_sessions_grant_lists_held_and_stored_sessions() {
    let w = W::start(instant()).await;
    w.stored("ses_old");
    let _t = w.tap("ses_new").await;
    let (_p, c) = w.phone(&[("*", Scope::Watch)]).await;
    let l = get(&c, "/v1/sessions").await;
    let ids: Vec<&str> = l
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["ses_new", "ses_old"]);
    w.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_session_is_read_only_from_the_store() {
    let w = W::start(instant()).await;
    w.stored("ses_a");
    w.append("ses_a", Message::user("hello"));
    w.append(
        "ses_a",
        Message {
            role: Role::Assistant,
            parts: vec![Part::text("hi")],
        },
    );
    let (_p, c) = w.phone(&[("ses_a", Scope::Control)]).await;
    let s = get(&c, "/v1/sessions/ses_a").await;
    assert_eq!(s.status, 200, "{}", s.body);
    assert_eq!(s.body["owner"], "closed");
    assert_eq!(s.body["busy"], json!(false));
    assert_eq!(s.body["epoch"], "closed");
    assert_eq!(s.body["events_seq"], 0);
    assert!(s.body["turn"].is_null());
    assert_eq!(s.body["messages"][0]["parts"][0]["data"], "hello");
    let mut t = Transcript::default();
    t.load(&s.body);
    assert_eq!(t.items.len(), 2);
    // Its stream waits, from the closed epoch; from another it resets.
    let mut e = stream(&c, "/v1/sessions/ses_a/events", Some("closed:0")).await;
    assert_eq!(e.status, 200);
    let mut old = stream(&c, "/v1/sessions/ses_a/events", Some("e1:5")).await;
    let (id, r) = next(&mut old).await;
    assert_eq!(
        (id, r["type"].as_str(), r["reason"].as_str()),
        (None, Some("reset"), Some("epoch"))
    );
    assert!(
        tokio::time::timeout(WAIT, old.event())
            .await
            .unwrap()
            .is_none(),
        "it ends"
    );
    // An owner taking it makes the waiting stream reload.
    let _tap = w.tap("ses_a").await;
    let (_, r) = next(&mut e).await;
    assert_eq!(r["reason"], "epoch");
    // Messages page.
    let m = get(&c, "/v1/sessions/ses_a/messages?limit=1").await;
    assert_eq!(m.body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(m.body["more"], json!(true));
    let before = m.body["seqs"][0].as_i64().unwrap();
    let m = get(
        &c,
        &format!("/v1/sessions/ses_a/messages?limit=5&before={before}"),
    )
    .await;
    assert_eq!(m.body["messages"][0]["parts"][0]["data"], "hello");
    assert_eq!(m.body["more"], json!(false));
    assert_eq!(
        get(&c, "/v1/sessions/ses_a/messages?limit=x").await.status,
        400
    );
    w.stop().await;
}

/// The scripted turn of [`the_cut_points`]: each step's events, and what
/// the store holds once it ends.
fn step(t: &Tap, i: usize) {
    match i {
        0 => t.turn_start(&Origin::terminal(), Some("list files")),
        1 => t.push(json!({"type": "text", "delta": "Look"})),
        2 => t.push(json!({"type": "text", "delta": "ing."})),
        3 => t.push(json!({"type": "tool_start", "id": "c1", "name": "shell", "input": {"command": "ls"}})),
        4 => t.push(json!({"type": "tool_output", "id": "c1", "delta": "a\n"})),
        5 => t.push(json!({"type": "permission", "id": "p1", "action": "edit", "resources": ["db.rs"], "title": "Edit db.rs"})),
        6 => t.push(json!({"type": "prompt_resolved", "id": "p1", "kind": "permission", "reply": "once"})),
        7 => t.push(json!({"type": "tool_end", "id": "c1", "name": "shell", "output": "a\n", "error": false})),
        8 => t.push(json!({"type": "text", "delta": "Done."})),
        _ => unreachable!(),
    }
}

const STEPS: usize = 9;

/// The turn ends: its messages are stored, then `done` says so, then the
/// session goes idle.
fn finish(w: &W, t: &Tap) {
    w.append("ses_a", Message::user("list files"));
    w.append(
        "ses_a",
        Message {
            role: Role::Assistant,
            parts: vec![
                Part::text("Looking."),
                Part::ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    input: json!({"command": "ls"}),
                    meta: None,
                },
            ],
        },
    );
    w.append(
        "ses_a",
        Message {
            role: Role::User,
            parts: vec![Part::ToolResult {
                id: "c1".into(),
                name: "shell".into(),
                content: "a\n".into(),
                is_error: false,
                media: vec![],
            }],
        },
    );
    let last = w.append(
        "ses_a",
        Message {
            role: Role::Assistant,
            parts: vec![Part::text("Done.")],
        },
    );
    t.mirror().unwrap().done(None, json!({}), last, None);
}

/// S22: a client that GETs at any point of a turn and subscribes from
/// `events_seq` rebuilds the transcript a client that watched it live
/// has: before `turn_start`, before the first text, mid-text, mid-tool,
/// during a permission prompt, just before and after `done`, and once
/// idle. Permission cards keep their place; `done` applies once.
#[tokio::test(flavor = "multi_thread")]
async fn the_cut_points() {
    for cut in 0..=STEPS + 2 {
        let w = W::start(instant()).await;
        w.stored("ses_a");
        w.append("ses_a", Message::user("earlier"));
        let tap = w.tap("ses_a").await;
        let (_p, c) = w.phone(&[("ses_a", Scope::Watch)]).await;
        let mut live = Transcript::default();
        let plan = live.load(&get(&c, "/v1/sessions/ses_a").await.body);
        let mut le = stream(
            &c,
            "/v1/sessions/ses_a/events",
            plan.cursor.map(|c| c.id()).as_deref(),
        )
        .await;
        let mut late = None;
        for i in 0..=STEPS + 1 {
            if i == cut {
                let snap = get(&c, "/v1/sessions/ses_a").await.body;
                let mut t = Transcript::default();
                let p = t.load(&snap);
                if (1..=STEPS).contains(&cut) {
                    assert!(snap["turn"]["id"].is_string(), "mid-turn at {cut}");
                    assert_eq!(snap["busy"], json!(true));
                    assert!(p.replaying_turn.is_some());
                }
                let e = stream(
                    &c,
                    "/v1/sessions/ses_a/events",
                    p.cursor.map(|c| c.id()).as_deref(),
                )
                .await;
                late = Some((t, e));
            }
            match i {
                i if i < STEPS => step(&tap, i),
                i if i == STEPS => finish(&w, &tap),
                _ => tap.idle(),
            }
        }
        let snap_cut = cut == STEPS + 2;
        if snap_cut {
            let snap = get(&c, "/v1/sessions/ses_a").await.body;
            let mut t = Transcript::default();
            t.load(&snap);
            let e = stream(
                &c,
                "/v1/sessions/ses_a/events",
                t.cursor.as_ref().map(|c| c.id()).as_deref(),
            )
            .await;
            late = Some((t, e));
        }
        let last = tap.mirror().unwrap().last_seq();
        follow(&mut le, &mut live, last).await;
        let (mut t, mut e) = late.unwrap();
        if !snap_cut {
            follow(&mut e, &mut t, last).await;
        }
        // A prompt's card is the live stream's alone: the stored history
        // a snapshot after `done` reads has the turn's messages, not its
        // prompts.
        let cards = |items: &[Value]| -> Vec<Value> {
            items
                .iter()
                .filter(|i| cut <= STEPS || (i["kind"] != "permission" && i["kind"] != "question"))
                .cloned()
                .collect()
        };
        assert_eq!(cards(&t.items), cards(&live.items), "cut {cut}");
        assert_eq!(t.busy, live.busy, "cut {cut}");
        assert!(!live.busy);
        assert_eq!(live.items.len(), 6, "{:?}", live.items);
        assert_eq!(live.items[4]["kind"], "permission");
        assert_eq!(live.items[4]["answered"], "once");
        // `done` never twice: resuming from it replays nothing of it.
        let done_seq = last - 1;
        let mut again = stream(
            &c,
            "/v1/sessions/ses_a/events",
            Some(&format!("{}:{done_seq}", w.hub.epoch())),
        )
        .await;
        let (_, ev) = next(&mut again).await;
        assert_eq!(ev["type"], "session_status");
        w.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resuming_mid_turn_leaves_no_gap_and_a_new_epoch_resets() {
    let w = W::start(instant()).await;
    w.stored("ses_a");
    let tap = w.tap("ses_a").await;
    let (_p, c) = w.phone(&[("ses_a", Scope::Watch)]).await;
    let mut e = stream(&c, "/v1/sessions/ses_a/events", None).await;
    step(&tap, 0);
    step(&tap, 1);
    let (id1, ev) = next(&mut e).await;
    assert_eq!(ev["type"], "turn_start");
    assert_eq!(
        id1.as_deref(),
        Some(format!("{}:1", w.hub.epoch()).as_str()),
        "SSE id is epoch:seq"
    );
    drop(e);
    for i in 2..STEPS {
        step(&tap, i);
    }
    let mut e = stream(&c, "/v1/sessions/ses_a/events", id1.as_deref()).await;
    let mut seqs = vec![];
    while seqs.last() != Some(&tap.mirror().unwrap().last_seq()) {
        let (_, ev) = next(&mut e).await;
        seqs.push(ev["seq"].as_u64().unwrap());
    }
    assert_eq!(seqs[0], 2, "from the one after the cursor");
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    // The owner goes and another process (a new epoch) takes the session.
    w.hub.release("ses_a");
    assert!(w.hub.sync().await);
    assert!(
        tokio::time::timeout(WAIT, e.event())
            .await
            .unwrap()
            .is_none(),
        "the stream ends"
    );
    let other = Hub::connect_to(
        kind::SERVE,
        w.dir.clone(),
        instant(),
        Duration::from_millis(20),
    );
    let _t2 = other.tap_for("ses_a", Info::default(), 0);
    assert!(other.sync().await);
    let mut e = stream(&c, "/v1/sessions/ses_a/events", id1.as_deref()).await;
    let (id, ev) = next(&mut e).await;
    assert_eq!(
        (id, ev["type"].as_str(), ev["reason"].as_str()),
        (None, Some("reset"), Some("epoch"))
    );
    let s = get(&c, "/v1/sessions/ses_a").await;
    assert_eq!(s.body["owner"], "serve");
    assert_eq!(s.body["epoch"], other.epoch());
    w.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_older_than_the_ring_resets() {
    let w = W::start(Limits {
        ring_events: 5,
        ..instant()
    })
    .await;
    let tap = w.tap("ses_a").await;
    let (_p, c) = w.phone(&[("ses_a", Scope::Watch)]).await;
    for i in 0..20 {
        tap.config("build", &format!("m/{i}"), None);
    }
    let mut e = stream(
        &c,
        "/v1/sessions/ses_a/events",
        Some(&format!("{}:2", w.hub.epoch())),
    )
    .await;
    assert_eq!(next(&mut e).await.1["reason"], "gap");
    // A fresh snapshot's cursor doesn't.
    let s = get(&c, "/v1/sessions/ses_a").await.body;
    assert_eq!(s["events_seq"], 20);
    assert_eq!(s["model"], "m/19");
    let mut e = stream(
        &c,
        "/v1/sessions/ses_a/events",
        Some(&format!("{}:20", w.hub.epoch())),
    )
    .await;
    tap.config("plan", "m/x", None);
    assert_eq!(next(&mut e).await.1["type"], "config");
    w.stop().await;
}

/// S20 end to end: a turn over its cap resets a resume inside it, and the
/// snapshot's partial and partial_seq resume without one.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_turn_gives_a_partial_to_resume_from() {
    let w = W::start(Limits {
        turn_bytes: 32 * 1024,
        event_bytes: 4096,
        ..instant()
    })
    .await;
    let tap = w.tap("ses_a").await;
    let (_p, c) = w.phone(&[("ses_a", Scope::Watch)]).await;
    tap.turn_start(&Origin::terminal(), Some("big"));
    for i in 0..100 {
        tap.push(json!({"type": "tool_start", "id": format!("c{i}"), "name": "read", "input": {"p": "x".repeat(400)}}));
    }
    let mut e = stream(
        &c,
        "/v1/sessions/ses_a/events",
        Some(&format!("{}:0", w.hub.epoch())),
    )
    .await;
    assert_eq!(next(&mut e).await.1["reason"], "turn_too_large");
    let s = get(&c, "/v1/sessions/ses_a").await.body;
    let mut t = Transcript::default();
    let plan = t.load(&s);
    assert!(plan.partial);
    assert_eq!(t.items.len(), 101, "the prompt and its calls");
    let mut e = stream(
        &c,
        "/v1/sessions/ses_a/events",
        plan.cursor.map(|c| c.id()).as_deref(),
    )
    .await;
    tap.push(json!({"type": "text", "delta": "and so"}));
    let (_, ev) = next(&mut e).await;
    assert!(matches!(t.deliver(None, &ev), Verdict::Apply(_)));
    assert_eq!(t.items.last().unwrap()["text"], "and so");
    w.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grants_keep_sessions_and_events_apart() {
    let w = W::start(instant()).await;
    let a = w.tap("ses_a").await;
    let b = w.tap("ses_b").await;
    let (_pa, ca) = w.phone(&[("ses_a", Scope::Watch)]).await;
    let (_pb, cb) = w.phone(&[("ses_b", Scope::Control)]).await;
    let mut ea = stream(&ca, "/v1/events", None).await;
    let mut eb = stream(&cb, "/v1/events", None).await;
    assert_eq!(
        stream(&ca, "/v1/sessions/ses_b/events", None).await.status,
        404
    );
    for i in 0..5 {
        b.push(json!({"type": "notice", "message": format!("b{i}")}));
        a.push(json!({"type": "notice", "message": format!("a{i}")}));
    }
    for i in 0..5 {
        let (_, x) = next(&mut ea).await;
        assert_eq!(
            (x["session"].as_str(), x["message"].as_str()),
            (Some("ses_a"), Some(format!("a{i}").as_str()))
        );
        let (_, y) = next(&mut eb).await;
        assert_eq!(y["session"], "ses_b");
    }
    let l = get(&ca, "/v1/sessions").await;
    assert_eq!(l.body.as_array().unwrap().len(), 1);
    assert_eq!(get(&ca, "/v1/sessions/ses_b").await.status, 404);
    w.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_ping_and_events_are_at_most_a_mebibyte() {
    let w = W::start_with(instant(), |o| o.ping = Duration::from_millis(100)).await;
    let tap = w.tap("ses_a").await;
    let (_p, c) = w.phone(&[("ses_a", Scope::Watch)]).await;
    let mut e = stream(&c, "/v1/sessions/ses_a/events", None).await;
    assert_eq!(
        tokio::time::timeout(WAIT, e.next()).await.unwrap(),
        Some(Sse::Comment("ping".into()))
    );
    tap.turn_start(&Origin::terminal(), None);
    tap.push(json!({"type": "tool_end", "id": "c", "name": "shell", "output": "z".repeat(3 << 20), "error": false}));
    let _ = next(&mut e).await;
    let (_, big) = next(&mut e).await;
    assert_eq!(big["type"], "tool_end");
    assert!(big.to_string().len() <= 1 << 20);
    assert_eq!(big["truncated"], json!(true));
    w.stop().await;
}

/// S8 end to end: a burst of deltas arrives as a few merged events.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_deltas_arrives_merged() {
    let w = W::start(Limits::default()).await;
    let tap = w.tap("ses_a").await;
    let (_p, c) = w.phone(&[("ses_a", Scope::Watch)]).await;
    let mut e = stream(&c, "/v1/sessions/ses_a/events", None).await;
    tap.turn_start(&Origin::terminal(), None);
    for _ in 0..200 {
        tap.push(json!({"type": "text", "delta": "x"}));
    }
    tap.push(json!({"type": "notice", "message": "end"}));
    let _ = next(&mut e).await;
    let mut text = String::new();
    let mut n = 0;
    loop {
        let (_, ev) = next(&mut e).await;
        if ev["type"] == "notice" {
            break;
        }
        text.push_str(ev["delta"].as_str().unwrap());
        n += 1;
    }
    assert_eq!(text, "x".repeat(200));
    assert!(n < 10, "{n} events for a burst");
    w.stop().await;
}

/// I9: an owner needs the registration's token and this user's uid.
#[tokio::test(flavor = "multi_thread")]
async fn owner_registration_needs_the_token_and_the_uid() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let w = W::start(instant()).await;
    let mut s = tokio::net::UnixStream::connect(w.dir.join(super::SOCKET))
        .await
        .unwrap();
    s.write_all(b"{\"token\":\"wrong\",\"protocol\":1,\"version\":\"0\",\"request\":{\"op\":\"owner\",\"kind\":\"tui\",\"pid\":1}}\n{\"op\":\"hold\",\"session\":\"ses_x\",\"info\":{},\"busy\":false}\n")
        .await
        .unwrap();
    let mut line = String::new();
    tokio::io::BufReader::new(s)
        .read_line(&mut line)
        .await
        .unwrap();
    assert!(line.contains("authentication failed"), "{line}");
    assert_eq!(
        control::status(&w.dir).await.unwrap().owners,
        1,
        "only the hub"
    );
    let (_p, c) = w.phone(&[("*", Scope::Watch)]).await;
    assert_eq!(get(&c, "/v1/sessions").await.body, json!([]));
    w.stop().await;
    // A gateway expecting another uid closes an owner's connection unanswered.
    let w = W::start_with(instant(), |o| o.uid = crate::daemon::fs::euid() + 1).await;
    let mut conn = control::connect(
        &w.dir,
        Request::Owner {
            kind: "tui".into(),
            pid: 1,
        },
    )
    .await
    .unwrap();
    assert!(tokio::time::timeout(WAIT, conn.next())
        .await
        .unwrap()
        .unwrap()
        .is_none());
    w.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_holder_keeps_a_session() {
    let w = W::start(instant()).await;
    let _a = w.tap("ses_a").await;
    let other = Hub::connect_to(
        kind::ACP,
        w.dir.clone(),
        instant(),
        Duration::from_millis(20),
    );
    let b = other.tap_for("ses_a", Info::default(), 0);
    assert!(other.sync().await);
    b.push(json!({"type": "notice", "message": "not mine"}));
    let (_p, c) = w.phone(&[("ses_a", Scope::Watch)]).await;
    let s = get(&c, "/v1/sessions/ses_a").await.body;
    assert_eq!(s["owner"], "tui");
    assert_eq!(s["epoch"], w.hub.epoch());
    assert_eq!(control::status(&w.dir).await.map(|s| s.owners), Some(2));
    w.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_gateway_gets_the_sessions_again() {
    let w = W::start(instant()).await;
    let tap = w.tap("ses_a").await;
    tap.turn_start(&Origin::terminal(), Some("x"));
    let dir = w.dir.clone();
    let _ = w.ready.stop.send(true);
    tokio::time::timeout(WAIT, w.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // Meanwhile the turn goes on.
    tap.push(json!({"type": "text", "delta": "while away"}));
    let mut o = Options::new(dir, Relay::None);
    o.key = w.db.with_file_name("remote-key");
    o.db = w.db.clone();
    o.sessions_db = Some(w.sessions.clone());
    o.config_dir = None;
    o.loopback = true;
    o.idle = Duration::from_secs(3600);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(gateway::serve(o, Some(tx)));
    let ready = tokio::time::timeout(WAIT, rx).await.unwrap().unwrap();
    assert!(w.hub.sync().await, "the hub came back");
    let p = Phone::new().await.unwrap();
    Store::open(&w.db)
        .unwrap()
        .pair(
            &p.id(),
            "P",
            "ios",
            None,
            None,
            Some(("ses_a", Scope::Watch)),
        )
        .unwrap();
    let c = p
        .connect(&ready.node_id, &ready.addrs, REMOTE_ALPN)
        .await
        .unwrap();
    let s = get(&c, "/v1/sessions/ses_a").await.body;
    assert_eq!(s["busy"], json!(true));
    let mut t = Transcript::default();
    let plan = t.load(&s);
    let mut e = stream(
        &c,
        "/v1/sessions/ses_a/events",
        plan.cursor.map(|c| c.id()).as_deref(),
    )
    .await;
    follow(&mut e, &mut t, 3).await;
    assert_eq!(t.items[1]["text"], "while away", "nothing lost meanwhile");
    drop(w.hub);
    let _ = ready.stop.send(true);
    tokio::time::timeout(WAIT, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// Stress: many phones follow one session while it streams; each gets
/// every event once, in order.
#[tokio::test(flavor = "multi_thread")]
async fn many_followers_get_every_event_in_order() {
    let w = W::start(instant()).await;
    let tap = w.tap("ses_a").await;
    let mut followers = vec![];
    for _ in 0..8 {
        let (p, c) = w.phone(&[("ses_a", Scope::Watch)]).await;
        let e = stream(
            &c,
            "/v1/sessions/ses_a/events",
            Some(&format!("{}:0", w.hub.epoch())),
        )
        .await;
        followers.push((p, c, e));
    }
    let pushing = {
        let tap = tap.clone();
        std::thread::spawn(move || {
            tap.turn_start(&Origin::terminal(), Some("go"));
            for i in 0..1500 {
                tap.push(json!({"type": "notice", "message": format!("{i}")}));
            }
        })
    };
    pushing.join().unwrap();
    let last = tap.mirror().unwrap().last_seq();
    let mut tasks = tokio::task::JoinSet::new();
    for (p, c, mut e) in followers {
        tasks.spawn(async move {
            let mut want = 1;
            while want <= last {
                let (_, ev) = next(&mut e).await;
                assert_eq!(ev["seq"].as_u64(), Some(want));
                want += 1;
            }
            drop((p, c));
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.unwrap();
    }
    w.stop().await;
}
