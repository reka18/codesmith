//! Sign in with ChatGPT (OpenAI token sharing), ported from OpenCode's
//! `plugin/provider-chatgpt.test.ts` and `plugin/provider-openai.test.ts`
//! (MIT), against loopback fixtures only: a scripted models list, signing
//! keys and the sign-in's own callback. The test RSA keys in
//! `tests/fixtures/chatgpt` sign nothing real. See
//! `crates/codesmith-core/PARITY.md`.

use codesmith_core::providers::chatgpt::{self, Attempt, Endpoints};
use codesmith_core::providers::credentials::{self, New};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Tests of this binary share the process environment (the data folder).
/// A tokio lock, which may be held across awaits; each test has its own
/// runtime, so it is taken without one.
fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    loop {
        if let Ok(g) = LOCK.try_lock() {
            return g;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// A scratch data folder, and no OpenAI key from the environment.
fn isolate() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_DATA_HOME", d.path().join("data"));
    std::env::set_var("HOME", d.path().join("home"));
    std::env::set_var("CODESMITH_CONFIG_DIR", d.path().join("config"));
    std::env::remove_var("OPENAI_API_KEY");
    d
}

type Respond = Arc<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

/// Answers each request from `respond(path, authorization)`.
async fn serve(respond: Respond) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let respond = respond.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                let auth = text
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_string())
                    })
                    .unwrap_or_default();
                let (status, body) = respond(&path, &auth);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

// provider-chatgpt.test.ts: "loads only visible API-supported ChatGPT models"
#[tokio::test(flavor = "multi_thread")]
async fn oc_chatgpt_loads_only_visible_api_supported_models() {
    let base = serve(Arc::new(|path: &str, auth: &str| {
        if path == "/error/models" {
            return (503, String::new());
        }
        if path == "/empty/models" {
            return (200, json!({"models": []}).to_string());
        }
        assert_eq!(auth, "Bearer test-token");
        (200, json!({"models": [
            {"slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol", "visibility": "list", "supported_in_api": true,
             "context_window": 272_000, "input_modalities": ["text", "image"],
             "supported_reasoning_levels": [{"effort": "low"}], "model_messages": {"base_instructions": "not retained"}},
            {"slug": "gpt-reserve", "display_name": "GPT-Reserve", "visibility": "hide", "supported_in_api": true,
             "context_window": 272_000, "input_modalities": ["text"]},
            {"slug": "not-in-api", "display_name": "Not in API", "visibility": "list", "supported_in_api": false,
             "context_window": 272_000, "input_modalities": ["text"]}
        ]}).to_string())
    }))
    .await;
    let client = reqwest::Client::new();
    assert_eq!(
        chatgpt::fetch_models(&client, "test-token", &base)
            .await
            .unwrap(),
        [
            json!({"slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol", "visibility": "list", "supported_in_api": true,
            "context_window": 272_000, "input_modalities": ["text", "image"], "supported_reasoning_levels": [{"effort": "low"}]})
        ]
    );
    assert!(
        chatgpt::fetch_models(&client, "test-token", &format!("{base}/empty"))
            .await
            .is_err()
    );
    assert!(
        chatgpt::fetch_models(&client, "test-token", &format!("{base}/error"))
            .await
            .is_err()
    );
}

fn key(name: &str) -> ring::signature::RsaKeyPair {
    let der = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/chatgpt")
            .join(format!("{name}.pk8")),
    )
    .unwrap();
    ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap()
}

fn sign(k: &ring::signature::RsaKeyPair, claims: Value) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let head = b64.encode(json!({"alg": "RS256", "kid": "sign-in"}).to_string());
    let body = b64.encode(claims.to_string());
    let signed = format!("{head}.{body}");
    let mut sig = vec![0u8; k.public().modulus_len()];
    k.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signed.as_bytes(),
        &mut sig,
    )
    .unwrap();
    format!("{signed}.{}", b64.encode(sig))
}

