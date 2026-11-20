//! `codesmith serve` terminals: the `/pty` routes, the WebSocket attach
//! with a bearer token or a single-use ticket, session terminals removed
//! with their session. The programs run are `cat` and `sh -c 'exit 3'`.

use crate::harness::Env;
use crate::model::{Model, Reply, Script, M};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "pty-test-token";
const WAIT: Duration = Duration::from_secs(10);

struct Serve {
    _child: tokio::process::Child,
    url: String,
    http: reqwest::Client,
}

impl Serve {
    async fn start(e: &Env) -> Serve {
        let mut child = e
            .command(&["serve", "--port", "0", "--token", TOKEN])
            .spawn()
            .unwrap();
        let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
        let url = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(line) = err.next_line().await.unwrap() {
                if let Some(at) = line.find("http://") {
                    return line[at..].split_whitespace().next().unwrap().to_string();
                }
            }
            panic!("serve exited before listening");
        })
        .await
        .expect("serve did not start");
        tokio::spawn(async move { while let Ok(Some(_)) = err.next_line().await {} });
        Serve {
            _child: child,
            url,
            http: reqwest::Client::new(),
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(TOKEN);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    fn ws_url(&self, path: &str) -> String {
        format!("{}{path}", self.url.replacen("http://", "ws://", 1))
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws_with_token(s: &Serve, pty: &str) -> Ws {
    let mut req = s
        .ws_url(&format!("/pty/{pty}/connect"))
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

/// Text frames until one of them (together) contains `text`; the cursor
/// frame is checked on the way.
async fn read_until(ws: &mut Ws, text: &str) -> String {
    let mut seen = String::new();
    tokio::time::timeout(WAIT, async {
        while !seen.contains(text) {
            match ws.next().await.unwrap().unwrap() {
                Message::Text(t) => seen.push_str(t.as_str()),
                Message::Binary(b) => {
                    assert_eq!(b[0], 0, "a control frame");
                    let v: Value = serde_json::from_slice(&b[1..]).unwrap();
                    assert!(v["cursor"].is_u64(), "{v}");
                }
                other => panic!("{other:?} before {text:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout waiting for {text:?} in {seen:?}"));
    seen
}

async fn closed_with(ws: &mut Ws) -> u16 {
    tokio::time::timeout(WAIT, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(Some(f)))) => return u16::from(f.code),
                Some(Ok(Message::Close(None))) | None | Some(Err(_)) => return 0,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .expect("the socket did not close")
}

fn env() -> Env {
    let e = Env::new();
    e.config("http://127.0.0.1:9/v1", json!({}));
    e
}

#[tokio::test(flavor = "multi_thread")]
async fn terminals_over_http_and_a_websocket() {
    let e = env();
    let s = Serve::start(&e).await;
    use reqwest::Method as V;

    let (code, info) = s
        .call(
            V::POST,
            "/pty",
            Some(json!({"command": "cat", "title": "cat"})),
        )
        .await;
    assert_eq!(code, 200, "{info}");
    let id = info["id"].as_str().unwrap().to_string();
    assert_eq!(info["status"], "running");
    assert_eq!(info["cwd"], e.project.display().to_string());
    let (_, all) = s.call(V::GET, "/pty", None).await;
    assert_eq!(all[0]["id"], id.as_str());

    let (code, upd) = s
        .call(
            V::PUT,
            &format!("/pty/{id}"),
            Some(json!({"title": "renamed", "size": {"cols": 100, "rows": 30}})),
        )
        .await;
    assert_eq!(code, 200, "{upd}");
    assert_eq!(
        (&upd["title"], &upd["userTitle"], &upd["size"]),
        (
            &json!("renamed"),
            &json!("renamed"),
            &json!({"cols": 100, "rows": 30})
        )
    );
    let (_, upd) = s
        .call(V::PUT, &format!("/pty/{id}"), Some(json!({"title": null})))
        .await;
    assert_eq!(upd["title"], "cat");

    // Attach, type, see it; removal closes the socket normally.
    let mut ws = ws_with_token(&s, &id).await;
    ws.send(Message::text("hello\n")).await.unwrap();
    read_until(&mut ws, "hello").await;
    // Binary input that is UTF-8 is typed too.
    ws.send(Message::binary(b"again\n".to_vec())).await.unwrap();
    read_until(&mut ws, "again").await;
    // A second attachment replays what was kept.
    let mut replay = ws_with_token(&s, &id).await;
    read_until(&mut replay, "again").await;

    let (code, _) = s.call(V::DELETE, &format!("/pty/{id}"), None).await;
    assert_eq!(code, 204);
    assert_eq!(closed_with(&mut ws).await, 1000);
    let (code, _) = s.call(V::GET, &format!("/pty/{id}"), None).await;
    assert_eq!(code, 404);

    // An exited terminal is kept with its code, and refuses attaching.
    let (_, info) = s
        .call(
            V::POST,
            "/pty",
            Some(json!({"command": "/bin/sh", "args": ["-c", "exit 3"]})),
        )
        .await;
    let id = info["id"].as_str().unwrap().to_string();
    let exited = tokio::time::timeout(WAIT, async {
        loop {
            let (_, v) = s.call(V::GET, &format!("/pty/{id}"), None).await;
            if v["status"] == "exited" {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(exited["exitCode"], 3);
    let mut ws = ws_with_token(&s, &id).await;
    assert_eq!(closed_with(&mut ws).await, 4404);

    // Bad input.
    let (code, _) = s
        .call(V::POST, "/pty", Some(json!({"cwd": "/no/such/dir"})))
        .await;
    assert_eq!(code, 400);
    let (code, _) = s.call(V::GET, "/pty/pty_missing", None).await;
    assert_eq!(code, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_websocket_opens_with_a_single_use_ticket() {
    let e = env();
    let s = Serve::start(&e).await;
    use reqwest::Method as V;
    let (_, info) = s
        .call(V::POST, "/pty", Some(json!({"command": "cat"})))
        .await;
    let id = info["id"].as_str().unwrap().to_string();
    let token_url = format!("{}/pty/{id}/connect-token", s.url);

    // The custom header is required, and so is the bearer token.
    let r = s
        .http
        .post(&token_url)
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = s
        .http
        .post(&token_url)
        .header("x-codesmith-ticket", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = s
        .http
        .post(&token_url)
        .bearer_auth(TOKEN)
        .header("x-codesmith-ticket", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let t: Value = r.json().await.unwrap();
    assert_eq!(t["expires_in"], 60);
    let ticket = t["ticket"].as_str().unwrap();

    // Without a token or ticket: refused.
    let plain = s.ws_url(&format!("/pty/{id}/connect"));
    assert!(tokio_tungstenite::connect_async(plain.as_str())
        .await
        .is_err());
    // A wrong ticket: refused.
    let wrong = format!("{plain}?ticket=nope");
    assert!(tokio_tungstenite::connect_async(wrong.as_str())
        .await
        .is_err());
    // With the ticket: open, once.
    let good = format!("{plain}?ticket={ticket}&cursor=-1");
    let (mut ws, _) = tokio_tungstenite::connect_async(good.as_str())
        .await
        .unwrap();
    ws.send(Message::text("ticketed\n")).await.unwrap();
    read_until(&mut ws, "ticketed").await;
    assert!(tokio_tungstenite::connect_async(good.as_str())
        .await
        .is_err());

    // A ticket for one terminal does not open another.
    let (_, other) = s
        .call(V::POST, "/pty", Some(json!({"command": "cat"})))
        .await;
    let other = other["id"].as_str().unwrap();
    let (_, t) = s
        .call(V::POST, &format!("/pty/{id}/connect-token"), None)
        .await;
    assert!(t.get("ticket").is_none(), "the header is still needed: {t}");
    let r = s
        .http
        .post(&token_url)
        .bearer_auth(TOKEN)
        .header("x-codesmith-ticket", "1")
        .send()
        .await
        .unwrap();
    let t: Value = r.json().await.unwrap();
    let cross = format!(
        "{}?ticket={}",
        s.ws_url(&format!("/pty/{other}/connect")),
        t["ticket"].as_str().unwrap()
    );
    assert!(tokio_tungstenite::connect_async(cross.as_str())
        .await
        .is_err());

    // The Host and Origin checks still apply to a ticketed upgrade.
    let mut req = good.into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
}

/// The status a WebSocket upgrade to `url` without a bearer token gets
/// (101 when it opens).
async fn ws_status(url: &str) -> u16 {
    use tokio_tungstenite::tungstenite::Error;
    match tokio_tungstenite::connect_async(url).await {
        Ok(_) => 101,
        Err(Error::Http(r)) => r.status().as_u16(),
        Err(e) => panic!("{url}: {e}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ticketless_or_doubled_ticket_websocket_is_refused() {
    let e = env();
    let s = Serve::start(&e).await;
    use reqwest::Method as V;
    let (_, info) = s
        .call(V::POST, "/pty", Some(json!({"command": "cat"})))
        .await;
    let id = info["id"].as_str().unwrap().to_string();
    let plain = s.ws_url(&format!("/pty/{id}/connect"));
    let mint = |pty: String| {
        let s = &s;
        async move {
            let r = s
                .http
                .post(format!("{}/pty/{pty}/connect-token", s.url))
                .bearer_auth(TOKEN)
                .header("x-codesmith-ticket", "1")
                .send()
                .await
                .unwrap();
            let t: Value = r.json().await.unwrap();
            t["ticket"].as_str().unwrap().to_string()
        }
    };

    // The bypass: one non-empty ticket to pass the guard, an empty one
    // last for the handler to read. Either order, any value: refused.
    assert_eq!(ws_status(&format!("{plain}?ticket=x&ticket=")).await, 400);
    assert_eq!(ws_status(&format!("{plain}?ticket=&ticket=x")).await, 400);
    let good = mint(id.clone()).await;
    assert_eq!(
        ws_status(&format!("{plain}?ticket={good}&ticket={good}")).await,
        400
    );
    // An empty ticket, or none, is no credential.
    assert_eq!(ws_status(&format!("{plain}?ticket=")).await, 401);
    assert_eq!(ws_status(&plain).await, 401);
    assert_eq!(ws_status(&format!("{plain}?cursor=-1")).await, 401);
    // A ticket for another terminal does not open this one.
    let (_, other) = s
        .call(V::POST, "/pty", Some(json!({"command": "cat"})))
        .await;
    let theirs = mint(other["id"].as_str().unwrap().to_string()).await;
    assert_eq!(ws_status(&format!("{plain}?ticket={theirs}")).await, 403);
    // An unknown terminal is not told apart from a known one without
    // credentials: never 404.
    let missing = s.ws_url("/pty/pty_missing/connect");
    assert_eq!(ws_status(&missing).await, 401);
    assert_eq!(ws_status(&format!("{missing}?ticket=nope")).await, 403);
    assert_eq!(ws_status(&format!("{missing}?ticket=x&ticket=")).await, 400);
    // The good ticket, alone, still opens it (once).
    assert_eq!(ws_status(&format!("{plain}?ticket={good}")).await, 101);
    assert_eq!(ws_status(&format!("{plain}?ticket={good}")).await, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_terminals_go_with_it() {
    let e = Env::new();
    let m = Model::start(Script::new().titled("Hi").then(
        "answer",
        M::prompt("hi"),
        Reply::text("Hello."),
    ))
    .await;
    e.config(&m.url, json!({}));
    let s = Serve::start(&e).await;
    use reqwest::Method as V;
    let (_, created) = s.call(V::POST, "/sessions", Some(json!({}))).await;
    let sid = created["id"].as_str().unwrap().to_string();
    // A turn stores the session.
    let r = s
        .http
        .post(format!("{}/sessions/{sid}/messages", s.url))
        .bearer_auth(TOKEN)
        .json(&json!({"text": "hi"}))
        .send()
        .await
        .unwrap();
    let body = r.text().await.unwrap();
    assert!(body.contains("done"), "{body}");

    let (code, _) = s.call(V::GET, "/sessions/ses_none/terminals", None).await;
    assert_eq!(code, 404);
    let (code, t) = s
        .call(
            V::POST,
            &format!("/sessions/{sid}/terminals"),
            Some(json!({"command": "cat"})),
        )
        .await;
    assert_eq!(code, 200, "{t}");
    assert_eq!(t["sessionId"], sid.as_str());
    let tid = t["id"].as_str().unwrap().to_string();
    let (_, list) = s
        .call(V::GET, &format!("/sessions/{sid}/terminals"), None)
        .await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Nothing selected yet; writing selects it.
    let (_, none) = s
        .call(V::GET, &format!("/sessions/{sid}/terminals/read"), None)
        .await;
    assert_eq!(none, Value::Null);
    let mut ws = ws_with_token(&s, &tid).await;
    ws.send(Message::text("line one\n")).await.unwrap();
    read_until(&mut ws, "line one\r\nline one").await;
    let (code, read) = s
        .call(
            V::GET,
            &format!("/sessions/{sid}/terminals/read?lines=1"),
            None,
        )
        .await;
    assert_eq!(code, 200, "{read}");
    assert_eq!(read["ptyID"], tid.as_str());
    assert_eq!(read["screen"]["text"], "");
    let (_, read) = s
        .call(
            V::GET,
            &format!("/sessions/{sid}/terminals/read?lines=3"),
            None,
        )
        .await;
    assert_eq!(read["screen"]["text"], "line one\nline one\n");
    let (code, _) = s
        .call(
            V::GET,
            &format!("/sessions/{sid}/terminals/read?lines=0"),
            None,
        )
        .await;
    assert_eq!(code, 400);

    let pid = t["pid"].as_i64().unwrap() as i32;
    let (code, _) = s.call(V::DELETE, &format!("/sessions/{sid}"), None).await;
    assert_eq!(code, 200);
    assert_eq!(closed_with(&mut ws).await, 1000);
    let (code, _) = s.call(V::GET, &format!("/pty/{tid}"), None).await;
    assert_eq!(code, 404);
    // Its program is gone too.
    tokio::time::timeout(WAIT, async {
        // SAFETY: signal 0 only checks that the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the terminal's program is still running");
}
