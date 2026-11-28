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
    let bytes = recv.read_to_end(1 << 20).await?;
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