// provider-chatgpt.test.ts: "verifies a signed ID token against OpenAI's
// issuer, client ID, expiry, and sign-in nonce"
#[tokio::test(flavor = "multi_thread")]
async fn oc_chatgpt_verifies_a_signed_id_token() {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let signer = key("sign-in");
    let other = key("other");
    let c = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(signer.public());
    let jwks = json!({"keys": [{"kty": "RSA", "kid": "sign-in", "alg": "RS256", "use": "sig", "n": b64.encode(&c.n), "e": b64.encode(&c.e)}]}).to_string();
    let base = serve(Arc::new(move |_: &str, _: &str| (200, jwks.clone()))).await;
    let jwks_url = format!("{base}/jwks");
    let client = reqwest::Client::new();
    let iss = chatgpt::ISSUER;
    let now = codesmith_core::providers::oauth::now_ms() / 1000;
    let exp = now + 60;
    let claims = |extra: Value| {
        let mut c = json!({"nonce": "expected", "iss": iss, "aud": "oaiapp_issued", "sub": "user_123", "exp": exp});
        for (k, v) in extra.as_object().unwrap() {
            if v.is_null() {
                c.as_object_mut().unwrap().remove(k);
            } else {
                c[k] = v.clone();
            }
        }
        c
    };
    let verify = |token: String, client_id: &'static str, nonce: &'static str| {
        let (client, url) = (client.clone(), jwks_url.clone());
        async move {
            chatgpt::verify_id_token(&client, &token, client_id, nonce, &url, iss)
                .await
                .is_ok()
        }
    };
    let valid = sign(&signer, claims(json!({})));
    assert!(verify(valid.clone(), "oaiapp_issued", "expected").await);
    assert!(
        !verify(
            sign(&signer, claims(json!({"sub": null}))),
            "oaiapp_issued",
            "expected"
        )
        .await
    );
    assert!(
        !verify(
            sign(&signer, claims(json!({"sub": " "}))),
            "oaiapp_issued",
            "expected"
        )
        .await
    );
    assert!(!verify(valid.clone(), "oaiapp_issued", "wrong").await);
    assert!(!verify(valid.clone(), "wrong-client", "expected").await);
    assert!(
        !verify(
            sign(
                &signer,
                claims(json!({"iss": "https://someone-else.example"}))
            ),
            "oaiapp_issued",
            "expected"
        )
        .await
    );
    assert!(
        !verify(
            sign(&signer, claims(json!({"exp": null}))),
            "oaiapp_issued",
            "expected"
        )
        .await
    );
    assert!(
        !verify(
            sign(&signer, claims(json!({"exp": exp - 120}))),
            "oaiapp_issued",
            "expected"
        )
        .await
    );
    assert!(!verify(sign(&other, claims(json!({}))), "oaiapp_issued", "expected").await);
}

// provider-chatgpt.test.ts: "registers the ChatGPT OAuth method"
#[test]
fn oc_chatgpt_registers_the_oauth_method() {
    assert_eq!(
        chatgpt::methods()[0],
        ("chatgpt-token-sharing", "Sign in with ChatGPT")
    );
    // No client of CodeSmith's own: the sign-in stops and explains.
    let none: codesmith_core::providers::Env = Arc::new(|_: &str| None);
    let e = chatgpt::client_id(None, None, false, &Default::default(), &none)
        .unwrap_err()
        .to_string();
    assert!(e.contains("CodeSmith ships none"), "{e}");
    assert!(
        e.contains(chatgpt::CLIENT_ID_VAR) && e.contains("--register"),
        "{e}"
    );
    // An existing connection's client wins.
    let env: codesmith_core::providers::Env =
        Arc::new(|k: &str| (k == chatgpt::CLIENT_ID_VAR).then(|| "mine".to_string()));
    assert_eq!(
        chatgpt::client_id(
            Some("oaiapp_issued"),
            None,
            false,
            &Default::default(),
            &env
        )
        .unwrap(),
        "oaiapp_issued"
    );
    assert_eq!(
        chatgpt::client_id(None, None, false, &Default::default(), &env).unwrap(),
        "mine"
    );
}

