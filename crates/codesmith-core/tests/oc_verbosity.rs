//! The configuration half of OpenCode's `plugin/verbosity.test.ts` (MIT):
//! the low text verbosity default for known OpenAI Responses models gives
//! way to a verbosity configured on the provider, the model or the chosen
//! variant, `null` included. The route half is in codesmith-ai's
//! `parity_options.rs`.

mod support;

use codesmith_core::config::Config;
use serde_json::json;

/// The verbosity a request with model `spec` goes out with.
fn verbosity(c: &Config, spec: &str) -> Option<String> {
    let m = c.resolve_model(spec).unwrap();
    codesmith_ai::defaults(m.provider.protocol, &m.provider.base_url, &m.model_id)
        .merged(&m.options)
        .str("textVerbosity")
        .map(str::to_string)
}

#[test]
fn configured_verbosity_wins_over_the_default() {
    support::isolate();
    let c: Config = serde_json::from_value(json!({"providers": {
        "openai": {"settings": {"apiKey": "x"}, "models": {
            "gpt-5.5": {},
            "gpt-6-astra": {"variants": {"quiet": {"textVerbosity": null}, "loud": {"textVerbosity": "high"}}},
            "no-default": {"id": "gpt-6-astra", "options": {"textVerbosity": null}},
            "alias": {"id": "gpt-6-astra"},
            "gpt-4o": {}
        }},
        "configured": {"protocol": "openai-responses",
            "settings": {"baseURL": "https://api.openai.com/v1", "apiKey": "x"},
            "options": {"textVerbosity": "medium"},
            "models": {"gpt-5.5": {}}}
    }}))
    .unwrap();
    let low = Some("low".to_string());
    assert_eq!(verbosity(&c, "openai/gpt-5.5"), low);
    assert_eq!(verbosity(&c, "openai/gpt-6-astra"), low);
    assert_eq!(verbosity(&c, "openai/alias"), low, "by the id it sends");
    assert_eq!(verbosity(&c, "openai/gpt-6-astra#quiet"), None);
    assert_eq!(
        verbosity(&c, "openai/gpt-6-astra#loud").as_deref(),
        Some("high")
    );
    assert_eq!(verbosity(&c, "openai/no-default"), None);
    assert_eq!(verbosity(&c, "openai/gpt-4o"), None);
    assert_eq!(
        verbosity(&c, "configured/gpt-5.5").as_deref(),
        Some("medium")
    );
}
