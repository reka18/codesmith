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

// plugin/provider-github-copilot.test.ts

use codesmith_core::providers::copilot;
use std::sync::Mutex;

/// A local server answering each path with a JSON body, recording the
/// requests (head and body) it got.
struct Fixture {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

async fn fixture(routes: Vec<(&'static str, Vec<(u16, String)>)>) -> Fixture {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let routes = Arc::new(Mutex::new(routes));
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut s, _)) = l.accept().await {
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 65536];
            loop {
                let n = s.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len = text[..i]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            log.lock().unwrap().push(text.clone());
            let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
            let (status, body) = {
                let mut r = routes.lock().unwrap();
                match r.iter_mut().find(|(p, _)| path.starts_with(p)) {
                    Some((_, answers)) if answers.len() > 1 => answers.remove(0),
                    Some((_, answers)) => answers[0].clone(),
                    None => (404, "{}".to_string()),
                }
            };
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    Fixture { url, seen }
}

fn copilot_case(stored: Value) -> Case {
    let mut c = Case::new("github-copilot", json!({}));
    c.stored = Some(stored);
    c
}

fn login(token: &str, metadata: Value) -> Value {
    json!({"type": "oauth", "method": "device", "access": token, "refresh": token, "expires": 0, "metadata": metadata})
}

#[test]
fn oc_copilot_prefers_the_account_specific_copilot_api_endpoint() {
    let c = copilot_case(login(
        "gho_x",
        json!({"apiEndpoint": "https://api.individual.githubcopilot.com/"}),
    ));
    assert_eq!(c.ok().base_url, "https://api.individual.githubcopilot.com");
}

#[test]
fn oc_copilot_rejects_accounts_without_copilot_chat_access() {
    assert!(copilot::entitlement_error(
        &json!({"chat_enabled": false, "can_signup_for_limited": true})
    )
    .unwrap()
    .contains("not signed up for GitHub Copilot"));
    assert!(copilot::entitlement_error(&json!({"chat_enabled": false}))
        .unwrap()
        .contains("does not have GitHub Copilot access"));
}

#[test]
fn oc_copilot_only_blocks_on_an_explicit_entitlement_denial() {
    assert_eq!(copilot::entitlement_error(&json!({})), None);
    assert_eq!(
        copilot::entitlement_error(&json!({"chat_enabled": true})),
        None
    );
    assert_eq!(copilot::entitlement_error(&Value::Null), None);
}

#[test]
fn oc_copilot_serves_an_enterprise_login_from_its_own_host_until_the_first_model_list_arrives() {
    let r = copilot_case(login("gho_x", json!({"enterpriseUrl": "company.ghe.com"}))).ok();
    assert_eq!(r.base_url, "https://copilot-api.company.ghe.com");
    assert_eq!(
        copilot::user_url(Some("https://company.ghe.com/")),
        "https://api.company.ghe.com/copilot_internal/user"
    );
}

#[test]
fn oc_copilot_serves_a_login_from_the_endpoint_saved_with_it_until_the_first_model_list_arrives() {
    let mut c = copilot_case(login(
        "gho_x",
        json!({"apiEndpoint": "https://api.business.githubcopilot.com"}),
    ));
    c.model = "claude-opus-4.8";
    c.package = Some("@opencode/ai/providers/anthropic");
    let r = c.ok();
    assert_eq!(r.protocol, Protocol::AnthropicMessages);
    assert_eq!(r.base_url, "https://api.business.githubcopilot.com/v1");
}

#[test]
fn oc_copilot_removes_the_generic_key_method_and_reads_copilot_github_token_instead_of_github_token(
) {
    assert_eq!(
        providers::builtin("github-copilot").unwrap().env,
        ["COPILOT_GITHUB_TOKEN"]
    );
    // An environment token serves without a login.
    let mut c = Case::new("github-copilot", json!({}));
    c.key = Some("env-token");
    assert_eq!(c.ok().base_url, "https://api.githubcopilot.com");
    let e = Case::new("github-copilot", json!({}))
        .route()
        .unwrap_err()
        .to_string();
    assert!(e.contains("codesmith auth login github-copilot"), "{e}");
}

#[test]
fn oc_copilot_ignores_other_providers_model_requests() {
    assert!(Case::new("openai", json!({})).route().unwrap().is_none());
}

#[test]
fn oc_copilot_routes_models_dev_fallback_models_to_native_copilot_apis() {
    use copilot::fallback_protocol as f;
    assert_eq!(f("gpt-5.4"), Protocol::OpenaiResponses);
    assert_eq!(f("gpt-6-sol"), Protocol::OpenaiResponses);
    assert_eq!(f("gpt-5-mini"), Protocol::OpenaiChat);
    assert_eq!(f("gpt-4.1"), Protocol::OpenaiChat);
    assert_eq!(f("grok-code-fast-1"), Protocol::OpenaiResponses);
    assert_eq!(f("mai-code-1"), Protocol::OpenaiResponses);
    assert_eq!(f("gemini-3-pro"), Protocol::OpenaiChat);
    // With a catalog package, the package decides.
    let mut c = copilot_case(login("t", json!({})));
    c.model = "gpt-6-sol";
    c.package = Some("@opencode/ai/providers/openai/responses");
    assert_eq!(c.ok().protocol, Protocol::OpenaiResponses);
    c.package = Some("@opencode/ai/providers/openai-compatible");
    assert_eq!(c.ok().protocol, Protocol::OpenaiChat);
}

#[test]
fn oc_copilot_disables_gpt_5_chat_latest_before_copilot_language_selection() {
    assert!(copilot::hidden("gpt-5-chat-latest"));
    assert!(!copilot::hidden("gpt-5"));
}

#[test]
fn copilot_catalog_models_take_the_catalog_route_on_the_account_root() {
    let c = bundled(json!({}));
    let route = |model: &str| {
        let m = c.resolve_model(&format!("github-copilot/{model}"));
        m.map(|m| (m.provider.protocol, m.provider.base_url))
    };
    // Resolving needs a login or COPILOT_GITHUB_TOKEN; without one it says so.
    if std::env::var("COPILOT_GITHUB_TOKEN").is_err()
        && codesmith_core::auth::entry("github-copilot").is_none()
    {
        assert!(route("claude-opus-4.8")
            .unwrap_err()
            .to_string()
            .contains("auth login github-copilot"));
        return;
    }
    let (p, url) = route("claude-opus-4.8").unwrap();
    assert_eq!(p, Protocol::AnthropicMessages);
    assert!(url.ends_with("/v1"), "{url}");
}

async fn step(
    f: &Fixture,
    protocol: Protocol,
    req: codesmith_ai::Request,
    stored: Value,
) -> String {
    let mut c = copilot_case(stored);
    c.model = "m";
    let mut r = c.ok();
    r.protocol = protocol;
    let base = match protocol {
        Protocol::AnthropicMessages => format!("{}/v1", f.url),
        _ => f.url.clone(),
    };
    let p = codesmith_ai::Provider {
        protocol,
        name: "github-copilot".into(),
        base_url: base,
        auth: r.auth,
        api_key: Some("not-sent".into()),
        ..Default::default()
    };
    let _ = p.stream(&reqwest::Client::new(), &req).await;
    let seen = f.seen.lock().unwrap();
    seen.last().unwrap().clone()
}

fn user_step(text: &str) -> codesmith_ai::Request {
    codesmith_ai::Request {
        model: "gpt-5.4".into(),
        messages: vec![codesmith_ai::Message::user(text)],
        prompt_cache_key: Some("ses_42".into()),
        ..Default::default()
    }
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_string())
    })
}