// provider-chatgpt.test.ts: "stops retries for ${code} from ${source} failures" (3 cases)
// and "stops deterministic ChatGPT stream failures while retrying temporary unavailability"
#[test]
fn oc_chatgpt_stops_retries_only_for_failures_retrying_cannot_fix() {
    for (source, code) in [
        ("http", "subscription_sharing_usage_limit_exceeded"),
        ("stream", "subscription_sharing_usage_limit_exceeded"),
        (
            "http",
            "subscription_sharing_workplace_individual_usage_limit_exceeded",
        ),
    ] {
        let body = if source == "http" {
            json!({"error": {"code": code}}).to_string()
        } else {
            json!({"type": "response.failed", "response": {"error": {"code": code}}}).to_string()
        };
        assert!(chatgpt::is_final(&body), "{source} {code}");
    }
    assert!(!chatgpt::is_final("{}"));
    for (code, retry) in [
        ("subscription_sharing_user_not_eligible", false),
        ("subscription_sharing_unsupported_capability", false),
        ("subscription_sharing_route_not_supported", false),
        ("subscription_sharing_invalid_user", false),
        ("chatpass_v2_scope_not_authorized", false),
        ("chatpass_v2_invalid_authorization_context", false),
        ("subscription_sharing_usage_unavailable", true),
        ("subscription_sharing_user_unavailable", true),
    ] {
        let body =
            json!({"type": "response.failed", "response": {"error": {"code": code}}}).to_string();
        assert_eq!(!chatgpt::is_final(&body), retry, "{code}");
        assert!(chatgpt::guidance(&body).is_some(), "{code}");
    }
}

fn login_value(client: &str, expires: u64) -> Value {
    json!({"type": "oauth", "method": "chatgpt-token-sharing", "access": "chatgpt-token", "refresh": "refresh",
        "expires": expires, "metadata": {"clientID": client}})
}

fn save_login(expires: u64) {
    credentials::create_in(
        &codesmith_core::paths::auth_file(),
        New {
            provider: "openai".into(),
            value: login_value("oaiapp_issued", expires),
            ..Default::default()
        },
    )
    .unwrap();
}

// provider-chatgpt.test.ts: "leaves API-key OpenAI retries alone"
#[test]
fn oc_chatgpt_leaves_api_key_openai_retries_alone() {
    let _g = serial();
    let _d = isolate();
    credentials::create_in(
        &codesmith_core::paths::auth_file(),
        New {
            provider: "openai".into(),
            value: json!({"type": "api", "key": "sk-test"}),
            ..Default::default()
        },
    )
    .unwrap();
    let config = codesmith_core::config::Config::default();
    assert!(!config.chatgpt(), "an API key is not a ChatGPT sign-in");
    save_login(codesmith_core::providers::oauth::now_ms() + 3_600_000);
    assert!(config.chatgpt());
    std::env::set_var("OPENAI_API_KEY", "sk-env");
    assert!(!config.chatgpt(), "a key in the environment wins");
    std::env::remove_var("OPENAI_API_KEY");
}

// provider-chatgpt.test.ts: "registers a new ChatGPT agent on first sign-in with a loopback callback"
#[test]
fn oc_chatgpt_registers_a_new_agent_on_first_sign_in() {
    let d = tempfile::tempdir().unwrap();
    let host_file = d.path().join("host");
    let host = chatgpt::host_id(&host_file).unwrap();
    let a = Attempt::new(
        chatgpt::REGISTRATION,
        &host,
        "http://127.0.0.1:4545/auth/callback",
    );
    let url = url::Url::parse(&chatgpt::authorize_url(&Endpoints::default(), &a)).unwrap();
    assert_eq!(
        format!("{}{}", url.origin().ascii_serialization(), url.path()),
        "https://auth.openai.com/api/accounts/authorize"
    );
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], "dynamic_agent_client");
    assert_eq!(q["agent_name_hint"], "CodeSmith");
    assert_eq!(q["response_type"], "code");
    assert_eq!(
        q["scope"],
        "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"
    );
    assert_eq!(q["resource"], "https://api.openai.com/v1");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(!q["state"].is_empty() && !q["nonce"].is_empty() && !q["code_challenge"].is_empty());
    assert_eq!(q["ext_agent_host_id"], host);
    assert_eq!(chatgpt::host_id(&host_file).unwrap(), host, "kept");
    let redirect = url::Url::parse(&q["redirect_uri"]).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), "/auth/callback");
}

