//! Provider plugins (OpenCode's `plugin/provider-*.test.ts` and the
//! provider routes of `model-resolver.test.ts`), driven through
//! `codesmith_core::providers` with a fixed environment, and through
//! `Config::resolve_model` with the bundled catalog.

use codesmith_ai::{Auth, Protocol};
use codesmith_core::config::{Config, ProviderSettings};
use codesmith_core::providers::{self, bedrock, Env, Input, Settings};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn env(pairs: &[(&str, &str)]) -> Env {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Arc::new(move |k| m.get(k).cloned())
}

fn settings(v: Value) -> ProviderSettings {
    serde_json::from_value(v).unwrap()
}

struct Case {
    provider: &'static str,
    settings: ProviderSettings,
    package: Option<&'static str>,
    catalog_url: Option<&'static str>,
    model: &'static str,
    key: Option<&'static str>,
    stored: Option<Value>,
    env: Env,
}

impl Case {
    fn new(provider: &'static str, s: Value) -> Case {
        Case {
            provider,
            settings: settings(s),
            package: None,
            catalog_url: None,
            model: "m",
            key: None,
            stored: None,
            env: env(&[]),
        }
    }

    fn route(&self) -> anyhow::Result<Option<providers::Route>> {
        providers::route(&Input {
            provider_id: self.provider,
            settings: &self.settings,
            configured_url: self.settings.base_url.as_deref(),
            configured_protocol: None,
            package: self.package,
            catalog_url: self.catalog_url,
            model_id: self.model,
            api_key: self.key.map(str::to_string),
            stored: self.stored.clone(),
            env: self.env.clone(),
        })
    }

    fn ok(&self) -> providers::Route {
        self.route().unwrap().expect("routed")
    }
}

fn sigv4(r: &providers::Route) -> String {
    match &r.auth {
        Auth::Dynamic(a) => a.describe(),
        a => panic!("not SigV4: {a:?}"),
    }
}

fn bundled(config: Value) -> Config {
    let mut c: Config = serde_json::from_value(config).unwrap();
    c.models_dev = Some(Arc::new(codesmith_core::catalog::ModelsDev::fixed(
        (*codesmith_core::catalog::bundled()).clone(),
    )));
    c
}

// plugin/provider-amazon-bedrock.test.ts

#[test]
fn oc_bedrock_moves_endpoint_setting_to_base_url() {
    let r = Case::new(
        "amazon-bedrock",
        json!({"endpoint": "https://bedrock.example"}),
    )
    .ok();
    assert_eq!(r.base_url, "https://bedrock.example");
    assert_eq!(r.protocol, Protocol::BedrockConverse);
    assert!(sigv4(&r).contains("in us-east-1"), "{}", sigv4(&r));
}

#[test]
fn oc_bedrock_gives_the_package_the_profile_of_a_stored_aws_profile_login() {
    let mut c = Case::new("amazon-bedrock", json!({}));
    c.stored =
        Some(json!({"type": "external", "method": "aws-profile", "metadata": {"profile": "work"}}));
    // The saved profile signs, even beside an ambient bearer token.
    c.key = Some("ambient-bearer");
    let r = c.ok();
    assert_eq!(
        sigv4(&r),
        "SigV4 for bedrock in us-east-1 from AWS profile work"
    );
    assert_eq!(r.api_key, None);
    assert!(bedrock::available(
        &Settings::default(),
        c.stored.as_ref(),
        &env(&[])
    ));
}

#[test]
fn oc_bedrock_keeps_an_explicit_base_url_over_endpoint() {
    let r = Case::new(
        "amazon-bedrock",
        json!({"baseURL": "https://base.example", "endpoint": "https://endpoint.example"}),
    )
    .ok();
    assert_eq!(r.base_url, "https://base.example");
}

#[test]
fn oc_bedrock_only_treats_the_bearer_token_env_var_as_a_key_credential() {
    let p = providers::builtin("amazon-bedrock").unwrap();
    assert_eq!(p.env, ["AWS_BEARER_TOKEN_BEDROCK"]);
    // A key (from that variable or saved) is a bearer token.
    let mut c = Case::new("amazon-bedrock", json!({}));
    c.key = Some("bedrock-key");
    let r = c.ok();
    assert_eq!(r.auth, Auth::Bearer);
    assert_eq!(r.api_key.as_deref(), Some("bedrock-key"));
}

