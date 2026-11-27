//! Azure (OpenCode's `plugin/provider-azure.test.ts`): resource names,
//! Azure CLI logins and deployment discovery, against a stub `az` and a
//! local server answering like Azure's resource and management APIs.

use codesmith_core::config::{Config, ProviderSettings};
use codesmith_core::providers::{self, azure, live};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn env(pairs: &[(&str, &str)]) -> providers::Env {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Arc::new(move |k| m.get(k).cloned())
}

fn settings(v: Value) -> ProviderSettings {
    serde_json::from_value(v).unwrap()
}

const RESOURCE_ID: &str =
    "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/test-resource";
const MANAGEMENT_BEARER: &str = "Bearer https://management.azure.com/.default-token";

/// A request as the fake Azure saw it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    key: Option<String>,
    authorization: Option<String>,
}

type Handler = Arc<dyn Fn(&Seen, &str) -> (u16, String) + Send + Sync>;

struct FakeAzure {
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeAzure {
    fn endpoints(&self) -> azure::Endpoints {
        azure::Endpoints {
            resource: format!("{}/openai", self.origin),
            management: self.origin.clone(),
        }
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Answers like Azure: the resource's own list under `/openai/deployments`,
/// Resource Graph, and the management API (`management`).
async fn fake_azure(
    resource: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static,
    management: impl Fn(&Seen, &str) -> (u16, String) + Send + Sync + 'static,
) -> FakeAzure {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", l.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Seen>>> = Default::default();
    let log = seen.clone();
    let resource: Handler = Arc::new(move |s, _| resource(s));
    let management: Handler = Arc::new(management);
    let o2 = origin.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let mut buf = vec![0u8; 65536];
            let n = s.read(&mut buf).await.unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            let mut lines = text.lines();
            let mut first = lines.next().unwrap_or("").split_whitespace();
            let method = first.next().unwrap_or("").to_string();
            let path = first.next().unwrap_or("").to_string();
            let header = |name: &str| {
                text.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                })
            };
            let req = Seen {
                method,
                path: path.clone(),
                key: header("api-key"),
                authorization: header("authorization"),
            };
            log.lock().unwrap().push(req.clone());
            let (status, body) = if path.starts_with("/openai/deployments") {
                resource(&req, &o2)
            } else if path.starts_with("/providers/Microsoft.ResourceGraph/") {
                (200, json!({"data": [{"id": RESOURCE_ID}]}).to_string())
            } else {
                management(&req, &o2)
            };
            let _ = s
                .write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        }
    });
    FakeAzure { origin, seen }
}

fn not_found() -> (u16, String) {
    (404, "Not found".into())
}

fn unauthorized() -> (u16, String) {
    (401, "Unauthorized".into())
}

fn listed(id: &str, model: &str, status: &str) -> Value {
    json!({"id": id, "model": model, "status": status})
}

fn resource_list(items: Vec<Value>) -> (u16, String) {
    (200, json!({"data": items}).to_string())
}

fn managed(name: &str, model: &str, state: &str) -> Value {
    json!({"name": name, "properties": {"model": {"name": model}, "provisioningState": state}})
}

fn management_page(items: Vec<Value>, next: Option<String>) -> (u16, String) {
    let mut v = json!({"value": items});
    if let Some(n) = next {
        v["nextLink"] = n.into();
    }
    (200, v.to_string())
}

/// A stub `az` that answers `account get-access-token --scope <s>` with
/// `<s>-token` and logs each call.
struct FakeCli {
    _dir: tempfile::TempDir,
    program: std::path::PathBuf,
    log: std::path::PathBuf,
}

impl FakeCli {
    fn new() -> FakeCli {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("az");
        let log = dir.path().join("calls");
        let script = format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nscope=\"\"\nwhile [ $# -gt 0 ]; do if [ \"$1\" = --scope ]; then scope=\"$2\"; fi; shift; done\nexp=$(( $(date +%s) + 3600 ))\nprintf '{{\"accessToken\":\"%s-token\",\"expires_on\":%s}}' \"$scope\" \"$exp\"\n",
            log.display()
        );
        std::fs::write(&program, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        FakeCli {
            _dir: dir,
            program,
            log,
        }
    }
    fn cli(&self) -> azure::Cli {
        azure::Cli::new(self.program.clone())
    }
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
    fn path(&self) -> String {
        self.program.parent().unwrap().display().to_string()
    }
}

