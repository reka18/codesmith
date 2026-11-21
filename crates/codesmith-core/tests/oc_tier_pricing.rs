//! Context-tier pricing (OpenCode's `SessionUsage.calculateCost`, from
//! `packages/core/test/session-runner.test.ts`, MIT): a step is priced at
//! the catalog's context tier its prompt passes, else at the default
//! price. See `crates/codesmith-core/PARITY.md`.

mod scripted;

use codesmith_ai::Usage;
use codesmith_core::catalog::{self, CacheCost, Cost, ModelsDev, Tier};
use codesmith_core::config::Loaded;
use codesmith_core::permission::{Fixed, Reply};
use codesmith_core::session::{Options, Session};
use codesmith_core::stats;
use scripted::*;
use serde_json::json;
use std::sync::Arc;

fn price(tier: Option<f64>, input: f64, output: f64, read: f64, write: f64) -> Cost {
    Cost {
        tier: tier.map(|size| Tier {
            kind: "context",
            size,
        }),
        input,
        output,
        cache: CacheCost { read, write },
    }
}

/// OpenCode's usage `{input: 80, output: 10, reasoning: 2, cache: {read:
/// 20, write}}`; CodeSmith's output includes the reasoning.
fn usage(write: u64) -> Usage {
    Usage {
        input: 80,
        output: 12,
        reasoning: 2,
        cache_read: 20,
        cache_write: write,
        ..Default::default()
    }
}

// session-runner.test.ts: "calculates step cost using the matching context tier"
#[test]
fn oc_usage_calculates_step_cost_using_the_matching_context_tier() {
    let costs = [
        price(None, 1.0, 2.0, 0.1, 0.5),
        price(Some(100.0), 3.0, 4.0, 0.2, 0.6),
    ];
    let cost = stats::step_cost(&costs, &usage(1)).unwrap();
    assert!((cost - 0.0002926).abs() < 1e-12, "{cost}");
    // At the tier's size exactly, the default price applies.
    let base = stats::step_cost(&costs, &usage(0)).unwrap();
    assert!((base - (80.0 + 24.0 + 2.0) / 1e6).abs() < 1e-12, "{base}");
    // The largest tier passed wins, whatever the order.
    let costs = [
        price(Some(100.0), 3.0, 4.0, 0.2, 0.6),
        price(None, 1.0, 2.0, 0.1, 0.5),
        price(Some(50.0), 9.0, 9.0, 9.0, 9.0),
    ];
    assert_eq!(stats::context_tier(&costs, &usage(1)), Some(&costs[0]));
}

// session-runner.test.ts: "does not apply an ineligible tier without base
// pricing". Adapted: no price is `None` (the cost is unknown, and not
// shown), where OpenCode records zero.
#[test]
fn oc_usage_does_not_apply_an_ineligible_tier_without_base_pricing() {
    let costs = [price(Some(100.0), 3.0, 4.0, 0.2, 0.6)];
    assert_eq!(stats::step_cost(&costs, &usage(0)), None);
    assert_eq!(stats::context_tier(&costs, &usage(0)), None);
}

/// The `fake` provider's model `m` in the catalog, with a default price
/// and a tier over 200k tokens of context.
fn tiered_catalog() -> Arc<ModelsDev> {
    let text = r#"{"fake": {"id": "fake", "name": "Fake", "env": ["FAKE_API_KEY"], "models": {
        "m": {"id": "m", "type": "chat", "name": "M", "release_date": "2026-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "reasoning": {"supported": false}},
            "limit": {"context": 1000000, "output": 1000},
            "cost": {"text": {
                "input": {"price": 2, "cache": {"read": 0.2},
                          "overrides": [{"when": {"context": {"gt": 200000}}, "price": 4, "cache": {"read": 0.4}}]},
                "output": {"price": 10, "overrides": [{"when": {"context": {"gt": 200000}}, "price": 15}]}}},
            "api": {"chat-completions": {"base_url": "https://api.fake.test/v1",
                "sdk": {"opencodeai": "@opencode/ai/providers/openai-compatible"}}}}}}}"#;
    let data = catalog::decode_snapshot(text).unwrap();
    assert_eq!(data.len(), 1);
    Arc::new(ModelsDev::fixed(data))
}

fn reply_with_usage(prompt: u64, cached: u64, completion: u64) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": "Done"}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": completion,
            "prompt_tokens_details": {"cached_tokens": cached}}}),
    ])
}

/// In a session, the step whose prompt passes 200k is priced at the tier
/// when it ends (and counts as billed at that price); the one below it is
/// priced with the rest at the default price. A configured price has no
/// tiers, so it prices every step.
#[tokio::test(flavor = "multi_thread")]
async fn oc_session_prices_each_step_at_its_context_tier() {
    let t = start(Setup {
        store: true,
        ..Default::default()
    })
    .await;
    let mut loaded: Loaded = t.loaded();
    loaded.config.models_dev = Some(tiered_catalog());
    let session = |loaded: &Loaded| {
        let mut s = Session::new(
            loaded,
            loaded.root.clone(),
            Options {
                agent: None,
                model: None,
                resume: None,
                title: Some("t".into()),
                parent: None,
                questioner: None,
                mcp: None,
            },
            Arc::new(Fixed(Reply::Once)),
            Some(t.store()),
        )
        .unwrap();
        s.set_retry_scale(0.0001);
        s
    };
    let mut s = session(&loaded);
    t.push([
        reply_with_usage(1_000, 0, 100),
        reply_with_usage(300_000, 100_000, 1_000),
    ]);
    let mut sink = Rec(t.seen.clone());
    s.prompt("Small", &mut sink).await.unwrap();
    s.prompt("Large", &mut sink).await.unwrap();
    // 200k fresh and 100k cached input, 1k output, at the tier.
    let tier = (200_000.0 * 4.0 + 100_000.0 * 0.4 + 1_000.0 * 15.0) / 1e6;
    let base = (1_000.0 * 2.0 + 100.0 * 10.0) / 1e6;
    let spend = s.spend();
    assert_eq!(spend.billed, Some(tier));
    assert_eq!(spend.unbilled.input, 1_000);
    let total = spend
        .cost(loaded.config.cost(&s.model_spec).as_ref())
        .unwrap();
    assert!((total - (tier + base)).abs() < 1e-12, "{total}");
    // The stored steps carry it too (for `codesmith stats`).
    let info = t.store().get(&s.id).unwrap().unwrap();
    assert_eq!(info.usage.cost, Some(tier));

    // A configured price wins, untiered.
    let mut configured = loaded.clone();
    configured.config = config_with(
        &t.base,
        "openai-chat",
        json!({"m": {"cost": {"input": 1.0, "output": 1.0}}}),
        json!({}),
    );
    configured.config.models_dev = Some(tiered_catalog());
    let mut s = session(&configured);
    t.push([reply_with_usage(300_000, 0, 0)]);
    s.prompt("Large again", &mut sink).await.unwrap();
    assert_eq!(s.spend().billed, None);
    assert_eq!(
        configured.config.tier_price(
            "fake/m",
            &Usage {
                input: 300_000,
                ..Default::default()
            }
        ),
        None
    );
}
