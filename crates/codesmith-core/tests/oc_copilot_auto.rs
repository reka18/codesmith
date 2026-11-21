//! GitHub Copilot's Auto router, ported from OpenCode's
//! `plugin/provider-github-copilot.test.ts` (MIT) against a scripted
//! Copilot API on loopback. See `crates/codesmith-core/PARITY.md`.

use codesmith_core::providers::copilot_auto::{self as auto, Account, Prompt};
use codesmith_core::providers::copilot_models;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct State {
    pick: String,
    available: bool,
    /// Router request bodies and their authorization.
    picks: Vec<(Value, String)>,
}

/// Answers `POST /auto` from `state`.
async fn serve(state: Arc<Mutex<State>>) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head, body) = loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len: usize = text[..i]
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            break (
                                text[..i].to_string(),
                                serde_json::from_slice::<Value>(&buf[i + 4..i + 4 + len])
                                    .unwrap_or(Value::Null),
                            );
                        }
                    }
                };
                let auth = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_string())
                    })
                    .unwrap_or_default();
                let (status, payload) = {
                    let mut st = state.lock().unwrap();
                    st.picks.push((body, auth));
                    if st.available && head.starts_with("POST /auto ") {
                        (200, json!({"session_token": format!("token-{}", st.pick), "expires_at": 4_000_000_000i64, "selected_model": {"id": st.pick}}).to_string())
                    } else {
                        (503, String::new())
                    }
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

fn remote(id: &str, extra: Value) -> Value {
    let mut v = json!({
        "model_picker_enabled": false,
        "id": id, "name": id, "version": id,
        "supported_endpoints": ["/chat/completions"],
        "capabilities": {
            "family": id,
            "limits": {"max_prompt_tokens": 128_000, "max_output_tokens": 64_000},
            "supports": {"tool_calls": true}
        }
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn kept(account: &str) -> Value {
    json!({"account": account, "models": [
        remote("routed", json!({"billing": {"auto_discount": 0.1}})),
        remote("other", json!({"billing": {"auto_discount": 0.1}})),
        remote("fallback", json!({"is_chat_fallback": true})),
    ]})
}

fn provider(root: &str, token: &str) -> codesmith_ai::Provider {
    codesmith_ai::Provider {
        base_url: root.into(),
        auth: codesmith_ai::Auth::Dynamic(Arc::new(codesmith_ai::copilot::Copilot::new(
            token.into(),
            root.into(),
            None,
        ))),
        ..Default::default()
    }
}

// provider-github-copilot.test.ts: "routes Auto through Copilot's router and
// keeps the pick until compaction"
#[tokio::test(flavor = "multi_thread")]
async fn oc_copilot_routes_auto_through_the_router_and_keeps_the_pick_until_compaction() {
    let state = Arc::new(Mutex::new(State {
        pick: "routed".into(),
        available: true,
        ..Default::default()
    }));
    let root = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let (parent, child) = ("ses_auto_parent", "ses_auto_child");
    let account = Account::from_kept(&kept("first")).unwrap();
    let p = provider(&root, "token");
    let prompt = Prompt::default();
    let now = 1_000;

    // Free plans hide every model from the picker; Auto is the one they can select.
    let models = copilot_models::parse(kept("first")["models"].as_array().unwrap());
    assert!(models.iter().all(|m| !m.picker));
    assert!(copilot_models::auto_model(&root, &models).is_some());

    let resolve = |session: &'static str, title: bool, account: &'static Account| {
        let (client, p, prompt) = (&client, &p, &prompt);
        async move {
            auto::resolve(client, p, session, title, account, prompt, now)
                .await
                .map(|x| x.model)
        }
    };
    let account: &'static Account = Box::leak(Box::new(account));
    // Titles never route.
    assert_eq!(resolve(parent, true, account).await, None);
    assert!(state.lock().unwrap().picks.is_empty());
    assert_eq!(
        resolve(parent, false, account).await.as_deref(),
        Some("routed")
    );
    assert_eq!(
        resolve(parent, false, account).await.as_deref(),
        Some("routed")
    );
    let picks: Vec<Value> = state
        .lock()
        .unwrap()
        .picks
        .iter()
        .map(|p| p.0.clone())
        .collect();
    assert_eq!(picks, [json!({"prompt": ""})]);
    assert_eq!(
        auto::token(parent, "routed").as_deref(),
        Some("token-routed")
    );
    // Copilot rejects the token for any model but the one it was issued for.
    assert_eq!(auto::token(parent, "other"), None);

    state.lock().unwrap().pick = "other".into();
    auto::compacted(parent);
    // The compaction request keeps the pick's token; the next routes again.
    assert_eq!(
        auto::token(parent, "routed").as_deref(),
        Some("token-routed")
    );
    assert_eq!(
        resolve(parent, false, account).await.as_deref(),
        Some("other")
    );
    assert_eq!(auto::token(parent, "other").as_deref(), Some("token-other"));
    assert_eq!(state.lock().unwrap().picks.len(), 2);
    assert_eq!(state.lock().unwrap().picks[1].1, "Bearer token");

    // A pick belongs to the account that made it.
    state.lock().unwrap().pick = "routed".into();
    let second: &'static Account =
        Box::leak(Box::new(Account::from_kept(&kept("second")).unwrap()));
    let p2 = provider(&root, "second");
    let routed = auto::resolve(&client, &p2, parent, false, second, &prompt, now)
        .await
        .unwrap();
    assert_eq!(routed.model, "routed");
    assert_eq!(state.lock().unwrap().picks[2].1, "Bearer second");

    // A failed router falls back to the account's chat fallback, briefly.
    state.lock().unwrap().available = false;
    let fb = auto::resolve(&client, &p2, child, false, second, &prompt, now)
        .await
        .unwrap();
    assert_eq!(fb.model, "fallback");
    assert_eq!(fb.token, None);
    assert!(fb.until <= now + 5 * 60_000);
}

#[tokio::test(flavor = "multi_thread")]
async fn copilot_auto_falls_back_when_the_router_picks_an_unlisted_model() {
    let state = Arc::new(Mutex::new(State {
        pick: "not-listed".into(),
        available: true,
        ..Default::default()
    }));
    let root = serve(state.clone()).await;
    let account = Account::from_kept(&kept("first")).unwrap();
    let pick = auto::route(
        &reqwest::Client::new(),
        &provider(&root, "token"),
        &account,
        &Prompt {
            text: "hi".into(),
            has_image: true,
        },
        0,
    )
    .await
    .unwrap();
    assert_eq!(pick.model, "fallback");
    assert_eq!(
        state.lock().unwrap().picks[0].0,
        json!({"prompt": "hi", "has_image": true})
    );
    assert!(auto::is_auto("github-copilot", "auto"));
    assert!(!auto::is_auto("openai", "auto"));
}
