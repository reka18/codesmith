//! OpenAPI tools in Code Mode, as OpenCode's `OpenAPI.fromSpec` makes
//! them: each operation of a configured OpenAPI 3.x spec becomes
//! `tools.<name>.<operationId>(input)` inside `execute`, and a call is an
//! HTTP request to the API.
//!
//! Specs come only from config (`openapi.<name>.spec`: a file, a URL or
//! the document itself). Every call is an outbound request and asks the
//! `openapi` permission for `<METHOD> <url>` (an "always" answer covers
//! that method on that origin). Credentials (`auth`, `headers`) come from
//! config, usually as `{env:VAR}`; they need `base_url` and are sent only
//! to its origin. Redirects are followed by hand, each target checked
//! against the deny rules; one to another origin asks the permission for
//! its target like the first call (OpenCode does not ask), and a public
//! host may not redirect to a local or private address. Once a redirect
//! leaves the origin, credentials and configured headers are dropped for
//! the rest of the chain. Responses are capped at [`MAX_RESPONSE_BYTES`].

pub mod runtime;
pub mod spec;

use crate::permission::{Gate, Request};
use crate::tool::webfetch::Reach;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use runtime::{AuthContext, Credential, Prepared, Resolver};
pub use spec::{from_spec, Generated, Operation, Scheme, Skipped, Tool};

/// The permission action an OpenAPI call asks.
pub const ACTION: &str = "openapi";
/// The most of a response body read (OpenCode reads 50 MiB; Code Mode
/// hands results to a program with a 512 MiB memory limit).
pub const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
/// The most of a spec read from a file or URL.
pub const MAX_SPEC_BYTES: usize = 20 * 1024 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 10;
const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];
/// A failed spec load is tried again after this long.
const RETRY_AFTER: Duration = Duration::from_secs(60);
/// The prefix of an OpenAPI tool's session name.
const PREFIX: &str = "openapi:";

/// One configured API (`openapi.<name>`).
#[derive(Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// A file path (absolute, `~/`, or relative to the project root), an
    /// `http(s)://` URL, or the document itself. JSON only.
    pub spec: Option<Value>,
    /// The server every operation goes to, overriding the spec's
    /// `servers`. Required with `auth` or `headers`.
    pub base_url: Option<String>,
    /// Headers sent with every request (declared header parameters may
    /// override them; credentials always win).
    pub headers: BTreeMap<String, String>,
    /// Credentials by security scheme name (`components.securitySchemes`).
    pub auth: BTreeMap<String, CredentialConfig>,
    /// `false` leaves the API out.
    pub enabled: Option<bool>,
    /// Seconds a request may take (default 60).
    pub timeout: Option<u64>,
    /// Why the entry did not parse (see [`lenient`]).
    #[serde(skip)]
    pub invalid: Option<String>,
}

/// The `openapi` map, each entry parsed on its own: one that does not
/// parse becomes an entry that reports why when loaded, so a typo in one
/// API leaves the rest of the config (and the other APIs) working.
pub fn lenient<'de, D>(d: D) -> Result<BTreeMap<String, ApiConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = BTreeMap::<String, Value>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .map(|(name, v)| {
            let c = serde_json::from_value::<ApiConfig>(v).unwrap_or_else(|e| ApiConfig {
                // Serde names the field or variant, never a value.
                invalid: Some(e.to_string()),
                ..Default::default()
            });
            (name, c)
        })
        .collect())
}

impl std::fmt::Debug for ApiConfig {
    // Header values and credentials stay out of debug output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiConfig")
            .field(
                "spec",
                &self.spec.as_ref().map(|s| s.as_str().unwrap_or("(inline)")),
            )
            .field("base_url", &self.base_url)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("auth", &self.auth.keys().collect::<Vec<_>>())
            .field("enabled", &self.enabled)
            .field("timeout", &self.timeout)
            .field("invalid", &self.invalid)
            .finish()
    }
}