#[tokio::test]
async fn oc_copilot_adds_copilot_authentication_and_request_metadata_headers_to_openai_requests() {
    let f = fixture(vec![]).await;
    // A configured endpoint, so no account lookup runs.
    let mut stored = login("gho_token", json!({}));
    stored["metadata"]["apiEndpoint"] = f.url.clone().into();
    let head = step(&f, Protocol::OpenaiResponses, user_step("hi"), stored).await;
    assert!(head.starts_with("POST /responses"), "{head}");
    assert_eq!(
        header(&head, "authorization").as_deref(),
        Some("Bearer gho_token")
    );
    assert_eq!(
        header(&head, "openai-intent").as_deref(),
        Some("conversation-edits")
    );
    assert_eq!(
        header(&head, "x-github-api-version").as_deref(),
        Some("2026-08-01")
    );
    assert_eq!(header(&head, "x-initiator").as_deref(), Some("user"));
    assert_eq!(
        header(&head, "x-interaction-type").as_deref(),
        Some("conversation-agent")
    );
    assert_eq!(header(&head, "x-interaction-id").as_deref(), Some("ses_42"));
    assert!(header(&head, "user-agent")
        .unwrap()
        .starts_with("codesmith/"));
    assert!(!head.contains("not-sent"));
}

#[tokio::test]
async fn oc_copilot_adds_copilot_authentication_to_native_anthropic_requests() {
    let f = fixture(vec![]).await;
    let mut stored = login("gho_token", json!({}));
    stored["metadata"]["apiEndpoint"] = f.url.clone().into();
    let mut req = user_step("hi");
    req.model = "claude-opus-4.8".into();
    let head = step(&f, Protocol::AnthropicMessages, req, stored).await;
    assert!(head.starts_with("POST /v1/messages"), "{head}");
    assert_eq!(
        header(&head, "authorization").as_deref(),
        Some("Bearer gho_token")
    );
    assert_eq!(header(&head, "x-api-key"), None);
}

#[tokio::test]
async fn oc_copilot_classifies_steps_by_kind() {
    use codesmith_ai::Interaction;
    let f = fixture(vec![]).await;
    let mut stored = login("gho_token", json!({}));
    stored["metadata"]["apiEndpoint"] = f.url.clone().into();
    for (kind, name) in [
        (Interaction::Agent, "conversation-agent"),
        (Interaction::Subagent, "conversation-subagent"),
        (Interaction::Background, "conversation-background"),
        (Interaction::Compaction, "conversation-compaction"),
    ] {
        let mut req = user_step("hi");
        req.interaction = kind;
        let head = step(&f, Protocol::OpenaiChat, req, stored.clone()).await;
        assert_eq!(header(&head, "x-interaction-type").as_deref(), Some(name));
        // A declared agent step stays agent-initiated whatever its body says.
        let initiator = if kind == Interaction::Agent {
            "user"
        } else {
            "agent"
        };
        assert_eq!(
            header(&head, "x-initiator").as_deref(),
            Some(initiator),
            "{name}"
        );
    }
}

#[tokio::test]
async fn oc_copilot_flags_images_returned_by_tools_in_responses_requests() {
    let f = fixture(vec![]).await;
    let mut stored = login("gho_token", json!({}));
    stored["metadata"]["apiEndpoint"] = f.url.clone().into();
    let mut req = user_step("look");
    req.messages.push(codesmith_ai::Message {
        role: codesmith_ai::Role::Assistant,
        parts: vec![codesmith_ai::Part::tool_call("c1", "read", json!({}))],
    });
    req.messages.push(codesmith_ai::Message {
        role: codesmith_ai::Role::User,
        parts: vec![codesmith_ai::Part::ToolResult {
            id: "c1".into(),
            name: "read".into(),
            content: "image".into(),
            is_error: false,
            media: vec![codesmith_ai::Media::base64("image/png", "aGk=")],
        }],
    });
    let head = step(&f, Protocol::OpenaiResponses, req.clone(), stored.clone()).await;
    assert_eq!(
        header(&head, "copilot-vision-request").as_deref(),
        Some("true")
    );
    assert_eq!(header(&head, "x-initiator").as_deref(), Some("agent"));
    // Chat moves the tool image into a user message after the tool message;
    // that is still the agent's turn.
    let head = step(&f, Protocol::OpenaiChat, req, stored).await;
    assert_eq!(
        header(&head, "copilot-vision-request").as_deref(),
        Some("true")
    );
    assert_eq!(header(&head, "x-initiator").as_deref(), Some("agent"));
}

#[tokio::test]
async fn oc_copilot_refreshes_the_account_api_endpoint_instead_of_keeping_the_one_stored_at_login()
{
    let f = fixture(vec![]).await;
    let account = fixture(vec![(
        "/copilot_internal/user",
        vec![(
            200,
            json!({"endpoints": {"api": format!("{}/", f.url)}}).to_string(),
        )],
    )])
    .await;
    let authorizer = codesmith_ai::copilot::Copilot::new(
        "gho_token".into(),
        "https://api.githubcopilot.com".into(),
        Some(format!("{}/copilot_internal/user", account.url)),
    );
    let p = codesmith_ai::Provider {
        protocol: Protocol::OpenaiChat,
        name: "github-copilot".into(),
        base_url: "https://api.githubcopilot.com".into(),
        auth: Auth::Dynamic(Arc::new(authorizer)),
        ..Default::default()
    };
    let _ = p.stream(&reqwest::Client::new(), &user_step("hi")).await;
    let _ = p.stream(&reqwest::Client::new(), &user_step("again")).await;
    assert_eq!(
        f.seen.lock().unwrap().len(),
        2,
        "both steps reach the account endpoint"
    );
    let lookups = account.seen.lock().unwrap();
    assert_eq!(lookups.len(), 1, "looked up once");
    assert_eq!(
        header(&lookups[0], "authorization").as_deref(),
        Some("Bearer gho_token")
    );
    assert_eq!(
        header(&lookups[0], "x-github-api-version").as_deref(),
        Some("2025-04-01")
    );
}

