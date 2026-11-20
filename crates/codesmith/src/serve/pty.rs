//! The server's terminals (OpenCode's `/api/pty` and session terminal
//! routes): create, list, inspect, rename, resize and remove terminals,
//! and attach to one over a WebSocket.
//!
//! - `GET    /pty` · `POST /pty {command?, args?, cwd?, title?, env?, size?: {cols, rows}}`
//! - `GET    /pty/{pty}` · `PUT /pty/{pty} {title?: string|null, size?}` · `DELETE /pty/{pty}`
//! - `POST   /pty/{pty}/connect-token` (header `x-codesmith-ticket: 1`) →
//!   `{ticket, expires_in}`: a single-use ticket, good for 60 seconds, for
//!   opening the WebSocket where a bearer token cannot be sent (a browser)
//! - `GET    /pty/{pty}/connect?cursor=&ticket=` → WebSocket: the kept
//!   output from `cursor` (all of it when absent, none for `-1`) as text
//!   frames, a binary `0x00 {"cursor": n}` frame, then live output; text
//!   or binary (UTF-8) frames from the client are typed into the terminal.
//!   It closes with 1000 when the program exits or the terminal is
//!   removed, and with 4404 when it does not exist or has already exited.
//! - `GET    /sessions/{id}/terminals` · `POST /sessions/{id}/terminals {...}`:
//!   a session's terminals, removed with the session
//! - `GET    /sessions/{id}/terminals/read?lines=` → the session's current
//!   terminal (the one last written to, resized or attached to) as text,
//!   or `null`
//!
//! These routes sit behind the same guard as the rest (Host, Origin,
//! bearer token); the WebSocket alone may instead carry a ticket, which
//! must then be good.

use super::{not_found, ApiError, St};
use axum::extract::{Path as UrlPath, Query, Request as HttpRequest, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use codesmith_core::pty::protocol;
use codesmith_core::pty::ticket::Scope;
use codesmith_core::pty::{CreateInput, Output, PtyError, Size, UpdateInput};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role};
use tokio_tungstenite::tungstenite::Message;

/// The header a connect-token request must carry: it makes a browser
/// preflight the request, so other sites cannot mint tickets.
pub(super) const TICKET_HEADER: &str = "x-codesmith-ticket";

pub(super) fn routes() -> Router<St> {
    Router::new()
        .route("/pty", get(list).post(create))
        .route("/pty/{pty}", get(show).put(update).delete(remove))
        .route("/pty/{pty}/connect-token", post(connect_token))
        .route("/pty/{pty}/connect", get(connect))
        .route(
            "/sessions/{id}/terminals",
            get(session_list).post(session_create),
        )
        .route("/sessions/{id}/terminals/read", get(session_read))
}

/// Whether `req` is a WebSocket connect that carries a ticket, which the
/// handler checks in place of the bearer token.
pub(super) fn ticketed(req: &HttpRequest) -> bool {
    let path = req.uri().path();
    let connect = path
        .strip_prefix("/pty/")
        .and_then(|r| r.strip_suffix("/connect"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'));
    connect
        && req.method() == axum::http::Method::GET
        && req.uri().query().is_some_and(|q| {
            q.split('&')
                .any(|kv| kv.strip_prefix("ticket=").is_some_and(|t| !t.is_empty()))
        })
}

fn error(e: PtyError) -> ApiError {
    let status = match e {
        PtyError::NotFound(_) => StatusCode::NOT_FOUND,
        PtyError::Exited(_) => StatusCode::CONFLICT,
        PtyError::Limit(_) => StatusCode::TOO_MANY_REQUESTS,
        PtyError::InvalidCwd(_) | PtyError::Invalid(_) | PtyError::Spawn(_) => {
            StatusCode::BAD_REQUEST
        }
    };
    ApiError(status, e.to_string())
}

fn create_input(body: Option<Json<Value>>) -> Result<CreateInput, ApiError> {
    let v = body.map(|b| b.0).unwrap_or(Value::Null);
    if v.is_null() {
        return Ok(CreateInput::default());
    }
    serde_json::from_value(v).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))
}

async fn list(State(st): State<St>) -> Json<Value> {
    Json(json!(st.ptys.list(None)))
}

async fn create(State(st): State<St>, body: Option<Json<Value>>) -> Result<Json<Value>, ApiError> {
    let input = create_input(body)?;
    Ok(Json(json!(st.ptys.create(input).map_err(error)?)))
}

async fn show(
    State(st): State<St>,
    UrlPath(pty): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(st.ptys.get(&pty).map_err(error)?)))
}

async fn update(
    State(st): State<St>,
    UrlPath(pty): UrlPath<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, ApiError> {
    let v = body.map(|b| b.0).unwrap_or_default();
    let bad = |m: &str| ApiError(StatusCode::BAD_REQUEST, m.into());
    let title = match v.get("title") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(_) => return Err(bad("title must be a string or null")),
    };
    let size = match v.get("size") {
        None | Some(Value::Null) => None,
        Some(s) => Some(
            serde_json::from_value::<Size>(s.clone())
                .map_err(|_| bad("size must be {cols, rows}, positive integers"))?,
        ),
    };
    let input = UpdateInput { title, size };
    Ok(Json(json!(st.ptys.update(&pty, input).map_err(error)?)))
}

