//! Bifrost as a built-in provider, against a fake gateway: which API each
//! model goes to and how the virtual key is sent, the model list per key
//! (fetched as a session starts, kept by a hash of the key, the saved one
//! only when the gateway is down), effort levels like the same model's
//! reached directly, streaming on both APIs with the gateway's request id
//! and routed model, the estimated cost's sources, the quota's arithmetic,
//! and which route each surface takes (`api`, the per-surface URLs, their
//! variables) with a clear error for a route the gateway doesn't serve.
//!
//! Every test runs in a temporary home and holds [`LOCK`]: the kept model
//! lists are one file per home.

mod bifrost_gateway;
mod support;

use bifrost_gateway::{model, Gateway, State, INFERENCE_ONLY};
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
      "google": {"id": "google", "name": "Google", "env": ["GEMINI_API_KEY"], "models": {
        "gemini-test-3": {"id": "gemini-test-3", "type": "chat", "name": "Gemini Test 3", "family": "gemini",
          "release_date": "2026-01-01",
          "modalities": {"input": ["text"], "output": ["text"]},
          "limit": {"context": 1000000, "output": 65536},
          "capabilities": {"tools": {"supported": true},
            "reasoning": {"supported": true, "effort": ["minimal", "low", "medium", "high"]}},
          "cost": {"text": {"input": {"price": 1, "cache": {"read": 0.1}}, "output": {"price": 5}}},
          "api": {"generate-content": {"base_url": "https://generativelanguage.googleapis.com/v1beta",
            "sdk": {"aisdk": "@ai-sdk/google", "opencodeai": "@opencode/ai/providers/google"}}}}
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
    assert!(text.contains(&bifrost::account(
        &format!("{}/v1/models", g.url),
        Some(KEY_B)
    )));
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
    // With no saved list for the key, every model is refused: nothing says
    // what the key allows.
    let other = config(&g.url, KEY_B);
    assert!(
        other.resolve_model("bifrost/groq/elsewhere").is_ok(),
        "before anything has asked the gateway, nothing is checked"
    );
    let o = fetch(&other).await;
    assert!(matches!(o, bifrost::Outcome::Missing { .. }), "{o:?}");
    assert!(o.notice().unwrap().contains("none is saved"));
    let e = other
        .resolve_model("bifrost/groq/elsewhere")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!(
            "the Bifrost gateway at {} couldn't be reached to list the models this key allows",
            g.url
        )) && e.contains("there's no saved list yet")
            && e.contains("check the URL, the key and the gateway, then try again"),
        "{e}"
    );
    assert!(!e.contains(KEY_B));
    // The provider itself still resolves, for listing.
    assert!(other.resolve_model("bifrost/_").is_ok());
    // Once the gateway answers again, the key's list is used.
    let g2 = gateway_with(&[]).await;
    let _ = g2;
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
    let urls = bifrost::urls_at(
        &g.url,
        &c.provider("bifrost").unwrap().settings,
        &codesmith_core::providers::process_env(),
    )
    .unwrap();
    let e = bifrost::refresh(&reqwest::Client::new(), &live::file(), &m.provider, &urls)
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
    let q = format!("{}/api/governance/virtual-keys/quota", g.url);
    assert_eq!(bifrost::quota(&client, &m.provider, &q).await, None);
    g.with(|s| s.quota = vec![(200, json!({"budgets": []}))]);
    assert_eq!(bifrost::quota(&client, &m.provider, &q).await, Some(vec![]));
    let seen = g.seen();
    let read = seen.iter().rfind(|s| s.path.ends_with("/quota")).unwrap();
    assert_eq!(read.header("x-bf-vk"), Some(KEY_A));
    // A gateway that's gone: no reading, quickly.
    g.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let t = std::time::Instant::now();
    assert_eq!(bifrost::quota(&client, &m.provider, &q).await, None);
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

// ------------------------------------------------- Responses and Gemini

/// Bifrost with the Responses API for an OpenAI model and Gemini's own
/// API for a Gemini one, by their `protocol` setting.
fn native(url: &str) -> Config {
    config_with(json!({"providers": {"bifrost": {
    "settings": {"baseURL": url, "apiKey": KEY_A},
    "models": {
        "openai/gpt-test-7": {"protocol": "openai-responses"},
        "gemini/gemini-test-3": {"protocol": "google-gemini"},
        "vertex/gemini-test-3": {"protocol": "google-gemini"}
    }}}}))
}

#[tokio::test]
async fn the_responses_api_goes_to_v1_responses_with_the_key_and_effort() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7"]).await;
    let c = native(&g.url);
    fetch(&c).await;
    let (url, h, body) = prepared(&c, "bifrost/openai/gpt-test-7#high");
    assert_eq!(url, format!("{}/v1/responses", g.url));
    assert_eq!(h.get("x-bf-vk").unwrap(), KEY_A);
    assert!(h.get("authorization").is_none());
    assert_eq!(body["model"], "openai/gpt-test-7");
    assert_eq!(body["reasoning"]["effort"], "high", "{body}");
    assert_eq!(body["stream"], true);
    // The same levels as the model reached directly (OpenAI's Responses).
    assert_eq!(
        c.resolve_model("bifrost/openai/gpt-test-7")
            .unwrap()
            .variants,
        c.resolve_model("openai/gpt-test-7").unwrap().variants
    );
    // Over HTTP: a gateway's Responses route is not OpenAI's WebSocket.
    assert!(c
        .resolve_model("bifrost/openai/gpt-test-7")
        .unwrap()
        .websocket
        .is_none());
}

#[tokio::test]
async fn gemini_goes_to_the_genai_route_with_the_key_and_thinking_config() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["gemini/gemini-test-3", "vertex/gemini-test-3"]).await;
    let c = native(&g.url);
    fetch(&c).await;
    for id in ["gemini/gemini-test-3", "vertex/gemini-test-3"] {
        let (url, h, body) = prepared(&c, &format!("bifrost/{id}#low"));
        assert_eq!(
            url,
            format!(
                "{}/genai/v1beta/models/{id}:streamGenerateContent?alt=sse",
                g.url
            )
        );
        assert_eq!(h.get("x-bf-vk").unwrap(), KEY_A);
        assert!(h.get("x-goog-api-key").is_none());
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true, "thinkingLevel": "low"}),
            "{body}"
        );
        // The catalog knows the model as Google's (`gemini/…`), with its
        // levels; under Vertex it doesn't, and the family rules apply.
        let via = c.resolve_model(&format!("bifrost/{id}")).unwrap().variants;
        if id.starts_with("gemini/") {
            assert_eq!(
                via,
                c.resolve_model("google/gemini-test-3").unwrap().variants
            );
        } else {
            assert_eq!(via, ["low", "medium", "high"]);
        }
    }
}