#[tokio::test]
async fn oc_copilot_keeps_the_stored_endpoint_when_the_account_lookup_fails() {
    let f = fixture(vec![]).await;
    let account = fixture(vec![("/copilot_internal/user", vec![(500, "{}".into())])]).await;
    let authorizer = codesmith_ai::copilot::Copilot::new(
        "gho_token".into(),
        f.url.clone(),
        Some(format!("{}/copilot_internal/user", account.url)),
    );
    let p = codesmith_ai::Provider {
        protocol: Protocol::OpenaiChat,
        name: "github-copilot".into(),
        base_url: f.url.clone(),
        auth: Auth::Dynamic(Arc::new(authorizer)),
        ..Default::default()
    };
    let _ = p.stream(&reqwest::Client::new(), &user_step("hi")).await;
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn oc_copilot_registers_github_copilot_device_oauth() {
    let f = fixture(vec![
        (
            "/login/device/code",
            vec![(200, json!({"verification_uri": "https://github.com/login/device", "user_code": "ABCD-1234", "device_code": "dev-code", "interval": 1}).to_string())],
        ),
        (
            "/login/oauth/access_token",
            vec![
                (200, json!({"error": "authorization_pending"}).to_string()),
                (200, json!({"access_token": "gho_new"}).to_string()),
            ],
        ),
        (
            "/copilot_internal/user",
            vec![(200, json!({"chat_enabled": true, "endpoints": {"api": "https://api.individual.githubcopilot.com"}}).to_string())],
        ),
    ])
    .await;
    let e = copilot::Endpoints {
        client_id: "Iv1.test-client".into(),
        device: format!("{}/login/device/code", f.url),
        token: format!("{}/login/oauth/access_token", f.url),
        user: format!("{}/copilot_internal/user", f.url),
    };
    let client = reqwest::Client::new();
    let code = copilot::start(&client, &e).await.unwrap();
    assert_eq!(code.user_code, "ABCD-1234");
    assert_eq!(code.verification_uri, "https://github.com/login/device");
    let saved = copilot::finish(
        &client,
        &e,
        &code,
        Some("https://company.ghe.com/"),
        std::time::Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(
        saved,
        json!({"type": "oauth", "method": "device", "access": "gho_new", "refresh": "gho_new", "expires": 0,
               "metadata": {"enterpriseUrl": "company.ghe.com", "apiEndpoint": "https://api.individual.githubcopilot.com"}})
    );
    let seen = f.seen.lock().unwrap();
    assert!(
        seen[0].contains("\"client_id\":\"Iv1.test-client\""),
        "{}",
        seen[0]
    );
    assert!(seen[0].contains("\"scope\":\"read:user\""));
    assert!(seen[1].contains("urn:ietf:params:oauth:grant-type:device_code"));
    // The saved login routes to its endpoint.
    let r = copilot_case(saved).ok();
    assert_eq!(r.base_url, "https://api.individual.githubcopilot.com");
}

#[tokio::test]
async fn copilot_device_login_fails_on_a_denial_or_without_entitlement() {
    let f = fixture(vec![
        (
            "/login/device/code",
            vec![(200, json!({"verification_uri": "u", "user_code": "c", "device_code": "d", "interval": 1}).to_string())],
        ),
        ("/login/oauth/access_token", vec![(200, json!({"access_token": "gho_x"}).to_string())]),
        ("/copilot_internal/user", vec![(200, json!({"chat_enabled": false}).to_string())]),
    ])
    .await;
    let e = copilot::Endpoints {
        client_id: "Iv1.test-client".into(),
        device: format!("{}/login/device/code", f.url),
        token: format!("{}/login/oauth/access_token", f.url),
        user: format!("{}/copilot_internal/user", f.url),
    };
    let client = reqwest::Client::new();
    let code = copilot::start(&client, &e).await.unwrap();
    let err = copilot::finish(&client, &e, &code, None, std::time::Duration::ZERO)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not have GitHub Copilot access"), "{err}");
    let denied = fixture(vec![(
        "/login/oauth/access_token",
        vec![(200, json!({"error": "access_denied"}).to_string())],
    )])
    .await;
    let e = copilot::Endpoints {
        token: format!("{}/login/oauth/access_token", denied.url),
        ..e
    };
    let err = copilot::finish(&client, &e, &code, None, std::time::Duration::ZERO)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(err, "Device authorization failed: access_denied");
}
#[test]
fn copilot_login_needs_a_client_id_of_your_own() {
    let s = Settings::default();
    let e = copilot::client_id(None, &s, &env(&[]))
        .unwrap_err()
        .to_string();
    assert!(e.contains("Enable Device Flow"), "{e}");
    assert!(
        e.contains("CODESMITH_COPILOT_CLIENT_ID") && e.contains("COPILOT_GITHUB_TOKEN"),
        "{e}"
    );
    assert_eq!(
        copilot::client_id(
            None,
            &s,
            &env(&[("CODESMITH_COPILOT_CLIENT_ID", "from-env")])
        )
        .unwrap(),
        "from-env"
    );
    let configured: ProviderSettings = settings(json!({"clientId": "from-config"}));
    assert_eq!(
        copilot::client_id(
            None,
            &configured.plugin,
            &env(&[("CODESMITH_COPILOT_CLIENT_ID", "from-env")])
        )
        .unwrap(),
        "from-config"
    );
    assert_eq!(
        copilot::client_id(Some("from-flag"), &configured.plugin, &env(&[])).unwrap(),
        "from-flag"
    );
}

// plugin/provider-{cerebras,kilo,llmgateway,nvidia,zenmux,openrouter,databricks}.test.ts

use codesmith_core::providers::gateways;

fn sent_headers(config: Value, spec: &str) -> Vec<(String, String)> {
    let c: Config = serde_json::from_value(config).unwrap();
    c.resolve_model(spec).unwrap().provider.headers
}

fn get<'a>(h: &'a [(String, String)], k: &str) -> Option<&'a str> {
    h.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(k))
        .map(|(_, v)| v.as_str())
}

#[test]
fn oc_cerebras_applies_the_legacy_integration_header() {
    let h = sent_headers(json!({}), "cerebras/llama-4");
    assert_eq!(
        get(&h, "X-Cerebras-3rd-Party-Integration"),
        Some("codesmith")
    );
}

#[test]
fn oc_cerebras_ignores_non_cerebras_providers() {
    let h = sent_headers(json!({}), "groq/llama-4");
    assert_eq!(get(&h, "X-Cerebras-3rd-Party-Integration"), None);
}

#[test]
fn oc_cerebras_applies_the_integration_header_to_custom_native_cerebras_providers() {
    let h = sent_headers(
        json!({"providers": {"mine": {"settings": {"baseURL": "https://api.cerebras.ai/v1"}}}}),
        "mine/x",
    );
    assert_eq!(
        get(&h, "X-Cerebras-3rd-Party-Integration"),
        Some("codesmith")
    );
}

#[test]
fn oc_kilo_applies_legacy_referer_headers_only_to_kilo_endpoints() {
    let h = gateways::headers("anything", "https://api.kilo.ai/api/gateway");
    assert_eq!(
        h,
        [
            ("HTTP-Referer", "https://github.com/reka18/codesmith"),
            ("X-Title", "codesmith")
        ]
    );
    assert!(gateways::headers("kilo", "https://api.kilo.ai/other").is_empty());
}

#[test]
fn oc_kilo_uses_endpoint_matching_instead_of_a_provider_id_guard() {
    let h = sent_headers(
        json!({"providers": {"my-kilo": {"settings": {"baseURL": "https://api.kilo.ai/api/gateway"}}}}),
        "my-kilo/x",
    );
    assert_eq!(get(&h, "X-Title"), Some("codesmith"));
}

#[test]
fn oc_llmgateway_applies_legacy_referer_headers() {
    let h = gateways::headers("llmgateway", "https://api.llmgateway.io/v1");
    assert_eq!(h.last(), Some(&("X-Source", "codesmith")));
}