// provider-chatgpt.test.ts: "reauthorizes with the client ID issued to an existing ChatGPT connection"
#[test]
fn oc_chatgpt_reauthorizes_with_the_issued_client() {
    let a = Attempt::new(
        "oaiapp_issued",
        "urn:uuid:00000000-0000-4000-8000-000000000000",
        "http://127.0.0.1:1/auth/callback",
    );
    let url = url::Url::parse(&chatgpt::authorize_url(&Endpoints::default(), &a)).unwrap();
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], "oaiapp_issued");
    assert!(!q.contains_key("agent_name_hint"));
    assert_eq!(
        q["ext_agent_host_id"],
        "urn:uuid:00000000-0000-4000-8000-000000000000"
    );
}

/// Run a sign-in whose browser goes straight to the callback with `extra`
/// query parameters; the callback's status and page.
async fn sign_in(
    client_id: &str,
    extra: &'static [(&'static str, &'static str)],
) -> (anyhow::Result<()>, u16, String) {
    let seen: Arc<Mutex<Option<(u16, String)>>> = Arc::default();
    let s2 = seen.clone();
    let done = Arc::new(tokio::sync::Notify::new());
    let d2 = done.clone();
    let open = move |url: &str| {
        let u = url::Url::parse(url).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        let mut cb = url::Url::parse(&q["redirect_uri"]).unwrap();
        cb.query_pairs_mut()
            .append_pair("code", "test-code")
            .append_pair("state", &q["state"]);
        for (k, v) in extra {
            cb.query_pairs_mut().append_pair(k, v);
        }
        tokio::spawn(async move {
            let r = reqwest::get(cb).await.unwrap();
            let status = r.status().as_u16();
            *s2.lock().unwrap() = Some((status, r.text().await.unwrap()));
            d2.notify_one();
        });
    };
    let e = Endpoints {
        issuer: "http://127.0.0.1:9".into(),
        jwks: "http://127.0.0.1:9/jwks".into(),
    };
    let r = chatgpt::login(
        &reqwest::Client::new(),
        &codesmith_core::paths::auth_file(),
        &e,
        client_id,
        open,
        None,
    )
    .await;
    done.notified().await;
    let (status, page) = seen.lock().unwrap().clone().unwrap();
    (r, status, page)
}

// provider-chatgpt.test.ts: "waits to show browser success until the sign-in callback succeeds"
#[tokio::test(flavor = "multi_thread")]
async fn oc_chatgpt_shows_success_only_once_the_callback_succeeds() {
    let _g = serial();
    let _d = isolate();
    let (r, status, page) = sign_in(chatgpt::REGISTRATION, &[]).await;
    assert!(r.is_err());
    assert_eq!(status, 400);
    assert!(page.contains("did not return a client ID"), "{page}");
    assert!(credentials::list_in(&codesmith_core::paths::auth_file(), "openai").is_empty());
}

// provider-chatgpt.test.ts: "rejects a reauthorization callback that returns a different client ID"
#[tokio::test(flavor = "multi_thread")]
async fn oc_chatgpt_rejects_a_callback_with_a_different_client() {
    let _g = serial();
    let _d = isolate();
    save_login(codesmith_core::providers::oauth::now_ms() + 3_600_000);
    let (r, status, page) = sign_in("oaiapp_issued", &[("client_id", "oaiapp_other")]).await;
    assert!(r.is_err());
    assert_eq!(status, 400);
    assert!(page.contains("different client"), "{page}");
    assert_eq!(
        credentials::list_in(&codesmith_core::paths::auth_file(), "openai").len(),
        1
    );
}

