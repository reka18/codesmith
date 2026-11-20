//! Ports of OpenCode's local model discovery tests
//! (`plugin/provider-lmstudio.test.ts`, `plugin/provider-ollama.test.ts`,
//! `plugin/provider-vllm.test.ts`). OpenCode polls in the background;
//! CodeSmith discovers when models are listed, so each test drives one
//! discovery at a time.

use codesmith_core::config::Config;
use codesmith_core::providers::live;
use codesmith_core::providers::local::{self, Kind};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A request the fixture got.
#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Value,
}

type Handler = Arc<dyn Fn(&Req) -> (u16, String) + Send + Sync>;

struct Server {
    origin: String,
    seen: Arc<Mutex<Vec<Req>>>,
}

impl Server {
    fn seen(&self) -> Vec<Req> {
        self.seen.lock().unwrap().clone()
    }
}

async fn serve(handler: Handler) -> Server {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut s, _)) = l.accept().await {
            let log = log.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 65536];
                let (head, body) = loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
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
                            break (text[..i].to_string(), text[i + 4..].to_string());
                        }
                    }
                };
                let mut words = head.split_whitespace();
                let req = Req {
                    method: words.next().unwrap_or("").into(),
                    path: words.next().unwrap_or("").into(),
                    authorization: head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_string())
                    }),
                    body: serde_json::from_str(&body).unwrap_or(Value::Null),
                };
                log.lock().unwrap().push(req.clone());
                let (status, body) = handler(&req);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    Server { origin, seen }
}

fn json_handler(v: Value) -> Handler {
    Arc::new(move |_| (200, v.to_string()))
}

async fn refresh(
    path: &Path,
    id: &str,
    kind: Kind,
    base: &str,
    key: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    local::refresh(&reqwest::Client::new(), path, id, kind, base, key).await
}

fn kept(path: &Path, id: &str) -> Vec<codesmith_core::catalog::Model> {
    live::derive(id, &live::load_in(path, id).unwrap(), &[])
}

fn find<'a>(
    models: &'a [codesmith_core::catalog::Model],
    id: &str,
) -> Option<&'a codesmith_core::catalog::Model> {
    models.iter().find(|m| m.id == id)
}

fn lmstudio_model(key: &str, context: i64) -> Value {
    json!({"type": "llm", "key": key, "display_name": key, "loaded_instances": [], "max_context_length": context})
}

// plugin/provider-lmstudio.test.ts

#[test]
fn oc_lmstudio_is_registered_as_a_built_in_provider_plugin() {
    let s = Default::default();
    assert_eq!(local::kind("lmstudio", &s), Some(Kind::LmStudio));
    assert_eq!(local::kind("ollama", &s), Some(Kind::Ollama));
    assert_eq!(local::kind("vllm", &s), Some(Kind::Vllm));
    assert_eq!(local::kind("llamacpp", &s), Some(Kind::OpenAi));
    let c = Config::default();
    for (id, url) in [
        ("lmstudio", "http://127.0.0.1:1234/v1"),
        ("ollama", "http://127.0.0.1:11434/v1"),
        ("vllm", "http://127.0.0.1:8000/v1"),
        ("llamacpp", "http://127.0.0.1:8080/v1"),
    ] {
        let p = c.provider(id).unwrap();
        assert_eq!(p.settings.base_url.as_deref(), Some(url), "{id}");
        assert!(codesmith_core::config::is_loopback_url(url));
    }
    assert!(!codesmith_core::config::is_loopback_url(
        "http://192.168.1.4:1234/v1"
    ));
    assert!(codesmith_core::config::is_loopback_url(
        "http://localhost:1234/v1"
    ));
    assert!(codesmith_core::config::is_loopback_url(
        "http://[::1]:1234/v1"
    ));
}

