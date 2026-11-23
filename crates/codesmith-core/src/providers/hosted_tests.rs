use super::*;
use serde_json::json;

fn ids(raw: &[Value]) -> Vec<&str> {
    raw.iter().filter_map(|m| m["id"].as_str()).collect()
}

fn entry<'a>(raw: &'a [Value], id: &str) -> &'a Value {
    raw.iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {raw:?}"))
}

// ------------------------------------------------------------ each API

#[test]
fn anthropic_models_keep_names_and_release_times() {
    let v = json!({
        "data": [
            {"type": "model", "id": "claude-opus-4-5-20251101", "display_name": "Claude Opus 4.5",
             "created_at": "2025-11-24T00:00:00Z"},
            {"type": "model", "id": "claude-haiku-4-5-20251001", "display_name": "Claude Haiku 4.5",
             "created_at": "2025-10-15T00:00:00Z"}
        ],
        "has_more": false, "first_id": "claude-opus-4-5-20251101", "last_id": "claude-haiku-4-5-20251001"
    });
    let raw = normalize(&v);
    assert_eq!(
        ids(&raw),
        ["claude-opus-4-5-20251101", "claude-haiku-4-5-20251001"]
    );
    let opus = entry(&raw, "claude-opus-4-5-20251101");
    assert_eq!(
        opus["kind"], "chat",
        "Anthropic's type \"model\" is a chat model"
    );
    assert_eq!(opus["name"], "Claude Opus 4.5");
    assert_eq!(opus["released"], 1_763_942_400_000i64);
}

#[test]
fn gemini_models_lose_their_prefix_and_keep_limits() {
    let v = json!({"models": [
        {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro",
         "inputTokenLimit": 1048576, "outputTokenLimit": 65536,
         "supportedGenerationMethods": ["generateContent", "countTokens"]},
        {"name": "models/text-embedding-004", "displayName": "Text Embedding 004",
         "supportedGenerationMethods": ["embedContent"]}
    ]});
    let raw = normalize(&v);
    assert_eq!(ids(&raw), ["gemini-2.5-pro", "text-embedding-004"]);
    let pro = entry(&raw, "gemini-2.5-pro");
    assert_eq!(pro["kind"], "chat");
    assert_eq!(pro["context"], 1_048_576);
    assert_eq!(pro["output"], 65_536);
    assert_eq!(pro["name"], "Gemini 2.5 Pro");
    assert_eq!(entry(&raw, "text-embedding-004")["kind"], "other");
    assert_eq!(chat_ids(&raw), ["gemini-2.5-pro"]);
}

#[test]
fn openai_lists_only_its_chat_models_as_chat() {
    let v = json!({"object": "list", "data": [
        {"id": "gpt-5", "object": "model", "created": 1754524800, "owned_by": "system"},
        {"id": "text-embedding-3-large", "object": "model", "created": 1705953180},
        {"id": "whisper-1", "object": "model", "created": 1677532384},
        {"id": "tts-1-hd", "object": "model", "created": 1699046015},
        {"id": "dall-e-3", "object": "model", "created": 1698785189},
        {"id": "omni-moderation-latest", "object": "model", "created": 1731689265},
        {"id": "gpt-4o-realtime-preview", "object": "model", "created": 1727659998},
        {"id": "gpt-image-1", "object": "model", "created": 1745517030},
        {"id": "davinci-002", "object": "model", "created": 1692634301}
    ]});
    let raw = normalize(&v);
    assert_eq!(chat_ids(&raw), ["gpt-5"]);
    assert_eq!(entry(&raw, "gpt-5")["released"], 1_754_524_800_000i64);
}