// The agent registration is sent only on an opt-in (`--register` or
// `settings.register`), and never stands in for a missing client.
#[test]
fn chatgpt_registers_only_when_asked() {
    let none: codesmith_core::providers::Env = Arc::new(|_: &str| None);
    let mine: codesmith_core::providers::Env =
        Arc::new(|k: &str| (k == chatgpt::CLIENT_ID_VAR).then(|| "mine".to_string()));
    let plain = codesmith_core::providers::Settings::default();
    let opted: codesmith_core::providers::Settings =
        serde_json::from_value(json!({"register": true})).unwrap();
    let off: codesmith_core::providers::Settings =
        serde_json::from_value(json!({"register": false})).unwrap();
    let id = |saved, flag, register, s: &codesmith_core::providers::Settings, env| {
        chatgpt::client_id(saved, flag, register, s, env)
    };
    // No opt-in: no registration, and no borrowed client.
    assert!(id(None, None, false, &plain, &none).is_err());
    assert!(id(None, None, false, &off, &none).is_err());
    // The flag registers, even over an existing login's client.
    assert_eq!(
        id(None, None, true, &plain, &none).unwrap(),
        chatgpt::REGISTRATION
    );
    assert_eq!(
        id(Some("oaiapp_issued"), None, true, &plain, &none).unwrap(),
        chatgpt::REGISTRATION
    );
    let both = id(None, Some("oaiapp_x"), true, &plain, &none)
        .unwrap_err()
        .to_string();
    assert!(both.contains("Give one of them"), "{both}");
    // settings.register opts in only when no client is given.
    assert_eq!(
        id(None, None, false, &opted, &none).unwrap(),
        chatgpt::REGISTRATION
    );
    assert_eq!(id(None, None, false, &opted, &mine).unwrap(), "mine");
    assert_eq!(
        id(Some("oaiapp_issued"), None, false, &opted, &none).unwrap(),
        "oaiapp_issued",
        "a later sign-in reuses the issued client"
    );
    // The registration id given as a client is not an opt-in, by any route.
    let as_flag = id(None, Some(chatgpt::REGISTRATION), false, &plain, &none);
    let as_setting: codesmith_core::providers::Settings =
        serde_json::from_value(json!({"clientId": chatgpt::REGISTRATION})).unwrap();
    let as_setting = id(None, None, false, &as_setting, &none);
    let reg_env: codesmith_core::providers::Env = Arc::new(|k: &str| {
        (k == chatgpt::CLIENT_ID_VAR).then(|| chatgpt::REGISTRATION.to_string())
    });
    let as_env = id(None, None, false, &opted, &reg_env);
    for r in [as_flag, as_setting, as_env] {
        let e = r.unwrap_err().to_string();
        assert!(e.contains("--register or settings.register"), "{e}");
    }
    let with_flag = id(None, Some(chatgpt::REGISTRATION), true, &plain, &none);
    assert!(with_flag
        .unwrap_err()
        .to_string()
        .contains("Give one of them"));
    assert!(chatgpt::REGISTER_NOTICE.contains("OpenAI may reject it"));
    // A registration is answered with an issued client, never itself.
    let a = chatgpt::Attempt::new(
        chatgpt::REGISTRATION,
        "urn:uuid:host",
        "http://127.0.0.1/cb",
    );
    assert!(chatgpt::callback_client(&a, Some(chatgpt::REGISTRATION)).is_err());
    assert_eq!(
        chatgpt::callback_client(&a, Some("oaiapp_issued")).unwrap(),
        "oaiapp_issued"
    );
}

fn no_env() -> codesmith_core::providers::Env {
    Arc::new(|_: &str| None)
}

type Form = std::collections::HashMap<String, String>;
type TokenAnswer = Arc<dyn Fn(&Form) -> Value + Send + Sync>;

/// A scripted OpenAI sign-in server: signing keys at `/jwks`, and a token
/// endpoint answering with `token` for each form posted (kept in `forms`).
struct Issuer {
    base: String,
    forms: Arc<Mutex<Vec<std::collections::HashMap<String, String>>>>,
}

