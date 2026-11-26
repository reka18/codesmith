//! Bifrost as a built-in provider, against a fake gateway: which API each
//! model goes to and how the virtual key is sent, the model list per key
//! (fetched as a session starts, kept by a hash of the key, the saved one
//! only when the gateway is down), effort levels like the same model's
//! reached directly, streaming on both APIs with the gateway's request id
//! and routed model, the estimated cost's sources, and the quota's arithmetic.
//!
//! Every test runs in a temporary home and holds [`LOCK`]: the kept model
//! lists are one file per home.

mod bifrost_gateway;
mod support;

use bifrost_gateway::{model, Gateway, State};
use codesmith_ai::{Event, Message, Request, Served, Usage};
use codesmith_core::catalog::{Json, ModelsDev};
use codesmith_core::config::Config;
use codesmith_core::providers::{bifrost, live};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const KEY_A: &str = "sk-bf-test-key-alpha-0001";
const KEY_B: &str = "sk-bf-test-key-bravo-0002";

/// A catalog with a made-up Claude model and a made-up OpenAI reasoning
/// model, each priced.
fn catalog() -> Arc<ModelsDev> {
    let text = r#"{
      "anthropic": {"id": "anthropic", "name": "Anthropic", "env": ["ANTHROPIC_API_KEY"], "models": {
        "claude-test-9-1": {"id": "claude-test-9-1", "type": "chat", "name": "Claude Test 9.1",
          "family": "claude-test", "release_date": "2026-01-01",
          "modalities": {"input": ["text"], "output": ["text"]},
          "limit": {"context": 200000, "output": 64000},
          "capabilities": {"tools": {"supported": true},
            "reasoning": {"supported": true, "effort": ["low", "medium", "high", "xhigh", "max"]}},
          "cost": {"text": {"input": {"price": 3, "cache": {"read": 0.3, "write": {"5m": 3.75}}}, "output": {"price": 15}}},
          "api": {"messages": {"base_url": "https://api.anthropic.com/v1",
            "sdk": {"aisdk": "@ai-sdk/anthropic", "opencodeai": "@opencode/ai/providers/anthropic"}}}}
      }},
      "openai": {"id": "openai", "name": "OpenAI", "env": ["OPENAI_API_KEY"], "models": {
        "gpt-test-7": {"id": "gpt-test-7", "type": "chat", "name": "GPT Test 7", "family": "gpt",
          "release_date": "2026-01-01",
          "modalities": {"input": ["text"], "output": ["text"]},
          "limit": {"context": 400000, "output": 128000},
          "capabilities": {"tools": {"supported": true},
            "reasoning": {"supported": true, "effort": ["minimal", "low", "medium", "high"]}},
          "cost": {"text": {"input": {"price": 1.25, "cache": {"read": 0.125}}, "output": {"price": 10}}},
          "api": {"responses": {"base_url": "https://api.openai.com/v1",
            "sdk": {"aisdk": "@ai-sdk/openai", "opencodeai": "@opencode/ai/providers/openai/responses"}}}},
        "plain-test-1": {"id": "plain-test-1", "type": "chat", "name": "Plain Test 1",
          "release_date": "2026-01-01",
          "modalities": {"input": ["text"], "output": ["text"]},
          "limit": {"context": 100000, "output": 8000},
          "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
          "cost": {"text": {"input": {"price": 0}, "output": {"price": 0}}},
          "api": {"responses": {"base_url": "https://api.openai.com/v1",
            "sdk": {"aisdk": "@ai-sdk/openai", "opencodeai": "@opencode/ai/providers/openai/responses"}}}}
      }}
    }"#;
    let data = codesmith_core::catalog::normalize(&Json::parse(text).unwrap());
    Arc::new(ModelsDev::fixed(data))
}

fn config_with(v: Value) -> Config {
    let mut c: Config = serde_json::from_value(v).unwrap();
    c.models_dev = Some(catalog());
    c
}

fn config(url: &str, key: &str) -> Config {
    config_with(json!({"providers": {"bifrost": {"settings": {"baseURL": url, "apiKey": key}}}}))
}

/// A fresh home's kept model lists: none.
fn clear() {
    support::isolate();
    let _ = std::fs::remove_file(live::file());
}

fn request(model: &str) -> Request {
    Request {
        model: model.into(),
        messages: vec![Message::user("hi")],
        max_tokens: Some(1000),
        headers: vec![("x-codesmith-session-id".into(), "ses_test1".into())],
        ..Default::default()
    }
}

