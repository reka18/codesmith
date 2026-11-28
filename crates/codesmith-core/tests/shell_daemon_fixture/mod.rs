//! A fake shell daemon for client tests, after OpenCode's
//! `fixture/persistent-pty-daemon.ts`: it answers over a real socket with
//! whatever a test's handler says, and claims ownership for any `own`.

#![allow(dead_code)]

use codesmith_core::daemon::protocol::{self, Registration};
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Handler = dyn Fn(&UnixStream, &Value, &str) -> Option<Value> + Send + Sync;

pub struct Fake {
    stop: Arc<AtomicBool>,
    pub path: PathBuf,
    conns: Arc<Mutex<Vec<UnixStream>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    /// Stop listening and drop every open connection.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for c in self.conns.lock().unwrap().drain(..) {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn pong() -> Value {
    json!({"type": "pong", "instance_id": "test", "pid": std::process::id(), "protocol": protocol::PROTOCOL, "features": []})
}

pub fn frame(v: &Value) -> Vec<u8> {
    protocol::encode(v).unwrap()
}

fn serve(conn: UnixStream, handler: Arc<Handler>) {
    let mut owner = false;
    let mut r = conn.try_clone().unwrap();
    loop {
        let Ok(Some(body)) = protocol::read_frame(&mut r) else {
            return;
        };
        let env: Value = serde_json::from_slice(&body).unwrap();
        let token = env["token"].as_str().unwrap_or("").to_string();
        let request = env["request"].clone();
        let owning = request["op"] == "own";
        if owning {
            owner = true;
        }
        let response = if owning {
            Some(json!({"type": "owned"}))
        } else {
            handler(&conn, &request, &token)
        };
        if let Some(resp) = response {
            if (&conn).write_all(&frame(&resp)).is_err() {
                return;
            }
        }
        if !owner {
            // Leave a held connection (a wait) open until the test ends it.
            loop {
                let mut b = [0u8; 1];
                use std::io::Read;
                match (&conn).read(&mut b) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        }
    }
}

pub fn listen(
    path: &Path,
    handler: impl Fn(&UnixStream, &Value, &str) -> Option<Value> + Send + Sync + 'static,
) -> Fake {
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let conns: Arc<Mutex<Vec<UnixStream>>> = Arc::default();
    let handler: Arc<Handler> = Arc::new(handler);
    let thread = {
        let (stop, conns) = (stop.clone(), conns.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((c, _)) => {
                        c.set_nonblocking(false).unwrap();
                        conns.lock().unwrap().push(c.try_clone().unwrap());
                        let h = handler.clone();
                        std::thread::spawn(move || serve(c, h));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        })
    };
    Fake {
        stop,
        path: path.to_path_buf(),
        conns,
        thread: Some(thread),
    }
}

pub fn write_registration(dir: &Path, socket: &Path, instance: &str, token: &str) {
    write_raw(
        dir,
        &Registration {
            instance_id: instance.into(),
            pid: std::process::id(),
            protocol: protocol::PROTOCOL,
            socket: socket.display().to_string(),
            token: token.into(),
            features: vec![protocol::FEATURE_SANDBOX.into()],
        },
    );
}

pub fn write_raw(dir: &Path, reg: &Registration) {
    codesmith_core::daemon::fs::write_registration(dir, reg).unwrap();
}

/// A temporary root and its runtime directory `daemon`. Kept short: the
/// socket path must fit the platform's limit.
pub fn runtime() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let t = tempfile::Builder::new()
        .prefix("csd")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().to_path_buf();
    let dir = root.join("daemon");
    std::fs::create_dir(&dir).unwrap();
    (t, root, dir)
}

/// Poll until `ready`, for at most two seconds.
pub fn poll(ready: impl Fn() -> bool) {
    for _ in 0..400 {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(ready(), "condition never held");
}
