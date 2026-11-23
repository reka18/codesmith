//! Tests of [`super`]: forgiving specs, Claude through Claude Code, and
//! the default a login sets.

use super::*;
use crate::catalog::{Json, ModelsDev};
use codesmith_ai::claude_agent::catalog::Model as ClaudeModel;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A catalog of `(provider, [(model, release date)])`.
fn catalog(providers: &[(&str, &[(&str, &str)])]) -> Arc<ModelsDev> {
    let ps: Vec<String> = providers
        .iter()
        .map(|(pid, models)| {
            let ms: Vec<String> = models
                .iter()
                .map(|(id, date)| {
                    format!(
                        r#""{id}": {{"id": "{id}", "type": "chat", "name": "{id}", "release_date": "{date}",
                        "modalities": {{"input": ["text"], "output": ["text"]}},
                        "capabilities": {{"tools": {{"supported": true}}, "reasoning": {{"supported": false}}}},
                        "limit": {{"context": 200000, "output": 32000}},
                        "api": {{"chat-completions": {{"base_url": "https://api.test.invalid/v1",
                            "sdk": {{"opencodeai": "@opencode/ai/providers/openai-compatible"}}}}}}}}"#
                    )
                })
                .collect();
            format!(
                r#""{pid}": {{"id": "{pid}", "name": "{pid}", "env": [], "models": {{{}}}}}"#,
                ms.join(",")
            )
        })
        .collect();
    let text = format!("{{{}}}", ps.join(","));
    Arc::new(ModelsDev::fixed(crate::catalog::normalize(
        &Json::parse(&text).unwrap(),
    )))
}

const ANTHROPIC: &[(&str, &str)] = &[
    ("claude-opus-4-1", "2025-08-05"),
    ("claude-opus-4-1-20250805", "2025-08-05"),
    ("claude-opus-4-5", "2025-11-24"),
    ("claude-sonnet-4-5", "2025-09-29"),
    ("claude-haiku-4-5", "2025-10-15"),
];

fn config(v: serde_json::Value) -> Config {
    let mut c: Config = serde_json::from_value(v).unwrap();
    c.models_dev = Some(catalog(&[("anthropic", ANTHROPIC)]));
    c
}

/// Anthropic with a key.
fn keyed() -> (Config, Access) {
    (
        config(json!({"providers": {"anthropic": {"settings": {"apiKey": "k"}}}})),
        Access {
            claude_code: None,
            anthropic_ready: true,
        },
    )
}

fn claude_model(id: &str, resolved: Option<&str>, efforts: &[&str]) -> ClaudeModel {
    ClaudeModel {
        id: id.into(),
        name: id.into(),
        efforts: efforts.iter().map(|e| e.to_string()).collect(),
        context_window: None,
        resolved_model: resolved.map(str::to_string),
    }
}

fn claude_saved() -> ClaudeCatalog {
    ClaudeCatalog {
        price_source: None,
        models: vec![
            claude_model("opus", Some("claude-opus-4-5-20251124"), &["low", "high"]),
            claude_model("sonnet", Some("claude-sonnet-4-5-20250929"), &[]),
            claude_model("sonnet[1m]", Some("claude-sonnet-4-5-20250929[1m]"), &[]),
            claude_model("haiku", Some("claude-haiku-4-5-20251015"), &[]),
        ],
    }
}

/// Claude Code signed in, Anthropic without a key.
fn claude_code() -> (Config, Access) {
    (
        config(json!({})),
        Access {
            claude_code: Some(claude_saved()),
            anthropic_ready: false,
        },
    )
}

fn spec(c: &Config, a: &Access, s: &str) -> String {
    canonical(c, s, a)
        .unwrap_or_else(|e| panic!("{s:?}: {e:#}"))
        .spec
}

fn err(c: &Config, a: &Access, s: &str) -> String {
    match canonical(c, s, a) {
        Ok(r) => panic!("{s:?} resolved to {}", r.spec),
        Err(e) => format!("{e:#}"),
    }
}

fn cands(list: &[(&str, &str, i64)]) -> Vec<Candidate> {
    list.iter()
        .map(|(p, id, r)| Candidate::new(p, id, *r))
        .collect()
}