#[test]
fn oc_bedrock_leaves_activation_on_auto_without_ambient_aws_configuration() {
    assert!(!bedrock::available(
        &Settings::default(),
        None,
        &env(&[
            ("AWS_REGION", "eu-west-1"),
            ("AWS_DEFAULT_REGION", "us-west-2")
        ])
    ));
}

#[test]
fn oc_bedrock_enables_the_provider_when_an_ambient_variable_is_set() {
    for name in [
        "AWS_PROFILE",
        "AWS_ACCESS_KEY_ID",
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    ] {
        assert!(
            bedrock::available(&Settings::default(), None, &env(&[(name, "value")])),
            "{name}"
        );
    }
}

#[test]
fn oc_bedrock_enables_the_provider_when_a_profile_is_configured() {
    let s: ProviderSettings = settings(json!({"profile": "work"}));
    assert!(bedrock::available(&s.plugin, None, &env(&[])));
    let r = Case::new("amazon-bedrock", json!({"profile": "work"})).ok();
    assert_eq!(
        sigv4(&r),
        "SigV4 for bedrock in us-east-1 from AWS profile work"
    );
}

#[test]
fn oc_bedrock_fills_region_from_aws_region_then_aws_default_region_without_overriding_config() {
    let mut c = Case::new("amazon-bedrock", json!({}));
    c.env = env(&[
        ("AWS_REGION", "eu-west-1"),
        ("AWS_DEFAULT_REGION", "us-west-2"),
    ]);
    assert_eq!(
        c.ok().base_url,
        "https://bedrock-runtime.eu-west-1.amazonaws.com"
    );
    c.settings = settings(json!({"region": "ap-southeast-2"}));
    let r = c.ok();
    assert_eq!(
        r.base_url,
        "https://bedrock-runtime.ap-southeast-2.amazonaws.com"
    );
    assert!(sigv4(&r).contains("in ap-southeast-2"));
}

#[test]
fn oc_bedrock_falls_back_to_aws_default_region_then_us_east_1() {
    let mut c = Case::new("amazon-bedrock", json!({}));
    c.env = env(&[("AWS_DEFAULT_REGION", "us-west-2")]);
    assert_eq!(
        c.ok().base_url,
        "https://bedrock-runtime.us-west-2.amazonaws.com"
    );
    c.env = env(&[]);
    assert_eq!(
        c.ok().base_url,
        "https://bedrock-runtime.us-east-1.amazonaws.com"
    );
}

#[test]
fn oc_bedrock_applies_to_mantle_and_native_bedrock_packages() {
    let mut c = Case::new("mantle", json!({}));
    c.package = Some("@opencode/ai/providers/amazon-bedrock/mantle/responses");
    let r = c.ok();
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert!(sigv4(&r).starts_with("SigV4 for bedrock-mantle"));
    c.provider = "native";
    c.package = Some("@opencode/ai/providers/amazon-bedrock");
    assert_eq!(c.ok().protocol, Protocol::BedrockConverse);
    c.provider = "other";
    c.package = Some("@opencode/ai/providers/anthropic");
    assert!(c.route().unwrap().is_none());
}

#[test]
fn oc_bedrock_auth_modes() {
    let mut c = Case::new("amazon-bedrock", json!({"auth": "sigv4"}));
    c.key = Some("ambient");
    assert!(matches!(c.ok().auth, Auth::Dynamic(_)));
    c.settings = settings(json!({"auth": "sigv4", "apiKey": "k"}));
    assert!(c
        .route()
        .unwrap_err()
        .to_string()
        .contains("SigV4 auth does not accept apiKey"));
    c.settings = settings(json!({"auth": "bearer"}));
    c.key = None;
    assert!(c
        .route()
        .unwrap_err()
        .to_string()
        .contains("bearer auth requires apiKey"));
}

// model-resolver.test.ts: Bedrock Mantle routes