fn azure_templates() -> Vec<codesmith_core::catalog::Model> {
    codesmith_core::catalog::bundled()
        .iter()
        .find(|p| p.info.id == "azure")
        .unwrap()
        .models
        .clone()
}

fn template(id: &str) -> codesmith_core::catalog::Model {
    azure_templates().into_iter().find(|m| m.id == id).unwrap()
}

fn kept(raw: Vec<Value>) -> Vec<codesmith_core::catalog::Model> {
    azure::derive(&raw, &azure_templates())
}

// AzurePlugin connecting

#[test]
fn oc_azure_hides_azure_cli_authentication_when_the_azure_cli_is_not_installed() {
    assert_eq!(azure::find_cli(&env(&[("PATH", "/nonexistent")])), None);
    let cli = FakeCli::new();
    assert_eq!(
        azure::find_cli(&env(&[("PATH", &cli.path())])),
        Some(cli.program.clone())
    );
}

#[test]
fn oc_azure_asks_for_the_resource_name_as_text_without_calling_azure() {
    let cli = FakeCli::new();
    let form = azure::key_form(&settings(json!({})));
    assert_eq!(form.fields[0].key, "resourceName");
    assert!(
        azure::key_form(&settings(json!({"resourceName": "configured"})))
            .fields
            .is_empty()
    );
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    assert!(azure::save_cli_login(&file, Some("not a hostname")).is_err());
    azure::save_cli_login(&file, Some("typed-resource")).unwrap();
    assert_eq!(
        codesmith_core::auth::entry_in(&file, "azure").unwrap(),
        json!({"type": "external", "method": "azure-cli", "metadata": {"resourceName": "typed-resource"}})
    );
    assert!(
        cli.calls().is_empty(),
        "the login itself runs no az command"
    );
}

// AzurePlugin startup

#[tokio::test]
async fn oc_azure_starts_without_calling_azure_then_lists_deployments() {
    let cli = FakeCli::new();
    let fake = fake_azure(
        |_| not_found(),
        |_, _| management_page(vec![managed("gpt-5-mini", "gpt-5-mini", "Succeeded")], None),
    )
    .await;
    let login = json!({"type": "external", "method": "azure-cli", "metadata": {"resourceName": "test-resource"}});
    assert_eq!(
        azure::resource_name(&settings(json!({})), Some(&login), &env(&[])).as_deref(),
        Some("test-resource")
    );
    // Building the authorizer calls neither az nor Azure.
    let auth = azure::cli_auth(&env(&[("PATH", &cli.path())])).unwrap();
    assert!(matches!(auth, codesmith_ai::Auth::Dynamic(_)));
    assert!(cli.calls().is_empty());
    assert!(fake.seen().is_empty());
    let c = cli.cli();
    let list = azure::deployments(
        &reqwest::Client::new(),
        &fake.endpoints(),
        "test-resource",
        &azure::Credential::Cli(&c),
    )
    .await
    .unwrap();
    let ids: Vec<String> = kept(azure::raw(&list)).into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["gpt-5-mini"]);
}

// AzurePlugin discovery

#[tokio::test]
async fn oc_azure_shows_the_deployments_listed_with_an_api_key_named_after_the_deployment() {
    let fake = fake_azure(
        |r| {
            if r.key.as_deref() != Some("secret") {
                return unauthorized();
            }
            resource_list(vec![
                listed("gpt-5-mini", "gpt-5-mini", "succeeded"),
                listed("prod-mini", "gpt-5-mini", "succeeded"),
                // Azure compares names without case.
                listed("GPT-5-Nano", "gpt-5-nano", "succeeded"),
                // A deployment named after another model is the model it deploys.
                listed("gpt-5", "gpt-5-mini", "succeeded"),
                // Azure's model name differs from the catalog's.
                listed("deepseek-v4-flash", "DeepSeek-V4-Flash-2026", "succeeded"),
                listed("ft-legal", "gpt-4o-mini.ft-123", "succeeded"),
                listed("pending", "gpt-5", "creating"),
                json!({"id": 42}),
            ])
        },
        |_, _| not_found(),
    )
    .await;
    let list = azure::deployments(
        &reqwest::Client::new(),
        &fake.endpoints(),
        "test-resource",
        &azure::Credential::Key("secret"),
    )
    .await
    .unwrap();
    let mut models = kept(azure::raw(&list));
    models.sort_by(|a, b| a.id.cmp(&b.id));
    let got: Vec<(String, String, String, u64)> = models
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                m.model_id.clone(),
                m.name.clone(),
                m.limit.context,
            )
        })
        .collect();
    let ctx = |id: &str| template(id).limit.context;
    assert_eq!(
        got,
        [
            (
                "deepseek-v4-flash".into(),
                "deepseek-v4-flash".into(),
                "DeepSeek-V4-Flash".into(),
                ctx("deepseek-v4-flash")
            ),
            (
                "gpt-5".into(),
                "gpt-5".into(),
                "GPT-5 Mini (gpt-5)".into(),
                ctx("gpt-5-mini")
            ),
            (
                "gpt-5-mini".into(),
                "gpt-5-mini".into(),
                "GPT-5 Mini".into(),
                ctx("gpt-5-mini")
            ),
            (
                "gpt-5-nano".into(),
                "GPT-5-Nano".into(),
                "GPT-5 Nano".into(),
                ctx("gpt-5-nano")
            ),
            (
                "prod-mini".into(),
                "prod-mini".into(),
                "GPT-5 Mini (prod-mini)".into(),
                ctx("gpt-5-mini")
            ),
        ]
    );
    // The resource entered with the key reaches the catalog's endpoints.
    let base = models[0]
        .settings
        .as_ref()
        .unwrap()
        .base_url
        .clone()
        .unwrap();
    assert_eq!(
        azure::expand(&base, "test-resource"),
        "https://test-resource.services.ai.azure.com/models"
    );
}