/// A configured credential: `{"type": "bearer", "token": …}`,
/// `{"type": "basic", "username": …, "password": …}`, `{"type": "api_key",
/// "value": …}` (sent where the scheme says) or `{"type": "header",
/// "name": …, "value": …}`.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialConfig {
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
    #[serde(alias = "apiKey")]
    ApiKey {
        value: String,
    },
    Header {
        name: String,
        value: String,
    },
}

impl std::fmt::Debug for CredentialConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            CredentialConfig::Bearer { .. } => "bearer",
            CredentialConfig::Basic { .. } => "basic",
            CredentialConfig::ApiKey { .. } => "api_key",
            CredentialConfig::Header { .. } => "header",
        };
        write!(f, "CredentialConfig({kind})")
    }
}

impl CredentialConfig {
    /// The credential, or `None` when its secret is empty (an unset
    /// `{env:VAR}`), so the next security alternative is tried.
    fn credential(&self) -> Option<Credential> {
        let set = |s: &String| !s.is_empty();
        match self {
            CredentialConfig::Bearer { token } => {
                set(token).then(|| Credential::Bearer(token.clone()))
            }
            CredentialConfig::Basic { username, password } => (set(username) || set(password))
                .then(|| Credential::Basic {
                    username: username.clone(),
                    password: password.clone(),
                }),
            CredentialConfig::ApiKey { value } => {
                set(value).then(|| Credential::ApiKey(value.clone()))
            }
            CredentialConfig::Header { name, value } => {
                (set(name) && set(value)).then(|| Credential::Header {
                    name: name.clone(),
                    value: value.clone(),
                })
            }
        }
    }
}

/// A loaded API: its tools and how its requests are sent.
pub struct Api {
    pub name: String,
    pub generated: Generated,
    /// Where credentials and configured headers may go: `base_url`'s
    /// origin, when either is configured.
    origin: Option<url::Origin>,
    headers: Vec<(String, String)>,
    resolver: Option<Arc<Resolver>>,
    timeout: Duration,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("name", &self.name)
            .field("tools", &self.generated.tools.len())
            .field("skipped", &self.generated.skipped.len())
            .finish()
    }
}