#[tokio::test]
async fn oc_lmstudio_discovers_local_language_models_with_their_capabilities_and_effective_context()
{
    let s = serve(json_handler(json!({"models": [
        {"type": "llm", "key": "google/gemma-4-26b-a4b", "display_name": "Gemma 4 26B A4B", "architecture": "gemma4",
         "loaded_instances": [{"config": {"context_length": 32768}}, {"config": {"context_length": 16384}}],
         "max_context_length": 262144, "capabilities": {"vision": true, "trained_for_tool_use": true}},
        {"type": "llm", "key": "deepseek-r1", "display_name": "DeepSeek R1", "architecture": "deepseek",
         "loaded_instances": [], "max_context_length": 131072, "capabilities": {"vision": false, "trained_for_tool_use": false}},
        {"type": "llm", "key": "unknown-context", "display_name": "Unknown Context", "loaded_instances": [], "max_context_length": 0},
        {"type": "embedding", "key": "nomic-embed", "display_name": "Nomic Embed", "loaded_instances": [], "max_context_length": 2048},
        {"type": "decision", "key": "clef-flash", "display_name": "Clef Flash", "loaded_instances": [], "max_context_length": null, "has_vision_adapter": true}
    ]})))
    .await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    let ids = refresh(
        &path,
        "lmstudio",
        Kind::LmStudio,
        &format!("{}/v1", s.origin),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        ids,
        ["deepseek-r1", "google/gemma-4-26b-a4b", "unknown-context"]
    );
    assert_eq!(s.seen()[0].path, "/api/v1/models");
    assert_eq!(s.seen()[0].authorization, None);
    let m = kept(&path, "lmstudio");
    let gemma = find(&m, "google/gemma-4-26b-a4b").unwrap();
    assert_eq!(gemma.family.as_deref(), Some("gemma4"));
    assert_eq!(gemma.name, "Gemma 4 26B A4B");
    assert!(gemma.capabilities.tools);
    assert_eq!(gemma.capabilities.input, ["text", "image"]);
    assert_eq!(gemma.capabilities.output, ["text"]);
    assert_eq!((gemma.limit.context, gemma.limit.output), (16_384, 32_000));
    let r1 = find(&m, "deepseek-r1").unwrap();
    assert!(!r1.capabilities.tools);
    assert_eq!(r1.capabilities.input, ["text"]);
    assert_eq!(r1.limit.context, 131_072);
    assert_eq!(find(&m, "unknown-context").unwrap().limit.context, 200_000);
    assert!(find(&m, "nomic-embed").is_none());
    let clef = find(&m, "clef-flash").unwrap();
    assert_eq!(clef.kind, "decision");
    assert_eq!(clef.name, "Clef Flash");
    assert!(!clef.capabilities.tools);
    assert_eq!(clef.capabilities.input, ["text", "image"]);
    assert_eq!(clef.capabilities.output, ["decision"]);
    assert_eq!(clef.limit.context, 200_000);
}

#[tokio::test]
async fn oc_lmstudio_refreshes_the_catalog_when_lm_studio_models_change() {
    let models = Arc::new(Mutex::new(Vec::<Value>::new()));
    let m2 = models.clone();
    let s = serve(Arc::new(move |_| {
        (200, json!({"models": *m2.lock().unwrap()}).to_string())
    }))
    .await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    let base = format!("{}/v1", s.origin);
    assert!(refresh(&path, "lmstudio", Kind::LmStudio, &base, None)
        .await
        .unwrap()
        .is_empty());
    models.lock().unwrap().push(json!({
        "type": "llm", "key": "qwen/qwen3-coder", "display_name": "Qwen 3 Coder", "architecture": "qwen3",
        "loaded_instances": [], "max_context_length": 65536,
        "capabilities": {"vision": false, "trained_for_tool_use": true}
    }));
    refresh(&path, "lmstudio", Kind::LmStudio, &base, None)
        .await
        .unwrap();
    assert_eq!(
        find(&kept(&path, "lmstudio"), "qwen/qwen3-coder")
            .unwrap()
            .name,
        "Qwen 3 Coder"
    );
    models.lock().unwrap().clear();
    refresh(&path, "lmstudio", Kind::LmStudio, &base, None)
        .await
        .unwrap();
    assert!(kept(&path, "lmstudio").is_empty());
}

