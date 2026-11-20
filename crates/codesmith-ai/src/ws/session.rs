//! A conversation's WebSocket (after OpenCode's `SessionModelTransport`):
//! one socket reused across the conversation's requests, one exchange at a
//! time, with the checkpoint of the latest completed response.

use super::{
    prepare, resolve, Channel, Checkpoint, Connection, Connector, Create, Delivery, Driver, Frame,
    Mode, Observation, Phase, Route, Tungstenite, WsError,
};
use crate::responses::Decode;
use crate::{sse, Decoder, Error, Event, EventStream, Provider, Request};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OwnedMutexGuard};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Reasoning models can stream nothing for minutes while still working.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Consecutive exchanges lost to the socket before the conversation stays
/// on HTTP.
const MAX_STREAM_FAILURES: u32 = 5;

struct Open {
    /// The connection's URL and headers; other ones need a new connection.
    affinity: String,
    conn: Connection,
    opened_at: Instant,
    checkpoint: Option<Checkpoint>,
}

#[derive(Default)]
struct State {
    closed: bool,
    /// Stay on HTTP (a connect failed, or the socket kept failing).
    http_fallback: bool,
    stream_failures: u32,
    open: Option<Open>,
    /// Connections opened, for tests and diagnostics.
    opened: usize,
    /// The latest socket failure.
    last_failure: Option<WsError>,
}

impl State {
    fn stream_failure(&mut self) {
        self.stream_failures += 1;
        if self.stream_failures >= MAX_STREAM_FAILURES {
            self.http_fallback = true;
        }
    }

    async fn close_open(&mut self) {
        if let Some(mut open) = self.open.take() {
            open.conn.outbound.close().await;
        }
    }
}

/// One conversation's Responses WebSocket. Cheap to create: nothing
/// connects until the first request.
pub struct SessionTransport {
    connector: Arc<dyn Connector>,
    state: Arc<Mutex<State>>,
    idle_timeout: Duration,
    connect_timeout: Duration,
}

impl Default for SessionTransport {
    fn default() -> Self {
        SessionTransport::new(Arc::new(Tungstenite::default()))
    }
}

impl std::fmt::Debug for SessionTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTransport").finish_non_exhaustive()
    }
}

/// How an exchange starts.
enum Start {
    /// Over HTTP instead.
    Fallback,
    Sent(Create),
}

impl SessionTransport {
    pub fn new(connector: Arc<dyn Connector>) -> Self {
        SessionTransport {
            connector,
            state: Arc::new(Mutex::new(State::default())),
            idle_timeout: IDLE_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
        }
    }

    /// How long to wait for the next frame of a response (default 30
    /// minutes).
    pub fn with_idle_timeout(mut self, d: Duration) -> Self {
        self.idle_timeout = d;
        self
    }

    /// Whether the conversation has fallen back to HTTP for good.
    pub async fn on_http(&self) -> bool {
        self.state.lock().await.http_fallback
    }

    /// Connections opened so far.
    pub async fn connections(&self) -> usize {
        self.state.lock().await.opened
    }

    /// The latest socket failure, with its phase and delivery.
    pub async fn last_failure(&self) -> Option<WsError> {
        self.state.lock().await.last_failure.clone()
    }

    /// Close the socket; later requests go over HTTP.
    pub async fn close(&self) {
        let mut s = self.state.lock().await;
        s.closed = true;
        s.close_open().await;
    }