/// The request a model sends, without sending it: URL, headers, body.
fn prepared(c: &Config, spec: &str) -> (String, reqwest::header::HeaderMap, Value) {
    let m = c.resolve_model(spec).unwrap();
    let mut req = request(&m.model_id);
    req.options = m.options.clone();
    req.body = m.body.clone();
    let r = m.provider.prepare(&reqwest::Client::new(), &req).unwrap();
    let body: Value = serde_json::from_slice(r.body().unwrap().as_bytes().unwrap()).unwrap();
    (r.url().to_string(), r.headers().clone(), body)
}

async fn gateway_with(models: &[&str]) -> Gateway {
    let g = Gateway::start(State {
        tokens: (100, 20, 0, 0),
        ..Default::default()
    })
    .await;
    g.set_models(KEY_A, models.iter().map(|m| model(m)).collect());
    g
}

async fn fetch(c: &Config) -> bifrost::Outcome {
    let f = bifrost::session_start(c).expect("bifrost is set up");
    bifrost::wait(f, std::time::Duration::from_secs(10))
        .await
        .unwrap()
}

// --------------------------------------------------------------- routing

#[tokio::test]
async fn an_anthropic_model_goes_to_the_messages_api_with_the_virtual_key_and_thinking() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["anthropic/claude-test-9-1"]).await;
    let c = config(&g.url, KEY_A);
    assert_eq!(fetch(&c).await, bifrost::Outcome::Fresh(1));
    let (url, h, body) = prepared(&c, "bifrost/anthropic/claude-test-9-1#high");
    assert_eq!(url, format!("{}/anthropic/v1/messages", g.url));
    assert_eq!(h.get("x-bf-vk").unwrap(), KEY_A);
    assert!(h.get("x-api-key").is_none() && h.get("authorization").is_none());
    assert_eq!(body["model"], "anthropic/claude-test-9-1");
    assert!(body["thinking"].is_object(), "{body}");
    assert_eq!(body["output_config"]["effort"], "high", "{body}");
    assert!(body.get("reasoning").is_none());
}

#[tokio::test]
async fn a_claude_model_of_another_upstream_goes_to_chat_with_reasoning_and_claudes_levels() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["fireworks/anthropic/claude-test-9-1"]).await;
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    let (url, h, body) = prepared(&c, "bifrost/fireworks/anthropic/claude-test-9-1#xhigh");
    assert_eq!(url, format!("{}/v1/chat/completions", g.url));
    assert_eq!(h.get("x-bf-vk").unwrap(), KEY_A);
    assert!(h.get("authorization").is_none());
    assert_eq!(body["model"], "fireworks/anthropic/claude-test-9-1");
    assert_eq!(body["reasoning"], json!({"effort": "xhigh"}), "{body}");
    assert!(body.get("thinking").is_none() && body.get("reasoning_effort").is_none());
    let via = c
        .resolve_model("bifrost/fireworks/anthropic/claude-test-9-1")
        .unwrap()
        .variants;
    let direct = c
        .resolve_model("anthropic/claude-test-9-1")
        .unwrap()
        .variants;
    assert_eq!(via, direct);
    assert_eq!(direct, ["low", "medium", "high", "xhigh", "max"]);
}

#[tokio::test]
async fn effort_levels_through_bifrost_match_the_same_model_reached_directly() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[
        "anthropic/claude-test-9-1",
        "openai/gpt-test-7",
        "openai/plain-test-1",
    ])
    .await;
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    for (via, direct) in [
        (
            "bifrost/anthropic/claude-test-9-1",
            "anthropic/claude-test-9-1",
        ),
        ("bifrost/openai/gpt-test-7", "openai/gpt-test-7"),
        ("bifrost/openai/plain-test-1", "openai/plain-test-1"),
    ] {
        let a = c.resolve_model(via).unwrap();
        let b = c.resolve_model(direct).unwrap();
        assert_eq!(a.variants, b.variants, "{via} vs {direct}");
    }
    // The Claude model's variants on Messages are the direct ones, options
    // and all.
    let a = c
        .resolve_model("bifrost/anthropic/claude-test-9-1")
        .unwrap();
    let b = c.resolve_model("anthropic/claude-test-9-1").unwrap();
    let opts = |m: &codesmith_core::config::ModelRef| {
        m.variant_list
            .iter()
            .map(|(n, v)| (n.clone(), serde_json::to_value(&v.options).unwrap()))
            .collect::<Vec<_>>()
    };
    assert_eq!(opts(&a), opts(&b));
    // The OpenAI model's levels go as Chat's `reasoning` object.
    let (_, _, body) = prepared(&c, "bifrost/openai/gpt-test-7#minimal");
    assert_eq!(body["reasoning"], json!({"effort": "minimal"}));
    assert!(c
        .resolve_model("bifrost/openai/plain-test-1")
        .unwrap()
        .variants
        .is_empty());
}

