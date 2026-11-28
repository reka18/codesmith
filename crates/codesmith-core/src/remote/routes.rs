//! A paired device's routes on `codesmith/remote/1` (Phase A, S1–S3,
//! S12–S16, S18, S20, S23): the machine, its sessions, one session's
//! snapshot and messages, and the event streams. Every route checks the
//! device's grants: a session it has none for answers 404, never 403, and
//! the lists show only granted sessions (S2). Errors are `{"error":
//! {"code", "message"}}` (E1).
//!
//! A session a process holds is read from that process (through
//! [`Owners`]) and the session store; one no process holds is `closed`,
//! read-only, from the store (S3).

use super::http::{Request, Response};
use super::owners::{Granted, Item, Owners, CLOSED_EPOCH};
use super::store::{Grant, Scope, ALL_SESSIONS};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// What a device may use, as this machine supports it (S16). Watching
/// is all there is so far: no `session_reply`, `terminal` or `push`.
pub const FEATURES: &[&str] = &["watch"];

/// The most sessions `GET /v1/sessions` lists for an all-sessions grant
/// from the store (the held ones are always listed).
pub const LIST_CLOSED: usize = 200;

/// A page of `GET /v1/sessions/{id}/messages`: the default and the most.
pub const PAGE: usize = 50;
pub const MAX_PAGE: usize = 500;

/// What a route needs to know.
pub struct Ctx {
    pub node_id: String,
    pub machine: String,
    pub version: String,
    /// The device's grants.
    pub grants: Vec<Grant>,
    pub owners: Arc<Owners>,
    /// The session store, when there is one.
    pub sessions_db: Option<PathBuf>,
}

pub enum Reply {
    Json(Response),
    /// An event stream: frames until it ends.
    Stream(mpsc::Receiver<Item>),
}

fn not_found() -> Reply {
    Reply::Json(Response::error(
        404,
        "not_found",
        "no such session or route",
    ))
}

/// The device's scope on `session`, if any.
pub fn scope(grants: &[Grant], session: &str) -> Option<Scope> {
    grants
        .iter()
        .find(|g| g.session == session)
        .or_else(|| grants.iter().find(|g| g.session == ALL_SESSIONS))
        .map(|g| g.scope)
}

fn granted(grants: &[Grant]) -> Granted {
    if grants.iter().any(|g| g.session == ALL_SESSIONS) {
        Granted::All
    } else {
        Granted::Some(grants.iter().map(|g| g.session.clone()).collect())
    }
}

/// The session store, read-only use; `None` when it doesn't exist (the
/// gateway never makes one).
fn sessions(ctx: &Ctx) -> Option<crate::store::Store> {
    let p = ctx.sessions_db.as_ref().filter(|p| p.exists())?;
    crate::store::Store::open(p).ok()
}

/// A stored session's messages up to record `upto`, as serve shows them,
/// with their record seqs.
fn stored_messages(
    db: &crate::store::Store,
    id: &str,
    upto: i64,
) -> (Vec<Value>, Vec<Option<i64>>) {
    let entries: Vec<_> = db
        .entries(id)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.seq <= upto)
        .collect();
    crate::store::history_with_seqs(&entries)
        .into_iter()
        .filter(|(_, m)| !crate::runner::is_engine(m))
        .map(|(s, m)| (serde_json::to_value(&m).unwrap_or_default(), s))
        .unzip()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

