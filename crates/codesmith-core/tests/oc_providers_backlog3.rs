//! Ports of OpenCode's provider config and model resolution tests
//! (`config/provider.test.ts` and `model-resolver.test.ts`, MIT) from the
//! porting backlog, through `Config::resolve_model` and `config::load_from`
//! (for several config documents), with the user's home, config and data
//! kept out. Adapted throughout: OpenCode's AI SDK packages are CodeSmith
//! protocols, its `modelID` is a model's `id`, and its `settings` (on a
//! variant) are `options`.

mod scripted;

use codesmith_ai::{Auth, Protocol};
use codesmith_core::config::{load_from, Config, ModelRef, Sources};
use serde_json::{json, Value};

fn config(v: Value) -> Config {
    scripted::isolate();
    serde_json::from_value(v).unwrap()
}

/// `first` as the global config and `second` as the project's per-repo
/// file, merged as CodeSmith loads them.
fn layered(first: Value, mut second: Value) -> Config {
    scripted::isolate();
    let d = tempfile::tempdir().unwrap();
    let base = d.path().canonicalize().unwrap();
    let global = base.join("global");
    let project = base.join("project");
    std::fs::create_dir_all(global.join("projects")).unwrap();
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::write(global.join("codesmith.json"), first.to_string()).unwrap();
    second["path"] = serde_json::json!(project);
    let file = global.join("projects/project.json");
    std::fs::write(&file, second.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let sources = Sources {
        global_dir: global,
        file: None,
        content: None,
        project: true,
        per_repo: true,
    };
    load_from(&project, &sources).unwrap().config
}

fn resolve(c: &Config, spec: &str) -> ModelRef {
    c.resolve_model(spec).unwrap()
}

fn header<'a>(m: &'a ModelRef, name: &str) -> Option<&'a str> {
    m.provider
        .headers
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// config/provider.test.ts: "keeps provider and model compatibility
// precedence across config documents" (adapted: `requireFinishReason` and
// `maxTokensField`; CodeSmith has no `supportsPromptCacheKey`)
#[test]
fn oc_provider_keeps_compatibility_precedence_across_config_documents() {
    let c = layered(
        json!({"providers": {"proxy": {"protocol": "openai-chat",
            "settings": {"baseURL": "https://proxy.example/v1", "apiKey": "k"},
            "compatibility": {"requireFinishReason": false},
            "models": {"claude": {"compatibility": {"maxTokensField": "max_tokens"}}}}}}),
        json!({"providers": {"proxy": {
            "compatibility": {"maxTokensField": "max_completion_tokens"},
            "models": {"alias": {"id": "catalog"}}}}}),
    );
    let provider_default = resolve(&c, "proxy/alias");
    assert_eq!(provider_default.model_id, "catalog");
    assert_eq!(
        provider_default.provider.compat.max_tokens_field.as_deref(),
        Some("max_completion_tokens")
    );
    assert_eq!(
        provider_default.provider.compat.require_finish_reason,
        Some(false)
    );
    // A later provider default does not override an earlier model setting.
    let claude = resolve(&c, "proxy/claude");
    assert_eq!(
        claude.provider.compat.max_tokens_field.as_deref(),
        Some("max_tokens")
    );
    assert_eq!(claude.provider.compat.require_finish_reason, Some(false));
}

// config/provider.test.ts: "applies provider compatibility to every
// provider model with model overrides"
#[test]
fn oc_provider_applies_provider_compatibility_to_every_model_with_model_overrides() {
    let c = config(json!({"providers": {"proxy": {"protocol": "openai-chat",
        "settings": {"baseURL": "https://proxy.example/v1", "apiKey": "k"},
        "compatibility": {"requireFinishReason": false, "maxTokensField": "max_tokens"},
        "models": {"configured": {"compatibility": {"maxTokensField": "max_completion_tokens"}}}}}}));
    // A model only the provider's defaults describe.
    let unlisted = resolve(&c, "proxy/unlisted");
    assert_eq!(unlisted.provider.compat.require_finish_reason, Some(false));
    assert_eq!(
        unlisted.provider.compat.max_tokens_field.as_deref(),
        Some("max_tokens")
    );
    let configured = resolve(&c, "proxy/configured");
    assert_eq!(
        configured.provider.compat.require_finish_reason,
        Some(false)
    );
    assert_eq!(
        configured.provider.compat.max_tokens_field.as_deref(),
        Some("max_completion_tokens")
    );
}

