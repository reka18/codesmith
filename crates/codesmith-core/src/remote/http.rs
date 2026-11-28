//! One HTTP/1.1 request and its response per bidirectional stream (I8).
//! The connection is the authentication: there's no `Authorization`
//! header, `Host` is ignored, and a request with an `Origin` header is
//! refused by the routes.

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The most a request's head may be.
pub const MAX_HEAD: usize = 16 * 1024;
/// The most a request's body may be.
pub const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// The path, without the query.
    pub path: String,
    /// The query, without the `?` (empty when there's none).
    pub query: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// A query parameter's value (no percent-decoding: the parameters
    /// the routes take are numbers).
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a request couldn't be read; each but `Closed` is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The stream ended before a whole request.
    Closed,
    /// 400.
    Bad(&'static str),
    /// 413.
    TooLarge,
}

/// Read one request from `r`.
pub async fn read_request<R: AsyncRead + Unpin>(r: &mut R) -> Result<Request, ReadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err(ReadError::TooLarge);
        }
        let n = r.read(&mut chunk).await.map_err(|_| ReadError::Closed)?;
        if n == 0 {
            return Err(if buf.is_empty() {
                ReadError::Closed
            } else {
                ReadError::Bad("the request ended early")
            });
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if head_end > MAX_HEAD {
        return Err(ReadError::TooLarge);
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(&buf[..head_end]) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(ReadError::Bad("not an HTTP request")),
    }
    if req.version != Some(1) {
        return Err(ReadError::Bad("only HTTP/1.1"));
    }
    let method = req.method.unwrap_or("").to_string();
    let target = req.path.unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    if !path.starts_with('/') {
        return Err(ReadError::Bad("a request target must be a path"));
    }
    let mut out_headers = Vec::new();
    for h in req.headers.iter() {
        let v = std::str::from_utf8(h.value).map_err(|_| ReadError::Bad("a header isn't UTF-8"))?;
        out_headers.push((h.name.to_ascii_lowercase(), v.trim().to_string()));
    }
    let request = Request {
        method,
        path,
        query,
        headers: out_headers,
        body: vec![],
    };
    if request.header("transfer-encoding").is_some() {
        return Err(ReadError::Bad(
            "send a Content-Length, not a transfer encoding",
        ));
    }
    let lengths: Vec<&str> = request
        .headers
        .iter()
        .filter(|(n, _)| n == "content-length")
        .map(|(_, v)| v.as_str())
        .collect();
    let len = match lengths.as_slice() {
        [] => 0,
        [one] => one
            .parse::<usize>()
            .map_err(|_| ReadError::Bad("a bad Content-Length"))?,
        _ => return Err(ReadError::Bad("more than one Content-Length")),
    };
    if len > MAX_BODY {
        return Err(ReadError::TooLarge);
    }
    let mut body = buf[head_end..].to_vec();
    if body.len() > len {
        return Err(ReadError::Bad("more body than its Content-Length"));
    }
    while body.len() < len {
        let n = r.read(&mut chunk).await.map_err(|_| ReadError::Closed)?;
        if n == 0 {
            return Err(ReadError::Bad("the body ended early"));
        }
        body.extend_from_slice(&chunk[..n]);
        if body.len() > len {
            return Err(ReadError::Bad("more body than its Content-Length"));
        }
    }
    Ok(Request { body, ..request })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A response with a JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Value,
}

impl Response {
    pub fn ok(body: Value) -> Response {
        Response { status: 200, body }
    }

