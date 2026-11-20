//! Terminals, ported from OpenCode's PTY tests (`pty/*.test.ts` and
//! `persistent-pty-session.test.ts`, MIT), and CodeSmith's own limits on
//! them. Every program started here is trivial and ends on its own or with
//! its terminal (`cat`, `sh -c 'exit 3'`, a `sleep` at most).
#![cfg(unix)]

use codesmith_core::pty::protocol;
use codesmith_core::pty::ticket::{Scope, Tickets};
use codesmith_core::pty::{
    Attachment, CreateInput, Event, Info, Limits, Output, PtyError, Ptys, Status, UpdateInput,
};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const WAIT: Duration = Duration::from_secs(5);

fn ptys() -> Ptys {
    Ptys::new("/tmp", "/bin/sh")
}

fn run(p: &Ptys, command: &str, args: &[&str]) -> Info {
    p.create(CreateInput {
        command: Some(command.into()),
        args: Some(args.iter().map(|a| a.to_string()).collect()),
        cwd: Some("/tmp".into()),
        ..Default::default()
    })
    .unwrap()
}

fn in_session(p: &Ptys, session: &str, title: &str) -> Info {
    p.create(CreateInput {
        command: Some("cat".into()),
        title: Some(title.into()),
        session_id: Some(session.into()),
        ..Default::default()
    })
    .unwrap()
}

/// The kinds of the next `n` events about `id`.
async fn events_for(rx: &mut broadcast::Receiver<Event>, id: &str, n: usize) -> Vec<&'static str> {
    let mut out = Vec::new();
    tokio::time::timeout(WAIT, async {
        while out.len() < n {
            let (kind, of) = match rx.recv().await.unwrap() {
                Event::Created { info } => ("created", info.id),
                Event::Updated { info } => ("updated", info.id),
                Event::Exited { id, .. } => ("exited", id),
                Event::Deleted { id, .. } => ("deleted", id),
            };
            if of == id {
                out.push(kind);
            }
        }
    })
    .await
    .expect("timeout waiting for pty events");
    out
}