#[tokio::test]
async fn the_gateways_reasoning_metadata_comes_before_the_catalog() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    g.set_models(
        KEY_A,
        vec![
            json!({"id": "openai/gpt-test-7", "reasoning": {"supported_efforts": ["high", "low"]}}),
            json!({"id": "openrouter/some/thinker", "supported_parameters": ["reasoning", "tools"]}),
            json!({"id": "openrouter/some/plain", "supported_parameters": ["tools"]}),
            json!({"id": "groq/unknown-model"}),
        ],
    );
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    let v = |s: &str| c.resolve_model(s).unwrap().variants;
    assert_eq!(v("bifrost/openai/gpt-test-7"), ["low", "high"]);
    assert_eq!(
        v("bifrost/openrouter/some/thinker"),
        ["low", "medium", "high"]
    );
    assert!(v("bifrost/openrouter/some/plain").is_empty());
    // Nothing known: the protocol's family rules (a gateway id on Chat).
    assert_eq!(v("bifrost/groq/unknown-model"), ["low", "medium", "high"]);
    let (_, _, body) = prepared(&c, "bifrost/groq/unknown-model#low");
    assert_eq!(body["reasoning"], json!({"effort": "low"}));
}

#[tokio::test]
async fn a_models_protocol_setting_wins_for_bifrost_and_any_provider() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7", "anthropic/claude-test-9-1"]).await;
    let c = config_with(json!({"providers": {
        "bifrost": {"settings": {"baseURL": format!("{}/v1/", g.url), "apiKey": KEY_A},
            "models": {
                "openai/gpt-test-7": {"protocol": "openai-responses"},
                "anthropic/claude-test-9-1": {"protocol": "openai-chat"}
            }},
        "custom": {"protocol": "openai-chat", "settings": {"baseURL": "http://127.0.0.1:9/v1", "apiKey": "k"},
            "models": {"claude-ish": {"protocol": "anthropic-messages"}}}
    }}));
    let m = c.resolve_model("bifrost/openai/gpt-test-7").unwrap();
    assert_eq!(m.provider.protocol, codesmith_ai::Protocol::OpenaiResponses);
    assert_eq!(m.provider.base_url, format!("{}/v1", g.url));
    let m = c
        .resolve_model("bifrost/anthropic/claude-test-9-1")
        .unwrap();
    assert_eq!(m.provider.protocol, codesmith_ai::Protocol::OpenaiChat);
    assert_eq!(m.provider.base_url, format!("{}/v1", g.url));
    let m = c.resolve_model("custom/claude-ish").unwrap();
    assert_eq!(
        m.provider.protocol,
        codesmith_ai::Protocol::AnthropicMessages
    );
    let m = c.resolve_model("custom/other").unwrap();
    assert_eq!(m.provider.protocol, codesmith_ai::Protocol::OpenaiChat);
}

#[tokio::test]
async fn multi_segment_ids_round_trip_through_the_list_the_spec_and_the_body() {
    let _g = LOCK.lock().await;
    clear();
    let ids = [
        "openai/gpt-test-7",
        "fireworks/anthropic/claude-test-9-1",
        "vertex/anthropic/models/claude-test-9-1",
    ];
    let g = gateway_with(&ids).await;
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    let listed = c.list_models("bifrost", &["chat"]);
    for id in ids {
        assert!(listed.contains(&id.to_string()), "{listed:?}");
        let spec = format!("bifrost/{id}");
        let m = c.resolve_model(&spec).unwrap();
        assert_eq!(m.model_id, id);
        let (_, _, body) = prepared(&c, &spec);
        assert_eq!(body["model"], id);
        // A variant suffix stays off the id.
        let m = c.resolve_model(&format!("{spec}#low")).unwrap();
        assert_eq!(m.model_id, id);
    }
    // The picker's rows name each model under bifrost.
    let rows = codesmith_core::plugin::codesmith::available(&c, "chat");
    let four = rows
        .iter()
        .find(|r| r.provider_id == "bifrost" && r.id == ids[2])
        .expect("listed in the picker");
    assert_eq!(four.provider_name, "Bifrost");
}

// ------------------------------------------------------------ model list

