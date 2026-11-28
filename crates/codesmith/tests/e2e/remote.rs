//! Remote access through the real binary: `codesmith remote pair` in a
//! pseudo-terminal that is its controlling terminal, a fake phone (its own
//! device key, loopback iroh, relays off) claiming the code from the link
//! it prints, and every answer the confirmation takes. Also: nothing
//! touches the network until you opt in (a loopback stand-in relay that
//! records every connection), and a piped `y` with no terminal refuses.

use crate::harness::{Env, BIN};
use crate::model::{Model, Reply, Script, M};
use codesmith::core::remote::{link::Link, phone};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(30);

/// Remote access set up in `e`'s global config, relays off (or `relay`).
fn remote_config(e: &Env, enabled: bool, relay: &str) {
    e.config(
        "http://127.0.0.1:9/v1",
        json!({"remote": {"enabled": enabled, "relay": relay}}),
    );
}

fn vars(e: &Env) -> Vec<(String, String)> {
    let mut v = e.vars();
    v.push(("CODESMITH_REMOTE_IDLE_MS".into(), "3000".into()));
    v
}

async fn run(e: &Env, args: &[&str]) -> crate::harness::Out {
    let mut c = e.command(args);
    c.env_clear().envs(vars(e));
    crate::harness::finish(c.spawn().unwrap()).await
}

/// A loopback listener standing in for a relay: it records every
/// connection and answers nothing.
struct Recorder {
    url: String,
    hits: Arc<Mutex<usize>>,
}

async fn recorder() -> Recorder {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(0usize));
    let h = hits.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            *h.lock().unwrap() += 1;
            drop(s);
        }
    });
    Recorder { url, hits }
}

/// G1: with nothing opted in, no command contacts the relay or starts the
/// gateway, a session's run included; `codesmith remote start` (an opt-in)
/// does both, which shows the stand-in would have seen it.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_touches_the_network_until_you_opt_in() {
    let e = Env::new();
    let relay = recorder().await;
    let m = Model::start(Script::new().titled("Hi").then(
        "hi",
        M::prompt("hi"),
        Reply::text("Hello."),
    ))
    .await;
    e.config(
        &m.url,
        json!({"remote": {"enabled": false, "relay": relay.url}}),
    );
    for args in [
        vec!["remote", "id"],
        vec!["remote", "status"],
        vec!["remote", "status", "--json"],
        vec!["remote", "devices"],
        vec!["remote", "stop"],
        vec!["run", "hi"],
        vec!["debug", "config"],
    ] {
        let out = run(&e, &args).await;
        assert_ne!(out.code, None, "{args:?}");
    }
    let revoke = run(&e, &["remote", "revoke", "abcd"]).await;
    assert_ne!(revoke.code, Some(0), "no such device");
    assert_eq!(*relay.hits.lock().unwrap(), 0, "the relay was contacted");
    assert!(
        !e.root.join("rm").join("remote.json").exists(),
        "a gateway started"
    );
    assert!(
        !e.data.join("codesmith/remote.db").exists(),
        "remote.db was made"
    );
    // `remote id` made the key only (no network).
    let id = run(&e, &["remote", "id"]).await;
    let id = id.stdout.trim().to_string();
    assert_eq!(id.len(), 64, "{id}");
    // Opting in reaches the relay.
    let out = run(&e, &["remote", "start"]).await;
    assert_eq!(out.code, Some(0), "{}", out.both());
    assert!(out.stdout.contains(&id), "{}", out.stdout);
    let deadline = tokio::time::Instant::now() + WAIT;
    while *relay.hits.lock().unwrap() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the relay was never tried"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // And `start` turned remote access on in the global config.
    let cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(e.config_dir.join("codesmith.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg["remote"]["enabled"], json!(true));
    let st = run(&e, &["remote", "status", "--json"]).await;
    let st: Value = serde_json::from_str(&st.stdout).unwrap();
    assert_eq!(st["running"], json!(true));
    assert_eq!(st["gateway"]["node_id"], json!(id));
    let out = run(&e, &["remote", "stop"]).await;
    assert_eq!(out.code, Some(0), "{}", out.both());
    assert!(out.stdout.contains("stopped"), "{}", out.stdout);
}

/// P10, X3: a piped `y` with no terminal refuses, before anything starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_piped_yes_with_no_terminal_refuses() {
    let e = Env::new();
    let relay = recorder().await;
    remote_config(&e, false, &relay.url);
    let mut c = e.command(&["remote", "pair"]);
    c.env_clear()
        .envs(vars(&e))
        .stdin(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"y\ny\nyes\n").await.unwrap();
    }
    let out = crate::harness::finish(child).await;
    assert_ne!(out.code, Some(0));
    assert!(
        out.stderr.contains("needs you at a terminal"),
        "{}",
        out.both()
    );
    assert_eq!(*relay.hits.lock().unwrap(), 0);
    assert!(!e.root.join("rm").join("remote.json").exists());
    // Remote access wasn't turned on either.
    let cfg = std::fs::read_to_string(e.config_dir.join("codesmith.json")).unwrap();
    let cfg: Value = serde_json::from_str(&cfg).unwrap();
    assert_eq!(cfg["remote"]["enabled"], json!(false));
}