/// Output (replay included) until it contains `text`.
async fn wait_output(a: &mut Attachment, seen: &mut String, text: &str) -> String {
    tokio::time::timeout(WAIT, async {
        while !seen.contains(text) {
            match a.recv().await {
                Some(Output::Data(s)) => seen.push_str(&s),
                other => panic!("stream ended before {text:?}: {other:?} after {seen:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout waiting for output containing {text:?}"));
    seen.clone()
}

fn attach(p: &Ptys, id: &str, cursor: Option<i64>) -> (Attachment, String) {
    let a = p.attach(id, cursor).unwrap();
    let seen = a.replay.clone();
    (a, seen)
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_gone(pid: i32) {
    let until = Instant::now() + WAIT;
    while alive(pid) {
        assert!(Instant::now() < until, "process {pid} is still running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// pty/info-schema.test.ts

fn sample(pid: i64) -> serde_json::Value {
    serde_json::json!({
        "id": "pty_01J5Y5H0AH4Q4NXJ6P4C3P5V2K",
        "title": "demo",
        "command": "cmd.exe",
        "args": [],
        "cwd": "C:\\",
        "status": "running",
        "pid": pid,
    })
}

#[test]
fn oc_info_accepts_pid_0() {
    let info: Info = serde_json::from_value(sample(0)).unwrap();
    assert_eq!(info.pid, 0);
}

#[test]
fn oc_info_accepts_a_positive_pid() {
    let info: Info = serde_json::from_value(sample(48012)).unwrap();
    assert_eq!(info.pid, 48012);
}

#[test]
fn oc_info_rejects_a_negative_pid() {
    assert!(serde_json::from_value::<Info>(sample(-1)).is_err());
}

#[test]
fn oc_info_accepts_an_exit_code_for_retained_exited_sessions() {
    let mut v = sample(48012);
    v["status"] = "exited".into();
    v["exitCode"] = 4.into();
    let info: Info = serde_json::from_value(v).unwrap();
    assert_eq!((info.status, info.exit_code), (Status::Exited, Some(4)));
}

// pty/protocol.test.ts

#[test]
fn oc_protocol_drops_invalid_binary_input_frames_and_decodes_valid_ones() {
    assert_eq!(protocol::decode_input(b"ready"), Some("ready"));
    assert_eq!(protocol::decode_input(&[0xff, 0xfe, 0xfd]), None);
    assert_eq!(protocol::decode_input("hello".as_bytes()), Some("hello"));
}

#[test]
fn oc_protocol_encodes_the_cursor_as_a_0x00_prefixed_json_control_frame() {
    let frame = protocol::meta_frame(42);
    assert_eq!(frame[0], 0);
    let v: serde_json::Value = serde_json::from_slice(&frame[1..]).unwrap();
    assert_eq!(v, serde_json::json!({"cursor": 42}));
}

#[test]
fn oc_protocol_splits_replay_into_bounded_frames() {
    assert!(protocol::chunks("").is_empty());
    assert_eq!(protocol::chunks("abc"), vec!["abc"]);
    let big = "x".repeat(protocol::REPLAY_CHUNK + 1);
    let frames = protocol::chunks(&big);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].len(), protocol::REPLAY_CHUNK);
    assert_eq!(frames.concat(), big);
    // Frames are cut on character boundaries.
    let wide = "é".repeat(protocol::REPLAY_CHUNK);
    assert!(protocol::chunks(&wide)
        .iter()
        .all(|f| f.len() <= protocol::REPLAY_CHUNK));
    assert_eq!(protocol::chunks(&wide).concat(), wide);
}

// pty/ticket.test.ts

#[test]
fn oc_ticket_consumes_tickets_once() {
    let t = Tickets::default();
    let scope = Scope::pty("pty_a").directory("/tmp/a");
    let issued = t.issue(scope.clone());
    assert_eq!(issued.expires_in, 60);
    assert!(t.consume(&scope, &issued.ticket));
    assert!(!t.consume(&scope, &issued.ticket));
}

#[test]
fn oc_ticket_rejects_tickets_scoped_to_a_different_request() {
    let t = Tickets::default();
    let issued = t.issue(Scope::pty("pty_a").directory("/tmp/a"));
    assert!(!t.consume(&Scope::pty("pty_a").directory("/tmp/b"), &issued.ticket));
    assert!(t.consume(&Scope::pty("pty_a").directory("/tmp/a"), &issued.ticket));
}

#[test]
fn oc_ticket_rejects_tickets_after_the_ttl_elapses() {
    let t = Tickets::new(Duration::from_millis(5));
    let issued = t.issue(Scope::pty("pty_a"));
    std::thread::sleep(Duration::from_millis(25));
    assert!(!t.consume(&Scope::pty("pty_a"), &issued.ticket));
}

#[test]
fn oc_ticket_rejects_tickets_scoped_to_a_different_workspace() {
    let t = Tickets::default();
    let issued = t.issue(Scope::pty("pty_a").workspace("wrk_1"));
    assert!(!t.consume(&Scope::pty("pty_a").workspace("wrk_2"), &issued.ticket));
    assert!(t.consume(&Scope::pty("pty_a").workspace("wrk_1"), &issued.ticket));
}

#[test]
fn oc_ticket_keeps_pty_and_client_mcp_targets_apart() {
    let t = Tickets::default();
    let pty = t.issue(Scope::pty("pty_a").directory("/tmp/a"));
    let mcp = t.issue(Scope::mcp_client("tui").directory("/tmp/a"));
    assert!(!t.consume(&Scope::mcp_client("tui").directory("/tmp/a"), &pty.ticket));
    assert!(!t.consume(&Scope::mcp_client("web").directory("/tmp/a"), &mcp.ticket));
    assert!(t.consume(&Scope::mcp_client("tui").directory("/tmp/a"), &mcp.ticket));
}

// pty/pty-session.test.ts

#[test]
fn oc_pty_returns_typed_not_found_errors_for_missing_sessions() {
    let p = ptys();
    let id = "pty_missing";
    let missing = PtyError::NotFound(id.into());
    assert_eq!(p.get(id).unwrap_err(), missing);
    assert_eq!(
        p.update(
            id,
            UpdateInput {
                title: Some(Some("missing".into())),
                size: None
            }
        )
        .unwrap_err(),
        missing
    );
    assert_eq!(p.remove(id).unwrap_err(), missing);
    assert_eq!(p.write(id, "input").unwrap_err(), missing);
    assert_eq!(p.attach(id, None).err(), Some(missing));
}

#[tokio::test]
async fn oc_pty_retains_exited_sessions_until_removed() {
    let p = ptys();
    let mut rx = p.events();
    let info = run(&p, "/usr/bin/env", &["sh", "-c", "exit 3"]);
    assert_eq!(
        events_for(&mut rx, &info.id, 2).await,
        ["created", "exited"]
    );
    let exited = p.get(&info.id).unwrap();
    assert_eq!(exited.status, Status::Exited);
    assert_eq!(exited.exit_code, Some(3));

    p.remove(&info.id).unwrap();
    assert_eq!(events_for(&mut rx, &info.id, 1).await, ["deleted"]);
    assert!(p.get(&info.id).is_err());
}

#[tokio::test]
async fn oc_pty_replays_buffered_output_and_streams_live_output_to_attachments() {
    let p = ptys();
    let info = run(&p, "cat", &[]);
    p.write(&info.id, "AAA\n").unwrap();

    let (mut first, mut seen) = attach(&p, &info.id, None);
    assert!(wait_output(&mut first, &mut seen, "AAA")
        .await
        .contains("AAA"));
    first.write("BBB\n").unwrap();
    wait_output(&mut first, &mut seen, "BBB").await;

    // A later attachment replays everything already buffered.
    let replayed = p.attach(&info.id, None).unwrap();
    assert!(replayed.replay.contains("AAA"));
    assert!(replayed.replay.contains("BBB"));
    assert!(replayed.cursor > 0);

    // Tail attachments skip the buffer and only see subsequent output.
    let tail = p.attach(&info.id, Some(-1)).unwrap();
    assert_eq!(tail.replay, "");
    assert_eq!(tail.cursor, replayed.cursor);
    p.remove(&info.id).unwrap();
}

#[tokio::test]
async fn oc_pty_stops_delivering_output_after_detach() {
    let p = ptys();
    let info = run(&p, "cat", &[]);
    let (attached, _) = attach(&p, &info.id, Some(-1));
    // Detaching consumes the attachment, so nothing can reach it after.
    attached.detach();
    p.write(&info.id, "AAA\n").unwrap();
    let (mut verify, mut seen) = attach(&p, &info.id, None);
    wait_output(&mut verify, &mut seen, "AAA").await;
    p.remove(&info.id).unwrap();
}

#[tokio::test]
async fn oc_pty_isolates_output_between_sessions() {
    let p = ptys();
    let a = run(&p, "cat", &[]);
    let b = run(&p, "cat", &[]);
    let (mut attached_a, mut seen) = attach(&p, &a.id, None);
    let (mut attached_b, _) = attach(&p, &b.id, None);
    p.write(&a.id, "AAA\n").unwrap();
    wait_output(&mut attached_a, &mut seen, "AAA").await;
    assert_eq!(attached_b.try_recv(), None);
    p.remove_all();
}

#[tokio::test]
async fn oc_pty_notifies_attachments_with_the_exit_code_and_rejects_attach_after_exit() {
    let p = ptys();
    let mut rx = p.events();
    let info = run(&p, "cat", &[]);
    let (mut attached, _) = attach(&p, &info.id, None);
    p.write(&info.id, "\u{4}").unwrap();
    let end = tokio::time::timeout(WAIT, async {
        loop {
            match attached.recv().await {
                Some(Output::Data(_)) => continue,
                other => return other,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(end, Some(Output::End { exit_code: Some(0) }));
    events_for(&mut rx, &info.id, 2).await;
    assert_eq!(
        p.attach(&info.id, None).err(),
        Some(PtyError::Exited(info.id.clone()))
    );
    p.remove(&info.id).unwrap();
}

#[tokio::test]
async fn oc_pty_defaults_command_login_args_and_cwd_from_shell_selection_and_location() {
    let Some(bash) = ["/bin/bash", "/usr/bin/bash"]
        .into_iter()
        .find(|b| std::path::Path::new(b).exists())
    else {
        return;
    };
    let p = Ptys::new("/tmp", bash);
    let info = p
        .create(CreateInput {
            title: Some("configured".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(info.command, bash);
    assert_eq!(info.args, ["-l"]);
    assert_eq!(info.cwd, "/tmp");
    assert_eq!(info.title, "configured");
    p.remove(&info.id).unwrap();
}

// persistent-pty-session.test.ts

#[tokio::test]
async fn oc_persistent_pty_removes_a_deleted_sessions_terminals() {
    let p = ptys();
    let mut rx = p.events();
    let terminal = in_session(&p, "ses_deleted", "deleted");
    let other = in_session(&p, "ses_kept", "kept");

    p.remove_session("ses_deleted");

    let removed = tokio::time::timeout(WAIT, async {
        loop {
            if let Event::Deleted { id, session_id } = rx.recv().await.unwrap() {
                return (id, session_id);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(removed, (terminal.id, Some("ses_deleted".to_string())));
    assert!(p.list(Some("ses_deleted")).is_empty());
    let left: Vec<String> = p.list(None).into_iter().map(|i| i.id).collect();
    assert_eq!(left, [other.id]);
    p.remove_all();
}

#[tokio::test]
async fn oc_persistent_pty_renames_a_terminal_publishes_the_update_and_clears_the_user_title() {
    let p = ptys();
    let terminal = in_session(&p, "ses_rename", "test");
    assert_eq!(terminal.user_title, None);
    let mut rx = p.events();

    let renamed = p.rename(&terminal.id, Some("build")).unwrap();

    assert_eq!(
        (renamed.title.as_str(), renamed.user_title.as_deref()),
        ("build", Some("build"))
    );
    let published = rx.recv().await.unwrap();
    assert_eq!(
        published,
        Event::Updated {
            info: renamed.clone()
        }
    );
    assert_eq!(renamed.session_id.as_deref(), Some("ses_rename"));
    let cleared = p.rename(&terminal.id, None).unwrap();
    assert_eq!((cleared.title.as_str(), cleared.user_title), ("test", None));
    p.remove_all();
}

// CodeSmith's limits and cleanup.

/// The pid a `sh -c 'sleep 30 & echo bg=$!; ...'` printed.
async fn background_pid(a: &mut Attachment, seen: &mut String) -> i32 {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(at) = seen.find("bg=") {
                let rest = &seen[at + 3..];
                if let Some(end) = rest.find(['\r', '\n']) {
                    return rest[..end].parse().unwrap();
                }
            }
            match a.recv().await {
                Some(Output::Data(s)) => seen.push_str(&s),
                other => panic!("stream ended: {other:?} after {seen:?}"),
            }
        }
    })
    .await
    .expect("timeout waiting for the background pid")
}

#[tokio::test]
async fn removing_a_terminal_kills_its_whole_process_group() {
    let p = ptys();
    let info = run(&p, "sh", &["-c", "sleep 30 & echo bg=$!; wait"]);
    let (mut a, mut seen) = attach(&p, &info.id, None);
    let bg = background_pid(&mut a, &mut seen).await;
    assert!(alive(bg));
    p.remove(&info.id).unwrap();
    assert_eq!(a.recv().await, Some(Output::End { exit_code: None }));
    wait_gone(info.pid as i32).await;
    wait_gone(bg).await;
}

#[tokio::test]
async fn a_program_that_exits_takes_what_it_left_running_with_it() {
    let p = ptys();
    let mut rx = p.events();
    let info = run(&p, "sh", &["-c", "sleep 30 & echo bg=$!; read line"]);
    let (mut a, mut seen) = attach(&p, &info.id, None);
    let bg = background_pid(&mut a, &mut seen).await;
    p.write(&info.id, "\n").unwrap();
    assert_eq!(
        events_for(&mut rx, &info.id, 2).await,
        ["created", "exited"]
    );
    assert_eq!(p.get(&info.id).unwrap().exit_code, Some(0));
    wait_gone(bg).await;
}

#[tokio::test]
async fn dropping_the_terminals_stops_them() {
    let p = ptys();
    let info = run(&p, "cat", &[]);
    drop(p);
    wait_gone(info.pid as i32).await;
}

#[tokio::test]
async fn at_most_the_running_limit_of_terminals_run() {
    let p = Ptys::with_limits(
        "/tmp",
        "/bin/sh",
        Limits {
            running: 2,
            ..Limits::default()
        },
    );
    let a = run(&p, "cat", &[]);
    run(&p, "cat", &[]);
    let third = p.create(CreateInput {
        command: Some("cat".into()),
        ..Default::default()
    });
    assert_eq!(third.err(), Some(PtyError::Limit(2)));
    // A removed terminal frees its place.
    p.remove(&a.id).unwrap();
    run(&p, "cat", &[]);
    p.remove_all();
}

#[tokio::test]
async fn scrollback_keeps_only_the_newest_output() {
    let p = Ptys::with_limits(
        "/tmp",
        "/bin/sh",
        Limits {
            scrollback: 1000,
            ..Limits::default()
        },
    );
    let live = run(&p, "cat", &[]);
    let (mut att, mut seen) = attach(&p, &live.id, None);
    // One line at a time: a terminal's input queue is small.
    for i in 0..30 {
        let line = format!("{i:02}{}", "x".repeat(97));
        let line = if i == 29 { "END".to_string() } else { line };
        p.write(&live.id, &format!("{line}\n")).unwrap();
        // Echoed, then printed by cat.
        while seen.matches(&format!("{line}\r\n")).count() < 2 {
            match tokio::time::timeout(WAIT, att.recv()).await.unwrap() {
                Some(Output::Data(s)) => seen.push_str(&s),
                other => panic!("{other:?}"),
            }
        }
    }
    let total = seen;
    assert!(total.len() > 5000);
    let later = p.attach(&live.id, None).unwrap();
    assert!(later.replay.len() <= 1000, "{}", later.replay.len());
    assert!(later.replay.ends_with("END\r\n"));
    assert!(later.cursor >= total.len() as u64);
    p.remove_all();
}

#[tokio::test]
async fn only_the_newest_exited_terminals_are_kept() {
    let p = Ptys::with_limits(
        "/tmp",
        "/bin/sh",
        Limits {
            exited: 2,
            ..Limits::default()
        },
    );
    let mut rx = p.events();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let info = run(&p, "/usr/bin/env", &["true"]);
        events_for(&mut rx, &info.id, 2).await;
        ids.push(info.id);
    }
    let kept: Vec<String> = p.list(None).into_iter().map(|i| i.id).collect();
    assert_eq!(kept, ids[1..]);
}

#[tokio::test]
async fn an_attachment_too_far_behind_is_dropped() {
    let p = Ptys::with_limits(
        "/tmp",
        "/bin/sh",
        Limits {
            pending: 2000,
            ..Limits::default()
        },
    );
    let info = run(&p, "cat", &[]);
    let (mut slow, _) = attach(&p, &info.id, Some(-1));
    let (mut fast, mut seen) = attach(&p, &info.id, Some(-1));
    // The fast one keeps up line by line; the slow one never reads.
    for i in 0..10 {
        let line = format!("{i}{}", "y".repeat(300));
        p.write(&info.id, &format!("{line}\n")).unwrap();
        while seen.matches(&line).count() < 2 {
            match tokio::time::timeout(WAIT, fast.recv()).await.unwrap() {
                Some(Output::Data(s)) => seen.push_str(&s),
                other => panic!("the fast attachment ended: {other:?}"),
            }
        }
    }
    // The slow one got what fit, then its stream ended.
    let mut got = 0;
    while let Some(Output::Data(s)) = slow.recv().await {
        got += s.len();
    }
    assert!(got <= 2000, "{got}");
    p.remove_all();
}

#[tokio::test]
async fn read_shows_the_sessions_current_terminal_as_text() {
    let p = ptys();
    assert_eq!(p.read("ses_read", None).unwrap(), None);
    let info = in_session(&p, "ses_read", "reader");
    let (mut a, mut seen) = attach(&p, &info.id, None);
    p.write(&info.id, "one\n\x1b[1mtwo\x1b[0m\n").unwrap();
    wait_output(&mut a, &mut seen, "two\u{1b}[0m\r\n").await;
    let r = p.read("ses_read", Some(2)).unwrap().unwrap();
    assert_eq!(r.pty_id, info.id);
    assert_eq!(r.screen.text, "two\n");
    assert!(p.read("ses_read", Some(0)).is_err());
    assert_eq!(p.read("ses_other", None).unwrap(), None);
    p.remove_all();
    assert_eq!(p.read("ses_read", None).unwrap(), None);
}

#[test]
fn a_terminal_needs_an_existing_directory() {
    let p = ptys();
    let r = p.create(CreateInput {
        command: Some("cat".into()),
        cwd: Some("/definitely/not/here".into()),
        ..Default::default()
    });
    assert!(matches!(r, Err(PtyError::InvalidCwd(_))));
}

#[tokio::test]
async fn a_terminal_does_not_see_the_server_token() {
    let p = ptys();
    let info = p
        .create(CreateInput {
            command: Some("sh".into()),
            args: Some(vec!["-c".into(), "read x; env; echo DONE".into()]),
            env: Some([("CODESMITH_SERVER_TOKEN".to_string(), "x".to_string())].into()),
            ..Default::default()
        })
        .unwrap();
    let (mut a, mut seen) = attach(&p, &info.id, None);
    p.write(&info.id, "\n").unwrap();
    let out = wait_output(&mut a, &mut seen, "DONE\r\n").await;
    assert!(out.contains("CODESMITH_TERMINAL=1"), "{out}");
    assert!(!out.contains("CODESMITH_SERVER_TOKEN"), "{out}");
    let _ = p.remove(&info.id);
}