    /// Stream `req` over the conversation's socket, or over HTTP when the
    /// route offers no WebSocket for this endpoint or the socket cannot
    /// carry it.
    pub async fn stream(
        &self,
        provider: &Provider,
        client: &reqwest::Client,
        req: &Request,
        route: Route,
    ) -> Result<EventStream, Error> {
        let req = resolve(provider, req)?;
        let prepared = prepare(provider, client, &req, route)?;
        let fallback = {
            let (p, c, r) = (provider.clone(), client.clone(), req.clone());
            move || -> futures_util::future::BoxFuture<'static, Result<EventStream, Error>> {
                let (p, c, r) = (p.clone(), c.clone(), r.clone());
                Box::pin(async move { crate::responses::stream(&p, &c, &r).await })
            }
        };
        let Some(channel) = prepared.channel else {
            return fallback().await;
        };
        let state = self.state.clone().lock_owned().await;
        if state.closed {
            drop(state);
            return fallback().await;
        }
        // From here an abandoned request (dropped mid-send) drops the
        // connection, which may yet carry its response.
        let mut exchange = Exchange { state, done: false };
        let mut driver = channel.driver();
        let started = self.start(&mut exchange.state, &channel, &mut driver).await;
        let create = match started {
            Ok(Start::Sent(create)) => create,
            Ok(Start::Fallback) => {
                exchange.finish();
                drop(exchange);
                return fallback().await;
            }
            Err(e) => {
                exchange.finish();
                return Err(e);
            }
        };
        Ok(self.frames(exchange, channel, driver, create, fallback))
    }

    /// Select (or open) the connection and send the request on it.
    async fn start(
        &self,
        state: &mut State,
        channel: &Channel,
        driver: &mut Driver,
    ) -> Result<Start, Error> {
        if state.http_fallback {
            return Ok(Start::Fallback);
        }
        let key = affinity(channel);
        let rotate = state.open.as_mut().is_some_and(|o| {
            o.affinity != key || o.opened_at.elapsed() >= channel.rotate_after || stale(&mut o.conn)
        });
        if rotate {
            state.close_open().await;
        }
        if state.open.is_none() {
            let opened = tokio::time::timeout(
                self.connect_timeout,
                self.connector.open(channel.handshake.clone()),
            )
            .await
            .unwrap_or_else(|_| {
                Err(
                    WsError::new("Timed out opening the WebSocket", Phase::Connect)
                        .code("connect-timeout")
                        .delivery(Delivery::NotSent),
                )
            });
            match opened {
                Ok(conn) => {
                    state.opened += 1;
                    state.open = Some(Open {
                        affinity: key,
                        conn,
                        opened_at: Instant::now(),
                        checkpoint: None,
                    });
                }
                // Any connect failure keeps the conversation on HTTP: a
                // network that refuses the upgrade would otherwise cost
                // every request a failed connect.
                Err(e) => {
                    state.last_failure = Some(e);
                    state.http_fallback = true;
                    return Ok(Start::Fallback);
                }
            }
        }
        let open = state.open.as_mut().expect("connection selected");
        let create = driver.create(open.checkpoint.as_ref());
        if create.mode == Mode::Full {
            open.checkpoint = None;
        }
        let mut message = create.message.clone();
        if let Some(h) = &channel.hooks {
            h.0.ws_send(&mut message);
        }
        match open.conn.outbound.send_text(message).await {
            Ok(()) => Ok(Start::Sent(create)),
            Err(e) => {
                state.close_open().await;
                if e.delivery == Some(Delivery::NotSent) {
                    return Ok(Start::Fallback);
                }
                state.stream_failure();
                let e = WsError {
                    phase: Phase::Send,
                    delivery: Some(Delivery::Ambiguous),
                    ..e
                };
                state.last_failure = Some(e.clone());
                Err(e.into())
            }
        }
    }

    fn frames(
        &self,
        mut exchange: Exchange,
        channel: Channel,
        mut driver: Driver,
        mut create: Create,
        fallback: impl Fn() -> futures_util::future::BoxFuture<'static, Result<EventStream, Error>>
            + Send
            + 'static,
    ) -> EventStream {
        let idle = self.idle_timeout;
        let this = SessionTransport {
            connector: self.connector.clone(),
            state: self.state.clone(),
            idle_timeout: self.idle_timeout,
            connect_timeout: self.connect_timeout,
        };
        async_stream::stream! {
            let mut decode = Decode::default();
            let mut yielded = false;
            let mut retried = false;
            'exchange: loop {
                let mut observed = false;
                loop {
                    let Some(open) = exchange.state.open.as_mut() else {
                        exchange.finish_failed(true);
                        yield Err(Error::Transport("WebSocket closed".into()));
                        return;
                    };
                    let next = tokio::time::timeout(idle, open.conn.inbound.recv()).await;
                    let frame = match next {
                        Err(_) => {
                            let e = WsError::new("Timed out waiting for WebSocket data", Phase::Receive)
                                .code("idle-timeout")
                                .delivery(if observed { Delivery::Accepted } else { Delivery::Ambiguous });
                            yield Err(exchange.fail(e).await);
                            return;
                        }
                        Ok(None) => {
                            let e = WsError::new("WebSocket closed", Phase::Close)
                                .code("closed")
                                .delivery(if observed { Delivery::Accepted } else { Delivery::Ambiguous });
                            yield Err(exchange.fail(e).await);
                            return;
                        }
                        // The provider closed the socket because the request
                        // was too large for it: this one and the rest go over HTTP.
                        Ok(Some(Err(e))) if e.code.as_deref() == Some("1009") && !observed => {
                            exchange.state.close_open().await;
                            exchange.state.http_fallback = true;
                            exchange.finish();
                            drop(exchange);
                            match fallback().await {
                                Ok(mut s) => while let Some(ev) = s.next().await { yield ev; },
                                Err(e) => yield Err(e),
                            }
                            return;
                        }
                        Ok(Some(Err(e))) => {
                            let delivery = if observed { Delivery::Accepted } else { Delivery::Ambiguous };
                            yield Err(exchange.fail(WsError { delivery: Some(delivery), ..e }).await);
                            return;
                        }
                        Ok(Some(Ok(Frame::Binary(_)))) => {
                            let e = WsError::new("Unsupported binary WebSocket frame", Phase::Receive)
                                .code("message")
                                .delivery(Delivery::Accepted);
                            yield Err(exchange.fail(e).await);
                            return;
                        }
                        Ok(Some(Ok(Frame::Text(mut t)))) => {
                            if let Some(h) = &channel.hooks {
                                h.0.ws_receive(&mut t);
                            }
                            t
                        }
                    };
                    observed = true;
                    let observation = match driver.observe(&create, &frame) {
                        Ok(o) => o,
                        Err(e) => {
                            exchange.poison().await;
                            yield Err(e);
                            return;
                        }
                    };
                    let (text, checkpoint) = match observation {
                        Observation::Frame(f) | Observation::Incomplete(f) => (f, None),
                        Observation::Completed { frame, checkpoint } => (frame, checkpoint),
                        Observation::ProviderFailure(e) => {
                            // Some backends stop serving a connection after
                            // any error frame; drop it so retries never race that.
                            exchange.terminal_error().await;
                            yield Err(e);
                            return;
                        }
                        // Either recovery needs a new connection: the
                        // rejected one was dropped above.
                        Observation::Rejected { error, .. } => {
                            exchange.terminal_error().await;
                            if yielded || retried {
                                yield Err(retryable(error));
                                return;
                            }
                            // Send the request again in full, on a new connection.
                            retried = true;
                            driver = channel.driver();
                            match this.start(&mut exchange.state, &channel, &mut driver).await {
                                Ok(Start::Sent(c)) => { create = c; exchange.done = false; continue 'exchange; }
                                Ok(Start::Fallback) => {
                                    exchange.finish();
                                    drop(exchange);
                                    match fallback().await {
                                        Ok(mut s) => while let Some(ev) = s.next().await { yield ev; },
                                        Err(e) => yield Err(e),
                                    }
                                    return;
                                }
                                Err(e) => { exchange.finish(); yield Err(e); return; }
                            }
                        }
                    };
                    let terminal = decode_frame(&mut decode, &text);
                    match terminal {
                        Ok(events) => {
                            for ev in events {
                                yielded = true;
                                yield Ok(ev);
                            }
                        }
                        Err(e) => {
                            exchange.poison().await;
                            yield Err(e);
                            return;
                        }
                    }
                    if decode.done() {
                        // Reached only once every event was taken: the
                        // checkpoint is committed for the next request.
                        exchange.state.stream_failures = 0;
                        if let Some(open) = exchange.state.open.as_mut() {
                            open.checkpoint = checkpoint;
                        }
                        exchange.finish();
                        return;
                    }
                }
            }
        }
        .boxed()
    }
}

