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
        sandbox: None,
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
        sandbox: None,
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
    // Long enough that a slow CI machine still checks inside the window.
    let mut d = real_with(|o| o.handoff_ttl = Duration::from_secs(2));
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

// --- shell commands on the daemon (`shell-daemon.test.ts`) ---

use codesmith_core::config::ToolOutputConfig;
use codesmith_core::permission::{Effect, Fixed, Gate, Reply, Rule};
use codesmith_core::tool::shell::{Ending, Jobs};
use codesmith_core::tool::{self, Ctx};

struct Project {
    dir: tempfile::TempDir,
    gate: Gate,
    session: String,
}

impl Project {
    fn new() -> Project {
        Project {
            dir: tempfile::tempdir().unwrap(),
            gate: Gate::new(
                vec![Rule::new("*", "*", Effect::Allow)],
                Arc::new(Fixed(Reply::Once)),
            ),
            session: format!("ses_daemon_{}", codesmith_core::paths::random_token(6)),
        }
    }
    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
    fn ctx(&self, jobs: &Jobs) -> Ctx<'_> {
        Ctx {
            cwd: self.root(),
            root: self.root(),
            gate: &self.gate,
            shell: "/bin/sh".into(),
            session_id: self.session.clone(),
            limits: ToolOutputConfig::default(),
            client: reqwest::Client::new(),
            jobs: Some(jobs.clone()),
            websearch: None,
            format: None,
            lsp: None,
            progress: None,
        }
    }
    fn jobs(&self, client: &Arc<Client>) -> Jobs {
        let jobs = Jobs::default();
        jobs.use_daemon(client.clone(), self.root().display().to_string());
        jobs
    }
    async fn shell(&self, jobs: &Jobs, input: Value) -> (String, bool) {
        tool::execute(&self.ctx(jobs), "shell", &input).await
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(codesmith_core::paths::session_output_dir(&self.session));
    }
}

fn processes(c: &Client) -> Vec<codesmith_core::daemon::protocol::ProcessInfo> {
    match c.request_if_running(&list()).unwrap() {
        Some(Response::Processes { processes }) => processes,
        _ => vec![],
    }
}

/// Wait (at most `secs`) until every job has finished.
async fn settle(jobs: &Jobs, secs: u64) {
    tokio::time::timeout(Duration::from_secs(secs), async {
        while jobs.running() > 0 {
            jobs.wait_any().await;
        }
    })
    .await
    .expect("the jobs did not finish");
}