#[test]
fn oc_resolver_maps_bedrock_mantle_chat_and_responses_packages() {
    let mut c = Case::new("amazon-bedrock", json!({"region": "us-east-2"}));
    c.key = Some("secret");
    c.package = Some("@opencode/ai/providers/amazon-bedrock/mantle/responses");
    let r = c.ok();
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert_eq!(r.base_url, "https://bedrock-mantle.us-east-2.api.aws/v1");
    c.package = Some("@opencode/ai/providers/amazon-bedrock/mantle/chat");
    let r = c.ok();
    assert_eq!(r.protocol, Protocol::OpenaiChat);
    assert_eq!(r.base_url, "https://bedrock-mantle.us-east-2.api.aws/v1");
    c.package = Some("@opencode/ai/providers/amazon-bedrock/mantle/messages");
    assert_eq!(
        c.ok().base_url,
        "https://bedrock-mantle.us-east-2.api.aws/anthropic/v1"
    );
}

#[test]
fn oc_resolver_resolves_bedrock_mantle_catalog_endpoints_from_the_configured_region() {
    let mut c = Case::new("amazon-bedrock", json!({"region": "us-west-2"}));
    c.package = Some("@opencode/ai/providers/amazon-bedrock/mantle/responses");
    c.catalog_url = Some("https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1");
    assert_eq!(
        c.ok().base_url,
        "https://bedrock-mantle.us-west-2.api.aws/openai/v1"
    );
}

#[test]
fn oc_resolver_fills_mantle_catalog_urls_from_the_provider_region() {
    let mut c = Case::new("amazon-bedrock", json!({}));
    c.package = Some("@opencode/ai/providers/amazon-bedrock/mantle/responses");
    c.catalog_url = Some("https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1");
    c.env = env(&[("AWS_DEFAULT_REGION", "us-west-2")]);
    assert_eq!(
        c.ok().base_url,
        "https://bedrock-mantle.us-west-2.api.aws/openai/v1"
    );
}

#[test]
fn oc_resolver_prefers_the_configured_mantle_region_over_the_environment() {
    let mut c = Case::new("amazon-bedrock", json!({"region": "us-west-2"}));
    c.package = Some("@opencode/ai/providers/amazon-bedrock/mantle/responses");
    c.catalog_url = Some("https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1");
    c.env = env(&[("AWS_REGION", "us-east-1")]);
    assert_eq!(
        c.ok().base_url,
        "https://bedrock-mantle.us-west-2.api.aws/openai/v1"
    );
}

#[test]
fn bedrock_catalog_models_resolve_with_their_metadata() {
    let c = bundled(
        json!({"providers": {"amazon-bedrock": {"settings": {"region": "eu-west-1", "auth": "sigv4"}}}}),
    );
    let m = c
        .resolve_model("amazon-bedrock/us.anthropic.claude-opus-4-7")
        .unwrap();
    assert_eq!(m.provider.protocol, Protocol::BedrockConverse);
    assert_eq!(
        m.provider.base_url,
        "https://bedrock-runtime.eu-west-1.amazonaws.com"
    );
    assert!(matches!(m.provider.auth, Auth::Dynamic(_)));
    assert_eq!(m.context, Some(1_000_000));
    assert!(!m.variants.is_empty(), "catalog reasoning variants");
    // A Mantle-only model takes its Mantle API and catalog URL.
    let m = c.resolve_model("amazon-bedrock/openai.gpt-5.5").unwrap();
    assert_eq!(m.provider.protocol, Protocol::OpenaiResponses);
    assert_eq!(
        m.provider.base_url,
        "https://bedrock-mantle.eu-west-1.api.aws/openai/v1"
    );
}

// plugin/provider-google-vertex.test.ts

use codesmith_core::providers::vertex;

const VERTEX_CHAT: &str = "@opencode/ai/providers/google-vertex/chat";
const VERTEX_TEMPLATE: &str =
    "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}";

#[test]
fn oc_vertex_ignores_openai_compatible_providers_that_are_not_google_vertex() {
    let mut c = Case::new("opencode", json!({"baseURL": "https://opencode.ai/zen/v1"}));
    c.package = Some("@opencode/ai/providers/openai-compatible");
    assert!(c.route().unwrap().is_none());
}

