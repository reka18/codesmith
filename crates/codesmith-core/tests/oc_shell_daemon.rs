//! The shell daemon, ported from OpenCode's process daemon tests
//! (`persistent-pty-daemon.test.ts`, `persistent-pty-session.test.ts`,
//! `shell-daemon.test.ts`, `shell-restart.test.ts`,
//! `shell-retention.test.ts`; MIT).
//!
//! Client tests talk to a fake daemon over a real socket. Daemon tests run
//! the real daemon on a thread of this process, so everything it starts is
//! a child of the test and is gone when the daemon stops; every command is
//! trivial and bounded (`echo`, `printf`, `sleep` of a few seconds at
//! most), and every test stops its daemon before it ends.
#![cfg(unix)]

mod shell_daemon_fixture;

use codesmith_core::daemon::client::{Client, Kind, Launcher, WaitHandle};
use codesmith_core::daemon::protocol::{Handoff, Mode, Registration, Request, Response};
use serde_json::{json, Value};
use shell_daemon_fixture::{frame, listen, poll, pong, runtime, write_raw, write_registration};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn list() -> Request {
    Request::List { mode: Mode::Pipe }
}

fn create() -> Request {
    Request::CreatePipe {
        program: "/bin/sh".into(),
        args: vec![],
        cwd: "/tmp".into(),
        env: Default::default(),
        group_id: "g".into(),
        output_path: "/tmp/x".into(),
        metadata: json!({}),
    }
}

fn op(r: &Value) -> &str {
    r["op"].as_str().unwrap_or("")
}

#[test]
fn oc_rediscovers_a_same_protocol_daemon_after_its_registration_rotates() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let token = Arc::new(Mutex::new(("old-token", "old-instance")));
    let creates = Arc::new(AtomicUsize::new(0));
    let _fake = {
        let (token, creates) = (token.clone(), creates.clone());
        listen(&socket, move |_, req, got| {
            let (want, instance) = *token.lock().unwrap();
            if got != want {
                return Some(json!({"type": "error", "message": "authentication failed"}));
            }
            match op(req) {
                "ping" => {
                    let mut p = pong();
                    p["instance_id"] = json!(instance);
                    Some(p)
                }
                "list" => Some(json!({"type": "processes", "processes": []})),
                "create_pipe" => {
                    creates.fetch_add(1, Ordering::SeqCst);
                    Some(json!({"type": "ok"}))
                }
                _ => Some(json!({"type": "ok"})),
            }
        })
    };
    write_registration(&dir, &socket, "old-instance", "old-token");
    let daemon = Client::new(&root, "daemon", None, None);
    daemon.request(&list(), false).unwrap();

    *token.lock().unwrap() = ("new-token", "new-instance");
    write_registration(&dir, &socket, "new-instance", "new-token");
    assert_eq!(
        daemon.request_if_running(&list()).unwrap(),
        Some(Response::Processes { processes: vec![] })
    );

    *token.lock().unwrap() = ("newest-token", "newest-instance");
    write_registration(&dir, &socket, "newest-instance", "newest-token");
    daemon.request(&create(), true).unwrap();
    assert_eq!(creates.load(Ordering::SeqCst), 1);
}