#[tokio::test]
async fn oc_lmstudio_discovers_from_configured_endpoints_with_bearer_authentication() {
    let s = serve(json_handler(
        json!({"models": [lmstudio_model("configured-model", 32768)]}),
    ))
    .await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    let base = format!("{}/proxy/v1", s.origin);
    refresh(&path, "lmstudio", Kind::LmStudio, &base, Some("secret"))
        .await
        .unwrap();
    refresh(&path, "lmstudio", Kind::LmStudio, &base, None)
        .await
        .unwrap();
    let api = format!("{}/proxy/api/v1", s.origin);
    refresh(&path, "lmstudio", Kind::LmStudio, &api, Some("api-secret"))
        .await
        .unwrap();
    let seen: Vec<(String, Option<String>)> = s
        .seen()
        .into_iter()
        .map(|r| (r.path, r.authorization))
        .collect();
    assert_eq!(
        seen,
        [
            (
                "/proxy/api/v1/models".to_string(),
                Some("Bearer secret".to_string())
            ),
            ("/proxy/api/v1/models".to_string(), None),
            (
                "/proxy/api/v1/models".to_string(),
                Some("Bearer api-secret".to_string())
            ),
        ]
    );
    // The list is kept with the base URL it was discovered from.
    assert_eq!(
        live::load_in(&path, "lmstudio").unwrap()["baseURL"],
        api.as_str()
    );
}

static GLOBAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Tests that read the data folder's kept lists through `Config` take
/// this lock and get their own data folder.
async fn data_folder() -> (tokio::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
    let g = GLOBAL.lock().await;
    let d = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_DATA_HOME", d.path());
    (g, d)
}

async fn discover(c: &Config, id: &str) -> Vec<String> {
    local::discover_providers(c, &live::file(), &[id.to_string()]).await
}

fn config(v: Value) -> Config {
    let mut c: Config = serde_json::from_value(v).unwrap();
    c.models_dev = Some(Arc::new(codesmith_core::catalog::ModelsDev::fixed(
        (*codesmith_core::catalog::bundled()).clone(),
    )));
    c
}

#[tokio::test]
async fn oc_lmstudio_discovers_with_a_configured_key_and_keeps_sending_it() {
    let (_g, _d) = data_folder().await;
    let s = serve(json_handler(
        json!({"models": [lmstudio_model("keyed-model", 8192)]}),
    ))
    .await;
    let c = config(json!({"providers": {"lmstudio": {"settings": {
        "baseURL": format!("{}/v1", s.origin), "apiKey": "saved-key"
    }}}}));
    assert_eq!(discover(&c, "lmstudio").await, ["lmstudio"]);
    assert_eq!(discover(&c, "lmstudio").await, ["lmstudio"]);
    let lm: Vec<Req> = s.seen();
    assert_eq!(lm.len(), 2);
    assert!(lm
        .iter()
        .all(|r| r.authorization.as_deref() == Some("Bearer saved-key")));
    assert_eq!(c.list_models("lmstudio", &[]), ["keyed-model"]);
}

#[tokio::test]
async fn oc_lmstudio_replaces_the_models_dev_catalog_when_discovery_succeeds() {
    let (_g, _d) = data_folder().await;
    let models = Arc::new(Mutex::new(vec![lmstudio_model("discovered-model", 8192)]));
    let m2 = models.clone();
    let s = serve(Arc::new(move |_| {
        (200, json!({"models": *m2.lock().unwrap()}).to_string())
    }))
    .await;
    let c = config(
        json!({"providers": {"lmstudio": {"settings": {"baseURL": format!("{}/v1", s.origin)}}}}),
    );
    // Before discovery the catalog's LM Studio models are listed.
    let before = c.list_models("lmstudio", &[]);
    assert!(
        before.iter().any(|m| m == "qwen/qwen3-30b-a3b-2507"),
        "{before:?}"
    );
    discover(&c, "lmstudio").await;
    assert_eq!(c.list_models("lmstudio", &[]), ["discovered-model"]);
    let r = c.resolve_model("lmstudio/discovered-model").unwrap();
    assert_eq!(r.provider.base_url, format!("{}/v1", s.origin));
    // An empty answer restores the catalog's.
    models.lock().unwrap().clear();
    discover(&c, "lmstudio").await;
    assert_eq!(c.list_models("lmstudio", &[]), before);
    // A list kept for another endpoint is not used.
    models
        .lock()
        .unwrap()
        .push(lmstudio_model("discovered-model", 8192));
    discover(&c, "lmstudio").await;
    let moved = config(
        json!({"providers": {"lmstudio": {"settings": {"baseURL": "http://127.0.0.1:9/v1"}}}}),
    );
    assert_eq!(moved.list_models("lmstudio", &[]), before);
}