pub async fn route(ctx: Arc<Ctx>, req: &Request) -> Reply {
    let parts: Vec<&str> = req.path.trim_end_matches('/').split('/').collect();
    let get = req.method == "GET";
    match parts.as_slice() {
        ["", "v1", "machine"] if get => Reply::Json(Response::ok(json!({
            "id": ctx.node_id,
            "name": ctx.machine,
            "version": ctx.version,
            "protocol": 1,
            "features": FEATURES,
        }))),
        ["", "v1", "sessions"] if get => Reply::Json(list(ctx).await),
        ["", "v1", "events"] if get => {
            let (tx, rx) = mpsc::channel(super::owners::QUEUE);
            ctx.owners.subscribe_all(granted(&ctx.grants), tx);
            Reply::Stream(rx)
        }
        ["", "v1", "sessions", id, rest @ ..] => {
            let Some(sc) = scope(&ctx.grants, id) else {
                return not_found();
            };
            let id = id.to_string();
            match (get, rest) {
                (true, []) => show(ctx, &id, sc).await,
                (true, ["messages"]) => messages(ctx, &id, req).await,
                (true, ["events"]) => events(ctx, &id, req).await,
                // Control (Phase B) isn't offered yet: a watch grant
                // never may (C8), and with control it's not here yet.
                (false, _) if sc == Scope::Watch => Reply::Json(Response::error(
                    403,
                    "forbidden_scope",
                    "this device may only watch this session",
                )),
                _ => not_found(),
            }
        }
        _ => not_found(),
    }
}

fn item(id: &str, info: &Value, busy: bool, owner: &str, sc: Scope) -> Value {
    json!({"id": id, "title": info["title"], "agent": info["agent"], "model": info["model"],
           "directory": info["directory"], "busy": busy, "owner": owner, "scope": sc})
}

fn stored_info(i: &crate::store::Info) -> Value {
    json!({"title": i.title, "agent": i.agent, "model": i.model, "directory": i.directory})
}