// --- 1. Forgiving specs ---

// Regression: `anthropic/ opus` (a stray space, a partial name) was sent
// as the model " opus".
#[test]
fn a_spaced_partial_spec_names_the_newest_matching_model() {
    let (c, a) = keyed();
    assert_eq!(spec(&c, &a, "anthropic/ opus"), "anthropic/claude-opus-4-5");
    assert_eq!(spec(&c, &a, "anthropic/opus"), "anthropic/claude-opus-4-5");
    let r = canonical(&c, "anthropic/ opus", &a).unwrap();
    assert_eq!(
        r.note.as_deref(),
        Some("anthropic/opus is anthropic/claude-opus-4-5")
    );
}

// Regression, at the lowest level: the parts of a spec are trimmed
// before the model id is sent.
#[test]
fn resolve_model_trims_each_part() {
    let c: Config = serde_json::from_value(json!({"providers": {"p": {
        "settings": {"baseURL": "https://p.test.invalid/v1", "apiKey": "k"},
        "models": {"m": {"variants": {"fast": {}}}}
    }}}))
    .unwrap();
    let m = c.resolve_model(" p / m # fast ").unwrap();
    assert_eq!(m.provider_id, "p");
    assert_eq!(m.model_id, "m");
    assert_eq!(m.variant.as_deref(), Some("fast"));
    assert!(c.resolve_model(" p /  ").is_err());
    assert!(c.resolve_model("  / m").is_err());
}

#[test]
fn whitespace_around_every_part_is_dropped() {
    let (c, a) = keyed();
    for s in [
        "  anthropic/claude-sonnet-4-5  ",
        "anthropic /claude-sonnet-4-5",
        "anthropic/\tclaude-sonnet-4-5\n",
        " anthropic / claude-sonnet-4-5 ",
    ] {
        assert_eq!(spec(&c, &a, s), "anthropic/claude-sonnet-4-5", "{s:?}");
    }
    // The variant too, and it stays.
    assert_eq!(
        spec(&c, &a, " anthropic / claude-sonnet-4-5 # high "),
        "anthropic/claude-sonnet-4-5#high"
    );
    // An exact spec has no note.
    let r = canonical(&c, " anthropic/claude-sonnet-4-5 ", &a).unwrap();
    assert_eq!(r.note, None);
}

#[test]
fn case_and_separators_do_not_matter() {
    let (c, a) = keyed();
    assert_eq!(spec(&c, &a, "Anthropic/OPUS"), "anthropic/claude-opus-4-5");
    assert_eq!(
        spec(&c, &a, "anthropic/Claude Opus 4.1"),
        "anthropic/claude-opus-4-1"
    );
    assert_eq!(
        spec(&c, &a, "anthropic/claude_haiku_4.5"),
        "anthropic/claude-haiku-4-5"
    );
}

#[test]
fn an_alias_wins_over_its_dated_twin() {
    let (c, a) = keyed();
    assert_eq!(
        spec(&c, &a, "anthropic/opus-4-1"),
        "anthropic/claude-opus-4-1"
    );
    // The dated id itself is kept.
    assert_eq!(
        spec(&c, &a, "anthropic/claude-opus-4-1-20250805"),
        "anthropic/claude-opus-4-1-20250805"
    );
}

#[test]
fn variants_and_slashed_ids_parse_as_in_opencode() {
    let mut c = config(json!({"providers": {"or": {
        "settings": {"baseURL": "https://or.test.invalid/v1", "apiKey": "k"},
        "models": {"openai/gpt-5": {}, "meta/llama-3": {}}
    }}}));
    c.models_dev = None;
    let a = Access::default();
    // The first `/` splits: the model id keeps its own.
    assert_eq!(spec(&c, &a, "or/openai/gpt-5#high"), "or/openai/gpt-5#high");
    assert_eq!(spec(&c, &a, "or/ openai/gpt-5 "), "or/openai/gpt-5");
    // An empty variant is none.
    assert_eq!(spec(&c, &a, "or/meta/llama-3#"), "or/meta/llama-3");
    assert_eq!(spec(&c, &a, "or/llama"), "or/meta/llama-3");
}