#[test]
fn oc_rediscovers_a_rotated_registration_when_acquiring_a_subscription() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let token = Arc::new(Mutex::new(("old-token", "old-instance")));
    let subscriptions = Arc::new(AtomicUsize::new(0));
    let _fake = {
        let (token, subscriptions) = (token.clone(), subscriptions.clone());
        listen(&socket, move |_, req, got| {
            let (want, instance) = *token.lock().unwrap();
            if got != want {
                return Some(json!({"type": "error", "message": "authentication failed"}));
            }
            match op(req) {
                "ping" => {
                    let mut p = pong();
                    p["instance_id"] = json!(instance);
                    Some(p)
                }
                "attach" => {
                    subscriptions.fetch_add(1, Ordering::SeqCst);
                    Some(json!({"type": "attached", "replay": "", "cursor": 0}))
                }
                _ => Some(json!({"type": "processes", "processes": []})),
            }
        })
    };
    write_registration(&dir, &socket, "old-instance", "old-token");
    let daemon = Client::new(&root, "daemon", None, None);
    daemon.request(&list(), false).unwrap();

    *token.lock().unwrap() = ("new-token", "new-instance");
    write_registration(&dir, &socket, "new-instance", "new-token");
    let (first, stream) = daemon
        .stream(&Request::Attach {
            id: "pty_1".into(),
            cursor: None,
        })
        .unwrap();
    assert_eq!(
        first,
        Response::Attached {
            replay: String::new(),
            cursor: 0
        }
    );
    assert_eq!(subscriptions.load(Ordering::SeqCst), 1);
    drop(stream);
}

#[test]
fn oc_waits_on_its_own_connection_until_the_process_exits() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let waiting: Arc<Mutex<Vec<UnixStream>>> = Arc::default();
    let _fake = {
        let waiting = waiting.clone();
        listen(&socket, move |conn, req, _| match op(req) {
            "ping" => Some(pong()),
            "wait" if req["id"] == 1 => {
                waiting.lock().unwrap().push(conn.try_clone().unwrap());
                None
            }
            _ => Some(json!({"type": "error", "message": "terminal 2 not found"})),
        })
    };
    write_registration(&dir, &socket, "test", "test");
    let daemon = Arc::new(Client::new(&root, "daemon", None, None));

    let missing = daemon.wait(2, &WaitHandle::default()).unwrap_err();
    assert_eq!(
        (missing.kind, missing.message.as_str()),
        (Kind::Request, "terminal 2 not found")
    );

    let d = daemon.clone();
    let waiter = std::thread::spawn(move || d.wait(1, &WaitHandle::default()));
    poll(|| waiting.lock().unwrap().len() == 1);
    let exited = json!({"type": "exited", "exit_code": null, "signal": "SIGTERM", "final_offset": 3, "output_lines": 1, "exited_at": 7});
    let mut c = waiting.lock().unwrap().pop().unwrap();
    c.write_all(&frame(&exited)).unwrap();
    c.shutdown(std::net::Shutdown::Both).unwrap();
    let got = waiter.join().unwrap().unwrap();
    assert_eq!(serde_json::to_value(Response::Exited(got)).unwrap(), exited);
}

#[tokio::test]
async fn oc_closes_the_wait_connection_when_the_wait_is_interrupted() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let waiting: Arc<Mutex<Vec<UnixStream>>> = Arc::default();
    let _fake = {
        let waiting = waiting.clone();
        listen(&socket, move |conn, req, _| match op(req) {
            "ping" => Some(pong()),
            _ => {
                waiting.lock().unwrap().push(conn.try_clone().unwrap());
                None
            }
        })
    };
    write_registration(&dir, &socket, "test", "test");
    let daemon = Arc::new(Client::new(&root, "daemon", None, None));
    let d = daemon.clone();
    let task = tokio::spawn(async move { d.wait_async(1).await });
    for _ in 0..400 {
        if waiting.lock().unwrap().len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    task.abort();
    let _ = task.await;
    // The daemon's end sees the connection close (macOS refuses socket
    // options on a socket whose peer already closed it).
    let mut c = waiting.lock().unwrap().pop().unwrap();
    if c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .is_ok()
    {
        let mut b = [0u8; 1];
        use std::io::Read;
        assert_eq!(c.read(&mut b).unwrap(), 0);
    }
}

#[test]
fn oc_reports_a_daemon_that_goes_away_during_a_wait_as_unreachable() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let _fake = listen(&socket, |conn, req, _| {
        if op(req) == "ping" {
            return Some(pong());
        }
        let _ = conn.shutdown(std::net::Shutdown::Both);
        None
    });
    write_registration(&dir, &socket, "test", "test");
    let daemon = Client::new(&root, "daemon", None, None);
    assert_eq!(
        daemon.wait(1, &WaitHandle::default()).unwrap_err().kind,
        Kind::Connect
    );
}