async fn issuer(token: TokenAnswer) -> Issuer {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let c = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(key("sign-in").public());
    let jwks = json!({"keys": [{"kty": "RSA", "kid": "sign-in", "alg": "RS256", "use": "sig", "n": b64.encode(&c.n), "e": b64.encode(&c.e)}]}).to_string();
    let forms: Arc<Mutex<Vec<_>>> = Arc::default();
    let f2 = forms.clone();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let (jwks, forms, token) = (jwks.clone(), f2.clone(), token.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the head, then the body its Content-Length gives.
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if body.len() >= len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                let body = text.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
                let answer = if path == "/jwks" {
                    jwks
                } else {
                    let form: std::collections::HashMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes())
                            .into_owned()
                            .collect();
                    let a = token(&form).to_string();
                    forms.lock().unwrap().push(form);
                    a
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    Issuer {
        base: format!("http://{addr}"),
        forms,
    }
}

// provider-chatgpt.test.ts: "registers a new ChatGPT agent on first sign-in
// with a loopback callback", end to end against a fixture issuer: the
// registration asks as dynamic_agent_client named CodeSmith, the client the
// callback names is the one exchanged, saved and refreshed with.
#[tokio::test(flavor = "multi_thread")]
async fn chatgpt_registration_saves_and_refreshes_the_issued_client() {
    let _g = serial();
    let _d = isolate();
    let nonce: Arc<Mutex<String>> = Arc::default();
    let n2 = nonce.clone();
    let claimed: Arc<Mutex<String>> = Arc::default();
    let c2 = claimed.clone();
    let iss = issuer(Arc::new(move |form| {
        let id_token = sign(
            &key("sign-in"),
            json!({"iss": c2.lock().unwrap().clone(), "aud": form["client_id"], "sub": "user_123",
                "exp": codesmith_core::providers::oauth::now_ms() / 1000 + 600,
                "nonce": n2.lock().unwrap().clone()}),
        );
        json!({"access_token": format!("access-{}", form["grant_type"]), "refresh_token": "refresh-1",
            "expires_in": 3600, "id_token": id_token,
            "scope": "openid offline_access chatgpt.tokens.use.direct"})
    }))
    .await;
    *claimed.lock().unwrap() = iss.base.clone();
    let e = Endpoints {
        issuer: iss.base.clone(),
        jwks: format!("{}/jwks", iss.base),
    };
    let asked: Arc<Mutex<Option<std::collections::HashMap<String, String>>>> = Arc::default();
    let a2 = asked.clone();
    let n3 = nonce.clone();
    let done = Arc::new(tokio::sync::Notify::new());
    let d2 = done.clone();
    let open = move |url: &str| {
        let u = url::Url::parse(url).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        *n3.lock().unwrap() = q["nonce"].clone();
        let mut cb = url::Url::parse(&q["redirect_uri"]).unwrap();
        cb.query_pairs_mut()
            .append_pair("code", "test-code")
            .append_pair("state", &q["state"])
            .append_pair("client_id", "oaiapp_issued");
        *a2.lock().unwrap() = Some(q);
        tokio::spawn(async move {
            let _ = reqwest::get(cb).await;
            d2.notify_one();
        });
    };
    let file = codesmith_core::paths::auth_file();
    let id = chatgpt::client_id(None, None, true, &Default::default(), &no_env()).unwrap();
    let r = chatgpt::login(&reqwest::Client::new(), &file, &e, &id, open, None).await;
    done.notified().await;
    r.unwrap();
    let q = asked.lock().unwrap().clone().unwrap();
    assert_eq!(q["client_id"], "dynamic_agent_client");
    assert_eq!(q["agent_name_hint"], "CodeSmith");
    let forms = iss.forms.lock().unwrap().clone();
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0]["grant_type"], "authorization_code");
    assert_eq!(forms[0]["client_id"], "oaiapp_issued");
    let saved = credentials::list_in(&file, "openai");
    assert_eq!(saved.len(), 1);
    let login = saved[0].value.clone();
    assert_eq!(login["metadata"]["clientID"], "oaiapp_issued");
    // A later sign-in without the opt-in reuses the issued client.
    assert_eq!(
        chatgpt::client_id(
            login["metadata"]["clientID"].as_str(),
            None,
            false,
            &Default::default(),
            &no_env()
        )
        .unwrap(),
        "oaiapp_issued"
    );
    let refreshed = chatgpt::refresh(&reqwest::Client::new(), &e, &login)
        .await
        .unwrap();
    let forms = iss.forms.lock().unwrap().clone();
    assert_eq!(forms[1]["grant_type"], "refresh_token");
    assert_eq!(forms[1]["client_id"], "oaiapp_issued");
    assert_eq!(refreshed["metadata"]["clientID"], "oaiapp_issued");
    assert_eq!(refreshed["access"], "access-refresh_token");
}