#[test]
fn an_unknown_model_is_refused_with_the_closest_ones() {
    let (c, a) = keyed();
    let e = err(&c, &a, "anthropic/claude-sonet-4-5");
    assert!(e.contains("unknown model \"claude-sonet-4-5\""), "{e}");
    assert!(
        e.contains("did you mean: anthropic/claude-sonnet-4-5"),
        "{e}"
    );
    // Nothing close: where to look.
    let e = err(&c, &a, "anthropic/zzzzzzzz");
    assert!(e.contains("unknown model"), "{e}");
    assert!(e.contains("`codesmith models anthropic`"), "{e}");
}

#[test]
fn an_ambiguous_partial_name_is_refused_with_the_choices() {
    let mut c = config(json!({}));
    c.models_dev = Some(catalog(&[(
        "anthropic",
        &[("claude-x-a", "2025-01-01"), ("claude-x-b", "2025-01-01")],
    )]));
    let a = Access {
        claude_code: None,
        anthropic_ready: true,
    };
    let e = err(&c, &a, "anthropic/claude-x");
    assert!(e.contains("matches more than one"), "{e}");
    assert!(
        e.contains("anthropic/claude-x-a, anthropic/claude-x-b"),
        "{e}"
    );
    // A newer one breaks the tie.
    c.models_dev = Some(catalog(&[(
        "anthropic",
        &[("claude-x-a", "2025-01-01"), ("claude-x-b", "2025-02-01")],
    )]));
    assert_eq!(spec(&c, &a, "anthropic/claude-x"), "anthropic/claude-x-b");
}

#[test]
fn empty_and_whitespace_specs_are_refused() {
    let (c, a) = keyed();
    for s in ["", "   ", "\t\n", "#high", " # "] {
        let e = err(&c, &a, s);
        assert!(e.contains("no model given"), "{s:?}: {e}");
    }
    for s in ["anthropic/", "anthropic/   ", "anthropic/#high"] {
        let e = err(&c, &a, s);
        assert!(
            e.contains("no model given for provider \"anthropic\""),
            "{s:?}: {e}"
        );
        assert!(e.contains("e.g. anthropic/claude-opus-4-5"), "{e}");
    }
}

#[test]
fn an_unknown_provider_is_refused_with_the_closest_ones() {
    let (c, a) = keyed();
    let e = err(&c, &a, "antropic/opus");
    assert!(e.contains("unknown provider \"antropic\""), "{e}");
    assert!(e.contains("did you mean:"), "{e}");
    assert!(e.contains("anthropic"), "{e}");
    let e = err(&c, &a, "qqqqqqqqqqqqqq/m");
    assert!(e.contains("unknown provider"), "{e}");
}

#[test]
fn a_disabled_provider_is_refused() {
    let (mut c, a) = keyed();
    c.disabled_providers = vec!["anthropic".into()];
    let e = err(&c, &a, "anthropic/opus");
    assert!(e.contains("disabled"), "{e}");
}

#[test]
fn a_model_named_alone_is_found_among_the_runnable_ones() {
    let (c, a) = keyed();
    assert_eq!(spec(&c, &a, "opus"), "anthropic/claude-opus-4-5");
    assert_eq!(
        spec(&c, &a, "  sonnet #high "),
        "anthropic/claude-sonnet-4-5#high"
    );
    assert_eq!(spec(&c, &a, "/haiku"), "anthropic/claude-haiku-4-5");
    // A provider's name alone says to name a model.
    let e = err(&c, &a, "anthropic");
    assert!(e.contains("is a provider"), "{e}");
    let e = err(&c, &a, "nothing-like-it-anywhere");
    assert!(e.contains("unknown model"), "{e}");
}

#[test]
fn with_no_providers_available_a_bare_name_says_so() {
    let mut c = config(json!({"disabled_providers": ["lmstudio", "ollama", "vllm", "llamacpp"]}));
    c.models_dev = None;
    let e = err(&c, &Access::default(), "opus");
    assert!(e.contains("no models available"), "{e}");
}