// plugin/provider-ollama.test.ts

fn ollama_handler(state: Arc<Mutex<Value>>) -> Handler {
    Arc::new(move |r: &Req| {
        let st = state.lock().unwrap();
        if st["fail"] == true {
            return (500, "{}".into());
        }
        match r.path.as_str() {
            p if p.ends_with("/api/tags") => (200, json!({"models": st["tags"]}).to_string()),
            p if p.ends_with("/api/show") => {
                let name = r.body["model"].as_str().unwrap_or("");
                match st["show"].get(name) {
                    Some(v) => (200, v.to_string()),
                    None => (404, "{}".into()),
                }
            }
            _ => (404, "{}".into()),
        }
    })
}

fn tag(model: &str, digest: &str) -> Value {
    json!({"name": model, "model": model, "modified_at": "2026-01-01T00:00:00Z", "size": 1, "digest": digest,
           "details": {"format": "gguf", "family": "tagfamily", "parameter_size": "4B", "quantization_level": "Q4"}})
}

#[tokio::test]
async fn oc_ollama_discovers_local_completion_models_and_native_metadata() {
    let state = Arc::new(Mutex::new(json!({
        "tags": [tag("gemma3:4b", "a"), tag("unknown-context", "b"), tag("nomic-embed", "c"), tag("removed-model", "d")],
        "show": {
            "gemma3:4b": {"capabilities": ["completion", "tools", "vision"], "details": {"family": "gemma3", "format": "gguf", "parameter_size": "4B", "quantization_level": "Q4"},
                          "model_info": {"gemma3.context_length": 131072}},
            "unknown-context": {"capabilities": ["completion"]},
            "nomic-embed": {"capabilities": ["embedding"]}
        }
    })));
    let s = serve(ollama_handler(state)).await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    let ids = refresh(
        &path,
        "ollama",
        Kind::Ollama,
        &format!("{}/v1", s.origin),
        None,
    )
    .await
    .unwrap();
    assert_eq!(ids, ["gemma3:4b", "unknown-context"]);
    let m = kept(&path, "ollama");
    let g = find(&m, "gemma3:4b").unwrap();
    assert_eq!(g.family.as_deref(), Some("gemma3"));
    assert!(g.capabilities.tools);
    assert_eq!(g.capabilities.input, ["text", "image"]);
    assert_eq!(g.limit.context, 131_072);
    let u = find(&m, "unknown-context").unwrap();
    assert_eq!(u.limit.context, 200_000);
    assert_eq!(u.family.as_deref(), Some("tagfamily"));
    assert!(find(&m, "nomic-embed").is_none());
    let seen = s.seen();
    assert!(seen
        .iter()
        .any(|r| r.method == "GET" && r.path == "/api/tags"));
    for name in ["gemma3:4b", "nomic-embed", "removed-model"] {
        assert!(seen
            .iter()
            .any(|r| r.method == "POST" && r.path == "/api/show" && r.body["model"] == name));
    }
}

#[tokio::test]
async fn oc_ollama_refreshes_changed_digests_and_retains_inventory_through_transient_failures() {
    let state = Arc::new(Mutex::new(json!({
        "tags": [tag("qwen3", "one")],
        "show": {"qwen3": {"capabilities": ["completion"], "model_info": {"qwen3.context_length": 32768}}}
    })));
    let s = serve(ollama_handler(state.clone())).await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    let base = s.origin.clone();
    let shows = || s.seen().iter().filter(|r| r.path == "/api/show").count();
    refresh(&path, "ollama", Kind::Ollama, &base, None)
        .await
        .unwrap();
    refresh(&path, "ollama", Kind::Ollama, &base, None)
        .await
        .unwrap();
    assert_eq!(shows(), 1, "an unchanged digest is not shown again");
    {
        let mut st = state.lock().unwrap();
        st["tags"] = json!([tag("qwen3", "two")]);
        st["show"]["qwen3"]["model_info"] = json!({"qwen3.context_length": 65536});
    }
    refresh(&path, "ollama", Kind::Ollama, &base, None)
        .await
        .unwrap();
    assert_eq!(shows(), 2);
    state.lock().unwrap()["fail"] = json!(true);
    assert!(refresh(&path, "ollama", Kind::Ollama, &base, None)
        .await
        .is_err());
    assert_eq!(
        find(&kept(&path, "ollama"), "qwen3").unwrap().limit.context,
        65_536
    );
}