#[test]
fn oc_retries_a_start_required_request_when_connection_fails_before_dispatch() {
    let (_t, root, dir) = runtime();
    let (first, second) = (dir.join("1.sock"), dir.join("2.sock"));
    let mut one = listen(&first, |_, req, _| match op(req) {
        "ping" => Some(pong()),
        _ => Some(json!({"type": "processes", "processes": []})),
    });
    write_registration(&dir, &first, "test", "test");
    let daemon = Client::new(&root, "daemon", None, None);
    daemon.request(&list(), false).unwrap();
    one.close();

    let creates = Arc::new(AtomicUsize::new(0));
    let _two = {
        let creates = creates.clone();
        listen(&second, move |_, req, _| match op(req) {
            "ping" => Some(pong()),
            _ => {
                creates.fetch_add(1, Ordering::SeqCst);
                Some(json!({"type": "ok"}))
            }
        })
    };
    write_registration(&dir, &second, "test", "test");
    daemon.request(&create(), true).unwrap();
    assert_eq!(creates.load(Ordering::SeqCst), 1);
}

#[test]
fn oc_does_not_replay_a_dispatched_mutating_request_when_its_response_is_lost() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let creates = Arc::new(AtomicUsize::new(0));
    let _fake = {
        let creates = creates.clone();
        listen(&socket, move |conn, req, _| {
            if op(req) == "ping" {
                return Some(pong());
            }
            creates.fetch_add(1, Ordering::SeqCst);
            let _ = conn.shutdown(std::net::Shutdown::Both);
            None
        })
    };
    write_registration(&dir, &socket, "test", "test");
    let daemon = Client::new(&root, "daemon", None, None);
    let e = daemon.request(&create(), true).unwrap_err();
    assert_eq!(e.kind, Kind::Response);
    assert_eq!(creates.load(Ordering::SeqCst), 1);
}

#[test]
fn oc_rejects_incompatible_daemons_without_replacing_or_killing_them() {
    let (_t, root, dir) = runtime();
    let mut existing = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    write_raw(
        &dir,
        &Registration {
            instance_id: "old".into(),
            pid: existing.id(),
            protocol: 7,
            socket: "/unused".into(),
            token: "old".into(),
            features: vec![],
        },
    );
    let daemon = Client::new(
        &root,
        "daemon",
        Some(Launcher {
            program: "/missing/codesmith".into(),
            args: vec![],
        }),
        None,
    );
    let optional = daemon.request_if_running(&list()).unwrap_err();
    assert_eq!(optional.kind, Kind::Protocol);
    assert_eq!(
        optional.message,
        format!(
            "shell daemon protocol mismatch: daemon=7, client={}",
            codesmith_core::daemon::protocol::PROTOCOL
        )
    );
    assert_eq!(optional.pid, Some(existing.id()));
    let starting = daemon.request(&create(), true).unwrap_err();
    assert_eq!(starting, optional);
    assert!(existing.try_wait().unwrap().is_none());
    existing.kill().unwrap();
    existing.wait().unwrap();
}

#[test]
fn oc_starts_fresh_instead_of_failing_when_an_inherited_registration_is_gone_or_expired() {
    let (_t, root, _dir) = runtime();
    let legacy = root.join("legacy");
    std::fs::create_dir(&legacy).unwrap();
    let now = codesmith_core::daemon::protocol::now_ms();
    for expires_at in [now + 60_000, now - 1] {
        let daemon = Client::new(
            root.join("root"),
            "fresh",
            None,
            Some(Handoff {
                directory: legacy.display().to_string(),
                instance_id: "test".into(),
                ticket: "ticket".into(),
                expires_at,
            }),
        );
        assert_eq!(daemon.handoff().unwrap(), None);
        assert_eq!(daemon.request_if_running(&list()).unwrap(), None);
    }
}