impl Api {
    /// An API from a parsed document, as OpenCode's `fromSpec` options
    /// give it: `base_url`, static headers and a credential resolver.
    /// Credentials and headers need `base_url` (they go only to its
    /// origin).
    pub fn new(
        name: &str,
        document: &Value,
        base_url: Option<&str>,
        headers: Vec<(String, String)>,
        resolver: Option<Arc<Resolver>>,
    ) -> Result<Api, String> {
        let origin = match base_url {
            Some(b) => Some(
                url::Url::parse(b)
                    .map_err(|_| format!("base_url '{b}' is not an absolute HTTP(S) URL"))?
                    .origin(),
            ),
            None if resolver.is_some() || !headers.is_empty() => {
                return Err(format!(
                    "openapi.{name}: `auth` and `headers` need `base_url` (credentials are sent only to its origin)"
                ))
            }
            None => None,
        };
        Ok(Api {
            name: name.to_string(),
            generated: from_spec(document, base_url),
            origin,
            headers,
            resolver,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn with_timeout(mut self, t: Duration) -> Api {
        self.timeout = t;
        self
    }

    /// The session's name for one of this API's tools.
    pub fn tool_name(&self, t: &Tool) -> String {
        format!("{PREFIX}{}:{}", self.name, t.dotted())
    }

    /// Its tools as Code Mode entries, those `keep` accepts.
    pub fn entries(&self, keep: impl Fn(&Tool) -> bool) -> Vec<crate::codemode::Entry> {
        self.generated
            .tools
            .iter()
            .filter(|t| keep(t))
            .map(|t| crate::codemode::Entry {
                tool: self.tool_name(t),
                namespace: self.name.clone(),
                name: t.dotted(),
                description: t.description.clone(),
                schema: t.input.clone(),
                output: t.output.clone(),
                freeform: false,
            })
            .collect()
    }
}

/// Whether a session tool name is an OpenAPI tool's.
pub fn is_tool(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// The API and tool a session tool name refers to.
pub fn find<'a>(apis: &'a [Arc<Api>], name: &str) -> Option<(&'a Arc<Api>, &'a Tool)> {
    let rest = name.strip_prefix(PREFIX)?;
    let (api, dotted) = rest.split_once(':')?;
    let a = apis.iter().find(|a| a.name == api)?;
    let t = a.generated.tool(dotted)?;
    Some((a, t))
}

/// The permission resource for a call: `<METHOD> <url>`, with no fragment
/// and no credential carried in the query.
fn resource(method: &str, url: &url::Url, hidden: &[&str]) -> String {
    let mut u = url.clone();
    u.set_fragment(None);
    if !hidden.is_empty() && u.query().is_some() {
        let kept: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| !hidden.contains(&k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        u.set_query(None);
        if !kept.is_empty() {
            u.query_pairs_mut().extend_pairs(kept);
        }
    }
    format!("{method} {u}")
}

/// The forms of a call's resource checked against the deny rules and
/// policies: [`resource`], and percent-decoded (a server reads `/%61dmin`
/// as `/admin`). Allowing goes by [`resource`] alone.
fn deny_forms(method: &str, url: &url::Url, hidden: &[&str]) -> Vec<String> {
    let shown = resource(method, url, hidden);
    let decoded = crate::tool::webfetch::percent_decoded(&shown);
    if decoded == shown {
        vec![shown]
    } else {
        vec![shown, decoded]
    }
}

/// A representative resource for an operation (its URL template), to hide
/// operations the rules deny outright.
pub fn template_resource(t: &Tool) -> String {
    format!("{} {}", t.plan.operation.method, t.plan.url)
}

fn client(reach: Reach) -> Result<&'static reqwest::Client, String> {
    static CLIENTS: [std::sync::OnceLock<Option<reqwest::Client>>; 2] =
        [const { std::sync::OnceLock::new() }; 2];
    crate::tool::webfetch::shared_client(&CLIENTS, reach, || {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
    })
}

/// Ask `gate` for `req` (the user may be prompted).
fn ask(gate: &Gate, req: &Request) -> Result<(), String> {
    tokio::task::block_in_place(|| gate.assert(req)).map_err(|e| e.to_string())
}

/// Call `tool` of `api` with the model's `input`: validate it, ask the
/// `openapi` permission, resolve credentials, send, and decode.
///
/// Redirects are followed here, one hop at a time, each target checked
/// against the deny rules. Credentials and configured headers go only to
/// the API's origin: once a redirect leaves it they are dropped for the
/// rest of the chain, even if a later hop comes back.
pub async fn call(api: &Api, tool: &Tool, input: &Value, gate: &Gate) -> Result<Value, String> {
    let plan = &tool.plan;
    let op = &plan.operation;
    let label = runtime::label_of(op);
    let mut prepared = runtime::build(plan, input, &api.headers)?;
    if let Some(o) = &api.origin {
        if prepared.url.origin() != *o {
            return Err(format!(
                "{label}: refusing to send credentials outside {}",
                o.ascii_serialization()
            ));
        }
    }
    let shown = resource(&prepared.method, &prepared.url, &[]);
    let origin = prepared.url.origin().ascii_serialization();
    gate.assert_not_denied(ACTION, &deny_forms(&prepared.method, &prepared.url, &[]))
        .map_err(|e| e.to_string())?;
    ask(
        gate,
        &Request {
            action: ACTION.into(),
            resources: vec![shown.clone()],
            save: vec![format!("{} {origin}/*", prepared.method)],
            title: format!("Call {}.{}: {shown}", api.name, tool.dotted()),
            access: None,
        },
    )?;
    let applied = runtime::resolve_auth(plan, api.resolver.as_deref())?;
    let secrets = applied.secrets();
    let hidden = applied.query_names();
    let configured: Vec<String> = api
        .headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    let mut reach = Reach::Any;
    let mut credentialed = true;
    let mut hops = 0;
    loop {
        if api
            .origin
            .as_ref()
            .is_some_and(|o| prepared.url.origin() != *o)
        {
            credentialed = false;
        }
        let mut wire = prepared.clone();
        if credentialed {
            applied.apply(&mut wire);
        } else {
            wire.headers.retain(|(k, _)| !configured.contains(k));
        }
        let method = reqwest::Method::from_bytes(wire.method.as_bytes())
            .map_err(|_| format!("{label}: invalid method"))?;
        let mut rb = client(reach)?
            .request(method, wire.url.clone())
            .timeout(api.timeout);
        for (k, v) in &wire.headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|_| format!("{label}: invalid header name '{k}'"))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|_| format!("{label}: invalid value for header '{k}'"))?;
            rb = rb.header(name, value);
        }
        if let Some((media, body)) = &wire.body {
            rb = rb
                .header(reqwest::header::CONTENT_TYPE, media)
                .body(body.clone());
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| match crate::tool::webfetch::refusal(&e) {
                Some(r) => format!("{label}: redirect refused: {r}"),
                None => runtime::transport(op, &e),
            })?;
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if let (true, Some(location)) = (REDIRECTS.contains(&status), location) {
            if hops >= MAX_REDIRECTS {
                return Err(format!(
                    "{label}: too many redirects (more than {MAX_REDIRECTS})"
                ));
            }
            let mut next = prepared
                .url
                .join(&location)
                .map_err(|_| format!("{label}: invalid redirect location"))?;
            crate::tool::webfetch::check_url(&next)
                .map_err(|e| format!("{label}: redirect refused: {e}"))?;
            next.set_fragment(None);
            if (status == 303 && prepared.method != "HEAD")
                || (matches!(status, 301 | 302) && prepared.method == "POST")
            {
                prepared.method = "GET".into();
                prepared.body = None;
            }
            gate.assert_not_denied(ACTION, &deny_forms(&prepared.method, &next, &hidden))
                .map_err(|e| e.to_string())?;
            let from_local = reach == Reach::Any
                && crate::tool::webfetch::answered_locally(&resp, &prepared.url).await;
            reach = crate::tool::webfetch::check_redirect_target(&prepared.url, from_local, &next)
                .await
                .map_err(|e| format!("{label}: redirect refused: {e}"))?;
            if next.origin() != prepared.url.origin() {
                // A request to another origin, perhaps with the body
                // (307/308): asked like the first one.
                let shown = resource(&prepared.method, &next, &hidden);
                ask(
                    gate,
                    &Request {
                        action: ACTION.into(),
                        resources: vec![shown.clone()],
                        save: vec![format!(
                            "{} {}/*",
                            prepared.method,
                            next.origin().ascii_serialization()
                        )],
                        title: format!(
                            "Call {}.{}: {shown} (redirected from {})",
                            api.name,
                            tool.dotted(),
                            prepared.url.origin().ascii_serialization()
                        ),
                        access: None,
                    },
                )
                .map_err(|e| redact(e, &secrets))?;
            }
            prepared.url = next;
            hops += 1;
            continue;
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
        {
            return Err(runtime::too_large(op, MAX_RESPONSE_BYTES));
        }
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| format!("{label} failed while reading the response body."))?;
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(runtime::too_large(op, MAX_RESPONSE_BYTES));
            }
            body.extend_from_slice(&chunk);
        }
        // A server that echoes a credential back in an error does not get
        // it shown to the model.
        return runtime::decode(op, status, content_type.as_deref(), &body)
            .map_err(|e| redact(e, &secrets));
    }
}