#[tokio::test]
async fn each_key_gets_its_own_list_and_a_model_outside_it_is_refused() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7", "anthropic/claude-test-9-1"]).await;
    g.set_models(KEY_B, vec![model("groq/only-for-b")]);
    let a = config(&g.url, KEY_A);
    fetch(&a).await;
    assert_eq!(
        a.list_models("bifrost", &["chat"]),
        ["anthropic/claude-test-9-1", "openai/gpt-test-7"]
    );
    assert!(a.resolve_model("bifrost/openai/gpt-test-7").is_ok());
    let e = a
        .resolve_model("bifrost/groq/only-for-b")
        .unwrap_err()
        .to_string();
    assert!(e.contains("isn't among the 2 models"), "{e}");
    // Under key B the list is B's, never A's.
    let b = config(&g.url, KEY_B);
    assert!(
        b.list_models("bifrost", &["chat"]).is_empty(),
        "A's list is not shown under B"
    );
    fetch(&b).await;
    assert_eq!(b.list_models("bifrost", &["chat"]), ["groq/only-for-b"]);
    assert!(b.resolve_model("bifrost/openai/gpt-test-7").is_err());
    // Each list request carried its own key, never in the URL.
    for s in g.seen().iter().filter(|s| s.path == "/v1/models") {
        assert!(matches!(s.header("x-bf-vk"), Some(KEY_A) | Some(KEY_B)));
        assert!(!s.query.contains("sk-bf"));
    }
    // The kept file holds hashes of the keys, never the keys.
    let text = std::fs::read_to_string(live::file()).unwrap();
    assert!(!text.contains(KEY_A) && !text.contains(KEY_B));
    assert!(text.contains(&bifrost::account(&g.url, Some(KEY_B))));
}

#[tokio::test]
async fn a_configured_model_is_allowed_whatever_the_list_says() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7"]).await;
    let c = config_with(json!({"providers": {"bifrost": {
        "settings": {"baseURL": g.url, "apiKey": KEY_A},
        "models": {"router/my-alias": {"name": "My alias"}}}}}));
    fetch(&c).await;
    assert!(c.resolve_model("bifrost/router/my-alias").is_ok());
}

#[tokio::test]
async fn the_list_is_read_page_by_page() {
    let _g = LOCK.lock().await;
    clear();
    let ids: Vec<String> = (0..7).map(|i| format!("openai/paged-{i}")).collect();
    let g = Gateway::start(State {
        page: 3,
        ..Default::default()
    })
    .await;
    g.set_models(KEY_A, ids.iter().map(|i| model(i)).collect());
    let c = config(&g.url, KEY_A);
    assert_eq!(fetch(&c).await, bifrost::Outcome::Fresh(7));
    let pages: Vec<String> = g
        .seen()
        .iter()
        .filter(|s| s.path == "/v1/models")
        .map(|s| s.query.clone())
        .collect();
    assert_eq!(
        pages,
        [
            "page_size=1000",
            "page_size=1000&page_token=3",
            "page_size=1000&page_token=6"
        ]
    );
    let mut listed = c.list_models("bifrost", &["chat"]);
    listed.sort();
    assert_eq!(listed, ids);
}

#[tokio::test]
async fn every_session_start_fetches_the_list_afresh() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7"]).await;
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    assert_eq!(c.list_models("bifrost", &["chat"]), ["openai/gpt-test-7"]);
    // The gateway's list changes; the next session sees it, however fresh
    // the kept one was.
    g.set_models(
        KEY_A,
        vec![model("openai/gpt-test-7"), model("mistral/new-one")],
    );
    assert_eq!(fetch(&c).await, bifrost::Outcome::Fresh(2));
    assert_eq!(
        c.list_models("bifrost", &["chat"]),
        ["mistral/new-one", "openai/gpt-test-7"]
    );
    let lists = g.seen().iter().filter(|s| s.path == "/v1/models").count();
    assert_eq!(lists, 2);
}

#[tokio::test]
async fn a_gateway_down_at_the_start_falls_back_to_the_saved_list_with_a_notice() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7"]).await;
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    g.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let o = fetch(&c).await;
    let bifrost::Outcome::Saved { saved, .. } = &o else {
        panic!("{o:?}");
    };
    assert!(*saved > 0);
    let n = o.notice().unwrap();
    assert!(n.contains("using the list saved"), "{n}");
    assert!(!n.contains(KEY_A));
    // The saved list still answers, and still refuses what's not on it.
    assert_eq!(c.list_models("bifrost", &["chat"]), ["openai/gpt-test-7"]);
    assert!(c.resolve_model("bifrost/groq/elsewhere").is_err());
    // With no saved list for the key, it says so; models aren't checked.
    let other = config(&g.url, KEY_B);
    let o = fetch(&other).await;
    assert!(matches!(o, bifrost::Outcome::Missing { .. }), "{o:?}");
    assert!(o.notice().unwrap().contains("none is saved"));
    assert!(other.resolve_model("bifrost/groq/elsewhere").is_ok());
}

