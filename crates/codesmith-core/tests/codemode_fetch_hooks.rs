//! `fetch` and OpenAPI calls from inside `execute` get plugins' per-call
//! hooks, as built-ins do: under the `webfetch` and `openapi` tool names, a
//! `tool_before` veto refuses the call before any request, and `tool_after`
//! can redact what the response says. Its own test binary, because the
//! plugin registry is process-wide.

mod support;

use codesmith_core::plugin::{self, Plugin, ToolCall};
use serde_json::json;
use std::sync::{Arc, Mutex};
use support::{Quiet, H};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Hooks {
    before: Mutex<Vec<String>>,
}

impl Plugin for Hooks {
    fn id(&self) -> &str {
        "hooks"
    }
    fn tool_before(&self, call: &mut ToolCall) -> Result<(), String> {
        self.before
            .lock()
            .unwrap()
            .push(format!("{} {}", call.name, call.input));
        // The `execute` call carries the program, which names "vetoed" too.
        if call.name != "execute" && call.input.to_string().contains("vetoed") {
            return Err(format!("{} vetoed by the test plugin", call.name));
        }
        Ok(())
    }
    fn tool_after(&self, _call: &ToolCall, output: &mut String, _is_error: &mut bool) {
        *output = output.replace("SECRET-TOKEN", "[redacted]");
    }
}

/// A loopback server: every path answers with JSON naming the secret.
async fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path);
                let body = r#"{"id": "42", "name": "SECRET-TOKEN"}"#;
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_and_openapi_inside_execute_get_plugin_hooks() {
    let hooks = Arc::new(Hooks::default());
    plugin::install(vec![hooks.clone()]);
    let (base, seen) = serve().await;
    let spec = json!({
        "openapi": "3.1.0",
        "paths": {"/users/{id}": {"get": {
            "operationId": "users.get",
            "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string"}}],
            "responses": {"200": {"description": "The user", "content": {"application/json": {"schema": {
                "type": "object", "properties": {"id": {"type": "string"}, "name": {"type": "string"}}
            }}}}}
        }}}
    });
    let mut h = H::with(
        json!({
            "model": "fake/claude-sonnet-4-5",
            "full_codemode": true,
            "openapi": {"api": {"spec": spec, "base_url": base}},
        }),
        None,
    )
    .await;
    let code = format!(
        "const a = await (await fetch('{base}/page')).text();\n\
         let b; try {{ await fetch('{base}/vetoed'); b = 'ran'; }} catch (e) {{ b = 'refused: ' + e.message; }}\n\
         const u = await tools.api.users.get({{ id: '42' }});\n\
         let c; try {{ await tools.api.users.get({{ id: 'vetoed' }}); c = 'ran'; }} catch (e) {{ c = 'refused: ' + e.message; }}\n\
         return [a, b, u.name, c].join(' | ');"
    );
    let prompt = format!("call execute {}", json!({ "code": code }));
    h.s.prompt(&prompt, &mut Quiet).await.unwrap();

    // The vetoed calls never reached the server.
    assert_eq!(*seen.lock().unwrap(), ["/page", "/users/42"]);
    let before = hooks.before.lock().unwrap().clone();
    assert_eq!(
        before.iter().filter(|b| b.starts_with("webfetch ")).count(),
        2,
        "{before:?}"
    );
    assert_eq!(
        before.iter().filter(|b| b.starts_with("openapi ")).count(),
        2,
        "{before:?}"
    );
    assert!(
        before
            .iter()
            .any(|b| b.starts_with("openapi ") && b.contains("users.get")),
        "{before:?}"
    );

    // What the model saw: both refusals, and both bodies redacted.
    let log = h.log.lock().unwrap().clone();
    let sent = log.last().unwrap()["messages"].to_string();
    assert!(
        sent.contains("webfetch vetoed by the test plugin"),
        "{sent}"
    );
    assert!(sent.contains("openapi vetoed by the test plugin"), "{sent}");
    assert!(sent.contains("[redacted]"), "{sent}");
    assert!(!sent.contains("SECRET-TOKEN"), "{sent}");
}