#[tokio::test]
async fn oc_azure_lists_azure_cli_deployments_through_the_management_api_before_the_resources_own_list(
) {
    let cli = FakeCli::new();
    let fake = fake_azure(
        |_| resource_list(vec![listed("from-resource", "gpt-5-nano", "succeeded")]),
        |r, _| {
            if r.authorization.as_deref() != Some(MANAGEMENT_BEARER) {
                return unauthorized();
            }
            management_page(
                vec![
                    managed("gpt-production", "gpt-5-mini", "Succeeded"),
                    managed("gpt-5-nano", "gpt-5-nano", "Failed"),
                ],
                None,
            )
        },
    )
    .await;
    let c = cli.cli();
    let list = azure::deployments(
        &reqwest::Client::new(),
        &fake.endpoints(),
        "test-resource",
        &azure::Credential::Cli(&c),
    )
    .await
    .unwrap();
    let got: Vec<(String, String)> = kept(azure::raw(&list))
        .into_iter()
        .map(|m| (m.id, m.name))
        .collect();
    assert_eq!(
        got,
        [(
            "gpt-production".to_string(),
            "GPT-5 Mini (gpt-production)".to_string()
        )]
    );
    let graph = &fake.seen()[0];
    assert_eq!(graph.method, "POST");
    assert!(graph
        .path
        .starts_with("/providers/Microsoft.ResourceGraph/resources"));
}

#[tokio::test]
async fn oc_azure_falls_back_to_the_resources_own_list_when_the_azure_cli_cannot_read_the_management_api(
) {
    let cli = FakeCli::new();
    let fake = fake_azure(
        |r| {
            if r.authorization.as_deref()
                == Some("Bearer https://cognitiveservices.azure.com/.default-token")
            {
                resource_list(vec![listed("gpt-5-mini", "gpt-5-mini", "succeeded")])
            } else {
                unauthorized()
            }
        },
        |_, _| unauthorized(),
    )
    .await;
    let c = cli.cli();
    let list = azure::deployments(
        &reqwest::Client::new(),
        &fake.endpoints(),
        "test-resource",
        &azure::Credential::Cli(&c),
    )
    .await
    .unwrap();
    assert_eq!(list, [("gpt-5-mini".to_string(), "gpt-5-mini".to_string())]);
}

fn key_provider(key: &str) -> codesmith_ai::Provider {
    codesmith_ai::Provider {
        protocol: codesmith_ai::Protocol::OpenaiResponses,
        name: "azure".into(),
        base_url: "https://unused".into(),
        api_key: Some(key.into()),
        headers: vec![],
        query: vec![],
        auth: codesmith_ai::Auth::Header("api-key".into()),
        encoding: Default::default(),
        compat: Default::default(),
        hooks: None,
        bifrost_route: None,
    }
}

#[tokio::test]
async fn oc_azure_keeps_the_catalog_for_a_custom_endpoint_without_listing_deployments() {
    let fake = fake_azure(
        |_| resource_list(vec![listed("gpt-5-mini", "gpt-5-mini", "succeeded")]),
        |_, _| not_found(),
    )
    .await;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("live.json");
    let e = live::refresh_azure(
        &reqwest::Client::new(),
        &key_provider("secret"),
        &settings(
            json!({"baseURL": "https://gateway.example/azure", "resourceName": "test-resource"}),
        ),
        &fake.endpoints(),
        &file,
        None,
        &env(&[]),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("custom Azure endpoint"), "{e}");
    assert!(fake.seen().is_empty());
    assert!(live::load_in(&file, "azure").is_none());
}