#[tokio::test]
async fn a_rejected_key_keeps_the_saved_list_and_never_shows_the_key() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7"]).await;
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    g.with(|s| s.list_status = Some(500));
    let o = fetch(&c).await;
    assert!(matches!(o, bifrost::Outcome::Saved { .. }), "{o:?}");
    // The kept list was not replaced by the failure.
    assert_eq!(c.list_models("bifrost", &["chat"]), ["openai/gpt-test-7"]);
    // An unknown key's error, which the gateway may echo, is redacted.
    let unknown = "sk-bf-not-a-known-key-9999";
    let m = config(&g.url, unknown).resolve_model("bifrost/_").unwrap();
    g.with(|s| s.list_status = None);
    let e = bifrost::refresh(&reqwest::Client::new(), &live::file(), &m.provider, &g.url)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("401"), "{e}");
    assert!(!format!("{e:#}").contains(unknown));
    assert!(!format!("{:?}", m.provider).contains(unknown));
}

#[tokio::test]
async fn concurrent_session_starts_share_one_fetch() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7"]).await;
    let c = config(&g.url, KEY_A);
    let fs: Vec<_> = (0..20)
        .map(|_| bifrost::session_start(&c).unwrap())
        .collect();
    for f in fs {
        assert_eq!(
            bifrost::wait(f, std::time::Duration::from_secs(10)).await,
            Some(bifrost::Outcome::Fresh(1))
        );
    }
    let lists = g.seen().iter().filter(|s| s.path == "/v1/models").count();
    assert_eq!(lists, 1);
}

#[tokio::test]
async fn a_long_list_is_kept_whole() {
    let _g = LOCK.lock().await;
    clear();
    let g = Gateway::start(State {
        page: 250,
        ..Default::default()
    })
    .await;
    let ids: Vec<Value> = (0..4000)
        .map(|i| model(&format!("openai/m-{i:05}")))
        .collect();
    g.set_models(KEY_A, ids);
    let c = config(&g.url, KEY_A);
    assert_eq!(fetch(&c).await, bifrost::Outcome::Fresh(4000));
    assert_eq!(c.list_models("bifrost", &["chat"]).len(), 4000);
    assert!(c.resolve_model("bifrost/openai/m-03999").is_ok());
}

// ------------------------------------------------------------ streaming

async fn stream(c: &Config, spec: &str) -> (Vec<Event>, String) {
    let m = c.resolve_model(spec).unwrap();
    let s = m
        .provider
        .stream(&reqwest::Client::new(), &request(&m.model_id))
        .await
        .unwrap();
    let events: Vec<Event> = s.map(|e| e.unwrap()).collect().await;
    let text = events
        .iter()
        .filter_map(|e| match e {
            Event::TextDelta(d) => Some(d.as_str()),
            _ => None,
        })
        .collect();
    (events, text)
}

fn served(events: &[Event]) -> Served {
    match events.first() {
        Some(Event::Served(s)) => s.clone(),
        other => panic!("the stream starts with who served it: {other:?}"),
    }
}

