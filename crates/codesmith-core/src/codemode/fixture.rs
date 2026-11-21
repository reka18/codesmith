//! Test fixtures for Code Mode's HTTP globals and tools: a loopback HTTP
//! server that records each request, and a permission asker that records
//! what it is asked. Nothing here reaches the network.

use crate::permission::{Asker, Gate, Reply, Request, Rule};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request the server received.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    /// The request target: path and query.
    pub target: String,
    /// Lower-case names; a repeated header keeps its last value.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    /// The decoded query pairs, in order.
    pub fn query(&self) -> Vec<(String, String)> {
        let u = url::Url::parse(&format!("http://x{}", self.target)).unwrap();
        u.query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    pub fn param(&self, name: &str) -> Option<String> {
        self.query()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// What the server answers.
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Send `content-length` (otherwise the body ends with the connection).
    pub length: bool,
    /// Claim this `content-length` instead of the body's.
    pub claimed: Option<u64>,
}

impl Answer {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Answer {
        let mut headers = vec![];
        if !content_type.is_empty() {
            headers.push(("content-type".into(), content_type.into()));
        }
        Answer {
            status,
            headers,
            body: body.into(),
            length: true,
            claimed: None,
        }
    }

    pub fn json(v: serde_json::Value) -> Answer {
        Answer::new(200, "application/json", v.to_string())
    }

    pub fn redirect(status: u16, location: &str) -> Answer {
        let mut a = Answer::new(status, "", Vec::new());
        a.headers.push(("location".into(), location.into()));
        a
    }
}

pub type Handler = Arc<dyn Fn(&Seen) -> Answer + Send + Sync>;

/// A loopback server answering with `handler` while the test's runtime
/// lives: its base URL (`http://127.0.0.1:<port>`) and the requests seen.
pub async fn serve(
    handler: impl Fn(&Seen) -> Answer + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let handler: Handler = Arc::new(handler);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen_log = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let log = seen_log.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap_or("").split(' ');
                let method = first.next().unwrap_or("").to_string();
                let target = first.next().unwrap_or("").to_string();
                let headers: BTreeMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let want: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = buf[end + 4..].to_vec();
                while body.len() < want {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&tmp[..n]),
                    }
                }
                let seen = Seen {
                    method,
                    target,
                    headers,
                    body,
                };
                log.lock().unwrap().push(seen.clone());
                let a = handler(&seen);
                let mut out = format!("HTTP/1.1 {} X\r\nconnection: close\r\n", a.status);
                for (k, v) in &a.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                if let Some(n) = a.claimed {
                    out.push_str(&format!("content-length: {n}\r\n"));
                } else if a.length {
                    out.push_str(&format!("content-length: {}\r\n", a.body.len()));
                }
                out.push_str("\r\n");
                let _ = s.write_all(out.as_bytes()).await;
                if seen.method != "HEAD" {
                    let _ = s.write_all(&a.body).await;
                }
                let _ = s.shutdown().await;
            });
        }
    });
    (base, log)
}

/// An asker that records each request and answers with `reply`.
pub struct Recorder {
    pub asked: Mutex<Vec<Request>>,
    pub reply: Mutex<Reply>,
}

impl Asker for Recorder {
    fn ask(&self, req: &Request) -> Reply {
        self.asked.lock().unwrap().push(req.clone());
        self.reply.lock().unwrap().clone()
    }
}

/// A gate with `rules` whose questions `reply` answers; and its recorder.
pub fn gate(rules: Vec<Rule>, reply: Reply) -> (Gate, Arc<Recorder>) {
    let r = Arc::new(Recorder {
        asked: Mutex::new(vec![]),
        reply: Mutex::new(reply),
    });
    (Gate::new(rules, r.clone()), r)
}