// config/provider.test.ts: "passes configured stream usage compatibility to
// the runtime model"
#[test]
fn oc_provider_passes_configured_stream_usage_compatibility_to_the_model() {
    let c = config(json!({"providers": {"strict": {"protocol": "openai-chat",
        "settings": {"baseURL": "https://strict.test/v1", "apiKey": "k"},
        "compatibility": {"streamUsage": false}}}}));
    assert_eq!(
        resolve(&c, "strict/catalog").provider.compat.stream_usage,
        Some(false)
    );
}

// config/provider.test.ts: "adds key auth for custom providers without env
// credentials" (adapted: any provider takes a key saved with `codesmith
// auth`; there is no list of login methods to add one to)
#[test]
fn oc_provider_takes_a_saved_key_for_a_custom_provider_without_env_credentials() {
    let c = config(
        json!({"providers": {"litellm-backlog3": {"protocol": "openai-chat",
        "settings": {"baseURL": "https://litellm.example/v1"}, "models": {"chat": {}}}}}),
    );
    assert_eq!(resolve(&c, "litellm-backlog3/chat").provider.api_key, None);
    codesmith_core::auth::set_in(
        &codesmith_core::paths::auth_file(),
        "litellm-backlog3",
        "saved",
    )
    .unwrap();
    assert_eq!(
        resolve(&c, "litellm-backlog3/chat")
            .provider
            .api_key
            .as_deref(),
        Some("saved")
    );
}

// config/provider.test.ts: "lists a custom model by its configured type and
// an untyped one as chat"
#[test]
fn oc_provider_lists_a_custom_model_by_its_configured_type() {
    let c = config(json!({"providers": {"custom": {"protocol": "openai-chat",
        "settings": {"baseURL": "https://custom.example/v1"},
        "models": {"chat": {}, "painter": {"type": "image"}}}}}));
    assert_eq!(c.list_models("custom", &[]), ["chat"]);
    assert_eq!(c.list_models("custom", &["image"]), ["painter"]);
}

// config/provider.test.ts: "resolves a config model with only input/output
// capabilities using default tools" (adapted: the input modalities; tools
// are always offered)
#[test]
fn oc_provider_resolves_a_model_with_only_input_and_output_capabilities() {
    let c = config(json!({"providers": {"custom": {"protocol": "openai-chat",
        "settings": {"baseURL": "https://custom.example/v1"},
        "models": {"demo": {"name": "Demo", "capabilities": {"input": ["text"], "output": ["text"]}}}}}}));
    assert_eq!(
        resolve(&c, "custom/demo").input,
        Some(vec!["text".to_string()])
    );
}

// config/provider.test.ts: "lets a configured provider baseURL override
// catalog model baseURLs" (adapted: the provider's URL serves each of its
// models; a model has no URL of its own in config)
#[test]
fn oc_provider_lets_a_configured_base_url_serve_every_model() {
    let c = config(json!({"providers": {"gateway": {"protocol": "openai-chat",
        "settings": {"baseURL": "http://127.0.0.1:17322"}, "models": {"chat": {}, "pinned": {}}}}}));
    for m in ["gateway/chat", "gateway/pinned", "gateway/other"] {
        assert_eq!(resolve(&c, m).provider.base_url, "http://127.0.0.1:17322");
    }
}

fn reasoning_body() -> Value {
    json!({"reasoningEffort": "high", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]})
}

