//! Ports of OpenCode's model catalog tests (MIT): `models.test.ts` (the
//! models.dev service and its normalization), `plugin/models-dev.test.ts`
//! (what the catalog offers) and the catalog cases of
//! `config/provider.test.ts` (config merged over catalog models).
//!
//! Adapted: OpenCode's Effect `HttpClient` mock is a local HTTP server
//! here, its KV service an in-memory store, and its `Refreshed` bus event
//! the service's `subscribe` channel. Model packages are kept as OpenCode
//! names them; where OpenCode moves a model to another package, CodeSmith
//! moves it to another protocol.

use codesmith_core::catalog::{
    self, body_digest, Json, Kv, MemoryKv, ModelsDev, Options, Snapshot, CATALOG_PATH,
};
use codesmith_core::config::{self, Config};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

const CACHE_KEY: &str = "models-dev:v2:catalog";

// ---------------------------------------------------------------------------
// Fixtures.

fn j(text: &str) -> Json {
    Json::parse(text).unwrap()
}

/// `{...base, ...over}`: keys of `over` replace those of `base` in place.
fn spread(base: Json, over: Json) -> Json {
    let (Json::Object(mut b), Json::Object(o)) = (base, over) else {
        panic!("objects")
    };
    for (k, v) in o {
        match b.iter_mut().find(|(key, _)| *key == k) {
            Some(slot) => slot.1 = v,
            None => b.push((k, v)),
        }
    }
    Json::Object(b)
}

fn text(v: &Json) -> String {
    serde_json::to_string(v).unwrap()
}

/// A chat model in the models.dev v2 shape, served over one
/// OpenAI-compatible API.
fn source_model(id: &str, over: &str) -> Json {
    let base = j(&format!(
        r#"{{
        "id": "{id}", "type": "chat", "name": "{id}", "description": "A model",
        "release_date": "2026-01-01",
        "modalities": {{"input": ["text"], "output": ["text"]}},
        "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}, "temperature": true}},
        "limit": {{"context": 128000, "output": 8192}},
        "api": {{"chat-completions": {{
            "base_url": "https://api.example.test/v1",
            "sdk": {{"aisdk": "@ai-sdk/openai-compatible", "opencodeai": "@opencode/ai/providers/openai-compatible"}}
        }}}}
    }}"#
    ));
    spread(base, j(over))
}

fn acme_provider(models: Vec<(&str, Json)>) -> Json {
    let models = Json::Object(models.into_iter().map(|(k, v)| (k.into(), v)).collect());
    spread(
        j(r#"{"id": "acme", "name": "Acme", "env": ["ACME_API_KEY"]}"#),
        Json::Object(vec![("models".into(), models)]),
    )
}

fn acme_one() -> Json {
    source_model(
        "acme-1",
        r#"{"name": "Acme One", "api": {"chat-completions": {
            "base_url": "https://api.acme.test/v1",
            "sdk": {"aisdk": "@ai-sdk/openai-compatible", "opencodeai": "@opencode/ai/providers/openai-compatible"},
            "compatibility": {"reasoning_field": "vendor_reasoning"}
        }}}"#,
    )
}

fn fixture() -> Json {
    Json::Object(vec![(
        "acme".into(),
        acme_provider(vec![("acme-1", acme_one())]),
    )])
}

fn fixture2() -> Json {
    Json::Object(vec![(
        "beta".into(),
        spread(
            j(r#"{"id": "beta", "name": "Beta", "env": ["BETA_API_KEY"]}"#),
            Json::Object(vec![(
                "models".into(),
                Json::Object(vec![(
                    "beta-1".into(),
                    source_model(
                        "beta-1",
                        r#"{"name": "Beta One", "release_date": "2026-02-01",
                        "capabilities": {"tools": {"supported": false}, "reasoning": {"supported": false}, "temperature": false},
                        "limit": {"context": 64000, "output": 4096}}"#,
                    ),
                )]),
            )]),
        ),
    )])
}

fn zero_cost() -> Value {
    json!([{"input": 0.0, "output": 0.0, "cache": {"read": 0.0, "write": 0.0}}])
}

fn fixture_snapshot() -> Value {
    json!([{
        "info": {
            "id": "acme", "name": "Acme", "activation": "auto",
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": "https://api.acme.test/v1"}
        },
        "models": [{
            "id": "acme-1", "modelID": "acme-1", "providerID": "acme", "type": "chat",
            "name": "Acme One",
            "compatibility": {"reasoningField": "vendor_reasoning"},
            "settings": {"provider": "acme"},
            "capabilities": {"tools": true, "input": ["text"], "output": ["text"], "temperature": true},
            "variants": [],
            "time": {"released": 1767225600000i64},
            "cost": zero_cost(),
            "status": "active",
            "enabled": true,
            "limit": {"context": 128000, "output": 8192}
        }],
        "environment": ["ACME_API_KEY"]
    }])
}

fn fixture2_snapshot() -> Value {
    json!([{
        "info": {
            "id": "beta", "name": "Beta", "activation": "auto",
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": "https://api.example.test/v1"}
        },
        "models": [{
            "id": "beta-1", "modelID": "beta-1", "providerID": "beta", "type": "chat",
            "name": "Beta One",
            "settings": {"provider": "beta"},
            "capabilities": {"tools": false, "input": ["text"], "output": ["text"], "temperature": false},
            "variants": [],
            "time": {"released": 1769904000000i64},
            "cost": zero_cost(),
            "status": "active",
            "enabled": true,
            "limit": {"context": 64000, "output": 4096}
        }],
        "environment": ["BETA_API_KEY"]
    }])
}

fn as_json(s: &[Snapshot]) -> Value {
    serde_json::to_value(s).unwrap()
}

fn write_cache_text(kv: &MemoryKv, text: &str, updated_at: i64) {
    kv.set(
        CACHE_KEY,
        json!({"updatedAt": updated_at, "digest": body_digest(text), "body": text}),
    )
    .unwrap();
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn write_cache(kv: &MemoryKv, data: &Json, updated_at: i64) {
    write_cache_text(kv, &text(data), updated_at);
}

fn service(kv: Arc<MemoryKv>, options: Options) -> ModelsDev {
    ModelsDev::new(options, kv)
}

fn offline() -> Options {
    Options {
        fetch: false,
        ..Options::default()
    }
}

fn decode(catalog: &Json) -> Vec<Snapshot> {
    catalog::decode_snapshot(&text(catalog)).unwrap()
}

// ---------------------------------------------------------------------------
// A local HTTP server standing in for models.dev.

#[derive(Debug, Clone)]
struct Call {
    path: String,
    user_agent: Option<String>,
    if_none_match: Option<String>,
}

struct State {
    status: u16,
    body: String,
    etag: Option<String>,
    /// Status by path suffix, before `status`.
    routes: Vec<(String, u16, String)>,
    calls: Vec<Call>,
}

struct Server {
    url: String,
    state: Arc<Mutex<State>>,
}

impl Server {
    fn start(body: String) -> Server {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State {
            status: 200,
            body,
            etag: None,
            routes: vec![],
            calls: vec![],
        }));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut call = Call {
                    path,
                    user_agent: None,
                    if_none_match: None,
                };
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.trim().split_once(':') {
                        match k.to_ascii_lowercase().as_str() {
                            "user-agent" => call.user_agent = Some(v.trim().into()),
                            "if-none-match" => call.if_none_match = Some(v.trim().into()),
                            _ => {}
                        }
                    }
                }
                let (status, body, etag) = {
                    let mut s = shared.lock().unwrap();
                    let route = s
                        .routes
                        .iter()
                        .find(|r| call.path.ends_with(&r.0))
                        .map(|r| (r.1, r.2.clone()));
                    s.calls.push(call);
                    let (status, body) = route.unwrap_or((s.status, s.body.clone()));
                    (status, body, s.etag.clone())
                };
                let body = if status == 304 { String::new() } else { body };
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    body.len()
                );
                if let Some(e) = etag {
                    head.push_str(&format!("ETag: {e}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
                let mut rest = Vec::new();
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(1)));
                let _ = reader.read_to_end(&mut rest);
            }
        });
        Server { url, state }
    }

    fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    fn set(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state.lock().unwrap())
    }

    fn options(&self) -> Options {
        Options {
            url: Some(self.url.clone()),
            fetch: true,
            snapshot: false,
            ..Options::default()
        }
    }

    /// The service's cache key for this server.
    fn key(&self) -> String {
        catalog::cache_key(&self.url)
    }
}