fn usage(events: &[Event]) -> Usage {
    events
        .iter()
        .find_map(|e| match e {
            Event::Finish { usage, .. } => Some(*usage),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn both_apis_stream_with_the_gateways_request_id_and_routed_model() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["anthropic/claude-test-9-1", "openai/gpt-test-7"]).await;
    g.with(|s| {
        s.routed = Some(("openai".into(), "gpt-test-7-mini".into()));
        s.cost = Some(json!({"input_cost": 0.001, "output_cost": 0.002, "total_cost": 0.003}));
        s.tokens = (100, 20, 30, 0);
    });
    let c = config(&g.url, KEY_A);
    let (events, text) = stream(&c, "bifrost/openai/gpt-test-7").await;
    assert_eq!(text, "Hello there.");
    let s = served(&events);
    let seen = g.seen();
    let chat = seen
        .iter()
        .find(|s| s.path == "/v1/chat/completions")
        .unwrap();
    let sent = chat.header("x-request-id").unwrap().to_string();
    assert_eq!(s.request_id, Some(format!("bf-{sent}")));
    assert_eq!(s.provider.as_deref(), Some("openai"));
    assert_eq!(s.model.as_deref(), Some("gpt-test-7-mini"));
    assert_eq!(chat.header("x-bf-vk"), Some(KEY_A));
    assert_eq!(chat.header("x-bf-agent-correlation-id"), Some("ses_test1"));
    assert_eq!(chat.body["stream"], true);
    let u = usage(&events);
    assert_eq!((u.input, u.output, u.cache_read), (100, 20, 30));
    assert_eq!(u.cost, Some(0.003));

    let (events, text) = stream(&c, "bifrost/anthropic/claude-test-9-1").await;
    assert_eq!(text, "Hello from Claude.");
    let s = served(&events);
    let seen = g.seen();
    let msgs = seen
        .iter()
        .find(|s| s.path == "/anthropic/v1/messages")
        .unwrap();
    assert_eq!(msgs.header("x-bf-vk"), Some(KEY_A));
    assert_eq!(msgs.body["model"], "anthropic/claude-test-9-1");
    let id = msgs.header("x-request-id").unwrap();
    assert_ne!(id, sent, "every call has its own id");
    assert_eq!(s.request_id, Some(format!("bf-{id}")));
}

#[tokio::test]
async fn an_older_gateway_without_the_header_keeps_the_id_that_was_sent() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7"]).await;
    g.with(|s| s.no_request_id = true);
    let c = config(&g.url, KEY_A);
    let (events, _) = stream(&c, "bifrost/openai/gpt-test-7").await;
    let s = served(&events);
    let sent = g
        .seen()
        .last()
        .unwrap()
        .header("x-request-id")
        .unwrap()
        .to_string();
    assert_eq!(s.request_id.as_deref(), Some(sent.as_str()));
    assert_eq!(s.provider, None);
    // Every call gets a fresh id, never one used before.
    let mut ids = std::collections::HashSet::new();
    for _ in 0..25 {
        stream(&c, "bifrost/openai/gpt-test-7").await;
        let id = g
            .seen()
            .last()
            .unwrap()
            .header("x-request-id")
            .unwrap()
            .to_string();
        assert!(ids.insert(id));
    }
}

#[tokio::test]
async fn another_providers_stream_has_no_served_event_and_no_request_id() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    let c = config_with(json!({"providers": {"plain": {"protocol": "openai-chat",
        "settings": {"baseURL": format!("{}/v1", g.url), "apiKey": "k"}}}}));
    let (events, _) = stream(&c, "plain/m").await;
    assert!(!events.iter().any(|e| matches!(e, Event::Served(_))));
    assert!(g.seen()[0].header("x-request-id").is_none());
}

// ------------------------------------------------------------ cost

fn served_by(p: &str, m: &str) -> Served {
    Served {
        request_id: Some("r".into()),
        provider: Some(p.into()),
        model: Some(m.into()),
    }
}

fn tokens(input: u64, output: u64, read: u64, write: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read: read,
        cache_write: write,
        ..Default::default()
    }
}

fn close(a: Option<f64>, b: f64) {
    let a = a.unwrap_or_else(|| panic!("no estimate, wanted {b}"));
    assert!((a - b).abs() < 1e-12, "{a} != {b}");
}

#[tokio::test]
async fn the_estimate_prices_from_config_then_the_gateway_then_the_catalog() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    g.set_models(
        KEY_A,
        vec![
            json!({"id": "openai/gpt-test-7", "pricing": {"prompt": "0.000002", "completion": "0.000020",
                   "input_cache_read": "0.0000002", "input_cache_write": "0"}}),
            json!({"id": "anthropic/claude-test-9-1"}),
            json!({"id": "groq/free-looking", "pricing": {"prompt": "0", "completion": "0"}}),
            json!({"id": "groq/per-call", "pricing": {"prompt": "0.000001", "completion": "0.000001", "request": "0.01"}}),
        ],
    );
    let c = config_with(json!({"providers": {"bifrost": {
        "settings": {"baseURL": g.url, "apiKey": KEY_A},
        "models": {"openai/gpt-test-7": {"cost": {"input": 5.0}}}}}}));
    fetch(&c).await;
    let u = tokens(1_000_000, 100_000, 0, 0);
    // Input from config ($5/M), output from the gateway ($20/M).
    close(c.estimate("bifrost/openai/gpt-test-7", None, &u), 5.0 + 2.0);
    // Cache reads: the gateway's $0.2/M.
    close(
        c.estimate(
            "bifrost/openai/gpt-test-7",
            None,
            &tokens(0, 0, 1_000_000, 0),
        ),
        0.2,
    );
    // The gateway's 0 is unpriced, and the catalog has no cache write for
    // this model: no estimate.
    assert_eq!(
        c.estimate("bifrost/openai/gpt-test-7", None, &tokens(10, 10, 0, 10)),
        None
    );
    // Not priced by the gateway: the catalog's upstream entry ($3, $15).
    close(
        c.estimate("bifrost/anthropic/claude-test-9-1", None, &u),
        3.0 + 1.5,
    );
    // Zero prices everywhere are unpriced, never free.
    assert_eq!(c.estimate("bifrost/groq/free-looking", None, &u), None);
    // A per-request price is added once per step.
    close(
        c.estimate("bifrost/groq/per-call", None, &tokens(1_000_000, 0, 0, 0)),
        1.0 + 0.01,
    );
}

