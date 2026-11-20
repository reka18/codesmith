//! Decision (classifier) models through the stateless generate API,
//! ported from OpenCode's `generate.test.ts` (MIT).

mod media_harness;

use codesmith_ai::decision::{Answer, Part, Question};
use codesmith_ai::{Media, Source};
use codesmith_core::generate::{self, DecisionInput, Failure, GenerateError};
use media_harness::*;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const FAST: generate::Options = generate::Options {
    retry_scale: 0.001,
    poll: Duration::from_millis(5),
};

const DECISION_RESPONSE: &str = r#"{"model":"typesafe/jev-1.13-20260917","answers":{"department":{"type":"choice","choice":"billing","probabilities":{"billing":1,"sales":0,"technical":0},"confidence":1},"urgency":{"type":"score","score":1.02,"legend":{"0":"Can wait for normal support","1":"Needs prompt attention","2":"Actively blocking revenue"},"probabilities":{"0":0.01,"1":0.96,"2":0.03},"confidence":0.95},"refund":{"type":"noul","noul":0.99}},"usage":{"input_tokens":422,"output_tokens":69,"cost":0.000017724},"id":"gen-dec-1790088886-JRlffOLUwQdx4Lltu2se","provider":"TypeSafe"}"#;

/// Answers each decision path as OpenCode's fake host does; anything else
/// is a 400.
async fn host() -> Server {
    serve(Arc::new(|s: &Seen| match s.path.as_str() {
        "/local/v1/systemone" => json_reply(
            json!({"model": "openjev", "answers": {"photo": {"type": "noul", "noul": 0.7}}}),
        ),
        "/typesafe/v1/systemone" => json_reply(
            json!({"model": "clef", "answers": {"photo": {"type": "noul", "noul": 0.9}}}),
        ),
        "/gemini/v1beta/decisions" => json_reply(json!({
            "answers": {"photo": {"type": "noul", "noul": 0.8, "confidence": 0.8}},
            "usage": {"total_input_tokens": 12, "total_output_tokens": 0, "total_tokens": 12}
        })),
        "/openrouter/api/v1/systemone" => (
            200,
            "application/json",
            DECISION_RESPONSE.as_bytes().to_vec(),
        ),
        _ => (
            400,
            "application/json",
            json!({"error": {"message": "Bad request"}})
                .to_string()
                .into_bytes(),
        ),
    }))
    .await
}

/// A decision model (models.dev's source shape) on the `api` entry.
fn decision(id: &str, api: &str, package: &str, base: &str, media_input: Option<&str>) -> Value {
    let mut entry = json!({"base_url": base, "sdk": {"opencodeai": package}});
    if let Some(m) = media_input {
        entry["compatibility"] = json!({"media_input": m});
    }
    json!({
        "id": id, "type": "decision", "name": id, "release_date": "2026-01-01",
        "modalities": {"input": ["text", "image/png"], "output": ["text"]},
        "capabilities": {"tools": {"supported": false}, "reasoning": {"supported": false}, "temperature": false},
        "limit": {"context": 65536, "output": 0},
        "api": {api: entry}
    })
}