fn write_cache_at(kv: &MemoryKv, key: &str, data: &Json, updated_at: i64) {
    let t = text(data);
    kv.set(
        key,
        json!({"updatedAt": updated_at, "digest": body_digest(&t), "body": t}),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// models.test.ts

#[test]
fn oc_catalog_normalizes_permissive_interleaved_values_to_compatibility() {
    use config::interleaved_field;
    assert_eq!(
        interleaved_field(&json!("reasoning_text")).as_deref(),
        Some("reasoning_text")
    );
    assert_eq!(
        interleaved_field(&json!({"field": "vendor_reasoning"})).as_deref(),
        Some("vendor_reasoning")
    );
    assert_eq!(interleaved_field(&json!(true)), None);
    assert_eq!(interleaved_field(&json!(false)), None);
    // And it reaches the request's compatibility.
    let c: Config = serde_json::from_value(json!({"providers": {"p": {
        "settings": {"baseURL": "http://x"},
        "models": {"m": {"interleaved": {"field": "vendor_reasoning"}}}}}}))
    .unwrap();
    let m = c.resolve_model("p/m").unwrap();
    assert_eq!(
        m.provider.compat.reasoning_field.as_deref(),
        Some("vendor_reasoning")
    );
}

#[test]
fn oc_catalog_get_returns_normalized_snapshots_from_kv_when_a_cache_entry_exists() {
    let kv = Arc::new(MemoryKv::default());
    write_cache(&kv, &fixture(), now());
    let server = Server::start(text(&fixture2()));
    let s = service(
        kv,
        Options {
            fetch: false,
            ..server.options()
        },
    );
    // A custom source has its own key; the default source's is the plain one.
    assert_ne!(server.key(), CACHE_KEY);
    let default = service(Arc::new(MemoryKv::default()), offline());
    assert_eq!(default.key(), CACHE_KEY);
    let kv2 = Arc::new(MemoryKv::default());
    write_cache(&kv2, &fixture(), now());
    let from_kv = service(kv2, offline());
    assert_eq!(as_json(&from_kv.get()), fixture_snapshot());
    drop(s);
    assert!(server.calls().is_empty());
}

#[test]
fn oc_catalog_takes_each_models_package_from_its_first_api_and_skips_urls_packages_build_themselves(
) {
    let azure_api = |protocol: &str, opencodeai: &str| {
        j(&format!(
            r#"{{"{protocol}": {{
                "base_url": "https://${{AZURE_RESOURCE_NAME}}.openai.azure.com/openai/v1",
                "sdk": {{"aisdk": "@ai-sdk/azure", "opencodeai": "{opencodeai}"}}
            }}}}"#
        ))
    };
    let with_api = |id: &str, api: Json| {
        spread(
            source_model(id, "{}"),
            Json::Object(vec![("api".into(), api)]),
        )
    };
    let catalog = Json::Object(vec![
        (
            "acme".into(),
            acme_provider(vec![
                ("acme-1", acme_one()),
                (
                    "acme-responses",
                    source_model(
                        "acme-responses",
                        r#"{"api": {
                        "responses": {
                            "base_url": "https://api.acme.test/v1",
                            "sdk": {"aisdk": "@ai-sdk/openai", "opencodeai": "@opencode/ai/providers/openai/responses"},
                            "capabilities": {"tools": {"freeform": ["text", "future-grammar", "lark"]}}
                        },
                        "chat-completions": {
                            "base_url": "https://other.acme.test/v1",
                            "sdk": {"aisdk": "@ai-sdk/openai"},
                            "capabilities": {"tools": {"freeform": ["regex"]}}
                        }
                    }}"#,
                    ),
                ),
                (
                    "acme-fallback",
                    source_model(
                        "acme-fallback",
                        r#"{"api": {"messages": {
                        "base_url": "https://claude.acme.test/v1",
                        "sdk": {"aisdk": "@ai-sdk/anthropic"},
                        "capabilities": {"tools": {"freeform": ["future-grammar"]}}
                    }}}"#,
                    ),
                ),
            ]),
        ),
        (
            "azure".into(),
            spread(
                j(
                    r#"{"id": "azure", "name": "Azure", "env": ["AZURE_RESOURCE_NAME", "AZURE_API_KEY"]}"#,
                ),
                Json::Object(vec![(
                    "models".into(),
                    Json::Object(vec![
                        (
                            "gpt-5".into(),
                            with_api(
                                "gpt-5",
                                azure_api("responses", "@opencode/ai/providers/azure/responses"),
                            ),
                        ),
                        (
                            "custom-chat".into(),
                            with_api(
                                "custom-chat",
                                azure_api("chat-completions", "@opencode/ai/providers/azure/chat"),
                            ),
                        ),
                        (
                            "claude".into(),
                            source_model(
                                "claude",
                                r#"{"api": {"messages": {
                                "base_url": "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1",
                                "sdk": {"aisdk": "@ai-sdk/anthropic", "opencodeai": "@opencode/ai/providers/anthropic-compatible"}
                            }}}"#,
                            ),
                        ),
                    ]),
                )]),
            ),
        ),
    ]);
    let kv = Arc::new(MemoryKv::default());
    write_cache(&kv, &catalog, now());
    let data = service(kv, offline()).get();
    let (acme, azure) = (&data[0], &data[1]);
    let model = |p: &Snapshot, id: &str| p.models.iter().find(|m| m.id == id).unwrap().clone();
    let r = model(acme, "acme-responses");
    assert_eq!(
        r.package.as_deref(),
        Some("@opencode/ai/providers/openai/responses")
    );
    assert_eq!(r.settings, None);
    assert_eq!(
        r.capabilities.freeform,
        Some(vec!["text".into(), "lark".into()])
    );
    assert_eq!(model(acme, "acme-1").capabilities.freeform, None);
    // Without an opencode package, the AI SDK package maps to the native one.
    let f = model(acme, "acme-fallback");
    assert_eq!(
        f.package.as_deref(),
        Some("@opencode/ai/providers/anthropic")
    );
    assert_eq!(f.capabilities.freeform, Some(vec![]));
    assert_eq!(
        serde_json::to_value(&f.settings).unwrap(),
        json!({"baseURL": "https://claude.acme.test/v1"})
    );

    assert_eq!(azure.info.package, "@opencode/ai/providers/azure/responses");
    assert_eq!(azure.info.settings, None);
    assert_eq!(model(azure, "gpt-5").package, None);
    assert_eq!(model(azure, "gpt-5").settings, None);
    let chat = model(azure, "custom-chat");
    assert_eq!(
        chat.package.as_deref(),
        Some("@opencode/ai/providers/azure/chat")
    );
    assert_eq!(chat.settings, None);
    assert_eq!(
        serde_json::to_value(&model(azure, "claude").settings).unwrap(),
        json!({
            "baseURL": "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1",
            "provider": "azure"
        })
    );
}

#[test]
fn oc_catalog_keeps_non_chat_models_with_their_type_maps_mime_modalities_and_skips_malformed_models(
) {
    let catalog = Json::Object(vec![(
        "acme".into(),
        acme_provider(vec![
            ("acme-1", acme_one()),
            (
                "acme-image",
                source_model(
                    "acme-image",
                    r#"{"type": "image",
                    "modalities": {"input": ["text", "image/png", "application/pdf"], "output": ["image/png", "image/webp"]},
                    "api": {
                        "chat-completions": {"base_url": "https://api.example.test/v1", "sdk": {"aisdk": "@acme/ai-sdk-provider"}},
                        "images": {"base_url": "https://api.example.test/v1", "sdk": {
                            "aisdk": "@acme/ai-sdk-provider",
                            "opencodeai": "@opencode/ai/providers/openai-compatible/images"
                        }}
                    },
                    "experimental": {"modes": {"fast": {}}},
                    "future_field": {"anything": true}}"#,
                ),
            ),
            ("acme-broken", j(r#"{"id": "acme-broken", "type": "chat"}"#)),
            (
                "acme-sdk-image",
                source_model(
                    "acme-sdk-image",
                    r#"{"type": "image", "api": {"images": {"base_url": "https://api.example.test/v1", "sdk": {"aisdk": "@acme/ai-sdk-provider"}}}}"#,
                ),
            ),
        ]),
    )]);
    let kv = Arc::new(MemoryKv::default());
    write_cache(&kv, &catalog, now());
    let data = service(kv, offline()).get();
    let acme = &data[0];
    let ids: Vec<(&str, &str)> = acme
        .models
        .iter()
        .map(|m| (m.id.as_str(), m.kind.as_str()))
        .collect();
    assert_eq!(
        ids,
        [
            ("acme-1", "chat"),
            ("acme-image", "image"),
            ("acme-image-fast", "image")
        ]
    );
    assert_eq!(acme.models[1].capabilities.input, ["text", "image", "pdf"]);
    assert_eq!(acme.models[1].capabilities.output, ["image"]);
    assert_eq!(
        acme.models[1].package.as_deref(),
        Some("@opencode/ai/providers/openai-compatible/images")
    );
}

#[test]
fn oc_catalog_maps_per_token_text_prices_context_tiers_and_mode_multipliers_onto_cost() {
    let catalog = Json::Object(vec![(
        "acme".into(),
        acme_provider(vec![(
            "acme-1",
            source_model(
                "acme-1",
                r#"{"cost": {
                "text": {
                    "input": {
                        "price": 2,
                        "cache": {"read": 0.2, "write": {"5m": 2.5, "1h": 4}},
                        "overrides": [
                            {"when": {"context": {"gt": 200000}}, "price": 4, "cache": {"read": 0.4}},
                            {"when": {"service_tier": {"equals": "flex"}}, "price": 1}
                        ]
                    },
                    "output": {"price": 10, "overrides": [{"when": {"context": {"gt": 200000}}, "price": 15}]}
                },
                "image": {"output": {"unit": "image", "price": 0.04}},
                "multipliers": [{"when": {"service_tier": {"equals": "priority"}}, "factor": {"text": 2}}]
            },
            "experimental": {"modes": {"fast": {"service_tier": "priority", "body": {"service_tier": "priority"}}}}}"#,
            ),
        )]),
    )]);
    let kv = Arc::new(MemoryKv::default());
    write_cache(&kv, &catalog, now());
    let data = service(kv, offline()).get();
    let acme = &data[0];
    let tier = json!({"tier": {"type": "context", "size": 200000.0}, "input": 4.0, "output": 15.0,
        "cache": {"read": 0.4, "write": 2.5}});
    assert_eq!(
        serde_json::to_value(&acme.models[0].cost).unwrap(),
        json!([{"input": 2.0, "output": 10.0, "cache": {"read": 0.2, "write": 2.5}}, tier])
    );
    assert_eq!(
        serde_json::to_value(&acme.models[1].cost).unwrap(),
        json!([{"input": 4.0, "output": 20.0, "cache": {"read": 0.4, "write": 5.0}}, tier])
    );
    assert_eq!(
        Value::Object(acme.models[1].body.clone().unwrap()),
        json!({"service_tier": "priority"})
    );
}

#[test]
fn oc_catalog_skips_apis_whose_opencode_entrypoint_this_build_doesnt_have() {
    let unknown = |path: &str| {
        format!(
            r#"{{"base_url": "https://new.acme.test/v1", "sdk": {{"opencodeai": "@opencode/ai/providers/acme/{path}"}}}}"#
        )
    };
    let image = |id: &str, api: &str| {
        source_model(
            id,
            &format!(
                r#"{{"type": "image", "modalities": {{"input": ["text"], "output": ["image/png"]}}, "api": {api}}}"#
            ),
        )
    };
    let catalog = Json::Object(vec![(
        "acme".into(),
        acme_provider(vec![
            (
                "next",
                source_model(
                    "next",
                    &format!(
                        r#"{{"api": {{"new-api": {}, "chat-completions": {{
                        "base_url": "https://api.example.test/v1",
                        "sdk": {{"aisdk": "@ai-sdk/openai-compatible", "opencodeai": "@opencode/ai/providers/openai-compatible"}}
                    }}}}}}"#,
                        unknown("new-api")
                    ),
                ),
            ),
            (
                "sdk-fallback",
                source_model(
                    "sdk-fallback",
                    r#"{"api": {"new-api": {"base_url": "https://claude.acme.test/v1",
                    "sdk": {"aisdk": "@ai-sdk/anthropic", "opencodeai": "@opencode/ai/providers/acme/new-api"}}}}"#,
                ),
            ),
            (
                "unknown-only",
                source_model(
                    "unknown-only",
                    &format!(r#"{{"api": {{"new-api": {}}}}}"#, unknown("new-api")),
                ),
            ),
            (
                "image-unknown",
                image(
                    "image-unknown",
                    &format!(r#"{{"images": {}}}"#, unknown("images")),
                ),
            ),
            (
                "image-known",
                image(
                    "image-known",
                    &format!(
                        r#"{{"images": {}, "openai-images": {{"base_url": "https://api.acme.test/v1",
                        "sdk": {{"opencodeai": "@opencode/ai/providers/openai-compatible/images"}}}}}}"#,
                        unknown("images")
                    ),
                ),
            ),
        ]),
    )]);
    let data = decode(&catalog);
    let acme = &data[0];
    let ids: Vec<&str> = acme.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["next", "sdk-fallback", "image-known"]);
    assert_eq!(
        acme.info.package,
        "@opencode/ai/providers/openai-compatible"
    );
    assert_eq!(
        acme.info.settings.as_ref().unwrap().base_url.as_deref(),
        Some("https://api.example.test/v1")
    );
    let model = |id: &str| acme.models.iter().find(|m| m.id == id).unwrap();
    assert_eq!(model("next").package, None);
    assert_eq!(
        serde_json::to_value(&model("next").settings).unwrap(),
        json!({"provider": "acme"})
    );
    assert_eq!(
        model("sdk-fallback").package.as_deref(),
        Some("@opencode/ai/providers/anthropic")
    );
    assert_eq!(
        serde_json::to_value(&model("sdk-fallback").settings).unwrap(),
        json!({"baseURL": "https://claude.acme.test/v1"})
    );
    assert_eq!(
        model("image-known").package.as_deref(),
        Some("@opencode/ai/providers/openai-compatible/images")
    );
    assert_eq!(
        serde_json::to_value(&model("image-known").settings).unwrap(),
        json!({"baseURL": "https://api.acme.test/v1"})
    );
}