/// `GET /v1/sessions`: the granted sessions, held ones first (S12).
async fn list(ctx: Arc<Ctx>) -> Response {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (id, h) in ctx.owners.all_held() {
        if let Some(sc) = scope(&ctx.grants, &id) {
            seen.insert(id.clone());
            out.push(item(&id, &h.info, h.busy, &h.kind, sc));
        }
    }
    let c = ctx.clone();
    let closed = blocking(move || {
        let Some(db) = sessions(&c) else {
            return vec![];
        };
        let mut infos: Vec<crate::store::Info> =
            if c.grants.iter().any(|g| g.session == ALL_SESSIONS) {
                let mut all = db.list(None, false).unwrap_or_default();
                all.sort_by_key(|i| std::cmp::Reverse(i.updated));
                all.truncate(LIST_CLOSED + seen.len());
                all
            } else {
                c.grants
                    .iter()
                    .filter_map(|g| db.get(&g.session).ok().flatten())
                    .collect()
            };
        infos.retain(|i| !seen.contains(&i.id));
        infos.truncate(LIST_CLOSED);
        infos
            .iter()
            .filter_map(|i| {
                let sc = scope(&c.grants, &i.id)?;
                Some(item(&i.id, &stored_info(i), false, "closed", sc))
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    out.extend(closed);
    Response::ok(Value::Array(out))
}

/// `GET /v1/sessions/{id}`: a snapshot (S12, S18, S20, S23). Its messages
/// are the store's up to the history the owner's cursor covers; the
/// running turn's events all come after `events_seq`.
async fn show(ctx: Arc<Ctx>, id: &str, sc: Scope) -> Reply {
    if let Some(h) = ctx.owners.held(id) {
        let snap = match ctx.owners.snapshot(id).await {
            Ok(s) => s,
            Err(e) => {
                return Reply::Json(Response::error(
                    503,
                    "unavailable",
                    &format!("the session's owner didn't answer: {e}"),
                ))
            }
        };
        let upto = snap["history_seq"].as_i64().unwrap_or(0);
        let (c, sid) = (ctx.clone(), id.to_string());
        let (messages, seqs) = blocking(move || {
            sessions(&c).map_or((vec![], vec![]), |db| stored_messages(&db, &sid, upto))
        })
        .await
        .unwrap_or_default();
        let info = &snap["info"];
        let mut v = item(
            id,
            info,
            snap["busy"].as_bool().unwrap_or(h.busy),
            &h.kind,
            sc,
        );
        v["effort"] = info["effort"].clone();
        v["epoch"] = snap["epoch"].clone();
        v["events_seq"] = snap["events_seq"].clone();
        v["turn"] = snap["turn"].clone();
        v["messages"] = json!(messages);
        v["seqs"] = json!(seqs);
        return Reply::Json(Response::ok(v));
    }
    let (c, sid) = (ctx.clone(), id.to_string());
    let found = blocking(move || {
        let db = sessions(&c)?;
        let info = db.get(&sid).ok().flatten()?;
        Some((info.clone(), stored_messages(&db, &sid, i64::MAX)))
    })
    .await
    .flatten();
    let Some((info, (messages, seqs))) = found else {
        return not_found();
    };
    let mut v = item(id, &stored_info(&info), false, "closed", sc);
    v["epoch"] = json!(CLOSED_EPOCH);
    v["events_seq"] = json!(0);
    v["turn"] = Value::Null;
    v["messages"] = json!(messages);
    v["seqs"] = json!(seqs);
    v["usage"] = json!(info.usage);
    Reply::Json(Response::ok(v))
}

/// `GET /v1/sessions/{id}/messages?limit=N&before=SEQ`: the newest `limit`
/// messages (50 by default, at most 500) whose record seq is below
/// `before`, oldest first, with `more` saying whether older ones remain.
async fn messages(ctx: Arc<Ctx>, id: &str, req: &Request) -> Reply {
    let num = |k: &str| -> Result<Option<i64>, ()> {
        match req.param(k) {
            None => Ok(None),
            Some(v) => v
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
                .map(Some)
                .ok_or(()),
        }
    };
    let (Ok(limit), Ok(before)) = (num("limit"), num("before")) else {
        return Reply::Json(Response::error(
            400,
            "bad_request",
            "limit and before are non-negative numbers",
        ));
    };
    let limit = (limit.unwrap_or(PAGE as i64) as usize).clamp(1, MAX_PAGE);
    let upto = match ctx.owners.held(id) {
        Some(h) => h.info["history_seq"].as_i64().unwrap_or(0),
        None => i64::MAX,
    };
    let held = ctx.owners.held(id).is_some();
    let (c, sid) = (ctx.clone(), id.to_string());
    let page = blocking(move || {
        let db = sessions(&c)?;
        if !held && db.get(&sid).ok().flatten().is_none() {
            return None;
        }
        let (m, s) = stored_messages(&db, &sid, upto);
        let keep: Vec<usize> = (0..m.len())
            .filter(|i| before.is_none_or(|b| s[*i].is_some_and(|x| x < b)))
            .collect();
        let from = keep.len().saturating_sub(limit);
        let more = from > 0;
        let pick = &keep[from..];
        Some((
            pick.iter().map(|i| m[*i].clone()).collect::<Vec<_>>(),
            pick.iter().map(|i| s[*i]).collect::<Vec<_>>(),
            more,
        ))
    })
    .await
    .flatten();
    match page {
        Some((messages, seqs, more)) => Reply::Json(Response::ok(
            json!({"messages": messages, "seqs": seqs, "more": more}),
        )),
        None if held => Reply::Json(Response::ok(
            json!({"messages": [], "seqs": [], "more": false}),
        )),
        None => not_found(),
    }
}

/// `GET /v1/sessions/{id}/events`, resuming after `Last-Event-ID` (S5).
async fn events(ctx: Arc<Ctx>, id: &str, req: &Request) -> Reply {
    let cursor = req
        .header("last-event-id")
        .filter(|c| super::client::Cursor::parse(c).is_some())
        .map(str::to_string);
    if ctx.owners.held(id).is_none() {
        let (c, sid) = (ctx.clone(), id.to_string());
        let exists =
            blocking(move || sessions(&c).is_some_and(|db| db.get(&sid).ok().flatten().is_some()))
                .await
                .unwrap_or(false);
        if !exists {
            return not_found();
        }
    }
    let (tx, rx) = mpsc::channel(super::owners::QUEUE);
    ctx.owners.subscribe(id, cursor.as_deref(), tx);
    Reply::Stream(rx)
}
