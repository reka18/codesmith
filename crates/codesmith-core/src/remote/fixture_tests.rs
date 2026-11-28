//! The app team's `remote_events.json` (S22, S24), pinned in
//! `testdata/README.md`: its own tables through the reference client
//! ([`super::client`]), and its scripted turns through this machine's
//! [`Mirror`], cut at every point, resumed and reloaded.

use super::client::{self, Cursor, Transcript, Verdict};
use super::mirror::{Info, Limits, Mirror, Origin, Out};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FIXTURE: &[u8] = include_bytes!("testdata/remote_events.json");

fn fixture() -> Value {
    serde_json::from_slice(FIXTURE).unwrap()
}

#[test]
fn the_copy_is_the_pinned_one() {
    use sha2::Digest;
    let hash: String = sha2::Sha256::digest(FIXTURE)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let readme = include_str!("testdata/README.md");
    let row = readme
        .lines()
        .find(|l| l.contains("`remote_events.json`"))
        .unwrap();
    assert!(row.contains(&hash), "{hash} isn't pinned in: {row}");
    assert!(row.contains("`ba23710`"));
}

#[test]
fn the_observe_table() {
    for case in fixture()["observe"].as_array().unwrap() {
        let cursor: Option<Cursor> = case["cursor"].as_object().map(|c| Cursor {
            epoch: c["epoch"].as_str().unwrap().into(),
            seq: c["seq"].as_u64().unwrap(),
        });
        let got = client::observe(cursor.as_ref(), case["id"].as_str(), &case["data"]);
        let e = &case["expect"];
        let want = match e["verdict"].as_str().unwrap() {
            "apply" => Verdict::Apply(e["cursor"].as_object().map(|c| Cursor {
                epoch: c["epoch"].as_str().unwrap().into(),
                seq: c["seq"].as_u64().unwrap(),
            })),
            "skip" => Verdict::Skip,
            _ => Verdict::Reset(e["reason"].as_str().unwrap().into()),
        };
        assert_eq!(got, want, "{}", case["name"]);
    }
}

#[test]
fn the_snapshot_plans_turn_keys_and_reload_delays() {
    let f = fixture();
    for case in f["snapshots"].as_array().unwrap() {
        let p = client::plan(&case["snapshot"]);
        let e = &case["expect"];
        assert_eq!(
            p.cursor.map(|c| c.json()).unwrap_or(Value::Null),
            e["cursor"],
            "{}",
            case["name"]
        );
        assert_eq!(
            p.replaying_turn.map(Value::from).unwrap_or(Value::Null),
            e["replayingTurn"],
            "{}",
            case["name"]
        );
        assert_eq!(json!(p.partial), e["partial"], "{}", case["name"]);
    }
    for k in f["turnKeys"].as_array().unwrap() {
        assert_eq!(
            client::turn_key(k["epoch"].as_str(), k["turn"].as_str().unwrap()),
            k["key"]
        );
    }
    for d in f["reloadDelays"].as_array().unwrap() {
        assert_eq!(
            json!(client::reload_delay(
                d["resetsInARow"].as_u64().unwrap(),
                d["backoffMs"].as_u64().unwrap()
            )),
            d["delayMs"]
        );
    }
}

/// The local steps of a replay: (the seq after which, the prompt id, the
/// reply).
fn local_steps(r: &Value) -> Vec<(u64, String, Value)> {
    r["local"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let s = &l["step"];
            if let Some(a) = s.get("answered") {
                (
                    l["afterSeq"].as_u64().unwrap(),
                    a["id"].as_str().unwrap().to_string(),
                    a["reply"].clone(),
                )
            } else {
                let a = &s["questionAnswered"];
                (
                    l["afterSeq"].as_u64().unwrap(),
                    a["id"].as_str().unwrap().to_string(),
                    a["answers"].clone(),
                )
            }
        })
        .collect()
}

/// A client that runs a replay's local steps once each, right after the
/// event they follow is first applied (`seq_of` maps the event to the
/// fixture's seq).
struct Phone {
    t: Transcript,
    steps: Vec<(u64, String, Value)>,
    done: Vec<bool>,
    /// The fixture seqs applied, in order.
    applied: Vec<u64>,
}

impl Phone {
    fn new(r: &Value) -> Phone {
        let steps = local_steps(r);
        Phone {
            t: Transcript::default(),
            done: vec![false; steps.len()],
            steps,
            applied: vec![],
        }
    }