// config/provider.test.ts: "keeps configured model variant bodies unchanged"
#[test]
fn oc_provider_keeps_configured_model_variant_bodies_unchanged() {
    let c = config(
        json!({"providers": {"opencode": {"protocol": "openai-responses",
        "settings": {"baseURL": "https://opencode.test/v1"},
        "models": {"alpha-gpt-next": {"variants": [{"id": "high", "body": reasoning_body()}]}}}}}),
    );
    let m = resolve(&c, "opencode/alpha-gpt-next#high");
    assert_eq!(Value::Object(m.body.clone()), reasoning_body());
    let (_, high) = m.variant_list.iter().find(|(id, _)| id == "high").unwrap();
    assert_eq!(Value::Object(high.body.clone()), reasoning_body());
}

// config/provider.test.ts: "keeps layered model variant bodies unchanged"
#[test]
fn oc_provider_keeps_layered_model_variant_bodies_unchanged() {
    let c = layered(
        json!({"providers": {"opencode": {"protocol": "openai-responses",
            "settings": {"baseURL": "https://opencode.test/v1"}}}}),
        json!({"providers": {"opencode": {"models": {"alpha-gpt-next": {
            "variants": [{"id": "high", "body": {"reasoningEffort": "high"}}]}}}}}),
    );
    let m = resolve(&c, "opencode/alpha-gpt-next#high");
    assert_eq!(Value::Object(m.body), json!({"reasoningEffort": "high"}));
}

// config/provider.test.ts: "loads configured providers and applies later
// model overrides" (adapted: no canonical providers, integrations or model
// transforms; what a model resolves to after two documents)
#[test]
fn oc_provider_loads_configured_providers_and_applies_later_model_overrides() {
    std::env::set_var("CS_BACKLOG3_CUSTOM_API_KEY", "secret");
    let c = layered(
        json!({"model": "custom/first", "providers": {"custom": {
            "name": "Configured", "protocol": "anthropic-messages",
            "env": ["CS_BACKLOG3_CUSTOM_API_KEY"],
            "headers": {"first": "first", "shared": "first"},
            "models": {"chat": {"name": "First",
                "compatibility": {"reasoningField": "vendor_reasoning", "maxTokensField": "max_completion_tokens", "requireFinishReason": false},
                "limit": {"context": 100, "output": 50},
                "cost": {"input": 1, "output": 2}}}}}}),
        json!({"model": "custom/default", "providers": {"custom": {
            "settings": {"baseURL": "https://example.test"},
            "headers": {"last": "last", "shared": "last"},
            "models": {"default": {"name": "Default"},
                "chat": {"id": "api-chat", "name": "Last", "limit": {"output": 75}}}}}}),
    );
    assert_eq!(c.model.as_deref(), Some("custom/default"));
    assert_eq!(
        c.provider("custom").unwrap().name.as_deref(),
        Some("Configured")
    );
    let m = resolve(&c, "custom/chat");
    assert_eq!(m.model_id, "api-chat");
    assert_eq!(m.provider.protocol, Protocol::AnthropicMessages);
    assert_eq!(m.provider.base_url, "https://example.test");
    assert_eq!(m.provider.api_key.as_deref(), Some("secret"));
    assert_eq!((m.context, m.max_output), (Some(100), Some(75)));
    assert_eq!(header(&m, "first"), Some("first"));
    assert_eq!(header(&m, "last"), Some("last"));
    assert_eq!(header(&m, "shared"), Some("last"));
    assert_eq!(
        m.provider.compat.reasoning_field.as_deref(),
        Some("vendor_reasoning")
    );
    assert_eq!(m.provider.compat.require_finish_reason, Some(false));
}