#[test]
fn openrouter_keeps_context_names_and_skips_image_makers() {
    let v = json!({"data": [
        {"id": "anthropic/claude-opus-4.5", "name": "Anthropic: Claude Opus 4.5",
         "created": 1763942400, "context_length": 200000,
         "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
         "top_provider": {"context_length": 200000, "max_completion_tokens": 64000}},
        {"id": "google/gemini-2.5-flash-image", "name": "Gemini Image",
         "architecture": {"output_modalities": ["image", "text"]}},
        {"id": "black-forest-labs/flux", "name": "Flux",
         "architecture": {"output_modalities": ["image"]}}
    ]});
    let raw = normalize(&v);
    let opus = entry(&raw, "anthropic/claude-opus-4.5");
    assert_eq!(opus["kind"], "chat");
    assert_eq!(opus["context"], 200_000);
    assert_eq!(opus["output"], 64_000, "from top_provider");
    assert_eq!(opus["name"], "Anthropic: Claude Opus 4.5");
    assert_eq!(entry(&raw, "black-forest-labs/flux")["kind"], "other");
    // Makes text too, but its id says it is for images.
    assert_eq!(
        entry(&raw, "google/gemini-2.5-flash-image")["kind"],
        "other"
    );
}

#[test]
fn together_types_decide_what_is_chat() {
    let v = json!([
        {"id": "meta-llama/Llama-3.3-70B-Instruct-Turbo", "type": "chat", "display_name": "Llama 3.3 70B",
         "context_length": 131072, "created": 1733443200},
        {"id": "Qwen/Qwen2.5-Coder-32B-Instruct", "type": "code", "context_length": 32768},
        {"id": "BAAI/bge-large-en-v1.5", "type": "embedding"},
        {"id": "stabilityai/stable-diffusion-xl", "type": "image"},
        {"id": "Salesforce/Llama-Rank-V1", "type": "rerank"}
    ]);
    let raw = normalize(&v);
    assert_eq!(
        chat_ids(&raw),
        [
            "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            "Qwen/Qwen2.5-Coder-32B-Instruct"
        ]
    );
    assert_eq!(
        entry(&raw, "meta-llama/Llama-3.3-70B-Instruct-Turbo")["context"],
        131_072
    );
}

#[test]
fn mistral_capabilities_decide_what_is_chat() {
    let v = json!({"object": "list", "data": [
        {"id": "mistral-large-latest", "max_context_length": 131072, "created": 1731024000,
         "capabilities": {"completion_chat": true, "function_calling": true}},
        {"id": "mistral-embed", "capabilities": {"completion_chat": false}},
        {"id": "codestral-2501", "capabilities": {"completion_chat": true, "completion_fim": true}}
    ]});
    let raw = normalize(&v);
    assert_eq!(chat_ids(&raw), ["mistral-large-latest", "codestral-2501"]);
    assert_eq!(entry(&raw, "mistral-large-latest")["context"], 131_072);
}

#[test]
fn groq_skips_speech_and_guard_models_and_keeps_context_windows() {
    let v = json!({"object": "list", "data": [
        {"id": "llama-3.3-70b-versatile", "context_window": 131072, "created": 1733447754},
        {"id": "whisper-large-v3", "context_window": 448},
        {"id": "playai-tts", "context_window": 8192},
        {"id": "meta-llama/llama-guard-4-12b", "context_window": 131072},
        {"id": "openai/gpt-oss-120b", "context_window": 131072, "max_completion_tokens": 65536}
    ]});
    let raw = normalize(&v);
    assert_eq!(
        chat_ids(&raw),
        ["llama-3.3-70b-versatile", "openai/gpt-oss-120b"]
    );
    assert_eq!(entry(&raw, "openai/gpt-oss-120b")["output"], 65_536);
}

#[test]
fn plain_openai_compatible_lists_work() {
    // DeepSeek, xAI, Cerebras, Fireworks: ids and little else.
    for v in [
        json!({"object": "list", "data": [{"id": "deepseek-chat", "object": "model", "owned_by": "deepseek"},
                                          {"id": "deepseek-reasoner", "object": "model"}]}),
        json!({"data": [{"id": "deepseek-chat", "created": 1700000000}, {"id": "deepseek-reasoner"}]}),
    ] {
        assert_eq!(
            chat_ids(&normalize(&v)),
            ["deepseek-chat", "deepseek-reasoner"]
        );
    }
}