#[tokio::test]
async fn responses_and_gemini_stream_with_usage_cost_request_id_and_routed_model() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["openai/gpt-test-7", "gemini/gemini-test-3"]).await;
    g.with(|s| {
        s.routed = Some(("vertex".into(), "gemini-test-3".into()));
        s.cost = Some(json!({"input_cost": 0.001, "output_cost": 0.001, "total_cost": 0.002}));
        s.tokens = (100, 20, 30, 0);
    });
    let c = native(&g.url);
    fetch(&c).await;
    let (events, text) = stream(&c, "bifrost/openai/gpt-test-7").await;
    assert_eq!(text, "Hello there.");
    let s = served(&events);
    let seen = g.seen();
    let r = seen.iter().find(|s| s.path == "/v1/responses").unwrap();
    assert_eq!(r.header("x-bf-vk"), Some(KEY_A));
    assert_eq!(r.header("x-bf-agent-correlation-id"), Some("ses_test1"));
    let sent = r.header("x-request-id").unwrap().to_string();
    assert_eq!(s.request_id, Some(format!("bf-{sent}")));
    assert_eq!(s.provider.as_deref(), Some("vertex"));
    let u = usage(&events);
    assert_eq!((u.input, u.output, u.cache_read), (100, 20, 30));
    assert_eq!(u.cost, Some(0.002));

    let (events, text) = stream(&c, "bifrost/gemini/gemini-test-3").await;
    assert_eq!(text, "Hello from Gemini.");
    let s = served(&events);
    let seen = g.seen();
    let gm = seen
        .iter()
        .find(|s| s.path.starts_with("/genai/v1beta/models/"))
        .unwrap();
    assert_eq!(gm.header("x-bf-vk"), Some(KEY_A));
    let id = gm.header("x-request-id").unwrap();
    assert_ne!(id, sent);
    assert_eq!(s.request_id, Some(format!("bf-{id}")));
    assert_eq!(s.model.as_deref(), Some("gemini-test-3"));
    let u = usage(&events);
    assert_eq!((u.input, u.output, u.cache_read), (100, 20, 30));
    // Gemini's native answer carries no billed cost.
    assert_eq!(u.cost, None);
    // Priced on the routed model, from the catalog's Google entry.
    let routed = served_by("gemini", "gemini-test-3");
    close(
        c.estimate(
            "bifrost/gemini/gemini-test-3",
            Some(&routed),
            &tokens(1_000_000, 1_000_000, 1_000_000, 0),
        ),
        1.0 + 5.0 + 0.1,
    );
}

fn tool_calls(events: &[Event]) -> Vec<(String, Value)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn tool_calls_come_through_every_api() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[
        "openai/gpt-test-7",
        "anthropic/claude-test-9-1",
        "gemini/gemini-test-3",
        "groq/chat-model",
    ])
    .await;
    let args = json!({"path": "src/lib.rs"});
    g.with(|s| s.tool_call = Some(("read".into(), args.clone())));
    let c = native(&g.url);
    fetch(&c).await;
    for spec in [
        "bifrost/openai/gpt-test-7",
        "bifrost/anthropic/claude-test-9-1",
        "bifrost/gemini/gemini-test-3",
        "bifrost/groq/chat-model",
    ] {
        let (events, _) = stream(&c, spec).await;
        assert_eq!(
            tool_calls(&events),
            [("read".to_string(), args.clone())],
            "{spec}"
        );
        assert!(matches!(events.first(), Some(Event::Served(_))), "{spec}");
    }
    let paths: Vec<String> = g
        .seen()
        .iter()
        .filter(|s| s.method == "POST")
        .map(|s| s.path.clone())
        .collect();
    assert_eq!(
        paths,
        [
            "/v1/responses",
            "/anthropic/v1/messages",
            "/genai/v1beta/models/gemini/gemini-test-3:streamGenerateContent",
            "/v1/chat/completions"
        ]
    );
}

#[tokio::test]
async fn gemini_budget_and_none_levels_are_spelled_for_gemini() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    g.set_models(
        KEY_A,
        vec![
            json!({"id": "gemini/thinker-x", "reasoning": {"supported_efforts": ["none", "high"]}}),
        ],
    );
    let c = config_with(json!({"providers": {"bifrost": {
        "settings": {"baseURL": g.url, "apiKey": KEY_A},
        "models": {"gemini/thinker-x": {"protocol": "google-gemini"}}}}}));
    fetch(&c).await;
    let (_, _, body) = prepared(&c, "bifrost/gemini/thinker-x#high");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high",
        "{body}"
    );
    let (_, _, body) = prepared(&c, "bifrost/gemini/thinker-x#none");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0,
        "{body}"
    );
}

// ------------------------------------------------------------ routes

/// A gateway that serves inference only on Bifrost's integration routes,
/// with every kind of model listed.
async fn integration_gateway() -> Gateway {
    let g = Gateway::start(
        State {
            tokens: (100, 20, 30, 0),
            routed: Some(("openai".into(), "gpt-test-7-mini".into())),
            cost: Some(json!({"input_cost": 0.001, "output_cost": 0.002, "total_cost": 0.003})),
            ..Default::default()
        }
        .integration_only(),
    )
    .await;
    g.set_models(
        KEY_A,
        [
            "openai/gpt-test-7",
            "openai/plain-test-1",
            "anthropic/claude-test-9-1",
            "gemini/gemini-test-3",
            "groq/chat-model",
        ]
        .iter()
        .map(|m| model(m))
        .collect(),
    );
    g
}

/// Bifrost on `url` with `settings` beside the URL and key, the Responses
/// API for `openai/gpt-test-7` and Gemini's for `gemini/gemini-test-3`.
fn routed(url: &str, settings: Value) -> Config {
    let mut s = json!({"baseURL": url, "apiKey": KEY_A});
    for (k, v) in settings.as_object().unwrap() {
        s[k] = v.clone();
    }
    config_with(json!({"providers": {"bifrost": {"settings": s, "models": {
        "openai/gpt-test-7": {"protocol": "openai-responses"},
        "gemini/gemini-test-3": {"protocol": "google-gemini"}
    }}}}))
}

fn posts(g: &Gateway) -> Vec<String> {
    g.seen()
        .iter()
        .filter(|s| s.method == "POST")
        .map(|s| s.path.clone())
        .collect()
}