#[tokio::test]
async fn oc_ollama_replaces_and_restores_the_same_id_catalog_provider() {
    let (_g, _d) = data_folder().await;
    let state = Arc::new(Mutex::new(json!({
        "tags": [tag("discovered-model", "a")],
        "show": {"discovered-model": {"capabilities": ["completion"]}}
    })));
    let s = serve(ollama_handler(state.clone())).await;
    let c = config(
        json!({"providers": {"ollama": {"models": {"static-model": {}}, "settings": {"baseURL": format!("{}/v1", s.origin)}}}}),
    );
    assert_eq!(c.list_models("ollama", &[]), ["static-model"]);
    discover(&c, "ollama").await;
    assert_eq!(
        c.list_models("ollama", &[]),
        ["discovered-model", "static-model"]
    );
    state.lock().unwrap()["tags"] = json!([]);
    discover(&c, "ollama").await;
    assert_eq!(c.list_models("ollama", &[]), ["static-model"]);
}

#[tokio::test]
async fn oc_ollama_reloads_layered_endpoint_and_bearer_authentication_settings() {
    let state = Arc::new(Mutex::new(json!({
        "tags": [tag("m", "a")],
        "show": {"m": {"capabilities": ["completion"]}}
    })));
    let s = serve(ollama_handler(state)).await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    refresh(
        &path,
        "ollama",
        Kind::Ollama,
        &format!("{}/proxy/v1", s.origin),
        Some("secret"),
    )
    .await
    .unwrap();
    refresh(
        &path,
        "ollama",
        Kind::Ollama,
        &format!("{}/proxy/api", s.origin),
        None,
    )
    .await
    .unwrap();
    let seen: Vec<(String, String, Option<String>)> = s
        .seen()
        .into_iter()
        .map(|r| (r.method, r.path, r.authorization))
        .collect();
    let b = |s: &str| Some(s.to_string());
    assert!(seen.contains(&("GET".into(), "/proxy/api/tags".into(), b("Bearer secret"))));
    assert!(seen.contains(&("POST".into(), "/proxy/api/show".into(), b("Bearer secret"))));
    assert!(seen.contains(&("GET".into(), "/proxy/api/tags".into(), None)));
    assert!(seen.contains(&("POST".into(), "/proxy/api/show".into(), None)));
}

// plugin/provider-vllm.test.ts

fn vllm_model(id: &str, max_model_len: i64, owned_by: &str) -> Value {
    json!({"id": id, "object": "model", "created": 1, "owned_by": owned_by, "root": id, "parent": null, "max_model_len": max_model_len, "permission": []})
}

#[tokio::test]
async fn oc_vllm_waits_for_readiness_and_discovers_official_vllm_model_metadata() {
    let healthy = Arc::new(Mutex::new(false));
    let h = healthy.clone();
    let s = serve(Arc::new(move |r: &Req| {
        if r.path == "/health" {
            return (if *h.lock().unwrap() { 200 } else { 503 }, String::new());
        }
        (
            200,
            json!({"object": "list", "data": [
                vllm_model("Qwen/Qwen3-Coder", 65536, "vllm"),
                vllm_model("unknown-limit", 0, "vllm"),
                vllm_model("foreign-model", 4096, "other"),
            ]})
            .to_string(),
        )
    }))
    .await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    let base = format!("{}/v1", s.origin);
    assert!(refresh(&path, "vllm", Kind::Vllm, &base, None)
        .await
        .is_err());
    assert!(
        s.seen().iter().all(|r| r.path == "/health"),
        "models are not asked before readiness"
    );
    assert!(live::load_in(&path, "vllm").is_none());
    *healthy.lock().unwrap() = true;
    refresh(&path, "vllm", Kind::Vllm, &base, None)
        .await
        .unwrap();
    let m = kept(&path, "vllm");
    let q = find(&m, "Qwen/Qwen3-Coder").unwrap();
    assert_eq!(q.model_id, "Qwen/Qwen3-Coder");
    assert_eq!(q.name, "Qwen/Qwen3-Coder");
    assert!(!q.capabilities.tools);
    assert_eq!(q.capabilities.input, ["text"]);
    assert_eq!((q.limit.context, q.limit.output), (65_536, 32_000));
    assert_eq!(find(&m, "unknown-limit").unwrap().limit.context, 200_000);
    assert!(find(&m, "foreign-model").is_none());
}

