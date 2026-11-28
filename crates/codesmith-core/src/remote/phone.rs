//! A minimal client, as a phone app is: its own device key, dialing the
//! machine's node id at given addresses with relays off. The tests use it
//! as the phone, and it shows what a client does on the wire.

use super::http::{self, Response};
use iroh::endpoint::{presets, Connection, ConnectionError};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, SecretKey};
use std::net::SocketAddr;

pub struct Phone {
    pub endpoint: Endpoint,
}

impl Phone {
    /// A phone with a new random device key, on loopback.
    pub async fn new() -> anyhow::Result<Phone> {
        Phone::with_key(SecretKey::generate()).await
    }

    pub async fn with_key(key: SecretKey) -> anyhow::Result<Phone> {
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(key)
            .relay_mode(RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .bind()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Phone { endpoint })
    }

    /// This phone's node id.
    pub fn id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Connect to `node_id` at `addrs` on `alpn`.
    pub async fn connect(
        &self,
        node_id: &str,
        addrs: &[SocketAddr],
        alpn: &[u8],
    ) -> anyhow::Result<Connection> {
        let key: PublicKey = node_id.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut addr = EndpointAddr::new(key);
        for a in addrs {
            addr = addr.with_ip_addr(*a);
        }
        self.endpoint
            .connect(addr, alpn)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// One request on its own stream, and its response.
pub async fn request(
    conn: &Connection,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> anyhow::Result<Response> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&http::request_bytes(method, path, headers, body))
        .await?;
    send.finish()?;
    let bytes = recv.read_to_end(64 << 20).await?;
    Ok(http::parse_response(&bytes)?)
}

/// `POST /v1/pair/claim` with this body.
pub async fn claim(conn: &Connection, body: &serde_json::Value) -> anyhow::Result<Response> {
    request(
        conn,
        "POST",
        "/v1/pair/claim",
        &[],
        &serde_json::to_vec(body)?,
    )
    .await
}

/// The application close code the connection ended with, if it ended
/// that way.
pub async fn close_code(conn: &Connection) -> Option<u64> {
    match conn.closed().await {
        ConnectionError::ApplicationClosed(c) => Some(c.error_code.into_inner()),
        _ => None,
    }
}

/// One thing an event stream sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Sse {
    /// An event: its SSE id (none for one sent again, or a `reset`) and
    /// its JSON.
    Event {
        id: Option<String>,
        data: serde_json::Value,
    },
    /// A comment line (`: ping`).
    Comment(String),
}

/// An open event stream.
pub struct Events {
    recv: iroh::endpoint::RecvStream,
    _send: iroh::endpoint::SendStream,
    buf: Vec<u8>,
    /// The response's status.
    pub status: u16,
    /// The body of a response that isn't a stream.
    pub body: serde_json::Value,
}

/// `GET path` as an event stream, resuming after `last_event_id`.
pub async fn events(
    conn: &Connection,
    path: &str,
    last_event_id: Option<&str>,
) -> anyhow::Result<Events> {
    let (mut send, mut recv) = conn.open_bi().await?;
    let headers: Vec<(&str, &str)> = last_event_id
        .map(|id| vec![("last-event-id", id)])
        .unwrap_or_default();
    send.write_all(&http::request_bytes("GET", path, &headers, b""))
        .await?;
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let mut chunk = [0u8; 4096];
        match recv.read(&mut chunk).await? {
            Some(n) => buf.extend_from_slice(&chunk[..n]),
            None => anyhow::bail!("the stream ended before its head"),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let rest = buf[head_end..].to_vec();
    if !head.contains("text/event-stream") {
        let mut all = rest;
        let mut chunk = [0u8; 4096];
        while let Ok(Some(n)) = recv.read(&mut chunk).await {
            all.extend_from_slice(&chunk[..n]);
        }
        let body = serde_json::from_slice(&all).unwrap_or_default();
        return Ok(Events {
            recv,
            _send: send,
            buf: vec![],
            status,
            body,
        });
    }
    Ok(Events {
        recv,
        _send: send,
        buf: rest,
        status,
        body: serde_json::Value::Null,
    })
}

impl Events {
    /// The next event or comment; `None` when the stream ended.
    pub async fn next(&mut self) -> Option<Sse> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let block = String::from_utf8_lossy(&self.buf[..i]).to_string();
                self.buf.drain(..i + 2);
                let (mut id, mut data) = (None, String::new());
                for l in block.lines() {
                    if let Some(c) = l.strip_prefix(':') {
                        return Some(Sse::Comment(c.trim().to_string()));
                    } else if let Some(v) = l.strip_prefix("id: ") {
                        id = Some(v.to_string());
                    } else if let Some(v) = l.strip_prefix("data: ") {
                        data.push_str(v);
                    }
                }
                return Some(Sse::Event {
                    id,
                    data: serde_json::from_str(&data).unwrap_or_default(),
                });
            }
            let mut chunk = vec![0u8; 64 * 1024];
            match self.recv.read(&mut chunk).await {
                Ok(Some(n)) => self.buf.extend_from_slice(&chunk[..n]),
                _ => return None,
            }
        }
    }

    /// The next event, skipping comments.
    pub async fn event(&mut self) -> Option<(Option<String>, serde_json::Value)> {
        loop {
            match self.next().await? {
                Sse::Event { id, data } => return Some((id, data)),
                Sse::Comment(_) => {}
            }
        }
    }
}