// A refused registration says OpenAI may reject it, and saves nothing.
#[tokio::test(flavor = "multi_thread")]
async fn chatgpt_refused_registration_says_openai_may_reject_it() {
    let _g = serial();
    let _d = isolate();
    let (r, status, _) = sign_in(
        chatgpt::REGISTRATION,
        &[
            ("error", "unauthorized_client"),
            ("error_description", "Unknown agent"),
        ],
    )
    .await;
    let err = format!("{:#}", r.unwrap_err());
    assert_eq!(status, 400);
    assert!(err.contains("Unknown agent"), "{err}");
    assert!(err.contains("OpenAI may have rejected"), "{err}");
    assert!(credentials::list_in(&codesmith_core::paths::auth_file(), "openai").is_empty());
    // A client of your own gets no such hint.
    let (r, _, _) = sign_in("oaiapp_mine", &[("error", "access_denied")]).await;
    assert!(!format!("{:#}", r.unwrap_err()).contains("OpenAI may have rejected"));
}

// provider-chatgpt.test.ts: "moves only token-sharing credentials to the new
// method ID and caches their saved models"
#[test]
fn oc_chatgpt_moves_only_token_sharing_credentials_and_keeps_their_models() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("auth.json");
    let models = d.path().join("models.json");
    let legacy = credentials::create_in(&file, New {
        provider: "openai".into(),
        value: json!({"type": "oauth", "method": "chatgpt-browser", "access": "codex-token", "refresh": "codex-refresh", "expires": 1, "metadata": {"accountID": "acct_123"}}),
        ..Default::default()
    })
    .unwrap();
    let snapshot = json!([{"slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list", "supported_in_api": true, "context_window": 272_000, "input_modalities": ["text"]}]);
    let sharing = credentials::create_in(&file, New {
        provider: "openai".into(),
        value: json!({"type": "oauth", "method": "chatgpt-browser", "access": "sharing-token", "refresh": "sharing-refresh", "expires": 1, "metadata": {"clientID": "oaiapp_issued", "models": snapshot}}),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(chatgpt::migrate(&file, &models).unwrap(), 1);
    assert_eq!(
        credentials::get_in(&file, &legacy.id).unwrap().value["method"],
        "chatgpt-browser"
    );
    let moved = credentials::get_in(&file, &sharing.id).unwrap().value;
    assert_eq!(moved["method"], "chatgpt-token-sharing");
    assert_eq!(moved["access"], "sharing-token");
    assert_eq!(moved["metadata"], json!({"clientID": "oaiapp_issued"}));
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&models).unwrap()).unwrap();
    assert_eq!(kept["models:oaiapp_issued"], snapshot);
}

fn openai_config(extra: Value) -> codesmith_core::config::Config {
    let mut v = json!({"providers": {"openai": {"protocol": "openai-responses", "models": {
        "gpt-5.5": {"limit": {"context": 1_050_000, "output": 128_000}},
        "gpt-4o": {},
        "gpt-6.1-sol": {"limit": {"context": 1_050_000, "output": 128_000}}
    }}}});
    for (k, x) in extra.as_object().unwrap() {
        v["providers"]["openai"][k] = x.clone();
    }
    serde_json::from_value(v).unwrap()
}