#[test]
fn a_provider_without_a_list_or_on_this_machine_takes_any_model() {
    let mut c = config(json!({"providers": {
        "box": {"settings": {"baseURL": "http://127.0.0.1:1234/v1"}, "models": {"qwen3-8b": {}}},
        "bare": {"settings": {"baseURL": "https://bare.test.invalid/v1", "apiKey": "k"}}
    }}));
    c.models_dev = None;
    let a = Access::default();
    assert_eq!(spec(&c, &a, "box/llama-new"), "box/llama-new");
    // Its own models are still found by id, ignoring case.
    assert_eq!(spec(&c, &a, "box/QWEN3-8B"), "box/qwen3-8b");
    // A partial name goes to the server as typed: it may serve it.
    assert_eq!(spec(&c, &a, "box/qwen3"), "box/qwen3");
    assert_eq!(spec(&c, &a, "bare/anything "), "bare/anything");
    assert_eq!(spec(&c, &a, "ollama/qwen3"), "ollama/qwen3");
}

#[test]
fn canonical_specs_are_fixed_points() {
    let (c, a) = keyed();
    for s in ["anthropic/ opus", "opus", "Anthropic/Claude Haiku 4.5#low"] {
        let once = spec(&c, &a, s);
        assert_eq!(spec(&c, &a, &once), once, "{s:?}");
        assert_eq!(canonical(&c, &once, &a).unwrap().note, None);
    }
}

// --- The matcher ---

#[test]
fn normalize_folds_case_and_separators() {
    assert_eq!(normalize(" Claude  Opus_4.5 "), "claude-opus-4-5");
    assert_eq!(normalize("--a..b__c--"), "a-b-c");
    assert_eq!(normalize(""), "");
    assert_eq!(normalize(" - . _ "), "");
    assert_eq!(normalize("ÉCLAIR"), "éclair");
}

#[test]
fn pick_prefers_exact_then_folded_then_newest_containing() {
    let c = cands(&[
        ("p", "Model-A", 1),
        ("p", "model-a-pro", 5),
        ("p", "other", 9),
    ]);
    assert_eq!(pick("Model-A", &c), Match::One(0));
    assert_eq!(pick("model-a", &c), Match::One(0));
    assert_eq!(pick("model a", &c), Match::One(0));
    assert_eq!(pick("a-pro", &c), Match::One(1));
    assert_eq!(pick("odel", &c), Match::One(1));
    assert_eq!(pick("zzz", &c), Match::None);
    assert_eq!(pick("", &c), Match::None);
    assert_eq!(pick("  ", &c), Match::None);
    assert_eq!(pick("-._", &c), Match::None);
    assert_eq!(pick("x", &[]), Match::None);
}

#[test]
fn pick_tells_aliases_from_different_models() {
    let c = cands(&[("a", "m", 1), ("b", "m", 1), ("b", "m-mini", 1)]);
    assert_eq!(pick("m", &c), Match::Ambiguous(vec![0, 1]));
    // Undated aliases of one model: the plain one.
    let c = cands(&[("p", "opus[1m]", 0), ("p", "opus", 0)]);
    assert_eq!(pick("opu", &c), Match::One(1));
    // Undated different models: ambiguous.
    let c = cands(&[("p", "qwen3-8b", 0), ("p", "qwen3-32b", 0)]);
    assert!(matches!(pick("qwen3", &c), Match::Ambiguous(_)));
    // One id that extends another in a different provider is a different
    // model.
    let c = cands(&[("a", "mx", 0), ("b", "mx-y", 0)]);
    assert!(matches!(pick("x", &c), Match::Ambiguous(_)));
    assert_eq!(pick("MX", &c), Match::One(0));
}

#[test]
fn closest_finds_typos_and_skips_the_unrelated() {
    let c = cands(&[
        ("p", "claude-sonnet-4-5", 0),
        ("p", "claude-opus-4-5", 0),
        ("p", "gpt-5", 0),
    ]);
    assert_eq!(closest("sonet", &c, 3), vec![0]);
    assert_eq!(closest("opuss", &c, 3), vec![1]);
    assert!(closest("zzzzzzzz", &c, 3).is_empty());
    assert!(closest("", &c, 3).is_empty());
    assert_eq!(closest("gpt-5", &c, 1), vec![2]);
}