async fn stream_err(c: &Config, spec: &str) -> codesmith_ai::Error {
    let m = c.resolve_model(spec).unwrap();
    match m
        .provider
        .stream(&reqwest::Client::new(), &request(&m.model_id))
        .await
    {
        Ok(_) => panic!("{spec} streamed"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn with_api_openai_every_surface_streams_on_an_integration_only_gateway() {
    let _g = LOCK.lock().await;
    clear();
    let g = integration_gateway().await;
    let c = routed(&g.url, json!({"api": "openai"}));
    assert_eq!(fetch(&c).await, bifrost::Outcome::Fresh(5));
    for (spec, text, path, cost) in [
        (
            "bifrost/groq/chat-model",
            "Hello there.",
            "/openai/v1/chat/completions",
            Some(0.003),
        ),
        (
            "bifrost/openai/gpt-test-7",
            "Hello there.",
            "/openai/v1/responses",
            Some(0.003),
        ),
        (
            "bifrost/anthropic/claude-test-9-1",
            "Hello from Claude.",
            "/anthropic/v1/messages",
            None,
        ),
        (
            "bifrost/gemini/gemini-test-3",
            "Hello from Gemini.",
            "/genai/v1beta/models/gemini/gemini-test-3:streamGenerateContent",
            None,
        ),
    ] {
        let before = g.seen().len();
        let (events, got) = stream(&c, spec).await;
        assert_eq!(got, text, "{spec}");
        let seen = g.seen();
        let req = &seen[before..]
            .iter()
            .find(|s| s.method == "POST")
            .unwrap_or_else(|| panic!("{spec}: {seen:?}"));
        assert_eq!(req.path, path, "{spec}");
        assert_eq!(req.header("x-bf-vk"), Some(KEY_A), "{spec}");
        assert_eq!(
            req.header("x-bf-agent-correlation-id"),
            Some("ses_test1"),
            "{spec}"
        );
        // Gemini streams by its path (`:streamGenerateContent`).
        if !spec.contains("gemini") {
            assert_eq!(req.body["stream"], true, "{spec}");
        }
        let s = served(&events);
        let sent = req.header("x-request-id").unwrap();
        assert_eq!(s.request_id, Some(format!("bf-{sent}")), "{spec}");
        assert_eq!(s.provider.as_deref(), Some("openai"), "{spec}");
        assert_eq!(s.model.as_deref(), Some("gpt-test-7-mini"), "{spec}");
        let u = usage(&events);
        assert_eq!((u.input, u.output, u.cache_read), (100, 20, 30), "{spec}");
        // The OpenAI integration's bare `usage.cost` is billed as the
        // unified API's `{"total_cost"}` is.
        assert_eq!(u.cost, cost, "{spec}");
    }
    assert!(
        !posts(&g).iter().any(|p| p.starts_with("/v1/")),
        "{:?}",
        posts(&g)
    );
}

#[tokio::test]
async fn on_the_openai_route_effort_usage_and_ids_go_as_on_the_unified_one() {
    let _g = LOCK.lock().await;
    clear();
    let g = integration_gateway().await;
    let unified = routed(&g.url, json!({}));
    let openai = routed(&g.url, json!({"api": "openai"}));
    fetch(&openai).await;
    for (spec, chat) in [
        ("bifrost/groq/chat-model#high", true),
        ("bifrost/openai/gpt-test-7#high", false),
    ] {
        let (u_url, u_h, u_body) = prepared(&unified, spec);
        let (o_url, o_h, o_body) = prepared(&openai, spec);
        let tail = if chat {
            "chat/completions"
        } else {
            "responses"
        };
        assert_eq!(u_url, format!("{}/v1/{tail}", g.url));
        assert_eq!(o_url, format!("{}/openai/v1/{tail}", g.url));
        // The same body: Bifrost's OpenAI integration reads the same
        // `ChatParameters` and `ResponsesParameters` as its unified API.
        assert_eq!(u_body, o_body, "{spec}");
        assert_eq!(o_body["reasoning"]["effort"], "high", "{o_body}");
        assert!(o_body.get("reasoning_effort").is_none(), "{o_body}");
        if chat {
            assert_eq!(o_body["stream_options"]["include_usage"], true, "{o_body}");
        }
        assert_eq!(o_h.get("x-bf-vk").unwrap(), KEY_A);
        assert!(o_h.get("authorization").is_none());
        assert_eq!(u_h.get("x-bf-vk"), o_h.get("x-bf-vk"));
    }
    // Ids keep Bifrost's `<upstream>/<model>` form there too.
    let (_, _, body) = prepared(&openai, "bifrost/openai/plain-test-1");
    assert_eq!(body["model"], "openai/plain-test-1");
}

#[tokio::test]
async fn tool_calls_come_through_every_surface_with_api_openai() {
    let _g = LOCK.lock().await;
    clear();
    let g = integration_gateway().await;
    let args = json!({"path": "src/main.rs"});
    g.with(|s| s.tool_call = Some(("read".into(), args.clone())));
    let c = routed(&g.url, json!({"api": "openai"}));
    fetch(&c).await;
    for spec in [
        "bifrost/groq/chat-model",
        "bifrost/openai/gpt-test-7",
        "bifrost/anthropic/claude-test-9-1",
        "bifrost/gemini/gemini-test-3",
    ] {
        let (events, _) = stream(&c, spec).await;
        assert_eq!(
            tool_calls(&events),
            [("read".to_string(), args.clone())],
            "{spec}"
        );
    }
    assert_eq!(
        posts(&g),
        [
            "/openai/v1/chat/completions",
            "/openai/v1/responses",
            "/anthropic/v1/messages",
            "/genai/v1beta/models/gemini/gemini-test-3:streamGenerateContent"
        ]
    );
}

#[tokio::test]
async fn the_unified_default_on_an_integration_only_gateway_fails_clearly_and_tries_nothing_else() {
    let _g = LOCK.lock().await;
    clear();
    let g = integration_gateway().await;
    let c = routed(&g.url, json!({}));
    fetch(&c).await;
    for (spec, path) in [
        ("bifrost/groq/chat-model", "/v1/chat/completions"),
        ("bifrost/openai/gpt-test-7", "/v1/responses"),
    ] {
        let before = posts(&g).len();
        let e = stream_err(&c, spec).await;
        let msg = e.to_string();
        for part in [
            format!("{}{path} answered HTTP 403", g.url),
            "This hostname serves inference only. Use the admin hostname for administration."
                .to_string(),
            "the gateway doesn't serve this route".to_string(),
            "Nothing else was tried".to_string(),
            "providers.bifrost.settings.api".to_string(),
            format!(
                r#"The route was chosen by the default (providers.bifrost.settings.api unset), which is "unified" ({}/v1)."#,
                g.url
            ),
            "to fix it, set providers.bifrost.settings.api to \"openai\"".to_string(),
            "integration routes".to_string(),
        ] {
            assert!(msg.contains(&part), "{part}:\n{msg}");
        }
        assert!(!msg.contains(KEY_A));
        assert!(!msg.to_lowercase().contains("access"), "{msg}");
        // Never retried, never sent to another route.
        assert!(!e.retryable());
        assert_eq!(&posts(&g)[before..], [path], "{spec}");
        // As the session explains it: no word of the key's access.
        let class = codesmith_core::retry::classify(&e);
        assert_ne!(class, codesmith_core::retry::Class::Authentication);
        let m = c.resolve_model(spec).unwrap();
        let shown =
            codesmith_core::retry::explain(&e, &codesmith_core::retry::Target::of(&m, Some(&c)));
        assert!(!shown.contains("may not have access"), "{shown}");
        assert!(shown.contains("api"), "{shown}");
    }
    // The model list and Anthropic still work there, as in 0.6.0.
    let (_, text) = stream(&c, "bifrost/anthropic/claude-test-9-1").await;
    assert_eq!(text, "Hello from Claude.");
}

#[tokio::test]
async fn a_refused_openai_route_points_back_at_unified_and_other_surfaces_at_their_setting() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[
        "groq/chat-model",
        "anthropic/claude-test-9-1",
        "gemini/gemini-test-3",
    ])
    .await;
    g.with(|s| {
        s.refused = vec![
            (
                "/openai/v1/chat/completions".into(),
                404,
                "404 page not found".into(),
            ),
            (
                "/anthropic/v1/messages".into(),
                404,
                json!({"is_bifrost_error": false, "status_code": 404,
                       "error": {"message": "Route not found: /anthropic/v1/messages"}})
                .to_string(),
            ),
            (
                "/genai/v1beta/models/gemini/gemini-test-3:streamGenerateContent".into(),
                405,
                "Method Not Allowed".into(),
            ),
        ]
    });
    let c = routed(&g.url, json!({"api": "openai"}));
    fetch(&c).await;
    let e = stream_err(&c, "bifrost/groq/chat-model").await.to_string();
    assert!(
        e.contains(&format!(
            "{}/openai/v1/chat/completions answered HTTP 404 (404 page not found)",
            g.url
        )),
        "{e}"
    );
    assert!(
        e.contains(r#"chosen by providers.bifrost.settings.api ("openai")"#)
            && e.contains(r#"to fix it, set providers.bifrost.settings.api to "unified""#),
        "{e}"
    );
    let e = stream_err(&c, "bifrost/anthropic/claude-test-9-1")
        .await
        .to_string();
    assert!(
        e.contains("Route not found: /anthropic/v1/messages")
            && e.contains("providers.bifrost.settings.anthropicBaseURL is unset")
            && e.contains("to fix it, set providers.bifrost.settings.anthropicBaseURL to where"),
        "{e}"
    );
    let e = stream_err(&c, "bifrost/gemini/gemini-test-3")
        .await
        .to_string();
    assert!(
        e.contains("HTTP 405")
            && e.contains("to fix it, set providers.bifrost.settings.geminiBaseURL to where"),
        "{e}"
    );
    // The URL shown never carries the query (`?alt=sse` here).
    assert!(!e.contains("alt=sse"), "{e}");
}

#[tokio::test]
async fn a_403_about_the_key_or_model_stays_an_access_error() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["groq/chat-model"]).await;
    // Bifrost's governance refusal: an API error, naming the key.
    let body = json!({"is_bifrost_error": false, "status_code": 403,
        "error": {"type": "policy_model_blocked",
                  "message": format!("model blocked for virtual key {KEY_A}")}})
    .to_string();
    g.with(|s| s.refused = vec![("/v1/chat/completions".into(), 403, body)]);
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    let e = stream_err(&c, "bifrost/groq/chat-model").await;
    assert!(
        matches!(e, codesmith_ai::Error::Status { status: 403, .. }),
        "{e:?}"
    );
    let msg = e.to_string();
    assert!(
        msg.contains("model blocked") && !msg.contains(KEY_A),
        "{msg}"
    );
    assert!(!msg.contains("doesn't serve this route"), "{msg}");
    assert_eq!(
        codesmith_core::retry::classify(&e),
        codesmith_core::retry::Class::Authentication
    );
}

#[tokio::test]
async fn the_key_is_never_in_a_route_error_even_when_the_gateway_echoes_it() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["groq/chat-model"]).await;
    g.with(|s| {
        s.refused = vec![(
            "/v1/chat/completions".into(),
            403,
            json!({"error": format!("hostname refuses key {KEY_A} at http://x.example/p?vk={KEY_A}")})
                .to_string(),
        )]
    });
    let c = config(&g.url, KEY_A);
    fetch(&c).await;
    let e = stream_err(&c, "bifrost/groq/chat-model").await;
    for shown in [e.to_string(), format!("{e:?}")] {
        assert!(!shown.contains(KEY_A), "{shown}");
        assert!(shown.contains("[redacted]"), "{shown}");
    }
    // In the URL's user info too.
    let url = g.url.replacen("http://", &format!("http://u:{KEY_A}@"), 1);
    let c = config(&url, KEY_A);
    let m = c.resolve_model("bifrost/groq/chat-model").unwrap();
    let e = m
        .provider
        .stream(&reqwest::Client::new(), &request(&m.model_id))
        .await
        .err()
        .unwrap();
    assert!(!format!("{e} {e:?}").contains(KEY_A), "{e}");
}

