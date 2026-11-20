//! AWS credentials and SigV4 request signing for Amazon Bedrock (after
//! OpenCode's `bedrock-auth`, which uses the AWS SDK's default credential
//! chain). CodeSmith resolves the chain itself: environment keys, the
//! shared `credentials` and `config` files (static keys,
//! `credential_process`, and SSO or assumed-role profiles through the AWS
//! CLI), web identity tokens and container credentials. Like OpenCode, it
//! runs the chain afresh for every request, so keys rewritten on disk (an
//! `aws sso login`) take effect at once; only credentials that say when they
//! expire are kept, until shortly before then. Secrets are never logged.

use crate::sigv4::Credentials;
use crate::Error;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Environment variables that make the default chain usable without any
/// key stored in CodeSmith (OpenCode's `CHAIN_ENV`).
pub const CHAIN_ENV: [&str; 5] = [
    "AWS_PROFILE",
    "AWS_ACCESS_KEY_ID",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
];

/// The process environment, or a fixed one in tests.
pub type Env = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The real environment (empty values count as unset).
pub fn process_env() -> Env {
    std::sync::Arc::new(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

/// The region: the configured one, else `AWS_REGION`, `AWS_DEFAULT_REGION`,
/// then `us-east-1`.
pub fn region(configured: Option<&str>, env: &Env) -> String {
    configured
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| "us-east-1".into())
}

/// Whether ambient AWS configuration makes the chain usable.
pub fn ambient(env: &Env) -> bool {
    CHAIN_ENV.iter().any(|k| env(k).is_some())
}

/// The shared config and credentials files.
pub fn files(env: &Env) -> (PathBuf, PathBuf) {
    let home = env("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_default();
    let config = env("AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".aws").join("config"));
    let creds = env("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".aws").join("credentials"));
    (config, creds)
}

/// `[section]` → `key = value` pairs of an INI file.
fn ini(text: &str) -> Vec<(String, Vec<(String, String)>)> {
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            out.push((name.trim().to_string(), Vec::new()));
            continue;
        }
        // Indented lines belong to a nested block (`s3 =`), which no key
        // here needs.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        if let (Some((k, v)), Some(last)) = (t.split_once('='), out.last_mut()) {
            last.1
                .push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    out
}

/// Profile names in the shared files, sorted (names only: nothing is
/// resolved, so no helper runs and AWS is not contacted).
pub fn profiles(env: &Env) -> Vec<String> {
    let (config, creds) = files(env);
    let mut names = Vec::new();
    for (path, is_config) in [(config, true), (creds, false)] {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        for (section, _) in ini(&text) {
            let name = if !is_config || section == "default" {
                Some(section.as_str())
            } else {
                section.strip_prefix("profile ").map(str::trim)
            };
            if let Some(n) = name.filter(|n| !n.is_empty()) {
                names.push(n.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// A profile's settings: its config-file section under the credentials
/// file's (which wins).
fn profile_settings(env: &Env, name: &str) -> Option<Vec<(String, String)>> {
    let (config, creds) = files(env);
    let mut found = false;
    let mut out: Vec<(String, String)> = Vec::new();
    let config_name = if name == "default" {
        "default".to_string()
    } else {
        format!("profile {name}")
    };
    let text = std::fs::read_to_string(config).unwrap_or_default();
    for (s, kv) in ini(&text) {
        if s == config_name || (name == "default" && s == "profile default") {
            found = true;
            out.extend(kv);
        }
    }
    let text = std::fs::read_to_string(creds).unwrap_or_default();
    for (s, kv) in ini(&text) {
        if s == name {
            found = true;
            for (k, v) in kv {
                out.retain(|(ok, _)| *ok != k);
                out.push((k, v));
            }
        }
    }
    found.then_some(out)
}

fn get<'a>(kv: &'a [(String, String)], k: &str) -> Option<&'a str> {
    kv.iter()
        .rev()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn chain_error(why: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("AWS default credential chain failed: {why}"))
}

/// Credentials with the time they stop working, if they say.
type Expiring = (Credentials, Option<SystemTime>);

fn expiry(s: Option<&str>) -> Option<SystemTime> {
    let t = chrono::DateTime::parse_from_rfc3339(s?).ok()?;
    Some(SystemTime::from(t))
}

/// The JSON a `credential_process` (or the container endpoint) prints.
fn from_process_json(text: &str, source: &str) -> Result<Expiring, Error> {
    let v: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|_| chain_error(format!("{source} did not print credentials JSON")))?;
    let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let (Some(id), Some(secret)) = (s("AccessKeyId"), s("SecretAccessKey")) else {
        return Err(chain_error(format!(
            "{source} returned no AccessKeyId and SecretAccessKey"
        )));
    };
    Ok((
        Credentials {
            access_key_id: id,
            secret_access_key: secret,
            session_token: s("SessionToken").or_else(|| s("Token")),
        },
        expiry(v["Expiration"].as_str()),
    ))
}

async fn run_process(command: &str, source: &str) -> Result<Expiring, Error> {
    #[cfg(unix)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c").arg(command);
        c
    };
    #[cfg(not(unix))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| chain_error(format!("{source} timed out")))?
        .map_err(|e| chain_error(format!("{source} could not run: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err: String = err.trim().chars().take(500).collect();
        return Err(chain_error(format!("{source} failed: {err}")));
    }
    from_process_json(&String::from_utf8_lossy(&out.stdout), source)
}

/// The text between `<tag>` and `</tag>`.
fn xml(text: &str, tag: &str) -> Option<String> {
    let start = text.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = text[start..].find(&format!("</{tag}>"))? + start;
    Some(text[start..end].trim().to_string())
}

async fn web_identity(
    client: &reqwest::Client,
    env: &Env,
    region: &str,
    token_file: &str,
    role: &str,
) -> Result<Expiring, Error> {
    let token = std::fs::read_to_string(token_file)
        .map_err(|e| chain_error(format!("reading AWS_WEB_IDENTITY_TOKEN_FILE: {e}")))?;
    let endpoint = env("AWS_ENDPOINT_URL_STS")
        .unwrap_or_else(|| format!("https://sts.{region}.amazonaws.com"));
    let session = env("AWS_ROLE_SESSION_NAME").unwrap_or_else(|| "codesmith".into());
    let resp = client
        .post(endpoint.trim_end_matches('/').to_string() + "/")
        .form(&[
            ("Action", "AssumeRoleWithWebIdentity"),
            ("Version", "2011-06-15"),
            ("RoleArn", role),
            ("RoleSessionName", session.as_str()),
            ("WebIdentityToken", token.trim()),
        ])
        .send()
        .await
        .map_err(|e| chain_error(format!("web identity: {}", Error::from(e))))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let why = xml(&text, "Message").unwrap_or_else(|| format!("HTTP {status}"));
        return Err(chain_error(format!("web identity: {why}")));
    }
    let (Some(id), Some(secret)) = (xml(&text, "AccessKeyId"), xml(&text, "SecretAccessKey"))
    else {
        return Err(chain_error("web identity: STS returned no credentials"));
    };
    Ok((
        Credentials {
            access_key_id: id,
            secret_access_key: secret,
            session_token: xml(&text, "SessionToken"),
        },
        expiry(xml(&text, "Expiration").as_deref()),
    ))
}

async fn container(client: &reqwest::Client, env: &Env) -> Option<Result<Expiring, Error>> {
    let url = match (
        env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
        env("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
    ) {
        (Some(rel), _) => format!("http://169.254.170.2{rel}"),
        (None, Some(full)) => full,
        (None, None) => return None,
    };
    let token = env("AWS_CONTAINER_AUTHORIZATION_TOKEN").or_else(|| {
        env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE")
            .and_then(|f| std::fs::read_to_string(f).ok())
            .map(|t| t.trim().to_string())
    });
    let mut rb = client.get(&url).timeout(Duration::from_secs(5));
    if let Some(t) = token {
        rb = rb.header("authorization", t);
    }
    let result = async {
        let resp = rb
            .send()
            .await
            .map_err(|e| chain_error(format!("container credentials: {}", Error::from(e))))?;
        if !resp.status().is_success() {
            return Err(chain_error(format!(
                "container credentials: HTTP {}",
                resp.status()
            )));
        }
        let text = resp.text().await.unwrap_or_default();
        from_process_json(&text, "the container credentials endpoint")
    }
    .await;
    Some(result)
}

/// Run the default chain once.
pub async fn resolve_chain(
    client: &reqwest::Client,
    env: &Env,
    profile: Option<&str>,
    region: &str,
) -> Result<Expiring, Error> {
    // An explicit profile (or AWS_PROFILE) skips the environment keys, as
    // the AWS SDK does.
    let named = profile
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| env("AWS_PROFILE"));
    if named.is_none() {
        if let (Some(id), Some(secret)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
            return Ok((
                Credentials {
                    access_key_id: id,
                    secret_access_key: secret,
                    session_token: env("AWS_SESSION_TOKEN"),
                },
                None,
            ));
        }
    }
    let name = named.clone().unwrap_or_else(|| "default".into());
    if let Some(kv) = profile_settings(env, &name) {
        if let (Some(id), Some(secret)) = (
            get(&kv, "aws_access_key_id"),
            get(&kv, "aws_secret_access_key"),
        ) {
            return Ok((
                Credentials {
                    access_key_id: id.into(),
                    secret_access_key: secret.into(),
                    session_token: get(&kv, "aws_session_token").map(str::to_string),
                },
                None,
            ));
        }
        if let Some(cmd) = get(&kv, "credential_process") {
            return run_process(cmd, "credential_process").await;
        }
        if let (Some(file), Some(role)) =
            (get(&kv, "web_identity_token_file"), get(&kv, "role_arn"))
        {
            return web_identity(client, env, region, file, role).await;
        }
        // SSO, assumed roles and console logins: the AWS CLI resolves them
        // (and refreshes its SSO token cache) for us.
        let delegated = ["sso_session", "sso_start_url", "role_arn", "login_session"];
        if delegated.iter().any(|k| get(&kv, k).is_some()) {
            let quoted = name.replace('\'', "'\\''");
            return run_process(
                &format!("aws configure export-credentials --profile '{quoted}' --format process"),
                &format!("the AWS CLI (profile {name})"),
            )
            .await;
        }
        if named.is_some() {
            return Err(chain_error(format!(
                "profile {name} has no credentials (keys, credential_process, SSO or role_arn)"
            )));
        }
    } else if named.is_some() {
        return Err(chain_error(format!(
            "profile {name} is not in the AWS config or credentials file"
        )));
    }
    if let (Some(file), Some(role)) = (env("AWS_WEB_IDENTITY_TOKEN_FILE"), env("AWS_ROLE_ARN")) {
        return web_identity(client, env, region, &file, &role).await;
    }
    if let Some(r) = container(client, env).await {
        return r;
    }
    Err(chain_error(
        "no credentials found (environment, shared config profile, web identity or container); \
         set AWS_PROFILE or AWS_ACCESS_KEY_ID, run `aws sso login`, or set AWS_BEARER_TOKEN_BEDROCK",
    ))
}

/// Where SigV4 credentials come from.
#[derive(Debug, Clone)]
pub enum Source {
    Static(Credentials),
    /// The default chain, with an optional shared-config profile.
    Chain {
        profile: Option<String>,
    },
}

/// SigV4 signing as an [`crate::Authorizer`].
pub struct SigV4 {
    pub service: String,
    pub region: String,
    pub source: Source,
    env: Env,
    cached: Mutex<Option<(Credentials, SystemTime)>>,
}

impl std::fmt::Debug for SigV4 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigV4")
            .field("service", &self.service)
            .field("region", &self.region)
            .field("source", &self.source)
            .finish()
    }
}

impl SigV4 {
    pub fn new(service: &str, region: &str, source: Source) -> Self {
        Self::with_env(service, region, source, process_env())
    }

    pub fn with_env(service: &str, region: &str, source: Source, env: Env) -> Self {
        SigV4 {
            service: service.into(),
            region: region.into(),
            source,
            env,
            cached: Mutex::new(None),
        }
    }

    /// The credentials to sign with now.
    pub async fn credentials(&self, client: &reqwest::Client) -> Result<Credentials, Error> {
        let profile = match &self.source {
            Source::Static(c) => return Ok(c.clone()),
            Source::Chain { profile } => profile.clone(),
        };
        let now = SystemTime::now();
        if let Some((c, until)) = self.cached.lock().unwrap().as_ref() {
            if *until > now {
                return Ok(c.clone());
            }
        }
        let (c, expires) =
            resolve_chain(client, &self.env, profile.as_deref(), &self.region).await?;
        if let Some(e) = expires {
            // Kept until five minutes before they expire.
            if let Ok(until) = e.duration_since(now) {
                if until > Duration::from_secs(300) {
                    *self.cached.lock().unwrap() = Some((c.clone(), e - Duration::from_secs(300)));
                }
            }
        }
        Ok(c)
    }
}

impl crate::Authorizer for SigV4 {
    fn authorize<'a>(
        &'a self,
        client: &'a reqwest::Client,
        req: &'a mut reqwest::Request,
    ) -> futures_util::future::BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let creds = self.credentials(client).await?;
            req.headers_mut().remove(reqwest::header::AUTHORIZATION);
            crate::sigv4::sign_request(req, &creds, &self.region, &self.service, SystemTime::now());
            Ok(())
        })
    }

    fn describe(&self) -> String {
        let source = match &self.source {
            Source::Static(_) => "static credentials".to_string(),
            Source::Chain { profile: Some(p) } => format!("AWS profile {p}"),
            Source::Chain { profile: None } => "the AWS credential chain".to_string(),
        };
        format!(
            "SigV4 for {} in {} from {source}",
            self.service, self.region
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> Env {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        std::sync::Arc::new(move |k| m.get(k).cloned())
    }

    fn files_env(dir: &std::path::Path, extra: &[(&str, &str)]) -> Env {
        let config = dir.join("config").display().to_string();
        let creds = dir.join("credentials").display().to_string();
        let mut pairs: Vec<(String, String)> = vec![
            ("AWS_CONFIG_FILE".into(), config),
            ("AWS_SHARED_CREDENTIALS_FILE".into(), creds),
        ];
        pairs.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let m: HashMap<String, String> = pairs.into_iter().collect();
        std::sync::Arc::new(move |k| m.get(k).cloned())
    }

    #[test]
    fn region_precedence() {
        let e = env(&[
            ("AWS_REGION", "eu-west-1"),
            ("AWS_DEFAULT_REGION", "us-west-2"),
        ]);
        assert_eq!(region(None, &e), "eu-west-1");
        assert_eq!(region(Some("ap-southeast-2"), &e), "ap-southeast-2");
        assert_eq!(
            region(None, &env(&[("AWS_DEFAULT_REGION", "us-west-2")])),
            "us-west-2"
        );
        assert_eq!(region(None, &env(&[])), "us-east-1");
    }

    #[test]
    fn lists_profiles_from_both_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config"),
            "[default]\nregion=us-east-1\n[profile work]\nsso_session = x\n[sso-session x]\n",
        )
        .unwrap();
        std::fs::write(d.path().join("credentials"), "[ci]\naws_access_key_id=A\n").unwrap();
        assert_eq!(
            profiles(&files_env(d.path(), &[])),
            ["ci", "default", "work"]
        );
    }

    #[tokio::test]
    async fn environment_keys_unless_a_profile_is_named() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("credentials"),
            "[work]\naws_access_key_id = WORK\naws_secret_access_key = s\n",
        )
        .unwrap();
        let client = reqwest::Client::new();
        let e = files_env(
            d.path(),
            &[
                ("AWS_ACCESS_KEY_ID", "ENV"),
                ("AWS_SECRET_ACCESS_KEY", "s"),
                ("AWS_SESSION_TOKEN", "t"),
            ],
        );
        let (c, _) = resolve_chain(&client, &e, None, "us-east-1").await.unwrap();
        assert_eq!(c.access_key_id, "ENV");
        assert_eq!(c.session_token.as_deref(), Some("t"));
        let (c, _) = resolve_chain(&client, &e, Some("work"), "us-east-1")
            .await
            .unwrap();
        assert_eq!(c.access_key_id, "WORK");
        let e = files_env(
            d.path(),
            &[
                ("AWS_ACCESS_KEY_ID", "ENV"),
                ("AWS_SECRET_ACCESS_KEY", "s"),
                ("AWS_PROFILE", "work"),
            ],
        );
        let (c, _) = resolve_chain(&client, &e, None, "us-east-1").await.unwrap();
        assert_eq!(c.access_key_id, "WORK");
    }

    #[tokio::test]
    async fn config_file_profiles_and_credentials_file_precedence() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config"),
            "[profile dev]\naws_access_key_id = CFG\naws_secret_access_key = c\n\n[default]\naws_access_key_id = DEF\naws_secret_access_key = d\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("credentials"),
            "# comment\n[dev]\naws_access_key_id = CRED\n",
        )
        .unwrap();
        let client = reqwest::Client::new();
        let e = files_env(d.path(), &[]);
        let (c, _) = resolve_chain(&client, &e, Some("dev"), "us-east-1")
            .await
            .unwrap();
        assert_eq!(
            (c.access_key_id.as_str(), c.secret_access_key.as_str()),
            ("CRED", "c")
        );
        let (c, _) = resolve_chain(&client, &e, None, "us-east-1").await.unwrap();
        assert_eq!(c.access_key_id, "DEF");
        let err = resolve_chain(&client, &e, Some("missing"), "us-east-1")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("profile missing is not in the AWS config"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_credential_process_and_keeps_expiring_credentials() {
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("creds.sh");
        let count = d.path().join("count");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho x >> '{}'\necho '{{\"Version\":1,\"AccessKeyId\":\"PROC\",\"SecretAccessKey\":\"p\",\"SessionToken\":\"st\",\"Expiration\":\"2999-01-01T00:00:00Z\"}}'\n",
                count.display()
            ),
        )
        .unwrap();
        std::fs::write(
            d.path().join("config"),
            format!(
                "[profile p]\ncredential_process = /bin/sh {}\n",
                script.display()
            ),
        )
        .unwrap();
        let s = SigV4::with_env(
            "bedrock",
            "us-east-1",
            Source::Chain {
                profile: Some("p".into()),
            },
            files_env(d.path(), &[]),
        );
        let client = reqwest::Client::new();
        let c = s.credentials(&client).await.unwrap();
        assert_eq!(c.access_key_id, "PROC");
        assert_eq!(c.session_token.as_deref(), Some("st"));
        s.credentials(&client).await.unwrap();
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_process_is_reported_without_its_output() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config"),
            "[profile p]\ncredential_process = echo SECRETVALUE; echo oops >&2; exit 3\n",
        )
        .unwrap();
        let err = resolve_chain(
            &reqwest::Client::new(),
            &files_env(d.path(), &[]),
            Some("p"),
            "us-east-1",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("credential_process failed: oops"), "{err}");
        assert!(!err.contains("SECRETVALUE"));
    }

    async fn serve(body: &'static str, seen: std::sync::Arc<Mutex<String>>) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = vec![0u8; 16384];
                let n = s.read(&mut buf).await.unwrap_or(0);
                *seen.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn container_credentials_with_an_authorization_token() {
        let seen = std::sync::Arc::new(Mutex::new(String::new()));
        let url = serve(
            r#"{"AccessKeyId":"CONT","SecretAccessKey":"c","Token":"tok","Expiration":"2999-01-01T00:00:00Z"}"#,
            seen.clone(),
        )
        .await;
        let d = tempfile::tempdir().unwrap();
        let full = format!("{url}/creds");
        let e = files_env(
            d.path(),
            &[
                ("AWS_CONTAINER_CREDENTIALS_FULL_URI", full.as_str()),
                ("AWS_CONTAINER_AUTHORIZATION_TOKEN", "Bearer abc"),
            ],
        );
        let (c, exp) = resolve_chain(&reqwest::Client::new(), &e, None, "us-east-1")
            .await
            .unwrap();
        assert_eq!(c.access_key_id, "CONT");
        assert_eq!(c.session_token.as_deref(), Some("tok"));
        assert!(exp.is_some());
        let req = seen.lock().unwrap().to_ascii_lowercase();
        assert!(req.starts_with("get /creds"), "{req}");
        assert!(req.contains("authorization: bearer abc"), "{req}");
    }

    #[tokio::test]
    async fn web_identity_assumes_the_role_through_sts() {
        let seen = std::sync::Arc::new(Mutex::new(String::new()));
        let url = serve(
            "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials>\
             <AccessKeyId>WEB</AccessKeyId><SecretAccessKey>w</SecretAccessKey><SessionToken>wt</SessionToken>\
             <Expiration>2999-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleWithWebIdentityResult>\
             </AssumeRoleWithWebIdentityResponse>",
            seen.clone(),
        )
        .await;
        let d = tempfile::tempdir().unwrap();
        let token = d.path().join("token");
        std::fs::write(&token, "jwt-token\n").unwrap();
        let token = token.display().to_string();
        let e = files_env(
            d.path(),
            &[
                ("AWS_WEB_IDENTITY_TOKEN_FILE", token.as_str()),
                ("AWS_ROLE_ARN", "arn:aws:iam::1:role/r"),
                ("AWS_ENDPOINT_URL_STS", url.as_str()),
            ],
        );
        let (c, _) = resolve_chain(&reqwest::Client::new(), &e, None, "us-east-1")
            .await
            .unwrap();
        assert_eq!(c.access_key_id, "WEB");
        assert_eq!(c.session_token.as_deref(), Some("wt"));
        let req = seen.lock().unwrap().clone();
        assert!(req.contains("Action=AssumeRoleWithWebIdentity"), "{req}");
        assert!(req.contains("WebIdentityToken=jwt-token"), "{req}");
    }

    #[tokio::test]
    async fn nothing_configured_is_a_clear_error() {
        let d = tempfile::tempdir().unwrap();
        let err = resolve_chain(
            &reqwest::Client::new(),
            &files_env(d.path(), &[]),
            None,
            "us-east-1",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("AWS default credential chain failed: no credentials found"),
            "{err}"
        );
    }

    #[test]
    fn ambient_configuration() {
        assert!(!ambient(&env(&[("AWS_REGION", "x")])));
        for k in CHAIN_ENV {
            assert!(ambient(&env(&[(k, "value")])), "{k}");
        }
    }
}