fn catalog(url: &str) -> Vec<(String, Value)> {
    let ts = "@opencode/ai/providers/typesafe-ai/decisions";
    let typesafe = format!("{url}/typesafe/v1");
    let mut gemini = decision(
        "decide-gemini",
        "gemini-decisions",
        "@opencode/ai/providers/google/decisions",
        &format!("{url}/gemini/v1beta"),
        None,
    );
    gemini["id"] = json!("decide-gemini");
    let mut flash = model(
        "gemini-3.1-flash-image",
        "image",
        "@opencode/ai/providers/google/images",
        &format!("{url}/google/v1beta"),
        &["text"],
        1,
    );
    flash["capabilities"]["reasoning"] = json!({"supported": true, "effort": ["high"]});
    vec![
        provider(
            "openrouter",
            "",
            vec![decision(
                "typesafe/jev-1.13",
                "systemone",
                "@opencode/ai/providers/openrouter/decisions",
                &format!("{url}/openrouter/api/v1"),
                None,
            )],
        ),
        // A local host: no base URL in the catalog, none of its own key.
        provider(
            "lmstudio",
            "",
            vec![decision(
                "openjev",
                "systemone",
                "@opencode/ai/providers/openai-compatible/decisions",
                "",
                None,
            )],
        ),
        provider(
            "decide-host",
            "",
            vec![
                decision("decide-default", "systemone", ts, &typesafe, None),
                decision("decide-root", "systemone", ts, &typesafe, Some("root")),
                decision("decide-state", "systemone", ts, &typesafe, Some("state")),
                decision(
                    "decide-unknown",
                    "systemone",
                    ts,
                    &typesafe,
                    Some("somewhere_new"),
                ),
                decision("decide-override", "systemone", ts, &typesafe, Some("state")),
                gemini,
            ],
        ),
        provider(
            "openai",
            "",
            vec![
                model(
                    "gpt-image-1-mini",
                    "image",
                    "@opencode/ai/providers/openai/images",
                    &format!("{url}/openai/v1"),
                    &["text"],
                    1,
                ),
                model(
                    "gpt-5",
                    "chat",
                    "@opencode/ai/providers/openai/responses",
                    &format!("{url}/openai/v1"),
                    &["text"],
                    1,
                ),
            ],
        ),
        provider("google", "", vec![flash]),
    ]
}

fn setup(url: &str) -> codesmith_core::config::Config {
    config(
        catalog(url),
        &["openrouter", "decide-host", "openai", "google"],
        json!({"providers": {
            "lmstudio": {"settings": {"baseURL": format!("{url}/local/v1"), "apiKey": ""}},
            // What a config `compatibility` override merges in.
            "decide-host": {"models": {"decide-override": {"compatibility": {"mediaInput": "root"}}}}
        }}),
    )
}

fn png() -> Part {
    Part::Media(Media {
        mime: "image/png".into(),
        source: Source::Base64("aW1hZ2U=".into()),
        filename: None,
        meta: Default::default(),
    })
}

fn photo() -> Vec<Question> {
    serde_json::from_value(
        json!([{"type": "predicate", "name": "photo", "instructions": "Is this a photo?"}]),
    )
    .unwrap()
}

fn ask(model: &str, input: Vec<Part>, questions: Vec<Question>) -> DecisionInput {
    DecisionInput {
        model: model.into(),
        input,
        questions,
    }
}

#[tokio::test]
async fn oc_generate_sends_keyless_local_decisions_without_auth_with_images_at_the_body_root() {
    let server = host().await;
    let config = setup(&server.url);
    let client = reqwest::Client::new();
    let data = codesmith_core::media::base64(&png_bytes());
    let input = vec![
        Part::Text("A cat".into()),
        Part::Media(Media {
            mime: "image/png".into(),
            source: Source::Base64(data.clone()),
            filename: None,
            meta: Default::default(),
        }),
    ];
    let result = generate::decisions(
        &config,
        &client,
        &ask("lmstudio/openjev", input, photo()),
        FAST,
    )
    .await
    .unwrap();
    let seen = &server.requests()[0];
    assert_eq!(seen.path, "/local/v1/systemone");
    assert_eq!(seen.header("authorization"), None);
    assert_eq!(
        seen.json(),
        json!({
            "model": "openjev",
            "state": "A cat",
            "images": [format!("data:image/png;base64,{data}")],
            "questions": {"photo": {"type": "noul", "instructions": "Is this a photo?"}}
        })
    );
    assert_eq!(
        serde_json::to_value(&result.answers).unwrap(),
        json!([{"type": "predicate", "name": "photo", "probability": 0.7}])
    );
}