// ------------------------------------------------------------ edges

#[test]
fn bad_ids_are_skipped() {
    let v = json!({"data": [
        {"id": ""}, {"id": "   "}, {"id": "has space"}, {"id": "tab\there"}, {"id": "nl\nx"},
        {"id": "/leading"}, {"id": "trailing/"}, {"id": "x".repeat(201)}, {"id": 42},
        {"no_id": true}, "a string", 7, null, [],
        {"id": " ok-model "}
    ]});
    assert_eq!(
        ids(&normalize(&v)),
        ["ok-model"],
        "only the usable id, trimmed"
    );
}

#[test]
fn duplicate_ids_keep_the_first() {
    let v = json!({"data": [
        {"id": "m", "context_length": 1000}, {"id": "m", "context_length": 2000}
    ]});
    let raw = normalize(&v);
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0]["context"], 1000);
}

#[test]
fn implausible_numbers_and_dates_are_dropped() {
    let v = json!({"data": [
        {"id": "a", "context_length": 0, "max_tokens": -5, "created": 12},
        {"id": "b", "context_length": 1_000_000_000_000u64, "created": 99_999_999_999_999i64},
        {"id": "c", "context_length": "131072", "created_at": "not a date"},
        {"id": "d", "context_length": 3.5, "created_at": "1970-01-01T00:00:00Z"}
    ]});
    for m in normalize(&v) {
        assert!(m.get("context").is_none(), "{m}");
        assert!(m.get("output").is_none(), "{m}");
        assert!(m.get("released").is_none(), "{m}");
    }
}

#[test]
fn created_in_milliseconds_is_not_multiplied_again() {
    let v = json!({"data": [{"id": "m", "created": 1_754_524_800_000i64}]});
    assert_eq!(normalize(&v)[0]["released"], 1_754_524_800_000i64);
}

#[test]
fn names_equal_to_the_id_or_full_of_control_characters_are_not_names() {
    let v = json!({"data": [
        {"id": "m1", "name": "m1"},
        {"id": "m2", "name": "\u{1b}[31m\u{7}"},
        {"id": "m3", "name": "Nice\u{1b} Name"},
        {"id": "m4", "name": "x".repeat(121)}
    ]});
    let raw = normalize(&v);
    assert!(entry(&raw, "m1").get("name").is_none());
    assert!(
        entry(&raw, "m2")
            .get("name")
            .is_some_and(|n| !n.as_str().unwrap().contains('\u{1b}'))
            || entry(&raw, "m2").get("name").is_none()
    );
    assert_eq!(entry(&raw, "m3")["name"], "Nice Name");
    assert!(entry(&raw, "m4").get("name").is_none());
}

#[test]
fn answers_that_are_not_lists_give_nothing() {
    for v in [
        json!(null),
        json!({}),
        json!({"data": {}}),
        json!("models"),
        json!(3),
        json!({"error": {"message": "bad key"}}),
    ] {
        assert!(normalize(&v).is_empty(), "{v}");
    }
}

// ------------------------------------------------------------ derive

fn template(id: &str, name: &str, context: u64) -> crate::catalog::Model {
    let mut m = crate::providers::live::blank("anthropic", id);
    m.name = name.into();
    m.limit.context = context;
    m.time.released = 1;
    m
}

#[test]
fn derive_uses_the_catalog_where_it_knows_the_model() {
    let raw = normalize(&json!({"data": [
        {"id": "known", "display_name": "API Name", "context_length": 5},
        {"id": "brand-new", "display_name": "Brand New", "context_length": 400000,
         "max_tokens": 128000, "created": 1790000000}
    ]}));
    let models = derive(
        "anthropic",
        &raw,
        &[template("known", "Catalog Name", 200_000)],
    );
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].name, "Catalog Name", "the catalog's details win");
    assert_eq!(models[0].limit.context, 200_000);
    let new = &models[1];
    assert_eq!(new.id, "brand-new");
    assert_eq!(new.name, "Brand New");
    assert_eq!(new.limit.context, 400_000);
    assert_eq!(new.limit.output, 128_000);
    assert_eq!(new.time.released, 1_790_000_000_000);
    assert_eq!(new.kind, "chat");
    assert!(new.enabled);
}