#[tokio::test]
async fn oc_vllm_refreshes_inventory_while_retaining_the_last_success_through_transient_failures() {
    let fail = Arc::new(Mutex::new(false));
    let f = fail.clone();
    let s = serve(Arc::new(move |_r: &Req| {
        if *f.lock().unwrap() {
            return (500, "{}".into());
        }
        (
            200,
            json!({"data": [vllm_model("kept", 4096, "vllm")]}).to_string(),
        )
    }))
    .await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    refresh(&path, "vllm", Kind::Vllm, &s.origin, None)
        .await
        .unwrap();
    *fail.lock().unwrap() = true;
    assert!(refresh(&path, "vllm", Kind::Vllm, &s.origin, None)
        .await
        .is_err());
    assert_eq!(
        find(&kept(&path, "vllm"), "kept").unwrap().limit.context,
        4096
    );
}

#[tokio::test]
async fn oc_vllm_reloads_layered_custom_endpoint_and_bearer_authentication_settings() {
    let s = serve(json_handler(
        json!({"data": [vllm_model("m", 4096, "vllm")]}),
    ))
    .await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    refresh(
        &path,
        "vllm",
        Kind::Vllm,
        &format!("{}/proxy/v1", s.origin),
        Some("secret"),
    )
    .await
    .unwrap();
    let seen: Vec<(String, Option<String>)> = s
        .seen()
        .into_iter()
        .map(|r| (r.path, r.authorization))
        .collect();
    assert_eq!(
        seen,
        [
            (
                "/proxy/health".to_string(),
                Some("Bearer secret".to_string())
            ),
            (
                "/proxy/v1/models".to_string(),
                Some("Bearer secret".to_string())
            ),
        ]
    );
}

// OpenAI-compatible servers (llama.cpp, oMLX): not in OpenCode.

#[tokio::test]
async fn openai_compatible_servers_list_v1_models_with_their_context() {
    let s = serve(json_handler(json!({"object": "list", "data": [
        {"id": "qwen3-coder", "object": "model", "owned_by": "omlx"},
        {"id": "gemma.gguf", "object": "model", "meta": {"n_ctx_train": 8192}},
        {"id": "big", "object": "model", "context_length": 131072},
    ]})))
    .await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    let ids = refresh(
        &path,
        "omlx",
        Kind::OpenAi,
        &format!("{}/v1", s.origin),
        None,
    )
    .await
    .unwrap();
    assert_eq!(ids, ["big", "gemma.gguf", "qwen3-coder"]);
    assert_eq!(s.seen()[0].path, "/v1/models");
    let m = kept(&path, "omlx");
    assert_eq!(find(&m, "gemma.gguf").unwrap().limit.context, 8192);
    assert_eq!(find(&m, "big").unwrap().limit.context, 131_072);
    assert_eq!(find(&m, "qwen3-coder").unwrap().limit.context, 200_000);
}

#[tokio::test]
async fn a_configured_provider_discovers_only_with_settings_discovery_on_loopback() {
    let (_g, _d) = data_folder().await;
    let s = serve(json_handler(json!({"data": [{"id": "local-model"}]}))).await;
    let c = config(json!({"providers": {
        "omlx": {"settings": {"baseURL": format!("{}/v1", s.origin), "discovery": "openai"}},
        "plain": {"settings": {"baseURL": format!("{}/v1", s.origin)}},
        "remote": {"settings": {"baseURL": "http://192.0.2.1:8000/v1", "discovery": "openai"}}
    }}));
    let ids = local::providers(&c);
    assert!(ids.contains(&"omlx".to_string()) && ids.contains(&"remote".to_string()));
    assert!(!ids.contains(&"plain".to_string()));
    let found =
        local::discover_providers(&c, &live::file(), &["omlx".into(), "remote".into()]).await;
    assert_eq!(found, ["omlx"], "a remote endpoint is not asked unprompted");
    assert_eq!(c.list_models("omlx", &[]), ["local-model"]);
    assert!(c.list_models("plain", &[]).is_empty());
}