#[tokio::test]
async fn each_surface_can_be_mounted_elsewhere_by_its_setting() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    g.set_models(
        KEY_A,
        [
            "groq/chat-model",
            "openai/gpt-test-7",
            "anthropic/claude-test-9-1",
            "gemini/gemini-test-3",
        ]
        .iter()
        .map(|m| model(m))
        .collect(),
    );
    g.with(|s| {
        s.models_path = Some("/gw/list/models".into());
        s.quota_path = Some("/gw/budget".into());
        s.quota = vec![(200, json!({"budgets": []}))];
        // Nothing at the usual places.
        s.refused = [
            "/v1/chat/completions",
            "/v1/responses",
            "/anthropic/v1/messages",
        ]
        .iter()
        .map(|p| (p.to_string(), 404, "nope".to_string()))
        .collect();
    });
    let c = routed(
        &g.url,
        json!({
            "api": "openai",
            "openaiBaseURL": format!("{}/gw/oai/v1/", g.url),
            "anthropicBaseURL": format!("{}/gw/claude/v1", g.url),
            "geminiBaseURL": format!("{}/gw/gem/v1beta", g.url),
            "modelsURL": format!("{}/gw/list/models", g.url),
            "quotaURL": format!("{}/gw/budget", g.url),
        }),
    );
    assert_eq!(fetch(&c).await, bifrost::Outcome::Fresh(4));
    for spec in [
        "bifrost/groq/chat-model",
        "bifrost/openai/gpt-test-7",
        "bifrost/anthropic/claude-test-9-1",
        "bifrost/gemini/gemini-test-3",
    ] {
        stream(&c, spec).await;
    }
    assert_eq!(
        posts(&g),
        [
            "/gw/oai/v1/chat/completions",
            "/gw/oai/v1/responses",
            "/gw/claude/v1/messages",
            "/gw/gem/v1beta/models/gemini/gemini-test-3:streamGenerateContent"
        ]
    );
    let urls = bifrost::urls(
        &c.provider("bifrost").unwrap().settings,
        &codesmith_core::providers::process_env(),
    )
    .unwrap()
    .unwrap();
    let m = c.resolve_model("bifrost/_").unwrap();
    assert_eq!(
        bifrost::quota(&reqwest::Client::new(), &m.provider, &urls.quota.url).await,
        Some(vec![])
    );
    let seen = g.seen();
    assert!(seen
        .iter()
        .any(|s| s.path == "/gw/budget" && s.header("x-bf-vk") == Some(KEY_A)));
    assert!(seen
        .iter()
        .any(|s| s.path == "/gw/list/models" && s.query == "page_size=1000"));
    // `debug model` names each surface and where it came from.
    let lines = urls.describe().join("\n");
    for part in [
        format!("{}/gw/oai/v1/chat/completions", g.url),
        "providers.bifrost.settings.openaiBaseURL".to_string(),
        format!("{}/gw/claude/v1/messages", g.url),
        "providers.bifrost.settings.quotaURL".to_string(),
        "providers.bifrost.settings.modelsURL".to_string(),
    ] {
        assert!(lines.contains(&part), "{part}:\n{lines}");
    }
}