#[test]
fn derive_never_lets_output_exceed_context() {
    let raw = normalize(&json!({"data": [
        {"id": "small", "context_length": 4096},
        {"id": "odd", "context_length": 8000, "max_tokens": 64000}
    ]}));
    for m in derive("p", &raw, &[]) {
        assert!(
            m.limit.output <= m.limit.context,
            "{}: {} > {}",
            m.id,
            m.limit.output,
            m.limit.context
        );
    }
}

#[test]
fn derive_keeps_non_chat_kinds_so_chat_lists_skip_them() {
    let raw = normalize(&json!({"data": [{"id": "text-embedding-3-small"}]}));
    let models = derive(
        "openai",
        &raw,
        &[template("text-embedding-3-small", "Embed", 8191)],
    );
    assert_eq!(models[0].kind, "other");
}

// ------------------------------------------------------------ freshness

#[test]
fn account_is_a_fingerprint_never_the_key() {
    let a = account(Some("sk-ant-secret-key"));
    assert!(a.starts_with("key:"));
    assert!(!a.contains("secret"));
    assert_ne!(a, account(Some("sk-ant-other-key")));
    assert_eq!(account(None), "key:none");
    assert_eq!(account(Some("")), "key:none");
}

#[test]
fn stale_when_missing_for_another_account_old_or_from_the_future() {
    let acct = account(Some("k"));
    let now = 10 * MAX_AGE_MS;
    let kept = |a: &str, fetched: u64| json!({"account": a, "fetched": fetched, "models": []});
    assert!(stale(None, &acct, now));
    assert!(
        stale(Some(&kept("key:other", now)), &acct, now),
        "another key's list"
    );
    assert!(!stale(Some(&kept(&acct, now - 1000)), &acct, now), "fresh");
    assert!(
        !stale(Some(&kept(&acct, now - MAX_AGE_MS + 1)), &acct, now),
        "just under a day"
    );
    assert!(
        stale(Some(&kept(&acct, now - MAX_AGE_MS)), &acct, now),
        "a day old"
    );
    assert!(
        stale(Some(&kept(&acct, now + 10 * 60_000)), &acct, now),
        "the clock moved back"
    );
    assert!(
        !stale(Some(&kept(&acct, now + 30_000)), &acct, now),
        "small skew is fine"
    );
    assert!(
        stale(Some(&json!({"account": acct})), &acct, now),
        "no fetch time"
    );
}

#[test]
fn hosted_names_the_providers_with_an_api_list() {
    for p in PROVIDERS {
        assert!(hosted(p));
    }
    for p in [
        "ollama",
        "lmstudio",
        "claude-agent",
        "azure",
        "copilot",
        "",
        "Anthropic",
    ] {
        assert!(!hosted(p), "{p}");
    }
}

// ------------------------------------------------------------ the store

#[test]
fn a_kept_hosted_list_reads_back_for_its_account_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live-models.json");
    let raw = normalize(&json!({"data": [{"id": "fresh-model", "context_length": 64000}]}));
    let acct = account(Some("key-one"));
    crate::providers::live::save_in(
        &path,
        "anthropic",
        &acct,
        "https://api.anthropic.com/v1",
        raw,
    )
    .unwrap();
    let kept = crate::providers::live::load_in(&path, "anthropic").unwrap();
    assert!(!stale(
        Some(&kept),
        &acct,
        crate::providers::oauth::now_ms()
    ));
    let models = crate::providers::live::derive("anthropic", &kept, &[]);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].limit.context, 64_000);
    // The file never holds the key.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("key-one"));
}

// ------------------------------------------------------------ property / stress