#[tokio::test]
async fn the_local_setting_decides_whether_a_server_is_asked_unprompted() {
    let (_g, _d) = data_folder().await;
    let s = serve(json_handler(json!({"data": [{"id": "proxied"}]}))).await;
    // A loopback proxy to a hosted service, marked not local: not asked.
    let c = config(json!({"providers": {
        "proxy": {"local": false, "settings": {"baseURL": format!("{}/v1", s.origin), "discovery": "openai"}},
    }}));
    let found = local::discover_providers(&c, &live::file(), &["proxy".into()]).await;
    assert!(found.is_empty());
    assert!(s.seen().is_empty(), "nothing was sent to it");
    // `local: true` vouches for a server; on loopback it is asked as before.
    let c = config(json!({"providers": {
        "proxy": {"local": true, "settings": {"baseURL": format!("{}/v1", s.origin), "discovery": "openai"}},
    }}));
    let found = local::discover_providers(&c, &live::file(), &["proxy".into()]).await;
    assert_eq!(found, ["proxy"]);
    // Off loopback, only `local: true` lets the listing ask.
    let p = |local: Option<bool>| codesmith_core::config::ProviderConfig {
        local,
        ..Default::default()
    };
    let lan = "http://192.168.1.20:8000/v1";
    assert!(!p(None).probe_unprompted(lan));
    assert!(p(Some(true)).probe_unprompted(lan));
}

#[tokio::test]
async fn unprompted_discovery_sends_the_key_only_to_a_configured_endpoint() {
    let (_g, _d) = data_folder().await;
    // A built-in provider's default port gets no key, even with one set.
    let c = config(json!({"providers": {"ollama": {"settings": {"apiKey": "sk-local"}}}}));
    let p = c.provider("ollama").unwrap();
    assert!(p.settings.base_url.is_some(), "a built-in default endpoint");
    assert_eq!(local::unprompted_key(&c, "ollama", &p), None);
    // The endpoint the user configured does.
    let s = serve(json_handler(json!({"data": [{"id": "m"}]}))).await;
    let c = config(json!({"providers": {"omlx": {"settings": {
        "baseURL": format!("{}/v1", s.origin), "discovery": "openai", "apiKey": "sk-local"}}}}));
    let p = c.provider("omlx").unwrap();
    assert_eq!(
        local::unprompted_key(&c, "omlx", &p).as_deref(),
        Some("sk-local")
    );
    let found = local::discover_providers(&c, &live::file(), &["omlx".into()]).await;
    assert_eq!(found, ["omlx"]);
    assert_eq!(
        s.seen()[0].authorization.as_deref(),
        Some("Bearer sk-local")
    );
}

#[tokio::test]
async fn discovery_answers_are_bounded_and_only_needed_show_fields_are_kept() {
    // A model list past the cap is refused, and the kept list stays.
    let huge = format!(
        "{{\"data\": [{{\"id\": \"m\", \"pad\": \"{}\"}}]}}",
        "x".repeat(local::MAX_ANSWER)
    );
    let s = serve(Arc::new(move |_| (200, huge.clone()))).await;
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("live.json");
    let err = refresh(
        &path,
        "omlx",
        Kind::OpenAi,
        &format!("{}/v1", s.origin),
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("larger than"), "{err:#}");

    // Of Ollama's details, only what the list reads is kept.
    let state = Arc::new(Mutex::new(json!({
        "tags": [tag("qwen3", "one")],
        "show": {"qwen3": {"capabilities": ["completion"], "license": "L".repeat(10_000),
            "modelfile": "FROM x", "details": {"family": "qwen3", "format": "gguf"},
            "model_info": {"qwen3.context_length": 32768, "tokenizer.ggml.tokens": ["a", "b"]}}}
    })));
    let s = serve(ollama_handler(state)).await;
    refresh(&path, "ollama", Kind::Ollama, &s.origin, None)
        .await
        .unwrap();
    let raw = live::load_in(&path, "ollama").unwrap();
    let show = &raw["models"][0]["show"];
    assert_eq!(
        *show,
        json!({"capabilities": ["completion"], "details": {"family": "qwen3"},
            "model_info": {"qwen3.context_length": 32768}})
    );
    assert_eq!(
        find(&kept(&path, "ollama"), "qwen3").unwrap().limit.context,
        32768
    );
}