// provider-chatgpt.test.ts: "routes ChatGPT over HTTP with only the listed
// catalog fallback models", and provider-openai-context-caps.test.ts:
// "caps OpenAI models under chatgpt-token-sharing"
#[test]
fn oc_chatgpt_routes_over_http_with_only_the_listed_models() {
    let _g = serial();
    let _d = isolate();
    save_login(codesmith_core::providers::oauth::now_ms() + 3_600_000);
    let config = openai_config(json!({}));
    let mut models = config.list_models("openai", &[]);
    models.sort();
    assert_eq!(models, ["gpt-5.5", "gpt-6.1-sol"]);
    let m = config.resolve_model("openai/gpt-5.5").unwrap();
    assert_eq!(m.provider.base_url, "https://api.openai.com/v1");
    assert!(m.websocket.is_none(), "HTTP, not the Responses WebSocket");
    assert!(
        matches!(&m.provider.auth, codesmith_ai::Auth::Dynamic(a) if a.describe() == chatgpt::DESCRIBE)
    );
    assert_eq!(m.context, Some(272_000 + 128_000));
    assert_eq!(
        config.resolve_model("openai/gpt-6.1-sol").unwrap().context,
        Some(500_000 + 128_000)
    );
    assert_eq!(config.cost("openai/gpt-5.5"), Some(Default::default()));
}

// provider-chatgpt.test.ts: "keeps the token-sharing safeguards when the token cannot be refreshed at load"
#[test]
fn oc_chatgpt_keeps_the_safeguards_with_an_expired_login() {
    let _g = serial();
    let _d = isolate();
    save_login(1);
    let config = openai_config(json!({}));
    assert!(!config
        .list_models("openai", &[])
        .contains(&"gpt-4o".to_string()));
    assert!(config
        .resolve_model("openai/gpt-5.5")
        .unwrap()
        .websocket
        .is_none());
}

// provider-chatgpt.test.ts and provider-openai.test.ts: "keeps the full OpenAI
// catalog under an API key connection", and provider-openai-context-caps.test.ts:
// "keeps catalog limits under an API key"
#[test]
fn oc_chatgpt_keeps_the_full_catalog_and_limits_under_an_api_key() {
    let _g = serial();
    let _d = isolate();
    save_login(codesmith_core::providers::oauth::now_ms() + 3_600_000);
    let config = openai_config(json!({"settings": {"apiKey": "sk-test"}}));
    let mut models = config.list_models("openai", &[]);
    models.sort();
    assert_eq!(models, ["gpt-4o", "gpt-5.5", "gpt-6.1-sol"]);
    let m = config.resolve_model("openai/gpt-5.5").unwrap();
    assert_eq!(m.context, Some(1_050_000));
    assert!(!matches!(&m.provider.auth, codesmith_ai::Auth::Dynamic(_)));
}

// provider-chatgpt.test.ts: "${connection} compaction uses plugin defaults but
// allows later model and variant overrides" (2 cases; adapted: a ChatGPT
// sign-in always compacts by summary, a key keeps the configured kind)
#[test]
fn oc_chatgpt_compacts_by_summary_and_a_key_keeps_native_compaction() {
    let _g = serial();
    let _d = isolate();
    save_login(codesmith_core::providers::oauth::now_ms() + 3_600_000);
    let native = json!({"compaction": {"type": "native"}});
    let chatgpt_config = openai_config(native.clone());
    assert_eq!(
        chatgpt_config
            .resolve_model("openai/gpt-5.5")
            .unwrap()
            .native_compaction,
        None
    );
    let mut keyed = native;
    keyed["settings"] = json!({"apiKey": "sk-test"});
    assert!(openai_config(keyed)
        .resolve_model("openai/gpt-5.5")
        .unwrap()
        .native_compaction
        .is_some());
}

#[test]
fn a_chatgpt_token_goes_only_to_the_openai_api() {
    let _g = serial();
    let _d = isolate();
    save_login(codesmith_core::providers::oauth::now_ms() + 3_600_000);
    let config = openai_config(json!({"settings": {"baseURL": "https://proxy.example/v1"}}));
    let e = config
        .resolve_model("openai/gpt-5.5")
        .unwrap_err()
        .to_string();
    assert!(e.contains("only to https://api.openai.com/v1"), "{e}");
}