// model-resolver.test.ts: "constructs native Azure requests with deployment
// IDs and projected resource URLs" (adapted: one provider per config)
#[test]
fn oc_resolver_constructs_azure_requests_with_deployment_ids_and_resource_urls() {
    let azure = |settings: Value, protocol: Option<&str>| {
        let mut p = json!({"settings": settings});
        if let Some(proto) = protocol {
            p["protocol"] = json!(proto);
        }
        config(json!({"providers": {"azure": p}}))
    };
    let c = azure(
        json!({"resourceName": "modern-resource", "apiVersion": "2025-01-01-preview", "apiKey": "secret"}),
        None,
    );
    let responses = resolve(&c, "azure/responses-deployment");
    assert_eq!(responses.model_id, "responses-deployment");
    assert_eq!(responses.provider.protocol, Protocol::OpenaiResponses);
    assert_eq!(
        responses.provider.base_url,
        "https://modern-resource.openai.azure.com/openai/v1"
    );
    assert_eq!(
        responses.provider.query,
        [("api-version".to_string(), "2025-01-01-preview".to_string())]
    );
    let c = azure(
        json!({"resourceName": "modern-resource", "apiKey": "secret"}),
        Some("openai-chat"),
    );
    assert_eq!(
        resolve(&c, "azure/chat-deployment").provider.protocol,
        Protocol::OpenaiChat
    );
    let c = azure(
        json!({"resourceName": "modern-resource", "apiVersion": "2025-01-01-preview",
            "useDeploymentBasedUrls": true, "apiKey": "secret"}),
        None,
    );
    let deployment = resolve(&c, "azure/legacy-url-deployment");
    assert_eq!(
        deployment.provider.base_url,
        "https://modern-resource.openai.azure.com/openai/deployments/legacy-url-deployment"
    );
    assert_eq!(
        deployment.provider.query,
        [("api-version".to_string(), "2025-01-01-preview".to_string())]
    );
    // Any other host (an OpenAI-compatible endpoint) is used as given.
    let c = azure(
        json!({"baseURL": "https://legacy-resource.cognitiveservices.azure.com/openai", "apiKey": "secret"}),
        Some("openai-chat"),
    );
    let compatible = resolve(&c, "azure/legacy-deployment");
    assert_eq!(compatible.model_id, "legacy-deployment");
    assert_eq!(
        compatible.provider.base_url,
        "https://legacy-resource.cognitiveservices.azure.com/openai"
    );
}

/// A provider like OpenCode's test model: `api-test-model` sent for
/// `test-model`, with a header and a body extension.
fn test_provider(protocol: &str, extra: Value) -> Value {
    let mut p = json!({"protocol": protocol,
        "settings": {"baseURL": "https://openai.example/v1"},
        "headers": {"x-test": "header"},
        "body": {"custom_extension": {"enabled": true}},
        "models": {"test-model": {"id": "api-test-model"}}});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    p
}

// model-resolver.test.ts: "uses the API modelID instead of the catalog ID
// for native OpenAI routes"
#[test]
fn oc_resolver_uses_the_api_model_id_instead_of_the_catalog_id() {
    let c = config(
        json!({"providers": {"test-provider": test_provider("openai-responses", json!({}))}}),
    );
    let m = resolve(&c, "test-provider/test-model");
    assert_eq!(m.model_id, "api-test-model");
    assert_eq!(m.provider.protocol, Protocol::OpenaiResponses);
    assert_eq!(m.provider.base_url, "https://openai.example/v1");
    assert_eq!(header(&m, "x-test"), Some("header"));
    assert_eq!(
        Value::Object(m.body),
        json!({"custom_extension": {"enabled": true}})
    );
}

// model-resolver.test.ts: "treats an empty configured API key as omitted"
#[test]
fn oc_resolver_treats_an_empty_configured_api_key_as_omitted() {
    std::env::set_var("CS_BACKLOG3_OPENAI_KEY", "environment-key");
    let c = config(
        json!({"providers": {"keyed": {"protocol": "openai-responses",
        "env": ["CS_BACKLOG3_OPENAI_KEY"],
        "settings": {"baseURL": "https://openai.example/v1", "apiKey": ""}}}}),
    );
    let m = resolve(&c, "keyed/m");
    assert_eq!(m.provider.api_key.as_deref(), Some("environment-key"));
    // The protocol's own header: `Authorization: Bearer` for OpenAI.
    assert!(matches!(m.provider.auth, Auth::Default));
}