#[test]
fn oc_catalog_skips_non_chat_models_whose_only_opencode_package_serves_another_type() {
    let api = |o: &str| {
        format!(
            r#"{{"chat-completions": {{"base_url": "https://api.acme.test/v1", "sdk": {{"opencodeai": "{o}"}}}}}}"#
        )
    };
    let chat = "@opencode/ai/providers/openai-compatible";
    let typed = |id: &str, kind: &str, api: String| {
        source_model(id, &format!(r#"{{"type": "{kind}", "api": {api}}}"#))
    };
    let catalog = Json::Object(vec![(
        "acme".into(),
        acme_provider(vec![
            ("chat", typed("chat", "chat", api(chat))),
            (
                "image-chat-only",
                typed("image-chat-only", "image", api(chat)),
            ),
            (
                "image",
                typed(
                    "image",
                    "image",
                    format!(
                        r#"{{"chat-completions": {{"base_url": "https://api.acme.test/v1", "sdk": {{"opencodeai": "{chat}"}}}},
                        "images": {{"base_url": "https://api.acme.test/v1", "sdk": {{"opencodeai": "@opencode/ai/providers/openai-compatible/images"}}}}}}"#
                    ),
                ),
            ),
            (
                "decision-chat-only",
                typed("decision-chat-only", "decision", api(chat)),
            ),
            (
                "decision-image-only",
                typed(
                    "decision-image-only",
                    "decision",
                    api("@opencode/ai/providers/openrouter/images"),
                ),
            ),
            (
                "decision",
                typed(
                    "decision",
                    "decision",
                    api("@opencode/ai/providers/openrouter/decisions"),
                ),
            ),
        ]),
    )]);
    let data = decode(&catalog);
    let acme = &data[0];
    let got: Vec<(String, String, String)> = acme
        .models
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                m.kind.clone(),
                m.package.clone().unwrap_or(acme.info.package.clone()),
            )
        })
        .collect();
    let want: Vec<(String, String, String)> = [
        ("chat", "chat", chat),
        (
            "image",
            "image",
            "@opencode/ai/providers/openai-compatible/images",
        ),
        (
            "decision",
            "decision",
            "@opencode/ai/providers/openrouter/decisions",
        ),
    ]
    .iter()
    .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
    .collect();
    assert_eq!(got, want);
}

#[test]
fn oc_catalog_drops_model_types_not_kept() {
    let model = |id: &str, kind: &str| {
        j(&format!(
            r#"{{"id": "{id}", "type": "{kind}", "name": "{id}", "release_date": "2026-01-01",
            "modalities": {{"input": ["text"], "output": ["image"]}},
            "capabilities": {{"tools": {{"supported": false}}, "reasoning": {{"supported": false}}}},
            "api": {{"images": {{"base_url": "https://api.acme.test/v1", "sdk": {{"opencodeai": "@opencode/ai/providers/openai/images"}}}}}}}}"#
        ))
    };
    let catalog = Json::Object(vec![(
        "acme".into(),
        acme_provider(vec![
            ("acme-image", model("acme-image", "image")),
            ("acme-video", model("acme-video", "video")),
        ]),
    )]);
    let kv = Arc::new(MemoryKv::default());
    write_cache(&kv, &catalog, now());
    let data = service(kv, offline()).get();
    let ids: Vec<&str> = data[0].models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["acme-image"]);
}

#[test]
fn oc_catalog_skips_api_entries_and_modes_it_cant_read_and_keeps_the_model() {
    let catalog = j(
        r#"{"acme": {"id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "models": {
        "acme-1": {
            "id": "acme-1", "type": "chat", "name": "Acme One", "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "api": {
                "future": {"url": 42},
                "chat-completions": {"base_url": "https://api.acme.test/v1", "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible"}}
            },
            "experimental": {"modes": {"future": {"cost": "free"}, "fast": {"body": {"service_tier": "priority"}}}}
        }
    }}}"#,
    );
    let kv = Arc::new(MemoryKv::default());
    write_cache(&kv, &catalog, now());
    let data = service(kv, offline()).get();
    let acme = &data[0];
    assert_eq!(
        acme.info.package,
        "@opencode/ai/providers/openai-compatible"
    );
    assert_eq!(
        acme.info.settings.as_ref().unwrap().base_url.as_deref(),
        Some("https://api.acme.test/v1")
    );
    let ids: Vec<&str> = acme.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["acme-1", "acme-1-fast"]);
}

#[test]
fn oc_catalog_reads_a_models_dev_v1_catalog_by_converting_it_to_v2() {
    let catalog = j(r#"{"acme": {
        "id": "acme", "name": "Acme", "env": ["ACME_API_KEY"],
        "npm": "@ai-sdk/openai-compatible", "api": "https://api.acme.test/v1",
        "models": {"acme-1": {
            "id": "acme-1", "name": "Acme One", "release_date": "2026-01-01",
            "attachment": false, "reasoning": true,
            "reasoning_options": [{"type": "effort", "values": [null, "low", "high"]}, {"type": "budget_tokens", "min": 1024}],
            "temperature": true, "tool_call": true,
            "interleaved": {"field": "vendor_reasoning"},
            "cost": {"input": 1, "output": 2, "tiers": [{"tier": {"type": "context", "size": 200000}, "input": 3, "output": 4}]},
            "limit": {"context": 128000, "output": 8192}
        }}
    }}"#);
    let kv = Arc::new(MemoryKv::default());
    write_cache(&kv, &catalog, now());
    let data = service(kv, offline()).get();
    let acme = &data[0];
    assert_eq!(
        acme.info.package,
        "@opencode/ai/providers/openai-compatible"
    );
    assert_eq!(
        acme.info.settings.as_ref().unwrap().base_url.as_deref(),
        Some("https://api.acme.test/v1")
    );
    let m = &acme.models[0];
    assert_eq!(m.kind, "chat");
    assert_eq!(
        m.compatibility.as_ref().unwrap().reasoning_field.as_deref(),
        Some("vendor_reasoning")
    );
    assert_eq!(
        serde_json::to_value(&m.capabilities).unwrap(),
        json!({"tools": true, "input": ["text"], "output": ["text"], "temperature": true})
    );
    let variants: Vec<&str> = m.variants.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(variants, ["low", "high"]);
    let costs: Vec<(Option<f64>, f64, f64)> = m
        .cost
        .iter()
        .map(|c| (c.tier.map(|t| t.size), c.input, c.output))
        .collect();
    assert_eq!(costs, [(None, 1.0, 2.0), (Some(200000.0), 3.0, 4.0)]);
}

fn v1_beta() -> String {
    r#"{"beta": {"id": "beta", "name": "Beta", "env": ["BETA_API_KEY"],
        "npm": "@ai-sdk/openai-compatible", "api": "https://api.beta.test/v1",
        "models": {"beta-1": {"id": "beta-1", "name": "Beta One", "release_date": "2026-02-01",
            "tool_call": true, "limit": {"context": 64000, "output": 4096}}}}}"#
        .into()
}

#[test]
fn oc_catalog_falls_back_to_api_json_when_a_custom_mirror_has_no_v2_catalog() {
    let server = Server::start(v1_beta());
    server.set(|s| {
        s.routes.push((CATALOG_PATH.into(), 404, String::new()));
    });
    let s = service(Arc::new(MemoryKv::default()), server.options());
    let data = s.get();
    let paths: Vec<String> = server.calls().into_iter().map(|c| c.path).collect();
    assert_eq!(
        paths,
        [
            "/experimental/v2.0/api.json?type=chat,decision,image,video",
            "/api.json"
        ]
    );
    let ids: Vec<&str> = data[0].models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["beta-1"]);
}

#[test]
fn oc_catalog_get_returns_empty_catalog_when_kv_is_empty_fetch_disabled_and_the_bundled_snapshot_is_disabled(
) {
    let server = Server::start(text(&fixture()));
    let s = service(
        Arc::new(MemoryKv::default()),
        Options {
            fetch: false,
            ..server.options()
        },
    );
    assert!(s.get().is_empty());
    assert!(server.calls().is_empty());
}

#[test]
fn oc_catalog_get_falls_back_to_the_bundled_snapshot_when_kv_is_empty_and_fetch_is_disabled() {
    let server = Server::start(text(&fixture()));
    let s = service(
        Arc::new(MemoryKv::default()),
        Options {
            fetch: false,
            snapshot: true,
            ..server.options()
        },
    );
    let data = s.get();
    assert!(data.len() > 100, "{} providers", data.len());
    let anthropic = data.iter().find(|p| p.info.id == "anthropic").unwrap();
    assert!(anthropic
        .environment
        .contains(&"ANTHROPIC_API_KEY".to_string()));
    assert!(server.calls().is_empty());
}

#[test]
fn oc_catalog_get_recovers_from_a_corrupted_kv_entry_by_fetching_a_fresh_catalog() {
    let server = Server::start(text(&fixture2()));
    let kv = Arc::new(MemoryKv::default());
    let key = server.key();
    kv.set(
        &key,
        json!({"updatedAt": now(), "digest": body_digest("{"), "body": "{"}),
    )
    .unwrap();
    let s = service(kv.clone(), server.options());
    assert_eq!(as_json(&s.get()), fixture2_snapshot());
    let entry = kv.get(&key).unwrap();
    assert_eq!(entry["body"], text(&fixture2()));
    assert_eq!(entry["digest"], body_digest(&text(&fixture2())));
    assert_eq!(server.calls().len(), 1);
}