    /// An error in the `/v1` shape (E1): `{"error": {"code", "message"}}`.
    pub fn error(status: u16, code: &str, message: &str) -> Response {
        Response {
            status,
            body: json!({"error": {"code": code, "message": message}}),
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        let body = serde_json::to_vec(&self.body).unwrap_or_default();
        let mut out = format!(
            "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            self.status,
            reason(self.status),
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(&body);
        out
    }
}

impl From<ReadError> for Response {
    fn from(e: ReadError) -> Response {
        match e {
            ReadError::TooLarge => Response::error(413, "too_large", "the request is too large"),
            ReadError::Bad(m) => Response::error(400, "bad_request", m),
            ReadError::Closed => Response::error(400, "bad_request", "no request"),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Write `resp` to `w`.
pub async fn write_response<W: AsyncWrite + Unpin>(
    w: &mut W,
    resp: &Response,
) -> std::io::Result<()> {
    w.write_all(&resp.bytes()).await?;
    w.flush().await
}

/// Read a response from `r` (for tests and clients): status and JSON body.
pub async fn read_response<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Response> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).await?;
    parse_response(&buf)
}

pub fn parse_response(buf: &[u8]) -> std::io::Result<Response> {
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not an HTTP response");
    let end = find(buf, b"\r\n\r\n").ok_or_else(bad)? + 4;
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut resp = httparse::Response::new(&mut headers);
    match resp.parse(&buf[..end]) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(bad()),
    }
    let status = resp.code.ok_or_else(bad)?;
    let body = if buf.len() > end {
        serde_json::from_slice(&buf[end..]).map_err(|_| bad())?
    } else {
        Value::Null
    };
    Ok(Response { status, body })
}

/// A request's bytes (for tests and clients).
pub fn request_bytes(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {path} HTTP/1.1\r\nhost: machine\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        out.push_str(&format!(
            "content-type: application/json\r\ncontent-length: {}\r\n",
            body.len()
        ));
    }
    out.push_str("\r\n");
    let mut b = out.into_bytes();
    b.extend_from_slice(body);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read(bytes: &[u8]) -> Result<Request, ReadError> {
        let mut r = bytes;
        read_request(&mut r).await
    }

    #[tokio::test]
    async fn a_request_with_a_body() {
        let b = request_bytes("POST", "/v1/pair/claim?x=1", &[], br#"{"a":1}"#);
        let r = read(&b).await.unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/v1/pair/claim");
        assert_eq!(r.query, "x=1");
        assert_eq!(r.param("x"), Some("1"));
        assert_eq!(r.param("y"), None);
        assert_eq!(r.body, br#"{"a":1}"#);
        assert_eq!(r.header("host"), Some("machine"));
    }

    #[tokio::test]
    async fn bad_requests_are_refused() {
        assert_eq!(read(b"").await, Err(ReadError::Closed));
        assert!(matches!(
            read(b"GET / HTTP/1.1\r\n").await,
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"nonsense\r\n\r\n").await,
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"GET / HTTP/1.0\r\n\r\n").await,
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"GET http://x/ HTTP/1.1\r\n\r\n").await,
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n0\r\n\r\n").await,
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\ncontent-length: 2\r\ncontent-length: 3\r\n\r\nab").await,
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\ncontent-length: 5\r\n\r\nab").await,
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\ncontent-length: 1\r\n\r\nabc").await,
            Err(ReadError::Bad(_))
        ));
        assert!(matches!(
            read(b"POST / HTTP/1.1\r\ncontent-length: -1\r\n\r\n").await,
            Err(ReadError::Bad(_))
        ));
    }

    #[tokio::test]
    async fn large_requests_are_413() {
        let big = format!("GET / HTTP/1.1\r\nx: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        assert_eq!(read(big.as_bytes()).await, Err(ReadError::TooLarge));
        let b = format!(
            "POST / HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(read(b.as_bytes()).await, Err(ReadError::TooLarge));
        assert_eq!(Response::from(ReadError::TooLarge).status, 413);
    }

    #[test]
    fn responses_round_trip() {
        let r = Response::error(410, "code_expired", "gone");
        let back = parse_response(&r.bytes()).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.body["error"]["code"], "code_expired");
    }

    /// Bounded property test: no byte string makes the reader panic, and
    /// whatever it accepts has a path.
    #[tokio::test]
    async fn arbitrary_bytes_never_panic() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(3);
        let pieces: [&[u8]; 8] = [
            b"GET ",
            b"/v1/x",
            b" HTTP/1.1\r\n",
            b"content-length: 3\r\n",
            b"\r\n",
            b"abc",
            b"\xff\x00",
            b"origin: x\r\n",
        ];
        for _ in 0..2000 {
            let mut b = Vec::new();
            for _ in 0..rng.gen_range(0..8) {
                b.extend_from_slice(pieces[rng.gen_range(0..pieces.len())]);
            }
            if let Ok(r) = read(&b).await {
                assert!(r.path.starts_with('/'));
            }
        }
    }
}