// Bounded property: padding any part of a spec with whitespace never
// changes what it resolves to, and nothing typed panics.
#[test]
fn padding_never_changes_the_result_and_nothing_panics() {
    let (c, a) = keyed();
    let specs = [
        "anthropic/opus",
        "anthropic/claude-haiku-4-5#low",
        "sonnet",
        "anthropic/zzz",
        "nope/x",
        "anthropic/",
    ];
    let pads = ["", " ", "\t", "  \n "];
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seed: u64 = 0x5eed;
    for _ in 0..300 {
        assert!(Instant::now() < deadline, "the property run took too long");
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let s = specs[(seed >> 33) as usize % specs.len()];
        let pad = |k: u64| pads[(seed >> k) as usize % pads.len()];
        let padded = match s.split_once('/') {
            Some((p, m)) => format!("{}{p}{}/{}{m}{}", pad(3), pad(7), pad(11), pad(17)),
            None => format!("{}{s}{}", pad(5), pad(13)),
        };
        let plain = canonical(&c, s, &a).map(|r| r.spec);
        let got = canonical(&c, &padded, &a).map(|r| r.spec);
        assert_eq!(got.is_ok(), plain.is_ok(), "{padded:?}");
        if let (Ok(g), Ok(p)) = (&got, &plain) {
            assert_eq!(g, p, "{padded:?}");
        }
    }
    // Odd input: control characters, unicode, many separators.
    let mut odd = String::new();
    for i in 0..200u32 {
        assert!(Instant::now() < deadline, "the odd-input run took too long");
        odd.push(char::from_u32(0x20 + (i * 7919) % 0x2fff).unwrap_or('?'));
        let _ = canonical(&c, &odd, &a);
        let _ = canonical(&c, &format!("anthropic/{odd}"), &a);
        let _ = normalize(&odd);
    }
}

// Stress: matching against a large list stays quick.
#[test]
fn matching_a_large_list_is_quick() {
    let list: Vec<Candidate> = (0..5000)
        .map(|i| Candidate::new("p", &format!("family-{}-model-{i}", i % 50), i as i64))
        .collect();
    let start = Instant::now();
    assert_eq!(pick("family-7-model-4957", &list), Match::One(4957));
    assert_eq!(pick("family-7-", &list), Match::One(4957));
    assert!(matches!(pick("no-such", &list), Match::None));
    let near = closest("family-7-model-495", &list, 3);
    assert_eq!(near.len(), 3);
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
}

// --- 4. Claude through Claude Code ---

// Regression: `anthropic/opus` without a key went to the Anthropic API and
// failed with "HTTP 401: x-api-key header is required".
#[test]
fn anthropic_without_a_key_runs_through_a_signed_in_claude_code() {
    let (c, a) = claude_code();
    let r = canonical(&c, "anthropic/ opus", &a).unwrap();
    assert_eq!(r.spec, "claude-agent/opus");
    let note = r.note.unwrap();
    assert!(note.contains("No Anthropic API key"), "{note}");
    assert!(note.contains("Claude Code"), "{note}");
    // By the model it resolves to; `[1m]` is the same model.
    assert_eq!(
        spec(&c, &a, "anthropic/claude-sonnet-4-5"),
        "claude-agent/sonnet"
    );
    assert_eq!(
        spec(&c, &a, "anthropic/claude-haiku-4-5"),
        "claude-agent/haiku"
    );
    assert_eq!(spec(&c, &a, "anthropic/Sonnet"), "claude-agent/sonnet");
}

#[test]
fn a_claude_model_claude_code_lacks_is_refused_with_the_fix() {
    let (c, a) = claude_code();
    let e = err(&c, &a, "anthropic/claude-opus-4-1");
    assert!(e.contains("needs an Anthropic API key"), "{e}");
    assert!(e.contains("switch to claude-agent/opus"), "{e}");
    assert!(e.contains("`codesmith auth login anthropic`"), "{e}");
    assert_eq!(e.lines().count(), 1, "{e}");
    // Unknown altogether: still the fix, with the newest Claude Code model.
    let e = err(&c, &a, "anthropic/zzz");
    assert!(e.contains("claude-agent/opus"), "{e}");
}