#[tokio::test]
async fn oc_azure_publishes_the_management_list_only_after_its_last_page() {
    let cli = FakeCli::new();
    let fake = fake_azure(
        |_| not_found(),
        |r, origin| {
            if r.path.starts_with("/page-2") {
                management_page(vec![managed("nano", "gpt-5-nano", "Succeeded")], None)
            } else {
                management_page(
                    vec![managed("mini", "gpt-5-mini", "Succeeded")],
                    Some(format!("{origin}/page-2")),
                )
            }
        },
    )
    .await;
    let c = cli.cli();
    let list = azure::deployments(
        &reqwest::Client::new(),
        &fake.endpoints(),
        "test-resource",
        &azure::Credential::Cli(&c),
    )
    .await
    .unwrap();
    let names: Vec<&str> = list.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(names, ["mini", "nano"]);
}

#[tokio::test]
async fn oc_azure_keeps_the_last_complete_list_when_a_later_page_fails() {
    let cli = FakeCli::new();
    let fake = fake_azure(
        |_| not_found(),
        |r, origin| {
            if r.path.starts_with("/page-2") {
                (503, "Unavailable".into())
            } else {
                management_page(
                    vec![managed("nano", "gpt-5-nano", "Succeeded")],
                    Some(format!("{origin}/page-2")),
                )
            }
        },
    )
    .await;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("live.json");
    live::save_in(
        &file,
        "azure",
        "a",
        "",
        azure::raw(&[("mini".into(), "gpt-5-mini".into())]),
    )
    .unwrap();
    let login = json!({"type": "external", "method": "azure-cli", "metadata": {"resourceName": "test-resource"}});
    let r = live::refresh_azure(
        &reqwest::Client::new(),
        &key_provider("unused"),
        &settings(json!({})),
        &fake.endpoints(),
        &file,
        Some(&login),
        &env(&[("PATH", &cli.path())]),
    )
    .await;
    assert!(r.is_err());
    let names: Vec<String> = live::derive(
        "azure",
        &live::load_in(&file, "azure").unwrap(),
        &azure_templates(),
    )
    .into_iter()
    .map(|m| m.id)
    .collect();
    assert_eq!(names, ["mini"]);
}

#[tokio::test]
async fn oc_azure_reports_the_management_api_error_when_the_resources_own_list_fails_too() {
    let cli = FakeCli::new();
    let fake = fake_azure(|_| not_found(), |_, _| (503, "Unavailable".into())).await;
    let c = cli.cli();
    let e = azure::deployments(
        &reqwest::Client::new(),
        &fake.endpoints(),
        "test-resource",
        &azure::Credential::Cli(&c),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("503"), "{e}");
    let resource_asks = fake
        .seen()
        .iter()
        .filter(|r| r.path.starts_with("/openai/deployments"))
        .count();
    assert_eq!(resource_asks, 1);
}

#[tokio::test]
async fn oc_azure_never_sends_the_management_token_to_a_page_link_outside_the_management_endpoint()
{
    let cli = FakeCli::new();
    let fake = fake_azure(
        |_| not_found(),
        |_, _| {
            management_page(
                vec![managed("mini", "gpt-5-mini", "Succeeded")],
                Some("https://attacker.example/page-2".into()),
            )
        },
    )
    .await;
    let c = cli.cli();
    let e = azure::deployments(
        &reqwest::Client::new(),
        &fake.endpoints(),
        "test-resource",
        &azure::Credential::Cli(&c),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("outside the management endpoint"), "{e}");
    // It fell back to the resource's own list, which failed too.
    assert!(fake
        .seen()
        .iter()
        .any(|r| r.path.starts_with("/openai/deployments")));
}

#[test]
fn oc_azure_keeps_a_model_that_is_configured_explicitly() {
    let mut c: Config = serde_json::from_value(json!({"providers": {"azure": {
        "settings": {"resourceName": "test-resource", "apiKey": "k"},
        "models": {"gpt-5-nano": {"id": "nano-production"}},
    }}}))
    .unwrap();
    c.models_dev = Some(Arc::new(codesmith_core::catalog::ModelsDev::fixed(
        (*codesmith_core::catalog::bundled()).clone(),
    )));
    let m = c.resolve_model("azure/gpt-5-nano").unwrap();
    assert_eq!(m.model_id, "nano-production");
    assert_eq!(m.context, Some(template("gpt-5-nano").limit.context));
}