/// `text` with each secret replaced.
fn redact(mut text: String, secrets: &[String]) -> String {
    for s in secrets.iter().filter(|s| s.len() >= 4) {
        text = text.replace(s.as_str(), "[redacted]");
    }
    text
}

/// A call's value as Code Mode's tool text (see
/// [`crate::codemode::JSON_RESULT`]).
pub fn result_text(v: &Value) -> String {
    format!("{}{v}", crate::codemode::JSON_RESULT)
}

// ----- loading from config ---------------------------------------------------

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Read the spec a config entry names.
async fn read_spec(name: &str, spec: &Value, root: &Path) -> Result<Value, String> {
    let text = match spec {
        Value::Object(_) => return check_document(name, spec.clone()),
        Value::String(s) if s.starts_with("http://") || s.starts_with("https://") => {
            fetch_spec(name, s).await?
        }
        Value::String(s) => {
            let path = match s.strip_prefix("~/") {
                Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
                None => root.join(s),
            };
            let meta = std::fs::metadata(&path)
                .map_err(|e| format!("openapi.{name}: cannot read {}: {e}", path.display()))?;
            if meta.len() > MAX_SPEC_BYTES as u64 {
                return Err(format!(
                    "openapi.{name}: {} is larger than {} MiB",
                    path.display(),
                    MAX_SPEC_BYTES / (1024 * 1024)
                ));
            }
            std::fs::read_to_string(&path)
                .map_err(|e| format!("openapi.{name}: cannot read {}: {e}", path.display()))?
        }
        _ => {
            return Err(format!(
                "openapi.{name}: `spec` must be a path, a URL or the document"
            ))
        }
    };
    let doc: Value = serde_json::from_str(&text).map_err(|e| {
        format!("openapi.{name}: the spec is not JSON ({e}); YAML specs are not supported, convert them to JSON")
    })?;
    check_document(name, doc)
}