#[test]
fn oc_vertex_resolves_project_and_location_from_env_using_legacy_precedence() {
    let mut c = Case::new("google-vertex", json!({}));
    c.package = Some(VERTEX_CHAT);
    c.catalog_url = Some(VERTEX_TEMPLATE);
    c.env = env(&[
        ("GOOGLE_CLOUD_PROJECT", "google-cloud-project"),
        ("GCP_PROJECT", "gcp-project"),
        ("GCLOUD_PROJECT", "gcloud-project"),
        ("GOOGLE_VERTEX_LOCATION", "google-vertex-location"),
        ("GOOGLE_CLOUD_LOCATION", "google-cloud-location"),
        ("VERTEX_LOCATION", "vertex-location"),
    ]);
    let r = c.ok();
    assert_eq!(r.protocol, Protocol::OpenaiChat);
    assert_eq!(
        r.base_url,
        "https://google-vertex-location-aiplatform.googleapis.com/v1/projects/google-cloud-project/locations/google-vertex-location"
    );
}

#[test]
fn oc_vertex_enables_the_provider_when_a_project_resolves_and_leaves_it_automatic_otherwise() {
    let s = Settings::default();
    assert!(!vertex::available(&s, None, &env(&[])));
    let mut c = Case::new("google-vertex", json!({}));
    c.package = Some("@opencode/ai/providers/google-vertex");
    let e = c.route().unwrap_err().to_string();
    assert!(e.contains("Google Vertex requires a project"), "{e}");
}

#[test]
fn oc_vertex_enables_the_provider_when_a_project_resolves_from_env() {
    assert!(vertex::available(
        &Settings::default(),
        None,
        &env(&[("GOOGLE_CLOUD_PROJECT", "adc-project")])
    ));
    let login = json!({"type": "external", "method": "google-adc", "metadata": {"project": "saved", "location": "eu"}});
    assert!(vertex::available(
        &Settings::default(),
        Some(&login),
        &env(&[])
    ));
}

#[test]
fn oc_vertex_resolves_the_advertised_google_vertex_project_env() {
    let mut c = Case::new("google-vertex", json!({}));
    c.package = Some(VERTEX_CHAT);
    c.catalog_url = Some(VERTEX_TEMPLATE);
    c.env = env(&[
        ("GOOGLE_VERTEX_PROJECT", "vertex-project"),
        ("GOOGLE_VERTEX_LOCATION", "europe-west4"),
    ]);
    assert_eq!(
        c.ok().base_url,
        "https://europe-west4-aiplatform.googleapis.com/v1/projects/vertex-project/locations/europe-west4"
    );
}

#[test]
fn oc_vertex_keeps_configured_project_and_location_over_env_and_uses_global_endpoint() {
    let mut c = Case::new(
        "google-vertex",
        json!({"project": "config-project", "location": "global"}),
    );
    c.package = Some(VERTEX_CHAT);
    c.catalog_url = Some(VERTEX_TEMPLATE);
    c.env = env(&[
        ("GOOGLE_CLOUD_PROJECT", "env-project"),
        ("GOOGLE_VERTEX_LOCATION", "env-location"),
    ]);
    assert_eq!(
        c.ok().base_url,
        "https://aiplatform.googleapis.com/v1/projects/config-project/locations/global"
    );
}

#[test]
fn oc_vertex_expands_endpoint_templates_to_the_multi_region_host() {
    for location in ["eu", "us"] {
        let mut c = Case::new(
            "google-vertex",
            json!({"project": "config-project", "location": location}),
        );
        c.package = Some(VERTEX_CHAT);
        c.catalog_url = Some(VERTEX_TEMPLATE);
        assert_eq!(
            c.ok().base_url,
            format!("https://aiplatform.{location}.rep.googleapis.com/v1/projects/config-project/locations/{location}")
        );
    }
}

#[test]
fn oc_vertex_expands_endpoint_templates_on_vertex_chat_models() {
    let mut c = Case::new(
        "google-vertex",
        json!({"project": "config-project", "location": "eu"}),
    );
    c.model = "meta/llama-maas";
    c.package = Some(VERTEX_CHAT);
    c.catalog_url = Some("https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/endpoints/openapi");
    assert_eq!(
        c.ok().base_url,
        "https://aiplatform.eu.rep.googleapis.com/v1/projects/config-project/locations/eu/endpoints/openapi"
    );
}

#[test]
fn oc_vertex_defaults_location_to_global_when_only_project_is_configured() {
    let mut c = Case::new("google-vertex", json!({"project": "config-project"}));
    c.model = "gemini-3-pro";
    let r = c.ok();
    assert_eq!(r.protocol, Protocol::GoogleGemini);
    assert_eq!(
        r.base_url,
        "https://aiplatform.googleapis.com/v1beta1/projects/config-project/locations/global/publishers/google"
    );
    assert!(matches!(r.auth, Auth::Dynamic(_)));
}