#[tokio::test]
async fn each_route_setting_has_an_environment_variable() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    g.set_models(
        KEY_A,
        [
            "groq/chat-model",
            "anthropic/claude-test-9-1",
            "gemini/gemini-test-3",
        ]
        .iter()
        .map(|m| model(m))
        .collect(),
    );
    g.with(|s| {
        s.models_path = Some("/e/models".into());
        s.quota_path = Some("/e/quota".into());
    });
    let vars = [
        (bifrost::OPENAI_URL_VAR, format!("{}/e/oai/v1", g.url)),
        (bifrost::ANTHROPIC_URL_VAR, format!("{}/e/claude/v1", g.url)),
        (bifrost::GEMINI_URL_VAR, format!("{}/e/gem/v1beta", g.url)),
        (bifrost::MODELS_URL_VAR, format!("{}/e/models", g.url)),
        (bifrost::QUOTA_URL_VAR, format!("{}/e/quota", g.url)),
    ];
    for (k, v) in &vars {
        std::env::set_var(k, v);
    }
    let c = routed(&g.url, json!({}));
    let outcome = fetch(&c).await;
    let mut paths = vec![];
    for spec in [
        "bifrost/groq/chat-model",
        "bifrost/anthropic/claude-test-9-1",
        "bifrost/gemini/gemini-test-3",
    ] {
        let m = c.resolve_model(spec).unwrap();
        paths.push(m.provider.base_url.clone());
    }
    let urls = bifrost::urls(
        &c.provider("bifrost").unwrap().settings,
        &codesmith_core::providers::process_env(),
    );
    // BIFROST_API too, and a setting beats its variable.
    std::env::set_var(bifrost::API_VAR, "openai");
    std::env::remove_var(bifrost::OPENAI_URL_VAR);
    let by_var = c
        .resolve_model("bifrost/groq/chat-model")
        .map(|m| m.provider.base_url);
    let by_setting = routed(&g.url, json!({"api": "unified"}))
        .resolve_model("bifrost/groq/chat-model")
        .map(|m| m.provider.base_url);
    std::env::set_var(bifrost::API_VAR, "sideways");
    let bad = c
        .resolve_model("bifrost/groq/chat-model")
        .err()
        .map(|e| e.to_string());
    std::env::remove_var(bifrost::API_VAR);
    for (k, _) in &vars {
        std::env::remove_var(k);
    }
    assert_eq!(outcome, bifrost::Outcome::Fresh(3));
    assert_eq!(
        paths,
        [
            format!("{}/e/oai/v1", g.url),
            format!("{}/e/claude/v1", g.url),
            format!("{}/e/gem/v1beta", g.url)
        ]
    );
    let urls = urls.unwrap().unwrap();
    assert_eq!(urls.quota.url, format!("{}/e/quota", g.url));
    assert_eq!(urls.quota.from, bifrost::QUOTA_URL_VAR);
    assert_eq!(urls.openai(None).from, bifrost::OPENAI_URL_VAR);
    assert!(g.seen().iter().any(|s| s.path == "/e/models"));
    assert_eq!(by_var.unwrap(), format!("{}/openai/v1", g.url));
    assert_eq!(by_setting.unwrap(), format!("{}/v1", g.url));
    let bad = bad.expect("a bad BIFROST_API is refused");
    assert!(
        bad.contains("BIFROST_API must be \"unified\" or \"openai\""),
        "{bad}"
    );
}