fn check_document(name: &str, doc: Value) -> Result<Value, String> {
    let version = doc.get("openapi").and_then(Value::as_str).unwrap_or("");
    if !version.starts_with("3.") {
        return Err(format!(
            "openapi.{name}: not an OpenAPI 3.x document (Swagger 2.0 is not supported)"
        ));
    }
    Ok(doc)
}

/// Fetch a spec URL: no credentials, at most [`MAX_SPEC_BYTES`].
async fn fetch_spec(name: &str, url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| "Unable to create the HTTP client".to_string())?;
    let fail =
        |e: reqwest::Error| format!("openapi.{name}: cannot fetch the spec: {}", e.without_url());
    let resp = client.get(url).send().await.map_err(fail)?;
    if !resp.status().is_success() {
        return Err(format!(
            "openapi.{name}: cannot fetch the spec: HTTP {}",
            resp.status().as_u16()
        ));
    }
    let too_large = || {
        format!(
            "openapi.{name}: the spec is larger than {} MiB",
            MAX_SPEC_BYTES / (1024 * 1024)
        )
    };
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_SPEC_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(fail)?;
        if body.len() + chunk.len() > MAX_SPEC_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| format!("openapi.{name}: the spec is not UTF-8"))
}

/// Load one configured API.
pub async fn load_one(name: &str, c: &ApiConfig, root: &Path) -> Result<Api, String> {
    if !valid_name(name) {
        return Err(format!(
            "openapi.{name}: a name is letters, digits, `_` and `-`"
        ));
    }
    if let Some(e) = &c.invalid {
        return Err(format!("openapi.{name}: {e}"));
    }
    let Some(spec) = &c.spec else {
        return Err(format!("openapi.{name}: `spec` is required"));
    };
    let doc = read_spec(name, spec, root).await?;
    let auth = c.auth.clone();
    let resolver: Option<Arc<Resolver>> = (!auth.is_empty()).then(|| {
        let f: Arc<Resolver> = Arc::new(move |cx: &AuthContext| {
            Ok(auth.get(cx.name).and_then(CredentialConfig::credential))
        });
        f
    });
    let headers = c
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let api = Api::new(name, &doc, c.base_url.as_deref(), headers, resolver)?;
    Ok(match c.timeout {
        Some(t) if t > 0 => api.with_timeout(Duration::from_secs(t)),
        _ => api,
    })
}

struct Slot {
    api: Option<Arc<Api>>,
    error: Option<String>,
    at: Instant,
    reported: bool,
}

type Key = [u8; 32];

