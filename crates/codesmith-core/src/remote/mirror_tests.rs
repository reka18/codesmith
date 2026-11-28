use super::*;
use rand::{Rng, SeedableRng};

/// A mirror with the outlet recorded.
fn mirror(limits: Limits) -> (Arc<Mirror>, Arc<Mutex<Vec<Out>>>) {
    let m = Arc::new(Mirror::new(
        "ses_a",
        "e1",
        Info {
            agent: "build".into(),
            model: "m/x".into(),
            directory: "/p".into(),
            ..Default::default()
        },
        0,
        limits,
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    m.connect(Some(Arc::new(move |_: &str, o: Out| {
        s.lock().unwrap().push(o)
    })));
    (m, seen)
}

fn instant() -> Limits {
    Limits {
        merge_window: Duration::ZERO,
        ..Default::default()
    }
}

fn events(seen: &Mutex<Vec<Out>>) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|o| match o {
            Out::Event(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_event_has_the_envelope_and_seqs_rise_across_turns() {
    let (m, seen) = mirror(instant());
    m.turn_start(&Origin::terminal(), Some("hi"));
    m.push(json!({"type": "text", "delta": "a"}));
    m.done(None, json!({}), 7, Some("T".into()));
    m.idle();
    m.turn_start(&Origin::device("Phone"), Some("again"));
    let ev = events(&seen);
    let seqs: Vec<u64> = ev.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=ev.len() as u64).collect::<Vec<_>>());
    for e in &ev {
        for k in ["v", "epoch", "seq", "session", "turn", "ts", "type"] {
            assert!(e.get(k).is_some(), "{k} in {e}");
        }
        assert_eq!(e["session"], "ses_a");
        assert_eq!(e["epoch"], "e1");
    }
    assert_eq!(ev[0]["origin"], json!({"kind": "terminal", "name": ""}));
    assert_eq!(ev[1]["type"], "user_message");
    assert_eq!(ev[1]["text"], "hi");
    assert_eq!(ev[3]["type"], "done");
    assert!(ev[3]["error"].is_null());
    assert_eq!(ev[4]["type"], "session_status");
    assert_eq!(ev[4]["busy"], json!(false));
    assert!(ev[4]["turn"].is_null(), "between turns");
    let (t1, t2) = (
        ev[0]["turn"].as_str().unwrap(),
        ev[5]["turn"].as_str().unwrap(),
    );
    assert_ne!(t1, t2);
    assert!(t1.starts_with("trn_e1_"), "unique across epochs: {t1}");
    assert_eq!(ev[5]["origin"]["kind"], "device");
}

#[test]
fn a_subagents_session_moves_to_child() {
    let (m, seen) = mirror(instant());
    m.turn_start(&Origin::terminal(), None);
    m.push(json!({"type": "tool_start", "id": "c", "name": "x › read", "input": {}, "session": "ses_sub"}));
    let ev = events(&seen);
    assert_eq!(ev[1]["session"], "ses_a");
    assert_eq!(ev[1]["child"], "ses_sub");
}

#[test]
fn the_snapshot_cursor_splits_the_stream() {
    let (m, _) = mirror(instant());
    assert_eq!(m.snapshot().events_seq, 0, "no events yet");
    assert!(m.snapshot().turn.is_null());
    m.turn_start(&Origin::terminal(), Some("x"));
    m.push(json!({"type": "text", "delta": "a"}));
    let s = m.snapshot();
    assert_eq!(s.events_seq, 0, "a running turn is all after the cursor");
    assert_eq!(s.turn["start_seq"], 1);
    assert!(s.turn.get("partial").is_none());
    assert!(s.busy);
    m.done(None, json!({}), 42, None);
    let s = m.snapshot();
    assert!(s.turn.is_null());
    assert_eq!(s.events_seq, 4);
    assert_eq!(s.history_seq, 42, "the store covers the turn now");
    assert!(s.busy, "busy until it is really idle (C10)");
    m.idle();
    assert!(!m.snapshot().busy);
}

#[test]
fn resuming_replays_with_the_original_seqs() {
    let (m, seen) = mirror(instant());
    m.turn_start(&Origin::terminal(), Some("x"));
    m.push(json!({"type": "tool_start", "id": "c", "name": "shell", "input": {}}));
    m.push(json!({"type": "done", "error": null}));
    let live = events(&seen);
    let r = m.subscribe(Some(("e1", 1)));
    assert!(r.reset.is_none());
    assert_eq!(r.events, live[1..].to_vec());
    // Up to date: nothing to replay.
    assert!(m.subscribe(Some(("e1", 4))).events.is_empty());
    assert_eq!(m.subscribe(Some(("e9", 1))).reset, Some(reason::EPOCH));
    assert_eq!(m.subscribe(Some(("e1", 99))).reset, Some(reason::GAP));
}

#[test]
fn a_gap_older_than_the_ring_resets() {
    let (m, _) = mirror(Limits {
        ring_events: 10,
        ..instant()
    });
    for _ in 0..30 {
        m.config("a", &format!("{}", rand::random::<u32>()), None);
    }
    assert_eq!(m.subscribe(Some(("e1", 5))).reset, Some(reason::GAP));
    let r = m.subscribe(Some(("e1", 25)));
    assert!(r.reset.is_none());
    assert_eq!(r.events.len(), 5);
    assert_eq!(m.kept().0, 10);
}

/// S19: a running turn longer than the ring is kept whole.
#[test]
fn a_turn_longer_than_the_ring_is_kept_from_its_start() {
    let (m, seen) = mirror(Limits {
        ring_events: 10,
        ring_bytes: 4096,
        ..instant()
    });
    m.turn_start(&Origin::terminal(), Some("x"));
    for i in 0..500 {
        m.push(json!({"type": "tool_start", "id": format!("c{i}"), "name": "read", "input": {}}));
    }
    let r = m.subscribe(Some(("e1", 0)));
    assert!(r.reset.is_none());
    assert_eq!(r.events, events(&seen));
    // Once it ends, the ring's limit applies again.
    m.done(None, json!({}), 1, None);
    assert_eq!(m.subscribe(Some(("e1", 0))).reset, Some(reason::GAP));
}

/// S14: nothing over 1 MiB; the marker says how much was cut.
#[test]
fn an_event_over_the_limit_is_cut_with_a_marker() {
    let (m, seen) = mirror(instant());
    m.turn_start(&Origin::terminal(), None);
    let big = "é".repeat(800_000) + "\"\\\n";
    m.push(json!({"type": "tool_end", "id": "c", "name": "shell", "output": big, "error": false}));
    m.push(json!({"type": "tool_output", "id": "c", "delta": "x".repeat(3 << 20)}));
    let ev = events(&seen);
    for e in &ev[1..] {
        let n = json_len(e);
        assert!(n <= 1 << 20, "{n}");
        assert!(n > (1 << 20) - 4096, "cut close to the limit: {n}");
        assert_eq!(e["truncated"], json!(true));
    }
    let out = ev[1]["output"].as_str().unwrap();
    assert!(out.starts_with("éé"));
    assert!(
        out.contains("more bytes not sent]"),
        "{}",
        &out[out.len() - 60..]
    );
    // Small events aren't touched.
    let mut small = json!({"type": "text", "delta": "x"});
    fit(&mut small, 1 << 20);
    assert!(small.get("truncated").is_none());
}

#[test]
fn deltas_are_merged_to_ten_a_second() {
    let (m, seen) = mirror(Limits::default());
    let t0 = Instant::now();
    m.turn_start(&Origin::terminal(), None);
    for i in 0..50u64 {
        m.push_at(
            json!({"type": "text", "delta": "ab"}),
            t0 + Duration::from_millis(i * 10),
        );
    }
    m.flush_due(t0 + Duration::from_secs(1));
    let texts: Vec<Value> = events(&seen)
        .into_iter()
        .filter(|e| e["type"] == "text")
        .collect();
    assert_eq!(texts.len(), 5, "500 ms of deltas, one each 100 ms");
    let all: String = texts.iter().map(|e| e["delta"].as_str().unwrap()).collect();
    assert_eq!(all, "ab".repeat(50));
    // Another event sends the held delta first, so order is kept.
    m.push_at(json!({"type": "text", "delta": "z"}), t0);
    m.push(json!({"type": "tool_start", "id": "c", "name": "x", "input": {}}));
    let ev = events(&seen);
    assert_eq!(ev[ev.len() - 2]["delta"], "z");
    // A different block isn't merged with it.
    m.push_at(json!({"type": "text", "delta": "1"}), t0);
    m.push_at(json!({"type": "reasoning", "delta": "2"}), t0);
    m.flush();
    let ev = events(&seen);
    assert_eq!(ev[ev.len() - 2]["type"], "text");
    assert_eq!(ev[ev.len() - 1]["type"], "reasoning");
}

/// What a client has after applying `events` in order: each block's text
/// and every other event, by seq. A merged event counts as the deltas it
/// stands for.
fn applied(events: &[Value]) -> (BTreeMap<String, String>, Vec<(u64, Value)>) {
    let mut text = BTreeMap::new();
    let mut other = Vec::new();
    for e in events {
        match block_of(e) {
            Some(b) => text
                .entry(b)
                .or_insert_with(String::new)
                .push_str(e["delta"].as_str().unwrap()),
            None => other.push((e["seq"].as_u64().unwrap(), e.clone())),
        }
    }
    (text, other)
}

/// Bounded property test: for random turns and every cursor, the merged
/// replay after it, added to what the live stream had sent up to it, is
/// exactly the unmerged stream: nothing lost, nothing twice, and no
/// replayed event begins at or before the cursor (the app resets on one
/// that does, as "merged").
#[test]
fn merged_replay_equals_the_unmerged_stream() {
    let mut rng = rand::rngs::StdRng::seed_from_u64(19);
    for round in 0..60 {
        let (m, seen) = mirror(instant());
        if round % 3 == 0 {
            m.config("plan", "m/y", None);
        }
        m.turn_start(&Origin::terminal(), Some("go"));
        for _ in 0..rng.gen_range(1..120) {
            let d: String = (0..rng.gen_range(0..6))
                .map(|_| ['a', 'é', '"', '\n', '🦀'][rng.gen_range(0..5)])
                .collect();
            let ev = match rng.gen_range(0..9) {
                0..=2 => json!({"type": "text", "delta": d}),
                3 => json!({"type": "reasoning", "delta": d}),
                4 => {
                    json!({"type": "tool_output", "id": format!("c{}", rng.gen_range(0..2)), "delta": d})
                }
                5 => json!({"type": "child_text", "session": "ses_s", "delta": d}),
                6 => {
                    json!({"type": "tool_start", "id": "c1", "name": "shell", "input": {"command": d}})
                }
                7 => {
                    json!({"type": "permission", "id": format!("p{}", rng.gen::<u8>()), "action": "edit", "resources": [], "title": "t"})
                }
                _ => json!({"type": "notice", "message": d}),
            };
            m.push(ev);
        }
        if round % 2 == 0 {
            m.done(None, json!({}), 3, None);
        }
        let live = events(&seen);
        let want = applied(&live);
        for c in 0..=m.last_seq() {
            let r = m.subscribe(Some(("e1", c)));
            assert!(r.reset.is_none(), "round {round} cursor {c}");
            let mut prev = c;
            for e in &r.events {
                let seq = e["seq"].as_u64().unwrap();
                let from = e["merged_from"].as_u64().unwrap_or(seq);
                assert!(from > c && seq > prev && from <= seq, "{e} after {c}");
                prev = seq;
            }
            let mut got: Vec<Value> = live
                .iter()
                .filter(|e| e["seq"].as_u64().unwrap() <= c)
                .cloned()
                .collect();
            got.extend(r.events.iter().cloned());
            let got_applied = applied(&got);
            assert_eq!(got_applied.0, want.0, "round {round} cursor {c}");
            assert_eq!(got_applied.1, want.1, "round {round} cursor {c}");
        }
    }
}

/// The running turn's merged events are counted exactly: what the cap
/// measures is what a replay sends.
#[test]
fn merged_sizes_are_counted_exactly() {
    let (m, _) = mirror(instant());
    m.turn_start(&Origin::terminal(), None);
    for i in 0..2000 {
        m.push(json!({"type": "text", "delta": format!("{i}é\"")}));
    }
    let st = m.state.lock().unwrap();
    for e in &st.turn.as_ref().unwrap().events {
        assert_eq!(e.bytes, json_len(&e.body), "{}", e.seq);
    }
    let merged = &st.turn.as_ref().unwrap().events[1];
    assert_eq!(merged.body["merged_from"], 2);
    assert_eq!(merged.body["seq"], 2001);
}

/// S20: over 8 MiB, a resume inside the turn gets turn_too_large; a fresh
/// snapshot has the partial transcript and a cursor that resumes without
/// a reset, however much more the turn sends.
#[test]
fn an_oversized_turn_resets_with_turn_too_large_and_a_partial() {
    let (m, seen) = mirror(Limits {
        turn_bytes: 64 * 1024,
        event_bytes: 8 * 1024,
        ..instant()
    });
    m.config("plan", "m/y", None);
    m.turn_start(&Origin::terminal(), Some("big"));
    for i in 0..200 {
        m.push(json!({"type": "tool_start", "id": format!("c{i}"), "name": "read", "input": {"p": "x".repeat(500)}}));
        m.push(json!({"type": "tool_end", "id": format!("c{i}"), "name": "read", "output": "y".repeat(500), "error": false}));
    }
    assert_eq!(
        m.subscribe(Some(("e1", 1))).reset,
        Some(reason::TURN_TOO_LARGE)
    );
    assert_eq!(
        m.subscribe(Some(("e1", 3))).reset,
        Some(reason::TURN_TOO_LARGE)
    );
    let s = m.snapshot();
    let partial = s.turn["partial"].as_array().unwrap();
    assert_eq!(partial[0]["role"], "user");
    assert_eq!(s.turn["partial_seq"], m.last_seq());
    assert_eq!(s.events_seq, 1, "the turn starts after config");
    // A fresh cursor never resets, even as the turn goes on.
    let cursor = s.turn["partial_seq"].as_u64().unwrap();
    m.push(json!({"type": "text", "delta": "more"}));
    let r = m.subscribe(Some(("e1", cursor)));
    assert!(r.reset.is_none());
    assert_eq!(r.events.len(), 1);
    assert!(m.kept().2.unwrap() <= 64 * 1024);
    // The partial is the turn so far: the same as rebuilding the live
    // stream.
    let mut rb = Rebuild::default();
    for e in events(&seen)
        .iter()
        .filter(|e| e["seq"].as_u64().unwrap() <= cursor)
    {
        rb.apply(e);
    }
    assert_eq!(
        s.turn["partial"],
        serde_json::to_value(rb.messages()).unwrap()
    );
    // After it ends, resuming from before it is a gap.
    m.done(None, json!({}), 9, None);
    assert_eq!(m.subscribe(Some(("e1", 1))).reset, Some(reason::GAP));
    assert!(m.snapshot().turn.is_null());
}

/// S6 and S25: a replay of the running turn has its prompts as sent; a
/// subscriber whose cursor is past them gets the open ones again.
#[test]
fn open_prompts_are_replayed_and_sent_again() {
    let (m, _) = mirror(instant());
    m.turn_start(&Origin::terminal(), None);
    m.push(json!({"type": "permission", "id": "p1", "action": "edit", "resources": ["a"], "title": "t"}));
    m.push(json!({"type": "question", "id": "q1", "prompts": []}));
    m.push(json!({"type": "prompt_resolved", "id": "q1", "kind": "question", "reply": [["x"]]}));
    let r = m.subscribe(Some(("e1", 0)));
    let kinds: Vec<&str> = r
        .events
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["turn_start", "permission", "question", "prompt_resolved"]
    );
    assert!(r.resend.is_empty(), "the replay has them");
    let r = m.subscribe(Some(("e1", 4)));
    assert_eq!(r.resend.len(), 1);
    assert_eq!(r.resend[0]["id"], "p1");
    assert_eq!(r.resend[0]["seq"], 2, "its own seq");
    let r = m.subscribe(None);
    assert_eq!(r.resend.len(), 1);
    m.idle();
    assert!(m.subscribe(None).resend.is_empty(), "idle: none open");
}

#[test]
fn a_disconnected_mirror_keeps_its_events_and_says_hold_on_connect() {
    let m = Mirror::new("ses_a", "e1", Info::default(), 0, instant());
    m.turn_start(&Origin::terminal(), Some("x"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    m.connect(Some(Arc::new(move |_: &str, o: Out| {
        s.lock().unwrap().push(o)
    })));
    assert!(matches!(
        &seen.lock().unwrap()[0],
        Out::Hold { busy: true, .. }
    ));
    assert_eq!(m.subscribe(Some(("e1", 0))).events.len(), 2);
    m.release();
    assert_eq!(seen.lock().unwrap().last(), Some(&Out::Release));
    m.connect(Some(Arc::new(|_: &str, _| panic!("released"))));
}

/// Stress: events appended from many threads while snapshots and resumes
/// run. Every snapshot's cursor joins its resume exactly, and seqs are
/// given out once each, in the order they are sent.
#[test]
fn snapshots_and_resumes_stay_consistent_under_concurrent_appends() {
    let (m, seen) = mirror(instant());
    m.turn_start(&Origin::terminal(), Some("x"));
    std::thread::scope(|s| {
        for t in 0..6 {
            let m = m.clone();
            s.spawn(move || {
                for i in 0..400 {
                    if i % 50 == 49 && t == 0 {
                        m.done(None, json!({}), i, None);
                        m.idle();
                        m.turn_start(&Origin::terminal(), Some("y"));
                    } else if i % 3 == 0 {
                        m.push(json!({"type": "text", "delta": format!("{t}.{i} ")}));
                    } else {
                        m.push(json!({"type": "notice", "message": format!("{t}.{i}")}));
                    }
                }
            });
        }
        for _ in 0..4 {
            let m = m.clone();
            s.spawn(move || {
                for _ in 0..300 {
                    let snap = m.snapshot();
                    let r = m.subscribe(Some(("e1", snap.events_seq)));
                    if let Some(first) = r.events.first() {
                        let from = first["merged_from"]
                            .as_u64()
                            .unwrap_or(first["seq"].as_u64().unwrap());
                        assert!(from > snap.events_seq);
                    }
                    // No reset for a fresh cursor.
                    assert!(r.reset.is_none(), "{:?}", r.reset);
                }
            });
        }
    });
    let ev = events(&seen);
    let seqs: Vec<u64> = ev.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=ev.len() as u64).collect::<Vec<_>>());
}