// model-resolver.test.ts: "uses merged API settings for OpenAI-compatible
// auth and request defaults"
#[test]
fn oc_resolver_uses_merged_api_settings_for_openai_compatible_auth_and_defaults() {
    let c = config(
        json!({"providers": {"deepseek-proxy": {"protocol": "openai-chat",
        "settings": {"baseURL": "https://compatible.example/v1", "apiKey": "settings-secret"},
        "models": {"m": {"compatibility": {"reasoningField": "vendor_reasoning", "requireReasoning": true,
            "maxTokensField": "max_completion_tokens", "requireFinishReason": false,
            "requireAssistantAfterTool": true}}}}}}),
    );
    let m = resolve(&c, "deepseek-proxy/m");
    assert_eq!(m.provider.api_key.as_deref(), Some("settings-secret"));
    // The protocol's own header: `Authorization: Bearer` for OpenAI.
    assert!(matches!(m.provider.auth, Auth::Default));
    assert_eq!(m.provider.protocol, Protocol::OpenaiChat);
    assert_eq!(m.provider.base_url, "https://compatible.example/v1");
    let compat = &m.provider.compat;
    assert_eq!(compat.reasoning_field.as_deref(), Some("vendor_reasoning"));
    assert_eq!(compat.require_reasoning, Some(true));
    assert_eq!(
        compat.max_tokens_field.as_deref(),
        Some("max_completion_tokens")
    );
    assert_eq!(compat.require_finish_reason, Some(false));
    assert_eq!(compat.require_assistant_after_tool, Some(true));
    assert!(m.body.is_empty());
}

// model-resolver.test.ts: "resolves provider URLs from environment without
// mutating the catalog model" (adapted: a configured URL takes a variable
// as `{env:NAME}`, CodeSmith's and OpenCode's config syntax)
#[test]
fn oc_resolver_resolves_provider_urls_from_the_environment() {
    std::env::set_var("CS_BACKLOG3_ACME_HOST", "api.acme.test");
    let c = layered(
        json!({}),
        json!({"providers": {"acme": {"protocol": "openai-chat",
            "settings": {"baseURL": "https://{env:CS_BACKLOG3_ACME_HOST}/v1", "apiKey": "k"}}}}),
    );
    assert_eq!(
        resolve(&c, "acme/m").provider.base_url,
        "https://api.acme.test/v1"
    );
}

// model-resolver.test.ts: "overlays selected OpenAI variant settings and
// bodies"
#[test]
fn oc_resolver_overlays_selected_openai_variant_settings_and_bodies() {
    let variant = json!({"xhigh": {
        "options": {"reasoningEffort": "xhigh", "reasoningSummary": "auto", "include": ["reasoning.encrypted_content"]},
        "headers": {"x-variant": "high"},
        "body": {"store": false, "service_tier": "priority", "temperature": 0.2}}});
    let mut p = test_provider(
        "openai-responses",
        json!({"settings": {"baseURL": "https://openai.example/v1", "apiKey": "secret"}}),
    );
    p["models"]["test-model"]["variants"] = variant;
    let c = config(json!({"providers": {"test-provider": p}}));
    let m = resolve(&c, "test-provider/test-model#xhigh");
    assert_eq!(m.variant.as_deref(), Some("xhigh"));
    assert_eq!(header(&m, "x-test"), Some("header"));
    assert_eq!(header(&m, "x-variant"), Some("high"));
    assert_eq!(
        Value::Object(m.body),
        json!({"custom_extension": {"enabled": true}, "store": false, "service_tier": "priority", "temperature": 0.2})
    );
    let options = serde_json::to_value(&m.options).unwrap();
    assert_eq!(options["reasoningEffort"], "xhigh");
    assert_eq!(options["reasoningSummary"], "auto");
    assert_eq!(options["include"], json!(["reasoning.encrypted_content"]));
}

// model-resolver.test.ts: "sends xAI Responses effort variants with
// reasoning summaries" (adapted: xAI on the Responses protocol, which
// CodeSmith uses for it when configured; the built-in xAI is OpenAI Chat)
#[test]
fn oc_resolver_sends_xai_responses_effort_variants_with_reasoning_summaries() {
    let c = config(json!({"providers": {"xai": {"protocol": "openai-responses",
        "settings": {"apiKey": "secret"}}}}));
    let m = resolve(&c, "xai/grok-4.6#high");
    assert_eq!(m.provider.protocol, Protocol::OpenaiResponses);
    let options = serde_json::to_value(&m.options).unwrap();
    assert_eq!(options["reasoningEffort"], "high");
    assert_eq!(options["reasoningSummary"], "auto");
    assert_eq!(options["include"], json!(["reasoning.encrypted_content"]));
}