#[test]
fn variants_carry_over_only_when_claude_code_offers_them() {
    let (c, a) = claude_code();
    assert_eq!(
        spec(&c, &a, "anthropic/opus#high"),
        "claude-agent/opus#high"
    );
    let r = canonical(&c, "anthropic/opus#max", &a).unwrap();
    assert_eq!(r.spec, "claude-agent/opus");
    assert!(r.note.unwrap().contains("no \"max\" effort"));
}

#[test]
fn with_a_key_or_without_claude_code_anthropic_stays_anthropic() {
    let (c, mut a) = claude_code();
    a.anthropic_ready = true;
    assert_eq!(spec(&c, &a, "anthropic/opus"), "anthropic/claude-opus-4-5");
    // Claude Code not signed in, or not installed: nothing to run it
    // through, and the spec is still made canonical.
    let a = Access {
        claude_code: None,
        anthropic_ready: false,
    };
    assert_eq!(spec(&c, &a, "anthropic/opus"), "anthropic/claude-opus-4-5");
}

#[test]
fn claude_agent_specs_are_checked_against_claude_codes_list() {
    let (c, a) = claude_code();
    assert_eq!(spec(&c, &a, "claude-agent/ OPUS"), "claude-agent/opus");
    let e = err(&c, &a, "claude-agent/gpt");
    assert!(e.contains("unknown model"), "{e}");
    // Equivalents, for an error message.
    assert_eq!(
        claude_code_for(&c, "claude-sonnet-4-5", &a).as_deref(),
        Some("claude-agent/sonnet")
    );
    assert_eq!(
        claude_code_for(&c, "claude-opus-4-1", &a).as_deref(),
        Some("claude-agent/opus")
    );
    assert_eq!(claude_code_for(&c, "x", &Access::default()), None);
}

// --- 3. Logins select their provider ---

#[test]
fn newest_of_a_provider_is_its_newest_text_model() {
    let (c, a) = claude_code();
    assert_eq!(
        newest_of(&c, "claude-agent", &a).as_deref(),
        Some("claude-agent/opus")
    );
    assert_eq!(
        newest_of(&c, "anthropic", &a).as_deref(),
        Some("anthropic/claude-opus-4-5")
    );
    assert_eq!(newest_of(&c, "nope", &a), None);
}

#[test]
fn only_an_unset_or_login_chosen_model_is_implicit() {
    assert!(implicit(None, None));
    assert!(implicit(None, Some("a/b")));
    assert!(implicit(Some("a/b"), Some("a/b")));
    assert!(implicit(Some(" a/b"), Some("a/b ")));
    assert!(!implicit(Some("a/b"), None));
    assert!(!implicit(Some("a/b"), Some("a/c")));
}

// Regression: signing in to Claude Code left the session on the model it
// had (a local server that was not running).
#[test]
fn a_login_saves_its_provider_as_the_default_unless_one_was_chosen() {
    let (mut c, a) = claude_code();
    let dir = tempfile::tempdir().unwrap();
    let (cfg, data) = (dir.path().join("config"), dir.path().join("data"));
    // Nothing chosen: switched and saved.
    let got = adopt_login(&c, "claude-agent", &a, &cfg, &data).unwrap();
    assert_eq!(got.spec, "claude-agent/opus");
    let file = got.saved.unwrap();
    assert_eq!(file, cfg.join("codesmith.json"));
    let read = |f: &std::path::Path| -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap()
    };
    assert_eq!(read(&file)["model"], "claude-agent/opus");
    assert_eq!(auto_model(&data).as_deref(), Some("claude-agent/opus"));
    // The login's own choice: the same login changes nothing, another
    // login may change it.
    c.model = Some("claude-agent/opus".into());
    assert_eq!(adopt_login(&c, "claude-agent", &a, &cfg, &data), None);
    let got = adopt_login(&c, "anthropic", &a, &cfg, &data).unwrap();
    assert_eq!(got.spec, "anthropic/claude-opus-4-5");
    assert_eq!(read(&file)["model"], "anthropic/claude-opus-4-5");
    // A model the user set stays, and the file is not touched.
    c.model = Some("openai/gpt-5".into());
    let before = std::fs::read_to_string(&file).unwrap();
    assert_eq!(adopt_login(&c, "claude-agent", &a, &cfg, &data), None);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    // A provider with nothing to run changes nothing.
    c.model = None;
    assert_eq!(adopt_login(&c, "nope", &a, &cfg, &data), None);
}