#[test]
fn a_models_api_overrides_the_providers_and_the_openai_base_url() {
    let _ = support::isolate();
    let url = "http://127.0.0.1:9";
    let c = |settings: Value, api: Option<&str>| {
        let mut m = json!({});
        if let Some(a) = api {
            m["api"] = json!(a);
        }
        let mut s = json!({"baseURL": url, "apiKey": KEY_A});
        for (k, v) in settings.as_object().unwrap() {
            s[k] = v.clone();
        }
        config_with(json!({"providers": {"bifrost": {"settings": s, "models": {
            "groq/m": m, "openai/r": {"protocol": "openai-responses", "api": api},
            "anthropic/c": {"api": api}, "gemini/g": {"protocol": "google-gemini", "api": api}
        }}}}))
    };
    let base = |c: &Config, id: &str| {
        c.resolve_model(&format!("bifrost/{id}"))
            .unwrap()
            .provider
            .base_url
    };
    let unified = format!("{url}/v1");
    let openai = format!("{url}/openai/v1");
    let custom = "http://127.0.0.1:9/elsewhere/v1".to_string();
    for (settings, api, want) in [
        (json!({}), None, &unified),
        (json!({}), Some("openai"), &openai),
        (json!({"api": "openai"}), None, &openai),
        (json!({"api": "openai"}), Some("unified"), &unified),
        (json!({"api": "OpenAI"}), None, &openai),
        (json!({"openaiBaseURL": custom}), None, &custom),
        (
            json!({"openaiBaseURL": custom, "api": "openai"}),
            None,
            &custom,
        ),
        (json!({"openaiBaseURL": custom}), Some("openai"), &openai),
        (json!({"openaiBaseURL": custom}), Some("unified"), &unified),
    ] {
        let c = c(settings.clone(), api);
        assert_eq!(&base(&c, "groq/m"), want, "{settings} {api:?}");
        assert_eq!(&base(&c, "openai/r"), want, "{settings} {api:?}");
        // `api` is only about the OpenAI-shaped routes.
        assert_eq!(base(&c, "anthropic/c"), format!("{url}/anthropic/v1"));
        assert_eq!(base(&c, "gemini/g"), format!("{url}/genai/v1beta"));
    }
    // Another model keeps the provider's choice.
    let c2 = config_with(json!({"providers": {"bifrost": {
        "settings": {"baseURL": url, "apiKey": KEY_A, "api": "unified"},
        "models": {"groq/a": {"api": "openai"}, "groq/b": {}}}}}));
    assert_eq!(base(&c2, "groq/a"), openai);
    assert_eq!(base(&c2, "groq/b"), unified);
}

#[test]
fn bad_route_settings_are_refused_with_the_setting_named() {
    let _ = support::isolate();
    let url = "http://127.0.0.1:9";
    for (settings, model_api, want) in [
        (
            json!({"api": "both"}),
            None,
            "providers.bifrost.settings.api must be \"unified\" or \"openai\", not \"both\"",
        ),
        (
            json!({}),
            Some("v2"),
            "providers.bifrost.models.groq/m.api must be \"unified\" or \"openai\"",
        ),
        (
            json!({"openaiBaseURL": "gw/openai/v1"}),
            None,
            "providers.bifrost.settings.openaiBaseURL must be an http:// or https:// URL",
        ),
        (
            json!({"anthropicBaseURL": "ftp://x/anthropic"}),
            None,
            "anthropicBaseURL must be an http:// or https:// URL",
        ),
        (
            json!({"geminiBaseURL": "https://"}),
            None,
            "geminiBaseURL must be",
        ),
        (
            json!({"modelsURL": "/v1/models"}),
            None,
            "modelsURL must be",
        ),
        (json!({"quotaURL": "quota"}), None, "quotaURL must be"),
    ] {
        let mut s = json!({"baseURL": url, "apiKey": KEY_A});
        for (k, v) in settings.as_object().unwrap() {
            s[k] = v.clone();
        }
        let c = config_with(json!({"providers": {"bifrost": {"settings": s,
            "models": {"groq/m": {"api": model_api}}}}}));
        let e = c.resolve_model("bifrost/groq/m").unwrap_err().to_string();
        assert!(e.contains(want), "{want}:\n{e}");
        assert!(!e.contains(KEY_A));
    }
    // Empty values are unset.
    let c = config_with(json!({"providers": {"bifrost": {"settings": {
        "baseURL": url, "apiKey": KEY_A, "api": " ", "openaiBaseURL": "", "quotaURL": ""}}}}));
    assert_eq!(
        c.resolve_model("bifrost/groq/m").unwrap().provider.base_url,
        format!("{url}/v1")
    );
}

#[test]
fn the_default_routes_are_0_6_0s() {
    let settings = codesmith_core::config::ProviderSettings::default();
    let none: codesmith_core::providers::Env = std::sync::Arc::new(|_: &str| None);
    let u = bifrost::urls_at("http://gw.example", &settings, &none).unwrap();
    assert_eq!(u.api, bifrost::Api::Unified);
    assert_eq!(u.openai(None).url, "http://gw.example/v1");
    assert_eq!(u.anthropic.url, "http://gw.example/anthropic/v1");
    assert_eq!(u.gemini.url, "http://gw.example/genai/v1beta");
    assert_eq!(u.models.url, "http://gw.example/v1/models");
    assert_eq!(
        u.quota.url,
        "http://gw.example/api/governance/virtual-keys/quota"
    );
    assert!(u.openai_override.is_none());
    for p in [
        codesmith_ai::Protocol::OpenaiChat,
        codesmith_ai::Protocol::OpenaiResponses,
        codesmith_ai::Protocol::AnthropicMessages,
        codesmith_ai::Protocol::GoogleGemini,
    ] {
        assert_eq!(
            u.base(p, None).url,
            bifrost::surface("http://gw.example", p)
        );
    }
}

#[test]
fn every_combination_of_route_settings_resolves_by_one_rule() {
    use codesmith_ai::Protocol::*;
    let root = "https://gw.example/base";
    let none: codesmith_core::providers::Env = std::sync::Arc::new(|_: &str| None);
    let apis = [None, Some("unified"), Some("openai"), Some(" OPENAI ")];
    let overrides = [None, Some("https://other.example/oai/v1/")];
    let mut n = 0;
    for provider_api in apis {
        for model_api in apis {
            for ov in overrides {
                for protocol in [OpenaiChat, OpenaiResponses, AnthropicMessages, GoogleGemini] {
                    let mut settings = codesmith_core::config::ProviderSettings::default();
                    settings.plugin.api = provider_api.map(str::to_string);
                    settings.plugin.openai_base_url = ov.map(str::to_string);
                    let u = bifrost::urls_at(root, &settings, &none).unwrap();
                    let m =
                        model_api.map(|a| (bifrost::Api::parse(a).unwrap(), "model".to_string()));
                    let got = u.base(protocol, m).url;
                    let want = match protocol {
                        AnthropicMessages => format!("{root}/anthropic/v1"),
                        GoogleGemini => format!("{root}/genai/v1beta"),
                        _ => match (model_api, ov, provider_api) {
                            (Some(a), _, _) => bifrost::Api::parse(a).unwrap().root(root),
                            (None, Some(o), _) => o.trim_end_matches('/').to_string(),
                            (None, None, Some(a)) => bifrost::Api::parse(a).unwrap().root(root),
                            _ => format!("{root}/v1"),
                        },
                    };
                    assert_eq!(
                        got, want,
                        "{provider_api:?} {model_api:?} {ov:?} {protocol:?}"
                    );
                    assert!(!got.ends_with('/'));
                    n += 1;
                }
            }
        }
    }
    assert_eq!(n, 4 * 4 * 2 * 4);
}