#[tokio::test]
async fn the_estimate_prices_the_model_the_gateway_says_ran() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    g.set_models(
        KEY_A,
        vec![
            json!({"id": "openai/gpt-test-7", "pricing": {"prompt": "0.00001", "completion": "0.00001"}}),
            json!({"id": "openai/fallback-x", "pricing": {"prompt": "0.000001", "completion": "0.000001"}}),
        ],
    );
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    let u = tokens(1_000_000, 0, 0, 0);
    close(c.estimate("bifrost/openai/gpt-test-7", None, &u), 10.0);
    close(
        c.estimate(
            "bifrost/openai/gpt-test-7",
            Some(&served_by("openai", "fallback-x")),
            &u,
        ),
        1.0,
    );
    // Routed to a model nothing prices: no estimate rather than the
    // requested model's price.
    assert_eq!(
        c.estimate(
            "bifrost/openai/gpt-test-7",
            Some(&served_by("azure", "unknown")),
            &u
        ),
        None
    );
    // Without routing headers, the requested model.
    let partial = Served {
        provider: None,
        model: Some("fallback-x".into()),
        request_id: None,
    };
    close(
        c.estimate("bifrost/openai/gpt-test-7", Some(&partial), &u),
        10.0,
    );
}

#[tokio::test]
async fn any_provider_with_a_price_gets_an_estimate() {
    let _g = LOCK.lock().await;
    clear();
    let c = config_with(json!({"providers": {"fake": {"protocol": "openai-chat",
        "settings": {"baseURL": "http://127.0.0.1:9/v1"},
        "models": {"m": {"cost": {"input": 1.0, "output": 2.0}}, "free": {"cost": {"input": 0.0}}}}}}));
    close(
        c.estimate("fake/m", None, &tokens(1_000_000, 1_000_000, 0, 0)),
        3.0,
    );
    // A count with no rate leaves the step unpriced.
    assert_eq!(c.estimate("fake/m", None, &tokens(1, 1, 1, 0)), None);
    assert_eq!(c.estimate("fake/free", None, &tokens(1, 0, 0, 0)), None);
    // The catalog prices a direct model; routing headers mean nothing here.
    close(
        c.estimate(
            "openai/gpt-test-7",
            Some(&served_by("x", "y")),
            &tokens(1_000_000, 0, 1_000_000, 0),
        ),
        1.25 + 0.125,
    );
}

// ------------------------------------------------------------ quota

fn budget(id: &str, usage: f64, reset: &str) -> Value {
    json!({"id": id, "max_limit": 100.0, "reset_duration": "1M", "last_reset": reset,
           "current_usage": usage, "per_model_usage": []})
}

#[test]
fn the_quota_difference_is_one_budgets_move_within_one_cycle() {
    let r = "2026-11-01T00:00:00Z";
    let before =
        bifrost::budgets(&json!({"budgets": [budget("b2", 5.0, r), budget("b1", 1.0, r)]}));
    let after =
        bifrost::budgets(&json!({"budgets": [budget("b1", 1.25, r), budget("b2", 5.25, r)]}));
    assert_eq!(bifrost::spent(&before, &after), Some(("b1".into(), 0.25)));
    // No budget: nothing.
    assert_eq!(bifrost::spent(&[], &[]), None);
    assert!(bifrost::budgets(&json!({"budgets": []})).is_empty());
    // A cycle that reset in between is left out; another budget still
    // counts.
    let reset = bifrost::budgets(
        &json!({"budgets": [budget("b1", 0.1, "2026-11-02T00:00:00Z"), budget("b2", 5.5, r)]}),
    );
    assert_eq!(bifrost::spent(&before, &reset), Some(("b2".into(), 0.5)));
    let all_reset =
        bifrost::budgets(&json!({"budgets": [budget("b1", 0.1, "x"), budget("b2", 0.1, "y")]}));
    assert_eq!(bifrost::spent(&before, &all_reset), None);
    // Malformed budgets are skipped.
    assert!(
        bifrost::budgets(&json!({"budgets": [{"id": "x", "current_usage": "1"}, 3]})).is_empty()
    );
}