/// A store whose writes fail, as an over-full KV backend's do.
#[derive(Default)]
struct FailingWrites(MemoryKv);

impl Kv for FailingWrites {
    fn get(&self, key: &str) -> Option<Value> {
        self.0.get(key)
    }
    fn set(&self, _: &str, _: Value) -> anyhow::Result<()> {
        anyhow::bail!("insert into kv failed")
    }
    fn remove(&self, key: &str) {
        self.0.remove(key)
    }
}

#[test]
fn oc_catalog_get_still_populates_the_catalog_when_the_kv_cache_write_fails() {
    let server = Server::start(text(&fixture2()));
    let kv = Arc::new(FailingWrites::default());
    let s = ModelsDev::new(server.options(), kv.clone());
    assert_eq!(as_json(&s.get()), fixture2_snapshot());
    assert!(kv.get(&server.key()).is_none());
    assert_eq!(server.calls().len(), 1);
}

#[test]
fn oc_catalog_uses_the_default_models_url_when_the_configured_url_is_empty() {
    // Adapted: the request itself would leave the machine, so the test
    // checks the source the catalog is fetched (and cached) from. CodeSmith's
    // default is the public models.dev catalog (its v1 `/api.json`), not
    // OpenCode's server.
    let s = service(
        Arc::new(MemoryKv::default()),
        Options {
            url: Some(String::new()),
            ..offline()
        },
    );
    assert_eq!(
        format!("{}{}", s.source(), catalog::V1_PATH),
        "https://models.dev/api.json"
    );
    assert_eq!(s.key(), CACHE_KEY);
}

#[test]
fn fetching_is_off_unless_asked_for() {
    // Not an OpenCode test: CodeSmith makes no network request for the
    // catalog by default. The variables are only read here.
    for v in [
        "CODESMITH_MODELS_FETCH",
        "CODESMITH_MODELS_URL",
        "CODESMITH_DISABLE_MODELS_FETCH",
    ] {
        std::env::remove_var(v);
    }
    assert!(!Options::default().fetch);
    assert!(!Options::resolve(None, None).fetch);
    assert!(Options::resolve(Some(true), None).fetch);
    // A named server turns it on, unless `fetch` says otherwise.
    let o = Options::resolve(None, Some("http://mirror"));
    assert!(o.fetch);
    assert_eq!(o.url.as_deref(), Some("http://mirror"));
    assert!(!Options::resolve(Some(false), Some("http://mirror")).fetch);
    std::env::set_var("CODESMITH_MODELS_FETCH", "1");
    assert!(Options::resolve(Some(false), None).fetch);
    std::env::set_var("CODESMITH_MODELS_FETCH", "0");
    assert!(!Options::resolve(Some(true), None).fetch);
    std::env::remove_var("CODESMITH_MODELS_FETCH");
    std::env::set_var("CODESMITH_MODELS_URL", "http://env");
    let o = Options::resolve(None, Some("http://config"));
    assert_eq!((o.fetch, o.url.as_deref()), (true, Some("http://env")));
    std::env::set_var("CODESMITH_DISABLE_MODELS_FETCH", "1");
    assert!(!Options::resolve(Some(true), None).fetch);
    assert!(catalog::fetch_disabled());
    for v in ["CODESMITH_MODELS_URL", "CODESMITH_DISABLE_MODELS_FETCH"] {
        std::env::remove_var(v);
    }
    // Without fetching, nothing is ever requested, even with no snapshot.
    let server = Server::start(text(&fixture()));
    let s = Arc::new(service(
        Arc::new(MemoryKv::default()),
        Options {
            fetch: false,
            ..server.options()
        },
    ));
    assert!(s.get().is_empty());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        s.spawn_refresh();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    });
    assert!(server.calls().is_empty());
}

#[tokio::test]
async fn an_on_demand_refresh_says_whether_the_catalog_changed() {
    let server = Server::start(text(&fixture()));
    let kv = Arc::new(MemoryKv::default());
    let s = service(kv, server.options());
    assert_eq!(
        s.try_refresh(true).await.unwrap(),
        catalog::Refreshed::Updated
    );
    assert_eq!(
        s.try_refresh(true).await.unwrap(),
        catalog::Refreshed::Unchanged
    );
    server.set(|st| st.status = 404);
    assert!(s.try_refresh(true).await.is_err());
}

#[test]
fn oc_catalog_get_is_single_flight_under_concurrent_calls() {
    let server = Server::start(text(&fixture()));
    let s = Arc::new(service(Arc::new(MemoryKv::default()), server.options()));
    let results: Vec<Value> = (0..5)
        .map(|_| {
            let s = s.clone();
            std::thread::spawn(move || as_json(&s.get()))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    for r in results {
        assert_eq!(r, fixture_snapshot());
    }
    assert_eq!(server.calls().len(), 1);
}

#[test]
fn oc_catalog_get_caches_across_calls_later_kv_writes_are_ignored_until_invalidate() {
    let kv = Arc::new(MemoryKv::default());
    write_cache(&kv, &fixture(), now());
    let s = service(kv.clone(), offline());
    let a = as_json(&s.get());
    write_cache(&kv, &fixture2(), now());
    let b = as_json(&s.get());
    assert_eq!(a, fixture_snapshot());
    assert_eq!(b, fixture_snapshot());
}

#[tokio::test]
async fn oc_catalog_refresh_true_fetches_via_http_and_updates_the_cache() {
    let server = Server::start(text(&fixture2()));
    let kv = Arc::new(MemoryKv::default());
    write_cache_at(&kv, &server.key(), &fixture(), now());
    let s = service(
        kv.clone(),
        Options {
            fetch: false,
            ..server.options()
        },
    );
    let before = as_json(&s.get());
    s.refresh(true).await;
    let after = as_json(&s.get());
    assert_eq!(before, fixture_snapshot());
    assert_eq!(after, fixture2_snapshot());
    let entry = kv.get(&server.key()).unwrap();
    assert_eq!(entry["body"], text(&fixture2()));
    assert_eq!(entry["digest"], body_digest(&text(&fixture2())));
    let calls = server.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].path.contains(CATALOG_PATH));
    // A generic User-Agent: the program and version, nothing about the user.
    let ua = calls[0].user_agent.clone().unwrap();
    assert_eq!(ua, catalog::user_agent());
    assert!(ua.starts_with("codesmith/"), "{ua}");
}

#[tokio::test]
async fn oc_catalog_refresh_false_skips_fetch_when_the_kv_entry_is_fresh() {
    let server = Server::start(text(&fixture2()));
    let kv = Arc::new(MemoryKv::default());
    write_cache_at(&kv, &server.key(), &fixture(), now() - 1000);
    let s = service(
        kv,
        Options {
            fetch: false,
            ..server.options()
        },
    );
    s.refresh(false).await;
    assert!(server.calls().is_empty());
}

#[tokio::test]
async fn oc_catalog_refresh_false_fetches_when_the_kv_entry_is_stale() {
    let server = Server::start(text(&fixture2()));
    let kv = Arc::new(MemoryKv::default());
    write_cache_at(&kv, &server.key(), &fixture(), now() - 10 * 60 * 1000);
    let s = service(
        kv.clone(),
        Options {
            fetch: false,
            ..server.options()
        },
    );
    let mut refreshed = s.subscribe();
    s.refresh(false).await;
    assert!(refreshed.has_changed().unwrap());
    assert_eq!(*refreshed.borrow_and_update(), 1);
    assert_eq!(server.calls().len(), 1);
    assert_eq!(as_json(&s.get()), fixture2_snapshot());
    let entry = kv.get(&server.key()).unwrap();
    assert_eq!(entry["body"], text(&fixture2()));
    assert_eq!(entry["digest"], body_digest(&text(&fixture2())));
}

#[tokio::test]
async fn oc_catalog_refresh_false_stays_quiet_when_the_fetched_body_matches_the_cached_digest() {
    let server = Server::start(text(&fixture()));
    let kv = Arc::new(MemoryKv::default());
    let key = server.key();
    write_cache_at(&kv, &key, &fixture(), now() - 10 * 60 * 1000);
    let body = kv.get(&key);
    let s = service(
        kv.clone(),
        Options {
            fetch: false,
            ..server.options()
        },
    );
    let refreshed = s.subscribe();
    s.refresh(false).await;
    assert!(!refreshed.has_changed().unwrap());
    assert_eq!(server.calls().len(), 1);
    assert_eq!(kv.get(&key), body);
    let meta = kv.get(&format!("{key}:meta")).unwrap();
    assert_eq!(meta["digest"], body_digest(&text(&fixture())));
    assert!(now() - meta["updatedAt"].as_i64().unwrap() < 60_000);
}