#[test]
fn oc_stops_a_discovered_inherited_daemon_when_adoption_fails() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let _fake = {
        let shutdowns = shutdowns.clone();
        listen(&socket, move |_, req, _| match op(req) {
            "ping" => Some(pong()),
            "shutdown" => {
                shutdowns.fetch_add(1, Ordering::SeqCst);
                Some(json!({"type": "ok"}))
            }
            _ => Some(json!({"type": "ok"})),
        })
    };
    write_registration(&dir, &socket, "test", "test");
    let _daemon = Client::new(
        &root,
        "fresh",
        None,
        Some(Handoff {
            directory: dir.display().to_string(),
            instance_id: "replaced".into(),
            ticket: "ticket".into(),
            expires_at: codesmith_core::daemon::protocol::now_ms() + 60_000,
        }),
    );
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
}

#[test]
fn oc_gives_up_at_once_on_a_daemon_binary_that_speaks_another_protocol_and_never_starts_it_again() {
    let (_t, root, _dir) = runtime();
    let runtime_root = root.join("runtime");
    let spawns = root.join("spawns");
    let binary = root.join("old-codesmith");
    // Registers like an older release would, then idles (bounded).
    std::fs::write(
        &binary,
        format!(
            "#!/bin/sh\necho spawn >> '{}'\nmkdir -p \"$2/$4\"\nchmod 700 \"$2\" \"$2/$4\"\nprintf '{{\"instance_id\":\"old\",\"pid\":%s,\"protocol\":0,\"socket\":\"none\",\"token\":\"t\",\"features\":[]}}' $$ > \"$2/$4/service.json\"\nchmod 600 \"$2/$4/service.json\"\nexec sleep 10\n",
            spawns.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let daemon = Client::new(
        &runtime_root,
        "daemon",
        Some(Launcher {
            program: binary,
            args: vec![],
        }),
        None,
    );
    let started = std::time::Instant::now();
    let first = daemon.request(&list(), true).unwrap_err();
    assert_eq!(first.kind, Kind::Protocol);
    let pid = first.pid.unwrap();
    let second = daemon.request(&list(), true).unwrap_err();
    assert_eq!(second.kind, Kind::Protocol);
    // Well under the 5 s start timeout: the daemon was not waited on again.
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    let count = std::fs::read_to_string(&spawns).unwrap();
    assert_eq!(count.trim().lines().count(), 1);
    // The incompatible daemon it started was stopped.
    poll(|| !codesmith_core::daemon::fs::alive(pid));
}

// --- the real daemon, on a thread of this process ---

use codesmith_core::daemon::server::{self, Options};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A daemon running on a thread, and a client that owns it.
struct Real {
    _t: tempfile::TempDir,
    root: PathBuf,
    dir: PathBuf,
    thread: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    client: Arc<Client>,
}

fn start(
    root: &Path,
    name: &str,
    tweak: impl FnOnce(&mut Options),
) -> std::thread::JoinHandle<std::io::Result<()>> {
    let dir = root.join(name);
    let mut opts = Options::new(&dir);
    tweak(&mut opts);
    let t = std::thread::spawn(move || server::run(opts));
    poll(|| dir.join("service.json").exists());
    t
}

fn real_with(tweak: impl FnOnce(&mut Options)) -> Real {
    let t = tempfile::Builder::new()
        .prefix("csd")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().join("root");
    let thread = start(&root, "d", tweak);
    let client = Arc::new(Client::new(&root, "d", None, None));
    client.request(&list(), false).unwrap();
    assert!(client.owns());
    Real {
        dir: root.join("d"),
        root,
        _t: t,
        thread: Some(thread),
        client,
    }
}

fn real() -> Real {
    real_with(|_| {})
}

impl Real {
    /// Wait for the daemon to stop by itself.
    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            t.join().unwrap().unwrap();
        }
    }
}