/// `codesmith remote pair` with a pseudo-terminal as its controlling
/// terminal.
struct Pairing {
    master: std::fs::File,
    screen: Arc<Mutex<String>>,
    child: tokio::process::Child,
}

impl Pairing {
    fn start(e: &Env, args: &[&str], extra: &[(&str, &str)]) -> Pairing {
        let (mut m, mut s) = (0, 0);
        let mut ws = libc::winsize {
            ws_row: 60,
            ws_col: 200,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty fills two descriptors this test then owns.
        let r = unsafe {
            libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        assert_eq!(r, 0);
        // SAFETY: fresh descriptors from openpty.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        let mut c = tokio::process::Command::new(BIN);
        c.args(args)
            .current_dir(&e.project)
            .env_clear()
            .envs(vars(e))
            .envs(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let fd = slave.as_raw_fd();
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            c.pre_exec(move || {
                libc::setsid();
                if libc::ioctl(fd, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = c.spawn().unwrap();
        drop(slave);
        let master: std::fs::File = master.into();
        let screen = Arc::new(Mutex::new(String::new()));
        let (sc, mut rd) = (screen.clone(), master.try_clone().unwrap());
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n) = rd.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sc.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Pairing {
            master,
            screen,
            child,
        }
    }

    /// Wait until the screen, from `from` on, contains `text`; the
    /// screen's length then.
    async fn until(&self, from: usize, text: &str) -> usize {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            {
                let s = self.screen.lock().unwrap();
                if let Some(i) = s.get(from..).and_then(|t| t.find(text)) {
                    return from + i + text.len();
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{text:?} never showed; the screen:\n{}",
                self.screen.lock().unwrap().get(from..).unwrap_or("")
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The newest link on the screen after `from`.
    async fn link(&self, from: usize) -> (Link, usize) {
        let at = self.until(from, "Link: codesmith://pair?").await;
        let end = self.until(at, "\n").await;
        let s = self.screen.lock().unwrap();
        let url = format!("codesmith://pair?{}", s[at..end].trim());
        (Link::parse(&url).unwrap(), end)
    }

    fn keys(&mut self, text: &str) {
        use std::io::Write;
        self.master.write_all(text.as_bytes()).unwrap();
    }
}

fn addrs(l: &Link) -> Vec<SocketAddr> {
    // The phone is on this machine and bound to loopback, which reaches
    // the gateway at the port its IPv4 addresses share.
    let a: SocketAddr = l
        .addrs
        .iter()
        .map(|a| a.parse::<SocketAddr>().unwrap())
        .find(|a| a.is_ipv4())
        .expect("an IPv4 direct address in the link");
    vec![SocketAddr::from(([127, 0, 0, 1], a.port()))]
}

/// A phone claims the code in `l`; its response, once `answer` is typed
/// after the confirmation shows (from `from` on the screen).
async fn claim_and_answer(
    p: &mut Pairing,
    phone: &phone::Phone,
    l: &Link,
    from: usize,
    answer: &[&str],
) -> (u16, Value, usize) {
    let conn = phone
        .connect(&l.node_id, &addrs(l), codesmith::core::remote::PAIR_ALPN)
        .await
        .unwrap();
    let body = json!({"code": l.code, "device": {"name": "Test Phone", "platform": "ios"}});
    let claim = tokio::spawn(async move { phone::claim(&conn, &body).await.unwrap() });
    let at = p.until(from, "Check code on the phone: ").await;
    // The confirmation takes keys once it shows "Answer:".
    let mut at = p.until(at, "Answer: ").await;
    for (i, a) in answer.iter().enumerate() {
        if i > 0 {
            at = p.until(at, "Type \"all\" to confirm").await;
        }
        p.keys(a);
    }
    let r = tokio::time::timeout(WAIT, claim).await.unwrap().unwrap();
    (r.status, r.body, at)
}

fn have_pty() -> bool {
    std::path::Path::new("/dev/ptmx").exists()
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_at_a_terminal_refuses_everything_but_yes() {
    if !have_pty() {
        eprintln!("skipped: no pseudo-terminals");
        return;
    }
    let e = Env::new();
    remote_config(&e, false, "none");
    let mut p = Pairing::start(
        &e,
        &[
            "remote",
            "pair",
            "--session",
            "ses_test1",
            "--title",
            "fix the login page",
        ],
        &[("CODESMITH_REMOTE_CONFIRM_MS", "4000")],
    );
    let phone = phone::Phone::new().await.unwrap();
    let (l, at) = p.link(0).await;
    assert_eq!(l.session.as_deref(), Some("ses_test1"));
    assert_eq!(newest_link(&p), l);
    p.keys("c");
    let at = p.until(at, "Link copied").await;
    let mut at = at;
    // n, Enter alone, Esc, `a` then anything but "all", and no answer in
    // time each refuse, and a new code shows.
    for answer in [&["n\r"][..], &["\r"], &["\x1b"], &["a\r", "yes\r"], &[]] {
        let l = newest_link(&p);
        let (status, body, next) = claim_and_answer(&mut p, &phone, &l, at, answer).await;
        assert_eq!(status, 403, "{answer:?}: {body}");
        assert_eq!(body["error"]["code"], "pairing_refused");
        let why = if answer.is_empty() {
            "Not paired: no answer in time"
        } else {
            "Not paired: refused at the terminal"
        };
        at = p.until(next, why).await;
    }
    // The prompt showed the device, its short id and the check code.
    {
        let s = p.screen.lock().unwrap();
        let short = &phone.id()[..10];
        assert!(
            s.contains(&format!(
                "Allow \"Test Phone\" (ios, {short}) to control \"fix the login page\"?"
            )),
            "{s}"
        );
    }
    // y allows, with control of the session.
    let l = newest_link(&p);
    let (status, body, _) = claim_and_answer(&mut p, &phone, &l, at, &["y\r"]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["grants"],
        json!([{"session": "ses_test1", "scope": "control"}])
    );
    let check = codesmith::core::remote::link::check_code(&l.code, &l.node_id, &phone.id());
    assert!(p.screen.lock().unwrap().contains(&check));
    let out = tokio::time::timeout(WAIT, p.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(out.success());
    let devs = run(&e, &["remote", "devices", "--json"]).await;
    let devs: Value = serde_json::from_str(&devs.stdout).unwrap();
    assert_eq!(devs[0]["id"], json!(phone.id()));
    assert_eq!(devs[0]["grants"][0]["scope"], json!("control"));
    // Revoked from the command line, it's gone.
    let out = run(&e, &["remote", "revoke", &phone.id()[..10]]).await;
    assert_eq!(out.code, Some(0), "{}", out.both());
    let devs = run(&e, &["remote", "devices", "--json"]).await;
    assert_eq!(devs.stdout.trim(), "[]");
    let _ = run(&e, &["remote", "stop"]).await;
}

/// The newest link on the screen (a new one shows after each refusal,
/// before its "Not paired" note).
fn newest_link(p: &Pairing) -> Link {
    let s = p.screen.lock().unwrap().clone();
    let i = s.rfind("Link: codesmith://pair?").unwrap();
    let rest = &s[i + "Link: ".len()..];
    let end = rest.find('\n').unwrap();
    Link::parse(rest[..end].trim()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_only_and_all_sessions_with_its_second_confirmation() {
    if !have_pty() {
        eprintln!("skipped: no pseudo-terminals");
        return;
    }
    let e = Env::new();
    remote_config(&e, true, "none");
    let phone = phone::Phone::new().await.unwrap();
    let args = ["remote", "pair", "--session", "ses_w", "--title", "t"];
    let mut p = Pairing::start(&e, &args, &[]);
    let (l, at) = p.link(0).await;
    let (status, body, _) = claim_and_answer(&mut p, &phone, &l, at, &["w\r"]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["grants"],
        json!([{"session": "ses_w", "scope": "watch"}])
    );
    tokio::time::timeout(WAIT, p.child.wait())
        .await
        .unwrap()
        .unwrap();
    // Paired again: it says so, and `a` + "all" adds every session.
    let mut p = Pairing::start(
        &e,
        &["remote", "pair", "--session", "ses_x", "--title", "x"],
        &[],
    );
    let (l, at) = p.link(0).await;
    let (status, body, at) = claim_and_answer(&mut p, &phone, &l, at, &["a\r", "all\r"]).await;
    assert_eq!(status, 200, "{body}");
    let _ = at;
    assert!(p.screen.lock().unwrap().contains("(paired device)"));
    let grants = body["grants"].as_array().unwrap();
    assert!(
        grants.contains(&json!({"session": "*", "scope": "control"})),
        "{body}"
    );
    assert!(
        grants.contains(&json!({"session": "ses_w", "scope": "watch"})),
        "{body}"
    );
    tokio::time::timeout(WAIT, p.child.wait())
        .await
        .unwrap()
        .unwrap();
    let _ = run(&e, &["remote", "stop"]).await;
}