#[tokio::test]
async fn oc_catalog_refresh_false_revalidates_with_the_stored_etag_and_treats_304_as_unchanged() {
    let server = Server::start(text(&fixture()));
    server.set(|s| s.etag = Some(r#"W/"v1""#.into()));
    let kv = Arc::new(MemoryKv::default());
    let key = server.key();
    let meta_key = format!("{key}:meta");
    let s = service(
        kv.clone(),
        Options {
            fetch: false,
            ..server.options()
        },
    );
    s.refresh(true).await;
    let mut meta = kv.get(&meta_key).unwrap();
    meta["updatedAt"] = json!(now() - 10 * 60 * 1000);
    kv.set(&meta_key, meta).unwrap();
    let body = kv.get(&key);
    server.set(|s| s.status = 304);
    let refreshed = s.subscribe();
    s.refresh(false).await;
    assert!(!refreshed.has_changed().unwrap());
    assert_eq!(as_json(&s.get()), fixture_snapshot());
    let etags: Vec<Option<String>> = server
        .calls()
        .into_iter()
        .map(|c| c.if_none_match)
        .collect();
    assert_eq!(etags, [None, Some(r#"W/"v1""#.into())]);
    assert_eq!(kv.get(&key), body);
    let meta = kv.get(&meta_key).unwrap();
    assert_eq!(meta["etag"], r#"W/"v1""#);
    assert!(now() - meta["updatedAt"].as_i64().unwrap() < 60_000);

    // An unrequested 304 (a forced refresh sends no ETag) is a failure and
    // changes nothing.
    let fresh = service(
        kv.clone(),
        Options {
            fetch: false,
            ..server.options()
        },
    );
    fresh.refresh(true).await;
    assert_eq!(as_json(&fresh.get()), fixture_snapshot());
    assert_eq!(kv.get(&key), body);
}

#[tokio::test]
async fn oc_catalog_refresh_false_republishes_once_for_legacy_cache_entries_without_a_digest() {
    let server = Server::start(text(&fixture()));
    let kv = Arc::new(MemoryKv::default());
    let key = server.key();
    kv.set(
        &key,
        json!({"updatedAt": now() - 10 * 60 * 1000, "body": text(&fixture())}),
    )
    .unwrap();
    let s = service(
        kv.clone(),
        Options {
            fetch: false,
            ..server.options()
        },
    );
    let refreshed = s.subscribe();
    s.refresh(false).await;
    assert!(refreshed.has_changed().unwrap());
    // The rewritten entry carries a digest, so later identical bodies stay quiet.
    assert_eq!(
        kv.get(&key).unwrap()["digest"],
        body_digest(&text(&fixture()))
    );
}

#[tokio::test]
async fn oc_catalog_refresh_swallows_http_errors_and_leaves_cache_intact() {
    let server = Server::start("boom".into());
    server.set(|s| s.status = 500);
    let kv = Arc::new(MemoryKv::default());
    write_cache_at(&kv, &server.key(), &fixture(), now());
    let s = service(
        kv,
        Options {
            fetch: false,
            ..server.options()
        },
    );
    s.refresh(true).await;
    assert_eq!(as_json(&s.get()), fixture_snapshot());
    // Transient (5xx) responses are retried.
    assert!(!server.calls().is_empty());
}

#[tokio::test]
async fn oc_catalog_background_refresh_never_blocks_and_fills_the_cache() {
    // Not an OpenCode test: the background refresh CodeSmith starts.
    let server = Server::start(text(&fixture2()));
    let kv = Arc::new(MemoryKv::default());
    write_cache_at(&kv, &server.key(), &fixture(), now() - 10 * 60 * 1000);
    let s = Arc::new(service(kv, server.options()));
    let mut refreshed = s.subscribe();
    assert_eq!(as_json(&s.get()), fixture_snapshot());
    s.spawn_refresh();
    tokio::time::timeout(std::time::Duration::from_secs(20), refreshed.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(as_json(&s.get()), fixture2_snapshot());
}

// ---------------------------------------------------------------------------
// plugin/models-dev.test.ts: what the catalog offers.

fn file_service(file: &str) -> ModelsDev {
    let path = format!(
        "{}/tests/fixtures/models-dev/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    ModelsDev::new(
        Options {
            file: Some(path.into()),
            fetch: false,
            ..Options::default()
        },
        Arc::new(MemoryKv::default()),
    )
}

fn with_catalog(config: Value, data: Vec<Snapshot>) -> Config {
    let mut c: Config = serde_json::from_value(config).unwrap();
    c.models_dev = Some(Arc::new(ModelsDev::fixed(data)));
    c
}

fn snapshot(v: Json) -> Vec<Snapshot> {
    decode(&v)
}

/// One provider with a mode model, as `projects normalized definitions`.
fn rich() -> Json {
    j(
        r#"{"acme": {"id": "acme", "name": "Acme", "env": ["ACME_API_KEY"], "models": {
        "gpt-5.4": {
            "id": "gpt-5.4", "type": "chat", "name": "GPT-5.4", "family": "gpt",
            "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "limit": {"context": 1050000, "input": 922000, "output": 128000},
            "cost": {"text": {
                "input": {"price": 2.5, "overrides": [
                    {"when": {"context": {"gt": 272000}}, "price": 3, "cache": {"read": 0.25}},
                    {"when": {"context": {"gt": 200000}}, "price": 5, "cache": {"read": 0.5}}
                ]},
                "output": {"price": 15, "overrides": [
                    {"when": {"context": {"gt": 272000}}, "price": 18},
                    {"when": {"context": {"gt": 200000}}, "price": 22.5}
                ]}
            }},
            "api": {"chat-completions": {"base_url": "https://api.acme.test/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible"}}},
            "experimental": {"modes": {"fast": {
                "cost": {"text": {"input": {"price": 5, "cache": {"read": 0.5}}, "output": {"price": 30}}},
                "body": {"service_tier": "priority"}, "headers": {"x-mode": "fast"}
            }}}
        }
    }}}"#,
    )
}

#[test]
fn oc_catalog_projects_normalized_definitions_for_available_providers() {
    let data = snapshot(rich());
    let c = with_catalog(json!({}), data.clone());
    let catalog = c.models_dev.as_ref().unwrap().catalog();
    let base = catalog.model("acme", "gpt-5.4").unwrap();
    assert!(base.variants.is_empty());
    assert_eq!(base.body, None);
    let fast = catalog.model("acme", "gpt-5.4-fast").unwrap();
    assert_eq!(
        (fast.id.as_str(), fast.model_id.as_str(), fast.name.as_str()),
        ("gpt-5.4-fast", "gpt-5.4", "GPT-5.4 Fast")
    );
    assert_eq!(
        serde_json::to_value(&fast.cost).unwrap(),
        json!([
            {"input": 5.0, "output": 30.0, "cache": {"read": 0.5, "write": 0.0}},
            {"tier": {"type": "context", "size": 272000.0}, "input": 3.0, "output": 18.0, "cache": {"read": 0.25, "write": 0.0}},
            {"tier": {"type": "context", "size": 200000.0}, "input": 5.0, "output": 22.5, "cache": {"read": 0.5, "write": 0.0}}
        ])
    );
    // Resolving the mode sends the catalog model with its body and headers.
    let m = c.resolve_model("acme/gpt-5.4-fast").unwrap();
    assert_eq!(m.model_id, "gpt-5.4");
    assert_eq!(m.provider.base_url, "https://api.acme.test/v1");
    assert_eq!(m.body["service_tier"], "priority");
    assert!(m
        .provider
        .headers
        .contains(&("x-mode".to_string(), "fast".to_string())));
    assert_eq!((m.context, m.max_output), (Some(1_050_000), Some(128_000)));
    let cost = c.cost("acme/gpt-5.4-fast").unwrap();
    assert_eq!((cost.input, cost.output, cost.cache_read), (5.0, 30.0, 0.5));
}

#[test]
fn oc_catalog_keeps_the_shared_snapshot_pristine_while_provider_and_model_transforms_mutate_owned_records(
) {
    let data = snapshot(rich());
    let before = as_json(&data);
    let service = Arc::new(ModelsDev::fixed(data));
    let mut c: Config = serde_json::from_value(json!({"providers": {"acme": {
        "settings": {"baseURL": "https://override.acme.test/v1"},
        "headers": {"x-acme": "override"},
        "models": {"gpt-5.4": {
            "modalities": {"input": ["text", "image"]},
            "limit": {"context": 1},
            "variants": {"configured": {"reasoningEffort": "high"}}
        }}
    }}}))
    .unwrap();
    c.models_dev = Some(service.clone());
    let m = c.resolve_model("acme/gpt-5.4").unwrap();
    assert_eq!(m.provider.base_url, "https://override.acme.test/v1");
    assert_eq!(m.input, Some(vec!["text".into(), "image".into()]));
    assert_eq!(m.context, Some(1));
    assert_eq!(m.variants, ["configured"]);
    assert_eq!(as_json(&service.get()), before);
}

#[test]
fn oc_catalog_omits_deprecated_and_non_text_model_definitions() {
    let model = |id: &str, status: &str, input: &str, output: &str| {
        format!(
            r#""{id}": {{"id": "{id}", "type": "chat", "name": "{id}", "release_date": "2026-01-01", "status": "{status}",
            "modalities": {{"input": {input}, "output": {output}}},
            "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}}},
            "limit": {{"context": 128000, "output": 32000}},
            "api": {{"chat-completions": {{"base_url": "https://api.acme.test/v1", "sdk": {{"opencodeai": "@opencode/ai/providers/openai-compatible"}}}}}}}}"#
        )
    };
    let catalog = j(&format!(
        r#"{{"acme": {{"id": "acme", "name": "Acme", "env": [], "models": {{{}, {}, {}, {}}}}}}}"#,
        model("current", "active", r#"["text", "image"]"#, r#"["text"]"#),
        model(
            "legacy",
            "deprecated",
            r#"["text", "image"]"#,
            r#"["text"]"#
        ),
        model(
            "video-gen",
            "active",
            r#"["text", "image"]"#,
            r#"["video"]"#
        ),
        model("transcribe", "active", r#"["audio"]"#, r#"["text"]"#),
    ));
    let c = with_catalog(json!({}), snapshot(catalog));
    let catalog = c.models_dev.as_ref().unwrap().catalog();
    assert!(catalog.model("acme", "current").is_some());
    assert!(catalog.model("acme", "legacy").is_none());
    assert!(catalog.model("acme", "video-gen").is_none());
    assert!(catalog.model("acme", "transcribe").is_none());
    assert_eq!(c.list_models("acme", &[]), ["current"]);
}

#[test]
fn oc_catalog_lists_non_chat_models_only_by_type_so_sessions_never_see_them() {
    let model = |id: &str, kind: &str, api: &str| {
        format!(
            r#""{id}": {{"id": "{id}", "type": "{kind}", "name": "{id}", "release_date": "2026-01-01",
            "modalities": {{"input": ["text"], "output": ["text"]}},
            "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}}},
            "limit": {{"context": 128000, "output": 32000}},
            "api": {{"x": {{"base_url": "https://api.acme.test/v1", "sdk": {{"opencodeai": "@opencode/ai/providers/{api}"}}}}}}}}"#
        )
    };
    let catalog = j(&format!(
        r#"{{"acme": {{"id": "acme", "name": "Acme", "env": [], "models": {{{}, {}, {}, {}}}}}}}"#,
        model("gpt-5.5", "chat", "openai-compatible"),
        model("gpt-audio", "chat", "openai-compatible"),
        model("jev-1.13", "decision", "openai-compatible/decisions"),
        model("gpt-image-2", "image", "openai-compatible/images"),
    ));
    let c = with_catalog(json!({}), snapshot(catalog));
    assert_eq!(c.list_models("acme", &[]), ["gpt-5.5", "gpt-audio"]);
    let catalog = c.models_dev.as_ref().unwrap().catalog();
    assert!(catalog.model("acme", "jev-1.13").is_none());
    assert_eq!(c.list_models("acme", &["decision"]), ["jev-1.13"]);
    assert_eq!(c.list_models("acme", &["image"]), ["gpt-image-2"]);
    assert_eq!(
        c.list_models("acme", &["chat", "image", "image"]),
        ["gpt-5.5", "gpt-audio", "gpt-image-2"]
    );
    assert!(c.list_models("acme", &["embedding"]).is_empty());
}

#[test]
fn oc_catalog_registers_key_methods_for_providers_with_environment_variables() {
    let s = file_service("models-dev.json");
    let data = s.get();
    let with_env: Vec<(&str, &str, &[String])> = data
        .iter()
        .filter(|p| !p.environment.is_empty())
        .map(|p| {
            (
                p.info.id.as_str(),
                p.info.name.as_str(),
                p.environment.as_slice(),
            )
        })
        .collect();
    assert_eq!(
        with_env,
        [("acme", "Acme", &["ACME_API_KEY".to_string()][..])]
    );
    // The key comes from that variable when a model of the provider is used.
    let c = Config {
        models_dev: Some(Arc::new(s)),
        ..Default::default()
    };
    let p = c.provider("acme").unwrap();
    assert_eq!(p.env, ["ACME_API_KEY"]);
    assert_eq!(p.name.as_deref(), Some("Acme"));
    assert_eq!(
        p.settings.base_url.as_deref(),
        Some("https://api.acme.test/v1")
    );
}

#[test]
fn oc_catalog_preserves_provider_and_model_url_templates_in_the_catalog() {
    let catalog = j(r#"{"acme": {"id": "acme", "name": "Acme",
        "env": ["CODESMITH_TEST_ACME_HOST", "CODESMITH_TEST_ACME_MODEL_PATH", "ACME_API_KEY"], "models": {
        "gpt-5.4": {"id": "gpt-5.4", "type": "chat", "name": "GPT-5.4", "release_date": "2026-01-01",
            "modalities": {"input": [], "output": []},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "limit": {"context": 1050000, "output": 128000},
            "api": {"chat-completions": {"base_url": "https://${CODESMITH_TEST_ACME_HOST}/${CODESMITH_TEST_ACME_MODEL_PATH}/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/openai/chat"}}}},
        "other": {"id": "other", "type": "chat", "name": "Other", "release_date": "2026-01-01",
            "modalities": {"input": [], "output": []},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "api": {"chat-completions": {"base_url": "https://${CODESMITH_TEST_ACME_HOST}/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/openai/chat"}}}},
        "third": {"id": "third", "type": "chat", "name": "Third", "release_date": "2026-01-01",
            "modalities": {"input": [], "output": []},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "api": {"chat-completions": {"base_url": "https://${CODESMITH_TEST_ACME_HOST}/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/openai/chat"}}}}
    }}}"#);
    let data = snapshot(catalog);
    assert_eq!(
        data[0].info.settings.as_ref().unwrap().base_url.as_deref(),
        Some("https://${CODESMITH_TEST_ACME_HOST}/v1")
    );
    let model = data[0].models.iter().find(|m| m.id == "gpt-5.4").unwrap();
    assert_eq!(
        model.settings.as_ref().unwrap().base_url.as_deref(),
        Some("https://${CODESMITH_TEST_ACME_HOST}/${CODESMITH_TEST_ACME_MODEL_PATH}/v1")
    );
    // CodeSmith fills the templates from the environment when it resolves a
    // model, and passes over a URL whose variables are unset.
    std::env::set_var("CODESMITH_TEST_ACME_HOST", "api.acme.test");
    std::env::remove_var("CODESMITH_TEST_ACME_MODEL_PATH");
    let c = with_catalog(json!({}), data);
    assert_eq!(
        c.resolve_model("acme/gpt-5.4").unwrap().provider.base_url,
        "https://api.acme.test/v1"
    );
    assert_eq!(
        c.resolve_model("acme/other").unwrap().provider.base_url,
        "https://api.acme.test/v1"
    );
}