async fn remove(
    State(st): State<St>,
    UrlPath(pty): UrlPath<String>,
) -> Result<StatusCode, ApiError> {
    st.ptys.remove(&pty).map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn scope(st: &St, pty: &str) -> Scope {
    Scope::pty(pty).directory(&st.cwd.display().to_string())
}

async fn connect_token(
    State(st): State<St>,
    UrlPath(pty): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    if headers.get(TICKET_HEADER).and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "Invalid PTY connect token request".into(),
        ));
    }
    st.ptys.get(&pty).map_err(error)?;
    Ok(Json(json!(st.tickets.issue(scope(&st, &pty)))))
}

async fn connect(
    State(st): State<St>,
    UrlPath(pty): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    mut req: HttpRequest,
) -> Response {
    if let Some(t) = q.get("ticket").filter(|t| !t.is_empty()) {
        if !st.tickets.consume(&scope(&st, &pty), t) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    if st.ptys.get(&pty).is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let cursor = q
        .get("cursor")
        .and_then(|c| c.parse::<i64>().ok())
        .filter(|&c| c >= -1);
    let h = req.headers();
    let has = |name: header::HeaderName, want: &str| {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|p| p.trim().eq_ignore_ascii_case(want)))
    };
    let upgrade = has(header::UPGRADE, "websocket")
        && has(header::CONNECTION, "upgrade")
        && has(header::SEC_WEBSOCKET_VERSION, "13");
    let Some(key) = h.get(header::SEC_WEBSOCKET_KEY).filter(|_| upgrade) else {
        return ApiError(
            StatusCode::UPGRADE_REQUIRED,
            "this route takes a WebSocket upgrade".into(),
        )
        .into_response();
    };
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let on = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let Ok(io) = on.await else { return };
        let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(io),
            Role::Server,
            None,
        )
        .await;
        follow(st, pty, cursor, ws).await;
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(axum::body::Body::empty())
        .unwrap()
}

fn close(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    }))
}

/// Run one attached WebSocket: replay, cursor, then output out and input
/// in until either side ends.
async fn follow<S>(
    st: St,
    pty: String,
    cursor: Option<i64>,
    ws: tokio_tungstenite::WebSocketStream<S>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut tx, mut rx) = ws.split();
    let mut att = match st.ptys.attach(&pty, cursor) {
        Ok(a) => a,
        Err(e) => {
            let why = match e {
                PtyError::Exited(_) => "session exited",
                _ => "session not found",
            };
            let _ = tx.send(close(4404, why)).await;
            return;
        }
    };
    for c in protocol::chunks(&att.replay) {
        if tx.send(Message::text(c)).await.is_err() {
            return;
        }
    }
    if tx
        .send(Message::binary(protocol::meta_frame(att.cursor)))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            out = att.recv() => match out {
                Some(Output::Data(s)) => {
                    if tx.send(Message::text(s)).await.is_err() {
                        return;
                    }
                }
                Some(Output::End { .. }) | None => {
                    let _ = tx.send(close(1000, "")).await;
                    return;
                }
            },
            msg = rx.next() => {
                let input = match msg {
                    Some(Ok(Message::Text(t))) => t.as_str().to_string(),
                    Some(Ok(Message::Binary(b))) => match protocol::decode_input(&b) {
                        Some(s) => s.to_string(),
                        None => continue,
                    },
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    Some(Ok(_)) => continue,
                };
                let (ptys, id) = (st.ptys.clone(), pty.clone());
                let _ = tokio::task::spawn_blocking(move || ptys.write(&id, &input)).await;
            }
        }
    }
}

/// Whether session `id` is one of this server's.
async fn session_exists(st: &St, id: &str) -> Result<(), ApiError> {
    if st.live.lock().await.contains_key(id) || st.store()?.get(id)?.is_some_and(|i| st.ours(&i)) {
        Ok(())
    } else {
        Err(not_found(id))
    }
}

async fn session_list(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    session_exists(&st, &id).await?;
    Ok(Json(json!(st.ptys.list(Some(&id)))))
}

async fn session_create(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, ApiError> {
    session_exists(&st, &id).await?;
    let mut input = create_input(body)?;
    input.session_id = Some(id);
    Ok(Json(json!(st.ptys.create(input).map_err(error)?)))
}

async fn session_read(
    State(st): State<St>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    session_exists(&st, &id).await?;
    let lines = match q.get("lines") {
        None => None,
        Some(l) => Some(l.parse::<u32>().map_err(|_| {
            error(PtyError::Invalid(format!(
                "lines must be an integer between 1 and {}",
                codesmith_core::pty::READ_LINES
            )))
        })?),
    };
    Ok(Json(json!(st.ptys.read(&id, lines).map_err(error)?)))
}