#[test]
fn oc_resolver_routes_vertex_anthropic_catalog_models_through_native_messages() {
    let mut c = Case::new(
        "google-vertex",
        json!({"project": "vertex-project", "location": "eu", "accessToken": "vertex-token"}),
    );
    c.model = "claude-sonnet-4-6";
    c.package = Some("@opencode/ai/providers/google-vertex/messages");
    let r = c.ok();
    assert_eq!(r.protocol, Protocol::AnthropicMessages);
    assert_eq!(
        r.base_url,
        "https://aiplatform.eu.rep.googleapis.com/v1/projects/vertex-project/locations/eu/publishers/anthropic/models"
    );
    assert_eq!(
        (r.auth, r.api_key.as_deref()),
        (Auth::Bearer, Some("vertex-token"))
    );
}

#[test]
fn vertex_express_mode_keys_and_the_saved_login() {
    // An Express Mode key serves Gemini without a project.
    let mut c = Case::new("google-vertex", json!({}));
    c.model = "gemini-3-pro";
    c.key = Some("express-key");
    let r = c.ok();
    assert_eq!(
        r.base_url,
        "https://aiplatform.googleapis.com/v1/publishers/google"
    );
    assert_eq!(
        (r.auth, r.api_key.as_deref()),
        (Auth::Default, Some("express-key"))
    );
    // A configured project selects Google credentials over the ambient key.
    c.settings = settings(json!({"project": "p"}));
    assert!(matches!(c.ok().auth, Auth::Dynamic(_)));
    // A saved login gives the project and location; Claude on the older id.
    let mut c = Case::new("google-vertex-anthropic", json!({}));
    c.model = "claude-opus-4-7@default";
    c.stored = Some(
        json!({"type": "external", "method": "google-adc", "metadata": {"project": "saved", "location": "us-east5"}}),
    );
    let r = c.ok();
    assert_eq!(r.protocol, Protocol::AnthropicMessages);
    assert_eq!(
        r.base_url,
        "https://us-east5-aiplatform.googleapis.com/v1/projects/saved/locations/us-east5/publishers/anthropic/models"
    );
}

#[test]
fn vertex_catalog_models_send_vertex_requests() {
    let c = bundled(
        json!({"providers": {"google-vertex": {"settings": {"project": "proj", "location": "us-east5", "accessToken": "tok"}}}}),
    );
    let m = c
        .resolve_model("google-vertex/claude-opus-4-7@default")
        .unwrap();
    assert_eq!(m.provider.protocol, Protocol::AnthropicMessages);
    let req = codesmith_ai::Request {
        model: m.model_id.clone(),
        messages: vec![codesmith_ai::Message::user("hi")],
        ..Default::default()
    };
    let http = m.provider.prepare(&reqwest::Client::new(), &req).unwrap();
    assert_eq!(
        http.url().as_str(),
        "https://us-east5-aiplatform.googleapis.com/v1/projects/proj/locations/us-east5/publishers/anthropic/models/claude-opus-4-7@default:streamRawPredict"
    );
    assert_eq!(http.headers()["authorization"], "Bearer tok");
    assert!(http.headers().get("x-api-key").is_none());
    let body: Value = serde_json::from_slice(http.body().unwrap().as_bytes().unwrap()).unwrap();
    assert!(body.get("model").is_none());
    assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
    let g = c
        .resolve_model("google-vertex/gemini-robotics-er-2-preview")
        .unwrap();
    assert_eq!(g.provider.protocol, Protocol::GoogleGemini);
    let req = codesmith_ai::Request {
        model: g.model_id.clone(),
        messages: vec![codesmith_ai::Message::user("hi")],
        ..Default::default()
    };
    let http = g.provider.prepare(&reqwest::Client::new(), &req).unwrap();
    assert_eq!(
        http.url().as_str(),
        "https://us-east5-aiplatform.googleapis.com/v1beta1/projects/proj/locations/us-east5/publishers/google/models/gemini-robotics-er-2-preview:streamGenerateContent?alt=sse"
    );
    assert_eq!(http.headers()["authorization"], "Bearer tok");
}