/// A small deterministic generator: no extra crates, reproducible runs.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn value(&mut self, depth: u32) -> Value {
        match self.next() % if depth > 3 { 5 } else { 8 } {
            0 => Value::Null,
            1 => json!(self.next().is_multiple_of(2)),
            2 => json!(self.next() as i64),
            3 => json!((self.next() % 1000) as f64 / 7.0),
            4 => {
                let pool = [
                    "id",
                    "name",
                    "models/x",
                    "gpt-5",
                    "",
                    " ",
                    "\u{0}",
                    "émbed",
                    "created_at",
                    "2025-01-01T00:00:00Z",
                ];
                json!(pool[(self.next() % pool.len() as u64) as usize])
            }
            5 => Value::Array(
                (0..self.next() % 4)
                    .map(|_| self.value(depth + 1))
                    .collect(),
            ),
            _ => {
                let keys = [
                    "id",
                    "name",
                    "data",
                    "models",
                    "type",
                    "created",
                    "created_at",
                    "context_length",
                    "max_tokens",
                    "supportedGenerationMethods",
                    "capabilities",
                    "architecture",
                    "top_provider",
                    "displayName",
                ];
                let mut m = Map::new();
                for _ in 0..self.next() % 6 {
                    let k = keys[(self.next() % keys.len() as u64) as usize];
                    m.insert(k.into(), self.value(depth + 1));
                }
                Value::Object(m)
            }
        }
    }
}

#[test]
fn any_answer_normalizes_without_panicking_into_well_formed_entries() {
    let mut rng = Rng(0x5eed_cafe_f00d_d00d);
    for _ in 0..3000 {
        let v = rng.value(0);
        let raw = normalize(&v);
        let mut seen = std::collections::HashSet::new();
        for e in &raw {
            let id = e["id"].as_str().expect("every entry has an id");
            assert!(
                !id.is_empty() && !id.chars().any(char::is_whitespace),
                "{id:?}"
            );
            assert!(seen.insert(id.to_string()), "duplicate {id}");
            assert!(matches!(e["kind"].as_str(), Some("chat" | "other")));
        }
        // And whatever comes out derives cleanly.
        for m in derive("p", &raw, &[]) {
            assert!(m.limit.output <= m.limit.context);
        }
    }
}