#[test]
fn oc_catalog_copies_model_request_bodies_without_reinterpreting_literal_proto_keys() {
    let catalog = j(
        r#"{"acme": {"id": "acme", "name": "Acme", "env": [], "models": {
        "gpt-5.4": {"id": "gpt-5.4", "type": "chat", "name": "GPT-5.4", "release_date": "2026-01-01",
            "modalities": {"input": [], "output": []},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "api": {"chat-completions": {"base_url": "https://api.acme.test/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible"}}},
            "experimental": {"modes": {"x": {"body": {"__proto__": {"service_tier": "priority"}, "keep": true}}}}}
    }}}"#,
    );
    let c = with_catalog(json!({}), snapshot(catalog));
    let m = c.resolve_model("acme/gpt-5.4-x").unwrap();
    assert_eq!(
        Value::Object(m.body.clone()),
        json!({"__proto__": {"service_tier": "priority"}, "keep": true})
    );
    assert!(m.body.get("service_tier").is_none());
}

#[test]
fn oc_catalog_omits_legacy_provider_aliases() {
    let provider = |id: &str, name: &str, env: &str| {
        format!(
            r#""{id}": {{"id": "{id}", "name": "{name}", "env": {env}, "models": {{"m": {{
            "id": "m", "type": "chat", "name": "m", "release_date": "2026-01-01",
            "modalities": {{"input": ["text"], "output": ["text"]}},
            "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}}},
            "api": {{"responses": {{"base_url": "", "sdk": {{"aisdk": "@ai-sdk/azure"}}}}}}}}}}}}"#
        )
    };
    let catalog = j(&format!(
        "{{{}, {}, {}, {}}}",
        provider(
            "azure",
            "Azure",
            r#"["AZURE_RESOURCE_NAME", "AZURE_API_KEY"]"#
        ),
        provider(
            "azure-cognitive-services",
            "Azure Cognitive Services",
            r#"["AZURE_COGNITIVE_SERVICES_API_KEY"]"#
        ),
        provider(
            "google-vertex",
            "Google Vertex",
            r#"["GOOGLE_APPLICATION_CREDENTIALS"]"#
        ),
        provider(
            "google-vertex-anthropic",
            "Google Vertex Anthropic",
            r#"["GOOGLE_APPLICATION_CREDENTIALS"]"#
        ),
    ));
    let c = with_catalog(json!({}), snapshot(catalog));
    let catalog = c.models_dev.as_ref().unwrap().catalog();
    assert!(catalog.provider("azure").is_some());
    assert!(catalog.provider("google-vertex").is_some());
    assert!(catalog.provider("azure-cognitive-services").is_none());
    assert!(catalog.provider("google-vertex-anthropic").is_none());
    assert_eq!(
        catalog.provider("azure").unwrap().environment,
        ["AZURE_RESOURCE_NAME", "AZURE_API_KEY"]
    );
}

#[test]
fn oc_catalog_registers_every_listed_environment_variable_for_provider_plugins_to_narrow() {
    let provider = |id: &str, name: &str, env: &str, package: &str| {
        format!(
            r#""{id}": {{"id": "{id}", "name": "{name}", "env": {env}, "models": {{"m": {{
            "id": "m", "type": "chat", "name": "m", "release_date": "2026-01-01",
            "modalities": {{"input": ["text"], "output": ["text"]}},
            "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}}},
            "api": {{"x": {{"sdk": {{"opencodeai": "@opencode/ai/providers/{package}"}}}}}}}}}}}}"#
        )
    };
    let catalog = j(&format!(
        "{{{}, {}}}",
        provider(
            "cloudflare-workers-ai",
            "Cloudflare Workers AI",
            r#"["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_KEY"]"#,
            "cloudflare-workers-ai"
        ),
        provider(
            "google-vertex",
            "Google Vertex",
            r#"["GOOGLE_VERTEX_PROJECT", "GOOGLE_VERTEX_LOCATION", "GOOGLE_APPLICATION_CREDENTIALS"]"#,
            "google-vertex"
        ),
    ));
    let data = snapshot(catalog);
    let env = |id: &str| {
        data.iter()
            .find(|p| p.info.id == id)
            .unwrap()
            .environment
            .clone()
    };
    assert_eq!(
        env("google-vertex"),
        [
            "GOOGLE_VERTEX_PROJECT",
            "GOOGLE_VERTEX_LOCATION",
            "GOOGLE_APPLICATION_CREDENTIALS"
        ]
    );
    assert_eq!(
        env("cloudflare-workers-ai"),
        ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_KEY"]
    );
}

