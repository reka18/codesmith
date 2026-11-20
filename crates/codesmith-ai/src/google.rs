//! Google Application Default Credentials for Vertex AI (after OpenCode's
//! use of `google-auth-library`): the file `GOOGLE_APPLICATION_CREDENTIALS`
//! names, else gcloud's `application_default_credentials.json` (under
//! `CLOUDSDK_CONFIG` when set), else the metadata server of a Google Cloud
//! machine. Service-account keys are exchanged for an access token with a
//! signed JWT (RS256), user credentials with their refresh token; other
//! credential types (workload identity federation, impersonation) are
//! handed to `gcloud auth application-default print-access-token`. Tokens
//! are kept until a minute before they expire and never logged.

use crate::Error;
use base64::Engine;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const METADATA_HOST: &str = "metadata.google.internal";

pub type Env = crate::aws::Env;

/// gcloud's configuration directory.
pub fn gcloud_dir(env: &Env) -> PathBuf {
    let home = env("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    if let Some(c) = env("CLOUDSDK_CONFIG") {
        return match c.as_str() {
            "~" => home,
            c => match c.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(c),
            },
        };
    }
    if cfg!(windows) {
        if let Some(a) = env("APPDATA") {
            return PathBuf::from(a).join("gcloud");
        }
    }
    home.join(".config").join("gcloud")
}

/// The ADC file in use, if any.
pub fn adc_file(env: &Env) -> Option<PathBuf> {
    if let Some(f) = env("GOOGLE_APPLICATION_CREDENTIALS") {
        return Some(PathBuf::from(f));
    }
    let f = gcloud_dir(env).join("application_default_credentials.json");
    f.exists().then_some(f)
}

/// The `quota_project_id` of the ADC file, a project to suggest.
pub fn quota_project(env: &Env) -> Option<String> {
    let text = std::fs::read_to_string(adc_file(env)?).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v["quota_project_id"].as_str().map(str::to_string)
}

fn adc_error(why: impl std::fmt::Display) -> Error {
    Error::Invalid(format!(
        "Google Application Default Credentials: {why} (run `gcloud auth application-default login`, or set GOOGLE_APPLICATION_CREDENTIALS)"
    ))
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The DER bytes of a PEM block.
fn pem_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

/// A signed RS256 JWT for a service account's token grant.
pub fn service_account_jwt(
    client_email: &str,
    private_key_pem: &str,
    key_id: Option<&str>,
    audience: &str,
    now: SystemTime,
) -> Result<String, Error> {
    let der = pem_der(private_key_pem).ok_or_else(|| adc_error("the private key is not PEM"))?;
    let key = ring::signature::RsaKeyPair::from_pkcs8(&der)
        .map_err(|_| adc_error("the private key is not a PKCS#8 RSA key"))?;
    let iat = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut header = serde_json::json!({"alg": "RS256", "typ": "JWT"});
    if let Some(k) = key_id {
        header["kid"] = k.into();
    }
    let claims = serde_json::json!({
        "iss": client_email,
        "scope": SCOPE,
        "aud": audience,
        "iat": iat,
        "exp": iat + 3600,
    });
    let input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    );
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        input.as_bytes(),
        &mut sig,
    )
    .map_err(|_| adc_error("signing the token request failed"))?;
    Ok(format!("{input}.{}", b64url(&sig)))
}

/// A token and how long it lasts.
type Token = (String, Duration);

async fn token_response(rb: reqwest::RequestBuilder, what: &str) -> Result<Token, Error> {
    let resp = rb
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| adc_error(format!("{what}: {}", Error::from(e))))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let why = v["error_description"]
            .as_str()
            .or(v["error"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(adc_error(format!("{what} failed: {why}")));
    }
    let token = v["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| adc_error(format!("{what} returned an empty access token")))?;
    let ttl = v["expires_in"].as_u64().unwrap_or(3600);
    Ok((token.to_string(), Duration::from_secs(ttl)))
}

/// Exchange the credentials of an ADC file for an access token.
pub async fn exchange(client: &reqwest::Client, creds: &Value) -> Result<Token, Error> {
    let s = |k: &str| creds[k].as_str().filter(|v| !v.is_empty());
    match s("type") {
        Some("service_account") => {
            let (Some(email), Some(key)) = (s("client_email"), s("private_key")) else {
                return Err(adc_error(
                    "the service account key has no client_email or private_key",
                ));
            };
            let uri = s("token_uri").unwrap_or(TOKEN_URI);
            let jwt = service_account_jwt(email, key, s("private_key_id"), uri, SystemTime::now())?;
            token_response(
                client.post(uri).form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", jwt.as_str()),
                ]),
                "the service account token exchange",
            )
            .await
        }
        Some("authorized_user") => {
            let (Some(id), Some(secret), Some(refresh)) =
                (s("client_id"), s("client_secret"), s("refresh_token"))
            else {
                return Err(adc_error(
                    "the user credentials have no client_id, client_secret or refresh_token",
                ));
            };
            token_response(
                client.post(s("token_uri").unwrap_or(TOKEN_URI)).form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", id),
                    ("client_secret", secret),
                    ("refresh_token", refresh),
                ]),
                "refreshing the user credentials",
            )
            .await
        }
        _ => gcloud_token().await,
    }
}