#[test]
fn a_huge_answer_is_capped_and_fast() {
    let data: Vec<Value> = (0..20_000)
        .map(|i| json!({"id": format!("model-{i}"), "context_length": 8192}))
        .collect();
    let started = std::time::Instant::now();
    let raw = normalize(&json!({ "data": data }));
    assert_eq!(raw.len(), MAX_MODELS);
    let models = derive("p", &raw, &[]);
    assert_eq!(models.len(), MAX_MODELS);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

// ------------------------------------------------------------ reasoning

/// An Anthropic model as `GET /v1/models` lists it, with the effort
/// levels named in `levels` supported.
fn anthropic_model(id: &str, levels: &[&str], thinking: bool) -> Value {
    let mut effort = serde_json::Map::new();
    for l in ["low", "medium", "high", "xhigh", "max"] {
        effort.insert(l.into(), json!({"supported": levels.contains(&l)}));
    }
    effort.insert("supported".into(), json!(!levels.is_empty()));
    json!({
        "type": "model", "id": id, "display_name": id,
        "max_input_tokens": 1_000_000, "max_tokens": 128_000,
        "capabilities": {
            "batch": {"supported": true},
            "effort": effort,
            "thinking": {"supported": thinking, "types": {
                "adaptive": {"supported": thinking}, "enabled": {"supported": thinking}}}
        }
    })
}

fn obj(v: &Value) -> &Map<String, Value> {
    v.as_object().unwrap()
}

fn variant_ids(m: &crate::catalog::Model) -> Vec<&str> {
    m.variants.iter().map(|v| v.id.as_str()).collect()
}

#[test]
fn anthropic_effort_levels_are_the_supported_ones_lowest_first() {
    let r = reasoning(obj(&anthropic_model("c", &["max", "low", "high"], true))).unwrap();
    assert!(r.supported);
    assert_eq!(r.efforts, ["low", "high", "max"]);
}

#[test]
fn anthropic_thinking_without_effort_reasons_with_no_levels() {
    let r = reasoning(obj(&anthropic_model("c", &[], true))).unwrap();
    assert_eq!(
        r,
        ApiReasoning {
            supported: true,
            efforts: vec![]
        }
    );
    let r = reasoning(obj(&anthropic_model("c", &[], false))).unwrap();
    assert!(!r.supported, "no thinking and no effort: does not reason");
}

#[test]
fn anthropic_levels_only_count_when_effort_is_supported() {
    let mut m = anthropic_model("c", &["low", "high"], true);
    m["capabilities"]["effort"]["supported"] = json!(false);
    assert!(reasoning(obj(&m)).unwrap().efforts.is_empty());
}

#[test]
fn openrouter_efforts_are_reordered_lowest_first() {
    let m = json!({"id": "x/y", "reasoning": {"supported_efforts": ["high", "medium", "low", "minimal"],
                   "mandatory": false}, "supported_parameters": ["reasoning"]});
    assert_eq!(
        reasoning(obj(&m)).unwrap().efforts,
        ["minimal", "low", "medium", "high"]
    );
}

#[test]
fn openrouter_mandatory_reasoning_drops_none() {
    let m = json!({"id": "x/y", "reasoning": {"supported_efforts": ["high", "low", "none"], "mandatory": true}});
    assert_eq!(reasoning(obj(&m)).unwrap().efforts, ["low", "high"]);
    let m = json!({"id": "x/y", "reasoning": {"supported_efforts": ["high", "low", "none"], "mandatory": false}});
    assert_eq!(reasoning(obj(&m)).unwrap().efforts, ["none", "low", "high"]);
}

#[test]
fn openrouter_without_an_allowlist_reasons_with_no_levels() {
    let m = json!({"id": "x/y", "reasoning": {"supported_efforts": null, "mandatory": false}});
    assert_eq!(
        reasoning(obj(&m)).unwrap(),
        ApiReasoning {
            supported: true,
            efforts: vec![]
        }
    );
}

#[test]
fn openrouter_parameters_say_whether_a_model_reasons() {
    let m = json!({"id": "x/y", "supported_parameters": ["temperature", "tools"]});
    assert!(!reasoning(obj(&m)).unwrap().supported);
    let m = json!({"id": "x/y", "supported_parameters": ["include_reasoning"]});
    assert!(reasoning(obj(&m)).unwrap().supported);
}

#[test]
fn gemini_thinking_says_whether_a_model_reasons() {
    let m = json!({"name": "models/gemini-9", "supportedGenerationMethods": ["generateContent"], "thinking": true});
    assert!(reasoning(obj(&m)).unwrap().supported);
    let m = json!({"name": "models/gemini-9", "supportedGenerationMethods": ["generateContent"], "thinking": false});
    assert!(!reasoning(obj(&m)).unwrap().supported);
    let m = json!({"name": "models/gemini-9", "supportedGenerationMethods": ["generateContent"]});
    assert_eq!(reasoning(obj(&m)), None);
}

#[test]
fn apis_that_say_nothing_about_reasoning_give_none() {
    for m in [
        json!({"id": "gpt-9", "object": "model", "created": 1, "owned_by": "openai"}),
        json!({"id": "mistral-9", "capabilities": {"completion_chat": true, "vision": false}}),
        json!({"id": "llama", "context_window": 8192}),
    ] {
        assert_eq!(reasoning(obj(&m)), None, "{m}");
    }
}

#[test]
fn odd_level_names_are_dropped_and_the_list_is_capped() {
    let mut levels: Vec<Value> = [
        "HIGH",
        "",
        "default",
        "a b",
        "low\u{7}",
        &"x".repeat(40),
        "high",
        "high",
    ]
    .iter()
    .map(|s| json!(s))
    .collect();
    levels.extend((0..100).map(|i| json!(format!("level-{i}"))));
    levels.push(json!(5));
    let m = json!({"id": "x/y", "reasoning": {"supported_efforts": levels}});
    let got = reasoning(obj(&m)).unwrap().efforts;
    assert_eq!(got[0], "high");
    assert_eq!(got.len(), MAX_EFFORTS);
    assert!(got.iter().all(|l| effort_name(l)), "{got:?}");
}

#[test]
fn normalize_keeps_reasoning_and_levels() {
    let raw = normalize(&json!({"data": [
        anthropic_model("claude-a", &["low", "medium", "high"], true),
        anthropic_model("claude-b", &[], false),
        {"id": "claude-c"}
    ]}));
    assert_eq!(entry(&raw, "claude-a")["reasoning"], true);
    assert_eq!(
        entry(&raw, "claude-a")["efforts"],
        json!(["low", "medium", "high"])
    );
    assert_eq!(entry(&raw, "claude-b")["reasoning"], false);
    assert!(entry(&raw, "claude-b").get("efforts").is_none());
    assert!(entry(&raw, "claude-c").get("reasoning").is_none());
    // Anthropic's limits are kept too.
    assert_eq!(entry(&raw, "claude-a")["context"], 1_000_000);
    assert_eq!(entry(&raw, "claude-a")["output"], 128_000);
}

#[test]
fn derive_spells_the_apis_levels_for_a_model_the_catalog_lacks() {
    let raw = normalize(
        &json!({"data": [anthropic_model("claude-future-9", &["low", "high", "xhigh"], true)]}),
    );
    let m = &derive("anthropic", &raw, &[])[0];
    assert_eq!(variant_ids(m), ["low", "high", "xhigh"]);
    let high = m.variants.iter().find(|v| v.id == "high").unwrap();
    let settings = high.settings.as_ref().unwrap();
    assert_eq!(settings["effort"], "high");
    assert_eq!(
        m.reasoning.effort.as_deref(),
        Some(&["low".to_string(), "high".into(), "xhigh".into()][..])
    );
}

#[test]
fn derive_puts_the_apis_levels_over_the_catalogs() {
    let mut t = template("claude-x", "Claude X", 200_000);
    t.variants = ["low", "medium", "high", "max"]
        .iter()
        .map(|id| crate::catalog::Variant {
            id: id.to_string(),
            settings: None,
            body: None,
            headers: None,
        })
        .collect();
    let raw = normalize(&json!({"data": [anthropic_model("claude-x", &["medium", "high"], true)]}));
    let m = &derive("anthropic", &raw, &[t.clone()])[0];
    assert_eq!(variant_ids(m), ["medium", "high"]);
    // A model the API says does not reason loses the catalog's levels.
    let raw = normalize(&json!({"data": [anthropic_model("claude-x", &[], false)]}));
    assert!(derive("anthropic", &raw, &[t.clone()])[0]
        .variants
        .is_empty());
    // Reasoning without named levels keeps the catalog's.
    let raw = normalize(&json!({"data": [anthropic_model("claude-x", &[], true)]}));
    assert_eq!(
        variant_ids(&derive("anthropic", &raw, &[t.clone()])[0]),
        ["low", "medium", "high", "max"]
    );
    // Saying nothing keeps them too.
    let raw = normalize(&json!({"data": [{"id": "claude-x"}]}));
    assert_eq!(
        variant_ids(&derive("anthropic", &raw, &[t])[0]),
        ["low", "medium", "high", "max"]
    );
}

#[test]
fn derive_gives_openrouter_models_the_listed_levels_as_reasoning_objects() {
    let raw = normalize(&json!({"data": [
        {"id": "z/think", "reasoning": {"supported_efforts": ["high", "low"]}, "supported_parameters": ["reasoning"]},
        {"id": "z/plain", "supported_parameters": ["tools"]}
    ]}));
    let models = derive("openrouter", &raw, &[]);
    let think = models.iter().find(|m| m.id == "z/think").unwrap();
    assert_eq!(variant_ids(think), ["low", "high"]);
    let low = &think.variants[0];
    assert_eq!(low.settings.as_ref().unwrap()["reasoning"]["effort"], "low");
    let plain = models.iter().find(|m| m.id == "z/plain").unwrap();
    assert!(plain.variants.is_empty());
}

#[test]
fn a_reasoning_model_the_catalog_lacks_gets_the_protocols_default_levels() {
    let raw = normalize(&json!({"models": [
        {"name": "models/gemini-9-pro", "supportedGenerationMethods": ["generateContent"], "thinking": true},
        {"name": "models/gemini-9-lite", "supportedGenerationMethods": ["generateContent"], "thinking": false}
    ]}));
    let models = derive("google", &raw, &[]);
    assert_eq!(variant_ids(&models[0]), ["low", "medium", "high"]);
    assert!(models[1].variants.is_empty());
}

#[test]
fn providers_whose_apis_say_nothing_are_unchanged() {
    // Groq's list has no reasoning fields: no levels are invented.
    let raw = normalize(&json!({"data": [{"id": "llama-9", "context_window": 131072}]}));
    assert!(derive("groq", &raw, &[])[0].variants.is_empty());
    // A reasoning flag from a provider without known controls changes only
    // a model that does not reason.
    let raw = vec![json!({"id": "m", "kind": "chat", "reasoning": true, "efforts": ["high"]})];
    assert!(derive("groq", &raw, &[])[0].variants.is_empty());
}

#[test]
fn kept_levels_are_checked_again_on_reading() {
    // A hand-edited file with odd levels: only valid ones are spelled.
    let raw = vec![json!({"id": "claude-z", "kind": "chat", "reasoning": true,
                          "efforts": ["HIGH", "low", "../x", "low", 7]})];
    assert_eq!(variant_ids(&derive("anthropic", &raw, &[])[0]), ["low"]);
}

#[test]
fn random_capabilities_never_panic_and_give_valid_levels() {
    // A small deterministic generator, so the cases are the same each run.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let pool = [
        json!(true),
        json!(false),
        json!(null),
        json!(3),
        json!("high"),
        json!({"supported": true}),
        json!({"supported": false}),
        json!(["low", "max"]),
        json!({}),
    ];
    for _ in 0..2000 {
        let pick = |n: u64| pool[(n % pool.len() as u64) as usize].clone();
        let m = json!({
            "id": "m",
            "capabilities": {"effort": {"supported": pick(next()), "low": pick(next()), "max": pick(next())},
                             "thinking": pick(next())},
            "reasoning": pick(next()),
            "supported_parameters": pick(next()),
        });
        if let Some(r) = reasoning(obj(&m)) {
            assert!(r.efforts.iter().all(|l| effort_name(l)), "{m}");
            assert!(r.efforts.len() <= MAX_EFFORTS);
            if !r.efforts.is_empty() {
                assert!(r.supported);
            }
        }
        for pid in ["anthropic", "openrouter", "google", "groq"] {
            let _ = derive(pid, &normalize(&json!({"data": [m.clone()]})), &[]);
        }
    }
}

#[test]
fn the_reasoning_packages_speak_each_providers_own_protocol() {
    // The resolver uses a live model's variants only when its package speaks
    // the provider's protocol, so the API's levels must not be dropped there.
    use codesmith_ai::Protocol;
    for (pid, protocol) in [
        ("anthropic", Protocol::AnthropicMessages),
        ("openrouter", Protocol::OpenaiChat),
        ("google", Protocol::GoogleGemini),
    ] {
        let package = reasoning_package(pid).unwrap();
        assert_eq!(crate::catalog::protocol(package), Some(protocol), "{pid}");
        let builtin = crate::config::builtin(pid).and_then(|b| b.protocol);
        assert_eq!(builtin.unwrap_or(Protocol::OpenaiChat), protocol, "{pid}");
    }
}