// AzurePlugin switching accounts

#[tokio::test]
async fn oc_azure_loads_again_when_the_api_key_changes_in_place() {
    let fake = fake_azure(
        |r| {
            let name = if r.key.as_deref() == Some("secret") {
                "production-a"
            } else {
                "production-b"
            };
            resource_list(vec![listed(name, "gpt-5-mini", "succeeded")])
        },
        |_, _| not_found(),
    )
    .await;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("live.json");
    let s = settings(json!({"resourceName": "test-resource"}));
    let refresh = |key: &'static str| {
        let (fake, file, s) = (&fake, &file, &s);
        async move {
            live::refresh_azure(
                &reqwest::Client::new(),
                &key_provider(key),
                s,
                &fake.endpoints(),
                file,
                None,
                &env(&[]),
            )
            .await
            .unwrap()
        }
    };
    assert_eq!(refresh("secret").await, ["production-a"]);
    let first = live::load_in(&file, "azure").unwrap()["account"].clone();
    assert_eq!(refresh("rotated-key").await, ["production-b"]);
    let kept = live::load_in(&file, "azure").unwrap();
    assert_ne!(kept["account"], first, "kept for the new key");
    assert!(!kept.to_string().contains("rotated-key"), "no key is kept");
}

// AzurePlugin requests

#[tokio::test]
async fn oc_azure_authorizes_azure_foundry_and_websocket_requests_with_a_token_for_their_audience()
{
    use codesmith_ai::Authorizer;
    let cli = FakeCli::new();
    let auth = azure::CliAuth(Arc::new(cli.cli()));
    let client = reqwest::Client::new();
    let mut req = client
        .post("https://test-resource.openai.azure.com/openai/v1/responses")
        .header("api-key", "stored-token")
        .header("x-keep", "yes")
        .build()
        .unwrap();
    auth.authorize(&client, &mut req).await.unwrap();
    assert_eq!(
        req.headers()["authorization"],
        "Bearer https://cognitiveservices.azure.com/.default-token"
    );
    assert!(req.headers().get("api-key").is_none());
    assert_eq!(req.headers()["x-keep"], "yes");
    let mut req = client
        .post("https://test-resource.services.ai.azure.com/anthropic/v1/messages")
        .header("x-api-key", "stored-token")
        .build()
        .unwrap();
    auth.authorize(&client, &mut req).await.unwrap();
    assert_eq!(
        req.headers()["authorization"],
        "Bearer https://ai.azure.com/.default-token"
    );
    assert!(req.headers().get("x-api-key").is_none());
    // Foundry's model inference API is a Cognitive Services audience.
    assert_eq!(
        azure::scope_for(
            &"https://r.services.ai.azure.com/models/chat"
                .parse()
                .unwrap()
        ),
        azure::COGNITIVE_SCOPE
    );
    // Tokens are reused per scope.
    assert_eq!(cli.calls().len(), 2);
    let mut req = client
        .post("https://test-resource.openai.azure.com/openai/v1/responses")
        .build()
        .unwrap();
    auth.authorize(&client, &mut req).await.unwrap();
    assert_eq!(cli.calls().len(), 2);
}

// AzurePlugin resource name

#[test]
fn oc_azure_resource_name_sources() {
    let key_login =
        json!({"type": "api", "key": "secret", "metadata": {"resourceName": "test-resource"}});
    let imported =
        json!({"type": "api", "key": "secret", "metadata": {"resourceName": "imported-resource"}});
    type Case<'a> = (
        &'a str,
        Vec<(&'a str, &'a str)>,
        Value,
        Option<Value>,
        &'a str,
    );
    let cases: Vec<Case> = vec![
        (
            "reads AZURE_RESOURCE_NAME",
            vec![("AZURE_RESOURCE_NAME", "from-env")],
            json!({}),
            None,
            "from-env",
        ),
        (
            "reads the legacy AZURE_COGNITIVE_SERVICES_RESOURCE_NAME",
            vec![("AZURE_COGNITIVE_SERVICES_RESOURCE_NAME", "legacy-resource")],
            json!({}),
            None,
            "legacy-resource",
        ),
        (
            "prefers configuration over the environment",
            vec![("AZURE_RESOURCE_NAME", "from-env")],
            json!({"resourceName": "from-config"}),
            None,
            "from-config",
        ),
        (
            "ignores a whitespace configured name",
            vec![("AZURE_RESOURCE_NAME", "from-env")],
            json!({"resourceName": "   "}),
            None,
            "from-env",
        ),
        (
            "prefers the connection's resource over configuration, as requests do",
            vec![("AZURE_RESOURCE_NAME", "from-env")],
            json!({"resourceName": "from-config"}),
            Some(key_login),
            "test-resource",
        ),
        (
            "reads the resource of an API key imported from V1",
            vec![("AZURE_RESOURCE_NAME", "from-env")],
            json!({}),
            Some(imported),
            "imported-resource",
        ),
    ];
    for (name, e, s, stored, expected) in cases {
        assert_eq!(
            azure::resource_name(&settings(s), stored.as_ref(), &env(&e)).as_deref(),
            Some(expected),
            "{name}"
        );
    }
}