/// shell-daemon.test.ts: "runs a command with the server's environment, an absolute cwd and its metadata"
#[tokio::test(flavor = "multi_thread")]
async fn oc_runs_a_command_with_the_servers_environment_an_absolute_cwd_and_its_metadata() {
    let d = real();
    let p = Project::new();
    std::fs::create_dir(p.root().join("sub")).unwrap();
    let jobs = p.jobs(&d.client);
    let mut env: std::collections::BTreeMap<String, String> = [
        ("PATH", "/usr/bin:/bin"),
        ("FOO", "bar"),
        ("CODESMITH_SERVER_TOKEN", "secret"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    env.insert("HOME".into(), p.root().display().to_string());
    jobs.set_environment(Some(env));
    let command =
        r#"printf '%s|%s|%s|%s' "$FOO" "$AGENT" "${CODESMITH_SERVER_TOKEN:-unset}" "$(pwd)""#;
    let (out, err) = p
        .shell(
            &jobs,
            json!({"command": command, "workdir": "sub", "background": true, "timeout": 60000}),
        )
        .await;
    assert!(!err, "{out}");
    let [proc_] = processes(&d.client).try_into().unwrap();
    // On macOS the shell runs under `sandbox-exec` and its profile.
    let shell = proc_
        .command
        .iter()
        .position(|a| a == "/bin/sh")
        .unwrap_or(0);
    if shell > 0 {
        assert_eq!(proc_.command[0], "/usr/bin/sandbox-exec");
    }
    assert_eq!(
        proc_.command[shell..],
        vec!["/bin/sh".to_string(), "-c".into(), command.into()]
    );
    assert_eq!(proc_.cwd, p.root().join("sub").display().to_string());
    let m = &proc_.metadata;
    assert_eq!(m["id"], "shell_1");
    assert_eq!(m["command"], command);
    assert_eq!(m["shell"], "/bin/sh");
    assert_eq!(m["sessionID"], p.session.as_str());
    assert_eq!(m["directory"], p.root().display().to_string());
    assert_eq!(m["file"], proc_.output_path.as_str());
    assert!(m["timeoutDeadline"].is_u64());
    settle(&jobs, 10).await;
    let [job] = jobs.take_finished().try_into().unwrap();
    assert_eq!(job.ending, Some(Ending::Exited(0)));
    assert_eq!(
        std::fs::read_to_string(&job.file).unwrap(),
        format!("bar|1|unset|{}", p.root().join("sub").display())
    );
    // Reported, so the daemon forgets it.
    poll(|| processes(&d.client).is_empty());
}

/// A daemon-run command loses the credentials CodeSmith runs with, as a
/// local one does (`codesmith_core::child_env`): the well-known provider
/// keys, a provider key variable the config names, any `*_TOKEN`; a name
/// the policy passes through stays, and so does what tools need.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_command_does_not_see_credentials() {
    let d = real();
    let p = Project::new();
    let jobs = p.jobs(&d.client);
    let env: std::collections::BTreeMap<String, String> = [
        ("PATH", "/usr/bin:/bin"),
        ("OPENAI_API_KEY", "sk-fake-openai"),
        ("ANTHROPIC_API_KEY", "sk-fake-anthropic"),
        ("MY_PROVIDER_CRED", "fake-configured"),
        ("SOME_SERVICE_TOKEN", "fake-token"),
        ("GITHUB_TOKEN", "fake-gh"),
        ("FOO", "bar"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    jobs.set_environment(Some(env));
    jobs.set_env_policy(Some(codesmith_core::child_env::Policy {
        known: ["MY_PROVIDER_CRED".to_string()].into(),
        pass: ["GITHUB_TOKEN".to_string()].into(),
    }));
    let command = r#"printf '%s|%s|%s|%s|%s|%s|%s' "${OPENAI_API_KEY-unset}" "${ANTHROPIC_API_KEY-unset}" "${MY_PROVIDER_CRED-unset}" "${SOME_SERVICE_TOKEN-unset}" "${GITHUB_TOKEN-unset}" "$FOO" "$PATH""#;
    let (out, err) = p
        .shell(
            &jobs,
            json!({"command": command, "background": true, "timeout": 60000}),
        )
        .await;
    assert!(!err, "{out}");
    settle(&jobs, 10).await;
    let [job] = jobs.take_finished().try_into().unwrap();
    assert_eq!(
        std::fs::read_to_string(&job.file).unwrap(),
        "unset|unset|unset|unset|fake-gh|bar|/usr/bin:/bin"
    );
    poll(|| processes(&d.client).is_empty());
}

/// shell-daemon.test.ts: "records successful and failed exits without signalling a finished command"
#[tokio::test(flavor = "multi_thread")]
async fn oc_records_successful_and_failed_exits_without_signalling_a_finished_command() {
    let d = real();
    let p = Project::new();
    let jobs = p.jobs(&d.client);
    for code in [0, 3] {
        let (out, err) = p
            .shell(
                &jobs,
                json!({"command": format!("exit {code}"), "background": true}),
            )
            .await;
        assert!(!err, "{out}");
    }
    settle(&jobs, 10).await;
    let mut endings: Vec<_> = jobs
        .take_finished()
        .into_iter()
        .map(|j| (j.id, j.ending))
        .collect();
    endings.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        endings,
        vec![
            ("shell_1".to_string(), Some(Ending::Exited(0))),
            ("shell_2".to_string(), Some(Ending::Exited(3)))
        ]
    );
    // A foreground call's result is the same as from a spawned command.
    let (out, err) = p
        .shell(&jobs, json!({"command": "printf hi; exit 3"}))
        .await;
    assert!(!err);
    assert_eq!(out, "hi\n\nExit code: 3");
}

/// shell-daemon.test.ts: "recreates the output directory removed while the Location is open"
/// and "the spawner recreates the output directory removed while the Location is open"
#[tokio::test(flavor = "multi_thread")]
async fn oc_recreates_the_output_directory_removed_while_the_location_is_open() {
    let d = real();
    let p = Project::new();
    for jobs in [p.jobs(&d.client), Jobs::default()] {
        let (out, _) = p.shell(&jobs, json!({"command": "printf first"})).await;
        assert_eq!(out, "first");
        let _ = std::fs::remove_dir_all(codesmith_core::paths::session_output_dir(&p.session));
        let (out, err) = p.shell(&jobs, json!({"command": "printf second"})).await;
        assert!(!err, "{out}");
        assert_eq!(out, "second");
    }
}

/// shell-daemon.test.ts: "counts lines like the spawner and reads only what reached the file after a write error"
/// (adapted: the result of a long command run by the daemon is the same
/// as the spawner's, line counts and the truncation notice included)
#[tokio::test(flavor = "multi_thread")]
async fn oc_counts_lines_like_the_spawner() {
    let d = real();
    let p = Project::new();
    let command = "i=1; while [ $i -le 3000 ]; do echo \"line $i\"; i=$((i+1)); done";
    let (daemon, _) = p
        .shell(&p.jobs(&d.client), json!({"command": command}))
        .await;
    let (local, _) = p.shell(&Jobs::default(), json!({"command": command})).await;
    let strip = |s: &str| {
        s.lines()
            .filter(|l| !l.contains("full output saved to"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(daemon.contains("line 3000"));
    assert_eq!(strip(&daemon), strip(&local));
    assert!(
        processes(&d.client).is_empty(),
        "a finished call's record is forgotten"
    );
}

/// shell-daemon.test.ts: "fails a command the daemon rejected instead of spawning it"
#[tokio::test(flavor = "multi_thread")]
async fn oc_fails_a_command_the_daemon_rejected_instead_of_spawning_it() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let _fake = listen(&socket, |_, req, _| match op(req) {
        "ping" => Some(pong()),
        "create_pipe" => Some(json!({"type": "error", "message": "output path is a symlink"})),
        _ => Some(json!({"type": "ok"})),
    });
    write_registration(&dir, &socket, "test", "test");
    let client = Arc::new(Client::new(&root, "daemon", None, None));
    let p = Project::new();
    let jobs = p.jobs(&client);
    let marker = p.root().join("ran");
    let (out, err) = p
        .shell(
            &jobs,
            json!({"command": format!("touch '{}'", marker.display())}),
        )
        .await;
    assert!(err);
    assert!(out.contains("output path is a symlink"), "{out}");
    assert!(!marker.exists(), "never spawned instead");
    assert_eq!(jobs.running_or_unreported(), 0);
}

/// A daemon that cannot be reached or started never ran the command, so
/// it is spawned here instead.
#[tokio::test(flavor = "multi_thread")]
async fn shell_spawns_a_command_itself_when_the_daemon_is_unavailable() {
    let (_t, root, _dir) = runtime();
    let client = Arc::new(Client::new(
        &root,
        "none",
        Some(Launcher {
            program: "/missing/codesmith".into(),
            args: vec![],
        }),
        None,
    ));
    let p = Project::new();
    let (out, err) = p
        .shell(&p.jobs(&client), json!({"command": "printf here"}))
        .await;
    assert!(!err, "{out}");
    assert_eq!(out, "here");
}

/// shell-daemon.test.ts: "reports a command lost with the daemon as killed, with what it wrote"
#[tokio::test(flavor = "multi_thread")]
async fn oc_reports_a_command_lost_with_the_daemon_as_killed_with_what_it_wrote() {
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let _fake = listen(&socket, |conn, req, _| match op(req) {
        "ping" => Some(pong()),
        "create_pipe" => {
            std::fs::write(req["output_path"].as_str().unwrap(), "partial\n").unwrap();
            Some(json!({"type": "created", "process": {
                "id": 1, "pid": 4_000_000, "command": ["/bin/sh"], "cwd": req["cwd"], "group_id": "g",
                "lifecycle": {"status": "running"}, "output_tail": 8, "output_lines": 1,
                "output_path": req["output_path"], "metadata": req["metadata"], "started_at": 1
            }}))
        }
        // The daemon goes away under the waiting server.
        "wait" => {
            let _ = conn.shutdown(std::net::Shutdown::Both);
            None
        }
        _ => Some(json!({"type": "ok"})),
    });
    write_registration(&dir, &socket, "test", "test");
    let client = Arc::new(Client::new(&root, "daemon", None, None));
    let p = Project::new();
    let jobs = p.jobs(&client);
    let (out, err) = p
        .shell(&jobs, json!({"command": "work", "background": true}))
        .await;
    assert!(!err, "{out}");
    settle(&jobs, 10).await;
    let [job] = jobs.take_finished().try_into().unwrap();
    assert_eq!(job.ending, Some(Ending::Lost));
    let note = codesmith_core::tool::shell::notification(&job, ToolOutputConfig::default());
    assert!(note.contains("partial"), "{note}");
    assert!(
        note.contains("Command was killed: the process daemon stopped"),
        "{note}"
    );
}

/// shell-daemon.test.ts: "stops a timed-out command with SIGTERM, then SIGKILL after the grace period, and keeps it"
#[tokio::test(flavor = "multi_thread")]
async fn oc_stops_a_timed_out_command_with_sigterm_then_sigkill_after_the_grace_period_and_keeps_it(
) {
    let d = real();
    let p = Project::new();
    let jobs = p.jobs(&d.client);
    let started = std::time::Instant::now();
    let (out, err) = p
        .shell(
            &jobs,
            json!({"command": "trap '' TERM; sleep 6", "background": true, "timeout": 100}),
        )
        .await;
    assert!(!err, "{out}");
    settle(&jobs, 10).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(3),
        "SIGKILL only after the grace period"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "but well before it ends by itself"
    );
    // The daemon knows why it was stopped, for a server taking it back.
    let [proc_] = processes(&d.client).try_into().unwrap();
    assert_eq!(proc_.metadata["stopReason"], "timeout");
    assert!(proc_.metadata.get("timeoutDeadline").is_none());
    let [job] = jobs.finished().try_into().unwrap();
    assert_eq!(
        job.ending,
        Some(Ending::TimedOut(Some(Box::new(Ending::Signaled(9)))))
    );
    assert!(job.file.exists(), "its output file is kept");
}

/// shell-daemon.test.ts: "takes its commands back after a restart and leaves other Locations' alone"
/// (and shell-restart.test.ts's foreground call, adapted: it is taken back
/// as a background job of its session, without its call's timeout)
#[test]
fn oc_takes_its_commands_back_after_a_restart_and_leaves_other_locations_alone() {
    let mut d = real();
    let p = Project::new();
    // Server 1, on a runtime of its own that the restart tears down.
    let one = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let jobs = p.jobs(&d.client);
    let (finished_at, waited) = one.block_on(async {
        let bg = |command: &str, timeout: u64| {
            json!({"command": command, "background": true, "timeout": timeout})
        };
        p.shell(&jobs, bg("sleep 5", 0)).await; // shell_1: running
        p.shell(&jobs, bg("printf 'done\\n'; exit 3", 0)).await; // shell_2: finished
        tokio::time::timeout(Duration::from_secs(5), async {
            while jobs.running() > 1 {
                jobs.wait_any().await;
            }
        })
        .await
        .unwrap();
        p.shell(&jobs, bg("sleep 5", 60_000)).await; // shell_3: expiring
        p.shell(&jobs, bg("trap '' TERM; echo ready; sleep 5", 0)).await; // shell_4: stopping
        let file = jobs.running_jobs().into_iter().find(|j| j.id == "shell_4").unwrap().file;
        for _ in 0..400 {
            if std::fs::read_to_string(&file).is_ok_and(|s| s.contains("ready")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(jobs.cancel("shell_4"));
        // shell_5: a foreground call waits on it.
        let (ctx_jobs, root, session) = (jobs.clone(), p.root(), p.session.clone());
        tokio::spawn(async move {
            let gate = Gate::new(vec![Rule::new("*", "*", Effect::Allow)], Arc::new(Fixed(Reply::Once)));
            let ctx = Ctx {
                cwd: root.clone(),
                root,
                gate: &gate,
                shell: "/bin/sh".into(),
                session_id: session,
                limits: ToolOutputConfig::default(),
                client: reqwest::Client::new(),
                jobs: Some(ctx_jobs),
                websearch: None,
                format: None,
                lsp: None,
                progress: None,
            };
            let _ = tool::execute(&ctx, "shell", &json!({"command": "sleep 5", "timeout": 60000})).await;
        });
        for _ in 0..400 {
            if !jobs.foreground().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        (jobs.finished().len(), jobs.foreground())
    });
    assert_eq!(finished_at, 1);
    assert_eq!(waited.len(), 1);
    // A graceful restart: server 1 hands its daemon over, then goes away.
    let h = d.client.handoff().unwrap().unwrap();
    one.shutdown_timeout(Duration::from_millis(100));
    drop(jobs);
    d.client.close();
    let next = Arc::new(Client::new(&d.root, "next", None, Some(h)));
    assert!(next.owns());
    // Nothing was stopped: everything but the finished one still runs.
    let before = processes(&next);
    assert_eq!(before.len(), 5);
    let running = |id: &str| {
        before
            .iter()
            .find(|p| p.metadata["id"] == id)
            .is_some_and(|p| p.lifecycle == codesmith_core::daemon::protocol::Lifecycle::Running)
    };
    assert!(["shell_1", "shell_3", "shell_4", "shell_5"]
        .iter()
        .all(|id| running(id)));
    // The expiring command's deadline passes while no server runs.
    let expiring = before
        .iter()
        .find(|p| p.metadata["id"] == "shell_3")
        .unwrap();
    let mut meta = expiring.metadata.clone();
    meta["timeoutDeadline"] = json!(codesmith_core::daemon::protocol::now_ms() - 1);
    next.request(
        &Request::SetMetadata {
            id: expiring.id,
            metadata: meta,
        },
        false,
    )
    .unwrap();

    let two = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    two.block_on(async {
        // Another directory's server takes nothing.
        let other = Project::new();
        let theirs = Jobs::default();
        theirs.use_daemon(next.clone(), other.root().display().to_string());
        assert_eq!(theirs.adopt(&p.session).await, 0);

        let jobs = p.jobs(&next);
        assert_eq!(jobs.adopt(&p.session).await, 5);
        // Taking back twice takes nothing more.
        assert_eq!(jobs.adopt(&p.session).await, 0);
        // The finished one is reported as it ended; the interrupted stop
        // goes straight to SIGKILL; the expired one is stopped as timed out
        // (its SIGTERM is enough).
        tokio::time::timeout(Duration::from_secs(4), async {
            while jobs.running() > 2 {
                jobs.wait_any().await;
            }
        })
        .await
        .expect("the stopped commands end");
        let mut ended: Vec<_> = jobs
            .take_finished()
            .into_iter()
            .map(|j| (j.id, j.ending, j.cancelled, j.file))
            .collect();
        ended.sort_by(|a, b| a.0.cmp(&b.0));
        assert!(std::fs::read_to_string(&ended[0].3)
            .unwrap()
            .starts_with("done"));
        let ended: Vec<_> = ended.into_iter().map(|(a, b, c, _)| (a, b, c)).collect();
        assert_eq!(
            ended,
            vec![
                ("shell_2".to_string(), Some(Ending::Exited(3)), false),
                (
                    "shell_3".to_string(),
                    Some(Ending::TimedOut(Some(Box::new(Ending::Signaled(15))))),
                    false
                ),
                ("shell_4".to_string(), Some(Ending::Signaled(9)), true),
            ]
        );
        // The waited-on command runs on, now without a timeout.
        let ids: Vec<String> = jobs.running_jobs().into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec!["shell_1".to_string(), "shell_5".to_string()]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let five = processes(&next)
            .into_iter()
            .find(|p| p.metadata["id"] == "shell_5")
            .unwrap();
        assert!(five.metadata.get("timeoutDeadline").is_none());
        assert!(five.metadata.get("waited").is_none());
        // New commands get new IDs.
        let (out, _) = p
            .shell(&jobs, json!({"command": "exit 0", "background": true}))
            .await;
        assert!(out.contains("shell_6"), "{out}");
        next.shutdown().unwrap();
    });
    two.shutdown_timeout(Duration::from_millis(100));
    d.join();
}

/// Terminals made through a daemon's `Ptys` live in the daemon: their
/// events reach the server, and after a handoff the next server lists
/// them and replays their output.
#[tokio::test(flavor = "multi_thread")]
async fn terminals_in_the_daemon_survive_a_handoff() {
    use codesmith_core::pty::{CreateInput, Event, Output, Ptys};
    let mut d = real();
    let ptys = Ptys::remote(d.client.clone(), d.root.clone(), "/bin/sh");
    let info = ptys
        .create(CreateInput {
            command: Some("cat".into()),
            title: Some("kept".into()),
            session_id: Some("ses_t".into()),
            ..Default::default()
        })
        .unwrap();
    let mut events = ptys.events();
    let mut att = ptys.attach(&info.id, None).unwrap();
    ptys.write(&info.id, "hello\n").unwrap();
    let mut seen = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !seen.contains("hello") {
            match att.recv().await {
                Some(Output::Data(s)) => seen.push_str(&s),
                other => panic!("{other:?}"),
            }
        }
    })
    .await
    .unwrap();
    let renamed = ptys.rename(&info.id, Some("build")).unwrap();
    assert_eq!(renamed.title, "build");
    let updated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Event::Updated { info }) = events.recv().await {
                return info;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(updated.title, "build");
    drop(att);

    let h = d.client.handoff().unwrap().unwrap();
    d.client.close();
    let next = Arc::new(Client::new(&d.root, "next", None, Some(h)));
    let after = Ptys::remote(next.clone(), d.root.clone(), "/bin/sh");
    let listed = after.list(Some("ses_t"));
    assert_eq!(
        listed.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
        vec![info.id.clone()]
    );
    let att = after.attach(&info.id, None).unwrap();
    assert!(att.replay.contains("hello"), "{:?}", att.replay);
    drop(att);
    after.remove(&info.id).unwrap();
    assert!(after.list(None).is_empty());
    next.close();
    d.join();
}

/// shell-daemon.test.ts: "forgets a command in the daemon before deleting its file, on remove and on eviction"
/// (adapted: CodeSmith keeps no shell records to remove or evict; the
/// daemon forgets a command once its result is reported, foreground or
/// background, and never deletes its output file)
#[tokio::test(flavor = "multi_thread")]
async fn oc_forgets_a_command_in_the_daemon_once_it_is_reported() {
    let d = real();
    let p = Project::new();
    let jobs = p.jobs(&d.client);
    p.shell(&jobs, json!({"command": "printf bg", "background": true}))
        .await;
    settle(&jobs, 10).await;
    // Finished but not yet reported: the next server would need it.
    assert_eq!(processes(&d.client).len(), 1);
    let [job] = jobs.take_finished().try_into().unwrap();
    poll(|| processes(&d.client).is_empty());
    assert!(job.file.exists());
    // A long foreground result keeps its file, and is forgotten too.
    let (out, _) = p
        .shell(
            &jobs,
            json!({"command": "i=0; while [ $i -lt 3000 ]; do echo $i; i=$((i+1)); done"}),
        )
        .await;
    assert!(out.contains("full output saved to"), "{out}");
    poll(|| processes(&d.client).is_empty());
}

/// Clearing a terminal's title (`null`) reaches the daemon as a clear,
/// not as "unchanged".
#[test]
fn a_cleared_title_crosses_the_wire() {
    let r = Request::UpdatePty {
        id: "pty_1".into(),
        title: Some(None),
        size: None,
    };
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v, json!({"op": "update_pty", "id": "pty_1", "title": null}));
    assert_eq!(serde_json::from_value::<Request>(v).unwrap(), r);
    let unchanged: Request =
        serde_json::from_value(json!({"op": "update_pty", "id": "pty_1"})).unwrap();
    assert_eq!(
        unchanged,
        Request::UpdatePty {
            id: "pty_1".into(),
            title: None,
            size: None
        }
    );
}

/// A command the daemon starts leads a session of its own (and the
/// process group a stop signals) with no controlling terminal, even when
/// the daemon has one (here it runs in the test process), and gets the
/// non-interactive defaults unless its environment sets them.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_command_has_no_terminal_and_does_not_prompt() {
    let d = real();
    let p = Project::new();
    let jobs = p.jobs(&d.client);
    let (out, err) = p
        .shell(
            &jobs,
            json!({"command": "echo pid=$$; ps -o pgid=,stat=,tty= -p $$", "description": "x"}),
        )
        .await;
    assert!(!err, "{out}");
    let mut lines = out.lines();
    let pid = lines.next().unwrap().strip_prefix("pid=").unwrap();
    let fields: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    let [pgid, stat, tty] = fields[..] else {
        panic!("{out}");
    };
    assert_eq!(pgid, pid, "its own group: {out}");
    assert!(stat.contains('s'), "a session leader: {out}");
    assert!(tty.chars().all(|c| c == '?'), "no terminal: {out}");

    let reads = "if read x < /dev/tty; then echo got-\"\"tty; else echo tty-refused; fi";
    let (out, err) = p
        .shell(
            &jobs,
            json!({"command": reads, "description": "x", "timeout": 5000}),
        )
        .await;
    assert!(!err && out.contains("tty-refused"), "{out}");

    let show = r#"printf '%s;' "$GIT_TERMINAL_PROMPT" "$SSH_ASKPASS_REQUIRE" "$GCM_INTERACTIVE" "$DEBIAN_FRONTEND""#;
    let base: std::collections::BTreeMap<String, String> =
        [("PATH".to_string(), "/usr/bin:/bin".to_string())].into();
    jobs.set_environment(Some(base.clone()));
    let (out, _) = p
        .shell(&jobs, json!({"command": show, "description": "x"}))
        .await;
    assert_eq!(out, "0;never;never;noninteractive;");
    let mut mine = base;
    mine.insert("GIT_TERMINAL_PROMPT".into(), "1".into());
    mine.insert("DEBIAN_FRONTEND".into(), "readline".into());
    jobs.set_environment(Some(mine));
    let (out, _) = p
        .shell(&jobs, json!({"command": show, "description": "x"}))
        .await;
    assert_eq!(out, "1;never;never;readline;", "the user's own values win");
}

/// A stop through the daemon still reaches everything the command
/// started, now that the command leads its own session: a `&` job and a
/// grandchild in a subshell, both ignoring SIGTERM, so SIGKILL is sent to
/// the group after the grace period.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_stop_still_kills_the_whole_group() {
    let d = real();
    let p = Project::new();
    let jobs = p.jobs(&d.client);
    let (job, nested) = (p.root().join("job"), p.root().join("nested"));
    let command = format!(
        "trap '' TERM; sleep 30 & echo $! > '{}'; \
         sh -c 'trap \"\" TERM; sleep 30 & echo $! > \"{}\"; wait' & wait",
        job.display(),
        nested.display()
    );
    let (out, err) = p
        .shell(
            &jobs,
            json!({"command": command, "background": true, "timeout": 300}),
        )
        .await;
    assert!(!err, "{out}");
    let pid_in = |f: &Path| {
        let read = || {
            std::fs::read_to_string(f)
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        poll(|| !read().is_empty());
        read()
    };
    let pids = [pid_in(&job), pid_in(&nested)];
    settle(&jobs, 15).await;
    let alive = |pid: &str| {
        std::process::Command::new("/bin/kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    // The group is gone by the time the leader is reported; allow a moment
    // for the kernel to reap the last members.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while pids.iter().any(|p| alive(p)) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survivors: Vec<&String> = pids.iter().filter(|p| alive(p)).collect();
    for p in &survivors {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-9", p])
            .status();
    }
    assert!(survivors.is_empty(), "survived the stop: {survivors:?}");
}

/// A command the daemon runs is sandboxed as a local one is: on macOS it
/// runs under `sandbox-exec`, on Linux the daemon applies the policy in
/// the child. It writes in the project and not outside, foreground and
/// background. Where the machine can't sandbox, it is refused before it
/// reaches the daemon.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_command_is_sandboxed() {
    use codesmith_core::sandbox::{support, Support};
    let d = real();
    let p = Project::new();
    let jobs = p.jobs(&d.client);
    let out = tempfile::tempdir().unwrap();
    let out = out.path().canonicalize().unwrap();
    std::fs::write(p.root().join("p"), out.display().to_string()).unwrap();
    let command = r#"echo in > inside; echo pwn > "$(cat p)/f"; true"#;
    let (text, err) = p.shell(&jobs, json!({"command": command})).await;
    if support() != Support::Yes {
        assert!(err, "{text}");
        assert!(text.contains(r#""unsupported": "run""#), "{text}");
        return;
    }
    assert!(!err, "{text}");
    assert!(p.root().join("inside").exists(), "{text}");
    assert!(!out.join("f").exists(), "{text}");
    let (text, err) = p
        .shell(
            &jobs,
            json!({"command": r#"echo pwn > "$(cat p)/g"; echo bg > bg"#, "background": true}),
        )
        .await;
    assert!(!err, "{text}");
    settle(&jobs, 20).await;
    assert!(p.root().join("bg").exists());
    assert!(!out.join("g").exists());
    let _ = jobs.take_finished();
}

/// A daemon that doesn't register the sandbox feature (one from before
/// it) would run a command without the sandbox on Linux, where the daemon
/// applies it: the command runs here, sandboxed, instead. On macOS the
/// daemon runs `sandbox-exec`, which any daemon can, so it is still used.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_without_the_sandbox_feature_is_passed_over_on_linux() {
    use codesmith_core::sandbox::{support, Support};
    if support() != Support::Yes {
        return;
    }
    let (_t, root, dir) = runtime();
    let socket = dir.join("d.sock");
    let creates = Arc::new(AtomicUsize::new(0));
    let _fake = {
        let creates = creates.clone();
        listen(&socket, move |_, req, _| match op(req) {
            "ping" => Some(pong()),
            "create_pipe" => {
                creates.fetch_add(1, Ordering::SeqCst);
                Some(json!({"type": "error", "message": "old daemon"}))
            }
            _ => Some(json!({"type": "ok"})),
        })
    };
    write_raw(
        &dir,
        &Registration {
            instance_id: "test".into(),
            pid: std::process::id(),
            protocol: codesmith_core::daemon::protocol::PROTOCOL,
            socket: socket.display().to_string(),
            token: "test".into(),
            features: vec![],
        },
    );
    let client = Arc::new(Client::new(&root, "daemon", None, None));
    let p = Project::new();
    let jobs = p.jobs(&client);
    let (out, err) = p.shell(&jobs, json!({"command": "printf here"})).await;
    if cfg!(target_os = "linux") {
        assert!(!err, "{out}");
        assert_eq!(out, "here");
        assert_eq!(creates.load(Ordering::SeqCst), 0);
    } else {
        assert!(err, "{out}");
        assert_eq!(creates.load(Ordering::SeqCst), 1);
    }
}