#[tokio::test]
async fn oc_generate_answers_decision_questions_and_reports_the_providers_cost() {
    let server = host().await;
    let config = setup(&server.url);
    let client = reqwest::Client::new();
    let questions: Vec<Question> = serde_json::from_value(json!([
        {"type": "choice", "name": "department",
         "instructions": "Which team should handle this support request?",
         "choices": [
            {"value": "billing", "description": "Payments, invoices, refunds, or failed charges"},
            {"value": "technical", "description": "Bugs, outages, or integrations"},
            {"value": "sales", "description": "Pricing, upgrades, or new accounts"}]},
        {"type": "score", "name": "urgency", "instructions": "How urgent is this support request?",
         "levels": [{"label": "Can wait for normal support"}, {"label": "Needs prompt attention"},
                    {"label": "Actively blocking revenue"}]},
        {"type": "predicate", "name": "refund", "instructions": "Is the customer asking for a refund?",
         "criteria": {"true": "They want money back.", "false": "Anything else."}}
    ]))
    .unwrap();
    let text =
        "I was charged twice for the same invoice. Please refund the duplicate payment today.";
    let result = generate::decisions(
        &config,
        &client,
        &ask(
            "openrouter/typesafe/jev-1.13",
            vec![Part::Text(text.into())],
            questions,
        ),
        FAST,
    )
    .await
    .unwrap();
    let seen = &server.requests()[0];
    assert_eq!(seen.path, "/openrouter/api/v1/systemone");
    assert_eq!(seen.header("authorization"), Some("Bearer openrouter-key"));
    assert_eq!(
        seen.json()["questions"]["refund"],
        json!({"type": "noul", "instructions": "Is the customer asking for a refund?",
            "criteria": {"true": "They want money back.", "false": "Anything else."}})
    );
    let kinds: Vec<(String, String)> = serde_json::to_value(&result.answers)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["name"].as_str().unwrap().into(),
                a["type"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("department".to_string(), "choice".to_string()),
            ("urgency".into(), "score".into()),
            ("refund".into(), "predicate".into())
        ]
    );
    match &result.answers[0] {
        Answer::Choice {
            choice,
            probabilities,
            ..
        } => {
            assert_eq!(choice, "billing");
            // In the question's order, not the host's.
            let order: Vec<&Value> = probabilities.iter().map(|p| &p.value).collect();
            assert_eq!(
                order,
                [&json!("billing"), &json!("technical"), &json!("sales")]
            );
        }
        other => panic!("{other:?}"),
    }
    match &result.answers[1] {
        Answer::Score { probabilities, .. } => {
            assert_eq!(probabilities[1].label, "Needs prompt attention")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(result.cost, Some(0.000017724));
    assert_eq!(
        (result.input_tokens, result.output_tokens),
        (Some(422), Some(69))
    );
}

#[tokio::test]
async fn oc_generate_places_decision_media_where_the_catalogs_media_input_says() {
    let server = host().await;
    let config = setup(&server.url);
    let client = reqwest::Client::new();
    let send = |id: &'static str| {
        let (config, client) = (&config, &client);
        let server = &server;
        async move {
            let before = server.requests().len();
            generate::decisions(
                config,
                client,
                &ask(
                    &format!("decide-host/{id}"),
                    vec![Part::Text("Is this a photo?".into()), png()],
                    photo(),
                ),
                FAST,
            )
            .await
            .unwrap();
            server.requests()[before].clone()
        }
    };
    let root = send("decide-root").await.json();
    assert_eq!(root["state"], "Is this a photo?");
    assert_eq!(root["images"], json!(["data:image/png;base64,aW1hZ2U="]));
    let state = send("decide-state").await.json();
    assert_eq!(
        state["state"],
        json!(["Is this a photo?", {"type": "image_url", "image_url": {"url": "data:image/png;base64,aW1hZ2U="}}])
    );
    // Unset or unknown values keep TypeSafe's assets map.
    for id in ["decide-default", "decide-unknown"] {
        let seen = send(id).await;
        let body = seen.json();
        assert_eq!(
            body["state"],
            json!(["Is this a photo?", "<image-1>"]),
            "{id}"
        );
        assert_eq!(
            body["assets"],
            json!({"<image-1>": {"type": "image", "format": "png", "data_base64": "aW1hZ2U="}})
        );
        // TypeSafe takes images only with its opt-in header.
        assert_eq!(seen.header("typesafe-danger-zone-images"), Some("true"));
    }
    let over = send("decide-override").await.json();
    assert_eq!(over["images"], json!(["data:image/png;base64,aW1hZ2U="]));
}