// model-resolver.test.ts: "overlays selected OpenAI-compatible variant
// bodies"
#[test]
fn oc_resolver_overlays_selected_openai_compatible_variant_bodies() {
    let mut p = test_provider("openai-chat", json!({}));
    p["models"]["test-model"]["variants"] =
        json!({"high": {"body": {"store": false, "reasoning_effort": "high"}}});
    let c = config(json!({"providers": {"test-provider": p}}));
    let m = resolve(&c, "test-provider/test-model#high");
    assert_eq!(
        Value::Object(m.body),
        json!({"custom_extension": {"enabled": true}, "store": false, "reasoning_effort": "high"})
    );
}

// model-resolver.test.ts: "rejects an explicit unavailable variant during
// model resolution"
#[test]
fn oc_resolver_rejects_an_explicit_unavailable_variant() {
    let c = config(
        json!({"providers": {"test-provider": test_provider("openai-responses", json!({}))}}),
    );
    let e = c
        .resolve_model("test-provider/test-model#unknown")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("test-provider/test-model") && e.contains("no variant \"unknown\""),
        "{e}"
    );
}

// model-resolver.test.ts: "overlays selected Anthropic variant settings"
#[test]
fn oc_resolver_overlays_selected_anthropic_variant_settings() {
    let mut p = test_provider("anthropic-messages", json!({}));
    p["models"]["test-model"]["variants"] =
        json!({"high": {"options": {"thinking": {"type": "enabled", "budgetTokens": 12000}}}});
    let c = config(json!({"providers": {"test-provider": p}}));
    let m = resolve(&c, "test-provider/test-model#high");
    assert_eq!(
        Value::Object(m.body),
        json!({"custom_extension": {"enabled": true}})
    );
    assert_eq!(
        serde_json::to_value(&m.options).unwrap(),
        json!({"thinking": {"type": "enabled", "budgetTokens": 12000}})
    );
}

// model-resolver.test.ts: "uses resolved credentials for bearer auth"
#[test]
fn oc_resolver_uses_resolved_credentials_for_bearer_auth() {
    let c = config(
        json!({"providers": {"bearer-backlog3": {"protocol": "openai-responses",
        "settings": {"baseURL": "https://openai.example/v1"}}}}),
    );
    codesmith_core::auth::set_in(
        &codesmith_core::paths::auth_file(),
        "bearer-backlog3",
        "secret",
    )
    .unwrap();
    let m = resolve(&c, "bearer-backlog3/m");
    assert_eq!(m.provider.api_key.as_deref(), Some("secret"));
    // The protocol's own header: `Authorization: Bearer` for OpenAI.
    assert!(matches!(m.provider.auth, Auth::Default));
}

// model-resolver.test.ts: "lets Azure CLI requests past package auth for
// the Azure plugin to authorize" (adapted: an Azure CLI login sends no key;
// the requests are authorized with a token, see oc_azure.rs)
#[test]
fn oc_resolver_lets_azure_cli_requests_past_key_auth() {
    let c = config(json!({"providers": {"azure": {"settings": {"resourceName": "cli-resource"}}}}));
    codesmith_core::auth::set_entry_in(
        &codesmith_core::paths::auth_file(),
        "azure",
        json!({"type": "external", "method": "azure-cli", "metadata": {"resourceName": "cli-resource"}}),
    )
    .unwrap();
    let m = match c.resolve_model("azure/gpt-5") {
        Ok(m) => m,
        // The Azure CLI is not installed here.
        Err(e) if e.to_string().to_lowercase().contains("azure cli") => return,
        Err(e) => panic!("{e:#}"),
    };
    assert_eq!(
        m.provider.base_url,
        "https://cli-resource.openai.azure.com/openai/v1"
    );
    assert_eq!(m.provider.api_key, None);
    assert!(matches!(m.provider.auth, Auth::Dynamic(_)));
}