impl Drop for Real {
    fn drop(&mut self) {
        let _ = self.client.shutdown();
        self.client.close();
        self.join();
    }
}

fn pipe(cwd: &Path, out: &Path, program: &str, args: &[&str]) -> Request {
    Request::CreatePipe {
        program: program.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: cwd.display().to_string(),
        env: [("TEST".to_string(), "1".to_string())].into(),
        group_id: "proj_test".into(),
        output_path: out.display().to_string(),
        metadata: json!({"id": "sh_test", "command": "sleep"}),
    }
}

fn created(r: Response) -> codesmith_core::daemon::protocol::ProcessInfo {
    match r {
        Response::Created { process } => process,
        other => panic!("not created: {other:?}"),
    }
}

/// persistent-pty-session.test.ts: "creates, inspects, signals and waits for a pipe process"
#[test]
fn oc_creates_inspects_signals_and_waits_for_a_pipe_process() {
    let d = real();
    let out = d.root.join("sh_test.out");
    let p = created(
        d.client
            .request(&pipe(&d.root, &out, "sleep", &["5"]), true)
            .unwrap(),
    );
    assert_eq!(p.group_id, "proj_test");
    assert_eq!(p.output_path, out.display().to_string());
    assert_eq!(
        p.lifecycle,
        codesmith_core::daemon::protocol::Lifecycle::Running
    );
    assert_eq!(p.metadata, json!({"id": "sh_test", "command": "sleep"}));
    assert_eq!(p.exited_at, None);

    let meta = json!({"id": "sh_test", "status": "killed"});
    d.client
        .request(
            &Request::SetMetadata {
                id: p.id,
                metadata: meta.clone(),
            },
            false,
        )
        .unwrap();
    let Response::Process { process } =
        d.client.request(&Request::Get { id: p.id }, false).unwrap()
    else {
        panic!()
    };
    assert_eq!(process.metadata, meta);
    let Response::Processes { processes } = d.client.request(&list(), false).unwrap() else {
        panic!()
    };
    assert_eq!(
        processes.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![p.id]
    );

    let c = d.client.clone();
    let waiting = std::thread::spawn(move || c.wait(p.id, &WaitHandle::default()));
    std::thread::sleep(Duration::from_millis(50));
    d.client
        .request(
            &Request::Signal {
                id: p.id,
                signal: "SIGTERM".into(),
            },
            false,
        )
        .unwrap();
    let exit = waiting.join().unwrap().unwrap();
    assert_eq!(
        (exit.exit_code, exit.signal.as_deref(), exit.output_lines),
        (None, Some("SIGTERM"), 0)
    );
    // A finished process answers a later wait at once and keeps its record
    // until it is terminated.
    let again = d.client.wait(p.id, &WaitHandle::default()).unwrap();
    assert_eq!(again.signal.as_deref(), Some("SIGTERM"));
    let Response::Process { process } =
        d.client.request(&Request::Get { id: p.id }, false).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        process.lifecycle,
        codesmith_core::daemon::protocol::Lifecycle::Exited {
            exit_code: None,
            signal: Some("SIGTERM".into())
        }
    );

    d.client
        .request(&Request::Terminate { id: p.id }, false)
        .unwrap();
    let gone = d
        .client
        .request(&Request::Get { id: p.id }, false)
        .unwrap_err();
    assert_eq!(
        (gone.kind, gone.message),
        (Kind::Request, format!("process {} not found", p.id))
    );
    assert_eq!(
        d.client.request(&list(), false).unwrap(),
        Response::Processes { processes: vec![] }
    );
}