#[test]
fn oc_catalog_converts_reasoning_options_into_settings_variants() {
    let s = file_service("models-dev-reasoning.json");
    let catalog = catalog::Catalog::new(&s.get());
    let variants = |p: &str, m: &str| -> Value {
        serde_json::to_value(
            &catalog
                .model(p, m)
                .unwrap_or_else(|| panic!("{p}/{m}"))
                .variants,
        )
        .unwrap()
    };
    let ids = |p: &str, m: &str| -> Vec<String> {
        catalog
            .model(p, m)
            .unwrap()
            .variants
            .iter()
            .map(|v| v.id.clone())
            .collect()
    };
    let responses = |e: &str| json!({"id": e, "settings": {"reasoningEffort": e, "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]}});
    assert_eq!(ids("openai", "gpt-reasoning"), ["low", "high"]);
    assert_eq!(
        variants("openai", "gpt-reasoning"),
        json!([responses("low"), responses("high")])
    );
    let mode = catalog.model("openai", "gpt-reasoning-high").unwrap();
    assert_eq!(mode.name, "GPT Reasoning High");
    assert_eq!(
        Value::Object(mode.headers.clone().unwrap()),
        json!({"x-mode": "high"})
    );
    assert_eq!(
        Value::Object(mode.body.clone().unwrap()),
        json!({"service_tier": "priority"})
    );
    assert_eq!(ids("openai", "gpt-reasoning-high"), ["low", "high"]);
    let pro = catalog.model("openai", "gpt-reasoning-pro").unwrap();
    assert_eq!(
        Value::Object(pro.body.clone().unwrap()),
        json!({"reasoning": {"mode": "pro"}})
    );

    let budget = variants("anthropic", "claude-budget");
    let budget = budget.as_array().unwrap();
    assert!(budget.contains(
        &json!({"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}})
    ));
    assert!(budget.contains(
        &json!({"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 31999}}})
    ));
    assert_eq!(
        variants("anthropic", "claude-opus-4.7"),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "low", "settings": {"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "low"}}
        ])
    );
    assert_eq!(
        variants("anthropic", "claude-toggle"),
        json!([
            {"id": "none", "settings": {"thinking": {"type": "disabled"}}},
            {"id": "thinking", "settings": {"thinking": {"type": "adaptive", "display": "summarized"}}}
        ])
    );
    assert_eq!(
        variants("anthropic", "claude-opus-4-5"),
        json!([
            {"id": "low", "settings": {"effort": "low", "thinking": {"type": "enabled", "budgetTokens": 8191}}},
            {"id": "high", "settings": {"effort": "high", "thinking": {"type": "enabled", "budgetTokens": 8191}}}
        ])
    );
    assert_eq!(
        variants("xai", "grok-4.5"),
        json!([responses("low"), responses("medium"), responses("high")])
    );
    let disabled = json!({"id": "none", "settings": {"thinking": {"type": "disabled"}}});
    assert_eq!(
        variants("opencode-go", "minimax-m3"),
        json!([disabled, {"id": "thinking", "settings": {"thinking": {"type": "adaptive", "display": "summarized"}}}])
    );
    assert_eq!(
        variants("alibaba", "toggle-only"),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "thinking", "settings": {"enableThinking": true}}
        ])
    );
    assert_eq!(
        variants("alibaba", "toggle-budget"),
        json!([
            {"id": "none", "settings": {"enableThinking": false}},
            {"id": "high", "settings": {"enableThinking": true, "thinkingBudget": 8000}},
            {"id": "max", "settings": {"enableThinking": true, "thinkingBudget": 16000}}
        ])
    );
    assert_eq!(
        variants("vercel", "alibaba/qwen-toggle"),
        json!([
            disabled,
            {"id": "high", "settings": {"thinking": {"type": "enabled", "budgetTokens": 8000}}},
            {"id": "max", "settings": {"thinking": {"type": "enabled", "budgetTokens": 16000}}}
        ])
    );
    let effort = |e: &str| json!({"id": e, "settings": {"reasoningEffort": e}});
    assert_eq!(
        variants("vercel", "amazon/nova-2-lite"),
        json!([disabled, effort("low"), effort("high")])
    );
    assert_eq!(
        variants("vercel", "deepseek/deepseek-toggle"),
        json!([disabled, effort("low"), effort("high")])
    );
    assert_eq!(
        variants("openrouter", "openrouter-toggle"),
        json!([
            {"id": "none", "settings": {"reasoning": {"enabled": false}}},
            {"id": "thinking", "settings": {"reasoning": {"enabled": true}}}
        ])
    );
    let gemini = json!([
        {"id": "none", "settings": {"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}}},
        {"id": "high", "settings": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 8000}}},
        {"id": "max", "settings": {"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 16000}}}
    ]);
    assert_eq!(variants("google", "gemini-2.5-flash"), gemini);
    assert_eq!(variants("google-vertex", "gemini-2.5-flash-lite"), gemini);
    let rc = |v: Value| json!({"additionalModelRequestFields": {"reasoningConfig": v}});
    assert_eq!(
        variants("amazon-bedrock", "us.amazon.nova-2-lite-v1:0"),
        json!([
            {"id": "none", "body": rc(json!({"type": "disabled"}))},
            {"id": "low", "body": rc(json!({"type": "enabled", "maxReasoningEffort": "low"}))},
            {"id": "high", "body": rc(json!({"type": "enabled", "maxReasoningEffort": "high"}))}
        ])
    );
    let sap = |v: Value| json!({"modelParams": v});
    assert_eq!(
        variants("sap-ai-core", "gemini-2.5-flash"),
        json!([
            {"id": "none", "settings": sap(json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}}))},
            {"id": "high", "settings": sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 8000}}))},
            {"id": "max", "settings": sap(json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 16000}}))}
        ])
    );
    let amr = |v: Value| sap(json!({ "additionalModelRequestFields": v }));
    assert_eq!(
        variants("sap-ai-core", "amazon--nova-lite"),
        json!([
            {"id": "none", "settings": amr(json!({"thinking": {"type": "disabled"}}))},
            {"id": "low", "settings": amr(json!({"output_config": {"effort": "low"}}))},
            {"id": "high", "settings": amr(json!({"output_config": {"effort": "high"}}))}
        ])
    );
    assert_eq!(
        variants("sap-ai-core", "cohere--command-a-reasoning"),
        json!([
            {"id": "none", "settings": sap(json!({"thinking": {"type": "disabled"}}))},
            {"id": "low", "settings": sap(json!({"reasoning_effort": "low"}))},
            {"id": "high", "settings": sap(json!({"reasoning_effort": "high"}))}
        ])
    );
    assert_eq!(
        variants("sap-ai-core", "anthropic--claude-4.7-opus"),
        json!([{"id": "low", "settings": amr(json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "low"}
        }))}])
    );
    assert_eq!(
        variants("sap-ai-core", "anthropic--claude-4-sonnet"),
        json!([
            {"id": "high", "settings": amr(json!({"thinking": {"type": "enabled", "budget_tokens": 8000}}))},
            {"id": "max", "settings": amr(json!({"thinking": {"type": "enabled", "budget_tokens": 16000}}))}
        ])
    );
}

#[test]
fn oc_catalog_adopts_a_refresh_that_completes_between_its_initial_read_and_its_subscription() {
    // Adapted: CodeSmith reads the catalog at each model resolution, so a
    // refreshed catalog is what the next resolution sees, whenever it landed.
    let server = Server::start(text(&fixture2()));
    let kv = Arc::new(MemoryKv::default());
    write_cache_at(&kv, &server.key(), &fixture(), now() - 10 * 60 * 1000);
    let s = Arc::new(service(
        kv,
        Options {
            fetch: false,
            ..server.options()
        },
    ));
    let c = Config {
        models_dev: Some(s.clone()),
        ..Default::default()
    };
    assert!(c.provider("acme").is_some());
    assert!(c.provider("beta").is_none());
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(s.refresh(false));
    assert!(c.provider("beta").is_some());
    assert_eq!(
        c.resolve_model("beta/beta-1").unwrap().context,
        Some(64_000)
    );
}

// ---------------------------------------------------------------------------
// config/provider.test.ts: config merged over catalog models.

fn custom_catalog(models: &str) -> Vec<Snapshot> {
    snapshot(j(&format!(
        r#"{{"custom": {{"id": "custom", "name": "Custom", "env": [], "models": {models}}}}}"#
    )))
}

fn custom_model(id: &str, extra: &str) -> String {
    let base = j(&format!(
        r#"{{"id": "{id}", "type": "chat", "name": "{id}", "release_date": "2026-01-01",
        "modalities": {{"input": ["text"], "output": ["text"]}},
        "capabilities": {{"tools": {{"supported": false}}, "reasoning": {{"supported": false}}}},
        "limit": {{"context": 100000, "output": 16000}},
        "api": {{"chat-completions": {{"base_url": "https://catalog.example/v1",
            "sdk": {{"opencodeai": "@opencode/ai/providers/openai-compatible"}}}}}}}}"#
    ));
    text(&spread(base, j(extra)))
}

#[test]
fn oc_catalog_lets_config_change_a_catalog_models_type_and_keeps_it_when_unset() {
    let data = custom_catalog(&format!(
        r#"{{"retyped": {}, "kept": {}}}"#,
        custom_model("retyped", "{}"),
        custom_model(
            "kept",
            r#"{"type": "decision", "api": {"x": {"base_url": "https://catalog.example/v1",
            "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible/decisions"}}}}"#
        )
    ));
    let c = with_catalog(
        json!({"providers": {"custom": {"models": {"retyped": {"type": "image"}, "kept": {"name": "Kept"}}}}}),
        data,
    );
    assert!(c.list_models("custom", &[]).is_empty());
    assert_eq!(c.list_models("custom", &["image"]), ["retyped"]);
    assert_eq!(c.list_models("custom", &["decision"]), ["kept"]);
}

#[test]
fn oc_catalog_preserves_catalog_capabilities_unless_config_overrides_them() {
    let data = custom_catalog(&format!(
        r#"{{"inherited": {}, "overridden": {}}}"#,
        custom_model("inherited", "{}"),
        custom_model("overridden", "{}")
    ));
    let c = with_catalog(
        json!({"providers": {"custom": {"models": {
            "inherited": {"name": "Inherited"},
            "overridden": {"capabilities": {"tools": true, "input": ["text", "image"], "output": ["text"]}}
        }}}}),
        data,
    );
    let caps = |id: &str| {
        let c = c.capabilities(&format!("custom/{id}")).unwrap();
        (c.tools, c.input, c.output)
    };
    assert_eq!(
        caps("inherited"),
        (false, vec!["text".into()], vec!["text".into()])
    );
    assert_eq!(
        caps("overridden"),
        (
            true,
            vec!["text".into(), "image".into()],
            vec!["text".into()]
        )
    );
    assert_eq!(
        c.resolve_model("custom/inherited").unwrap().input,
        Some(vec!["text".into()])
    );
}

#[test]
fn oc_catalog_merges_partial_config_capabilities_onto_the_base_models_existing_fields() {
    let data = custom_catalog(&format!(
        r#"{{"partial": {}}}"#,
        custom_model("partial", "{}")
    ));
    let c = with_catalog(
        json!({"providers": {"custom": {"models": {"partial": {"capabilities": {"input": ["text", "image"]}}}}}}),
        data,
    );
    let caps = c.capabilities("custom/partial").unwrap();
    assert_eq!(
        (caps.tools, caps.input, caps.output),
        (
            false,
            vec!["text".into(), "image".into()],
            vec!["text".into()]
        )
    );
    // Limits the config leaves out still come from the catalog.
    let m = c.resolve_model("custom/partial").unwrap();
    assert_eq!((m.context, m.max_output), (Some(100_000), Some(16_000)));
    assert_eq!(m.provider.base_url, "https://catalog.example/v1");
}

#[test]
fn oc_catalog_generates_variants_after_rewriting_a_configured_model_package() {
    // Adapted: a configured protocol stands for OpenCode's package.
    let c: Config = serde_json::from_value(json!({"providers": {"custom": {
        "protocol": "anthropic-messages",
        "settings": {"baseURL": "http://x"},
        "models": {"claude": {"id": "claude-opus-4-8"}}
    }}}))
    .unwrap();
    assert_eq!(
        c.resolve_model("custom/claude").unwrap().variants,
        ["low", "medium", "high", "xhigh", "max"]
    );
}

#[test]
fn oc_catalog_respells_inherited_variants_when_config_moves_a_catalog_model_to_another_package() {
    // A catalog Messages model moved to OpenAI Chat by config.
    let data = snapshot(j(
        r#"{"bedrock": {"id": "bedrock", "name": "Bedrock", "env": [], "models": {
        "claude-opus-5-5": {"id": "claude-opus-5-5", "type": "chat", "name": "Opus", "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": true, "effort": ["high"]}},
            "limit": {"context": 200000, "output": 64000},
            "api": {"messages": {"base_url": "https://catalog.example/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/anthropic"}}}}
    }}}"#,
    ));
    let model = &catalog::Catalog::new(&data)
        .model("bedrock", "claude-opus-5-5")
        .unwrap()
        .clone();
    assert_eq!(
        serde_json::to_value(&model.variants).unwrap(),
        json!([{"id": "high", "settings": {"thinking": {"type": "adaptive", "display": "summarized"}, "effort": "high"}}])
    );
    let c = with_catalog(
        json!({"providers": {"bedrock": {"protocol": "openai-chat", "settings": {"baseURL": "http://chat/v1"}}}}),
        data.clone(),
    );
    let m = c.resolve_model("bedrock/claude-opus-5-5#high").unwrap();
    assert_eq!(m.variants, ["low", "medium", "high"]);
    assert_eq!(m.options.reasoning_effort(), Some("high"));
    assert!(m.options.get("thinking").is_none());
    // Left on its catalog protocol, it keeps the catalog's variants.
    let c = with_catalog(json!({}), data);
    let m = c.resolve_model("bedrock/claude-opus-5-5#high").unwrap();
    assert_eq!(m.variants, ["high"]);
    assert_eq!(m.options.str("effort"), Some("high"));
}

