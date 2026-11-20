//! A scripted HTTP provider for the media generation ports: it records
//! every request and answers with what the test's handler returns, and a
//! catalog of image and video models pointing at it.
#![allow(dead_code)]

use codesmith_core::catalog::{self, ModelsDev};
use codesmith_core::config::Config;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
pub const JPEG: &str = "/9j/4AAQSkZJRgABAQAASABIAAD/2wBDAAEBAQ==";

pub fn png_bytes() -> Vec<u8> {
    codesmith_core::media::unbase64(PNG).unwrap()
}

pub fn jpeg_bytes() -> Vec<u8> {
    codesmith_core::media::unbase64(JPEG).unwrap()
}

/// The start of an MP4: a `ftyp` box, so its type is read from its bytes.
pub fn mp4() -> Vec<u8> {
    let mut v = vec![0, 0, 0, 0x14];
    v.extend_from_slice(b"ftypmp42");
    v.extend_from_slice(&[0, 0, 0, 0, 1, 2, 3, 4]);
    v
}

#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A response: status, content type, body.
pub type Reply = (u16, &'static str, Vec<u8>);

pub fn json_reply(v: Value) -> Reply {
    (200, "application/json", v.to_string().into_bytes())
}

pub type Handler = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

pub struct Server {
    pub url: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Serve `handler` on a local port until the test ends.
pub async fn serve(handler: Handler) -> Server {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let (log, handler) = (log.clone(), handler.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 65536];
                let (head, body) = loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..i]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        break (head, buf[i + 4..i + 4 + len].to_vec());
                    }
                };
                let mut lines = head.lines();
                let first = lines.next().unwrap_or("");
                let mut parts = first.split(' ');
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let headers = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                    .collect();
                let seen = Seen {
                    method,
                    path,
                    headers,
                    body,
                };
                log.lock().unwrap().push(seen.clone());
                let (status, kind, body) = handler(&seen);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });
    Server { url, seen }
}

/// A catalog model (models.dev's v2 source shape) of `kind` with
/// `package`, served at `base`, released on day `released` of 2026.
pub fn model(
    id: &str,
    kind: &str,
    package: &str,
    base: &str,
    input: &[&str],
    released: i64,
) -> Value {
    let api = match kind {
        "image" => "images",
        "video" => "videos",
        _ => "chat-completions",
    };
    let output: Vec<String> = match kind {
        "image" => vec!["image/png".into()],
        "video" => vec!["video/mp4".into()],
        _ => vec!["text".into()],
    };
    let input: Vec<String> = input
        .iter()
        .map(|i| match *i {
            "image" => "image/png".to_string(),
            "video" => "video/mp4".to_string(),
            i => i.to_string(),
        })
        .collect();
    let day = released.clamp(1, 28);
    json!({
        "id": id, "type": kind, "name": id,
        "release_date": format!("2026-01-{day:02}"),
        "modalities": {"input": input, "output": output},
        "capabilities": {"tools": {"supported": false}, "reasoning": {"supported": false}},
        "limit": {"context": 0, "output": 0},
        "api": {api: {"base_url": base, "sdk": {"opencodeai": package}}}
    })
}

/// A catalog provider `id` with `models` (`package` is unused: each model
/// names its own).
pub fn provider(id: &str, _package: &str, models: Vec<Value>) -> (String, Value) {
    let models: serde_json::Map<String, Value> = models
        .into_iter()
        .map(|m| (m["id"].as_str().unwrap().to_string(), m))
        .collect();
    (
        id.to_string(),
        json!({
            "id": id, "name": id,
            "env": [format!("{}_TEST_KEY_UNSET", id.to_uppercase().replace('-', "_"))],
            "models": models
        }),
    )
}

/// Point home, config and data at a scratch folder for the whole test
/// binary, so generated files and saved keys never touch the user's.
pub fn isolate() -> std::path::PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let base = std::env::temp_dir().join(format!("codesmith-media-{}", std::process::id()));
    ONCE.call_once(|| {
        for d in ["home", "config", "data"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::env::set_var("HOME", base.join("home"));
        std::env::set_var("CODESMITH_CONFIG_DIR", base.join("config"));
        std::env::set_var("XDG_DATA_HOME", base.join("data"));
        std::env::set_var("CODESMITH_DISABLE_CLAUDE_CODE", "1");
    });
    base
}

/// A config over `providers` (the catalog), with keys for `keyed`.
pub fn config(providers: Vec<(String, Value)>, keyed: &[&str], extra: Value) -> Config {
    isolate();
    let source: serde_json::Map<String, Value> = providers.into_iter().collect();
    let snaps = catalog::decode_snapshot(&Value::Object(source).to_string()).unwrap();
    let mut c = json!({"snapshots": false, "providers": {}});
    for k in keyed {
        c["providers"][k] = json!({"settings": {"apiKey": format!("{k}-key")}});
    }
    if let Value::Object(e) = extra {
        for (k, v) in e {
            match (c.get_mut(&k), v) {
                (Some(Value::Object(base)), Value::Object(over)) => {
                    for (kk, vv) in over {
                        match (base.get_mut(&kk), vv) {
                            (Some(Value::Object(b2)), Value::Object(o2)) => b2.extend(o2),
                            (_, vv) => {
                                base.insert(kk, vv);
                            }
                        }
                    }
                }
                (_, v) => c[k] = v,
            }
        }
    }
    let mut config: Config = serde_json::from_value(c).unwrap();
    config.models_dev = Some(Arc::new(ModelsDev::fixed(snaps)));
    config
}