#[test]
fn oc_nvidia_applies_tracking_headers_and_preserves_an_explicit_billing_origin() {
    let h = sent_headers(
        json!({"providers": {"nvidia": {"settings": {"baseURL": "https://integrate.api.nvidia.com/v1"}}}}),
        "nvidia/x",
    );
    assert_eq!(get(&h, "X-BILLING-INVOKE-ORIGIN"), Some("CodeSmith"));
    let h = sent_headers(
        json!({"providers": {"nvidia": {"settings": {"baseURL": "https://integrate.api.nvidia.com/v1"}, "headers": {"X-BILLING-INVOKE-ORIGIN": "Mine"}}}}),
        "nvidia/x",
    );
    assert_eq!(get(&h, "X-BILLING-INVOKE-ORIGIN"), Some("Mine"));
}

#[test]
fn oc_zenmux_lets_configured_legacy_headers_override_defaults() {
    let h = sent_headers(
        json!({"providers": {"zenmux": {"settings": {"baseURL": "https://zenmux.ai/api/v1"}, "headers": {"X-Title": "mine", "X-Other": "1"}}}}),
        "zenmux/x",
    );
    assert_eq!(get(&h, "X-Title"), Some("mine"));
    assert_eq!(get(&h, "X-Other"), Some("1"));
    assert_eq!(
        get(&h, "HTTP-Referer"),
        Some("https://github.com/reka18/codesmith")
    );
}

#[test]
fn oc_openrouter_applies_referer_headers() {
    let h = sent_headers(json!({}), "openrouter/anthropic/claude-sonnet-4.5");
    assert_eq!(get(&h, "X-Title"), Some("codesmith"));
}

#[test]
fn oc_databricks_fills_the_base_url_from_a_bare_host_or_one_with_a_scheme() {
    for host in [
        "adb-1.azuredatabricks.net",
        "https://adb-1.azuredatabricks.net/",
    ] {
        let mut c = Case::new("databricks", json!({}));
        c.catalog_url = Some("https://${DATABRICKS_HOST}/ai-gateway/mlflow/v1");
        c.env = env(&[("DATABRICKS_HOST", host)]);
        assert_eq!(
            c.ok().base_url,
            "https://adb-1.azuredatabricks.net/ai-gateway/mlflow/v1",
            "{host}"
        );
    }
    assert_eq!(
        providers::builtin("databricks").unwrap().env,
        ["DATABRICKS_TOKEN"]
    );
}

// plugin/provider-cloudflare-ai-gateway.test.ts

fn cf_form(id: &str, s: Value, e: &[(&str, &str)]) -> providers::KeyForm {
    providers::key_form(id, &settings(s), &env(e)).unwrap()
}

fn field_keys(f: &providers::KeyForm) -> Vec<&'static str> {
    f.fields.iter().map(|f| f.key).collect()
}

#[test]
fn oc_cloudflare_gateway_registers_account_and_gateway_forms_when_the_environment_does_not_provide_them(
) {
    let f = cf_form("cloudflare-ai-gateway", json!({}), &[]);
    assert_eq!(f.label, "Gateway API token");
    assert_eq!(field_keys(&f), ["accountId", "gatewayId"]);
    assert!(f.fields.iter().all(|f| f.required));
}

#[test]
fn oc_cloudflare_gateway_uses_environment_account_and_gateway_values_without_requesting_form_fields(
) {
    let f = cf_form(
        "cloudflare-ai-gateway",
        json!({}),
        &[
            ("CLOUDFLARE_ACCOUNT_ID", "account"),
            ("CLOUDFLARE_GATEWAY_ID", "gateway"),
        ],
    );
    assert_eq!(f.label, "Gateway API token");
    assert!(f.fields.is_empty());
}

#[test]
fn oc_cloudflare_gateway_requests_only_the_gateway_id_when_the_environment_provides_the_account_id()
{
    let f = cf_form(
        "cloudflare-ai-gateway",
        json!({}),
        &[("CLOUDFLARE_ACCOUNT_ID", "account")],
    );
    assert_eq!(field_keys(&f), ["gatewayId"]);
}

fn gateway_case(s: Value, e: &[(&str, &str)], model: &'static str) -> Case {
    let mut c = Case::new("cloudflare-ai-gateway", s);
    c.env = env(e);
    c.model = model;
    c.key = Some("cf-token");
    c
}

#[test]
fn oc_cloudflare_gateway_gives_the_provider_its_environment_account_and_gateway_ids() {
    let e = [
        ("CLOUDFLARE_ACCOUNT_ID", "account"),
        ("CLOUDFLARE_GATEWAY_ID", "gateway"),
    ];
    // Models of OpenAI and Anthropic go to the gateway's passthroughs, with
    // the token in cf-aig-authorization and their upstream model ids.
    let r = gateway_case(json!({}), &e, "openai/gpt-5.4").ok();
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert_eq!(
        r.base_url,
        "https://gateway.ai.cloudflare.com/v1/account/gateway/openai"
    );
    assert_eq!(r.auth, Auth::BearerHeader("cf-aig-authorization".into()));
    assert_eq!(r.model_id.as_deref(), Some("gpt-5.4"));
    let r = gateway_case(json!({}), &e, "anthropic/claude-sonnet-4.6").ok();
    assert_eq!(r.protocol, Protocol::AnthropicMessages);
    assert_eq!(
        r.base_url,
        "https://gateway.ai.cloudflare.com/v1/account/gateway/anthropic/v1"
    );
    assert_eq!(r.model_id.as_deref(), Some("claude-sonnet-4-6"));
    // The rest go to the account's REST API, naming the gateway.
    let r = gateway_case(json!({}), &e, "workers-ai/@cf/meta/llama-3.1-8b").ok();
    assert_eq!(r.protocol, Protocol::OpenaiChat);
    assert_eq!(
        r.base_url,
        "https://api.cloudflare.com/client/v4/accounts/account/ai/v1"
    );
    assert_eq!(r.auth, Auth::Bearer);
    assert_eq!(
        r.headers,
        [("cf-aig-gateway-id".to_string(), "gateway".to_string())]
    );
    assert_eq!(r.model_id.as_deref(), Some("@cf/meta/llama-3.1-8b"));
    assert_eq!(r.api_key.as_deref(), Some("cf-token"));
}

#[test]
fn oc_cloudflare_gateway_prefers_configured_ids_over_environment_ids() {
    let r = gateway_case(
        json!({"accountId": "configured-account", "gatewayId": "configured-gateway"}),
        &[
            ("CLOUDFLARE_ACCOUNT_ID", "env-account"),
            ("CLOUDFLARE_GATEWAY_ID", "env-gateway"),
        ],
        "openai/gpt-5.4",
    )
    .ok();
    assert_eq!(
        r.base_url,
        "https://gateway.ai.cloudflare.com/v1/configured-account/configured-gateway/openai"
    );
}

#[test]
fn oc_cloudflare_gateway_gives_the_provider_the_account_and_gateway_ids_of_a_stored_form_login() {
    let mut c = gateway_case(json!({}), &[], "openai/gpt-5.4");
    c.stored = Some(
        json!({"type": "api", "key": "token", "metadata": {"accountId": "form-account", "gatewayId": "form-gateway"}}),
    );
    assert_eq!(
        c.ok().base_url,
        "https://gateway.ai.cloudflare.com/v1/form-account/form-gateway/openai"
    );
    // Without any account id the route explains where to give one.
    let e = gateway_case(json!({}), &[], "openai/gpt-5.4")
        .route()
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("codesmith auth login cloudflare-ai-gateway"),
        "{e}"
    );
}