/// gcloud's own token, for credential types CodeSmith does not exchange.
async fn gcloud_token() -> Result<Token, Error> {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new("gcloud")
            .args(["auth", "application-default", "print-access-token"])
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| adc_error("gcloud timed out"))?
    .map_err(|_| adc_error("this credential type needs the gcloud CLI, which is not installed"))?;
    if !out.status.success() {
        let err: String = String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(500)
            .collect();
        return Err(adc_error(format!("gcloud failed: {err}")));
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() {
        return Err(adc_error("gcloud printed no token"));
    }
    // gcloud's tokens last an hour; take a little less.
    Ok((token, Duration::from_secs(3000)))
}

async fn metadata_token(client: &reqwest::Client, env: &Env) -> Result<Token, Error> {
    let host = env("GCE_METADATA_HOST").unwrap_or_else(|| METADATA_HOST.into());
    let url = format!(
        "http://{host}/computeMetadata/v1/instance/service-accounts/default/token?scopes={SCOPE}"
    );
    token_response(
        client
            .get(url)
            .header("Metadata-Flavor", "Google")
            .timeout(Duration::from_secs(3)),
        "the metadata server",
    )
    .await
    .map_err(|_| adc_error("no credentials file found and no metadata server answered"))
}

/// ADC as an [`crate::Authorizer`]: `Authorization: Bearer` with a cached
/// access token, and the quota project of user credentials.
pub struct Adc {
    env: Env,
    cached: tokio::sync::Mutex<Option<(String, Option<String>, Instant)>>,
}

impl std::fmt::Debug for Adc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Adc")
    }
}

impl Adc {
    pub fn new(env: Env) -> Self {
        Adc {
            env,
            cached: tokio::sync::Mutex::new(None),
        }
    }

    /// An access token and the quota project to bill, refreshed when the
    /// cached one is within a minute of expiring. Concurrent requests share
    /// one refresh.
    pub async fn token(&self, client: &reqwest::Client) -> Result<(String, Option<String>), Error> {
        let mut cached = self.cached.lock().await;
        if let Some((t, q, until)) = cached.as_ref() {
            if Instant::now() < *until {
                return Ok((t.clone(), q.clone()));
            }
        }
        let (token, ttl, quota) = match adc_file(&self.env) {
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| adc_error(format!("reading {}: {e}", path.display())))?;
                let creds: Value = serde_json::from_str(&text)
                    .map_err(|_| adc_error(format!("{} is not JSON", path.display())))?;
                let (t, ttl) = exchange(client, &creds).await?;
                let quota = (creds["type"] == "authorized_user")
                    .then(|| creds["quota_project_id"].as_str().map(str::to_string))
                    .flatten();
                (t, ttl, quota)
            }
            None => {
                let (t, ttl) = metadata_token(client, &self.env).await?;
                (t, ttl, None)
            }
        };
        let until = Instant::now() + ttl.saturating_sub(Duration::from_secs(60));
        *cached = Some((token.clone(), quota.clone(), until));
        Ok((token, quota))
    }
}