/// persistent-pty-session.test.ts: "keeps pipe processes and terminals apart"
#[test]
fn oc_keeps_pipe_processes_and_terminals_apart() {
    let d = real();
    let term = match d
        .client
        .request(
            &Request::CreatePty {
                input: codesmith_core::daemon::protocol::PtyCreate {
                    command: "cat".into(),
                    cwd: d.root.display().to_string(),
                    title: Some("test".into()),
                    session_id: Some("ses_pipe".into()),
                    ..Default::default()
                },
            },
            true,
        )
        .unwrap()
    {
        Response::Pty { info } => info,
        other => panic!("{other:?}"),
    };
    let p = created(
        d.client
            .request(&pipe(&d.root, &d.root.join("o"), "sleep", &["5"]), true)
            .unwrap(),
    );
    let Response::Ptys { terminals } = d
        .client
        .request(&Request::ListPty { session: None }, false)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        terminals.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
        vec![term.id.clone()]
    );
    let Response::Processes { processes } = d.client.request(&list(), false).unwrap() else {
        panic!()
    };
    assert_eq!(
        processes.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![p.id]
    );
    let e = d
        .client
        .request(
            &Request::GetPty {
                id: format!("pty_{}", p.id),
            },
            false,
        )
        .unwrap_err();
    assert_eq!(e.code.as_deref(), Some("not_found"));
}

/// persistent-pty-session.test.ts: "reports daemon rejections as request errors and a terminated wait as one"
#[test]
fn oc_reports_daemon_rejections_as_request_errors_and_a_terminated_wait_as_one() {
    let d = real();
    let out = d.root.join("o");
    let e = d
        .client
        .request(&pipe(Path::new("relative"), &out, "sleep", &["5"]), true)
        .unwrap_err();
    assert_eq!(
        (e.kind, e.message.as_str()),
        (Kind::Request, "cwd must be absolute")
    );
    assert!(!out.exists(), "a rejected request does not create its file");
    let p = created(
        d.client
            .request(&pipe(&d.root, &out, "sleep", &["5"]), true)
            .unwrap(),
    );
    let c = d.client.clone();
    let waiting = std::thread::spawn(move || c.wait(p.id, &WaitHandle::default()));
    std::thread::sleep(Duration::from_millis(50));
    d.client
        .request(&Request::Terminate { id: p.id }, false)
        .unwrap();
    let e = waiting.join().unwrap().unwrap_err();
    assert_eq!((e.kind, e.message.as_str()), (Kind::Request, "terminated"));
}

/// persistent-pty-session.test.ts: "knows a handoff is prepared until a shutdown or until its ticket expires"
#[test]
fn oc_knows_a_handoff_is_prepared_until_a_shutdown_or_until_its_ticket_expires() {
    let d = real();
    assert!(!d.client.handoff_prepared());
    created(
        d.client
            .request(&pipe(&d.root, &d.root.join("o"), "sleep", &["5"]), true)
            .unwrap(),
    );
    assert!(d.client.handoff().unwrap().is_some());
    assert!(d.client.handoff_prepared());
    // Abandoned by a shutdown.
    d.client.shutdown().unwrap();
    assert!(!d.client.handoff_prepared());
    drop(d);

    // Abandoned by letting the ticket expire.
    let d = real_with(|o| o.handoff_ttl = Duration::from_millis(50));
    assert!(d.client.handoff().unwrap().is_some());
    assert!(d.client.handoff_prepared());
    std::thread::sleep(Duration::from_millis(80));
    assert!(!d.client.handoff_prepared());
    // The owner is still connected, so expiry only cancelled the handoff.
    assert!(d.client.request(&list(), false).is_ok());
}

/// persistent-pty-session.test.ts: "prepares a handoff more than once on the same owner connection"
#[test]
fn oc_prepares_a_handoff_more_than_once_on_the_same_owner_connection() {
    let d = real();
    let first = d.client.handoff().unwrap().unwrap();
    let second = d.client.handoff().unwrap().unwrap();
    assert_eq!(first.ticket, second.ticket);
    assert_eq!(first.expires_at, second.expires_at);
}

// --- CodeSmith's own checks on the daemon's lifecycle and security ---

fn pid_of(p: &codesmith_core::daemon::protocol::ProcessInfo) -> u32 {
    p.pid.unwrap()
}

