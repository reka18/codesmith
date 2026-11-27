use super::*;

#[test]
fn prices_are_per_token_strings_kept_per_million() {
    let m = json!({"pricing": {"prompt": "0.0000030000", "completion": "0.000015",
        "input_cache_read": "0.0000003", "input_cache_write": "0", "request": "0.02",
        "image": "0.001", "web_search": "0.01"}});
    let p = pricing(m.as_object().unwrap()).unwrap();
    let close = |k: &str, v: f64| assert!((p[k].as_f64().unwrap() - v).abs() < 1e-9, "{k}: {p}");
    close("input", 3.0);
    close("output", 15.0);
    close("cache_read", 0.3);
    close("request", 0.02);
    // A zero price is unpriced; images and searches aren't counted per step.
    assert!(p.get("cache_write").is_none());
    assert!(p.get("image").is_none() && p.get("web_search").is_none());
    for bad in [
        json!("abc"),
        json!("-1"),
        json!("NaN"),
        json!(null),
        json!("inf"),
    ] {
        assert_eq!(per_million(&bad), None, "{bad}");
    }
    assert!(pricing(json!({"pricing": {}}).as_object().unwrap()).is_none());
    assert!(pricing(json!({}).as_object().unwrap()).is_none());
}

#[test]
fn levels_are_spelled_for_each_api() {
    let levels = vec![
        Level {
            id: "none".into(),
            budget: None,
        },
        Level {
            id: "high".into(),
            budget: None,
        },
        Level {
            id: "max".into(),
            budget: Some(31_999),
        },
    ];
    let chat = spell(&levels, Protocol::OpenaiChat);
    let s = |v: &variant::Variant| Value::Object(v.settings.clone().unwrap());
    assert_eq!(s(&chat[0]), json!({"reasoning": {"effort": "none"}}));
    assert_eq!(s(&chat[1]), json!({"reasoning": {"effort": "high"}}));
    assert_eq!(s(&chat[2]), json!({"reasoning": {"max_tokens": 31_999}}));
    let msgs = spell(&levels, Protocol::AnthropicMessages);
    assert_eq!(s(&msgs[0]), json!({"thinking": {"type": "disabled"}}));
    assert_eq!(s(&msgs[1])["effort"], "high");
    assert_eq!(
        s(&msgs[2]),
        json!({"thinking": {"type": "enabled", "budgetTokens": 31_999}})
    );
    let gem = spell(&levels, Protocol::GoogleGemini);
    assert_eq!(s(&gem[0]), json!({"thinkingConfig": {"thinkingBudget": 0}}));
    assert_eq!(
        s(&gem[1]),
        json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"}})
    );
    assert_eq!(
        s(&gem[2]),
        json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 31_999}})
    );
    let resp = spell(&levels, Protocol::OpenaiResponses);
    assert_eq!(s(&resp[1])["reasoningEffort"], "high");
    // Spelled and read back, the levels are the same.
    for p in [
        Protocol::OpenaiChat,
        Protocol::AnthropicMessages,
        Protocol::GoogleGemini,
    ] {
        assert_eq!(levels_of(&spell(&levels, p)), levels);
    }
}

#[test]
fn a_kept_list_of_another_key_never_refuses_or_allows() {
    let kept = json!({"account": "vk:a", "models": [{"id": "openai/x"}]});
    assert!(check(Some(&kept), "vk:a", "openai/x").is_ok());
    let e = check(Some(&kept), "vk:a", "openai/y")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("bifrost/openai/y") && e.contains("1 models"),
        "{e}"
    );
    // Kept for another key: not this key's list, so nothing to check by.
    assert!(check(Some(&kept), "vk:b", "openai/y").is_ok());
    assert!(check(None, "vk:a", "anything").is_ok());
}

#[test]
fn the_account_is_a_hash_of_the_model_list_url_and_key() {
    let a = account("http://gw/v1/models", Some("sk-bf-secret"));
    assert!(a.starts_with("vk:") && !a.contains("secret"));
    assert!(is_account(&a) && is_account(&legacy_account("http://gw", None)));
    assert_ne!(a, account("http://gw/v1/models", Some("sk-bf-other")));
    assert_ne!(a, account("http://gw2/v1/models", Some("sk-bf-secret")));
    assert_ne!(
        a,
        account("http://gw/openai/v1/models", Some("sk-bf-secret"))
    );
    assert_eq!(a, account("http://gw/v1/models", Some("sk-bf-secret")));
    // Never 0.6.0's hash of the root, whatever the URL.
    assert_ne!(
        account("http://gw", Some("sk-bf-secret")),
        legacy_account("http://gw", Some("sk-bf-secret"))
    );
}

#[test]
fn the_tail_of_an_id_finds_its_catalog_entry() {
    let c = catalog::Catalog::new(&[]);
    assert!(upstream_model(&c, "fireworks/anthropic/claude-x").is_none());
    assert!(upstream_model(&c, "").is_none());
    assert!(upstream_model(&c, "single").is_none());
    assert_eq!(catalog_provider("gemini"), "google");
    assert_eq!(catalog_provider("bedrock"), "amazon-bedrock");
    assert_eq!(catalog_provider("openai"), "openai");
}