fn decode_frame(decode: &mut Decode, text: &str) -> Result<Vec<Event>, Error> {
    decode.frame(&sse::Frame {
        event: None,
        data: text.to_string(),
    })
}

fn retryable(e: Error) -> Error {
    Error::Provider {
        message: e.to_string(),
        retryable: true,
    }
}

/// The connection key: its URL and sorted headers.
fn affinity(channel: &Channel) -> String {
    let mut headers = channel.handshake.headers.clone();
    headers.sort();
    let mut key = channel.handshake.url.clone();
    for (k, v) in headers {
        key.push('\n');
        key.push_str(&k);
        key.push(':');
        key.push_str(&v);
    }
    key
}

/// Whether a connection between exchanges closed or received data: either
/// way it cannot carry the next one.
fn stale(conn: &mut Connection) -> bool {
    !matches!(
        conn.inbound.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    )
}

/// The conversation's state for the length of one exchange. Dropped before
/// the exchange ends (the response was abandoned), it drops the connection:
/// the rest of that response would arrive on it.
struct Exchange {
    state: OwnedMutexGuard<State>,
    done: bool,
}

impl Exchange {
    fn finish(&mut self) {
        self.done = true;
    }

    /// The socket failed mid-exchange.
    async fn fail(&mut self, e: WsError) -> Error {
        self.poison().await;
        self.state.last_failure = Some(e.clone());
        e.into()
    }

    /// The exchange cannot go on over this connection.
    async fn poison(&mut self) {
        self.state.close_open().await;
        self.state.stream_failure();
        self.done = true;
    }

    fn finish_failed(&mut self, count: bool) {
        self.state.open = None;
        if count {
            self.state.stream_failure();
        }
        self.done = true;
    }

    /// The provider ended the exchange with an error.
    async fn terminal_error(&mut self) {
        self.state.stream_failures = 0;
        self.state.close_open().await;
        self.done = true;
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        if !self.done {
            self.state.open = None;
        }
    }
}