#[test]
fn oc_azure_leaves_models_unchanged_and_uncloned_without_a_resource_name() {
    assert_eq!(
        azure::resource_name(&settings(json!({})), None, &env(&[])),
        None
    );
    // Without a resource the catalog's template stays as it is.
    let base = template("deepseek-v4-flash")
        .settings
        .unwrap()
        .base_url
        .unwrap();
    assert_eq!(
        base,
        "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/models"
    );
}

#[test]
fn oc_azure_expands_provider_and_model_resource_urls() {
    let c: Config = serde_json::from_value(json!({"providers": {"azure": {
        "protocol": "openai-chat",
        "settings": {
            "resourceName": "from-config",
            "apiKey": "k",
            "baseURL": "https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.cognitiveservices.azure.com/openai",
        },
    }}}))
    .unwrap();
    let m = c.resolve_model("azure/gpt-5-mini").unwrap();
    assert!(
        m.provider
            .base_url
            .starts_with("https://from-config.cognitiveservices.azure.com/openai"),
        "{}",
        m.provider.base_url
    );
    assert_eq!(
        azure::expand(
            "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1",
            "model-resource"
        ),
        "https://model-resource.services.ai.azure.com/anthropic/v1"
    );
}

// AzurePlugin transport

#[test]
fn oc_azure_prefers_websockets_on_the_provider_for_the_azure_responses_api_only() {
    let resolve = |extra: Value| {
        let mut s = json!({"resourceName": "r", "apiKey": "k"});
        let mut provider = json!({});
        for (k, v) in extra.as_object().unwrap() {
            if k == "protocol" {
                provider["protocol"] = v.clone();
            } else {
                s[k] = v.clone();
            }
        }
        provider["settings"] = s;
        let c: Config = serde_json::from_value(json!({"providers": {"azure": provider}})).unwrap();
        c.resolve_model("azure/gpt-5-mini")
            .unwrap()
            .websocket
            .is_some()
    };
    assert!(resolve(json!({})), "Responses on a v1 resource endpoint");
    assert!(!resolve(json!({"protocol": "openai-chat"})));
    assert!(!resolve(json!({"apiVersion": "2025-04-01-preview"})));
    assert!(!resolve(json!({"useDeploymentBasedUrls": true})));
    assert!(!resolve(
        json!({"baseURL": "https://gateway.example/azure"})
    ));
    assert!(!resolve(json!({"protocol": "anthropic-messages"})));
}

/// Not in OpenCode: the resource id from the Resource Graph is the path of
/// a URL the management token goes to, so it must be a plain ARM path that
/// keeps the URL on the management endpoint.
#[test]
fn a_resource_id_must_keep_the_management_token_at_home() {
    use codesmith_core::providers::azure::deployments_url;
    let m = "https://management.azure.com";
    let id = "/subscriptions/s/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/a";
    assert_eq!(
        deployments_url(m, id).unwrap(),
        format!("{m}{id}/deployments?api-version=2024-10-01")
    );
    for bad in [
        "@evil.example/subscriptions/x",
        "/subscriptions/x@evil.example",
        ".evil.example/subscriptions/x",
        "//evil.example/subscriptions/x",
        "/subscriptions//evil.example",
        "/subscriptions/../../x",
        "/subscriptions/x?y=1",
        "/subscriptions/x#y",
        "/subscriptions/x\\y",
        "/subscriptions/x%2F..",
        "/subscriptions/x y",
        "/tenants/x",
        "",
    ] {
        assert!(deployments_url(m, bad).is_err(), "{bad}");
    }
}