#[tokio::test]
async fn a_model_list_route_the_gateway_doesnt_serve_names_the_url_and_setting() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["groq/chat-model"]).await;
    g.with(|s| {
        s.refused = vec![("/v1/models".into(), 403, INFERENCE_ONLY.into())];
        s.quota = vec![(200, json!({"budgets": []}))];
    });
    let c = config(&g.url, KEY_A);
    let o = fetch(&c).await;
    let bifrost::Outcome::Missing { error } = &o else {
        panic!("{o:?}");
    };
    for part in [
        format!("{}/v1/models answered HTTP 403", g.url),
        "This hostname serves inference only".to_string(),
        "providers.bifrost.settings.modelsURL (BIFROST_MODELS_URL)".to_string(),
    ] {
        assert!(error.contains(&part), "{part}:\n{error}");
    }
    assert!(
        !error.contains(KEY_A) && !error.contains("page_size"),
        "{error}"
    );
    // A refused quota URL stays silent: no reading.
    g.with(|s| {
        s.refused = vec![(
            "/api/governance/virtual-keys/quota".into(),
            403,
            INFERENCE_ONLY.into(),
        )]
    });
    let m = c.resolve_model("bifrost/_").unwrap();
    let q = format!("{}/api/governance/virtual-keys/quota", g.url);
    assert_eq!(
        bifrost::quota(&reqwest::Client::new(), &m.provider, &q).await,
        None
    );
}

#[tokio::test]
async fn a_models_url_with_a_query_keeps_it_and_pages_after_it() {
    let _g = LOCK.lock().await;
    clear();
    let g = Gateway::start(State {
        page: 2,
        ..Default::default()
    })
    .await;
    g.set_models(
        KEY_A,
        (0..3).map(|i| model(&format!("groq/m-{i}"))).collect(),
    );
    let c = routed(
        &g.url,
        json!({"modelsURL": format!("{}/v1/models?scope=chat", g.url)}),
    );
    assert_eq!(fetch(&c).await, bifrost::Outcome::Fresh(3));
    let qs: Vec<String> = g
        .seen()
        .iter()
        .filter(|s| s.path == "/v1/models")
        .map(|s| s.query.clone())
        .collect();
    assert_eq!(
        qs,
        [
            "scope=chat&page_size=1000",
            "scope=chat&page_size=1000&page_token=2"
        ]
    );
}