#[tokio::test]
async fn a_quota_read_that_fails_or_has_no_budget_says_nothing() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    let c = config(&g.url, KEY_A);
    let m = c.resolve_model("bifrost/_").unwrap();
    let client = reqwest::Client::new();
    // No quota route (404): no reading.
    assert_eq!(bifrost::quota(&client, &m.provider, &g.url).await, None);
    g.with(|s| s.quota = vec![(200, json!({"budgets": []}))]);
    assert_eq!(
        bifrost::quota(&client, &m.provider, &g.url).await,
        Some(vec![])
    );
    let seen = g.seen();
    let q = seen.iter().rfind(|s| s.path.ends_with("/quota")).unwrap();
    assert_eq!(q.header("x-bf-vk"), Some(KEY_A));
    // A gateway that's gone: no reading, quickly.
    g.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let t = std::time::Instant::now();
    assert_eq!(bifrost::quota(&client, &m.provider, &g.url).await, None);
    assert!(t.elapsed() < bifrost::QUOTA_TIMEOUT + std::time::Duration::from_millis(500));
}

// ------------------------------------------------------------ setup

#[tokio::test]
async fn the_url_and_key_can_come_from_the_environment_alone() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7"]).await;
    std::env::set_var(bifrost::URL_VAR, format!("{}/v1", g.url));
    std::env::set_var(bifrost::KEY_VAR, KEY_A);
    let c = config_with(json!({}));
    let ready = bifrost::ready(&c);
    let outcome = bifrost::session_start(&c)
        .map(|f| async move { bifrost::wait(f, std::time::Duration::from_secs(10)).await });
    let outcome = match outcome {
        Some(f) => f.await,
        None => None,
    };
    std::env::remove_var(bifrost::URL_VAR);
    std::env::remove_var(bifrost::KEY_VAR);
    assert!(ready);
    assert_eq!(outcome, Some(bifrost::Outcome::Fresh(1)));
    // Without a URL there is no default host.
    let c = config_with(json!({"providers": {"bifrost": {"settings": {"apiKey": KEY_A}}}}));
    assert!(!bifrost::ready(&c));
    let e = c.resolve_model("bifrost/openai/x").unwrap_err().to_string();
    assert!(e.contains(bifrost::URL_VAR), "{e}");
    assert!(bifrost::session_start(&c).is_none());
}

#[test]
fn an_api_key_header_setting_overrides_x_bf_vk() {
    let c = config_with(json!({"providers": {"bifrost": {"settings": {
        "baseURL": "http://127.0.0.1:9", "apiKey": KEY_A, "apiKeyHeader": "authorization"}}}}));
    support::isolate();
    let m = c.resolve_model("bifrost/openai/x").unwrap();
    assert_eq!(
        m.provider.auth,
        codesmith_ai::Auth::Header("authorization".into())
    );
}

// ------------------------------------------------------------ properties

#[test]
fn the_api_depends_only_on_the_first_upstream_and_ids_pass_through() {
    use codesmith_ai::Protocol::*;
    let segs = [
        "anthropic",
        "openai",
        "fireworks",
        "vertex",
        "a.b",
        "x-1",
        "models",
        "claude-z-1",
    ];
    // Every id of up to four segments drawn from `segs`, deterministically.
    let mut n = 0;
    let mut stack: Vec<Vec<&str>> = segs.iter().map(|s| vec![*s]).collect();
    while let Some(parts) = stack.pop() {
        let id = parts.join("/");
        let want = if parts[0] == "anthropic" {
            AnthropicMessages
        } else {
            OpenaiChat
        };
        assert_eq!(bifrost::protocol_for(&id, None), want, "{id}");
        assert_eq!(
            bifrost::protocol_for(&id, Some(OpenaiResponses)),
            OpenaiResponses
        );
        n += 1;
        if parts.len() < 4 {
            for s in segs.iter().step_by(2) {
                let mut p = parts.clone();
                p.push(s);
                stack.push(p);
            }
        }
    }
    assert!(n > 300, "{n}");
}

#[test]
fn the_root_url_is_normalized() {
    for (given, want) in [
        ("http://gw:8080", "http://gw:8080"),
        ("http://gw:8080/", "http://gw:8080"),
        ("http://gw:8080/v1", "http://gw:8080"),
        ("http://gw:8080/v1/", "http://gw:8080"),
        (" https://gw.example/base/v1 ", "https://gw.example/base"),
    ] {
        assert_eq!(bifrost::normalize_root(given), want);
    }
    assert_eq!(
        bifrost::surface("http://gw", codesmith_ai::Protocol::AnthropicMessages),
        "http://gw/anthropic/v1"
    );
    assert_eq!(
        bifrost::surface("http://gw", codesmith_ai::Protocol::OpenaiChat),
        "http://gw/v1"
    );
}