#[test]
fn oc_catalog_keeps_a_moved_catalog_model_without_reasoning_variants_free_of_variants() {
    let data = snapshot(j(
        r#"{"bedrock": {"id": "bedrock", "name": "Bedrock", "env": [], "models": {
        "claude-plain": {"id": "claude-plain", "type": "chat", "name": "Plain", "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "api": {"messages": {"base_url": "https://catalog.example/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/anthropic"}}}}
    }}}"#,
    ));
    let c = with_catalog(
        json!({"providers": {"bedrock": {"protocol": "openai-chat", "settings": {"baseURL": "http://chat/v1"}}}}),
        data,
    );
    assert!(c
        .resolve_model("bedrock/claude-plain")
        .unwrap()
        .variants
        .is_empty());
}

#[test]
fn oc_catalog_builds_configured_reasoning_variants_after_provider_and_model_package_normalization()
{
    // Adapted: one provider per protocol, as CodeSmith's config sets the
    // protocol per provider.
    let c: Config = serde_json::from_value(json!({"providers": {
        "responses": {"protocol": "openai-responses", "settings": {"baseURL": "http://x"}, "models": {
            "effort": {
                "capabilities": {"reasoning": {"supported": true, "effort": ["default", "low", "high"]}},
                "variants": [{"id": "high", "headers": {"x-tier": "high"}}]
            },
            "fixed": {"capabilities": {"reasoning": {"supported": true}}},
            "plain": {"capabilities": {"reasoning": {"supported": false}}}
        }},
        "messages": {"protocol": "anthropic-messages", "settings": {"baseURL": "http://x"}, "models": {
            "claude-sonnet-4-5": {
                "capabilities": {"reasoning": {"supported": true, "budget": {"min": 1024}}},
                "limit": {"output": 64000}
            }
        }},
        "gemini": {"protocol": "google-gemini", "settings": {"baseURL": "http://x"}, "models": {
            "gemini-3.1-pro": {"capabilities": {"reasoning": {"supported": true, "effort": ["low", "high"]}}}
        }},
        "chat": {"protocol": "openai-chat", "settings": {"baseURL": "http://x"}, "models": {
            "magistral-medium": {"capabilities": {"reasoning": {"supported": true, "effort": ["low", "high"]}}}
        }}
    }}))
    .unwrap();
    let r = |spec: &str| c.resolve_model(spec).unwrap();
    let effort = r("responses/effort");
    assert_eq!(effort.variants, ["low", "high"]);
    let high = r("responses/effort#high");
    assert_eq!(high.options.reasoning_effort(), Some("high"));
    assert_eq!(high.options.str("reasoningSummary"), Some("auto"));
    assert_eq!(
        high.options.strings("include"),
        Some(vec!["reasoning.encrypted_content".into()])
    );
    assert!(high
        .provider
        .headers
        .contains(&("x-tier".to_string(), "high".to_string())));
    let claude = r("messages/claude-sonnet-4-5");
    assert_eq!(claude.variants, ["high", "max"]);
    assert_eq!(
        r("messages/claude-sonnet-4-5#high").options.get("thinking"),
        Some(&json!({"type": "enabled", "budgetTokens": 16000}))
    );
    assert_eq!(
        r("messages/claude-sonnet-4-5#max").options.get("thinking"),
        Some(&json!({"type": "enabled", "budgetTokens": 31999}))
    );
    assert_eq!(r("gemini/gemini-3.1-pro").variants, ["low", "high"]);
    assert_eq!(
        r("gemini/gemini-3.1-pro#low").options.get("thinkingConfig"),
        Some(&json!({"includeThoughts": true, "thinkingLevel": "low"}))
    );
    assert_eq!(r("chat/magistral-medium").variants, ["low", "high"]);
    assert_eq!(
        r("chat/magistral-medium#high").options.reasoning_effort(),
        Some("high")
    );
    assert!(r("responses/fixed").variants.is_empty());
    assert!(r("responses/plain").variants.is_empty());
}

#[test]
fn oc_catalog_applies_configured_release_date_temperature_support_and_reasoning_field() {
    // Adapted: CodeSmith keeps no release dates or freeform tool formats;
    // temperature support and the reasoning field are pinned.
    let data = custom_catalog(&format!(
        r#"{{"reasoner": {}}}"#,
        custom_model(
            "reasoner",
            r#"{"capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}, "temperature": true}}"#
        )
    ));
    let c = with_catalog(
        json!({"providers": {"custom": {"models": {"reasoner": {
            "capabilities": {"temperature": false},
            "compatibility": {"reasoningField": "reasoning_content"}
        }}}}}),
        data,
    );
    assert_eq!(
        c.capabilities("custom/reasoner").unwrap().temperature,
        Some(false)
    );
    assert_eq!(
        c.resolve_model("custom/reasoner")
            .unwrap()
            .provider
            .compat
            .reasoning_field
            .as_deref(),
        Some("reasoning_content")
    );
    assert_eq!(catalog::released("2026-02-03"), 1_770_076_800_000);
}

#[test]
fn oc_catalog_configured_provider_transport_overrides_builtin_defaults() {
    // `configured provider transport overrides ${builtin.id} defaults` for
    // the OpenAI and Azure rows (CodeSmith's xAI is Chat, and only Azure
    // itself defaults to WebSocket).
    for (id, extra) in [
        ("openai", json!({})),
        ("azure", json!({"resourceName": "res"})),
    ] {
        let mut settings = extra.clone();
        let default: Config =
            serde_json::from_value(json!({"providers": {id: {"settings": settings.clone()}}}))
                .unwrap();
        assert!(
            default
                .resolve_model(&format!("{id}/gpt-5.6-sol"))
                .unwrap()
                .websocket
                .is_some(),
            "{id}"
        );
        settings["transport"] = "http".into();
        let http: Config = serde_json::from_value(json!({"providers": {id: {
            "settings": settings,
            "models": {"override": {"id": "gpt-5.6-sol"}}
        }}}))
        .unwrap();
        for m in ["gpt-5.6-sol", "override"] {
            assert!(
                http.resolve_model(&format!("{id}/{m}"))
                    .unwrap()
                    .websocket
                    .is_none(),
                "{id}/{m}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The bundled snapshot through config resolution.

#[test]
fn oc_catalog_fills_unconfigured_limits_costs_and_modalities_from_the_catalog() {
    let data = catalog::bundled();
    let c = with_catalog(json!({}), data.to_vec());
    let catalog = c.models_dev.as_ref().unwrap().catalog();
    let id = c.list_models("anthropic", &[]).into_iter().next().unwrap();
    let cm = catalog.model("anthropic", &id).unwrap();
    let m = c.resolve_model(&format!("anthropic/{id}")).unwrap();
    assert_eq!(m.context, Some(cm.limit.context).filter(|c| *c > 0));
    assert_eq!(m.input, Some(cm.capabilities.input.clone()));
    assert!(c.cost(&format!("anthropic/{id}")).is_some());
    // Configured values win.
    let c = with_catalog(
        json!({"providers": {"anthropic": {"models": {id.clone(): {
            "limit": {"context": 1000, "output": 10},
            "cost": {"input": 9, "output": 9},
            "modalities": {"input": ["text"]}
        }}}}}),
        data.to_vec(),
    );
    let m = c.resolve_model(&format!("anthropic/{id}")).unwrap();
    assert_eq!((m.context, m.max_output), (Some(1000), Some(10)));
    assert_eq!(m.input, Some(vec!["text".into()]));
    assert_eq!(c.cost(&format!("anthropic/{id}")).unwrap().input, 9.0);
    // A catalog provider CodeSmith speaks becomes a provider.
    assert!(c.provider("moonshotai").is_some() || c.provider("deepinfra").is_some());
}

#[test]
fn a_configured_provider_keeps_its_own_protocol_over_the_catalogs() {
    // A catalog provider speaking Anthropic Messages.
    let data = snapshot(j(
        r#"{"msgs": {"id": "msgs", "name": "Msgs", "env": ["MSGS_KEY"], "models": {
        "m": {"id": "m", "type": "chat", "name": "M", "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "limit": {"context": 50000, "output": 5000},
            "api": {"messages": {"base_url": "https://msgs.example/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/anthropic"}}}}
    }}}"#,
    ));
    // Not configured: the catalog's protocol and URL.
    let c = with_catalog(json!({}), data.clone());
    let m = c.resolve_model("msgs/m").unwrap();
    assert_eq!(
        m.provider.protocol,
        codesmith_ai::Protocol::AnthropicMessages
    );
    assert_eq!(m.provider.base_url, "https://msgs.example/v1");
    // Configured without a protocol: OpenAI Chat as before, its own URL,
    // and the catalog's name, key variables and limits still fill in.
    let c = with_catalog(
        json!({"providers": {"msgs": {"settings": {"baseURL": "http://proxy/v1"}}}}),
        data.clone(),
    );
    let m = c.resolve_model("msgs/m").unwrap();
    assert_eq!(m.provider.protocol, codesmith_ai::Protocol::OpenaiChat);
    assert_eq!(m.provider.base_url, "http://proxy/v1");
    assert_eq!(m.context, Some(50_000));
    let p = c.provider("msgs").unwrap();
    assert_eq!(
        (p.name.as_deref(), p.env.clone()),
        (Some("Msgs"), vec!["MSGS_KEY".to_string()])
    );
    // Configured without a URL either: the catalog's Messages URL is not
    // borrowed for a Chat provider.
    let c = with_catalog(
        json!({"providers": {"msgs": {"name": "Mine"}}}),
        data.clone(),
    );
    assert!(c
        .resolve_model("msgs/m")
        .unwrap_err()
        .to_string()
        .contains("baseURL"));
    // Configured with the catalog's protocol: its URL fills in.
    let c = with_catalog(
        json!({"providers": {"msgs": {"protocol": "anthropic-messages"}}}),
        data,
    );
    assert_eq!(
        c.resolve_model("msgs/m").unwrap().provider.base_url,
        "https://msgs.example/v1"
    );
}