/// Losing its owner without a handoff stops the daemon and everything it
/// runs; its files go with it.
#[test]
fn daemon_stops_everything_when_its_owner_leaves_without_a_handoff() {
    let mut d = real();
    let p = created(
        d.client
            .request(&pipe(&d.root, &d.root.join("o"), "sleep", &["5"]), true)
            .unwrap(),
    );
    let reg = codesmith_core::daemon::fs::read_registration(&d.dir).unwrap();
    d.client.close();
    d.join();
    assert!(!codesmith_core::daemon::fs::alive(pid_of(&p)));
    assert!(!d.dir.exists());
    assert!(!Path::new(&reg.socket).exists());
}

/// A successor claims the daemon with the ticket while the old owner is
/// still connected; the old owner can then neither hand it over again nor
/// take it down by leaving, the ticket cannot be used twice, and the
/// commands keep running until the new owner leaves.
#[test]
fn daemon_hands_its_processes_over_to_a_successor() {
    let mut d = real();
    let p = created(
        d.client
            .request(&pipe(&d.root, &d.root.join("o"), "sleep", &["5"]), true)
            .unwrap(),
    );
    let h = d.client.handoff().unwrap().unwrap();
    assert_eq!(h.directory, d.dir.display().to_string());
    let next = Client::new(&d.root, "other", None, Some(h.clone()));
    assert!(next.owns());
    assert_eq!(next.directory(), d.dir);
    // The superseded owner cannot hand it over again.
    assert_eq!(d.client.handoff().unwrap_err().kind, Kind::Request);
    // The ticket was consumed: a second successor is refused, and leaves
    // the daemon alone.
    let third = Client::new(&d.root, "third", None, Some(h));
    assert!(!third.owns());
    drop(third);
    // The old owner leaving does not stop it.
    d.client.close();
    std::thread::sleep(Duration::from_millis(100));
    assert!(codesmith_core::daemon::fs::alive(pid_of(&p)));
    let Response::Processes { processes } = next.request(&list(), false).unwrap() else {
        panic!()
    };
    assert_eq!(processes.len(), 1);
    // The new owner leaving without a handoff does.
    next.close();
    d.join();
    assert!(!codesmith_core::daemon::fs::alive(pid_of(&p)));
}

/// A handoff that expires with no owner stops the daemon; so does a daemon
/// no one claims in time.
#[test]
fn daemon_stops_when_unclaimed() {
    let mut d = real_with(|o| o.handoff_ttl = Duration::from_millis(100));
    let p = created(
        d.client
            .request(&pipe(&d.root, &d.root.join("o"), "sleep", &["5"]), true)
            .unwrap(),
    );
    d.client.handoff().unwrap().unwrap();
    d.client.close();
    std::thread::sleep(Duration::from_millis(30));
    assert!(
        codesmith_core::daemon::fs::alive(pid_of(&p)),
        "kept for the successor"
    );
    d.join();
    assert!(!codesmith_core::daemon::fs::alive(pid_of(&p)));

    let t = tempfile::Builder::new()
        .prefix("csd")
        .tempdir_in("/tmp")
        .unwrap();
    let started = std::time::Instant::now();
    let thread = start(t.path(), "d", |o| {
        o.claim_within = Duration::from_millis(100)
    });
    thread.join().unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!t.path().join("d").exists());
}