#[test]
fn oc_cloudflare_gateway_applies_the_account_and_gateway_ids_of_a_form_login_made_after_startup() {
    // Routes read the saved login each time a model is resolved.
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    let mut answers = serde_json::Map::new();
    answers.insert("accountId".into(), "form-account".into());
    answers.insert("gatewayId".into(), "form-gateway".into());
    providers::save_key(&file, "cloudflare-ai-gateway", "token", answers).unwrap();
    let mut c = gateway_case(json!({}), &[], "anthropic/claude-sonnet-4.6");
    c.stored = codesmith_core::auth::entry_in(&file, "cloudflare-ai-gateway");
    assert_eq!(
        c.ok().base_url,
        "https://gateway.ai.cloudflare.com/v1/form-account/form-gateway/anthropic/v1"
    );
    assert_eq!(
        codesmith_core::auth::key_in(&file, "cloudflare-ai-gateway").as_deref(),
        Some("token")
    );
}

#[test]
fn oc_cloudflare_gateway_leaves_a_configured_endpoint_url_untouched() {
    let s = json!({"baseURL": "https://proxy.example/v1"});
    let e = [
        ("CLOUDFLARE_ACCOUNT_ID", "account"),
        ("CLOUDFLARE_GATEWAY_ID", "gateway"),
    ];
    for model in ["openai/gpt-5.4", "anthropic/claude-sonnet-4.6", "x/y"] {
        let r = gateway_case(s.clone(), &e, model).ok();
        assert_eq!(r.base_url, "https://proxy.example/v1");
        assert_eq!(r.model_id, None, "{model}: gateway model ids are kept");
        assert_eq!(r.auth, Auth::Bearer);
    }
    assert!(cf_form("cloudflare-ai-gateway", s, &[]).fields.is_empty());
}

// plugin/provider-cloudflare-workers-ai.test.ts

fn workers_case(s: Value, e: &[(&str, &str)]) -> Case {
    let mut c = Case::new("cloudflare-workers-ai", s);
    c.env = env(e);
    c.model = "@cf/meta/llama-3.1-8b-instruct";
    c.key = Some("key");
    c
}

#[test]
fn oc_cloudflare_workers_registers_an_account_form_when_the_environment_does_not_provide_one() {
    let f = cf_form("cloudflare-workers-ai", json!({}), &[]);
    assert_eq!(f.label, "API key");
    assert_eq!(
        f.fields,
        [providers::Field {
            key: "accountId",
            title: "Enter your Cloudflare Account ID",
            placeholder: "e.g. 1234567890abcdef1234567890abcdef",
            required: true,
        }]
    );
    let p = providers::builtin("cloudflare-workers-ai").unwrap();
    assert_eq!(
        p.env,
        [
            "CLOUDFLARE_API_KEY",
            "CLOUDFLARE_WORKERS_AI_TOKEN",
            "CLOUDFLARE_API_TOKEN"
        ]
    );
}

#[test]
fn oc_cloudflare_workers_routes_the_openai_compatible_package_to_the_native_workers_ai_package() {
    let e = [
        ("CLOUDFLARE_ACCOUNT_ID", "acct"),
        ("CLOUDFLARE_API_KEY", "key"),
    ];
    assert!(cf_form("cloudflare-workers-ai", json!({}), &e)
        .fields
        .is_empty());
    for package in [
        "@opencode/ai/providers/cloudflare-workers-ai",
        "@opencode/ai/providers/openai-compatible",
    ] {
        let mut c = workers_case(json!({}), &e);
        c.package = Some(package);
        let r = c.ok();
        assert_eq!(r.protocol, Protocol::OpenaiChat);
        assert_eq!(
            r.base_url,
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1"
        );
        assert_eq!(r.auth, Auth::Bearer);
    }
}

#[test]
fn oc_cloudflare_workers_gives_the_native_workers_ai_package_its_account_id() {
    let r = workers_case(json!({}), &[("CLOUDFLARE_ACCOUNT_ID", "acct")]).ok();
    assert_eq!(
        r.base_url,
        "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1"
    );
}

#[test]
fn oc_cloudflare_workers_gives_the_native_workers_ai_package_the_account_id_of_a_stored_form_login()
{
    let mut c = workers_case(json!({}), &[]);
    c.stored =
        Some(json!({"type": "api", "key": "secret", "metadata": {"accountId": "form-acct"}}));
    assert_eq!(
        c.ok().base_url,
        "https://api.cloudflare.com/client/v4/accounts/form-acct/ai/v1"
    );
}

#[test]
fn oc_cloudflare_workers_follows_the_account_id_when_another_form_login_becomes_active() {
    // CodeSmith keeps one login per provider: a new login replaces it.
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    let save = |acct: &str| {
        let mut m = serde_json::Map::new();
        m.insert("accountId".into(), acct.into());
        providers::save_key(&file, "cloudflare-workers-ai", "secret", m).unwrap();
        let mut c = workers_case(json!({}), &[]);
        c.stored = codesmith_core::auth::entry_in(&file, "cloudflare-workers-ai");
        c.ok().base_url
    };
    assert!(save("second-acct").contains("/accounts/second-acct/"));
    assert!(save("first-acct").contains("/accounts/first-acct/"));
}

#[test]
fn oc_cloudflare_workers_preserves_a_configured_endpoint_url_instead_of_deriving_one_from_account_id(
) {
    let r = workers_case(
        json!({"baseURL": "https://proxy.example/v1"}),
        &[("CLOUDFLARE_ACCOUNT_ID", "acct")],
    )
    .ok();
    assert_eq!(r.base_url, "https://proxy.example/v1");
}

#[test]
fn oc_cloudflare_workers_uses_configured_account_id_over_env_account_id() {
    let r = workers_case(
        json!({"accountId": "configured-acct"}),
        &[("CLOUDFLARE_ACCOUNT_ID", "env-acct")],
    )
    .ok();
    assert_eq!(
        r.base_url,
        "https://api.cloudflare.com/client/v4/accounts/configured-acct/ai/v1"
    );
}

#[test]
fn oc_cloudflare_resolves_bundled_models_through_the_gateway_and_workers_ai() {
    let acct = json!({"settings": {"accountId": "acct-from-env"}});
    let c = bundled(json!({"providers": {
        "cloudflare-workers-ai": acct,
        "cloudflare-ai-gateway": acct,
    }}));
    let m = c
        .resolve_model("cloudflare-workers-ai/@cf/moonshotai/kimi-k2.6")
        .unwrap();
    assert_eq!(m.provider.protocol, Protocol::OpenaiChat);
    assert_eq!(
        m.provider.base_url,
        "https://api.cloudflare.com/client/v4/accounts/acct-from-env/ai/v1"
    );
    let m = c
        .resolve_model("cloudflare-ai-gateway/anthropic/claude-opus-4.8")
        .unwrap();
    assert_eq!(m.provider.protocol, Protocol::AnthropicMessages);
    assert_eq!(m.model_id, "claude-opus-4-8");
    assert_eq!(
        m.provider.base_url,
        "https://gateway.ai.cloudflare.com/v1/acct-from-env/default/anthropic/v1"
    );
}