#[tokio::test]
async fn a_refused_route_names_exactly_the_setting_that_chose_it_and_one_fix() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&[]).await;
    g.set_models(
        KEY_A,
        [
            "groq/m",
            "openai/r",
            "anthropic/claude-test-9-1",
            "gemini/gemini-test-3",
        ]
        .iter()
        .map(|m| model(m))
        .collect(),
    );
    g.with(|s| s.refuse_posts = Some((403, INFERENCE_ONLY.into())));
    let u = g.url.clone();
    let s = "providers.bifrost.settings";
    // (settings, variables, the model's own api, model, chosen by, fix)
    type Case<'a> = (
        Value,
        Vec<(&'a str, String)>,
        Option<&'a str>,
        &'a str,
        String,
        String,
    );
    let cases: Vec<Case> = vec![
        (
            json!({}),
            vec![],
            None,
            "groq/m",
            format!(r#"the default ({s}.api unset), which is "unified" ({u}/v1)"#),
            format!(r#"set {s}.api to "openai" (or BIFROST_API=openai) (this gateway may serve only Bifrost's integration routes)"#),
        ),
        (
            json!({"api": "openai"}),
            vec![],
            None,
            "groq/m",
            format!(r#"{s}.api ("openai")"#),
            format!(r#"set {s}.api to "unified" (this gateway may serve only Bifrost's unified API)"#),
        ),
        (
            json!({"api": "unified"}),
            vec![("BIFROST_API", "openai".into())],
            None,
            "openai/r",
            format!(r#"{s}.api ("unified")"#),
            format!(r#"set {s}.api to "openai""#),
        ),
        (
            json!({}),
            vec![("BIFROST_API", "openai".into())],
            None,
            "openai/r",
            r#"BIFROST_API ("openai")"#.into(),
            r#"set BIFROST_API to "unified""#.into(),
        ),
        (
            json!({"api": "unified", "openaiBaseURL": format!("{u}/x/v1")}),
            vec![],
            Some("openai"),
            "groq/m",
            r#"providers.bifrost.models.groq/m.api ("openai")"#.into(),
            r#"set providers.bifrost.models.groq/m.api to "unified""#.into(),
        ),
        (
            json!({"api": "openai", "openaiBaseURL": format!("{u}/x/v1")}),
            vec![(bifrost::OPENAI_URL_VAR, format!("{u}/y/v1"))],
            None,
            "groq/m",
            format!("{s}.openaiBaseURL ({u}/x/v1)"),
            format!("set {s}.openaiBaseURL to where this gateway serves the OpenAI API, or remove it so {s}.api picks the route"),
        ),
        (
            json!({"api": "openai"}),
            vec![(bifrost::OPENAI_URL_VAR, format!("{u}/y/v1"))],
            None,
            "openai/r",
            format!("BIFROST_OPENAI_BASE_URL ({u}/y/v1)"),
            format!("set BIFROST_OPENAI_BASE_URL to where this gateway serves the OpenAI API, or unset it so {s}.api picks the route"),
        ),
        (
            json!({"api": "openai"}),
            vec![],
            Some("openai"),
            "anthropic/claude-test-9-1",
            format!("the default, from {s}.baseURL ({u}/anthropic/v1; {s}.anthropicBaseURL is unset)"),
            format!("set {s}.anthropicBaseURL to where this gateway serves Bifrost's Anthropic API"),
        ),
        (
            json!({"anthropicBaseURL": format!("{u}/c/v1")}),
            vec![],
            None,
            "anthropic/claude-test-9-1",
            format!("{s}.anthropicBaseURL ({u}/c/v1)"),
            format!("set {s}.anthropicBaseURL to where this gateway serves Bifrost's Anthropic API"),
        ),
        (
            json!({}),
            vec![(bifrost::GEMINI_URL_VAR, format!("{u}/g/v1beta"))],
            None,
            "gemini/gemini-test-3",
            format!("BIFROST_GEMINI_BASE_URL ({u}/g/v1beta)"),
            "set BIFROST_GEMINI_BASE_URL to where this gateway serves Bifrost's Gemini API".into(),
        ),
        (
            json!({}),
            vec![],
            None,
            "gemini/gemini-test-3",
            format!("the default, from {s}.baseURL ({u}/genai/v1beta; {s}.geminiBaseURL is unset)"),
            format!("set {s}.geminiBaseURL to where this gateway serves Bifrost's Gemini API"),
        ),
    ];
    for (settings, vars, model_api, id, chosen, fix) in cases {
        for (k, v) in &vars {
            std::env::set_var(k, v);
        }
        let mut st = json!({"baseURL": u, "apiKey": KEY_A});
        for (k, v) in settings.as_object().unwrap() {
            st[k] = v.clone();
        }
        let mut models = json!({
            "openai/r": {"protocol": "openai-responses"},
            "gemini/gemini-test-3": {"protocol": "google-gemini"},
        });
        if let Some(a) = model_api {
            models[id]["api"] = json!(a);
        }
        let c = config_with(json!({"providers": {"bifrost": {"settings": st, "models": models}}}));
        let m = c.resolve_model(&format!("bifrost/{id}"));
        let e = match m {
            Ok(m) => m
                .provider
                .stream(&reqwest::Client::new(), &request(&m.model_id))
                .await
                .err(),
            Err(e) => panic!("{id}: {e}"),
        };
        for (k, _) in &vars {
            std::env::remove_var(k);
        }
        let e = e.unwrap_or_else(|| panic!("{id} streamed")).to_string();
        assert!(
            e.contains(&format!(
                "The route was chosen by {chosen}. Nothing else was tried"
            )),
            "{id} {settings} {vars:?}: wanted chosen by {chosen}\n{e}"
        );
        assert!(
            e.contains(&format!("to fix it, {fix}")),
            "{id} {settings} {vars:?}: wanted fix {fix}\n{e}"
        );
        assert_eq!(e.matches("to fix it").count(), 1, "one change: {e}");
        assert!(!e.contains(KEY_A));
    }
}

#[tokio::test]
async fn the_kept_list_is_keyed_by_the_model_list_url_and_key() {
    let _g = LOCK.lock().await;
    clear();
    let g = gateway_with(&["groq/m"]).await;
    let default = config(&g.url, KEY_A);
    assert_eq!(fetch(&default).await, bifrost::Outcome::Fresh(1));
    let kept = live::load("bifrost").unwrap();
    let list = format!("{}/v1/models", g.url);
    assert_eq!(kept["account"], bifrost::account(&list, Some(KEY_A)));
    assert_ne!(
        bifrost::account(&list, Some(KEY_A)),
        bifrost::legacy_account(&g.url, Some(KEY_A))
    );
    assert_ne!(
        bifrost::account(&list, Some(KEY_A)),
        bifrost::account(&format!("{list}?x=1"), Some(KEY_A))
    );
    // Another model list URL is another list: the kept one isn't used.
    g.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let other = config_with(json!({"providers": {"bifrost": {"settings": {
        "baseURL": g.url, "apiKey": KEY_A, "modelsURL": format!("{list}?scope=other")}}}}));
    assert!(other.list_models("bifrost", &["chat"]).is_empty());
    assert!(matches!(
        fetch(&other).await,
        bifrost::Outcome::Missing { .. }
    ));
    // The default's still is.
    assert!(matches!(
        fetch(&default).await,
        bifrost::Outcome::Saved { .. }
    ));
    assert_eq!(default.list_models("bifrost", &["chat"]), ["groq/m"]);
}

#[tokio::test]
async fn a_list_0_6_0_kept_stands_in_with_the_default_model_list_url() {
    let _g = LOCK.lock().await;
    let fetched = 1_767_225_600_000u64;
    // 0.6.0's file: the list under a hash of the root and key.
    let keep_legacy = |root: &str| {
        clear();
        let file = live::file();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let v = json!({"bifrost": {
            "account": bifrost::legacy_account(root, Some(KEY_A)),
            "baseURL": root, "fetched": fetched,
            "models": bifrost::normalize(&[model("groq/kept-in-0-6-0")])}});
        std::fs::write(&file, v.to_string()).unwrap();
    };
    let g = gateway_with(&[]).await;
    let url = g.url.clone();
    g.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The default model list URL: adopted as this key's, its age kept.
    keep_legacy(&url);
    let c = config(&url, KEY_A);
    let o = fetch(&c).await;
    assert_eq!(
        o,
        bifrost::Outcome::Saved {
            error: match &o {
                bifrost::Outcome::Saved { error, .. } => error.clone(),
                _ => String::new(),
            },
            saved: fetched
        }
    );
    assert_eq!(c.list_models("bifrost", &["chat"]), ["groq/kept-in-0-6-0"]);
    assert!(c.resolve_model("bifrost/groq/kept-in-0-6-0").is_ok());
    assert!(c.resolve_model("bifrost/groq/not-kept").is_err());
    let kept = live::load("bifrost").unwrap();
    assert_eq!(
        kept["account"],
        bifrost::account(&format!("{url}/v1/models"), Some(KEY_A))
    );
    assert_eq!(kept["fetched"], fetched);
    assert!(!std::fs::read_to_string(live::file())
        .unwrap()
        .contains(KEY_A));

    // Another key's 0.6.0 list is never adopted.
    keep_legacy(&url);
    let b = config(&url, KEY_B);
    assert!(matches!(fetch(&b).await, bifrost::Outcome::Missing { .. }));
    assert_eq!(
        live::load("bifrost").unwrap()["account"],
        bifrost::legacy_account(&url, Some(KEY_A))
    );

    // A configured model list URL: 0.6.0's list wasn't read from it, so
    // there is no saved list.
    keep_legacy(&url);
    let c = config_with(json!({"providers": {"bifrost": {"settings": {
        "baseURL": url, "apiKey": KEY_A, "modelsURL": format!("{url}/elsewhere/models")}}}}));
    assert!(matches!(fetch(&c).await, bifrost::Outcome::Missing { .. }));
    assert!(c.list_models("bifrost", &["chat"]).is_empty());
    assert_eq!(
        live::load("bifrost").unwrap()["account"],
        bifrost::legacy_account(&url, Some(KEY_A))
    );
}