    /// Deliver an event that stands for the fixture's `fixture_seqs` (a
    /// merged one stands for several).
    fn deliver(&mut self, id: Option<&str>, data: &Value, fixture_seqs: Vec<u64>) -> Verdict {
        let v = self.t.deliver(id, data);
        if matches!(v, Verdict::Apply(_)) {
            self.applied.extend(&fixture_seqs);
            for (i, (after, pid, reply)) in self.steps.iter().enumerate() {
                if fixture_seqs.contains(after) && !self.done[i] {
                    self.done[i] = true;
                    self.t.answer(pid, reply.clone());
                }
            }
        }
        v
    }

    fn check(&self, r: &Value, how: &str) {
        let e = &r["expect"];
        assert_eq!(
            Value::Array(self.t.items.clone()),
            e["items"],
            "{}: {how}",
            r["name"]
        );
        assert_eq!(json!(self.t.busy), e["busy"], "{}: {how}", r["name"]);
    }
}

/// The fixture's own replays, by its rules: for each cut, the resume path
/// (the last two applied delivered again, then the rest) and the reload
/// path (load again, replay the turn).
#[test]
fn the_fixture_replays_by_its_own_events() {
    for r in fixture()["replays"].as_array().unwrap() {
        let evs = r["events"].as_array().unwrap();
        let n = evs.len();
        let seq = |e: &Value| e["data"]["seq"].as_u64().unwrap();
        let mut snap = r["snapshot"].clone();
        snap["messages"] = r["history"].clone();
        for k in r["cuts"].as_array().unwrap() {
            let k = k.as_u64().unwrap() as usize;
            let start = |p: &mut Phone| {
                p.t.load(&snap);
                for e in &evs[..k] {
                    p.deliver(e["id"].as_str(), &e["data"], vec![seq(e)]);
                }
            };
            let mut p = Phone::new(r);
            start(&mut p);
            for e in &evs[k.saturating_sub(2)..] {
                let v = p.deliver(e["id"].as_str(), &e["data"], vec![seq(e)]);
                assert!(!matches!(v, Verdict::Reset(_)));
            }
            p.check(r, &format!("resume at {k}"));
            if k < n {
                let mut p = Phone::new(r);
                start(&mut p);
                p.t.load(&snap);
                for e in evs {
                    p.deliver(e["id"].as_str(), &e["data"], vec![seq(e)]);
                }
                p.check(r, &format!("reload at {k}"));
            }
        }
    }
}

/// One fixture event into a mirror, as an owner would send it.
fn feed(m: &Mirror, data: &Value) {
    let mut d = data.clone();
    for k in ["v", "epoch", "seq", "session", "turn", "ts"] {
        d.as_object_mut().unwrap().remove(k);
    }
    match d["type"].as_str().unwrap() {
        "turn_start" => {
            let kind = match d["origin"]["kind"].as_str().unwrap() {
                "device" => "device",
                "bus" => "bus",
                "resumed" => "resumed",
                _ => "terminal",
            };
            m.turn_start(
                &Origin {
                    kind,
                    name: d["origin"]["name"].as_str().unwrap_or("").into(),
                },
                None,
            );
        }
        "done" => m.done(d["error"].as_str(), json!({}), 1, None),
        _ => m.push(d),
    }
}

struct Run {
    m: Mirror,
    log: Arc<Mutex<Vec<Value>>>,
}