// plugin/provider-sap-ai-core.test.ts

use codesmith_core::providers::sap;

fn service_key(api: &str, auth: &str) -> String {
    json!({
        "clientid": "sb-client",
        "clientsecret": "secret",
        "url": auth,
        "serviceurls": {"AI_API_URL": api},
    })
    .to_string()
}

fn sap_case(s: Value, e: &[(&str, &str)]) -> Case {
    let mut c = Case::new("sap-ai-core", s);
    c.env = env(e);
    c
}

#[test]
fn oc_sap_copies_service_key_option_into_aicore_service_key_but_keeps_sdk_options_to_deployment_metadata(
) {
    let key = service_key("https://api.ai.example", "https://auth.example");
    let r = sap_case(
        json!({"serviceKey": key}),
        &[
            ("AICORE_DEPLOYMENT_ID", "deployment"),
            ("AICORE_RESOURCE_GROUP", "resource-group"),
        ],
    )
    .ok();
    assert_eq!(
        r.base_url,
        "https://api.ai.example/v2/inference/deployments/deployment"
    );
    assert_eq!(
        r.headers,
        [(
            "AI-Resource-Group".to_string(),
            "resource-group".to_string()
        )]
    );
    assert_eq!(sigv4(&r), "SAP AI Core service key sb-client");
    assert_eq!(r.api_key, None);
}

#[test]
fn oc_sap_preserves_existing_aicore_service_key_over_service_key_option() {
    let r = sap_case(
        json!({"serviceKey": service_key("https://option.example", "https://auth.example")}),
        &[
            (
                "AICORE_SERVICE_KEY",
                &service_key("https://env.example", "https://auth.example"),
            ),
            ("AICORE_DEPLOYMENT_ID", "deployment"),
            ("AICORE_RESOURCE_GROUP", "resource-group"),
        ],
    )
    .ok();
    assert_eq!(
        r.base_url,
        "https://env.example/v2/inference/deployments/deployment"
    );
}

#[test]
fn oc_sap_omits_deployment_and_resource_group_sdk_options_when_no_service_key_is_available() {
    let e = [
        ("AICORE_DEPLOYMENT_ID", "deployment"),
        ("AICORE_RESOURCE_GROUP", "resource-group"),
    ];
    let mut c = sap_case(json!({"baseURL": "https://proxy.example/v1"}), &e);
    c.key = Some("plain-key");
    let r = c.ok();
    assert_eq!(r.base_url, "https://proxy.example/v1");
    assert!(r.headers.is_empty());
    assert_eq!(r.auth, Auth::Bearer);
    assert_eq!(r.api_key.as_deref(), Some("plain-key"));
    let err = sap_case(json!({}), &e).route().unwrap_err().to_string();
    assert!(err.contains("AICORE_SERVICE_KEY"), "{err}");
}

#[test]
fn oc_sap_ignores_non_sap_ai_core_providers() {
    let mut c = Case::new(
        "openai",
        json!({"serviceKey": service_key("https://api.ai.example", "https://auth.example")}),
    );
    c.env = env(&[("AICORE_DEPLOYMENT_ID", "deployment")]);
    assert!(c.route().unwrap().is_none());
}

#[tokio::test]
async fn oc_sap_fetches_client_credentials_tokens_from_the_service_key() {
    let f = fixture(vec![(
        "/oauth/token",
        vec![(
            200,
            json!({"access_token": "sap-token", "expires_in": 3600}).to_string(),
        )],
    )])
    .await;
    let key = sap::parse_key(&service_key("https://api.ai.example", &f.url)).unwrap();
    let a = sap::ClientCredentials::new(key);
    let client = reqwest::Client::new();
    assert_eq!(a.token(&client).await.unwrap(), "sap-token");
    assert_eq!(a.token(&client).await.unwrap(), "sap-token");
    let seen = f.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "cached");
    use base64::Engine;
    let basic = base64::engine::general_purpose::STANDARD.encode("sb-client:secret");
    assert!(seen[0].to_ascii_lowercase().contains(&format!(
        "authorization: basic {}",
        basic.to_ascii_lowercase()
    )));
    assert!(seen[0].ends_with("grant_type=client_credentials"));
    assert!(sap::parse_key("not json").is_err());
}

// plugin/provider-openrouter.test.ts

#[test]
fn oc_openrouter_filters_openrouters_gpt_5_chat_alias() {
    assert!(providers::hidden("openrouter", "openai/gpt-5-chat"));
    assert!(!providers::hidden("openrouter", "openai/gpt-5"));
    assert!(!providers::hidden("openai", "openai/gpt-5-chat"));
}

#[test]
fn oc_openrouter_does_not_disable_gpt_5_chat_latest_for_non_openrouter_providers() {
    assert!(!providers::hidden("custom-openrouter", "gpt-5-chat-latest"));
    assert!(!providers::hidden("openrouter", "gpt-5-chat-latest"));
}

// modal/models.test.ts

use codesmith_core::providers::{live, modal};

fn modal_template(id: &str) -> codesmith_core::catalog::Model {
    let c = codesmith_core::catalog::bundled();
    let mut m = c.iter().find(|p| p.info.id == "modal").unwrap().models[0].clone();
    m.id = id.to_string();
    m.model_id = id.to_string();
    m.name = format!("{id} catalog");
    m.family = Some("catalog-family".into());
    m.variants = vec![];
    m.compatibility = None;
    m.cost = vec![];
    m.limit = Default::default();
    m.capabilities.input = vec!["text".into()];
    m
}

#[tokio::test]
async fn oc_modal_modal_plugin_is_registered() {
    let f = fixture(vec![(
        "/v1/models",
        vec![(200, json!({"data": [{"id": "a"}]}).to_string())],
    )])
    .await;
    let d = tempfile::tempdir().unwrap();
    // `codesmith models modal` fetches and keeps the list.
    let raw = modal::load(&reqwest::Client::new(), &format!("{}/v1", f.url), "k")
        .await
        .unwrap();
    live::save_in(
        &d.path().join("live.json"),
        "modal",
        "acct",
        "https://x/v1",
        raw,
    )
    .unwrap();
    let kept = live::load_in(&d.path().join("live.json"), "modal").unwrap();
    let models = live::derive("modal", &kept, &[]);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "a");
    assert!(codesmith_core::catalog::bundled()
        .iter()
        .any(|p| p.info.id == "modal"));
}