impl crate::Authorizer for Adc {
    fn authorize<'a>(
        &'a self,
        client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> futures_util::future::BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let (token, quota) = self.token(client).await?;
            let h = req.headers_mut();
            h.remove("x-goog-api-key");
            h.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| adc_error("the access token is not a valid header"))?,
            );
            if let Some(q) = quota.and_then(|q| reqwest::header::HeaderValue::from_str(&q).ok()) {
                h.entry("x-goog-user-project").or_insert(q);
            }
            Ok(())
        })
    }

    fn describe(&self) -> String {
        "Google Application Default Credentials".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    pub(crate) const TEST_KEY: &str = include_str!("../tests/fixtures/google/test-key.pem");

    fn env(pairs: &[(&str, String)]) -> Env {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        Arc::new(move |k| m.get(k).cloned())
    }

    #[test]
    fn signs_a_verifiable_service_account_jwt() {
        let jwt = service_account_jwt(
            "sa@p.iam.gserviceaccount.com",
            TEST_KEY,
            Some("kid-1"),
            "https://oauth2.example/token",
            UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
        .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let dec = |s: &str| {
            serde_json::from_slice::<Value>(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(s)
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            dec(parts[0]),
            serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": "kid-1"})
        );
        let claims = dec(parts[1]);
        assert_eq!(claims["iss"], "sa@p.iam.gserviceaccount.com");
        assert_eq!(claims["aud"], "https://oauth2.example/token");
        assert_eq!(claims["scope"], SCOPE);
        assert_eq!(claims["exp"], 1_700_003_600u64);
        let der = pem_der(TEST_KEY).unwrap();
        let pair = ring::signature::RsaKeyPair::from_pkcs8(&der).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            pair.public().as_ref().to_vec(),
        );
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .unwrap();
        public
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }

    #[test]
    fn finds_the_adc_file() {
        let d = tempfile::tempdir().unwrap();
        let e = env(&[("HOME", d.path().display().to_string())]);
        assert_eq!(adc_file(&e), None);
        let dir = d.path().join(".config/gcloud");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("application_default_credentials.json"),
            r#"{"type":"authorized_user","quota_project_id":"qp"}"#,
        )
        .unwrap();
        assert_eq!(
            adc_file(&e),
            Some(dir.join("application_default_credentials.json"))
        );
        assert_eq!(quota_project(&e).as_deref(), Some("qp"));
        let e = env(&[
            ("HOME", d.path().display().to_string()),
            ("CLOUDSDK_CONFIG", "~/elsewhere".into()),
        ]);
        assert_eq!(gcloud_dir(&e), d.path().join("elsewhere"));
        assert_eq!(adc_file(&e), None);
        let e = env(&[("GOOGLE_APPLICATION_CREDENTIALS", "/x/key.json".into())]);
        assert_eq!(adc_file(&e), Some(PathBuf::from("/x/key.json")));
    }

    struct Server {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn token_server(body: &'static str) -> Server {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 65536];
                let n = s.read(&mut buf).await.unwrap_or(0);
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        Server { url, requests }
    }

    #[tokio::test]
    async fn exchanges_user_credentials_once_and_bills_the_quota_project() {
        let srv = token_server(r#"{"access_token":"user-token","expires_in":3599}"#).await;
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("adc.json");
        std::fs::write(
            &file,
            serde_json::json!({
                "type": "authorized_user",
                "client_id": "cid",
                "client_secret": "csecret",
                "refresh_token": "rtoken",
                "quota_project_id": "billing-project",
                "token_uri": format!("{}/token", srv.url),
            })
            .to_string(),
        )
        .unwrap();
        let adc = Adc::new(env(&[(
            "GOOGLE_APPLICATION_CREDENTIALS",
            file.display().to_string(),
        )]));
        let client = reqwest::Client::new();
        let mut req = client.post("https://example.invalid/x").build().unwrap();
        crate::Authorizer::authorize(&adc, &client, &mut req)
            .await
            .unwrap();
        crate::Authorizer::authorize(&adc, &client, &mut req)
            .await
            .unwrap();
        assert_eq!(req.headers()["authorization"], "Bearer user-token");
        assert_eq!(req.headers()["x-goog-user-project"], "billing-project");
        let seen = srv.requests.lock().unwrap();
        assert_eq!(seen.len(), 1, "the token is cached");
        assert!(seen[0].contains("grant_type=refresh_token"));
        assert!(seen[0].contains("refresh_token=rtoken"));
    }

    #[tokio::test]
    async fn exchanges_a_service_account_key_with_a_jwt() {
        let srv = token_server(r#"{"access_token":"sa-token","expires_in":3600}"#).await;
        let creds = serde_json::json!({
            "type": "service_account",
            "client_email": "sa@p.iam.gserviceaccount.com",
            "private_key": TEST_KEY,
            "private_key_id": "k1",
            "token_uri": format!("{}/token", srv.url),
        });
        let (t, ttl) = exchange(&reqwest::Client::new(), &creds).await.unwrap();
        assert_eq!((t.as_str(), ttl.as_secs()), ("sa-token", 3600));
        let seen = srv.requests.lock().unwrap();
        assert!(
            seen[0].contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains("assertion=eyJ"));
    }

    #[tokio::test]
    async fn reports_a_failed_exchange_without_secrets() {
        let srv = token_server(
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )
        .await;
        // A 200 with an error body still has no token.
        let creds = serde_json::json!({
            "type": "authorized_user", "client_id": "c", "client_secret": "SECRET",
            "refresh_token": "REFRESH", "token_uri": format!("{}/token", srv.url),
        });
        let e = exchange(&reqwest::Client::new(), &creds)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("empty access token"), "{e}");
        assert!(!e.contains("SECRET") && !e.contains("REFRESH"));
    }

    #[tokio::test]
    async fn falls_back_to_the_metadata_server() {
        let srv =
            token_server(r#"{"access_token":"gce-token","expires_in":100,"token_type":"Bearer"}"#)
                .await;
        let d = tempfile::tempdir().unwrap();
        let host = srv.url.trim_start_matches("http://").to_string();
        let adc = Adc::new(env(&[
            ("HOME", d.path().display().to_string()),
            ("GCE_METADATA_HOST", host),
        ]));
        let (t, q) = adc.token(&reqwest::Client::new()).await.unwrap();
        assert_eq!((t.as_str(), q), ("gce-token", None));
        let seen = srv.requests.lock().unwrap();
        assert!(seen[0]
            .to_ascii_lowercase()
            .contains("metadata-flavor: google"));
    }
}