fn cache() -> &'static Mutex<HashMap<Key, Slot>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<Key, Slot>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The cache key: a digest, so no secret is kept as text.
fn key(name: &str, c: &ApiConfig, root: &Path) -> Key {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update([0]);
    h.update(root.to_string_lossy().as_bytes());
    h.update([0]);
    h.update(
        c.spec
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_default()
            .as_bytes(),
    );
    h.update([0]);
    h.update(c.base_url.as_deref().unwrap_or("").as_bytes());
    for (k, v) in &c.headers {
        h.update([1]);
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
    }
    for (k, v) in &c.auth {
        h.update([2]);
        h.update(k.as_bytes());
        h.update(format!("{:?}", v.credential().is_some()).as_bytes());
        if let Some(cr) = v.credential() {
            match cr {
                Credential::Bearer(t) | Credential::ApiKey(t) => h.update(t.as_bytes()),
                Credential::Basic { username, password } => {
                    h.update(username.as_bytes());
                    h.update([0]);
                    h.update(password.as_bytes());
                }
                Credential::Header { name, value } => {
                    h.update(name.as_bytes());
                    h.update([0]);
                    h.update(value.as_bytes());
                }
            }
        }
    }
    h.update(c.timeout.unwrap_or(0).to_le_bytes());
    h.update(c.invalid.as_deref().unwrap_or("").as_bytes());
    h.finalize().into()
}

fn enabled(c: &ApiConfig) -> bool {
    c.enabled != Some(false)
}

/// Load the configured APIs not loaded yet (once per process; a failed
/// load is tried again after a minute). Returns the problems not reported
/// before, to show the user.
pub async fn load(config: &BTreeMap<String, ApiConfig>, root: &Path) -> Vec<String> {
    let mut notices = Vec::new();
    for (name, c) in config.iter().filter(|(_, c)| enabled(c)) {
        let k = key(name, c, root);
        let due = {
            let g = cache().lock().unwrap_or_else(|e| e.into_inner());
            match g.get(&k) {
                None => true,
                Some(s) => s.api.is_none() && s.at.elapsed() >= RETRY_AFTER,
            }
        };
        if due {
            let r = load_one(name, c, root).await;
            let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
            let reported = g
                .get(&k)
                .is_some_and(|s| s.reported && s.error == r.as_ref().err().cloned());
            g.insert(
                k,
                match r {
                    Ok(api) => Slot {
                        api: Some(Arc::new(api)),
                        error: None,
                        at: Instant::now(),
                        reported: false,
                    },
                    Err(e) => Slot {
                        api: None,
                        error: Some(e),
                        at: Instant::now(),
                        reported,
                    },
                },
            );
        }
        let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = g.get_mut(&k) {
            if !s.reported {
                s.reported = true;
                if let Some(e) = &s.error {
                    notices.push(format!("OpenAPI tools unavailable: {e}"));
                }
            }
        }
    }
    notices
}

/// The configured APIs loaded so far (see [`load`]).
pub fn loaded(config: &BTreeMap<String, ApiConfig>, root: &Path) -> Vec<Arc<Api>> {
    if config.is_empty() {
        return vec![];
    }
    let g = cache().lock().unwrap_or_else(|e| e.into_inner());
    config
        .iter()
        .filter(|(_, c)| enabled(c))
        .filter_map(|(name, c)| g.get(&key(name, c, root)).and_then(|s| s.api.clone()))
        .collect()
}

/// Add OpenAPI entries to the other Code Mode entries, leaving out any
/// whose path is taken by (or nests with) an existing tool's.
pub fn merge_entries(
    existing: &mut Vec<crate::codemode::Entry>,
    extra: Vec<crate::codemode::Entry>,
) {
    let path = |e: &crate::codemode::Entry| {
        if e.namespace.is_empty() {
            e.name.clone()
        } else {
            format!("{}.{}", e.namespace, e.name)
        }
    };
    let taken: Vec<String> = existing.iter().map(path).collect();
    let clashes = |p: &str| {
        taken
            .iter()
            .any(|t| t == p || p.starts_with(&format!("{t}.")) || t.starts_with(&format!("{p}.")))
    };
    existing.extend(extra.into_iter().filter(|e| !clashes(&path(e))));
}

#[cfg(test)]
mod tests;