impl Run {
    fn new() -> Run {
        let m = Mirror::new(
            "ses_a",
            &super::mirror::new_epoch(),
            Info::default(),
            0,
            Limits {
                merge_window: Duration::ZERO,
                ..Default::default()
            },
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        m.connect(Some(Arc::new(move |_: &str, o: Out| {
            if let Out::Event(e) = o {
                l.lock().unwrap().push(e)
            }
        })));
        Run { m, log }
    }

    fn snapshot(&self, history: &Value) -> Value {
        let s = self.m.snapshot();
        json!({"epoch": s.epoch, "events_seq": s.events_seq, "turn": s.turn,
               "busy": s.busy, "messages": history})
    }

    fn sent(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    fn since(&self, from: usize) -> Vec<Value> {
        self.log.lock().unwrap()[from..].to_vec()
    }
}

fn id(e: &Value) -> String {
    format!("{}:{}", e["epoch"].as_str().unwrap(), e["seq"])
}

/// The fixture's scripted turns through this machine's mirror: the
/// snapshot it gives during the turn, its live events, and for every cut
/// its resume and its reload, each ending in the fixture's transcript,
/// with never a reset. The events it sends have the fixture's fields.
#[test]
fn the_fixture_replays_through_the_mirror() {
    for r in fixture()["replays"].as_array().unwrap() {
        let evs = r["events"].as_array().unwrap();
        let n = evs.len();
        // The fixture's seq of the event this mirror numbered `seq`: one
        // each, in order, from 1.
        let fseq = |e: &Value| -> Vec<u64> {
            let last = e["seq"].as_u64().unwrap();
            let first = e["merged_from"].as_u64().unwrap_or(last);
            (first..=last)
                .map(|s| evs[s as usize - 1]["data"]["seq"].as_u64().unwrap())
                .collect()
        };
        let history = &r["history"];
        for k in r["cuts"].as_array().unwrap() {
            let k = k.as_u64().unwrap() as usize;
            for reload in [false, true] {
                if reload && k >= n {
                    continue;
                }
                let run = Run::new();
                feed(&run.m, &evs[0]["data"]);
                let mut p = Phone::new(r);
                let plan = p.t.load(&run.snapshot(history));
                assert!(plan.replaying_turn.is_some(), "a mid-turn snapshot");
                let at = run.sent();
                let first = run
                    .m
                    .subscribe(plan.cursor.as_ref().map(|c| (c.epoch.as_str(), c.seq)));
                assert!(first.reset.is_none());
                let mut stream = first.events.clone();
                for e in &evs[1..k.max(1)] {
                    feed(&run.m, &e["data"]);
                }
                stream.extend(run.since(at));
                let mut delivered = Vec::new();
                for e in stream.iter().take(k) {
                    let v = p.deliver(Some(&id(e)), e, fseq(e));
                    assert!(matches!(v, Verdict::Apply(_)), "{v:?}");
                    delivered.push(e.clone());
                }
                // The stream drops here.
                if !reload {
                    for e in &evs[k.max(1)..] {
                        feed(&run.m, &e["data"]);
                    }
                    let c = p.t.cursor.clone().unwrap();
                    let again = run.m.subscribe(Some((&c.epoch, c.seq)));
                    assert!(again.reset.is_none(), "resume at {k}");
                    for e in &delivered[delivered.len().saturating_sub(2)..] {
                        assert_eq!(p.deliver(Some(&id(e)), e, fseq(e)), Verdict::Skip);
                    }
                    for e in &again.events {
                        assert!(matches!(
                            p.deliver(Some(&id(e)), e, fseq(e)),
                            Verdict::Apply(_)
                        ));
                    }
                    for e in &again.resend {
                        p.deliver(None, e, fseq(e));
                    }
                } else {
                    let plan = p.t.load(&run.snapshot(history));
                    assert!(plan.replaying_turn.is_some());
                    p.applied.clear();
                    let at = run.sent();
                    let again = run
                        .m
                        .subscribe(plan.cursor.as_ref().map(|c| (c.epoch.as_str(), c.seq)));
                    assert!(again.reset.is_none(), "reload at {k}");
                    for e in &again.events {
                        assert!(matches!(
                            p.deliver(Some(&id(e)), e, fseq(e)),
                            Verdict::Apply(_)
                        ));
                    }
                    for e in &evs[k.max(1)..] {
                        feed(&run.m, &e["data"]);
                    }
                    for e in run.since(at) {
                        assert!(matches!(
                            p.deliver(Some(&id(&e)), &e, fseq(&e)),
                            Verdict::Apply(_)
                        ));
                    }
                }
                let how = format!("{} at {k}", if reload { "reload" } else { "resume" });
                p.check(r, &how);
                let all: Vec<u64> = evs
                    .iter()
                    .map(|e| e["data"]["seq"].as_u64().unwrap())
                    .collect();
                assert_eq!(p.applied, all, "each event applied once, in order: {how}");
                assert_eq!(p.t.resets, 0);
            }
        }
        // What the mirror sent has the fixture's fields, event by event.
        let run = Run::new();
        for e in evs {
            feed(&run.m, &e["data"]);
        }
        for (ours, theirs) in run.since(0).iter().zip(evs) {
            let keys = |v: &Value| {
                let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
                k.sort();
                k
            };
            assert_eq!(
                keys(ours),
                keys(&theirs["data"]),
                "{}",
                theirs["data"]["type"]
            );
        }
    }
}