#[tokio::test]
async fn oc_generate_routes_a_gemini_decisions_catalog_entry_to_the_google_decisions_entrypoint() {
    let server = host().await;
    let config = setup(&server.url);
    let client = reqwest::Client::new();
    let result = generate::decisions(
        &config,
        &client,
        &ask(
            "decide-host/decide-gemini",
            vec![Part::Text("Is this a photo?".into()), png()],
            photo(),
        ),
        FAST,
    )
    .await
    .unwrap();
    let seen = &server.requests()[0];
    assert_eq!(seen.path, "/gemini/v1beta/decisions");
    assert_eq!(
        seen.json(),
        json!({
            "model": "decide-gemini",
            "input": [
                {"type": "text", "text": "Is this a photo?"},
                {"type": "image", "mime_type": "image/png", "data": "aW1hZ2U="}
            ],
            "questions": {"photo": {"type": "noul", "instructions": "Is this a photo?"}}
        })
    );
    assert_eq!(
        serde_json::to_value(&result.answers).unwrap(),
        json!([{"type": "predicate", "name": "photo", "probability": 0.8}])
    );
    assert_eq!(
        (result.input_tokens, result.output_tokens),
        (Some(12), Some(0))
    );
}

#[tokio::test]
async fn oc_generate_sends_a_variants_reasoning_setting_and_maps_provider_rejections() {
    let server = host().await;
    let config = setup(&server.url);
    let client = reqwest::Client::new();
    let input = generate::ImageInput {
        model: "google/gemini-3.1-flash-image#high".into(),
        prompt: "A black circle".into(),
        ..Default::default()
    };
    let err = generate::image(&config, &client, &input, FAST)
        .await
        .unwrap_err();
    assert!(
        String::from_utf8_lossy(&server.requests()[0].body).contains("thinkingLevel"),
        "{:?}",
        server.requests()[0].json()
    );
    match err {
        GenerateError::ProviderRequest { reason, status, .. } => {
            assert_eq!(reason, Failure::InvalidRequest);
            assert_eq!(status, Some(400));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn oc_generate_returns_a_typed_error_for_an_invalid_request_instead_of_crashing() {
    let server = host().await;
    let config = setup(&server.url);
    let client = reqwest::Client::new();
    let mut twice = photo();
    twice.extend(photo());
    let err = generate::decisions(
        &config,
        &client,
        &ask("openrouter/typesafe/jev-1.13", vec![], twice),
        FAST,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, GenerateError::InvalidRequest { .. }),
        "{err:?}"
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn oc_generate_rejects_models_of_another_type_and_unknown_models() {
    let server = host().await;
    let config = setup(&server.url);
    let client = reqwest::Client::new();
    let image = |m: &str| generate::ImageInput {
        model: m.into(),
        prompt: "x".into(),
        ..Default::default()
    };
    assert!(matches!(
        generate::image(&config, &client, &image("openai/gpt-5"), FAST).await,
        Err(GenerateError::UnsupportedModel { .. })
    ));
    assert!(matches!(
        generate::decisions(
            &config,
            &client,
            &ask("openai/gpt-image-1-mini", vec![], photo()),
            FAST
        )
        .await,
        Err(GenerateError::UnsupportedModel { .. })
    ));
    assert!(matches!(
        generate::image(&config, &client, &image("openai/nope"), FAST).await,
        Err(GenerateError::ModelNotFound { .. })
    ));
    assert!(matches!(
        generate::text(&config, &client, Some("openai/gpt-image-1-mini"), "x", FAST).await,
        Err(GenerateError::UnsupportedModel { .. })
    ));
    assert!(matches!(
        generate::text(&config, &client, Some("openai/nope"), "x", FAST).await,
        Err(GenerateError::ModelNotFound { .. })
    ));
    assert!(server.requests().is_empty());
}