#[tokio::test]
async fn oc_modal_maps_live_modal_models_onto_catalog_templates() {
    let f = fixture(vec![(
        "/v1/models",
        vec![(
            200,
            json!({"data": [
                {
                    "id": "live-model",
                    "base_model_id": "base-model",
                    "name": "Live Model",
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"],
                    "context_length": 128000,
                    "max_output_length": 8192,
                    "pricing": {"prompt": "0.000001", "completion": 0.000002, "input_cache_read": "0.0000002"},
                    "supported_sampling_parameters": ["temperature"],
                    "supported_features": ["tools", "reasoning"],
                    "reasoning_options": [{"type": "effort", "values": ["low", "high", null]}],
                    "interleaved": {"field": "reasoning_content"},
                },
                {"id": "standalone", "context_length": 64000},
                {"id": "malformed", "context_length": "huge"},
            ]})
            .to_string(),
        )],
    )])
    .await;
    let base = format!("{}/v1", f.url);
    let raw = modal::load(&reqwest::Client::new(), &base, "test-key")
        .await
        .unwrap();
    let seen = f.seen.lock().unwrap().clone();
    assert!(seen[0].starts_with("GET /v1/models "));
    assert!(seen[0]
        .to_ascii_lowercase()
        .contains("authorization: bearer test-key"));
    let models = modal::derive(
        &base,
        &modal::parse(&raw),
        &[modal_template("base-model"), modal_template("stale")],
    );
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["live-model", "standalone"],
        "no stale or malformed models"
    );
    let m = &models[0];
    assert_eq!(m.name, "Live Model");
    assert_eq!(m.family.as_deref(), Some("catalog-family"));
    assert_eq!(m.provider_id, "modal");
    assert_eq!(m.model_id, "live-model");
    assert_eq!(
        m.package.as_deref(),
        Some("@opencode/ai/providers/openai-compatible")
    );
    assert_eq!(
        m.settings.as_ref().unwrap().base_url.as_deref(),
        Some(base.as_str())
    );
    assert_eq!(
        m.compatibility.as_ref().unwrap().reasoning_field.as_deref(),
        Some("reasoning_content")
    );
    assert!(m.capabilities.tools);
    assert_eq!(m.capabilities.input, ["text", "image"]);
    assert_eq!(m.capabilities.output, ["text"]);
    assert!((m.cost[0].input - 1.0).abs() < 1e-9);
    assert!((m.cost[0].output - 2.0).abs() < 1e-9);
    assert!((m.cost[0].cache.read - 0.2).abs() < 1e-9);
    assert_eq!(m.cost[0].cache.write, 0.0);
    assert_eq!((m.limit.context, m.limit.output), (128000, 8192));
    let v: Vec<&str> = m.variants.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(v, ["low", "high", "none"]);
    assert_eq!(
        m.variants[0].settings.as_ref().unwrap()["reasoningEffort"],
        "low"
    );
    assert_eq!(m.status, "active");
    let fresh = &models[1];
    assert_eq!(fresh.name, "standalone");
    assert_eq!(fresh.family, None);
    assert!(fresh.capabilities.tools);
    assert_eq!(fresh.capabilities.input, ["text"]);
    assert!(fresh.variants.is_empty());
    assert_eq!((fresh.limit.context, fresh.limit.output), (64000, 0));
}

#[test]
fn oc_modal_keeps_template_cost_and_limits_when_the_proxy_omits_them() {
    let mut base = modal_template("hf-base");
    base.cost = vec![codesmith_core::catalog::Cost {
        tier: None,
        input: 5.0,
        output: 10.0,
        cache: codesmith_core::catalog::CacheCost {
            read: 1.0,
            write: 2.0,
        },
    }];
    base.limit = codesmith_core::catalog::Limit {
        context: 1000,
        input: Some(500),
        output: 250,
    };
    let remote = modal::parse(&[json!({"id": "sparse", "hugging_face_id": "hf-base"})]);
    let m = &modal::derive("https://proxy", &remote, &[base])[0];
    assert_eq!(m.name, "hf-base catalog");
    let c = m.cost[0];
    assert_eq!(
        (c.input, c.output, c.cache.read, c.cache.write),
        (5.0, 10.0, 1.0, 2.0)
    );
    assert_eq!(
        (m.limit.context, m.limit.input, m.limit.output),
        (1000, Some(500), 250)
    );
}

#[tokio::test]
async fn oc_modal_throws_on_proxy_failure_so_the_plugin_can_fail_soft() {
    let f = fixture(vec![("/models", vec![(500, "nope".into())])]).await;
    assert!(modal::load(&reqwest::Client::new(), &f.url, "test-key")
        .await
        .is_err());
}

// github-copilot/models.test.ts

use codesmith_core::providers::copilot_models;

fn copilot_template(id: &str, name: &str) -> codesmith_core::catalog::Model {
    let mut m = modal_template(id);
    m.provider_id = "github-copilot".into();
    m.name = name.into();
    m.family = None;
    m.time.released = 0;
    m
}

#[test]
fn oc_copilot_models_defensively_syncs_advertised_copilot_models() {
    let raw: Vec<Value> = serde_json::from_value(json!([
        {
            "model_picker_enabled": true,
            "id": "gpt-5",
            "name": "GPT-5 remote",
            "version": "gpt-5-2026-06-01",
            "supported_endpoints": ["/responses"],
            "billing": {"token_prices": {"batch_size": 0, "default": {"input_price": 10, "output_price": 20, "cache_price": 5}}},
            "capabilities": {
                "family": "gpt",
                "limits": {
                    "max_context_window_tokens": 200000,
                    "max_output_tokens": 16384,
                    "max_prompt_tokens": 180000,
                    "vision": {"max_prompt_image_size": 10000000, "max_prompt_images": 10, "supported_media_types": ["image/png", "application/pdf"]},
                },
                "supports": {"tool_calls": true, "vision": true, "reasoning_effort": ["low", "high"]},
            },
        },
        {
            "model_picker_enabled": true,
            "id": "claude-sonnet",
            "name": "Claude Sonnet",
            "version": "claude-sonnet-2026-06-01",
            "supported_endpoints": ["/v1/messages"],
            "capabilities": {"family": "claude", "limits": {"max_output_tokens": 16384, "max_prompt_tokens": 180000}, "supports": {"tool_calls": true}},
        },
        {
            "model_picker_enabled": true,
            "id": "vision-only",
            "name": "Vision only",
            "version": "vision-only-2026-06-01",
            "capabilities": {
                "family": "vision",
                "limits": {"max_output_tokens": 16384, "max_prompt_tokens": 180000, "vision": {"max_prompt_image_size": 10000000, "max_prompt_images": 10, "supported_media_types": ["image/png"]}},
                "supports": {"tool_calls": true, "vision": true},
            },
        },
        {
            "model_picker_enabled": false,
            "id": "utility",
            "name": "Utility",
            "version": "utility-2026-06-01",
            "capabilities": {"family": "utility", "limits": {"max_output_tokens": 1000, "max_prompt_tokens": 8000}, "supports": {"tool_calls": false}},
        },
        {"model_picker_enabled": true, "id": "incomplete"},
    ]))
    .unwrap();
    let root = "https://copilot.example";
    let models = copilot_models::derive(
        root,
        &copilot_models::parse(&raw),
        &[
            copilot_template("gpt-5", "GPT-5 local"),
            copilot_template("stale", "stale"),
        ],
    );
    let get = |id: &str| models.iter().find(|m| m.id == id);
    let m = get("gpt-5").unwrap();
    assert_eq!(m.name, "GPT-5 local");
    assert_eq!(
        m.package.as_deref(),
        Some("@opencode/ai/providers/openai-compatible-responses")
    );
    assert_eq!(m.settings.as_ref().unwrap().base_url.as_deref(), Some(root));
    let c = m.cost[0];
    assert_eq!(
        (c.input, c.output, c.cache.read, c.cache.write),
        (0.0, 0.0, 0.0, 0.0)
    );
    let v: Vec<&str> = m.variants.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(v, ["low", "high"]);
    assert_eq!(m.capabilities.input, ["text", "image", "pdf"]);
    assert_eq!(m.time.released, 1_780_272_000_000, "2026-06-01");
    let claude = get("claude-sonnet").unwrap();
    assert_eq!(
        claude.package.as_deref(),
        Some("@opencode/ai/providers/anthropic")
    );
    assert_eq!(
        claude.settings.as_ref().unwrap().base_url.as_deref(),
        Some("https://copilot.example/v1")
    );
    assert_eq!(
        get("vision-only").unwrap().capabilities.input,
        ["text", "image"]
    );
    assert!(!get("utility").unwrap().enabled);
    assert!(get("stale").is_none());
    assert!(get("incomplete").is_none());
    // No Auto pool, no Auto model.
    assert!(get("auto").is_none());
}