/// Its files are private, a wrong token is refused, and so is a second
/// owner.
#[test]
fn daemon_files_are_private_and_requests_need_the_token() {
    use std::os::unix::fs::PermissionsExt;
    let d = real();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let reg = codesmith_core::daemon::fs::read_registration(&d.dir).unwrap();
    assert_eq!(mode(&d.dir), 0o700);
    assert_eq!(mode(&d.dir.join("service.json")), 0o600);
    assert_eq!(mode(Path::new(&reg.socket)), 0o600);
    assert_eq!(mode(Path::new(&reg.socket).parent().unwrap()), 0o700);
    assert_eq!(reg.token.len(), 64);

    let mut s = UnixStream::connect(&reg.socket).unwrap();
    let env = json!({"token": "wrong", "request": {"op": "ping"}});
    s.write_all(&frame(&env)).unwrap();
    let body = codesmith_core::daemon::protocol::read_frame(&mut s)
        .unwrap()
        .unwrap();
    assert_eq!(
        codesmith_core::daemon::protocol::decode(&body).unwrap(),
        Response::error("authentication failed")
    );
    assert_eq!(
        codesmith_core::daemon::protocol::read_frame(&mut s).unwrap(),
        None
    );

    // Another client cannot take ownership without a ticket.
    let mut s = UnixStream::connect(&reg.socket).unwrap();
    let env = json!({"token": reg.token, "request": {"op": "own", "instance_id": reg.instance_id}});
    s.write_all(&frame(&env)).unwrap();
    let body = codesmith_core::daemon::protocol::read_frame(&mut s)
        .unwrap()
        .unwrap();
    assert!(matches!(
        codesmith_core::daemon::protocol::decode(&body).unwrap(),
        Response::Error { .. }
    ));

    // An unknown request is answered, not dropped.
    let mut s = UnixStream::connect(&reg.socket).unwrap();
    let env = json!({"token": reg.token, "request": {"op": "fly"}});
    s.write_all(&frame(&env)).unwrap();
    let body = codesmith_core::daemon::protocol::read_frame(&mut s)
        .unwrap()
        .unwrap();
    assert_eq!(
        codesmith_core::daemon::protocol::decode(&body).unwrap(),
        Response::error("unsupported request: fly")
    );
}

/// At most `running` commands run at once, and the records are capped.
#[test]
fn daemon_caps_its_processes() {
    let d = real_with(|o| {
        o.limits.running = 2;
        o.limits.records = 3;
    });
    let out = d.root.join("o");
    for _ in 0..2 {
        created(
            d.client
                .request(&pipe(&d.root, &out, "sleep", &["5"]), true)
                .unwrap(),
        );
    }
    let e = d
        .client
        .request(&pipe(&d.root, &out, "sleep", &["5"]), true)
        .unwrap_err();
    assert_eq!(e.message, "too many commands running (at most 2)");
    // Make room: forget both, then keep three finished records.
    let Response::Processes { processes } = d.client.request(&list(), false).unwrap() else {
        panic!()
    };
    for p in processes {
        d.client
            .request(&Request::Terminate { id: p.id }, false)
            .unwrap();
    }
    for _ in 0..3 {
        let quick = created(
            d.client
                .request(&pipe(&d.root, &out, "true", &[]), true)
                .unwrap(),
        );
        d.client.wait(quick.id, &WaitHandle::default()).unwrap();
    }
    let e = d
        .client
        .request(&pipe(&d.root, &out, "true", &[]), true)
        .unwrap_err();
    assert!(
        e.message.starts_with("too many command records"),
        "{}",
        e.message
    );
}

/// The output goes to the file, lines counted; a failed command's
/// leftover background processes are stopped with it.
#[test]
fn daemon_writes_and_counts_output_and_cleans_up_after_a_failed_command() {
    let d = real();
    let out = d.root.join("o");
    let marker = d.root.join("bg.pid");
    let script = format!(
        "printf 'a\nb\n'; sleep 5 & echo $! > '{}'; exit 3",
        marker.display()
    );
    let p = created(
        d.client
            .request(&pipe(&d.root, &out, "/bin/sh", &["-c", &script]), true)
            .unwrap(),
    );
    let exit = d.client.wait(p.id, &WaitHandle::default()).unwrap();
    assert_eq!(
        (exit.exit_code, exit.output_lines, exit.final_offset),
        (Some(3), 2, 4)
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "a\nb\n");
    let bg: u32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    poll(|| !codesmith_core::daemon::fs::alive(bg));
}