#[test]
fn a_login_does_not_override_a_model_set_by_hand_in_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("codesmith.json");
    std::fs::write(&file, r#"{"model": "mine/m"}"#).unwrap();
    let e = save_global_model(dir.path(), "claude-agent/opus", None).unwrap_err();
    assert!(e.to_string().contains("already sets"), "{e}");
    let e = save_global_model(dir.path(), "claude-agent/opus", Some("other/x")).unwrap_err();
    assert!(e.to_string().contains("already sets"), "{e}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        r#"{"model": "mine/m"}"#
    );
    // The jsonc file is used when it is the one there.
    let dir = tempfile::tempdir().unwrap();
    let jsonc = dir.path().join("codesmith.jsonc");
    std::fs::write(&jsonc, "// mine\n{\n  \"theme\": \"x\" // keep\n}\n").unwrap();
    assert_eq!(save_global_model(dir.path(), "a/b", None).unwrap(), jsonc);
    let text = std::fs::read_to_string(&jsonc).unwrap();
    assert!(
        text.contains("// mine") && text.contains("// keep"),
        "{text}"
    );
    assert!(!dir.path().join("codesmith.json").exists());
    // No stray temporary file is left.
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(left.len(), 1);
}

#[test]
fn setting_the_model_keeps_the_rest_of_the_file() {
    assert_eq!(
        set_model_text("", "a/b", None).unwrap(),
        "{\n  \"model\": \"a/b\"\n}\n"
    );
    assert_eq!(
        set_model_text("  \n", "a/b", None).unwrap(),
        "{\n  \"model\": \"a/b\"\n}\n"
    );
    assert_eq!(
        set_model_text("{}", "a/b", None).unwrap(),
        "{\n  \"model\": \"a/b\"\n}"
    );
    let t = set_model_text("{\"theme\": \"x\"}", "a/b", None).unwrap();
    assert_eq!(t, "{\n  \"model\": \"a/b\",\"theme\": \"x\"}");
    // Comments, a nested `model`, braces and quotes in strings stay.
    let src = r#"/* top { */ {
  // "model": "commented/out"
  "agent": {"build": {"model": "nested/m"}},
  "note": "a } \" { \"model\": \"x\"",
}"#;
    let t = set_model_text(src, "c/d", None).unwrap();
    assert!(t.contains("\"model\": \"nested/m\""), "{t}");
    assert!(t.contains("// \"model\": \"commented/out\""), "{t}");
    assert!(t.contains(r#""note": "a } \" { \"model\": \"x\"""#), "{t}");
    let v: serde_json::Value = serde_json::from_str(&crate::config::strip_jsonc(&t)).unwrap();
    assert_eq!(v["model"], "c/d");
    assert_eq!(v["agent"]["build"]["model"], "nested/m");
    // Replacing the login's own choice, in place.
    let t = set_model_text(
        "{\"a\": 1, \"model\": \"old/x\", \"b\": 2}",
        "new/y",
        Some("old/x"),
    )
    .unwrap();
    assert_eq!(t, "{\"a\": 1, \"model\": \"new/y\", \"b\": 2}");
    // A spec that needs escaping.
    let t = set_model_text("{}", "p/m\"q", None).unwrap();
    let v: serde_json::Value = serde_json::from_str(&t).unwrap();
    assert_eq!(v["model"], "p/m\"q");
}

#[test]
fn a_file_that_is_not_a_config_object_is_left_alone() {
    for bad in [
        "[1, 2]",
        "\"x\"",
        "{\"model\": 3}",
        "{\"model\": \"a",
        "nonsense",
        "{\"a\": ",
        "/* unterminated",
    ] {
        assert!(set_model_text(bad, "a/b", None).is_err(), "{bad:?}");
    }
}