#[test]
fn oc_copilot_models_prices_cache_reads_from_either_token_price_spelling() {
    let item = |id: &str, prices: Value| {
        let mut d = json!({"input_price": 250, "output_price": 1500});
        d.as_object_mut()
            .unwrap()
            .extend(prices.as_object().unwrap().clone());
        json!({
            "model_picker_enabled": true,
            "id": id,
            "name": id,
            "version": format!("{id}-2026-08-01"),
            "supported_endpoints": ["/chat/completions"],
            "billing": {"token_prices": {"batch_size": 1_000_000, "default": d}},
            "capabilities": {"family": "gpt", "limits": {"max_output_tokens": 1000, "max_prompt_tokens": 8000}, "supports": {"tool_calls": true}},
        })
    };
    let raw = vec![
        item(
            "renamed",
            json!({"cache_read_price": 25, "cache_write_price": 0}),
        ),
        item("legacy", json!({"cache_price": 25})),
        item("unpriced", json!({})),
    ];
    let models = copilot_models::derive("https://c", &copilot_models::parse(&raw), &[]);
    for id in ["renamed", "legacy"] {
        let c = models.iter().find(|m| m.id == id).unwrap().cost[0];
        assert!(
            (c.input - 2.5).abs() < 1e-9 && (c.output - 15.0).abs() < 1e-9,
            "{id}"
        );
        assert!((c.cache.read - 0.25).abs() < 1e-9, "{id}");
    }
    let c = models.iter().find(|m| m.id == "unpriced").unwrap().cost[0];
    assert_eq!(c.cache.read, 0.0);
}

#[test]
fn oc_copilot_models_builds_auto_from_the_auto_pool() {
    let item = |id: &str, ctx: u64, out: u64| {
        json!({
            "model_picker_enabled": true, "id": id, "name": id, "version": "1",
            "billing": {"auto_discount": 0.1},
            "capabilities": {"family": "x", "limits": {"max_context_window_tokens": ctx, "max_output_tokens": out, "max_prompt_tokens": ctx - 1000}, "supports": {"tool_calls": true}},
        })
    };
    let raw = vec![item("a", 200_000, 64_000), item("b", 128_000, 16_000)];
    let models = copilot_models::derive("https://c", &copilot_models::parse(&raw), &[]);
    let auto = models.iter().find(|m| m.id == "auto").unwrap();
    assert_eq!(auto.name, "Auto");
    assert_eq!(
        auto.package.as_deref(),
        Some("@opencode/ai/providers/openai-compatible")
    );
    assert_eq!(
        (auto.limit.context, auto.limit.input, auto.limit.output),
        (128_000, Some(127_000), 16_000)
    );
    assert_eq!(auto.capabilities.input, ["text", "image"]);
}

// plugin/provider-github-copilot.test.ts: utility titles

fn utility_provider(url: &str) -> codesmith_ai::Provider {
    codesmith_ai::Provider {
        protocol: Protocol::OpenaiResponses,
        name: "github-copilot".into(),
        base_url: url.to_string(),
        api_key: Some("token".into()),
        headers: vec![],
        query: vec![],
        auth: Auth::Bearer,
        encoding: Default::default(),
        compat: Default::default(),
        hooks: None,
    }
}

async fn utility(url: &str) -> anyhow::Result<String> {
    copilot::utility_title(
        &reqwest::Client::new(),
        &utility_provider(url),
        "gpt-4o-mini",
        "ses_title",
        "You are a title generator.",
        &[("user", "how do I make my python script faster")],
        Some(32),
    )
    .await
}

#[tokio::test]
async fn oc_copilot_names_sessions_with_the_utility_model_as_a_free_background_request() {
    let f = fixture(vec![(
        "/chat/completions",
        vec![(
            200,
            json!({"choices": [{"message": {"content": "  Speed Up Python Script\n"}}]})
                .to_string(),
        )],
    )])
    .await;
    assert_eq!(utility(&f.url).await.unwrap(), "Speed Up Python Script");
    let seen = f.seen.lock().unwrap()[0].clone();
    let lower = seen.to_ascii_lowercase();
    assert!(seen.starts_with("POST /chat/completions "));
    for h in [
        "authorization: bearer token",
        "x-interaction-type: agent-session-name-generation",
        "x-interaction-id: ses_title",
        "x-initiator: agent",
        "x-github-api-version: 2026-08-01",
    ] {
        assert!(lower.contains(h), "{h}: {seen}");
    }
    let body: Value = serde_json::from_str(seen.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "gpt-4o-mini",
            "stream": false,
            "max_tokens": 32,
            "messages": [
                {"role": "system", "content": "You are a title generator."},
                {"role": "user", "content": "how do I make my python script faster"},
            ],
        })
    );
}

#[tokio::test]
async fn oc_copilot_sends_the_utility_title_request_through_the_hosts_fetch() {
    // The utility model is used when the account's kept list has it, and
    // the request goes to the Copilot API root, also from a Messages route.
    let mut m = modal_template("gpt-4o-mini");
    m.model_id = "gpt-4o-mini".into();
    assert_eq!(copilot::utility_title_model_in(&[m]), Some("gpt-4o-mini"));
    assert_eq!(
        copilot::utility_title_model_in(&[modal_template("gpt-5")]),
        None
    );
    let f = fixture(vec![(
        "/chat/completions",
        vec![(
            200,
            json!({"choices": [{"message": {"content": "Faster Python"}}]}).to_string(),
        )],
    )])
    .await;
    assert_eq!(
        utility(&format!("{}/v1", f.url)).await.unwrap(),
        "Faster Python"
    );
}

#[tokio::test]
async fn oc_copilot_fails_the_utility_title_on_rate_limits_so_the_billable_path_can_run() {
    let f = fixture(vec![("/chat/completions", vec![(429, "slow down".into())])]).await;
    let e = utility(&f.url).await.unwrap_err().to_string();
    assert!(e.contains("429"), "{e}");
}

#[tokio::test]
async fn oc_copilot_fails_the_utility_title_on_an_empty_completion() {
    let f = fixture(vec![(
        "/chat/completions",
        vec![(
            200,
            json!({"choices": [{"message": {"content": null}}]}).to_string(),
        )],
    )])
    .await;
    let e = utility(&f.url).await.unwrap_err().to_string();
    assert!(e.contains("empty"), "{e}");
}
